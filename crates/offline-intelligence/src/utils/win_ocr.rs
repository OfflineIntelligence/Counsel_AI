//! OCR via the built-in Windows OCR engine (Windows.Media.Ocr).
//!
//! Fully offline, ships with Windows 10/11, uses the user's installed
//! language packs, adds zero bytes to the installer. Called from a blocking
//! thread (WinRT async operations are resolved synchronously with .get()).
//!
//! # One decode, by Windows' own codecs
//!
//! `BitmapDecoder` is backed by the Windows Imaging Component - the same
//! codec stack Explorer and Photos use - and natively parses BMP, JPEG, PNG,
//! TIFF, GIF, JPEG-XR, ICO, HEIF and WebP. The user's ORIGINAL file bytes go
//! straight into it. There is deliberately no third-party decode, no
//! re-encode into an intermediate container, and no resampling anywhere in
//! this path.
//!
//! That is not just a tidiness argument. The previous implementation decoded
//! with the `image` crate, downscaled anything over a hardcoded 2600px, and
//! re-encoded to BMP for `BitmapDecoder` to parse a second time. Three
//! separate defects came out of that:
//!
//!   1. **Rotation was silently dropped.** Phone cameras store an upright
//!      photo as sideways pixels plus an EXIF orientation flag. WIC honours
//!      that flag; the `image` crate does not unless explicitly asked (its
//!      own docs cite the smartphone case), and the old code never asked. A
//!      photographed document therefore reached OCR rotated 90 degrees,
//!      which the engine's small tilt tolerance cannot recover - it simply
//!      returned nothing. `RespectExifOrientation` below is the fix.
//!   2. **Detail was destroyed for no reason.** The 2600px ceiling was
//!      documented as "the engine's own limit"; the engine actually reports
//!      10000 (`OcrEngine::MaxImageDimension`, queried at runtime below).
//!      Small text - clause numbers, footnotes, signature blocks - was being
//!      blurred away by a limit that did not exist.
//!   3. **Colour profiles were ignored.** Scanner output often carries a
//!      non-sRGB ICC profile; unmanaged, contrast suffers.
//!      `ColorManageToSRgb` below hands that to WIC too.
//!
//! Nothing here changes WHICH OCR engine runs, or its offline guarantee -
//! only what it is handed. It is now the user's actual file rather than a
//! shrunken, rotation-stripped copy.

use anyhow::{anyhow, Result};
use tracing::{info, warn};
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
    ExifOrientationMode,
};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

