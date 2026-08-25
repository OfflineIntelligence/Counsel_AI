use crate::memory::Message;
use crate::memory_db::MemoryDatabase;
use std::sync::Arc;
use tracing::{debug, info};

/// Plan for retrieving content from memory
#[derive(Debug, Clone)]
pub struct RetrievalPlan {
    /// Whether to retrieve from memory at all
    pub needs_retrieval: bool,
    
    /// Which memory tiers to use
    pub use_tier1: bool,  // Current KV cache
    pub use_tier3: bool,  // Full database
    
    /// Whether to search across different sessions
    pub cross_session_search: bool,
    
    /// Search strategies to employ. There is no vector/semantic strategy:
    /// this system has no embedding model, so tier-3 retrieval is keyword and
    /// temporal only.
    pub keyword_search: bool,
    pub temporal_search: bool,
    
    /// Limits for retrieval
    pub max_messages: usize,
    pub max_tokens: usize,
    
    /// Target compression ratio if summarizing
    pub target_compression: f32,
    
    /// Specific topics to search for
    pub search_topics: Vec<String>,
}

impl Default for RetrievalPlan {
    fn default() -> Self {
        Self {
            needs_retrieval: false,
            use_tier1: true,
            use_tier3: false,
            cross_session_search: false,
            keyword_search: false,
            temporal_search: false,
            max_messages: 100,
            max_tokens: 4000,
            target_compression: 0.3,
            search_topics: Vec::new(),
        }
    }
}

/// Plans retrieval strategies based on conversation context
pub struct RetrievalPlanner {
    database: Arc<MemoryDatabase>,
    recent_threshold_messages: usize,
    max_retrieval_time_ms: u64,
}

impl RetrievalPlanner {
    /// Create a new retrieval planner
    pub fn new(database: Arc<MemoryDatabase>) -> Self {
        Self {
            database,
            recent_threshold_messages: 20,
            max_retrieval_time_ms: 200,
        }
    }
    
    /// Analyze conversation and create retrieval plan
    pub async fn create_plan(
        &self,
        session_id: &str,
        current_messages: &[Message],
        max_context_tokens: usize,
        user_query: Option<&str>,
        has_past_refs: bool, // NEW parameter
    ) -> anyhow::Result<RetrievalPlan> {
        let mut plan = RetrievalPlan {
            max_tokens: max_context_tokens,
            ..Default::default()
        };
        
        // Check user query first for past references
        let mut has_past_references_in_query = false;
        if let Some(query) = user_query {
            // Check for cross-session references
            if self.is_cross_session_query(query, session_id) {
                plan.needs_retrieval = true;
                plan.cross_session_search = true;
                plan.search_topics = self.extract_topics_from_query(query);
            }
            
            // Check for past references in the CURRENT query
            has_past_references_in_query = self.has_past_references_in_text(query);
        }

        // Also use the passed has_past_refs parameter if available
        if !has_past_references_in_query && has_past_refs {
            has_past_references_in_query = true;
        }

        // Check if we need retrieval based on context window limits
        if !plan.needs_retrieval && !self.needs_retrieval(current_messages, max_context_tokens) {
            // Even if within limits, check if query asks for past content
            if has_past_references_in_query {
                plan.needs_retrieval = true;
                debug!("Retrieval needed: query asks for past content");
            } else {
                debug!("No retrieval needed - within context limits and no past references");
                return Ok(plan);
            }
        }
        
        plan.needs_retrieval = true;
        
        // Always use current context (Tier 1)
        plan.use_tier1 = true;
        
        // Analyze conversation to determine retrieval strategy
        let analysis = self.analyze_conversation(current_messages, user_query).await?;
        
        // Determine which tiers to use based on analysis
        self.plan_tier_usage(&mut plan, &analysis, session_id, has_past_references_in_query).await?;
        
        // Determine search strategies
        self.plan_search_strategies(&mut plan, &analysis, user_query);
        
        // Extract search topics from analysis if not already set by cross-session logic
        if plan.search_topics.is_empty() {
            plan.search_topics = analysis.extracted_topics;
        }
        
        // Adjust limits based on available tokens
        self.adjust_limits(&mut plan, current_messages, max_context_tokens);
        
        info!(
            "Created retrieval plan: Tiers({}{}), CrossSession({}), Search({}{}), PastRefs={}",
            if plan.use_tier1 { "1" } else { "" },
            if plan.use_tier3 { "3" } else { "" },
            plan.cross_session_search,
            if plan.keyword_search { "K" } else { "" },
            if plan.temporal_search { "T" } else { "" },
            has_past_references_in_query
        );
        
        Ok(plan)
    }
    
