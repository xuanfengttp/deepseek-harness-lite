//! HTTP server: serves the web client + API endpoints.
//!
//! Uses hyper directly (no axum) to minimize binary size and memory.
//! The web client is embedded at compile time via include_str!.
//!
//! Endpoints:
//! - GET  /            → web client HTML
//! - POST /api/chat    → SSE stream of LoopEvents for one turn
//! - POST /api/command → execute a slash command locally (no LLM)
//! - GET  /api/sessions → list all sessions (JSON)
//! - POST /api/sessions/create → create a new session
//! - POST /api/sessions/switch → switch active session
//! - GET  /api/skills   → list loaded skills
//! - GET  /api/health   → health check
//! - GET  /api/models   → fetch model list from LLM endpoint
//! - GET  /api/config   → get config (JSON)
//! - POST /api/config   → merge-save config (JSON → TOML)
//! - GET  /api/config/raw → get raw TOML config file
//! - POST /api/config/raw → save raw TOML config file
//! - GET  /api/config/path → get current config file path
//! - POST /api/config/path → set config file path (applies on restart)

use crate::types::*;
use crate::agent::LoopEvent;
use crate::dispatcher::DispatchResult;
use std::sync::Arc;
use tokio::sync::Mutex;
use http_body_util::{BodyExt, Full, StreamBody};
use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;

/// The web client HTML, embedded at compile time.
const WEB_CLIENT_HTML: &str = include_str!("../web/index.html");

/// Shared state for the HTTP server.
pub struct ServerState {
    pub session_mgr: Arc<Mutex<crate::session_manager::SessionManager>>,
    pub skills: Vec<Skill>,
    pub active_skill_name: Arc<Mutex<String>>,
    pub config: Config,
}

