//! Background indexing: chunk + keyword-index every document (fast), then
//! embed chunks in batches. Yields while a chat response is generating and
//! stops the embedding server when there is nothing to do.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{info, warn};

use super::store::{message_part_entry, MAX_CONTENT_PARTS, ROUND_PART};
use super::{model, runtime, vector};
use crate::memory_db::MemoryDatabase;

/// Chunks embedded per request (RAG_EMBED_BATCH, default 16).
fn embed_batch() -> i64 {
    crate::tuning::parse_or("RAG_EMBED_BATCH", 16i64).max(1)
}
const SETTINGS_MODEL_KEY: &str = "rag.embedding_model_key";
/// How often the database is checked for compaction (RAG_COMPACT_INTERVAL_HOURS, default 6).
fn compaction_interval() -> Duration {
    Duration::from_secs(crate::tuning::parse_or("RAG_COMPACT_INTERVAL_HOURS", 6u64).max(1) * 3600)
}
const COMPACT_MIN_FREE_BYTES: u64 = 16 * 1024 * 1024;
const COMPACT_MIN_FREE_RATIO: f64 = 0.2;

lazy_static::lazy_static! {
    /// Chunks and messages the embedding server refused on their own while
    /// the rest of their batch embedded; not retried until the next launch.
    static ref SKIPPED_CHUNKS: Mutex<HashSet<i64>> = Mutex::new(HashSet::new());
    static ref SKIPPED_MESSAGES: Mutex<HashSet<i64>> = Mutex::new(HashSet::new());
}

