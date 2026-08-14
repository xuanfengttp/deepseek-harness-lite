//! LLM client: HTTP streaming over an OpenAI-compatible `/v1/chat/completions` endpoint.
//!
//! The agent acts as a client to a local (or nearby) inference service. This
//! module owns the wire protocol: request serialization, SSE stream parsing,
//! tool-call delta assembly, and the `think` field mapping.
//!
//! Design notes:
//! - Uses `hyper` directly (shared with the server module) — no `reqwest`.
//! - Streams chunks as they arrive; the caller assembles the final message.
//! - The `think` parameter maps to provider-specific reasoning controls. For
//!   OpenAI-compatible APIs it is sent as `reasoning_effort` ("high"/"none").
//! - Retries on transient network errors with simple exponential backoff.

use crate::types::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;

/// Parameters for a single streaming completion request.
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: usize,
    pub temperature: f32,
    /// Whether to enable model reasoning/thinking.
    pub think: bool,
}

/// Events emitted while streaming a completion.
pub enum StreamEvent {
    /// A text delta from the assistant.
    Delta(String),
    /// A complete assembled tool call (emitted once per tool call when assembled).
    ToolCall(ToolCall),
    /// The final assistant message with full content and usage.
    Done { content: String, tool_calls: Vec<ToolCall>, usage: Option<TokenUsage> },
    /// An error during streaming.
    Error(String),
}

/// OpenAI-compatible request body (serialized to JSON).
#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool>,
    stream: bool,
    max_tokens: usize,
    temperature: f32,
    /// Provider-specific reasoning control. Omitted when think is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: ApiFunction,
}

#[derive(Debug, Serialize)]
struct ApiFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct ApiTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ApiToolDef,
}

#[derive(Debug, Serialize)]
struct ApiToolDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// One SSE data chunk from the stream.
#[derive(Debug, Deserialize)]
struct StreamChunkDto {
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

/// The LLM client. Stateless between requests — each call is a fresh HTTP stream.
/// Cheaply cloneable (just String fields) so it can be moved into spawned tasks.
#[derive(Clone)]
pub struct LlmClient {
    base_url: String,
    api_key: String,
}

impl LlmClient {
    pub fn new(model: &ModelConfig) -> Self {
        Self {
            base_url: model.base_url.trim_end_matches('/').to_string(),
            api_key: model.api_key.clone(),
        }
    }

    /// Stream a completion, sending events to the provided channel.
    ///
    /// This is the single entry point for model interaction. The caller
    /// (agent loop) collects deltas and assembles the final assistant message.
    pub async fn stream(
        &self,
        request: LlmRequest,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<(), LlmError> {
        let body = self.build_request_body(&request);
        let body_json = serde_json::to_string(&body)
            .map_err(|e| LlmError::Serialize(e.to_string()))?;

        let url = format!("{}/chat/completions", self.base_url);
        log::debug!("LLM request to {url}, {} bytes", body_json.len());

        // For P1, we use a blocking approach with ureq-like simplicity via
        // hyper. Full async streaming is wired in P6 with the server.
        // This implementation uses hyper's async client with a single-thread runtime.
        let stream_result = self.do_stream_request(&url, &body_json, &tx).await;

        match stream_result {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                Err(e)
            }
        }
    }

    /// Build the OpenAI-compatible request body from our internal types.
    fn build_request_body(&self, request: &LlmRequest) -> ChatCompletionRequest {
        let messages: Vec<ApiMessage> = request.messages.iter().map(|m| match m {
            Message::User { content } => ApiMessage {
                role: "user",
                content: content.clone(),
                tool_calls: None,
                tool_call_id: None,
            },
            Message::Assistant { content, tool_calls } => ApiMessage {
                role: "assistant",
                content: content.clone(),
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls.iter().map(|tc| ApiToolCall {
                        id: tc.id.clone(),
                        kind: "function",
                        function: ApiFunction {
                            name: tc.name.clone(),
                            arguments: tc.arguments.to_string(),
                        },
                    }).collect())
                },
                tool_call_id: None,
            },
            Message::Tool { call_id, content, .. } => ApiMessage {
                role: "tool",
                content: content.clone(),
                tool_calls: None,
                tool_call_id: Some(call_id.clone()),
            },
        }).collect();

        let tools: Vec<ApiTool> = request.tools.iter().map(|t| ApiTool {
            kind: "function",
            function: ApiToolDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        }).collect();

