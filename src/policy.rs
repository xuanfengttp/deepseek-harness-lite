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
    ///
    /// Permission presets (mirror upstream dsh):
    /// - `read-only`           — view-only; write-capable tools (shell, file_write,
    ///   ssh_exec, memory_write, todo_write) are denied regardless of their bool flags.
    /// - `workspace-write`     — default; all enabled tools allowed.
    /// - `danger-full-access`  — all enabled tools allowed (confirmation handled by UI).
    pub fn from_config(config: &ToolsConfig) -> Self {
        let mut allowed = std::collections::HashSet::new();
        // Base tools (read-only safe)
        if config.file_read { allowed.insert("file_read".into()); }
        if config.file_search { allowed.insert("file_search".into()); }
        if config.memory { allowed.insert("memory_read".into()); allowed.insert("memory_recall".into()); }
        // Write-capable tools — gated by permission level
        let write_allowed = config.permission != "read-only";
        if write_allowed {
            if config.shell { allowed.insert("shell".into()); }
            if config.file_write { allowed.insert("file_write".into()); }
            if config.ssh_exec { allowed.insert("ssh_exec".into()); }
            if config.memory { allowed.insert("memory_write".into()); }
            if config.todo { allowed.insert("todo_write".into()); }
        }
        // Subagent and workflow are always allowed (registered separately).
        allowed.insert("subagent".into());
        allowed.insert("workflow".into());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(permission: &str) -> ToolsConfig {
        ToolsConfig {
            shell: true,
            file_read: true,
            file_write: true,
            file_search: true,
            ssh_exec: true,
            memory: true,
            todo: true,
            permission: permission.into(),
        }
    }

    #[test]
    fn read_only_denies_write_tools() {
        let p = Policy::from_config(&cfg("read-only"));
        // Read-only safe tools remain allowed.
        for t in ["file_read", "file_search", "memory_read", "memory_recall", "subagent", "workflow"] {
            assert!(p.check_tool(t).is_none(), "read-only should allow {t}");
        }
        // Write-capable tools are denied even when their bool flags are enabled.
        for t in ["shell", "file_write", "ssh_exec", "memory_write", "todo_write"] {
            assert!(p.check_tool(t).is_some(), "read-only should deny {t}");
        }
    }

    #[test]
    fn write_levels_allow_all_enabled() {
        for level in ["workspace-write", "danger-full-access"] {
            let p = Policy::from_config(&cfg(level));
            for t in [
                "shell", "file_read", "file_write", "file_search", "ssh_exec",
                "memory_read", "memory_write", "memory_recall", "todo_write",
                "subagent", "workflow",
            ] {
                assert!(p.check_tool(t).is_none(), "{level} should allow {t}");
            }
        }
    }
}
