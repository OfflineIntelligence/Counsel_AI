//! Retrieval: routed exact lookups and hybrid (keyword + vector) search,
//! returning cited passages that fit a byte budget.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, warn};

use super::chunker;
use super::router::{self, Route};
use super::store::ChunkRow;
use super::vector;
use super::RagRuntime;
use crate::memory_db::MemoryDatabase;

const RRF_K: f64 = 60.0;
const CANDIDATES: i64 = 50;
const MAX_PASSAGE_BYTES: usize = 4000;
const SMALL_PASSAGE_BYTES: usize = 700;
/// Passages are built from the strongest hits only; weaker ones add noise.
const PASSAGE_CANDIDATES: usize = 12;
/// A merged passage spans at most this many extra adjacent chunks.
const MAX_MERGED_EXTRA_CHUNKS: i64 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct Passage {
    pub document_id: i64,
    pub filename: String,
    pub page_start: Option<i32>,
    pub page_end: Option<i32>,
    pub section_label: Option<String>,
    pub citation: String,
    pub text: String,
    pub score: f64,
    pub truncated: bool,
    /// Set when this document has other versions in scope.
    pub version_label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Retrieval {
    pub route: String,
    pub passages: Vec<Passage>,
    pub used_vectors: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RetrieveOptions {
    /// Byte budget for all passage text.
    pub budget_bytes: usize,
    /// Minimum cosine similarity for a vector-only hit to count (used when
    /// retrieving unrequested material, e.g. matter documents).
    pub min_similarity: Option<f32>,
    /// Allow starting the embedding server if it is not running.
    pub allow_cold_start: bool,
    /// Route exact lookups (page/section/definition).
    pub use_router: bool,
    /// At most this many passages.
    pub max_passages: usize,
    /// At most this many bytes per passage.
    pub max_passage_bytes: usize,
}

impl Default for RetrieveOptions {
    fn default() -> Self {
        Self {
            budget_bytes: 16_000,
            min_similarity: None,
            allow_cold_start: true,
            use_router: true,
            max_passages: 8,
            max_passage_bytes: MAX_PASSAGE_BYTES,
        }
    }
}

impl RetrieveOptions {
    /// Limits scaled to the prompt space available. A small model (small
    /// window) gets a few short passages, so the best match is not buried; a
    /// large one gets more and longer passages.
    pub fn for_budget(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            max_passages: (budget_bytes / 1500).clamp(3, 10),
            max_passage_bytes: (budget_bytes / 3).clamp(1_200, MAX_PASSAGE_BYTES),
            ..Default::default()
        }
    }
}

/// Passages scoring below this share of the best one are dropped: a weak
/// match adds reading, not evidence.
const RELATIVE_SCORE_FLOOR: f64 = 0.4;

struct DocCache<'a> {
    db: &'a MemoryDatabase,
    docs: HashMap<i64, Option<(String, String)>>,
}

impl<'a> DocCache<'a> {
    fn new(db: &'a MemoryDatabase) -> Self {
        Self { db, docs: HashMap::new() }
    }
    fn get(&mut self, id: i64) -> Option<&(String, String)> {
        let db = self.db;
        self.docs
            .entry(id)
            .or_insert_with(|| db.documents.get_document(id).ok().map(|d| (d.original_filename, d.extracted_text)))
            .as_ref()
    }
}

fn truncate_to(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{} […]", text[..end].trim_end()), true)
}

/// Text of pages `from..=to` (by `[[page:N]]` markers), markers removed.
pub fn page_text(text: &str, from: i32, to: i32) -> Option<String> {
    let start_marker = format!("[[page:{}]]", from);
    let start = text.find(&start_marker)?;
    let mut end = text.len();
    let mut p = to + 1;
    // The next existing page marker after `to` ends the slice.
    while p <= to + 50 {
        if let Some(pos) = text[start..].find(&format!("[[page:{}]]", p)) {
            end = start + pos;
            break;
        }
        p += 1;
    }
    let cleaned = chunker::clean_text(&text[start..end]);
    if cleaned.is_empty() { None } else { Some(cleaned) }
}

lazy_static::lazy_static! {
    static ref ROW_REF: regex::Regex = regex::Regex::new(r"(?i)\brows?\s+(?:no\.?\s*|number\s+|#)?(\d{1,6})\b").unwrap();
}

fn contains_phrase(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back().map(|c| !c.is_alphanumeric()).unwrap_or(true);
        let after = haystack[end..].chars().next().map(|c| !c.is_alphanumeric()).unwrap_or(true);
        if before && after {
            return true;
        }
        from = start + needle.len().max(1);
        if from >= haystack.len() {
            break;
        }
    }
    false
}

