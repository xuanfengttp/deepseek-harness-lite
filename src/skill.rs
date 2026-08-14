//! Skill loader: parse YAML frontmatter + Markdown body from skill files.
//!
//! Mirrors dsh `skill/skill` + `skill/skill-filesystem`, drastically simplified.
//! No file watchers, no scope chains, no provider registry — just scan a
//! directory, parse each file, and return a list of skills.
//!
//! Supported file formats (Claude/dsh-compatible):
//! - Flat: `<name>.md` — frontmatter + body in one file
//! - Directory: `<name>/SKILL.md` — same format inside a directory
//!
//! Frontmatter fields (YAML between `---` delimiters):
//! - name (required, kebab-case)
//! - description (required)
//! - whenToUse (optional)
//! - mode (optional: workflow | todo | plan, default: plan)
//! - think (optional: bool, default: false)
//! - tools.allow (optional: list of tool names)
//! - variables (optional: key-value map)
//! - steps (optional: list of step definitions, for workflow/todo modes)

use crate::types::*;
use std::collections::HashMap;
use std::path::Path;
use yaml_rust2::YamlLoader;

/// Load all skills from a directory.
///
/// Scans for `*.md` files and `*/SKILL.md` directories. Returns skills sorted
/// by name. Invalid files are skipped with a warning (fail-loud per-file, not
/// fatal to the whole scan).
pub fn load_dir(dir: &str) -> Vec<Skill> {
    let path = Path::new(dir);
    if !path.exists() {
        log::warn!("Skill directory does not exist: {dir}");
        return Vec::new();
    }
    if !path.is_dir() {
        log::warn!("Skill path is not a directory: {dir}");
        return Vec::new();
    }

    let mut skills = Vec::new();

    // Scan for flat .md files (not SKILL.md, those are inside directories).
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_file() && entry_path.extension().map(|e| e == "md").unwrap_or(false) {
                match parse_skill_file(&entry_path) {
                    Ok(skill) => skills.push(skill),
                    Err(e) => log::warn!("Skipping skill file {}: {e}", entry_path.display()),
                }
            }
            // Also check for directory/SKILL.md
            if entry_path.is_dir() {
                let skill_file = entry_path.join("SKILL.md");
                if skill_file.exists() {
                    match parse_skill_file(&skill_file) {
                        Ok(skill) => skills.push(skill),
                        Err(e) => log::warn!("Skipping skill dir {}: {e}", entry_path.display()),
                    }
                }
            }
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    log::info!("Loaded {} skill(s) from {dir}", skills.len());
    skills
}

/// Parse a single skill file (frontmatter + Markdown body).
pub fn parse_skill_file(path: &Path) -> Result<Skill, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("read error: {e}"))?;

    parse_skill_content(&content, path)
}

/// Parse skill content into a Skill struct.
fn parse_skill_content(content: &str, path: &Path) -> Result<Skill, String> {
    let (frontmatter, body) = split_frontmatter(content);

    // Parse YAML frontmatter.
    let docs = YamlLoader::load_from_str(&frontmatter)
        .map_err(|e| format!("YAML parse error: {e}"))?;
    let meta = docs.into_iter().next()
        .ok_or_else(|| "empty frontmatter".to_string())?;

    // Extract name — use frontmatter, fall back to filename.
    let name = meta["name"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "unnamed".into())
        });

    let description = meta["description"]
        .as_str()
        .ok_or_else(|| "missing required field: description".to_string())?
        .to_string();

    let when_to_use = meta["whenToUse"].as_str().map(String::from);

    let mode = parse_mode(meta["mode"].as_str().unwrap_or("plan"));
    let think = meta["think"].as_bool().unwrap_or(false);

    // Parse tools.allow list.
    let tools_allow = parse_string_list(&meta["tools"]["allow"]);

    // Parse variables map.
    let variables = parse_variables(&meta["variables"]);

    // Parse steps (for workflow/todo modes).
    let steps = parse_steps(&meta["steps"]);

    Ok(Skill {
        name,
        description,
        when_to_use,
        mode,
        think,
        tools_allow,
        variables,
        body: body.trim().to_string(),
        steps,
    })
}

/// Split content into YAML frontmatter and Markdown body.
/// Frontmatter is delimited by `---` at the start and after the YAML block.
fn split_frontmatter(content: &str) -> (String, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (String::new(), content.to_string());
    }

    // Find the closing `---`.
    let after_first = &trimmed[3..]; // skip opening ---
    if let Some(end) = after_first.find("\n---") {
        let frontmatter = after_first[..end].trim().to_string();
        let body_start = end + 4; // skip \n---
        let body = after_first[body_start..].trim_start_matches('\n').to_string();
        (frontmatter, body)
    } else {
        (String::new(), content.to_string())
    }
}

