//! OOXML package I/O with a byte-preservation guarantee.
//!
//! # The one rule
//!
//! A part this module was not explicitly asked to change comes out of `save()`
//! **byte-identical** to how it went in. That is what makes "the document loses
//! nothing" a property of the code rather than a hope.
//!
//! Every other document editor regenerates the file from an editor model, which
//! silently drops whatever that model does not understand: theme fonts,
//! numbering definitions, headers and footers, footnotes, embedded charts,
//! tracked changes, content controls, custom XML, macros. Here the original ZIP
//! entries are held verbatim and only the named parts are replaced.
//!
//! # Why the raw bytes are kept in memory
//!
//! `zip::ZipArchive` cannot be read while a `ZipWriter` borrows the same
//! reader, and `raw_copy_file` needs a live handle to the source entry. Holding
//! the decompressed bytes for every part sidesteps both problems and keeps
//! `save()` infallible with respect to the source. OOXML parts are text-ish and
//! small relative to the media they reference; the guardrail in
//! `document_workspace` bounds total package size before we ever get here.
//!
//! # Compression
//!
//! Entries are rewritten with Deflate, matching what Word/Excel/PowerPoint
//! themselves emit. The *compressed* bytes of an untouched part are therefore
//! not guaranteed to be identical to the original — but the **decompressed**
//! bytes are, which is the property that matters: Office reads content, not
//! compressor output. `assert_roundtrip_identical` in the tests checks exactly
//! that, part by part.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};

use anyhow::{anyhow, Context, Result};
use tracing::{debug, warn};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// An opened OOXML package: every part, in its original order.
///
/// Order is preserved deliberately. `[Content_Types].xml` must be the first
/// entry for some consumers, and keeping the original sequence avoids any
/// chance of a reader that depends on it being surprised.
pub struct OoxmlPackage {
    /// Part name (ZIP entry path) → decompressed bytes.
    parts: HashMap<String, Vec<u8>>,
    /// Original entry order, so `save()` writes them back in sequence.
    order: Vec<String>,
}

impl OoxmlPackage {
    /// Open a package from raw bytes.
    ///
    /// Fails loudly on anything that is not a readable ZIP — which includes
    /// password-protected Office files, whose parts are encrypted. Opening
    /// those as an empty document would be the silent-corruption failure this
    /// whole module exists to prevent.
    pub fn open(bytes: &[u8]) -> Result<Self> {
        let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|e| {
            anyhow!(
                "This file is not a readable Office package: {}. If it is password-protected, \
                 remove the protection and try again.",
                e
            )
        })?;

        let mut parts = HashMap::with_capacity(archive.len());
        let mut order = Vec::with_capacity(archive.len());

        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .with_context(|| format!("could not read package entry {}", i))?;

            // Directory entries carry no content and are re-created implicitly
            // by the paths of the files inside them.
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut buf)
                .with_context(|| format!("could not decompress part '{}'", name))?;

            order.push(name.clone());
            parts.insert(name, buf);
        }

        if parts.is_empty() {
            return Err(anyhow!("This Office package contains no parts."));
        }
        debug!("Opened OOXML package with {} parts", parts.len());
        Ok(Self { parts, order })
    }

    /// Read a part's bytes, if present.
    pub fn part(&self, name: &str) -> Option<&[u8]> {
        self.parts.get(name).map(|v| v.as_slice())
    }

    /// Read a part as UTF-8 XML text.
    pub fn part_str(&self, name: &str) -> Result<&str> {
        let bytes = self
            .part(name)
            .ok_or_else(|| anyhow!("This document is missing its '{}' part.", name))?;
        std::str::from_utf8(bytes)
            .with_context(|| format!("part '{}' is not valid UTF-8", name))
    }

    /// Replace a part's bytes. The part must already exist — adding new parts
    /// requires content-type registration and is handled by the callers that
    /// genuinely need it, not by a general-purpose setter.
    pub fn replace_part(&mut self, name: &str, bytes: Vec<u8>) -> Result<()> {
        if !self.parts.contains_key(name) {
            return Err(anyhow!(
                "Refusing to replace '{}': no such part in this package.",
                name
            ));
        }
        self.parts.insert(name.to_string(), bytes);
        Ok(())
    }

    /// Every part name, in original package order.
    pub fn part_names(&self) -> &[String] {
        &self.order
    }

    /// Part names under a directory prefix, in package order.
    pub fn parts_with_prefix(&self, prefix: &str) -> Vec<&str> {
        self.order
            .iter()
            .filter(|n| n.starts_with(prefix))
            .map(|n| n.as_str())
            .collect()
    }

    /// Repack the package. Parts not replaced are written back exactly as they
    /// were read.
    pub fn save(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut out));
            let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

            for name in &self.order {
                let Some(bytes) = self.parts.get(name) else {
                    // Unreachable while `order` and `parts` are maintained
                    // together, but a missing part must never silently vanish
                    // from the output.
                    warn!("Part '{}' vanished between open and save; skipping", name);
                    continue;
                };
                writer
                    .start_file(name.as_str(), options)
                    .with_context(|| format!("could not start part '{}'", name))?;
                writer
                    .write_all(bytes)
                    .with_context(|| format!("could not write part '{}'", name))?;
            }
            writer.finish().context("could not finalise the package")?;
        }
        Ok(out)
    }
}

