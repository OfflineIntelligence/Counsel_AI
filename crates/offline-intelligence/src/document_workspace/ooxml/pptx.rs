//! PPTX: read slides into positioned shapes, and splice text and geometry back
//! into the individual slide parts.
//!
//! # Addressing
//!
//! `slide[2]/sp[1]` for a shape, `slide[2]/sp[1]/p[0]` for a paragraph inside
//! its text body. Slide order comes from `p:sldIdLst` in `presentation.xml`,
//! resolved through relationships — never from the `slideN.xml` filename, which
//! PowerPoint renumbers freely as slides are added and removed.
//!
//! # Rendering honesty
//!
//! This module reports geometry, text and image references. SmartArt, charts,
//! 3-D effects, transitions and animations are read past and rendered as a
//! labelled placeholder by the UI. That is a *display* limitation only: an
//! effect we never address is an XML part we never patch, so the file keeps it
//! in full.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::package::{read_relationships, resolve_target, OoxmlPackage};
use super::spans::{apply_splices, escape, scan, unescape, Span, Splice, XmlScan};
use crate::document_workspace::view_model::{wrong_format, DraftPatch, DraftViewModel};

const PRESENTATION_PART: &str = "ppt/presentation.xml";
const PRESENTATION_RELS_PART: &str = "ppt/_rels/presentation.xml.rels";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slide {
    pub addr: String,
    pub index: usize,
    pub part: String,
    /// Slide dimensions in EMU, from `p:sldSz`.
    pub width: i64,
    pub height: i64,
    pub shapes: Vec<Shape>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shape {
    pub addr: String,
    /// `sp` for a shape, `pic` for a picture, `graphicFrame` for charts and
    /// tables, `grpSp` for a group.
    pub kind: String,
    pub name: String,
    /// Placeholder type (`title`, `body`, …) when this shape is one.
    pub placeholder: Option<String>,
    /// `None` when the shape inherits its position from the layout, which the
    /// UI renders by falling back to the layout box rather than to (0,0).
    pub bbox: Option<BBox>,
    pub paragraphs: Vec<Paragraph>,
    /// Relationship id of the embedded image, for `pic` shapes.
    pub image_rel: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BBox {
    pub x: i64,
    pub y: i64,
    pub cx: i64,
    pub cy: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paragraph {
    pub addr: String,
    pub text: String,
    /// Indent level, so bulleted bodies render at the right depth.
    pub level: u32,
}

// ---------------------------------------------------------------- reading

pub fn build(bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    let pkg = OoxmlPackage::open(bytes)?;
    let (width, height) = slide_size(&pkg)?;

    let mut slides = Vec::new();
    for (index, part) in slide_parts(&pkg)?.into_iter().enumerate() {
        let source = pkg.part_str(&part)?;
        slides.push(read_slide(index, &part, source, width, height)?);
    }

    if slides.is_empty() {
        return Err(anyhow!("This presentation contains no slides."));
    }
    debug!("PPTX view model: {} slides", slides.len());
    Ok(DraftViewModel::Pptx { version, slides })
}

fn slide_size(pkg: &OoxmlPackage) -> Result<(i64, i64)> {
    let source = pkg.part_str(PRESENTATION_PART)?;
    let s = scan(source)?;
    let sz = s
        .by_local("sldSz")
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("This presentation does not declare a slide size."))?;
    let el = &s.elements[sz];
    Ok((
        el.attr(source, "cx").and_then(|v| v.parse().ok()).unwrap_or(12_192_000),
        el.attr(source, "cy").and_then(|v| v.parse().ok()).unwrap_or(6_858_000),
    ))
}

/// Slide part paths in presentation order.
fn slide_parts(pkg: &OoxmlPackage) -> Result<Vec<String>> {
    let pres = pkg.part_str(PRESENTATION_PART)?;
    let rels = pkg.part_str(PRESENTATION_RELS_PART)?;

    let targets = read_relationships(rels)?;
    let pres_scan = scan(pres)?;
    let mut out = Vec::new();
    for &s in pres_scan.by_local("sldId").iter() {
        let Some(rid) = pres_scan.elements[s].attr(pres, "r:id") else { continue };
        let target = targets
            .get(&rid)
            .ok_or_else(|| anyhow!("a slide points at missing relationship {}", rid))?;
        out.push(resolve_target("ppt", target));
    }
    Ok(out)
}

fn read_slide(index: usize, part: &str, source: &str, width: i64, height: i64) -> Result<Slide> {
    let s = scan(source)?;
    let addr = format!("slide[{}]", index);

    let mut shapes = Vec::new();
    for (ord, idx) in shape_indices(&s).into_iter().enumerate() {
        let shape_addr = format!("{}/sp[{}]", addr, ord);
        let el = &s.elements[idx];

        let name = s
            .first_descendant(idx, "cNvPr")
            .and_then(|i| s.elements[i].attr(source, "name"))
            .unwrap_or_else(|| format!("{} {}", el.local, ord + 1));

        let placeholder = s
            .first_descendant(idx, "ph")
            .and_then(|i| s.elements[i].attr(source, "type"));

        let bbox = read_bbox(&s, source, idx);

        let mut paragraphs = Vec::new();
        if let Some(body) = s.first_descendant(idx, "txBody") {
            for (p_ord, &p) in s.descendants_by_local(body, "p").iter().enumerate() {
                let text: String = s
                    .descendants_by_local(p, "t")
                    .iter()
                    .map(|&t| unescape(s.elements[t].inner.of(source)))
                    .collect();
                let level = s
                    .first_descendant(p, "pPr")
                    .and_then(|pr| s.elements[pr].attr(source, "lvl"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                paragraphs.push(Paragraph {
                    addr: format!("{}/p[{}]", shape_addr, p_ord),
                    text,
                    level,
                });
            }
        }

        let image_rel = s
            .first_descendant(idx, "blip")
            .and_then(|i| s.elements[i].attr(source, "r:embed"));

        shapes.push(Shape {
            addr: shape_addr,
            kind: el.local.clone(),
            name,
            placeholder,
            bbox,
            paragraphs,
            image_rel,
        });
    }

    Ok(Slide { addr, index, part: part.to_string(), width, height, shapes })
}

/// Top-level drawable elements of the shape tree, in document order.
///
/// Nested shapes inside a group are deliberately NOT given their own addresses:
/// their coordinates are relative to the group's own transform, so exposing
/// them as if they were slide-level would let a drag move them to the wrong
/// place. A group is edited as one object.
fn shape_indices(s: &XmlScan) -> Vec<usize> {
    let Some(tree) = s.by_local("spTree").into_iter().next() else {
        return Vec::new();
    };
    let mut out: Vec<usize> = s
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.parent == Some(tree) && matches!(e.local.as_str(), "sp" | "pic" | "graphicFrame" | "grpSp")
        })
        .map(|(i, _)| i)
        .collect();
    out.sort_by_key(|&i| s.elements[i].outer.start);
    out
}

fn read_bbox(s: &XmlScan, source: &str, shape: usize) -> Option<BBox> {
    let xfrm = s.first_descendant(shape, "xfrm")?;
    let off = s.first_descendant(xfrm, "off")?;
    let ext = s.first_descendant(xfrm, "ext")?;
    Some(BBox {
        x: s.elements[off].attr(source, "x")?.parse().ok()?,
        y: s.elements[off].attr(source, "y")?.parse().ok()?,
        cx: s.elements[ext].attr(source, "cx")?.parse().ok()?,
        cy: s.elements[ext].attr(source, "cy")?.parse().ok()?,
    })
}

// ---------------------------------------------------------------- writing

pub fn apply(bytes: &[u8], patches: &[DraftPatch]) -> Result<Vec<u8>> {
    let mut pkg = OoxmlPackage::open(bytes)?;
    let parts = slide_parts(&pkg)?;

    let mut by_slide: std::collections::HashMap<usize, Vec<&DraftPatch>> = Default::default();
    for patch in patches {
        let addr = match patch {
            DraftPatch::SetShapeText { addr, .. }
            | DraftPatch::SetShapeBox { addr, .. }
            | DraftPatch::DeleteShape { addr } => addr,
            other => return Err(wrong_format(other, "pptx")),
        };
        let slide = parse_addr(addr)?.0;
        if slide >= parts.len() {
            return Err(anyhow!(
                "'{}' refers to slide {}, but this presentation has {}. \
                 Reopen the draft to pick up the current version.",
                addr,
                slide,
                parts.len()
            ));
        }
        by_slide.entry(slide).or_default().push(patch);
    }

    for (slide_idx, slide_patches) in by_slide {
        let part = parts[slide_idx].clone();
        let source = pkg.part_str(&part)?.to_string();
        let s = scan(&source)?;
        let shapes = shape_indices(&s);

        let mut splices = Vec::new();
        for patch in slide_patches {
            match patch {
                DraftPatch::SetShapeText { addr, text } => {
                    let (_, shape_ord, para) = parse_addr(addr)?;
                    let shape = resolve_shape(&shapes, shape_ord, addr)?;
                    splices.extend(set_text(&s, shape, para.unwrap_or(0), text, addr)?);
                }
                DraftPatch::SetShapeBox { addr, x, y, cx, cy } => {
                    let (_, shape_ord, _) = parse_addr(addr)?;
                    let shape = resolve_shape(&shapes, shape_ord, addr)?;
                    splices.push(set_box(&source, &s, shape, *x, *y, *cx, *cy, addr)?);
                }
                DraftPatch::DeleteShape { addr } => {
                    let (_, shape_ord, _) = parse_addr(addr)?;
                    let shape = resolve_shape(&shapes, shape_ord, addr)?;
                    splices.push(Splice { span: s.elements[shape].outer, replacement: String::new() });
                }
                other => return Err(wrong_format(other, "pptx")),
            }
        }

        let patched = apply_splices(&source, splices)?;
        pkg.replace_part(&part, patched.into_bytes())?;
    }

    pkg.save()
}

/// Set one paragraph's text within a shape.
///
/// As in DOCX, PowerPoint splits a line into several `a:t` runs at formatting
/// boundaries. The whole line goes into the first and the rest are emptied,
/// which keeps line breaks and run-level properties in the file even though the
/// editor presents the paragraph as one string.
fn set_text(
    s: &XmlScan,
    shape: usize,
    para_ord: usize,
    text: &str,
    addr: &str,
) -> Result<Vec<Splice>> {
    let body = s.first_descendant(shape, "txBody").ok_or_else(|| {
        anyhow!(
            "'{}' is not a text shape, so it has no text to set. \
             (Pictures, charts and diagrams are positioned but not typed into.)",
            addr
        )
    })?;

    let paragraphs = s.descendants_by_local(body, "p");
    let p = paragraphs.get(para_ord).copied().ok_or_else(|| {
        anyhow!(
            "'{}' no longer exists (that shape has {} paragraphs). \
             Reopen the draft to pick up the current version.",
            addr,
            paragraphs.len()
        )
    })?;

    let ts = s.descendants_by_local(p, "t");
    if ts.is_empty() {
        // An empty placeholder the user has just typed into: it has a
        // paragraph but no run yet.
        let at = s.elements[p].inner.end;
        return Ok(vec![Splice {
            span: Span { start: at, end: at },
            replacement: format!("<a:r><a:t>{}</a:t></a:r>", escape(text)),
        }]);
    }

    let mut splices = vec![Splice {
        span: s.elements[ts[0]].inner,
        replacement: escape(text),
    }];
    for &t in &ts[1..] {
        splices.push(Splice { span: s.elements[t].inner, replacement: String::new() });
    }
    Ok(splices)
}

/// Rewrite a shape's transform.
///
/// Only `a:off` and `a:ext` are replaced, each as its own splice, so rotation
/// and flip attributes on the enclosing `a:xfrm` survive a drag or resize.
fn set_box(
    source: &str,
    s: &XmlScan,
    shape: usize,
    x: i64,
    y: i64,
    cx: i64,
    cy: i64,
    addr: &str,
) -> Result<Splice> {
    if cx <= 0 || cy <= 0 {
        return Err(anyhow!(
            "'{}' cannot be given a zero or negative size ({}x{} EMU).",
            addr, cx, cy
        ));
    }

    let sp_pr = s
        .first_descendant(shape, "spPr")
        .or_else(|| s.first_descendant(shape, "grpSpPr"))
        .or_else(|| s.first_descendant(shape, "xfrm"))
        .ok_or_else(|| {
            anyhow!("'{}' has no shape properties, so it cannot be moved.", addr)
        })?;

    let xfrm = s.first_descendant(shape, "xfrm");
    let replacement = format!(
        "<a:off x=\"{}\" y=\"{}\"/><a:ext cx=\"{}\" cy=\"{}\"/>",
        x, y, cx, cy
    );

    match xfrm {
        Some(xfrm) => {
            // Replace the whole xfrm content: any child other than off/ext
            // (chOff/chExt on groups) is re-derived by PowerPoint from these.
            let kept: String = {
                let inner = s.elements[xfrm].inner;
                let mut drop: Vec<Span> = s
                    .elements
                    .iter()
                    .filter(|e| e.parent == Some(xfrm) && matches!(e.local.as_str(), "off" | "ext"))
                    .map(|e| e.outer)
                    .collect();
                drop.sort_by_key(|d| d.start);
                let mut out = String::new();
                let mut cursor = inner.start;
                for d in drop {
                    if d.start >= cursor {
                        out.push_str(&source[cursor..d.start]);
                        cursor = d.end;
                    }
                }
                out.push_str(&source[cursor..inner.end]);
                out
            };
            Ok(Splice {
                span: s.elements[xfrm].inner,
                replacement: format!("{}{}", replacement, kept),
            })
        }
        None => {
            // The shape inherited its position from the layout; give it an
            // explicit transform, which must lead spPr.
            let at = s.elements[sp_pr].inner.start;
            Ok(Splice {
                span: Span { start: at, end: at },
                replacement: format!("<a:xfrm>{}</a:xfrm>", replacement),
            })
        }
    }
}

// ---------------------------------------------------------------- addressing

/// `slide[2]`, `slide[2]/sp[1]` or `slide[2]/sp[1]/p[0]`.
fn parse_addr(addr: &str) -> Result<(usize, usize, Option<usize>)> {
    let mut parts = addr.split('/');
    let slide = parts
        .next()
        .and_then(|s| s.strip_prefix("slide[")?.strip_suffix(']')?.parse().ok())
        .ok_or_else(|| bad_addr(addr))?;
    let shape = parts
        .next()
        .and_then(|s| s.strip_prefix("sp[")?.strip_suffix(']')?.parse().ok())
        .ok_or_else(|| bad_addr(addr))?;
    let para = match parts.next() {
        None => None,
        Some(p) => Some(
            p.strip_prefix("p[")
                .and_then(|s| s.strip_suffix(']'))
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| bad_addr(addr))?,
        ),
    };
    Ok((slide, shape, para))
}

fn bad_addr(addr: &str) -> anyhow::Error {
    anyhow!(
        "'{}' is not a valid address; expected 'slide[2]/sp[1]' or 'slide[2]/sp[1]/p[0]'",
        addr
    )
}

fn resolve_shape(shapes: &[usize], ord: usize, addr: &str) -> Result<usize> {
    shapes.get(ord).copied().ok_or_else(|| {
        anyhow!(
            "'{}' no longer exists (that slide has {} shapes). \
             Reopen the draft to pick up the current version.",
            addr,
            shapes.len()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_workspace::ooxml::package::fixtures;

    fn slides(bytes: &[u8]) -> Vec<Slide> {
        match build(bytes, 1).unwrap() {
            DraftViewModel::Pptx { slides, .. } => slides,
            other => panic!("expected a pptx model, got {:?}", other),
        }
    }

    #[test]
    fn slides_shapes_and_geometry_are_read_with_stable_addresses() {
        let s = slides(&fixtures::pptx());
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].part, "ppt/slides/slide1.xml");
        assert_eq!(s[0].width, 12_192_000);

        let title = &s[0].shapes[0];
        assert_eq!(title.addr, "slide[0]/sp[0]");
        assert_eq!(title.name, "Title 1");
        assert_eq!(title.placeholder.as_deref(), Some("title"));
        assert_eq!(title.paragraphs[0].text, "Deal Overview");

        let bbox = title.bbox.unwrap();
        assert_eq!((bbox.x, bbox.y), (838_200, 365_125));
        assert_eq!((bbox.cx, bbox.cy), (10_515_600, 1_325_563));
    }

    #[test]
    fn slide_order_follows_the_presentation_not_the_filenames() {
        let s = slides(&fixtures::pptx());
        assert_eq!(s[0].shapes[0].paragraphs[0].text, "Deal Overview");
        assert_eq!(s[1].shapes[0].paragraphs[0].text, "Risks");
    }

    #[test]
    fn editing_slide_text_touches_only_that_slide_part() {
        let original = fixtures::pptx();
        let saved = apply(
            &original,
            &[DraftPatch::SetShapeText {
                addr: "slide[0]/sp[0]".into(),
                text: "Transaction Overview".into(),
            }],
        )
        .unwrap();

        let before = OoxmlPackage::open(&original).unwrap();
        let after = OoxmlPackage::open(&saved).unwrap();
        for name in before.part_names() {
            if name == "ppt/slides/slide1.xml" {
                continue;
            }
            assert_eq!(
                before.part(name).unwrap(),
                after.part(name).unwrap(),
                "part '{}' must be untouched by a slide-1 text edit",
                name
            );
        }
        assert_eq!(slides(&saved)[0].shapes[0].paragraphs[0].text, "Transaction Overview");
        assert_eq!(slides(&saved)[1].shapes[0].paragraphs[0].text, "Risks");
    }

    #[test]
    fn moving_and_resizing_a_shape_writes_the_new_transform() {
        let saved = apply(
            &fixtures::pptx(),
            &[DraftPatch::SetShapeBox {
                addr: "slide[0]/sp[1]".into(),
                x: 100_000,
                y: 200_000,
                cx: 3_000_000,
                cy: 1_000_000,
            }],
        )
        .unwrap();
        let b = slides(&saved)[0].shapes[1].bbox.unwrap();
        assert_eq!((b.x, b.y, b.cx, b.cy), (100_000, 200_000, 3_000_000, 1_000_000));
        // The other shape on the slide is unmoved.
        assert_eq!(slides(&saved)[0].shapes[0].bbox.unwrap().x, 838_200);
    }

    #[test]
    fn a_shape_cannot_be_collapsed_to_nothing() {
        let err = apply(
            &fixtures::pptx(),
            &[DraftPatch::SetShapeBox {
                addr: "slide[0]/sp[0]".into(), x: 0, y: 0, cx: 0, cy: 100,
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("zero or negative"), "{}", err);
    }

    #[test]
    fn deleting_a_shape_leaves_the_others_addressable() {
        let saved = apply(
            &fixtures::pptx(),
            &[DraftPatch::DeleteShape { addr: "slide[0]/sp[0]".into() }],
        )
        .unwrap();
        let s = slides(&saved);
        assert_eq!(s[0].shapes.len(), 1);
        assert_eq!(s[0].shapes[0].addr, "slide[0]/sp[0]", "addresses reindex");
        assert_eq!(s[0].shapes[0].paragraphs[0].text, "Closing scheduled for Q1.");
    }

    #[test]
    fn text_with_markup_characters_survives_a_round_trip() {
        let tricky = "Risk & reward <material> \"terms\"";
        let saved = apply(
            &fixtures::pptx(),
            &[DraftPatch::SetShapeText { addr: "slide[1]/sp[0]".into(), text: tricky.into() }],
        )
        .unwrap();
        assert_eq!(slides(&saved)[1].shapes[0].paragraphs[0].text, tricky);
    }

    #[test]
    fn an_out_of_range_slide_is_refused_with_advice() {
        let err = apply(
            &fixtures::pptx(),
            &[DraftPatch::SetShapeText { addr: "slide[9]/sp[0]".into(), text: "x".into() }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("has 2"), "{}", err);
        assert!(err.contains("Reopen the draft"), "{}", err);
    }

    #[test]
    fn a_malformed_address_names_the_expected_forms() {
        let err = apply(
            &fixtures::pptx(),
            &[DraftPatch::SetShapeText { addr: "slide2/shape1".into(), text: "x".into() }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("slide[2]/sp[1]"), "{}", err);
    }

    #[test]
    fn a_docx_patch_is_refused_before_anything_is_written() {
        let err = apply(
            &fixtures::pptx(),
            &[DraftPatch::DeleteParagraph { addr: "body/p[0]".into() }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("DeleteParagraph"), "{}", err);
        assert!(err.contains("PPTX"), "{}", err);
    }
}
