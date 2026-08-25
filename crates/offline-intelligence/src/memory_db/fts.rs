//! Full-text search over documents and conversation messages (SQLite FTS5).
//!
//! This is the retrieval engine for "what did we discuss / which document said
//! that". It replaces nothing — the embedding model this product used to ship
//! is gone, so lexical search IS the retrieval layer now, and its quality
//! ceiling is the product's.
//!
//! # Verified properties (probed against this exact build, not assumed)
//!
//! SQLite 3.46.0 with `ENABLE_FTS5 = 1`, so FTS5 and `bm25()` are available.
//! The `porter` tokenizer stems inflections: a search for `termination` matches
//! text containing `terminate`, and `terminated` matches it too.
//!
//! Porter alone does NOT stem derivational shifts, and measurably maps one word
//! family to several different tokens — verified with `fts5vocab`:
//! `indemnification` is stored as `indemnif`, `indemnify` as `indemnifi`, and
//! `indemnity` as `indemn`. Exact-term matching therefore never connected them.
//!
//! That gap is closed on the QUERY side rather than by changing the tokenizer:
//! all three stems share the prefix `indemn`, and a prefix query reaches every
//! one of them (`indemn*` → all three, measured). `fts_query_from_user_text`
//! derives such a prefix per token via `morphological_prefix` and searches for
//! the exact term OR the family prefix. The index is untouched, so this needed
//! no migration and no rebuild.
//!
//! Words that share no letters are related by a curated synonym list
//! (`utils::thesaurus`), also applied query-side: `lease` additionally searches
//! `tenancy`, `leasehold` and `letting`. Synonyms are appended AFTER the exact
//! term and the family prefix, so a document containing the user's own word
//! matches more clauses and BM25 still ranks it first — recall grows without
//! costing precision. `MAX_SYNONYM_CLAUSES` bounds the total.
//!
//! What remains true is the honest limit: this layer matches WORDS, their
//! WRITTEN FAMILIES, and their LISTED SYNONYMS — not meaning. A phrase and a
//! differently-worded description of the same concept ("may terminate without
//! cause" versus "termination for convenience") still do not connect, and no
//! thesaurus will connect them. That needs a model.
//!
//! # Why external-content tables
//!
//! Both indexes use `content=` so FTS5 reads the text from the base table
//! rather than storing a second copy. `documents.extracted_text` holds whole
//! contracts; duplicating that would roughly double the database for no gain.
//! Verified: external-content indexes return exact and stemmed hits, `rebuild`
//! backfills existing rows, and triggers keep insert/update/delete in sync.
//!
//! # Why this is not a numbered migration
//!
//! Deliberate, and a departure from the `migrations/` pattern. Migrations run
//! only on the on-disk path (`MemoryDatabase::new`); the in-memory database
//! used throughout the test suite is built from `schema::SCHEMA_SQL` plus each
//! store's `initialize_schema` and never runs them. A numbered migration would
//! therefore need its DDL duplicated for the in-memory path — the exact drift
//! that forced migration 010 to special-case itself.
//!
//! An FTS index is DERIVED data: every byte is reconstructible from the base
//! tables. So the right shape is an idempotent `ensure_fts_schema` invoked on
//! both paths at startup, which additionally self-heals — if the index is ever
//! missing, dropped, or emptied, the next startup rebuilds it instead of
//! needing a new schema version. Version-gating a cache buys nothing.

use rusqlite::Connection;
use tracing::{info, warn};

/// Minimum token length to index a search term on. One- and two-character
/// tokens ("a", "of", "is") match nearly everything and only add noise to a
/// BM25 ranking.
const MIN_TOKEN_CHARS: usize = 3;

/// Conversational filler that must never be used as a SEARCH term.
///
/// This is not a generic English stop-word list. It is specifically the
/// vocabulary of ASKING, and it exists because of a measured failure: the
/// question "what else did we discuss in our previous conversation?" built the
/// query `what OR else OR did OR discuss OR our OR previous OR conversation`.
/// Every one of those describes the ACT of conversing; none describes what the
/// conversation was ABOUT. Words that common also collapse BM25's IDF term, so
/// the ranking degrades to noise on top of matching the wrong thing.
///
/// Deliberately EXCLUDED from this list, despite being frequent English:
/// `will`, `trust`, `action`, `party`, `charge`, `interest`, `note`, `title`,
/// `record`, `security`, `motion`, `brief`, `service`, `instrument`. Each is a
/// legal noun this product must be able to search for - "my will", "the trust
/// deed", "the charge over the property". They are handled instead by
/// `thesaurus::AMBIGUOUS_TERMS`, which stops them being EXPANDED without
/// stopping them being SEARCHED.
const STOP_WORDS: &[&str] = &[
    // pronouns and determiners
    "the", "and", "but", "for", "with", "from", "that", "this", "these",
    "those", "they", "them", "their", "there", "here", "you", "your", "yours",
    "our", "ours", "its", "his", "her", "him", "she", "hers", "who", "whom",
    "whose", "which", "what", "when", "where", "why", "how", "all", "any",
    "each", "some", "other", "another", "both", "either", "neither",
    // auxiliaries and common verbs of asking
    "are", "was", "were", "been", "being", "have", "has", "had", "having",
    "does", "did", "doing", "can", "could", "would", "should", "please",
    "let", "get", "got", "give", "want", "need", "know", "think", "make",
    // the vocabulary of referring to a conversation - the actual defect
    "discuss", "discussed", "discussing", "discussion", "conversation",
    "conversations", "chat", "chats", "talk", "talked", "talking",
    "mention", "mentioned", "say", "said", "tell", "told", "ask", "asked",
    "remember", "remembered", "recall", "recap", "summarise", "summarize",
    "summarised", "summarized", "summarisation", "summarization",
    "previous", "previously", "earlier", "before", "already", "again",
    "back", "last", "past", "ago", "then", "now", "still", "yet",
    // filler
    "else", "also", "just", "very", "much", "many", "more", "most", "less",
    "than", "too", "only", "own", "same", "such", "about", "into", "over",
    "under", "out", "off", "down", "not", "nor", "yes", "okay", "thanks",
    "possible", "possibly", "maybe", "perhaps", "something", "anything",
    "everything", "nothing", "someone", "anyone", "everyone",
];

