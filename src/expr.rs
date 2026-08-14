//! Condition expression evaluator for skill step `when` clauses.
//!
//! Supports a small expression language for workflow/todo step conditions:
//!
//! - `steps.<id>.result` — references a previous step's result string
//! - `steps.<id>.result contains "text"` — substring check
//! - `steps.<id>.result length > 0` — non-empty check
//! - `steps.<id>.result == "text"` — exact match
//! - `steps.<id>.result != "text"` — inequality
//! - `<expr> and <expr>` — logical and
//! - `<expr> or <expr>` — logical or
//! - `not <expr>` — logical not
//!
//! Unknown references evaluate to empty string. This is intentionally simple —
//! no arbitrary code execution, no regex, no arithmetic beyond length.

use std::collections::HashMap;

/// Evaluate a `when` condition expression.
///
/// Returns `true` if the step should run, `false` if it should be skipped.
pub fn evaluate(expr: &str, step_results: &HashMap<String, String>) -> bool {
    let expr = expr.trim();
    if expr.is_empty() {
        return true;
    }

    // Handle `or` (lowest precedence) by splitting on " or ".
    if let Some(idx) = find_keyword(expr, "or") {
        let left = &expr[..idx];
        let right = &expr[idx + 4..];
        return evaluate(left, step_results) || evaluate(right, step_results);
    }

    // Handle `and`.
    if let Some(idx) = find_keyword(expr, "and") {
        let left = &expr[..idx];
        let right = &expr[idx + 4..];
        return evaluate(left, step_results) && evaluate(right, step_results);
    }

    // Handle `not`.
    if let Some(rest) = expr.strip_prefix("not ") {
        return !evaluate(rest, step_results);
    }

    // Handle comparison expressions.
    evaluate_comparison(expr, step_results)
}