/// Spreadsheet questions: "row 42 of Fees", or anything naming a sheet, are
/// answered from that sheet's rows (with the header row) rather than from
/// whatever else happens to match.
async fn sheet_retrieval(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    query: &str,
    scope: Option<&HashSet<i64>>,
    opts: &RetrieveOptions,
    cache: &mut DocCache<'_>,
) -> Option<Retrieval> {
    let sheets = db.rag.sheet_labels(scope).ok()?;
    if sheets.is_empty() {
        return None;
    }
    let lower = query.to_lowercase();
    let named: Vec<(String, i64)> = sheets
        .iter()
        .filter(|(label, _)| {
            let name = label.trim_start_matches("Sheet:").trim().to_lowercase();
            name.chars().count() >= 2 && contains_phrase(&lower, &name)
        })
        .cloned()
        .collect();

    if let Some(row) = ROW_REF.captures(query).and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<i32>().ok()) {
        let targets = if named.is_empty() && sheets.len() == 1 { sheets.clone() } else { named.clone() };
        let mut ranked: Vec<(i64, f64)> = Vec::new();
        for (label, doc) in &targets {
            for c in db.rag.sheet_row_chunks(*doc, label, row).unwrap_or_default() {
                ranked.push((c.id, 1.0));
            }
        }
        if !ranked.is_empty() {
            return Some(Retrieval {
                route: "sheet_row".into(),
                passages: build_passages(db, &ranked, opts.budget_bytes, cache, opts),
                used_vectors: false,
                note: None,
            });
        }
    }

    if named.is_empty() {
        return None;
    }
    let docs: HashSet<i64> = named.iter().map(|(_, d)| *d).collect();
    let (ranked, used_vectors) = hybrid_rank(db, rt, query, Some(&docs), opts).await;
    let ids: Vec<i64> = ranked.iter().map(|(id, _)| *id).collect();
    let rows: HashMap<i64, ChunkRow> = db.rag.chunks_by_ids(&ids).unwrap_or_default().into_iter().map(|c| (c.id, c)).collect();
    let wanted: HashSet<(String, i64)> = named.into_iter().collect();
    let filtered: Vec<(i64, f64)> = ranked
        .into_iter()
        .filter(|(id, _)| {
            rows.get(id)
                .and_then(|c| c.location_label.clone().map(|l| wanted.contains(&(l, c.document_id))))
                .unwrap_or(false)
        })
        .collect();
    if filtered.is_empty() {
        return None;
    }
    Some(Retrieval {
        route: "sheet".into(),
        passages: build_passages(db, &filtered, opts.budget_bytes, cache, opts),
        used_vectors,
        note: None,
    })
}

fn last_page_marker(raw: &str) -> Option<i32> {
    let pos = raw.rfind("[[page:")?;
    let rest = &raw[pos + 7..];
    rest[..rest.find("]]")?].trim().parse().ok()
}

fn fuse(lists: &[Vec<i64>]) -> Vec<(i64, f64)> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut v: Vec<(i64, f64)> = scores.into_iter().collect();
    // Equal scores fall back to document order: chunk ids are assigned in
    // reading order when a document is indexed, so a lower id is an earlier page.
    // Scores are compared at 1e-9 resolution so sums that differ only in
    // floating-point rounding count as ties.
    v.sort_by(|a, b| {
        let (qa, qb) = ((a.1 * 1e9).round() as i64, (b.1 * 1e9).round() as i64);
        qb.cmp(&qa).then(a.0.cmp(&b.0))
    });
    v
}

/// Highest cosine similarity between `query` and any chunk in `scope`
/// (None when no embedding model is available).
pub async fn best_similarity(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    query: &str,
    scope: Option<&HashSet<i64>>,
) -> Option<f32> {
    let opts = RetrieveOptions::default();
    vector_candidates(db, rt, query, scope, &opts).await.and_then(|v| v.first().map(|(_, s)| *s))
}

/// Keyword evidence: how many distinct query words the best-matching chunk
/// in `scope` contains.
pub fn keyword_overlap(db: &MemoryDatabase, query: &str, scope: Option<&HashSet<i64>>) -> usize {
    let tokens = crate::memory_db::fts::content_tokens(query);
    let Some(q) = crate::memory_db::fts::fts_query_from_user_text(query) else { return 0 };
    let hits = db.rag.keyword_search(&q, scope, 3).unwrap_or_default();
    let ids: Vec<i64> = hits.iter().map(|(id, _, _)| *id).collect();
    let mut best = 0usize;
    let mut cache = DocCache::new(db);
    for c in db.rag.chunks_by_ids(&ids).unwrap_or_default() {
        let Some((_, text)) = cache.get(c.document_id).cloned() else { continue };
        let body = c.text(&text).to_lowercase();
        let n = tokens.iter().filter(|t| body.contains(t.as_str())).count();
        best = best.max(n);
    }
    best
}

