//! Session log: append-only event log + message derivation + ring buffer.
//!
//! Mirrors dsh `core/session`. The log is the single source of truth for model
//! context: `derive_messages()` projects surface events (User/Assistant/Tool)
//! into the model history. Model-visible ⟺ logged — anything reaching a model
//! request is reconstructable from the log.
//!
//! Memory strategy: a ring buffer holds the most recent N events in memory;
//! older events are checkpointed to flash (P4) and may be released. When
//! compaction runs (P4), older turns are replaced by a summary message.

use crate::types::*;
use std::collections::VecDeque;

/// The append-only session event log with a bounded in-memory ring buffer.
pub struct SessionLog {
    /// Monotonic sequence number for the next appended event.
    next_seq: u64,
    /// Bounded ring buffer of recent events (surface + structural).
    events: VecDeque<SessionEvent>,
    /// Maximum events retained in memory; older ones are evicted (flash in P4).
    max_in_memory: usize,
    /// Current turn counter.
    current_turn: u64,
    /// Current step counter within the turn.
    current_step: u64,
}

impl SessionLog {
    /// Create a new log with the given in-memory capacity.
    pub fn new(max_in_memory: usize) -> Self {
        Self {
            next_seq: 0,
            events: VecDeque::with_capacity(max_in_memory.min(512)),
            max_in_memory,
            current_turn: 0,
            current_step: 0,
        }
    }

