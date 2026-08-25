//! DOCX: read `word/document.xml` into addressable blocks, and splice edits
//! back into it.
//!
//! # Addressing
//!
//! Paragraphs are indexed in document order across the whole body — including
//! paragraphs inside table cells — as `body/p[N]`. Runs within a paragraph are
//! `body/p[N]/r[M]`. A flat index is deliberate: it is stable under formatting
//! changes, it needs no path-walking on either side, and a paragraph in a table
//! is addressed exactly like one outside it, so table editing needs no separate
//! machinery.
//!
//! # What is deliberately not addressed
//!
//! Headers, footers, footnotes, comments and numbering live in other parts and
//! are never touched. Fields, content controls, bookmarks, tracked changes and
//! drawings inside the body are read past, not rewritten. They are invisible to
//! the editor and therefore safe from it.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::package::OoxmlPackage;
use super::spans::{apply_splices, escape, scan, unescape, Span, Splice, XmlScan};
use crate::document_workspace::view_model::{wrong_format, DraftPatch, DraftViewModel};

pub const DOCUMENT_PART: &str = "word/document.xml";
pub const STYLES_PART: &str = "word/styles.xml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub addr: String,
    /// Paragraph style id, e.g. `Heading1`. Absent means the document default.
    pub style: Option<String>,
    /// Numbering id when the paragraph is part of a list, so the UI can show a
    /// bullet or number without inventing one.
    pub num_id: Option<String>,
    /// Set when the paragraph sits inside a table cell.
    pub cell: Option<CellRef>,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellRef {
    pub table: usize,
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub addr: String,
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleInfo {
    pub id: String,
    pub name: String,
    pub bold: bool,
    /// Half-points, as Word stores it; the UI halves it for CSS pixels.
    pub size_half_points: Option<u32>,
    pub color: Option<String>,
}

// ---------------------------------------------------------------- reading

pub fn build(bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    let pkg = OoxmlPackage::open(bytes)?;
    let doc = pkg.part_str(DOCUMENT_PART)?;
    let blocks = read_blocks(doc)?;
    let styles = pkg
        .part_str(STYLES_PART)
        .ok()
        .map(read_styles)
        .transpose()?
        .unwrap_or_default();

    debug!("DOCX view model: {} blocks, {} styles", blocks.len(), styles.len());
    Ok(DraftViewModel::Docx { version, blocks, styles })
}

fn read_blocks(source: &str) -> Result<Vec<Block>> {
    let scan = scan(source)?;
    let paragraphs = scan.by_local("p");
    let tables = scan.by_local("tbl");
    let rows = scan.by_local("tr");
    let cells = scan.by_local("tc");

    let mut blocks = Vec::with_capacity(paragraphs.len());
    for (p_ord, &p_idx) in paragraphs.iter().enumerate() {
        let addr = format!("body/p[{}]", p_ord);
        let pr = scan.first_descendant(p_idx, "pPr");

        let style = pr
            .and_then(|pr| direct_child(&scan, pr, "pStyle"))
            .and_then(|i| scan.elements[i].attr(source, "w:val"));
        let num_id = pr
            .and_then(|pr| scan.first_descendant(pr, "numId"))
            .and_then(|i| scan.elements[i].attr(source, "w:val"));

        let cell = enclosing_cell(&scan, p_idx, &tables, &rows, &cells);

        let mut runs = Vec::new();
        for (r_ord, &r_idx) in scan.descendants_by_local(p_idx, "r").iter().enumerate() {
            // A run's visible text is every w:t it contains, concatenated —
            // Word splits runs at spell-check and revision boundaries, so one
            // logical word is routinely several w:t nodes.
            let text: String = scan
                .descendants_by_local(r_idx, "t")
                .iter()
                .map(|&t| unescape(scan.elements[t].inner.of(source)))
                .collect();

            let rpr = scan.first_descendant(r_idx, "rPr");
            runs.push(Run {
                addr: format!("{}/r[{}]", addr, r_ord),
                text,
                bold: toggle_on(&scan, source, rpr, "b"),
                italic: toggle_on(&scan, source, rpr, "i"),
                underline: toggle_on(&scan, source, rpr, "u"),
            });
        }

        blocks.push(Block { addr, style, num_id, cell, runs });
    }
    Ok(blocks)
}

/// Which table cell, if any, encloses this paragraph.
fn enclosing_cell(
    scan: &XmlScan,
    p_idx: usize,
    tables: &[usize],
    rows: &[usize],
    cells: &[usize],
) -> Option<CellRef> {
    let tc = ancestor_with_local(scan, p_idx, "tc")?;
    let tr = ancestor_with_local(scan, tc, "tr")?;
    let tbl = ancestor_with_local(scan, tr, "tbl")?;

    Some(CellRef {
        table: tables.iter().position(|&i| i == tbl)?,
        // Row and column are positions WITHIN their parent, not globally, so
        // the UI can lay out a grid directly.
        row: rows
            .iter()
            .filter(|&&r| scan.elements[r].outer.start >= scan.elements[tbl].outer.start
                && scan.elements[r].outer.end <= scan.elements[tbl].outer.end)
            .position(|&r| r == tr)?,
        col: cells
            .iter()
            .filter(|&&c| scan.elements[c].outer.start >= scan.elements[tr].outer.start
                && scan.elements[c].outer.end <= scan.elements[tr].outer.end)
            .position(|&c| c == tc)?,
    })
}

fn ancestor_with_local(scan: &XmlScan, from: usize, local: &str) -> Option<usize> {
    let mut cur = scan.elements[from].parent;
    while let Some(i) = cur {
        if scan.elements[i].local == local {
            return Some(i);
        }
        cur = scan.elements[i].parent;
    }
    None
}

fn direct_child(scan: &XmlScan, parent: usize, local: &str) -> Option<usize> {
    scan.elements
        .iter()
        .enumerate()
        .find(|(_, e)| e.parent == Some(parent) && e.local == local)
        .map(|(i, _)| i)
}

/// A Word toggle property is on when the element is present, UNLESS it carries
/// `w:val="0"`/`"false"`. Treating mere presence as "on" would show text as
/// bold that Word renders normally.
fn toggle_on(scan: &XmlScan, source: &str, rpr: Option<usize>, local: &str) -> bool {
    let Some(rpr) = rpr else { return false };
    let Some(el) = direct_child(scan, rpr, local) else { return false };
    match scan.elements[el].attr(source, "w:val").as_deref() {
        Some("0") | Some("false") | Some("none") => false,
        _ => true,
    }
}

fn read_styles(source: &str) -> Result<Vec<StyleInfo>> {
    let scan = scan(source)?;
    let mut out = Vec::new();
    for &s in scan.by_local("style").iter() {
        let Some(id) = scan.elements[s].attr(source, "w:styleId") else { continue };
        let name = scan
            .first_descendant(s, "name")
            .and_then(|i| scan.elements[i].attr(source, "w:val"))
            .unwrap_or_else(|| id.clone());
        let rpr = scan.first_descendant(s, "rPr");
        out.push(StyleInfo {
            bold: toggle_on(&scan, source, rpr, "b"),
            size_half_points: rpr
                .and_then(|r| direct_child(&scan, r, "sz"))
                .and_then(|i| scan.elements[i].attr(source, "w:val"))
                .and_then(|v| v.parse().ok()),
            color: rpr
                .and_then(|r| direct_child(&scan, r, "color"))
                .and_then(|i| scan.elements[i].attr(source, "w:val"))
                .filter(|c| c != "auto"),
            id,
            name,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- writing

pub fn apply(bytes: &[u8], patches: &[DraftPatch]) -> Result<Vec<u8>> {
    let mut pkg = OoxmlPackage::open(bytes)?;
    let source = pkg.part_str(DOCUMENT_PART)?.to_string();
    let scan = scan(&source)?;
    let paragraphs = scan.by_local("p");

    let mut splices = Vec::new();
    for patch in patches {
        match patch {
            DraftPatch::SetRunText { addr, text } => {
                let (p, r) = parse_run_addr(addr)?;
                let run = resolve_run(&scan, &paragraphs, p, r, addr)?;
                splices.extend(set_run_text(&scan, run, text));
            }
            DraftPatch::SetRunFormat { addr, bold, italic, underline } => {
                let (p, r) = parse_run_addr(addr)?;
                let run = resolve_run(&scan, &paragraphs, p, r, addr)?;
                splices.push(set_run_format(&source, &scan, run, *bold, *italic, *underline));
            }
            DraftPatch::SetParagraphStyle { addr, style } => {
                let p = parse_paragraph_addr(addr)?;
                let para = resolve_paragraph(&paragraphs, p, addr)?;
                splices.push(set_paragraph_style(&source, &scan, para, style.as_deref()));
            }
            DraftPatch::DeleteParagraph { addr } => {
                let p = parse_paragraph_addr(addr)?;
                let para = resolve_paragraph(&paragraphs, p, addr)?;
                splices.push(Splice {
                    span: scan.elements[para].outer,
                    replacement: String::new(),
                });
            }
            DraftPatch::InsertParagraphAfter { addr, text } => {
                let p = parse_paragraph_addr(addr)?;
                let para = resolve_paragraph(&paragraphs, p, addr)?;
                let prefix = prefix_of(&scan.elements[para].qname);
                let end = scan.elements[para].outer.end;
                splices.push(Splice {
                    span: Span { start: end, end },
                    replacement: new_paragraph(&prefix, text),
                });
            }
            other => return Err(wrong_format(other, "docx")),
        }
    }

    let patched = apply_splices(&source, splices)?;
    pkg.replace_part(DOCUMENT_PART, patched.into_bytes())?;
    pkg.save()
}

/// Replace a run's text.
///
/// A run can hold several `w:t` nodes with `w:br`/`w:tab` between them. The
/// full text goes into the first, and the rest are emptied rather than deleted,
/// so line breaks and tabs the editor never showed the user still survive.
fn set_run_text(scan: &XmlScan, run: usize, text: &str) -> Vec<Splice> {
    let ts = scan.descendants_by_local(run, "t");
    let prefix = prefix_of(&scan.elements[run].qname);

    if ts.is_empty() {
        // A run with no text node at all (a drawing, a field char). Give it
        // one rather than refusing — the user typed into it on screen.
        let at = scan.elements[run].inner.end;
        return vec![Splice {
            span: Span { start: at, end: at },
            replacement: new_text_node(&prefix, text),
        }];
    }

    let mut splices = vec![Splice {
        span: scan.elements[ts[0]].outer,
        replacement: new_text_node(&prefix, text),
    }];
    for &t in &ts[1..] {
        splices.push(Splice { span: scan.elements[t].inner, replacement: String::new() });
    }
    if ts.len() > 1 {
        debug!("Run had {} text nodes; consolidated into the first", ts.len());
    }
    splices
}

/// Rewrite a run's `w:rPr` as a single splice.
///
/// Rewriting the whole property block in one go — rather than inserting and
/// deleting individual toggles — is what keeps schema order valid and avoids
/// several zero-width edits landing on the same offset. Properties we do not
/// control (fonts, sizes, colours, languages) are carried through untouched.
fn set_run_format(
    source: &str,
    scan: &XmlScan,
    run: usize,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
) -> Splice {
    let prefix = prefix_of(&scan.elements[run].qname);
    let rpr = scan.first_descendant(run, "rPr").filter(|&i| scan.elements[i].parent == Some(run));

    // Existing state, so an unspecified property keeps its current value.
    let want = [
        (bold.unwrap_or_else(|| toggle_on(scan, source, rpr, "b")), "b"),
        (italic.unwrap_or_else(|| toggle_on(scan, source, rpr, "i")), "i"),
        (underline.unwrap_or_else(|| toggle_on(scan, source, rpr, "u")), "u"),
    ];

    let mut toggles = String::new();
    for (on, local) in want {
        if on {
            // Underline needs a style value; bold and italic are bare toggles.
            if local == "u" {
                toggles.push_str(&format!("<{p}:u {p}:val=\"single\"/>", p = prefix));
            } else {
                toggles.push_str(&format!("<{}:{}/>", prefix, local));
            }
        }
    }

    match rpr {
        Some(rpr) => {
            let kept = remove_direct_children(source, scan, rpr, &["b", "i", "u"]);
            Splice {
                span: scan.elements[rpr].inner,
                replacement: format!("{}{}", toggles, kept),
            }
        }
        None => {
            // `w:rPr` must be the first child of `w:r`.
            let at = scan.elements[run].inner.start;
            Splice {
                span: Span { start: at, end: at },
                replacement: format!("<{p}:rPr>{}</{p}:rPr>", toggles, p = prefix),
            }
        }
    }
}

fn set_paragraph_style(source: &str, scan: &XmlScan, para: usize, style: Option<&str>) -> Splice {
    let prefix = prefix_of(&scan.elements[para].qname);
    let new_style = style
        .map(|s| format!("<{p}:pStyle {p}:val=\"{}\"/>", escape(s), p = prefix))
        .unwrap_or_default();

    let ppr = scan.first_descendant(para, "pPr").filter(|&i| scan.elements[i].parent == Some(para));
    match ppr {
        Some(ppr) => {
            // `w:pStyle` must lead `w:pPr`; everything else (numbering,
            // spacing, justification, borders) is preserved in place.
            let kept = remove_direct_children(source, scan, ppr, &["pStyle"]);
            Splice {
                span: scan.elements[ppr].inner,
                replacement: format!("{}{}", new_style, kept),
            }
        }
        None => {
            let at = scan.elements[para].inner.start;
            Splice {
                span: Span { start: at, end: at },
                replacement: format!("<{p}:pPr>{}</{p}:pPr>", new_style, p = prefix),
            }
        }
    }
}

/// The inner XML of `parent` with the named direct children removed.
fn remove_direct_children(
    source: &str,
    scan: &XmlScan,
    parent: usize,
    locals: &[&str],
) -> String {
    let inner = scan.elements[parent].inner;
    let mut drop: Vec<Span> = scan
        .elements
        .iter()
        .filter(|e| e.parent == Some(parent) && locals.contains(&e.local.as_str()))
        .map(|e| e.outer)
        .collect();
    drop.sort_by_key(|s| s.start);

    let mut out = String::new();
    let mut cursor = inner.start;
    for span in drop {
        if span.start >= cursor {
            out.push_str(&source[cursor..span.start]);
            cursor = span.end;
        }
    }
    out.push_str(&source[cursor..inner.end]);
    out
}

/// Always writes `xml:space="preserve"`. Without it Word collapses leading and
/// trailing spaces, so "Section 2. " silently loses its trailing space and the
/// next run runs into it.
fn new_text_node(prefix: &str, text: &str) -> String {
    format!(
        "<{p}:t xml:space=\"preserve\">{}</{p}:t>",
        escape(text),
        p = prefix
    )
}

fn new_paragraph(prefix: &str, text: &str) -> String {
    format!(
        "<{p}:p><{p}:r>{}</{p}:r></{p}:p>",
        new_text_node(prefix, text),
        p = prefix
    )
}

fn prefix_of(qname: &str) -> String {
    qname.split_once(':').map(|(p, _)| p.to_string()).unwrap_or_else(|| "w".into())
}

// ---------------------------------------------------------------- addressing

fn parse_paragraph_addr(addr: &str) -> Result<usize> {
    index_in(addr, "body/p[").ok_or_else(|| bad_addr(addr, "body/p[12]"))
}

fn parse_run_addr(addr: &str) -> Result<(usize, usize)> {
    let (p_part, r_part) = addr
        .split_once("/r[")
        .ok_or_else(|| bad_addr(addr, "body/p[12]/r[0]"))?;
    let p = index_in(p_part, "body/p[").ok_or_else(|| bad_addr(addr, "body/p[12]/r[0]"))?;
    let r = r_part
        .strip_suffix(']')
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad_addr(addr, "body/p[12]/r[0]"))?;
    Ok((p, r))
}

fn index_in(s: &str, open: &str) -> Option<usize> {
    s.strip_prefix(open)?.strip_suffix(']')?.parse().ok()
}

fn bad_addr(addr: &str, example: &str) -> anyhow::Error {
    anyhow!("'{}' is not a valid address; expected the form '{}'", addr, example)
}

/// Resolving is where a stale editor is caught. If the browser holds a view
/// model from before a paragraph was deleted, its address now points past the
/// end — and saying so is far better than editing whatever moved into that
/// slot.
fn resolve_paragraph(paragraphs: &[usize], p: usize, addr: &str) -> Result<usize> {
    paragraphs.get(p).copied().ok_or_else(|| {
        anyhow!(
            "'{}' no longer exists in this document (it has {} paragraphs). \
             Reopen the draft to pick up the current version.",
            addr,
            paragraphs.len()
        )
    })
}

fn resolve_run(scan: &XmlScan, paragraphs: &[usize], p: usize, r: usize, addr: &str) -> Result<usize> {
    let para = resolve_paragraph(paragraphs, p, addr)?;
    let runs = scan.descendants_by_local(para, "r");
    runs.get(r).copied().ok_or_else(|| {
        anyhow!(
            "'{}' no longer exists (that paragraph has {} runs). \
             Reopen the draft to pick up the current version.",
            addr,
            runs.len()
        )
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_workspace::ooxml::package::fixtures;
    use crate::document_workspace::view_model::DraftViewModel;

    fn blocks(bytes: &[u8]) -> Vec<Block> {
        match build(bytes, 1).unwrap() {
            DraftViewModel::Docx { blocks, .. } => blocks,
            other => panic!("expected a docx model, got {:?}", other),
        }
    }

    #[test]
    fn paragraphs_runs_and_styles_are_read_with_stable_addresses() {
        let b = blocks(&fixtures::docx());
        assert_eq!(b[0].addr, "body/p[0]");
        assert_eq!(b[0].style.as_deref(), Some("Heading1"));
        assert_eq!(b[0].runs[0].text, "Master Services Agreement");

        // Three runs, because the date is bolded in the middle of the sentence.
        assert_eq!(b[1].runs.len(), 3);
        assert_eq!(b[1].runs[1].addr, "body/p[1]/r[1]");
        assert!(b[1].runs[1].bold, "the date run is bold in the fixture");
        assert!(!b[1].runs[0].bold);
    }

    #[test]
    fn table_paragraphs_are_addressed_like_any_other_and_carry_their_cell() {
        let b = blocks(&fixtures::docx());
        let in_table: Vec<&Block> = b.iter().filter(|x| x.cell.is_some()).collect();
        assert_eq!(in_table.len(), 2, "the fixture table has two cells");

        let c0 = in_table[0].cell.as_ref().unwrap();
        let c1 = in_table[1].cell.as_ref().unwrap();
        assert_eq!((c0.table, c0.row, c0.col), (0, 0, 0));
        assert_eq!((c1.table, c1.row, c1.col), (0, 0, 1));
        assert_eq!(in_table[0].runs[0].text, "Term");
    }

    #[test]
    fn styles_are_read_from_the_styles_part() {
        let model = build(&fixtures::docx(), 1).unwrap();
        let DraftViewModel::Docx { styles, .. } = model else { panic!() };
        let h1 = styles.iter().find(|s| s.id == "Heading1").unwrap();
        assert_eq!(h1.name, "heading 1");
        assert!(h1.bold);
        assert_eq!(h1.size_half_points, Some(32));
        assert_eq!(h1.color.as_deref(), Some("1F3864"));
    }

    /// The invariant that matters: editing text changes document.xml and
    /// nothing else.
    #[test]
    fn editing_text_touches_only_the_document_part() {
        let original = fixtures::docx();
        let saved = apply(
            &original,
            &[DraftPatch::SetRunText {
                addr: "body/p[0]/r[0]".into(),
                text: "Amended and Restated Agreement".into(),
            }],
        )
        .unwrap();

        let before = OoxmlPackage::open(&original).unwrap();
        let after = OoxmlPackage::open(&saved).unwrap();
        for name in before.part_names() {
            if name == DOCUMENT_PART {
                continue;
            }
            assert_eq!(
                before.part(name).unwrap(),
                after.part(name).unwrap(),
                "part '{}' must be untouched by a text edit",
                name
            );
        }
        assert_eq!(blocks(&saved)[0].runs[0].text, "Amended and Restated Agreement");
    }

    #[test]
    fn text_with_markup_characters_survives_a_round_trip() {
        let tricky = "Clauses 3 & 4 <indemnity> apply \"as-is\"";
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunText { addr: "body/p[0]/r[0]".into(), text: tricky.into() }],
        )
        .unwrap();
        assert_eq!(blocks(&saved)[0].runs[0].text, tricky);
    }

    #[test]
    fn leading_and_trailing_spaces_are_preserved() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunText { addr: "body/p[0]/r[0]".into(), text: "  spaced  ".into() }],
        )
        .unwrap();
        assert_eq!(blocks(&saved)[0].runs[0].text, "  spaced  ");
        let pkg = OoxmlPackage::open(&saved).unwrap();
        assert!(pkg.part_str(DOCUMENT_PART).unwrap().contains("xml:space=\"preserve\""));
    }

    #[test]
    fn bold_can_be_added_to_a_run_that_had_no_properties_at_all() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunFormat {
                addr: "body/p[1]/r[0]".into(),
                bold: Some(true),
                italic: None,
                underline: None,
            }],
        )
        .unwrap();
        let r = &blocks(&saved)[1].runs[0];
        assert!(r.bold);
        assert_eq!(r.text, "This Agreement is entered into as of ");
    }

    #[test]
    fn turning_bold_off_removes_it_without_disturbing_the_text() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunFormat {
                addr: "body/p[1]/r[1]".into(),
                bold: Some(false),
                italic: None,
                underline: None,
            }],
        )
        .unwrap();
        let r = &blocks(&saved)[1].runs[1];
        assert!(!r.bold);
        assert_eq!(r.text, "1 January 2026");
    }

    #[test]
    fn an_unspecified_format_property_keeps_its_current_value() {
        // Italicise the already-bold run; it must stay bold.
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunFormat {
                addr: "body/p[1]/r[1]".into(),
                bold: None,
                italic: Some(true),
                underline: None,
            }],
        )
        .unwrap();
        let r = &blocks(&saved)[1].runs[1];
        assert!(r.bold, "bold was not mentioned, so it must survive");
        assert!(r.italic);
    }

    #[test]
    fn changing_a_paragraph_style_preserves_the_rest_of_its_properties() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetParagraphStyle {
                addr: "body/p[0]".into(),
                style: Some("Normal".into()),
            }],
        )
        .unwrap();
        assert_eq!(blocks(&saved)[0].style.as_deref(), Some("Normal"));
    }

    #[test]
    fn a_style_can_be_cleared_back_to_the_document_default() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetParagraphStyle { addr: "body/p[0]".into(), style: None }],
        )
        .unwrap();
        assert_eq!(blocks(&saved)[0].style, None);
    }

    #[test]
    fn a_style_can_be_applied_to_a_paragraph_that_had_no_properties() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetParagraphStyle {
                addr: "body/p[2]".into(),
                style: Some("Heading1".into()),
            }],
        )
        .unwrap();
        assert_eq!(blocks(&saved)[2].style.as_deref(), Some("Heading1"));
        assert!(blocks(&saved)[2].runs[0].text.starts_with("Section 2."));
    }

    #[test]
    fn inserting_and_deleting_paragraphs_shifts_the_addresses_as_expected() {
        let original = fixtures::docx();
        let before = blocks(&original).len();

        let saved = apply(
            &original,
            &[DraftPatch::InsertParagraphAfter {
                addr: "body/p[0]".into(),
                text: "Recitals".into(),
            }],
        )
        .unwrap();
        let after = blocks(&saved);
        assert_eq!(after.len(), before + 1);
        assert_eq!(after[1].runs[0].text, "Recitals");

        let deleted = apply(&saved, &[DraftPatch::DeleteParagraph { addr: "body/p[1]".into() }]).unwrap();
        assert_eq!(blocks(&deleted).len(), before);
    }

    #[test]
    fn several_edits_in_one_batch_all_land() {
        let saved = apply(
            &fixtures::docx(),
            &[
                DraftPatch::SetRunText { addr: "body/p[0]/r[0]".into(), text: "One".into() },
                DraftPatch::SetRunText { addr: "body/p[2]/r[0]".into(), text: "Three".into() },
                DraftPatch::SetParagraphStyle { addr: "body/p[2]".into(), style: Some("Heading1".into()) },
            ],
        )
        .unwrap();
        let b = blocks(&saved);
        assert_eq!(b[0].runs[0].text, "One");
        assert_eq!(b[2].runs[0].text, "Three");
        assert_eq!(b[2].style.as_deref(), Some("Heading1"));
    }

    /// A stale address must be refused, not applied to whatever moved into that
    /// position. This is the last line of defence behind the version guard.
    #[test]
    fn an_out_of_range_address_is_refused_with_advice() {
        let err = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunText { addr: "body/p[99]/r[0]".into(), text: "x".into() }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no longer exists"), "{}", err);
        assert!(err.contains("Reopen the draft"), "{}", err);
    }

    #[test]
    fn a_malformed_address_names_the_expected_form() {
        let err = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunText { addr: "paragraph 3".into(), text: "x".into() }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("body/p[12]/r[0]"), "{}", err);
    }

    #[test]
    fn a_patch_meant_for_another_format_is_refused_before_anything_is_written() {
        let err = apply(
            &fixtures::docx(),
            &[DraftPatch::SetCellValue { addr: "sheet[0]/A1".into(), value: "1".into() }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("SetCellValue"), "{}", err);
        assert!(err.contains("has not been changed"), "{}", err);
    }
}
