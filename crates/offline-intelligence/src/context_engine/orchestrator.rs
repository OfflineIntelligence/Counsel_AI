//! Main orchestrator that coordinates all memory subsystems

use crate::memory::Message;
use crate::memory_db::MemoryDatabase;
use crate::context_engine::{
    retrieval_planner::RetrievalPlan,
    retrieval_planner::RetrievalPlanner,
    tier_manager::{TierManager, TierManagerConfig},
};

use std::sync::Arc;
use tracing::{info, debug};
use tokio::sync::RwLock;

/// Main orchestrator for the context engine
pub struct ContextOrchestrator {
    database: Arc<MemoryDatabase>,
    retrieval_planner: Arc<RwLock<RetrievalPlanner>>,
    tier_manager: Arc<RwLock<TierManager>>,
    config: OrchestratorConfig,
    /// Per-session sticky front-truncation boundary. Only moves forward,
    /// with hysteresis - keeps the prompt prefix byte-identical across turns
    /// so llama-server's prompt cache keeps hitting.
    stable_starts: std::sync::RwLock<std::collections::HashMap<String, usize>>,
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
        };

        info!("Context orchestrator initialized successfully");

        Ok(orchestrator)
    }

    /// Chat persistence: Expose database for conversation API handlers
    pub fn database(&self) -> &Arc<MemoryDatabase> {
        &self.database
    }
    
    /// Process conversation and return optimized context
    pub async fn process_conversation(
        &self,
        session_id: &str,
        messages: &[Message],
        user_query: Option<&str>,
        max_context_tokens: usize,
    ) -> anyhow::Result<Vec<Message>> {
        if !self.config.enabled || messages.is_empty() {
            debug!("Context engine disabled or no messages");
            return Ok(messages.to_vec());
        }
        
        info!("Processing conversation for session {} ({} messages)", session_id, messages.len());
        
        // Update current messages in Tier 1
        {
            let tier_manager = self.tier_manager.write().await;
            tier_manager.store_tier1_content(session_id, messages).await;
        }
        
        // User message persistence is handled by stream_api.rs before this is called.
        // Do NOT persist here — the tokio::spawn in stream_api races with this function,
        // causing duplicate user messages in the database.
        
        // Create retrieval plan
        let mut plan = {
            let retrieval_planner = self.retrieval_planner.read().await;
            
            // --- UPDATED CALL ---
            // Detect if the user is referring to past conversations
            let has_past_refs = if let Some(query) = user_query {
                retrieval_planner.has_past_references_in_text(query)
            } else {
                false
            };
            
            // Now create the plan using the detected references and the user query
            retrieval_planner.create_plan(
                session_id,
                messages,
                max_context_tokens,
                user_query,
                has_past_refs, // Passing the reference check to the planner
            ).await?
        };
        
        // A document injected into the CURRENT conversation is the thing
        // being asked about - trawling past sessions for it costs extra
        // keyword queries against the message store and can only add noise.
        // Only an explicit past-reference in the query overrides this.
        let doc_in_context = messages.iter().any(|m| {
            m.content.contains("--- Document:")
                || m.content.contains("[ATTACHED DOCUMENTS - THE AUTHORITATIVE SOURCE FOR THIS ANSWER]")
        });
        if doc_in_context && plan.cross_session_search {
            let query_has_past_refs = match user_query {
                Some(q) => {
                    let planner = self.retrieval_planner.read().await;
                    planner.has_past_references_in_text(q)
                }
                None => false,
            };
            if !query_has_past_refs {
                info!(
                    "Attached document already in context and query has no past \
                     references - skipping cross-session search"
                );
                plan.cross_session_search = false;
            }
        }

        if !plan.needs_retrieval {
            debug!("No retrieval needed, returning current messages");
            return Ok(messages.to_vec());
        }
        
        // Execute retrieval plan across the memory tiers.
        let retrieved_content = self.execute_retrieval_plan(session_id, &plan).await?;

        // === STABLE-PREFIX ASSEMBLY ===
        // The prompt prefix sent to llama-server must stay byte-identical
        // across turns, or the server's prompt cache misses and the ENTIRE
        // context (including any attached document) re-prefills before the
        // first token. Three rules:
        //   1. Original messages keep their order - nothing is reordered.
        //   2. Retrieved memory goes into ONE block positioned just before
        //      the latest user message (the tail - cheap to prefill).
        //   3. Front-truncation, when unavoidable, uses a sticky per-session
        //      boundary with hysteresis so the prefix holds until the next
        //      overflow.
        let retrieved_texts = Self::flatten_retrieved(&retrieved_content, messages);
        let optimized_context = self.assemble_stable_context(
            session_id,
            messages,
            retrieved_texts,
            max_context_tokens,
        );

        info!(
            "Context optimization complete: {} -> {} messages (stable-prefix assembly)",
            messages.len(),
            optimized_context.len()
        );

        Ok(optimized_context)
    }
    
    /// Rough token estimate (chars/4 plus per-message overhead).
    fn estimate_msg_tokens(m: &Message) -> usize {
        m.content.len() / 4 + 8
    }

    /// Move a front-truncation boundary forward to the next user message.
    ///
    /// # Why this is a correctness requirement, not a tidy-up
    ///
    /// The assembled prompt is `[system] + messages[start..]`, so whatever
    /// `start` points at becomes the FIRST turn after the system message. Chat
    /// templates with strict role alternation — gemma-3, mistral and qwen all
    /// qualify — reject a prompt whose first turn is an assistant message with
    /// a hard HTTP 500 ("Conversation roles must alternate ..."). The request
    /// fails outright; it is not a degraded answer.
    ///
    /// The budget loop that produces `start` stops wherever the token
    /// arithmetic happens to land, and has no reason to prefer a user message.
    /// Before this existed the invariant held only by luck of message sizes,
    /// which is exactly the kind of bug that survives testing and then appears
    /// three or four turns into a real conversation.
    ///
    /// Never advances past the final message: the last message is always kept,
    /// so a conversation with no user message after `start` still produces a
    /// valid `[system] + [last]` prompt.
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

    /// Flatten retrieved tiers into deduplicated text blocks, skipping
    /// anything already present verbatim in the current conversation.
    /// Tier 1 is deliberately ignored - it IS the current conversation.
    fn flatten_retrieved(retrieved: &RetrievedContent, current: &[Message]) -> Vec<String> {
        let in_current: std::collections::HashSet<&str> =
            current.iter().map(|m| m.content.as_str()).collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut texts: Vec<String> = Vec::new();

        for (stored, label) in [
            (&retrieved.tier3, "earlier in this conversation"),
            (&retrieved.cross_session, "from another conversation"),
        ] {
            if let Some(msgs) = stored {
                for m in msgs {
                    let t = m.content.trim();
                    if t.is_empty() || in_current.contains(t) || !seen.insert(t.to_string()) {
                        continue;
                    }
                    texts.push(format!("[{} - {}]: {}", label, m.role, t));
                }
            }
        }

        texts
    }

    /// Prompt-cache-friendly context assembly (see process_conversation for
    /// the rules). The per-session truncation boundary is sticky: it only
    /// moves forward, with hysteresis to 70% of budget, so the byte prefix
    /// llama-server sees stays identical across turns.
    fn assemble_stable_context(
        &self,
        session_id: &str,
        messages: &[Message],
        retrieved_texts: Vec<String>,
        budget: usize,
    ) -> Vec<Message> {
        // Retrieved-memory block, capped at ~25% of the token budget
        // (budget/4 tokens = budget chars at ~4 chars/token).
        let cap_chars = budget;
        let mut retrieved_block = String::new();
        for t in retrieved_texts {
            if retrieved_block.len() + t.len() + 1 > cap_chars {
                retrieved_block
                    .push_str("[Further retrieved context omitted to fit the context window.]\n");
                break;
            }
            retrieved_block.push_str(&t);
            retrieved_block.push('\n');
        }
        let retrieved_tokens = retrieved_block.len() / 4 + 16;

        let n = messages.len();
        if n == 0 {
            return Vec::new();
        }
        let sys_end = if messages[0].role == "system" { 1 } else { 0 };
        let fixed_tokens: usize = messages[..sys_end]
            .iter()
            .map(Self::estimate_msg_tokens)
            .sum::<usize>()
            + retrieved_tokens;
        let tail_tokens =
            |s: usize| -> usize { messages[s..].iter().map(Self::estimate_msg_tokens).sum() };

        let mut start = self
            .stable_starts
            .read()
            .unwrap()
            .get(session_id)
            .copied()
            .unwrap_or(sys_end);
        if start < sys_end || start >= n {
            start = sys_end;
        }
        // A boundary restored from a previous turn is re-checked, not trusted.
        start = Self::snap_to_user_boundary(messages, start);

        if fixed_tokens + tail_tokens(start) > budget {
            // Advance to ~70% of budget so the boundary then holds for many
            // turns (hysteresis); always keep at least the last two messages.
            let target = budget.saturating_mul(7) / 10;
            let mut s = start;
            while s + 2 < n && fixed_tokens + tail_tokens(s) > target {
                s += 1;
            }
            // The budget loop stops wherever the arithmetic lands, which may be
            // an assistant message. Snapping forward to the next user message
            // is what keeps the emitted prompt alternating; see
            // `snap_to_user_boundary` for why that is a hard requirement and
            // not a nicety. Moving forward only ever removes tokens, so it
            // cannot push the prompt back over budget.
            s = Self::snap_to_user_boundary(messages, s);
            if s != start {
                info!(
                    "Context window exceeded - front-truncation boundary moved {} -> {} \
                     (sticky; prefix stays cacheable until the next overflow)",
                    start, s
                );
                start = s;
                self.stable_starts
                    .write()
                    .unwrap()
                    .insert(session_id.to_string(), start);
            }
        }

        let truncated = start > sys_end;
        let last = n - 1;
        let mut out: Vec<Message> = Vec::with_capacity(n);

        // Fold the truncation note into the system message's own TEXT rather
        // than inserting an extra system-role message. Verified live against
        // gemma-3's chat template (b8037): a second system-role message, or
        // any break in strict user/assistant alternation, throws a Jinja
        // "Conversation roles must alternate" error and the request fails
        // outright - not a degraded response, a hard 500. Folding into
        // existing message content changes no role, ever.
        if sys_end == 1 {
            let mut sys_content = messages[0].content.clone();
            if truncated {
                sys_content.push_str(
                    "\n\n[Note: earlier parts of this conversation were omitted to fit the context window.]",
                );
            }
            out.push(Message { role: "system".to_string(), content: sys_content });
        } else if truncated {
            // No system message existed - create exactly one so the note has
            // a safe home without touching the user/assistant sequence.
            out.push(Message {
                role: "system".to_string(),
                content: "[Note: earlier parts of this conversation were omitted to fit the context window.]".to_string(),
            });
        }

        if start < last {
            out.extend_from_slice(&messages[start..last]);
        }

        // Fold retrieved memory into the LATEST message's own text (the same
        // convention attachments already use) instead of a new message.
        let mut last_msg = messages[last].clone();
        if !retrieved_block.trim().is_empty() {
            last_msg.content = format!(
                "{}\n\n[Context retrieved from memory - earlier or related conversations:]\n{}",
                last_msg.content, retrieved_block
            );
        }
        out.push(last_msg);
        out
    }

    /// Execute retrieval plan across all tiers.
    ///
    /// Tier 3 retrieval is KEYWORD-based (tier_manager's SQL search over the
    /// message store). There is no vector/semantic path - this system has no
    /// embedding model - so `keyword_search` alone selects how tier 3 is
    /// queried.
    async fn execute_retrieval_plan(
        &self,
        session_id: &str,
        plan: &RetrievalPlan,
    ) -> anyhow::Result<RetrievedContent> {
        let mut retrieved = RetrievedContent::default();

        // Retrieve from Tier 1 (current context — hot KV cache)
        if plan.use_tier1 {
            let tier_manager = self.tier_manager.read().await;
            retrieved.tier1 = tier_manager.get_tier1_content(session_id).await;
        }

        // Retrieve from Tier 3 (full database) — keyword search over the
        // message store, or the most recent slice when there are no topics to
        // search for.
        if plan.use_tier3 {
            let tier_manager = self.tier_manager.read().await;
            if plan.keyword_search && !plan.search_topics.is_empty() {
                for topic in &plan.search_topics {
                    let limit_per_topic = plan.max_messages / plan.search_topics.len().max(1);

                    if let Ok(results) = tier_manager.search_tier3_content(
                        session_id,
                        topic,
                        limit_per_topic,
                    ).await {
                        retrieved.tier3 = Some(results);
                        break;
                    }
                }
            } else {
                retrieved.tier3 = tier_manager.get_tier3_content(
                    session_id,
                    Some((plan.max_messages as i64).min(i32::MAX as i64) as i32),
                    Some(0),
                ).await.ok();
            }
        }

        // Add cross-session search if needed
        if plan.cross_session_search && !plan.search_topics.is_empty() {
            let tier_manager = self.tier_manager.read().await;
            if let Ok(cross_session_results) = tier_manager.search_cross_session_content(
                session_id,
                &plan.search_topics.join(" "),
                10,
            ).await {
                retrieved.cross_session = Some(cross_session_results);
            }
        }

        Ok(retrieved)
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
        }
    }
}

