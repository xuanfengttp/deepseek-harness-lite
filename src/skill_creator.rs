//! Skill Creator tool: AI-assisted skill file generation.
//!
//! Maps to dsh's concept of a skill creator, adapted for the lite plugin system.
//! The main agent calls this tool to generate new skill YAML files with
//! appropriate templates based on the requested mode (workflow/todo/plan).
//!
//! Templates ensure generated skills are compatible with the StepHook architecture:
//! - workflow: Tool steps (deterministic, 0 LLM) + optional LlmJudge steps
//! - todo: step-by-step guidance with LLM execution
//! - plan: autonomous LLM exploration with think=true

use crate::types::{ToolDefinition, ToolResult};
use crate::tools::ToolPlugin;
use std::path::Path;
use serde_json;

/// The skill creator tool plugin.
///
/// Generates `.md` skill files in the skills directory. The LLM provides
/// the skill name, description, mode, and optional step intentions, and the
/// tool writes a well-structured skill file with the correct frontmatter
/// and body template.
pub struct SkillCreatorTool {
    /// Directory where skill files are stored.
    pub skills_dir: String,
}

impl ToolPlugin for SkillCreatorTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "skill_creator".into(),
            description: "Generate a new skill file. Creates a well-structured .md skill file with correct frontmatter and body template based on the requested mode. Use this to create reusable SOPs, diagnostic procedures, or workflow definitions.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill name in kebab-case (e.g. 'interface-health-check'). Will be used as the filename."
                    },
                    "description": {
                        "type": "string",
                        "description": "One-line description of what the skill does"
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["workflow", "todo", "plan"],
                        "description": "Execution mode: workflow=deterministic steps, todo=LLM with step guidance, plan=autonomous LLM exploration"
                    },
                    "when_to_use": {
                        "type": "string",
                        "description": "When this skill should be activated (optional)"
                    },
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of allowed tool names (e.g. [\"shell\", \"file_read\"]). If omitted, all tools are allowed."
                    },
                    "steps_intent": {
                        "type": "string",
                        "description": "For workflow/todo modes: describe the steps in natural language. The tool will generate appropriate step definitions. For plan mode, this is ignored."
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional custom body text for the skill. If omitted, a template body is generated based on the mode."
                    }
                },
                "required": ["name", "description", "mode"]
            }),
            timeout_ms: 10_000,
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let name = args.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let description = args.get("description").and_then(|d| d.as_str()).unwrap_or("");
        let mode = args.get("mode").and_then(|m| m.as_str()).unwrap_or("plan");
        let when_to_use = args.get("when_to_use").and_then(|w| w.as_str());
        let steps_intent = args.get("steps_intent").and_then(|s| s.as_str()).unwrap_or("");
        let custom_body = args.get("body").and_then(|b| b.as_str());

        // Validate name (kebab-case).
        if name.is_empty() {
            return ToolResult {
                content: "Error: `name` is required and must be kebab-case".into(),
                is_error: true,
            };
        }
        if !is_kebab_case(name) {
            return ToolResult {
                content: format!("Error: name `{name}` must be kebab-case (lowercase letters, numbers, hyphens only)"),
                is_error: true,
            };
        }
        if description.is_empty() {
            return ToolResult {
                content: "Error: `description` is required".into(),
                is_error: true,
            };
        }

        // Validate mode.
        let mode = match mode {
            "workflow" | "todo" | "plan" => mode,
            _ => return ToolResult {
                content: format!("Error: mode must be 'workflow', 'todo', or 'plan', got '{mode}'"),
                is_error: true,
            },
        };

        // Parse tools list.
        let tools_list: Vec<String> = args
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        // Generate the skill file content.
        let content = match mode {
            "workflow" => generate_workflow_skill(name, description, when_to_use, &tools_list, steps_intent, custom_body),
            "todo" => generate_todo_skill(name, description, when_to_use, &tools_list, steps_intent, custom_body),
            "plan" => generate_plan_skill(name, description, when_to_use, &tools_list, custom_body),
            _ => unreachable!(),
        };

        // Write the file.
        let filename = format!("{name}.md");
        let filepath = Path::new(&self.skills_dir).join(&filename);

        // Check if file already exists.
        if filepath.exists() {
            return ToolResult {
                content: format!("Error: skill file `{filename}` already exists in {dir}. Use a different name or delete the existing file first.", dir = self.skills_dir),
                is_error: true,
            };
        }

        // Ensure the directory exists.
        if let Err(e) = std::fs::create_dir_all(&self.skills_dir) {
            return ToolResult {
                content: format!("Error: failed to create skills directory: {e}"),
                is_error: true,
            };
        }

        match std::fs::write(&filepath, &content) {
            Ok(()) => {
                log::info!("Skill Creator: wrote {filepath:?} ({} bytes)", content.len());
                ToolResult {
                    content: format!(
                        "Created skill file: {filepath}\n\nName: {name}\nMode: {mode}\nDescription: {description}\n\nThe skill is now available. Restart or reload skills to use it.",
                        filepath = filepath.display()
                    ),
                    is_error: false,
                }
            }
            Err(e) => ToolResult {
                content: format!("Error: failed to write skill file: {e}"),
                is_error: true,
            },
        }
    }
}