    /// Append an event and return its sequence number.
    pub fn append(&mut self, event: SessionEvent) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        // Track turn/step counters for boundary events.
        match &event {
            SessionEvent::TurnStart { turn } => self.current_turn = *turn,
            SessionEvent::StepStart { step, .. } => self.current_step = *step,
            _ => {}
        }
        self.events.push_back(event);
        // Evict oldest if over capacity.
        while self.events.len() > self.max_in_memory {
            self.events.pop_front();
        }
        seq
    }

    /// Begin a new turn, returning the turn number.
    pub fn begin_turn(&mut self) -> u64 {
        self.current_turn += 1;
        self.current_step = 0;
        self.append(SessionEvent::TurnStart { turn: self.current_turn });
        self.current_turn
    }

    /// Clear all events from the log (for the `/clear` command).
    /// Resets turn/step counters and seq, but keeps the same capacity.
    pub fn clear(&mut self) {
        self.events.clear();
        self.next_seq = 0;
        self.current_turn = 0;
        self.current_step = 0;
    }

    /// Estimate total token count of the derived messages.
    /// Uses a rough heuristic: ~4 chars per token for mixed text/JSON.
    pub fn estimated_tokens(&self) -> usize {
        let messages = self.derive_messages();
        let total_chars: usize = messages.iter().map(|m| match m {
            Message::User { content } => content.len(),
            Message::Assistant { content, tool_calls } => {
                content.len() + tool_calls.iter().map(|tc| tc.arguments.to_string().len() + tc.name.len()).sum::<usize>()
            }
            Message::Tool { content, .. } => content.len(),
        }).sum();
        total_chars / 4
    }

    /// End the current turn.
    pub fn end_turn(&mut self, reason: TurnEndReason) {
        self.append(SessionEvent::TurnEnd { turn: self.current_turn, reason });
    }

    /// Begin a new step within the current turn.
    pub fn begin_step(&mut self) -> u64 {
        self.current_step += 1;
        self.append(SessionEvent::StepStart { turn: self.current_turn, step: self.current_step });
        self.current_step
    }

    /// End the current step.
    pub fn end_step(&mut self) {
        self.append(SessionEvent::StepEnd { turn: self.current_turn, step: self.current_step });
    }

    /// Derive the model-facing message history from surface events.
    ///
    /// Events arrive in chronological order: UserMessage, AssistantMessage,
    /// ToolCall, ToolResult, then another AssistantMessage (next step), etc.
    /// Tool results must follow the assistant message that triggered them,
    /// in the correct order — NOT all accumulated at the end.
    pub fn derive_messages(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        // Collect tool results that belong to the current assistant step.
        // They are flushed as Tool messages before the next User/Assistant message.
        let mut pending_tool_results: Vec<(CallId, String, bool)> = Vec::new();

        for event in &self.events {
            match event {
                SessionEvent::UserMessage { content } => {
                    // Flush any pending tool results before the next user message.
                    for (call_id, content, is_error) in pending_tool_results.drain(..) {
                        messages.push(Message::Tool { call_id, content, is_error });
                    }
                    messages.push(Message::User { content: content.clone() });
                }
                SessionEvent::AssistantMessage { content, tool_calls, .. } => {
                    // Flush tool results from the PREVIOUS step before this new
                    // assistant message (tool results belong between steps).
                    for (call_id, content, is_error) in pending_tool_results.drain(..) {
                        messages.push(Message::Tool { call_id, content, is_error });
                    }
                    messages.push(Message::Assistant {
                        content: content.clone(),
                        tool_calls: tool_calls.clone(),
                    });
                }
                SessionEvent::ToolResult { call_id, content, is_error } => {
                    pending_tool_results.push((call_id.clone(), content.clone(), *is_error));
                }
                SessionEvent::CompactionSummary { summary } => {
                    // Flush pending tool results, then emit summary as a user message.
                    for (call_id, content, is_error) in pending_tool_results.drain(..) {
                        messages.push(Message::Tool { call_id, content, is_error });
                    }
                    messages.push(Message::User {
                        content: format!("[Previous conversation summary]\n{summary}"),
                    });
                }
                _ => {}
            }
        }

        // Flush any remaining tool results (e.g. after the last assistant step).
        for (call_id, content, is_error) in pending_tool_results {
            messages.push(Message::Tool { call_id, content, is_error });
        }

        messages
    }

    /// Iterate over all in-memory events (for trajectory / persistence).
    pub fn events(&self) -> impl Iterator<Item = &SessionEvent> {
        self.events.iter()
    }

    /// Apply compaction: replace older events with a summary message.
    ///
    /// Takes a summary string and the number of recent events to keep.
    /// All older events are discarded and replaced with a single
    /// `CompactionSummary` event containing the summary text.
    /// The `keep_recent` most recent events are preserved.
    pub fn apply_compaction(&mut self, summary: String, keep_recent: usize) {
        if self.events.len() <= keep_recent {
            return; // Not enough events to compact.
        }

        // Collect the recent events to keep.
        let recent: Vec<SessionEvent> = self.events
            .iter()
            .rev()
            .take(keep_recent)
            .rev()
            .cloned()
            .collect();

        // Clear and rebuild: summary first, then recent events.
        self.events.clear();
        self.events.push_back(SessionEvent::CompactionSummary { summary });
        for event in recent {
            self.events.push_back(event);
        }

        log::info!(
            "Compaction applied: {} events → 1 summary + {} recent",
            self.events.len(),
            keep_recent
        );
    }

    /// Current turn number.
    #[allow(dead_code)]
    pub fn current_turn(&self) -> u64 {
        self.current_turn
    }

    /// Number of events currently in memory.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Serialize the session log to JSON for flash persistence.
    /// JSON is forward-compatible: #[serde(default)] fields in SessionEvent
    /// and TokenUsage allow old logs to load with defaults for new fields.
    pub fn serialize(&self) -> Vec<u8> {
        let snapshot = SessionSnapshot {
            next_seq: self.next_seq,
            events: self.events.iter().cloned().collect(),
            current_turn: self.current_turn,
            current_step: self.current_step,
        };
        serde_json::to_vec(&snapshot).unwrap_or_default()
    }

    /// Deserialize from JSON and restore a session log.
    /// Falls back to bincode for legacy files saved by older binary versions.
    pub fn deserialize(data: &[u8], max_in_memory: usize) -> Option<Self> {
        // Try JSON first (current format).
        if let Ok(snapshot) = serde_json::from_slice::<SessionSnapshot>(data) {
            return Self::from_snapshot(snapshot, max_in_memory);
        }
        // Fallback: try bincode (legacy format from older binary versions).
        // This handles old files that were saved before the JSON migration.
        // New struct fields get serde defaults, so this should work for most cases.
        if let Ok(snapshot) = bincode::deserialize::<SessionSnapshot>(data) {
            return Self::from_snapshot(snapshot, max_in_memory);
        }
        None
    }

    fn from_snapshot(snapshot: SessionSnapshot, max_in_memory: usize) -> Option<Self> {
        let mut events = VecDeque::with_capacity(snapshot.events.len().min(max_in_memory));
        for e in snapshot.events {
            events.push_back(e);
            while events.len() > max_in_memory {
                events.pop_front();
            }
        }
        Some(Self {
            next_seq: snapshot.next_seq,
            events,
            max_in_memory,
            current_turn: snapshot.current_turn,
            current_step: snapshot.current_step,
        })
    }

    /// Checkpoint the session to a flash file.
    pub fn checkpoint(&self, path: &str) -> Result<(), String> {
        let data = self.serialize();
        std::fs::write(path, &data).map_err(|e| format!("checkpoint write: {e}"))
    }

    /// Load a session from a flash file.
    pub fn load(path: &str, max_in_memory: usize) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("checkpoint read: {e}"))?;
        Self::deserialize(&data, max_in_memory)
            .ok_or_else(|| "checkpoint deserialize failed".into())
    }
}

