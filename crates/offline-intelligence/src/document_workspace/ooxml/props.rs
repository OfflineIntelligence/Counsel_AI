//! Rewriting WordprocessingML property blocks (`w:rPr`, `w:pPr`) in schema order.
//!
//! # Why order matters
//!
//! `w:rPr` and `w:pPr` are `xsd:sequence`s: `w:b` must come before `w:sz`,
//! `w:numPr` before `w:jc`. Word opens an out-of-order file with a "needs
//! repair" prompt and sometimes drops the formatting outright. Appending a new
//! child at the end is therefore not an option; every rewrite here takes the
//! existing children, swaps the ones being changed, and re-emits them sorted by
//! the schema's own sequence. Children we do not know keep their relative order
//! at the end, where extension elements belong.

use super::spans::{escape_attr, XmlScan};
use crate::document_workspace::view_model::TextFormat;

const RPR_ORDER: &[&str] = &[
    "rStyle", "rFonts", "b", "bCs", "i", "iCs", "caps", "smallCaps", "strike", "dstrike",
    "outline", "shadow", "emboss", "imprint", "noProof", "snapToGrid", "vanish", "webHidden",
    "color", "spacing", "w", "kern", "position", "sz", "szCs", "highlight", "u", "effect", "bdr",
    "shd", "fitText", "vertAlign", "rtl", "cs", "em", "lang", "eastAsianLayout", "specVanish",
    "oMath",
];

const PPR_ORDER: &[&str] = &[
    "pStyle", "keepNext", "keepLines", "pageBreakBefore", "framePr", "widowControl", "numPr",
    "suppressLineNumbers", "pBdr", "shd", "tabs", "suppressAutoHyphens", "kinsoku", "wordWrap",
    "overflowPunct", "topLinePunct", "autoSpaceDE", "autoSpaceDN", "bidi", "adjustRightInd",
    "snapToGrid", "spacing", "ind", "contextualSpacing", "mirrorIndents", "suppressOverlap", "jc",
    "textDirection", "textAlignment", "textboxTightWrap", "outlineLvl", "divId", "cnfStyle", "rPr",
    "sectPr", "pPrChange",
];

/// The Word highlight palette. `w:highlight` accepts only these names.
pub const HIGHLIGHTS: &[&str] = &[
    "yellow", "green", "cyan", "magenta", "blue", "red", "darkBlue", "darkCyan", "darkGreen",
    "darkMagenta", "darkRed", "darkYellow", "darkGray", "lightGray", "black", "white",
];

/// The direct children of `parent` as `(local name, outer xml)`.
pub fn children(source: &str, s: &XmlScan, parent: usize) -> Vec<(String, String)> {
    let mut kids: Vec<usize> = s
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| e.parent == Some(parent))
        .map(|(i, _)| i)
        .collect();
    kids.sort_by_key(|&i| s.elements[i].outer.start);
    kids.into_iter()
        .map(|i| (s.elements[i].local.clone(), s.elements[i].outer.of(source).to_string()))
        .collect()
}

fn sort_by(order: &[&str], mut kids: Vec<(String, String)>) -> String {
    // Stable: equal keys (unknown elements) keep their original relative order.
    kids.sort_by_key(|(local, _)| order.iter().position(|o| o == local).unwrap_or(order.len()));
    kids.into_iter().map(|(_, xml)| xml).collect()
}

fn set(kids: &mut Vec<(String, String)>, locals: &[&str], replacement: Vec<(&str, String)>) {
    kids.retain(|(l, _)| !locals.contains(&l.as_str()));
    for (local, xml) in replacement {
        kids.push((local.to_string(), xml));
    }
}

fn clean_hex(value: &str) -> Option<String> {
    let hex = value.trim().trim_start_matches('#');
    (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| hex.to_ascii_uppercase())
}

