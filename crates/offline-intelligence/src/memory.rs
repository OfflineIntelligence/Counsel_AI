// Server/src/memory.rs

use serde::{Deserialize, Serialize};
use dashmap::DashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

pub trait MemoryStore: Send + Sync {
    fn get_history(&self, session_id: &str) -> Vec<Message>;
    fn add_message(&self, session_id: &str, message: Message);
    fn clear_history(&self, session_id: &str);
}

#[derive(Clone)]
pub struct InMemoryMemoryStore {
    store: Arc<DashMap<String, Vec<Message>>>,
}

impl InMemoryMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for InMemoryMemoryStore {
    fn default() -> Self {
        Self {
            store: Arc::new(DashMap::new()),
        }
    }
}

impl MemoryStore for InMemoryMemoryStore {
    fn get_history(&self, session_id: &str) -> Vec<Message> {
        match self.store.get(session_id) {
            Some(history) => history.clone(),
            None => Vec::new(),
        }
    }

    fn add_message(&self, session_id: &str, message: Message) {
        let mut entry = self.store.entry(session_id.to_string()).or_default();
        entry.push(message);
    }

    fn clear_history(&self, session_id: &str) {
        self.store.remove(session_id);
    }
}