fn parse_mode(s: &str) -> ExecMode {
    match s.to_lowercase().as_str() {
        "workflow" => ExecMode::Workflow,
        "todo" => ExecMode::Todo,
        _ => ExecMode::Plan,
    }
}

fn parse_string_list(yaml: &yaml_rust2::Yaml) -> Vec<String> {
    if let Some(vec) = yaml.as_vec() {
        vec.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        Vec::new()
    }
}

fn parse_variables(yaml: &yaml_rust2::Yaml) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    if let Some(hash) = yaml.as_hash() {
        for (key, value) in hash {
            if let (Some(k), Some(v)) = (key.as_str(), value.as_str()) {
                vars.insert(k.to_string(), v.to_string());
            }
        }
    }
    vars
}

fn parse_steps(yaml: &yaml_rust2::Yaml) -> Vec<SkillStep> {
    let mut steps = Vec::new();
    if let Some(vec) = yaml.as_vec() {
        for step_yaml in vec {
            let id = step_yaml["id"].as_str().unwrap_or("step").to_string();
            let when = step_yaml["when"].as_str().map(String::from);

            let action = if let Some(tool) = step_yaml["tool"].as_str() {
                let args = step_yaml["args"].clone();
                StepAction::Tool { tool: tool.to_string(), args: yaml_to_json(&args) }
            } else if let Some(prompt) = step_yaml["llm_judge"].as_str() {
                let input = step_yaml["input"].as_str().unwrap_or("").to_string();
                StepAction::LlmJudge { prompt: prompt.to_string(), input }
            } else {
                // Skip steps without a recognized action.
                continue;
            };

            steps.push(SkillStep { id, action, when });
        }
    }
    steps
}

/// Convert a Yaml value to a serde_json::Value.
fn yaml_to_json(yaml: &yaml_rust2::Yaml) -> serde_json::Value {
    match yaml {
        yaml_rust2::Yaml::Null => serde_json::Value::Null,
        yaml_rust2::Yaml::Boolean(b) => serde_json::Value::Bool(*b),
        yaml_rust2::Yaml::Integer(i) => serde_json::Value::Number((*i).into()),
        yaml_rust2::Yaml::Real(s) => s.parse::<f64>().ok()
            .map(|f| serde_json::Value::from(f))
            .unwrap_or(serde_json::Value::Null),
        yaml_rust2::Yaml::String(s) => serde_json::Value::String(s.clone()),
        yaml_rust2::Yaml::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(yaml_to_json).collect())
        }
        yaml_rust2::Yaml::Hash(hash) => {
            let mut map = serde_json::Map::new();
            for (k, v) in hash {
                if let Some(key) = k.as_str() {
                    map.insert(key.to_string(), yaml_to_json(v));
                }
            }
            serde_json::Value::Object(map)
        }
        yaml_rust2::Yaml::Alias(_) | yaml_rust2::Yaml::BadValue => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let content = "---\nname: test-skill\ndescription: A test skill\nmode: plan\nthink: true\ntools:\n  allow: [shell, file_read]\n---\n# Test Skill\n\nYou are a test agent.\n";
        let skill = parse_skill_content(content, Path::new("test-skill.md")).unwrap();
        assert_eq!(skill.name, "test-skill");
        assert_eq!(skill.description, "A test skill");
        assert_eq!(skill.mode, ExecMode::Plan);
        assert!(skill.think);
        assert_eq!(skill.tools_allow, vec!["shell", "file_read"]);
        assert!(skill.body.contains("test agent"));
    }

    #[test]
    fn parses_workflow_steps() {
        let content = "---\nname: health-check\ndescription: Health check\nmode: workflow\nsteps:\n  - id: check_cpu\n    tool: shell\n    args:\n      command: \"top -bn1\"\n  - id: summarize\n    llm_judge: \"Summarize the results\"\n    input: \"{{steps.check_cpu.result}}\"\n---\nBody";
        let skill = parse_skill_content(content, Path::new("health-check.md")).unwrap();
        assert_eq!(skill.mode, ExecMode::Workflow);
        assert_eq!(skill.steps.len(), 2);
        assert!(matches!(skill.steps[0].action, StepAction::Tool { .. }));
        assert!(matches!(skill.steps[1].action, StepAction::LlmJudge { .. }));
    }

    #[test]
    fn parses_variables() {
        let content = "---\nname: test\ndescription: test\nvariables:\n  device: AX-200\n  version: \"1.0\"\n---\nBody";
        let skill = parse_skill_content(content, Path::new("test.md")).unwrap();
        assert_eq!(skill.variables.get("device"), Some(&"AX-200".to_string()));
        assert_eq!(skill.variables.get("version"), Some(&"1.0".to_string()));
    }
}
