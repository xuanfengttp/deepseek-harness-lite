//! Agent loop: turn/step driver for plan-mode execution.
//!
//! Mirrors dsh `core/agent-loop`, simplified for a single-process embedded agent.
//! The loop drives one session through turns and steps:
//!
//! ```text
//! turn/start
//!   claim input → assemble prompt
//!   → step/start → build request → llm.stream → assistant_message
//!     → tool_call* → execute → tool_result*
//!   → step/end
//!   → (tools pending or new input → next step)
//! turn/end
//! ```
//!
//! Simplifications vs dsh:
//! - No waterfall middleware — pre_step and on_result are optional plain hooks
//! - Single agent, single session (multi-session arrives in P5)
//! - Cancellation via tokio CancellationToken (P5+)

use crate::types::*;
use crate::session::SessionLog;
use crate::prompt;
use crate::llm::{LlmClient, LlmRequest, StreamEvent};
use crate::tools::ToolRegistry;
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
    Usage { prompt_tokens: u64, completion_tokens: u64 },
    /// An error occurred.
    Error { message: String },
}

/// The agent loop, owning the session log, tool registry, and LLM client.
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
        }
    }

    /// Set compaction parameters from config.
    pub fn with_compaction(mut self, threshold: f32, keep_recent: usize) -> Self {
        self.compaction_threshold = threshold;
        self.keep_recent_turns = keep_recent;
        self
    }

    /// Run one turn with a user message and an active skill.
    ///
    /// Streams events to the provided channel. The turn completes when the
    /// model finishes with no pending tool calls, or an error occurs.
    pub async fn run_turn(
        &mut self,
        user_message: String,
        skill: &Skill,
        event_tx: mpsc::Sender<LoopEvent>,
    ) -> Result<TurnEndReason, String> {
        let turn = self.session.begin_turn();
        let _ = event_tx.send(LoopEvent::TurnStart { turn }).await;

        // Record the user message.
        self.session.append(SessionEvent::UserMessage { content: user_message });

        // Assemble the prompt from the active skill + available tools.
        let all_tools = self.tools.definitions();
        let assembled = prompt::assemble(skill, &all_tools, &skill.variables);

        // Loop: steps continue as long as the model requests tool calls.
        loop {
            let step = self.session.begin_step();
            let _ = event_tx.send(LoopEvent::StepStart { turn, step }).await;

            // Check if compaction is needed before building the request.
            let messages = self.session.derive_messages();
            let message_count = messages.len();
            if crate::compaction::needs_compaction(message_count, self.context_window, self.compaction_threshold) {
                log::info!("Compaction triggered: {message_count} messages, threshold {:.0}%", self.compaction_threshold * 100.0);
                if let Some(result) = crate::compaction::compact(
                    &self.llm,
                    &self.model,
                    self.temperature,
                    &messages,
                    self.keep_recent_turns,
                ).await {
                    log::info!("Compacted {} messages into summary ({} chars)",
                        result.turns_compacted, result.summary.len());
                    // TODO: replace older messages in the session log with the summary.
                    // For P4, we log the compaction event. Full message replacement
                    // requires a session log restructure (insert summary, drop old).
                    // This is safe to defer — the ring buffer naturally bounds memory.
                }
            }

            // Build and send the LLM request.
            let request = LlmRequest {
                model: self.model.clone(),
                messages,
                tools: assembled.tools.clone(),
                max_tokens: self.max_tokens,
                temperature: self.temperature,
                think: skill.think,
            };

            // Stream the completion.
            let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(64);

            // Spawn the streaming task (clone LlmClient — it's cheap and stateless).
            let llm_clone = self.llm.clone();
            let stream_handle = tokio::spawn(async move {
                llm_clone.stream(request, stream_tx).await
            });

            // Collect stream events.
            let mut full_content = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut had_error = false;
            let mut captured_usage: Option<TokenUsage> = None;

            while let Some(event) = stream_rx.recv().await {
                match event {
                    StreamEvent::Delta(text) => {
                        full_content.push_str(&text);
                        let _ = event_tx.send(LoopEvent::Delta { text }).await;
                    }
                    StreamEvent::ToolCall(tc) => {
                        // A fully assembled tool call from the stream — collect it.
                        tool_calls.push(tc);
                    }
                    StreamEvent::Done { content, tool_calls: tc, usage } => {
                        // Use the accumulated content from Done (authoritative).
                        if !content.is_empty() {
                            full_content = content;
                        }
                        tool_calls = tc;
                        // Capture token usage for the stats footer.
                        if let Some(u) = &usage {
                            let _ = event_tx.send(LoopEvent::Usage {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                            }).await;
                        }
                        captured_usage = usage;
                    }
                    StreamEvent::Error(msg) => {
                        had_error = true;
                        let _ = event_tx.send(LoopEvent::Error { message: msg }).await;
                    }
                }
            }

            // Wait for the stream task to complete.
            if let Err(e) = stream_handle.await {
                log::warn!("Stream task panicked: {e}");
            }

            if had_error {
                self.session.end_step();
                let _ = event_tx.send(LoopEvent::StepEnd { turn, step }).await;
                self.session.end_turn(TurnEndReason::Error);
                let _ = event_tx.send(LoopEvent::TurnEnd { turn, reason: TurnEndReason::Error }).await;
                return Ok(TurnEndReason::Error);
            }

            // Record the assistant message.
            let usage = captured_usage;
            self.session.append(SessionEvent::AssistantMessage {
                content: full_content.clone(),
                tool_calls: tool_calls.clone(),
                usage,
            });
            let _ = event_tx.send(LoopEvent::AssistantMessage {
                content: full_content.clone(),
                tool_calls: tool_calls.clone(),
            }).await;

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
                let _ = event_tx.send(LoopEvent::ToolResult {
                    call_id: call.id.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                }).await;
            }

            self.session.end_step();
            let _ = event_tx.send(LoopEvent::StepEnd { turn, step }).await;

            // If there were tool calls, we need another step to process results.
            // If no tool calls, the turn is complete.
            if tool_calls.is_empty() {
                break;
            }
            // Otherwise, loop again — the next step will derive messages including
            // the tool results, and the model can continue or finish.
        }

        let reason = TurnEndReason::Completed;
        self.session.end_turn(reason.clone());
        let _ = event_tx.send(LoopEvent::TurnEnd { turn, reason: reason.clone() }).await;
        Ok(reason)
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
    /// Used by the dispatcher to restore ownership after a plan-mode turn.
    pub fn into_parts(self) -> (SessionLog, ToolRegistry, LlmClient) {
        (self.session, self.tools, self.llm)
    }
}