fn is_stop_word(token: &str) -> bool {
    STOP_WORDS.contains(&token)
}

/// Content-bearing tokens from user text: lowercased, long enough, not filler.
///
/// Exposed because two callers need the SAME answer. The query builder uses it
/// to avoid searching for filler; past-material retrieval uses "is this empty?"
/// to decide that a question is asking about the conversation itself rather
/// than about any subject, which needs a completely different kind of
/// retrieval (recency, not relevance).
pub fn content_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= MIN_TOKEN_CHARS)
        .map(|t| t.to_lowercase())
        .filter(|t| !is_stop_word(t))
        .take(MAX_QUERY_TOKENS)
        .collect()
}

/// Upper bound on synonym clauses added to one query.
///
/// Separate from `MAX_QUERY_TOKENS` because the two bound different risks.
/// The token cap stops a pasted paragraph becoming an enormous query; this cap
/// stops a short but highly expandable question doing the same. A query of six
/// well-known legal terms could otherwise pull in dozens of alternatives, and
/// past a point every extra OR clause matches more documents while telling
/// BM25 less about which one is right.
const MAX_SYNONYM_CLAUSES: usize = 24;

/// Upper bound on tokens taken from one query. A pasted paragraph would
/// otherwise build an enormous OR query that matches everything, which is both
/// slow and useless as a ranking.
const MAX_QUERY_TOKENS: usize = 12;

/// Relative BM25 weight of a document's FILENAME versus its body text.
///
/// A query naming a file ("what's in schedule-b.pdf") should rank that
/// document above one that merely mentions "schedule" a few times in its body.
/// Filenames are short, so without an explicit weight BM25's length
/// normalisation already favours them somewhat — this makes the intent
/// explicit and strong enough to be reliable.
const FILENAME_WEIGHT: f64 = 8.0;
const BODY_WEIGHT: f64 = 1.0;

/// DDL for both indexes and the triggers that keep them current.
///
/// `IF NOT EXISTS` throughout so this is safe to run on every startup.
///
/// Trigger shape is the form FTS5 requires for external-content tables: a
/// delete is expressed as an `INSERT ... VALUES('delete', ...)` carrying the
/// OLD values, because FTS5 needs the previous text to remove the right index
/// entries. An update is a delete of the old row followed by an insert of the
/// new one. Verified end to end: after an UPDATE the old term stops matching
/// and the new term starts; after a DELETE the term stops matching.
const FTS_DDL: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
    original_filename,
    extracted_text,
    content='documents',
    content_rowid='id',
    tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS documents_fts_ai AFTER INSERT ON documents BEGIN
    INSERT INTO documents_fts(rowid, original_filename, extracted_text)
    VALUES (new.id, new.original_filename, new.extracted_text);
END;

CREATE TRIGGER IF NOT EXISTS documents_fts_ad AFTER DELETE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, original_filename, extracted_text)
    VALUES ('delete', old.id, old.original_filename, old.extracted_text);
END;

CREATE TRIGGER IF NOT EXISTS documents_fts_au AFTER UPDATE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, original_filename, extracted_text)
    VALUES ('delete', old.id, old.original_filename, old.extracted_text);
    INSERT INTO documents_fts(rowid, original_filename, extracted_text)
    VALUES (new.id, new.original_filename, new.extracted_text);
END;

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    content='messages',
    content_rowid='id',
    tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS messages_fts_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content)
    VALUES ('delete', old.id, old.content);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content)
    VALUES ('delete', old.id, old.content);
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;
";

