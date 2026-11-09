//! The contract between the Rust patcher and the React editors.
//!
//! # The address is the contract
//!
//! Every editable thing carries an `addr` — `body/p[12]/r[0]`, `sheet[0]/B7`,
//! `slide[2]/sp[1]`. The browser never sends a document; it sends "replace the
//! text of THIS node with THIS string". Rust resolves the address to a byte
//! span in the original part and splices it.
//!
//! Two consequences fall straight out of that, and they are the whole reason
//! the design is shaped this way:
//!
//! 1. A construct the editor cannot render is never *addressed*, so it can
//!    never be damaged. Unrendered is not the same as lost.
//! 2. Two edits in different parts of a document cannot clobber each other,
//!    because each names exactly one node.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use super::ooxml::{docx, pptx, xlsx};
use super::{pdf_workspace, text_workspace};

/// What the editor renders. Tagged by format so the React side can switch on
/// one field rather than guessing from which keys are present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "lowercase")]
pub enum DraftViewModel {
    Docx {
        version: i64,
        blocks: Vec<docx::Block>,
        styles: Vec<docx::StyleInfo>,
        /// Document-wide default font and size, so unformatted text renders in
        /// the face Word would use rather than the browser's.
        defaults: docx::DocDefaults,
    },
    Xlsx {
        version: i64,
        sheets: Vec<xlsx::Sheet>,
    },
    Pptx {
        version: i64,
        slides: Vec<pptx::Slide>,
    },
    /// PDF pages are rendered as images by pdfium and fetched separately from
    /// `/drafts/:id/page/:n`; the model carries only geometry and annotations.
    Pdf {
        version: i64,
        pages: Vec<PdfPage>,
        /// The document outline, flattened depth-first.
        #[serde(default)]
        bookmarks: Vec<PdfBookmark>,
    },
    Txt {
        version: i64,
        content: String,
        /// Reported so the UI can say "Windows-1252, CRLF" rather than
        /// pretending every text file is UTF-8 with Unix endings.
        encoding: String,
        line_ending: String,
        bom: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfPage {
    pub index: usize,
    pub width: f32,
    pub height: f32,
    pub annotations: Vec<PdfAnnotation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfAnnotation {
    /// Index of the annotation on its page — pdfium's own addressing.
    pub index: usize,
    pub kind: String,
    pub rect: [f32; 4],
    pub contents: Option<String>,
    /// RGB, when the annotation carries a readable colour.
    #[serde(default)]
    pub color: Option<[u8; 3]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfBookmark {
    pub title: String,
    /// Zero-based page the bookmark points at, when it points at one.
    pub page: Option<usize>,
    /// Nesting depth, 0 for top-level entries.
    pub level: usize,
}

/// Character formatting shared by Word runs, slide text and sheet cells.
///
/// Every field is optional and means "leave as is" when absent. For the string
/// fields an empty string means "clear back to inherited".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TextFormat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strike: Option<bool>,
    /// Font family name, e.g. `Calibri`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    /// Size in points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    /// Text colour as `RRGGBB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Word: a highlight name (`yellow`, `green`…). Slides/cells: `RRGGBB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<String>,
    /// `superscript`, `subscript` or `baseline`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vert_align: Option<String>,
}

/// One edit, addressed to one node.
///
/// Deliberately a closed set. A patch the backend does not understand is
/// rejected by name rather than ignored: silently dropping an edit the user
/// made and watched appear on screen is the worst failure this system could
/// have.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum DraftPatch {
    // ---- DOCX ----
    SetRunText {
        addr: String,
        text: String,
    },
    /// Format a whole run.
    SetRunFormat {
        addr: String,
        #[serde(flatten)]
        format: TextFormat,
    },
    /// Format the characters `[start, end)` of a paragraph, splitting runs at
    /// the boundaries. Offsets are UTF-16 code units into the concatenated run
    /// text, which is exactly what a browser selection measures.
    FormatRange {
        addr: String,
        start: usize,
        end: usize,
        #[serde(flatten)]
        format: TextFormat,
    },
    SetParagraphStyle {
        addr: String,
        /// `None` clears the style back to the document default.
        #[serde(default)]
        style: Option<String>,
    },
    /// Alignment, lists, indentation and line spacing of one paragraph.
    SetParagraphFormat {
        addr: String,
        /// `left`, `center`, `right` or `justify`.
        #[serde(default)]
        align: Option<String>,
        /// `bullet`, `number` or `none`.
        #[serde(default)]
        list: Option<String>,
        /// Indent change in levels: +1 indents, -1 outdents.
        #[serde(default)]
        indent: Option<i32>,
        /// Line spacing multiple, e.g. 1.0, 1.15, 1.5, 2.0.
        #[serde(default)]
        line_spacing: Option<f32>,
    },
    /// Give a paragraph that has no runs its first one.
    SetParagraphText {
        addr: String,
        text: String,
    },
    InsertParagraphAfter {
        addr: String,
        text: String,
    },
    /// Enter: split one paragraph in two at a UTF-16 offset.
    SplitParagraph {
        addr: String,
        offset: usize,
    },
    /// Backspace at the start of a paragraph: join it onto the previous one.
    MergeParagraphWithPrevious {
        addr: String,
    },
    DeleteParagraph {
        addr: String,
    },
    DeleteRun {
        addr: String,
    },
    InsertTable {
        addr: String,
        rows: usize,
        cols: usize,
    },
    /// `addr` is any paragraph inside the row.
    InsertTableRow {
        addr: String,
        #[serde(default)]
        below: bool,
    },
    DeleteTableRow {
        addr: String,
    },
    InsertTableColumn {
        addr: String,
        #[serde(default)]
        right: bool,
    },
    DeleteTableColumn {
        addr: String,
    },
    DeleteTable {
        addr: String,
    },
    /// Word: an inline picture in a new paragraph after `addr`.
    /// Slides: a picture on the slide `addr`, at `x`/`y`.
    InsertImage {
        addr: String,
        data_base64: String,
        file_name: String,
        width_emu: i64,
        height_emu: i64,
        #[serde(default)]
        x: Option<i64>,
        #[serde(default)]
        y: Option<i64>,
    },
    /// Resize an inline picture; `addr` is its run.
    SetImageSize {
        addr: String,
        width_emu: i64,
        height_emu: i64,
    },
    /// A display equation after `addr`, written in the linear form
    /// `x^{2} + \frac{a}{b}`; see `ooxml::math`.
    InsertEquation {
        addr: String,
        text: String,
    },
    /// Replace the equation held by the paragraph `addr`.
    SetEquation {
        addr: String,
        text: String,
    },
    InsertPageBreak {
        addr: String,
    },

    // ---- XLSX ----
    SetCellValue {
        addr: String,
        /// The literal the user typed. A leading `=` makes it a formula.
        value: String,
    },
    ClearCell {
        addr: String,
    },
    /// Font, fill, alignment and number format of one cell.
    SetCellFormat {
        addr: String,
        #[serde(flatten)]
        format: TextFormat,
        /// Background fill `RRGGBB`; empty clears.
        #[serde(default)]
        fill: Option<String>,
        /// `left`, `center`, `right` or `general`.
        #[serde(default)]
        align: Option<String>,
        #[serde(default)]
        wrap: Option<bool>,
        /// An Excel number format code, e.g. `#,##0.00`; `General` clears.
        #[serde(default)]
        num_fmt: Option<String>,
    },

    // ---- PPTX ----
    SetShapeText {
        addr: String,
        text: String,
    },
    /// Format every run of a paragraph (`slide[0]/sp[1]/p[0]`) or of every
    /// paragraph of a shape (`slide[0]/sp[1]`).
    SetShapeTextFormat {
        addr: String,
        #[serde(flatten)]
        format: TextFormat,
        #[serde(default)]
        align: Option<String>,
        /// `bullet`, `number` or `none`.
        #[serde(default)]
        list: Option<String>,
    },
    /// A new paragraph after `addr` in the same shape.
    InsertShapeParagraph {
        addr: String,
        text: String,
    },
    SetShapeFill {
        addr: String,
        /// `RRGGBB`; empty removes the fill.
        color: String,
    },
    SetShapeBox {
        addr: String,
        x: i64,
        y: i64,
        cx: i64,
        cy: i64,
    },
    DeleteShape {
        addr: String,
    },
    /// A text box on slide `addr`.
    InsertTextBox {
        addr: String,
        x: i64,
        y: i64,
        cx: i64,
        cy: i64,
        text: String,
    },
    /// A preset geometry (`rect`, `roundRect`, `ellipse`, `triangle`,
    /// `rightArrow`, `line`) on slide `addr`.
    InsertShape {
        addr: String,
        geometry: String,
        x: i64,
        y: i64,
        cx: i64,
        cy: i64,
        #[serde(default)]
        fill: Option<String>,
    },
    /// A table on slide `addr`.
    InsertSlideTable {
        addr: String,
        rows: usize,
        cols: usize,
        x: i64,
        y: i64,
        cx: i64,
        cy: i64,
    },
    /// A new slide after slide `after`, using that slide's layout.
    AddSlide {
        after: usize,
    },
    DuplicateSlide {
        slide: usize,
    },
    DeleteSlide {
        slide: usize,
    },

    // ---- PDF ----
    AddAnnotation {
        page: usize,
        kind: String,
        rect: [f32; 4],
        #[serde(default)]
        contents: Option<String>,
        /// RGB 0-255. Defaults to the highlighter yellow when absent.
        #[serde(default)]
        color: Option<[u8; 3]>,
    },
    DeleteAnnotation {
        page: usize,
        index: usize,
    },
    DeletePage {
        page: usize,
    },
    /// Clockwise, in degrees, relative to the page's current rotation.
    /// Only quarter turns; anything else is refused rather than rounded.
    RotatePage {
        page: usize,
        degrees: i32,
    },
    // NOTE: there is deliberately no page-reorder operation.
    //
    // pdfium exposes `FPDF_MovePages`, but pdfium-render keeps the document
    // handle it needs `pub(crate)`, so it cannot be reached from here. The
    // usual workaround — copy the pages into a fresh document in the new order
    // — produces a document that has lost the original's form fields,
    // bookmarks, attachments and metadata. Reordering two pages must not
    // quietly discard the rest of the file, so this stays unimplemented until
    // the safe binding exists.

    // ---- TXT ----
    SetText {
        content: String,
    },
}

impl DraftPatch {
    /// Human name used in the "that edit does not apply to this format" error.
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::SetRunText { .. } => "SetRunText",
            Self::SetRunFormat { .. } => "SetRunFormat",
            Self::FormatRange { .. } => "FormatRange",
            Self::SetParagraphStyle { .. } => "SetParagraphStyle",
            Self::SetParagraphFormat { .. } => "SetParagraphFormat",
            Self::SetParagraphText { .. } => "SetParagraphText",
            Self::InsertParagraphAfter { .. } => "InsertParagraphAfter",
            Self::SplitParagraph { .. } => "SplitParagraph",
            Self::MergeParagraphWithPrevious { .. } => "MergeParagraphWithPrevious",
            Self::DeleteParagraph { .. } => "DeleteParagraph",
            Self::DeleteRun { .. } => "DeleteRun",
            Self::InsertTable { .. } => "InsertTable",
            Self::InsertTableRow { .. } => "InsertTableRow",
            Self::DeleteTableRow { .. } => "DeleteTableRow",
            Self::InsertTableColumn { .. } => "InsertTableColumn",
            Self::DeleteTableColumn { .. } => "DeleteTableColumn",
            Self::DeleteTable { .. } => "DeleteTable",
            Self::InsertImage { .. } => "InsertImage",
            Self::SetImageSize { .. } => "SetImageSize",
            Self::InsertEquation { .. } => "InsertEquation",
            Self::SetEquation { .. } => "SetEquation",
            Self::InsertPageBreak { .. } => "InsertPageBreak",
            Self::SetCellValue { .. } => "SetCellValue",
            Self::ClearCell { .. } => "ClearCell",
            Self::SetCellFormat { .. } => "SetCellFormat",
            Self::SetShapeText { .. } => "SetShapeText",
            Self::SetShapeTextFormat { .. } => "SetShapeTextFormat",
            Self::InsertShapeParagraph { .. } => "InsertShapeParagraph",
            Self::SetShapeFill { .. } => "SetShapeFill",
            Self::SetShapeBox { .. } => "SetShapeBox",
            Self::DeleteShape { .. } => "DeleteShape",
            Self::InsertTextBox { .. } => "InsertTextBox",
            Self::InsertShape { .. } => "InsertShape",
            Self::InsertSlideTable { .. } => "InsertSlideTable",
            Self::AddSlide { .. } => "AddSlide",
            Self::DuplicateSlide { .. } => "DuplicateSlide",
            Self::DeleteSlide { .. } => "DeleteSlide",
            Self::AddAnnotation { .. } => "AddAnnotation",
            Self::DeleteAnnotation { .. } => "DeleteAnnotation",
            Self::DeletePage { .. } => "DeletePage",
            Self::RotatePage { .. } => "RotatePage",
            Self::SetText { .. } => "SetText",
        }
    }
}

