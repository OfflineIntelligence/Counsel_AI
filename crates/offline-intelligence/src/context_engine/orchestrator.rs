//! Main orchestrator that coordinates all memory subsystems

use crate::memory::Message;
use crate::memory_db::MemoryDatabase;
use crate::context_engine::{
    retrieval_planner::RetrievalPlanner,
    tier_manager::{TierManager, TierManagerConfig},
};

use std::sync::Arc;
use tracing::info;
use tokio::sync::RwLock;

/// Main orchestrator for the context engine
/// History passes this share of the budget: compact.
const COMPACT_AT_PCT: usize = 80;
/// After compacting, history plus memory fits in this share, leaving room
/// for many more turns before the next compaction.
const COMPACT_TO_PCT: usize = 45;
/// Budget share for the extractive summary of compacted turns.
const SUMMARY_PCT: usize = 10;
/// Budget share for recalled question-and-answer pairs.
const RECALL_PCT: usize = 15;
/// Floors so small context windows still get a usable summary and at least
/// one full recalled exchange; the engine token check trims if needed.
const MIN_SUMMARY_CHARS: usize = 900;
const MIN_RECALL_CHARS: usize = 1_400;

/// A conversation fitted into the prompt, plus the memory blocks that were
/// added (for the echo guard and the citation check).
pub struct ConversationAssembly {
    pub messages: Vec<Message>,
    pub memory_blocks: Vec<String>,
}

pub struct ContextOrchestrator {
    database: Arc<MemoryDatabase>,
    retrieval_planner: Arc<RwLock<RetrievalPlanner>>,
    tier_manager: Arc<RwLock<TierManager>>,
    config: OrchestratorConfig,
    /// Per-session sticky front-truncation boundary. Only moves forward,
    /// with hysteresis - keeps the prompt prefix byte-identical across turns
    /// so llama-server's prompt cache keeps hitting.
    stable_starts: std::sync::RwLock<std::collections::HashMap<String, usize>>,
    /// Per conversation: the form in which each newest message was sent
    /// (question plus retrieved material), resent unchanged on later turns.
    sent_forms: std::sync::RwLock<std::collections::HashMap<String, SentForms>>,
}

/// What a conversation's newest messages looked like when sent, valid while
/// the condensed boundary (`start`) stays where it was.
#[derive(Debug, Clone, Default)]
struct SentForms {
    start: usize,
    /// (message index, its text as stored, its text as sent)
    items: Vec<(usize, String, String)>,
}

/// Configuration for the orchestrator
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    pub enabled: bool,
    pub max_context_tokens: usize,
    pub auto_optimize: bool,
    pub enable_metrics: bool,
    pub session_timeout_seconds: u64,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_context_tokens: 4000,
            auto_optimize: true,
            enable_metrics: true,
            session_timeout_seconds: 3600,
        }
    }
}

impl ContextOrchestrator {
    /// Create a new context orchestrator
    pub async fn new(
        database: Arc<MemoryDatabase>,
        config: OrchestratorConfig,
    ) -> anyhow::Result<Self> {
        // Create retrieval planner wrapped in Arc<RwLock>
        let retrieval_planner = Arc::new(RwLock::new(RetrievalPlanner::new(database.clone())));
        
        // Create tier manager
        let tier_manager_config = TierManagerConfig::default();
        let tier_manager = TierManager::new(
            database.clone(),
            tier_manager_config,
        );
        let tier_manager = Arc::new(RwLock::new(tier_manager));
        
        let orchestrator = Self {
            database,
            retrieval_planner,
            tier_manager,
            config,
            stable_starts: std::sync::RwLock::new(std::collections::HashMap::new()),
            sent_forms: std::sync::RwLock::new(std::collections::HashMap::new()),
        };

        info!("Context orchestrator initialized successfully");

        Ok(orchestrator)
    }

    /// Chat persistence: Expose database for conversation API handlers
    pub fn database(&self) -> &Arc<MemoryDatabase> {
        &self.database
    }
    