fn skipped(set: &Mutex<HashSet<i64>>) -> HashSet<i64> {
    set.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Embed `texts` as one batch; if the batch is refused, embed each text on
/// its own so a single bad input cannot hold the rest back. `None` marks an
/// input that failed alone. Errors only when nothing embeds at all, which
/// points at the server rather than at an input.
async fn embed_isolating(rt: &super::RagRuntime, texts: &[String]) -> anyhow::Result<Vec<Option<Vec<f32>>>> {
    let batch_err = match rt.embedder.embed_documents(texts).await {
        Ok(v) if v.len() == texts.len() => return Ok(v.into_iter().map(Some).collect()),
        Ok(v) => anyhow::anyhow!("{} vectors for {} inputs", v.len(), texts.len()),
        Err(e) => e,
    };
    if texts.len() == 1 {
        return Err(batch_err);
    }
    let mut out = Vec::with_capacity(texts.len());
    for t in texts {
        let one = rt.embedder.embed_documents(std::slice::from_ref(t)).await;
        out.push(match one {
            Ok(mut v) if v.len() == 1 => v.pop(),
            _ => None,
        });
    }
    if out.iter().all(Option::is_none) {
        return Err(batch_err);
    }
    Ok(out)
}

/// `content` split into at most `max_parts` pieces of at most `max_bytes`
/// each, breaking at whitespace where possible, so text past the first
/// embedding input is still searchable by meaning. The first piece is the
/// same prefix that was embedded before parts existed.
pub fn message_parts(content: &str, max_bytes: usize, max_parts: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = content.trim();
    while !rest.is_empty() && out.len() < max_parts.max(1) {
        if rest.len() <= max_bytes {
            out.push(rest.to_string());
            break;
        }
        let mut end = max_bytes;
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        // Prefer the last whitespace in the final fifth of the piece.
        if let Some(ws) = rest[..end].rfind(char::is_whitespace).filter(|&w| w >= end * 4 / 5) {
            end = ws;
        }
        if end == 0 {
            break;
        }
        out.push(rest[..end].trim_end().to_string());
        rest = rest[end..].trim_start();
    }
    out
}

/// Text embedded for a chunk: header (file + section path) and content.
pub fn embedding_input(filename: &str, section_path: Option<&str>, body: &str) -> String {
    let header = super::store::index_header(filename, section_path);
    format!("{}\n{}", header, body)
}

async fn chunk_pending(db: &Arc<MemoryDatabase>) -> usize {
    let mut done = 0usize;
    loop {
        let pending = match db.rag.documents_needing_chunking(8) {
            Ok(p) => p,
            Err(e) => {
                warn!("Indexer: could not list documents to chunk: {}", e);
                return done;
            }
        };
        if pending.is_empty() {
            return done;
        }
        for (id, name) in pending {
            let db2 = db.clone();
            let result = tokio::task::spawn_blocking(move || db2.rag.index_document(id)).await;
            match result {
                Ok(Ok(n)) => {
                    done += 1;
                    info!("Indexed '{}' (document {}): {} chunks", name, id, n);
                }
                Ok(Err(e)) => {
                    warn!("Indexing '{}' (document {}) failed: {}", name, id, e);
                    if let Ok(conn) = db.rag.conn() {
                        let _ = super::store::clear_document_index(&conn, id, "failed", Some(&e.to_string()));
                    }
                }
                Err(e) => warn!("Indexing task for document {} panicked: {}", id, e),
            }
        }
    }
}

fn reconcile_model(db: &MemoryDatabase, rt: &super::RagRuntime) {
    let key = model::model_key();
    let stored = db.settings.get(SETTINGS_MODEL_KEY).ok().flatten();
    if stored.as_deref() != Some(key.as_str()) {
        if stored.is_some() {
            info!("Embedding model changed ({:?} -> {}); rebuilding vectors", stored, key);
            let _ = db.rag.clear_all_vectors();
            if let Ok(mut idx) = rt.index.write() {
                idx.loaded = false;
            }
            if let Ok(mut idx) = rt.message_index.write() {
                idx.loaded = false;
            }
        }
        let _ = db.settings.set(SETTINGS_MODEL_KEY, &key);
    }
}

/// Embed pending chunks until none remain. Returns chunks embedded.
async fn embed_pending(db: &Arc<MemoryDatabase>, rt: &super::RagRuntime) -> usize {
    if !rt.embedder.available() {
        return 0;
    }
    reconcile_model(db, rt);
    super::search::ensure_index_loaded(db, rt);
    let key = model::model_key();
    let mut embedded = 0usize;
    loop {
        while rt.generation_active() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let skip = skipped(&SKIPPED_CHUNKS);
        let batch: Vec<_> = match db.rag.chunks_needing_vectors(embed_batch() + skip.len() as i64) {
            Ok(b) => b.into_iter().filter(|(c, _)| !skip.contains(&c.id)).take(embed_batch() as usize).collect(),
            Err(e) => {
                warn!("Indexer: could not list chunks to embed: {}", e);
                return embedded;
            }
        };
        if batch.is_empty() {
            return embedded;
        }

        let mut texts = Vec::with_capacity(batch.len());
        let mut doc_texts: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
        for (chunk, filename) in &batch {
            let text = match doc_texts.get(&chunk.document_id) {
                Some(t) => t,
                None => {
                    let t = db.documents.get_document(chunk.document_id).map(|d| d.extracted_text).unwrap_or_default();
                    doc_texts.entry(chunk.document_id).or_insert(t)
                }
            };
            let body = chunk.text(text);
            texts.push(embedding_input(filename, chunk.section_path.as_deref(), &body));
        }

        let vectors = match embed_isolating(rt, &texts).await {
            Ok(v) => v,
            Err(e) => {
                warn!("Embedding batch failed ({}); indexing will retry later", e);
                return embedded;
            }
        };

        let mut items: Vec<(i64, Vec<u8>)> = Vec::with_capacity(batch.len());
        let mut docs: Vec<i64> = Vec::with_capacity(batch.len());
        for ((c, _), v) in batch.iter().zip(vectors.iter()) {
            match v {
                Some(v) => {
                    items.push((c.id, vector::quantize(v)));
                    docs.push(c.document_id);
                }
                None => {
                    warn!("Chunk {} of document {} could not be embedded; skipping it", c.id, c.document_id);
                    if let Ok(mut s) = SKIPPED_CHUNKS.lock() {
                        s.insert(c.id);
                    }
                }
            }
        }
        if let Err(e) = db.rag.store_vectors(&key, &items) {
            warn!("Could not store embeddings: {}", e);
            return embedded;
        }
        if let Ok(mut idx) = rt.index.write() {
            if idx.loaded {
                for ((id, blob), doc) in items.iter().zip(docs.iter()) {
                    idx.insert_blob(*id, *doc, blob);
                }
            }
        }
        embedded += items.len();
        tokio::task::yield_now().await;
    }
}

/// Embed chat messages that have no vector yet. Returns messages embedded.
async fn embed_pending_messages(db: &Arc<MemoryDatabase>, rt: &super::RagRuntime) -> usize {
    if !rt.embedder.available() {
        return 0;
    }
    super::search::ensure_message_index_loaded(db, rt);
    let mut embedded = 0usize;
    loop {
        while rt.generation_active() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let skip = skipped(&SKIPPED_MESSAGES);
        let batch: Vec<_> = match db.rag.messages_needing_vectors(embed_batch() + skip.len() as i64) {
            Ok(b) => b.into_iter().filter(|(id, _, _)| !skip.contains(id)).take(embed_batch() as usize).collect(),
            Err(e) => {
                warn!("Indexer: could not list messages to embed: {}", e);
                return embedded;
            }
        };
        if batch.is_empty() {
            return embedded;
        }
        // Every part of every message in one request: (batch position, part).
        let mut owner: Vec<(usize, i64)> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        for (i, (_, _, content)) in batch.iter().enumerate() {
            let part_bytes = super::chunker::HARD_MAX_BYTES.min(super::embedder::input_byte_cap()).max(256);
            for (part, text) in message_parts(content, part_bytes, MAX_CONTENT_PARTS as usize)
                .into_iter()
                .enumerate()
            {
                owner.push((i, part as i64));
                texts.push(text);
            }
            // A reply also gets a key for the whole exchange it completes.
            if let Ok(Some(question)) = db.rag.question_answered_by(batch[i].0) {
                owner.push((i, ROUND_PART));
                texts.push(crate::context_engine::conversation_memory::round_key(&question, content));
            }
        }
        let vectors = match embed_isolating(rt, &texts).await {
            Ok(v) => v,
            Err(e) => {
                warn!("Message embedding batch failed ({}); will retry later", e);
                return embedded;
            }
        };
        let mut items: Vec<(i64, Vec<u8>)> = Vec::with_capacity(batch.len());
        let mut parts: Vec<(i64, i64, Vec<u8>)> = Vec::new();
        let mut index_entries: Vec<(i64, i64, Vec<u8>)> = Vec::with_capacity(texts.len());
        let mut first_ok = vec![false; batch.len()];
        for ((i, part), v) in owner.iter().zip(vectors.iter()) {
            let (id, session, _) = &batch[*i];
            let Some(v) = v else { continue };
            let blob = vector::quantize(v);
            index_entries.push((message_part_entry(*id, *part), *session, blob.clone()));
            if *part == 0 {
                first_ok[*i] = true;
                items.push((*id, blob));
            } else {
                parts.push((*id, *part, blob));
            }
        }
        for (i, ok) in first_ok.iter().enumerate() {
            if !ok {
                let id = batch[i].0;
                warn!("Message {} could not be embedded; skipping it", id);
                if let Ok(mut s) = SKIPPED_MESSAGES.lock() {
                    s.insert(id);
                }
            }
        }
        // A message whose first part failed is skipped whole; drop its other parts.
        let stored: HashSet<i64> = items.iter().map(|(id, _)| *id).collect();
        parts.retain(|(id, _, _)| stored.contains(id));
        index_entries.retain(|(entry, _, _)| stored.contains(&super::store::entry_message(*entry)));
        if let Err(e) = db.rag.store_message_part_vectors(&parts) {
            warn!("Could not store message part embeddings: {}", e);
        }
        if let Err(e) = db.rag.store_message_vectors(&items) {
            warn!("Could not store message embeddings: {}", e);
            return embedded;
        }
        if let Ok(mut idx) = rt.message_index.write() {
            if idx.loaded {
                for (entry, session, blob) in &index_entries {
                    idx.insert_blob(*entry, *session, blob);
                }
            }
        }
        embedded += items.len();
        tokio::task::yield_now().await;
    }
}

/// One full pass: chunk everything pending, then embed everything pending.
pub async fn run_once(db: &Arc<MemoryDatabase>) -> (usize, usize) {
    run_once_with(db, &runtime()).await
}

/// One full pass with an explicit runtime (tests, evaluation).
pub async fn run_once_with(db: &Arc<MemoryDatabase>, rt: &super::RagRuntime) -> (usize, usize) {
    let chunked = chunk_pending(db).await;
    let embedded = embed_pending(db, rt).await;
    let messages = embed_pending_messages(db, rt).await;
    if messages > 0 {
        info!("Indexer: embedded {} chat message(s)", messages);
    }
    if embedded > 0 {
        info!("Indexer: embedded {} chunk(s)", embedded);
    }
    (chunked, embedded)
}

/// Start the background indexer (idempotent per process is the caller's job).
pub fn spawn(db: Arc<MemoryDatabase>) {
    tokio::spawn(async move {
        info!("Retrieval indexer started (embedding model installed: {})", model::is_installed());
        let mut last_compaction: Option<std::time::Instant> = None;
        loop {
            run_once(&db).await;
            let due = last_compaction.map(|t| t.elapsed() >= compaction_interval()).unwrap_or(true);
            if due && !runtime().generation_active() {
                last_compaction = Some(std::time::Instant::now());
                let db2 = db.clone();
                match tokio::task::spawn_blocking(move || db2.rag.compact_if_worthwhile(COMPACT_MIN_FREE_BYTES, COMPACT_MIN_FREE_RATIO)).await {
                    Ok(Err(e)) => warn!("Database compaction failed: {}", e),
                    Err(e) => warn!("Database compaction task failed: {}", e),
                    _ => {}
                }
            }
            if let Some(llama) = runtime().llama.clone() {
                llama.reap_if_idle().await;
            }
            tokio::select! {
                _ = super::index_work_signalled() => {}
                _ = tokio::time::sleep(Duration::from_secs(crate::tuning::parse_or("RAG_INDEXER_POLL_SECONDS", 60u64).max(5))) => {}
            }
            // Coalesce bursts of attachments.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::embedder::Embedder;
    use async_trait::async_trait;

    /// Refuses any request that contains a "POISON" input, the way the
    /// embedding server refuses a batch when one input exceeds its slot.
    struct PoisonRefusing;

    #[async_trait]
    impl Embedder for PoisonRefusing {
        async fn embed_documents(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            if texts.iter().any(|t| t.contains("POISON")) {
                anyhow::bail!("request exceeds the available context size");
            }
            Ok(texts.iter().map(|t| vec![t.len() as f32, 1.0, 0.5]).collect())
        }
        async fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![1.0, 1.0, 1.0])
        }
        fn available(&self) -> bool {
            true
        }
        fn warm(&self) -> bool {
            true
        }
    }

    /// Down entirely: every request fails.
    struct Down;

    #[async_trait]
    impl Embedder for Down {
        async fn embed_documents(&self, _texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            anyhow::bail!("connection refused")
        }
        async fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("connection refused")
        }
        fn available(&self) -> bool {
            true
        }
        fn warm(&self) -> bool {
            true
        }
    }

    fn db_with_messages(session: &str, n: usize, poison_at: Option<usize>) -> Arc<MemoryDatabase> {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id(session, None).unwrap();
        let msgs: Vec<(String, String, i32, i32, f32)> = (0..n)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                let content = if Some(i) == poison_at {
                    "POISON table 0.1 0.2 0.3 0.4 0.5 0.6 0.7 0.8 0.9".to_string()
                } else {
                    format!("ordinary message number {} with enough words to embed", i)
                };
                (role.to_string(), content, i as i32 + 1, 0, 0.5)
            })
            .collect();
        db.conversations.store_messages_batch(session, &msgs).unwrap();
        Arc::new(db)
    }

    fn pending_contents(db: &MemoryDatabase) -> Vec<String> {
        db.rag.messages_needing_vectors(1000).unwrap().into_iter().map(|(_, _, c)| c).collect()
    }

    #[tokio::test]
    async fn one_unembeddable_message_no_longer_blocks_the_queue() {
        // 40 messages; the newest one (embedded first, ids run DESC) is the poison.
        let db = db_with_messages("poison-session", 40, Some(39));
        let rt = super::super::RagRuntime::with_embedder(Arc::new(PoisonRefusing));

        let embedded = embed_pending_messages(&db, &rt).await;

        assert_eq!(embedded, 39, "every other message must be embedded");
        let left = pending_contents(&db);
        assert_eq!(left.len(), 1, "only the poison message stays without a vector: {:?}", left);
        assert!(left[0].contains("POISON"));

        // A second pass terminates and does not retry the skipped message.
        assert_eq!(embed_pending_messages(&db, &rt).await, 0);
    }

    #[tokio::test]
    async fn a_server_that_is_down_skips_nothing_and_retries_later() {
        let db = db_with_messages("down-session", 10, None);
        let rt = super::super::RagRuntime::with_embedder(Arc::new(Down));

        assert_eq!(embed_pending_messages(&db, &rt).await, 0);
        assert_eq!(pending_contents(&db).len(), 10, "nothing is embedded");

        // Once the server is back, every message embeds: none was written off as unembeddable.
        let healthy = super::super::RagRuntime::with_embedder(Arc::new(PoisonRefusing));
        assert_eq!(embed_pending_messages(&db, &healthy).await, 10);
        assert!(pending_contents(&db).is_empty());
    }
}

