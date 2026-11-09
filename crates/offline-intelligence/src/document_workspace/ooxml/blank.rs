//! Minimal, genuinely valid empty OOXML packages.
//!
//! # Why these exist
//!
//! `POST /drafts/blank` used to refuse every format but plain text, because a
//! blank .docx is not an empty file — it is a complete OOXML package with
//! content types, relationships and (for PowerPoint) a master, a layout and a
//! theme. That refusal was correct while nothing here produced one.
//!
//! # Why they are built rather than shipped as binary
//!
//! A checked-in .docx is an opaque blob in the repository that nobody can
//! review, diff, or reason about, and the parts below are small enough to
//! read. Building them also means the ZIP is produced by the same writer the
//! rest of the workspace uses, so a change to compression or ordering cannot
//! make shipped templates and round-tripped documents disagree.
//!
//! # The standard these target
//!
//! ECMA-376. Each package carries the minimum the consuming application
//! genuinely requires, not the much larger set Word and PowerPoint themselves
//! write:
//!
//! - **docx** — content types, the root relationship, and one `document.xml`
//!   holding an empty paragraph. Styles, numbering and a theme are all
//!   optional and are omitted; Word adds its own on first save.
//! - **xlsx** — the same, plus a `styles.xml`. Excel tolerates its absence but
//!   warns on some builds, and it is twelve lines.
//! - **pptx** — the demanding one. A presentation is invalid without a slide
//!   master, the master is invalid without a layout AND a theme, and the
//!   colour map must be present or PowerPoint offers to repair the file.
//!
//! The empty paragraph in the Word document and the empty `sheetData` in the
//! worksheet are deliberate: our own view-model builders address nodes, and a
//! document with no addressable node at all gives the editor nothing to put a
//! caret in.

use std::io::{Cursor, Write};

use anyhow::Result;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Build a blank package for `format`, or `None` if there is no template.
///
/// Returning an Option rather than erroring keeps the decision about what to
/// tell the user in the API layer, which is where the other format refusals
/// are worded.
pub fn for_format(format: &str) -> Option<Result<Vec<u8>>> {
    match format {
        "docx" => Some(docx()),
        "xlsx" => Some(xlsx()),
        "pptx" => Some(pptx()),
        _ => None,
    }
}

fn pack(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut w = ZipWriter::new(Cursor::new(&mut out));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in entries {
            w.start_file(*name, opts)?;
            w.write_all(bytes)?;
        }
        w.finish()?;
    }
    Ok(out)
}

// ------------------------------------------------------------------- docx

