//! Prefix-addressed KV store on disk.
//!
//! The engine holds the KV of one context window in RAM or VRAM and loses it
//! when it stops or moves to another conversation. This store keeps the
//! engine's KV states on disk instead, so memory of every conversation lives
//! on storage and only the active window occupies RAM or VRAM:
//!
//! - After a turn, the engine's slot holds the KV of exactly the messages
//!   that were sent plus the reply. That state is named by a key chained over
//!   those messages ([`prefix_keys`]).
//! - Before a request, the longest prefix of the new prompt that has a saved
//!   state is restored from disk ([`prepare`]), so the engine computes only
//!   the messages after it instead of the whole prompt.
//! - The state in the slot is written to disk only when it would otherwise
//!   be lost: before another conversation's state replaces it, before the
//!   model is unloaded when idle, and at shutdown.
//! - Disk use is bounded: a per-conversation cap and a total byte budget,
//!   least recently used first, and entries of deleted conversations go first.
//!
//! Correctness never depends on this bookkeeping. The engine compares the
//! tokens it holds with the prompt it receives and recomputes from the first
//! difference, so a stale or wrong state costs time, never a wrong answer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use lazy_static::lazy_static;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::{debug, info, warn};

use crate::memory::Message;
use crate::rag::store::KvStoreEntry;
use crate::shared_state::SharedSystemState;

lazy_static! {
    /// Serializes everything that uses the engine's single slot: restoring,
    /// generating, and saving. Held from `prepare` until the reply finishes.
    static ref GATE: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    /// What the engine's slot holds now, as far as this process knows.
    static ref ACTIVE: StdMutex<Option<Active>> = StdMutex::new(None);
}

/// Model, engine build and context size a KV state is valid for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub model_id: String,
    pub engine_id: String,
    pub ctx_size: u32,
}

/// The state in the engine's slot.
#[derive(Debug, Clone, PartialEq)]
pub struct Active {
    pub session_id: String,
    /// Prefix keys of the messages the slot holds, one per message.
    pub keys: Vec<String>,
    /// Prompt chars up to and including each message (same length as keys
    /// when known), to estimate the tokens of any prefix.
    pub chars: Vec<u64>,
    pub identity: Identity,
    /// Not yet written to disk.
    pub dirty: bool,
}

/// What `prepare` will do for one request.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Write the slot's current state to disk before it is replaced.
    pub save_current: bool,
    /// Restore this saved state into the slot.
    pub restore: Option<KvStoreEntry>,
    /// Messages of the new prompt the engine already holds (no restore needed).
    pub engine_has: usize,
}

/// Holds the slot for one request; dropped when the reply has finished.
pub struct KvGuard {
    _guard: OwnedMutexGuard<()>,
}

pub fn enabled() -> bool {
    crate::tuning::flag_or("KV_STORE", true)
}

fn budget_bytes() -> u64 {
    let configured = crate::tuning::parse_or("KV_STORE_MAX_MB", 20_480u64);
    // The resource plan bounds it by the free storage where the states live.
    let planned = super::machine::current().map(|p| p.kv_store_mb).unwrap_or(configured);
    configured.min(planned) * 1024 * 1024
}

fn per_session() -> usize {
    crate::tuning::parse_or("KV_STORE_PER_SESSION", 2usize).max(1)
}

/// One key per message: key k names messages[..=k] exactly (role and
/// trimmed text, chained), so two prompts share key k only if their first
/// k+1 messages are identical.
pub fn prefix_keys(messages: &[Message]) -> Vec<String> {
    let mut out = Vec::with_capacity(messages.len());
    let mut chain = blake3::Hasher::new();
    chain.update(b"kv-store-v1");
    for m in messages {
        chain.update(m.role.as_bytes());
        chain.update(&[0x1f]);
        chain.update(m.content.trim().as_bytes());
        chain.update(&[0x1e]);
        out.push(chain.clone().finalize().to_hex().to_string());
    }
    out
}

