//! XLSX: read sheets into a cell map, and splice cell edits back into the
//! individual worksheet parts.
//!
//! # Addressing
//!
//! `sheet[0]/B7` — the sheet's position in the workbook's own sheet order, then
//! the cell reference Excel itself uses. Nothing is invented.
//!
//! # Why writes never touch `sharedStrings.xml`
//!
//! Excel stores most text once in a shared table and has cells point at it by
//! index. Appending to that table means rewriting its `count` and
//! `uniqueCount`, and removing from it would renumber every later index — so a
//! single text edit could silently corrupt unrelated cells across every sheet.
//!
//! Instead a text edit is written as an **inline string** (`t="inlineStr"`),
//! which is fully valid OOXML and entirely local to the one cell. The shared
//! table stays byte-identical, and cells we did not touch keep pointing exactly
//! where they did.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::package::{read_relationships, resolve_target, OoxmlPackage};
use super::spans::{apply_splices, escape, escape_attr, scan, unescape, Span, Splice, XmlScan};
use crate::document_workspace::view_model::{wrong_format, DraftPatch, DraftViewModel, TextFormat};

const WORKBOOK_PART: &str = "xl/workbook.xml";
const WORKBOOK_RELS_PART: &str = "xl/_rels/workbook.xml.rels";
const SHARED_STRINGS_PART: &str = "xl/sharedStrings.xml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sheet {
    pub index: usize,
    pub name: String,
    /// Package part this sheet lives in — useful in logs and error messages.
    pub part: String,
    pub rows: usize,
    pub cols: usize,
    pub cells: HashMap<String, Cell>,
    pub merges: Vec<String>,
    /// Column widths in Excel character units, for columns that set one.
    #[serde(default)]
    pub columns: Vec<ColumnWidth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnWidth {
    /// 1-based, inclusive, as Excel stores them.
    pub min: usize,
    pub max: usize,
    pub width: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cell {
    pub addr: String,
    /// The displayable value. For a formula cell this is Excel's own cached
    /// result — see the note on `formula`.
    pub value: String,
    /// Formula text without the leading `=`, when the cell has one.
    pub formula: Option<String>,
    /// Style index into `cellXfs`, so the UI can apply number formats.
    pub style: Option<u32>,
    pub numeric: bool,
    /// The resolved look of the cell, when its style is not the default.
    #[serde(default)]
    pub format: Option<CellFormat>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CellFormat {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub font: Option<String>,
    pub size: Option<f32>,
    /// `RRGGBB`.
    pub color: Option<String>,
    /// `RRGGBB`.
    pub fill: Option<String>,
    pub align: Option<String>,
    pub wrap: bool,
    pub num_fmt: Option<String>,
}

// ---------------------------------------------------------------- reading

pub fn build(bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    let pkg = OoxmlPackage::open(bytes)?;
    let shared = read_shared_strings(&pkg)?;
    let formats = styles_part(&pkg)
        .and_then(|p| pkg.part_str(&p).ok().map(read_cell_formats))
        .unwrap_or_default();
    let mut sheets = Vec::new();

    for (index, (name, part)) in sheet_index(&pkg)?.into_iter().enumerate() {
        let source = pkg.part_str(&part)?;
        let mut sheet = read_sheet(index, &name, &part, source, &shared)?;
        for cell in sheet.cells.values_mut() {
            cell.format = cell
                .style
                .and_then(|s| formats.get(s as usize).cloned())
                .filter(|f| *f != CellFormat::default());
        }
        sheets.push(sheet);
    }

    if sheets.is_empty() {
        return Err(anyhow!("This workbook contains no worksheets."));
    }
    debug!("XLSX view model: {} sheets", sheets.len());
    Ok(DraftViewModel::Xlsx { version, sheets })
}

/// Sheet display names paired with their part paths, in workbook order.
///
/// The order comes from `workbook.xml`, and the part path from the relationship
/// the sheet points at. Guessing `sheet{n}.xml` from the position is a common
/// shortcut and it is wrong: Excel reuses and renumbers those files as sheets
/// are added and deleted, so `sheet3.xml` is routinely the second tab.
fn sheet_index(pkg: &OoxmlPackage) -> Result<Vec<(String, String)>> {
    let wb = pkg.part_str(WORKBOOK_PART)?;
    let rels = pkg.part_str(WORKBOOK_RELS_PART)?;
    let rel_targets = read_relationships(rels)?;

    let wb_scan = scan(wb)?;
    let mut out = Vec::new();
    for &s in wb_scan.by_local("sheet").iter() {
        let el = &wb_scan.elements[s];
        let name = el.attr(wb, "name").unwrap_or_else(|| format!("Sheet{}", out.len() + 1));
        let rid = el
            .attr(wb, "r:id")
            .ok_or_else(|| anyhow!("sheet '{}' has no relationship id", name))?;
        let target = rel_targets
            .get(&rid)
            .ok_or_else(|| anyhow!("sheet '{}' points at missing relationship {}", name, rid))?;
        out.push((name, resolve_target("xl", target)));
    }
    Ok(out)
}

fn read_shared_strings(pkg: &OoxmlPackage) -> Result<Vec<String>> {
    let Ok(source) = pkg.part_str(SHARED_STRINGS_PART) else {
        return Ok(Vec::new());
    };
    let s = scan(source)?;
    let mut out = Vec::new();
    for &si in s.by_local("si").iter() {
        // A shared string can be split across several `t` runs when parts of it
        // are formatted differently; the value is their concatenation.
        let text: String = s
            .descendants_by_local(si, "t")
            .iter()
            .map(|&t| unescape(s.elements[t].inner.of(source)))
            .collect();
        out.push(text);
    }
    Ok(out)
}

fn read_sheet(
    index: usize,
    name: &str,
    part: &str,
    source: &str,
    shared: &[String],
) -> Result<Sheet> {
    let s = scan(source)?;
    let mut cells = HashMap::new();
    let (mut max_row, mut max_col) = (0usize, 0usize);

    for &c in s.by_local("c").iter() {
        let el = &s.elements[c];
        let Some(reference) = el.attr(source, "r") else { continue };
        let (col, row) = parse_ref(&reference)?;
        max_row = max_row.max(row);
        max_col = max_col.max(col + 1);

        let kind = el.attr(source, "t").unwrap_or_default();
        let style = el.attr(source, "s").and_then(|v| v.parse().ok());

        let formula = s
            .first_descendant(c, "f")
            .map(|f| unescape(s.elements[f].inner.of(source)));

        let raw = s
            .first_descendant(c, "v")
            .map(|v| unescape(s.elements[v].inner.of(source)))
            .unwrap_or_default();

        let value = match kind.as_str() {
            "s" => raw
                .parse::<usize>()
                .ok()
                .and_then(|i| shared.get(i).cloned())
                // A dangling shared-string index means the file is
                // inconsistent; showing the raw index beats showing nothing.
                .unwrap_or(raw.clone()),
            "inlineStr" => s
                .first_descendant(c, "is")
                .map(|is| {
                    s.descendants_by_local(is, "t")
                        .iter()
                        .map(|&t| unescape(s.elements[t].inner.of(source)))
                        .collect()
                })
                .unwrap_or_default(),
            "b" => if raw == "1" { "TRUE".into() } else { "FALSE".into() },
            _ => raw.clone(),
        };

        let numeric = kind.is_empty() && !raw.is_empty() && raw.parse::<f64>().is_ok();
        cells.insert(
            reference.clone(),
            Cell { addr: format!("sheet[{}]/{}", index, reference), value, formula, style, numeric, format: None },
        );
    }

    let merges = s
        .by_local("mergeCell")
        .iter()
        .filter_map(|&m| s.elements[m].attr(source, "ref"))
        .collect();

    let columns = s
        .by_local("col")
        .iter()
        .filter_map(|&c| {
            let el = &s.elements[c];
            Some(ColumnWidth {
                min: el.attr(source, "min")?.parse().ok()?,
                max: el.attr(source, "max")?.parse().ok()?,
                width: el.attr(source, "width")?.parse().ok()?,
            })
        })
        .collect();

    Ok(Sheet {
        index,
        name: name.to_string(),
        part: part.to_string(),
        rows: max_row,
        cols: max_col,
        cells,
        merges,
        columns,
    })
}

// ---------------------------------------------------------------- styles

const REL_STYLES: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
const CT_STYLES: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml";

/// The stylesheet's children in schema order; a missing one is inserted after
/// the last present predecessor.
const STYLESHEET_ORDER: &[&str] = &[
    "numFmts", "fonts", "fills", "borders", "cellStyleXfs", "cellXfs", "cellStyles", "dxfs",
    "tableStyles", "colors", "extLst",
];
const FONT_ORDER: &[&str] = &[
    "b", "i", "strike", "condense", "extend", "outline", "shadow", "u", "vertAlign", "sz", "color",
    "name", "family", "charset", "scheme",
];

/// Excel's built-in number formats that have no `numFmt` element.
const BUILTIN_NUM_FMTS: &[(u32, &str)] = &[
    (0, "General"), (1, "0"), (2, "0.00"), (3, "#,##0"), (4, "#,##0.00"), (9, "0%"),
    (10, "0.00%"), (11, "0.00E+00"), (12, "# ?/?"), (14, "m/d/yyyy"), (15, "d-mmm-yy"),
    (16, "d-mmm"), (17, "mmm-yy"), (18, "h:mm AM/PM"), (20, "h:mm"), (22, "m/d/yyyy h:mm"),
    (37, "#,##0 ;(#,##0)"), (39, "#,##0.00;(#,##0.00)"), (49, "@"),
];

fn styles_part(pkg: &OoxmlPackage) -> Option<String> {
    let rels = pkg.part_str(WORKBOOK_RELS_PART).ok()?;
    let s = scan(rels).ok()?;
    s.by_local("Relationship").into_iter().find_map(|r| {
        let el = &s.elements[r];
        (el.attr(rels, "Type").as_deref() == Some(REL_STYLES))
            .then(|| el.attr(rels, "Target"))
            .flatten()
            .map(|t| resolve_target("xl", &t))
    })
}

fn kids(s: &XmlScan, parent: usize, local: &str) -> Vec<usize> {
    let mut out: Vec<usize> = s
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| e.parent == Some(parent) && e.local == local)
        .map(|(i, _)| i)
        .collect();
    out.sort_by_key(|&i| s.elements[i].outer.start);
    out
}

fn kid(s: &XmlScan, parent: usize, local: &str) -> Option<usize> {
    kids(s, parent, local).into_iter().next()
}

fn argb_to_hex(v: &str) -> Option<String> {
    let v = v.trim();
    match v.len() {
        8 => Some(v[2..].to_ascii_uppercase()),
        6 => Some(v.to_ascii_uppercase()),
        _ => None,
    }
}

/// One resolved `CellFormat` per `cellXfs` entry.
fn read_cell_formats(source: &str) -> Vec<CellFormat> {
    let Ok(s) = scan(source) else { return Vec::new() };
    let Some(root) = s.by_local("styleSheet").into_iter().next() else { return Vec::new() };

    let mut codes: HashMap<u32, String> = BUILTIN_NUM_FMTS.iter().map(|(i, c)| (*i, c.to_string())).collect();
    if let Some(nf) = kid(&s, root, "numFmts") {
        for f in kids(&s, nf, "numFmt") {
            let el = &s.elements[f];
            if let (Some(id), Some(code)) = (el.attr(source, "numFmtId").and_then(|v| v.parse().ok()), el.attr(source, "formatCode")) {
                codes.insert(id, code);
            }
        }
    }

    let toggle = |font: usize, local: &str| {
        kid(&s, font, local)
            .map(|b| !matches!(s.elements[b].attr(source, "val").as_deref(), Some("0") | Some("false") | Some("none")))
            .unwrap_or(false)
    };
    let fonts: Vec<CellFormat> = kid(&s, root, "fonts")
        .map(|fs| {
            kids(&s, fs, "font")
                .into_iter()
                .map(|f| CellFormat {
                    bold: toggle(f, "b"),
                    italic: toggle(f, "i"),
                    underline: toggle(f, "u"),
                    strike: toggle(f, "strike"),
                    font: kid(&s, f, "name").and_then(|n| s.elements[n].attr(source, "val")),
                    size: kid(&s, f, "sz").and_then(|n| s.elements[n].attr(source, "val")).and_then(|v| v.parse().ok()),
                    color: kid(&s, f, "color").and_then(|c| s.elements[c].attr(source, "rgb")).and_then(|v| argb_to_hex(&v)),
                    ..Default::default()
                })
                .collect()
        })
        .unwrap_or_default();
    let fills: Vec<Option<String>> = kid(&s, root, "fills")
        .map(|fs| {
            kids(&s, fs, "fill")
                .into_iter()
                .map(|f| {
                    let pattern = s.first_descendant(f, "patternFill")?;
                    if s.elements[pattern].attr(source, "patternType").as_deref() != Some("solid") {
                        return None;
                    }
                    s.first_descendant(pattern, "fgColor")
                        .and_then(|c| s.elements[c].attr(source, "rgb"))
                        .and_then(|v| argb_to_hex(&v))
                })
                .collect()
        })
        .unwrap_or_default();

    let default_font = fonts.first().cloned().unwrap_or_default();
    let Some(xfs) = kid(&s, root, "cellXfs") else { return Vec::new() };
    kids(&s, xfs, "xf")
        .into_iter()
        .map(|xf| {
            let el = &s.elements[xf];
            let num = |a: &str| el.attr(source, a).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            let font = fonts.get(num("fontId")).cloned().unwrap_or_default();
            let mut out = CellFormat {
                bold: font.bold,
                italic: font.italic,
                underline: font.underline,
                strike: font.strike,
                // Only report the face and size where they differ from the
                // workbook default, so the grid does not restyle every cell.
                font: font.font.clone().filter(|f| Some(f) != default_font.font.as_ref()),
                size: font.size.filter(|z| Some(*z) != default_font.size),
                color: font.color.clone(),
                fill: fills.get(num("fillId")).cloned().flatten(),
                ..Default::default()
            };
            let fmt_id = num("numFmtId") as u32;
            if fmt_id != 0 {
                out.num_fmt = codes.get(&fmt_id).cloned();
            }
            if let Some(al) = kid(&s, xf, "alignment") {
                out.align = s.elements[al].attr(source, "horizontal").filter(|h| h != "general");
                out.wrap = matches!(s.elements[al].attr(source, "wrapText").as_deref(), Some("1") | Some("true"));
            }
            out
        })
        .collect()
}

/// Make sure the workbook has a stylesheet with every collection a new `xf`
/// may reference. Returns the stylesheet part name.
fn ensure_stylesheet(pkg: &mut OoxmlPackage) -> Result<String> {
    let part = match styles_part(pkg) {
        Some(p) if pkg.has_part(&p) => p,
        _ => {
            let part = "xl/styles.xml".to_string();
            if !pkg.has_part(&part) {
                pkg.add_part(
                    &part,
                    b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
<fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>\
<cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellXfs></styleSheet>"
                        .to_vec(),
                )?;
            }
            pkg.ensure_override_content_type(&part, CT_STYLES)?;
            pkg.add_relationship(WORKBOOK_RELS_PART, REL_STYLES, "styles.xml")?;
            part
        }
    };

    let mut source = pkg.part_str(&part)?.to_string();
    for (local, default) in [
        ("fonts", "<fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>"),
        (
            "fills",
            "<fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills>",
        ),
        ("borders", "<borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>"),
        ("cellXfs", "<cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellXfs>"),
    ] {
        source = ensure_stylesheet_child(&source, local, default)?;
    }
    pkg.replace_part(&part, source.into_bytes())?;
    Ok(part)
}

fn ensure_stylesheet_child(source: &str, local: &str, default: &str) -> Result<String> {
    let s = scan(source)?;
    let root = s.by_local("styleSheet").into_iter().next().ok_or_else(|| anyhow!("styles.xml has no styleSheet"))?;
    if kid(&s, root, local).is_some() {
        return Ok(source.to_string());
    }
    let position = STYLESHEET_ORDER.iter().position(|l| *l == local).unwrap_or(0);
    let at = STYLESHEET_ORDER[..position]
        .iter()
        .rev()
        .find_map(|prev| kid(&s, root, prev).map(|i| s.elements[i].outer.end))
        .unwrap_or(s.elements[root].inner.start);
    apply_splices(source, vec![Splice { span: Span { start: at, end: at }, replacement: default.to_string() }])
}

/// Append `item` to the collection `local`, or reuse an identical entry.
/// Returns the new source and the item's index.
fn intern(source: &str, collection: &str, item_local: &str, item: &str) -> Result<(String, usize)> {
    let s = scan(source)?;
    let root = s.by_local("styleSheet").into_iter().next().ok_or_else(|| anyhow!("styles.xml has no styleSheet"))?;
    let coll = kid(&s, root, collection).ok_or_else(|| anyhow!("styles.xml has no {}", collection))?;
    let items = kids(&s, coll, item_local);
    if let Some(pos) = items.iter().position(|&i| s.elements[i].outer.of(source) == item) {
        return Ok((source.to_string(), pos));
    }
    let count = items.len() + 1;
    let tag = s.elements[coll].start_tag.of(source);
    let new_tag = set_attr(tag, "count", &count.to_string());
    let at = s.elements[coll].inner.end;
    let patched = apply_splices(
        source,
        vec![
            Splice { span: s.elements[coll].start_tag, replacement: new_tag },
            Splice { span: Span { start: at, end: at }, replacement: item.to_string() },
        ],
    )?;
    Ok((patched, items.len()))
}

/// Set or add an attribute on a start tag.
fn set_attr(tag: &str, name: &str, value: &str) -> String {
    let needle = format!(" {}=\"", name);
    if let Some(at) = tag.find(&needle) {
        let from = at + needle.len();
        let end = tag[from..].find('"').map(|e| from + e).unwrap_or(from);
        return format!("{}{}{}", &tag[..from], escape_attr(value), &tag[end..]);
    }
    let close = if tag.ends_with("/>") { tag.len() - 2 } else { tag.len() - 1 };
    format!("{} {}=\"{}\"{}", &tag[..close].trim_end(), name, escape_attr(value), &tag[close..])
}

fn hex_argb(value: &str) -> Option<String> {
    let hex = value.trim().trim_start_matches('#');
    (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| format!("FF{}", hex.to_ascii_uppercase()))
}

struct CellStyleChange<'a> {
    format: &'a TextFormat,
    fill: Option<&'a str>,
    align: Option<&'a str>,
    wrap: Option<bool>,
    num_fmt: Option<&'a str>,
}

