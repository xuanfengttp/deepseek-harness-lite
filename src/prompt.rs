//! Prompt assembly: ordered sections + tool schemas + variable interpolation.
//!
//! Maps to dsh `core/system-prompt` + `ctx.systemPrompt.section()`.
//! The system prompt is assembled from ordered `PromptSection`s, sorted by
//! `order` field. Sections are built dynamically — any plugin can contribute.
//!
//! Small-model constraints enforced here:
//! - Tool guidance uses behavior rules (HOW to use), not descriptions (WHAT it does)
//! - Each section is short and high-signal — every token has a permanent context cost
//! - Total system prompt stays under ~300 tokens for the default skill

use crate::types::*;
use std::collections::HashMap;

/// Assembled system prompt ready for the LLM request.
pub struct AssembledPrompt {
    /// The system message text (sections joined + variables interpolated).
    pub system: String,
    /// Tool schemas filtered to the active skill's allow-list.
    pub tools: Vec<ToolDefinition>,
}

/// A system prompt section with ordering.
///
/// Plugins contribute sections by pushing `PromptSection`s into the assemble
/// call. Sections are sorted by `order` (ascending) before joining.
pub struct PromptSection {
    #[allow(dead_code)]
    pub name: String,
    pub order: i32,
    pub text: String,
}

/// Section order conventions (mirrors dsh system-prompt order semantics):
///   -100  harness:identity  — fixed agent identity
///      0  persona           — skill body (role/methodology)
///     10  behavior-rules    — universal behavior rules
///    100  tools             — per-tool behavior guidance
pub const ORDER_IDENTITY: i32 = -100;
pub const ORDER_PERSONA: i32 = 0;
pub const ORDER_RULES: i32 = 10;
pub const ORDER_TOOLS: i32 = 100;

/// Universal behavior rules for all agents (order=10).
/// Short, high-signal rules that prevent the most common small-model mistakes.
const BEHAVIOR_RULES: &str = "## Rules\n\n- Check command exit codes; investigate failures before proceeding.\n- Verify facts with tools; do not guess or fabricate.\n- Be concise; answer the question directly.";

/// Build prompt sections from a skill and available tools.
///
/// Returns dynamic sections that `assemble()` will sort and join.
/// The layered design mirrors dsh's section-based architecture:
///   1. Identity (fixed, ~20 tokens)
///   2. Persona (skill body, variable)
///   3. Behavior rules (fixed, ~80 tokens)
///   4. Tool guidance (per-tool behavior rules, ~15 tokens/tool)
pub fn build_sections(
    skill: &Skill,
    all_tools: &[ToolDefinition],
) -> (Vec<PromptSection>, Vec<ToolDefinition>) {
    let mut sections: Vec<PromptSection> = Vec::new();

    // Layer 1: Harness identity (order=-100, always present, ~20 tokens).
    sections.push(PromptSection {
        name: "harness:identity".into(),
        order: ORDER_IDENTITY,
        text: "You are an AI agent. Working directory: {{cwd}}.".into(),
    });

    // Layer 2: Persona section (order=0, skill Markdown body).
    if !skill.body.is_empty() {
        sections.push(PromptSection {
            name: "persona".into(),
            order: ORDER_PERSONA,
            text: skill.body.clone(),
        });
    }

    // Layer 3: Behavior rules (order=10, always present, ~80 tokens).
    sections.push(PromptSection {
        name: "behavior-rules".into(),
        order: ORDER_RULES,
        text: BEHAVIOR_RULES.into(),
    });

    // Layer 4: Tool guidance (order=100, one behavior rule per allowed tool).
    // Uses `guidance` (HOW to use) not `description` (WHAT it does).
    // Description stays in the tool schema sent separately to the LLM.
    let allowed_tools = filter_tools(all_tools, &skill.tools_allow);
    if !allowed_tools.is_empty() {
        let tool_lines: Vec<String> = allowed_tools
            .iter()
            .filter(|t| !t.guidance.is_empty())
            .map(|t| format!("- `{}`: {}", t.name, t.guidance))
            .collect();
        if !tool_lines.is_empty() {
            let tool_section = format!(
                "## Tool guidance\n\n{}\n\nCall a tool by name with JSON arguments. Wait for the result before proceeding.",
                tool_lines.join("\n")
            );
            sections.push(PromptSection {
                name: "tools".into(),
                order: ORDER_TOOLS,
                text: tool_section,
            });
        }
    }

    (sections, allowed_tools)
}

/// Assemble the system prompt from dynamic sections + tool schemas.
///
/// - `sections`: ordered prompt sections (sorted by `order` field)
/// - `tools`: tool schemas (already filtered to the skill's allow-list)
/// - `variables`: variables for `{{var}}` interpolation
pub fn assemble_sections(
    sections: Vec<PromptSection>,
    tools: Vec<ToolDefinition>,
    variables: &HashMap<String, String>,
) -> AssembledPrompt {
    // Sort sections by order, then join.
    let mut sorted = sections;
    sorted.sort_by_key(|s| s.order);
    let mut system = sorted
        .into_iter()
        .map(|s| s.text)
        .collect::<Vec<_>>()
        .join("\n\n");

    // Interpolate variables: {{var}} → value.
    system = interpolate(&system, variables);

    AssembledPrompt { system, tools }
}