    /// Fit a conversation into the prompt budget, backed by the database.
    /// Returns only the messages; see `process_conversation_with_memory`.
    pub async fn process_conversation(
        &self,
        session_id: &str,
        messages: &[Message],
        user_query: Option<&str>,
        max_context_tokens: usize,
    ) -> anyhow::Result<Vec<Message>> {
        Ok(self
            .process_conversation_with_memory(session_id, messages, user_query, max_context_tokens)
            .await?
            .messages)
    }

    /// Fit a conversation into `budget_tokens`, using the database as memory
    /// (context_engine::conversation_memory):
    ///
    /// 1. While history fits under 80% of the budget, every turn is sent as is.
    /// 2. When it passes 80%, the oldest turns leave the prompt (down to 45%,
    ///    cut at a user message) and are condensed, without the LLM, into the
    ///    conversation's stored summary. The cut point is persisted, so the
    ///    prompt's prefix stays identical between compactions and the engine
    ///    keeps reusing its KV cache.
    /// 3. Every turn, dropped turns relevant to the question (by keyword and
    ///    meaning, or by position for "my first question") are recalled as
    ///    question-and-answer pairs and attached to the current message.
    pub async fn process_conversation_with_memory(
        &self,
        session_id: &str,
        messages: &[Message],
        user_query: Option<&str>,
        budget_tokens: usize,
    ) -> anyhow::Result<ConversationAssembly> {
        use super::conversation_memory as memory;

        if !self.config.enabled || messages.is_empty() {
            return Ok(ConversationAssembly { messages: messages.to_vec(), memory_blocks: Vec::new() });
        }
        {
            let tier_manager = self.tier_manager.write().await;
            tier_manager.store_tier1_content(session_id, messages).await;
        }

        let n = messages.len();
        let last = n - 1;
        let sys_end = if messages[0].role == "system" { 1 } else { 0 };
        let budget = budget_tokens.max(512);
        let summary_cap = budget * SUMMARY_PCT / 100;
        let recall_cap = budget * RECALL_PCT / 100;
        let fixed: usize = messages[..sys_end].iter().map(Self::estimate_msg_tokens).sum();
        let tail = |s: usize| -> usize { messages[s..].iter().map(Self::estimate_msg_tokens).sum() };
        let reserve = |s: usize| -> usize { if s > sys_end { summary_cap + recall_cap } else { 0 } };

        // Where the previous compaction left the window. Positions in the
        // request list are message indices in the database.
        let (mut covered, mut entries) = memory::load_entries(&self.database, session_id);
        if covered > last {
            // The conversation is shorter than the stored cut (edited or
            // restored); start again from the top.
            covered = 0;
            entries.clear();
        }
        let mut start = Self::snap_to_user_boundary(messages, covered.max(sys_end));

        // Compact at 80%: condense the oldest turns and move the window.
        if fixed + reserve(start) + tail(start) > budget * COMPACT_AT_PCT / 100 {
            let target = budget * COMPACT_TO_PCT / 100;
            let mut s = start;
            while s + 2 < n && fixed + summary_cap + recall_cap + tail(s) > target {
                s += 1;
            }
            s = Self::snap_to_user_boundary(messages, s);
            if s > start {
                let new_entries = memory::extract_entries(messages, start, s);
                entries = memory::merge_entries(entries, new_entries);
                memory::save_entries(&self.database, session_id, s, &entries);
                start = s;
            }
        }

        let truncated = start > sys_end;
        let summary_block = if truncated {
            memory::render_summary(&entries, (summary_cap * 4).max(MIN_SUMMARY_CHARS))
        } else {
            None
        };
        let recall_chars = Self::recall_budget_chars(
            budget,
            recall_cap,
            fixed + summary_block.as_ref().map_or(0, |s| s.len() / 4 + 8) + tail(start),
        );
        let recall_block = match user_query {
            Some(q) if !q.trim().is_empty() => {
                memory::recall_dropped(
                    &self.database,
                    &crate::rag::runtime(),
                    session_id,
                    q,
                    start as i32,
                    last as i32,
                    recall_chars,
                )
                .await
            }
            _ => None,
        };

        let mut out: Vec<Message> = Vec::with_capacity(n - start + 2);
        let mut system = if sys_end == 1 { messages[0].content.clone() } else { String::new() };
        if let Some(ref s) = summary_block {
            // In the system message, which only changes at a compaction, so
            // the cached prefix survives every turn in between.
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(s);
        }
        if truncated && summary_block.is_none() {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str("[Note: earlier parts of this conversation are not shown; they are retrieved from the saved record when a question needs them.]");
        }
        if !system.is_empty() {
            out.push(Message { role: "system".to_string(), content: system });
        }
        if start < last {
            out.extend_from_slice(&messages[start..last]);
        }
        let mut last_msg = messages[last].clone();
        if let Some(ref r) = recall_block {
            super::document_memory::fold_into_latest_message(std::slice::from_mut(&mut last_msg), r);
        }
        out.push(last_msg);

        self.resend_as_sent(session_id, messages, start, last, budget, &mut out);

        if truncated || recall_block.is_some() {
            info!(
                "Conversation {}: {} of {} messages in view, summary {} chars, recall {} chars",
                session_id,
                n - start,
                n,
                summary_block.as_ref().map_or(0, |s| s.len()),
                recall_block.as_ref().map_or(0, |s| s.len())
            );
        }
        let memory_blocks = [summary_block, recall_block].into_iter().flatten().collect();
        Ok(ConversationAssembly { messages: out, memory_blocks })
    }