/// Produce (and intern) the `cellXfs` index for `base` with `change` applied.
fn derive_style(pkg: &mut OoxmlPackage, base: usize, change: &CellStyleChange) -> Result<usize> {
    let part = ensure_stylesheet(pkg)?;
    let mut source = pkg.part_str(&part)?.to_string();

    let (base_xf, base_font, base_fill) = {
        let s = scan(&source)?;
        let root = s.by_local("styleSheet").into_iter().next().ok_or_else(|| anyhow!("styles.xml has no styleSheet"))?;
        let xfs = kids(&s, kid(&s, root, "cellXfs").unwrap(), "xf");
        let xf = xfs.get(base).or_else(|| xfs.first()).copied().ok_or_else(|| anyhow!("styles.xml has no cell formats"))?;
        let font_id = s.elements[xf].attr(&source, "fontId").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        let fonts = kids(&s, kid(&s, root, "fonts").unwrap(), "font");
        let font = fonts.get(font_id).or_else(|| fonts.first()).map(|&f| {
            let children: Vec<(String, String)> = s
                .elements
                .iter()
                .enumerate()
                .filter(|(_, e)| e.parent == Some(f))
                .map(|(_, e)| (e.local.clone(), e.outer.of(&source).to_string()))
                .collect();
            children
        });
        let fill_id = s.elements[xf].attr(&source, "fillId").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        (s.elements[xf].clone(), font.unwrap_or_default(), fill_id)
    };
    let xf_tag = base_xf.start_tag.of(&source).to_string();
    // Read before the source is rewritten below; the span goes stale after.
    let base_num_fmt = base_xf.attr(&source, "numFmtId").unwrap_or_else(|| "0".into());
    let xf_inner: Vec<(String, String)> = {
        let s = scan(&source)?;
        let xf_idx = s.elements.iter().position(|e| e.outer == base_xf.outer).unwrap_or(0);
        s.elements
            .iter()
            .filter(|e| e.parent == Some(xf_idx))
            .map(|e| (e.local.clone(), e.outer.of(&source).to_string()))
            .collect()
    };

    // ---- font
    let f = change.format;
    let mut font = base_font;
    let mut put = |local: &str, xml: Option<String>| {
        font.retain(|(l, _)| l != local);
        if let Some(x) = xml {
            font.push((local.to_string(), x));
        }
    };
    if let Some(on) = f.bold {
        put("b", on.then(|| "<b/>".to_string()));
    }
    if let Some(on) = f.italic {
        put("i", on.then(|| "<i/>".to_string()));
    }
    if let Some(on) = f.strike {
        put("strike", on.then(|| "<strike/>".to_string()));
    }
    if let Some(on) = f.underline {
        put("u", on.then(|| "<u/>".to_string()));
    }
    if let Some(size) = f.size {
        put("sz", (size > 0.0).then(|| format!("<sz val=\"{}\"/>", size)));
    }
    if let Some(color) = &f.color {
        put("color", hex_argb(color).map(|c| format!("<color rgb=\"{}\"/>", c)));
    }
    if let Some(name) = &f.font {
        if !name.trim().is_empty() {
            put("name", Some(format!("<name val=\"{}\"/>", escape_attr(name.trim()))));
            // A named face overrides the theme's; a stale scheme would win.
            put("scheme", None);
        }
    }
    font.sort_by_key(|(l, _)| FONT_ORDER.iter().position(|o| o == l).unwrap_or(FONT_ORDER.len()));
    let font_xml = format!("<font>{}</font>", font.into_iter().map(|(_, x)| x).collect::<String>());
    let (next, font_id) = intern(&source, "fonts", "font", &font_xml)?;
    source = next;

    // ---- fill
    let fill_id = match change.fill {
        None => base_fill,
        Some(color) => match hex_argb(color) {
            None => 0,
            Some(argb) => {
                let xml = format!(
                    "<fill><patternFill patternType=\"solid\"><fgColor rgb=\"{}\"/><bgColor indexed=\"64\"/></patternFill></fill>",
                    argb
                );
                let (next, id) = intern(&source, "fills", "fill", &xml)?;
                source = next;
                id
            }
        },
    };

    // ---- number format
    let mut num_fmt_id = base_num_fmt;
    if let Some(code) = change.num_fmt {
        num_fmt_id = match BUILTIN_NUM_FMTS.iter().find(|(_, c)| *c == code) {
            Some((id, _)) => id.to_string(),
            None => {
                source = ensure_stylesheet_child(&source, "numFmts", "<numFmts count=\"0\"></numFmts>")?;
                let s = scan(&source)?;
                let root = s.by_local("styleSheet").into_iter().next().unwrap();
                let nf = kid(&s, root, "numFmts").unwrap();
                let existing: Vec<(u32, String)> = kids(&s, nf, "numFmt")
                    .into_iter()
                    .filter_map(|i| {
                        let el = &s.elements[i];
                        Some((el.attr(&source, "numFmtId")?.parse().ok()?, el.attr(&source, "formatCode")?))
                    })
                    .collect();
                match existing.iter().find(|(_, c)| c == code) {
                    Some((id, _)) => id.to_string(),
                    None => {
                        let id = existing.iter().map(|(i, _)| *i).max().unwrap_or(163).max(163) + 1;
                        let xml = format!("<numFmt numFmtId=\"{}\" formatCode=\"{}\"/>", id, escape_attr(code));
                        let (next, _) = intern(&source, "numFmts", "numFmt", &xml)?;
                        source = next;
                        id.to_string()
                    }
                }
            }
        };
    }

    // ---- the xf itself
    let mut tag = xf_tag.trim_end_matches("/>").trim_end_matches('>').to_string() + ">";
    tag = set_attr(&tag, "numFmtId", &num_fmt_id);
    tag = set_attr(&tag, "fontId", &font_id.to_string());
    tag = set_attr(&tag, "fillId", &fill_id.to_string());
    if !tag.contains(" borderId=\"") {
        tag = set_attr(&tag, "borderId", "0");
    }
    tag = set_attr(&tag, "applyFont", "1");
    if change.fill.is_some() {
        tag = set_attr(&tag, "applyFill", "1");
    }
    if change.num_fmt.is_some() {
        tag = set_attr(&tag, "applyNumberFormat", "1");
    }

    let mut inner = xf_inner;
    if change.align.is_some() || change.wrap.is_some() {
        tag = set_attr(&tag, "applyAlignment", "1");
        let existing = inner.iter().find(|(l, _)| l == "alignment").map(|(_, x)| x.clone()).unwrap_or_else(|| "<alignment/>".into());
        let mut al = existing;
        if let Some(h) = change.align {
            al = if matches!(h, "left" | "center" | "right" | "justify") {
                set_attr(&al, "horizontal", h)
            } else {
                remove_attr(&al, "horizontal")
            };
        }
        if let Some(w) = change.wrap {
            al = if w { set_attr(&al, "wrapText", "1") } else { remove_attr(&al, "wrapText") };
        }
        inner.retain(|(l, _)| l != "alignment");
        // alignment precedes protection inside an xf.
        inner.insert(0, ("alignment".into(), al));
    }
    let xf_xml = format!("{}{}</xf>", tag, inner.into_iter().map(|(_, x)| x).collect::<String>());
    let (next, xf_id) = intern(&source, "cellXfs", "xf", &xf_xml)?;
    source = next;

    pkg.replace_part(&part, source.into_bytes())?;
    Ok(xf_id)
}

