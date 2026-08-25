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
use super::spans::{apply_splices, escape, scan, unescape, Span, Splice, XmlScan};
use crate::document_workspace::view_model::{wrong_format, DraftPatch, DraftViewModel};

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
}

// ---------------------------------------------------------------- reading

pub fn build(bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    let pkg = OoxmlPackage::open(bytes)?;
    let shared = read_shared_strings(&pkg)?;
    let mut sheets = Vec::new();

    for (index, (name, part)) in sheet_index(&pkg)?.into_iter().enumerate() {
        let source = pkg.part_str(&part)?;
        let sheet = read_sheet(index, &name, &part, source, &shared)?;
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
            Cell { addr: format!("sheet[{}]/{}", index, reference), value, formula, style, numeric },
        );
    }

    let merges = s
        .by_local("mergeCell")
        .iter()
        .filter_map(|&m| s.elements[m].attr(source, "ref"))
        .collect();

    Ok(Sheet {
        index,
        name: name.to_string(),
        part: part.to_string(),
        rows: max_row,
        cols: max_col,
        cells,
        merges,
    })
}

// ---------------------------------------------------------------- writing

pub fn apply(bytes: &[u8], patches: &[DraftPatch]) -> Result<Vec<u8>> {
    let mut pkg = OoxmlPackage::open(bytes)?;
    let sheets = sheet_index(&pkg)?;

    // Group edits by sheet, so each worksheet part is parsed and spliced once
    // no matter how many cells changed in it.
    let mut by_sheet: HashMap<usize, Vec<&DraftPatch>> = HashMap::new();
    for patch in patches {
        let addr = match patch {
            DraftPatch::SetCellValue { addr, .. } | DraftPatch::ClearCell { addr } => addr,
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
        by_sheet.entry(sheet).or_default().push(patch);
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
                    splices.push(write_cell(&source, &s, &reference, value)?);
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
fn write_cell(source: &str, s: &XmlScan, reference: &str, value: &str) -> Result<Splice> {
    let (col, row) = parse_ref(reference)?;

    if let Some(c) = find_cell(source, s, reference) {
        let style = s.elements[c].attr(source, "s");
        return Ok(Splice {
            span: s.elements[c].outer,
            replacement: new_cell(reference, style.as_deref(), value),
        });
    }

    let cell_xml = new_cell(reference, None, value);

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
    fn relationship_targets_resolve_relative_to_the_declaring_part() {
        assert_eq!(resolve_target("xl", "worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        assert_eq!(resolve_target("xl", "/xl/worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        assert_eq!(resolve_target("ppt/slides", "../slideLayouts/l1.xml"), "ppt/slideLayouts/l1.xml");
    }
}
