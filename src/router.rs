//! Skill router — decides which skill (if any) should handle a user input.
//!
//! Two-stage dispatch:
//! 1. Zero-cost rule fast-check: if the user input explicitly mentions a
//!    skill name (case-insensitive substring), use that skill directly.
//! 2. Otherwise ask the LLM (a tiny, cheap request) to pick the single best
//!    matching skill from the loaded list; anything that fails or is ambiguous
//!    falls back to `Direct` so the conversation is never blocked.

use crate::llm::{LlmClient, LlmRequest, StreamEvent};
use crate::types::{Message, ModelConfig, Skill, ThinkLevel};
use std::time::Duration;

/// Outcome of routing one user input.
pub enum RouteDecision {
    /// Use this skill by name (must match a loaded skill name).
    UseSkill(String),
    /// No skill matches — plain direct conversation (no skill persona).
    Direct,
}

/// Route `user_input` to a skill, or `Direct` when nothing matches.
///
/// Never fails: every error path (stream failure, error event, timeout,
/// unparseable response) silently falls back to `RouteDecision::Direct`.
pub async fn route_skill(
    llm: &LlmClient,
    model: &ModelConfig,
    user_input: &str,
    skills: &[Skill],
    recent_context: &[Message],
) -> RouteDecision {
    // (a) Rule fast-check — zero cost, highest priority.
    if let Some(name) = rule_match(user_input, skills) {
        log::debug!("skill router: rule fast-check matched '{name}'");
        return RouteDecision::UseSkill(name);
    }
    // (b) LLM judgment.
    llm_route(llm, model, user_input, skills, recent_context).await
}

/// Rule fast-check: case-insensitive substring match of a skill name in the
/// input. When several names match (e.g. "check" inside "health-check"),
/// the longest name wins so the most specific skill is chosen.
fn rule_match(user_input: &str, skills: &[Skill]) -> Option<String> {
    let lower = user_input.to_lowercase();
    let mut best: Option<&Skill> = None;
    for skill in skills {
        let name = skill.name.to_lowercase();
        if !lower.contains(&name) {
            continue;
        }
        match best {
            Some(b) if b.name.len() >= skill.name.len() => {}
            _ => best = Some(skill),
        }
    }
    best.map(|s| s.name.clone())
}

/// LLM judgment path. Sends a tiny deterministic request and parses the reply.
async fn llm_route(
    llm: &LlmClient,
    model: &ModelConfig,
    user_input: &str,
    skills: &[Skill],
    recent_context: &[Message],
) -> RouteDecision {
    if skills.is_empty() {
        return RouteDecision::Direct;
    }

    let request = LlmRequest {
        model: model.model.clone(),
        system: build_system_prompt(skills),
        messages: vec![Message::User {
            content: build_user_message(user_input, recent_context),
            images: Vec::new(),
        }],
        tools: Vec::new(),
        max_tokens: 16,
        temperature: 0.0,
        think: ThinkLevel::Off,
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    // One hard timeout around the whole interaction (stream + recv loop):
    // a hung or slow router must never block the conversation.
    let outcome = tokio::time::timeout(Duration::from_secs(8), async {
        if let Err(e) = llm.stream(request, tx).await {
            log::warn!("skill router: stream failed, falling back to direct: {e}");
            return None;
        }
        let mut content = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Done { content: c, .. } => {
                    content = c;
                    break;
                }
                StreamEvent::Error(e) => {
                    log::warn!("skill router: error event, falling back to direct: {e}");
                    return None;
                }
                _ => {}
            }
        }
        Some(content)
    })
    .await;

    match outcome {
        Ok(Some(content)) => match parse_response(&content, skills) {
            Some(name) => RouteDecision::UseSkill(name),
            None => RouteDecision::Direct,
        },
        Ok(None) => RouteDecision::Direct,
        Err(_) => {
            log::warn!("skill router: timed out, falling back to direct");
            RouteDecision::Direct
        }
    }
}

/// System prompt listing the available skills and the reply contract.
fn build_system_prompt(skills: &[Skill]) -> String {
    let mut out = String::from("You are a skill router. Available skills:\n");
    for skill in skills {
        let when = skill.when_to_use.as_deref().unwrap_or("always");
        out.push_str(&format!(
            "- name: {} — {} | when: {}\n",
            skill.name, skill.description, when
        ));
    }
    out.push_str("\nThe user request is below. Choose the ONE skill that best matches it.\n");
    out.push_str("Reply with ONLY the skill name, or \"none\" if no skill applies.");
    out
}

