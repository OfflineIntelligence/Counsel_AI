//! Document versions: files that are revisions of one another (e.g.
//! `SPA_v3.pdf`, `SPA v4 (signed).pdf`) are recognised by name, both are kept
//! searchable, and every excerpt says which version it came from.

use std::collections::HashMap;

use lazy_static::lazy_static;
use regex::Regex;

use crate::memory_db::MemoryDatabase;

lazy_static! {
    static ref NOISE: Regex = Regex::new(
        r"(?ix)
        ^(?:
            v\d+(?:\.\d+)* | ver\d* | version\d* | rev\d* | revision\d* | r\d+ |
            final\d* | draft\d* | signed | executed | clean | redline[d]? | markup | marked |
            copy | updated | update | new | old | latest | current | amended | amendment\d* |
            \d{4}[-_.]?\d{2}[-_.]?\d{2} | \d{8} | \d{4} | \d{1,2}
        )$"
    ).unwrap();
}

/// Name shared by all versions of a document ("spa" for `SPA_v3_signed.pdf`).
pub fn family_key(filename: &str) -> Option<String> {
    let stem = filename.rsplit_once('.').map(|(s, _)| s).unwrap_or(filename).to_lowercase();
    let words: Vec<&str> = stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .filter(|w| !NOISE.is_match(w))
        .collect();
    if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    }
}

#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub document_id: i64,
    pub filename: String,
    pub uploaded: chrono::DateTime<chrono::Utc>,
    /// 1-based position by upload time within the family.
    pub position: usize,
    pub total: usize,
}

impl VersionInfo {
    pub fn is_latest(&self) -> bool {
        self.position == self.total
    }

    pub fn label(&self) -> String {
        format!(
            "version {} of {}, uploaded {}{}",
            self.position,
            self.total,
            self.uploaded.format("%Y-%m-%d"),
            if self.is_latest() { ", latest" } else { ", superseded by a later upload" }
        )
    }
}

/// Version families among `document_ids` (only families with 2+ members).
pub fn families(db: &MemoryDatabase, document_ids: &[i64]) -> HashMap<i64, VersionInfo> {
    let mut groups: HashMap<String, Vec<(i64, String, chrono::DateTime<chrono::Utc>)>> = HashMap::new();
    for id in document_ids {
        let Ok(doc) = db.documents.get_document_meta(*id) else { continue };
        if doc.extraction_status != "ok" {
            continue;
        }
        if let Some(key) = family_key(&doc.original_filename) {
            groups.entry(key).or_default().push((doc.id, doc.original_filename, doc.created_at));
        }
    }
    let mut out = HashMap::new();
    for (_, mut members) in groups {
        if members.len() < 2 {
            continue;
        }
        members.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(&b.0)));
        let total = members.len();
        for (i, (id, filename, uploaded)) in members.into_iter().enumerate() {
            out.insert(id, VersionInfo { document_id: id, filename, uploaded, position: i + 1, total });
        }
    }
    out
}

/// Other versions of `document_id` within `candidates`.
pub fn siblings(db: &MemoryDatabase, document_id: i64, candidates: &[i64]) -> Vec<i64> {
    let Ok(doc) = db.documents.get_document_meta(document_id) else { return Vec::new() };
    let Some(key) = family_key(&doc.original_filename) else { return Vec::new() };
    candidates
        .iter()
        .copied()
        .filter(|id| *id != document_id)
        .filter(|id| {
            db.documents
                .get_document_meta(*id)
                .map(|d| d.extraction_status == "ok" && family_key(&d.original_filename).as_deref() == Some(key.as_str()))
                .unwrap_or(false)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_share_a_family() {
        let a = family_key("SPA_v3.pdf");
        assert_eq!(a.as_deref(), Some("spa"));
        assert_eq!(family_key("SPA v4 (signed).pdf"), a);
        assert_eq!(family_key("spa-final-2026-03-02.docx"), a);
        assert_eq!(family_key("Master Lease Agreement - Draft 2.docx"), family_key("Master_Lease_Agreement_FINAL.pdf"));
        assert_ne!(family_key("Master Lease Agreement.pdf"), family_key("Sublease Agreement.pdf"));
        assert_eq!(family_key("v2.pdf"), None);
    }
}
