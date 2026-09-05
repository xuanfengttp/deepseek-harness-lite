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
use tokio::sync::mpsc;

/// Parameters for a single streaming completion request.
pub struct LlmRequest {
    pub model: String,
    /// System prompt (prepended to messages as a system-role message).
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: usize,
    pub temperature: f32,
    /// Reasoning/thinking effort level (Off = no reasoning).
    pub think: ThinkLevel,
}

/// Events emitted while streaming a completion.
pub enum StreamEvent {
    /// A text delta from the assistant.
    Delta(String),
    /// A thinking/reasoning delta from the model (reasoning_content field).
    ThinkDelta(String),
    /// A complete assembled tool call (emitted once per tool call when assembled).
    #[allow(dead_code)]
    ToolCall(ToolCall),
    /// The final assistant message with full content and usage.
    Done { content: String, tool_calls: Vec<ToolCall>, usage: Option<TokenUsage>, finish_reason: Option<String> },
    /// An error during streaming.
    Error(String),
}

/// OpenAI-compatible request body (serialized to JSON).
#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool>,
    stream: bool,
    /// Request usage stats in streaming responses (OpenAI stream_options).
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    max_tokens: usize,
    temperature: f32,
    /// Provider-specific reasoning control. Omitted when think is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: &'static str,
    /// Message content: either a plain string (text-only) or an array of
    /// content parts (when images are present). The OpenAI API accepts both
    /// forms under the `content` key.
    content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// DeepSeek thinking-mode passback: sent on every reasoning-carrying turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
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
    /// Reasoning/thinking content (DeepSeek, llama.cpp, and other OpenAI-compatible APIs).
    /// Some providers use `reasoning`, `reasoning_text` — we try all via serde aliases.
    #[serde(default, alias = "reasoning", alias = "reasoning_text")]
    reasoning_content: Option<String>,
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
    #[serde(default)]
    prompt_cache_hit_tokens: u64,
    #[serde(default)]
    prompt_cache_miss_tokens: u64,
    /// OpenAI-compatible alias: some gateways send prompt_tokens_details.cached_tokens
    /// instead of prompt_cache_hit_tokens. Fallback resolved in `to_token_usage`.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// Reasoning tokens from completion_tokens_details (thinking consumption).
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

/// The LLM client. Stateless between requests — each call is a fresh HTTP stream.
/// Cheaply cloneable (just String fields) so it can be moved into spawned tasks.
#[derive(Clone)]
pub struct LlmClient {
    base_url: String,
    api_key: String,
    /// HTTP proxy URL (e.g. "http://127.0.0.1:7890"). Empty = direct connection.
    proxy: String,
}

impl LlmClient {
    pub fn new(model: &ModelConfig) -> Self {
        Self {
            base_url: model.base_url.trim_end_matches('/').to_string(),
            api_key: model.api_key.clone(),
            proxy: model.proxy.clone(),
        }
    }

    /// Stream a completion, sending events to the provided channel.
    ///
    /// This is the single entry point for model interaction. The caller
    /// (agent loop) collects deltas and assembles the final assistant message.
    /// Retries on transient connection errors with exponential backoff.
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

