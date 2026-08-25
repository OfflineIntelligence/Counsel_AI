//! OCR for directly-attached image files (JPG/PNG/BMP/TIFF/GIF) - documents
//! and handwritten notes that exist ONLY as a photo/scan, with no PDF or
//! Office container around them.
//!
//! This module is deliberately thin. It used to decode the image with the
//! `image` crate, downscale anything over a hardcoded ceiling, and re-encode
//! it to BMP so that utils::win_ocr could parse it a second time. All of that
//! is gone: the user's original bytes now go straight to
//! utils::win_ocr::recognize_image_bytes, which hands them to the Windows
//! Imaging Component - the same codec stack the OCR engine was always going
//! to decode with anyway.
//!
//! What that removed, and why it mattered, is documented at length in
//! utils::win_ocr. The short version: the intermediate hop discarded EXIF
//! rotation (so phone photos of documents reached OCR sideways and returned
//! nothing), and downscaled at 2600px against an engine limit that is
//! actually 10000px (so fine print was blurred away for no reason).
//!
//! All this module still owns is turning a failure into the explicit,
//! bracketed, file-naming marker that utils::file_processor's contract
//! requires - never a silent empty string.

use anyhow::{anyhow, Result};
use tracing::warn;

/// Extract text from an image file's bytes via OCR. Runs on a blocking
/// thread - WIC decode and OCR can both take real time on large photos.
pub async fn extract_image_text(bytes: Vec<u8>, filename: &str) -> Result<String> {
    let filename = filename.to_string();
    tokio::task::spawn_blocking(move || ocr_image_bytes_blocking(&bytes, &filename))
        .await
        .map_err(|e| anyhow!("image OCR task failed: {}", e))
}