/// Build the view model for a draft's current bytes.
pub fn build(format: &str, bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    match format {
        "docx" => docx::build(bytes, version),
        "xlsx" => xlsx::build(bytes, version),
        "pptx" => pptx::build(bytes, version),
        "pdf" => pdf_workspace::build(bytes, version),
        "txt" => text_workspace::build(bytes, version),
        other => Err(anyhow!("'{}' is not an editable workspace format", other)),
    }
}

/// Apply patches, returning the new file bytes.
pub fn apply(format: &str, bytes: &[u8], patches: &[DraftPatch]) -> Result<Vec<u8>> {
    if patches.is_empty() {
        return Ok(bytes.to_vec());
    }
    match format {
        "docx" => docx::apply(bytes, patches),
        "xlsx" => xlsx::apply(bytes, patches),
        "pptx" => pptx::apply(bytes, patches),
        "pdf" => pdf_workspace::apply(bytes, patches),
        "txt" => text_workspace::apply(bytes, patches),
        other => Err(anyhow!("'{}' is not an editable workspace format", other)),
    }
}

/// The error raised when a patch reaches the wrong format's handler.
///
/// This is a caller bug, not user error, so it names the operation and the
/// format rather than saying "invalid request".
pub fn wrong_format(patch: &DraftPatch, format: &str) -> anyhow::Error {
    anyhow!(
        "The edit '{}' does not apply to a {} document. This is a bug in the editor, \
         not something you did — the document has not been changed.",
        patch.op_name(),
        format.to_uppercase()
    )
}

