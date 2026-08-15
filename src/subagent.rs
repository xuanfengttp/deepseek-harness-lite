//! Subagent tool: delegate a task to a child agent loop.
//!
//! Maps to dsh `tool-subagent` + `subagent-spawn-in-process`.
//! The child agent runs in the same process with an independent SessionLog
//! but shared ToolRegistry + LlmClient. Only the final assistant output is
//! returned to the parent — intermediate steps are invisible.
//!
//! Key design (from dsh reference):
//! - `inheritsParentContext = false` — child starts fresh, no parent history
//! - `maxDepth = 3` — recursion limit to prevent infinite delegation
//! - Foreground only — single-threaded, one subagent at a time
//! - Optional `skill` param — if specified, child uses that skill's strategy
//!   (workflow=deterministic, plan=autonomous); if not, uses PlanStrategy

use crate::types::*;
use crate::tools::ToolPlugin;
use crate::agent::{AgentLoop, LoopEvent};
use crate::session::SessionLog;
use crate::llm::LlmClient;
use crate::tools::ToolRegistry;
use crate::strategies;
use crate::hooks::StepHook;
use crate::skill;
use std::sync::{Arc, Mutex};

/// Maximum delegation depth (matches dsh's maxDepth=3).
const MAX_DEPTH: u32 = 3;

// Thread-local delegation depth counter (prevents infinite recursion).
thread_local! {
    static DEPTH: std::cell::Cell<u32> = std::cell::Cell::new(0);
}

/// The subagent tool plugin.
///
/// Holds shared references to the LLM client, tool registry template,
/// model config, and skill loader — enough to spawn a child agent loop.
pub struct SubagentTool {
    llm: LlmClient,
    model_config: ModelConfig,
    compaction_threshold: f32,
    keep_recent_turns: usize,
    skills_dir: String,
    /// Shared registry of tool plugins (for re-registration in child).
    /// We store Arc so child can clone and register the same tools.
    tool_plugins: Arc<Mutex<Vec<Arc<dyn ToolPlugin>>>>,
}

impl std::fmt::Debug for SubagentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentTool")
            .field("model", &self.model_config.model)
            .finish()
    }
}

