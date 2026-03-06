use chrono::{DateTime, Utc};
use serde::Serialize;
use specta::Type;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Maximum number of chat entries retained in memory.
const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, Serialize, Type)]
pub struct ChatEntry {
    pub timestamp: DateTime<Utc>,
    pub direction: ChatDirection,
    pub chat_id: i64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ChatDirection {
    Incoming,
    Outgoing,
}

/// Thread-safe ring buffer of recent chat messages.
pub struct ChatLog {
    entries: Mutex<VecDeque<ChatEntry>>,
}

impl ChatLog {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(MAX_ENTRIES)),
        }
    }

    pub fn push(&self, direction: ChatDirection, chat_id: i64, text: String) {
        let entry = ChatEntry {
            timestamp: Utc::now(),
            direction,
            chat_id,
            text,
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= MAX_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn entries(&self) -> Vec<ChatEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}