/// Create the FTS indexes and their triggers if absent, backfilling any index
/// this call newly created.
///
/// Idempotent and safe on every startup. Call AFTER the base tables exist
/// (migrations on the on-disk path, `schema::SCHEMA_SQL` plus the stores'
/// `initialize_schema` in memory).
///
/// Errors are returned rather than swallowed, but callers should treat a
/// failure as non-fatal: without an index, search degrades to finding nothing
/// (which the caller reports honestly) rather than the app failing to start.
///
/// # Why backfill is keyed on "did I just create this table"
///
/// The intuitive check — "is the index empty?" — cannot be written, which was
/// measured rather than assumed:
///
/// * `SELECT count(*) FROM documents_fts` returns the row count of the CONTENT
///   table, not of the index. It reads 1 for a base table with one row whether
///   the index holds that row or nothing at all.
/// * `SELECT count(*) FROM documents_fts_data` (the shadow table) only ever
///   grows: 2 freshly created, 3 after a rebuild, and 4 after the index was
///   emptied. A larger number does not mean more indexed content.
///
/// The only reliable signal is running an actual MATCH, which costs the same as
/// just rebuilding. So instead of probing, this rebuilds exactly when it has
/// created the index itself — which is precisely the upgrade case that matters:
/// an existing installation whose whole library predates the index and for
/// which no trigger has ever fired. Once the index exists, the triggers keep it
/// current (proven by this module's update/delete tests).
///
/// For the rare case of an index that exists but has lost its content, use
/// [`rebuild_fts_indexes`] explicitly. There is deliberately no automatic
/// detection, because there is no cheap way to detect it and a silent
/// unconditional rebuild on every launch would re-tokenize the entire library
/// each time the app starts.
pub fn ensure_fts_schema(conn: &Connection) -> anyhow::Result<()> {
    // Base tables must exist first. In-memory pools built by a single store's
    // initialize_schema legitimately have one and not the other, so each index
    // is set up independently instead of failing the pair.
    let documents_ready = table_exists(conn, "documents")?;
    let messages_ready = table_exists(conn, "messages")?;
    if !documents_ready && !messages_ready {
        return Ok(());
    }

    // Checked BEFORE the DDL runs - afterwards it always exists and the
    // distinction that decides backfill is gone.
    let documents_index_existed = table_exists(conn, "documents_fts")?;
    let messages_index_existed = table_exists(conn, "messages_fts")?;

    // execute_batch on the whole DDL would abort at the first statement whose
    // base table is missing, taking the other index with it. Split so each
    // half stands alone.
    if documents_ready {
        conn.execute_batch(documents_ddl())?;
        if !documents_index_existed {
            rebuild_index(conn, "documents_fts", "documents")?;
        }
    }
    if messages_ready {
        conn.execute_batch(&messages_ddl())?;
        if !messages_index_existed {
            rebuild_index(conn, "messages_fts", "messages")?;
        }
    }
    Ok(())
}

/// Rebuild both indexes from their base tables, unconditionally.
///
/// The explicit repair path for an index that exists but has lost content.
/// Cost is proportional to the whole corpus, so this is never called
/// automatically - see [`ensure_fts_schema`].
pub fn rebuild_fts_indexes(conn: &Connection) -> anyhow::Result<()> {
    if table_exists(conn, "documents_fts")? {
        rebuild_index(conn, "documents_fts", "documents")?;
    }
    if table_exists(conn, "messages_fts")? {
        rebuild_index(conn, "messages_fts", "messages")?;
    }
    Ok(())
}

fn documents_ddl() -> &'static str {
    FTS_DDL
        .split("CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts")
        .next()
        .unwrap_or("")
}

fn messages_ddl() -> String {
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts{}",
        FTS_DDL
            .split("CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts")
            .nth(1)
            .unwrap_or("")
    )
}

fn table_exists(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |r| r.get(0),
        )
        .ok();
    Ok(found.is_some())
}

/// Index every existing row of `base_table` into `fts_table`.
///
/// FTS5's `'rebuild'` command discards the index and reconstructs it from the
/// content table, so this is safe to run on a populated index as well as an
/// empty one - verified: rebuilding twice leaves the index correct both times.
fn rebuild_index(conn: &Connection, fts_table: &str, base_table: &str) -> anyhow::Result<()> {
    let rows: i64 = conn
        .query_row(&format!("SELECT count(*) FROM {}", base_table), [], |r| r.get(0))
        .unwrap_or(0);
    if rows == 0 {
        // Nothing to index. Skipped rather than run for its own sake, so a
        // fresh install does no pointless work.
        return Ok(());
    }
    info!(
        "Building {} full-text index over {} existing {} row(s)",
        fts_table, rows, base_table
    );
    if let Err(e) = conn.execute(
        &format!("INSERT INTO {}({}) VALUES('rebuild')", fts_table, fts_table),
        [],
    ) {
        // Named loudly: search silently returning nothing is far worse than a
        // visible failure, because it is indistinguishable from "no results".
        warn!(
            "Failed to build {} index: {} - full-text search over {} will return \
             nothing until this succeeds",
            fts_table, e, base_table
        );
        return Err(e.into());
    }
    Ok(())
}