impl SubagentTool {
    pub fn new(
        llm: LlmClient,
        model_config: ModelConfig,
        compaction_threshold: f32,
        keep_recent_turns: usize,
        skills_dir: String,
    ) -> Self {
        Self {
            llm,
            model_config,
            compaction_threshold,
            keep_recent_turns,
            skills_dir,
            tool_plugins: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a tool plugin that subagents should also have access to.
    /// Called during tool registration to share built-ins with children.
    pub fn share_tool(&self, plugin: Arc<dyn ToolPlugin>) {
        let mut plugins = self.tool_plugins.lock().unwrap();
        // Avoid duplicates by name.
        let name = plugin.definition().name;
        if !plugins.iter().any(|p| p.definition().name == name) {
            plugins.push(plugin);
        }
    }
}

impl ToolPlugin for SubagentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "subagent".into(),
            description: "Delegate a self-contained task to a child agent. The child runs independently and returns only its final result. Use for focused subtasks (research, scoped analysis, verification) to save parent context.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "Short 3-5 word description of the delegated task"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The complete, self-contained task for the child agent. Include all context needed — the child sees nothing from the parent conversation."
                    },
                    "skill": {
                        "type": "string",
                        "description": "Optional skill name to use for strategy selection. If specified, child uses that skill's mode (workflow=deterministic, plan=autonomous). If omitted, child uses plan mode (full LLM autonomy)."
                    }
                },
                "required": ["description", "prompt"]
            }),
            timeout_ms: 120_000,
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let description = args
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("subagent task");
        let prompt = args
            .get("prompt")
            .and_then(|p| p.as_str())
            .unwrap_or("");
        let skill_name = args
            .get("skill")
            .and_then(|s| s.as_str())
            .map(String::from);

        if prompt.is_empty() {
            return ToolResult {
                content: "Error: `prompt` parameter is required and must be non-empty".into(),
                is_error: true,
            };
        }

        // Check recursion depth.
        let current_depth = DEPTH.with(|d| d.get());
        if current_depth >= MAX_DEPTH {
            log::warn!(
                "Subagent delegation blocked: max depth {} reached",
                MAX_DEPTH
            );
            return ToolResult {
                content: format!(
                    "Error: subagent delegation depth limit ({}) reached. Cannot spawn another subagent.",
                    MAX_DEPTH
                ),
                is_error: true,
            };
        }

        log::info!(
            "Subagent: delegating `{description}` (depth {}/{})",
            current_depth + 1,
            MAX_DEPTH
        );

        // Load the skill (if specified), otherwise use a default plan skill.
        let skill = match &skill_name {
            Some(name) => {
                match skill::load_dir(&self.skills_dir)
                    .into_iter()
                    .find(|s| s.name == *name)
                {
                    Some(s) => {
                        log::info!("Subagent: using skill `{name}` (mode: {:?})", s.mode);
                        s
                    }
                    None => {
                        log::warn!("Subagent: skill `{name}` not found, using default plan");
                        default_plan_skill()
                    }
                }
            }
            None => default_plan_skill(),
        };

        // Build a child AgentLoop with independent SessionLog.
        let child_session = SessionLog::new(512);

        // Re-register shared tools into a fresh registry.
        let policy = crate::policy::Policy::from_config(&ToolsConfig {
            shell: true,
            file_read: true,
            file_write: true,
            file_search: true,
            ssh_exec: false,
            memory: true,
            todo: false,
        });
        let mut child_tools = ToolRegistry::new(policy);
        {
            let plugins = self.tool_plugins.lock().unwrap();
            for plugin in plugins.iter() {
                child_tools.register_arc(Arc::clone(plugin));
            }
        }

        let llm = self.llm.clone();
        let hooks: Vec<Box<dyn StepHook>> = strategies::build_hooks(&skill);

        let mut child_loop = AgentLoop::new(
            child_session,
            child_tools,
            llm,
            &self.model_config,
        )
        .with_hooks(hooks)
        .with_compaction(self.compaction_threshold, self.keep_recent_turns);

        // Run the child turn on a dedicated runtime.
        //
        // WHY a separate runtime: execute() runs inside spawn_blocking on a
        // dedicated blocking thread. The main runtime uses current_thread mode,
        // so its I/O driver lives on the main thread — which is blocked waiting
        // for this spawn_blocking task. Calling Handle::current().block_on()
        // would deadlock because tokio::spawn (used by the LLM connection driver)
        // needs the main runtime's scheduler, which is stuck.
        //
        // Solution: create an independent current_thread runtime on this blocking
        // thread. LlmClient only holds base_url + api_key (no runtime-bound state),
        // and each stream() call creates a fresh TcpStream, so the independent
        // runtime can drive the child agent's async work without any dependency
        // on the main runtime.
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<LoopEvent>(128);

        // Increment depth for the child.
        DEPTH.with(|d| d.set(current_depth + 1));

        let run_result = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(child_rt) => child_rt.block_on(async {
                // Spawn a task to drain events (we don't bubble them to parent).
                let _drain = tokio::spawn(async move {
                    while event_rx.recv().await.is_some() {}
                });

                child_loop.run_turn(prompt.to_string(), &skill, event_tx).await
            }),
            Err(e) => {
                log::error!("Subagent: failed to create child runtime: {e}");
                Err(format!("Failed to create child runtime: {e}"))
            }
        };

        // Decrement depth after child completes.
        DEPTH.with(|d| d.set(current_depth));

        // Extract the final assistant output from the child session.
        let child_session = child_loop.session();
        let final_output = extract_final_output(child_session);

        match run_result {
            Ok(reason) => {
                log::info!("Subagent `{description}` completed: {:?}", reason);
                let content = if final_output.is_empty() {
                    format!("Subagent completed (no text output). Turn end: {:?}", reason)
                } else {
                    final_output
                };
                ToolResult {
                    content,
                    is_error: matches!(reason, TurnEndReason::Error),
                }
            }
            Err(e) => {
                log::warn!("Subagent `{description}` failed: {e}");
                ToolResult {
                    content: format!("Subagent failed: {e}"),
                    is_error: true,
                }
            }
        }
    }
}

/// Extract the last assistant message content from a session log.
/// This is the "finalAssistantOutput" — only the last assistant text is
/// returned, not intermediate tool calls or results.
fn extract_final_output(session: &SessionLog) -> String {
    let mut last_assistant = String::new();
    for event in session.events() {
        if let SessionEvent::AssistantMessage { content, .. } = event {
            if !content.is_empty() {
                last_assistant = content.clone();
            }
        }
    }
    last_assistant
}

/// Create a default plan-mode skill for subagents with no skill specified.
fn default_plan_skill() -> Skill {
    use std::collections::HashMap;
    Skill {
        name: "subagent-default".into(),
        description: "Default plan skill for subagent delegation".into(),
        when_to_use: None,
        mode: ExecMode::Plan,
        think: true,
        tools_allow: vec![], // empty = all tools allowed
        variables: HashMap::new(),
        body: "You are a focused subagent. Complete the given task autonomously. Use available tools as needed. Provide a clear final answer.".into(),
        steps: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_plan_skill_is_plan_mode() {
        let skill = default_plan_skill();
        assert!(matches!(skill.mode, ExecMode::Plan));
        assert!(skill.think);
        assert!(!skill.body.is_empty());
    }

    #[test]
    fn extract_final_output_finds_last_assistant() {
        let mut session = SessionLog::new(64);
        session.append(SessionEvent::UserMessage {
            content: "test".into(),
        });
        session.append(SessionEvent::AssistantMessage {
            content: "first response".into(),
            tool_calls: vec![],
            usage: None,
            ttft_ms: 0,
            decode_ms: 0,
        });
        session.append(SessionEvent::AssistantMessage {
            content: "final response".into(),
            tool_calls: vec![],
            usage: None,
            ttft_ms: 0,
            decode_ms: 0,
        });
        assert_eq!(extract_final_output(&session), "final response");
    }

    #[test]
    fn extract_final_output_empty_when_no_assistant() {
        let session = SessionLog::new(64);
        assert_eq!(extract_final_output(&session), "");
    }

    #[test]
    fn max_depth_is_three() {
        assert_eq!(MAX_DEPTH, 3);
    }
}
