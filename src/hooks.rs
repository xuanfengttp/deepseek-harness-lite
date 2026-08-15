//! Step hooks: lightweight plugin extension points for the agent loop.
//!
//! Maps to dsh `agent/pre-step` + `agent/turn-stopping` waterfall hooks.
//! Hooks are synchronous decision-makers; the loop does the async work
//! (LLM call, tool execution) based on their decision.
//!
//! The agent loop calls `pre_step` before each step and `post_step` after.
//! A strategy hook (Plan/Todo/Workflow) makes the primary decision;
//! other hooks can be added later (compaction, logging, etc.).

use crate::types::*;

/// Context passed to `StepHook::pre_step`.
pub struct PreStepContext<'a> {
    pub turn: u64,
    pub step: u64,
    pub skill: &'a Skill,
}

/// Decision returned by `pre_step` — controls what the loop does this step.
pub enum StepDecision {
    /// Let the LLM run normally. Optionally inject guidance text
    /// (appended as a user-role message before the LLM call).
    Proceed {
        injection: Option<String>,
    },
    /// Skip the LLM entirely. Execute this tool call directly.
    /// Used by WorkflowStrategy for deterministic tool steps.
    ForceTool {
        call: ToolCall,
    },
    /// Call the LLM with a specific prompt, independent context (no history).
    /// Used by WorkflowStrategy for llm_judge steps.
    ForceLlm {
        system: String,
        prompt: String,
    },
    /// End the turn now.
    Stop {
        reason: TurnEndReason,
    },
}

/// Context passed to `StepHook::post_step`.
pub struct PostStepContext<'a> {
    pub turn: u64,
    pub step: u64,
    pub skill: &'a Skill,
    /// What happened this step (the assistant content or tool result).
    pub content: String,
    pub is_error: bool,
    /// Whether the LLM requested tool calls (only meaningful for Proceed).
    pub had_tool_calls: bool,
}

/// Flow control returned by `post_step` — whether to continue or stop.
pub enum StepFlow {
    /// Run another step.
    Continue,
    /// End the turn with this reason.
    Stop {
        reason: TurnEndReason,
    },
}

/// A step hook plugs into the agent loop's step boundaries.
///
/// Implementations are stateful (they track progress internally).
/// The loop calls `pre_step` before each step and `post_step` after.
///
/// Built-in implementations:
/// - `PlanStrategy` — full LLM autonomy, no intervention
/// - `TodoStrategy` — LLM with step-by-step guidance injection
/// - `WorkflowStrategy` — deterministic SOP, ForceTool/ForceLlm bypasses LLM
pub trait StepHook: Send + Sync {
    /// Called before each step. Returns a decision that controls execution.
    fn pre_step(&mut self, ctx: &PreStepContext) -> StepDecision;

    /// Called after each step completes. Returns whether to continue.
    fn post_step(&mut self, ctx: &PostStepContext) -> StepFlow;
}