    /// Resend earlier newest-messages exactly as they were sent, so the
    /// engine's cached prefix covers them instead of breaking at the previous
    /// question; then remember how this turn's newest message was sent.
    ///
    /// The remembered forms are dropped when the condensed boundary moves (a
    /// compaction already changes the prefix) and, all at once, when resending
    /// them would push the prompt past the budget.
    fn resend_as_sent(&self, session_id: &str, messages: &[Message], start: usize, last: usize, budget: usize, out: &mut [Message]) {
        let Ok(mut all) = self.sent_forms.write() else { return };
        let forms = all.entry(session_id.to_string()).or_default();
        if forms.start != start {
            forms.items.clear();
            forms.start = start;
        }
        // Where messages[start..] begin in `out` (after the system message, if any).
        let offset = out.len().saturating_sub(last + 1 - start);
        let replace: Vec<(usize, &String)> = forms
            .items
            .iter()
            .filter(|(i, raw, _)| *i >= start && *i < last && messages.get(*i).map(|m| &m.content) == Some(raw))
            .map(|(i, _, sent)| (offset + i - start, sent))
            .collect();
        let used: usize = out.iter().map(Self::estimate_msg_tokens).sum();
        let extra: usize = replace
            .iter()
            .map(|(pos, sent)| (sent.len() / 4).saturating_sub(out[*pos].content.len() / 4))
            .sum();
        if used + extra <= budget {
            let swaps: Vec<(usize, String)> = replace.into_iter().map(|(p, s)| (p, s.clone())).collect();
            for (pos, sent) in swaps {
                out[pos].content = sent;
            }
        } else {
            forms.items.clear();
        }
        forms.items.retain(|(i, _, _)| *i < last);
        if let Some(sent) = out.last() {
            if sent.content != messages[last].content {
                forms.items.push((last, messages[last].content.clone(), sent.content.clone()));
            }
        }
    }

    /// Characters for recalled exchanges: the budget the prompt leaves unused
    /// (less a tenth for estimation error), never below the reserved share.
    /// Recall sits in the newest message, so a larger block never disturbs
    /// the cached prefix; the reserve used for compaction stays fixed.
    fn recall_budget_chars(budget: usize, reserved_tokens: usize, used_tokens: usize) -> usize {
        let free = budget.saturating_sub(used_tokens) * 9 / 10;
        (reserved_tokens.max(free) * 4).max(MIN_RECALL_CHARS)
    }

