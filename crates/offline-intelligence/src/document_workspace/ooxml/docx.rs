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
//! # Patches apply in sequence
//!
//! Each patch is resolved against the document as the previous patch left it.
//! Structural edits (split, merge, insert table) move addresses, and a batch
//! that mixed them with text edits resolved against the ORIGINAL part would
//! land edits on the wrong paragraph or produce overlapping splices. The editor
//! sends structural edits on their own for the same reason.
//!
//! # What is deliberately not addressed
//!
//! Headers, footers, footnotes and comments live in other parts and are never
//! touched. Fields, content controls, bookmarks and tracked changes inside the
//! body are read past, not rewritten. They are invisible to the editor and
//! therefore safe from it.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::math;
use super::package::{read_relationships, rels_part_for, resolve_target, OoxmlPackage};
use super::props::{children, ppr_inner, rpr_inner, ParagraphChange};
use super::spans::{apply_splices, escape, escape_attr, scan, unescape, Span, Splice, XmlScan};
use crate::document_workspace::view_model::{wrong_format, DraftPatch, DraftViewModel, TextFormat};

pub const DOCUMENT_PART: &str = "word/document.xml";
pub const STYLES_PART: &str = "word/styles.xml";
const DOCUMENT_RELS_PART: &str = "word/_rels/document.xml.rels";

const REL_IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
const REL_NUMBERING: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering";
const CT_NUMBERING: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml";
const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// Numbering definitions this editor adds, well clear of Word's own low ids.
const BULLET_NUM_ID: u32 = 9101;
const NUMBER_NUM_ID: u32 = 9102;

/// 6.5 inches: the text width of a Letter page with 1-inch margins.
const MAX_IMAGE_WIDTH_EMU: i64 = 5_943_600;
/// Table width in twips for the same text block.
const TABLE_WIDTH_TWIPS: usize = 9_360;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub addr: String,
    /// Paragraph style id, e.g. `Heading1`. Absent means the document default.
    pub style: Option<String>,
    /// Numbering id when the paragraph is part of a list.
    pub num_id: Option<String>,
    /// `bullet` or `number` when the paragraph is a list item.
    #[serde(default)]
    pub list: Option<String>,
    /// List or indent level, 0-based.
    #[serde(default)]
    pub level: u32,
    /// `left`, `center`, `right` or `justify`, when set on the paragraph.
    #[serde(default)]
    pub align: Option<String>,
    #[serde(default)]
    pub line_spacing: Option<f32>,
    /// Set when the paragraph sits inside a table cell.
    pub cell: Option<CellRef>,
    /// The linear form of a display equation, when this paragraph is one.
    #[serde(default)]
    pub equation: Option<String>,
    #[serde(default)]
    pub page_break: bool,
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
    #[serde(default)]
    pub strike: bool,
    #[serde(default)]
    pub font: Option<String>,
    /// Points.
    #[serde(default)]
    pub size: Option<f32>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub highlight: Option<String>,
    #[serde(default)]
    pub vert_align: Option<String>,
    /// Part of an equation; its text is maths, not prose.
    #[serde(default)]
    pub math: bool,
    #[serde(default)]
    pub image: Option<InlineImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineImage {
    pub data_url: String,
    pub width_emu: i64,
    pub height_emu: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleInfo {
    pub id: String,
    pub name: String,
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    /// Half-points, as Word stores it; the UI halves it for CSS pixels.
    pub size_half_points: Option<u32>,
    pub color: Option<String>,
    #[serde(default)]
    pub font: Option<String>,
    /// `paragraph`, `character`, `table` or `numbering`.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocDefaults {
    pub font: Option<String>,
    pub size: Option<f32>,
}

// ---------------------------------------------------------------- reading

pub fn build(bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    let pkg = OoxmlPackage::open(bytes)?;
    let doc = pkg.part_str(DOCUMENT_PART)?;
    let lists = read_list_kinds(&pkg);
    let blocks = read_blocks(&pkg, doc, &lists)?;
    let (styles, defaults) = match pkg.part_str(STYLES_PART) {
        Ok(src) => (read_styles(src)?, read_defaults(src)),
        Err(_) => (Vec::new(), DocDefaults::default()),
    };

    debug!("DOCX view model: {} blocks, {} styles", blocks.len(), styles.len());
    Ok(DraftViewModel::Docx { version, blocks, styles, defaults })
}

fn read_blocks(pkg: &OoxmlPackage, source: &str, lists: &HashMap<String, Vec<String>>) -> Result<Vec<Block>> {
    let scan = scan(source)?;
    let paragraphs = scan.by_local("p");
    let tables = scan.by_local("tbl");
    let rows = scan.by_local("tr");
    let cells = scan.by_local("tc");
    let rels = pkg
        .part_str(DOCUMENT_RELS_PART)
        .ok()
        .and_then(|r| read_relationships(r).ok())
        .unwrap_or_default();

    let mut blocks = Vec::with_capacity(paragraphs.len());
    for (p_ord, &p_idx) in paragraphs.iter().enumerate() {
        let addr = format!("body/p[{}]", p_ord);
        let pr = direct_child(&scan, p_idx, "pPr");

        let style = pr
            .and_then(|pr| direct_child(&scan, pr, "pStyle"))
            .and_then(|i| scan.elements[i].attr(source, "w:val"));
        let num_pr = pr.and_then(|pr| direct_child(&scan, pr, "numPr"));
        let num_id = num_pr
            .and_then(|n| direct_child(&scan, n, "numId"))
            .and_then(|i| scan.elements[i].attr(source, "w:val"))
            .filter(|v| v != "0");
        let ilvl: u32 = num_pr
            .and_then(|n| direct_child(&scan, n, "ilvl"))
            .and_then(|i| scan.elements[i].attr(source, "w:val"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let list = num_id.as_ref().map(|id| {
            let kind = lists
                .get(id)
                .and_then(|levels| levels.get(ilvl as usize).or_else(|| levels.first()))
                .map(|fmt| fmt.as_str())
                .unwrap_or("bullet");
            if kind == "bullet" || kind == "none" { "bullet".to_string() } else { "number".to_string() }
        });
        let indent_level = pr
            .and_then(|pr| direct_child(&scan, pr, "ind"))
            .and_then(|i| scan.elements[i].attr(source, "w:left").or_else(|| scan.elements[i].attr(source, "w:start")))
            .and_then(|v| v.parse::<i64>().ok())
            .map(|t| (t / 720).max(0) as u32)
            .unwrap_or(0);
        let align = pr
            .and_then(|pr| direct_child(&scan, pr, "jc"))
            .and_then(|i| scan.elements[i].attr(source, "w:val"))
            .map(|v| match v.as_str() {
                "both" | "distribute" => "justify".to_string(),
                "start" => "left".to_string(),
                "end" => "right".to_string(),
                other => other.to_string(),
            });
        let line_spacing = pr
            .and_then(|pr| direct_child(&scan, pr, "spacing"))
            .filter(|&i| scan.elements[i].attr(source, "w:lineRule").map(|r| r == "auto").unwrap_or(true))
            .and_then(|i| scan.elements[i].attr(source, "w:line"))
            .and_then(|v| v.parse::<f32>().ok())
            .map(|l| (l / 240.0 * 100.0).round() / 100.0);

        let equation = scan
            .descendants_by_local(p_idx, "oMathPara")
            .first()
            .map(|&m| math::to_linear(source, &scan, m));

        let cell = enclosing_cell(&scan, p_idx, &tables, &rows, &cells);

        let mut page_break = false;
        let mut runs = Vec::new();
        for (r_ord, r_idx) in para_runs(&scan, p_idx).into_iter().enumerate() {
            // A run's visible text is every w:t it contains, concatenated —
            // Word splits runs at spell-check and revision boundaries, so one
            // logical word is routinely several w:t nodes.
            let text: String = scan
                .descendants_by_local(r_idx, "t")
                .iter()
                .map(|&t| unescape(scan.elements[t].inner.of(source)))
                .collect();

            if scan
                .descendants_by_local(r_idx, "br")
                .iter()
                .any(|&b| scan.elements[b].attr(source, "w:type").as_deref() == Some("page"))
            {
                page_break = true;
            }

            let rpr = direct_child(&scan, r_idx, "rPr");
            let is_math = is_math_run(&scan, r_idx);
            let val = |local: &str| {
                rpr.and_then(|r| direct_child(&scan, r, local))
                    .and_then(|i| scan.elements[i].attr(source, "w:val"))
            };
            runs.push(Run {
                addr: format!("{}/r[{}]", addr, r_ord),
                text,
                bold: toggle_on(&scan, source, rpr, "b"),
                italic: toggle_on(&scan, source, rpr, "i"),
                underline: toggle_on(&scan, source, rpr, "u"),
                strike: toggle_on(&scan, source, rpr, "strike"),
                font: rpr
                    .and_then(|r| direct_child(&scan, r, "rFonts"))
                    .and_then(|i| {
                        let el = &scan.elements[i];
                        el.attr(source, "w:ascii").or_else(|| el.attr(source, "w:hAnsi"))
                    }),
                size: val("sz").and_then(|v| v.parse::<f32>().ok()).map(|h| h / 2.0),
                color: val("color").filter(|c| c != "auto"),
                highlight: val("highlight").filter(|h| h != "none"),
                vert_align: val("vertAlign").filter(|v| v != "baseline"),
                math: is_math,
                image: read_inline_image(pkg, &rels, &scan, source, r_idx),
            });
        }

        blocks.push(Block {
            addr,
            style,
            num_id,
            list,
            level: if num_pr.is_some() { ilvl } else { indent_level },
            align,
            line_spacing,
            cell,
            equation,
            page_break,
            runs,
        });
    }
    Ok(blocks)
}

/// The runs of a paragraph, in order, excluding runs of paragraphs nested
/// inside it (text boxes), which belong to those paragraphs.
fn para_runs(scan: &XmlScan, para: usize) -> Vec<usize> {
    scan.descendants_by_local(para, "r")
        .into_iter()
        .filter(|&r| ancestor_with_local(scan, r, "p") == Some(para))
        // `w:r` inside `w:r` does not exist, but `m:r` wraps nothing; a run
        // nested in a drawing's text box is filtered by the check above.
        .collect()
}

fn is_math_run(scan: &XmlScan, run: usize) -> bool {
    ancestor_with_local(scan, run, "oMath").is_some()
}

fn read_inline_image(
    pkg: &OoxmlPackage,
    rels: &HashMap<String, String>,
    scan: &XmlScan,
    source: &str,
    run: usize,
) -> Option<InlineImage> {
    let blip = scan.first_descendant(run, "blip")?;
    let rid = scan.elements[blip].attr(source, "r:embed")?;
    let part = resolve_target("word", rels.get(&rid)?);
    let bytes = pkg.part(&part)?;
    let ext = part.rsplit('.').next().unwrap_or("png").to_ascii_lowercase();
    let mime = mime_for(&ext)?;
    let extent = scan.first_descendant(run, "extent");
    let dim = |attr: &str| {
        extent
            .and_then(|e| scan.elements[e].attr(source, attr))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(914_400)
    };
    Some(InlineImage {
        data_url: format!("data:{};base64,{}", mime, BASE64.encode(bytes)),
        width_emu: dim("cx"),
        height_emu: dim("cy"),
    })
}

fn mime_for(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        _ => return None,
    })
}

/// numId → the number format of each level (`bullet`, `decimal`, …).
fn read_list_kinds(pkg: &OoxmlPackage) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    let Some(part) = numbering_part(pkg) else { return out };
    let Ok(source) = pkg.part_str(&part) else { return out };
    let Ok(s) = scan(source) else { return out };

    let mut abstracts: HashMap<String, Vec<String>> = HashMap::new();
    for &a in s.by_local("abstractNum").iter() {
        let Some(id) = s.elements[a].attr(source, "w:abstractNumId") else { continue };
        let mut levels = Vec::new();
        for &lvl in s.descendants_by_local(a, "lvl").iter() {
            let fmt = direct_child(&s, lvl, "numFmt")
                .and_then(|f| s.elements[f].attr(source, "w:val"))
                .unwrap_or_else(|| "decimal".into());
            levels.push(fmt);
        }
        abstracts.insert(id, levels);
    }
    for &n in s.by_local("num").iter() {
        let Some(num_id) = s.elements[n].attr(source, "w:numId") else { continue };
        let Some(abs) = direct_child(&s, n, "abstractNumId").and_then(|i| s.elements[i].attr(source, "w:val")) else {
            continue;
        };
        if let Some(levels) = abstracts.get(&abs) {
            out.insert(num_id, levels.clone());
        }
    }
    out
}