async fn vector_candidates(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    query: &str,
    scope: Option<&HashSet<i64>>,
    opts: &RetrieveOptions,
) -> Option<Vec<(i64, f32)>> {
    if !rt.embedder.available() {
        return None;
    }
    if !rt.embedder.warm() && !opts.allow_cold_start {
        return None;
    }
    ensure_index_loaded(db, rt);
    if rt.index.read().map(|i| i.is_empty()).unwrap_or(true) {
        return None;
    }
    let raw = match tokio::time::timeout(Duration::from_secs(60), rt.embedder.embed_query(query)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            warn!("Query embedding failed, using keyword search only: {}", e);
            return None;
        }
        Err(_) => {
            warn!("Query embedding timed out, using keyword search only");
            return None;
        }
    };
    let q = vector::prepare(&raw);
    let shortlist = rt.index.read().ok()?.shortlist(&q, scope);
    let blobs = db.rag.vectors_for(&shortlist).ok()?;
    let mut scored: Vec<(i64, f32)> = shortlist
        .into_iter()
        .filter_map(|id| blobs.get(&id).and_then(|b| vector::score_blob(&q, b)).map(|s| (id, s)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(CANDIDATES as usize);
    Some(scored)
}

/// Load stored vectors into RAM once (and again after a model change).
pub fn ensure_index_loaded(db: &MemoryDatabase, rt: &RagRuntime) {
    let key = super::model::model_key();
    {
        if let Ok(idx) = rt.index.read() {
            if idx.loaded && idx.model_key == key {
                return;
            }
        }
    }
    let Ok(mut idx) = rt.index.write() else { return };
    if idx.loaded && idx.model_key == key {
        return;
    }
    idx.clear();
    let result = db.rag.for_each_vector(|id, doc, blob| idx.insert_blob(id, doc, blob));
    if let Err(e) = result {
        warn!("Could not load stored vectors: {}", e);
    }
    idx.model_key = key;
    idx.loaded = true;
    debug!("Vector index loaded: {} chunks", idx.len());
}

/// Load stored message vectors into RAM once (and again after a model change).
pub fn ensure_message_index_loaded(db: &MemoryDatabase, rt: &RagRuntime) {
    let key = super::model::model_key();
    if let Ok(idx) = rt.message_index.read() {
        if idx.loaded && idx.model_key == key {
            return;
        }
    }
    let Ok(mut idx) = rt.message_index.write() else { return };
    if idx.loaded && idx.model_key == key {
        return;
    }
    idx.clear();
    if let Err(e) = db.rag.for_each_message_vector(|id, session, blob| idx.insert_blob(id, session, blob)) {
        warn!("Could not load stored message vectors: {}", e);
    }
    if let Err(e) = db.rag.for_each_message_part_vector(|entry, session, blob| idx.insert_blob(entry, session, blob)) {
        warn!("Could not load stored message part vectors: {}", e);
    }
    idx.model_key = key;
    idx.loaded = true;
    debug!("Message vector index loaded: {} messages", idx.len());
}

/// Earlier chat messages similar in meaning to `query`, limited to
/// `allowed_sessions` (None = all chats) and excluding `exclude_session`.
pub async fn semantic_message_hits(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    query: &str,
    allowed_sessions: Option<&HashSet<String>>,
    exclude_session: &str,
    time_range: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    limit: usize,
    min_similarity: f32,
) -> Vec<(crate::memory_db::StoredMessage, f64)> {
    if !rt.embedder.available() {
        return Vec::new();
    }
    ensure_message_index_loaded(db, rt);
    if rt.message_index.read().map(|i| i.is_empty()).unwrap_or(true) {
        return Vec::new();
    }
    let scope: Option<HashSet<i64>> = match allowed_sessions {
        Some(a) => match db.rag.session_rowids(a) {
            Ok(s) => Some(s),
            Err(_) => return Vec::new(),
        },
        None => None,
    };
    let raw = match tokio::time::timeout(Duration::from_secs(60), rt.embedder.embed_query(query)).await {
        Ok(Ok(v)) => v,
        _ => return Vec::new(),
    };
    let q = vector::prepare(&raw);
    let shortlist = match rt.message_index.read() {
        Ok(idx) => idx.shortlist(&q, scope.as_ref()),
        Err(_) => return Vec::new(),
    };
    let mut blobs = db.rag.message_vectors_for(&shortlist).unwrap_or_default();
    blobs.extend(db.rag.message_part_vectors_for(&shortlist).unwrap_or_default());
    // Index entries are message parts; a message scores as its best part.
    let mut best: HashMap<i64, f32> = HashMap::new();
    for entry in shortlist {
        if let Some(s) = blobs.get(&entry).and_then(|b| vector::score_blob(&q, b)) {
            let id = super::store::entry_message(entry);
            let e = best.entry(id).or_insert(f32::MIN);
            if s > *e {
                *e = s;
            }
        }
    }
    let mut scored: Vec<(i64, f32)> = best.into_iter().filter(|(_, s)| *s >= min_similarity).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    scored.truncate(limit * 4);
    let ids: Vec<i64> = scored.iter().map(|(id, _)| *id).collect();
    let messages: HashMap<i64, crate::memory_db::StoredMessage> =
        db.rag.messages_by_ids(&ids).unwrap_or_default().into_iter().map(|m| (m.id, m)).collect();
    scored
        .into_iter()
        .filter_map(|(id, s)| messages.get(&id).cloned().map(|m| (m, s as f64)))
        .filter(|(m, _)| m.session_id != exclude_session)
        .filter(|(m, _)| time_range.map(|(a, b)| m.timestamp >= a && m.timestamp <= b).unwrap_or(true))
        .take(limit)
        .collect()
}

/// Hybrid ranking of chunk ids inside `scope`.
pub async fn hybrid_rank(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    query: &str,
    scope: Option<&HashSet<i64>>,
    opts: &RetrieveOptions,
) -> (Vec<(i64, f64)>, bool) {
    let keyword: Vec<i64> = match crate::memory_db::fts::fts_query_from_user_text(query) {
        Some(q) => db.rag.keyword_search(&q, scope, CANDIDATES).unwrap_or_default().into_iter().map(|(id, _, _)| id).collect(),
        None => Vec::new(),
    };
    let vectors = vector_candidates(db, rt, query, scope, opts).await;
    let used_vectors = vectors.is_some();

    let (vector_ids, gate): (Vec<i64>, Option<HashSet<i64>>) = match vectors {
        Some(v) => {
            let gate = opts.min_similarity.map(|min| v.iter().filter(|(_, s)| *s >= min).map(|(id, _)| *id).collect());
            (v.into_iter().map(|(id, _)| id).collect(), gate)
        }
        None => (Vec::new(), None),
    };

    let mut fused = fuse(&[keyword.clone(), vector_ids]);
    if let Some(min) = opts.min_similarity {
        match gate {
            // Unrequested material: keep only chunks the model judges similar.
            Some(g) => fused.retain(|(id, _)| g.contains(id)),
            // No vectors: require a strong keyword signal (several query terms).
            None => {
                let tokens = crate::memory_db::fts::content_tokens(query);
                if tokens.len() < 2 || min > 0.0 && keyword.is_empty() {
                    fused.clear();
                }
                fused.truncate(4);
            }
        }
    }
    (fused, used_vectors)
}

/// Build passages from ranked chunks: merge neighbours of the same section,
/// extend very short hits with their neighbours, respect the byte budget.
fn build_passages(
    db: &MemoryDatabase,
    ranked: &[(i64, f64)],
    budget_bytes: usize,
    cache: &mut DocCache,
    opts: &RetrieveOptions,
) -> Vec<Passage> {
    let ids: Vec<i64> = ranked.iter().map(|(id, _)| *id).take(PASSAGE_CANDIDATES).collect();
    let rows: HashMap<i64, ChunkRow> = db.rag.chunks_by_ids(&ids).unwrap_or_default().into_iter().map(|c| (c.id, c)).collect();

    struct Group {
        doc: i64,
        ord_from: i64,
        ord_to: i64,
        byte_start: i64,
        byte_end: i64,
        page_start: Option<i32>,
        page_end: Option<i32>,
        location: Option<String>,
        section: Option<String>,
        para: (Option<i32>, Option<i32>),
        prefix: Option<(usize, usize)>,
        score: f64,
    }

    let mut groups: Vec<Group> = Vec::new();
    for (id, score) in ranked.iter().take(PASSAGE_CANDIDATES) {
        let Some(c) = rows.get(id) else { continue };
        // Spreadsheet chunks are already a few rows each; merging neighbours
        // would turn a precise row hit back into a screenful.
        let is_sheet_chunk = c.location_label.as_deref().map(|l| l.starts_with("Sheet:")).unwrap_or(false);
        if let Some(g) = groups.iter_mut().find(|g| {
            !is_sheet_chunk
                && g.doc == c.document_id
                && c.ord >= g.ord_from - 1
                && c.ord <= g.ord_to + 1
                && (g.ord_to.max(c.ord) - g.ord_from.min(c.ord)) <= MAX_MERGED_EXTRA_CHUNKS
                && g.section == c.section_label
                && g.location == c.location_label
        }) {
            g.ord_from = g.ord_from.min(c.ord);
            g.para = (
                match (g.para.0, c.para_start) { (Some(a), Some(b)) => Some(a.min(b)), (a, b) => a.or(b) },
                match (g.para.1, c.para_end) { (Some(a), Some(b)) => Some(a.max(b)), (a, b) => a.or(b) },
            );
            g.prefix = g.prefix.or(c.prefix());
            g.ord_to = g.ord_to.max(c.ord);
            g.byte_start = g.byte_start.min(c.byte_start);
            g.byte_end = g.byte_end.max(c.byte_end);
            g.page_start = match (g.page_start, c.page_start) { (Some(a), Some(b)) => Some(a.min(b)), (a, b) => a.or(b) };
            g.page_end = match (g.page_end, c.page_end) { (Some(a), Some(b)) => Some(a.max(b)), (a, b) => a.or(b) };
            continue;
        }
        groups.push(Group {
            doc: c.document_id,
            ord_from: c.ord,
            ord_to: c.ord,
            byte_start: c.byte_start,
            byte_end: c.byte_end,
            page_start: c.page_start,
            page_end: c.page_end,
            location: c.location_label.clone(),
            section: c.section_label.clone(),
            para: (c.para_start, c.para_end),
            prefix: c.prefix(),
            score: *score,
        });
    }

    let best_score = groups.iter().map(|g| g.score).fold(0f64, f64::max);
    let mut out: Vec<Passage> = Vec::new();
    let mut used = 0usize;
    for mut g in groups {
        if used >= budget_bytes || out.len() >= opts.max_passages.max(1) {
            break;
        }
        if !out.is_empty() && best_score > 0.0 && g.score < best_score * RELATIVE_SCORE_FLOOR {
            continue;
        }
        let is_sheet = g.location.as_deref().map(|l| l.starts_with("Sheet:")).unwrap_or(false);
        if !is_sheet && (g.byte_end - g.byte_start) as usize <= SMALL_PASSAGE_BYTES {
            if let Ok(neigh) = db.rag.neighbour_chunks(g.doc, g.ord_from - 1, g.ord_to + 1) {
                for n in neigh.iter().filter(|n| n.section_label == g.section) {
                    g.byte_start = g.byte_start.min(n.byte_start);
                    g.byte_end = g.byte_end.max(n.byte_end);
                    g.page_start = match (g.page_start, n.page_start) { (Some(a), Some(b)) => Some(a.min(b)), (a, b) => a.or(b) };
                    g.page_end = match (g.page_end, n.page_end) { (Some(a), Some(b)) => Some(a.max(b)), (a, b) => a.or(b) };
                }
            }
        }
        let Some((filename, text)) = cache.get(g.doc).cloned() else { continue };
        let (s, e) = (g.byte_start as usize, (g.byte_end as usize).min(text.len()));
        if s >= e || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        let remaining = budget_bytes - used;
        if remaining < 200 && !out.is_empty() {
            break;
        }
        let limit = remaining.min(opts.max_passage_bytes.max(400));
        let raw = &text[s..e];
        let header = match g.prefix {
            Some((ps, pe)) if pe <= s && text.is_char_boundary(ps) && text.is_char_boundary(pe) => {
                format!("{}\n", chunker::clean_text(&text[ps..pe]))
            }
            _ => String::new(),
        };
        let limit = limit.saturating_sub(header.len()).max(200);
        let (body, truncated) = if chunker::clean_text(raw).len() <= limit {
            (format!("{}{}", header, chunker::clean_text(raw)), false)
        } else {
            // Cut the raw text so the citation can name only pages actually shown.
            let mut cut = (limit + 32).min(raw.len());
            while cut > 0 && !raw.is_char_boundary(cut) {
                cut -= 1;
            }
            let shown = &raw[..cut];
            if let Some(last_page) = last_page_marker(shown) {
                g.page_end = Some(last_page.max(g.page_start.unwrap_or(last_page)));
            } else if g.page_start.is_some() {
                g.page_end = g.page_start;
            }
            let (t, _) = truncate_to(&chunker::clean_text(shown), limit);
            (format!("{}{}", header, t), true)
        };
        used += body.len();
        let citation = chunker::citation(g.page_start, g.page_end, g.location.as_deref(), g.section.as_deref(), g.para.0, g.para.1);
        out.push(Passage {
            document_id: g.doc,
            filename,
            page_start: g.page_start,
            page_end: g.page_end,
            section_label: g.section,
            citation,
            text: body,
            score: g.score,
            truncated,
            version_label: None,
        });
    }
    out
}

fn docs_matching_hint(db: &MemoryDatabase, candidates: &[i64], hint: &str) -> Vec<i64> {
    let hint = hint.to_lowercase();
    let stem = hint.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(hint.clone());
    candidates
        .iter()
        .copied()
        .filter(|id| {
            db.documents
                .get_document(*id)
                .map(|d| {
                    let f = d.original_filename.to_lowercase();
                    f == hint || f.contains(&stem)
                })
                .unwrap_or(false)
        })
        .collect()
}

fn scope_list(scope: Option<&HashSet<i64>>, prefer: &[i64], db: &MemoryDatabase) -> Vec<i64> {
    if !prefer.is_empty() {
        return prefer.to_vec();
    }
    match scope {
        Some(s) => {
            let mut v: Vec<i64> = s.iter().copied().collect();
            v.sort_unstable_by(|a, b| b.cmp(a));
            v
        }
        None => db.rag.all_document_ids().unwrap_or_default(),
    }
}

/// Retrieve cited passages for `query`.
///
/// `prefer` are the chat's own documents: exact lookups look there first and
/// searches are restricted to them when non-empty. `scope` bounds everything.
pub async fn retrieve(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    query: &str,
    scope: Option<&HashSet<i64>>,
    prefer: &[i64],
    opts: &RetrieveOptions,
) -> Retrieval {
    let mut retrieval = retrieve_inner(db, rt, query, scope, prefer, opts).await;
    add_other_versions(db, rt, query, scope, prefer, opts, &mut retrieval).await;
    retrieval
}

/// When an excerpt's document has other versions in scope, also show the
/// matching part of each other version, and label every excerpt with its
/// version so answers can say which version a point comes from.
async fn add_other_versions(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    query: &str,
    scope: Option<&HashSet<i64>>,
    prefer: &[i64],
    opts: &RetrieveOptions,
    retrieval: &mut Retrieval,
) {
    if retrieval.passages.is_empty() {
        return;
    }
    let mut candidates: Vec<i64> = match scope {
        Some(s) => s.iter().copied().collect(),
        None => db.rag.all_document_ids().unwrap_or_default(),
    };
    candidates.extend(prefer.iter().copied());
    candidates.sort_unstable();
    candidates.dedup();

    let present: Vec<i64> = {
        let mut v: Vec<i64> = retrieval.passages.iter().map(|p| p.document_id).collect();
        v.dedup();
        v
    };
    let extra_budget = opts.budget_bytes / 4;
    let mut used = 0usize;
    let mut additions: Vec<Passage> = Vec::new();
    let mut cache = DocCache::new(db);
    for doc in present.iter().take(3) {
        for sibling in super::versions::siblings(db, *doc, &candidates) {
            if used >= extra_budget
                || retrieval.passages.iter().any(|p| p.document_id == sibling)
                || additions.iter().any(|p| p.document_id == sibling)
            {
                continue;
            }
            let _ = db.rag.ensure_chunked(&[sibling]);
            let reference = retrieval.passages.iter().find(|p| p.document_id == *doc).cloned();
            let one: HashSet<i64> = [sibling].into_iter().collect();
            let mut found: Vec<Passage> = Vec::new();
            if retrieval.route == "page" {
                if let (Some(r), Some((filename, text))) = (reference.as_ref(), cache.get(sibling).cloned()) {
                    if let (Some(from), Some(to)) = (r.page_start, r.page_end) {
                        if let Some(body) = page_text(&text, from, to) {
                            let (body, truncated) = truncate_to(&body, extra_budget.saturating_sub(used).max(400));
                            found.push(Passage {
                                document_id: sibling,
                                filename,
                                page_start: Some(from),
                                page_end: Some(to),
                                section_label: None,
                                citation: chunker::citation(Some(from), Some(to), None, None, None, None),
                                text: body,
                                score: r.score,
                                truncated,
                                version_label: None,
                            });
                        }
                    }
                }
            }
            if found.is_empty() {
                let section_key = reference
                    .as_ref()
                    .and_then(|r| r.section_label.as_deref())
                    .map(chunker::section_number_key)
                    .filter(|k| !k.is_empty());
                let ranked: Vec<(i64, f64)> = match section_key {
                    Some(key) => db.rag.section_search(&key, Some(&one), 2).unwrap_or_default().into_iter().map(|c| (c.id, 1.0)).collect(),
                    None => Vec::new(),
                };
                let ranked = if ranked.is_empty() {
                    let mut o = opts.clone();
                    o.min_similarity = None;
                    hybrid_rank(db, rt, query, Some(&one), &o).await.0.into_iter().take(1).collect()
                } else {
                    ranked
                };
                found = build_passages(db, &ranked, extra_budget.saturating_sub(used).max(400), &mut cache, opts);
                found.truncate(1);
            }
            for p in found {
                used += p.text.len();
                additions.push(p);
            }
        }
    }
    retrieval.passages.extend(additions);

    let ids: Vec<i64> = {
        let mut v: Vec<i64> = retrieval.passages.iter().map(|p| p.document_id).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let mut family_ids = candidates.clone();
    family_ids.extend(ids.iter().copied());
    family_ids.sort_unstable();
    family_ids.dedup();
    let versions = super::versions::families(db, &family_ids);
    for p in &mut retrieval.passages {
        if let Some(v) = versions.get(&p.document_id) {
            p.version_label = Some(v.label());
        }
    }
}

async fn retrieve_inner(
    db: &MemoryDatabase,
    rt: &RagRuntime,
    query: &str,
    scope: Option<&HashSet<i64>>,
    prefer: &[i64],
    opts: &RetrieveOptions,
) -> Retrieval {
    let _ = db.rag.ensure_chunked(prefer);
    let search_scope: Option<HashSet<i64>> = if prefer.is_empty() {
        scope.cloned()
    } else {
        Some(prefer.iter().copied().filter(|d| scope.map(|s| s.contains(d)).unwrap_or(true)).collect())
    };
    let search_scope_ref = search_scope.as_ref();
    let mut cache = DocCache::new(db);

    let route = if opts.use_router {
        let terms = db.rag.all_terms(search_scope_ref, 400).unwrap_or_default();
        router::classify(query, &terms)
    } else {
        Route::Semantic
    };

    match &route {
        Route::Page { from, to, file_hint } => {
            let mut docs = scope_list(search_scope_ref, prefer, db);
            if let Some(h) = file_hint {
                let hinted = docs_matching_hint(db, &scope_list(scope, &[], db), h);
                if !hinted.is_empty() {
                    docs = hinted;
                }
            }
            let mut passages = Vec::new();
            let mut used = 0usize;
            for id in docs.into_iter().take(12) {
                let Some((filename, text)) = cache.get(id).cloned() else { continue };
                if let Some(body) = page_text(&text, *from, *to) {
                    let remaining = opts.budget_bytes.saturating_sub(used);
                    if remaining < 200 {
                        break;
                    }
                    let (body, truncated) = truncate_to(&body, remaining);
                    used += body.len();
                    passages.push(Passage {
                        document_id: id,
                        filename,
                        page_start: Some(*from),
                        page_end: Some(*to),
                        section_label: None,
                        citation: chunker::citation(Some(*from), Some(*to), None, None, None, None),
                        text: body,
                        score: 1.0,
                        truncated,
                        version_label: None,
                    });
                }
            }
            if !passages.is_empty() {
                let note = (passages.len() > 1 && file_hint.is_none()).then(|| {
                    format!(
                        "Page {} exists in {} documents; each is shown below. If only one was meant, the user can name the file.",
                        from,
                        passages.len()
                    )
                });
                return Retrieval { route: "page".into(), passages, used_vectors: false, note };
            }
            let (ranked, used_vectors) = hybrid_rank(db, rt, query, search_scope_ref, opts).await;
            return Retrieval {
                route: "page_fallback".into(),
                passages: build_passages(db, &ranked, opts.budget_bytes, &mut cache, opts),
                used_vectors,
                note: Some(format!("No document in scope has page {}; showing the closest matches instead.", from)),
            };
        }
        Route::Section { number_key, file_hint } => {
            let mut section_scope = search_scope.clone();
            if let Some(h) = file_hint {
                let hinted = docs_matching_hint(db, &scope_list(scope, &[], db), h);
                if !hinted.is_empty() {
                    section_scope = Some(hinted.into_iter().collect());
                }
            }
            let all_rows = db.rag.section_search(number_key, section_scope.as_ref(), 200).unwrap_or_default();
            // The clause itself starts at the first chunk carrying its label;
            // later chunks inherit the label until the next heading, so keep
            // only the opening chunks of each matching section.
            let mut rows: Vec<ChunkRow> = Vec::new();
            for r in all_rows {
                let opening = rows
                    .iter()
                    .filter(|x| x.document_id == r.document_id && x.section_label == r.section_label)
                    .count();
                let contiguous = rows
                    .iter()
                    .any(|x| x.document_id == r.document_id && x.section_label == r.section_label && x.ord + 1 == r.ord);
                if opening == 0 || (opening < 2 && contiguous) {
                    rows.push(r);
                }
            }
            if !rows.is_empty() {
                let ranked: Vec<(i64, f64)> = rows.iter().enumerate().map(|(i, r)| (r.id, 1.0 / (i as f64 + 1.0))).collect();
                let passages = build_passages(db, &ranked, opts.budget_bytes, &mut cache, opts);
                if !passages.is_empty() {
                    return Retrieval { route: "section".into(), passages, used_vectors: false, note: None };
                }
            }
        }
        Route::Definition { term } => {
            let rows = db.rag.term_search(term, search_scope_ref, 6).unwrap_or_default();
            if !rows.is_empty() {
                let ranked: Vec<(i64, f64)> = rows.iter().enumerate().map(|(i, r)| (r.id, 1.0 / (i as f64 + 1.0))).collect();
                let mut passages = build_passages(db, &ranked, opts.budget_bytes / 2, &mut cache, opts);
                // Also show where the term is used.
                let (ranked, used_vectors) = hybrid_rank(db, rt, query, search_scope_ref, opts).await;
                let used: usize = passages.iter().map(|p| p.text.len()).sum();
                let seen: HashSet<(i64, Option<i32>)> = passages.iter().map(|p| (p.document_id, p.page_start)).collect();
                for p in build_passages(db, &ranked, opts.budget_bytes.saturating_sub(used), &mut cache, opts) {
                    if !seen.contains(&(p.document_id, p.page_start)) {
                        passages.push(p);
                    }
                }
                return Retrieval { route: "definition".into(), passages, used_vectors, note: None };
            }
        }
        Route::Coverage => {
            let docs = scope_list(search_scope_ref, prefer, db);
            if docs.len() > 1 {
                let per_doc_budget = (opts.budget_bytes / docs.len().min(12)).max(600);
                let mut passages = Vec::new();
                let mut used_vectors = false;
                for id in docs.iter().take(12) {
                    let one: HashSet<i64> = [*id].into_iter().collect();
                    let (mut ranked, uv) = hybrid_rank(db, rt, query, Some(&one), opts).await;
                    used_vectors |= uv;
                    if ranked.is_empty() {
                        ranked = db.rag.document_chunks(*id).unwrap_or_default().iter().take(2).map(|c| (c.id, 0.0)).collect();
                    }
                    passages.extend(build_passages(db, &ranked, per_doc_budget, &mut cache, opts));
                }
                return Retrieval { route: "coverage".into(), passages, used_vectors, note: None };
            }
        }
        Route::Semantic => {}
    }

    if let Some(sheet) = sheet_retrieval(db, rt, query, search_scope_ref, opts, &mut cache).await {
        return sheet;
    }

    let (ranked, used_vectors) = hybrid_rank(db, rt, query, search_scope_ref, opts).await;
    Retrieval {
        route: "semantic".into(),
        passages: build_passages(db, &ranked, opts.budget_bytes, &mut cache, opts),
        used_vectors,
        note: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_text_slices_between_markers() {
        let text = "[[page:1]]\nOne\n\n[[page:2]]\nTwo\n\n[[page:3]]\nThree";
        assert_eq!(page_text(text, 2, 2).unwrap(), "Two");
        assert_eq!(page_text(text, 1, 2).unwrap(), "One\n\nTwo");
        assert_eq!(page_text(text, 3, 3).unwrap(), "Three");
        assert!(page_text(text, 9, 9).is_none());
    }

    #[test]
    fn ties_keep_reading_order() {
        let db = MemoryDatabase::new_in_memory().unwrap();
        // Every page used to read "The indemnity survives termination."
        // verbatim. Six identical lines over six pages is a running header as
        // far as the chunker is concerned, so it removed all of them, the
        // document produced ZERO chunks, and this test panicked on `ids[1]`
        // with "index out of bounds: the len is 0 but the index is 1" rather
        // than testing tie ordering at all.
        // See eval::distinct_page_text.
        let text = (1..=6)
            .map(|p| format!("[[page:{}]]\n{}", p, crate::rag::eval::distinct_page_text(p)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let doc = db
            .documents
            .upsert_document(crate::memory_db::NewDocument {
                original_bytes: text.as_bytes(),
                original_filename: "ties.txt",
                source_path: None,
                source_kind: "paperclip",
                mime_type: None,
                size_bytes: text.len() as i64,
                extracted_text: text.clone(),
                extraction_status: "ok",
                extraction_error: None,
                local_file_id: None,
            })
            .unwrap();
        let chunks = db.rag.document_chunks(doc.id).unwrap();
        let ids: Vec<i64> = chunks.iter().map(|c| c.id).collect();
        // A later page ranked first by one list and an earlier page ranked
        // first by the other: identical fused scores.
        let (early, late) = (ids[1], ids[4]);
        let fused = fuse(&[vec![late, early], vec![early, late]]);
        assert!((fused[0].1 - fused[1].1).abs() < 1e-12, "scores must tie");
        let pages: Vec<Option<i32>> = fused
            .iter()
            .map(|(id, _)| chunks.iter().find(|c| c.id == *id).unwrap().page_start)
            .collect();
        let mut sorted = pages.clone();
        sorted.sort();
        assert_eq!(pages, sorted, "tied results must be in page order");
    }

    #[test]
    fn rrf_rewards_agreement() {
        let fused = fuse(&[vec![1, 2, 3], vec![3, 4, 1]]);
        assert_eq!(fused[0].0, 1);
        assert_eq!(fused[1].0, 3);
    }
}