/// Serializable snapshot of a session log for flash persistence.
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionSnapshot {
    next_seq: u64,
    events: Vec<SessionEvent>,
    current_turn: u64,
    current_step: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_messages_projects_surface_events() {
        let mut log = SessionLog::new(128);
        log.begin_turn();
        log.append(SessionEvent::UserMessage { content: "hello".into() });
        log.begin_step();
        log.append(SessionEvent::AssistantMessage {
            content: "hi there".into(),
            tool_calls: vec![],
            usage: None,
            ttft_ms: 0,
            decode_ms: 0,
        });
        log.end_step();
        log.end_turn(TurnEndReason::Completed);

        let msgs = log.derive_messages();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0], Message::User { .. }));
        assert!(matches!(msgs[1], Message::Assistant { .. }));
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let mut log = SessionLog::new(4);
        for i in 0..10 {
            log.append(SessionEvent::UserMessage { content: format!("msg {i}") });
        }
        assert_eq!(log.len(), 4);
        // Only the last 4 should remain.
        let msgs = log.derive_messages();
        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn tool_results_follow_assistant_message() {
        let mut log = SessionLog::new(128);
        log.append(SessionEvent::AssistantMessage {
            content: "running a command".into(),
            tool_calls: vec![ToolCall { id: "call_1".into(), name: "shell".into(), arguments: serde_json::json!({}) }],
            usage: None,
            ttft_ms: 0,
            decode_ms: 0,
        });
        log.append(SessionEvent::ToolResult { call_id: "call_1".into(), content: "output".into(), is_error: false });

        let msgs = log.derive_messages();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[1], Message::Tool { .. }));
    }

    #[test]
    fn tool_results_interleaved_between_steps() {
        // Multi-step: assistant step1 → tool result → assistant step2 → tool result → assistant final.
        // Tool results must appear AFTER the assistant that triggered them, BEFORE the next assistant.
        let mut log = SessionLog::new(128);
        log.begin_turn();
        log.append(SessionEvent::UserMessage { content: "do two things".into() });
        log.begin_step();
        log.append(SessionEvent::AssistantMessage {
            content: "doing first thing".into(),
            tool_calls: vec![ToolCall { id: "c1".into(), name: "tool_a".into(), arguments: serde_json::json!({}) }],
            usage: None, ttft_ms: 0, decode_ms: 0,
        });
        log.append(SessionEvent::ToolResult { call_id: "c1".into(), content: "result_a".into(), is_error: false });
        log.end_step();
        log.begin_step();
        log.append(SessionEvent::AssistantMessage {
            content: "doing second thing".into(),
            tool_calls: vec![ToolCall { id: "c2".into(), name: "tool_b".into(), arguments: serde_json::json!({}) }],
            usage: None, ttft_ms: 0, decode_ms: 0,
        });
        log.append(SessionEvent::ToolResult { call_id: "c2".into(), content: "result_b".into(), is_error: false });
        log.end_step();
        log.begin_step();
        log.append(SessionEvent::AssistantMessage {
            content: "done".into(),
            tool_calls: vec![],
            usage: None, ttft_ms: 0, decode_ms: 0,
        });
        log.end_step();
        log.end_turn(TurnEndReason::Completed);

        let msgs = log.derive_messages();
        // Expected order:
        // 0: User
        // 1: Assistant("doing first thing", [c1])
        // 2: Tool(c1, "result_a")
        // 3: Assistant("doing second thing", [c2])
        // 4: Tool(c2, "result_b")
        // 5: Assistant("done")
        assert_eq!(msgs.len(), 6);
        assert!(matches!(msgs[0], Message::User { .. }));
        assert!(matches!(msgs[1], Message::Assistant { .. }));
        assert!(matches!(msgs[2], Message::Tool { .. }));
        assert!(matches!(msgs[3], Message::Assistant { .. }));
        assert!(matches!(msgs[4], Message::Tool { .. }));
        assert!(matches!(msgs[5], Message::Assistant { .. }));
    }

    #[test]
    fn clear_empties_log() {
        let mut log = SessionLog::new(128);
        log.append(SessionEvent::UserMessage { content: "hello".into() });
        log.append(SessionEvent::AssistantMessage {
            content: "hi".into(), tool_calls: vec![], usage: None, ttft_ms: 0, decode_ms: 0,
        });
        assert_eq!(log.derive_messages().len(), 2);
        log.clear();
        assert_eq!(log.derive_messages().len(), 0);
        // Can still append after clear.
        log.append(SessionEvent::UserMessage { content: "fresh start".into() });
        assert_eq!(log.derive_messages().len(), 1);
    }

    #[test]
    fn estimated_tokens_positive() {
        let mut log = SessionLog::new(128);
        log.append(SessionEvent::UserMessage { content: "This is a test message with enough characters".into() });
        let tokens = log.estimated_tokens();
        assert!(tokens > 0);
    }

    #[test]
    fn apply_compaction_replaces_old_events() {
        let mut log = SessionLog::new(128);
        // Add 10 user messages.
        for i in 0..10 {
            log.append(SessionEvent::UserMessage { content: format!("msg {i}") });
        }
        assert_eq!(log.len(), 10);

        // Compact: keep 3 recent, replace rest with summary.
        log.apply_compaction("Summary of old messages".into(), 3);

        // Should have: 1 summary + 3 recent = 4 events.
        assert_eq!(log.len(), 4);

        // Derived messages: summary (as User) + 3 recent User messages = 4.
        let msgs = log.derive_messages();
        assert_eq!(msgs.len(), 4);
        // First message should be the summary.
        if let Message::User { content } = &msgs[0] {
            assert!(content.contains("Summary of old messages"));
        } else {
            panic!("Expected first message to be summary User message");
        }
        // Last 3 should be msg 7, 8, 9.
        if let Message::User { content } = &msgs[3] {
            assert_eq!(content, "msg 9");
        } else {
            panic!("Expected last message to be msg 9");
        }
    }

    #[test]
    fn apply_compaction_noop_when_few_events() {
        let mut log = SessionLog::new(128);
        log.append(SessionEvent::UserMessage { content: "only one".into() });
        log.apply_compaction("summary".into(), 5);
        // Should not compact (only 1 event, keep 5).
        assert_eq!(log.len(), 1);
    }
}