/// Assemble the system prompt from the active skill and available tools.
///
/// Convenience function: builds sections from skill + tools, then assembles.
/// This preserves backward compatibility with the original assemble() signature.
pub fn assemble(
    skill: &Skill,
    all_tools: &[ToolDefinition],
    extra_variables: &HashMap<String, String>,
) -> AssembledPrompt {
    // Merge variables: skill variables first, then extras override.
    let mut vars = skill.variables.clone();
    for (k, v) in extra_variables {
        vars.insert(k.clone(), v.clone());
    }

    let (sections, allowed_tools) = build_sections(skill, all_tools);
    assemble_sections(sections, allowed_tools, &vars)
}

/// Filter tools to only those in the allow-list. If the allow-list is empty,
/// return all tools (no restriction).
fn filter_tools(all: &[ToolDefinition], allow: &[String]) -> Vec<ToolDefinition> {
    if allow.is_empty() {
        return all.to_vec();
    }
    all.iter()
        .filter(|t| allow.iter().any(|name| name == &t.name))
        .cloned()
        .collect()
}

/// Replace all `{{var}}` occurrences with the corresponding value.
/// Unknown variables are left as-is (not stripped) so misconfiguration is visible.
fn interpolate(text: &str, vars: &HashMap<String, String>) -> String {
    let mut result = text.to_string();
    for (key, value) in vars {
        let placeholder = format!("{{{{{key}}}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_skill(name: &str, body: &str, tools: Vec<&str>) -> Skill {
        Skill {
            name: name.into(),
            description: "test".into(),
            when_to_use: None,
            mode: ExecMode::Plan,
            think: false,
            tools_allow: tools.into_iter().map(String::from).collect(),
            variables: HashMap::new(),
            body: body.into(),
            steps: vec![],
        }
    }

    fn make_tool(name: &str, desc: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: desc.into(),
            guidance: format!("Use {name} correctly."),
            parameters: serde_json::json!({"type": "object"}),
            timeout_ms: 5000,
        }
    }

    #[test]
    fn assembles_persona_and_tools() {
        let skill = make_skill("diag", "You are a diagnostic agent.", vec!["shell", "file_read"]);
        let tools = vec![
            make_tool("shell", "Execute a shell command"),
            make_tool("file_read", "Read a file"),
            make_tool("file_write", "Write a file"),
        ];
        let prompt = assemble(&skill, &tools, &HashMap::new());
        // Persona present.
        assert!(prompt.system.contains("diagnostic agent"));
        // Tool guidance uses behavior rules (guidance), not descriptions.
        assert!(prompt.system.contains("Use shell correctly."));
        assert!(prompt.system.contains("Use file_read correctly."));
        // file_write is not in the allow-list, should be filtered out.
        assert!(!prompt.system.contains("file_write"));
        assert_eq!(prompt.tools.len(), 2);
    }

    #[test]
    fn includes_harness_identity_and_rules() {
        let skill = make_skill("diag", "You are a diagnostic agent.", vec![]);
        let prompt = assemble(&skill, &[], &HashMap::new());
        // Identity section (order=-100) should be present.
        assert!(prompt.system.contains("You are an AI agent."));
        assert!(prompt.system.contains("{{cwd}}")); // uninterpolated when no vars provided
        // Behavior rules section (order=10) should be present.
        assert!(prompt.system.contains("## Rules"));
        assert!(prompt.system.contains("investigate failures"));
        // Identity should come before persona.
        let id_pos = prompt.system.find("You are an AI agent.").unwrap();
        let persona_pos = prompt.system.find("diagnostic agent").unwrap();
        assert!(id_pos < persona_pos);
        // Persona should come before rules.
        let rules_pos = prompt.system.find("## Rules").unwrap();
        assert!(persona_pos < rules_pos);
    }

    #[test]
    fn interpolates_variables() {
        let mut skill = make_skill("diag", "Device model: {{device_model}}", vec![]);
        skill.variables.insert("device_model".into(), "AX-200".into());
        let prompt = assemble(&skill, &[], &HashMap::new());
        assert!(prompt.system.contains("AX-200"));
        assert!(!prompt.system.contains("{{device_model}}"));
    }

    #[test]
    fn cwd_variable_interpolated() {
        let skill = make_skill("diag", "You are a diagnostic agent.", vec![]);
        let mut extra = HashMap::new();
        extra.insert("cwd".into(), "/tmp/work".into());
        let prompt = assemble(&skill, &[], &extra);
        assert!(prompt.system.contains("/tmp/work"));
        assert!(!prompt.system.contains("{{cwd}}"));
    }

    #[test]
    fn empty_allow_list_returns_all_tools() {
        let skill = make_skill("diag", "body", vec![]);
        let tools = vec![make_tool("shell", "x"), make_tool("file_read", "y")];
        let prompt = assemble(&skill, &tools, &HashMap::new());
        assert_eq!(prompt.tools.len(), 2);
    }

    #[test]
    fn dynamic_sections_sorted_by_order() {
        let sections = vec![
            PromptSection { name: "tools".into(), order: 100, text: "Tools section".into() },
            PromptSection { name: "persona".into(), order: 0, text: "Persona section".into() },
            PromptSection { name: "custom".into(), order: 50, text: "Custom section".into() },
        ];
        let prompt = assemble_sections(sections, vec![], &HashMap::new());
        // Persona (0) should come before custom (50) before tools (100).
        let persona_pos = prompt.system.find("Persona section").unwrap();
        let custom_pos = prompt.system.find("Custom section").unwrap();
        let tools_pos = prompt.system.find("Tools section").unwrap();
        assert!(persona_pos < custom_pos);
        assert!(custom_pos < tools_pos);
    }
}
