//! Agent loop: turn/step driver with pluggable step hooks.
//!
//! Mirrors dsh `core/agent-loop`, adapted for a single-process embedded agent.
//! The loop drives one session through turns and steps:
//!
//! ```text
//! turn/start
//!   claim input → assemble prompt
//!   loop {
//!     step/start
//!     StepHook::pre_step() → StepDecision
//!       Proceed(injection)   → LLM step (normal flow, optional guidance)
//!       ForceTool(call)      → execute tool directly (0 LLM calls)
//!       ForceLlm(system,prompt) → single LLM call, independent context
//!       Stop(reason)         → end turn
//!     execute / stream
//!     StepHook::post_step() → StepFlow
//!       Continue → next step
//!       Stop(reason) → end turn
//!     step/end
//!   }
//! turn/end
//! ```
//!
//! The hook mechanism maps to dsh `agent/pre-step` + `agent/turn-stopping`
//! waterfall hooks, simplified to a single decisive hook (first non-Proceed
//! wins). Strategies (Plan/Todo/Workflow) are the primary hook implementations.

use crate::types::*;
use crate::session::SessionLog;
use crate::prompt;
use crate::llm::{LlmClient, LlmRequest, StreamEvent};
use crate::tools::ToolRegistry;
use crate::hooks::*;
use tokio::sync::mpsc;

/// Events emitted by the agent loop for UI/trajectory consumption.
#[derive(Debug, Clone)]
pub enum LoopEvent {
    /// A turn started.
    TurnStart { turn: u64 },
    /// A step started.
    StepStart { turn: u64, step: u64 },
    /// A streaming text delta from the assistant.
    Delta { text: String },
    /// The assistant produced a complete message.
    AssistantMessage { content: String, tool_calls: Vec<ToolCall> },
    /// A tool call was initiated.
    ToolCall { call: ToolCall },
    /// A tool produced a result.
    ToolResult { call_id: String, content: String, is_error: bool },
    /// A step ended.
    StepEnd { turn: u64, step: u64 },
    /// A turn ended.
    TurnEnd { turn: u64, reason: TurnEndReason },
    /// Token usage reported by the model after an assistant message.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        cache_hit_tokens: u64,
        cache_miss_tokens: u64,
        /// Wall time from step start to first token (TTFT), in milliseconds.
        ttft_ms: u64,
        /// Wall time from first token to done (decode), in milliseconds.
        decode_ms: u64,
    },
    /// An error occurred.
    Error { message: String },
}

/// The agent loop, owning the session log, tool registry, LLM client, and hooks.
pub struct AgentLoop {
    session: SessionLog,
    tools: ToolRegistry,
    llm: LlmClient,
    model: String,
    max_tokens: usize,
    temperature: f32,
    /// Context window size for compaction trigger.
    context_window: usize,
    /// Fraction of context window that triggers compaction.
    compaction_threshold: f32,
    /// Number of recent turns to keep during compaction.
    keep_recent_turns: usize,
    /// Step hooks — control execution flow (strategy, compaction, etc.).
    hooks: Vec<Box<dyn StepHook>>,
}

impl AgentLoop {
    pub fn new(
        session: SessionLog,
        tools: ToolRegistry,
        llm: LlmClient,
        model_config: &ModelConfig,
    ) -> Self {
        Self {
            session,
            tools,
            llm,
            model: model_config.model.clone(),
            max_tokens: model_config.max_tokens,
            temperature: model_config.temperature,
            context_window: model_config.context_window,
            compaction_threshold: 0.7,
            keep_recent_turns: 3,
            hooks: Vec::new(),
        }
    }