/// Cumulative prompt chars per message, matching `prefix_keys`.
pub fn prefix_chars(messages: &[Message]) -> Vec<u64> {
    let mut total = 0u64;
    messages
        .iter()
        .map(|m| {
            total += m.content.trim().len() as u64 + m.role.len() as u64;
            total
        })
        .collect()
}

lazy_static! {
    /// Measured engine prompt-processing speed (tokens per second) and disk
    /// restore speed (bytes per millisecond), moving averages. Seeded with
    /// conservative values until the first measurement.
    static ref RATES: StdMutex<(f64, f64)> = StdMutex::new((400.0, 400_000.0));
}

/// The engine reported computing `tokens` prompt tokens in `ms`.
pub fn observe_prefill(tokens: u64, ms: f64) {
    if tokens < 32 || ms <= 0.0 {
        return;
    }
    let tps = tokens as f64 * 1000.0 / ms;
    if let Ok(mut r) = RATES.lock() {
        r.0 = r.0 * 0.7 + tps * 0.3;
    }
}

fn observe_restore(bytes: u64, ms: u128) {
    if bytes == 0 || ms == 0 {
        return;
    }
    let rate = bytes as f64 / ms as f64;
    if let Ok(mut r) = RATES.lock() {
        r.1 = r.1 * 0.7 + rate * 0.3;
    }
}

/// Restoring is worth it when reading `bytes` from disk is faster than the
/// engine computing `tokens` itself (KV_RESTORE_MIN_TOKENS sets a floor).
pub fn restore_pays(bytes: u64, tokens: u64, prefill_tps: f64, restore_bytes_per_ms: f64) -> bool {
    let min = crate::tuning::parse_or("KV_RESTORE_MIN_TOKENS", 64u64);
    if tokens < min {
        return false;
    }
    let restore_ms = bytes as f64 / restore_bytes_per_ms.max(1.0);
    let compute_ms = tokens as f64 * 1000.0 / prefill_tps.max(1.0);
    restore_ms < compute_ms
}

