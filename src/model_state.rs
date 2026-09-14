//! Last-used model persistence — the "default" model is whatever was used
//! last (mirrors upstream dsh's modelSelection.lastUsed projection): no fixed
//! default is forced; the last effective model survives restarts.

use crate::types::ModelConfig;
use std::path::PathBuf;

/// Path of the last-model state file, inside the session persist dir.
fn state_path() -> PathBuf {
    // Resolve persist dir the same way session_manager does: relative to CWD.
    // Read config.session.persist_dir from the resolved config; fallback ".sessions".
    let dir = crate::load_config_file()
        .map(|c| c.session.persist_dir.clone())
        .unwrap_or_else(|| ".sessions".into());
    PathBuf::from(dir).join("last-model.json")
}

/// Persist the last effective model (atomic: write temp + rename).
pub fn save_last_model(model: &ModelConfig) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(model) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Load the last effective model, if any.
pub fn load_last_model() -> Option<ModelConfig> {
    let path = state_path();
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}