fn numbering_part(pkg: &OoxmlPackage) -> Option<String> {
    let rels = pkg.part_str(DOCUMENT_RELS_PART).ok()?;
    let s = scan(rels).ok()?;
    s.by_local("Relationship").into_iter().find_map(|r| {
        let el = &s.elements[r];
        (el.attr(rels, "Type").as_deref() == Some(REL_NUMBERING))
            .then(|| el.attr(rels, "Target"))
            .flatten()
            .map(|t| resolve_target("word", &t))
    })
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
        row: rows.iter().filter(|&&r| scan.elements[r].parent == Some(tbl)).position(|&r| r == tr)?,
        col: cells.iter().filter(|&&c| scan.elements[c].parent == Some(tr)).position(|&c| c == tc)?,
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
        .skip(parent + 1)
        .take_while(|(_, e)| e.outer.start < scan.elements[parent].outer.end)
        .find(|(_, e)| e.parent == Some(parent) && e.local == local)
        .map(|(i, _)| i)
}

fn direct_children(scan: &XmlScan, parent: usize, local: &str) -> Vec<usize> {
    scan.elements
        .iter()
        .enumerate()
        .skip(parent + 1)
        .take_while(|(_, e)| e.outer.start < scan.elements[parent].outer.end)
        .filter(|(_, e)| e.parent == Some(parent) && e.local == local)
        .map(|(i, _)| i)
        .collect()
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
        let name = direct_child(&scan, s, "name")
            .and_then(|i| scan.elements[i].attr(source, "w:val"))
            .unwrap_or_else(|| id.clone());
        let rpr = direct_child(&scan, s, "rPr");
        out.push(StyleInfo {
            bold: toggle_on(&scan, source, rpr, "b"),
            italic: toggle_on(&scan, source, rpr, "i"),
            size_half_points: rpr
                .and_then(|r| direct_child(&scan, r, "sz"))
                .and_then(|i| scan.elements[i].attr(source, "w:val"))
                .and_then(|v| v.parse().ok()),
            color: rpr
                .and_then(|r| direct_child(&scan, r, "color"))
                .and_then(|i| scan.elements[i].attr(source, "w:val"))
                .filter(|c| c != "auto"),
            font: rpr
                .and_then(|r| direct_child(&scan, r, "rFonts"))
                .and_then(|i| scan.elements[i].attr(source, "w:ascii")),
            kind: scan.elements[s].attr(source, "w:type"),
            id,
            name,
        });
    }
    Ok(out)
}

fn read_defaults(source: &str) -> DocDefaults {
    let Ok(s) = scan(source) else { return DocDefaults::default() };
    let Some(rpr_default) = s.by_local("rPrDefault").into_iter().next() else {
        return DocDefaults::default();
    };
    let fonts = s.first_descendant(rpr_default, "rFonts");
    let font = fonts.and_then(|f| s.elements[f].attr(source, "w:ascii")).or_else(|| {
        // Theme fonts: Word's modern default is Calibri / Aptos body.
        fonts
            .and_then(|f| s.elements[f].attr(source, "w:asciiTheme"))
            .map(|_| "Calibri".to_string())
    });
    let size = s
        .first_descendant(rpr_default, "sz")
        .and_then(|i| s.elements[i].attr(source, "w:val"))
        .and_then(|v| v.parse::<f32>().ok())
        .map(|h| h / 2.0);
    DocDefaults { font, size }
}

// ---------------------------------------------------------------- writing

pub fn apply(bytes: &[u8], patches: &[DraftPatch]) -> Result<Vec<u8>> {
    let mut pkg = OoxmlPackage::open(bytes)?;
    let mut source = pkg.part_str(DOCUMENT_PART)?.to_string();

    for patch in patches {
        source = apply_one(&mut pkg, &source, patch)?;
    }

    pkg.replace_part(DOCUMENT_PART, source.into_bytes())?;
    pkg.save()
}