/// Find a top-level keyword (not inside quotes), returning the byte index
/// where the keyword starts.
fn find_keyword(expr: &str, keyword: &str) -> Option<usize> {
    let pattern = format!(" {keyword} ");
    let mut in_string = false;
    let mut i = 0;
    let bytes = expr.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'"' {
            in_string = !in_string;
        } else if !in_string && i + pattern.len() <= expr.len() {
            if &expr[i..i + pattern.len()] == pattern {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Evaluate a single comparison expression.
fn evaluate_comparison(expr: &str, step_results: &HashMap<String, String>) -> bool {
    let expr = expr.trim();

    // Check for operators: contains, length >, length <, ==, !=
    if let Some(idx) = find_operator(expr, "contains") {
        let left = resolve_reference(expr[..idx].trim(), step_results);
        let right = parse_string_literal(expr[idx + 8..].trim());
        return left.contains(&right);
    }

    if let Some(idx) = find_operator(expr, "length >") {
        let left_ref = expr[..idx].trim();
        let value = resolve_reference(left_ref, step_results);
        let right = expr[idx + 8..].trim();
        let threshold: usize = right.parse().unwrap_or(0);
        return value.len() > threshold;
    }

    if let Some(idx) = find_operator(expr, "length >=") {
        let left_ref = expr[..idx].trim();
        let value = resolve_reference(left_ref, step_results);
        let right = expr[idx + 9..].trim();
        let threshold: usize = right.parse().unwrap_or(0);
        return value.len() >= threshold;
    }

    if let Some(idx) = find_operator(expr, "length <") {
        let left_ref = expr[..idx].trim();
        let value = resolve_reference(left_ref, step_results);
        let right = expr[idx + 8..].trim();
        let threshold: usize = right.parse().unwrap_or(0);
        return value.len() < threshold;
    }

    if let Some(idx) = find_operator(expr, "==") {
        let left = resolve_reference(expr[..idx].trim(), step_results);
        let right = parse_string_literal(expr[idx + 2..].trim());
        return left == right;
    }

    if let Some(idx) = find_operator(expr, "!=") {
        let left = resolve_reference(expr[..idx].trim(), step_results);
        let right = parse_string_literal(expr[idx + 2..].trim());
        return left != right;
    }

    // No operator — treat as truthiness check (non-empty reference).
    let value = resolve_reference(expr, step_results);
    !value.is_empty()
}

/// Find an operator in the expression (not inside quotes).
fn find_operator(expr: &str, op: &str) -> Option<usize> {
    let mut in_string = false;
    let mut i = 0;
    let bytes = expr.as_bytes();
    let op_bytes = op.as_bytes();
    while i + op_bytes.len() <= bytes.len() {
        if bytes[i] == b'"' {
            in_string = !in_string;
        } else if !in_string && &expr[i..i + op_bytes.len()] == op {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Resolve a reference like `steps.<id>.result` to its string value.
/// Returns empty string for unknown references.
fn resolve_reference(ref_str: &str, step_results: &HashMap<String, String>) -> String {
    let ref_str = ref_str.trim();

    // Strip surrounding quotes — it's a literal, not a reference.
    if ref_str.starts_with('"') && ref_str.ends_with('"') {
        return ref_str[1..ref_str.len() - 1].to_string();
    }

    // steps.<id>.result
    if let Some(rest) = ref_str.strip_prefix("steps.") {
        if let Some(end) = rest.find('.') {
            let step_id = &rest[..end];
            return step_results.get(step_id).cloned().unwrap_or_default();
        }
        return step_results.get(rest).cloned().unwrap_or_default();
    }

    // Unknown reference — return as-is (might be a bare string).
    ref_str.to_string()
}

/// Parse a string literal (with or without quotes).
fn parse_string_literal(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

// ─── Variable interpolation ────────────────────────────────────────────────

/// Interpolate `{{steps.xxx.result}}` and `{{var}}` in a string.
///
/// - `step_results`: outputs of previous workflow steps
/// - `variables`: skill-level variables (and any extra runtime variables)
///
/// Unknown placeholders are left as-is (visible, not silently stripped).
pub fn interpolate_str(
    text: &str,
    step_results: &HashMap<String, String>,
    variables: &HashMap<String, String>,
) -> String {
    let mut result = text.to_string();

    // Interpolate step results: {{steps.xxx.result}}
    for (step_id, value) in step_results {
        let placeholder = format!("{{{{steps.{step_id}.result}}}}");
        result = result.replace(&placeholder, value);
    }

    // Interpolate variables: {{var}}
    for (key, value) in variables {
        let placeholder = format!("{{{{{key}}}}}");
        result = result.replace(&placeholder, value);
    }

    result
}

/// Interpolate in a JSON value recursively (strings inside objects/arrays).
pub fn interpolate_json(
    value: &serde_json::Value,
    step_results: &HashMap<String, String>,
    variables: &HashMap<String, String>,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(interpolate_str(s, step_results, variables))
        }
        serde_json::Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (k, v) in map {
                new_map.insert(k.clone(), interpolate_json(v, step_results, variables));
            }
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(
                arr.iter()
                    .map(|v| interpolate_json(v, step_results, variables))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_results() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("check".into(), "interface eth0 is down".into());
        m.insert("empty".into(), "".into());
        m.insert("ok".into(), "all systems go".into());
        m
    }

    #[test]
    fn test_empty_expr_is_true() {
        assert!(evaluate("", &HashMap::new()));
        assert!(evaluate("   ", &HashMap::new()));
    }

    #[test]
    fn test_length_check() {
        let r = make_results();
        assert!(evaluate("steps.check.result length > 0", &r));
        assert!(!evaluate("steps.empty.result length > 0", &r));
        assert!(evaluate("steps.empty.result length < 1", &r));
    }

    #[test]
    fn test_contains() {
        let r = make_results();
        assert!(evaluate("steps.check.result contains \"down\"", &r));
        assert!(!evaluate("steps.check.result contains \"up\"", &r));
    }

    #[test]
    fn test_equality() {
        let r = make_results();
        assert!(evaluate("steps.ok.result == \"all systems go\"", &r));
        assert!(evaluate("steps.ok.result != \"down\"", &r));
    }

    #[test]
    fn test_and_or() {
        let r = make_results();
        assert!(evaluate("steps.check.result length > 0 and steps.ok.result length > 0", &r));
        assert!(!evaluate("steps.check.result length > 0 and steps.empty.result length > 0", &r));
        assert!(evaluate("steps.empty.result length > 0 or steps.ok.result length > 0", &r));
    }

    #[test]
    fn test_not() {
        let r = make_results();
        assert!(evaluate("not steps.empty.result length > 0", &r));
        assert!(!evaluate("not steps.check.result length > 0", &r));
    }

    #[test]
    fn test_truthiness() {
        let r = make_results();
        assert!(evaluate("steps.check.result", &r));
        assert!(!evaluate("steps.empty.result", &r));
    }
}
