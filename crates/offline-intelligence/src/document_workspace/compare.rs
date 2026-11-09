//! Document comparison — the redline.
//!
//! # Why paragraphs and not characters
//!
//! A character-level diff of two contracts produces thousands of
//! single-letter operations that no one can read. A paragraph-level diff
//! produces the unit lawyers already argue in — the clause — and only descends
//! to words inside a paragraph that actually changed.
//!
//! # The step that makes it useful
//!
//! Aligning paragraphs is the easy half. The half that decides whether the
//! output is worth reading is [`pair_replacements`]: when a delete sits
//! opposite an insert, is that one clause EDITED, or one clause removed and an
//! unrelated one added? Report it wrongly and changing a single word in a
//! 300-word clause renders as 300 words deleted and 300 added — technically
//! true, and exactly what nobody wants to read.
//!
//! # What it does not do
//!
//! Formatting is not compared: bold, spacing and styles are invisible here.
//! This answers "what does it now SAY", which is the question being asked when
//! a draft comes back from the other side. Comparing presentation would need a
//! different tool and a different UI.

use anyhow::{anyhow, Result};
use serde::Serialize;
use similar::{Algorithm, ChangeTag, TextDiff};

use crate::document_workspace::view_model::DraftViewModel;

/// Above this, a delete/insert pair is one paragraph EDITED rather than two
/// unrelated paragraphs.
///
/// 0.5 is deliberately permissive. The failure modes are asymmetric: calling a
/// real edit "unrelated" hides the very thing the reader is looking for, while
/// calling two unrelated paragraphs an edit shows both texts side by side with
/// every word marked — visibly odd, but nothing is concealed.
const CHANGE_THRESHOLD: f32 = 0.5;