fn apply_one(pkg: &mut OoxmlPackage, source: &str, patch: &DraftPatch) -> Result<String> {
    let scan = scan(source)?;
    let paragraphs = scan.by_local("p");

    let splices: Vec<Splice> = match patch {
        DraftPatch::SetRunText { addr, text } => {
            let (p, r) = parse_run_addr(addr)?;
            let run = resolve_run(&scan, &paragraphs, p, r, addr)?;
            set_run_text(&scan, run, text)
        }
        DraftPatch::SetRunFormat { addr, format } => {
            let (p, r) = parse_run_addr(addr)?;
            let run = resolve_run(&scan, &paragraphs, p, r, addr)?;
            if is_math_run(&scan, run) {
                return Ok(source.to_string());
            }
            vec![format_run(source, &scan, run, format)]
        }
        DraftPatch::FormatRange { addr, start, end, format } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            format_range(source, &scan, para, *start, *end, format)
        }
        DraftPatch::SetParagraphStyle { addr, style } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            if let Some(id) = style {
                ensure_style(pkg, id)?;
            }
            let change = ParagraphChange { style: Some(style.clone()), ..Default::default() };
            vec![change_paragraph(source, &scan, para, &change)]
        }
        DraftPatch::SetParagraphFormat { addr, align, list, indent, line_spacing } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let mut change = ParagraphChange {
                align: align.clone(),
                line_spacing: *line_spacing,
                ..Default::default()
            };

            let ppr = direct_child(&scan, para, "pPr");
            let current_num = ppr
                .and_then(|pr| direct_child(&scan, pr, "numPr"))
                .and_then(|n| {
                    let id = direct_child(&scan, n, "numId").and_then(|i| scan.elements[i].attr(source, "w:val"))?;
                    let lvl = direct_child(&scan, n, "ilvl")
                        .and_then(|i| scan.elements[i].attr(source, "w:val"))
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(0);
                    Some((lvl, id.parse::<u32>().ok()?))
                })
                .filter(|(_, id)| *id != 0);

            match list.as_deref() {
                Some("bullet") | Some("number") => {
                    ensure_numbering(pkg)?;
                    let id = if list.as_deref() == Some("bullet") { BULLET_NUM_ID } else { NUMBER_NUM_ID };
                    change.numbering = Some(Some((current_num.map(|(l, _)| l).unwrap_or(0), id)));
                }
                Some("none") => change.numbering = Some(None),
                _ => {}
            }

            if let Some(delta) = indent {
                let listed = change.numbering.clone().unwrap_or(current_num.map(|n| Some(n)).unwrap_or(None));
                match listed {
                    Some((lvl, id)) => {
                        let next = (lvl as i32 + delta).clamp(0, 8) as u32;
                        change.numbering = Some(Some((next, id)));
                    }
                    None => {
                        let left = ppr
                            .and_then(|pr| direct_child(&scan, pr, "ind"))
                            .and_then(|i| {
                                scan.elements[i].attr(source, "w:left").or_else(|| scan.elements[i].attr(source, "w:start"))
                            })
                            .and_then(|v| v.parse::<i64>().ok())
                            .unwrap_or(0);
                        let next = (left + 720 * *delta as i64).clamp(0, 720 * 10);
                        change.indent_left = Some(Some(next));
                    }
                }
            }
            vec![change_paragraph(source, &scan, para, &change)]
        }
        DraftPatch::SetParagraphText { addr, text } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let prefix = prefix_of(&scan.elements[para].qname);
            let run = format!("<{p}:r>{}</{p}:r>", new_text_node(&prefix, text), p = prefix);
            vec![append_to_paragraph(source, &scan, para, &run)]
        }
        DraftPatch::DeleteParagraph { addr } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            delete_paragraph(&scan, para, addr)?
        }
        DraftPatch::DeleteRun { addr } => {
            let (p, r) = parse_run_addr(addr)?;
            let run = resolve_run(&scan, &paragraphs, p, r, addr)?;
            vec![Splice { span: scan.elements[run].outer, replacement: String::new() }]
        }
        DraftPatch::InsertParagraphAfter { addr, text } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let prefix = prefix_of(&scan.elements[para].qname);
            vec![insert_after(&scan, para, new_paragraph(&prefix, text))]
        }
        DraftPatch::SplitParagraph { addr, offset } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            split_paragraph(source, &scan, para, *offset)
        }
        DraftPatch::MergeParagraphWithPrevious { addr } => {
            let p = parse_paragraph_addr(addr)?;
            let para = resolve_paragraph(&paragraphs, p, addr)?;
            if p == 0 {
                return Ok(source.to_string());
            }
            merge_with_previous(source, &scan, paragraphs[p - 1], para)?
        }
        DraftPatch::InsertPageBreak { addr } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let p = prefix_of(&scan.elements[para].qname);
            vec![insert_after(
                &scan,
                para,
                format!("<{p}:p><{p}:r><{p}:br {p}:type=\"page\"/></{p}:r></{p}:p>{}", new_paragraph(&p, "")),
            )]
        }
        DraftPatch::InsertTable { addr, rows, cols } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let p = prefix_of(&scan.elements[para].qname);
            vec![insert_after(&scan, para, new_table(&p, *rows, *cols)?)]
        }
        DraftPatch::InsertTableRow { addr, below } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            vec![insert_table_row(source, &scan, para, *below, addr)?]
        }
        DraftPatch::DeleteTableRow { addr } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let (_, tr, tbl) = table_of(&scan, para, addr)?;
            let rows = direct_children(&scan, tbl, "tr");
            if rows.len() <= 1 {
                delete_table(&scan, tbl)
            } else {
                vec![Splice { span: scan.elements[tr].outer, replacement: String::new() }]
            }
        }
        DraftPatch::InsertTableColumn { addr, right } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            insert_table_column(source, &scan, para, *right, addr)?
        }
        DraftPatch::DeleteTableColumn { addr } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            delete_table_column(&scan, para, addr)?
        }
        DraftPatch::DeleteTable { addr } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let (_, _, tbl) = table_of(&scan, para, addr)?;
            delete_table(&scan, tbl)
        }
        DraftPatch::InsertImage { addr, data_base64, file_name, width_emu, height_emu, .. } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let p = prefix_of(&scan.elements[para].qname);
            let doc_pr_id = scan
                .by_local("docPr")
                .iter()
                .filter_map(|&d| scan.elements[d].attr(source, "id")?.parse::<u64>().ok())
                .max()
                .unwrap_or(0)
                + 1;
            let xml = insert_image(pkg, &p, data_base64, file_name, *width_emu, *height_emu, doc_pr_id)?;
            vec![insert_after(&scan, para, xml)]
        }
        DraftPatch::SetImageSize { addr, width_emu, height_emu } => {
            let (p, r) = parse_run_addr(addr)?;
            let run = resolve_run(&scan, &paragraphs, p, r, addr)?;
            set_image_size(source, &scan, run, *width_emu, *height_emu, addr)?
        }
        DraftPatch::InsertEquation { addr, text } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let p = prefix_of(&scan.elements[para].qname);
            vec![insert_after(
                &scan,
                para,
                format!("<{p}:p>{}</{p}:p>{}", math::display_equation(text), new_paragraph(&p, "")),
            )]
        }
        DraftPatch::SetEquation { addr, text } => {
            let para = resolve_paragraph(&paragraphs, parse_paragraph_addr(addr)?, addr)?;
            let target = scan
                .descendants_by_local(para, "oMathPara")
                .first()
                .copied()
                .ok_or_else(|| anyhow!("'{}' does not hold an equation.", addr))?;
            vec![Splice { span: scan.elements[target].outer, replacement: math::display_equation(text) }]
        }
        other => return Err(wrong_format(other, "docx")),
    };

    apply_splices(source, splices)
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
        return vec![Splice { span: Span { start: at, end: at }, replacement: new_text_node(&prefix, text) }];
    }

    let mut splices = vec![Splice { span: scan.elements[ts[0]].outer, replacement: new_text_node(&prefix, text) }];
    for &t in &ts[1..] {
        splices.push(Splice { span: scan.elements[t].inner, replacement: String::new() });
    }
    splices
}

/// The new inner XML of a run's `w:rPr` after `format`.
fn formatted_rpr(source: &str, scan: &XmlScan, run: usize, format: &TextFormat) -> String {
    let prefix = prefix_of(&scan.elements[run].qname);
    let kids = direct_child(scan, run, "rPr").map(|r| children(source, scan, r)).unwrap_or_default();
    rpr_inner(&prefix, kids, format)
}

