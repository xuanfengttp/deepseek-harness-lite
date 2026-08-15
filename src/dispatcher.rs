//! Dispatcher: routes user requests to the agent loop with strategy hooks.
//!
//! The entry point for all user requests. Based on the active skill's declared
//! `mode`, builds the appropriate strategy hook (Plan/Todo/Workflow) and
//! attaches it to the AgentLoop. All execution paths share the same
//! `AgentLoop::run_turn()` — the hook controls behavior via `StepDecision`.
//!
//! This replaces the old tri-mode dispatcher (run_workflow/run_todo/run_plan)
//! with a unified "build hooks → run loop" flow, matching dsh's "one loop +
//! pluggable hooks" architecture.

use crate::types::*;
use crate::session::SessionLog;
use crate::agent::{AgentLoop, LoopEvent};
use crate::llm::LlmClient;
use crate::tools::ToolRegistry;
use crate::strategies;
use crate::hooks::StepHook;
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
/// each user request through the agent loop with the appropriate strategy.
pub struct Dispatcher {
    session: SessionLog,
    tools: ToolRegistry,
    llm: LlmClient,
    model: String,
    max_tokens: usize,
    temperature: f32,
    context_window: usize,
    compaction_threshold: f32,
    keep_recent_turns: usize,
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

    /// Dispatch a user request using the active skill's mode.
    ///
    /// Builds strategy hooks from the skill's `mode`, then runs the unified
    /// agent loop. Streams events to the provided channel.
    pub async fn dispatch(
        &mut self,
        user_message: String,
        skill: &Skill,
        event_tx: mpsc::Sender<LoopEvent>,
    ) -> DispatchResult {
        log::info!(
            "Dispatching with mode: {:?}, think: {}",
            skill.mode,
            skill.think
        );

        // Build hooks from the skill's mode (strategy selection, not execution branching).
        let hooks: Vec<Box<dyn StepHook>> = strategies::build_hooks(skill);

        // Lend parts to a temporary AgentLoop, attach hooks, run.
        let session = std::mem::replace(&mut self.session, SessionLog::new(0));
        let tools = std::mem::replace(
            &mut self.tools,
            ToolRegistry::new(crate::policy::Policy::from_config(&ToolsConfig {
                shell: false,
                file_read: false,
                file_write: false,
                file_search: false,
                ssh_exec: false,
                memory: false,
                todo: false,
            })),
        );
        let llm = self.llm.clone();

        let mut agent_loop = AgentLoop::new(session, tools, llm, &ModelConfig {
            base_url: String::new(),
            api_key: String::new(),
            model: self.model.clone(),
            context_window: self.context_window,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
        })
        .with_hooks(hooks)
        .with_compaction(self.compaction_threshold, self.keep_recent_turns);

        let result = agent_loop.run_turn(user_message, skill, event_tx).await;

        // Take the parts back.
        let (session, tools, _llm) = agent_loop.into_parts();
        self.session = session;
        self.tools = tools;

        match result {
            Ok(reason) => DispatchResult::Done { mode: skill.mode, reason },
            Err(e) => DispatchResult::Failed { mode: skill.mode, message: e },
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