fn remove_attr(tag: &str, name: &str) -> String {
    let needle = format!(" {}=\"", name);
    match tag.find(&needle) {
        Some(at) => {
            let from = at + needle.len();
            let end = tag[from..].find('"').map(|e| from + e + 1).unwrap_or(tag.len());
            format!("{}{}", &tag[..at], &tag[end..])
        }
        None => tag.to_string(),
    }
}

// ---------------------------------------------------------------- writing

pub fn apply(bytes: &[u8], patches: &[DraftPatch]) -> Result<Vec<u8>> {
    let mut pkg = OoxmlPackage::open(bytes)?;
    let sheets = sheet_index(&pkg)?;

    // Group edits by sheet, so each worksheet part is parsed and spliced once
    // no matter how many cells changed in it.
    let mut by_sheet: HashMap<usize, Vec<&DraftPatch>> = HashMap::new();
    let mut formats: Vec<&DraftPatch> = Vec::new();
    for patch in patches {
        let addr = match patch {
            DraftPatch::SetCellValue { addr, .. } | DraftPatch::ClearCell { addr } => addr,
            DraftPatch::SetCellFormat { addr, .. } => {
                formats.push(patch);
                addr
            }
            other => return Err(wrong_format(other, "xlsx")),
        };
        let (sheet, _) = parse_cell_addr(addr)?;
        if sheet >= sheets.len() {
            return Err(anyhow!(
                "'{}' refers to sheet {}, but this workbook has {}. \
                 Reopen the draft to pick up the current version.",
                addr,
                sheet,
                sheets.len()
            ));
        }
        if !matches!(patch, DraftPatch::SetCellFormat { .. }) {
            by_sheet.entry(sheet).or_default().push(patch);
        }
    }

    for (sheet_idx, sheet_patches) in by_sheet {
        let part = sheets[sheet_idx].1.clone();
        let source = pkg.part_str(&part)?.to_string();
        let s = scan(&source)?;

        let mut splices = Vec::new();
        for patch in sheet_patches {
            match patch {
                DraftPatch::SetCellValue { addr, value } => {
                    let (_, reference) = parse_cell_addr(addr)?;
                    splices.push(write_cell(&source, &s, &reference, value, None)?);
                }
                DraftPatch::ClearCell { addr } => {
                    let (_, reference) = parse_cell_addr(addr)?;
                    if let Some(c) = find_cell(&source, &s, &reference) {
                        splices.push(Splice { span: s.elements[c].outer, replacement: String::new() });
                    }
                }
                other => return Err(wrong_format(other, "xlsx")),
            }
        }

        let patched = apply_splices(&source, splices)?;
        pkg.replace_part(&part, patched.into_bytes())?;
    }

    // Formatting runs after values, one cell at a time, because each one may
    // add to the stylesheet and must see the entries the previous one added.
    for patch in formats {
        let DraftPatch::SetCellFormat { addr, format, fill, align, wrap, num_fmt } = patch else { continue };
        let (sheet_idx, reference) = parse_cell_addr(addr)?;
        let part = sheets[sheet_idx].1.clone();
        let source = pkg.part_str(&part)?.to_string();
        let s = scan(&source)?;
        let cell = find_cell(&source, &s, &reference);
        let base = cell
            .and_then(|c| s.elements[c].attr(&source, "s"))
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        let style = derive_style(
            &mut pkg,
            base,
            &CellStyleChange {
                format,
                fill: fill.as_deref(),
                align: align.as_deref(),
                wrap: *wrap,
                num_fmt: num_fmt.as_deref(),
            },
        )?;

        let splice = match cell {
            Some(c) => {
                let tag = s.elements[c].start_tag.of(&source);
                Splice { span: s.elements[c].start_tag, replacement: set_attr(tag, "s", &style.to_string()) }
            }
            None => write_cell(&source, &s, &reference, "", Some(&style.to_string()))?,
        };
        let patched = apply_splices(&source, vec![splice])?;
        pkg.replace_part(&part, patched.into_bytes())?;
    }

    pkg.save()
}