    fn estimate_msg_tokens(m: &Message) -> usize {
        m.content.len() / 4 + 8
    }

    /// Move `start` forward to the next user message, so the window never
    /// opens on an answer without its question (keeps roles alternating).
    fn snap_to_user_boundary(messages: &[Message], start: usize) -> usize {
        let Some(last) = messages.len().checked_sub(1) else {
            return start;
        };
        let mut s = start;
        while s < last && messages[s].role != "user" {
            s += 1;
        }
        s
    }

    
    pub async fn get_session_stats(&self, session_id: &str) -> anyhow::Result<SessionStats> {
        let tier_manager = self.tier_manager.read().await;
        let tier_stats = tier_manager.get_tier_stats(session_id).await;
        let db_stats = self.database.get_stats()?;
        
        Ok(SessionStats {
            session_id: session_id.to_string(),
            tier_stats,
            database_stats: db_stats,
        })
    }
    
    pub async fn cleanup(&self, older_than_seconds: u64) -> anyhow::Result<CleanupStats> {
        info!("Starting cleanup of old data");
        let db_cleaned = self.database.cleanup_old_data((older_than_seconds / 86400) as i32)?;
        let tier_manager = self.tier_manager.read().await;
        let cache_cleaned = tier_manager.cleanup_cache(older_than_seconds).await;
        
        Ok(CleanupStats {
            sessions_cleaned: db_cleaned,
            cache_entries_cleaned: cache_cleaned,
        })
    }
    
    /// Search messages across sessions by keywords
    pub async fn search_messages(
        &self,
        session_id: Option<&str>,
        keywords: &[String],
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory_db::StoredMessage>> {
        if keywords.is_empty() {
            return Ok(Vec::new());
        }
        
        if let Some(sid) = session_id {
            // Search within specific session
            self.database.conversations.search_messages_by_keywords(sid, keywords, limit).await
        } else {
            // Search across all sessions (would need cross-session search implementation)
            // For now, return empty results for global search
            Ok(Vec::new())
        }
    }
    
    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
        info!("Context engine {}", if enabled { "enabled" } else { "disabled" });
    }
    
    pub fn update_config(&mut self, config: OrchestratorConfig) {
        self.config = config;
        info!("Context engine configuration updated");
    }
    
    pub fn get_config(&self) -> &OrchestratorConfig {
        &self.config
    }

    // Chat persistence: Expose tier manager to ensure sessions exist before processing
}

impl Clone for ContextOrchestrator {
    fn clone(&self) -> Self {
        Self {
            database: self.database.clone(),
            retrieval_planner: self.retrieval_planner.clone(),
            tier_manager: self.tier_manager.clone(),
            config: self.config.clone(),
            stable_starts: std::sync::RwLock::new(
                self.stable_starts.read().unwrap().clone(),
            ),
            sent_forms: std::sync::RwLock::new(self.sent_forms.read().map(|g| g.clone()).unwrap_or_default()),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_engine::conversation_memory::{RECALL_HEADING, SUMMARY_HEADING};

    #[test]
    fn recall_gets_the_budget_the_prompt_leaves_unused() {
        // 4,096-token budget, 1,300 used: recall may use 90% of the 2,796 left.
        assert_eq!(ContextOrchestrator::recall_budget_chars(4096, 614, 1300), (2796 * 9 / 10) * 4);
        // A full prompt still keeps the reserved share.
        assert_eq!(ContextOrchestrator::recall_budget_chars(4096, 614, 4000), 614 * 4);
        assert_eq!(ContextOrchestrator::recall_budget_chars(4096, 614, 9000), 614 * 4);
        // Never below the floor.
        assert_eq!(ContextOrchestrator::recall_budget_chars(512, 10, 512), MIN_RECALL_CHARS);
    }

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    async fn test_orchestrator() -> ContextOrchestrator {
        let db = Arc::new(crate::memory_db::MemoryDatabase::new_in_memory().unwrap());
        ContextOrchestrator::new(db, OrchestratorConfig::default()).await.unwrap()
    }

    const BASE_PROMPT: &str = "<base system prompt supplied by the client>";
    const FILLER: &str = "with enough words in it to take up a realistic share of the estimate tokens.";

    /// Save a conversation the way generate_stream does (message_index =
    /// position in the request list).
    fn save(orch: &ContextOrchestrator, session: &str, messages: &[Message]) {
        let db = orch.database();
        let _ = db.conversations.delete_session(session);
        let _ = db.conversations.create_session_with_id(session, None);
        let rows: Vec<(String, String, i32, i32, f32)> = messages
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, m)| (m.role.clone(), m.content.clone(), i as i32, 0, 0.5))
            .collect();
        db.conversations.store_messages_batch(session, &rows).unwrap();
    }