/// Apply `format` to the children of a `w:rPr`, returning its new inner XML.
pub fn rpr_inner(p: &str, mut kids: Vec<(String, String)>, format: &TextFormat) -> String {
    // A toggle is written explicitly off (`w:val="0"`) rather than removed, so
    // un-bolding text whose style is bold actually takes effect.
    let toggle = |local: &str, on: bool| {
        if on {
            format!("<{p}:{local}/>")
        } else {
            format!("<{p}:{local} {p}:val=\"0\"/>")
        }
    };

    if let Some(on) = format.bold {
        set(&mut kids, &["b", "bCs"], vec![("b", toggle("b", on)), ("bCs", toggle("bCs", on))]);
    }
    if let Some(on) = format.italic {
        set(&mut kids, &["i", "iCs"], vec![("i", toggle("i", on)), ("iCs", toggle("iCs", on))]);
    }
    if let Some(on) = format.strike {
        set(&mut kids, &["strike"], vec![("strike", toggle("strike", on))]);
    }
    if let Some(on) = format.underline {
        let u = if on {
            format!("<{p}:u {p}:val=\"single\"/>")
        } else {
            format!("<{p}:u {p}:val=\"none\"/>")
        };
        set(&mut kids, &["u"], vec![("u", u)]);
    }
    if let Some(font) = &format.font {
        let v = if font.trim().is_empty() {
            vec![]
        } else {
            let f = escape_attr(font.trim());
            vec![(
                "rFonts",
                format!("<{p}:rFonts {p}:ascii=\"{f}\" {p}:hAnsi=\"{f}\" {p}:cs=\"{f}\" {p}:eastAsia=\"{f}\"/>"),
            )]
        };
        set(&mut kids, &["rFonts"], v);
    }
    if let Some(size) = format.size {
        let v = if size > 0.0 {
            let half = (size * 2.0).round().clamp(2.0, 3276.0) as u32;
            vec![
                ("sz", format!("<{p}:sz {p}:val=\"{half}\"/>")),
                ("szCs", format!("<{p}:szCs {p}:val=\"{half}\"/>")),
            ]
        } else {
            vec![]
        };
        set(&mut kids, &["sz", "szCs"], v);
    }
    if let Some(color) = &format.color {
        let v = clean_hex(color)
            .map(|hex| vec![("color", format!("<{p}:color {p}:val=\"{hex}\"/>"))])
            .unwrap_or_default();
        set(&mut kids, &["color"], v);
    }
    if let Some(name) = &format.highlight {
        let v = HIGHLIGHTS
            .iter()
            .find(|h| h.eq_ignore_ascii_case(name.trim()))
            .map(|h| vec![("highlight", format!("<{p}:highlight {p}:val=\"{h}\"/>"))])
            .unwrap_or_default();
        set(&mut kids, &["highlight"], v);
    }
    if let Some(va) = &format.vert_align {
        let v = match va.as_str() {
            "superscript" | "subscript" => vec![("vertAlign", format!("<{p}:vertAlign {p}:val=\"{va}\"/>"))],
            _ => vec![],
        };
        set(&mut kids, &["vertAlign"], v);
    }
    sort_by(RPR_ORDER, kids)
}

/// Paragraph property changes, resolved by the caller into concrete XML.
#[derive(Default)]
pub struct ParagraphChange {
    /// `Some(None)` clears the style.
    pub style: Option<Option<String>>,
    /// `Some(None)` removes numbering; `Some((level, num_id))` sets it.
    pub numbering: Option<Option<(u32, u32)>>,
    /// Left indent in twips; `Some(None)` removes the indent.
    pub indent_left: Option<Option<i64>>,
    pub align: Option<String>,
    pub line_spacing: Option<f32>,
}