fn find_cell(source: &str, s: &XmlScan, reference: &str) -> Option<usize> {
    s.by_local("c")
        .into_iter()
        .find(|&c| s.elements[c].attr(source, "r").as_deref() == Some(reference))
}

/// Build the splice that puts `value` into `reference`.
///
/// Three cases, in order of how often they happen: the cell exists (replace it,
/// keeping its style), the row exists but the cell does not (insert in column
/// order), or neither exists (insert a whole row in row order). Column and row
/// order are not cosmetic — Excel requires `sheetData` to be sorted, and a
/// file that is not will be reported as needing repair.
fn write_cell(source: &str, s: &XmlScan, reference: &str, value: &str, style: Option<&str>) -> Result<Splice> {
    let (col, row) = parse_ref(reference)?;

    if let Some(c) = find_cell(source, s, reference) {
        let existing = s.elements[c].attr(source, "s");
        return Ok(Splice {
            span: s.elements[c].outer,
            replacement: new_cell(reference, style.or(existing.as_deref()), value),
        });
    }

    let cell_xml = new_cell(reference, style, value);

    // Row present?
    let existing_row = s
        .by_local("row")
        .into_iter()
        .find(|&r| s.elements[r].attr(source, "r").and_then(|v| v.parse::<usize>().ok()) == Some(row));

    if let Some(r) = existing_row {
        let mut at = s.elements[r].inner.end;
        for &c in s.descendants_by_local(r, "c").iter() {
            let Some(cref) = s.elements[c].attr(source, "r") else { continue };
            let (other_col, _) = parse_ref(&cref)?;
            if other_col > col {
                at = s.elements[c].outer.start;
                break;
            }
        }
        return Ok(Splice { span: Span { start: at, end: at }, replacement: cell_xml });
    }

    // Neither: insert a new row into sheetData, in row order.
    let sheet_data = s
        .by_local("sheetData")
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("this worksheet has no sheetData element"))?;

    let mut at = s.elements[sheet_data].inner.end;
    for &r in s.descendants_by_local(sheet_data, "row").iter() {
        let other = s.elements[r].attr(source, "r").and_then(|v| v.parse::<usize>().ok());
        if other.map(|o| o > row).unwrap_or(false) {
            at = s.elements[r].outer.start;
            break;
        }
    }
    Ok(Splice {
        span: Span { start: at, end: at },
        replacement: format!("<row r=\"{}\">{}</row>", row, cell_xml),
    })
}