#[cfg(test)]
mod part_tests {
    use super::*;
    use crate::rag::store::{entry_message, message_part_entry, MAX_MESSAGE_PARTS};

    #[test]
    fn short_messages_are_one_part_exactly_as_before() {
        assert_eq!(message_parts("hello there", 2800, 8), vec!["hello there"]);
        assert!(message_parts("   ", 2800, 8).is_empty());
    }

    #[test]
    fn long_messages_split_at_whitespace_within_the_byte_limit() {
        let text = "word ".repeat(2000);
        let parts = message_parts(&text, 2800, 8);
        assert!(parts.len() >= 3);
        for p in &parts {
            assert!(p.len() <= 2800);
            assert!(!p.starts_with(' ') && !p.ends_with(' '));
            assert!(p.split(' ').all(|w| w == "word"), "a word was split");
        }
        assert_eq!(parts.concat().replace(' ', "").len(), text.replace(' ', "").len());
    }

    #[test]
    fn the_part_count_is_capped_and_multibyte_text_is_never_cut_mid_character() {
        let text = "é".repeat(20_000);
        let parts = message_parts(&text, 100, MAX_CONTENT_PARTS as usize);
        assert_eq!(parts.len(), MAX_CONTENT_PARTS as usize);
        assert!(parts.iter().all(|p| p.len() <= 100 && p.chars().all(|c| c == 'é')));
    }

