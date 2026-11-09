//! Compact vector storage and search.
//!
//! Embeddings are truncated to `DIMS` (Matryoshka), renormalised and stored
//! as int8 with one f32 scale (516 bytes). Search keeps only 512-bit sign
//! signatures in RAM, shortlists by Hamming distance inside the allowed
//! document scope, then rescores the shortlist exactly from SQLite.

use std::collections::{BinaryHeap, HashSet};
use std::cmp::Reverse;

pub const DIMS: usize = 512;
const WORDS: usize = DIMS / 64;
const SHORTLIST: usize = 300;

/// Truncate to DIMS and L2-normalise.
pub fn prepare(v: &[f32]) -> Vec<f32> {
    let mut out: Vec<f32> = v.iter().take(DIMS).copied().collect();
    out.resize(DIMS, 0.0);
    let norm = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut out {
            *x /= norm;
        }
    }
    out
}

/// f32 LE scale followed by DIMS int8 values.
pub fn quantize(v: &[f32]) -> Vec<u8> {
    let p = prepare(v);
    let max = p.iter().fold(0f32, |m, x| m.max(x.abs()));
    let scale = if max > 0.0 { max / 127.0 } else { 1.0 };
    let mut out = Vec::with_capacity(4 + DIMS);
    out.extend_from_slice(&scale.to_le_bytes());
    for x in &p {
        out.push((x / scale).round().clamp(-127.0, 127.0) as i8 as u8);
    }
    out
}

pub fn signature_of_blob(blob: &[u8]) -> Option<[u64; WORDS]> {
    if blob.len() != 4 + DIMS {
        return None;
    }
    let mut sig = [0u64; WORDS];
    for (i, b) in blob[4..].iter().enumerate() {
        if (*b as i8) > 0 {
            sig[i / 64] |= 1 << (i % 64);
        }
    }
    Some(sig)
}

fn signature_of(v: &[f32]) -> [u64; WORDS] {
    let mut sig = [0u64; WORDS];
    for (i, x) in v.iter().take(DIMS).enumerate() {
        if *x > 0.0 {
            sig[i / 64] |= 1 << (i % 64);
        }
    }
    sig
}

/// Cosine similarity between a prepared query and a stored blob.
pub fn score_blob(query: &[f32], blob: &[u8]) -> Option<f32> {
    if blob.len() != 4 + DIMS || query.len() != DIMS {
        return None;
    }
    let scale = f32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]);
    let mut dot = 0f32;
    for (q, b) in query.iter().zip(&blob[4..]) {
        dot += q * (*b as i8) as f32;
    }
    Some(dot * scale)
}

#[derive(Default)]
pub struct VectorIndex {
    ids: Vec<i64>,
    docs: Vec<i64>,
    sigs: Vec<[u64; WORDS]>,
    present: HashSet<i64>,
    pub model_key: String,
    pub loaded: bool,
}

impl VectorIndex {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn clear(&mut self) {
        *self = VectorIndex { model_key: std::mem::take(&mut self.model_key), ..Default::default() };
    }

    pub fn insert_blob(&mut self, chunk_id: i64, document_id: i64, blob: &[u8]) {
        if !self.present.insert(chunk_id) {
            return;
        }
        if let Some(sig) = signature_of_blob(blob) {
            self.ids.push(chunk_id);
            self.docs.push(document_id);
            self.sigs.push(sig);
        } else {
            self.present.remove(&chunk_id);
        }
    }

    /// Candidate chunk ids closest to `query` (prepared) within `scope`.
    pub fn shortlist(&self, query: &[f32], scope: Option<&HashSet<i64>>) -> Vec<i64> {
        let q = signature_of(query);
        let mut heap: BinaryHeap<(u32, i64)> = BinaryHeap::with_capacity(SHORTLIST + 1);
        for i in 0..self.ids.len() {
            if let Some(s) = scope {
                if !s.contains(&self.docs[i]) {
                    continue;
                }
            }
            let sig = &self.sigs[i];
            let mut d = 0u32;
            for w in 0..WORDS {
                d += (sig[w] ^ q[w]).count_ones();
            }
            if heap.len() < SHORTLIST {
                heap.push((d, self.ids[i]));
            } else if let Some(&(worst, _)) = heap.peek() {
                if d < worst {
                    heap.pop();
                    heap.push((d, self.ids[i]));
                }
            }
        }
        let mut v: Vec<(u32, i64)> = heap.into_vec();
        v.sort_by_key(|(d, id)| (*d, Reverse(*id)));
        v.into_iter().map(|(_, id)| id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(seed: u64) -> Vec<f32> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..1024)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ((x % 2000) as f32 / 1000.0) - 1.0
            })
            .collect()
    }

    #[test]
    fn quantized_score_tracks_float_cosine() {
        let a = unit(1);
        let b = unit(2);
        let pa = prepare(&a);
        let pb = prepare(&b);
        let exact: f32 = pa.iter().zip(&pb).map(|(x, y)| x * y).sum();
        let approx = score_blob(&pa, &quantize(&b)).unwrap();
        assert!((exact - approx).abs() < 0.02, "{} vs {}", exact, approx);
        let self_score = score_blob(&pa, &quantize(&a)).unwrap();
        assert!(self_score > 0.99);
        assert_eq!(quantize(&a).len(), 4 + DIMS);
    }

    #[test]
    fn shortlist_respects_scope_and_finds_the_nearest() {
        let mut idx = VectorIndex::default();
        for i in 0..1000 {
            idx.insert_blob(i, i % 10, &quantize(&unit(i as u64 + 100)));
        }
        let target = unit(100 + 423);
        let q = prepare(&target);
        let all = idx.shortlist(&q, None);
        assert_eq!(all[0], 423);
        let scope: HashSet<i64> = [1, 2].into_iter().collect();
        let scoped = idx.shortlist(&q, Some(&scope));
        assert!(!scoped.contains(&423), "doc 3 is out of scope");
        assert!(scoped.iter().all(|id| id % 10 == 1 || id % 10 == 2));
    }

    #[test]
    fn duplicate_inserts_are_ignored() {
        let mut idx = VectorIndex::default();
        let blob = quantize(&unit(9));
        idx.insert_blob(1, 1, &blob);
        idx.insert_blob(1, 1, &blob);
        assert_eq!(idx.len(), 1);
    }
}