/// Serialise one cell from what the user typed.
///
/// A leading `=` makes it a formula, and the cached `<v>` is deliberately NOT
/// written: we do not evaluate formulas here, and writing a stale cached result
/// would make Excel display a number that does not match its own formula until
/// something forces a recalculation. Omitting it makes Excel compute the real
/// answer on open.
fn new_cell(reference: &str, style: Option<&str>, value: &str) -> String {
    let style_attr = style.map(|s| format!(" s=\"{}\"", s)).unwrap_or_default();

    if let Some(expr) = value.strip_prefix('=') {
        return format!(
            "<c r=\"{}\"{}><f>{}</f></c>",
            reference,
            style_attr,
            escape(expr)
        );
    }
    if value.is_empty() {
        return format!("<c r=\"{}\"{}/>", reference, style_attr);
    }
    // Numbers are stored bare so Excel treats them as numbers, not as text that
    // merely looks numeric — the difference every SUM in the sheet depends on.
    if value.parse::<f64>().is_ok() {
        return format!("<c r=\"{}\"{}><v>{}</v></c>", reference, style_attr, value);
    }
    format!(
        "<c r=\"{}\"{} t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
        reference,
        style_attr,
        escape(value)
    )
}

// ---------------------------------------------------------------- addressing

fn parse_cell_addr(addr: &str) -> Result<(usize, String)> {
    let (sheet_part, reference) = addr
        .split_once('/')
        .ok_or_else(|| anyhow!("'{}' is not a valid address; expected 'sheet[0]/B7'", addr))?;
    let sheet = sheet_part
        .strip_prefix("sheet[")
        .and_then(|s| s.strip_suffix(']'))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("'{}' is not a valid address; expected 'sheet[0]/B7'", addr))?;
    // Validated here so a malformed reference is rejected before any splice.
    parse_ref(reference)?;
    Ok((sheet, reference.to_string()))
}