/// Rewrite a run's `w:rPr` as one splice. Properties not named in `format`
/// (languages, spacing, effects) are carried through untouched.
fn format_run(source: &str, scan: &XmlScan, run: usize, format: &TextFormat) -> Splice {
    let prefix = prefix_of(&scan.elements[run].qname);
    let inner = formatted_rpr(source, scan, run, format);
    match direct_child(scan, run, "rPr") {
        Some(rpr) => Splice { span: scan.elements[rpr].inner, replacement: inner },
        None => {
            // `w:rPr` must be the first child of `w:r`.
            let at = scan.elements[run].inner.start;
            Splice { span: Span { start: at, end: at }, replacement: format!("<{p}:rPr>{}</{p}:rPr>", inner, p = prefix) }
        }
    }
}

fn run_text(source: &str, scan: &XmlScan, run: usize) -> String {
    scan.descendants_by_local(run, "t").iter().map(|&t| unescape(scan.elements[t].inner.of(source))).collect()
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Split at a UTF-16 offset, never inside a surrogate pair.
fn split_utf16(s: &str, at: usize) -> (String, String) {
    let mut units = 0;
    for (byte, ch) in s.char_indices() {
        if units >= at {
            return (s[..byte].to_string(), s[byte..].to_string());
        }
        units += ch.len_utf16();
    }
    (s.to_string(), String::new())
}

/// A run whose only children are `w:rPr` and a single `w:t` can be cut in two
/// without losing anything.
fn is_simple_run(scan: &XmlScan, run: usize) -> bool {
    let kids: Vec<&str> = scan
        .elements
        .iter()
        .filter(|e| e.parent == Some(run))
        .map(|e| e.local.as_str())
        .collect();
    kids.iter().filter(|k| **k == "t").count() == 1 && kids.iter().all(|k| *k == "t" || *k == "rPr")
}

/// A copy of `run` with new text and the given rPr inner XML.
fn rebuild_run(source: &str, scan: &XmlScan, run: usize, rpr_inner: &str, text: &str) -> String {
    let el = &scan.elements[run];
    let prefix = prefix_of(&el.qname);
    let open = el.start_tag.of(source).trim_end_matches("/>").trim_end_matches('>');
    let rpr = if rpr_inner.is_empty() { String::new() } else { format!("<{p}:rPr>{}</{p}:rPr>", rpr_inner, p = prefix) };
    format!("{}>{}{}</{}>", open, rpr, new_text_node(&prefix, text), el.qname)
}

fn format_range(
    source: &str,
    scan: &XmlScan,
    para: usize,
    start: usize,
    end: usize,
    format: &TextFormat,
) -> Vec<Splice> {
    let (start, end) = (start.min(end), start.max(end));
    let mut splices = Vec::new();
    let mut cursor = 0usize;

    for run in para_runs(scan, para) {
        let text = run_text(source, scan, run);
        let (rs, re) = (cursor, cursor + utf16_len(&text));
        cursor = re;

        // Empty runs and runs outside the range are left alone; so is maths.
        let overlaps = if start == end { false } else { rs < end && re > start };
        if !overlaps || is_math_run(scan, run) {
            continue;
        }

        if (start <= rs && re <= end) || !is_simple_run(scan, run) {
            splices.push(format_run(source, scan, run, format));
            continue;
        }

        let original = direct_child(scan, run, "rPr").map(|r| scan.elements[r].inner.of(source).to_string()).unwrap_or_default();
        let changed = formatted_rpr(source, scan, run, format);
        let (before, rest) = split_utf16(&text, start.saturating_sub(rs));
        let (middle, after) = split_utf16(&rest, end.min(re) - rs.max(start));

        let mut xml = String::new();
        if !before.is_empty() {
            xml.push_str(&rebuild_run(source, scan, run, &original, &before));
        }
        xml.push_str(&rebuild_run(source, scan, run, &changed, &middle));
        if !after.is_empty() {
            xml.push_str(&rebuild_run(source, scan, run, &original, &after));
        }
        splices.push(Splice { span: scan.elements[run].outer, replacement: xml });
    }
    splices
}

fn change_paragraph(source: &str, scan: &XmlScan, para: usize, change: &ParagraphChange) -> Splice {
    let prefix = prefix_of(&scan.elements[para].qname);
    match direct_child(scan, para, "pPr") {
        Some(ppr) => Splice { span: scan.elements[ppr].inner, replacement: ppr_inner(&prefix, children(source, scan, ppr), change) },
        None => {
            let inner = ppr_inner(&prefix, Vec::new(), change);
            let xml = format!("<{p}:pPr>{}</{p}:pPr>", inner, p = prefix);
            if scan.elements[para].outer.end == scan.elements[para].start_tag.end
                && scan.elements[para].start_tag.of(source).ends_with("/>")
            {
                let open = scan.elements[para].start_tag.of(source).trim_end_matches("/>").trim_end();
                Splice {
                    span: scan.elements[para].outer,
                    replacement: format!("{}>{}</{}>", open, xml, scan.elements[para].qname),
                }
            } else {
                let at = scan.elements[para].inner.start;
                Splice { span: Span { start: at, end: at }, replacement: xml }
            }
        }
    }
}

/// Append XML at the end of a paragraph, opening a self-closing `<w:p/>`.
fn append_to_paragraph(source: &str, scan: &XmlScan, para: usize, xml: &str) -> Splice {
    let el = &scan.elements[para];
    if el.outer.end == el.start_tag.end && el.start_tag.of(source).ends_with("/>") {
        let open = el.start_tag.of(source).trim_end_matches("/>").trim_end();
        return Splice { span: el.outer, replacement: format!("{}>{}</{}>", open, xml, el.qname) };
    }
    let at = el.inner.end;
    Splice { span: Span { start: at, end: at }, replacement: xml.to_string() }
}

fn insert_after(scan: &XmlScan, para: usize, xml: String) -> Splice {
    let end = scan.elements[para].outer.end;
    Splice { span: Span { start: end, end }, replacement: xml }
}

/// Delete a paragraph without leaving a table cell or the body invalid.
fn delete_paragraph(scan: &XmlScan, para: usize, addr: &str) -> Result<Vec<Splice>> {
    let parent = scan.elements[para].parent;
    if let Some(parent) = parent {
        let siblings = direct_children(scan, parent, "p");
        let local = scan.elements[parent].local.as_str();
        // A cell must end with a paragraph, and the body needs one before its
        // section properties; the last one is emptied instead of removed.
        if (local == "tc" || local == "body") && siblings.len() <= 1 {
            let p = prefix_of(&scan.elements[para].qname);
            return Ok(vec![Splice { span: scan.elements[para].outer, replacement: new_paragraph(&p, "") }]);
        }
    }
    let _ = addr;
    Ok(vec![Splice { span: scan.elements[para].outer, replacement: String::new() }])
}

fn split_paragraph(source: &str, scan: &XmlScan, para: usize, offset: usize) -> Vec<Splice> {
    let el = &scan.elements[para];
    let prefix = prefix_of(&el.qname);
    let ppr = direct_child(scan, para, "pPr");

    // The new paragraph keeps the formatting of this one, minus a section
    // break (which must exist once) and minus a heading style (Enter after a
    // heading starts body text, as in Word).
    let new_ppr = ppr.map(|pr| {
        let style = direct_child(scan, pr, "pStyle").and_then(|s| scan.elements[s].attr(source, "w:val"));
        let heading = style.map(|s| {
            let l = s.to_ascii_lowercase();
            l.starts_with("heading") || l.starts_with("title") || l.starts_with("subtitle")
        }).unwrap_or(false);
        let kids: Vec<(String, String)> = children(source, scan, pr)
            .into_iter()
            .filter(|(l, _)| l != "sectPr" && l != "pPrChange" && !(heading && l == "pStyle"))
            .collect();
        format!("<{p}:pPr>{}</{p}:pPr>", kids.into_iter().map(|(_, x)| x).collect::<String>(), p = prefix)
    });

    let mut splices = Vec::new();
    let mut moved = String::new();
    let mut cursor = 0usize;
    let mut last_rpr = String::new();
    let mut first_keeps_text = false;

    for run in para_runs(scan, para) {
        if is_math_run(scan, run) {
            continue;
        }
        let text = run_text(source, scan, run);
        let (rs, re) = (cursor, cursor + utf16_len(&text));
        cursor = re;
        let rpr_inner = direct_child(scan, run, "rPr").map(|r| scan.elements[r].inner.of(source).to_string()).unwrap_or_default();
        last_rpr = rpr_inner.clone();

        if re <= offset {
            first_keeps_text |= !text.is_empty() || !is_simple_run(scan, run);
            continue;
        }
        if rs >= offset {
            // Wholly after the split: moves to the new paragraph.
            moved.push_str(scan.elements[run].outer.of(source));
            splices.push(Splice { span: scan.elements[run].outer, replacement: String::new() });
            continue;
        }
        // The split falls inside this run.
        if is_simple_run(scan, run) {
            let (head, tail) = split_utf16(&text, offset - rs);
            first_keeps_text |= !head.is_empty();
            splices.push(Splice { span: scan.elements[run].outer, replacement: rebuild_run(source, scan, run, &rpr_inner, &head) });
            moved.push_str(&rebuild_run(source, scan, run, &rpr_inner, &tail));
        } else {
            first_keeps_text = true;
        }
    }

    let carry = |rpr: &str| {
        let r = if rpr.is_empty() { String::new() } else { format!("<{p}:rPr>{}</{p}:rPr>", rpr, p = prefix) };
        format!("<{p}:r>{}{}</{p}:r>", r, new_text_node(&prefix, ""), p = prefix)
    };
    if moved.is_empty() {
        moved = carry(&last_rpr);
    }
    if !first_keeps_text {
        // Keep the first paragraph typeable.
        splices.push(append_to_paragraph(source, scan, para, &carry(&last_rpr)));
    }

    let new_p = format!("<{p}:p>{}{}</{p}:p>", new_ppr.unwrap_or_default(), moved, p = prefix);
    splices.push(insert_after(scan, para, new_p));
    splices
}

fn merge_with_previous(source: &str, scan: &XmlScan, prev: usize, para: usize) -> Result<Vec<Splice>> {
    if scan.elements[prev].parent != scan.elements[para].parent {
        return Err(anyhow!(
            "These two paragraphs are in different places (a table sits between them), so they cannot be joined."
        ));
    }
    // Everything the paragraph holds except its own properties moves across.
    let content: String = children(source, scan, para)
        .into_iter()
        .filter(|(l, _)| l != "pPr")
        .map(|(_, x)| x)
        .collect();

    // The previous paragraph's empty placeholder runs would sit between the
    // two texts; drop them so the caret lands where the user expects.
    let mut splices = Vec::new();
    for run in para_runs(scan, prev) {
        if is_simple_run(scan, run) && run_text(source, scan, run).is_empty() && !content.is_empty() {
            splices.push(Splice { span: scan.elements[run].outer, replacement: String::new() });
        }
    }
    splices.push(append_to_paragraph(source, scan, prev, &content));
    splices.push(Splice { span: scan.elements[para].outer, replacement: String::new() });

    // If the previous paragraph was self-closing, the append replaced its whole
    // span, which a run removal cannot overlap because it had no runs.
    Ok(splices)
}

fn new_table(p: &str, rows: usize, cols: usize) -> Result<String> {
    if rows == 0 || cols == 0 || rows > 200 || cols > 30 {
        return Err(anyhow!("A table needs between 1 and 200 rows and 1 and 30 columns."));
    }
    let col_w = TABLE_WIDTH_TWIPS / cols;
    let border = |side: &str| format!("<{p}:{side} {p}:val=\"single\" {p}:sz=\"4\" {p}:space=\"0\" {p}:color=\"auto\"/>");
    let borders: String = ["top", "left", "bottom", "right", "insideH", "insideV"].iter().map(|s| border(s)).collect();
    let grid: String = (0..cols).map(|_| format!("<{p}:gridCol {p}:w=\"{col_w}\"/>")).collect();
    let row: String = format!(
        "<{p}:tr>{}</{p}:tr>",
        (0..cols).map(|_| new_cell(p, col_w)).collect::<String>()
    );
    Ok(format!(
        "<{p}:tbl><{p}:tblPr><{p}:tblW {p}:w=\"0\" {p}:type=\"auto\"/><{p}:tblBorders>{borders}</{p}:tblBorders>\
<{p}:tblLook {p}:val=\"04A0\"/></{p}:tblPr><{p}:tblGrid>{grid}</{p}:tblGrid>{}</{p}:tbl>{}",
        row.repeat(rows),
        // A table must be followed by a paragraph (inside a cell it is required).
        new_paragraph(p, "")
    ))
}

fn new_cell(p: &str, width: usize) -> String {
    format!(
        "<{p}:tc><{p}:tcPr><{p}:tcW {p}:w=\"{width}\" {p}:type=\"dxa\"/></{p}:tcPr>{}</{p}:tc>",
        new_paragraph(p, "")
    )
}

/// (cell, row, table) enclosing a paragraph.
fn table_of(scan: &XmlScan, para: usize, addr: &str) -> Result<(usize, usize, usize)> {
    let tc = ancestor_with_local(scan, para, "tc").ok_or_else(|| anyhow!("'{}' is not inside a table.", addr))?;
    let tr = ancestor_with_local(scan, tc, "tr").ok_or_else(|| anyhow!("'{}' is not inside a table row.", addr))?;
    let tbl = ancestor_with_local(scan, tr, "tbl").ok_or_else(|| anyhow!("'{}' is not inside a table.", addr))?;
    Ok((tc, tr, tbl))
}

fn cell_width(source: &str, scan: &XmlScan, tc: usize) -> usize {
    scan.first_descendant(tc, "tcW")
        .and_then(|w| scan.elements[w].attr(source, "w:w"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000)
}

fn insert_table_row(source: &str, scan: &XmlScan, para: usize, below: bool, addr: &str) -> Result<Splice> {
    let (_, tr, _) = table_of(scan, para, addr)?;
    let p = prefix_of(&scan.elements[tr].qname);
    let cells: String = direct_children(scan, tr, "tc").iter().map(|&tc| new_cell(&p, cell_width(source, scan, tc))).collect();
    let row = format!("<{p}:tr>{cells}</{p}:tr>");
    let at = if below { scan.elements[tr].outer.end } else { scan.elements[tr].outer.start };
    Ok(Splice { span: Span { start: at, end: at }, replacement: row })
}

fn insert_table_column(source: &str, scan: &XmlScan, para: usize, right: bool, addr: &str) -> Result<Vec<Splice>> {
    let (tc, tr, tbl) = table_of(scan, para, addr)?;
    let p = prefix_of(&scan.elements[tbl].qname);
    let col = direct_children(scan, tr, "tc").iter().position(|&c| c == tc).unwrap_or(0);
    let width = cell_width(source, scan, tc);

    let mut splices = Vec::new();
    for row in direct_children(scan, tbl, "tr") {
        let cells = direct_children(scan, row, "tc");
        let Some(&target) = cells.get(col.min(cells.len().saturating_sub(1))) else { continue };
        let at = if right { scan.elements[target].outer.end } else { scan.elements[target].outer.start };
        splices.push(Splice { span: Span { start: at, end: at }, replacement: new_cell(&p, width) });
    }
    if let Some(grid) = direct_child(scan, tbl, "tblGrid") {
        let cols = direct_children(scan, grid, "gridCol");
        let new_col = format!("<{p}:gridCol {p}:w=\"{width}\"/>");
        match cols.get(col) {
            Some(&g) => {
                let at = if right { scan.elements[g].outer.end } else { scan.elements[g].outer.start };
                splices.push(Splice { span: Span { start: at, end: at }, replacement: new_col });
            }
            None => {
                let at = scan.elements[grid].inner.end;
                splices.push(Splice { span: Span { start: at, end: at }, replacement: new_col });
            }
        }
    }
    Ok(splices)
}

fn delete_table_column(scan: &XmlScan, para: usize, addr: &str) -> Result<Vec<Splice>> {
    let (tc, tr, tbl) = table_of(scan, para, addr)?;
    let col = direct_children(scan, tr, "tc").iter().position(|&c| c == tc).unwrap_or(0);
    if direct_children(scan, tr, "tc").len() <= 1 {
        return Ok(delete_table(scan, tbl));
    }
    let mut splices = Vec::new();
    for row in direct_children(scan, tbl, "tr") {
        let cells = direct_children(scan, row, "tc");
        // A row with fewer cells (merged) keeps at least one.
        if cells.len() > 1 {
            if let Some(&target) = cells.get(col) {
                splices.push(Splice { span: scan.elements[target].outer, replacement: String::new() });
            }
        }
    }
    if let Some(grid) = direct_child(scan, tbl, "tblGrid") {
        if let Some(&g) = direct_children(scan, grid, "gridCol").get(col) {
            splices.push(Splice { span: scan.elements[g].outer, replacement: String::new() });
        }
    }
    Ok(splices)
}

fn delete_table(scan: &XmlScan, tbl: usize) -> Vec<Splice> {
    // Inside a cell the table's removal could leave the cell ending without a
    // paragraph; the paragraph `new_table` wrote after it covers that case.
    vec![Splice { span: scan.elements[tbl].outer, replacement: String::new() }]
}

// ---------------------------------------------------------------- styles

const REL_STYLES: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
const CT_STYLES: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml";

/// The built-in paragraph styles the editor offers, as Word defines them
/// closely enough that a document opened in Word looks the same.
fn builtin_style(id: &str) -> Option<String> {
    let para = |id: &str, name: &str, ppr: &str, rpr: &str| {
        format!(
            "<w:style w:type=\"paragraph\" w:styleId=\"{id}\"><w:name w:val=\"{name}\"/><w:basedOn w:val=\"Normal\"/>\
<w:next w:val=\"Normal\"/><w:qFormat/><w:pPr>{ppr}</w:pPr><w:rPr>{rpr}</w:rPr></w:style>"
        )
    };
    Some(match id {
        "Normal" => "<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/>\
<w:pPr><w:spacing w:after=\"160\" w:line=\"259\" w:lineRule=\"auto\"/></w:pPr></w:style>"
            .to_string(),
        "Title" => para("Title", "Title", "<w:spacing w:after=\"80\"/>", "<w:sz w:val=\"56\"/><w:szCs w:val=\"56\"/>"),
        "Subtitle" => para("Subtitle", "Subtitle", "<w:spacing w:after=\"160\"/>", "<w:color w:val=\"595959\"/><w:sz w:val=\"30\"/><w:szCs w:val=\"30\"/>"),
        "Heading1" => para(
            "Heading1",
            "heading 1",
            "<w:keepNext/><w:spacing w:before=\"360\" w:after=\"80\"/><w:outlineLvl w:val=\"0\"/>",
            "<w:b/><w:color w:val=\"1F3864\"/><w:sz w:val=\"32\"/><w:szCs w:val=\"32\"/>",
        ),
        "Heading2" => para(
            "Heading2",
            "heading 2",
            "<w:keepNext/><w:spacing w:before=\"160\" w:after=\"80\"/><w:outlineLvl w:val=\"1\"/>",
            "<w:b/><w:color w:val=\"1F3864\"/><w:sz w:val=\"26\"/><w:szCs w:val=\"26\"/>",
        ),
        "Heading3" => para(
            "Heading3",
            "heading 3",
            "<w:keepNext/><w:spacing w:before=\"160\" w:after=\"80\"/><w:outlineLvl w:val=\"2\"/>",
            "<w:b/><w:sz w:val=\"24\"/><w:szCs w:val=\"24\"/>",
        ),
        "Quote" => para("Quote", "Quote", "<w:spacing w:before=\"200\" w:after=\"160\"/><w:ind w:left=\"864\" w:right=\"864\"/><w:jc w:val=\"center\"/>", "<w:i/><w:color w:val=\"404040\"/>"),
        _ => return None,
    })
}

pub const BUILTIN_STYLE_IDS: &[&str] = &["Normal", "Title", "Subtitle", "Heading1", "Heading2", "Heading3", "Quote"];

/// A complete `word/styles.xml` holding the built-in set, for new documents.
pub fn default_styles_part() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:styles xmlns:w=\"{W_NS}\">\
<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii=\"Calibri\" w:hAnsi=\"Calibri\" w:cs=\"Calibri\" w:eastAsia=\"Calibri\"/>\
<w:sz w:val=\"22\"/><w:szCs w:val=\"22\"/><w:lang w:val=\"en-US\"/></w:rPr></w:rPrDefault><w:pPrDefault/></w:docDefaults>{}</w:styles>",
        BUILTIN_STYLE_IDS.iter().filter_map(|id| builtin_style(id)).collect::<String>()
    )
}

