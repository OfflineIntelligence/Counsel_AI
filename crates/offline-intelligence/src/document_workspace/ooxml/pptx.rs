//! PPTX: read slides into positioned shapes, and splice text, formatting,
//! geometry and new objects back into the individual slide parts.
//!
//! # Addressing
//!
//! `slide[2]/sp[1]` for a shape, `slide[2]/sp[1]/p[0]` for a paragraph inside
//! it. Paragraphs are numbered across every text body the shape holds, so a
//! table's cells are addressed exactly like a text box's lines. Slide order
//! comes from `p:sldIdLst` in `presentation.xml`, resolved through
//! relationships — never from the `slideN.xml` filename, which PowerPoint
//! renumbers freely as slides are added and removed.
//!
//! # Patches apply in sequence
//!
//! Adding or deleting a slide renumbers every later slide, so each patch is
//! resolved against the presentation as the previous one left it.
//!
//! # Rendering honesty
//!
//! SmartArt, charts, 3-D effects, transitions and animations are read past and
//! rendered as a labelled placeholder by the UI. That is a *display* limitation
//! only: an effect we never address is XML we never patch.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::package::{read_relationships, rels_part_for, resolve_target, OoxmlPackage};
use super::spans::{apply_splices, escape, escape_attr, scan, unescape, Span, Splice, XmlScan};
use crate::document_workspace::view_model::{wrong_format, DraftPatch, DraftViewModel, TextFormat};

const PRESENTATION_PART: &str = "ppt/presentation.xml";
const PRESENTATION_RELS_PART: &str = "ppt/_rels/presentation.xml.rels";

const REL_SLIDE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
const REL_IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
const CT_SLIDE: &str = "application/vnd.openxmlformats-officedocument.presentationml.slide+xml";
const NS_A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const NS_P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const NS_R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
/// PowerPoint's built-in "Medium Style 2 – Accent 1"; needs no tableStyles entry.
const TABLE_STYLE: &str = "{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}";

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
    /// `None` when the shape inherits its position from the layout.
    pub bbox: Option<BBox>,
    pub paragraphs: Vec<Paragraph>,
    /// Relationship id of the embedded image, for `pic` shapes.
    pub image_rel: Option<String>,
    /// The picture itself, for `pic` shapes whose media could be read.
    #[serde(default)]
    pub image: Option<String>,
    /// Solid fill `RRGGBB`, when the shape sets one.
    #[serde(default)]
    pub fill: Option<String>,
    /// Preset geometry, e.g. `rect`, `ellipse`.
    #[serde(default)]
    pub geometry: Option<String>,
    /// For a table: rows of cells, each the indices into `paragraphs` it holds.
    #[serde(default)]
    pub table: Option<Vec<Vec<Vec<usize>>>>,
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
    #[serde(default)]
    pub align: Option<String>,
    /// `bullet` or `number` when the paragraph sets one explicitly.
    #[serde(default)]
    pub list: Option<String>,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub strike: bool,
    /// Points.
    #[serde(default)]
    pub size: Option<f32>,
    #[serde(default)]
    pub font: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

// ---------------------------------------------------------------- reading

pub fn build(bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    let pkg = OoxmlPackage::open(bytes)?;
    let (width, height) = slide_size(&pkg)?;

    let mut slides = Vec::new();
    for (index, part) in slide_parts(&pkg)?.into_iter().enumerate() {
        let source = pkg.part_str(&part)?;
        slides.push(read_slide(&pkg, index, &part, source, width, height)?);
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
    let Some(sz) = s.by_local("sldSz").into_iter().next() else {
        return Ok((12_192_000, 6_858_000));
    };
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
        let target = targets.get(&rid).ok_or_else(|| anyhow!("a slide points at missing relationship {}", rid))?;
        out.push(resolve_target("ppt", target));
    }
    Ok(out)
}

fn children_of(s: &XmlScan, parent: usize) -> Vec<usize> {
    let mut out: Vec<usize> = s.elements.iter().enumerate().filter(|(_, e)| e.parent == Some(parent)).map(|(i, _)| i).collect();
    out.sort_by_key(|&i| s.elements[i].outer.start);
    out
}

fn child(s: &XmlScan, parent: usize, local: &str) -> Option<usize> {
    children_of(s, parent).into_iter().find(|&i| s.elements[i].local == local)
}

/// Every text paragraph of a shape, in order.
fn shape_paragraphs(s: &XmlScan, shape: usize) -> Vec<usize> {
    s.descendants_by_local(shape, "p")
        .into_iter()
        .filter(|&p| s.elements[s.elements[p].parent.unwrap_or(p)].local == "txBody")
        .collect()
}

fn srgb(s: &XmlScan, source: &str, fill: usize) -> Option<String> {
    s.first_descendant(fill, "srgbClr").and_then(|c| s.elements[c].attr(source, "val"))
}

fn read_slide(pkg: &OoxmlPackage, index: usize, part: &str, source: &str, width: i64, height: i64) -> Result<Slide> {
    let s = scan(source)?;
    let addr = format!("slide[{}]", index);
    let slide_dir = part.rsplit_once('/').map(|(d, _)| d).unwrap_or("ppt/slides");
    let rels = pkg.part_str(&rels_part_for(part)).ok().and_then(|r| read_relationships(r).ok()).unwrap_or_default();

    let mut shapes = Vec::new();
    for (ord, idx) in shape_indices(&s).into_iter().enumerate() {
        let shape_addr = format!("{}/sp[{}]", addr, ord);
        let el = &s.elements[idx];

        let name = s
            .first_descendant(idx, "cNvPr")
            .and_then(|i| s.elements[i].attr(source, "name"))
            .unwrap_or_else(|| format!("{} {}", el.local, ord + 1));
        let placeholder = s.first_descendant(idx, "ph").map(|i| s.elements[i].attr(source, "type").unwrap_or_else(|| "body".into()));
        let bbox = read_bbox(&s, source, idx);

        let para_idx = shape_paragraphs(&s, idx);
        let paragraphs = para_idx
            .iter()
            .enumerate()
            .map(|(p_ord, &p)| read_paragraph(&s, source, p, format!("{}/p[{}]", shape_addr, p_ord)))
            .collect();

        let table = s.first_descendant(idx, "tbl").map(|tbl| {
            s.descendants_by_local(tbl, "tr")
                .into_iter()
                .filter(|&tr| s.elements[tr].parent == Some(tbl))
                .map(|tr| {
                    children_of(&s, tr)
                        .into_iter()
                        .filter(|&tc| s.elements[tc].local == "tc")
                        .map(|tc| {
                            para_idx
                                .iter()
                                .enumerate()
                                .filter(|(_, &p)| s.elements[p].outer.start >= s.elements[tc].outer.start && s.elements[p].outer.end <= s.elements[tc].outer.end)
                                .map(|(i, _)| i)
                                .collect()
                        })
                        .collect()
                })
                .collect()
        });

        let image_rel = s.first_descendant(idx, "blip").and_then(|i| s.elements[i].attr(source, "r:embed"));
        let image = if el.local == "pic" {
            image_rel.as_ref().and_then(|rid| {
                let media = resolve_target(slide_dir, rels.get(rid)?);
                let bytes = pkg.part(&media)?;
                let mime = mime_for(media.rsplit('.').next()?)?;
                Some(format!("data:{};base64,{}", mime, BASE64.encode(bytes)))
            })
        } else {
            None
        };

        let sp_pr = child(&s, idx, "spPr");
        let fill = sp_pr.and_then(|pr| child(&s, pr, "solidFill")).and_then(|f| srgb(&s, source, f));
        let geometry = sp_pr.and_then(|pr| child(&s, pr, "prstGeom")).and_then(|g| s.elements[g].attr(source, "prst"));

        shapes.push(Shape {
            addr: shape_addr,
            kind: el.local.clone(),
            name,
            placeholder,
            bbox,
            paragraphs,
            image_rel,
            image,
            fill,
            geometry,
            table,
        });
    }

    Ok(Slide { addr, index, part: part.to_string(), width, height, shapes })
}

