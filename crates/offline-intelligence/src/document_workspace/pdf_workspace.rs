//! PDF: page geometry, annotation CRUD and page rendering, all through pdfium.
//!
//! # Why pdfium and not pdf.js
//!
//! The engine is already shipped for text extraction, so rendering costs the
//! user no extra installer bytes and runs as native C++ rather than as WASM in
//! the WebView. The UI fetches page images from `/drafts/:id/page/:n` instead
//! of decoding the document itself.
//!
//! # Serialisation
//!
//! `bind_pdfium()` acquires the process-wide pdfium permit and hands back an
//! instance that cannot outlive it. That means page rendering queues behind an
//! in-flight OCR pass on a large scanned PDF — deliberately. The alternative is
//! two live pdfium bindings in one process, which is how that library corrupts
//! state. The API layer caches rendered pages on disk so the queue is paid once
//! per page, not once per scroll.
//!
//! # Why there is no text editing
//!
//! A PDF has no paragraph model — text is positioned glyph runs with no notion
//! of a line, let alone a sentence that can reflow. Editing a word means
//! re-laying out everything after it, which no offline engine does correctly.
//! Annotating, filling and reorganising is what document review actually needs,
//! and all three are exact.

use anyhow::{anyhow, Context, Result};
use pdfium_render::prelude::*;
use tracing::{debug, info};

use super::view_model::{wrong_format, DraftPatch, DraftViewModel, PdfAnnotation, PdfPage};
use crate::utils::pdf_text::bind_pdfium;

/// Highlighter yellow, used when the client does not name a colour.
const DEFAULT_ANNOTATION_COLOR: [u8; 3] = [255, 235, 59];

/// Largest page bitmap we will produce, per side.
///
/// A 200-inch architectural drawing at 4x scale would otherwise ask pdfium for
/// a bitmap measured in gigabytes. Clamping keeps a pathological page slow
/// rather than fatal.
const MAX_RENDER_PX: i32 = 4000;

pub fn build(bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    let pdfium = bind_pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|e| password_aware_error(e))?;

    let mut pages = Vec::new();
    for (index, page) in doc.pages().iter().enumerate() {
        let mut annotations = Vec::new();
        for (a_index, annotation) in page.annotations().iter().enumerate() {
            // An annotation with unreadable bounds is reported at the origin
            // rather than dropped: the user must be able to see and delete it.
            let rect = annotation
                .bounds()
                .map(|b| [b.left().value, b.bottom().value, b.right().value, b.top().value])
                .unwrap_or([0.0; 4]);
            annotations.push(PdfAnnotation {
                index: a_index,
                kind: format!("{:?}", annotation.annotation_type()),
                rect,
                contents: annotation.contents(),
            });
        }

        pages.push(PdfPage {
            index,
            width: page.width().value,
            height: page.height().value,
            annotations,
        });
    }

    if pages.is_empty() {
        return Err(anyhow!("This PDF contains no pages."));
    }
    debug!("PDF view model: {} pages", pages.len());
    Ok(DraftViewModel::Pdf { version, pages })
}

pub fn apply(bytes: &[u8], patches: &[DraftPatch]) -> Result<Vec<u8>> {
    let pdfium = bind_pdfium()?;
    let mut doc = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|e| password_aware_error(e))?;

    // Deletions are applied by index, so they must all be resolved against the
    // page as it is now. Applying them highest-index-first means removing one
    // annotation never shifts the index of another still to be removed.
    let mut ordered: Vec<&DraftPatch> = patches.iter().collect();
    ordered.sort_by_key(|p| match p {
        DraftPatch::DeleteAnnotation { index, .. } => (0i8, std::cmp::Reverse(*index)),
        _ => (1i8, std::cmp::Reverse(0)),
    });

    for patch in ordered {
        match patch {
            DraftPatch::AddAnnotation { page, kind, rect, contents, color } => {
                add_annotation(&mut doc, *page, kind, *rect, contents.as_deref(), *color)?;
            }
            DraftPatch::DeleteAnnotation { page, index } => {
                delete_annotation(&mut doc, *page, *index)?;
            }
            DraftPatch::DeletePage { page } => delete_page(&mut doc, *page)?,
            DraftPatch::RotatePage { page, degrees } => rotate_page(&mut doc, *page, *degrees)?,
            other => return Err(wrong_format(other, "pdf")),
        }
    }

    doc.save_to_bytes()
        .map_err(|e| anyhow!("This PDF could not be saved: {}", e))
}

