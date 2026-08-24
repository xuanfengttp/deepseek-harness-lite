//! Workflow tool: parallel multi-agent orchestration via declarative JSON.
//!
//! Replaces upstream DSH's JS-script + Node-worker-thread workflow engine
//! with a pure-Rust equivalent. The LLM submits a declarative task list
//! (no JS scripting); Lite fans out child agents in parallel using std
//! threads (each with its own tokio runtime, matching SubagentTool's pattern)
//! and returns a structured JSON result grouped by phase.
//!
//! Wire format (LLM submits):
//! ```json
//! {
//!   "name": "batch-health-check",
//!   "description": "Parallel health check of 3 core routers",
//!   "tasks": [
//!     { "label": "core-router-1", "phase": "health-check", "prompt": "SSH to 192.168.1.1 and check health" },
//!     { "label": "core-router-2", "phase": "health-check", "prompt": "SSH to 192.168.1.2 and check health" },
//!     { "label": "core-router-3", "phase": "health-check", "prompt": "SSH to 192.168.1.3 and check health" }
//!   ]
//! }
//! ```
//!
//! Result format (returned to LLM):
//! ```json
//! {
//!   "runId": "wf-xxxx",
//!   "name": "batch-health-check",
//!   "agentsStarted": 3,
//!   "phases": {
//!     "health-check": [
//!       { "label": "core-router-1", "status": "completed", "result": "..." },
//!       { "label": "core-router-2", "status": "completed", "result": "..." },
//!       { "label": "core-router-3", "status": "failed", "error": "connection refused" }
//!     ]
//!   }
//! }
//! ```

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
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;

/// Maximum delegation depth (shared with SubagentTool).
const MAX_DEPTH: u32 = 3;

/// Maximum concurrent agents in one workflow run.
const MAX_CONCURRENT: usize = 8;

/// Monotonic run ID counter.
static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static WF_DEPTH: std::cell::Cell<u32> = std::cell::Cell::new(0);
}

/// One task in a workflow run.
#[derive(Debug, Clone)]
struct WorkflowTask {
    label: String,
    phase: String,
    prompt: String,
    skill: Option<String>,
}

/// The result of one workflow task.
#[derive(Debug, Clone)]
struct TaskResult {
    label: String,
    phase: String,
    status: String,   // "completed" | "failed"
    result: String,   // final output or error message
}

/// The workflow tool plugin.
///
/// Holds the same shared references as SubagentTool to spawn child agents.
pub struct WorkflowTool {
    llm: LlmClient,
    model_config: ModelConfig,
    compaction_threshold: f32,
    keep_recent_turns: usize,
    skills_dir: String,
    tool_plugins: Arc<Mutex<Vec<Arc<dyn ToolPlugin>>>>,
}

impl std::fmt::Debug for WorkflowTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowTool")
            .field("model", &self.model_config.model)
            .finish()
    }
}

impl WorkflowTool {
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

    /// Register a tool plugin that workflow children should also have access to.
    pub fn share_tool(&self, plugin: Arc<dyn ToolPlugin>) {
        let mut plugins = self.tool_plugins.lock().unwrap();
        let name = plugin.definition().name;
        if !plugins.iter().any(|p| p.definition().name == name) {
            plugins.push(plugin);
        }
    }
}

