//! SSH tool: persistent interactive sessions to network element devices.
//!
//! Maintains a background SSH connection per device target. Each `execute()`
//! call sends a command over the persistent connection and reads the response,
//! keeping the session alive for subsequent commands — no reconnect overhead.
//!
//! Design:
//! - Connection pool: `HashMap<device_key, SshHandle>` behind a `Mutex`
//! - Reuse: if a connection to the same host+user exists, reuse it; if it's
//!   closed, reconnect
//! - Async bridge: russh is async; `execute()` is sync, so we run on an
//!   independent `current_thread` runtime (same pattern as SubagentTool)
//! - Auth: password-based (embedded devices typically use password auth)
//! - Server key: accepted unconditionally (embedded device, often self-signed)

use crate::types::{ToolDefinition, ToolResult, SshTarget};
use crate::tools::ToolPlugin;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Background SSH session manager — holds persistent connections per device.
///
/// One instance is shared (via `Arc`) across all tool invocations. The map
/// keys on `host:user` so different users to the same host get separate
/// sessions.
pub struct SshSessionManager {
    /// Active connections: key = `host:port:user`
    connections: Mutex<HashMap<String, SshConnection>>,
    /// Pre-configured targets from config (for name → target resolution)
    targets: Vec<SshTarget>,
}

/// One persistent SSH connection.
struct SshConnection {
    /// The russh client handle — kept alive between commands.
    handle: russh::client::Handle<SshClientHandler>,
    /// Host this connection is to (for logging).
    #[allow(dead_code)]
    host: String,
}

/// russh client handler — accepts all server keys (embedded device context).
struct SshClientHandler;

impl russh::client::Handler for SshClientHandler {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        // Accept all server keys — embedded network devices often use
        // self-signed or manufacturer keys. This is acceptable in a
        // controlled management network.
        std::future::ready(Ok(true))
    }
}

impl SshSessionManager {
    /// Create a new manager with pre-configured targets.
    pub fn new(targets: Vec<SshTarget>) -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            targets,
        }
    }

    /// Resolve a target by name from config, or build one from inline params.
    fn resolve_target(
        &self,
        target_name: Option<&str>,
        host: Option<&str>,
        user: Option<&str>,
        password: Option<&str>,
        port: Option<u16>,
    ) -> Result<SshTarget, String> {
        // If a target name is given, look it up in config.
        if let Some(name) = target_name {
            if let Some(t) = self.targets.iter().find(|t| t.name == name) {
                return Ok(t.clone());
            }
            return Err(format!("SSH target '{}' not found in config", name));
        }

        // Otherwise, build from inline params.
        let host = host.ok_or_else(|| "Missing 'host' parameter (or specify 'target')".to_string())?;
        let user = user.ok_or_else(|| "Missing 'user' parameter (or specify 'target')".to_string())?;
        Ok(SshTarget {
            name: format!("{}@{}", user, host),
            host: host.to_string(),
            port: port.unwrap_or(22),
            user: user.to_string(),
            password: password.unwrap_or("").to_string(),
        })
    }

    /// Execute a command on a device, reusing or creating a persistent connection.
    pub fn execute_command(
        &self,
        target: SshTarget,
        command: &str,
        timeout_secs: u64,
    ) -> Result<String, String> {
        let key = format!("{}:{}:{}", target.host, target.port, target.user);

        // Take the existing connection out of the map (if any) so we don't
        // hold the Mutex lock across the async runtime.
        let existing = {
            let mut conns = self.connections.lock().unwrap();
            conns.remove(&key)
        };

        // Check if the existing connection is still alive.
        let handle = match existing {
            Some(conn) if !conn.handle.is_closed() => {
                log::debug!("SSH: reusing connection to {}@{}", target.user, target.host);
                conn.handle
            }
            _ => {
                log::info!("SSH: connecting to {}@{}:{}", target.user, target.host, target.port);

                // Build a fresh current_thread runtime for the async SSH work.
                // Same pattern as SubagentTool — avoids deadlocking the main runtime.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("Failed to create SSH runtime: {}", e))?;

                let connect_result = runtime.block_on(async {
                    let config = Arc::new(russh::client::Config::default());
                    let addr = format!("{}:{}", target.host, target.port);

                    let mut handle = russh::client::connect(config, addr, SshClientHandler)
                        .await
                        .map_err(|e| format!("SSH connect failed: {}", e))?;

                    // Authenticate with password.
                    let auth_result = handle
                        .authenticate_password(&target.user, &target.password)
                        .await
                        .map_err(|e| format!("SSH auth failed: {}", e))?;

                    if !auth_result.success() {
                        return Err(format!("SSH authentication rejected for user '{}'", target.user));
                    }

                    Ok::<_, String>(handle)
                });

                match connect_result {
                    Ok(h) => {
                        log::info!("SSH: connected to {}@{}:{}", target.user, target.host, target.port);
                        h
                    }
                    Err(e) => return Err(e),
                }
            }
        };

        // Execute the command using the persistent connection.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("Failed to create SSH runtime: {}", e))?;

        let exec_result = runtime.block_on(async {
            // Open a new channel for this command.
            let channel = handle
                .channel_open_session()
                .await
                .map_err(|e| format!("SSH channel open failed: {}", e))?;

            // Execute the command.
            channel
                .exec(true, command)
                .await
                .map_err(|e| format!("SSH exec failed: {}", e))?;

            // Read output until EOF/Close.
            let mut output = String::new();
            let mut stderr_output = String::new();
            let mut exit_code: i32 = 0;
            let mut channel = channel;

            // Read with timeout.
            let read_result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                async {
                    loop {
                        match channel.wait().await {
                            Some(russh::ChannelMsg::Data { data }) => {
                                output.push_str(&String::from_utf8_lossy(&data));
                            }
                            Some(russh::ChannelMsg::ExtendedData { data, ext }) => {
                                // ext=1 is stderr
                                if ext == 1 {
                                    stderr_output.push_str(&String::from_utf8_lossy(&data));
                                } else {
                                    output.push_str(&String::from_utf8_lossy(&data));
                                }
                            }
                            Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                                exit_code = exit_status as i32;
                            }
                            Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => {
                                break;
                            }
                            _ => {}
                        }
                    }
                },
            )
            .await;

            if read_result.is_err() {
                return Err(format!("SSH command timed out after {}s", timeout_secs));
            }

            // Build the result string.
            let mut result = output;
            if !stderr_output.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str("[stderr]\n");
                result.push_str(&stderr_output);
            }
            if exit_code != 0 {
                result.push_str(&format!("\n[exit code: {}]", exit_code));
            }

            Ok(result)
        });

        // Put the connection back into the pool (if still alive).
        if !handle.is_closed() {
            self.connections.lock().unwrap().insert(key, SshConnection {
                handle,
                host: target.host.clone(),
            });
        }

        exec_result
    }
}