/// Check if a string is valid kebab-case.
fn is_kebab_case(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

/// Generate a workflow-mode skill file.
///
/// Workflow mode: deterministic steps with Tool (0 LLM) or LlmJudge (single LLM call).
fn generate_workflow_skill(
    name: &str,
    description: &str,
    when_to_use: Option<&str>,
    tools: &[String],
    steps_intent: &str,
    custom_body: Option<&str>,
) -> String {
    let tools_yaml = if tools.is_empty() {
        String::from("  allow: [shell, file_read, file_search]")
    } else {
        format!("  allow: [{}]", tools.join(", "))
    };

    // Generate steps from the intent description.
    // If no intent provided, generate a placeholder template.
    let steps_yaml = if steps_intent.is_empty() {
        let mut steps = String::new();
        steps.push_str("  - id: step_1\n");
        steps.push_str("    tool: shell\n");
        steps.push_str("    args:\n");
        steps.push_str("      command: \"echo 'Replace with actual command'\"\n");
        steps.push_str("  - id: step_2\n");
        steps.push_str("    tool: shell\n");
        steps.push_str("    args:\n");
        steps.push_str("      command: \"echo 'Replace with actual command'\"\n");
        steps.push_str("  - id: summarize\n");
        steps.push_str("    llm_judge: \"Summarize the results and flag any issues.\"\n");
        steps.push_str("    input: \"{{steps.step_1.result}}\\n{{steps.step_2.result}}\"");
        steps
    } else {
        generate_workflow_steps_from_intent(steps_intent)
    };

    let when_line = when_to_use
        .map(|w| format!("whenToUse: {w}\n"))
        .unwrap_or_default();

    let body: String = match custom_body {
        Some(b) => b.to_string(),
        None => {
            let title = name.replace('-', " ").to_uppercase();
            let mut s = String::new();
            s.push_str(&format!("# {title}\n\n"));
            s.push_str("This skill runs a deterministic workflow. Each step executes a fixed command (0 LLM calls), with an optional LLM judge step at the end to summarize results.\n\n");
            s.push_str("## Steps\n\n");
            s.push_str("The steps are defined in the frontmatter above. Tool steps run deterministically; llm_judge steps make a single LLM call with an independent context (no conversation history).\n\n");
            s.push_str("## Customization\n\n");
            s.push_str("Edit the steps in the frontmatter to match your SOP. Use {{steps.step_id.result}} to reference previous step outputs.");
            s
        }
    };

    format!(
        "---\n\
         name: {name}\n\
         description: {description}\n\
         {when_line}\
         mode: workflow\n\
         think: false\n\
         tools:\n\
         {tools_yaml}\n\
         steps:\n\
         {steps_yaml}\n\
         ---\n\n\
         {body}\n"
    )
}

/// Generate a todo-mode skill file.
fn generate_todo_skill(
    name: &str,
    description: &str,
    when_to_use: Option<&str>,
    tools: &[String],
    steps_intent: &str,
    custom_body: Option<&str>,
) -> String {
    let tools_yaml = if tools.is_empty() {
        String::from("  allow: [shell, file_read, file_write, memory_read, memory_write]")
    } else {
        format!("  allow: [{}]", tools.join(", "))
    };

    let when_line = when_to_use
        .map(|w| format!("whenToUse: {w}\n"))
        .unwrap_or_default();

    let steps_section = if steps_intent.is_empty() {
        String::from(
            "## Suggested steps\n\n\
             1. First step description\n\
             2. Second step description\n\
             3. Final step description\n"
        )
    } else {
        format!("## Suggested steps\n\n{steps_intent}\n")
    };

    let body: String = match custom_body {
        Some(b) => b.to_string(),
        None => {
            let title = name.replace('-', " ").to_uppercase();
            let mut s = String::new();
            s.push_str(&format!("# {title}\n\n"));
            s.push_str("This skill provides step-by-step guidance. The LLM executes each step with awareness of which step it is on, receiving guidance injection before each step.\n\n");
            s.push_str(&steps_section);
            s.push_str("## Notes\n\n");
            s.push_str("The LLM will follow the suggested steps but can adapt based on tool results. Each step result is recorded and the LLM is guided to the next step.");
            s
        }
    };

    format!(
        "---\n\
         name: {name}\n\
         description: {description}\n\
         {when_line}\
         mode: todo\n\
         think: false\n\
         tools:\n\
         {tools_yaml}\n\
         ---\n\n\
         {body}\n"
    )
}

/// Generate a plan-mode skill file.
fn generate_plan_skill(
    name: &str,
    description: &str,
    when_to_use: Option<&str>,
    tools: &[String],
    custom_body: Option<&str>,
) -> String {
    let tools_yaml = if tools.is_empty() {
        String::from("  allow: [shell, file_read, file_write, file_search, memory_read, memory_write]")
    } else {
        format!("  allow: [{}]", tools.join(", "))
    };

    let when_line = when_to_use
        .map(|w| format!("whenToUse: {w}\n"))
        .unwrap_or_default();

    let body: String = match custom_body {
        Some(b) => b.to_string(),
        None => {
            let title = name.replace('-', " ").to_uppercase();
            let mut s = String::new();
            s.push_str(&format!("# {title}\n\n"));
            s.push_str("You are an autonomous diagnostic agent. Explore the problem freely using available tools, form hypotheses, and verify them.\n\n");
            s.push_str("## Approach\n\n");
            s.push_str("1. **Gather information** — use tools to inspect device state\n");
            s.push_str("2. **Form hypotheses** — reason about possible causes\n");
            s.push_str("3. **Verify** — run targeted checks to confirm or rule out\n");
            s.push_str("4. **Report** — provide findings with evidence\n\n");
            s.push_str("## Rules\n\n");
            s.push_str("- Always inspect actual device state before drawing conclusions\n");
            s.push_str("- Report exact values from command output\n");
            s.push_str("- Prefer targeted fixes over broad changes\n");
            s.push_str("- Record findings to long-term memory for future reference");
            s
        }
    };

    format!(
        "---\n\
         name: {name}\n\
         description: {description}\n\
         {when_line}\
         mode: plan\n\
         think: true\n\
         tools:\n\
         {tools_yaml}\n\
         ---\n\n\
         {body}\n"
    )
}

/// Generate workflow steps YAML from a natural language intent.
///
/// Parses simple numbered/step descriptions and generates placeholder
/// step definitions. The user should edit the generated commands.
fn generate_workflow_steps_from_intent(intent: &str) -> String {
    let mut steps = String::new();
    let mut step_num = 1;

    // Split by newlines and look for step-like patterns.
    for line in intent.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Strip leading "1.", "2.", "-", "*" etc.
        let cleaned = line
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-' || c == '*' || c == ' ');
        if cleaned.is_empty() {
            continue;
        }

        steps.push_str(&format!(
            "  - id: step_{step_num}\n    tool: shell\n    args:\n      command: \"# TODO: {cleaned}\"\n"
        ));
        step_num += 1;
    }

    // If no steps were generated, add a placeholder.
    if steps.is_empty() {
        steps.push_str("  - id: step_1\n    tool: shell\n    args:\n      command: \"echo 'TODO: replace with actual command'\"\n");
    }

    // Add a summary llm_judge step.
    steps.push_str("  - id: summarize\n    llm_judge: \"Summarize the results and report any issues found.\"\n    input: \"{{steps.step_1.result}}\"");

    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_kebab_case_valid() {
        assert!(is_kebab_case("interface-health-check"));
        assert!(is_kebab_case("cpu-check"));
        assert!(is_kebab_case("step1"));
    }

    #[test]
    fn is_kebab_case_invalid() {
        assert!(!is_kebab_case(""));
        assert!(!is_kebab_case("InterfaceHealth"));
        assert!(!is_kebab_case("-leading"));
        assert!(!is_kebab_case("trailing-"));
        assert!(!is_kebab_case("double--hyphen"));
        assert!(!is_kebab_case("has space"));
    }

    #[test]
    fn generate_plan_skill_has_correct_frontmatter() {
        let content = generate_plan_skill("test-skill", "Test description", Some("when testing"), &[], None);
        assert!(content.contains("name: test-skill"));
        assert!(content.contains("mode: plan"));
        assert!(content.contains("think: true"));
        assert!(content.contains("whenToUse: when testing"));
    }

    #[test]
    fn generate_workflow_skill_has_steps() {
        let content = generate_workflow_skill(
            "test-workflow",
            "Test workflow",
            None,
            &["shell".to_string()],
            "",
            None,
        );
        assert!(content.contains("mode: workflow"));
        assert!(content.contains("think: false"));
        assert!(content.contains("steps:"));
        assert!(content.contains("step_1"));
        assert!(content.contains("summarize"));
        assert!(content.contains("llm_judge"));
    }

    #[test]
    fn generate_workflow_steps_from_intent_parses_lines() {
        let steps = generate_workflow_steps_from_intent("1. Check CPU usage\n2. Check memory\n3. Check disk");
        assert!(steps.contains("step_1"));
        assert!(steps.contains("step_2"));
        assert!(steps.contains("step_3"));
        assert!(steps.contains("Check CPU usage"));
        assert!(steps.contains("summarize"));
    }

    #[test]
    fn generate_todo_skill_has_guidance() {
        let content = generate_todo_skill("test-todo", "Test todo", None, &[], "1. Do thing A\n2. Do thing B", None);
        assert!(content.contains("mode: todo"));
        assert!(content.contains("Do thing A"));
        assert!(content.contains("Do thing B"));
    }

    #[test]
    fn custom_body_is_used() {
        let content = generate_plan_skill("test", "desc", None, &[], Some("Custom body text here."));
        assert!(content.contains("Custom body text here."));
    }
}