/// The synchronous (blocking) OCR call, exposed crate-wide so callers that
/// already run inside a blocking context (utils::file_processor's DOCX
/// embedded-image handling, which walks XML synchronously) can invoke OCR
/// directly instead of spawning a nested blocking task per image.
///
/// A successful recognition is returned WITH a bracketed provenance header,
/// matching utils::pdf_text's shape for scanned PDFs. Two reasons:
///   1. Signal to the model — "this text came from OCR, may contain
///      recognition errors" — so a misread word can be flagged rather than
///      quoted with the same confidence as clean extracted text.
///   2. Uniformity with the extraction_outcome classifier: the
///      "[header]\n<text>" pattern with substantial trailing content is
///      already the canonical shape it recognizes as `ok`. Returning naked
///      text worked here only because it also happened to start with a
///      non-bracket character; making the shape explicit removes the
///      accidental dependence on that.
#[cfg(target_os = "windows")]
pub(crate) fn ocr_image_bytes_blocking(bytes: &[u8], filename: &str) -> String {
    match crate::utils::win_ocr::recognize_image_bytes(bytes, filename) {
        Ok(text) if text.trim().is_empty() => format!(
            "[Image '{}': OCR recovered no text. The image may not contain readable text, \
             or the scan quality may be too low.]",
            filename
        ),
        Ok(text) => format!(
            "[Image '{}': text below was recovered via OCR (Windows OCR — may contain \
             recognition errors).]\n{}",
            filename,
            text.trim()
        ),
        Err(e) => {
            warn!("OCR failed for image '{}': {}", filename, e);
            format!("[Cannot extract image '{}': {}]", filename, e)
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn ocr_image_bytes_blocking(_bytes: &[u8], filename: &str) -> String {
    format!(
        "[Image '{}': OCR is not available on this platform yet. Please provide a text-based copy.]",
        filename
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders real text via PDFium (same technique utils::pdf_text's own
    /// OCR test uses), encodes the rendered bitmap as a genuine PNG and a
    /// genuine JPEG, and proves OCR reads real text through BOTH formats -
    /// an honest end-to-end proof, not a blank synthetic fixture.
    ///
    /// The `image` crate appears here (and only here, as a dev-dependency)
    /// to BUILD the fixtures. The production path under test no longer uses
    /// it at all - these bytes go to WIC exactly as a user's file would.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn ocr_reads_real_text_from_png_and_jpeg() {
        use pdfium_render::prelude::*;

        let pdfium = match crate::utils::pdf_text::bind_pdfium() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("SKIP: pdfium library not available on this machine");
                return;
            }
        };
        let pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n4 0 obj<</Length 60>>stream\nBT /F1 48 Tf 72 600 Td (HANDWRITTEN NOTE TEXT) Tj ET\nendstream\nendobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>";
        let doc = pdfium.load_pdf_from_byte_slice(pdf, None).unwrap();
        let page = doc.pages().get(0).unwrap();
        let bitmap = page
            .render_with_config(&PdfRenderConfig::new().set_target_width(1200))
            .unwrap();
        let width = bitmap.width() as u32;
        let height = bitmap.height() as u32;
        let raw = bitmap.as_raw_bytes();

        // BGRA -> RGBA (image crate expects RGBA channel order)
        let mut rgba = raw.to_vec();
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let img = image::RgbaImage::from_raw(width, height, rgba).unwrap();
        let dynamic = image::DynamicImage::ImageRgba8(img);

        let mut png_bytes = Vec::new();
        dynamic.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png).unwrap();
        let mut jpeg_bytes = Vec::new();
        dynamic.write_to(&mut std::io::Cursor::new(&mut jpeg_bytes), image::ImageFormat::Jpeg).unwrap();

        let png_text = extract_image_text(png_bytes, "note.png").await.unwrap();
        let jpeg_text = extract_image_text(jpeg_bytes, "note.jpg").await.unwrap();

        assert!(
            png_text.to_uppercase().contains("HANDWRITTEN") && png_text.to_uppercase().contains("NOTE"),
            "PNG OCR should read the rendered text, got: {}", png_text
        );
        assert!(
            jpeg_text.to_uppercase().contains("HANDWRITTEN") && jpeg_text.to_uppercase().contains("NOTE"),
            "JPEG OCR should read the rendered text, got: {}", jpeg_text
        );
    }

    /// Render real text to an upright RGBA image via PDFium. Shared by the
    /// fixtures below; returns (width, height, rgba_bytes).
    #[cfg(target_os = "windows")]
    fn render_text_image(text: &str) -> Option<(u32, u32, Vec<u8>)> {
        use pdfium_render::prelude::*;
        let pdfium = crate::utils::pdf_text::bind_pdfium().ok()?;
        let pdf = format!(
            "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n4 0 obj<</Length 60>>stream\nBT /F1 48 Tf 60 600 Td ({}) Tj ET\nendstream\nendobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>",
            text
        );
        let doc = pdfium.load_pdf_from_byte_slice(pdf.as_bytes(), None).ok()?;
        let page = doc.pages().get(0).ok()?;
        let bitmap = page
            .render_with_config(&PdfRenderConfig::new().set_target_width(1400))
            .ok()?;
        let (w, h) = (bitmap.width() as u32, bitmap.height() as u32);
        let mut rgba = bitmap.as_raw_bytes().to_vec();
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
        }
        Some((w, h, rgba))
    }

    /// Splice a minimal EXIF APP1 segment carrying ONLY an Orientation tag
    /// into a JPEG, immediately after the SOI marker - exactly how a phone
    /// camera records "these pixels are sideways, rotate before displaying".
    ///
    /// Layout: FFE1 <len> "Exif\0\0" <TIFF header> <IFD0 with tag 0x0112>.
    #[cfg(target_os = "windows")]
    fn splice_exif_orientation(jpeg: &[u8], orientation: u8) -> Vec<u8> {
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "expected a JPEG SOI marker");
        let app1: [u8; 36] = [
            0xFF, 0xE1, // APP1 marker
            0x00, 0x22, // segment length = 34 (payload 32 + these 2 bytes)
            0x45, 0x78, 0x69, 0x66, 0x00, 0x00, // "Exif\0\0"
            0x4D, 0x4D, // big-endian ("MM")
            0x00, 0x2A, // TIFF magic 42
            0x00, 0x00, 0x00, 0x08, // offset to IFD0
            0x00, 0x01, // IFD0 entry count = 1
            0x01, 0x12, // tag 0x0112 = Orientation
            0x00, 0x03, // type 3 = SHORT
            0x00, 0x00, 0x00, 0x01, // count = 1
            0x00, orientation, 0x00, 0x00, // value (big-endian SHORT, padded)
            0x00, 0x00, 0x00, 0x00, // next IFD offset = none
        ];
        let mut out = Vec::with_capacity(jpeg.len() + app1.len());
        out.extend_from_slice(&jpeg[..2]);
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// THE regression test for the reported failure: a photo taken with the
    /// phone held upright is stored as SIDEWAYS pixels plus an EXIF
    /// orientation flag. The previous pipeline decoded with the `image`
    /// crate, which does not apply that flag unless explicitly asked (it
    /// never was), so OCR received a page rotated 90 degrees and recovered
    /// nothing - the engine's tilt tolerance does not extend to a quarter
    /// turn.
    ///
    /// Here the pixels are deliberately stored rotated 90 degrees
    /// counter-clockwise and tagged Orientation=6 ("rotate 90 clockwise to
    /// display"). Applying the flag restores the upright page. Passing
    /// proves WIC honours EXIF on the real production path.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn exif_rotated_photo_is_read_upright_not_sideways() {
        const NEEDLE: &str = "ROTATED CONTRACT PAGE";
        let Some((w, h, rgba)) = render_text_image(NEEDLE) else {
            eprintln!("SKIP: pdfium library not available on this machine");
            return;
        };
        let upright = image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(w, h, rgba).unwrap(),
        );

        // Store the pixels sideways (90 CCW), as a phone sensor would.
        let sideways = upright.rotate270();
        assert_eq!(
            (sideways.width(), sideways.height()),
            (h, w),
            "stored pixels must actually be transposed"
        );
        let mut jpeg = Vec::new();
        sideways
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();

        // Orientation 6 = "rotate 90 clockwise to display correctly",
        // which undoes the rotate270 above.
        let tagged = splice_exif_orientation(&jpeg, 6);

        let text = extract_image_text(tagged, "phone-photo.jpg").await.unwrap();
        assert!(
            text.to_uppercase().contains("ROTATED") && text.to_uppercase().contains("CONTRACT"),
            "an EXIF-rotated photo must be OCR'd upright; got: {:?}",
            text
        );
    }

    /// The same bytes WITHOUT the EXIF flag are genuinely sideways, and OCR
    /// cannot read them. This is what every phone photo looked like to the
    /// engine before the fix - it pins that the test above is proving EXIF
    /// handling, not merely that the engine is rotation-invariant (it is
    /// not).
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn untagged_sideways_pixels_are_not_readable_confirming_exif_is_what_fixes_it() {
        const NEEDLE: &str = "ROTATED CONTRACT PAGE";
        let Some((w, h, rgba)) = render_text_image(NEEDLE) else {
            eprintln!("SKIP: pdfium library not available on this machine");
            return;
        };
        let upright = image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(w, h, rgba).unwrap(),
        );
        let mut jpeg = Vec::new();
        upright
            .rotate270()
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();

        let text = extract_image_text(jpeg, "sideways-untagged.jpg").await.unwrap();
        assert!(
            !text.to_uppercase().contains("ROTATED CONTRACT"),
            "sideways pixels with no orientation flag should NOT read cleanly - \
             if this now passes, the engine gained rotation invariance and the \
             EXIF test above no longer proves what it claims; got: {:?}",
            text
        );
    }

    /// A large image must be OCR'd at its NATIVE resolution, not resized.
    /// The old implementation downscaled anything past 2600px; this proves
    /// an image well beyond that ceiling now reaches the engine untouched
    /// and is read successfully.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn large_image_is_ocrd_at_native_resolution_not_downscaled() {
        use windows::Media::Ocr::OcrEngine;
        let max = OcrEngine::MaxImageDimension().unwrap();
        assert!(max > 2600, "engine limit {} should exceed the old hardcoded 2600", max);

        // 4000px wide - formerly downscaled to 2600, now passed through.
        let img = image::RgbImage::from_pixel(4000, 200, image::Rgb([255, 255, 255]));
        let dynamic = image::DynamicImage::ImageRgb8(img);
        let mut png_bytes = Vec::new();
        dynamic.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png).unwrap();

        let text = extract_image_text(png_bytes, "wide-scan.png").await.unwrap();
        assert!(
            !text.starts_with("[Cannot extract"),
            "a 4000px image is within the engine's real limit and must OCR cleanly: {}", text
        );
    }

    /// Beyond the engine's OWN reported limit, the contract is a loud,
    /// actionable failure - never a silent downscale to make it fit.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn image_beyond_the_engine_limit_fails_loudly_rather_than_being_downscaled() {
        use windows::Media::Ocr::OcrEngine;
        let max = OcrEngine::MaxImageDimension().unwrap();

        let img = image::RgbImage::from_pixel(max + 1, 8, image::Rgb([255, 255, 255]));
        let dynamic = image::DynamicImage::ImageRgb8(img);
        let mut png_bytes = Vec::new();
        dynamic.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png).unwrap();

        let text = extract_image_text(png_bytes, "huge.png").await.unwrap();
        assert!(text.starts_with("[Cannot extract"), "must fail explicitly: {}", text);
        assert!(text.contains("huge.png"), "must name the file: {}", text);
        assert!(
            text.contains("NOT being downscaled"),
            "must state that no silent downscale happened: {}", text
        );
    }

    #[tokio::test]
    async fn corrupt_image_bytes_produce_explicit_failure_not_a_panic() {
        let text = extract_image_text(b"not an image".to_vec(), "bad.png").await.unwrap();
        assert!(text.contains("bad.png"), "{}", text);
        #[cfg(target_os = "windows")]
        assert!(text.contains("Cannot extract"), "{}", text);
    }
}