/// `B7` → (column 1, row 7). Columns are zero-based; rows keep Excel's
/// one-based numbering because that is what the file itself stores.
fn parse_ref(reference: &str) -> Result<(usize, usize)> {
    let split = reference
        .find(|c: char| c.is_ascii_digit())
        .ok_or_else(|| anyhow!("'{}' is not a cell reference", reference))?;
    let (letters, digits) = reference.split_at(split);
    if letters.is_empty() || !letters.chars().all(|c| c.is_ascii_uppercase()) {
        return Err(anyhow!("'{}' is not a cell reference", reference));
    }

    let mut col = 0usize;
    for ch in letters.chars() {
        col = col * 26 + (ch as usize - 'A' as usize + 1);
    }
    let row: usize = digits
        .parse()
        .map_err(|_| anyhow!("'{}' has no valid row number", reference))?;
    if row == 0 {
        return Err(anyhow!("'{}' has row 0; rows start at 1", reference));
    }
    Ok((col - 1, row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_workspace::ooxml::package::fixtures;

    fn sheets(bytes: &[u8]) -> Vec<Sheet> {
        match build(bytes, 1).unwrap() {
            DraftViewModel::Xlsx { sheets, .. } => sheets,
            other => panic!("expected an xlsx model, got {:?}", other),
        }
    }

    #[test]
    fn cell_references_convert_in_both_directions() {
        assert_eq!(parse_ref("A1").unwrap(), (0, 1));
        assert_eq!(parse_ref("B7").unwrap(), (1, 7));
        assert_eq!(parse_ref("Z1").unwrap(), (25, 1));
        assert_eq!(parse_ref("AA1").unwrap(), (26, 1));
        assert_eq!(parse_ref("AMJ1048576").unwrap(), (1023, 1048576));
    }

    #[test]
    fn malformed_references_are_refused() {
        for bad in ["1A", "A0", "", "a1", "AB"] {
            assert!(parse_ref(bad).is_err(), "'{}' should be refused", bad);
        }
    }

    #[test]
    fn sheets_are_read_in_workbook_order_with_their_real_part_paths() {
        let s = sheets(&fixtures::xlsx());
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "Fees");
        assert_eq!(s[0].part, "xl/worksheets/sheet1.xml");
        assert_eq!(s[1].name, "Summary");
    }

    #[test]
    fn shared_strings_numbers_and_formulas_all_read_correctly() {
        let s = sheets(&fixtures::xlsx());
        let fees = &s[0];
        assert_eq!(fees.cells["A1"].value, "Matter");
        assert_eq!(fees.cells["A2"].value, "Drafting");

        let b2 = &fees.cells["B2"];
        assert_eq!(b2.value, "1200");
        assert!(b2.numeric);

        let b4 = &fees.cells["B4"];
        assert_eq!(b4.formula.as_deref(), Some("SUM(B2:B3)"));
        assert_eq!(b4.value, "2050", "Excel's cached result is shown until recalc");
        assert_eq!(b4.addr, "sheet[0]/B4");
    }

    /// The central XLSX guarantee: a text edit leaves the shared string table
    /// and every other part byte-identical.
    #[test]
    fn editing_a_cell_leaves_shared_strings_and_all_other_parts_untouched() {
        let original = fixtures::xlsx();
        let saved = apply(
            &original,
            &[DraftPatch::SetCellValue { addr: "sheet[0]/A2".into(), value: "Negotiation".into() }],
        )
        .unwrap();

        let before = OoxmlPackage::open(&original).unwrap();
        let after = OoxmlPackage::open(&saved).unwrap();
        for name in before.part_names() {
            if name == "xl/worksheets/sheet1.xml" {
                continue;
            }
            assert_eq!(
                before.part(name).unwrap(),
                after.part(name).unwrap(),
                "part '{}' must survive a cell edit untouched",
                name
            );
        }
        assert_eq!(sheets(&saved)[0].cells["A2"].value, "Negotiation");
        // And the OTHER cell that shared the string table still reads right.
        assert_eq!(sheets(&saved)[0].cells["A3"].value, "Review");
    }

    #[test]
    fn a_numeric_edit_is_stored_as_a_number_not_as_text() {
        let saved = apply(
            &fixtures::xlsx(),
            &[DraftPatch::SetCellValue { addr: "sheet[0]/B2".into(), value: "1500".into() }],
        )
        .unwrap();
        let b2 = &sheets(&saved)[0].cells["B2"];
        assert_eq!(b2.value, "1500");
        assert!(b2.numeric, "a number typed into a cell must stay a number");
    }

    #[test]
    fn a_formula_is_written_without_a_stale_cached_result() {
        let saved = apply(
            &fixtures::xlsx(),
            &[DraftPatch::SetCellValue { addr: "sheet[0]/B4".into(), value: "=SUM(B2:B3)*2".into() }],
        )
        .unwrap();
        let b4 = &sheets(&saved)[0].cells["B4"];
        assert_eq!(b4.formula.as_deref(), Some("SUM(B2:B3)*2"));
        assert_eq!(b4.value, "", "no cached value, so Excel recalculates on open");
    }

    #[test]
    fn a_cell_style_survives_an_edit() {
        // B4 carries no style in the fixture; give the test a styled cell by
        // writing one and confirming the attribute is carried through.
        let once = apply(
            &fixtures::xlsx(),
            &[DraftPatch::SetCellValue { addr: "sheet[0]/B2".into(), value: "10".into() }],
        )
        .unwrap();
        let pkg = OoxmlPackage::open(&once).unwrap();
        let xml = pkg.part_str("xl/worksheets/sheet1.xml").unwrap();
        assert!(xml.contains("<c r=\"B2\"><v>10</v></c>"), "{}", xml);
    }

    #[test]
    fn typing_into_an_empty_cell_inserts_it_in_column_order() {
        let saved = apply(
            &fixtures::xlsx(),
            &[DraftPatch::SetCellValue { addr: "sheet[0]/A5".into(), value: "Expenses".into() }],
        )
        .unwrap();
        assert_eq!(sheets(&saved)[0].cells["A5"].value, "Expenses");

        // And into a row that did not exist at all.
        let further = apply(
            &saved,
            &[DraftPatch::SetCellValue { addr: "sheet[0]/C2".into(), value: "note".into() }],
        )
        .unwrap();
        let pkg = OoxmlPackage::open(&further).unwrap();
        let xml = pkg.part_str("xl/worksheets/sheet1.xml").unwrap();
        let b2 = xml.find("r=\"B2\"").unwrap();
        let c2 = xml.find("r=\"C2\"").unwrap();
        assert!(b2 < c2, "cells must stay in column order:\n{}", xml);
    }

    #[test]
    fn a_new_row_is_inserted_in_row_order() {
        let saved = apply(
            &fixtures::xlsx(),
            &[DraftPatch::SetCellValue { addr: "sheet[0]/A9".into(), value: "Later".into() }],
        )
        .unwrap();
        let then = apply(
            &saved,
            &[DraftPatch::SetCellValue { addr: "sheet[0]/A6".into(), value: "Earlier".into() }],
        )
        .unwrap();

        let pkg = OoxmlPackage::open(&then).unwrap();
        let xml = pkg.part_str("xl/worksheets/sheet1.xml").unwrap();
        let r6 = xml.find("<row r=\"6\"").unwrap();
        let r9 = xml.find("<row r=\"9\"").unwrap();
        assert!(r6 < r9, "rows must stay sorted:\n{}", xml);
    }

    #[test]
    fn edits_across_two_sheets_are_applied_in_one_pass() {
        let saved = apply(
            &fixtures::xlsx(),
            &[
                DraftPatch::SetCellValue { addr: "sheet[0]/A1".into(), value: "Matter ref".into() },
                DraftPatch::SetCellValue { addr: "sheet[1]/A1".into(), value: "Commentary".into() },
            ],
        )
        .unwrap();
        let s = sheets(&saved);
        assert_eq!(s[0].cells["A1"].value, "Matter ref");
        assert_eq!(s[1].cells["A1"].value, "Commentary");
    }

    #[test]
    fn clearing_a_cell_removes_it() {
        let saved = apply(
            &fixtures::xlsx(),
            &[DraftPatch::ClearCell { addr: "sheet[0]/B3".into() }],
        )
        .unwrap();
        assert!(!sheets(&saved)[0].cells.contains_key("B3"));
        assert!(sheets(&saved)[0].cells.contains_key("B2"), "neighbours are untouched");
    }

    #[test]
    fn an_edit_to_a_sheet_that_does_not_exist_is_refused_with_advice() {
        let err = apply(
            &fixtures::xlsx(),
            &[DraftPatch::SetCellValue { addr: "sheet[7]/A1".into(), value: "x".into() }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("has 2"), "{}", err);
        assert!(err.contains("Reopen the draft"), "{}", err);
    }

    #[test]
    fn cell_formatting_adds_styles_and_reads_back() {
        let saved = apply(
            &fixtures::xlsx(),
            &[
                DraftPatch::SetCellFormat {
                    addr: "sheet[0]/B2".into(),
                    format: TextFormat { bold: Some(true), color: Some("C00000".into()), ..Default::default() },
                    fill: Some("FFF2CC".into()),
                    align: Some("center".into()),
                    wrap: None,
                    num_fmt: Some("#,##0.00".into()),
                },
                // A cell that does not exist yet gets created with the style.
                DraftPatch::SetCellFormat {
                    addr: "sheet[0]/D9".into(),
                    format: TextFormat { italic: Some(true), ..Default::default() },
                    fill: None,
                    align: None,
                    wrap: None,
                    num_fmt: None,
                },
            ],
        )
        .unwrap();
        let s = sheets(&saved);
        let f = s[0].cells["B2"].format.clone().expect("B2 is formatted");
        assert!(f.bold);
        assert_eq!(f.color.as_deref(), Some("C00000"));
        assert_eq!(f.fill.as_deref(), Some("FFF2CC"));
        assert_eq!(f.align.as_deref(), Some("center"));
        assert_eq!(f.num_fmt.as_deref(), Some("#,##0.00"));
        assert_eq!(s[0].cells["B2"].value, "1200", "formatting leaves the value alone");
        assert!(s[0].cells["D9"].format.as_ref().unwrap().italic);
    }

    #[test]
    fn relationship_targets_resolve_relative_to_the_declaring_part() {
        assert_eq!(resolve_target("xl", "worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        assert_eq!(resolve_target("xl", "/xl/worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        assert_eq!(resolve_target("ppt/slides", "../slideLayouts/l1.xml"), "ppt/slideLayouts/l1.xml");
    }
}