/// Resolve a relationship `Target` against the directory of the part that
/// declared it.
///
/// Office writes these three ways and all of them occur in real files:
/// `worksheets/sheet1.xml` (relative), `/xl/worksheets/sheet1.xml` (absolute
/// from the package root), and `../slideLayouts/l1.xml` (parent-relative, which
/// every slide's rels part uses). Treating a target as a plain suffix works
/// until the first PowerPoint file arrives.
pub fn resolve_target(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut segments: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Read a `.rels` part into `Id` → `Target`.
pub fn read_relationships(source: &str) -> anyhow::Result<HashMap<String, String>> {
    let s = super::spans::scan(source)?;
    let mut map = HashMap::new();
    for &r in s.by_local("Relationship").iter() {
        let el = &s.elements[r];
        if let (Some(id), Some(target)) = (el.attr(source, "Id"), el.attr(source, "Target")) {
            map.insert(id, target);
        }
    }
    Ok(map)
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Programmatically generated OOXML fixtures.
    //!
    //! Generated rather than checked in as real files, for the same reason the
    //! document-memory tests use synthetic documents: a real user document must
    //! never end up in the shipped test suite. These carry the awkward parts
    //! that a regenerating editor would destroy — theme, numbering, headers,
    //! custom XML, a binary media part — so the invariant test has something
    //! meaningful to protect.

    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn pack(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut w = ZipWriter::new(Cursor::new(&mut out));
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            for (name, bytes) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(bytes).unwrap();
            }
            w.finish().unwrap();
        }
        out
    }

    /// A .docx with body text, styles, numbering, a header, theme, custom XML
    /// and a binary media part.
    pub fn docx() -> Vec<u8> {
        pack(&[
            ("[Content_Types].xml", CONTENT_TYPES_DOCX.as_bytes()),
            ("_rels/.rels", RELS_ROOT.as_bytes()),
            ("word/document.xml", DOCX_DOCUMENT.as_bytes()),
            ("word/_rels/document.xml.rels", DOCX_DOC_RELS.as_bytes()),
            ("word/styles.xml", DOCX_STYLES.as_bytes()),
            ("word/numbering.xml", DOCX_NUMBERING.as_bytes()),
            ("word/header1.xml", DOCX_HEADER.as_bytes()),
            ("word/theme/theme1.xml", THEME.as_bytes()),
            ("customXml/item1.xml", CUSTOM_XML.as_bytes()),
            ("word/media/image1.png", PNG_1X1),
        ])
    }

    /// An .xlsx with two sheets, shared strings, styles and a formula.
    pub fn xlsx() -> Vec<u8> {
        pack(&[
            ("[Content_Types].xml", CONTENT_TYPES_XLSX.as_bytes()),
            ("_rels/.rels", RELS_ROOT_XLSX.as_bytes()),
            ("xl/workbook.xml", XLSX_WORKBOOK.as_bytes()),
            ("xl/_rels/workbook.xml.rels", XLSX_WB_RELS.as_bytes()),
            ("xl/worksheets/sheet1.xml", XLSX_SHEET1.as_bytes()),
            ("xl/worksheets/sheet2.xml", XLSX_SHEET2.as_bytes()),
            ("xl/sharedStrings.xml", XLSX_SHARED.as_bytes()),
            ("xl/styles.xml", XLSX_STYLES.as_bytes()),
            ("xl/theme/theme1.xml", THEME.as_bytes()),
        ])
    }

    /// A .pptx with two slides, a layout, master and theme.
    pub fn pptx() -> Vec<u8> {
        pack(&[
            ("[Content_Types].xml", CONTENT_TYPES_PPTX.as_bytes()),
            ("_rels/.rels", RELS_ROOT_PPTX.as_bytes()),
            ("ppt/presentation.xml", PPTX_PRESENTATION.as_bytes()),
            ("ppt/_rels/presentation.xml.rels", PPTX_PRES_RELS.as_bytes()),
            ("ppt/slides/slide1.xml", PPTX_SLIDE1.as_bytes()),
            ("ppt/slides/slide2.xml", PPTX_SLIDE2.as_bytes()),
            ("ppt/slides/_rels/slide1.xml.rels", PPTX_SLIDE_RELS.as_bytes()),
            ("ppt/slides/_rels/slide2.xml.rels", PPTX_SLIDE_RELS.as_bytes()),
            ("ppt/slideLayouts/slideLayout1.xml", PPTX_LAYOUT.as_bytes()),
            ("ppt/slideMasters/slideMaster1.xml", PPTX_MASTER.as_bytes()),
            ("ppt/theme/theme1.xml", THEME.as_bytes()),
            ("ppt/media/image1.png", PNG_1X1),
        ])
    }

    /// Smallest valid PNG — a real binary part, so the invariant test proves
    /// non-XML content survives too.
    pub const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    pub const DOCX_DOCUMENT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Master Services Agreement</w:t></w:r></w:p><w:p><w:r><w:t xml:space="preserve">This Agreement is entered into as of </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>1 January 2026</w:t></w:r><w:r><w:t xml:space="preserve"> between the parties.</w:t></w:r></w:p><w:p><w:r><w:t>Section 2. Confidentiality obligations survive termination.</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Term</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Twelve months</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:sectPr><w:headerReference w:type="default" r:id="rId4" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"/></w:sectPr></w:body></w:document>"#;

    pub const DOCX_STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:rPr><w:b/><w:sz w:val="32"/><w:color w:val="1F3864"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Normal"><w:name w:val="Normal"/><w:rPr><w:sz w:val="22"/></w:rPr></w:style></w:styles>"#;

    pub const DOCX_NUMBERING: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;

    pub const DOCX_HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:t>CONFIDENTIAL</w:t></w:r></w:p></w:hdr>"#;

    pub const DOCX_DOC_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/></Relationships>"#;

    pub const CONTENT_TYPES_DOCX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="png" ContentType="image/png"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

    pub const RELS_ROOT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

    pub const CUSTOM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<matter xmlns="urn:firm:matter"><id>2026-0042</id><client>Acme Corp</client></matter>"#;

    pub const THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office"><a:themeElements><a:clrScheme name="Office"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:accent1><a:srgbClr val="4472C4"/></a:accent1></a:clrScheme></a:themeElements></a:theme>"#;

    pub const XLSX_WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Fees" sheetId="1" r:id="rId1"/><sheet name="Summary" sheetId="2" r:id="rId2"/></sheets></workbook>"#;

    pub const XLSX_WB_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;

    pub const XLSX_SHEET1: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>1200</v></c></row><row r="3"><c r="A3" t="s"><v>3</v></c><c r="B3"><v>850</v></c></row><row r="4"><c r="A4" t="s"><v>4</v></c><c r="B4"><f>SUM(B2:B3)</f><v>2050</v></c></row></sheetData></worksheet>"#;

    pub const XLSX_SHEET2: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>5</v></c></row></sheetData></worksheet>"#;

    pub const XLSX_SHARED: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="6" uniqueCount="6"><si><t>Matter</t></si><si><t>Amount</t></si><si><t>Drafting</t></si><si><t>Review</t></si><si><t>Total</t></si><si><t>Notes</t></si></sst>"#;

    // r##...##: the number format `="#,##0.00"` contains the `"#` sequence,
    // which would terminate an ordinary r#"..."# literal.
    pub const XLSX_STYLES: &str = r##"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="1"><numFmt numFmtId="164" formatCode="#,##0.00"/></numFmts><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><cellXfs count="2"><xf numFmtId="0" fontId="0"/><xf numFmtId="164" fontId="0" applyNumberFormat="1"/></cellXfs></styleSheet>"##;

    pub const CONTENT_TYPES_XLSX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#;

    pub const RELS_ROOT_XLSX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

    pub const PPTX_PRESENTATION: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId2"/><p:sldId id="257" r:id="rId3"/></p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/></p:presentation>"#;

    pub const PPTX_PRES_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/></Relationships>"#;

    pub const PPTX_SLIDE1: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="838200" y="365125"/><a:ext cx="10515600" cy="1325563"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:p><a:r><a:t>Deal Overview</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Content 2"/></p:nvSpPr><p:spPr><a:xfrm><a:off x="838200" y="1825625"/><a:ext cx="10515600" cy="4351338"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:p><a:r><a:t>Closing scheduled for Q1.</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;

    pub const PPTX_SLIDE2: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/></p:nvSpPr><p:spPr><a:xfrm><a:off x="838200" y="365125"/><a:ext cx="10515600" cy="1325563"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:p><a:r><a:t>Risks</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;

    pub const PPTX_SLIDE_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#;

    pub const PPTX_LAYOUT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="title"><p:cSld name="Title Slide"/></p:sldLayout>"#;

    pub const PPTX_MASTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld/></p:sldMaster>"#;

    pub const CONTENT_TYPES_PPTX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="png" ContentType="image/png"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#;

    pub const RELS_ROOT_PPTX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert that every part of `saved` matches `original` except those named
    /// in `expected_changes`. This is THE invariant of this module.
    pub(crate) fn assert_only_these_parts_changed(
        original: &[u8],
        saved: &[u8],
        expected_changes: &[&str],
    ) {
        let before = OoxmlPackage::open(original).unwrap();
        let after = OoxmlPackage::open(saved).unwrap();

        assert_eq!(
            before.part_names(),
            after.part_names(),
            "the set/order of parts must not change"
        );

        for name in before.part_names() {
            let b = before.part(name).unwrap();
            let a = after.part(name).unwrap();
            if expected_changes.contains(&name.as_str()) {
                assert_ne!(b, a, "part '{}' was expected to change but did not", name);
            } else {
                assert_eq!(
                    b, a,
                    "part '{}' MUST be byte-identical after a save that did not target it",
                    name
                );
            }
        }
    }

    #[test]
    fn docx_survives_open_and_save_untouched() {
        let original = fixtures::docx();
        let pkg = OoxmlPackage::open(&original).unwrap();
        let saved = pkg.save().unwrap();
        assert_only_these_parts_changed(&original, &saved, &[]);
    }

    #[test]
    fn xlsx_survives_open_and_save_untouched() {
        let original = fixtures::xlsx();
        let pkg = OoxmlPackage::open(&original).unwrap();
        let saved = pkg.save().unwrap();
        assert_only_these_parts_changed(&original, &saved, &[]);
    }

    #[test]
    fn pptx_survives_open_and_save_untouched() {
        let original = fixtures::pptx();
        let pkg = OoxmlPackage::open(&original).unwrap();
        let saved = pkg.save().unwrap();
        assert_only_these_parts_changed(&original, &saved, &[]);
    }

    /// The surgical guarantee: replacing ONE part leaves every other part —
    /// including styles, theme, numbering, headers, custom XML and the binary
    /// image — byte-identical. This is precisely what a regenerating editor
    /// cannot promise.
    #[test]
    fn replacing_one_part_leaves_every_other_part_untouched() {
        let original = fixtures::docx();
        let mut pkg = OoxmlPackage::open(&original).unwrap();
        let edited = fixtures::DOCX_DOCUMENT.replace("Master Services Agreement", "Amended Agreement");
        pkg.replace_part("word/document.xml", edited.into_bytes()).unwrap();
        let saved = pkg.save().unwrap();

        assert_only_these_parts_changed(&original, &saved, &["word/document.xml"]);

        // And the binary media part specifically, since that is what a
        // regenerating exporter drops first.
        let after = OoxmlPackage::open(&saved).unwrap();
        assert_eq!(after.part("word/media/image1.png").unwrap(), fixtures::PNG_1X1);
        assert!(after.part_str("customXml/item1.xml").unwrap().contains("2026-0042"));
    }

    #[test]
    fn a_non_zip_file_fails_loudly_and_mentions_password_protection() {
        let msg = match OoxmlPackage::open(b"this is not a zip") {
            Ok(_) => panic!("garbage bytes must not open as a package"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("not a readable Office package"), "{}", msg);
        assert!(msg.contains("password-protected"), "{}", msg);
    }

    #[test]
    fn replacing_a_part_that_does_not_exist_is_refused() {
        let mut pkg = OoxmlPackage::open(&fixtures::docx()).unwrap();
        let err = pkg.replace_part("word/nonexistent.xml", vec![]).unwrap_err();
        assert!(err.to_string().contains("no such part"), "{}", err);
    }

    #[test]
    fn parts_can_be_listed_by_prefix_in_package_order() {
        let pkg = OoxmlPackage::open(&fixtures::pptx()).unwrap();
        let slides = pkg.parts_with_prefix("ppt/slides/slide");
        assert_eq!(slides, vec!["ppt/slides/slide1.xml", "ppt/slides/slide2.xml"]);
    }
}