fn add_annotation(
    doc: &mut PdfDocument,
    page_index: usize,
    kind: &str,
    rect: [f32; 4],
    contents: Option<&str>,
    color: Option<[u8; 3]>,
) -> Result<()> {
    let page_count = doc.pages().len() as usize;
    let mut page = doc
        .pages()
        .get(page_index as u16)
        .map_err(|_| out_of_range_page(page_index, page_count))?;

    // PdfRect orders its arguments bottom, left, top, right — not the
    // left/top/right/bottom order every web API uses. Getting this wrong
    // silently places annotations in the wrong corner of the page.
    let bounds = PdfRect::new_from_values(rect[1], rect[0], rect[3], rect[2]);
    let [r, g, b] = color.unwrap_or(DEFAULT_ANNOTATION_COLOR);
    let fill = PdfColor::new(r, g, b, 255);

    let annotations = &mut page.annotations_mut();

    // Each `create_*` returns its own concrete type and they share no common
    // enum, so the shared setup is applied by macro at each arm rather than by
    // erasing them to one type. `PdfPageAnnotationCommon` is what makes the
    // three calls below valid for all of them.
    macro_rules! finish {
        ($created:expr) => {{
            let mut annotation = $created;
            annotation.set_bounds(bounds)?;
            // Not every annotation type carries a fill colour (a strikeout
            // draws in its stroke colour), so a refusal here is not a failure.
            let _ = annotation.set_fill_color(fill);
            if let Some(text) = contents {
                // Free-text and notes already carry their text; setting it
                // again is what attaches a comment to a highlight.
                let _ = annotation.set_contents(text);
            }
        }};
    }

    match kind.to_ascii_lowercase().as_str() {
        "highlight" => finish!(annotations.create_highlight_annotation()?),
        "strikeout" => finish!(annotations.create_strikeout_annotation()?),
        "underline" => finish!(annotations.create_underline_annotation()?),
        "squiggly" => finish!(annotations.create_squiggly_annotation()?),
        "square" => finish!(annotations.create_square_annotation()?),
        "ink" => finish!(annotations.create_ink_annotation()?),
        "freetext" | "free_text" => {
            finish!(annotations.create_free_text_annotation(contents.unwrap_or_default())?)
        }
        // A sticky note. The PDF spec calls this "text", which is confusing
        // enough next to free-text that "note" is accepted too.
        "text" | "note" => {
            finish!(annotations.create_text_annotation(contents.unwrap_or_default())?)
        }
        other => {
            return Err(anyhow!(
                "'{}' is not an annotation type this workspace can create. \
                 Supported: highlight, strikeout, underline, squiggly, square, ink, \
                 freetext, note.",
                other
            ))
        }
    }

    info!("Added a {} annotation to page {}", kind, page_index);
    Ok(())
}

fn delete_annotation(doc: &mut PdfDocument, page_index: usize, index: usize) -> Result<()> {
    let page_count = doc.pages().len() as usize;
    let mut page = doc
        .pages()
        .get(page_index as u16)
        .map_err(|_| out_of_range_page(page_index, page_count))?;

    let annotations = &mut page.annotations_mut();
    let count = annotations.len() as usize;
    let annotation = annotations.get(index).map_err(|_| {
        anyhow!(
            "Annotation {} no longer exists on page {} (it has {}). \
             Reopen the draft to pick up the current version.",
            index,
            page_index + 1,
            count
        )
    })?;

    annotations
        .delete_annotation(annotation)
        .map_err(|e| anyhow!("That annotation could not be removed: {}", e))?;
    Ok(())
}

fn delete_page(doc: &mut PdfDocument, page_index: usize) -> Result<()> {
    let count = doc.pages().len() as usize;
    if count <= 1 {
        return Err(anyhow!(
            "This is the only page in the document, and a PDF cannot have none. \
             Delete the draft instead if you no longer need it."
        ));
    }
    if page_index >= count {
        return Err(out_of_range_page(page_index, count));
    }
    let page = doc
        .pages()
        .get(page_index as u16)
        .map_err(|_| out_of_range_page(page_index, count))?;
    page.delete()
        .map_err(|e| anyhow!("Page {} could not be removed: {}", page_index + 1, e))?;
    info!("Deleted page {} of {}", page_index + 1, count);
    Ok(())
}