/// One paragraph of a document, with the address that identifies it.
#[derive(Debug, Clone)]
struct Para {
    addr: String,
    text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum DiffOp {
    Equal {
        text: String,
    },
    Insert {
        text: String,
        addr: String,
    },
    Delete {
        text: String,
        addr: String,
    },
    Change {
        left: String,
        right: String,
        left_addr: String,
        right_addr: String,
        words: Vec<WordOp>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct WordOp {
    /// `equal` | `insert` | `delete`
    pub op: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DiffSummary {
    pub insertions: usize,
    pub deletions: usize,
    pub changes: usize,
    /// True when the two sides are word-for-word identical.
    pub identical: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffResult {
    pub ops: Vec<DiffOp>,
    pub summary: DiffSummary,
}

/// Flatten a view model into comparable paragraphs.
///
/// A PDF yields nothing here: its model carries geometry and annotations, not
/// text. That is reported as an error naming the format rather than returning
/// an empty document, which would render as "everything was deleted".
fn paragraphs(model: &DraftViewModel) -> Result<Vec<Para>> {
    let mut out = Vec::new();

    match model {
        DraftViewModel::Docx { blocks, .. } => {
            for b in blocks {
                out.push(Para {
                    addr: b.addr.clone(),
                    text: b.runs.iter().map(|r| r.text.as_str()).collect::<String>(),
                });
            }
        }
        DraftViewModel::Txt { content, .. } => {
            for (i, line) in content.lines().enumerate() {
                out.push(Para {
                    addr: format!("line:{}", i + 1),
                    text: line.to_string(),
                });
            }
        }
        DraftViewModel::Pptx { slides, .. } => {
            for slide in slides {
                for shape in &slide.shapes {
                    for p in &shape.paragraphs {
                        out.push(Para {
                            addr: p.addr.clone(),
                            text: p.text.clone(),
                        });
                    }
                }
            }
        }
        DraftViewModel::Xlsx { sheets, .. } => {
            // A row is the comparable unit in a spreadsheet: comparing cell by
            // cell would report an inserted row as every cell below it having
            // changed, which is how spreadsheet diffs usually become useless.
            for sheet in sheets {
                let mut cells: Vec<_> = sheet.cells.values().collect();
                cells.sort_by(|a, b| row_of(&a.addr).cmp(&row_of(&b.addr)).then(a.addr.cmp(&b.addr)));
                let mut current_row = 0usize;
                let mut buf: Vec<String> = Vec::new();
                for cell in cells {
                    let row = row_of(&cell.addr);
                    if row != current_row && !buf.is_empty() {
                        out.push(Para {
                            addr: format!("{}!row:{}", sheet.name, current_row),
                            text: buf.join("\t"),
                        });
                        buf.clear();
                    }
                    current_row = row;
                    buf.push(cell.value.clone());
                }
                if !buf.is_empty() {
                    out.push(Para {
                        addr: format!("{}!row:{}", sheet.name, current_row),
                        text: buf.join("\t"),
                    });
                }
            }
        }
        DraftViewModel::Pdf { .. } => {
            return Err(anyhow!(
                "A PDF has no text layer in the workspace, so it cannot be compared here. \
                 Compare the Word or text version instead."
            ))
        }
    }

    Ok(out)
}

/// Row number out of an A1-style reference. Non-conforming addresses sort to 0.
fn row_of(addr: &str) -> usize {
    // Cell addresses are `sheet[0]/B7`; only the reference after the slash
    // carries the row.
    let reference = addr.rsplit('/').next().unwrap_or(addr);
    reference
        .chars()
        .skip_while(|c| c.is_alphabetic())
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

/// Fold away differences that are not edits.
///
/// Word and most editors rewrite straight quotes to typographic ones and
/// normalise spacing on save. Without this, reopening a document in Word and
/// saving it would produce a redline claiming every paragraph changed.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for c in text.chars() {
        let c = match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201F}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            '\u{00A0}' => ' ',
            other => other,
        };
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.extend(c.to_lowercase());
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

/// Compare two documents.
pub fn compare(left: &DraftViewModel, right: &DraftViewModel) -> Result<DiffResult> {
    let left_paras = paragraphs(left)?;
    let right_paras = paragraphs(right)?;

    let left_keys: Vec<String> = left_paras.iter().map(|p| normalise(&p.text)).collect();
    let right_keys: Vec<String> = right_paras.iter().map(|p| normalise(&p.text)).collect();

    // Myers over paragraph keys. Patience would produce tidier output on code;
    // on prose, where near-duplicate boilerplate paragraphs are common, its
    // reliance on unique anchor lines makes it fall back constantly.
    // diff_slices wants &[&T], so the owned keys above are borrowed here. They
    // must outlive the diff, which is why they are not built inline.
    let left_refs: Vec<&str> = left_keys.iter().map(String::as_str).collect();
    let right_refs: Vec<&str> = right_keys.iter().map(String::as_str).collect();

    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_slices(&left_refs, &right_refs);

    let mut ops = Vec::new();
    let mut summary = DiffSummary::default();

    for op in diff.ops() {
        match *op {
            similar::DiffOp::Equal { old_index, len, .. } => {
                for p in &left_paras[old_index..old_index + len] {
                    ops.push(DiffOp::Equal { text: p.text.clone() });
                }
            }
            similar::DiffOp::Delete { old_index, old_len, .. } => {
                for p in &left_paras[old_index..old_index + old_len] {
                    summary.deletions += 1;
                    ops.push(DiffOp::Delete {
                        text: p.text.clone(),
                        addr: p.addr.clone(),
                    });
                }
            }
            similar::DiffOp::Insert { new_index, new_len, .. } => {
                for p in &right_paras[new_index..new_index + new_len] {
                    summary.insertions += 1;
                    ops.push(DiffOp::Insert {
                        text: p.text.clone(),
                        addr: p.addr.clone(),
                    });
                }
            }
            similar::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                pair_replacements(
                    &left_paras[old_index..old_index + old_len],
                    &right_paras[new_index..new_index + new_len],
                    &mut ops,
                    &mut summary,
                );
            }
        }
    }

    summary.identical = summary.insertions == 0 && summary.deletions == 0 && summary.changes == 0;
    Ok(DiffResult { ops, summary })
}

/// Decide, for a block of replaced paragraphs, which are edits and which are
/// genuine removals and additions.
///
/// Paragraphs are zipped positionally and scored. Anything left over when one
/// side is longer is a plain delete or insert — there is nothing to have
/// edited it into.
fn pair_replacements(
    old: &[Para],
    new: &[Para],
    ops: &mut Vec<DiffOp>,
    summary: &mut DiffSummary,
) {
    let paired = old.len().min(new.len());

    for i in 0..paired {
        let (l, r) = (&old[i], &new[i]);
        let ratio = TextDiff::from_words(&normalise(&l.text), &normalise(&r.text)).ratio();

        if ratio >= CHANGE_THRESHOLD {
            summary.changes += 1;
            ops.push(DiffOp::Change {
                left: l.text.clone(),
                right: r.text.clone(),
                left_addr: l.addr.clone(),
                right_addr: r.addr.clone(),
                words: word_ops(&l.text, &r.text),
            });
        } else {
            // Too different to be the same clause. Reported as what it is:
            // one thing gone, a different thing arrived.
            summary.deletions += 1;
            summary.insertions += 1;
            ops.push(DiffOp::Delete {
                text: l.text.clone(),
                addr: l.addr.clone(),
            });
            ops.push(DiffOp::Insert {
                text: r.text.clone(),
                addr: r.addr.clone(),
            });
        }
    }

    for p in &old[paired..] {
        summary.deletions += 1;
        ops.push(DiffOp::Delete {
            text: p.text.clone(),
            addr: p.addr.clone(),
        });
    }
    for p in &new[paired..] {
        summary.insertions += 1;
        ops.push(DiffOp::Insert {
            text: p.text.clone(),
            addr: p.addr.clone(),
        });
    }
}

/// Word-level operations inside one changed paragraph.
///
/// Run on the ORIGINAL text, not the normalised key: the reader has to see the
/// document's real words, capitals and punctuation.
fn word_ops(left: &str, right: &str) -> Vec<WordOp> {
    let diff = TextDiff::from_words(left, right);
    let mut out: Vec<WordOp> = Vec::new();

    for change in diff.iter_all_changes() {
        let op = match change.tag() {
            ChangeTag::Equal => "equal",
            ChangeTag::Delete => "delete",
            ChangeTag::Insert => "insert",
        };
        let text = change.value().to_string();

        // Coalesce runs: word diffing emits one change per word and per
        // separating space, so an untouched sentence would arrive as fifty
        // fragments for the client to render as fifty elements.
        match out.last_mut() {
            Some(last) if last.op == op => last.text.push_str(&text),
            _ => out.push(WordOp { op, text }),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_workspace::view_model::DraftViewModel;

    #[test]
    fn spreadsheet_rows_are_compared_as_rows_not_as_one_line() {
        use crate::document_workspace::ooxml::{package::fixtures, xlsx};
        let original = fixtures::xlsx();
        let edited = xlsx::apply(
            &original,
            &[crate::document_workspace::view_model::DraftPatch::SetCellValue {
                addr: "sheet[0]/A2".into(),
                value: "Negotiation".into(),
            }],
        )
        .unwrap();
        let left = xlsx::build(&original, 1).unwrap();
        let right = xlsx::build(&edited, 2).unwrap();
        let rows = paragraphs(&left).unwrap();
        assert!(rows.len() > 2, "each row is its own unit: {:?}", rows.iter().map(|p| &p.addr).collect::<Vec<_>>());
        let diff = compare(&left, &right).unwrap();
        assert_eq!(diff.summary.changes + diff.summary.insertions + diff.summary.deletions >= 1, true);
        assert!(diff.ops.iter().any(|op| matches!(op, DiffOp::Equal { .. })), "unchanged rows stay equal");
    }

    fn txt(content: &str) -> DraftViewModel {
        DraftViewModel::Txt {
            version: 1,
            content: content.to_string(),
            encoding: "UTF-8".into(),
            line_ending: "LF".into(),
            bom: false,
        }
    }

    #[test]
    fn an_unchanged_document_reports_itself_identical() {
        let a = txt("One.\nTwo.\nThree.");
        let d = compare(&a, &a).unwrap();
        assert!(d.summary.identical);
        assert_eq!(d.summary.insertions, 0);
        assert_eq!(d.summary.deletions, 0);
        assert!(d.ops.iter().all(|o| matches!(o, DiffOp::Equal { .. })));
    }

    #[test]
    fn editing_one_word_is_a_change_not_a_delete_and_an_insert() {
        // The whole point of the similarity step. Without it this renders as
        // the entire clause removed and an entirely new one added.
        let a = txt("The Receiving Party shall keep the Confidential Information secret.");
        let b = txt("The Receiving Party shall keep the Confidential Information confidential.");
        let d = compare(&a, &b).unwrap();

        assert_eq!(d.summary.changes, 1, "{:?}", d.summary);
        assert_eq!(d.summary.deletions, 0);
        assert_eq!(d.summary.insertions, 0);

        let words = match &d.ops[0] {
            DiffOp::Change { words, .. } => words,
            other => panic!("expected a change, got {other:?}"),
        };
        assert!(words.iter().any(|w| w.op == "delete" && w.text.contains("secret")));
        assert!(words.iter().any(|w| w.op == "insert" && w.text.contains("confidential")));
        assert!(words.iter().any(|w| w.op == "equal" && w.text.contains("Receiving Party")));
    }

    #[test]
    fn an_unrelated_replacement_is_reported_as_a_removal_and_an_addition() {
        let a = txt("Governing law is England and Wales.");
        let b = txt("Payment falls due within thirty days of invoice.");
        let d = compare(&a, &b).unwrap();

        assert_eq!(d.summary.changes, 0, "nothing here is an edit of the other");
        assert_eq!(d.summary.deletions, 1);
        assert_eq!(d.summary.insertions, 1);
    }

    #[test]
    fn a_resaved_document_with_smart_quotes_shows_no_changes() {
        // Word rewrites quotes and dashes on save. Without normalisation,
        // opening a document and saving it produces a redline claiming every
        // paragraph changed — which would make the feature untrustworthy on
        // exactly the documents it exists for.
        let a = txt("The \"Disclosing Party\" - as defined - shall not be liable.");
        let b = txt("The \u{201C}Disclosing Party\u{201D} \u{2014} as defined \u{2014} shall not be liable.");
        let d = compare(&a, &b).unwrap();
        assert!(d.summary.identical, "{:?}", d.summary);
    }

    #[test]
    fn whitespace_and_case_alone_do_not_count_as_a_change() {
        let a = txt("Clause 4.2   applies.");
        let b = txt("clause 4.2 applies.");
        assert!(compare(&a, &b).unwrap().summary.identical);
    }

    #[test]
    fn inserted_and_deleted_paragraphs_are_counted_and_addressed() {
        let a = txt("One.\nTwo.\nThree.");
        let b = txt("One.\nTwo.\nTwo and a half.\nThree.");
        let d = compare(&a, &b).unwrap();

        assert_eq!(d.summary.insertions, 1, "{:?}", d.summary);
        assert_eq!(d.summary.deletions, 0);
        let inserted = d.ops.iter().find_map(|o| match o {
            DiffOp::Insert { text, addr } => Some((text.clone(), addr.clone())),
            _ => None,
        });
        let (text, addr) = inserted.expect("an insert op");
        assert_eq!(text, "Two and a half.");
        assert!(addr.starts_with("line:"), "addresses let the UI locate it: {addr}");
    }

    #[test]
    fn equal_word_runs_are_coalesced_rather_than_emitted_per_word() {
        let a = txt("alpha beta gamma delta epsilon zeta one");
        let b = txt("alpha beta gamma delta epsilon zeta two");
        let d = compare(&a, &b).unwrap();

        let words = match &d.ops[0] {
            DiffOp::Change { words, .. } => words,
            other => panic!("expected a change, got {other:?}"),
        };
        // Six untouched words must not arrive as eleven fragments.
        let equal_runs = words.iter().filter(|w| w.op == "equal").count();
        assert_eq!(equal_runs, 1, "adjacent equal words coalesce: {words:?}");
    }

    #[test]
    fn comparing_a_pdf_says_why_rather_than_reporting_everything_deleted() {
        let pdf = DraftViewModel::Pdf { version: 1, pages: vec![], bookmarks: vec![] };
        let err = compare(&pdf, &txt("anything")).unwrap_err().to_string();
        assert!(err.contains("PDF"), "{err}");
        assert!(err.contains("cannot be compared"), "{err}");
    }
}