/// Apply a paragraph change to the children of a `w:pPr`, returning new inner XML.
pub fn ppr_inner(p: &str, mut kids: Vec<(String, String)>, change: &ParagraphChange) -> String {
    if let Some(style) = &change.style {
        let v = style
            .as_ref()
            .map(|s| vec![("pStyle", format!("<{p}:pStyle {p}:val=\"{}\"/>", escape_attr(s)))])
            .unwrap_or_default();
        set(&mut kids, &["pStyle"], v);
    }
    if let Some(numbering) = &change.numbering {
        let v = numbering
            .map(|(level, id)| {
                vec![(
                    "numPr",
                    format!("<{p}:numPr><{p}:ilvl {p}:val=\"{level}\"/><{p}:numId {p}:val=\"{id}\"/></{p}:numPr>"),
                )]
            })
            .unwrap_or_default();
        set(&mut kids, &["numPr"], v);
        // A list supplies its own indent; a stale manual one would fight it.
        if numbering.is_some() {
            set(&mut kids, &["ind"], vec![]);
        }
    }
    if let Some(indent) = change.indent_left {
        let v = indent
            .filter(|t| *t > 0)
            .map(|t| vec![("ind", format!("<{p}:ind {p}:left=\"{t}\"/>"))])
            .unwrap_or_default();
        set(&mut kids, &["ind"], v);
    }
    if let Some(align) = &change.align {
        let val = match align.as_str() {
            "center" => Some("center"),
            "right" => Some("right"),
            "justify" | "both" => Some("both"),
            "left" => Some("left"),
            _ => None,
        };
        let v = val.map(|v| vec![("jc", format!("<{p}:jc {p}:val=\"{v}\"/>"))]).unwrap_or_default();
        set(&mut kids, &["jc"], v);
    }
    if let Some(multiple) = change.line_spacing {
        // Keep any before/after spacing the paragraph already had.
        let existing = kids.iter().find(|(l, _)| l == "spacing").map(|(_, x)| x.clone());
        let keep = |attr: &str| {
            existing.as_ref().and_then(|x| {
                let needle = format!("{p}:{attr}=\"");
                let at = x.find(&needle)? + needle.len();
                let end = x[at..].find('"')?;
                Some(format!(" {p}:{attr}=\"{}\"", &x[at..at + end]))
            })
        };
        let line = (multiple.clamp(0.5, 5.0) * 240.0).round() as i64;
        let xml = format!(
            "<{p}:spacing{}{} {p}:line=\"{line}\" {p}:lineRule=\"auto\"/>",
            keep("before").unwrap_or_default(),
            keep("after").unwrap_or_default(),
        );
        set(&mut kids, &["spacing"], vec![("spacing", xml)]);
    }
    sort_by(PPR_ORDER, kids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_properties_are_emitted_in_schema_order() {
        let kids = vec![("u".to_string(), "<w:u w:val=\"single\"/>".to_string())];
        let out = rpr_inner(
            "w",
            kids,
            &TextFormat { bold: Some(true), size: Some(12.0), font: Some("Arial".into()), ..Default::default() },
        );
        let fonts = out.find("rFonts").unwrap();
        let b = out.find("<w:b/>").unwrap();
        let sz = out.find("<w:sz ").unwrap();
        let u = out.find("<w:u ").unwrap();
        assert!(fonts < b && b < sz && sz < u, "{}", out);
    }

    #[test]
    fn an_unknown_highlight_clears_rather_than_writing_invalid_xml() {
        let kids = vec![("highlight".to_string(), "<w:highlight w:val=\"yellow\"/>".to_string())];
        let out = rpr_inner("w", kids, &TextFormat { highlight: Some("#123456".into()), ..Default::default() });
        assert!(!out.contains("highlight"), "{}", out);
    }

    #[test]
    fn numbering_precedes_alignment_in_paragraph_properties() {
        let out = ppr_inner(
            "w",
            vec![],
            &ParagraphChange {
                align: Some("center".into()),
                numbering: Some(Some((0, 9101))),
                ..Default::default()
            },
        );
        assert!(out.find("numPr").unwrap() < out.find("<w:jc").unwrap(), "{}", out);
    }
}