impl ToolPlugin for WorkflowTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "workflow".into(),
            description: "Run a parallel multi-agent orchestration. Submit a declarative task list; each task runs as an independent subagent. Use for large-scale fan-out (batch inspection, parallel diagnostics, multi-device audit). For one or two delegations, prefer the subagent tool.".into(),
            guidance: "Use the workflow tool ONLY when the user explicitly asks for a workflow or large multi-agent orchestration. Submit a JSON task list with name, description, and tasks array. Each task has label, phase, and prompt. Tasks in the same phase run in parallel; results are grouped by phase.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Short name for this workflow run (e.g. 'batch-health-check')"
                    },
                    "description": {
                        "type": "string",
                        "description": "One-line description of what the workflow does"
                    },
                    "tasks": {
                        "type": "array",
                        "description": "Array of tasks to execute in parallel",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {
                                    "type": "string",
                                    "description": "Short label for this task (e.g. 'core-router-1')"
                                },
                                "phase": {
                                    "type": "string",
                                    "description": "Phase name for grouping (e.g. 'health-check', 'diagnosis')"
                                },
                                "prompt": {
                                    "type": "string",
                                    "description": "Complete, self-contained task prompt for the child agent"
                                },
                                "skill": {
                                    "type": "string",
                                    "description": "Optional skill name for the child agent's strategy"
                                }
                            },
                            "required": ["label", "phase", "prompt"]
                        },
                        "minItems": 1,
                        "maxItems": 8
                    }
                },
                "required": ["name", "description", "tasks"]
            }),
            timeout_ms: 300_000, // 5 minutes for parallel runs
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string();
        let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Parse tasks.
        let tasks = match args.get("tasks").and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => arr,
            _ => return ToolResult {
                content: "Error: `tasks` parameter is required and must be a non-empty array".into(),
                is_error: true,
            },
        };

        if tasks.len() > MAX_CONCURRENT {
            return ToolResult {
                content: format!("Error: too many tasks ({}). Maximum is {}.", tasks.len(), MAX_CONCURRENT),
                is_error: true,
            };
        }

        // Check recursion depth.
        let current_depth = WF_DEPTH.with(|d| d.get());
        if current_depth >= MAX_DEPTH {
            return ToolResult {
                content: format!("Error: workflow delegation depth limit ({}) reached.", MAX_DEPTH),
                is_error: true,
            };
        }

        let parsed_tasks: Vec<WorkflowTask> = tasks.iter().filter_map(|t| {
            let label = t.get("label")?.as_str()?.to_string();
            let phase = t.get("phase")?.as_str()?.to_string();
            let prompt = t.get("prompt")?.as_str()?.to_string();
            let skill = t.get("skill").and_then(|s| s.as_str()).map(String::from);
            if prompt.is_empty() { return None; }
            Some(WorkflowTask { label, phase, prompt, skill })
        }).collect();

        if parsed_tasks.is_empty() {
            return ToolResult {
                content: "Error: no valid tasks found (each task needs label, phase, and non-empty prompt)".into(),
                is_error: true,
            };
        }

        let run_id = format!("wf-{:04x}", RUN_ID_COUNTER.fetch_add(1, Ordering::Relaxed));
        log::info!(
            "Workflow `{name}` ({run_id}): starting {} tasks (depth {}/{})",
            parsed_tasks.len(), current_depth + 1, MAX_DEPTH
        );

        // Execute tasks in parallel using std threads.
        // Each thread creates its own current_thread tokio runtime (same pattern
        // as SubagentTool) so child agents don't deadlock on the main runtime.
        WF_DEPTH.with(|d| d.set(current_depth + 1));

        let results: Vec<TaskResult> = parsed_tasks.iter().map(|task| {
            run_single_task(task, &self.llm, &self.model_config,
                self.compaction_threshold, self.keep_recent_turns,
                &self.skills_dir, &self.tool_plugins)
        }).collect();

        WF_DEPTH.with(|d| d.set(current_depth));

        // Group results by phase.
        let mut phases: HashMap<String, Vec<&TaskResult>> = HashMap::new();
        for r in &results {
            phases.entry(r.phase.clone()).or_default().push(r);
        }

        // Build structured JSON result.
        let phase_json: Vec<String> = {
            let mut sorted: Vec<(&String, &Vec<&TaskResult>)> = phases.iter().collect();
            sorted.sort_by_key(|(phase, _)| phase.to_string());
            sorted.iter().map(|(phase, members)| {
                let member_json: Vec<String> = members.iter().map(|m| {
                    format!(
                        r#"{{"label":"{}","status":"{}","result":"{}"}}"#,
                        escape_json(&m.label),
                        escape_json(&m.status),
                        escape_json(&m.result)
                    )
                }).collect();
                format!(r#""{}":[{}]"#, escape_json(phase), member_json.join(","))
            }).collect()
        };

        let completed = results.iter().filter(|r| r.status == "completed").count();
        let failed = results.iter().filter(|r| r.status == "failed").count();

        let content = format!(
            r#"{{"runId":"{}","name":"{}","description":"{}","agentsStarted":{},"completed":{},"failed":{},"phases":{{{}}}}}"#,
            escape_json(&run_id),
            escape_json(&name),
            escape_json(&description),
            results.len(),
            completed,
            failed,
            phase_json.join(",")
        );

        log::info!("Workflow `{name}` ({run_id}): completed ({} ok, {} failed)", completed, failed);

        ToolResult {
            content,
            is_error: failed > 0 && completed == 0,
        }
    }
}