/// Make sure a built-in style used by the document is defined in it, so a
/// "Heading 1" chosen here is a heading in Word too. Unknown ids are left to
/// the document (they are its own styles).
fn ensure_style(pkg: &mut OoxmlPackage, id: &str) -> Result<()> {
    let Some(definition) = builtin_style(id) else { return Ok(()) };
    let rels = pkg.part_str(DOCUMENT_RELS_PART).map(|s| s.to_string()).unwrap_or_default();
    let linked = scan(&rels)
        .ok()
        .and_then(|s| {
            s.by_local("Relationship").into_iter().find_map(|r| {
                let el = &s.elements[r];
                (el.attr(&rels, "Type").as_deref() == Some(REL_STYLES)).then(|| el.attr(&rels, "Target")).flatten()
            })
        })
        .map(|t| resolve_target("word", &t));

    let part = match linked {
        Some(p) if pkg.has_part(&p) => p,
        _ => {
            if !pkg.has_part(STYLES_PART) {
                pkg.add_part(STYLES_PART, default_styles_part().into_bytes())?;
            }
            pkg.ensure_override_content_type(STYLES_PART, CT_STYLES)?;
            pkg.add_relationship(DOCUMENT_RELS_PART, REL_STYLES, "styles.xml")?;
            return Ok(());
        }
    };

    let source = pkg.part_str(&part)?.to_string();
    if source.contains(&format!("w:styleId=\"{}\"", id)) {
        return Ok(());
    }
    let s = scan(&source)?;
    let root = s.by_local("styles").into_iter().next().ok_or_else(|| anyhow!("styles.xml has no root"))?;
    let at = s.elements[root].inner.end;
    // A document whose styles use a prefix other than `w` gets the definition
    // with its own prefix.
    let prefix = prefix_of(&s.elements[root].qname);
    let definition = if prefix == "w" { definition } else { definition.replace("<w:", &format!("<{}:", prefix)).replace("</w:", &format!("</{}:", prefix)).replace(" w:", &format!(" {}:", prefix)) };
    let patched = apply_splices(&source, vec![Splice { span: Span { start: at, end: at }, replacement: definition }])?;
    pkg.replace_part(&part, patched.into_bytes())
}