/// Flatten a view model to plain text for the search index.
///
/// # Why the join characters differ by format
///
/// FTS5 tokenises on non-alphanumerics, so what separates two fragments only
/// matters where it could fuse two words into one nonexistent token or split
/// one real word in two. Paragraphs and cells are joined with a newline and a
/// space respectively; a spreadsheet row reads as a line so that "Governing
/// law" in A1 and "England" in B1 do not become "lawEngland".
///
/// Formulas are indexed alongside their cached values: someone looking for
/// where a number came from searches for `VLOOKUP`, not for `412000`.
pub fn plain_text(model: &DraftViewModel) -> String {
    let mut out = String::new();

    match model {
        DraftViewModel::Docx { blocks, .. } => {
            for block in blocks {
                for run in &block.runs {
                    out.push_str(&run.text);
                }
                out.push('\n');
            }
        }
        DraftViewModel::Txt { content, .. } => out.push_str(content),
        DraftViewModel::Xlsx { sheets, .. } => {
            for sheet in sheets {
                // The sheet name is content: "Schedule 2" is a real thing to
                // search for and exists nowhere else in the document.
                out.push_str(&sheet.name);
                out.push('\n');
                // Iterating the map gives no row order, so cells are grouped
                // by their row prefix to keep a row on one line.
                let mut cells: Vec<_> = sheet.cells.values().collect();
                cells.sort_by(|a, b| a.addr.cmp(&b.addr));
                for cell in cells {
                    if !cell.value.is_empty() {
                        out.push_str(&cell.value);
                        out.push(' ');
                    }
                    if let Some(f) = &cell.formula {
                        out.push_str(f);
                        out.push(' ');
                    }
                }
                out.push('\n');
            }
        }
        DraftViewModel::Pptx { slides, .. } => {
            for slide in slides {
                for shape in &slide.shapes {
                    for para in &shape.paragraphs {
                        out.push_str(&para.text);
                        out.push('\n');
                    }
                }
            }
        }
        // A PDF's text lives in the Vault's extraction pipeline, not in this
        // model — the model carries only geometry and annotations. Annotation
        // contents ARE ours, though, and are often the only thing anyone
        // wrote on the document.
        DraftViewModel::Pdf { pages, .. } => {
            for page in pages {
                for ann in &page.annotations {
                    if let Some(text) = &ann.contents {
                        out.push_str(text);
                        out.push('\n');
                    }
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_round_trip_through_json_with_their_op_tag() {
        let p = DraftPatch::SetRunText {
            addr: "body/p[12]/r[0]".into(),
            text: "Amended".into(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"op\":\"SetRunText\""), "{}", json);

        let back: DraftPatch = serde_json::from_str(&json).unwrap();
        match back {
            DraftPatch::SetRunText { addr, text } => {
                assert_eq!(addr, "body/p[12]/r[0]");
                assert_eq!(text, "Amended");
            }
            other => panic!("round-tripped into {:?}", other),
        }
    }

    #[test]
    fn an_unknown_format_is_refused_by_name() {
        let err = build("rtf", b"", 1).unwrap_err().to_string();
        assert!(err.contains("rtf"), "{}", err);
    }

    #[test]
    fn a_misdirected_patch_says_so_and_promises_the_document_is_intact() {
        let p = DraftPatch::SetCellValue { addr: "sheet[0]/A1".into(), value: "1".into() };
        let msg = wrong_format(&p, "docx").to_string();
        assert!(msg.contains("SetCellValue"), "{}", msg);
        assert!(msg.contains("DOCX"), "{}", msg);
        assert!(msg.contains("has not been changed"), "{}", msg);
    }
}
