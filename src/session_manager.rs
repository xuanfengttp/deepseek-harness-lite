//! Session manager: multi-session registry with memory offloading + double-page cache.
//!
//! Design (from DESIGN-lite.md):
//! - Single active session in memory at a time
//! - Multi-session sidebar: list of session metadata (id, title, event count, timestamps)
//! - Memory offloading: inactive sessions are evicted to flash; active stays in RAM
//! - Double-page cache: keep the 2 most recently used sessions in memory for
//!   fast switching (avoids flash reload on back-and-forth)
//!
//! Memory budget: with ~6 MB RSS baseline, each in-memory session log costs
//! ~50-200 KB (512 events × ~100-400 bytes each). Two cached sessions add
//! ~400 KB max — well within the 10 MB target.

use crate::session::SessionLog;
use crate::types::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Metadata for a session (stored in the sidebar index, cheap to keep in memory).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    pub id: String,
    /// Human-readable title (first user message or auto-generated).
    pub title: String,
    /// Number of events in the log.
    pub event_count: usize,
    /// Creation timestamp (Unix millis).
    pub created_at: u64,
    /// Last activity timestamp (Unix millis).
    pub updated_at: u64,
    /// Whether this session is currently loaded in memory.
    pub in_memory: bool,
    /// Whether this is the currently active session (the one being viewed).
    pub is_active: bool,
}

/// The session manager owns the session registry, the active session,
/// and the double-page cache.
pub struct SessionManager {
    /// All known sessions (metadata index — always in memory, cheap).
    sessions: HashMap<String, SessionMeta>,
    /// Double-page cache: up to 2 sessions loaded in memory.
    /// Key = session id, Value = the SessionLog.
    cache: HashMap<String, SessionLog>,
    /// The currently active session id.
    active_id: Option<String>,
    /// Flash persistence directory for offloaded sessions.
    persist_dir: PathBuf,
    /// Max events per session log (ring buffer size).
    max_events: usize,
    /// Max sessions in the double-page cache.
    cache_capacity: usize,
}

impl SessionManager {
    /// Create a new session manager.
    pub fn new(persist_dir: &str, max_events: usize) -> Self {
        let persist_dir = PathBuf::from(persist_dir);
        let _ = std::fs::create_dir_all(&persist_dir);

        let mut mgr = Self {
            sessions: HashMap::new(),
            cache: HashMap::new(),
            active_id: None,
            persist_dir,
            max_events,
            cache_capacity: 2,
        };

        // Load existing session metadata index from flash.
        mgr.load_index();
        mgr
    }

    /// Create a new session, making it the active one.
    /// Returns the session id.
    pub fn create(&mut self, title: &str) -> String {
        let id = generate_session_id();
        let now = current_millis();

        let meta = SessionMeta {
            id: id.clone(),
            title: title.to_string(),
            event_count: 0,
            created_at: now,
            updated_at: now,
            in_memory: true,
            is_active: true,
        };

        self.sessions.insert(id.clone(), meta);
        self.cache.insert(id.clone(), SessionLog::new(self.max_events));

        // Evict excess cache entries (keep only cache_capacity).
        self.evict_cache();

        // Set as active.
        self.active_id = Some(id.clone());
        self.save_index();
        log::info!("Session created: {id} ({title})");
        id
    }

    /// Switch to a different session. Loads it from flash if not in cache.
    /// Returns the session id if successful, None if not found.
    pub fn switch(&mut self, id: &str) -> Option<String> {
        if !self.sessions.contains_key(id) {
            log::warn!("Session not found: {id}");
            return None;
        }

        // Checkpoint the current active session to flash BEFORE switching,
        // so its history survives even if it gets evicted from the cache.
        if let Some(old_id) = &self.active_id {
            if old_id != id {
                if let Some(log) = self.cache.get(old_id) {
                    let path = self.session_path(old_id);
                    if let Err(e) = log.checkpoint(&path.to_string_lossy()) {
                        log::warn!("Pre-switch checkpoint failed for {old_id}: {e}");
                    } else {
                        log::info!("Session checkpointed before switch: {old_id}");
                    }
                }
            }
        }

        // If not in cache, load from flash.
        if !self.cache.contains_key(id) {
            let path = self.session_path(id);
            match SessionLog::load(&path.to_string_lossy(), self.max_events) {
                Ok(log) => {
                    log::info!("Session loaded from flash: {id}");
                    self.cache.insert(id.to_string(), log);
                }
                Err(_) => {
                    log::warn!("Session flash load failed, starting fresh: {id}");
                    self.cache.insert(id.to_string(), SessionLog::new(self.max_events));
                }
            }
        }

        // Mark as active BEFORE evicting, so the newly loaded session is
        // never the LRU victim (evict_cache excludes the active session).
        self.active_id = Some(id.to_string());
        self.evict_cache();

        // Update metadata.
        if let Some(meta) = self.sessions.get_mut(id) {
            meta.in_memory = true;
            meta.updated_at = current_millis();
        }

        log::info!("Switched to session: {id}");
        Some(id.to_string())
    }