fn common_prefix(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Decide what to do before sending `keys` (the prompt minus its newest
/// message): restore the best saved state when it covers more of the prompt
/// than the slot already holds, and save the slot first whenever its state is
/// unsaved and about to be replaced.
pub fn plan(active: Option<&Active>, identity: &Identity, keys: &[String], best: Option<KvStoreEntry>) -> Plan {
    let same_engine = active.filter(|a| &a.identity == identity);
    let engine_has = same_engine.map(|a| common_prefix(&a.keys, keys)).unwrap_or(0);
    let restore = best.filter(|e| e.n_messages as usize > engine_has);
    // The slot's state survives only if the new prompt extends all of it.
    let replaced = match active {
        Some(a) => &a.identity != identity || engine_has < a.keys.len() || restore.is_some(),
        None => false,
    };
    // Only the engine that computed a state can write it: under another
    // configuration the state is already gone (switches save it beforehand).
    let savable = same_engine.map(|a| a.dirty).unwrap_or(false);
    Plan {
        save_current: savable && replaced,
        restore,
        engine_has,
    }
}

/// Entries to delete: those of deleted conversations, those past the
/// per-conversation cap (most recent kept), then the least recently used
/// until the total fits `budget`. `entries` is most recently used first.
pub fn eviction(entries: &[KvStoreEntry], live_sessions: &HashSet<String>, budget: u64, per_session: usize) -> Vec<String> {
    let mut doomed: Vec<String> = Vec::new();
    let mut kept: Vec<&KvStoreEntry> = Vec::new();
    let mut per: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for e in entries {
        let n = per.entry(e.session_id.as_str()).or_insert(0);
        if !live_sessions.contains(&e.session_id) || *n >= per_session {
            doomed.push(e.key.clone());
        } else {
            *n += 1;
            kept.push(e);
        }
    }
    let mut total: u64 = kept.iter().map(|e| e.bytes).sum();
    for e in kept.iter().rev() {
        if total <= budget {
            break;
        }
        total = total.saturating_sub(e.bytes);
        doomed.push(e.key.clone());
    }
    doomed
}

fn file_for(key: &str) -> String {
    format!("kv_{}.bin", &key[..key.len().min(32)])
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn set_active(a: Option<Active>) {
    if let Ok(mut g) = ACTIVE.lock() {
        *g = a;
    }
}

fn active() -> Option<Active> {
    ACTIVE.lock().ok().and_then(|g| g.clone())
}

/// The engine process was started, stopped or replaced: its slot is empty.
pub fn engine_reset() {
    set_active(None);
}

/// Base URL, identity and save directory of the running engine, when it
/// can save and restore its slot (one slot and a --slot-save-path).
async fn engine(state: &SharedSystemState) -> Option<(String, Identity, PathBuf)> {
    let rt = state.runtime_manager.read().ok().and_then(|g| g.clone())?;
    let base = rt.get_base_url().await?;
    let config = rt.get_current_config().await?;
    let dir = config.slot_save_path.clone()?;
    let ctx_size = rt.get_effective_context().await.unwrap_or(config.context_size);
    Some((
        base,
        Identity { model_id: super::lifecycle::model_id_of(&config), engine_id: super::lifecycle::engine_id_of(state).await, ctx_size },
        dir,
    ))
}

/// Take the slot for one request.
pub async fn lock() -> KvGuard {
    KvGuard { _guard: GATE.clone().lock_owned().await }
}

/// Before sending `messages` for `session_id`: save the slot if its state
/// would be lost, then restore the longest saved prefix of the prompt.
pub async fn prepare(_slot: &KvGuard, state: &SharedSystemState, session_id: &str, messages: &[Message]) {
    if !enabled() || messages.is_empty() {
        return;
    }
    let Some((base, identity, dir)) = engine(state).await else { return };
    let db = &state.database_pool;
    let history = &messages[..messages.len() - 1];
    let keys = prefix_keys(history);
    let chars = prefix_chars(history);
    let found = db.rag.kv_best_prefix(&keys, &identity.model_id, &identity.engine_id, identity.ctx_size).unwrap_or(None);
    // Restore only when reading the state is faster than computing it.
    let (tps, bpm) = RATES.lock().map(|r| *r).unwrap_or((400.0, 400_000.0));
    let best = match found {
        Some((entry, tokens)) if restore_pays(entry.bytes, tokens, tps, bpm) => Some(entry),
        Some((entry, tokens)) => {
            debug!(
                "KV store: skipping restore of {} ({} bytes for ~{} tokens): computing is faster",
                entry.file, entry.bytes, tokens
            );
            None
        }
        None => None,
    };
    let current = active();
    let p = plan(current.as_ref(), &identity, &keys, best);
    if p.save_current {
        if let Some(a) = current.as_ref() {
            save(state, &base, &dir, a).await;
        }
    }
    match p.restore {
        Some(entry) => {
            if !dir.join(&entry.file).exists() {
                let _ = db.rag.kv_delete(&entry.key);
                set_active(None);
                return;
            }
            let started = std::time::Instant::now();
            match super::slot_cache::engine_slot_restore(&base, &entry.file).await {
                Ok((tokens, bytes)) => {
                    observe_restore(bytes, started.elapsed().as_millis());
                    let _ = db.rag.kv_touch(&entry.key, now());
                    info!(
                        "KV store: restored {} tokens ({} bytes) of {} messages for {} in {} ms",
                        tokens, bytes, entry.n_messages, session_id, started.elapsed().as_millis()
                    );
                    set_active(Some(Active {
                        session_id: session_id.to_string(),
                        keys: keys[..entry.n_messages as usize].to_vec(),
                        chars: chars[..entry.n_messages as usize].to_vec(),
                        identity,
                        dirty: false,
                    }));
                }
                Err(e) => {
                    warn!("KV store: restore of {} failed ({}); dropping it", entry.file, e);
                    let _ = db.rag.kv_delete(&entry.key);
                    let _ = std::fs::remove_file(dir.join(&entry.file));
                    set_active(None);
                }
            }
        }
        None => {
            if p.engine_has == 0 {
                // The engine computes this prompt from scratch; whatever it held is gone.
                set_active(None);
            }
            debug!("KV store: engine already holds {} of {} messages for {}", p.engine_has, keys.len(), session_id);
        }
    }
}

/// After a reply: the slot now holds `sent` plus the reply.
pub async fn record_served(_slot: &KvGuard, state: &SharedSystemState, session_id: &str, sent: &[Message], reply: &str) {
    if !enabled() {
        return;
    }
    let Some((_, identity, _)) = engine(state).await else { return };
    let mut all = sent.to_vec();
    all.push(Message { role: "assistant".to_string(), content: reply.to_string() });
    set_active(Some(Active {
        session_id: session_id.to_string(),
        keys: prefix_keys(&all),
        chars: prefix_chars(&all),
        identity,
        dirty: true,
    }));
}

/// Before a generation that is not part of any conversation (for example the
/// reading-notes step): keep the slot's state, since that call replaces it.
pub async fn checkpoint_and_release(_slot: &KvGuard, state: &SharedSystemState) {
    if let Some(a) = active() {
        if a.dirty {
            if let Some((base, _, dir)) = engine(state).await {
                save(state, &base, &dir, &a).await;
            }
        }
    }
    set_active(None);
}

/// Write the slot's state to disk if unsaved (idle unload, shutdown).
pub async fn save_active(state: &SharedSystemState) {
    let _slot = lock().await;
    if let Some(a) = active().filter(|a| a.dirty) {
        if let Some((base, identity, dir)) = engine(state).await {
            if identity == a.identity {
                save(state, &base, &dir, &a).await;
            }
        }
    }
}

async fn save(state: &SharedSystemState, base: &str, dir: &Path, a: &Active) {
    let Some(key) = a.keys.last() else { return };
    let file = file_for(key);
    let started = std::time::Instant::now();
    match super::slot_cache::engine_slot_save(base, &file).await {
        Ok((tokens, bytes)) => {
            let entry = KvStoreEntry {
                key: key.clone(),
                session_id: a.session_id.clone(),
                n_messages: a.keys.len() as i64,
                model_id: a.identity.model_id.clone(),
                engine_id: a.identity.engine_id.clone(),
                ctx_size: a.identity.ctx_size,
                n_tokens: tokens,
                bytes,
                file,
                last_used: now(),
            };
            if let Err(e) = state.database_pool.rag.kv_put(&entry, &a.keys.iter().cloned().zip(a.chars.iter().copied().chain(std::iter::repeat(0))).collect::<Vec<_>>()) {
                warn!("KV store: saved {} but could not index it ({}); removing it", entry.file, e);
                let _ = std::fs::remove_file(dir.join(&entry.file));
                return;
            }
            info!(
                "KV store: saved {} tokens ({} bytes) of {} messages for {} in {} ms",
                tokens, bytes, entry.n_messages, a.session_id, started.elapsed().as_millis()
            );
            set_active(Some(Active { dirty: false, ..a.clone() }));
            evict(state, dir);
        }
        Err(e) => warn!("KV store: save failed ({}); this state will be recomputed when needed", e),
    }
}

fn evict(state: &SharedSystemState, dir: &Path) {
    let db = &state.database_pool;
    let entries = db.rag.kv_all().unwrap_or_default();
    let live = db.rag.kv_live_sessions().unwrap_or_default();
    for key in eviction(&entries, &live, budget_bytes(), per_session()) {
        if let Some(e) = entries.iter().find(|e| e.key == key) {
            let _ = std::fs::remove_file(dir.join(&e.file));
        }
        let _ = db.rag.kv_delete(&key);
    }
}

#[cfg(test)]
mod cost_tests {
    use super::*;

    #[test]
    fn a_restore_happens_only_when_reading_beats_computing() {
        // 1 GB/s disk, 500 tok/s prefill: 100 MB costs 100 ms, 4,000 tokens 8 s.
        assert!(restore_pays(100 * 1024 * 1024, 4_000, 500.0, 1_000_000.0));
        // A shared system prompt of 300 tokens inside a 1.5 GB state: 1.5 s
        // of reading against 0.6 s of computing - compute it.
        assert!(!restore_pays(1_500 * 1024 * 1024, 300, 500.0, 1_000_000.0));
        // Below the floor nothing is restored.
        assert!(!restore_pays(1, 10, 1.0, 1e9));
    }

    #[test]
    fn prefix_chars_accumulate_per_message() {
        let msgs = vec![
            Message { role: "system".into(), content: "abc".into() },
            Message { role: "user".into(), content: " de ".into() },
        ];
        assert_eq!(prefix_chars(&msgs), vec![9, 15]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(role: &str, c: &str) -> Message {
        Message { role: role.to_string(), content: c.to_string() }
    }

    fn id() -> Identity {
        Identity { model_id: "llama-3b".into(), engine_id: "b8037".into(), ctx_size: 8192 }
    }

    fn entry(key: &str, session: &str, n: i64, bytes: u64, last_used: i64) -> KvStoreEntry {
        KvStoreEntry {
            key: key.into(),
            session_id: session.into(),
            n_messages: n,
            model_id: "llama-3b".into(),
            engine_id: "b8037".into(),
            ctx_size: 8192,
            n_tokens: 100,
            bytes,
            file: file_for(&format!("{:0>32}", key)),
            last_used,
        }
    }

    #[test]
    fn prefix_keys_name_exact_prefixes() {
        let a = prefix_keys(&[m("system", "s"), m("user", "q1"), m("assistant", "a1")]);
        let b = prefix_keys(&[m("system", "s"), m("user", "q1 "), m("assistant", "different")]);
        assert_eq!(a.len(), 3);
        assert_eq!(a[..2], b[..2], "trailing whitespace does not change a key");
        assert_ne!(a[2], b[2]);
        let c = prefix_keys(&[m("user", "q1")]);
        assert_ne!(a[1], c[0], "the same message at a different position is a different prefix");
        let d = prefix_keys(&[m("system", "s"), m("assistant", "q1")]);
        assert_ne!(a[1], d[1], "the role is part of the key");
    }

    #[test]
    fn a_continuing_conversation_needs_no_disk_io() {
        let sent = [m("system", "s"), m("user", "q1"), m("assistant", "a1")];
        let active = Active { session_id: "A".into(), keys: prefix_keys(&sent), chars: Vec::new(), identity: id(), dirty: true };
        let next = prefix_keys(&sent);
        let p = plan(Some(&active), &id(), &next, None);
        assert_eq!(p, Plan { save_current: false, restore: None, engine_has: 3 });
    }

    #[test]
    fn switching_conversations_saves_the_old_one_and_restores_the_new_one() {
        let a = Active { session_id: "A".into(), keys: prefix_keys(&[m("user", "a")]), chars: Vec::new(), identity: id(), dirty: true };
        let b_keys = prefix_keys(&[m("system", "s"), m("user", "b1"), m("assistant", "b2")]);
        let stored = entry(&b_keys[2], "B", 3, 10, 1);
        let p = plan(Some(&a), &id(), &b_keys, Some(stored.clone()));
        assert!(p.save_current, "A's unsaved state is kept before B replaces it");
        assert_eq!(p.restore, Some(stored));
    }

    #[test]
    fn a_saved_state_is_not_written_again() {
        let a = Active { session_id: "A".into(), keys: prefix_keys(&[m("user", "a")]), chars: Vec::new(), identity: id(), dirty: false };
        let p = plan(Some(&a), &id(), &prefix_keys(&[m("user", "zzz")]), None);
        assert!(!p.save_current);
        assert_eq!(p.engine_has, 0);
    }

    #[test]
    fn a_shorter_saved_prefix_than_the_slot_holds_is_not_restored() {
        let sent = prefix_keys(&[m("system", "s"), m("user", "q1"), m("assistant", "a1")]);
        let a = Active { session_id: "A".into(), keys: sent.clone(), chars: Vec::new(), identity: id(), dirty: false };
        let p = plan(Some(&a), &id(), &sent, Some(entry(&sent[0], "A", 1, 10, 1)));
        assert_eq!(p.restore, None);
    }

    #[test]
    fn a_different_model_or_context_never_counts_as_held() {
        let sent = prefix_keys(&[m("user", "q1")]);
        let a = Active { session_id: "A".into(), keys: sent.clone(), chars: Vec::new(), identity: id(), dirty: true };
        let other = Identity { ctx_size: 4096, ..id() };
        let p = plan(Some(&a), &other, &sent, None);
        assert_eq!(p.engine_has, 0);
        assert!(!p.save_current, "the engine that held the state is gone: saving now would store another engine's slot under it");
    }

    #[test]
    fn eviction_drops_deleted_chats_then_extra_states_then_the_least_recently_used() {
        let live: HashSet<String> = ["A".to_string(), "B".to_string()].into_iter().collect();
        // Most recently used first.
        let entries = vec![
            entry("a3", "A", 9, 40, 9),
            entry("b2", "B", 5, 40, 8),
            entry("a2", "A", 7, 40, 7),
            entry("gone", "Z", 3, 40, 6),
            entry("a1", "A", 5, 40, 5),
            entry("b1", "B", 3, 40, 4),
        ];
        let doomed = eviction(&entries, &live, 1_000, 2);
        assert!(doomed.contains(&"gone".to_string()), "deleted conversation");
        assert!(doomed.contains(&"a1".to_string()), "third state of A");
        assert_eq!(doomed.len(), 2);
        // A tight budget then removes the least recently used of what is left.
        let doomed = eviction(&entries, &live, 90, 2);
        assert!(doomed.contains(&"b1".to_string()) && doomed.contains(&"a2".to_string()));
        assert!(!doomed.contains(&"a3".to_string()) && !doomed.contains(&"b2".to_string()));
    }

    #[test]
    fn the_index_returns_the_longest_saved_prefix_for_this_configuration() {
        let db = crate::memory_db::MemoryDatabase::new_in_memory().unwrap();
        db.conversations.create_session_with_id("A", None).unwrap();
        let keys = prefix_keys(&[m("system", "s"), m("user", "q1"), m("assistant", "a1"), m("user", "q2")]);
        db.rag.kv_put(&entry(&keys[1], "A", 2, 10, 1), &[]).unwrap();
        db.rag.kv_put(&entry(&keys[3], "A", 4, 10, 2), &[]).unwrap();
        let other = KvStoreEntry { key: "x".into(), model_id: "qwen".into(), n_messages: 9, ..entry(&keys[2], "A", 3, 10, 3) };
        db.rag.kv_put(&KvStoreEntry { key: keys[2].clone(), ..other }, &[]).unwrap();

        let best = db.rag.kv_best(&keys, "llama-3b", "b8037", 8192).unwrap().unwrap();
        assert_eq!(best.key, keys[3], "longest prefix for this model wins");
        assert!(db.rag.kv_best(&keys, "llama-3b", "b8037", 4096).unwrap().is_none(), "context size must match");
        assert!(db.rag.kv_best(&keys[..1], "llama-3b", "b8037", 8192).unwrap().is_none());

        assert_eq!(db.rag.kv_live_sessions().unwrap(), ["A".to_string()].into_iter().collect());
        db.rag.kv_delete(&keys[3]).unwrap();
        assert_eq!(db.rag.kv_best(&keys, "llama-3b", "b8037", 8192).unwrap().unwrap().key, keys[1]);
        assert_eq!(db.rag.kv_all().unwrap().len(), 2);
    }

    #[test]
    fn state_files_stay_inside_the_save_directory() {
        let k = prefix_keys(&[m("user", "../../etc/passwd")]).pop().unwrap();
        let f = file_for(&k);
        assert!(f.starts_with("kv_") && f.ends_with(".bin"));
        assert!(f[3..f.len() - 4].chars().all(|c| c.is_ascii_hexdigit()));
    }
}
