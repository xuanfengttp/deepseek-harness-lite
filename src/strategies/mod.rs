//! Execution strategies: StepHook implementations for each ExecMode.
//!
//! Each strategy maps an `ExecMode` to hook behavior:
//!
//! - `PlanStrategy` — full LLM autonomy (Plan mode)
//! - `TodoStrategy` — LLM with step guidance injection (Todo mode)
//! - `WorkflowStrategy` — deterministic SOP, bypasses LLM (Workflow mode)
//!
//! The dispatcher builds the appropriate strategy from the skill's `mode`
//! field and attaches it to the AgentLoop as a hook.

pub mod plan;
pub mod todo;
pub mod workflow;

pub use plan::PlanStrategy;
pub use todo::TodoStrategy;
pub use workflow::WorkflowStrategy;

use crate::hooks::StepHook;
use crate::types::*;

/// Build a hook list from the skill's execution mode.
///
/// Returns a `Vec<Box<dyn StepHook>>` containing the strategy hook.
/// Future hooks (compaction, logging) can be added here.
pub fn build_hooks(skill: &Skill) -> Vec<Box<dyn StepHook>> {
    match skill.mode {
        ExecMode::Workflow => vec![Box::new(WorkflowStrategy::new(skill))],
        ExecMode::Todo => vec![Box::new(TodoStrategy::new(skill))],
        ExecMode::Plan => vec![Box::new(PlanStrategy)],
    }
}