/// User message: the raw input, plus (when context exists) the text of the
/// most recent messages so "continue"/"what next" references resolve.
fn build_user_message(user_input: &str, recent_context: &[Message]) -> String {
    let mut msg = user_input.to_string();
    if !recent_context.is_empty() {
        let mut lines: Vec<String> = Vec::new();
        for m in recent_context.iter().rev().take(4) {
            if let Some(text) = message_text(m) {
                lines.push(text);
            }
        }
        lines.reverse();
        if !lines.is_empty() {
            msg.push_str("\n[context]\n");
            msg.push_str(&lines.join("\n"));
        }
    }
    msg
}

/// Conversational text of a message; tool results are omitted (not intent).
fn message_text(m: &Message) -> Option<String> {
    match m {
        Message::User { content, .. } => Some(content.clone()),
        Message::Assistant { content, .. } => Some(content.clone()),
        Message::Tool { .. } => None,
    }
}

/// Parse the LLM's reply into a skill name, or `None` when no skill applies.
///
/// Accepts surrounding quotes/backticks, a trailing period and any casing;
/// the cleaned text must exactly equal a skill name (case-insensitive).
fn parse_response(content: &str, skills: &[Skill]) -> Option<String> {
    let cleaned = clean_response(content);
    let lower = cleaned.to_lowercase();
    if lower.is_empty() || lower.contains("none") {
        return None;
    }
    skills
        .iter()
        .find(|s| s.name.to_lowercase() == lower)
        .map(|s| s.name.clone())
}

/// Strip quotes/backticks from both ends and a trailing period, then trim.
fn clean_response(content: &str) -> String {
    let mut s = content.trim();
    while let Some(ch) = s.chars().next() {
        if matches!(ch, '"' | '\'' | '`') {
            s = &s[ch.len_utf8()..];
        } else {
            break;
        }
    }
    while let Some(ch) = s.chars().next_back() {
        if matches!(ch, '"' | '\'' | '`' | '.') {
            s = &s[..s.len() - ch.len_utf8()];
        } else {
            break;
        }
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ExecMode;

    fn skill(name: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("test skill {name}"),
            when_to_use: Some("when the user asks about it".to_string()),
            mode: ExecMode::Plan,
            think: ThinkLevel::Off,
            tools_allow: Vec::new(),
            variables: Default::default(),
            body: "body".to_string(),
            steps: Vec::new(),
        }
    }

    fn dummy_model() -> ModelConfig {
        ModelConfig {
            base_url: "http://127.0.0.1:9".into(), // unreachable — LLM path must not run
            api_key: String::new(),
            model: "router-test".into(),
            context_window: 8192,
            max_tokens: 2048,
            temperature: 0.0,
            proxy: String::new(),
        }
    }

    #[test]
    fn rule_fast_check_matches_explicit_skill_name() {
        let skills = vec![skill("health-check")];
        assert_eq!(
            rule_match("用 health-check 检查设备", &skills),
            Some("health-check".to_string())
        );
    }

    #[test]
    fn rule_fast_check_longest_name_wins() {
        let skills = vec![skill("check"), skill("health-check")];
        assert_eq!(
            rule_match("请按 health-check 流程执行", &skills),
            Some("health-check".to_string())
        );
    }

    #[test]
    fn rule_fast_check_no_mention_is_none() {
        let skills = vec![skill("health-check")];
        assert_eq!(rule_match("你好，帮我看看这个设备的状态", &skills), None);
    }

    #[tokio::test]
    async fn route_skill_rule_hit_returns_use_skill() {
        let model = dummy_model();
        let llm = LlmClient::new(&model);
        let skills = vec![skill("health-check")];
        let decision = route_skill(&llm, &model, "用 health-check 检查设备", &skills, &[]).await;
        assert!(matches!(
            decision,
            RouteDecision::UseSkill(name) if name == "health-check"
        ));
    }

    #[test]
    fn parse_none_returns_none() {
        let skills = vec![skill("health-check")];
        assert_eq!(parse_response("none", &skills), None);
    }

    #[test]
    fn parse_quoted_skill_name() {
        let skills = vec![skill("health-check")];
        assert_eq!(
            parse_response("\"health-check\"", &skills),
            Some("health-check".to_string())
        );
        assert_eq!(
            parse_response("`health-check`", &skills),
            Some("health-check".to_string())
        );
    }

    #[test]
    fn parse_case_insensitive() {
        let skills = vec![skill("health-check")];
        assert_eq!(
            parse_response("HEALTH-CHECK.", &skills),
            Some("health-check".to_string())
        );
    }

    #[test]
    fn parse_unknown_name_returns_none() {
        let skills = vec![skill("health-check")];
        assert_eq!(parse_response("diagnostics", &skills), None);
    }
}