    fn conversation(turns: usize) -> Vec<Message> {
        let mut messages = vec![msg("system", BASE_PROMPT)];
        for i in 0..turns {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            messages.push(msg(role, &format!("Message {} {}", i, FILLER)));
        }
        messages
    }

    fn assert_shape(out: &[Message], ctx: &str) {
        assert!(!out.is_empty(), "empty prompt ({})", ctx);
        let body_start = if out[0].role == "system" { 1 } else { 0 };
        for (i, m) in out[body_start..].iter().enumerate() {
            assert_ne!(m.role, "system", "system role after position 0 ({})", ctx);
            let expected = if i % 2 == 0 { "user" } else { "assistant" };
            assert_eq!(m.role, expected, "role alternation broke at {} ({})", i + body_start, ctx);
        }
        assert_eq!(out.last().unwrap().role, "user", "latest question missing ({})", ctx);
    }

    #[tokio::test]
    async fn a_short_conversation_is_sent_whole() {
        let orch = test_orchestrator().await;
        let messages = conversation(5);
        save(&orch, "short", &messages);
        let out = orch.process_conversation_with_memory("short", &messages, Some("next"), 4_000).await.unwrap();
        assert_eq!(out.messages.len(), messages.len());
        assert!(out.memory_blocks.is_empty());
        assert_eq!(out.messages[0].content, BASE_PROMPT);
    }

    #[tokio::test]
    async fn passing_80_percent_compacts_into_a_stored_summary() {
        let orch = test_orchestrator().await;
        let messages = conversation(121);
        save(&orch, "long", &messages);
        let out = orch.process_conversation_with_memory("long", &messages, Some("anything"), 2_000).await.unwrap();
        assert!(out.messages.len() < messages.len(), "must compact");
        assert_shape(&out.messages, "compacted");
        assert!(out.messages[0].content.starts_with(BASE_PROMPT));
        assert!(out.messages[0].content.contains(SUMMARY_HEADING), "summary goes in the system message");
        // The shown summary is capped (oldest lines fold into a count, and are
        // recalled on demand); the stored record keeps every compacted turn.
        let (covered, json) = orch.database().conversations.get_memory("long").unwrap().expect("summary stored");
        assert!(covered > 1);
        assert!(json.contains("\"turn\":1,"), "stored summary starts at turn 1");
        let summary_len = out.memory_blocks.first().map_or(0, |s| s.len());
        assert!(summary_len <= 900, "shown summary respects its cap ({} chars)", summary_len);
    }

    #[tokio::test]
    async fn the_prefix_stays_identical_between_compactions() {
        let orch = test_orchestrator().await;
        let mut messages = conversation(121);
        save(&orch, "stable", &messages);
        let out1 = orch.process_conversation_with_memory("stable", &messages, Some("x"), 2_000).await.unwrap();
        messages.push(msg("assistant", "short reply"));
        messages.push(msg("user", "short follow up"));
        let out2 = orch.process_conversation_with_memory("stable", &messages, Some("y"), 2_000).await.unwrap();
        let p1: Vec<&str> = out1.messages[..out1.messages.len() - 1].iter().map(|m| m.content.as_str()).collect();
        let p2: Vec<&str> = out2.messages[..p1.len()].iter().map(|m| m.content.as_str()).collect();
        assert_eq!(p1, p2, "prefix must not change between compactions (KV cache reuse)");
    }