#[derive(Debug, Default)]
struct RetrievedContent {
    tier1: Option<Vec<Message>>,
    tier3: Option<Vec<crate::memory_db::StoredMessage>>,
    cross_session: Option<Vec<crate::memory_db::StoredMessage>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    async fn test_orchestrator(budget: usize) -> ContextOrchestrator {
        let db = Arc::new(crate::memory_db::MemoryDatabase::new_in_memory().unwrap());
        let mut cfg = OrchestratorConfig::default();
        cfg.max_context_tokens = budget;
        ContextOrchestrator::new(db, cfg).await.unwrap()
    }

    /// Stand-in for whatever base prompt the client sent.
    ///
    /// Deliberately not phrased as a real prompt: the backend does not own the
    /// base system prompt (the frontend's `systemPrompt.ts` does), and a
    /// realistic-looking literal here reads like a second, competing one.
    /// Every assertion below cares only about POSITION and whether the content
    /// was modified - never about what it says.
    const BASE_PROMPT: &str = "<base system prompt supplied by the client>";

    #[tokio::test]
    async fn stable_assembly_preserves_order_and_prefix_across_turns() {
        let orch = test_orchestrator(200).await; // tiny budget forces truncation
        let mut messages = vec![msg("system", BASE_PROMPT)];
        for i in 0..20 {
            messages.push(msg(
                if i % 2 == 0 { "user" } else { "assistant" },
                &format!("Message number {} with enough words to consume estimate tokens.", i),
            ));
        }

        let out1 = orch.assemble_stable_context("s1", &messages, vec![], 200);
        assert!(out1.len() < messages.len(), "must truncate under tiny budget");
        assert_eq!(
            out1.last().unwrap().content,
            messages.last().unwrap().content,
            "latest user message must always survive"
        );
        assert!(
            out1.iter().any(|m| m.content.contains("omitted to fit")),
            "truncation must be announced with the constant marker"
        );

        // Next turn: two short messages appended. The previously emitted
        // prefix (everything except out1's final message) must reappear
        // byte-identically at the start of the new assembly.
        let mut messages2 = messages.clone();
        messages2.push(msg("assistant", "reply"));
        messages2.push(msg("user", "next question"));
        let out2 = orch.assemble_stable_context("s1", &messages2, vec![], 200);

        let prefix1: Vec<&str> = out1[..out1.len() - 1].iter().map(|m| m.content.as_str()).collect();
        let prefix2: Vec<&str> = out2[..prefix1.len()].iter().map(|m| m.content.as_str()).collect();
        assert_eq!(prefix1, prefix2, "prefix must stay identical across turns (prompt cache)");
    }

