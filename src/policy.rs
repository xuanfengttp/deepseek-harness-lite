//! Policy: minimal allow/deny permission checks for tool execution.
//!
//! Mirrors dsh `sandbox/sandbox-policy`, drastically simplified. No sandbox
//! executor — the device itself is the restricted environment. The policy just
//! gates which tools may run, configurable via the ToolsConfig.

use crate::types::ToolsConfig;

/// The permission policy. Checks whether a tool is allowed to execute.
pub struct Policy {
    /// Set of allowed tool names (derived from ToolsConfig at startup).
    allowed: std::collections::HashSet<String>,
}

impl Policy {
    /// Build the policy from the tools configuration.
    pub fn from_config(config: &ToolsConfig) -> Self {
        let mut allowed = std::collections::HashSet::new();
        if config.shell { allowed.insert("shell".into()); }
        if config.file_read { allowed.insert("file_read".into()); }
        if config.file_write { allowed.insert("file_write".into()); }
        if config.file_search { allowed.insert("file_search".into()); }
        if config.ssh_exec { allowed.insert("ssh_exec".into()); }
        if config.memory { allowed.insert("memory_read".into()); allowed.insert("memory_write".into()); allowed.insert("memory_recall".into()); }
        if config.todo { allowed.insert("todo_write".into()); }
        // Subagent is always allowed (registered separately via register_subagent).
        allowed.insert("subagent".into());
        Self { allowed }
    }

    /// Check a tool call. Returns `Some(reason)` if denied, `None` if allowed.
    pub fn check_tool(&self, name: &str) -> Option<String> {
        if self.allowed.contains(name) {
            None
        } else {
            Some(format!("tool `{name}` is not enabled in configuration"))
        }
    }
}