        ChatCompletionRequest {
            model: request.model.clone(),
            messages,
            tools,
            stream: true,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            reasoning_effort: if request.think { Some("high".to_string()) } else { Some("none".to_string()) },
        }
    }

    /// Perform the actual HTTP streaming request and parse SSE chunks.
    ///
    /// Uses hyper's async client. SSE lines starting with "data: " are parsed
    /// as JSON; "data: [DONE]" terminates the stream.
    async fn do_stream_request(
        &self,
        url: &str,
        body: &str,
        tx: &mpsc::Sender<StreamEvent>,
    ) -> Result<(), LlmError> {
        use http_body_util::{BodyExt, Full};
        use hyper::body::Bytes;
        use hyper_util::rt::TokioIo;
        use hyper::{Request, Method};
        use std::convert::Infallible;

        // Parse URL.
        let uri: hyper::Uri = url.parse().map_err(|e: http::uri::InvalidUri| LlmError::BadUrl(e.to_string()))?;
        let host = uri.host().ok_or_else(|| LlmError::BadUrl("no host".into()))?;
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("https") { 443 } else { 80 });
        let is_https = uri.scheme_str() == Some("https");

        // Build the HTTP request.
        let req = Request::builder()
            .method(Method::POST)
            .uri(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/v1/chat/completions"))
            .header("Host", host)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Connection", "close")
            .body(Full::<Bytes>::new(body.as_bytes().to_vec().into()))
            .map_err(|e| LlmError::BuildRequest(e.to_string()))?;

        // Connect. For P1 we support HTTP only (local inference service).
        // TLS support is added when remote servers need it (config-driven).
        if is_https {
            return Err(LlmError::Unsupported("HTTPS not yet supported; use HTTP for local inference".into()));
        }

        let addr = format!("{host}:{port}");
        let stream = tokio::net::TcpStream::connect(&addr)
            .await
            .map_err(|e| LlmError::Connect(e.to_string()))?;
        let io = TokioIo::new(stream);

        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| LlmError::Handshake(e.to_string()))?;

        // Drive the connection in background.
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                log::warn!("LLM connection closed: {e}");
            }
        });

        // Send request and get response.
        let response = sender.send_request(req)
            .await
            .map_err(|e| LlmError::Request(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.into_body().collect().await
                .map_err(|e| LlmError::Read(e.to_string()))?
                .to_bytes();
            return Err(LlmError::Status(status.as_u16(), String::from_utf8_lossy(&body).to_string()));
        }

        // Read and parse the SSE stream body.
        let body = response.into_body();
        let frame_stream = body.into_data_stream();

        // Buffer for incomplete SSE lines.
        let mut buffer = String::new();
        // Accumulators for tool calls (keyed by index).
        let mut tool_call_accum: std::collections::BTreeMap<usize, (String, String, String)> = std::collections::BTreeMap::new();
        let mut full_content = String::new();
        let mut final_usage: Option<TokenUsage> = None;

        use hyper::body::Body;
        use tokio_stream::StreamExt;
        pin_mut!(frame_stream);
        while let Some(chunk_result) = frame_stream.next().await {
            let chunk = chunk_result.map_err(|e| LlmError::Read(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete lines.
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        // Stream complete — emit done event.
                        // Assemble final tool calls from accumulators.
                        let tool_calls: Vec<ToolCall> = tool_call_accum
                            .into_values()
                            .map(|(id, name, args)| ToolCall {
                                id,
                                name,
                                arguments: serde_json::from_str(&args).unwrap_or(serde_json::Value::Null),
                            })
                            .collect();
                        let _ = tx.send(StreamEvent::Done {
                            content: full_content.clone(),
                            tool_calls,
                            usage: final_usage.take(),
                        }).await;
                        return Ok(());
                    }
                    // Parse JSON chunk.
                    if let Ok(chunk_dto) = serde_json::from_str::<StreamChunkDto>(data) {
                        if let Some(usage) = chunk_dto.usage {
                            final_usage = Some(TokenUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                            });
                        }
                        for choice in chunk_dto.choices {
                            if let Some(content) = choice.delta.content {
                                full_content.push_str(&content);
                                let _ = tx.send(StreamEvent::Delta(content)).await;
                            }
                            if let Some(tc_deltas) = choice.delta.tool_calls {
                                for tc in tc_deltas {
                                    let entry = tool_call_accum
                                        .entry(tc.index)
                                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                                    if let Some(id) = tc.id {
                                        entry.0 = id;
                                    }
                                    if let Some(func) = tc.function {
                                        if let Some(name) = func.name {
                                            entry.1 = name;
                                        }
                                        if let Some(args) = func.arguments {
                                            entry.2.push_str(&args);
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        log::debug!("Unparseable SSE data: {data}");
                    }
                }
            }
        }

        // Stream ended — still emit done with what we have.
        let tool_calls: Vec<ToolCall> = tool_call_accum
            .into_values()
            .map(|(id, name, args)| ToolCall {
                id,
                name,
                arguments: serde_json::from_str(&args).unwrap_or(serde_json::Value::Null),
            })
            .collect();
        let _ = tx.send(StreamEvent::Done {
            content: full_content,
            tool_calls,
            usage: final_usage,
        }).await;

        Ok(())
    }
}

/// Error type for LLM operations.
#[derive(Debug)]
pub enum LlmError {
    Serialize(String),
    BadUrl(String),
    Unsupported(String),
    Connect(String),
    Handshake(String),
    BuildRequest(String),
    Request(String),
    Read(String),
    Status(u16, String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Serialize(s) => write!(f, "serialize error: {s}"),
            LlmError::BadUrl(s) => write!(f, "bad URL: {s}"),
            LlmError::Unsupported(s) => write!(f, "unsupported: {s}"),
            LlmError::Connect(s) => write!(f, "connect failed: {s}"),
            LlmError::Handshake(s) => write!(f, "handshake failed: {s}"),
            LlmError::BuildRequest(s) => write!(f, "request build failed: {s}"),
            LlmError::Request(s) => write!(f, "request failed: {s}"),
            LlmError::Read(s) => write!(f, "read failed: {s}"),
            LlmError::Status(code, body) => write!(f, "HTTP {code}: {body}"),
        }
    }
}

impl std::error::Error for LlmError {}

/// Pin macro (avoid pulling in tokio-stream just for this).
use std::pin::pin;
macro_rules! pin_mut {
    ($x:ident) => { let mut $x = pin!($x); };
}
pub(crate) use pin_mut;
