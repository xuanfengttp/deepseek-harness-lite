//! Shell tool: execute a command and return stdout+stderr.
//!
//! Platform-aware: uses `sh -c` on Unix, `cmd /c` on Windows. Runs in a
//! blocking thread via `spawn_blocking` to avoid blocking the async reactor.
//! Output is captured and truncated by the registry's spill stage.

use crate::types::ToolResult;
use crate::tools::make_executor;
use crate::types::ToolDefinition;
use serde_json;

/// The shell tool definition.
pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "shell".into(),
        description: "Execute a shell command on the device and return its output".into(),
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

/// Create the shell tool executor.
pub fn make_executor_fn() -> crate::tools::ToolExecutor {
    make_executor(|args: serde_json::Value, tx: tokio::sync::oneshot::Sender<ToolResult>| {
        let command = args.get("command")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        if command.is_empty() {
            let _ = tx.send(ToolResult {
                content: "Error: `command` parameter is required and must be non-empty".into(),
                is_error: true,
            });
            return;
        }

        log::info!("shell: executing `{command}`");

        // Platform-specific shell invocation.
        let (program, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };

        let output = std::process::Command::new(program)
            .arg(flag)
            .arg(command)
            .output();

        let result = match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let exit_code = out.status.code().unwrap_or(-1);

                let mut content = stdout;
                if !stderr.is_empty() {
                    if !content.is_empty() {
                        content.push_str("\n");
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
        };

        let _ = tx.send(result);
    })
}