    /// Attach step hooks (strategies, compaction, etc.).
    pub fn with_hooks(mut self, hooks: Vec<Box<dyn StepHook>>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Set compaction parameters from config.
    pub fn with_compaction(mut self, threshold: f32, keep_recent: usize) -> Self {
        self.compaction_threshold = threshold;
        self.keep_recent_turns = keep_recent;
        self
    }

    /// Run one turn with a user message and an active skill.
    ///
    /// Streams events to the provided channel. The turn completes when a hook
    /// returns `Stop`, or an error occurs.
    pub async fn run_turn(
        &mut self,
        user_message: String,
        skill: &Skill,
        event_tx: mpsc::Sender<LoopEvent>,
    ) -> Result<TurnEndReason, String> {
        let turn = self.session.begin_turn();
        let _ = event_tx.send(LoopEvent::TurnStart { turn }).await;

        // Record the user message.
        self.session
            .append(SessionEvent::UserMessage { content: user_message });

        // Assemble the prompt from the active skill + available tools.
        let all_tools = self.tools.definitions();
        let assembled = prompt::assemble(skill, &all_tools, &skill.variables);

        // Loop: steps continue as long as hooks say Continue.
        loop {
            let step = self.session.begin_step();
            let _ = event_tx.send(LoopEvent::StepStart { turn, step }).await;

            // Call hooks' pre_step. First decisive (non-Proceed-None) decision wins.
            let mut decision = StepDecision::Proceed { injection: None };
            let pre_ctx = PreStepContext { turn, step, skill };
            for hook in &mut self.hooks {
                decision = hook.pre_step(&pre_ctx);
                if !matches!(decision, StepDecision::Proceed { injection: None }) {
                    break;
                }
            }

            // Execute the decision.
            let step_outcome = match decision {
                StepDecision::Proceed { injection } => {
                    self.run_llm_step(skill, &assembled, &event_tx, turn, step, injection)
                        .await
                }
                StepDecision::ForceTool { call } => {
                    self.run_forced_tool(skill, &event_tx, turn, step, call).await
                }
                StepDecision::ForceLlm { system, prompt } => {
                    self.run_forced_llm(skill, &event_tx, turn, step, system, prompt)
                        .await
                }
                StepDecision::Stop { reason } => {
                    self.session.end_step();
                    let _ = event_tx.send(LoopEvent::StepEnd { turn, step }).await;
                    self.session.end_turn(reason.clone());
                    let _ = event_tx
                        .send(LoopEvent::TurnEnd { turn, reason: reason.clone() })
                        .await;
                    return Ok(reason);
                }
            };

            self.session.end_step();
            let _ = event_tx.send(LoopEvent::StepEnd { turn, step }).await;

            // Check for errors that should abort immediately.
            if let StepOutcome::Error(msg) = &step_outcome {
                let _ = event_tx
                    .send(LoopEvent::Error { message: msg.clone() })
                    .await;
                self.session.end_turn(TurnEndReason::Error);
                let _ = event_tx
                    .send(LoopEvent::TurnEnd { turn, reason: TurnEndReason::Error })
                    .await;
                return Ok(TurnEndReason::Error);
            }

            // Call hooks' post_step.
            let (content, is_error, had_tool_calls) = step_outcome.as_tuple();
            let post_ctx = PostStepContext {
                turn,
                step,
                skill,
                content,
                is_error,
                had_tool_calls,
            };
            let mut flow = StepFlow::Continue;
            for hook in &mut self.hooks {
                flow = hook.post_step(&post_ctx);
                if matches!(flow, StepFlow::Stop { .. }) {
                    break;
                }
            }

            match flow {
                StepFlow::Continue => {} // loop again
                StepFlow::Stop { reason } => {
                    self.session.end_turn(reason.clone());
                    let _ = event_tx
                        .send(LoopEvent::TurnEnd { turn, reason: reason.clone() })
                        .await;
                    return Ok(reason);
                }
            }
        }
    }

    /// Normal LLM step: derive messages, stream completion, execute tool calls.
    ///
    /// Optionally injects guidance text as a user message before the LLM call
    /// (used by TodoStrategy).
    async fn run_llm_step(
        &mut self,
        skill: &Skill,
        assembled: &prompt::AssembledPrompt,
        event_tx: &mpsc::Sender<LoopEvent>,
        turn: u64,
        step: u64,
        injection: Option<String>,
    ) -> StepOutcome {
        // Inject guidance if provided (Todo mode).
        if let Some(text) = injection {
            self.session
                .append(SessionEvent::UserMessage { content: text });
        }

        // Check if compaction is needed before building the request.
        let messages = self.session.derive_messages();
        let message_count = messages.len();
        if crate::compaction::needs_compaction(
            message_count,
            self.context_window,
            self.compaction_threshold,
        ) {
            log::info!(
                "Compaction triggered: {message_count} messages, threshold {:.0}%",
                self.compaction_threshold * 100.0
            );
            if let Some(result) = crate::compaction::compact(
                &self.llm,
                &self.model,
                self.temperature,
                &messages,
                self.keep_recent_turns,
            )
            .await
            {
                log::info!(
                    "Compacted {} messages into summary ({} chars)",
                    result.turns_compacted,
                    result.summary.len()
                );
                // TODO: replace older messages in the session log with the summary.
            }
        }

        // Build and send the LLM request.
        let request = LlmRequest {
            model: self.model.clone(),
            system: assembled.system.clone(),
            messages,
            tools: assembled.tools.clone(),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            think: skill.think,
        };

        // Stream the completion.
        let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(64);
        let llm_clone = self.llm.clone();
        let stream_handle = tokio::spawn(async move {
            llm_clone.stream(request, stream_tx).await
        });

        // Collect stream events.
        let mut full_content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut had_error = false;
        let mut captured_usage: Option<TokenUsage> = None;
        let step_start = std::time::Instant::now();
        let mut first_token_time: Option<std::time::Instant> = None;
        let mut done_time: Option<std::time::Instant> = None;

        while let Some(event) = stream_rx.recv().await {
            match event {
                StreamEvent::Delta(text) => {
                    if first_token_time.is_none() {
                        first_token_time = Some(std::time::Instant::now());
                    }
                    full_content.push_str(&text);
                    let _ = event_tx.send(LoopEvent::Delta { text }).await;
                }
                StreamEvent::ToolCall(tc) => {
                    tool_calls.push(tc);
                }
                StreamEvent::Done {
                    content,
                    tool_calls: tc,
                    usage,
                } => {
                    done_time = Some(std::time::Instant::now());
                    if !content.is_empty() {
                        full_content = content;
                    }
                    tool_calls = tc;
                    if let Some(u) = &usage {
                        let ttft = first_token_time
                            .map(|t| t.duration_since(step_start).as_millis() as u64)
                            .unwrap_or(0);
                        let decode = first_token_time
                            .and_then(|ft| done_time.map(|dt| dt.duration_since(ft).as_millis() as u64))
                            .unwrap_or(0);
                        let _ = event_tx
                            .send(LoopEvent::Usage {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                                cache_hit_tokens: u.cache_hit_tokens,
                                cache_miss_tokens: u.cache_miss_tokens,
                                ttft_ms: ttft,
                                decode_ms: decode,
                            })
                            .await;
                    }
                    captured_usage = usage;
                }
                StreamEvent::Error(msg) => {
                    had_error = true;
                    let _ = event_tx.send(LoopEvent::Error { message: msg }).await;
                }
            }
        }

        if let Err(e) = stream_handle.await {
            log::warn!("Stream task panicked: {e}");
        }

        if had_error {
            return StepOutcome::Error("LLM stream error".into());
        }

        // Record the assistant message.
        let usage = captured_usage;
        let ttft = first_token_time
            .map(|t| t.duration_since(step_start).as_millis() as u64)
            .unwrap_or(0);
        let decode = first_token_time
            .and_then(|ft| done_time.map(|dt| dt.duration_since(ft).as_millis() as u64))
            .unwrap_or(0);
        self.session.append(SessionEvent::AssistantMessage {
            content: full_content.clone(),
            tool_calls: tool_calls.clone(),
            usage,
            ttft_ms: ttft,
            decode_ms: decode,
        });
        let _ = event_tx
            .send(LoopEvent::AssistantMessage {
                content: full_content.clone(),
                tool_calls: tool_calls.clone(),
            })
            .await;

        // Execute tool calls (enforcing skill's tool allow-list).
        for call in &tool_calls {
            self.session.append(SessionEvent::ToolCall { call: call.clone() });
            let _ = event_tx.send(LoopEvent::ToolCall { call: call.clone() }).await;

            let result = self.tools.execute_checked(call, &skill.tools_allow).await;

            self.session.append(SessionEvent::ToolResult {
                call_id: call.id.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
            });
            let _ = event_tx
                .send(LoopEvent::ToolResult {
                    call_id: call.id.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                })
                .await;
        }

        // Content for post_step: the assistant's text.
        let content = full_content;
        let is_error = false;
        StepOutcome::Done {
            content,
            is_error,
            had_tool_calls: !tool_calls.is_empty(),
        }
    }

    /// Forced tool execution: skip LLM, execute tool directly.
    ///
    /// Used by WorkflowStrategy for deterministic tool steps.
    async fn run_forced_tool(
        &mut self,
        skill: &Skill,
        event_tx: &mpsc::Sender<LoopEvent>,
        _turn: u64,
        _step: u64,
        call: ToolCall,
    ) -> StepOutcome {
        self.session.append(SessionEvent::ToolCall { call: call.clone() });
        let _ = event_tx.send(LoopEvent::ToolCall { call: call.clone() }).await;

        let result = self.tools.execute_checked(&call, &skill.tools_allow).await;

        self.session.append(SessionEvent::ToolResult {
            call_id: call.id.clone(),
            content: result.content.clone(),
            is_error: result.is_error,
        });
        let _ = event_tx
            .send(LoopEvent::ToolResult {
                call_id: call.id.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
            })
            .await;

        StepOutcome::Done {
            content: result.content,
            is_error: result.is_error,
            had_tool_calls: false,
        }
    }

    /// Forced LLM call: single LLM call with independent context (no history).
    ///
    /// Used by WorkflowStrategy for llm_judge steps.
    async fn run_forced_llm(
        &mut self,
        skill: &Skill,
        event_tx: &mpsc::Sender<LoopEvent>,
        _turn: u64,
        _step: u64,
        system: String,
        prompt: String,
    ) -> StepOutcome {
        let messages = vec![Message::User { content: prompt }];

        let request = LlmRequest {
            model: self.model.clone(),
            system,
            messages,
            tools: vec![],
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            think: skill.think,
        };

        let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(64);
        let llm_clone = self.llm.clone();
        let handle = tokio::spawn(async move {
            llm_clone.stream(request, stream_tx).await
        });

        let mut judge_content = String::new();
        let mut had_error = false;

        while let Some(event) = stream_rx.recv().await {
            match event {
                StreamEvent::Delta(text) => {
                    judge_content.push_str(&text);
                    let _ = event_tx.send(LoopEvent::Delta { text }).await;
                }
                StreamEvent::Done { content, .. } => {
                    if !content.is_empty() {
                        judge_content = content;
                    }
                }
                StreamEvent::ToolCall(_) => {}
                StreamEvent::Error(msg) => {
                    let _ = event_tx.send(LoopEvent::Error { message: msg }).await;
                    had_error = true;
                }
            }
        }
        let _ = handle.await;

        if had_error {
            return StepOutcome::Error("LLM judge error".into());
        }

        // Record the assistant message (for session log continuity).
        self.session.append(SessionEvent::AssistantMessage {
            content: judge_content.clone(),
            tool_calls: vec![],
            usage: None,
            ttft_ms: 0,
            decode_ms: 0,
        });
        let _ = event_tx
            .send(LoopEvent::AssistantMessage {
                content: judge_content.clone(),
                tool_calls: vec![],
            })
            .await;

        StepOutcome::Done {
            content: judge_content,
            is_error: false,
            had_tool_calls: false,
        }
    }

    /// Access the session log (for persistence/trajectory).
    pub fn session(&self) -> &SessionLog {
        &self.session
    }

    /// Mutable access to the session log.
    pub fn session_mut(&mut self) -> &mut SessionLog {
        &mut self.session
    }

    /// Consume the agent loop and return its parts.
    /// Used by the dispatcher to restore ownership after a turn.
    pub fn into_parts(self) -> (SessionLog, ToolRegistry, LlmClient) {
        (self.session, self.tools, self.llm)
    }
}

/// Internal outcome of one step execution.
enum StepOutcome {
    Done {
        content: String,
        is_error: bool,
        had_tool_calls: bool,
    },
    Error(String),
}

impl StepOutcome {
    fn as_tuple(self) -> (String, bool, bool) {
        match self {
            StepOutcome::Done { content, is_error, had_tool_calls } => {
                (content, is_error, had_tool_calls)
            }
            StepOutcome::Error(msg) => (msg, true, false),
        }
    }
}
