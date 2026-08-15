//! Shell tool: execute a command and return stdout+stderr.
//!
//! Platform-aware: uses `sh -c` on Unix, `cmd /c` on Windows.

use crate::types::{ToolDefinition, ToolResult};
use crate::tools::ToolPlugin;
use serde_json;

/// The shell tool plugin.
pub struct ShellTool;

impl ToolPlugin for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".into(),
            description: "Execute a shell command on the device and return its output".into(),
            guidance: "Check the [exit code: N] marker on every result; investigate failures before moving on.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                },
                "required": ["command"]
            }),
            timeout_ms: 30_000,
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let command = args
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        if command.is_empty() {
            return ToolResult {
                content: "Error: `command` parameter is required and must be non-empty".into(),
                is_error: true,
            };
        }

        log::info!("shell: executing `{command}`");

        let (program, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };

        let output = std::process::Command::new(program)
            .arg(flag)
            .arg(command)
            .output();

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let exit_code = out.status.code().unwrap_or(-1);

                let mut content = stdout;
                if !stderr.is_empty() {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str("[stderr]\n");
                    content.push_str(&stderr);
                }
                if exit_code != 0 {
                    content.push_str(&format!("\n[exit code: {exit_code}]"));
                }
                ToolResult {
                    content,
                    is_error: !out.status.success(),
                }
            }
            Err(e) => ToolResult {
                content: format!("Error: failed to execute command: {e}"),
                is_error: true,
            },
        }
    }
}
