//! HTTP server: serves the web client + API endpoints.
//!
//! Uses hyper directly (no axum) to minimize binary size and memory.
//! The web client is embedded at compile time via include_str!.
//!
//! Endpoints:
//! - GET  /            → web client HTML
//! - POST /api/chat    → SSE stream of LoopEvents for one turn
//! - GET  /api/sessions → list all sessions (JSON)
//! - POST /api/sessions/create → create a new session
//! - POST /api/sessions/switch → switch active session
//! - GET  /api/skills   → list loaded skills
//! - GET  /api/health   → health check

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

    // Build the tools + LLM client.
    let policy = crate::policy::Policy::from_config(&state.config.tools);
    let mut tools = crate::tools::ToolRegistry::new(policy);
    if state.config.tools.shell {
        tools.register(crate::tools::shell::definition(), crate::tools::shell::make_executor_fn());
    }
    if state.config.tools.file_read {
        tools.register(crate::tools::file::read_definition(), crate::tools::file::make_read_executor());
    }
    if state.config.tools.file_write {
        tools.register(crate::tools::file::write_definition(), crate::tools::file::make_write_executor());
    }
    if state.config.tools.file_search {
        tools.register(crate::tools::file::search_definition(), crate::tools::file::make_search_executor());
    }
    if state.config.tools.memory {
        let store = std::sync::Arc::new(crate::memory::MemoryStore::open(
            &state.config.memory.path,
            state.config.memory.max_entries,
        ));
        tools.register(crate::tools::memory::read_definition(), crate::tools::memory::make_read_executor(store.clone()));
        tools.register(crate::tools::memory::write_definition(), crate::tools::memory::make_write_executor(store.clone()));
        tools.register(crate::tools::memory::recall_definition(), crate::tools::memory::make_recall_executor(store));
    }

    let llm = crate::llm::LlmClient::new(&state.config.model);
    let mut dispatcher = crate::dispatcher::Dispatcher::new(session, tools, llm, &state.config.model);

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
            format!(r#"{{"type":"tool_call","name":"{}","arguments":{}}}"#,
                escape_json_string(&call.name), call.arguments)
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