/// Rotate a page clockwise, relative to whatever rotation it already has.
///
/// PDF stores rotation as a quarter-turn count, so anything that is not a
/// multiple of 90 is refused rather than silently rounded to one — a page
/// quietly turned 90° when the caller asked for 45° is worse than an error.
fn rotate_page(doc: &mut PdfDocument, page_index: usize, degrees: i32) -> Result<()> {
    if degrees % 90 != 0 {
        return Err(anyhow!(
            "A PDF page can only be rotated in quarter turns, so {}° is not possible.",
            degrees
        ));
    }
    let count = doc.pages().len() as usize;
    let mut page = doc
        .pages()
        .get(page_index as u16)
        .map_err(|_| out_of_range_page(page_index, count))?;

    let current = page.rotation().unwrap_or(PdfPageRenderRotation::None);
    // rem_euclid, not %, so a negative (anticlockwise) turn lands in 0..3
    // rather than staying negative and falling through to the error arm.
    let quarters = (current.as_degrees() as i32 / 90 + degrees / 90).rem_euclid(4);

    page.set_rotation(match quarters {
        0 => PdfPageRenderRotation::None,
        1 => PdfPageRenderRotation::Degrees90,
        2 => PdfPageRenderRotation::Degrees180,
        _ => PdfPageRenderRotation::Degrees270,
    });
    info!("Rotated page {} by {}°", page_index + 1, degrees);
    Ok(())
}

/// Render one page to PNG bytes at `scale` times its natural size.
///
/// Returns PNG rather than raw pixels so the result can be written straight to
/// the page cache and served to an `<img>` unchanged.
pub fn render_page(bytes: &[u8], page_index: usize, scale: f32) -> Result<Vec<u8>> {
    if !(scale.is_finite() && scale > 0.0) {
        return Err(anyhow!("A render scale of {} is not usable.", scale));
    }

    let pdfium = bind_pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|e| password_aware_error(e))?;

    let page_count = doc.pages().len() as usize;
    let page = doc
        .pages()
        .get(page_index as u16)
        .map_err(|_| out_of_range_page(page_index, page_count))?;

    let width = ((page.width().value * scale).round() as i32).clamp(1, MAX_RENDER_PX);
    let height = ((page.height().value * scale).round() as i32).clamp(1, MAX_RENDER_PX);

    let bitmap = page
        .render(width, height, None)
        .map_err(|e| anyhow!("Page {} could not be rendered: {}", page_index + 1, e))?;

    let image = bitmap.as_image();
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .context("could not encode the rendered page as PNG")?;

    debug!("Rendered PDF page {} at {}x{}", page_index, width, height);
    Ok(png.into_inner())
}

/// How many pages, without building the whole view model.
pub fn page_count(bytes: &[u8]) -> Result<usize> {
    let pdfium = bind_pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|e| password_aware_error(e))?;
    Ok(doc.pages().len() as usize)
}

fn out_of_range_page(index: usize, count: usize) -> anyhow::Error {
    anyhow!(
        "Page {} does not exist in this PDF (it has {}). \
         Reopen the draft to pick up the current version.",
        index + 1,
        count
    )
}

