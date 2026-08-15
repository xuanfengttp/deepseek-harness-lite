//! Workflow strategy: deterministic SOP, bypasses LLM for tool steps.
//!
//! Each step is either a `ForceTool` (execute a tool directly, 0 LLM calls)
//! or a `ForceLlm` (single LLM call with independent context, no history).
//! Steps may have a `when` condition that skips them.
//!
//! This is the most accuracy-critical strategy: tool steps are 100%
//! deterministic because the LLM never participates in deciding what to do.

use crate::hooks::*;
use crate::types::*;
use crate::expr;
use std::collections::HashMap;

/// Workflow strategy: run fixed steps deterministically.
pub struct WorkflowStrategy {
    steps: Vec<SkillStep>,
    current: usize,
    step_results: HashMap<String, String>,
    variables: HashMap<String, String>,
    /// Set when pre_step returns ForceTool/ForceLlm, so post_step knows
    /// which step to record the result for.
    active_step: usize,
}

impl WorkflowStrategy {
    pub fn new(skill: &Skill) -> Self {
        Self {
            steps: skill.steps.clone(),
            current: 0,
            step_results: HashMap::new(),
            variables: skill.variables.clone(),
            active_step: 0,
        }
    }
}

impl StepHook for WorkflowStrategy {
    fn pre_step(&mut self, _ctx: &PreStepContext) -> StepDecision {
        // Skip steps whose `when` condition is not met.
        loop {
            if self.current >= self.steps.len() {
                return StepDecision::Stop {
                    reason: TurnEndReason::Completed,
                };
            }
            let step = &self.steps[self.current];
            if let Some(when) = &step.when {
                if !expr::evaluate(when, &self.step_results) {
                    log::info!(
                        "Workflow step `{}` skipped (condition not met: {})",
                        step.id,
                        when
                    );
                    self.current += 1;
                    continue;
                }
            }
            break;
        }

        self.active_step = self.current;
        let step = &self.steps[self.current];

        match &step.action {
            StepAction::Tool { tool, args } => {
                let interpolated =
                    expr::interpolate_json(args, &self.step_results, &self.variables);
                let call = ToolCall {
                    id: format!("wf_{}", step.id),
                    name: tool.clone(),
                    arguments: interpolated,
                };
                log::info!("Workflow step `{}`: force tool `{}`", step.id, tool);
                StepDecision::ForceTool { call }
            }
            StepAction::LlmJudge { prompt, input } => {
                let interpolated_prompt =
                    expr::interpolate_str(prompt, &self.step_results, &self.variables);
                let interpolated_input =
                    expr::interpolate_str(input, &self.step_results, &self.variables);
                let full_prompt = format!("{interpolated_prompt}\n\n---\n\n{interpolated_input}");
                log::info!("Workflow step `{}`: force llm_judge", step.id);
                StepDecision::ForceLlm {
                    system: String::new(),
                    prompt: full_prompt,
                }
            }
        }
    }

    fn post_step(&mut self, ctx: &PostStepContext) -> StepFlow {
        let step = &self.steps[self.active_step];
        self.step_results.insert(step.id.clone(), ctx.content.clone());
        self.current = self.active_step + 1;
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
