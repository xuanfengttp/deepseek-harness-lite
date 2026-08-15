//! Todo strategy: LLM runs with step-by-step guidance.
//!
//! The agent loop runs normally, but each step the LLM is told which step
//! it's on and instructed to complete it before proceeding. The path is
//! known; the LLM fills in the specifics.
//!
//! This replaces the old `run_todo` which just concatenated all step ids
//! into the user message. Now guidance is injected per-step via the hook.

use crate::hooks::*;
use crate::types::*;
use std::collections::HashMap;

/// Todo strategy: guide the LLM through a known step sequence.
pub struct TodoStrategy {
    steps: Vec<SkillStep>,
    current: usize,
    step_results: HashMap<String, String>,
    #[allow(dead_code)]
    variables: HashMap<String, String>,
}

impl TodoStrategy {
    pub fn new(skill: &Skill) -> Self {
        Self {
            steps: skill.steps.clone(),
            current: 0,
            step_results: HashMap::new(),
            variables: skill.variables.clone(),
        }
    }
}

impl StepHook for TodoStrategy {
    fn pre_step(&mut self, _ctx: &PreStepContext) -> StepDecision {
        if self.current >= self.steps.len() {
            return StepDecision::Stop {
                reason: TurnEndReason::Completed,
            };
        }
        let step = &self.steps[self.current];
        let guidance = format!(
            "You are on step {n} of {total}: `{id}`.\nComplete this step before proceeding.",
            n = self.current + 1,
            total = self.steps.len(),
            id = step.id,
        );
        log::info!("Todo step {}/{}: `{}`", self.current + 1, self.steps.len(), step.id);
        StepDecision::Proceed {
            injection: Some(guidance),
        }
    }

    fn post_step(&mut self, ctx: &PostStepContext) -> StepFlow {
        if self.current < self.steps.len() {
            let step = &self.steps[self.current];
            self.step_results.insert(step.id.clone(), ctx.content.clone());
            self.current += 1;
        }
        if self.current >= self.steps.len() {
            StepFlow::Stop {
                reason: if ctx.is_error {
                    TurnEndReason::Error
                } else {
                    TurnEndReason::Completed
                },
            }
        } else {
            StepFlow::Continue
        }
    }
}
