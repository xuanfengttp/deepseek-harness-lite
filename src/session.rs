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
    /// Only `UserMessage`, `AssistantMessage`, and `ToolResult` contribute.
    /// `AssistantChunk` is trajectory-only (raw stream fidelity for UI) and
    /// does NOT appear in derived history — the assembled `AssistantMessage`
    /// carries the full content.
    pub fn derive_messages(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        // Collect pending tool results to attach after the next assistant message
        // that triggered them. The model expects tool results as separate messages
        // following the assistant message containing the tool calls.
        let mut pending_tool_results: Vec<(CallId, String, bool)> = Vec::new();

        for event in &self.events {
            match event {
                SessionEvent::UserMessage { content } => {
                    messages.push(Message::User { content: content.clone() });
                }
                SessionEvent::AssistantMessage { content, tool_calls, .. } => {
                    messages.push(Message::Assistant {
                        content: content.clone(),
                        tool_calls: tool_calls.clone(),
                    });
                }
                SessionEvent::ToolResult { call_id, content, is_error } => {
                    pending_tool_results.push((call_id.clone(), content.clone(), *is_error));
                }
                _ => {}
            }
        }

        // Append accumulated tool results as Tool messages.
        for (call_id, content, is_error) in pending_tool_results {
            messages.push(Message::Tool { call_id, content, is_error });
        }

        messages
    }

    /// Iterate over all in-memory events (for trajectory / persistence).
    pub fn events(&self) -> impl Iterator<Item = &SessionEvent> {
        self.events.iter()
    }

    /// Current turn number.
    pub fn current_turn(&self) -> u64 {
        self.current_turn
    }

    /// Number of events currently in memory.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Serialize the session log to bytes (bincode) for flash persistence.
    pub fn serialize(&self) -> Vec<u8> {
        let snapshot = SessionSnapshot {
            next_seq: self.next_seq,
            events: self.events.iter().cloned().collect(),
            current_turn: self.current_turn,
            current_step: self.current_step,
        };
        bincode::serialize(&snapshot).unwrap_or_default()
    }

    /// Deserialize from bytes and restore a session log.
    pub fn deserialize(data: &[u8], max_in_memory: usize) -> Option<Self> {
        let snapshot: SessionSnapshot = bincode::deserialize(data).ok()?;
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
        });
        log.append(SessionEvent::ToolResult { call_id: "call_1".into(), content: "output".into(), is_error: false });

        let msgs = log.derive_messages();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[1], Message::Tool { .. }));
    }
}
