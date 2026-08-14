//! File tools: read, write, and search the local filesystem.
//!
//! Three tools exposed to the model:
//! - `file_read`: read a file's contents
//! - `file_write`: write content to a file
//! - `file_search`: list files matching a glob pattern
//!
//! All operations run synchronously inside the blocking executor wrapper.

use crate::types::ToolResult;
use crate::types::ToolDefinition;

/// `file_read` tool definition.
pub fn read_definition() -> ToolDefinition {
    ToolDefinition {
        name: "file_read".into(),
        description: "Read the contents of a file".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                }
            },
            "required": ["path"]
        }),
        timeout_ms: 10_000,
    }
}

/// `file_write` tool definition.
pub fn write_definition() -> ToolDefinition {
    ToolDefinition {
        name: "file_write".into(),
        description: "Write content to a file (overwrites if exists)".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write"
                }
            },
            "required": ["path", "content"]
        }),
        timeout_ms: 10_000,
    }
}

/// `file_search` tool definition.
pub fn search_definition() -> ToolDefinition {
    ToolDefinition {
        name: "file_search".into(),
        description: "List files matching a glob pattern (e.g. *.log, config/*.toml)".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files"
                },
                "directory": {
                    "type": "string",
                    "description": "Directory to search in (default: current directory)"
                }
            },
            "required": ["pattern"]
        }),
        timeout_ms: 15_000,
    }
}

/// Create the file_read executor.
pub fn make_read_executor() -> crate::tools::ToolExecutor {
    crate::tools::make_executor(|args: serde_json::Value, tx: tokio::sync::oneshot::Sender<ToolResult>| {
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
        if path.is_empty() {
            let _ = tx.send(ToolResult { content: "Error: `path` is required".into(), is_error: true });
            return;
        }
        log::info!("file_read: {path}");
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let _ = tx.send(ToolResult { content, is_error: false });
            }
            Err(e) => {
                let _ = tx.send(ToolResult { content: format!("Error: {e}"), is_error: true });
            }
        }
    })
}

/// Create the file_write executor.
pub fn make_write_executor() -> crate::tools::ToolExecutor {
    crate::tools::make_executor(|args: serde_json::Value, tx: tokio::sync::oneshot::Sender<ToolResult>| {
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let content = args.get("content").and_then(|c| c.as_str()).unwrap_or("");
        if path.is_empty() {
            let _ = tx.send(ToolResult { content: "Error: `path` is required".into(), is_error: true });
            return;
        }
        log::info!("file_write: {path} ({} bytes)", content.len());
        match std::fs::write(path, content) {
            Ok(()) => {
                let _ = tx.send(ToolResult { content: format!("Wrote {} bytes to {path}", content.len()), is_error: false });
            }
            Err(e) => {
                let _ = tx.send(ToolResult { content: format!("Error: {e}"), is_error: true });
            }
        }
    })
}

/// Create the file_search executor (simple glob via std::fs walk).
pub fn make_search_executor() -> crate::tools::ToolExecutor {
    crate::tools::make_executor(|args: serde_json::Value, tx: tokio::sync::oneshot::Sender<ToolResult>| {
        let pattern = args.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
        let dir = args.get("directory").and_then(|d| d.as_str()).unwrap_or(".");

        // Simple glob matching: support * and ? wildcards.
        let matches = simple_glob(dir, pattern);
        let content = if matches.is_empty() {
            format!("No files matching '{pattern}' in {dir}")
        } else {
            matches.join("\n")
        };
        let _ = tx.send(ToolResult { content, is_error: false });
    })
}

/// Simple recursive glob matcher (supports * and ? in the filename portion).
fn simple_glob(dir: &str, pattern: &str) -> Vec<String> {
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path().to_string_lossy().to_string();
            if glob_match(pattern, &name) {
                results.push(path);
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let subdir = format!("{dir}/{name}");
                results.extend(simple_glob(&subdir, pattern));
            }
        }
    }
    results.sort();
    results
}

/// Match a glob pattern (* and ?) against a string.
fn glob_match(pattern: &str, text: &str) -> bool {
    // Simple character-by-character glob with * and ? support.
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, 0, &t, 0)
}

fn glob_match_inner(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => {
            // * matches zero or more characters.
            for skip in ti..=t.len() {
                if glob_match_inner(p, pi + 1, t, skip) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if ti < t.len() {
                glob_match_inner(p, pi + 1, t, ti + 1)
            } else {
                false
            }
        }
        c => {
            if ti < t.len() && t[ti] == c {
                glob_match_inner(p, pi + 1, t, ti + 1)
            } else {
                false
            }
        }
    }
}