    #[tokio::test]
    async fn retrieved_block_folds_into_latest_message_not_a_new_message() {
        // Verified live against gemma-3's chat template (b8037): an extra
        // system-role message anywhere but position 0 throws a hard Jinja
        // "roles must alternate" error. Retrieved memory must therefore be
        // folded into an EXISTING message's text, never a new array entry -
        // this test locks in that no message count is added.
        let orch = test_orchestrator(10_000).await;
        let messages = vec![
            msg("system", BASE_PROMPT),
            msg("user", "first question"),
            msg("assistant", "first answer"),
            msg("user", "what did the contract say?"),
        ];
        let out = orch.assemble_stable_context(
            "s2",
            &messages,
            vec!["[Summary]: the contract caps liability at 1M".to_string()],
            10_000,
        );
        assert_eq!(out.len(), messages.len(), "no truncation/retrieval may ever add a message entry");
        assert!(
            out.iter().all(|m| m.role == "system" || m.role == "user" || m.role == "assistant"),
            "role sequence must be preserved exactly"
        );
        assert_eq!(out[0].content, BASE_PROMPT, "system message untouched (no truncation)");
        assert_eq!(out[1].content, "first question");
        assert_eq!(out[2].content, "first answer");
        assert_eq!(out.last().unwrap().role, "user");
        assert!(out.last().unwrap().content.starts_with("what did the contract say?"),
            "original question text must lead the folded message: {}", out.last().unwrap().content);
        assert!(out.last().unwrap().content.contains("Context retrieved from memory"));
        assert!(out.last().unwrap().content.contains("liability at 1M"));
    }