fn read_paragraph(s: &XmlScan, source: &str, p: usize, addr: String) -> Paragraph {
    let text: String = s.descendants_by_local(p, "t").iter().map(|&t| unescape(s.elements[t].inner.of(source))).collect();
    let ppr = child(s, p, "pPr");
    let level = ppr.and_then(|pr| s.elements[pr].attr(source, "lvl")).and_then(|v| v.parse().ok()).unwrap_or(0);
    let align = ppr.and_then(|pr| s.elements[pr].attr(source, "algn")).map(|a| match a.as_str() {
        "ctr" => "center".to_string(),
        "r" => "right".to_string(),
        "just" | "dist" => "justify".to_string(),
        _ => "left".to_string(),
    });
    let list = ppr.and_then(|pr| {
        if child(s, pr, "buChar").is_some() {
            Some("bullet".to_string())
        } else if child(s, pr, "buAutoNum").is_some() {
            Some("number".to_string())
        } else {
            None
        }
    });

    // The first run's properties stand for the paragraph; an empty paragraph
    // reports what it will type in.
    let rpr = s
        .descendants_by_local(p, "r")
        .first()
        .and_then(|&r| child(s, r, "rPr"))
        .or_else(|| child(s, p, "endParaRPr"));
    let attr = |name: &str| rpr.and_then(|r| s.elements[r].attr(source, name));
    let on = |name: &str| matches!(attr(name).as_deref(), Some("1") | Some("true"));
    Paragraph {
        addr,
        text,
        level,
        align,
        list,
        bold: on("b"),
        italic: on("i"),
        underline: attr("u").map(|u| u != "none").unwrap_or(false),
        strike: attr("strike").map(|v| v != "noStrike").unwrap_or(false),
        size: attr("sz").and_then(|v| v.parse::<f32>().ok()).map(|v| v / 100.0),
        font: rpr.and_then(|r| child(s, r, "latin")).and_then(|l| s.elements[l].attr(source, "typeface")).filter(|f| !f.starts_with('+')),
        color: rpr.and_then(|r| child(s, r, "solidFill")).and_then(|f| srgb(s, source, f)),
    }
}