    #[test]
    fn part_entries_map_back_to_their_message_and_never_collide() {
        for id in [1i64, 7, 123_456, 9_999_999] {
            assert_eq!(message_part_entry(id, 0), id);
            for part in 1..MAX_MESSAGE_PARTS {
                let e = message_part_entry(id, part);
                assert!(e < 0, "later parts never collide with message ids");
                assert_eq!(entry_message(e), id);
            }
        }
        assert_ne!(message_part_entry(1, 2), message_part_entry(2, 1));
    }

    #[tokio::test]
    async fn text_past_the_first_part_is_found_by_meaning() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("long-msg", None).unwrap();
        let filler = "general remarks about scheduling and logistics for the week ahead. ".repeat(120);
        let long = format!("{} My grandmother gave me a silver necklace when I turned eighteen.", filler);
        assert!(long.len() > super::super::chunker::HARD_MAX_BYTES * 2);
        db.conversations
            .store_messages_batch("long-msg", &[("user".into(), long, 1, 0, 0.5), ("assistant".into(), "Noted, thanks for sharing that.".into(), 2, 0, 0.5)])
            .unwrap();
        let db = Arc::new(db);
        let rt = super::super::RagRuntime::with_embedder(Arc::new(crate::rag::eval::TrigramEmbedder));
        assert_eq!(embed_pending_messages(&db, &rt).await, 2);