    #[tokio::test]
    async fn a_dropped_turn_is_recalled_as_a_question_and_answer_pair() {
        let orch = test_orchestrator().await;
        let mut messages = conversation(121);
        messages[1].content = "What is the notice period in the Lisbon warehouse lease?".into();
        messages[2].content = "The Lisbon warehouse lease requires ninety days written notice.".into();
        messages.push(msg("assistant", "ok"));
        let question = "Remind me about the Lisbon warehouse notice period";
        messages.push(msg("user", question));
        save(&orch, "recall", &messages);
        let out = orch.process_conversation_with_memory("recall", &messages, Some(question), 2_000).await.unwrap();
        let last = &out.messages.last().unwrap().content;
        assert!(last.contains(RECALL_HEADING), "recall block attached to the question: {}", last);
        assert!(last.contains("ninety days"), "the answer comes back with its question");
        assert!(last.contains("Turn 1"));
        assert_shape(&out.messages, "recall");
    }

    #[tokio::test]
    async fn recalled_material_comes_before_the_question_oldest_first_with_dates() {
        let orch = test_orchestrator().await;
        let mut messages = conversation(121);
        // Two exchanges about the same subject, the later one updating the earlier.
        messages[1].content = "Where is the Lisbon warehouse lease signed?".into();
        messages[2].content = "The Lisbon warehouse lease is signed in Porto.".into();
        messages[9].content = "Update on the Lisbon warehouse lease signing location?".into();
        messages[10].content = "The Lisbon warehouse lease is now signed in Faro instead.".into();
        messages.push(msg("assistant", "ok"));
        let question = "Where is the Lisbon warehouse lease signed now?";
        messages.push(msg("user", question));
        save(&orch, "order", &messages);
        let out = orch.process_conversation_with_memory("order", &messages, Some(question), 2_000).await.unwrap();
        let last = &out.messages.last().unwrap().content;
        assert!(last.ends_with(question), "the question is the last thing the model reads: {}", last);
        let porto = last.find("Porto").expect("earlier exchange recalled");
        let faro = last.find("Faro").expect("later exchange recalled");
        assert!(porto < faro, "exchanges are listed oldest first");
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        assert!(last.contains(&format!("Turn 1, {}", today)), "each exchange carries its date: {}", last);
        assert!(last.contains("the most recent one is current"));
        assert_shape(&out.messages, "order");
    }

    #[tokio::test]
    async fn earlier_turns_are_resent_exactly_as_they_were_sent() {
        let orch = test_orchestrator().await;
        let mut messages = conversation(121);
        messages[1].content = "Where is the Lisbon warehouse lease signed?".into();
        messages[2].content = "The Lisbon warehouse lease is signed in Porto.".into();
        messages.push(msg("assistant", "ok"));
        let q1 = "Remind me where the Lisbon warehouse lease is signed";
        messages.push(msg("user", q1));
        save(&orch, "resend", &messages);
        let turn1 = orch.process_conversation_with_memory("resend", &messages, Some(q1), 2_000).await.unwrap();
        let sent1 = turn1.messages.last().unwrap().content.clone();
        assert!(sent1.len() > q1.len(), "turn 1 carried recalled material");

        messages.push(msg("assistant", "It is signed in Porto."));
        let q2 = "thanks";
        messages.push(msg("user", q2));
        save(&orch, "resend", &messages);
        let turn2 = orch.process_conversation_with_memory("resend", &messages, Some(q2), 2_000).await.unwrap();
        let n = turn2.messages.len();
        assert_eq!(turn2.messages[n - 3].content, sent1, "the previous question is resent as it was sent");
        assert!(turn2.messages[n - 1].content.ends_with(q2), "the new question still ends the prompt");
        assert_shape(&turn2.messages, "resend");
    }