/// A new Word document carries the built-in styles (Normal, Title, Headings,
/// Quote), so the style picker works from the first keystroke and a heading
/// made here is a heading when the file is opened in Word.
pub fn docx() -> Result<Vec<u8>> {
    let styles = super::docx::default_styles_part();
    pack(&[
        ("[Content_Types].xml", DOCX_CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", ROOT_RELS_DOCX.as_bytes()),
        ("word/document.xml", DOCX_DOCUMENT.as_bytes()),
        ("word/_rels/document.xml.rels", DOCX_DOCUMENT_RELS.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
    ])
}

const DOCX_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"#;

const DOCX_DOCUMENT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

const ROOT_RELS_DOCX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

/// One empty paragraph, then the section properties Word expects at the end of
/// a body. A4-ish Letter page size with one-inch margins, in twentieths of a
/// point (12240 x 15840 = 8.5in x 11in at 1440 twips per inch).
const DOCX_DOCUMENT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:r><w:t xml:space="preserve"></w:t></w:r></w:p>
<w:sectPr>
<w:pgSz w:w="12240" w:h="15840"/>
<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>
</w:sectPr>
</w:body>
</w:document>"#;

// ------------------------------------------------------------------- xlsx

pub fn xlsx() -> Result<Vec<u8>> {
    pack(&[
        ("[Content_Types].xml", XLSX_CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", ROOT_RELS_XLSX.as_bytes()),
        ("xl/workbook.xml", XLSX_WORKBOOK.as_bytes()),
        ("xl/_rels/workbook.xml.rels", XLSX_WORKBOOK_RELS.as_bytes()),
        ("xl/worksheets/sheet1.xml", XLSX_SHEET.as_bytes()),
        ("xl/styles.xml", XLSX_STYLES.as_bytes()),
    ])
}

const XLSX_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"#;

const ROOT_RELS_XLSX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

const XLSX_WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;

const XLSX_WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

/// An empty grid. `sheetData` must be present even with no rows in it.
const XLSX_SHEET: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData/>
</worksheet>"#;

/// The smallest stylesheet Excel accepts: one font, one fill, one border, and
/// the single cell format that every cell implicitly points at.
const XLSX_STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
<fills count="1"><fill><patternFill patternType="none"/></fill></fills>
<borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>
<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
<cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs>
</styleSheet>"#;

// ------------------------------------------------------------------- pptx

pub fn pptx() -> Result<Vec<u8>> {
    pack(&[
        ("[Content_Types].xml", PPTX_CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", ROOT_RELS_PPTX.as_bytes()),
        ("ppt/presentation.xml", PPTX_PRESENTATION.as_bytes()),
        ("ppt/_rels/presentation.xml.rels", PPTX_PRES_RELS.as_bytes()),
        ("ppt/slides/slide1.xml", PPTX_SLIDE.as_bytes()),
        ("ppt/slides/_rels/slide1.xml.rels", PPTX_SLIDE_RELS.as_bytes()),
        ("ppt/slideLayouts/slideLayout1.xml", PPTX_LAYOUT.as_bytes()),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", PPTX_LAYOUT_RELS.as_bytes()),
        ("ppt/slideMasters/slideMaster1.xml", PPTX_MASTER.as_bytes()),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", PPTX_MASTER_RELS.as_bytes()),
        ("ppt/theme/theme1.xml", PPTX_THEME.as_bytes()),
    ])
}

const PPTX_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
</Types>"#;

const ROOT_RELS_PPTX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#;

/// 16:9 at 914400 EMU per inch: 13.333in x 7.5in.
const PPTX_PRESENTATION: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
<p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst>
<p:sldSz cx="12192000" cy="6858000"/>
<p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#;

const PPTX_PRES_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/>
</Relationships>"#;

/// One empty title placeholder, so the slide has something addressable to type
/// into rather than being a blank canvas with no shapes at all.
const PPTX_SLIDE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
<p:sp>
<p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
<p:spPr><a:xfrm><a:off x="1524000" y="2130425"/><a:ext cx="9144000" cy="1470025"/></a:xfrm></p:spPr>
<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US" dirty="0"/><a:t></a:t></a:r></a:p></p:txBody>
</p:sp>
</p:spTree></p:cSld>
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:sld>"#;

const PPTX_SLIDE_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#;

const PPTX_LAYOUT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="title" preserve="1">
<p:cSld name="Title Slide"><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
</p:spTree></p:cSld>
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:sldLayout>"#;

const PPTX_LAYOUT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#;

/// `p:clrMap` is not optional — PowerPoint offers to repair a package whose
/// master omits it, because every theme colour reference on every slide
/// resolves through this mapping.
const PPTX_MASTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
</p:spTree></p:cSld>
<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>"#;

const PPTX_MASTER_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>"#;

/// A complete DrawingML theme. Every one of clrScheme, fontScheme and
/// fmtScheme is required, and fmtScheme must carry exactly three fill styles,
/// three line styles, three effect styles and two background fill styles —
/// PowerPoint indexes into these lists positionally.
const PPTX_THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office">
<a:themeElements>
<a:clrScheme name="Office">
<a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
<a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
<a:dk2><a:srgbClr val="44546A"/></a:dk2>
<a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
<a:accent1><a:srgbClr val="4472C4"/></a:accent1>
<a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
<a:accent3><a:srgbClr val="A5A5A5"/></a:accent3>
<a:accent4><a:srgbClr val="FFC000"/></a:accent4>
<a:accent5><a:srgbClr val="5B9BD5"/></a:accent5>
<a:accent6><a:srgbClr val="70AD47"/></a:accent6>
<a:hlink><a:srgbClr val="0563C1"/></a:hlink>
<a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
</a:clrScheme>
<a:fontScheme name="Office">
<a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont>
<a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont>
</a:fontScheme>
<a:fmtScheme name="Office">
<a:fillStyleLst>
<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
</a:fillStyleLst>
<a:lnStyleLst>
<a:ln w="6350" cap="flat" cmpd="sng" algn="ctr"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/></a:ln>
<a:ln w="12700" cap="flat" cmpd="sng" algn="ctr"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/></a:ln>
<a:ln w="19050" cap="flat" cmpd="sng" algn="ctr"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/></a:ln>
</a:lnStyleLst>
<a:effectStyleLst>
<a:effectStyle><a:effectLst/></a:effectStyle>
<a:effectStyle><a:effectLst/></a:effectStyle>
<a:effectStyle><a:effectLst/></a:effectStyle>
</a:effectStyleLst>
<a:bgFillStyleLst>
<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
</a:bgFillStyleLst>
</a:fmtScheme>
</a:themeElements>
</a:theme>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_workspace::ooxml::{docx as docx_reader, package::OoxmlPackage, pptx as pptx_reader, xlsx as xlsx_reader};

    /// The templates have to satisfy OUR readers, because the editor opens a
    /// blank document the instant it is created. A template that only a word
    /// processor could read would be created and then fail to open.
    #[test]
    fn every_blank_template_opens_in_its_own_editor() {
        let d = docx().unwrap();
        let model = docx_reader::build(&d, 1).unwrap();
        assert!(
            matches!(&model, crate::document_workspace::view_model::DraftViewModel::Docx { blocks, .. } if !blocks.is_empty()),
            "a blank docx must still expose one addressable paragraph to type into",
        );

        let x = xlsx().unwrap();
        xlsx_reader::build(&x, 1).unwrap();

        let p = pptx().unwrap();
        pptx_reader::build(&p, 1).unwrap();
    }

    /// Guards the part lists above. A missing relationship or content-type
    /// override is the difference between a file that opens and one that
    /// prompts to repair, and it is invisible until a real Office app sees it.
    #[test]
    fn every_blank_template_is_a_well_formed_package() {
        for (label, bytes) in [
            ("docx", docx().unwrap()),
            ("xlsx", xlsx().unwrap()),
            ("pptx", pptx().unwrap()),
        ] {
            let pkg = OoxmlPackage::open(&bytes)
                .unwrap_or_else(|e| panic!("{label} is not a readable package: {e}"));
            let names = pkg.part_names();
            assert!(
                names.iter().any(|n| n == "[Content_Types].xml"),
                "{label} has no content types",
            );
            assert!(names.iter().any(|n| n == "_rels/.rels"), "{label} has no root rels");
        }
    }

    /// PowerPoint repairs a package whose master lacks a colour map or a theme,
    /// and both are easy to drop when trimming a template down.
    #[test]
    fn the_pptx_master_keeps_its_colour_map_and_theme() {
        let bytes = pptx().unwrap();
        let pkg = OoxmlPackage::open(&bytes).unwrap();

        let master = pkg.part_str("ppt/slideMasters/slideMaster1.xml").unwrap();
        assert!(master.contains("<p:clrMap "), "the master must map theme colours");

        let rels = pkg.part_str("ppt/slideMasters/_rels/slideMaster1.xml.rels").unwrap();
        assert!(rels.contains("theme/theme1.xml"), "the master must reference a theme");

        assert!(pkg.part("ppt/theme/theme1.xml").is_some(), "and the theme must be present");
    }

    #[test]
    fn there_is_no_template_for_a_format_we_cannot_synthesise() {
        assert!(for_format("txt").is_none());
        assert!(for_format("pdf").is_none());
        assert!(for_format("docx").is_some());
    }
}