    #[tokio::test]
    async fn truncation_note_folds_into_system_message_never_a_new_role() {
        let orch = test_orchestrator(200).await; // tiny budget forces truncation
        let mut messages = vec![msg("system", BASE_PROMPT)];
        for i in 0..20 {
            messages.push(msg(
                if i % 2 == 0 { "user" } else { "assistant" },
                &format!("Message number {} with enough words to consume estimate tokens.", i),
            ));
        }
        let out = orch.assemble_stable_context("s3", &messages, vec![], 200);
        assert!(out.len() < messages.len(), "must actually truncate under this budget");

        // The exact defect this guards: llama-server's gemma-3 chat template
        // (live-verified against b8037) throws a hard 500 "Conversation
        // roles must alternate user/assistant" if any non-alternating or
        // extra system-role message appears after position 0.
        assert_eq!(out[0].role, "system");
        for (i, m) in out[1..].iter().enumerate() {
            assert_ne!(m.role, "system", "no system-role message may appear after position 0");
            let expected = if i % 2 == 0 { "user" } else { "assistant" };
            assert_eq!(m.role, expected, "role alternation must hold at position {}", i + 1);
        }
        assert!(out[0].content.contains("omitted to fit"), "truncation note must be folded into the system message");
    }

    /// The regression test for the live 502.
    ///
    /// The single-shape test above passed while the invariant was broken,
    /// because whether the boundary landed on a user or an assistant message
    /// depended entirely on message sizes. This sweeps conversation lengths,
    /// budgets and message sizes — including one enormous assistant turn,
    /// which is what a "print the whole document" answer looks like and is
    /// precisely the shape that pushed the boundary onto an assistant message
    /// in production.
    #[tokio::test]
    async fn truncation_never_starts_the_prompt_on_an_assistant_message() {
        // Odd turn counts only: a real request always ends with the user's new
        // question, so an even count (ending on an assistant reply) is a shape
        // this function is never given.
        for turns in (3..25usize).step_by(2) {
            for budget in [80usize, 120, 200, 400, 800, 1500] {
                for giant_at in [usize::MAX, 1, 2, 3, 6] {
                    let orch = test_orchestrator(budget).await;

                    let mut messages = vec![msg("system", BASE_PROMPT)];
                    for i in 0..turns {
                        let role = if i % 2 == 0 { "user" } else { "assistant" };
                        // One turn is enormous, mimicking an answer that dumped
                        // an entire document back into the conversation.
                        let body = if i == giant_at {
                            "A very long recital of the whole agreement. ".repeat(120)
                        } else {
                            format!("Message {} with a moderate amount of text in it.", i)
                        };
                        messages.push(msg(role, &body));
                    }

                    let out = orch.assemble_stable_context(
                        &format!("s-{}-{}-{}", turns, budget, giant_at),
                        &messages,
                        vec![],
                        budget,
                    );

                    assert!(!out.is_empty(), "assembly must never produce an empty prompt");

                    // Exactly the invariant gemma-3's template enforces, and
                    // the one whose violation is a hard 500 rather than a
                    // degraded reply.
                    let body_start = if out[0].role == "system" { 1 } else { 0 };
                    for (i, m) in out[body_start..].iter().enumerate() {
                        assert_ne!(
                            m.role, "system",
                            "no system-role message may appear after position 0 \
                             (turns={}, budget={}, giant_at={})",
                            turns, budget, giant_at
                        );
                        let expected = if i % 2 == 0 { "user" } else { "assistant" };
                        assert_eq!(
                            m.role, expected,
                            "role alternation broke at position {} \
                             (turns={}, budget={}, giant_at={}); full sequence: {:?}",
                            i + body_start,
                            turns,
                            budget,
                            giant_at,
                            out.iter().map(|m| m.role.as_str()).collect::<Vec<_>>()
                        );
                    }

                    // The user's actual question must always survive.
                    assert_eq!(
                        out.last().unwrap().role,
                        "user",
                        "the latest user message must always be present \
                         (turns={}, budget={}, giant_at={})",
                        turns, budget, giant_at
                    );
                }
            }
        }
    }

