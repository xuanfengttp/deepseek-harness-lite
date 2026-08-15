//! File tools: read, write, and search the local filesystem.
//!
//! Three tool plugins:
//! - `FileReadTool`: read a file's contents
//! - `FileWriteTool`: write content to a file
//! - `FileSearchTool`: list files matching a glob pattern

use crate::types::{ToolDefinition, ToolResult};
use crate::tools::ToolPlugin;
use serde_json;

/// `file_read` tool plugin.
pub struct FileReadTool;

impl ToolPlugin for FileReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_read".into(),
            description: "Read the contents of a file".into(),
            guidance: "Use this, not shell cat, to read text files. Results include line numbers.".into(),
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

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
        if path.is_empty() {
            return ToolResult {
                content: "Error: `path` is required".into(),
                is_error: true,
            };
        }
        log::info!("file_read: {path}");
        match std::fs::read_to_string(path) {
            Ok(content) => ToolResult { content, is_error: false },
            Err(e) => ToolResult {
                content: format!("Error: {e}"),
                is_error: true,
            },
        }
    }
}

/// `file_write` tool plugin.
pub struct FileWriteTool;

impl ToolPlugin for FileWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_write".into(),
            description: "Write content to a file (overwrites if exists)".into(),
            guidance: "Read the file first before editing to avoid overwriting unknown content.".into(),
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

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let content = args.get("content").and_then(|c| c.as_str()).unwrap_or("");
        if path.is_empty() {
            return ToolResult {
                content: "Error: `path` is required".into(),
                is_error: true,
            };
        }
        log::info!("file_write: {path} ({} bytes)", content.len());
        match std::fs::write(path, content) {
            Ok(()) => ToolResult {
                content: format!("Wrote {} bytes to {path}", content.len()),
                is_error: false,
            },
            Err(e) => ToolResult {
                content: format!("Error: {e}"),
                is_error: true,
            },
        }
    }
}

/// `file_search` tool plugin.
pub struct FileSearchTool;

impl ToolPlugin for FileSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_search".into(),
            description: "List files matching a glob pattern (e.g. *.log, config/*.yaml)".into(),
            guidance: "Use glob patterns to discover files; prefer specific patterns over broad ones.".into(),
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

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let pattern = args.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
        let dir = args.get("directory").and_then(|d| d.as_str()).unwrap_or(".");

        let matches = simple_glob(dir, pattern);
        let content = if matches.is_empty() {
            format!("No files matching '{pattern}' in {dir}")
        } else {
            matches.join("\n")
        };
        ToolResult { content, is_error: false }
    }
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
