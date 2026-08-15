//! Memory tools: memory_read, memory_write, memory_recall.
//!
//! These provide LLM-accessible access to the long-term memory store.
//! Each tool is a `ToolPlugin` struct holding a shared `Arc<MemoryStore>`.

use crate::types::{ToolDefinition, ToolResult};
use crate::tools::ToolPlugin;
use crate::memory::MemoryStore;
use std::sync::Arc;
use serde_json;

/// `memory_read` tool plugin.
pub struct MemoryReadTool {
    pub store: Arc<MemoryStore>,
}

impl ToolPlugin for MemoryReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_read".into(),
            description: "Read a value from long-term memory by exact key.".into(),
            guidance: "Use memory_read to recall facts learned in earlier sessions (e.g. device configs, known issues).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The memory key to read" }
                },
                "required": ["key"]
            }),
            timeout_ms: 2000,
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
        match self.store.read(key) {
            Some(value) => ToolResult { content: value, is_error: false },
            None => ToolResult {
                content: format!("No memory entry for key: {key}"),
                is_error: false,
            },
        }
    }
}

/// `memory_write` tool plugin.
pub struct MemoryWriteTool {
    pub store: Arc<MemoryStore>,
}

impl ToolPlugin for MemoryWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_write".into(),
            description: "Write a key-value pair to long-term memory (persists across sessions).".into(),
            guidance: "Record durable facts (device IPs, known bugs, config templates) for future sessions.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The memory key" },
                    "value": { "type": "string", "description": "The value to store" },
                    "category": { "type": "string", "description": "Optional category tag" }
                },
                "required": ["key", "value"]
            }),
            timeout_ms: 2000,
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
        let category = args.get("category").and_then(|v| v.as_str());

        if key.is_empty() {
            return ToolResult {
                content: "Error: key is required".into(),
                is_error: true,
            };
        }

        self.store.write(key, value, category);
        ToolResult {
            content: format!("Stored: {key} = {value}"),
            is_error: false,
        }
    }
}

/// `memory_recall` tool plugin.
pub struct MemoryRecallTool {
    pub store: Arc<MemoryStore>,
}

impl ToolPlugin for MemoryRecallTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_recall".into(),
            description: "Search long-term memory by fuzzy key/value matching. Returns all matching entries.".into(),
            guidance: "Use recall when you do not know the exact key; it searches both keys and values.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query (substring match on key and value)" }
                },
                "required": ["query"]
            }),
            timeout_ms: 2000,
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let results = self.store.recall(query);

        if results.is_empty() {
            ToolResult {
                content: format!("No memory entries matching: {query}"),
                is_error: false,
            }
        } else {
            let formatted: Vec<String> = results
                .iter()
                .map(|e| {
                    let cat = e.category.as_deref().unwrap_or("uncategorized");
                    format!("[{}] {} = {}", cat, e.key, e.value)
                })
                .collect();
            ToolResult {
                content: format!("Found {} entries:\n{}", results.len(), formatted.join("\n")),
                is_error: false,
            }
        }
    }
}