/// Start the HTTP server. Blocks until the server is shut down.
pub async fn run(
    listen_addr: &str,
    state: Arc<ServerState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = listen_addr.parse()
        .map_err(|e| format!("invalid listen address {listen_addr}: {e}"))?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("HTTP server listening on http://{addr}");

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();

        tokio::spawn(async move {
            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| {
                    let state = state.clone();
                    async move { handle_request(req, state).await }
                }))
                .await
            {
                log::debug!("Connection closed: {e}");
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    state: Arc<ServerState>,
) -> Result<Response<BoxBody<Bytes, Infallible>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    log::debug!("{} {path}", method);

    let response = match (method, path.as_str()) {
        // Web client
        (Method::GET, "/") | (Method::GET, "/index.html") => {
            serve_html(WEB_CLIENT_HTML)
        }

        // Health check
        (Method::GET, "/api/health") => {
            serve_json(r#"{"status":"ok"}"#)
        }

        // List sessions
        (Method::GET, "/api/sessions") => {
            let mgr = state.session_mgr.lock().await;
            let sessions = mgr.list();
            let json = serde_json::to_string(&sessions).unwrap_or_else(|_| "[]".into());
            serve_json(&json)
        }

        // Create session
        (Method::POST, "/api/sessions/create") => {
            let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
            let title = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_else(|| "New session".into());

            let mut mgr = state.session_mgr.lock().await;
            let id = mgr.create(&title);
            serve_json(&format!(r#"{{"id":"{id}","title":"{title}"}}"#))
        }

        // Switch session
        (Method::POST, "/api/sessions/switch") => {
            let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
            let id = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("id").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_default();

            let mut mgr = state.session_mgr.lock().await;
            match mgr.switch(&id) {
                Some(sid) => serve_json(&format!(r#"{{"id":"{sid}","ok":true}}"#)),
                None => serve_json(r#"{"ok":false,"error":"session not found"}"#),
            }
        }

        // Rename session
        (Method::POST, "/api/sessions/rename") => {
            let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
            let (id, title) = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    let id = v.get("id").and_then(|t| t.as_str()).map(String::from)?;
                    let title = v.get("title").and_then(|t| t.as_str()).map(String::from)?;
                    Some((id, title))
                })
                .unwrap_or_default();

            if id.is_empty() || title.is_empty() {
                return Ok(serve_json(r#"{"ok":false,"error":"id and title required"}"#));
            }
            let mut mgr = state.session_mgr.lock().await;
            mgr.rename(&id, &title);
            serve_json(r#"{"ok":true}"#)
        }

        // Generate a session title via LLM (auxiliary call).
        // Body: {message: "first user message"}  Response: {title: "..."}
        (Method::POST, "/api/sessions/title") => {
            let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
            let message = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or_default();

            if message.is_empty() {
                return Ok(serve_json(r#"{"error":"message is required"}"#));
            }

            let config = crate::load_config_file().unwrap_or_else(|| state.config.clone());
            let llm = crate::llm::LlmClient::new(&config.model);

            match llm.generate_title(&config.model.model, &message).await {
                Ok(title) => serve_json(&format!(r#"{{"title":{:?}}}"#, title)),
                Err(e) => {
                    log::warn!("Title generation failed: {e}");
                    serve_json(r#"{"error":"title generation failed"}"#)
                }
            }
        }

        // Delete a session
        (Method::POST, "/api/sessions/delete") => {
            let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
            let id = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("id").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_default();

            if id.is_empty() {
                return Ok(serve_json(r#"{"ok":false,"error":"id required"}"#));
            }
            let mut mgr = state.session_mgr.lock().await;
            mgr.delete(&id);
            serve_json(r#"{"ok":true}"#)
        }

        // Get session history (messages + stats + trajectory for display)
        (Method::GET, "/api/sessions/history") => {
            // Parse id from query string
            let uri = req.uri().to_string();
            let id = uri.split("id=")
                .nth(1)
                .and_then(|s| s.split('&').next())
                .unwrap_or("");
            if id.is_empty() {
                return Ok(serve_json(r#"{"messages":[],"stats":null,"trajectory":[]}"#));
            }
            let mut mgr = state.session_mgr.lock().await;
            // Always switch to the requested session so active_messages()
            // returns the correct session's history.
            mgr.switch(id);

            // Build messages for chat display
            let messages: Vec<serde_json::Value> = mgr.active_messages()
                .unwrap_or_default()
                .into_iter()
                .map(|m| match m {
                    crate::types::Message::User { content } => serde_json::json!({
                        "role": "user", "content": content
                    }),
                    crate::types::Message::Assistant { content, tool_calls } => {
                        let tc_json: Vec<serde_json::Value> = tool_calls.iter().map(|tc| {
                            serde_json::json!({
                                "id": tc.id,
                                "name": tc.name,
                                "arguments": tc.arguments
                            })
                        }).collect();
                        serde_json::json!({
                            "role": "assistant", "content": content, "tool_calls": tc_json
                        })
                    }
                    crate::types::Message::Tool { call_id, content, is_error } => serde_json::json!({
                        "role": "tool", "call_id": call_id, "content": content, "is_error": is_error
                    }),
                })
                .collect();

            // Build stats + trajectory by replaying session events
            let mut stats = serde_json::json!({
                "turns": 0, "steps": 0,
                "totalInputTokens": 0, "totalOutputTokens": 0, "latestInputTokens": 0,
                "llmMs": 0, "toolMs": 0,
                "ttftMs": 0, "ttftSteps": 0,
                "decodeMs": 0, "decodeTokens": 0,
                "cacheHitTokens": 0, "cacheMissTokens": 0,
                "startTime": 0
            });
            let mut trajectory: Vec<serde_json::Value> = Vec::new();
            let mut step_start_time: u64 = 0;
            let mut tool_call_time: u64 = 0;
            let mut turn_start_time: u64 = 0;
            let mut current_turn: u64 = 0;

            if let Some(log) = mgr.active() {
                for event in log.events() {
                    match event {
                        SessionEvent::TurnStart { turn } => {
                            current_turn = *turn;
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            turn_start_time = now;
                            if stats["startTime"].as_u64() == Some(0) {
                                stats["startTime"] = serde_json::json!(now);
                            }
                            stats["turns"] = serde_json::json!(stats["turns"].as_u64().unwrap_or(0) + 1);
                            trajectory.push(serde_json::json!({
                                "type": "turn_start", "turn": turn
                            }));
                        }
                        SessionEvent::StepStart { .. } => {
                            stats["steps"] = serde_json::json!(stats["steps"].as_u64().unwrap_or(0) + 1);
                            step_start_time = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            trajectory.push(serde_json::json!({
                                "type": "step_start"
                            }));
                        }
                        SessionEvent::StepEnd { .. } => {
                            trajectory.push(serde_json::json!({
                                "type": "step_end"
                            }));
                        }
                        SessionEvent::UserMessage { content } => {
                            trajectory.push(serde_json::json!({
                                "type": "user_message", "content": content
                            }));
                        }
                        SessionEvent::AssistantMessage { content, tool_calls, usage, ttft_ms, decode_ms } => {
                            // Track LLM time
                            if step_start_time > 0 {
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                                stats["llmMs"] = serde_json::json!(stats["llmMs"].as_u64().unwrap_or(0) + (now - step_start_time));
                                step_start_time = 0;
                            }
                            // Track TTFT/decode
                            if *ttft_ms > 0 {
                                stats["ttftMs"] = serde_json::json!(stats["ttftMs"].as_u64().unwrap_or(0) + ttft_ms);
                                stats["ttftSteps"] = serde_json::json!(stats["ttftSteps"].as_u64().unwrap_or(0) + 1);
                            }
                            if *decode_ms > 0 {
                                stats["decodeMs"] = serde_json::json!(stats["decodeMs"].as_u64().unwrap_or(0) + decode_ms);
                            }
                            // Track usage
                            if let Some(u) = usage {
                                stats["latestInputTokens"] = serde_json::json!(u.prompt_tokens);
                                stats["totalInputTokens"] = serde_json::json!(stats["totalInputTokens"].as_u64().unwrap_or(0) + u.prompt_tokens);
                                stats["totalOutputTokens"] = serde_json::json!(stats["totalOutputTokens"].as_u64().unwrap_or(0) + u.completion_tokens);
                                stats["cacheHitTokens"] = serde_json::json!(stats["cacheHitTokens"].as_u64().unwrap_or(0) + u.cache_hit_tokens);
                                stats["cacheMissTokens"] = serde_json::json!(stats["cacheMissTokens"].as_u64().unwrap_or(0) + u.cache_miss_tokens);
                                stats["decodeTokens"] = serde_json::json!(stats["decodeTokens"].as_u64().unwrap_or(0) + u.completion_tokens);
                                trajectory.push(serde_json::json!({
                                    "type": "usage",
                                    "prompt_tokens": u.prompt_tokens,
                                    "completion_tokens": u.completion_tokens,
                                    "cache_hit_tokens": u.cache_hit_tokens,
                                    "cache_miss_tokens": u.cache_miss_tokens
                                }));
                            }
                            // Tool calls in the assistant message
                            for tc in tool_calls {
                                trajectory.push(serde_json::json!({
                                    "type": "tool_call",
                                    "name": tc.name,
                                    "arguments": tc.arguments
                                }));
                                tool_call_time = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                            }
                            if !content.is_empty() {
                                trajectory.push(serde_json::json!({
                                    "type": "assistant_message", "content": content
                                }));
                            }
                        }
                        SessionEvent::ToolCall { call } => {
                            trajectory.push(serde_json::json!({
                                "type": "tool_call",
                                "name": call.name,
                                "arguments": call.arguments
                            }));
                            tool_call_time = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                        }
                        SessionEvent::ToolResult { call_id: _, content, is_error } => {
                            if tool_call_time > 0 {
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                                stats["toolMs"] = serde_json::json!(stats["toolMs"].as_u64().unwrap_or(0) + (now - tool_call_time));
                                tool_call_time = 0;
                            }
                            trajectory.push(serde_json::json!({
                                "type": "tool_result",
                                "content": content,
                                "is_error": is_error
                            }));
                        }
                        SessionEvent::TurnEnd { .. } => {
                            trajectory.push(serde_json::json!({
                                "type": "turn_end"
                            }));
                        }
                        _ => {}
                    }
                }
            }

            serve_json(&serde_json::to_string(&serde_json::json!({
                "messages": messages,
                "stats": stats,
                "trajectory": trajectory
            })).unwrap_or_else(|_| r#"{"messages":[],"stats":null,"trajectory":[]}"#.into()))
        }

        // List model presets (for hot-switch dropdown in chat UI)
        (Method::GET, "/api/models/presets") => {
            let config = crate::load_config_file().unwrap_or_else(|| state.config.clone());
            let presets: Vec<serde_json::Value> = config.models.iter().map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "model": p.model,
                    "base_url": p.base_url,
                })
            }).collect();
            // Always include the default model as the first entry.
            let default = serde_json::json!({
                "name": "默认",
                "model": config.model.model,
                "base_url": config.model.base_url,
            });
            let mut all = vec![default];
            all.extend(presets);
            serve_json(&serde_json::to_string(&all).unwrap_or_else(|_| "[]".into()))
        }

        // List skills
        (Method::GET, "/api/skills") => {
            let skills_json: Vec<serde_json::Value> = state.skills.iter().map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "description": s.description,
                    "mode": format!("{:?}", s.mode).to_lowercase(),
                    "think": s.think,
                })
            }).collect();
            let json = serde_json::to_string(&skills_json).unwrap_or_else(|_| "[]".into());
            serve_json(&json)
        }

        // Chat (SSE stream)
        (Method::POST, "/api/chat") => {
            handle_chat(req, state).await
        }

        // Slash command (local, not sent to LLM)
        (Method::POST, "/api/command") => {
            handle_command(req, state).await
        }

        // Fetch available models from the LLM endpoint (OpenAI-compatible /v1/models)
        // Accepts {base_url, api_key} from the request body so it always uses
        // the values the user just typed in the settings panel — not the
        // stale config loaded at startup.
        (Method::POST, "/api/models") => {
            handle_fetch_models(req, state).await
        }

        // Get config path info
        (Method::GET, "/api/config/path") => {
            let path = crate::resolve_config_path();
            serve_json(&format!(r#"{{"path":{:?}}}"#, path))
        }

        // Set config path (written to <exe_dir>/.dsh-lite-path, applied on restart)
        (Method::POST, "/api/config/path") => {
            let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
            match serde_json::from_slice::<serde_json::Value>(&body) {
                Ok(v) => {
                    if let Some(new_path) = v.get("path").and_then(|p| p.as_str()) {
                        let exe_dir = std::env::current_exe()
                            .ok()
                            .and_then(|e| e.parent().map(|p| p.to_path_buf()));
                        if let Some(dir) = exe_dir {
                            let marker = dir.join(".dsh-lite-path");
                            if std::fs::write(&marker, new_path).is_ok() {
                                serve_json(r#"{"ok":true,"message":"Config path saved. Restart to apply."}"#)
                            } else {
                                serve_json(r#"{"ok":false,"error":"Failed to write path file"}"#)
                            }
                        } else {
                            serve_json(r#"{"ok":false,"error":"Cannot determine exe directory"}"#)
                        }
                    } else {
                        serve_json(r#"{"ok":false,"error":"Missing 'path' field"}"#)
                    }
                }
                Err(_) => serve_json(r#"{"ok":false,"error":"Invalid JSON"}"#),
            }
        }

        // Get config (JSON)
        (Method::GET, "/api/config") => {
            let config_path = crate::resolve_config_path();
            match std::fs::read_to_string(&config_path) {
                Ok(content) => {
                    match toml::from_str::<serde_json::Value>(&content) {
                        Ok(val) => serve_json(&serde_json::to_string(&val).unwrap_or_default()),
                        Err(_) => serve_json("{}"),
                    }
                }
                Err(_) => serve_json("{}"),
            }
        }

        // Save config (JSON merge into TOML file)
        (Method::POST, "/api/config") => {
            let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
            let config_path = crate::resolve_config_path();

            // Read current config, parse the incoming JSON as TOML-compatible,
            // merge, and write back.
            let current = std::fs::read_to_string(&config_path).unwrap_or_default();
            let mut current_val: serde_json::Value = toml::from_str(&current).unwrap_or(serde_json::json!({}));

            if let Ok(incoming) = serde_json::from_slice::<serde_json::Value>(&body) {
                merge_json(&mut current_val, &incoming);
                let new_toml = toml::to_string(&current_val).unwrap_or_default();
                if std::fs::write(&config_path, new_toml).is_ok() {
                    serve_json(r#"{"ok":true,"message":"Config saved. Applied on next chat request (hot-reload)."}"#)
                } else {
                    serve_json(r#"{"ok":false,"error":"Failed to write config file"}"#)
                }
            } else {
                serve_json(r#"{"ok":false,"error":"Invalid JSON"}"#)
            }
        }

        // Get raw config file
        (Method::GET, "/api/config/raw") => {
            let config_path = crate::resolve_config_path();
            match std::fs::read_to_string(&config_path) {
                Ok(content) => {
                    let mut response = Response::new(BoxBody::new(Full::new(Bytes::from(content)).map_err(|e| match e {})));
                    response.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"));
                    *response.status_mut() = StatusCode::OK;
                    response
                }
                Err(_) => {
                    let mut response = Response::new(BoxBody::new(Full::new(Bytes::from("# Config file not found")).map_err(|e| match e {})));
                    *response.status_mut() = StatusCode::NOT_FOUND;
                    response
                }
            }
        }

        // Save raw config file
        (Method::POST, "/api/config/raw") => {
            let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
            let config_path = crate::resolve_config_path();
            let content = String::from_utf8_lossy(&body).to_string();

            // Validate it's parseable TOML before saving.
            match toml::from_str::<serde_json::Value>(&content) {
                Ok(_) => {
                    if std::fs::write(&config_path, &content).is_ok() {
                        serve_json(r#"{"ok":true,"message":"Config file saved. Applied on next chat request (hot-reload)."}"#)
                    } else {
                        serve_json(r#"{"ok":false,"error":"Failed to write file"}"#)
                    }
                }
                Err(e) => {
                    serve_json(&format!(r#"{{"ok":false,"error":"Invalid TOML: {e}"}}"#))
                }
            }
        }

        // 404
        _ => {
            let mut resp = Response::new(empty_body());
            *resp.status_mut() = StatusCode::NOT_FOUND;
            resp
        }
    };

    Ok(response)
}

/// Handle POST /api/chat — starts a turn and streams LoopEvents as SSE.
async fn handle_chat(
    req: Request<Incoming>,
    state: Arc<ServerState>,
) -> Response<BoxBody<Bytes, Infallible>> {
    // Parse the request body to get the message.
    let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
    let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_default();

    if message.is_empty() {
        return serve_json(r#"{"error":"message is required"}"#);
    }

    // Re-read config from disk so changes made in the settings panel
    // (model base_url, api_key, tools toggles) take effect immediately
    // without a restart.
    let mut config = crate::load_config_file().unwrap_or_else(|| state.config.clone());

    // If the request specifies a model preset name, override config.model
    // with that preset's values (hot-switching).
    let preset_name = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("preset").and_then(|p| p.as_str()).map(String::from));
    if let Some(ref pname) = preset_name {
        if pname != "默认" && !pname.is_empty() {
            if let Some(preset) = config.models.iter().find(|p| &p.name == pname) {
                config.model.base_url = preset.base_url.clone();
                config.model.api_key = preset.api_key.clone();
                config.model.model = preset.model.clone();
                config.model.context_window = preset.context_window;
                config.model.max_tokens = preset.max_tokens;
                config.model.temperature = preset.temperature;
                log::info!("Using model preset: {pname} ({})", preset.model);
            }
        }
    }

    // Find the active skill.
    let skill_name = state.active_skill_name.lock().await.clone();
    let skill = state.skills.iter()
        .find(|s| s.name == skill_name)
        .cloned()
        .unwrap_or_else(|| state.skills.first().cloned().unwrap_or_else(|| {
            Skill {
                name: "default".into(),
                description: "default".into(),
                when_to_use: None,
                mode: ExecMode::Plan,
                think: false,
                tools_allow: vec![],
                variables: std::collections::HashMap::new(),
                body: "You are a helpful assistant.".into(),
                steps: vec![],
            }
        }));

    // Take the session out of the manager.
    let session = {
        let mut mgr = state.session_mgr.lock().await;
        mgr.take_active().unwrap_or_else(|| crate::session::SessionLog::new(512))
    };

    // Build the tools + LLM client (shared registration function).
    let policy = crate::policy::Policy::from_config(&config.tools);
    let mut tools = crate::tools::ToolRegistry::new(policy);
    crate::tools::register_builtins(&mut tools, &config);
    crate::tools::register_subagent(&mut tools, &config);

    let llm = crate::llm::LlmClient::new(&config.model);
    let mut dispatcher = crate::dispatcher::Dispatcher::new(session, tools, llm, &config.model)
        .with_compaction(config.compaction.threshold, config.compaction.keep_recent_turns);

    // Create SSE stream.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<LoopEvent>(128);

    // Spawn the dispatch task.
    let state_clone = state.clone();
    tokio::spawn(async move {
        let result = dispatcher.dispatch(message, &skill, event_tx).await;

        // Return the session to the manager.
        let session = dispatcher.take_session();
        let mut mgr = state_clone.session_mgr.lock().await;
        mgr.return_session(session);
        mgr.checkpoint_active();

        match result {
            DispatchResult::Done { mode, reason } => log::info!("Web dispatch done: mode={mode:?}, reason={reason:?}"),
            DispatchResult::Failed { mode, message } => log::error!("Web dispatch failed: mode={mode:?}, {message}"),
        }
    });

    // Stream LoopEvents as SSE.
    let stream = async_stream::stream! {
        while let Some(event) = event_rx.recv().await {
            let json = format_loop_event(&event);
            let sse = format!("data: {json}\n\n");
            yield Ok::<_, Infallible>(Frame::data(Bytes::from(sse)));
        }
        // Send a final done event.
        yield Ok::<_, Infallible>(Frame::data(Bytes::from("data: [DONE]\n\n")));
    };

    let body = StreamBody::new(stream);
    let mut response = Response::new(BoxBody::new(body.map_err(|e| match e {})));
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-cache"),
    );
    *response.status_mut() = StatusCode::OK;
    response
}

/// Format a LoopEvent as JSON for SSE.
fn format_loop_event(event: &LoopEvent) -> String {
    match event {
        LoopEvent::TurnStart { turn } => {
            format!(r#"{{"type":"turn_start","turn":{turn}}}"#)
        }
        LoopEvent::StepStart { turn, step } => {
            format!(r#"{{"type":"step_start","turn":{turn},"step":{step}}}"#)
        }
        LoopEvent::Delta { text } => {
            let escaped = escape_json_string(text);
            format!(r#"{{"type":"delta","text":"{escaped}"}}"#)
        }
        LoopEvent::AssistantMessage { content, tool_calls } => {
            let escaped = escape_json_string(content);
            let tc_json: Vec<String> = tool_calls.iter().map(|tc| {
                format!(r#"{{"name":"{}","arguments":{}}}"#, escape_json_string(&tc.name), tc.arguments)
            }).collect();
            format!(r#"{{"type":"assistant_message","content":"{escaped}","tool_calls":[{}]}}"#, tc_json.join(","))
        }
        LoopEvent::ToolCall { call } => {
            format!(r#"{{"type":"tool_call","call_id":"{}","name":"{}","arguments":{}}}"#,
                escape_json_string(&call.id), escape_json_string(&call.name), call.arguments)
        }
        LoopEvent::ToolResult { call_id, content, is_error } => {
            let escaped = escape_json_string(content);
            format!(r#"{{"type":"tool_result","call_id":"{}","content":"{escaped}","is_error":{is_error}}}"#,
                escape_json_string(call_id))
        }
        LoopEvent::StepEnd { turn, step } => {
            format!(r#"{{"type":"step_end","turn":{turn},"step":{step}}}"#)
        }
        LoopEvent::TurnEnd { turn, reason } => {
            format!(r#"{{"type":"turn_end","turn":{turn},"reason":"{:?}"}}"#, reason)
        }
        LoopEvent::Usage { prompt_tokens, completion_tokens, cache_hit_tokens, cache_miss_tokens, ttft_ms, decode_ms } => {
            format!(r#"{{"type":"usage","prompt_tokens":{prompt_tokens},"completion_tokens":{completion_tokens},"cache_hit_tokens":{cache_hit_tokens},"cache_miss_tokens":{cache_miss_tokens},"ttft_ms":{ttft_ms},"decode_ms":{decode_ms}}}"#)
        }
        LoopEvent::Error { message } => {
            let escaped = escape_json_string(message);
            format!(r#"{{"type":"error","message":"{escaped}"}}"#)
        }
    }
}

fn escape_json_string(s: &str) -> String {
    s.replace('\\', "\\\\")
     .replace('"', "\\\"")
     .replace('\n', "\\n")
     .replace('\r', "\\r")
     .replace('\t', "\\t")
}

// ─── Response helpers ──────────────────────────────────────────────────────

fn serve_html(html: &str) -> Response<BoxBody<Bytes, Infallible>> {
    let mut response = Response::new(BoxBody::new(Full::new(Bytes::from(html.to_string())).map_err(|e| match e {})));
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    *response.status_mut() = StatusCode::OK;
    response
}

fn serve_json(json: &str) -> Response<BoxBody<Bytes, Infallible>> {
    let mut response = Response::new(BoxBody::new(Full::new(Bytes::from(json.to_string())).map_err(|e| match e {})));
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    *response.status_mut() = StatusCode::OK;
    response
}

fn empty_body() -> BoxBody<Bytes, Infallible> {
    BoxBody::new(Full::new(Bytes::new()).map_err(|e| match e {}))
}

/// Handle GET /api/models — fetches model list from the configured LLM endpoint.
///
/// Calls `<base_url>/v1/models` (OpenAI-compatible).
async fn handle_fetch_models(
    req: Request<Incoming>,
    state: Arc<ServerState>,
) -> Response<BoxBody<Bytes, Infallible>> {
    // Parse {base_url, api_key} from the request body.  Fall back to the
    // startup config if the body is missing or malformed.
    let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
    let (base_url, api_key) = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .map(|v| {
            let bu = v.get("base_url").and_then(|b| b.as_str()).unwrap_or("");
            let ak = v.get("api_key").and_then(|a| a.as_str()).unwrap_or("");
            (bu.to_string(), ak.to_string())
        })
        .unwrap_or_else(|| (state.config.model.base_url.clone(), state.config.model.api_key.clone()));

    let base_url = base_url.trim_end_matches('/').to_string();
    let models_url = if base_url.ends_with("/v1") {
        format!("{base_url}/models")
    } else {
        format!("{base_url}/v1/models")
    };

    log::info!("Fetching models from {models_url}");

    match fetch_models_blocking(&models_url, &api_key).await {
        Ok(body) => serve_json(&body),
        Err(e) => {
            log::warn!("Failed to fetch models: {e}");
            serve_json(&format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\"")))
        }
    }
}

/// Fetch the /models endpoint via hyper client.
async fn fetch_models_blocking(url: &str, api_key: &str) -> Result<String, String> {
    let no_scheme = url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let (host_port, path) = match no_scheme.find('/') {
        Some(i) => (&no_scheme[..i], &no_scheme[i..]),
        None => (no_scheme, "/"),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(80)),
        None => (host_port, 80),
    };

    let addr = format!("{host}:{port}");
    let stream = tokio::net::TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("connect {addr}: {e}"))?;

    let io = TokioIo::new(stream);
    let mut req_builder = Request::builder()
        .method(Method::GET)
        .uri(path)
        .header(hyper::header::HOST, host);
    if !api_key.is_empty() {
        req_builder = req_builder.header(hyper::header::AUTHORIZATION, format!("Bearer {api_key}"));
    }
    let req = req_builder
        .body(empty_body())
        .map_err(|e| format!("build request: {e}"))?;

    let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, BoxBody<Bytes, Infallible>>(io)
        .await
        .map_err(|e| format!("handshake: {e}"))?;
    tokio::spawn(async move { let _ = conn.await; });

    let res = sender.send_request(req).await.map_err(|e| format!("send: {e}"))?;
    let body = res.into_body().collect().await
        .map_err(|e| format!("read body: {e}"))?
        .to_bytes();
    String::from_utf8(body.to_vec()).map_err(|e| format!("utf8: {e}"))
}

/// Recursively merge JSON values (incoming overrides current).
fn merge_json(current: &mut serde_json::Value, incoming: &serde_json::Value) {
    match (current, incoming) {
        (serde_json::Value::Object(cur_map), serde_json::Value::Object(inc_map)) => {
            for (key, val) in inc_map {
                if let Some(existing) = cur_map.get_mut(key) {
                    merge_json(existing, val);
                } else {
                    cur_map.insert(key.clone(), val.clone());
                }
            }
        }
        (cur, inc) => { *cur = inc.clone(); }
    }
}

/// Handle POST /api/command — execute a slash command locally (no LLM).
///
/// Commands are registered as CommandPlugin instances. The server iterates
/// the registered plugins to find a match, dispatching without a model turn.
///
/// Request body: { "command": "/clear", "args": "" }
/// Response: { "ok": true, "text": "...", "action": "clear|new|none" }
async fn handle_command(
    req: Request<Incoming>,
    state: Arc<ServerState>,
) -> Response<BoxBody<Bytes, Infallible>> {
    let body = req.into_body().collect().await.unwrap_or_default().to_bytes();
    let parsed = serde_json::from_slice::<serde_json::Value>(&body).ok();

    let command = parsed.as_ref()
        .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(String::from))
        .unwrap_or_default();
    let args = parsed.as_ref()
        .and_then(|v| v.get("args").and_then(|a| a.as_str()).map(String::from))
        .unwrap_or_default();

    // Parse the command name (strip leading /).
    let cmd_name = command.trim_start_matches('/').trim().to_lowercase();

    log::info!("Command: /{cmd_name} (args: {args})");

    // Build command plugins (fresh each request — they're lightweight).
    let plugins = crate::commands::register_builtins();

    // Find the matching command plugin.
    let matched = plugins.iter().find(|p| p.name() == cmd_name);

    match matched {
        Some(plugin) => {
            let mut mgr = state.session_mgr.lock().await;
            let config = &state.config;
            let mut ctx = crate::commands::CommandContext {
                config,
                session_mgr: &mut *mgr,
            };
            let result = plugin.execute(&args, &mut ctx);

            // Build JSON response.
            let text_escaped = result.text.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
            let ok_str = if result.ok { "true" } else { "false" };
            let error_field = if result.ok {
                String::new()
            } else {
                format!(r#","error":"{text_escaped}""#)
            };
            let session_id_field = result.session_id
                .as_ref()
                .map(|id| format!(r#","sessionId":{:?}"#, id))
                .unwrap_or_default();
            let text_field = if result.ok {
                format!(r#""text":"{text_escaped}""#)
            } else {
                String::new()
            };

            serve_json(&format!(
                r#"{{"ok":{ok_str},{text_field}{error_field},"action":"{}"{session_id_field}}}"#,
                result.action
            ))
        }
        None => {
            serve_json(&format!(
                r#"{{"ok":false,"error":"未知命令：/{cmd_name}。输入 /help 查看可用命令。","action":"none"}}"#
            ))
        }
    }
}