/// Shortest a stripped stem may be before it is used as a search prefix.
///
/// Below this, a prefix stops being a word and starts being a syllable.
/// Measured example: "creation" minus "ation" leaves "cre", and `cre*` matches
/// create, credit, crew, cream, credential — noise, not recall. "indemn" (6) and
/// "termin" (6) are the shape we want. 5 is the lowest value that admits real
/// stems like "oblig" (from obligation) while rejecting "cre".
const MIN_STEM_CHARS: usize = 5;

/// Length at or above which a word with NO recognised suffix is still worth
/// searching as a prefix. "contract" → `contract*` also finds contracts,
/// contractual, contractor. Short words are left exact, because a 4-letter
/// prefix matches far too much.
const MIN_BARE_PREFIX_CHARS: usize = 6;

/// Derivational and inflectional endings, LONGEST FIRST so the most specific
/// match wins ("ification" must be tried before "ation", which must be tried
/// before "ion").
///
/// This is a deliberately blunt suffix stripper, not a linguistic stemmer. Its
/// only job is to find a common PREFIX shared by a word's morphological family,
/// which is a much easier target than producing a correct lemma — it does not
/// matter that "indemn" is not a word, only that indemnify, indemnification,
/// indemnity, indemnitor and indemnified all begin with it.
const DERIVATIONAL_SUFFIXES: &[&str] = &[
    "ifications", "ification", "ications", "ication",
    "abilities", "ability", "ibilities", "ibility",
    "izations", "ization", "isations", "isation",
    "ifying", "ifies", "ified", "ify",
    "ations", "ation", "itions", "ition", "utions", "ution",
    "sions", "sion", "tions", "tion",
    "ances", "ance", "ences", "ence",
    "ments", "ment",
    "ities", "ity",
    "ives", "ive",
    "ings", "ing",
    "ates", "ated", "ating", "ate",
    "ors", "ies", "ied",
    "als", "al",
    "es", "ed", "s",
];

/// Reduce a word to the prefix shared by its morphological family, or None when
/// no reduction is safe.
///
/// This is what makes `indemnify` find `indemnification`. Porter stemming — which
/// the INDEX uses — handles inflection but not derivation, and measurably maps
/// those two words to different tokens (`indemnifi` vs `indemnif`, with
/// `indemnity` a third at `indemn`). Since all three nevertheless share the
/// prefix `indemn`, a prefix query reaches all of them where an exact term
/// cannot.
fn morphological_prefix(token: &str) -> Option<String> {
    for suffix in DERIVATIONAL_SUFFIXES {
        if let Some(stem) = token.strip_suffix(suffix) {
            if stem.chars().count() >= MIN_STEM_CHARS {
                return Some(stem.to_string());
            }
            // A recognised suffix that would leave too little behind: stop
            // rather than trying a shorter suffix, which would only leave MORE.
            return None;
        }
    }
    None
}

/// Turn arbitrary user text into a safe FTS5 MATCH expression.
///
/// Returns `None` when there is nothing worth searching for, which callers MUST
/// treat as "do not search" rather than "match everything".
///
/// User text cannot be passed to MATCH directly. FTS5 has its own query
/// grammar, so a quote, `*`, `-`, `:`, `^`, or a bare `AND`/`OR`/`NOT`/`NEAR`
/// either changes the query's meaning or raises a syntax error — and a question
/// like `what about "termination" - specifically?` contains three of those. The
/// safe construction is to discard the grammar entirely: keep alphanumeric
/// tokens, quote each one so it is treated as a literal, and join with OR.
///
/// OR rather than AND is deliberate. This drives relevance ranking, not
/// filtering: BM25 already scores a row matching four query terms above one
/// matching a single term, so OR plus ranking degrades gracefully where AND
/// would return nothing the moment one word is absent.
pub fn fts_query_from_user_text(text: &str) -> Option<String> {
    // Filler is removed BEFORE anything else. Searching for "what" and "our"
    // does not merely waste clauses - those terms appear in nearly every stored
    // message, so they flatten the ranking of the terms that do matter.
    //
    // An all-filler question yields None, which every caller already treats as
    // "do not search". That is the correct outcome, not a degraded one: the
    // question was never about a subject, and answering it needs recency-based
    // recall instead (see context_engine::past_retrieval).
    let tokens: Vec<String> = content_tokens(text);
    if tokens.is_empty() {
        return None;
    }

    // Each token contributes its EXACT form plus, where one can be derived
    // safely, a morphological PREFIX.
    //
    // The exact form is kept so a precise match still scores as a precise match;
    // a document containing the user's actual word matches both halves and BM25
    // ranks it above one that only matches the family prefix.
    //
    // Nothing about the INDEX changes here. The prefix is evaluated against the
    // porter stems already stored, which is why this needed no migration and no
    // rebuild — verified against the live index: `indemn*` matches documents
    // stemmed to `indemnif`, `indemnifi` and `indemn` alike.
    let mut clauses: Vec<String> = Vec::new();
    let mut synonym_clauses = 0usize;
    let push_synonym = |clauses: &mut Vec<String>, count: &mut usize, term: &str| {
        if *count >= MAX_SYNONYM_CLAUSES {
            return;
        }
        // Multi-word synonyms are emitted as FTS5 PHRASE queries, which match
        // only that exact sequence. That is what makes "act of god" safe to add
        // without its individual words matching everywhere.
        let clause = format!("\"{}\"", term);
        if !clauses.contains(&clause) {
            clauses.push(clause);
            *count += 1;
        }
    };

    for token in &tokens {
        clauses.push(format!("\"{}\"", token));
        if let Some(stem) = morphological_prefix(token) {
            clauses.push(format!("\"{}\"*", stem));
        } else if token.chars().count() >= MIN_BARE_PREFIX_CHARS {
            clauses.push(format!("\"{}\"*", token));
        }

        // Different words for the same idea: "lease" -> "tenancy".
        // Added AFTER the exact form and the family prefix, deliberately - a
        // document containing the user's own word matches more clauses and BM25
        // ranks it above one matching only a synonym, so recall grows without
        // displacing precision.
        for synonym in crate::utils::thesaurus::synonyms_for(token) {
            push_synonym(&mut clauses, &mut synonym_clauses, synonym);
        }
    }

    // Two-word legal terms ("force majeure", "due diligence", "hold harmless")
    // are the ones users type verbatim, and splitting them into single tokens
    // loses the term entirely. Adjacent pairs get their own lookup.
    for pair in tokens.windows(2) {
        for synonym in crate::utils::thesaurus::synonyms_for_pair(&pair[0], &pair[1]) {
            push_synonym(&mut clauses, &mut synonym_clauses, synonym);
        }
    }

    Some(clauses.join(" OR "))
}