    /// The boundary helper itself, in isolation.
    #[test]
    fn snapping_moves_forward_to_a_user_message_and_never_past_the_last() {
        let messages = vec![
            msg("system", "s"),
            msg("user", "u1"),
            msg("assistant", "a1"),
            msg("user", "u2"),
            msg("assistant", "a2"),
        ];
        // Already on a user message: unchanged.
        assert_eq!(ContextOrchestrator::snap_to_user_boundary(&messages, 1), 1);
        // On an assistant message: moves forward to the next user.
        assert_eq!(ContextOrchestrator::snap_to_user_boundary(&messages, 2), 3);
        // No user message left: clamps to the last index rather than running off.
        assert_eq!(ContextOrchestrator::snap_to_user_boundary(&messages, 4), 4);
    }

    #[tokio::test]
    async fn flatten_skips_content_already_in_conversation() {
        let current = vec![msg("user", "duplicate content here")];
        let retrieved = RetrievedContent {
            tier1: None,
            tier3: Some(vec![crate::memory_db::StoredMessage {
                id: 1,
                session_id: "s".into(),
                message_index: 0,
                role: "user".into(),
                content: "duplicate content here".into(),
                tokens: 5,
                timestamp: chrono::Utc::now(),
                importance_score: 0.5,
            }]),
            cross_session: None,
        };
        let texts = ContextOrchestrator::flatten_retrieved(&retrieved, &current);
        assert!(texts.is_empty(), "in-context content must not be re-injected: {:?}", texts);
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