    /// Check if retrieval is needed based on message volume
    fn needs_retrieval(&self, messages: &[Message], max_tokens: usize) -> bool {
        if messages.len() <= 1 {
            return false;
        }
        
        // Estimate tokens
        let estimated_tokens: usize = messages.iter()
            .map(|m| m.content.len() / 4)
            .sum();
        
        estimated_tokens > max_tokens
    }

    /// Detect if the user query is asking for information from other sessions
    fn is_cross_session_query(&self, query: &str, _current_session_id: &str) -> bool {
        let cross_session_patterns = [
            "previously", "before", "earlier", "last time", "yesterday",
            "do you remember", "we discussed", "we talked about",
            "what did we talk", "remember when", "recall",
        ];
        
        let query_lower = query.to_lowercase();
        
        // Check for explicit cross-session references
        cross_session_patterns.iter().any(|pattern| query_lower.contains(pattern))
    }
    
    /// Check for past references in ANY text (not just current messages).
    ///
    /// Delegates to `context_engine::reference_detector`, which is the single
    /// definition of "does this point backwards" in the system. This used to
    /// carry its own list of 16 substrings; that list is preserved in full
    /// inside the detector's STRONG_PHRASES (pinned by
    /// `all_sixteen_original_phrases_are_still_detected`) alongside a much wider
    /// set, plus filename and date recognition.
    ///
    /// Two behavioural improvements come along with the consolidation, both from
    /// the detector's confidence tiers. The old list matched bare substrings, so
    /// "before" fired on "before you answer" and "previous" fired on "priority"
    /// — false positives on genuinely new questions. Here, boundaries are
    /// respected and a lone weak word is not sufficient.
    pub fn has_past_references_in_text(&self, text: &str) -> bool {
        crate::context_engine::reference_detector::detect_reference(
            text,
            &[],
            chrono::Utc::now(),
        )
        .refers_to_past()
    }

    /// Helper to extract topics directly from a single query string
    fn extract_topics_from_query(&self, query: &str) -> Vec<String> {
        let words: Vec<&str> = query.split_whitespace().collect();
        if words.len() < 3 {
            return vec![query.to_string()];
        }
        
        // Simple extraction logic: take the last few words as the topic
        let topic = words.iter()
            .rev()
            .take(4)
            .rev()
            .copied()
            .collect::<Vec<&str>>()
            .join(" ");
            
        vec![topic]
    }
    
    /// Analyze conversation context
    async fn analyze_conversation(
        &self,
        messages: &[Message],
        user_query: Option<&str>,
    ) -> anyhow::Result<ConversationAnalysis> {
        let mut analysis = ConversationAnalysis {
            extracted_topics: self.extract_topics(messages),
            has_past_references: self.has_past_references_in_messages(messages),
            ..Default::default()
        };
        
        // Check if query asks for specific information
        if let Some(query) = user_query {
            analysis.requires_specific_details = self.requires_specific_details(query);
            analysis.query_complexity = self.assess_query_complexity(query);
        }
        
        // Analyze conversation length and patterns
        analysis.conversation_length = messages.len();
        analysis.recency_pattern = self.analyze_recency_pattern(messages);
        
        Ok(analysis)
    }
    
