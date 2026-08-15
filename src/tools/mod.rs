//! Tool registry and execution pipeline with plugin-based registration.
//!
//! Maps to dsh `core/tools` + `ctx.tools.register(defineTool())`.
//! Tools implement the `ToolPlugin` trait — definition + execution in one unit.
//! The registry handles the 3-stage pipeline: check → execute → result.
//!
//! Stages:
//! 1. `check` — permission (policy) + tool existence
//! 2. `execute` — the tool body, wrapped with timeout (inside spawn_blocking)
//! 3. `result` — truncate (spill) + normalize errors

pub mod shell;
pub mod file;
pub mod memory;
pub mod ssh;

use crate::types::*;
use crate::policy::Policy;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Maximum output size before truncation (spill). Keeps tool results small for
/// the model's limited context.
const MAX_OUTPUT_BYTES: usize = 16_384;

/// A tool plugin provides its definition and execution logic together.
///
/// Maps to dsh `defineTool()` + `ctx.tools.register()`. Each built-in tool
/// implements this trait, and `register_builtins()` registers them all.
pub trait ToolPlugin: Send + Sync {
    /// The tool's schema (name, description, parameters, timeout).
    fn definition(&self) -> ToolDefinition;

    /// Execute the tool with JSON arguments. Returns the result.
    ///
    /// This runs inside `spawn_blocking`, so it must not hold non-Send data
    /// and must not call async functions. Tools that need async execution
    /// (like SubagentTool) use `Handle::current().block_on()` internally.
    fn execute(&self, args: serde_json::Value) -> ToolResult;
}

/// The tool registry: maps tool names to plugins, enforces policy.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn ToolPlugin>>,
    policy: Policy,
}

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

    /// Register a tool plugin.
    pub fn register(&mut self, plugin: Box<dyn ToolPlugin>) {
        let def = plugin.definition();
        log::debug!("Registered tool: {}", def.name);
        self.tools.insert(def.name.clone(), Arc::from(plugin));
    }

    /// Register a tool plugin from an Arc (for sharing between parent and child agents).
    pub fn register_arc(&mut self, plugin: Arc<dyn ToolPlugin>) {
        let def = plugin.definition();
        log::debug!("Registered tool (shared): {}", def.name);
        self.tools.insert(def.name.clone(), plugin);
    }

    /// Get Arc references to all registered plugins (for sharing with subagents).
    pub fn plugins_arc(&self) -> Vec<Arc<dyn ToolPlugin>> {
        self.tools.values().cloned().collect()
    }

    /// Get all registered tool definitions (for prompt assembly).
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|p| p.definition()).collect()
    }

    /// Get definitions filtered to a specific allow-list.
    #[allow(dead_code)]
    pub fn definitions_for(&self, allow: &[String]) -> Vec<ToolDefinition> {
        if allow.is_empty() {
            return self.definitions();
        }
        self.tools
            .values()
            .filter(|p| allow.iter().any(|n| n == &p.definition().name))
            .map(|p| p.definition())
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
        // Stage 1: check (permission + existence)
        match self.check(call) {
            CheckResult::Deny(reason) => {
                log::info!("Tool `{}` denied: {}", call.name, reason);
                return ToolResult {
                    content: format!("Error: permission denied — {reason}"),
                    is_error: true,
                };
            }
            CheckResult::Allow => {}
        }

        // Stage 2: execute (with timeout, inside spawn_blocking)
        let plugin = match self.tools.get(&call.name) {
            Some(p) => p,
            None => {
                return ToolResult {
                    content: format!("Error: unknown tool `{}`", call.name),
                    is_error: true,
                };
            }
        };

        let args = call.arguments.clone();
        let tool_name = call.name.clone();
        let timeout_ms = plugin.definition().timeout_ms;

        // Run the plugin's execute() inside spawn_blocking, wrapped with timeout.
        // Arc clone the plugin so the 'static spawn_blocking task owns it.
        let timeout_dur = Duration::from_millis(timeout_ms);
        let plugin = plugin.clone();
        let result = tokio::task::spawn_blocking(move || plugin.execute(args));

        match timeout(timeout_dur, result).await {
            Ok(Ok(r)) => self.finalize_result(r, &tool_name),
            Ok(Err(e)) => {
                log::warn!("Tool `{}` panicked: {e}", tool_name);
                ToolResult {
                    content: format!("Error: tool `{}` panicked: {e}", tool_name),
                    is_error: true,
                }
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
        if let Some(reason) = self.policy.check_tool(&call.name) {
            return CheckResult::Deny(reason);
        }
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

/// Register all built-in tools based on config. Called once at startup.
///
/// This replaces the duplicated if-else registration blocks that were
/// in both main.rs and server.rs.
pub fn register_builtins(registry: &mut ToolRegistry, config: &crate::types::Config) {
    if config.tools.shell {
        registry.register(Box::new(shell::ShellTool));
    }
    if config.tools.file_read {
        registry.register(Box::new(file::FileReadTool));
    }
    if config.tools.file_write {
        registry.register(Box::new(file::FileWriteTool));
    }
    if config.tools.file_search {
        registry.register(Box::new(file::FileSearchTool));
    }
    if config.tools.memory {
        let store = std::sync::Arc::new(crate::memory::MemoryStore::open(
            &config.memory.path,
            config.memory.max_entries,
        ));
        log::info!("Memory store: {} entries", store.len());
        registry.register(Box::new(memory::MemoryReadTool { store: store.clone() }));
        registry.register(Box::new(memory::MemoryWriteTool { store: store.clone() }));
        registry.register(Box::new(memory::MemoryRecallTool { store }));
    }
    if config.tools.ssh_exec {
        let ssh_tool = ssh::SshExecTool::new(config.ssh.targets.clone());
        if config.ssh.targets.is_empty() {
            log::info!("SSH tool registered (no pre-configured targets — pass host/user at call time)");
        } else {
            log::info!("SSH tool registered ({} targets: {})", config.ssh.targets.len(),
                config.ssh.targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "));
        }
        registry.register(Box::new(ssh_tool));
    }
}

/// Register the subagent tool, sharing all currently-registered tools with it.
///
/// Call this after `register_builtins()` so the subagent can access the same
/// tools as the parent. The subagent tool is registered under the name "subagent".
pub fn register_subagent(
    registry: &mut ToolRegistry,
    config: &crate::types::Config,
) {
    let subagent = crate::subagent::SubagentTool::new(
        crate::llm::LlmClient::new(&config.model),
        config.model.clone(),
        config.compaction.threshold,
        config.compaction.keep_recent_turns,
        config.skill.dir.clone(),
    );

    // Share all currently-registered tools with the subagent.
    let plugins = registry.plugins_arc();
    let shared_count = plugins.len();
    for plugin in &plugins {
        subagent.share_tool(Arc::clone(plugin));
    }

    registry.register(Box::new(subagent));
    log::info!("Registered subagent tool (shared {} tools)", shared_count);
}