fn mime_for(ext: &str) -> Option<&'static str> {
    Some(match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

/// Top-level drawable elements of the shape tree, in document order.
///
/// Nested shapes inside a group are deliberately NOT given their own addresses:
/// their coordinates are relative to the group's own transform. A group is
/// edited as one object.
fn shape_indices(s: &XmlScan) -> Vec<usize> {
    let Some(tree) = s.by_local("spTree").into_iter().next() else {
        return Vec::new();
    };
    children_of(s, tree)
        .into_iter()
        .filter(|&i| matches!(s.elements[i].local.as_str(), "sp" | "pic" | "graphicFrame" | "grpSp" | "cxnSp"))
        .collect()
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
    for patch in patches {
        apply_one(&mut pkg, patch)?;
    }
    pkg.save()
}

fn slide_part_for(pkg: &OoxmlPackage, slide: usize, addr: &str) -> Result<String> {
    let parts = slide_parts(pkg)?;
    parts.get(slide).cloned().ok_or_else(|| {
        anyhow!(
            "'{}' refers to slide {}, but this presentation has {}. \
             Reopen the draft to pick up the current version.",
            addr,
            slide,
            parts.len()
        )
    })
}

fn apply_one(pkg: &mut OoxmlPackage, patch: &DraftPatch) -> Result<()> {
    match patch {
        DraftPatch::AddSlide { after } => return add_slide(pkg, *after, None),
        DraftPatch::DuplicateSlide { slide } => return add_slide(pkg, *slide, Some(*slide)),
        DraftPatch::DeleteSlide { slide } => return delete_slide(pkg, *slide),
        _ => {}
    }

    let addr = match patch {
        DraftPatch::SetShapeText { addr, .. }
        | DraftPatch::SetShapeTextFormat { addr, .. }
        | DraftPatch::InsertShapeParagraph { addr, .. }
        | DraftPatch::SetShapeFill { addr, .. }
        | DraftPatch::SetShapeBox { addr, .. }
        | DraftPatch::DeleteShape { addr }
        | DraftPatch::InsertTextBox { addr, .. }
        | DraftPatch::InsertShape { addr, .. }
        | DraftPatch::InsertSlideTable { addr, .. }
        | DraftPatch::InsertImage { addr, .. } => addr,
        other => return Err(wrong_format(other, "pptx")),
    };
    let slide = parse_slide(addr)?;
    let part = slide_part_for(pkg, slide, addr)?;
    let source = pkg.part_str(&part)?.to_string();
    let s = scan(&source)?;
    let shapes = shape_indices(&s);

    let splices: Vec<Splice> = match patch {
        DraftPatch::SetShapeText { addr, text } => {
            let (_, shape_ord, para) = parse_addr(addr)?;
            let shape = resolve_shape(&shapes, shape_ord, addr)?;
            set_text(&s, shape, para.unwrap_or(0), text, addr)?
        }
        DraftPatch::SetShapeTextFormat { addr, format, align, list } => {
            let (_, shape_ord, para) = parse_addr(addr)?;
            let shape = resolve_shape(&shapes, shape_ord, addr)?;
            let paras = shape_paragraphs(&s, shape);
            let targets: Vec<usize> = match para {
                Some(p) => vec![*paras.get(p).ok_or_else(|| stale(addr, paras.len(), "paragraphs"))?],
                None => paras,
            };
            let mut out = Vec::new();
            for p in targets {
                out.extend(format_paragraph(&source, &s, p, format, align.as_deref(), list.as_deref()));
            }
            out
        }
        DraftPatch::InsertShapeParagraph { addr, text } => {
            let (_, shape_ord, para) = parse_addr(addr)?;
            let shape = resolve_shape(&shapes, shape_ord, addr)?;
            let paras = shape_paragraphs(&s, shape);
            let p = *paras.get(para.unwrap_or(paras.len().saturating_sub(1))).ok_or_else(|| stale(addr, paras.len(), "paragraphs"))?;
            let a = prefix_of(&s.elements[p].qname, "a");
            let ppr = child(&s, p, "pPr").map(|i| s.elements[i].outer.of(&source).to_string()).unwrap_or_default();
            let rpr = s
                .descendants_by_local(p, "r")
                .first()
                .and_then(|&r| child(&s, r, "rPr"))
                .map(|i| s.elements[i].outer.of(&source).to_string())
                .unwrap_or_else(|| format!("<{a}:rPr lang=\"en-US\" dirty=\"0\"/>"));
            let xml = format!("<{a}:p>{ppr}<{a}:r>{rpr}<{a}:t>{}</{a}:t></{a}:r></{a}:p>", escape(text));
            let end = s.elements[p].outer.end;
            vec![Splice { span: Span { start: end, end }, replacement: xml }]
        }
        DraftPatch::SetShapeFill { addr, color } => {
            let (_, shape_ord, _) = parse_addr(addr)?;
            let shape = resolve_shape(&shapes, shape_ord, addr)?;
            vec![set_fill(&source, &s, shape, color, addr)?]
        }
        DraftPatch::SetShapeBox { addr, x, y, cx, cy } => {
            let (_, shape_ord, _) = parse_addr(addr)?;
            let shape = resolve_shape(&shapes, shape_ord, addr)?;
            vec![set_box(&source, &s, shape, *x, *y, *cx, *cy, addr)?]
        }
        DraftPatch::DeleteShape { addr } => {
            let (_, shape_ord, _) = parse_addr(addr)?;
            let shape = resolve_shape(&shapes, shape_ord, addr)?;
            vec![Splice { span: s.elements[shape].outer, replacement: String::new() }]
        }
        DraftPatch::InsertTextBox { x, y, cx, cy, text, .. } => {
            let (p, a) = prefixes(&source, &s);
            let id = next_shape_id(&source, &s);
            let xml = format!(
                "<{p}:sp{ns}><{p}:nvSpPr><{p}:cNvPr id=\"{id}\" name=\"TextBox {id}\"/><{p}:cNvSpPr txBox=\"1\"/><{p}:nvPr/></{p}:nvSpPr>\
<{p}:spPr>{xfrm}<{a}:prstGeom prst=\"rect\"><{a}:avLst/></{a}:prstGeom><{a}:noFill/></{p}:spPr>\
<{p}:txBody><{a}:bodyPr wrap=\"square\" rtlCol=\"0\"><{a}:spAutoFit/></{a}:bodyPr><{a}:lstStyle/>\
<{a}:p><{a}:r><{a}:rPr lang=\"en-US\" sz=\"1800\" dirty=\"0\"/><{a}:t>{}</{a}:t></{a}:r></{a}:p></{p}:txBody></{p}:sp>",
                escape(text),
                ns = inline_ns(&source, &a),
                xfrm = xfrm(&a, *x, *y, *cx, *cy)?,
            );
            vec![append_to_tree(&source, &s, xml)?]
        }
        DraftPatch::InsertShape { geometry, x, y, cx, cy, fill, .. } => {
            let allowed = ["rect", "roundRect", "ellipse", "triangle", "rightArrow", "leftArrow", "line", "star5", "chevron", "hexagon", "flowChartProcess", "wedgeRectCallout"];
            if !allowed.contains(&geometry.as_str()) {
                return Err(anyhow!("'{}' is not a shape this editor can draw.", geometry));
            }
            let (p, a) = prefixes(&source, &s);
            let id = next_shape_id(&source, &s);
            let colour = fill.as_deref().and_then(clean_hex).unwrap_or_else(|| "4472C4".into());
            let body = if geometry == "line" {
                format!("<{a}:ln w=\"19050\"><{a}:solidFill><{a}:srgbClr val=\"{colour}\"/></{a}:solidFill></{a}:ln>")
            } else {
                format!("<{a}:solidFill><{a}:srgbClr val=\"{colour}\"/></{a}:solidFill>")
            };
            let xml = format!(
                "<{p}:sp{ns}><{p}:nvSpPr><{p}:cNvPr id=\"{id}\" name=\"Shape {id}\"/><{p}:cNvSpPr/><{p}:nvPr/></{p}:nvSpPr>\
<{p}:spPr>{xfrm}<{a}:prstGeom prst=\"{geometry}\"><{a}:avLst/></{a}:prstGeom>{body}</{p}:spPr>\
<{p}:txBody><{a}:bodyPr rtlCol=\"0\" anchor=\"ctr\"/><{a}:lstStyle/><{a}:p><{a}:pPr algn=\"ctr\"/><{a}:endParaRPr lang=\"en-US\" dirty=\"0\"/></{a}:p></{p}:txBody></{p}:sp>",
                ns = inline_ns(&source, &a),
                xfrm = xfrm(&a, *x, *y, *cx, *cy)?,
            );
            vec![append_to_tree(&source, &s, xml)?]
        }
        DraftPatch::InsertSlideTable { rows, cols, x, y, cx, cy, .. } => {
            if *rows == 0 || *cols == 0 || *rows > 50 || *cols > 20 {
                return Err(anyhow!("A slide table needs between 1 and 50 rows and 1 and 20 columns."));
            }
            let (p, a) = prefixes(&source, &s);
            let id = next_shape_id(&source, &s);
            let (cx, cy) = ((*cx).max(1), (*cy).max(1));
            let col_w = cx / *cols as i64;
            let row_h = (cy / *rows as i64).max(370_840);
            let cell = format!(
                "<{a}:tc><{a}:txBody><{a}:bodyPr/><{a}:lstStyle/><{a}:p><{a}:endParaRPr lang=\"en-US\" dirty=\"0\"/></{a}:p></{a}:txBody><{a}:tcPr/></{a}:tc>"
            );
            let row = format!("<{a}:tr h=\"{row_h}\">{}</{a}:tr>", cell.repeat(*cols));
            let grid: String = (0..*cols).map(|_| format!("<{a}:gridCol w=\"{col_w}\"/>")).collect();
            let xml = format!(
                "<{p}:graphicFrame{ns}><{p}:nvGraphicFramePr><{p}:cNvPr id=\"{id}\" name=\"Table {id}\"/>\
<{p}:cNvGraphicFramePr><{a}:graphicFrameLocks noGrp=\"1\"/></{p}:cNvGraphicFramePr><{p}:nvPr/></{p}:nvGraphicFramePr>\
<{p}:xfrm><{a}:off x=\"{x}\" y=\"{y}\"/><{a}:ext cx=\"{cx}\" cy=\"{cy}\"/></{p}:xfrm>\
<{a}:graphic><{a}:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/table\">\
<{a}:tbl><{a}:tblPr firstRow=\"1\" bandRow=\"1\"><{a}:tableStyleId>{TABLE_STYLE}</{a}:tableStyleId></{a}:tblPr>\
<{a}:tblGrid>{grid}</{a}:tblGrid>{}</{a}:tbl></{a}:graphicData></{a}:graphic></{p}:graphicFrame>",
                row.repeat(*rows),
                ns = inline_ns(&source, &a),
            );
            vec![append_to_tree(&source, &s, xml)?]
        }
        DraftPatch::InsertImage { data_base64, file_name, width_emu, height_emu, x, y, .. } => {
            let (p, a) = prefixes(&source, &s);
            let id = next_shape_id(&source, &s);
            let data = data_base64.split_once(',').map(|(_, d)| d).unwrap_or(data_base64);
            let bytes = BASE64.decode(data.trim()).map_err(|e| anyhow!("That image could not be read: {}", e))?;
            let mut ext = file_name.rsplit('.').next().unwrap_or("png").to_ascii_lowercase();
            if ext == "jpeg" {
                ext = "jpg".into();
            }
            let mime = mime_for(&ext).ok_or_else(|| anyhow!("'{}' is not a picture format PowerPoint can embed.", file_name))?;
            let media = pkg.unused_part_name("ppt/media", "counsel_image", &ext);
            pkg.add_part(&media, bytes)?;
            pkg.ensure_default_content_type(&ext, mime)?;
            let rid = pkg.add_relationship(&rels_part_for(&part), REL_IMAGE, &format!("../media/{}", media.rsplit('/').next().unwrap()))?;
            let xml = format!(
                "<{p}:pic{ns}><{p}:nvPicPr><{p}:cNvPr id=\"{id}\" name=\"{name}\"/><{p}:cNvPicPr><{a}:picLocks noChangeAspect=\"1\"/></{p}:cNvPicPr><{p}:nvPr/></{p}:nvPicPr>\
<{p}:blipFill><{a}:blip r:embed=\"{rid}\" xmlns:r=\"{NS_R}\"/><{a}:stretch><{a}:fillRect/></{a}:stretch></{p}:blipFill>\
<{p}:spPr>{xfrm}<{a}:prstGeom prst=\"rect\"><{a}:avLst/></{a}:prstGeom></{p}:spPr></{p}:pic>",
                name = escape_attr(file_name),
                ns = inline_ns(&source, &a),
                xfrm = xfrm(&a, x.unwrap_or(914_400), y.unwrap_or(914_400), *width_emu, *height_emu)?,
            );
            vec![append_to_tree(&source, &s, xml)?]
        }
        _ => unreachable!("slide-level patches returned above"),
    };

    let patched = apply_splices(&source, splices)?;
    pkg.replace_part(&part, patched.into_bytes())
}

fn stale(addr: &str, n: usize, what: &str) -> anyhow::Error {
    anyhow!("'{}' no longer exists (that shape has {} {}). Reopen the draft to pick up the current version.", addr, n, what)
}

fn prefix_of(qname: &str, default: &str) -> String {
    qname.split_once(':').map(|(p, _)| p.to_string()).unwrap_or_else(|| default.into())
}

/// (presentationml prefix, drawingml prefix) as this slide writes them.
fn prefixes(source: &str, s: &XmlScan) -> (String, String) {
    let p = s.by_local("spTree").into_iter().next().map(|t| prefix_of(&s.elements[t].qname, "p")).unwrap_or_else(|| "p".into());
    let root = s.elements.first().map(|e| e.start_tag.of(source)).unwrap_or("");
    let a = root
        .split("xmlns:")
        .skip(1)
        .find_map(|decl| {
            let (name, rest) = decl.split_once("=\"")?;
            rest.starts_with(NS_A).then(|| name.to_string())
        })
        .unwrap_or_else(|| "a".into());
    (p, a)
}

/// Declares the drawingml namespace on a new element when the slide root does not.
fn inline_ns(source: &str, a: &str) -> String {
    let root = scan(source).ok().and_then(|s| s.elements.first().map(|e| e.start_tag.of(source).to_string())).unwrap_or_default();
    if root.contains(&format!("xmlns:{}=", a)) {
        String::new()
    } else {
        format!(" xmlns:{}=\"{}\"", a, NS_A)
    }
}

fn xfrm(a: &str, x: i64, y: i64, cx: i64, cy: i64) -> Result<String> {
    if cx <= 0 || cy <= 0 {
        return Err(anyhow!("A new object needs a positive size."));
    }
    Ok(format!("<{a}:xfrm><{a}:off x=\"{x}\" y=\"{y}\"/><{a}:ext cx=\"{cx}\" cy=\"{cy}\"/></{a}:xfrm>"))
}

fn next_shape_id(source: &str, s: &XmlScan) -> u64 {
    s.by_local("cNvPr").iter().filter_map(|&i| s.elements[i].attr(source, "id")?.parse::<u64>().ok()).max().unwrap_or(1) + 1
}

/// Insert a new object at the top of the z-order: last in the shape tree, but
/// before any `extLst`, which must stay last.
fn append_to_tree(source: &str, s: &XmlScan, xml: String) -> Result<Splice> {
    let tree = s.by_local("spTree").into_iter().next().ok_or_else(|| anyhow!("This slide has no shape tree."))?;
    let at = child(s, tree, "extLst").map(|e| s.elements[e].outer.start).unwrap_or(s.elements[tree].inner.end);
    let _ = source;
    Ok(Splice { span: Span { start: at, end: at }, replacement: xml })
}

fn clean_hex(value: &str) -> Option<String> {
    let hex = value.trim().trim_start_matches('#');
    (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| hex.to_ascii_uppercase())
}

fn set_attr(tag: &str, name: &str, value: Option<&str>) -> String {
    let needle = format!(" {}=\"", name);
    let base = match tag.find(&needle) {
        Some(at) => {
            let from = at + needle.len();
            let end = tag[from..].find('"').map(|e| from + e + 1).unwrap_or(tag.len());
            format!("{}{}", &tag[..at], &tag[end..])
        }
        None => tag.to_string(),
    };
    match value {
        None => base,
        Some(v) => {
            let close = if base.ends_with("/>") { base.len() - 2 } else { base.len() - 1 };
            format!("{} {}=\"{}\"{}", base[..close].trim_end(), name, escape_attr(v), &base[close..])
        }
    }
}

/// Children order inside `a:rPr` / `a:endParaRPr` (CT_TextCharacterProperties).
const RPR_ORDER: &[&str] = &[
    "ln", "noFill", "solidFill", "gradFill", "blipFill", "pattFill", "grpFill", "effectLst", "effectDag",
    "highlight", "uLnTx", "uLn", "uFillTx", "uFill", "latin", "ea", "cs", "sym", "hlinkClick",
    "hlinkMouseOver", "rtl", "extLst",
];
/// Children order inside `a:pPr` (CT_TextParagraphProperties).
const PPR_ORDER: &[&str] = &[
    "lnSpc", "spcBef", "spcAft", "buClrTx", "buClr", "buSzTx", "buSzPct", "buSzPts", "buFontTx", "buFont",
    "buNone", "buAutoNum", "buChar", "buBlip", "tabLst", "defRPr", "extLst",
];

fn element_parts(source: &str, s: &XmlScan, el: usize) -> (String, Vec<(String, String)>) {
    let tag = s.elements[el].start_tag.of(source).trim_end_matches("/>").trim_end_matches('>').trim_end().to_string() + ">";
    let kids = children_of(s, el).into_iter().map(|i| (s.elements[i].local.clone(), s.elements[i].outer.of(source).to_string())).collect();
    (tag, kids)
}

fn assemble(tag: &str, qname: &str, mut kids: Vec<(String, String)>, order: &[&str]) -> String {
    kids.sort_by_key(|(l, _)| order.iter().position(|o| o == l).unwrap_or(order.len()));
    if kids.is_empty() {
        return format!("{}/>", tag.trim_end_matches('>'));
    }
    format!("{}{}</{}>", tag, kids.into_iter().map(|(_, x)| x).collect::<String>(), qname)
}

fn rewrite_rpr(a: &str, tag: String, mut kids: Vec<(String, String)>, f: &TextFormat) -> (String, Vec<(String, String)>) {
    let mut tag = tag;
    let flag = |on: bool| if on { "1" } else { "0" };
    if let Some(v) = f.bold {
        tag = set_attr(&tag, "b", Some(flag(v)));
    }
    if let Some(v) = f.italic {
        tag = set_attr(&tag, "i", Some(flag(v)));
    }
    if let Some(v) = f.underline {
        tag = set_attr(&tag, "u", Some(if v { "sng" } else { "none" }));
    }
    if let Some(v) = f.strike {
        tag = set_attr(&tag, "strike", Some(if v { "sngStrike" } else { "noStrike" }));
    }
    if let Some(size) = f.size {
        let hundredths = (size * 100.0).round().clamp(100.0, 400_000.0) as i64;
        tag = set_attr(&tag, "sz", (size > 0.0).then(|| hundredths.to_string()).as_deref());
    }
    if let Some(va) = &f.vert_align {
        tag = set_attr(
            &tag,
            "baseline",
            match va.as_str() {
                "superscript" => Some("30000"),
                "subscript" => Some("-25000"),
                _ => None,
            },
        );
    }
    let mut put = |locals: &[&str], item: Option<(&str, String)>| {
        kids.retain(|(l, _)| !locals.contains(&l.as_str()));
        if let Some((l, x)) = item {
            kids.push((l.to_string(), x));
        }
    };
    if let Some(color) = &f.color {
        put(
            &["noFill", "solidFill", "gradFill", "blipFill", "pattFill", "grpFill"],
            clean_hex(color).map(|c| ("solidFill", format!("<{a}:solidFill><{a}:srgbClr val=\"{c}\"/></{a}:solidFill>"))),
        );
    }
    if let Some(h) = &f.highlight {
        put(&["highlight"], clean_hex(h).map(|c| ("highlight", format!("<{a}:highlight><{a}:srgbClr val=\"{c}\"/></{a}:highlight>"))));
    }
    if let Some(font) = &f.font {
        let face = font.trim();
        put(
            &["latin", "cs"],
            (!face.is_empty()).then(|| ("latin", format!("<{a}:latin typeface=\"{}\"/>", escape_attr(face)))),
        );
    }
    (tag, kids)
}

fn format_paragraph(
    source: &str,
    s: &XmlScan,
    p: usize,
    f: &TextFormat,
    align: Option<&str>,
    list: Option<&str>,
) -> Vec<Splice> {
    let a = prefix_of(&s.elements[p].qname, "a");
    let mut splices = Vec::new();

    let targets: Vec<usize> = s
        .descendants_by_local(p, "r")
        .into_iter()
        .filter(|&r| s.elements[r].parent == Some(p))
        .chain(child(s, p, "endParaRPr").into_iter())
        .collect();
    if *f != TextFormat::default() {
        for t in targets {
            if s.elements[t].local == "endParaRPr" {
                let (tag, kids) = element_parts(source, s, t);
                let (tag, kids) = rewrite_rpr(&a, tag, kids, f);
                splices.push(Splice { span: s.elements[t].outer, replacement: assemble(&tag, &s.elements[t].qname, kids, RPR_ORDER) });
                continue;
            }
            match child(s, t, "rPr") {
                Some(rpr) => {
                    let (tag, kids) = element_parts(source, s, rpr);
                    let (tag, kids) = rewrite_rpr(&a, tag, kids, f);
                    splices.push(Splice { span: s.elements[rpr].outer, replacement: assemble(&tag, &s.elements[rpr].qname, kids, RPR_ORDER) });
                }
                None => {
                    let (tag, kids) = rewrite_rpr(&a, format!("<{a}:rPr lang=\"en-US\" dirty=\"0\">"), Vec::new(), f);
                    let at = s.elements[t].inner.start;
                    splices.push(Splice { span: Span { start: at, end: at }, replacement: assemble(&tag, &format!("{a}:rPr"), kids, RPR_ORDER) });
                }
            }
        }
    }

    if align.is_some() || list.is_some() {
        let (mut tag, mut kids) = match child(s, p, "pPr") {
            Some(pr) => element_parts(source, s, pr),
            None => (format!("<{a}:pPr>"), Vec::new()),
        };
        if let Some(al) = align {
            let v = match al {
                "center" => "ctr",
                "right" => "r",
                "justify" => "just",
                _ => "l",
            };
            tag = set_attr(&tag, "algn", Some(v));
        }
        if let Some(kind) = list {
            kids.retain(|(l, _)| !matches!(l.as_str(), "buNone" | "buAutoNum" | "buChar" | "buBlip" | "buFont" | "buFontTx"));
            match kind {
                "bullet" => {
                    kids.push(("buFont".into(), format!("<{a}:buFont typeface=\"Arial\"/>")));
                    kids.push(("buChar".into(), format!("<{a}:buChar char=\"•\"/>")));
                    tag = set_attr(&set_attr(&tag, "marL", Some("342900")), "indent", Some("-342900"));
                }
                "number" => {
                    kids.push(("buAutoNum".into(), format!("<{a}:buAutoNum type=\"arabicPeriod\"/>")));
                    tag = set_attr(&set_attr(&tag, "marL", Some("457200")), "indent", Some("-457200"));
                }
                _ => {
                    kids.push(("buNone".into(), format!("<{a}:buNone/>")));
                    tag = set_attr(&set_attr(&tag, "marL", None), "indent", None);
                }
            }
        }
        let xml = assemble(&tag, &format!("{a}:pPr"), kids, PPR_ORDER);
        match child(s, p, "pPr") {
            Some(pr) => splices.push(Splice { span: s.elements[pr].outer, replacement: xml }),
            None => {
                // `a:pPr` leads the paragraph.
                let at = s.elements[p].inner.start;
                splices.push(Splice { span: Span { start: at, end: at }, replacement: xml });
            }
        }
    }
    splices
}

fn set_fill(source: &str, s: &XmlScan, shape: usize, color: &str, addr: &str) -> Result<Splice> {
    let sp_pr = child(s, shape, "spPr").ok_or_else(|| anyhow!("'{}' has no shape properties to fill.", addr))?;
    let a = prefix_of(&s.elements[sp_pr].qname, "a");
    let a = if a == "p" { "a".to_string() } else { a };
    let (tag, mut kids) = element_parts(source, s, sp_pr);
    kids.retain(|(l, _)| !matches!(l.as_str(), "noFill" | "solidFill" | "gradFill" | "blipFill" | "pattFill" | "grpFill"));
    if let Some(hex) = clean_hex(color) {
        kids.push(("solidFill".into(), format!("<{a}:solidFill><{a}:srgbClr val=\"{hex}\"/></{a}:solidFill>")));
    }
    const SPPR_ORDER: &[&str] = &["xfrm", "custGeom", "prstGeom", "noFill", "solidFill", "gradFill", "blipFill", "pattFill", "grpFill", "ln", "effectLst", "effectDag", "scene3d", "sp3d", "extLst"];
    Ok(Splice { span: s.elements[sp_pr].outer, replacement: assemble(&tag, &s.elements[sp_pr].qname, kids, SPPR_ORDER) })
}

/// Set one paragraph's text within a shape.
///
/// As in DOCX, PowerPoint splits a line into several `a:t` runs at formatting
/// boundaries. The whole line goes into the first and the rest are emptied.
fn set_text(s: &XmlScan, shape: usize, para_ord: usize, text: &str, addr: &str) -> Result<Vec<Splice>> {
    if s.first_descendant(shape, "txBody").is_none() {
        return Err(anyhow!(
            "'{}' is not a text shape, so it has no text to set. \
             (Pictures, charts and diagrams are positioned but not typed into.)",
            addr
        ));
    }

    let paragraphs = shape_paragraphs(s, shape);
    let p = paragraphs.get(para_ord).copied().ok_or_else(|| stale(addr, paragraphs.len(), "paragraphs"))?;
    let a = prefix_of(&s.elements[p].qname, "a");

    let ts = s.descendants_by_local(p, "t");
    if ts.is_empty() {
        // An empty placeholder or table cell: it has a paragraph but no run.
        // The run goes before `a:endParaRPr`, which must stay last.
        let at = child(s, p, "endParaRPr").map(|e| s.elements[e].outer.start).unwrap_or(s.elements[p].inner.end);
        return Ok(vec![Splice {
            span: Span { start: at, end: at },
            replacement: format!("<{a}:r><{a}:rPr lang=\"en-US\" dirty=\"0\"/><{a}:t>{}</{a}:t></{a}:r>", escape(text)),
        }]);
    }

    let mut splices = vec![Splice { span: s.elements[ts[0]].inner, replacement: escape(text) }];
    for &t in &ts[1..] {
        splices.push(Splice { span: s.elements[t].inner, replacement: String::new() });
    }
    Ok(splices)
}

/// Rewrite a shape's transform.
///
/// Only `a:off` and `a:ext` are replaced, so rotation and flip attributes on
/// the enclosing `a:xfrm` survive a drag or resize.
#[allow(clippy::too_many_arguments)]
fn set_box(source: &str, s: &XmlScan, shape: usize, x: i64, y: i64, cx: i64, cy: i64, addr: &str) -> Result<Splice> {
    if cx <= 0 || cy <= 0 {
        return Err(anyhow!("'{}' cannot be given a zero or negative size ({}x{} EMU).", addr, cx, cy));
    }

    let sp_pr = s
        .first_descendant(shape, "spPr")
        .or_else(|| s.first_descendant(shape, "grpSpPr"))
        .or_else(|| s.first_descendant(shape, "xfrm"))
        .ok_or_else(|| anyhow!("'{}' has no shape properties, so it cannot be moved.", addr))?;

    let xfrm = s.first_descendant(shape, "xfrm");
    let a = xfrm.map(|x| prefix_of(&s.elements[s.first_descendant(x, "off").unwrap_or(x)].qname, "a")).unwrap_or_else(|| "a".into());
    let replacement = format!("<{a}:off x=\"{}\" y=\"{}\"/><{a}:ext cx=\"{}\" cy=\"{}\"/>", x, y, cx, cy);

    match xfrm {
        Some(xfrm) => {
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
            Ok(Splice { span: s.elements[xfrm].inner, replacement: format!("{}{}", replacement, kept) })
        }
        None => {
            let at = s.elements[sp_pr].inner.start;
            Ok(Splice { span: Span { start: at, end: at }, replacement: format!("<a:xfrm>{}</a:xfrm>", replacement) })
        }
    }
}

// ---------------------------------------------------------------- slides

fn add_slide(pkg: &mut OoxmlPackage, after: usize, copy_of: Option<usize>) -> Result<()> {
    let parts = slide_parts(pkg)?;
    let source_part = parts.get(after).cloned().ok_or_else(|| anyhow!("Slide {} does not exist.", after + 1))?;
    let (width, height) = slide_size(pkg)?;

    let new_part = pkg.unused_part_name("ppt/slides", "slide", "xml");
    let new_rels = rels_part_for(&new_part);
    let source_rels = pkg.part_str(&rels_part_for(&source_part)).ok().map(|s| s.to_string());

    if copy_of.is_some() {
        let xml = pkg.part_str(&source_part)?.to_string();
        pkg.add_part(&new_part, xml.into_bytes())?;
        if let Some(rels) = source_rels {
            // Speaker notes belong to exactly one slide; a copy must not share them.
            let s = scan(&rels)?;
            let drops: Vec<Splice> = s
                .by_local("Relationship")
                .into_iter()
                .filter(|&r| s.elements[r].attr(&rels, "Type").map(|t| t.ends_with("/notesSlide")).unwrap_or(false))
                .map(|r| Splice { span: s.elements[r].outer, replacement: String::new() })
                .collect();
            pkg.add_part(&new_rels, apply_splices(&rels, drops)?.into_bytes())?;
        }
    } else {
        let layout = source_rels.as_deref().and_then(|rels| {
            let s = scan(rels).ok()?;
            s.by_local("Relationship").into_iter().find_map(|r| {
                let el = &s.elements[r];
                el.attr(rels, "Type")?.ends_with("/slideLayout").then(|| el.attr(rels, "Target")).flatten()
            })
        });
        let sx = |v: i64| v * width / 12_192_000;
        let sy = |v: i64| v * height / 6_858_000;
        let placeholder = |id: u32, name: &str, ph: &str, x: i64, y: i64, cx: i64, cy: i64| {
            format!(
                "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr>{ph}</p:nvPr></p:nvSpPr>\
<p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm></p:spPr>\
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang=\"en-US\" dirty=\"0\"/></a:p></p:txBody></p:sp>"
            )
        };
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<p:sld xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\"><p:cSld><p:spTree>\
<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{}{}\
</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>",
            placeholder(2, "Title 1", "<p:ph type=\"title\"/>", sx(838_200), sy(365_125), sx(10_515_600), sy(1_325_563)),
            placeholder(3, "Content Placeholder 2", "<p:ph idx=\"1\"/>", sx(838_200), sy(1_825_625), sx(10_515_600), sy(4_351_338)),
        );
        pkg.add_part(&new_part, xml.into_bytes())?;
        if let Some(target) = layout {
            pkg.add_relationship(&new_rels, "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout", &target)?;
        }
    }

    pkg.ensure_override_content_type(&new_part, CT_SLIDE)?;
    let rid = pkg.add_relationship(PRESENTATION_RELS_PART, REL_SLIDE, new_part.trim_start_matches("ppt/"))?;

    let pres = pkg.part_str(PRESENTATION_PART)?.to_string();
    let s = scan(&pres)?;
    let ids = s.by_local("sldId");
    let next_id = ids.iter().filter_map(|&i| s.elements[i].attr(&pres, "id")?.parse::<u64>().ok()).max().unwrap_or(255).max(255) + 1;
    let anchor = ids.get(after).copied().ok_or_else(|| anyhow!("Slide {} does not exist.", after + 1))?;
    let prefix = prefix_of(&s.elements[anchor].qname, "p");
    // The relationships prefix as this file writes it on `sldId`.
    let r = s.elements[anchor]
        .start_tag
        .of(&pres)
        .split_whitespace()
        .find_map(|attr| attr.split_once(":id=").map(|(p, _)| p.to_string()))
        .unwrap_or_else(|| "r".into());
    let entry = format!("<{prefix}:sldId id=\"{next_id}\" {r}:id=\"{rid}\"/>");
    let at = s.elements[anchor].outer.end;
    let patched = apply_splices(&pres, vec![Splice { span: Span { start: at, end: at }, replacement: entry }])?;
    pkg.replace_part(PRESENTATION_PART, patched.into_bytes())
}