    /// Rename a session's title.
    pub fn rename(&mut self, id: &str, title: &str) {
        if let Some(meta) = self.sessions.get_mut(id) {
            meta.title = title.to_string();
            meta.updated_at = current_millis();
            self.save_index();
            log::info!("Session renamed: {id} → {title}");
        }
    }

    /// Get the active session log (mutable).
    pub fn active_mut(&mut self) -> Option<&mut SessionLog> {
        let id = self.active_id.as_ref()?;
        self.cache.get_mut(id)
    }

    /// Check if a session is currently in the cache.
    pub fn is_cached(&self, id: &str) -> bool {
        self.cache.contains_key(id)
    }

    /// Get the active session's messages (for history display).
    pub fn active_messages(&self) -> Option<Vec<crate::types::Message>> {
        let id = self.active_id.as_ref()?;
        self.cache.get(id).map(|log| log.derive_messages())
    }

    /// Get the active session log (read-only).
    pub fn active(&self) -> Option<&SessionLog> {
        let id = self.active_id.as_ref()?;
        self.cache.get(id)
    }

    /// Get the active session id.
    pub fn active_id(&self) -> Option<&str> {
        self.active_id.as_deref()
    }

    /// List all sessions (metadata), sorted by last activity (most recent first).
    pub fn list(&self) -> Vec<SessionMeta> {
        let active = self.active_id.clone();
        let mut sessions: Vec<SessionMeta> = self.sessions.values().map(|m| {
            let mut m = m.clone();
            m.is_active = Some(&m.id) == active.as_ref();
            m
        }).collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        sessions
    }

    /// Close a session (offload to flash, remove from cache).
    /// If it's the active session, active_id is cleared.
    pub fn close(&mut self, id: &str) -> bool {
        if let Some(log) = self.cache.remove(id) {
            // Persist to flash before removing from memory.
            let path = self.session_path(id);
            if let Err(e) = log.checkpoint(&path.to_string_lossy()) {
                log::warn!("Session checkpoint failed: {e}");
            }
        }

        if let Some(meta) = self.sessions.get_mut(id) {
            meta.in_memory = false;
        }

        if self.active_id.as_deref() == Some(id) {
            self.active_id = None;
        }

        self.save_index();
        log::info!("Session closed: {id}");
        true
    }

    /// Delete a session permanently (remove from index + flash).
    pub fn delete(&mut self, id: &str) -> bool {
        self.cache.remove(id);
        self.sessions.remove(id);

        let path = self.session_path(id);
        let _ = std::fs::remove_file(&path);

        if self.active_id.as_deref() == Some(id) {
            self.active_id = None;
        }

        self.save_index();
        log::info!("Session deleted: {id}");
        true
    }

    /// Sync the active session to flash (checkpoint).
    pub fn checkpoint_active(&mut self) {
        if let Some(id) = &self.active_id {
            if let Some(log) = self.cache.get(id) {
                let path = self.session_path(id);
                if let Err(e) = log.checkpoint(&path.to_string_lossy()) {
                    log::warn!("Active session checkpoint failed: {e}");
                }
            }
        }
    }

    /// Update the active session's metadata (event count, timestamp).
    pub fn touch_active(&mut self) {
        if let Some(id) = self.active_id.clone() {
            if let Some(log) = self.cache.get(&id) {
                let count = log.len();
                if let Some(meta) = self.sessions.get_mut(&id) {
                    meta.event_count = count;
                    meta.updated_at = current_millis();
                    meta.in_memory = true;
                }
            }
        }
    }

    /// Evict the least recently used cache entry if over capacity.
    /// The evicted session is checkpointed to flash before removal.
    fn evict_cache(&mut self) {
        while self.cache.len() > self.cache_capacity {
            // Find the LRU session in cache (not the active one).
            let lru_id = self.cache.keys()
                .filter(|id| self.active_id.as_deref() != Some(id.as_str()))
                .min_by_key(|id| {
                    self.sessions.get(*id).map(|m| m.updated_at).unwrap_or(0)
                })
                .cloned();

            if let Some(evict_id) = lru_id {
                if let Some(log) = self.cache.remove(&evict_id) {
                    let path = self.session_path(&evict_id);
                    if let Err(e) = log.checkpoint(&path.to_string_lossy()) {
                        log::warn!("Cache eviction checkpoint failed: {e}");
                    }
                    if let Some(meta) = self.sessions.get_mut(&evict_id) {
                        meta.in_memory = false;
                    }
                    log::info!("Session evicted from cache: {evict_id}");
                }
            } else {
                break; // Can't evict (only active session in cache)
            }
        }
    }

    /// Flash path for a session's checkpoint file.
    fn session_path(&self, id: &str) -> PathBuf {
        self.persist_dir.join(format!("session-{id}.bin"))
    }

    /// Flash path for the metadata index.
    fn index_path(&self) -> PathBuf {
        self.persist_dir.join("session-index.bin")
    }

    /// Save the metadata index to flash.
    fn save_index(&self) {
        let data = bincode::serialize(&self.sessions)
            .unwrap_or_default();
        let _ = std::fs::write(self.index_path(), &data);
    }