    #[tokio::test]
    async fn resent_forms_never_push_the_prompt_over_budget() {
        let orch = test_orchestrator().await;
        let mut messages = conversation(121);
        messages[1].content = "Where is the Lisbon warehouse lease signed?".into();
        messages[2].content = "The Lisbon warehouse lease is signed in Porto.".into();
        messages.push(msg("assistant", "ok"));
        let q1 = "Remind me where the Lisbon warehouse lease is signed";
        messages.push(msg("user", q1));
        save(&orch, "tight", &messages);
        orch.process_conversation_with_memory("tight", &messages, Some(q1), 2_000).await.unwrap();

        messages.push(msg("assistant", "It is signed in Porto."));
        messages.push(msg("user", "thanks"));
        save(&orch, "tight", &messages);
        // A budget the resent form cannot fit in: the plain text is sent instead.
        let out = orch.process_conversation_with_memory("tight", &messages, Some("thanks"), 512).await.unwrap();
        let est: usize = out.messages.iter().map(ContextOrchestrator::estimate_msg_tokens).sum();
        let resent = out.messages.iter().any(|m| m.content.starts_with("[EARLIER IN THIS CONVERSATION"));
        assert!(!resent || est <= 512, "resending must stay within the budget ({} tokens)", est);
        assert_shape(&out.messages, "tight");
    }

    #[tokio::test]
    async fn my_first_question_is_answered_at_any_length() {
        let orch = test_orchestrator().await;
        let mut messages = conversation(399);
        messages.push(msg("assistant", "ok"));
        messages.push(msg("user", "What was my first question?"));
        save(&orch, "first", &messages);
        let out = orch
            .process_conversation_with_memory("first", &messages, Some("What was my first question?"), 2_000)
            .await
            .unwrap();
        let last = &out.messages.last().unwrap().content;
        assert!(last.contains("Turn 1, ") && last.contains("(your 1st question, the one asked about)"), "{}", last);
        assert!(last.contains("Message 0 "), "the first question's text is present");
        assert_shape(&out.messages, "ordinal");
    }

    /// Sweep of lengths, budgets and one enormous turn: the prompt never
    /// opens on an answer and the question always survives (a broken
    /// alternation is a hard 500 from strict chat templates).
    #[tokio::test]
    async fn assembly_keeps_roles_alternating_under_any_shape() {
        for turns in (3..25usize).step_by(2) {
            for budget in [80usize, 200, 400, 800, 1500] {
                for giant_at in [usize::MAX, 1, 2, 6] {
                    let orch = test_orchestrator().await;
                    let session = format!("s-{}-{}-{}", turns, budget, giant_at);
                    let mut messages = conversation(turns);
                    if giant_at < messages.len() - 1 {
                        messages[giant_at + 1].content = "A very long recital of the whole agreement. ".repeat(120);
                    }
                    save(&orch, &session, &messages);
                    let out = orch
                        .process_conversation_with_memory(&session, &messages, Some("question"), budget)
                        .await
                        .unwrap();
                    assert_shape(&out.messages, &session);
                }
            }
        }
    }

    #[test]
    fn snapping_moves_forward_to_a_user_message_and_never_past_the_last() {
        let messages = vec![
            msg("system", "s"),
            msg("user", "u1"),
            msg("assistant", "a1"),
            msg("user", "u2"),
            msg("assistant", "a2"),
        ];
        assert_eq!(ContextOrchestrator::snap_to_user_boundary(&messages, 1), 1);
        assert_eq!(ContextOrchestrator::snap_to_user_boundary(&messages, 2), 3);
        assert_eq!(ContextOrchestrator::snap_to_user_boundary(&messages, 4), 4);
    }
}

#[derive(Debug, Clone)]
pub struct SessionStats {
    pub session_id: String,
    pub tier_stats: crate::context_engine::tier_manager::TierStats,
    pub database_stats: crate::memory_db::schema::DatabaseStats,
}

#[derive(Debug, Clone)]
pub struct CleanupStats {
    pub sessions_cleaned: usize,
    pub cache_entries_cleaned: usize,
}