        let only: HashSet<String> = ["long-msg".to_string()].into_iter().collect();
        let hits = super::super::search::semantic_message_hits(
            &db, &rt, "silver necklace from my grandmother when I turned eighteen", Some(&only), "", None, 5, 0.1,
        )
        .await;
        assert!(!hits.is_empty(), "the late part must be searchable");
        assert_eq!(hits[0].0.message_index, 1, "the long message ranks first");
        let ids: Vec<i64> = hits.iter().map(|(m, _)| m.id).collect();
        let mut dedup = ids.clone();
        dedup.dedup();
        assert_eq!(ids, dedup, "a message appears once however many parts matched");
    }
}

#[cfg(test)]
mod round_key_tests {
    use super::*;
    use crate::rag::store::{entry_message, message_part_entry, ROUND_PART};

    #[test]
    fn a_round_key_holds_the_question_and_the_answers_strongest_points() {
        let key = crate::context_engine::conversation_memory::round_key(
            "What did I name my Spotify playlist?",
            "Hello there. You told me your playlist on Spotify is called Summer Vibes, with 42 chill tracks. Enjoy.",
        );
        assert!(key.starts_with("Question: What did I name my Spotify playlist?"));
        assert!(key.contains("Summer Vibes"), "{}", key);
    }

    #[tokio::test]
    async fn every_reply_gets_a_round_key_and_questions_do_not() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("rounds", None).unwrap();
        db.conversations
            .store_messages_batch(
                "rounds",
                &[
                    ("user".into(), "Which city hosts the Porto lease signing next month?".into(), 1, 0, 0.5),
                    ("assistant".into(), "The lease signing is scheduled in Porto on 12 June 2024 at the notary office.".into(), 2, 0, 0.5),
                ],
            )
            .unwrap();
        let ids: Vec<i64> = db.rag.messages_needing_vectors(10).unwrap().into_iter().map(|(id, _, _)| id).collect();
        let (user_id, reply_id) = (*ids.iter().min().unwrap(), *ids.iter().max().unwrap());
        assert!(db.rag.question_answered_by(user_id).unwrap().is_none());
        assert!(db.rag.question_answered_by(reply_id).unwrap().unwrap().contains("Porto lease"));

        let db = Arc::new(db);
        let rt = super::super::RagRuntime::with_embedder(Arc::new(crate::rag::eval::TrigramEmbedder));
        assert_eq!(embed_pending_messages(&db, &rt).await, 2);

        let round = message_part_entry(reply_id, ROUND_PART);
        let stored = db.rag.message_part_vectors_for(&[round, message_part_entry(user_id, ROUND_PART)]).unwrap();
        assert!(stored.contains_key(&round), "the reply's round key is stored");
        assert_eq!(stored.len(), 1, "a question has no round key");
        assert_eq!(entry_message(round), reply_id);
    }
}
