//! Long-term memory store: bounded LRU key-value cache with flash persistence.
//!
//! This is the "long-term" layer of the two-layer memory system:
//! - Short-term = session event log (ring buffer, in-memory, checkpointed per P4.4)
//! - Long-term = this module (cross-session, flash-backed, bounded, LRU eviction)
//!
//! Design constraints:
//! - Pure Rust (bincode serialization, no external DB)
//! - Bounded by `max_entries` — LRU eviction when full
//! - Flash persistence: single file, full rewrite on checkpoint (simple, reliable)
//! - Thread-safe via `Mutex` (single-thread tokio runtime, contention is nil)
//! - `memory_recall` does fuzzy key matching (substring + prefix) for human-friendly lookups

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// A single memory entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    /// Unix timestamp of last access (for LRU ordering).
    pub last_access: u64,
    /// Optional category tag for grouping.
    pub category: Option<String>,
}

/// The long-term memory store. Flash-backed, bounded, LRU.
pub struct MemoryStore {
    /// In-memory index: key → entry. The authoritative state.
    entries: Mutex<HashMap<String, MemoryEntry>>,
    /// Bounded capacity.
    max_entries: usize,
    /// Flash persistence path.
    path: PathBuf,
}

impl MemoryStore {
    /// Create a new memory store, loading existing data from flash if present.
    pub fn open(path: &str, max_entries: usize) -> Self {
        let path = PathBuf::from(path);
        let entries = if path.exists() {
            match fs::read(&path) {
                Ok(data) => {
                    let map: HashMap<String, MemoryEntry> = bincode::deserialize(&data)
                        .unwrap_or_else(|e| {
                            log::warn!("Memory store corrupt, starting fresh: {e}");
                            HashMap::new()
                        });
                    log::info!("Memory store loaded: {} entries from {}", map.len(), path.display());
                    map
                }
                Err(e) => {
                    log::warn!("Memory store read failed, starting fresh: {e}");
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };

        Self {
            entries: Mutex::new(entries),
            max_entries,
            path,
        }
    }

    /// Write a key-value pair to memory. If the key exists, update it.
    /// Triggers LRU eviction if at capacity. Persists to flash.
    pub fn write(&self, key: &str, value: &str, category: Option<&str>) {
        let mut entries = self.entries.lock().unwrap();

        let now = current_timestamp();
        entries.insert(key.to_string(), MemoryEntry {
            key: key.to_string(),
            value: value.to_string(),
            last_access: now,
            category: category.map(String::from),
        });

        // LRU eviction if over capacity.
        if entries.len() > self.max_entries {
            let to_remove = entries.len() - self.max_entries;
            let mut by_age: Vec<(String, u64)> = entries.iter()
                .map(|(k, e)| (k.clone(), e.last_access))
                .collect();
            by_age.sort_by_key(|(_, age)| *age);
            for (k, _) in by_age.into_iter().take(to_remove) {
                entries.remove(&k);
                log::debug!("Memory LRU evicted: {k}");
            }
            log::info!("Memory evicted {to_remove} entries (capacity {})", self.max_entries);
        }

        drop(entries);
        self.persist();
    }

    /// Read a value by exact key. Updates last_access (LRU touch).
    pub fn read(&self, key: &str) -> Option<String> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(key) {
            entry.last_access = current_timestamp();
            let value = entry.value.clone();
            drop(entries);
            self.persist();
            Some(value)
        } else {
            None
        }
    }

    /// Recall entries by fuzzy matching (substring or prefix on key).
    /// Returns all matching entries, sorted by last_access descending.
    /// Does NOT touch LRU (read-only scan).
    pub fn recall(&self, query: &str) -> Vec<MemoryEntry> {
        let entries = self.entries.lock().unwrap();
        let query_lower = query.to_lowercase();
        let mut matches: Vec<MemoryEntry> = entries.values()
            .filter(|e| {
                e.key.to_lowercase().contains(&query_lower)
                    || e.value.to_lowercase().contains(&query_lower)
            })
            .cloned()
            .collect();
        matches.sort_by(|a, b| b.last_access.cmp(&a.last_access));
        matches
    }

    /// Delete a key. Persists to flash.
    #[allow(dead_code)]
    pub fn delete(&self, key: &str) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let removed = entries.remove(key).is_some();
        drop(entries);
        if removed {
            self.persist();
        }
        removed
    }

    /// Number of entries currently stored.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Check if empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().is_empty()
    }

    /// Persist the full state to flash (single-file rewrite).
    fn persist(&self) {
        let entries = self.entries.lock().unwrap();
        match bincode::serialize(&*entries) {
            Ok(data) => {
                // Ensure parent directory exists before writing.
                if let Some(parent) = self.path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if let Err(e) = fs::write(&self.path, &data) {
                    log::warn!("Memory persist failed: {e}");
                }
            }
            Err(e) => log::warn!("Memory serialize failed: {e}"),
        }
    }
}

/// Current timestamp (milliseconds) for fine-grained LRU ordering.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!(".test-memory-{nanos}.bin")
    }

    #[test]
    fn test_write_and_read() {
        let path = temp_path();
        {
            let store = MemoryStore::open(&path, 100);
            store.write("device_model", "AX-200", Some("hardware"));
            assert_eq!(store.read("device_model"), Some("AX-200".into()));
            assert_eq!(store.read("nonexistent"), None);
            assert_eq!(store.len(), 1);
        }
        // Reload from flash.
        let store2 = MemoryStore::open(&path, 100);
        assert_eq!(store2.read("device_model"), Some("AX-200".into()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_lru_eviction() {
        let path = temp_path();
        let store = MemoryStore::open(&path, 3);
        store.write("a", "1", None);
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.write("b", "2", None);
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.write("c", "3", None);
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.write("d", "4", None); // should evict "a" (oldest)

        assert_eq!(store.read("a"), None);
        assert_eq!(store.read("b"), Some("2".into()));
        assert_eq!(store.len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_recall_fuzzy() {
        let path = temp_path();
        let store = MemoryStore::open(&path, 100);
        store.write("interface_eth0", "down", Some("network"));
        store.write("interface_eth1", "up", Some("network"));
        store.write("disk_usage", "85%", Some("storage"));

        let results = store.recall("interface");
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|e| e.key.contains("interface")));

        let results = store.recall("down");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "interface_eth0");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_delete() {
        let path = temp_path();
        let store = MemoryStore::open(&path, 100);
        store.write("temp", "value", None);
        assert!(store.delete("temp"));
        assert!(!store.delete("temp"));
        assert_eq!(store.read("temp"), None);
        let _ = std::fs::remove_file(&path);
    }
}
