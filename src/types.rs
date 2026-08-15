//! Core types shared across modules — session events, messages, tools, config.
//!
//! This module is the type spine: other modules import from here rather than
//! defining their own parallel structures. Mirrors the dsh `core/session` +
//! `core/agent` + `llm` type vocabulary, simplified for a single-process
//! embedded agent with no plugin loader.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Identifiers ───────────────────────────────────────────────────────────

/// Branded session id (newtype for type safety at boundaries).
#[allow(dead_code)]
pub type SessionId = String;

/// Branded tool-call id (correlates a tool_call with its tool_result).
pub type CallId = String;

// ─── Messages (LLM vocabulary) ─────────────────────────────────────────────

/// One message in the model conversation history, derived from the session log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// User-authored or injected content.
    User { content: String },
    /// Assistant-generated content (may contain tool calls).
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    /// The outcome of one tool call, fed back to the model.
    Tool { call_id: CallId, content: String, is_error: bool },
}

/// A tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: CallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// One chunk from the streaming response (kept for trajectory; not in derived history).
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub delta: String,
    pub tool_call_delta: Option<ToolCallDelta>,
}

/// Incremental tool-call fragment from streaming (assembled into full ToolCall).
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments_fragment: Option<String>,
}

/// Token usage reported by the model on assistant message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Cache-hit tokens (DeepSeek prompt_cache_hit_tokens); 0 if not reported.
    #[serde(default)]
    pub cache_hit_tokens: u64,
    /// Cache-miss tokens (DeepSeek prompt_cache_miss_tokens); 0 if not reported.
    #[serde(default)]
    pub cache_miss_tokens: u64,
}

// ─── Session Events (durable log) ──────────────────────────────────────────

/// Append-only session event. Only surface events (User/Assistant/Tool) feed
/// `derive_messages()`; others are structural or trajectory-only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    TurnStart { turn: u64 },
    TurnEnd { turn: u64, reason: TurnEndReason },
    StepStart { turn: u64, step: u64 },
    StepEnd { turn: u64, step: u64 },
    UserMessage { content: String },
    AssistantChunk { delta: String },
    AssistantMessage {
        content: String,
        tool_calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
        /// Wall time from step start to first token (TTFT), in milliseconds.
        #[serde(default)]
        ttft_ms: u64,
        /// Wall time from first token to done (decode), in milliseconds.
        #[serde(default)]
        decode_ms: u64,
    },
    ToolCall { call: ToolCall },
    ToolResult { call_id: CallId, content: String, is_error: bool },
    TodoWrite { todos: Vec<String> },
    RequestHeader { model: String },
    /// A compaction summary that replaced older events. Derives into a
    /// single User message containing the summary text.
    CompactionSummary { summary: String },
}

/// Why a turn ended. Merge-extensible: new variants don't break old logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TurnEndReason {
    /// Model finished naturally with no pending tool calls.
    Completed,
    /// Model hit max-tokens limit.
    MaxTokens,
    /// Agent loop was aborted by user or signal.
    Aborted,
    /// An error occurred during the turn.
    Error,
}

// ─── Tool Definitions ──────────────────────────────────────────────────────

/// A registered tool's schema and execution contract.
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON schema for the parameters object.
    pub parameters: serde_json::Value,
    /// Maximum execution time in milliseconds.
    pub timeout_ms: u64,
}

/// The outcome of one tool execution.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

// ─── Configuration ─────────────────────────────────────────────────────────

/// Top-level configuration loaded from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub model: ModelConfig,
    /// Optional list of named model presets for hot-switching in the chat UI.
    /// Each entry has a display name + the same fields as the default model.
    #[serde(default)]
    pub models: Vec<ModelPreset>,
    pub server: ServerConfig,
    pub session: SessionConfig,
    pub memory: MemoryConfig,
    pub compaction: CompactionConfig,
    pub skill: SkillConfig,
    pub trajectory: TrajectoryConfig,
    pub tools: ToolsConfig,
    /// Optional SSH target presets for persistent device sessions.
    #[serde(default)]
    pub ssh: SshConfig,
}

/// A named model preset for hot-switching in the chat UI.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ModelPreset {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_context_window")]
    pub context_window: usize,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

fn default_context_window() -> usize { 8192 }
fn default_max_tokens() -> usize { 2048 }
fn default_temperature() -> f32 { 0.0 }

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub context_window: usize,
    pub max_tokens: usize,
    pub temperature: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionConfig {
    pub persist_dir: String,
    pub checkpoint_events: usize,
    pub checkpoint_turn_end: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryConfig {
    pub backend: String,
    pub path: String,
    pub max_entries: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompactionConfig {
    pub threshold: f32,
    pub keep_recent_turns: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillConfig {
    pub dir: String,
    /// Active skill name (optional). If set, this skill is used by default.
    /// Can be overridden by CLI `--skill <name>`.
    #[serde(default)]
    pub active: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrajectoryConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolsConfig {
    pub shell: bool,
    pub file_read: bool,
    pub file_write: bool,
    pub file_search: bool,
    pub ssh_exec: bool,
    pub memory: bool,
    pub todo: bool,
}

/// SSH configuration — named device targets for persistent interactive sessions.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct SshConfig {
    /// Named SSH targets. Each target is a persistent connection that stays
    /// open across multiple commands, enabling interactive device queries.
    #[serde(default)]
    pub targets: Vec<SshTarget>,
}

/// One SSH device target — a persistent connection to a network element.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct SshTarget {
    /// Friendly name for this device (used as the `target` parameter in ssh_exec).
    pub name: String,
    /// Host address (IP or hostname).
    pub host: String,
    /// SSH port (default 22).
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// SSH username.
    pub user: String,
    /// SSH password (plaintext — embedded device, no key agent needed).
    #[serde(default)]
    pub password: String,
}

fn default_ssh_port() -> u16 {
    22
}

/// Execution mode declared by a skill or chosen at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecMode {
    /// Deterministic SOP — bypass agent loop, run fixed steps.
    Workflow,
    /// Agent-guided deterministic — each step is certain, LLM guides.
    Todo,
    /// Full exploration — agent plans, executes, re-plans.
    Plan,
}

impl Default for ExecMode {
    fn default() -> Self {
        ExecMode::Plan
    }
}

// ─── Skill ─────────────────────────────────────────────────────────────────

/// A loaded skill definition (YAML frontmatter + Markdown body).
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    #[allow(dead_code)]
    pub when_to_use: Option<String>,
    pub mode: ExecMode,
    pub think: bool,
    pub tools_allow: Vec<String>,
    pub variables: HashMap<String, String>,
    /// The Markdown body (skill instructions rendered to the model).
    pub body: String,
    /// Workflow steps (workflow/todo modes only).
    pub steps: Vec<SkillStep>,
}

/// One step in a workflow or todo-mode skill.
#[derive(Debug, Clone)]
pub struct SkillStep {
    pub id: String,
    /// Tool to execute, or an LLM judgment call.
    pub action: StepAction,
    /// Condition for skipping (template expression, evaluated at runtime).
    pub when: Option<String>,
}

/// What a step does.
#[derive(Debug, Clone)]
pub enum StepAction {
    /// Execute a built-in tool with templated arguments.
    Tool { tool: String, args: serde_json::Value },
    /// Ask the LLM to judge/parse, then store the result.
    LlmJudge { prompt: String, input: String },
}
