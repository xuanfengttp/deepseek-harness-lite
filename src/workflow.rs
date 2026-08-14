//! Workflow runner: deterministic SOP step execution.
//!
//! In workflow mode, the dispatcher runs the skill's `steps` directly. This
//! module provides the step execution logic as a separate concern for clarity.
//! The dispatcher's `run_workflow` method calls into this module's helpers.
//!
//! Key design: workflow mode bypasses the agent loop entirely. No prompt
//! assembly, no message derivation, no multi-turn negotiation. Each step is
//! either a direct tool call or a single LLM judgment call. This minimizes
//! LLM usage and maximizes execution speed for deterministic device operations.

use crate::types::*;
use crate::tools::ToolRegistry;
use crate::llm::{LlmClient, LlmRequest, StreamEvent};
use std::collections::HashMap;
use tokio::sync::mpsc;

/// The result of executing one workflow step.
pub struct StepOutput {
    pub content: String,
    pub is_error: bool,
}

/// Execute a single tool step.
pub async fn execute_tool_step(
    tools: &ToolRegistry,
    step_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    step_results: &HashMap<String, String>,
    variables: &HashMap<String, String>,
    event_tx: &mpsc::Sender<crate::agent::LoopEvent>,
) -> StepOutput {
    let interpolated_args = interpolate_json(args, step_results, variables);
    let call = ToolCall {
        id: format!("wf_{step_id}"),
        name: tool_name.to_string(),
        arguments: interpolated_args,
    };

    log::info!("Workflow step `{step_id}`: tool `{tool_name}`");
    let _ = event_tx.send(crate::agent::LoopEvent::ToolCall { call: call.clone() }).await;

    let result = tools.execute(&call).await;

    let _ = event_tx.send(crate::agent::LoopEvent::ToolResult {
        call_id: call.id.clone(),
        content: result.content.clone(),
        is_error: result.is_error,
    }).await;

    StepOutput {
        content: result.content,
        is_error: result.is_error,
    }
}

/// Execute a single LLM judgment step.
///
/// Uses an independent context (just the prompt + input), not the session log.
/// This keeps the judgment focused and avoids context bloat.
pub async fn execute_judge_step(
    llm: &LlmClient,
    model: &str,
    max_tokens: usize,
    temperature: f32,
    think: bool,
    prompt: &str,
    input: &str,
    step_results: &HashMap<String, String>,
    variables: &HashMap<String, String>,
    event_tx: &mpsc::Sender<crate::agent::LoopEvent>,
) -> StepOutput {
    let interpolated_input = interpolate_str(input, step_results, variables);
    let interpolated_prompt = interpolate_str(prompt, step_results, variables);

    log::info!("Workflow llm_judge step");

    let judge_messages = vec![
        Message::User {
            content: format!("{interpolated_prompt}\n\n---\n\n{interpolated_input}"),
        },
    ];

    let request = LlmRequest {
        model: model.to_string(),
        messages: judge_messages,
        tools: vec![],
        max_tokens,
        temperature,
        think,
    };

    let (stream_tx, mut stream_rx) = mpsc::channel(64);
    let llm_clone = llm.clone();
    let handle = tokio::spawn(async move {
        llm_clone.stream(request, stream_tx).await
    });

    let mut judge_content = String::new();
    let mut had_error = false;

    while let Some(event) = stream_rx.recv().await {
        match event {
            StreamEvent::Delta(text) => {
                judge_content.push_str(&text);
                let _ = event_tx.send(crate::agent::LoopEvent::Delta { text }).await;
            }
            StreamEvent::Done { content, .. } => {
                if !content.is_empty() {
                    judge_content = content;
                }
            }
            StreamEvent::ToolCall(_) => {}
            StreamEvent::Error(msg) => {
                let _ = event_tx.send(crate::agent::LoopEvent::Error { message: msg }).await;
                had_error = true;
            }
        }
    }
    let _ = handle.await;

    let _ = event_tx.send(crate::agent::LoopEvent::AssistantMessage {
        content: judge_content.clone(),
        tool_calls: vec![],
    }).await;

    StepOutput {
        content: judge_content,
        is_error: had_error,
    }
}

/// Interpolate `{{steps.xxx.result}}` and `{{var}}` in a string.
fn interpolate_str(text: &str, step_results: &HashMap<String, String>, variables: &HashMap<String, String>) -> String {
    let mut result = text.to_string();
    for (step_id, value) in step_results {
        let placeholder = format!("{{{{steps.{step_id}.result}}}}");
        result = result.replace(&placeholder, value);
    }
    for (key, value) in variables {
        let placeholder = format!("{{{{{key}}}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

/// Interpolate in a JSON value recursively.
fn interpolate_json(value: &serde_json::Value, step_results: &HashMap<String, String>, variables: &HashMap<String, String>) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(interpolate_str(s, step_results, variables))
        }
        serde_json::Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (k, v) in map {
                new_map.insert(k.clone(), interpolate_json(v, step_results, variables));
            }
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|v| interpolate_json(v, step_results, variables)).collect())
        }
        other => other.clone(),
    }
}