/// The ssh_exec tool plugin — persistent interactive SSH command execution.
pub struct SshExecTool {
    /// Shared session manager — holds persistent connections.
    manager: Arc<SshSessionManager>,
}

impl SshExecTool {
    pub fn new(targets: Vec<SshTarget>) -> Self {
        Self {
            manager: Arc::new(SshSessionManager::new(targets)),
        }
    }
}

impl std::fmt::Debug for SshExecTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshExecTool")
            .field("targets", &self.manager.targets.len())
            .finish()
    }
}

impl ToolPlugin for SshExecTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ssh_exec".into(),
            description: "Execute a command on a remote network element via persistent SSH session. The connection stays open between calls — subsequent commands reuse the same session. Use for interactive device queries (show commands, config retrieval, diagnostics).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command to execute on the remote device"
                    },
                    "target": {
                        "type": "string",
                        "description": "Name of a pre-configured SSH target from config [ssh.targets]. If specified, host/user/password are taken from config."
                    },
                    "host": {
                        "type": "string",
                        "description": "SSH host address (IP or hostname). Required if 'target' is not specified."
                    },
                    "user": {
                        "type": "string",
                        "description": "SSH username. Required if 'target' is not specified."
                    },
                    "password": {
                        "type": "string",
                        "description": "SSH password. Optional if 'target' is specified (uses config)."
                    },
                    "port": {
                        "type": "integer",
                        "description": "SSH port (default 22).",
                        "default": 22
                    }
                },
                "required": ["command"]
            }),
            timeout_ms: 60_000,
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

        let target_name = args.get("target").and_then(|t| t.as_str());
        let host = args.get("host").and_then(|h| h.as_str());
        let user = args.get("user").and_then(|u| u.as_str());
        let password = args.get("password").and_then(|p| p.as_str());
        let port = args.get("port").and_then(|p| p.as_u64()).map(|p| p as u16);

        // Resolve the target (from config name or inline params).
        let target = match self.manager.resolve_target(target_name, host, user, password, port) {
            Ok(t) => t,
            Err(e) => {
                return ToolResult {
                    content: format!("Error: {}", e),
                    is_error: true,
                }
            }
        };

        log::info!("ssh_exec: `{}` on {}@{}:{}", command, target.user, target.host, target.port);

        // Execute via persistent session (timeout 55s, slightly under tool timeout).
        match self.manager.execute_command(target, command, 55) {
            Ok(output) => {
                log::info!("ssh_exec: command completed ({} bytes)", output.len());
                ToolResult {
                    content: output,
                    is_error: false,
                }
            }
            Err(e) => {
                log::warn!("ssh_exec: failed — {}", e);
                ToolResult {
                    content: format!("SSH error: {}", e),
                    is_error: true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_target_by_name() {
        let manager = SshSessionManager::new(vec![SshTarget {
            name: "router1".into(),
            host: "10.0.0.1".into(),
            port: 22,
            user: "admin".into(),
            password: "pass".into(),
        }]);

        let target = manager.resolve_target(Some("router1"), None, None, None, None);
        assert!(target.is_ok());
        assert_eq!(target.unwrap().host, "10.0.0.1");
    }

    #[test]
    fn test_resolve_target_not_found() {
        let manager = SshSessionManager::new(vec![]);
        let target = manager.resolve_target(Some("nonexistent"), None, None, None, None);
        assert!(target.is_err());
    }

    #[test]
    fn test_resolve_target_inline() {
        let manager = SshSessionManager::new(vec![]);
        let target = manager.resolve_target(
            None,
            Some("192.168.1.1"),
            Some("admin"),
            Some("password"),
            Some(2222),
        );
        assert!(target.is_ok());
        let t = target.unwrap();
        assert_eq!(t.host, "192.168.1.1");
        assert_eq!(t.user, "admin");
        assert_eq!(t.port, 2222);
    }

    #[test]
    fn test_resolve_target_missing_host() {
        let manager = SshSessionManager::new(vec![]);
        let target = manager.resolve_target(None, None, Some("admin"), None, None);
        assert!(target.is_err());
    }

    #[test]
    fn test_resolve_target_default_port() {
        let manager = SshSessionManager::new(vec![]);
        let target = manager.resolve_target(
            None,
            Some("10.0.0.1"),
            Some("admin"),
            None,
            None,
        );
        assert!(target.is_ok());
        assert_eq!(target.unwrap().port, 22);
    }
}