/// Recognize text in an image using the user's profile languages. Accepts the
/// ORIGINAL bytes of any WIC-supported format (PNG/JPEG/BMP/TIFF/GIF/...) -
/// format detection is WIC's job, not ours. `label` names the file in error
/// messages only. Returns line-per-row text; any engine/decode failure is a
/// named error, never a silent empty string.
pub fn recognize_image_bytes(bytes: &[u8], label: &str) -> Result<String> {
    // Serialise ALL OCR, process-wide.
    //
    // This one function is the sole entry point to Windows OCR for every
    // format: standalone images (utils::image_ocr), scanned PDF pages
    // (utils::pdf_text) and pictures embedded in DOCX files
    // (utils::file_processor). Those live in three different extraction lanes
    // and therefore run in parallel with each other by design - so the gate has
    // to sit HERE, at the shared resource, rather than in the scheduler, which
    // only knows about lanes.
    //
    // Held for the whole call. Every caller is already on a blocking thread
    // (WinRT interop is synchronous), so waiting here parks a blocking-pool
    // thread rather than an async task, which is the correct place to block.
    let _ocr_permit = crate::utils::extraction_scheduler::ocr_permit();

    let stream = InMemoryRandomAccessStream::new()
        .map_err(|e| anyhow!("OCR stream creation failed: {}", e))?;
    let writer = DataWriter::CreateDataWriter(&stream)
        .map_err(|e| anyhow!("OCR writer creation failed: {}", e))?;
    writer
        .WriteBytes(bytes)
        .map_err(|e| anyhow!("OCR image write failed: {}", e))?;
    writer
        .StoreAsync()
        .map_err(|e| anyhow!("OCR store failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("OCR store await failed: {}", e))?;
    writer
        .FlushAsync()
        .map_err(|e| anyhow!("OCR flush failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("OCR flush await failed: {}", e))?;
    writer
        .DetachStream()
        .map_err(|e| anyhow!("OCR stream detach failed: {}", e))?;
    stream
        .Seek(0)
        .map_err(|e| anyhow!("OCR stream seek failed: {}", e))?;

    // WIC sniffs the container itself - PNG, JPEG, BMP, TIFF, GIF, and more.
    // A failure here means the bytes are not a decodable image at all.
    let decoder = BitmapDecoder::CreateAsync(&stream)
        .map_err(|e| anyhow!("OCR decoder creation failed: {}", e))?
        .get()
        .map_err(|e| {
            anyhow!(
                "'{}' could not be decoded as an image - it may be corrupt, truncated, \
                 or in a format this system has no codec for ({})",
                label, e
            )
        })?;

    // Multi-frame containers (animated GIF, multi-page TIFF - a real scanner
    // output format) decode frame 0 here. Announced rather than silently
    // dropped; whole-document handling would need a per-frame decode loop.
    if let Ok(frames) = decoder.FrameCount() {
        if frames > 1 {
            warn!(
                "'{}' contains {} frames/pages; only the first is OCR'd by this path",
                label, frames
            );
        }
    }

    // ORIENTED dimensions: post-EXIF-rotation, i.e. what the engine will
    // actually receive. Checked against the engine's OWN reported limit,
    // queried at runtime - never a hardcoded guess.
    let max_dimension = OcrEngine::MaxImageDimension()
        .map_err(|e| anyhow!("Windows OCR engine unavailable (could not read its image limit): {}", e))?;
    let width = decoder
        .OrientedPixelWidth()
        .map_err(|e| anyhow!("OCR could not read image width: {}", e))?;
    let height = decoder
        .OrientedPixelHeight()
        .map_err(|e| anyhow!("OCR could not read image height: {}", e))?;
    if width > max_dimension || height > max_dimension {
        // Deliberately a hard, named failure. Downscaling to fit would be a
        // silent quality reduction of exactly the kind that made small text
        // unreadable before - the user is told, and can decide.
        return Err(anyhow!(
            "'{}' is {}x{} pixels, beyond the Windows OCR engine's {}px limit. \
             It is NOT being downscaled to fit, because that would silently \
             destroy the fine detail OCR depends on. Please split or re-scan \
             the image so neither side exceeds {}px.",
            label, width, height, max_dimension, max_dimension
        ));
    }

    // The single decode: straight to the pixel format OcrEngine requires
    // (Bgra8/Premultiplied), with EXIF rotation applied and colour managed
    // to sRGB. BitmapTransform::new() is the identity - present because the
    // API requires it, explicitly performing NO scaling, cropping or flip.
    let transform = BitmapTransform::new()
        .map_err(|e| anyhow!("OCR transform creation failed: {}", e))?;
    let bitmap = decoder
        .GetSoftwareBitmapTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::ColorManageToSRgb,
        )
        .map_err(|e| anyhow!("OCR bitmap conversion failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("OCR bitmap conversion await failed: {}", e))?;

    let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|e| {
        // A bare "engine unavailable" gives the user nothing to act on. The
        // cause is almost always that no recognizer language pack matching
        // their profile languages is installed - so name what IS installed.
        let installed = available_languages_summary();
        anyhow!(
            "Windows OCR engine unavailable for your profile languages ({}). \
             Installed OCR languages: {}. Add a matching language pack via \
             Settings > Time & language > Language & region.",
            e, installed
        )
    })?;

    let result = engine
        .RecognizeAsync(&bitmap)
        .map_err(|e| anyhow!("OCR recognition start failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("OCR recognition failed: {}", e))?;

    let mut out = String::new();
    let lines = result
        .Lines()
        .map_err(|e| anyhow!("OCR result lines unavailable: {}", e))?;
    for line in lines {
        if let Ok(text) = line.Text() {
            out.push_str(&text.to_string_lossy());
            out.push('\n');
        }
    }
    info!(
        "OCR read '{}' ({}x{}, oriented) - {} chars recovered",
        label, width, height, out.len()
    );
    Ok(out)
}

/// Downscale an image to at most `max_pixels` total pixels, re-encoded as
/// PNG, using the same WIC stack as OCR. Returns `None` when the image is
/// already within budget — the caller then sends the ORIGINAL bytes untouched.
///
/// # Why this exists (found in production, 2026-08-08)
///
/// This is for the VISION-MODEL path, not OCR. llama-server's multimodal
/// projector allocates GPU memory proportional to input pixels: a real
/// 2400x1800 screenshot drove a 1.75GB Vulkan allocation, exhausted a 4GB
/// card already holding model layers, and ABORTED the whole llama-server
/// process (GGML_ASSERT) — killing chat along with the extraction. Measured
/// on the same hardware: ≤1.8MP encodes fine; CPU-side encoding
/// (--no-mmproj-offload) was disqualified at 205s per image. Bounding pixels
/// BEFORE the request is the deterministic fix; VLMs are trained at these
/// budgets, so recognition quality is not what full resolution is for.
///
/// EXIF orientation is applied during the decode (same flag as OCR), so a
/// phone photo comes out upright; the PNG produced carries no EXIF, which is
/// then correct rather than lossy.
#[cfg(target_os = "windows")]
pub(crate) fn downscale_to_png_blocking(
    bytes: &[u8],
    max_pixels: u64,
    label: &str,
) -> Result<Option<Vec<u8>>> {
    use windows::Graphics::Imaging::{BitmapEncoder, BitmapInterpolationMode};
    use windows::Storage::Streams::DataReader;

    let stream = InMemoryRandomAccessStream::new()
        .map_err(|e| anyhow!("downscale stream creation failed: {}", e))?;
    let writer = DataWriter::CreateDataWriter(&stream)
        .map_err(|e| anyhow!("downscale writer creation failed: {}", e))?;
    writer.WriteBytes(bytes).map_err(|e| anyhow!("downscale write failed: {}", e))?;
    writer
        .StoreAsync()
        .map_err(|e| anyhow!("downscale store failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("downscale store await failed: {}", e))?;
    writer.DetachStream().map_err(|e| anyhow!("downscale detach failed: {}", e))?;
    stream.Seek(0).map_err(|e| anyhow!("downscale seek failed: {}", e))?;

    let decoder = BitmapDecoder::CreateAsync(&stream)
        .map_err(|e| anyhow!("downscale decoder failed: {}", e))?
        .get()
        .map_err(|e| {
            anyhow!(
                "'{}' could not be decoded as an image for vision processing: {}",
                label, e
            )
        })?;

    let width = decoder
        .OrientedPixelWidth()
        .map_err(|e| anyhow!("downscale width read failed: {}", e))? as u64;
    let height = decoder
        .OrientedPixelHeight()
        .map_err(|e| anyhow!("downscale height read failed: {}", e))? as u64;
    if width == 0 || height == 0 {
        return Err(anyhow!("'{}' reports zero dimensions", label));
    }
    if width * height <= max_pixels {
        return Ok(None);
    }

    let scale = (max_pixels as f64 / (width * height) as f64).sqrt();
    let new_w = ((width as f64 * scale).floor() as u32).max(1);
    let new_h = ((height as f64 * scale).floor() as u32).max(1);

    let transform = BitmapTransform::new()
        .map_err(|e| anyhow!("downscale transform creation failed: {}", e))?;
    transform
        .SetScaledWidth(new_w)
        .map_err(|e| anyhow!("downscale set width failed: {}", e))?;
    transform
        .SetScaledHeight(new_h)
        .map_err(|e| anyhow!("downscale set height failed: {}", e))?;
    // Fant: WIC's high-quality reduction filter — preserves stroke detail far
    // better than nearest/linear when shrinking, which matters for handwriting.
    transform
        .SetInterpolationMode(BitmapInterpolationMode::Fant)
        .map_err(|e| anyhow!("downscale interpolation set failed: {}", e))?;

    let bitmap = decoder
        .GetSoftwareBitmapTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::ColorManageToSRgb,
        )
        .map_err(|e| anyhow!("downscale conversion failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("downscale conversion await failed: {}", e))?;

    let out_stream = InMemoryRandomAccessStream::new()
        .map_err(|e| anyhow!("downscale output stream failed: {}", e))?;
    let encoder = BitmapEncoder::CreateAsync(BitmapEncoder::PngEncoderId().map_err(|e| anyhow!("png encoder id: {}", e))?, &out_stream)
        .map_err(|e| anyhow!("downscale encoder failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("downscale encoder await failed: {}", e))?;
    encoder
        .SetSoftwareBitmap(&bitmap)
        .map_err(|e| anyhow!("downscale set bitmap failed: {}", e))?;
    encoder
        .FlushAsync()
        .map_err(|e| anyhow!("downscale flush failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("downscale flush await failed: {}", e))?;

    let size = out_stream.Size().map_err(|e| anyhow!("downscale size read failed: {}", e))?;
    out_stream.Seek(0).map_err(|e| anyhow!("downscale output seek failed: {}", e))?;
    let input = out_stream
        .GetInputStreamAt(0)
        .map_err(|e| anyhow!("downscale input stream failed: {}", e))?;
    let reader = DataReader::CreateDataReader(&input)
        .map_err(|e| anyhow!("downscale reader failed: {}", e))?;
    reader
        .LoadAsync(size as u32)
        .map_err(|e| anyhow!("downscale load failed: {}", e))?
        .get()
        .map_err(|e| anyhow!("downscale load await failed: {}", e))?;
    let mut out = vec![0u8; size as usize];
    reader
        .ReadBytes(&mut out)
        .map_err(|e| anyhow!("downscale read failed: {}", e))?;

    info!(
        "Vision downscale: '{}' {}x{} ({:.1}MP) -> {}x{} ({:.1}MP) for the vision model",
        label,
        width, height, (width * height) as f64 / 1e6,
        new_w, new_h, (new_w as u64 * new_h as u64) as f64 / 1e6
    );
    Ok(Some(out))
}

/// Comma-separated recognizer languages actually installed on this machine,
/// for error messages. Never fails - diagnostics must not themselves error.
fn available_languages_summary() -> String {
    match OcrEngine::AvailableRecognizerLanguages() {
        Ok(langs) => {
            let tags: Vec<String> = langs
                .into_iter()
                .filter_map(|l| l.LanguageTag().ok().map(|t| t.to_string_lossy()))
                .collect();
            if tags.is_empty() {
                "none installed".to_string()
            } else {
                tags.join(", ")
            }
        }
        Err(_) => "could not be determined".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's real limit, read from the engine itself. Pins the fact
    /// that drove the old hardcoded 2600px constant out of the codebase.
    #[test]
    fn engine_reports_a_usable_image_dimension_limit() {
        let max = OcrEngine::MaxImageDimension().expect("engine must report its limit");
        assert!(
            max >= 4096,
            "engine limit {} is far below the 2600px the old code assumed was the limit",
            max
        );
    }

    #[test]
    fn undecodable_bytes_produce_a_named_error_not_a_panic() {
        let err = recognize_image_bytes(b"definitely not an image", "bad.png")
            .expect_err("garbage bytes must fail");
        let msg = err.to_string();
        assert!(msg.contains("bad.png"), "error must name the file: {}", msg);
    }

    /// The vision-model pixel bound: an image over budget comes back as a
    /// decodable PNG within budget with aspect ratio preserved; an image
    /// within budget is passed through untouched (None). This is what stands
    /// between a large screenshot and the reproduced llama-server GPU-OOM
    /// abort (2026-08-08).
    #[test]
    fn oversized_images_are_downscaled_within_budget_and_small_ones_untouched() {
        use image::GenericImageView;

        let make_png = |w: u32, h: u32| {
            let img = image::RgbImage::from_fn(w, h, |x, y| {
                image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
            });
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            bytes
        };

        const BUDGET: u64 = 1_150_000;

        // 2400x1800 = 4.32MP — the real crashing size from production.
        let big = make_png(2400, 1800);
        let out = downscale_to_png_blocking(&big, BUDGET, "big.png")
            .expect("downscale must succeed")
            .expect("4.3MP must be over a 1.15MP budget");
        let decoded = image::load_from_memory(&out).expect("output must be a decodable PNG");
        let (w, h) = decoded.dimensions();
        assert!(
            (w as u64) * (h as u64) <= BUDGET,
            "downscaled to {}x{} = {} pixels, still over the {} budget",
            w, h, (w as u64) * (h as u64), BUDGET
        );
        let src_aspect = 2400.0 / 1800.0;
        let out_aspect = w as f64 / h as f64;
        assert!(
            (src_aspect - out_aspect).abs() < 0.02,
            "aspect ratio must be preserved: {} vs {}", src_aspect, out_aspect
        );

        // 800x600 = 0.48MP — under budget, must be passed through untouched.
        let small = make_png(800, 600);
        assert!(
            downscale_to_png_blocking(&small, BUDGET, "small.png")
                .expect("downscale must succeed")
                .is_none(),
            "an in-budget image must not be re-encoded"
        );
    }

    #[test]
    fn downscale_rejects_undecodable_bytes_with_a_named_error() {
        let err = downscale_to_png_blocking(b"not an image", 1_000_000, "bad.png")
            .expect_err("garbage must fail");
        assert!(err.to_string().contains("bad.png"), "{}", err);
    }
}