    /// Plan which memory tiers to use
    async fn plan_tier_usage(
        &self,
        plan: &mut RetrievalPlan,
        analysis: &ConversationAnalysis,
        session_id: &str,
        has_past_references_in_query: bool,
    ) -> anyhow::Result<()> {
        // NEW: Check if we have messages in database for this session
        let has_db_messages = self.check_if_session_has_db_messages(session_id).await?;
        
        // TIER 3 LOGIC FIXED:
        // 1. Always use Tier 3 if query asks for past content (regardless of conversation length)
        if has_past_references_in_query && has_db_messages {
            plan.use_tier3 = true;
            debug!("Query asks for past content, using Tier 3 (database)");
        }
        
        // 2. Use Tier 3 for specific details
        if analysis.requires_specific_details && has_db_messages {
            plan.use_tier3 = true;
            debug!("Specific details requested, using Tier 3");
        }
        
        // 3. Use Tier 3 for cross-session search
        if plan.cross_session_search {
            plan.use_tier3 = true;
            debug!("Cross-session search, using Tier 3");
        }
        
        // 4. Use Tier 3 for long conversations (for summarization)
        if analysis.conversation_length > 30 && has_db_messages && !plan.use_tier3 {
            plan.use_tier3 = true;
            debug!("Long conversation ({} messages), using Tier 3", analysis.conversation_length);
        }
        
        // 5. Use Tier 3 if we have past references in recent messages
        if analysis.has_past_references && has_db_messages && !plan.use_tier3 {
            plan.use_tier3 = true;
            debug!("Past references in messages, using Tier 3");
        }
        
        if analysis.conversation_length > 100 {
            plan.target_compression = 0.2;
        }
        
        Ok(())
    }
    
    /// Check if session has messages in database
    async fn check_if_session_has_db_messages(&self, session_id: &str) -> anyhow::Result<bool> {
        // Quick check: get just 1 message to see if session exists in DB
        match self.database.conversations.get_session_messages(session_id, Some(1), Some(0)) {
            Ok(messages) => Ok(!messages.is_empty()),
            Err(e) => {
                debug!("Error checking DB for session {}: {}", session_id, e);
                Ok(false)
            }
        }
    }
    
    /// Plan search strategies
    fn plan_search_strategies(
        &self,
        plan: &mut RetrievalPlan,
        analysis: &ConversationAnalysis,
        user_query: Option<&str>,
    ) {
        // Keyword search for specific references or cross-session topic matches
        plan.keyword_search = analysis.requires_specific_details 
            || analysis.has_past_references 
            || plan.cross_session_search
            || !analysis.extracted_topics.is_empty();
        
        // Temporal search for time-based references
        plan.temporal_search = self.has_temporal_references(user_query.unwrap_or(""));
    }
    
    /// Adjust limits based on available context
    fn adjust_limits(
        &self,
        plan: &mut RetrievalPlan,
        current_messages: &[Message],
        max_context_tokens: usize,
    ) {
        let current_tokens: usize = current_messages.iter()
            .map(|m| m.content.len() / 4)
            .sum();
        
        let available_for_retrieval = max_context_tokens.saturating_sub(current_tokens);
        
        // Assume ~50 tokens per message on average
        let estimated_messages = available_for_retrieval / 50;
        plan.max_messages = estimated_messages.clamp(10, 100);
    }
    
    /// Extract topics from messages
    fn extract_topics(&self, messages: &[Message]) -> Vec<String> {
        let mut topics = Vec::new();
        
        for message in messages.iter().rev().filter(|m| m.role == "user").take(3) {
            let words: Vec<&str> = message.content.split_whitespace().collect();
            
            for i in 0..words.len().saturating_sub(2) {
                let word_lower = words[i].to_lowercase();
                
                if word_lower == "about" || word_lower == "regarding" {
                    let topic = words[i + 1..].iter()
                        .take(3)
                        .copied()
                        .collect::<Vec<&str>>()
                        .join(" ");
                    
                    if !topic.is_empty() {
                        topics.push(topic);
                    }
                }
                
                if ["what", "how", "why", "when", "where", "who"].contains(&word_lower.as_str()) {
                    let topic = words[i + 1..].iter()
                        .take(4)
                        .copied()
                        .collect::<Vec<&str>>()
                        .join(" ");
                    
                    if !topic.is_empty() {
                        topics.push(topic);
                    }
                }
            }
        }
        
        topics.dedup();
        topics.truncate(3);
        
        topics
    }
    
