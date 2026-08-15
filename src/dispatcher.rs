//! Dispatcher: tri-mode task routing.
//!
//! The entry point for all user requests. Based on the active skill's declared
//! `mode`, routes to one of three execution paths:
//!
//! - `workflow` — deterministic SOP. Runs fixed tool steps from the skill's
//!   `steps` definition, bypassing the agent loop entirely. Only calls the LLM
//!   for `llm_judge` steps (parsing/summarizing). Maximum efficiency.
//!
//! - `todo` — agent-guided deterministic. The agent loop runs, but the skill
//!   constrains the step sequence. Each step the LLM is guided by the skill's
//!   instructions to execute a specific action. The path is known, the content
//!   needs LLM judgment.
//!
//! - `plan` — full exploration. The agent loop runs freely; the agent plans,
//!   executes tools, and re-plans based on results. For unknown problems.
//!
//! The `think` field controls LLM reasoning mode per skill:
//! - `think: false` (workflow/todo) → reasoning off, fast, cheap
//! - `think: true` (plan) → reasoning on, quality-first

use crate::types::*;
use crate::session::SessionLog;
use crate::agent::{AgentLoop, LoopEvent};
use crate::llm::LlmClient;
use crate::tools::ToolRegistry;
use crate::prompt;
use tokio::sync::mpsc;

/// Why a dispatch completed.
#[derive(Debug, Clone)]
pub enum DispatchResult {
    /// The task completed successfully.
    Done { mode: ExecMode, reason: TurnEndReason },
    /// An error occurred during dispatch.
    Failed { mode: ExecMode, message: String },
}

/// The dispatcher owns the shared session, tools, and LLM client, and routes
/// each user request to the appropriate mode runner.
pub struct Dispatcher {
    session: SessionLog,
    tools: ToolRegistry,
    llm: LlmClient,
    model: String,
    max_tokens: usize,
    temperature: f32,
}