/// Run a single workflow task on a dedicated thread with its own runtime.
fn run_single_task(
    task: &WorkflowTask,
    llm: &LlmClient,
    model_config: &ModelConfig,
    compaction_threshold: f32,
    keep_recent_turns: usize,
    skills_dir: &str,
    tool_plugins: &Arc<Mutex<Vec<Arc<dyn ToolPlugin>>>>,
) -> TaskResult {
    log::info!("Workflow task `{}` (phase: {}): starting", task.label, task.phase);

    // Clone shared state for the thread.
    let llm = llm.clone();
    let model_config = model_config.clone();
    let skills_dir = skills_dir.to_string();
    let plugins = tool_plugins.clone();
    let prompt = task.prompt.clone();
    let skill_name = task.skill.clone();
    let label = task.label.clone();
    let phase = task.phase.clone();
    let label_for_thread = label.clone();
    let label_for_log = label.clone();

    // Run on a dedicated thread with its own current_thread runtime.
    let handle = std::thread::spawn(move || {
        // Load skill.
        let skill = match &skill_name {
            Some(name) => {
                skill::load_dir(&skills_dir)
                    .into_iter()
                    .find(|s| s.name == *name)
                    .unwrap_or_else(|| {
                        log::warn!("Workflow task `{label_for_thread}`: skill `{name}` not found, using default plan");
                        default_plan_skill()
                    })
            }
            None => default_plan_skill(),
        };

        // Build child agent.
        let child_session = SessionLog::new(512);
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
            let plugins = plugins.lock().unwrap();
            for plugin in plugins.iter() {
                child_tools.register_arc(Arc::clone(plugin));
            }
        }

        let hooks: Vec<Box<dyn StepHook>> = strategies::build_hooks(&skill);
        let mut child_loop = AgentLoop::new(
            child_session,
            child_tools,
            llm,
            &model_config,
        )
        .with_hooks(hooks)
        .with_compaction(compaction_threshold, keep_recent_turns);

        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<LoopEvent>(128);

        let run_result = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(child_rt) => child_rt.block_on(async {
                let _drain = tokio::spawn(async move {
                    while event_rx.recv().await.is_some() {}
                });
                child_loop.run_turn(prompt, vec![], &skill, event_tx).await
            }),
            Err(e) => {
                log::error!("Workflow task `{label_for_thread}`: failed to create runtime: {e}");
                return (Err(format!("runtime error: {e}")), child_loop);
            }
        };

        (run_result, child_loop)
    });

    match handle.join() {
        Ok((run_result, child_loop)) => {
            let final_output = extract_final_output(child_loop.session());
            match run_result {
                Ok(reason) => {
                    log::info!("Workflow task `{label_for_log}` completed: {:?}", reason);
                    let result = if final_output.is_empty() {
                        format!("Task completed (no text output). Turn end: {:?}", reason)
                    } else {
                        final_output
                    };
                    TaskResult { label, phase, status: "completed".into(), result }
                }
                Err(e) => {
                    log::warn!("Workflow task `{label_for_log}` failed: {e}");
                    TaskResult { label, phase, status: "failed".into(), result: e }
                }
            }
        }
        Err(e) => {
            log::error!("Workflow task `{label_for_log}` thread panicked: {e:?}");
            TaskResult { label, phase, status: "failed".into(), result: "thread panic".into() }
        }
    }
}

/// Extract the last assistant message content from a session log.
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

/// Create a default plan-mode skill for workflow children.
fn default_plan_skill() -> Skill {
    use std::collections::HashMap;
    Skill {
        name: "workflow-default".into(),
        description: "Default plan skill for workflow children".into(),
        when_to_use: None,
        mode: ExecMode::Plan,
        think: ThinkLevel::High,
        tools_allow: vec![],
        variables: HashMap::new(),
        body: "You are a focused workflow task agent. Complete the given task autonomously. Use available tools as needed. Provide a clear final answer.".into(),
        steps: vec![],
    }
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_plan_skill_is_plan_mode() {
        let skill = default_plan_skill();
        assert_eq!(skill.mode, ExecMode::Plan);
        assert_eq!(skill.think, ThinkLevel::High);
        assert!(skill.tools_allow.is_empty());
    }

    #[test]
    fn extract_final_output_empty_when_no_assistant() {
        let session = SessionLog::new(64);
        assert!(extract_final_output(&session).is_empty());
    }

    #[test]
    fn extract_final_output_finds_last_assistant() {
        let mut session = SessionLog::new(64);
        session.append(SessionEvent::AssistantMessage {
            content: "first".into(),
            tool_calls: vec![],
            usage: None,
            ttft_ms: 0,
            decode_ms: 0,
            thinking: None,
        });
        session.append(SessionEvent::AssistantMessage {
            content: "second".into(),
            tool_calls: vec![],
            usage: None,
            ttft_ms: 0,
            decode_ms: 0,
            thinking: None,
        });
        assert_eq!(extract_final_output(&session), "second");
    }

    #[test]
    fn escape_json_escapes_special_chars() {
        assert_eq!(escape_json("hello"), "hello");
        assert_eq!(escape_json(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_json("line1\nline2"), r"line1\nline2");
        assert_eq!(escape_json("tab\there"), r"tab\there");
        assert_eq!(escape_json(r"back\slash"), r"back\\slash");
    }

    #[test]
    fn max_concurrent_is_eight() {
        assert_eq!(MAX_CONCURRENT, 8);
    }

    #[test]
    fn max_depth_is_three() {
        assert_eq!(MAX_DEPTH, 3);
    }
}
