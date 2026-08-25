//! Byte-span scanning for OOXML parts.
//!
//! # Why spans rather than a parse tree
//!
//! Every format module here edits XML by SPLICING the original string at
//! recorded byte offsets, never by re-serialising a parsed tree. Re-emitting
//! XML — even with a faithful library — normalises attribute quoting,
//! whitespace, namespace prefixes and self-closing forms, so a document that
//! was merely *opened* would come back subtly different. Splicing leaves every
//! byte we did not target exactly as Word/Excel/PowerPoint wrote it.
//!
//! The scanner walks the part once, recording the span of each element and of
//! each text node. Patches then become "replace bytes [a,b) with X", applied
//! back-to-front so earlier offsets stay valid.

use anyhow::{anyhow, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

/// A half-open byte range `[start, end)` into the original part string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn of<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start..self.end]
    }
}

/// One element occurrence found by the scanner.
#[derive(Debug, Clone)]
pub struct ElementSpan {
    /// Local name without namespace prefix, e.g. `p`, `r`, `t`, `c`.
    pub local: String,
    /// Prefixed name as written, e.g. `w:p`.
    pub qname: String,
    /// Span covering the whole element including its start and end tags.
    pub outer: Span,
    /// Span covering only the content between the tags. Equal to an empty span
    /// at the tag's end for self-closing elements.
    pub inner: Span,
    /// Depth in the document, 0 for the root.
    pub depth: usize,
    /// Index of the parent element in the scan's `elements` vector.
    pub parent: Option<usize>,
    /// Raw attribute text of the start tag, for cheap attribute reads.
    pub start_tag: Span,
}

impl ElementSpan {
    /// Value of an attribute on this element's start tag.
    ///
    /// Reads the recorded start-tag text directly rather than re-parsing: the
    /// scanner already isolated it, and OOXML attribute values do not contain
    /// unescaped quotes.
    pub fn attr(&self, source: &str, name: &str) -> Option<String> {
        let tag = self.start_tag.of(source);
        let needle = format!("{}=\"", name);
        let at = tag.find(&needle)? + needle.len();
        let rest = &tag[at..];
        let end = rest.find('"')?;
        Some(crate::document_workspace::ooxml::spans::unescape(&rest[..end]))
    }
}

/// Result of scanning one XML part.
#[derive(Debug, Clone, Default)]
pub struct XmlScan {
    pub elements: Vec<ElementSpan>,
}

impl XmlScan {
    /// Indices of elements with the given local name, in document order.
    pub fn by_local(&self, local: &str) -> Vec<usize> {
        self.elements
            .iter()
            .enumerate()
            .filter(|(_, e)| e.local == local)
            .map(|(i, _)| i)
            .collect()
    }