    /// Check for references to past content in messages (renamed for clarity)
    fn has_past_references_in_messages(&self, messages: &[Message]) -> bool {
        let reference_patterns = [
            "earlier", "before", "previous", "last time", "yesterday",
            "we discussed", "we talked about", "remember", "recall",
        ];
        
        for message in messages.iter().rev().take(5) {
            let content_lower = message.content.to_lowercase();
            if reference_patterns.iter().any(|p| content_lower.contains(p)) {
                return true;
            }
        }
        
        false
    }
    
    /// Check if query requires specific details
    fn requires_specific_details(&self, query: &str) -> bool {
        let detail_patterns = [
            "exactly", "specifically", "in detail", "step by step",
            "the code", "the number", "the date", "the name",
            "show me", "give me", "tell me",
        ];
        
        let query_lower = query.to_lowercase();
        detail_patterns.iter().any(|p| query_lower.contains(p))
    }
    
    /// Assess query complexity
    fn assess_query_complexity(&self, query: &str) -> f32 {
        let words: Vec<&str> = query.split_whitespace().collect();
        
        if words.len() < 3 {
            return 0.2;
        }
        
        let mut complexity = 0.0;
        complexity += (words.len() as f32).min(50.0) / 100.0;
        
        let clause_count = query.split(&[',', ';', '&']).count();
        complexity += (clause_count as f32).min(5.0) / 10.0;
        
        let technical_terms = ["code", "function", "algorithm", "parameter", "variable"];
        for term in technical_terms {
            if query.to_lowercase().contains(term) {
                complexity += 0.2;
            }
        }
        
        complexity.min(1.0)
    }
    
    /// Analyze recency pattern
    fn analyze_recency_pattern(&self, messages: &[Message]) -> RecencyPattern {
        if messages.len() < 5 {
            return RecencyPattern::RecentOnly;
        }
        
        let recent_topics = self.extract_topics(&messages[messages.len().saturating_sub(5)..]);
        let older_topics = self.extract_topics(&messages[..messages.len().saturating_sub(5)]);
        
        let overlap = recent_topics.iter()
            .filter(|topic| older_topics.contains(topic))
            .count();
        
        match overlap {
            0 => RecencyPattern::TopicJumping,
            1 => RecencyPattern::Mixed,
            _ => RecencyPattern::TopicContinuation,
        }
    }
    
    /// Check for temporal references
    fn has_temporal_references(&self, query: &str) -> bool {
        let temporal_patterns = [
            "yesterday", "today", "tomorrow", "last week", "last month",
            "earlier", "before", "previously", "in the past",
        ];
        
        let query_lower = query.to_lowercase();
        temporal_patterns.iter().any(|p| query_lower.contains(p))
    }
}

/// Analysis of conversation context
#[derive(Debug, Default)]
struct ConversationAnalysis {
    extracted_topics: Vec<String>,
    has_past_references: bool,
    requires_specific_details: bool,
    query_complexity: f32,
    conversation_length: usize,
    recency_pattern: RecencyPattern,
}

/// Pattern of message recency
#[derive(Debug, Clone, PartialEq, Default)]
enum RecencyPattern {
    #[default]
    RecentOnly,
    TopicContinuation,
    TopicJumping,
    Mixed,
}

impl Clone for RetrievalPlanner {
    fn clone(&self) -> Self {
        Self {
            database: self.database.clone(),
            recent_threshold_messages: self.recent_threshold_messages,
            max_retrieval_time_ms: self.max_retrieval_time_ms,
        }
    }
}