/// `bm25()` weights for the documents index, in column order.
///
/// SQLite's `bm25()` returns a NEGATIVE score where more negative is a better
/// match, so callers order ASCENDING.
pub fn documents_bm25_weights() -> (f64, f64) {
    (FILENAME_WEIGHT, BODY_WEIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                original_filename TEXT NOT NULL,
                extracted_text TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                content TEXT NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    fn doc_hits(conn: &Connection, q: &str) -> Vec<String> {
        let query = fts_query_from_user_text(q).expect("query must build");
        let mut stmt = conn
            .prepare(
                "SELECT d.original_filename FROM documents_fts f
                 JOIN documents d ON d.id = f.rowid
                 WHERE documents_fts MATCH ?1 ORDER BY bm25(documents_fts, 8.0, 1.0)",
            )
            .unwrap();
        stmt.query_map([query], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    /// The end-to-end case this feature exists for: the user's word appears
    /// NOWHERE in the document, and the document is still found.
    ///
    /// Driven through the real FTS5 index rather than by inspecting the query
    /// string, because a query that looks right and matches nothing would pass
    /// a string assertion.
    /// The measured defect from a real session: a question ABOUT the
    /// conversation contains no term describing what the conversation was
    /// about, so searching for its words retrieves noise.
    #[test]
    fn a_question_about_the_conversation_itself_has_nothing_to_search_for() {
        for q in [
            "what else did we discuss in our previous conversation?",
            "Do you remember our previous conversation?",
            "can you recap what we talked about earlier",
        ] {
            assert!(
                content_tokens(q).is_empty(),
                "{:?} still yielded search terms {:?} - these describe the act of                  conversing, not a subject, and searching for them returns noise",
                q,
                content_tokens(q)
            );
            assert!(
                fts_query_from_user_text(q).is_none(),
                "{:?} must not build a search query",
                q
            );
        }
    }

    /// The same sentence shape WITH a subject in it must still search - the
    /// filter must remove filler, not questions.
    #[test]
    fn a_question_naming_a_subject_still_searches_for_that_subject() {
        let tokens = content_tokens("what did we discuss about the indemnity clause?");
        assert!(tokens.contains(&"indemnity".to_string()));
        assert!(tokens.contains(&"clause".to_string()));
        assert!(!tokens.contains(&"discuss".to_string()), "filler must be gone");
        assert!(!tokens.contains(&"what".to_string()));
        assert!(fts_query_from_user_text("what did we discuss about the indemnity clause?").is_some());
    }

    /// Legal nouns that are also common English must remain searchable. These
    /// are handled by NOT EXPANDING them, never by refusing to search them.
    #[test]
    fn legal_nouns_that_look_like_filler_are_still_searchable() {
        for term in ["will", "trust", "action", "party", "charge", "interest", "title", "service"] {
            assert!(
                !content_tokens(term).is_empty(),
                "{:?} is a legal noun and must stay searchable",
                term
            );
        }
    }

    #[test]
    fn a_document_using_a_different_word_for_the_same_idea_is_found() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('agreement.pdf', 'The tenancy shall continue for three years.')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('fleet.pdf', 'Each vehicle must be insured by the operator.')",
            [],
        )
        .unwrap();

        // "lease" appears nowhere in agreement.pdf; "car" nowhere in fleet.pdf.
        assert_eq!(doc_hits(&conn, "lease"), vec!["agreement.pdf"]);
        assert_eq!(doc_hits(&conn, "car"), vec!["fleet.pdf"]);
    }

    /// Recall must not come at the cost of ranking: a document containing the
    /// user's actual word has to outrank one that only matches a synonym.
    #[test]
    fn an_exact_match_still_outranks_a_synonym_match() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('synonym-only.pdf', 'The tenancy and the tenancy terms apply.')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('exact.pdf', 'The lease and the lease terms apply.')",
            [],
        )
        .unwrap();

        let hits = doc_hits(&conn, "lease");
        assert_eq!(
            hits.first().map(String::as_str),
            Some("exact.pdf"),
            "the document using the user's own word must rank first, got {:?}",
            hits
        );
        assert!(hits.contains(&"synonym-only.pdf".to_string()), "but the synonym match must still be found");
    }

    /// Two-word legal terms are what users type verbatim, and tokenising them
    /// apart would lose the term.
    #[test]
    fn a_two_word_legal_term_matches_its_equivalent() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('supply.pdf', 'Neither party is liable for an act of god.')",
            [],
        )
        .unwrap();
        assert_eq!(doc_hits(&conn, "force majeure"), vec!["supply.pdf"]);
    }

    /// The precision guard, end to end. "consideration" is on the do-not-expand
    /// list, so a query about contractual consideration must not drag in
    /// documents about thinking something over.
    #[test]
    fn an_ambiguous_word_does_not_pull_in_its_everyday_meaning() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('contract.pdf', 'The consideration for this agreement is ten pounds.')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('memo.pdf', 'After careful thought and reflection we decided to proceed.')",
            [],
        )
        .unwrap();

        assert_eq!(
            doc_hits(&conn, "consideration"),
            vec!["contract.pdf"],
            "an ambiguous term must match only its own word"
        );
    }

    /// Bounded expansion. Without a cap, a question made entirely of expandable
    /// legal terms would OR together enough clauses to match the whole library.
    #[test]
    fn synonym_expansion_is_capped() {
        let query = fts_query_from_user_text(
            "lease contract termination breach indemnity warranty payment employee              shareholder trademark claimant arbitration",
        )
        .expect("query must build");
        let clauses = query.matches(" OR ").count() + 1;
        // 12 tokens can each contribute an exact clause and a prefix clause,
        // plus at most MAX_SYNONYM_CLAUSES synonyms overall.
        let ceiling = MAX_QUERY_TOKENS * 2 + MAX_SYNONYM_CLAUSES;
        assert!(
            clauses <= ceiling,
            "query built {} clauses, above the {} ceiling",
            clauses,
            ceiling
        );
    }

    #[test]
    fn ensure_is_idempotent_and_indexes_new_rows_via_triggers() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        // Running again must not error - it happens on every startup.
        ensure_fts_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('lease.pdf', 'The tenant shall not assign this lease.')",
            [],
        )
        .unwrap();
        assert_eq!(doc_hits(&conn, "assign"), vec!["lease.pdf"]);
    }

    /// Rows written BEFORE the index existed must be found. This is the upgrade
    /// path for every existing installation, where the whole library predates
    /// the index and no trigger ever fired for it.
    #[test]
    fn existing_rows_are_backfilled_when_the_index_is_created_later() {
        let conn = db();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('old-contract.pdf', 'Confidentiality survives termination.')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (content) VALUES ('we agreed the deposit is refundable')",
            [],
        )
        .unwrap();

        ensure_fts_schema(&conn).unwrap();

        assert_eq!(
            doc_hits(&conn, "confidentiality"),
            vec!["old-contract.pdf"],
            "a document predating the index must be searchable"
        );
        let msg_hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                [fts_query_from_user_text("refundable deposit").unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(msg_hits, 1, "a message predating the index must be searchable");
    }

    /// An index that exists but has lost its content is repaired by the
    /// explicit rebuild entry point.
    ///
    /// `ensure_fts_schema` deliberately does NOT detect this case - see its doc
    /// comment for the measurements showing there is no cheap signal to detect
    /// it with. This test pins the repair path that exists instead.
    #[test]
    fn explicit_rebuild_repairs_an_index_that_lost_its_content() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('nda.pdf', 'The receiving party shall protect trade secrets.')",
            [],
        )
        .unwrap();
        assert_eq!(doc_hits(&conn, "secrets"), vec!["nda.pdf"]);

        // Simulate index loss.
        conn.execute("DELETE FROM documents_fts", []).unwrap();
        assert!(doc_hits(&conn, "secrets").is_empty(), "precondition: index emptied");

        rebuild_fts_indexes(&conn).unwrap();
        assert_eq!(
            doc_hits(&conn, "secrets"),
            vec!["nda.pdf"],
            "an explicit rebuild must restore the index"
        );
    }

    /// Pins the two traps that made the first version of `ensure_fts_schema`
    /// silently skip its backfill, so nobody reintroduces an "is the index
    /// empty?" check built on either of them.
    ///
    /// Measured behaviour of an EXTERNAL-CONTENT FTS5 table:
    ///   * `count(*)` on the virtual table reports the CONTENT table's rows,
    ///     so it reads non-zero even when the index holds nothing.
    ///   * the `_data` shadow table's row count only ever grows - emptying the
    ///     index makes it LARGER, not smaller.
    #[test]
    fn count_star_on_an_external_content_index_does_not_measure_the_index() {
        let conn = db();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('a.pdf', 'unique-token-here')",
            [],
        )
        .unwrap();
        conn.execute_batch(documents_ddl()).unwrap();

        // Index created but never rebuilt: nothing matches...
        let matched: i64 = conn
            .query_row(
                "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH 'unique'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(matched, 0, "precondition: the index holds nothing yet");

        // ...yet count(*) reports a row, because it is reading `documents`.
        let counted: i64 = conn
            .query_row("SELECT count(*) FROM documents_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            counted, 1,
            "count(*) reflects the content table, so it can never detect an empty index"
        );

        let data_before: i64 = conn
            .query_row("SELECT count(*) FROM documents_fts_data", [], |r| r.get(0))
            .unwrap();
        rebuild_index(&conn, "documents_fts", "documents").unwrap();
        conn.execute("DELETE FROM documents_fts", []).unwrap();
        let data_after_emptying: i64 = conn
            .query_row("SELECT count(*) FROM documents_fts_data", [], |r| r.get(0))
            .unwrap();
        assert!(
            data_after_emptying > data_before,
            "the shadow table grew ({} -> {}) while the index was EMPTIED, so its size \
             cannot detect emptiness either",
            data_before, data_after_emptying
        );
    }

    #[test]
    fn updates_and_deletes_keep_the_index_truthful() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('draft.pdf', 'Arbitration shall be held in Delaware.')",
            [],
        )
        .unwrap();
        assert_eq!(doc_hits(&conn, "arbitration"), vec!["draft.pdf"]);

        conn.execute(
            "UPDATE documents SET extracted_text = 'Mediation shall be held in New York.'
             WHERE original_filename = 'draft.pdf'",
            [],
        )
        .unwrap();
        assert!(
            doc_hits(&conn, "arbitration").is_empty(),
            "stale text must stop matching after an update - otherwise a repaired \
             extraction leaves the old content searchable forever"
        );
        assert_eq!(doc_hits(&conn, "mediation"), vec!["draft.pdf"]);

        conn.execute("DELETE FROM documents", []).unwrap();
        assert!(
            doc_hits(&conn, "mediation").is_empty(),
            "a deleted document must not remain searchable"
        );
    }

    /// The stemming the product depends on, pinned against the tokenizer
    /// choice. If `porter` were ever dropped from the DDL these still-plausible
    /// queries would silently stop matching.
    #[test]
    fn porter_stemming_matches_inflections() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('terms.pdf', 'Either party may terminate this agreement on notice.')",
            [],
        )
        .unwrap();
        for q in ["termination", "terminated", "terminates", "terminating"] {
            assert_eq!(
                doc_hits(&conn, q),
                vec!["terms.pdf"],
                "'{}' must match 'terminate' via porter stemming",
                q
            );
        }
    }

    /// Derivational families must match each other - the ceiling porter alone
    /// could not clear.
    ///
    /// The index stores porter stems, and porter maps this family to THREE
    /// different tokens (measured via fts5vocab): indemnification -> "indemnif",
    /// indemnify -> "indemnifi", indemnity -> "indemn". Exact-term matching
    /// therefore never connected them. Query-side prefix expansion does, because
    /// all three stems begin with "indemn".
    #[test]
    fn derivational_families_match_each_other() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO documents (original_filename, extracted_text)
                VALUES ('a.pdf', 'Vendor indemnification obligations are unlimited.');
             INSERT INTO documents (original_filename, extracted_text)
                VALUES ('b.pdf', 'The party shall indemnify the client.');
             INSERT INTO documents (original_filename, extracted_text)
                VALUES ('c.pdf', 'An indemnity is provided under clause 9.');",
        )
        .unwrap();

        for query in ["indemnify", "indemnification", "indemnity", "indemnified"] {
            let hits = doc_hits(&conn, query);
            for expected in ["a.pdf", "b.pdf", "c.pdf"] {
                assert!(
                    hits.contains(&expected.to_string()),
                    "'{}' must find {} - got {:?}",
                    query, expected, hits
                );
            }
        }
    }

    /// The inflectional family porter already handled, kept so the new prefix
    /// logic cannot regress what already worked.
    #[test]
    fn derivational_expansion_does_not_break_inflectional_matching() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('terms.pdf', 'Either party may terminate this agreement on notice.')",
            [],
        )
        .unwrap();
        for q in ["termination", "terminated", "terminates", "terminating", "terminate"] {
            assert_eq!(doc_hits(&conn, q), vec!["terms.pdf"], "'{}' must still match", q);
        }
    }

    /// The guard on the expansion: a stem too short to be a word must NOT become
    /// a search prefix. "creation" minus "ation" is "cre", and `cre*` would drag
    /// in credit, crew, cream and credential.
    #[test]
    fn over_short_stems_are_not_expanded_into_noise() {
        assert_eq!(morphological_prefix("creation"), None, "'cre' is too short to search");
        assert_eq!(morphological_prefix("nation"), None);
        assert_eq!(morphological_prefix("indemnification").as_deref(), Some("indemn"));
        assert_eq!(morphological_prefix("indemnify").as_deref(), Some("indemn"));
        assert_eq!(morphological_prefix("indemnity").as_deref(), Some("indemn"));
        assert_eq!(morphological_prefix("termination").as_deref(), Some("termin"));
        assert_eq!(morphological_prefix("obligation").as_deref(), Some("oblig"));

        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO documents (original_filename, extracted_text)
                VALUES ('x.pdf', 'Creation of a new entity requires consent.');
             INSERT INTO documents (original_filename, extracted_text)
                VALUES ('y.pdf', 'The credit facility and the crew roster.');",
        )
        .unwrap();
        let hits = doc_hits(&conn, "creation");
        assert!(hits.contains(&"x.pdf".to_string()), "the real match must be found");
        assert!(
            !hits.contains(&"y.pdf".to_string()),
            "an over-short stem must not pull in unrelated words: {:?}",
            hits
        );
    }

    /// Filenames are indexed and weighted above body text, so naming a file
    /// finds that file.
    #[test]
    fn filename_matches_are_found_and_outrank_body_mentions() {
        let conn = db();
        ensure_fts_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO documents (original_filename, extracted_text)
             VALUES ('schedule-b.pdf', 'Deliverables and milestones.');
             INSERT INTO documents (original_filename, extracted_text)
             VALUES ('other.pdf', 'See schedule for the schedule of the schedule.');",
        )
        .unwrap();
        let hits = doc_hits(&conn, "schedule-b.pdf");
        assert_eq!(
            hits.first().map(String::as_str),
            Some("schedule-b.pdf"),
            "naming a file must rank that file first, got {:?}",
            hits
        );
    }

    /// FTS5 has its own query grammar, so raw user text is not a valid query.
    /// These inputs would previously either error or silently mean something
    /// other than what the user typed.
    #[test]
    fn user_text_with_fts_operators_is_neutralised() {
        const QUOTE: &str = "\"";
        const QUOTE_STAR: &str = "\"*";
        for raw in [
            "what about \"termination\" - specifically?",
            "NEAR AND OR NOT",
            "cost * 100%",
            "^anchor: value",
            "don't -exclude this",
        ] {
            let q = fts_query_from_user_text(raw);
            if let Some(ref built) = q {
                // A '*' is legitimate ONLY as the prefix operator this module
                // appends after a quoted term. Stripping those leaves text that
                // must contain no operator characters at all - anything still
                // there came from the user's input.
                let without_our_prefixes = built.replace(QUOTE_STAR, QUOTE);
                assert!(
                    !without_our_prefixes.contains('*')
                        && !without_our_prefixes.contains('^')
                        && !without_our_prefixes.contains(':')
                        && !without_our_prefixes.contains('-'),
                    "operators leaked from {:?} into {:?}",
                    raw, built
                );
                // Must actually be executable.
                let conn = db();
                ensure_fts_schema(&conn).unwrap();
                conn.query_row(
                    "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH ?1",
                    [built],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or_else(|e| panic!("built query {:?} is not valid FTS5: {}", built, e));
            }
        }
    }

    #[test]
    fn empty_and_noise_queries_return_none_rather_than_matching_everything() {
        assert!(fts_query_from_user_text("").is_none());
        assert!(fts_query_from_user_text("   ").is_none());
        assert!(fts_query_from_user_text("a of is").is_none(), "sub-3-char tokens only");
        assert!(fts_query_from_user_text("?! ...").is_none());
        assert!(fts_query_from_user_text("termination").is_some());
    }

    /// A pasted paragraph must not build an unbounded query.
    ///
    /// Counted by DISTINCT source tokens rather than OR separators: each token
    /// now contributes an exact clause and possibly a prefix clause, so the
    /// separator count is no longer a proxy for the token count.
    #[test]
    fn query_token_count_is_bounded() {
        let long = (0..100).map(|i| format!("word{}", i)).collect::<Vec<_>>().join(" ");
        let q = fts_query_from_user_text(&long).unwrap();
        let distinct_sources: std::collections::HashSet<String> = q
            .split(" OR ")
            .map(|c| c.trim_end_matches('*').trim_matches('"').to_string())
            .collect();
        assert!(
            distinct_sources.len() <= MAX_QUERY_TOKENS,
            "expected at most {} source tokens, got {} in: {}",
            MAX_QUERY_TOKENS,
            distinct_sources.len(),
            q
        );
        assert!(
            q.matches(" OR ").count() <= MAX_QUERY_TOKENS * 2,
            "at most two clauses per token: {}",
            q
        );
    }
}