fn ensure_numbering(pkg: &mut OoxmlPackage) -> Result<()> {
    let part = match numbering_part(pkg) {
        Some(p) if pkg.has_part(&p) => p,
        _ => {
            let part = "word/numbering.xml".to_string();
            if !pkg.has_part(&part) {
                pkg.add_part(
                    &part,
                    format!(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:numbering xmlns:w=\"{}\"></w:numbering>",
                        W_NS
                    )
                    .into_bytes(),
                )?;
            }
            pkg.ensure_override_content_type(&part, CT_NUMBERING)?;
            pkg.add_relationship(DOCUMENT_RELS_PART, REL_NUMBERING, "numbering.xml")?;
            part
        }
    };

    let source = pkg.part_str(&part)?.to_string();
    if source.contains(&format!("abstractNumId=\"{}\"", BULLET_NUM_ID)) {
        return Ok(());
    }
    let s = scan(&source)?;
    let root = s.by_local("numbering").into_iter().next().ok_or_else(|| anyhow!("numbering.xml has no root"))?;
    let p = prefix_of(&s.elements[root].qname);

    let level = |i: usize, bullet: bool| {
        let (fmt, text) = if bullet {
            ("bullet", ["•", "◦", "▪"][i % 3].to_string())
        } else {
            (["decimal", "lowerLetter", "lowerRoman"][i % 3], format!("%{}.", i + 1))
        };
        format!(
            "<{p}:lvl {p}:ilvl=\"{i}\"><{p}:start {p}:val=\"1\"/><{p}:numFmt {p}:val=\"{fmt}\"/>\
<{p}:lvlText {p}:val=\"{text}\"/><{p}:lvlJc {p}:val=\"left\"/><{p}:pPr><{p}:ind {p}:left=\"{}\" {p}:hanging=\"360\"/></{p}:pPr></{p}:lvl>",
            720 * (i + 1)
        )
    };
    let abstract_num = |id: u32, bullet: bool| {
        format!(
            "<{p}:abstractNum {p}:abstractNumId=\"{id}\"><{p}:multiLevelType {p}:val=\"hybridMultilevel\"/>{}</{p}:abstractNum>",
            (0..9).map(|i| level(i, bullet)).collect::<String>()
        )
    };
    let abstracts = format!("{}{}", abstract_num(BULLET_NUM_ID, true), abstract_num(NUMBER_NUM_ID, false));
    let nums = format!(
        "<{p}:num {p}:numId=\"{b}\"><{p}:abstractNumId {p}:val=\"{b}\"/></{p}:num><{p}:num {p}:numId=\"{n}\"><{p}:abstractNumId {p}:val=\"{n}\"/></{p}:num>",
        b = BULLET_NUM_ID,
        n = NUMBER_NUM_ID
    );

    // Schema order: every abstractNum before every num, and numIdMacAtCleanup last.
    let first_num = direct_children(&s, root, "num").first().map(|&n| s.elements[n].outer.start);
    let cleanup = direct_child(&s, root, "numIdMacAtCleanup").map(|c| s.elements[c].outer.start);
    let abs_at = first_num.or(cleanup).unwrap_or(s.elements[root].inner.end);
    let num_at = cleanup.unwrap_or(s.elements[root].inner.end);

    let patched = apply_splices(
        &source,
        vec![
            Splice { span: Span { start: abs_at, end: abs_at }, replacement: abstracts },
            Splice { span: Span { start: num_at, end: num_at }, replacement: nums },
        ],
    )?;
    pkg.replace_part(&part, patched.into_bytes())
}