fn delete_slide(pkg: &mut OoxmlPackage, slide: usize) -> Result<()> {
    let pres = pkg.part_str(PRESENTATION_PART)?.to_string();
    let s = scan(&pres)?;
    let ids = s.by_local("sldId");
    if ids.len() <= 1 {
        return Err(anyhow!("A presentation needs at least one slide."));
    }
    let target = ids.get(slide).copied().ok_or_else(|| anyhow!("Slide {} does not exist.", slide + 1))?;
    let rid = s.elements[target].attr(&pres, "r:id");
    let patched = apply_splices(&pres, vec![Splice { span: s.elements[target].outer, replacement: String::new() }])?;
    pkg.replace_part(PRESENTATION_PART, patched.into_bytes())?;

    // The relationship goes too; the slide part stays in the package as an
    // unreferenced part, which Office ignores and the version history keeps.
    if let Some(rid) = rid {
        let rels = pkg.part_str(PRESENTATION_RELS_PART)?.to_string();
        let rs = scan(&rels)?;
        if let Some(r) = rs.by_local("Relationship").into_iter().find(|&r| rs.elements[r].attr(&rels, "Id").as_deref() == Some(rid.as_str())) {
            let patched = apply_splices(&rels, vec![Splice { span: rs.elements[r].outer, replacement: String::new() }])?;
            pkg.replace_part(PRESENTATION_RELS_PART, patched.into_bytes())?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- addressing

fn parse_slide(addr: &str) -> Result<usize> {
    addr.split('/')
        .next()
        .and_then(|s| s.strip_prefix("slide[")?.strip_suffix(']')?.parse().ok())
        .ok_or_else(|| bad_addr(addr))
}

/// `slide[2]/sp[1]` or `slide[2]/sp[1]/p[0]`.
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
        Some(p) => Some(p.strip_prefix("p[").and_then(|s| s.strip_suffix(']')).and_then(|s| s.parse().ok()).ok_or_else(|| bad_addr(addr))?),
    };
    Ok((slide, shape, para))
}

fn bad_addr(addr: &str) -> anyhow::Error {
    anyhow!("'{}' is not a valid address; expected 'slide[2]/sp[1]' or 'slide[2]/sp[1]/p[0]'", addr)
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
        let saved = apply(&original, &[DraftPatch::SetShapeText { addr: "slide[0]/sp[0]".into(), text: "Transaction Overview".into() }]).unwrap();

        let before = OoxmlPackage::open(&original).unwrap();
        let after = OoxmlPackage::open(&saved).unwrap();
        for name in before.part_names() {
            if name == "ppt/slides/slide1.xml" {
                continue;
            }
            assert_eq!(before.part(name).unwrap(), after.part(name).unwrap(), "part '{}' must be untouched by a slide-1 text edit", name);
        }
        assert_eq!(slides(&saved)[0].shapes[0].paragraphs[0].text, "Transaction Overview");
        assert_eq!(slides(&saved)[1].shapes[0].paragraphs[0].text, "Risks");
    }

    #[test]
    fn moving_and_resizing_a_shape_writes_the_new_transform() {
        let saved = apply(
            &fixtures::pptx(),
            &[DraftPatch::SetShapeBox { addr: "slide[0]/sp[1]".into(), x: 100_000, y: 200_000, cx: 3_000_000, cy: 1_000_000 }],
        )
        .unwrap();
        let b = slides(&saved)[0].shapes[1].bbox.unwrap();
        assert_eq!((b.x, b.y, b.cx, b.cy), (100_000, 200_000, 3_000_000, 1_000_000));
        assert_eq!(slides(&saved)[0].shapes[0].bbox.unwrap().x, 838_200);
    }

    #[test]
    fn a_shape_cannot_be_collapsed_to_nothing() {
        let err = apply(&fixtures::pptx(), &[DraftPatch::SetShapeBox { addr: "slide[0]/sp[0]".into(), x: 0, y: 0, cx: 0, cy: 100 }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("zero or negative"), "{}", err);
    }

    #[test]
    fn deleting_a_shape_leaves_the_others_addressable() {
        let saved = apply(&fixtures::pptx(), &[DraftPatch::DeleteShape { addr: "slide[0]/sp[0]".into() }]).unwrap();
        let s = slides(&saved);
        assert_eq!(s[0].shapes.len(), 1);
        assert_eq!(s[0].shapes[0].addr, "slide[0]/sp[0]", "addresses reindex");
        assert_eq!(s[0].shapes[0].paragraphs[0].text, "Closing scheduled for Q1.");
    }

    #[test]
    fn text_with_markup_characters_survives_a_round_trip() {
        let tricky = "Risk & reward <material> \"terms\"";
        let saved = apply(&fixtures::pptx(), &[DraftPatch::SetShapeText { addr: "slide[1]/sp[0]".into(), text: tricky.into() }]).unwrap();
        assert_eq!(slides(&saved)[1].shapes[0].paragraphs[0].text, tricky);
    }

    #[test]
    fn paragraph_formatting_is_written_and_read_back() {
        let saved = apply(
            &fixtures::pptx(),
            &[DraftPatch::SetShapeTextFormat {
                addr: "slide[0]/sp[1]/p[0]".into(),
                format: TextFormat { bold: Some(true), size: Some(28.0), color: Some("C00000".into()), font: Some("Georgia".into()), ..Default::default() },
                align: Some("center".into()),
                list: Some("bullet".into()),
            }],
        )
        .unwrap();
        let p = &slides(&saved)[0].shapes[1].paragraphs[0];
        assert!(p.bold);
        assert_eq!(p.size, Some(28.0));
        assert_eq!(p.color.as_deref(), Some("C00000"));
        assert_eq!(p.font.as_deref(), Some("Georgia"));
        assert_eq!(p.align.as_deref(), Some("center"));
        assert_eq!(p.list.as_deref(), Some("bullet"));
        assert_eq!(p.text, "Closing scheduled for Q1.");
    }

    #[test]
    fn text_boxes_shapes_tables_and_pictures_can_be_added() {
        let png = BASE64.encode(fixtures::PNG_1X1);
        let saved = apply(
            &fixtures::pptx(),
            &[
                DraftPatch::InsertTextBox { addr: "slide[1]".into(), x: 1, y: 1, cx: 2_000_000, cy: 500_000, text: "Note".into() },
                DraftPatch::InsertShape { addr: "slide[1]".into(), geometry: "ellipse".into(), x: 1, y: 1, cx: 1_000_000, cy: 1_000_000, fill: Some("00B050".into()) },
                DraftPatch::InsertSlideTable { addr: "slide[1]".into(), rows: 2, cols: 2, x: 1, y: 1, cx: 4_000_000, cy: 1_000_000 },
                DraftPatch::InsertImage { addr: "slide[1]".into(), data_base64: png, file_name: "logo.png".into(), width_emu: 500_000, height_emu: 500_000, x: None, y: None },
            ],
        )
        .unwrap();
        let s = slides(&saved);
        let shapes = &s[1].shapes;
        assert_eq!(shapes.len(), 5);
        assert_eq!(shapes[1].paragraphs[0].text, "Note");
        assert_eq!(shapes[2].fill.as_deref(), Some("00B050"));
        let table = shapes[3].table.as_ref().expect("the table is modelled");
        assert_eq!((table.len(), table[0].len()), (2, 2));
        assert!(shapes[4].image.as_deref().unwrap_or("").starts_with("data:image/png"));

        // Typing into a table cell lands in that cell.
        let typed = apply(&saved, &[DraftPatch::SetShapeText { addr: "slide[1]/sp[3]/p[3]".into(), text: "Cell".into() }]).unwrap();
        let t = &slides(&typed)[1].shapes[3];
        let last_cell = t.table.as_ref().unwrap()[1][1][0];
        assert_eq!(t.paragraphs[last_cell].text, "Cell");
    }

    #[test]
    fn slides_can_be_added_duplicated_and_deleted() {
        let saved = apply(&fixtures::pptx(), &[DraftPatch::AddSlide { after: 0 }]).unwrap();
        let s = slides(&saved);
        assert_eq!(s.len(), 3);
        assert_eq!(s[1].shapes[0].placeholder.as_deref(), Some("title"));
        assert_eq!(s[2].shapes[0].paragraphs[0].text, "Risks");

        let dup = apply(&saved, &[DraftPatch::DuplicateSlide { slide: 2 }]).unwrap();
        let d = slides(&dup);
        assert_eq!(d.len(), 4);
        assert_eq!(d[3].shapes[0].paragraphs[0].text, "Risks");

        let del = apply(&dup, &[DraftPatch::DeleteSlide { slide: 1 }]).unwrap();
        assert_eq!(slides(&del).len(), 3);
    }

    #[test]
    fn an_out_of_range_slide_is_refused_with_advice() {
        let err = apply(&fixtures::pptx(), &[DraftPatch::SetShapeText { addr: "slide[9]/sp[0]".into(), text: "x".into() }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("has 2"), "{}", err);
        assert!(err.contains("Reopen the draft"), "{}", err);
    }

    #[test]
    fn a_malformed_address_names_the_expected_forms() {
        let err = apply(&fixtures::pptx(), &[DraftPatch::SetShapeText { addr: "slide2/shape1".into(), text: "x".into() }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("slide[2]/sp[1]"), "{}", err);
    }

    #[test]
    fn a_docx_patch_is_refused_before_anything_is_written() {
        let err = apply(&fixtures::pptx(), &[DraftPatch::DeleteParagraph { addr: "body/p[0]".into() }]).unwrap_err().to_string();
        assert!(err.contains("DeleteParagraph"), "{}", err);
        assert!(err.contains("PPTX"), "{}", err);
    }
}
