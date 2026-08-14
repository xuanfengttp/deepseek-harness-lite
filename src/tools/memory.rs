//! Memory tools: memory_read, memory_write, memory_recall.
//!
//! These provide LLM-accessible access to the long-term memory store.
//! The store itself lives in `MemoryStore` (shared via Arc), and these
//! tool executors wrap it with the standard ToolExecutor signature.

use crate::types::{ToolDefinition, ToolResult};
use crate::tools::ToolExecutor;
use crate::memory::MemoryStore;
use std::sync::Arc;

/// Tool definition for memory_read.
pub fn read_definition() -> ToolDefinition {
    ToolDefinition {
        name: "memory_read".into(),
        description: "Read a value from long-term memory by exact key.".into(),
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

/// Tool definition for memory_write.
pub fn write_definition() -> ToolDefinition {
    ToolDefinition {
        name: "memory_write".into(),
        description: "Write a key-value pair to long-term memory (persists across sessions).".into(),
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

/// Tool definition for memory_recall.
pub fn recall_definition() -> ToolDefinition {
    ToolDefinition {
        name: "memory_recall".into(),
        description: "Search long-term memory by fuzzy key/value matching. Returns all matching entries.".into(),
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

/// Create the memory_read executor. Needs a shared MemoryStore handle.
pub fn make_read_executor(store: Arc<MemoryStore>) -> ToolExecutor {
    crate::tools::make_executor(move |args: serde_json::Value, tx: tokio::sync::oneshot::Sender<ToolResult>| {
        let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
        match store.read(key) {
            Some(value) => {
                let _ = tx.send(ToolResult { content: value, is_error: false });
            }
            None => {
                let _ = tx.send(ToolResult {
                    content: format!("No memory entry for key: {key}"),
                    is_error: false, // not an error, just empty
                });
            }
        }
    })
}

/// Create the memory_write executor.
pub fn make_write_executor(store: Arc<MemoryStore>) -> ToolExecutor {
    crate::tools::make_executor(move |args: serde_json::Value, tx: tokio::sync::oneshot::Sender<ToolResult>| {
        let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
        let category = args.get("category").and_then(|v| v.as_str());

        if key.is_empty() {
            let _ = tx.send(ToolResult {
                content: "Error: key is required".into(),
                is_error: true,
            });
            return;
        }

        store.write(key, value, category);
        let _ = tx.send(ToolResult {
            content: format!("Stored: {key} = {value}"),
            is_error: false,
        });
    })
}

/// Create the memory_recall executor.
pub fn make_recall_executor(store: Arc<MemoryStore>) -> ToolExecutor {
    crate::tools::make_executor(move |args: serde_json::Value, tx: tokio::sync::oneshot::Sender<ToolResult>| {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let results = store.recall(query);

        if results.is_empty() {
            let _ = tx.send(ToolResult {
                content: format!("No memory entries matching: {query}"),
                is_error: false,
            });
        } else {
            let formatted: Vec<String> = results.iter()
                .map(|e| {
                    let cat = e.category.as_deref().unwrap_or("uncategorized");
                    format!("[{}] {} = {}", cat, e.key, e.value)
                })
                .collect();
            let _ = tx.send(ToolResult {
                content: format!("Found {} entries:\n{}", results.len(), formatted.join("\n")),
                is_error: false,
            });
        }
    })
}