fn insert_image(
    pkg: &mut OoxmlPackage,
    p: &str,
    data_base64: &str,
    file_name: &str,
    width_emu: i64,
    height_emu: i64,
    doc_pr_id: u64,
) -> Result<String> {
    let data = data_base64.split_once(',').map(|(_, d)| d).unwrap_or(data_base64);
    let bytes = BASE64.decode(data.trim()).map_err(|e| anyhow!("That image could not be read: {}", e))?;
    let mut ext = file_name.rsplit('.').next().unwrap_or("png").to_ascii_lowercase();
    if ext == "jpeg" {
        ext = "jpg".into();
    }
    let mime = mime_for(&ext).ok_or_else(|| anyhow!("'{}' is not a picture format Word can embed (PNG, JPEG, GIF, BMP).", file_name))?;

    let part = pkg.unused_part_name("word/media", "counsel_image", &ext);
    pkg.add_part(&part, bytes)?;
    pkg.ensure_default_content_type(&ext, mime)?;
    let target = part.trim_start_matches("word/");
    let rid = pkg.add_relationship(&rels_part_for(DOCUMENT_PART), REL_IMAGE, target)?;

    let (mut cx, mut cy) = (width_emu.max(1), height_emu.max(1));
    if cx > MAX_IMAGE_WIDTH_EMU {
        cy = cy * MAX_IMAGE_WIDTH_EMU / cx;
        cx = MAX_IMAGE_WIDTH_EMU;
    }
    let name = escape_attr(file_name);
    Ok(format!(
        "<{p}:p><{p}:r><{p}:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\">\
<wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:docPr id=\"{doc_pr_id}\" name=\"Picture {doc_pr_id}\" descr=\"{name}\"/>\
<wp:cNvGraphicFramePr><a:graphicFrameLocks xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" noChangeAspect=\"1\"/></wp:cNvGraphicFramePr>\
<a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
<a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:nvPicPr><pic:cNvPr id=\"0\" name=\"{name}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip r:embed=\"{rid}\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"/>\
<a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr>\
</pic:pic></a:graphicData></a:graphic></wp:inline></{p}:drawing></{p}:r></{p}:p>{}",
        new_paragraph(p, "")
    ))
}

fn set_image_size(source: &str, scan: &XmlScan, run: usize, cx: i64, cy: i64, addr: &str) -> Result<Vec<Splice>> {
    if cx <= 0 || cy <= 0 {
        return Err(anyhow!("A picture cannot have a zero or negative size."));
    }
    let mut splices = Vec::new();
    for local in ["extent", "ext"] {
        for el in scan.descendants_by_local(run, local) {
            let e = &scan.elements[el];
            // Only the size-carrying elements: `a:ext` inside `a:xfrm`, and `wp:extent`.
            if local == "ext" && scan.elements[e.parent.unwrap_or(el)].local != "xfrm" {
                continue;
            }
            let tag = e.start_tag.of(source);
            let rewritten = replace_attr(&replace_attr(tag, "cx", &cx.to_string()), "cy", &cy.to_string());
            splices.push(Splice { span: e.start_tag, replacement: rewritten });
        }
    }
    if splices.is_empty() {
        return Err(anyhow!("'{}' is not a picture.", addr));
    }
    Ok(splices)
}

fn replace_attr(tag: &str, name: &str, value: &str) -> String {
    let needle = format!(" {}=\"", name);
    match tag.find(&needle) {
        Some(at) => {
            let from = at + needle.len();
            let end = tag[from..].find('"').map(|e| from + e).unwrap_or(from);
            format!("{}{}{}", &tag[..from], value, &tag[end..])
        }
        None => tag.to_string(),
    }
}

/// Always writes `xml:space="preserve"`. Without it Word collapses leading and
/// trailing spaces, so "Section 2. " silently loses its trailing space and the
/// next run runs into it.
fn new_text_node(prefix: &str, text: &str) -> String {
    format!("<{p}:t xml:space=\"preserve\">{}</{p}:t>", escape(text), p = prefix)
}

fn new_paragraph(prefix: &str, text: &str) -> String {
    format!("<{p}:p><{p}:r>{}</{p}:r></{p}:p>", new_text_node(prefix, text), p = prefix)
}

fn prefix_of(qname: &str) -> String {
    qname.split_once(':').map(|(p, _)| p.to_string()).unwrap_or_else(|| "w".into())
}

// ---------------------------------------------------------------- addressing

fn parse_paragraph_addr(addr: &str) -> Result<usize> {
    index_in(addr, "body/p[").ok_or_else(|| bad_addr(addr, "body/p[12]"))
}