impl Dispatcher {
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
        }
    }

    /// Dispatch a user request using the active skill's mode.
    ///
    /// Streams events to the provided channel. Returns the final result.
    pub async fn dispatch(
        &mut self,
        user_message: String,
        skill: &Skill,
        event_tx: mpsc::Sender<LoopEvent>,
    ) -> DispatchResult {
        log::info!("Dispatching with mode: {:?}, think: {}", skill.mode, skill.think);

        match skill.mode {
            ExecMode::Workflow => {
                self.run_workflow(user_message, skill, event_tx).await
            }
            ExecMode::Todo => {
                self.run_todo(user_message, skill, event_tx).await
            }
            ExecMode::Plan => {
                self.run_plan(user_message, skill, event_tx).await
            }
        }
    }

    /// Workflow mode: deterministic SOP, bypass agent loop.
    ///
    /// Executes the skill's `steps` in order. Each step is either a tool call
    /// (direct execution, no LLM) or an `llm_judge` (single LLM call to
    /// parse/summarize). Steps may have a `when` condition.
    async fn run_workflow(
        &mut self,
        user_message: String,
        skill: &Skill,
        event_tx: mpsc::Sender<LoopEvent>,
    ) -> DispatchResult {
        log::info!("Workflow mode: {} steps", skill.steps.len());

        let turn = self.session.begin_turn();
        let _ = event_tx.send(LoopEvent::TurnStart { turn }).await;
        self.session.append(SessionEvent::UserMessage { content: user_message });

        let mut step_results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut had_error = false;

        for (i, step) in skill.steps.iter().enumerate() {
            let step_num = (i + 1) as u64;
            let _ = event_tx.send(LoopEvent::StepStart { turn, step: step_num }).await;

            // Evaluate `when` condition using the expression evaluator.
            if let Some(when_expr) = &step.when {
                if !crate::expr::evaluate(when_expr, &step_results) {
                    log::info!("Step `{}` skipped (condition not met: {})", step.id, when_expr);
                    let _ = event_tx.send(LoopEvent::StepEnd { turn, step: step_num }).await;
                    continue;
                }
            }

            let result = match &step.action {
                StepAction::Tool { tool, args } => {
                    // Interpolate variables in args.
                    let interpolated_args = crate::expr::interpolate_json(args, &step_results, &skill.variables);
                    let call = ToolCall {
                        id: format!("wf_{}", step.id),
                        name: tool.clone(),
                        arguments: interpolated_args,
                    };

                    log::info!("Workflow step `{}`: tool `{}`", step.id, tool);
                    let _ = event_tx.send(LoopEvent::ToolCall { call: call.clone() }).await;
                    self.session.append(SessionEvent::ToolCall { call: call.clone() });

                    let result = self.tools.execute_checked(&call, &skill.tools_allow).await;

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

                    result
                }
                StepAction::LlmJudge { prompt, input } => {
                    // Single LLM call with independent context (just the prompt + input).
                    let interpolated_input = crate::expr::interpolate_str(input, &step_results, &skill.variables);
                    let interpolated_prompt = crate::expr::interpolate_str(prompt, &step_results, &skill.variables);

                    log::info!("Workflow step `{}`: llm_judge", step.id);

                    let judge_messages = vec![
                        Message::User {
                            content: format!("{interpolated_prompt}\n\n---\n\n{interpolated_input}"),
                        },
                    ];

                    let request = crate::llm::LlmRequest {
                        model: self.model.clone(),
                        system: String::new(),
                        messages: judge_messages,
                        tools: vec![],
                        max_tokens: self.max_tokens,
                        temperature: self.temperature,
                        think: skill.think,
                    };

                    let (stream_tx, mut stream_rx) = mpsc::channel(64);
                    let llm_clone = self.llm.clone();
                    let handle = tokio::spawn(async move {
                        llm_clone.stream(request, stream_tx).await
                    });

                    let mut judge_content = String::new();
                    while let Some(event) = stream_rx.recv().await {
                        match event {
                            crate::llm::StreamEvent::Delta(text) => {
                                judge_content.push_str(&text);
                                let _ = event_tx.send(LoopEvent::Delta { text }).await;
                            }
                            crate::llm::StreamEvent::Done { content, .. } => {
                                if !content.is_empty() {
                                    judge_content = content;
                                }
                            }
                            crate::llm::StreamEvent::ToolCall(_) => {}
                            crate::llm::StreamEvent::Error(msg) => {
                                let _ = event_tx.send(LoopEvent::Error { message: msg }).await;
                                had_error = true;
                            }
                        }
                    }
                    let _ = handle.await;

                    let _ = event_tx.send(LoopEvent::AssistantMessage {
                        content: judge_content.clone(),
                        tool_calls: vec![],
                    }).await;

                    ToolResult { content: judge_content, is_error: false }
                }
            };

            if result.is_error {
                log::warn!("Workflow step `{}` failed", step.id);
                had_error = true;
            }

            step_results.insert(step.id.clone(), result.content);
            let _ = event_tx.send(LoopEvent::StepEnd { turn, step: step_num }).await;
        }

        let reason = if had_error { TurnEndReason::Error } else { TurnEndReason::Completed };
        self.session.end_turn(reason.clone());
        let _ = event_tx.send(LoopEvent::TurnEnd { turn, reason: reason.clone() }).await;

        DispatchResult::Done { mode: ExecMode::Workflow, reason }
    }

    /// Todo mode: agent-guided with skill-constrained steps.
    ///
    /// The agent loop runs, but the skill provides step guidance. Each step
    /// the LLM is told what to do (from the skill's step instructions) and
    /// executes within the constrained tool set. The path is deterministic;
    /// the LLM fills in the specifics.
    async fn run_todo(
        &mut self,
        user_message: String,
        skill: &Skill,
        event_tx: mpsc::Sender<LoopEvent>,
    ) -> DispatchResult {
        log::info!("Todo mode: {} guided steps", skill.steps.len());

        // For P2, todo mode uses the agent loop but injects step guidance
        // into each step's user message. This is a simplified approach:
        // we build a guided prompt combining the user's request with the
        // step sequence, and let the agent loop handle tool execution.
        //
        // Full per-step enforcement (forcing the agent to do step N before
        // step N+1) arrives with the todo module in P3.

        let guided_message = if skill.steps.is_empty() {
            user_message
        } else {
            let steps_guide = skill.steps.iter().enumerate()
                .map(|(i, s)| format!("{}. {}", i + 1, s.id))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{user_message}\n\nFollow these steps in order:\n{steps_guide}")
        };

        self.run_plan(guided_message, skill, event_tx).await
    }

    /// Plan mode: full agent loop exploration.
    async fn run_plan(
        &mut self,
        user_message: String,
        skill: &Skill,
        event_tx: mpsc::Sender<LoopEvent>,
    ) -> DispatchResult {
        log::info!("Plan mode: full agent loop");

        // Lend our parts to a temporary AgentLoop, then take them back.
        let session = std::mem::replace(&mut self.session, SessionLog::new(0));
        let tools = std::mem::replace(
            &mut self.tools,
            ToolRegistry::new(crate::policy::Policy::from_config(&ToolsConfig {
                shell: false, file_read: false, file_write: false, file_search: false,
                ssh_exec: false, memory: false, todo: false,
            })),
        );
        let llm = self.llm.clone();

        let mut agent_loop = AgentLoop::new(session, tools, llm, &ModelConfig {
            base_url: String::new(),
            api_key: String::new(),
            model: self.model.clone(),
            context_window: 0,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
        });

        let result = agent_loop.run_turn(user_message, skill, event_tx).await;

        // Take the parts back.
        let (session, tools, _llm) = agent_loop.into_parts();
        self.session = session;
        self.tools = tools;

        match result {
            Ok(reason) => DispatchResult::Done { mode: ExecMode::Plan, reason },
            Err(e) => DispatchResult::Failed { mode: ExecMode::Plan, message: e },
        }
    }

    /// Access the session log.
    pub fn session(&self) -> &SessionLog {
        &self.session
    }

    /// Take the session log out of the dispatcher (for returning to SessionManager).
    pub fn take_session(&mut self) -> SessionLog {
        std::mem::replace(&mut self.session, SessionLog::new(0))
    }
}