        // Retry on transient connection errors (up to 3 attempts).
        // Backoff: 100ms, 400ms, 1600ms (exponential with base 4).
        let mut last_err = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                let backoff_ms = 100u64 * (4u64.pow(attempt - 1));
                log::warn!("LLM retry {attempt}/3 after {backoff_ms}ms: {}", last_err.as_ref().map(|e: &LlmError| e.to_string()).unwrap_or_default());
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            }

            match self.do_stream_request(&url, &body_json, &tx).await {
                Ok(()) => return Ok(()),
                Err(LlmError::Connect(e)) | Err(LlmError::Handshake(e)) => {
                    log::warn!("LLM transient error (attempt {}): {e}", attempt + 1);
                    last_err = Some(LlmError::Connect(e));
                    continue;
                }
                Err(e) => {
                    // Non-transient error — don't retry.
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return Err(e);
                }
            }
        }

        // All retries exhausted.
        let err = last_err.unwrap_or(LlmError::Connect("retry exhausted".into()));
        let _ = tx.send(StreamEvent::Error(err.to_string())).await;
        Err(err)
    }

    /// Generate a concise session title from the first user message.
    ///
    /// This is an auxiliary non-streaming LLM call (mirrors dsh's
    /// session-title-llm provider). Uses a specialized system prompt that
    /// asks for a one-line plain-text title in the message's language.
    /// Falls back to `Err` on any failure — the caller uses the deterministic
    /// fallback in that case.
    pub async fn generate_title(&self, model: &str, first_message: &str) -> Result<String, LlmError> {
        use http_body_util::{BodyExt, Full};
        use hyper::body::Bytes;
        use hyper_util::rt::TokioIo;
        use hyper::{Request, Method};

        // System prompt — adapted from dsh session-title-llm.
        let system = "Create a concise title for an AI assistant session from the supplied human message. \
Return only the title on one line, in plain text of natural language, with no quotes, \
prefix, explanation, Markdown, XML, or terminal control codes. No code is allowed. \
Use the language of the message. Aim for about 6 words in non-CJK languages or 12 CJK characters.";

        let user_content = format!("Generate the session title from this human message:\n{}", first_message);

        let body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user_content},
            ],
            "stream": false,
            "max_tokens": 64,
            "temperature": 0.0,
        });
        let body_json = serde_json::to_string(&body)
            .map_err(|e| LlmError::Serialize(e.to_string()))?;

        let url = format!("{}/chat/completions", self.base_url);
        let uri: hyper::Uri = url.parse().map_err(|e: http::uri::InvalidUri| LlmError::BadUrl(e.to_string()))?;
        let host = uri.host().ok_or_else(|| LlmError::BadUrl("no host".into()))?;
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("https") { 443 } else { 80 });
        let is_https = uri.scheme_str() == Some("https");

        let req = Request::builder()
            .method(Method::POST)
            .uri(uri.path_and_query().map(|p| p.as_str()).unwrap_or("/v1/chat/completions"))
            .header("Host", host)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Connection", "close")
            .body(Full::<Bytes>::new(body_json.as_bytes().to_vec().into()))
            .map_err(|e| LlmError::BuildRequest(e.to_string()))?;

        let stream = if is_https {
            return Err(LlmError::Connect("HTTPS not yet supported for title generation".into()));
        } else {
            tokio::net::TcpStream::connect((host, port)).await
                .map_err(|e| LlmError::Connect(e.to_string()))?
        };
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| LlmError::Handshake(e.to_string()))?;
        tokio::spawn(async move { let _ = conn.await; });

        let resp = sender.send_request(req).await
            .map_err(|e| LlmError::Request(e.to_string()))?;
        let body_bytes = resp.into_body().collect().await
            .map_err(|e| LlmError::Read(e.to_string()))?
            .to_bytes();
        let resp_text = String::from_utf8_lossy(&body_bytes);

        // Parse the non-streaming response.
        let resp_json: serde_json::Value = serde_json::from_str(&resp_text)
            .map_err(|e| LlmError::Read(format!("title response parse: {e}")))?;

        let title = resp_json
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|t| t.as_str())
            .unwrap_or("");

        // Normalize: strip control codes, collapse whitespace, trim.
        let title = title
            .replace(['\n', '\r'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();

        if title.is_empty() {
            Err(LlmError::Read("title model produced empty output".into()))
        } else {
            log::info!("LLM generated title: {title}");
            Ok(title)
        }
    }

    /// Build the OpenAI-compatible request body from our internal types.
    fn build_request_body(&self, request: &LlmRequest) -> ChatCompletionRequest {
        let mut messages: Vec<ApiMessage> = Vec::new();
        // Prepend system message if non-empty (skill persona + tool guidance).
        if !request.system.is_empty() {
            messages.push(ApiMessage {
                role: "system",
                content: serde_json::Value::String(request.system.clone()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        // Append conversation messages.
        for m in &request.messages {
            match m {
                Message::User { content, images } => {
                    if images.is_empty() {
                        messages.push(ApiMessage {
                            role: "user",
                            content: serde_json::Value::String(content.clone()),
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                        });
                    } else {
                        // Build OpenAI content parts: text + image_url blocks.
                        let mut parts: Vec<serde_json::Value> = Vec::new();
                        if !content.is_empty() {
                            parts.push(serde_json::json!({ "type": "text", "text": content }));
                        }
                        for img in images {
                            parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{};base64,{}", img.media_type, img.data) }
                            }));
                        }
                        messages.push(ApiMessage {
                            role: "user",
                            content: serde_json::Value::Array(parts),
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                        });
                    }
                }
                Message::Assistant { content, tool_calls, reasoning_content } => messages.push(ApiMessage {
                    role: "assistant",
                    content: serde_json::Value::String(content.clone()),
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
                    reasoning_content: reasoning_content.clone(),
                }),
                Message::Tool { call_id, content, images, .. } => {
                    // If tool result has images, use multi-part content (text + image_url).
                    let content_val = if images.is_empty() {
                        serde_json::Value::String(content.clone())
                    } else {
                        let mut parts = vec![serde_json::json!({
                            "type": "text",
                            "text": content
                        })];
                        for img in images {
                            parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", img.media_type, img.data)
                                }
                            }));
                        }
                        serde_json::Value::Array(parts)
                    };
                    messages.push(ApiMessage {
                        role: "tool",
                        content: content_val,
                        tool_calls: None,
                        tool_call_id: Some(call_id.clone()),
                        reasoning_content: None,
                    });
                }
            }
        }

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
            stream_options: Some(StreamOptions { include_usage: true }),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            reasoning_effort: request.think.as_effort_str().map(|s| s.to_string()),
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

        // Connect. Support both HTTP (local inference) and HTTPS (remote APIs).
        let addr = format!("{host}:{port}");

        // If proxy is configured, connect through proxy using HTTP CONNECT tunnel.
        // For HTTPS: connect to proxy → CONNECT host:port → TLS over tunnel.
        // For HTTP: connect to proxy → send request with full URL (not just path).
        if !self.proxy.is_empty() {
            return self.do_stream_request_via_proxy(url, body, tx, host, port, is_https, &addr).await;
        }

        // Get a SendRequest by connecting via TCP (HTTP) or TLS (HTTPS).
        // Both produce the same SendRequest<Full<Bytes>> type, so the rest
        // of the flow (send request, process SSE) is shared.
        let sender = if is_https {
            // TLS connection: load native root certs, connect TCP, handshake TLS.
            use tokio_rustls::TlsConnector;
            let mut roots = rustls::RootCertStore::empty();
            match rustls_native_certs::load_native_certs() {
                Ok(certs) => {
                    for cert in certs {
                        let _ = roots.add(cert);
                    }
                }
                Err(e) => {
                    log::warn!("Failed to load native certs: {e} — using rustls built-in roots");
                }
            }
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = TlsConnector::from(std::sync::Arc::new(config));
            let dns_name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|e| LlmError::BadUrl(format!("invalid TLS server name: {e}")))?;
            let tcp_stream = tokio::net::TcpStream::connect(&addr)
                .await
                .map_err(|e| LlmError::Connect(e.to_string()))?;
            let tls_stream = connector.connect(dns_name, tcp_stream)
                .await
                .map_err(|e| LlmError::Handshake(format!("TLS handshake: {e}")))?;
            let io = TokioIo::new(tls_stream);

            let (sender, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| LlmError::Handshake(e.to_string()))?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    log::warn!("LLM TLS connection closed: {e}");
                }
            });
            sender
        } else {
            let stream = tokio::net::TcpStream::connect(&addr)
                .await
                .map_err(|e| LlmError::Connect(e.to_string()))?;
            let io = TokioIo::new(stream);

            let (sender, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| LlmError::Handshake(e.to_string()))?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    log::warn!("LLM connection closed: {e}");
                }
            });
            sender
        };

        // Send request and get response.
        let mut sender = sender;
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
        let mut final_finish_reason: Option<String> = None;

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
                            finish_reason: final_finish_reason.take(),
                        }).await;
                        return Ok(());
                    }
                    // Parse JSON chunk.
                    if let Ok(chunk_dto) = serde_json::from_str::<StreamChunkDto>(data) {
                        if let Some(usage) = chunk_dto.usage {
                            // Resolve cache hit: prefer prompt_cache_hit_tokens,
                            // fallback to prompt_tokens_details.cached_tokens
                            // (OpenAI-compatible alias used by some gateways).
                            let cache_hit = if usage.prompt_cache_hit_tokens > 0 {
                                usage.prompt_cache_hit_tokens
                            } else {
                                usage.prompt_tokens_details
                                    .map(|d| d.cached_tokens)
                                    .unwrap_or(0)
                            };
                            // Reasoning tokens from completion_tokens_details.
                            let reasoning_tokens = usage
                                .completion_tokens_details
                                .map(|d| d.reasoning_tokens)
                                .unwrap_or(0);
                            final_usage = Some(TokenUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                                cache_hit_tokens: cache_hit,
                                cache_miss_tokens: usage.prompt_cache_miss_tokens,
                                reasoning_tokens,
                            });
                        }
                        for choice in chunk_dto.choices {
                            if let Some(content) = choice.delta.content {
                                full_content.push_str(&content);
                                let _ = tx.send(StreamEvent::Delta(content)).await;
                            }
                            // Thinking/reasoning content — emitted as ThinkDelta
                            // so the frontend can render it in a collapsible block.
                            if let Some(reasoning) = choice.delta.reasoning_content {
                                if !reasoning.is_empty() {
                                    let _ = tx.send(StreamEvent::ThinkDelta(reasoning)).await;
                                }
                            }
                            // Capture finish_reason for truncation detection.
                            if let Some(fr) = choice.finish_reason {
                                final_finish_reason = Some(fr);
                            }
                            if let Some(tc_deltas) = choice.delta.tool_calls {
                                for tc in tc_deltas {
                                    let entry = tool_call_accum
                                        .entry(tc.index)
                                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                                    if let Some(id) = tc.id {
                                        // Guard against empty-string identity erasure:
                                        // some OpenAI-compatible gateways send "" or
                                        // null on continuation deltas. null → None
                                        // (serde handles), but "" → Some("") must NOT
                                        // overwrite an established id. (upstream fix
                                        // a1271a4903 — acceptIdentity)
                                        if !id.is_empty() {
                                            entry.0 = id;
                                        }
                                    }
                                    if let Some(func) = tc.function {
                                        if let Some(name) = func.name {
                                            if !name.is_empty() {
                                                entry.1 = name;
                                            }
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
            finish_reason: final_finish_reason,
        }).await;

        Ok(())
    }

    /// Stream request via HTTP proxy (CONNECT tunnel for HTTPS, direct proxy for HTTP).
    async fn do_stream_request_via_proxy(
        &self,
        url: &str,
        body: &str,
        tx: &mpsc::Sender<StreamEvent>,
        host: &str,
        port: u16,
        is_https: bool,
        target_addr: &str,
    ) -> Result<(), LlmError> {
        use http_body_util::Full;
        use hyper::body::Bytes;
        use hyper_util::rt::TokioIo;
        use hyper::{Request, Method};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Parse proxy URL.
        let proxy_uri: hyper::Uri = self.proxy.parse()
            .map_err(|e: http::uri::InvalidUri| LlmError::BadUrl(format!("invalid proxy URL: {e}")))?;
        let proxy_host = proxy_uri.host().ok_or_else(|| LlmError::BadUrl("proxy URL has no host".into()))?;
        let proxy_port = proxy_uri.port_u16().unwrap_or(8080);
        let proxy_addr = format!("{proxy_host}:{proxy_port}");

        log::debug!("Connecting via proxy {proxy_addr} to {target_addr}");

        // Connect to proxy.
        let mut tcp_stream = tokio::net::TcpStream::connect(&proxy_addr)
            .await
            .map_err(|e| LlmError::Connect(format!("proxy connect {proxy_addr}: {e}")))?;

        if is_https {
            // HTTPS via proxy: CONNECT tunnel + TLS.
            // Step 1: Send CONNECT request on raw TCP.
            let connect_req = format!(
                "CONNECT {target_addr} HTTP/1.1\r\nHost: {target_addr}\r\n\r\n"
            );
            tcp_stream.write_all(connect_req.as_bytes()).await
                .map_err(|e| LlmError::Connect(format!("proxy CONNECT write: {e}")))?;

            // Step 2: Read CONNECT response (expect 200).
            let mut response_buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = tcp_stream.read(&mut byte).await
                    .map_err(|e| LlmError::Connect(format!("proxy CONNECT read: {e}")))?;
                if n == 0 { break; }
                response_buf.push(byte[0]);
                if response_buf.ends_with(b"\r\n\r\n") { break; }
                if response_buf.len() > 4096 {
                    return Err(LlmError::Connect("proxy CONNECT response too large".into()));
                }
            }
            let response_str = String::from_utf8_lossy(&response_buf);
            if !response_str.starts_with("HTTP/1.1 200") && !response_str.starts_with("HTTP/1.0 200") {
                return Err(LlmError::Connect(format!("proxy CONNECT failed: {}", response_str.lines().next().unwrap_or("empty"))));
            }
            log::debug!("Proxy CONNECT tunnel established to {target_addr}");

            // Step 3: TLS handshake over the tunnel.
            use tokio_rustls::TlsConnector;
            let mut roots = rustls::RootCertStore::empty();
            if let Ok(certs) = rustls_native_certs::load_native_certs() {
                for cert in certs { let _ = roots.add(cert); }
            }
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = TlsConnector::from(std::sync::Arc::new(config));
            let dns_name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|e| LlmError::BadUrl(format!("invalid TLS server name: {e}")))?;

            let tls_stream = connector.connect(dns_name, tcp_stream)
                .await
                .map_err(|e| LlmError::Handshake(format!("TLS over proxy: {e}")))?;
            let io = TokioIo::new(tls_stream);

            let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| LlmError::Handshake(e.to_string()))?;
            tokio::spawn(async move {
                if let Err(e) = conn.await { log::warn!("Proxy TLS connection closed: {e}"); }
            });

            // Rebuild request for tunnel (use path only, not full URL).
            let req = Request::builder()
                .method(Method::POST)
                .uri(format!("/v1/chat/completions"))
                .header("Host", host)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Connection", "close")
                .body(Full::<Bytes>::new(body.as_bytes().to_vec().into()))
                .map_err(|e| LlmError::BuildRequest(e.to_string()))?;

            let response = sender.send_request(req)
                .await
                .map_err(|e| LlmError::Request(e.to_string()))?;
            return self.process_sse(response, tx).await;
        } else {
            // HTTP via proxy: send request with full URL as target.
            let req = Request::builder()
                .method(Method::POST)
                .uri(url)  // Full URL for HTTP proxy
                .header("Host", host)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Connection", "close")
                .body(Full::<Bytes>::new(body.as_bytes().to_vec().into()))
                .map_err(|e| LlmError::BuildRequest(e.to_string()))?;

            let io = TokioIo::new(tcp_stream);
            let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| LlmError::Handshake(e.to_string()))?;
            tokio::spawn(async move {
                if let Err(e) = conn.await { log::warn!("Proxy HTTP connection closed: {e}"); }
            });

            let response = sender.send_request(req)
                .await
                .map_err(|e| LlmError::Request(e.to_string()))?;
            return self.process_sse(response, tx).await;
        }
    }

    /// Process SSE response — shared by direct and proxy paths.
    async fn process_sse(
        &self,
        response: hyper::Response<hyper::body::Incoming>,
        tx: &mpsc::Sender<StreamEvent>,
    ) -> Result<(), LlmError> {
        use http_body_util::BodyExt;
        let status = response.status();
        if !status.is_success() {
            let body = response.into_body().collect().await
                .map_err(|e| LlmError::Read(e.to_string()))?
                .to_bytes();
            return Err(LlmError::Status(status.as_u16(), String::from_utf8_lossy(&body).to_string()));
        }

        let body = response.into_body();
        let frame_stream = body.into_data_stream();
        let mut buffer = String::new();
        let mut tool_call_accum: std::collections::BTreeMap<usize, (String, String, String)> = std::collections::BTreeMap::new();
        let mut full_content = String::new();
        let mut final_usage: Option<TokenUsage> = None;
        let mut final_finish_reason: Option<String> = None;

        use tokio_stream::StreamExt;
        pin_mut!(frame_stream);
        while let Some(chunk_result) = frame_stream.next().await {
            let chunk = chunk_result.map_err(|e| LlmError::Read(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.is_empty() || line.starts_with(':') { continue; }
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        let tool_calls: Vec<ToolCall> = tool_call_accum.into_values()
                            .map(|(id, name, args)| ToolCall {
                                id, name,
                                arguments: serde_json::from_str(&args).unwrap_or(serde_json::Value::Null),
                            }).collect();
                        let _ = tx.send(StreamEvent::Done {
                            content: full_content.clone(),
                            tool_calls,
                            usage: final_usage.take(),
                            finish_reason: final_finish_reason.take(),
                        }).await;
                        return Ok(());
                    }
                    if let Ok(chunk_dto) = serde_json::from_str::<StreamChunkDto>(data) {
                        if let Some(usage) = chunk_dto.usage {
                            let cache_hit = if usage.prompt_cache_hit_tokens > 0 {
                                usage.prompt_cache_hit_tokens
                            } else {
                                usage.prompt_tokens_details.map(|d| d.cached_tokens).unwrap_or(0)
                            };
                            let reasoning_tokens = usage.completion_tokens_details.map(|d| d.reasoning_tokens).unwrap_or(0);
                            final_usage = Some(TokenUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                                cache_hit_tokens: cache_hit,
                                cache_miss_tokens: usage.prompt_cache_miss_tokens,
                                reasoning_tokens,
                            });
                        }
                        for choice in chunk_dto.choices {
                            if let Some(content) = choice.delta.content {
                                full_content.push_str(&content);
                                let _ = tx.send(StreamEvent::Delta(content)).await;
                            }
                            if let Some(reasoning) = choice.delta.reasoning_content {
                                if !reasoning.is_empty() {
                                    let _ = tx.send(StreamEvent::ThinkDelta(reasoning)).await;
                                }
                            }
                            if let Some(fr) = choice.finish_reason {
                                final_finish_reason = Some(fr);
                            }
                            if let Some(tc_deltas) = choice.delta.tool_calls {
                                for tc in tc_deltas {
                                    let entry = tool_call_accum
                                        .entry(tc.index)
                                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                                    if let Some(id) = tc.id {
                                        if !id.is_empty() { entry.0 = id; }
                                    }
                                    if let Some(func) = tc.function {
                                        if let Some(name) = func.name {
                                            if !name.is_empty() { entry.1 = name; }
                                        }
                                        if let Some(args) = func.arguments {
                                            entry.2.push_str(&args);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let tool_calls: Vec<ToolCall> = tool_call_accum.into_values()
            .map(|(id, name, args)| ToolCall {
                id, name,
                arguments: serde_json::from_str(&args).unwrap_or(serde_json::Value::Null),
            }).collect();
        let _ = tx.send(StreamEvent::Done {
            content: full_content,
            tool_calls,
            usage: final_usage,
            finish_reason: final_finish_reason,
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
