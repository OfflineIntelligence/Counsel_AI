//! Manages the three-tier memory system with robust persistence and indexing

use crate::memory::Message;
use crate::memory_db::{MemoryDatabase, StoredMessage};
use moka::sync::Cache;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Configuration for tier management
#[derive(Debug, Clone)]
pub struct TierManagerConfig {
    pub tier1_max_messages: usize,
}

impl Default for TierManagerConfig {
    fn default() -> Self {
        Self {
            tier1_max_messages: 50,
        }
    }
}

/// Statistics about tier usage
#[derive(Debug, Clone, Default)]
pub struct TierStats {
    pub tier1_count: usize,
    pub tier3_count: usize,
}

pub struct TierManager {
    database: Arc<MemoryDatabase>,
    tier1_cache: Cache<String, (Vec<Message>, Instant)>,
    pub config: TierManagerConfig,
}

impl TierManager {
    pub fn new(
        database: Arc<MemoryDatabase>, 
        config: TierManagerConfig
    ) -> Self {
        Self {
            database,
            tier1_cache: Cache::builder()
                .max_capacity(1000)
                .time_to_idle(Duration::from_secs(3600))
                .build(),
            config,
        }
    }

    // --- Tier 1 (Cache) Methods ---

    pub async fn store_tier1_content(&self, session_id: &str, messages: &[Message]) {
        // Apply tier1 max messages limit
        let messages_to_store = if messages.len() > self.config.tier1_max_messages {
            &messages[messages.len() - self.config.tier1_max_messages..]
        } else {
            messages
        };
        
        self.tier1_cache.insert(session_id.to_string(), (messages_to_store.to_vec(), Instant::now()));
    }

    pub async fn get_tier1_content(&self, session_id: &str) -> Option<Vec<Message>> {
        self.tier1_cache.get(session_id).map(|(m, _)| m)
    }

    // --- Tier 2 removed ---
    //
    // Tier 2 was a summary cache over a `summaries` table that nothing ever
    // wrote (see migration 011, which drops it). The tiers that remain are
    // 1 (in-memory recent messages) and 3 (the message store).

    // --- Tier 3 (Database) Methods ---

    pub async fn get_tier3_content(
        &self, 
        session_id: &str, 
        limit: Option<i32>, 
        offset: Option<i32>
    ) -> anyhow::Result<Vec<StoredMessage>> {
        self.database.conversations.get_session_messages(session_id, limit, offset)
    }

    pub async fn search_tier3_content(
        &self, 
        session_id: &str, 
        query: &str, 
        limit: usize
    ) -> anyhow::Result<Vec<StoredMessage>> {
        let messages = self.database.conversations.get_session_messages(session_id, Some(1000), None)?;
        let query_lower = query.to_lowercase();
        
        let filtered = messages.into_iter()
            .filter(|m| m.content.to_lowercase().contains(&query_lower))
            .take(limit)
            .collect();
        
        Ok(filtered)
    }

    // --- Cross-Session Content Methods ---

    /// Searches across all sessions except the current one based on keyword extraction
    pub async fn search_cross_session_content(
        &self,
        current_session_id: &str,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<StoredMessage>> {
        // Extract keywords from query
        let keywords = self.extract_keywords(query);
        
        if keywords.is_empty() {
            return Ok(vec![]);
        }

        // Search across ALL sessions except current one
        self.database.conversations.search_messages_by_topic_across_sessions(
            &keywords,
            limit,
            Some(current_session_id), // Exclude current session
        ).await
    }

    fn extract_keywords(&self, text: &str) -> Vec<String> {
        let words: Vec<&str> = text.split_whitespace().collect();
        words.iter()
            .filter(|w| w.len() > 3)
            .map(|w| w.to_lowercase())
            .filter(|w| !self.is_stop_word(w))
            .collect()
    }

    fn is_stop_word(&self, word: &str) -> bool {
        let stop_words = [
            "the", "a", "an", "and", "or", "but", "in", "on", "at", "to", "for",
            "of", "with", "by", "is", "am", "are", "was", "were", "be", "been",
            "being", "have", "has", "had", "do", "does", "did", "will", "would",
            "shall", "should", "may", "might", "must", "can", "could",
        ];
        stop_words.contains(&word)
    }

    // --- Maintenance & Stats ---

    pub async fn get_tier_stats(&self, session_id: &str) -> TierStats {
        let tier1_count = self.get_tier1_content(session_id).await
            .map(|m| m.len())
            .unwrap_or(0);
        
        let tier3_count = match self.database.conversations.get_session_messages(session_id, Some(10000), None) {
            Ok(messages) => messages.len(),
            Err(_) => 0,
        };

        TierStats { 
            tier1_count, 
            tier3_count 
        }
    }

    pub async fn cleanup_cache(&self, _older_than_seconds: u64) -> usize {
        let count = self.tier1_cache.entry_count();
        
        // Invalidate entries older than threshold
        // Note: Moka automatically handles TTL, but we force cleanup
        self.tier1_cache.invalidate_all();
        
        count as usize
    }

}

impl Clone for TierManager {
    fn clone(&self) -> Self {
        Self {
            database: self.database.clone(),
            tier1_cache: Cache::builder()
                .max_capacity(1000)
                .time_to_idle(Duration::from_secs(3600))
                .build(),
            config: self.config.clone(),
        }
    }
}