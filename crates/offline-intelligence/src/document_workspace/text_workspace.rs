//! Plain text — which is not the trivial case it looks like.
//!
//! A `.txt` file carries three things outside its characters: an encoding, an
//! optional byte-order mark, and a line-ending convention. Reading with
//! `String::from_utf8_lossy` and writing back UTF-8 with `\n` would quietly
//! mangle a Windows-1252 file from a 1990s case management system into
//! mojibake, and turn CRLF into LF so every downstream diff shows the whole
//! file as changed.
//!
//! So all three are detected on open and re-applied on save. "Lossless" has to
//! mean lossless here too, or the guarantee is a slogan.

use anyhow::{anyhow, Result};
use encoding_rs::Encoding;
use tracing::debug;

use super::view_model::{wrong_format, DraftPatch, DraftViewModel};

/// How a file's lines were terminated when we opened it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
        }
    }

    fn sequence(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

/// Everything needed to write the file back the way it arrived.
#[derive(Debug, Clone)]
pub struct TextShape {
    pub encoding: &'static Encoding,
    pub bom: bool,
    pub line_ending: LineEnding,
}

/// Detect encoding, BOM and line endings, and decode to a normalised string.
///
/// The returned text always uses `\n`, so the editor has one convention to deal
/// with; the original convention is recorded in the shape and restored on save.
pub fn decode(bytes: &[u8]) -> (String, TextShape) {
    let (encoding, bom_len) = match Encoding::for_bom(bytes) {
        Some((enc, len)) => (enc, len),
        None => {
            // chardetng is what the extraction pipeline already uses, so a file
            // reads the same way in the workspace as it does in chat.
            let mut detector = chardetng::EncodingDetector::new();
            detector.feed(bytes, true);
            (detector.guess(None, true), 0)
        }
    };

    let (decoded, _, _) = encoding.decode(&bytes[bom_len..]);
    let line_ending = if decoded.contains("\r\n") { LineEnding::Crlf } else { LineEnding::Lf };
    let normalised = decoded.replace("\r\n", "\n");

    debug!(
        "Text draft: {} encoding, BOM {}, {} endings",
        encoding.name(),
        bom_len > 0,
        line_ending.as_str()
    );

    (
        normalised,
        TextShape { encoding, bom: bom_len > 0, line_ending },
    )
}

/// Re-encode text in its original shape.
pub fn encode(text: &str, shape: &TextShape) -> Result<Vec<u8>> {
    // Normalise first, so a buffer that picked up a stray CRLF somewhere does
    // not come back out as `\r\r\n`.
    let normalised = text.replace("\r\n", "\n");
    let restored = match shape.line_ending {
        LineEnding::Lf => normalised,
        LineEnding::Crlf => normalised.replace('\n', shape.line_ending.sequence()),
    };

    let (encoded, _, had_errors) = shape.encoding.encode(&restored);
    if had_errors {
        return Err(anyhow!(
            "This file is {} encoded, which cannot represent some of the characters you typed. \
             Remove them, or save the draft as a new UTF-8 file.",
            shape.encoding.name()
        ));
    }

    let mut out = Vec::with_capacity(encoded.len() + 3);
    if shape.bom {
        // Only UTF-8 BOMs are re-emitted: `Encoding::for_bom` is the only thing
        // that can have set this, and a UTF-16 file would have been decoded to
        // UTF-8-encodable text whose original BOM no longer applies.
        if shape.encoding == encoding_rs::UTF_8 {
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
    }
    out.extend_from_slice(&encoded);
    Ok(out)
}

pub fn build(bytes: &[u8], version: i64) -> Result<DraftViewModel> {
    let (content, shape) = decode(bytes);
    Ok(DraftViewModel::Txt {
        version,
        content,
        encoding: shape.encoding.name().to_string(),
        line_ending: shape.line_ending.as_str().to_string(),
        bom: shape.bom,
    })
}

pub fn apply(bytes: &[u8], patches: &[DraftPatch]) -> Result<Vec<u8>> {
    let (mut content, shape) = decode(bytes);
    for patch in patches {
        match patch {
            DraftPatch::SetText { content: new } => content = new.clone(),
            other => return Err(wrong_format(other, "txt")),
        }
    }
    encode(&content, &shape)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(bytes: &[u8], new_text: Option<&str>) -> Vec<u8> {
        let patches = new_text
            .map(|t| vec![DraftPatch::SetText { content: t.to_string() }])
            .unwrap_or_default();
        let (content, shape) = decode(bytes);
        let text = new_text.map(str::to_string).unwrap_or(content);
        let _ = patches;
        encode(&text, &shape).unwrap()
    }

    #[test]
    fn a_utf8_file_with_unix_endings_is_returned_byte_identical() {
        let original = b"Clause 1\nClause 2\n";
        assert_eq!(round_trip(original, None), original);
    }

    #[test]
    fn crlf_endings_survive_an_edit() {
        let original = b"Clause 1\r\nClause 2\r\n";
        assert_eq!(round_trip(original, None), original);

        // The editor works in \n; saving must restore \r\n.
        let edited = round_trip(original, Some("Clause 1\nClause 2\nClause 3\n"));
        assert_eq!(edited, b"Clause 1\r\nClause 2\r\nClause 3\r\n");
        assert!(!String::from_utf8(edited).unwrap().contains("\r\r"));
    }

    #[test]
    fn a_utf8_bom_is_preserved() {
        let mut original = vec![0xEF, 0xBB, 0xBF];
        original.extend_from_slice(b"Confidential\n");
        let out = round_trip(&original, None);
        assert_eq!(&out[..3], &[0xEF, 0xBB, 0xBF]);
        assert_eq!(out, original);
    }

    #[test]
    fn a_file_without_a_bom_does_not_gain_one() {
        let out = round_trip(b"plain\n", None);
        assert_ne!(&out[..3.min(out.len())], &[0xEF, 0xBB, 0xBF]);
    }

    /// The failure this module exists to prevent: a legacy single-byte file
    /// must not be silently promoted to UTF-8.
    #[test]
    fn a_windows_1252_file_is_not_silently_converted_to_utf8() {
        // 0x93/0x94 are curly quotes in Windows-1252 and invalid UTF-8.
        let mut original = Vec::new();
        original.extend_from_slice("Fee note ".as_bytes());
        original.push(0x93);
        original.extend_from_slice(b"as agreed");
        original.push(0x94);
        original.push(b'\n');

        let (text, shape) = decode(&original);
        assert!(text.contains('\u{201C}'), "curly quote should decode, got {:?}", text);
        assert_ne!(shape.encoding, encoding_rs::UTF_8, "must not be read as UTF-8");

        let out = encode(&text, &shape).unwrap();
        assert_eq!(out, original, "re-encoding must reproduce the original bytes");
    }

    #[test]
    fn a_character_the_original_encoding_cannot_hold_fails_loudly() {
        let mut original = b"Fee note ".to_vec();
        original.push(0x93);
        let (_, shape) = decode(&original);
        if shape.encoding != encoding_rs::UTF_8 {
            let err = encode("emoji \u{1F600} here", &shape).unwrap_err().to_string();
            assert!(err.contains("cannot represent"), "{}", err);
        }
    }

    #[test]
    fn an_empty_file_is_handled_without_panicking() {
        let (text, shape) = decode(b"");
        assert_eq!(text, "");
        assert_eq!(encode(&text, &shape).unwrap(), b"");
    }

    #[test]
    fn the_view_model_reports_the_shape_it_detected() {
        let model = build(b"a\r\nb\r\n", 3).unwrap();
        let DraftViewModel::Txt { version, line_ending, content, .. } = model else { panic!() };
        assert_eq!(version, 3);
        assert_eq!(line_ending, "CRLF");
        assert_eq!(content, "a\nb\n", "the editor always sees \\n");
    }

    #[test]
    fn a_patch_for_another_format_is_refused() {
        let err = apply(b"x", &[DraftPatch::DeleteParagraph { addr: "body/p[0]".into() }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("DeleteParagraph"), "{}", err);
    }
}