/// pdfium reports an encrypted file as a generic load failure. Since a
/// password-protected PDF is far and away the most common reason a real file
/// fails to open, the message says so rather than leaving the user with a
/// numeric error code.
fn password_aware_error(e: PdfiumError) -> anyhow::Error {
    anyhow!(
        "This PDF could not be opened ({}). If it is password-protected, \
         remove the protection and add it again.",
        e
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-page PDF built by pdfium itself, so the tests never depend on a
    /// checked-in binary or on anyone's real document.
    fn blank_pdf() -> Option<Vec<u8>> {
        let pdfium = bind_pdfium().ok()?;
        let mut doc = pdfium.create_new_pdf().ok()?;
        doc.pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .ok()?;
        doc.save_to_bytes().ok()
    }

    /// Every test here needs the real pdfium binary. On a machine where it is
    /// not resolvable the tests skip rather than fail, matching how the
    /// existing pdf_text tests behave.
    macro_rules! pdf_or_skip {
        () => {
            match blank_pdf() {
                Some(b) => b,
                None => {
                    eprintln!("pdfium unavailable; skipping");
                    return;
                }
            }
        };
    }

    #[test]
    fn page_geometry_is_reported_in_points() {
        let bytes = pdf_or_skip!();
        let DraftViewModel::Pdf { pages, .. } = build(&bytes, 1).unwrap() else { panic!() };
        assert_eq!(pages.len(), 1);
        // A4 is 595 x 842 points.
        assert!((pages[0].width - 595.0).abs() < 2.0, "width {}", pages[0].width);
        assert!((pages[0].height - 842.0).abs() < 2.0, "height {}", pages[0].height);
        assert!(pages[0].annotations.is_empty());
    }

    #[test]
    fn an_annotation_can_be_added_read_back_and_deleted() {
        let bytes = pdf_or_skip!();

        let with = apply(
            &bytes,
            &[DraftPatch::AddAnnotation {
                page: 0,
                kind: "highlight".into(),
                rect: [72.0, 700.0, 300.0, 720.0],
                contents: Some("Check this indemnity".into()),
                color: None,
            }],
        )
        .unwrap();

        let DraftViewModel::Pdf { pages, .. } = build(&with, 2).unwrap() else { panic!() };
        assert_eq!(pages[0].annotations.len(), 1);
        let a = &pages[0].annotations[0];
        assert_eq!(a.contents.as_deref(), Some("Check this indemnity"));
        // Bounds are reported left, bottom, right, top.
        assert!((a.rect[0] - 72.0).abs() < 1.0, "rect {:?}", a.rect);
        assert!((a.rect[3] - 720.0).abs() < 1.0, "rect {:?}", a.rect);

        let without = apply(&with, &[DraftPatch::DeleteAnnotation { page: 0, index: 0 }]).unwrap();
        let DraftViewModel::Pdf { pages, .. } = build(&without, 3).unwrap() else { panic!() };
        assert!(pages[0].annotations.is_empty());
    }

    /// Deleting several annotations in one batch must not have earlier
    /// deletions shift the indices of later ones.
    #[test]
    fn deleting_several_annotations_at_once_removes_the_intended_ones() {
        let bytes = pdf_or_skip!();
        let mut with = bytes;
        for i in 0..3 {
            with = apply(
                &with,
                &[DraftPatch::AddAnnotation {
                    page: 0,
                    kind: "square".into(),
                    rect: [72.0, 100.0 * (i + 1) as f32, 200.0, 100.0 * (i + 1) as f32 + 20.0],
                    contents: Some(format!("note {}", i)),
                    color: None,
                }],
            )
            .unwrap();
        }

        let after = apply(
            &with,
            &[
                DraftPatch::DeleteAnnotation { page: 0, index: 0 },
                DraftPatch::DeleteAnnotation { page: 0, index: 2 },
            ],
        )
        .unwrap();

        let DraftViewModel::Pdf { pages, .. } = build(&after, 2).unwrap() else { panic!() };
        assert_eq!(pages[0].annotations.len(), 1);
        assert_eq!(
            pages[0].annotations[0].contents.as_deref(),
            Some("note 1"),
            "the middle annotation is the one that should survive"
        );
    }

    #[test]
    fn an_unknown_annotation_type_lists_the_supported_ones() {
        let bytes = pdf_or_skip!();
        let err = apply(
            &bytes,
            &[DraftPatch::AddAnnotation {
                page: 0,
                kind: "hologram".into(),
                rect: [0.0, 0.0, 10.0, 10.0],
                contents: None,
                color: None,
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hologram"), "{}", err);
        assert!(err.contains("highlight"), "{}", err);
    }

    #[test]
    fn an_out_of_range_page_is_refused_with_advice() {
        let bytes = pdf_or_skip!();
        let err = apply(
            &bytes,
            &[DraftPatch::AddAnnotation {
                page: 9, kind: "highlight".into(), rect: [0.0, 0.0, 10.0, 10.0],
                contents: None, color: None,
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not exist"), "{}", err);
        assert!(err.contains("Reopen the draft"), "{}", err);
    }

    #[test]
    fn a_page_renders_to_a_png_whose_size_follows_the_scale() {
        let bytes = pdf_or_skip!();
        let png = render_page(&bytes, 0, 1.0).unwrap();
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

        let bigger = render_page(&bytes, 0, 2.0).unwrap();
        assert!(
            bigger.len() > png.len(),
            "a 2x render should carry more data than a 1x one ({} vs {})",
            bigger.len(),
            png.len()
        );
    }

    #[test]
    fn a_nonsensical_render_scale_is_refused() {
        let bytes = pdf_or_skip!();
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(render_page(&bytes, 0, bad).is_err(), "scale {} should be refused", bad);
        }
    }

    #[test]
    fn a_non_pdf_fails_with_a_message_that_mentions_password_protection() {
        if bind_pdfium().is_err() {
            return;
        }
        let err = build(b"not a pdf at all", 1).unwrap_err().to_string();
        assert!(err.contains("password-protected"), "{}", err);
    }

    /// A blank document with `n` pages, for the page-operation tests.
    fn pdf_with_pages(n: usize) -> Option<Vec<u8>> {
        let pdfium = bind_pdfium().ok()?;
        let mut doc = pdfium.create_new_pdf().ok()?;
        for _ in 0..n {
            doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).ok()?;
        }
        doc.save_to_bytes().ok()
    }

    #[test]
    fn a_page_can_be_deleted() {
        let Some(bytes) = pdf_with_pages(3) else { return };
        assert_eq!(page_count(&bytes).unwrap(), 3);

        let after = apply(&bytes, &[DraftPatch::DeletePage { page: 1 }]).unwrap();
        assert_eq!(page_count(&after).unwrap(), 2);
    }

    /// A PDF cannot have zero pages, so the last one is protected — and the
    /// message says what to do instead rather than just refusing.
    #[test]
    fn the_last_remaining_page_cannot_be_deleted() {
        let Some(bytes) = pdf_with_pages(1) else { return };
        let err = apply(&bytes, &[DraftPatch::DeletePage { page: 0 }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("only page"), "{}", err);
        assert!(err.contains("Delete the draft"), "{}", err);
    }

    /// Rotating swaps the reported width and height, which is the observable
    /// difference between "rotated" and "not rotated".
    #[test]
    fn rotating_a_page_by_a_quarter_turn_swaps_its_dimensions() {
        let Some(bytes) = pdf_with_pages(1) else { return };
        let DraftViewModel::Pdf { pages, .. } = build(&bytes, 1).unwrap() else { panic!() };
        let (w, h) = (pages[0].width, pages[0].height);
        assert!(h > w, "A4 starts portrait");

        let turned = apply(&bytes, &[DraftPatch::RotatePage { page: 0, degrees: 90 }]).unwrap();
        let DraftViewModel::Pdf { pages, .. } = build(&turned, 2).unwrap() else { panic!() };
        assert!((pages[0].width - h).abs() < 1.0, "{:?}", pages[0]);
        assert!((pages[0].height - w).abs() < 1.0, "{:?}", pages[0]);
    }

    /// Rotation accumulates, and four quarter turns come back to where it
    /// started rather than drifting.
    #[test]
    fn rotation_is_relative_and_wraps_around() {
        let Some(bytes) = pdf_with_pages(1) else { return };
        let mut current = bytes.clone();
        for _ in 0..4 {
            current = apply(&current, &[DraftPatch::RotatePage { page: 0, degrees: 90 }]).unwrap();
        }
        let DraftViewModel::Pdf { pages, .. } = build(&current, 2).unwrap() else { panic!() };
        let DraftViewModel::Pdf { pages: original, .. } = build(&bytes, 1).unwrap() else { panic!() };
        assert!((pages[0].width - original[0].width).abs() < 1.0);
        assert!((pages[0].height - original[0].height).abs() < 1.0);
    }

    #[test]
    fn an_anticlockwise_turn_is_handled_rather_than_landing_on_a_negative_index() {
        let Some(bytes) = pdf_with_pages(1) else { return };
        let turned = apply(&bytes, &[DraftPatch::RotatePage { page: 0, degrees: -90 }]).unwrap();
        let DraftViewModel::Pdf { pages, .. } = build(&turned, 2).unwrap() else { panic!() };
        assert!(pages[0].width > pages[0].height, "should now be landscape");
    }

    #[test]
    fn a_rotation_that_is_not_a_quarter_turn_is_refused_rather_than_rounded() {
        let Some(bytes) = pdf_with_pages(1) else { return };
        let err = apply(&bytes, &[DraftPatch::RotatePage { page: 0, degrees: 45 }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("quarter turns"), "{}", err);
    }

    #[test]
    fn a_docx_patch_is_refused() {
        let bytes = pdf_or_skip!();
        let err = apply(&bytes, &[DraftPatch::SetRunText { addr: "body/p[0]/r[0]".into(), text: "x".into() }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("SetRunText"), "{}", err);
    }
}