fn parse_run_addr(addr: &str) -> Result<(usize, usize)> {
    let (p_part, r_part) = addr.split_once("/r[").ok_or_else(|| bad_addr(addr, "body/p[12]/r[0]"))?;
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
    let runs = para_runs(scan, para);
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

    fn text(b: &Block) -> String {
        b.runs.iter().map(|r| r.text.as_str()).collect()
    }

    fn bold() -> TextFormat {
        TextFormat { bold: Some(true), ..Default::default() }
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
            &[DraftPatch::SetRunText { addr: "body/p[0]/r[0]".into(), text: "Amended and Restated Agreement".into() }],
        )
        .unwrap();

        let before = OoxmlPackage::open(&original).unwrap();
        let after = OoxmlPackage::open(&saved).unwrap();
        for name in before.part_names() {
            if name == DOCUMENT_PART {
                continue;
            }
            assert_eq!(before.part(name).unwrap(), after.part(name).unwrap(), "part '{}' must be untouched by a text edit", name);
        }
        assert_eq!(blocks(&saved)[0].runs[0].text, "Amended and Restated Agreement");
    }

    #[test]
    fn text_with_markup_characters_survives_a_round_trip() {
        let tricky = "Clauses 3 & 4 <indemnity> apply \"as-is\"";
        let saved = apply(&fixtures::docx(), &[DraftPatch::SetRunText { addr: "body/p[0]/r[0]".into(), text: tricky.into() }]).unwrap();
        assert_eq!(blocks(&saved)[0].runs[0].text, tricky);
    }

    #[test]
    fn leading_and_trailing_spaces_are_preserved() {
        let saved = apply(&fixtures::docx(), &[DraftPatch::SetRunText { addr: "body/p[0]/r[0]".into(), text: "  spaced  ".into() }]).unwrap();
        assert_eq!(blocks(&saved)[0].runs[0].text, "  spaced  ");
    }

    #[test]
    fn bold_can_be_added_to_a_run_that_had_no_properties_at_all() {
        let saved = apply(&fixtures::docx(), &[DraftPatch::SetRunFormat { addr: "body/p[1]/r[0]".into(), format: bold() }]).unwrap();
        let r = &blocks(&saved)[1].runs[0];
        assert!(r.bold);
        assert_eq!(r.text, "This Agreement is entered into as of ");
    }

    #[test]
    fn turning_bold_off_removes_it_without_disturbing_the_text() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunFormat { addr: "body/p[1]/r[1]".into(), format: TextFormat { bold: Some(false), ..Default::default() } }],
        )
        .unwrap();
        let r = &blocks(&saved)[1].runs[1];
        assert!(!r.bold);
        assert_eq!(r.text, "1 January 2026");
    }

    #[test]
    fn an_unspecified_format_property_keeps_its_current_value() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunFormat { addr: "body/p[1]/r[1]".into(), format: TextFormat { italic: Some(true), ..Default::default() } }],
        )
        .unwrap();
        let r = &blocks(&saved)[1].runs[1];
        assert!(r.bold, "bold was not mentioned, so it must survive");
        assert!(r.italic);
    }

    #[test]
    fn font_size_colour_and_highlight_are_written_and_read_back() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetRunFormat {
                addr: "body/p[1]/r[0]".into(),
                format: TextFormat {
                    font: Some("Georgia".into()),
                    size: Some(14.0),
                    color: Some("#C00000".into()),
                    highlight: Some("yellow".into()),
                    strike: Some(true),
                    ..Default::default()
                },
            }],
        )
        .unwrap();
        let r = &blocks(&saved)[1].runs[0];
        assert_eq!(r.font.as_deref(), Some("Georgia"));
        assert_eq!(r.size, Some(14.0));
        assert_eq!(r.color.as_deref(), Some("C00000"));
        assert_eq!(r.highlight.as_deref(), Some("yellow"));
        assert!(r.strike);
    }

    #[test]
    fn formatting_a_range_splits_the_run_and_formats_only_the_selection() {
        // "This Agreement is entered into as of " — bold "Agreement".
        let original = fixtures::docx();
        let whole = text(&blocks(&original)[1]);
        let saved = apply(
            &original,
            &[DraftPatch::FormatRange { addr: "body/p[1]".into(), start: 5, end: 14, format: bold() }],
        )
        .unwrap();
        let b = &blocks(&saved)[1];
        assert_eq!(text(b), whole, "formatting never changes the text");
        let agreement = b.runs.iter().find(|r| r.text == "Agreement").expect("the selection became its own run");
        assert!(agreement.bold);
        assert!(!b.runs[0].bold, "text before the selection is untouched");
    }

    #[test]
    fn splitting_and_merging_a_paragraph_round_trips_its_text() {
        let original = fixtures::docx();
        let before = blocks(&original);
        let whole = text(&before[1]);

        let split = apply(&original, &[DraftPatch::SplitParagraph { addr: "body/p[1]".into(), offset: 5 }]).unwrap();
        let after = blocks(&split);
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(text(&after[1]), "This ");
        assert_eq!(format!("{}{}", text(&after[1]), text(&after[2])), whole);

        let merged = apply(&split, &[DraftPatch::MergeParagraphWithPrevious { addr: "body/p[2]".into() }]).unwrap();
        let back = blocks(&merged);
        assert_eq!(back.len(), before.len());
        assert_eq!(text(&back[1]), whole);
    }

    #[test]
    fn a_bullet_list_creates_numbering_definitions_and_reads_as_a_list() {
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::SetParagraphFormat {
                addr: "body/p[2]".into(),
                align: Some("center".into()),
                list: Some("bullet".into()),
                indent: None,
                line_spacing: None,
            }],
        )
        .unwrap();
        let b = &blocks(&saved)[2];
        assert_eq!(b.list.as_deref(), Some("bullet"));
        assert_eq!(b.align.as_deref(), Some("center"));

        let indented = apply(
            &saved,
            &[DraftPatch::SetParagraphFormat { addr: "body/p[2]".into(), align: None, list: None, indent: Some(1), line_spacing: None }],
        )
        .unwrap();
        assert_eq!(blocks(&indented)[2].level, 1);
    }

    #[test]
    fn a_table_can_be_inserted_grown_and_shrunk() {
        let original = fixtures::docx();
        let count = blocks(&original).len();
        let saved = apply(&original, &[DraftPatch::InsertTable { addr: "body/p[0]".into(), rows: 2, cols: 3 }]).unwrap();
        let b = blocks(&saved);
        // 6 cell paragraphs plus the trailing paragraph.
        assert_eq!(b.len(), count + 7);
        assert!(b[1].cell.is_some());

        let grown = apply(&saved, &[DraftPatch::InsertTableRow { addr: "body/p[1]".into(), below: true }]).unwrap();
        assert_eq!(blocks(&grown).len(), count + 10);
        let wider = apply(&grown, &[DraftPatch::InsertTableColumn { addr: "body/p[1]".into(), right: true }]).unwrap();
        assert_eq!(blocks(&wider).len(), count + 13);
        let gone = apply(&wider, &[DraftPatch::DeleteTable { addr: "body/p[1]".into() }]).unwrap();
        assert_eq!(blocks(&gone).len(), count + 1);
    }

    #[test]
    fn an_image_is_embedded_as_a_new_part_and_read_back() {
        let png = BASE64.encode(fixtures::PNG_1X1);
        let saved = apply(
            &fixtures::docx(),
            &[DraftPatch::InsertImage {
                addr: "body/p[0]".into(),
                data_base64: png,
                file_name: "seal.png".into(),
                width_emu: 914_400,
                height_emu: 914_400,
                x: None,
                y: None,
            }],
        )
        .unwrap();
        let pkg = OoxmlPackage::open(&saved).unwrap();
        assert!(pkg.part_names().iter().any(|n| n.starts_with("word/media/counsel_image")));
        let b = blocks(&saved);
        let img = b[1].runs.iter().find_map(|r| r.image.clone()).expect("the picture is in the model");
        assert!(img.data_url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn an_equation_is_written_as_office_math_and_read_back_as_notation() {
        let saved = apply(&fixtures::docx(), &[DraftPatch::InsertEquation { addr: "body/p[0]".into(), text: "\\frac{a}{b}".into() }]).unwrap();
        assert_eq!(blocks(&saved)[1].equation.as_deref(), Some("\\frac{a}{b}"));
    }

    #[test]
    fn a_heading_applied_to_a_blank_document_is_defined_in_its_styles() {
        let blank = crate::document_workspace::ooxml::blank::docx().unwrap();
        let model = build(&blank, 1).unwrap();
        let DraftViewModel::Docx { styles, defaults, .. } = model else { panic!() };
        assert!(styles.iter().any(|s| s.id == "Heading1"), "blank documents ship the built-in styles");
        assert_eq!(defaults.font.as_deref(), Some("Calibri"));

        // A document with no styles part at all gets one on first use.
        let saved = apply(&fixtures::docx(), &[DraftPatch::SetParagraphStyle { addr: "body/p[2]".into(), style: Some("Quote".into()) }]).unwrap();
        let DraftViewModel::Docx { styles, blocks, .. } = build(&saved, 2).unwrap() else { panic!() };
        assert!(styles.iter().any(|s| s.id == "Quote"));
        assert_eq!(blocks[2].style.as_deref(), Some("Quote"));
    }

    #[test]
    fn inserting_and_deleting_paragraphs_shifts_the_addresses_as_expected() {
        let original = fixtures::docx();
        let before = blocks(&original).len();
        let saved = apply(&original, &[DraftPatch::InsertParagraphAfter { addr: "body/p[0]".into(), text: "Recitals".into() }]).unwrap();
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

    #[test]
    fn an_out_of_range_address_is_refused_with_advice() {
        let err = apply(&fixtures::docx(), &[DraftPatch::SetRunText { addr: "body/p[99]/r[0]".into(), text: "x".into() }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no longer exists"), "{}", err);
        assert!(err.contains("Reopen the draft"), "{}", err);
    }

    #[test]
    fn a_patch_meant_for_another_format_is_refused_before_anything_is_written() {
        let err = apply(&fixtures::docx(), &[DraftPatch::SetCellValue { addr: "sheet[0]/A1".into(), value: "1".into() }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("SetCellValue"), "{}", err);
        assert!(err.contains("has not been changed"), "{}", err);
    }
}