    /// Indices of descendants of `parent_idx` with the given local name.
    pub fn descendants_by_local(&self, parent_idx: usize, local: &str) -> Vec<usize> {
        let parent = &self.elements[parent_idx];
        self.elements
            .iter()
            .enumerate()
            .filter(|(i, e)| {
                *i != parent_idx
                    && e.local == local
                    && e.outer.start >= parent.outer.start
                    && e.outer.end <= parent.outer.end
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// First descendant of `parent_idx` with the given local name.
    pub fn first_descendant(&self, parent_idx: usize, local: &str) -> Option<usize> {
        self.descendants_by_local(parent_idx, local).into_iter().next()
    }
}

/// Scan an XML part, recording every element's spans.
pub fn scan(source: &str) -> Result<XmlScan> {
    let mut reader = Reader::from_str(source);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;

    let mut elements: Vec<ElementSpan> = Vec::new();
    // (element index, content start offset)
    let mut open: Vec<(usize, usize)> = Vec::new();

    loop {
        let before = reader.buffer_position() as usize;
        let event = reader
            .read_event()
            .map_err(|e| anyhow!("malformed XML at byte {}: {}", before, e))?;
        let after = reader.buffer_position() as usize;

        match event {
            Event::Start(ref e) => {
                let qname = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = qname.rsplit(':').next().unwrap_or(&qname).to_string();
                let idx = elements.len();
                elements.push(ElementSpan {
                    local,
                    qname,
                    outer: Span { start: before, end: after }, // end fixed on close
                    inner: Span { start: after, end: after },
                    depth: open.len(),
                    parent: open.last().map(|(i, _)| *i),
                    start_tag: Span { start: before, end: after },
                });
                open.push((idx, after));
            }
            Event::Empty(ref e) => {
                let qname = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = qname.rsplit(':').next().unwrap_or(&qname).to_string();
                elements.push(ElementSpan {
                    local,
                    qname,
                    outer: Span { start: before, end: after },
                    inner: Span { start: after, end: after },
                    depth: open.len(),
                    parent: open.last().map(|(i, _)| *i),
                    start_tag: Span { start: before, end: after },
                });
            }
            Event::End(_) => {
                if let Some((idx, content_start)) = open.pop() {
                    elements[idx].outer.end = after;
                    elements[idx].inner = Span { start: content_start, end: before };
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(XmlScan { elements })
}

/// One splice: replace `span` with `replacement`.
#[derive(Debug, Clone)]
pub struct Splice {
    pub span: Span,
    pub replacement: String,
}

/// Apply splices to the source.
///
/// Applied back-to-front so each span still refers to the right bytes when it
/// is used. Overlapping splices are rejected rather than silently producing
/// corrupt XML — an overlap means two patches targeted the same region, which
/// is a bug in the caller, not something to paper over.
pub fn apply_splices(source: &str, mut splices: Vec<Splice>) -> Result<String> {
    splices.sort_by(|a, b| b.span.start.cmp(&a.span.start));

    let mut last_start = usize::MAX;
    for s in &splices {
        if s.span.end > last_start {
            return Err(anyhow!(
                "two edits target overlapping regions of the same part ({}..{} and beyond {})",
                s.span.start,
                s.span.end,
                last_start
            ));
        }
        if s.span.end > source.len() || s.span.start > s.span.end {
            return Err(anyhow!(
                "edit span {}..{} is outside this part ({} bytes)",
                s.span.start,
                s.span.end,
                source.len()
            ));
        }
        last_start = s.span.start;
    }

    let mut out = source.to_string();
    for s in splices {
        out.replace_range(s.span.start..s.span.end, &s.replacement);
    }
    Ok(out)
}

/// Escape text for an XML text node.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Reverse of `escape`, for reading text back out of a part.
pub fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<root><a id="1"><b>hello</b><c/></a><a id="2"><b>world</b></a></root>"#;

    #[test]
    fn scanning_records_nesting_and_spans() {
        let scan = scan(SAMPLE).unwrap();
        let a_indices = scan.by_local("a");
        assert_eq!(a_indices.len(), 2);

        let first_a = &scan.elements[a_indices[0]];
        assert_eq!(first_a.attr(SAMPLE, "id").as_deref(), Some("1"));
        assert!(first_a.outer.of(SAMPLE).starts_with("<a id=\"1\">"));
        assert!(first_a.outer.of(SAMPLE).ends_with("</a>"));

        let b = scan.first_descendant(a_indices[0], "b").unwrap();
        assert_eq!(scan.elements[b].inner.of(SAMPLE), "hello");
    }

    #[test]
    fn self_closing_elements_are_recorded() {
        let scan = scan(SAMPLE).unwrap();
        let c = scan.by_local("c");
        assert_eq!(c.len(), 1);
        assert_eq!(scan.elements[c[0]].outer.of(SAMPLE), "<c/>");
    }

    #[test]
    fn splicing_edits_only_the_targeted_bytes() {
        let scan = scan(SAMPLE).unwrap();
        let b = scan.by_local("b");
        let target = scan.elements[b[1]].inner;
        let out = apply_splices(
            SAMPLE,
            vec![Splice { span: target, replacement: "planet".into() }],
        )
        .unwrap();
        assert_eq!(
            out,
            r#"<root><a id="1"><b>hello</b><c/></a><a id="2"><b>planet</b></a></root>"#
        );
    }

    #[test]
    fn multiple_splices_apply_without_shifting_each_other() {
        let scan = scan(SAMPLE).unwrap();
        let b = scan.by_local("b");
        let out = apply_splices(
            SAMPLE,
            vec![
                Splice { span: scan.elements[b[0]].inner, replacement: "AAAAAAAA".into() },
                Splice { span: scan.elements[b[1]].inner, replacement: "Z".into() },
            ],
        )
        .unwrap();
        assert!(out.contains("<b>AAAAAAAA</b>"));
        assert!(out.contains("<b>Z</b>"));
    }

    #[test]
    fn overlapping_splices_are_refused_rather_than_corrupting_the_part() {
        let err = apply_splices(
            SAMPLE,
            vec![
                Splice { span: Span { start: 0, end: 20 }, replacement: "x".into() },
                Splice { span: Span { start: 10, end: 30 }, replacement: "y".into() },
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("overlapping"), "{}", err);
    }

    #[test]
    fn escaping_round_trips_the_characters_that_matter_in_legal_text() {
        let raw = "Clause 3 & 4 <indemnity> \"as-is\"";
        assert_eq!(unescape(&escape(raw)), raw);
    }
}