    /// Load the metadata index from flash.
    fn load_index(&mut self) {
        let path = self.index_path();
        if !path.exists() {
            return;
        }
        match std::fs::read(&path) {
            Ok(data) => {
                if let Ok(mut sessions) = bincode::deserialize::<HashMap<String, SessionMeta>>(&data) {
                    log::info!("Session index loaded: {} sessions", sessions.len());
                    // All sessions start as not-in-memory and not-active (will set on switch/resume).
                    for meta in sessions.values_mut() {
                        meta.in_memory = false;
                        meta.is_active = false;
                    }
                    self.sessions = sessions;
                }
            }
            Err(e) => log::warn!("Session index load failed: {e}"),
        }
    }

    /// Number of sessions in the registry.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Number of sessions currently in the cache.
    pub fn cached_len(&self) -> usize {
        self.cache.len()
    }

    /// Take the active session out of the cache (for passing to a Dispatcher).
    /// The session metadata stays in the index. Use `return_session` to put it back.
    pub fn take_active(&mut self) -> Option<SessionLog> {
        let id = self.active_id.clone()?;
        self.cache.remove(&id)
    }

    /// Return a session to the cache after a turn.
    pub fn return_session(&mut self, log: SessionLog) {
        if let Some(id) = &self.active_id {
            self.cache.insert(id.clone(), log);
            self.touch_active();
            self.save_index();
        }
    }

    /// Get the active session id (for logging).
    pub fn active_session_id(&self) -> Option<&str> {
        self.active_id.as_deref()
    }
}

/// Generate a unique session id (timestamp + random suffix).
fn generate_session_id() -> String {
    let millis = current_millis();
    let random: u32 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{millis:x}{random:04x}")
}

fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!(".test-sessions-{nanos}")
    }

    #[test]
    fn test_create_and_switch() {
        let dir = temp_dir();
        let mut mgr = SessionManager::new(&dir, 128);

        let id1 = mgr.create("Session 1");
        let id2 = mgr.create("Session 2");

        assert_eq!(mgr.len(), 2);
        assert_eq!(mgr.active_id(), Some(id2.as_str()));

        // Switch back to session 1.
        mgr.switch(&id1);
        assert_eq!(mgr.active_id(), Some(id1.as_str()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_double_page_cache() {
        let dir = temp_dir();
        let mut mgr = SessionManager::new(&dir, 128);

        let id1 = mgr.create("S1");
        let id2 = mgr.create("S2");
        let id3 = mgr.create("S3");

        // Cache capacity is 2, so after creating 3, only 2 should be cached.
        assert!(mgr.cached_len() <= 2);

        // Switching to id1 should load it back.
        mgr.switch(&id1);
        assert!(mgr.cached_len() <= 2);
        assert_eq!(mgr.active_id(), Some(id1.as_str()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_close_and_reload() {
        let dir = temp_dir();
        let mut mgr = SessionManager::new(&dir, 128);

        let id = mgr.create("Test session");
        assert!(mgr.active().is_some());

        mgr.close(&id);
        assert!(mgr.active_id().is_none());
        assert!(mgr.active().is_none());

        // Reload from flash — should find the session in the index.
        let mut mgr2 = SessionManager::new(&dir, 128);
        assert_eq!(mgr2.len(), 1);

        // Switch to it — should load from flash.
        mgr2.switch(&id);
        assert!(mgr2.active().is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_delete() {
        let dir = temp_dir();
        let mut mgr = SessionManager::new(&dir, 128);

        let id = mgr.create("To delete");
        assert_eq!(mgr.len(), 1);

        mgr.delete(&id);
        assert_eq!(mgr.len(), 0);
        assert!(mgr.active_id().is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_sorted_by_recency() {
        let dir = temp_dir();
        let mut mgr = SessionManager::new(&dir, 128);

        let _id1 = mgr.create("Oldest");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _id2 = mgr.create("Newer");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _id3 = mgr.create("Newest");

        let list = mgr.list();
        assert_eq!(list.len(), 3);
        // Most recent first.
        assert_eq!(list[0].title, "Newest");
        assert_eq!(list[2].title, "Oldest");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_persistence_across_restart() {
        let dir = temp_dir();
        let id = {
            let mut mgr = SessionManager::new(&dir, 128);
            let id = mgr.create("Persistent session");
            // Write some events.
            if let Some(log) = mgr.active_mut() {
                log.begin_turn();
                log.append(SessionEvent::UserMessage { content: "hello from session".into() });
                log.end_turn(TurnEndReason::Completed);
            }
            mgr.checkpoint_active();
            id
        };

        // New manager instance — should reload index.
        let mut mgr2 = SessionManager::new(&dir, 128);
        assert_eq!(mgr2.len(), 1);

        // Switch to the session — should load from flash with events.
        mgr2.switch(&id);
        let log = mgr2.active().unwrap();
        assert!(log.len() > 0);

        let _ = fs::remove_dir_all(&dir);
    }
}
