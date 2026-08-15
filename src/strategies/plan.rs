//! Plan strategy: full agent loop, LLM drives freely.
//!
//! The simplest strategy — no intervention. The LLM plans, executes tools,
//! and decides when to stop. Maps to dsh plan-mode (which is a plugin that
//! hooks `agent/pre-step` with no injection when inactive).

use crate::hooks::*;
use crate::types::*;

/// Plan strategy: let the LLM run with full autonomy.
pub struct PlanStrategy;

impl StepHook for PlanStrategy {
    fn pre_step(&mut self, _ctx: &PreStepContext) -> StepDecision {
        StepDecision::Proceed { injection: None }
    }

    fn post_step(&mut self, ctx: &PostStepContext) -> StepFlow {
        // Original behavior: if no tool calls, the turn is complete.
        if ctx.had_tool_calls {
            StepFlow::Continue
        } else {
            StepFlow::Stop {
                reason: TurnEndReason::Completed,
            }
        }
    }
}
