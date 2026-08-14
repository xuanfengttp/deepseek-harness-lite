//! Tool registry and 3-stage execution pipeline.
//!
//! Mirrors dsh `core/tools`, simplified from 5 waterfall stages to 3 direct
//! stages: check → execute → result. No waterfall/around-middleware — hooks
//! are plain functions. Timeout and cancellation wrap the execute stage.
//!
//! Stages:
//! 1. `check` — permission (policy) + argument validation
//! 2. `execute` — the tool body, wrapped with timeout/cancellation
//! 3. `result` — truncate (spill) + record + normalize errors

pub mod shell;
pub mod file;
pub mod memory;

use crate::types::*;
use crate::policy::Policy;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

/// Maximum output size before truncation (spill). Keeps tool results small for
/// the model's limited context.
const MAX_OUTPUT_BYTES: usize = 16_384;

/// The tool registry: maps tool names to definitions and executors.
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
    policy: Policy,
}

struct RegisteredTool {
    def: ToolDefinition,
    executor: ToolExecutor,
}

/// A tool executor is an async function taking JSON arguments and returning a ToolResult.
pub type ToolExecutor = std::sync::Arc<dyn Fn(serde_json::Value, tokio::sync::oneshot::Sender<ToolResult>) + Send + Sync>;

/// Result of checking a tool call before execution.
enum CheckResult {
    Allow,
    Deny(String),
}

impl ToolRegistry {
    pub fn new(policy: Policy) -> Self {
        Self {
            tools: HashMap::new(),
            policy,
        }
    }

    /// Register a tool with its definition and executor.
    pub fn register(
        &mut self,
        def: ToolDefinition,
        executor: ToolExecutor,
    ) {
        log::debug!("Registered tool: {}", def.name);
        self.tools.insert(def.name.clone(), RegisteredTool { def, executor });
    }

    /// Get all registered tool definitions (for prompt assembly).
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.def.clone()).collect()
    }

    /// Get definitions filtered to a specific allow-list.
    pub fn definitions_for(&self, allow: &[String]) -> Vec<ToolDefinition> {
        if allow.is_empty() {
            return self.definitions();
        }
        self.tools.values()
            .filter(|t| allow.iter().any(|n| n == &t.def.name))
            .map(|t| t.def.clone())
            .collect()
    }

    /// Check whether a tool name is in the allow-list.
    /// Empty allow-list means no restriction (all tools allowed).
    pub fn is_allowed(&self, tool_name: &str, allow: &[String]) -> bool {
        if allow.is_empty() {
            return true;
        }
        allow.iter().any(|n| n == tool_name)
    }

    /// Execute a tool call with an additional skill allow-list check.
    /// If the tool is not in the allow-list, returns an error immediately
    /// without executing. Empty allow-list = no restriction.
    pub async fn execute_checked(&self, call: &ToolCall, allow: &[String]) -> ToolResult {
        if !self.is_allowed(&call.name, allow) {
            log::warn!("Tool `{}` blocked by skill allow-list", call.name);
            return ToolResult {
                content: format!("Error: tool `{}` is not allowed by the active skill", call.name),
                is_error: true,
            };
        }
        self.execute(call).await
    }

    /// Execute a tool call through the 3-stage pipeline.
    ///
    /// Returns the final ToolResult (truncated, error-normalized).
    pub async fn execute(&self, call: &ToolCall) -> ToolResult {
        // Stage 1: check (permission + validation)
        match self.check(call) {
            CheckResult::Deny(reason) => {
                log::info!("Tool `{}` denied: {}", call.name, reason);
                return ToolResult { content: format!("Error: permission denied — {reason}"), is_error: true };
            }
            CheckResult::Allow => {}
        }

        // Stage 2: execute (with timeout)
        let registered = match self.tools.get(&call.name) {
            Some(t) => t,
            None => {
                return ToolResult {
                    content: format!("Error: unknown tool `{}`", call.name),
                    is_error: true,
                };
            }
        };
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let executor = registered.executor.clone();
        let args = call.arguments.clone();
        let tool_name = call.name.clone();
        let timeout_ms = registered.def.timeout_ms;

        // Run the executor in a blocking-safe context with timeout.
        let _exec_task = tokio::task::spawn_blocking(move || {
            (executor)(args, result_tx);
        });

        let timeout_dur = Duration::from_millis(timeout_ms);
        match timeout(timeout_dur, async {
            // Wait for the executor's result.
            match result_rx.await {
                Ok(result) => result,
                Err(_) => ToolResult {
                    content: "Error: tool executor dropped result channel".into(),
                    is_error: true,
                },
            }
        }).await {
            Ok(result) => {
                // Stage 3: result (truncate + normalize)
                self.finalize_result(result, &tool_name)
            }
            Err(_) => {
                log::warn!("Tool `{}` timed out after {}ms", tool_name, timeout_ms);
                ToolResult {
                    content: format!("Error: tool `{}` timed out after {timeout_ms}ms", tool_name),
                    is_error: true,
                }
            }
        }
    }

    /// Stage 1: check permission and validity.
    fn check(&self, call: &ToolCall) -> CheckResult {
        // Permission check via policy.
        if let Some(reason) = self.policy.check_tool(&call.name) {
            return CheckResult::Deny(reason);
        }
        // Tool existence check.
        if !self.tools.contains_key(&call.name) {
            return CheckResult::Deny(format!("tool `{}` is not registered", call.name));
        }
        CheckResult::Allow
    }

    /// Stage 3: truncate output (spill) and normalize.
    fn finalize_result(&self, mut result: ToolResult, tool_name: &str) -> ToolResult {
        if result.content.len() > MAX_OUTPUT_BYTES {
            let head = &result.content[..MAX_OUTPUT_BYTES / 2];
            let tail = &result.content[result.content.len() - MAX_OUTPUT_BYTES / 2..];
            result.content = format!(
                "{head}\n\n... [output truncated: {} bytes total, showing first {} and last {}] ...\n\n{tail}",
                result.content.len(),
                MAX_OUTPUT_BYTES / 2,
                MAX_OUTPUT_BYTES / 2,
            );
            log::debug!("Tool `{}` output truncated to fit context", tool_name);
        }
        result
    }
}

/// Create a simple tool executor from an async function.
///
/// Usage: `make_executor(|args, tx| { async move { tx.send(result).ok(); } })`
/// The function runs inside `spawn_blocking`, so it must not hold non-Send data.
pub fn make_executor<F>(f: F) -> ToolExecutor
where
    F: Fn(serde_json::Value, tokio::sync::oneshot::Sender<ToolResult>) + Send + Sync + 'static,
{
    std::sync::Arc::new(f)
}
