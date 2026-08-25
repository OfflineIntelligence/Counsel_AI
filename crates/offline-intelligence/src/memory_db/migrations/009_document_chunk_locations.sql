-- Migration 009: Generalize chunk provenance beyond PDF pages.
--
-- Migration 008 added page_start/page_end (PDF-only). Structure-aware
-- chunking (utils::doc_context::chunk_text) now also anchors chunks in
-- documents with no numeric page concept:
--   - location_label: a free-form physical location for PPTX/spreadsheets,
--     e.g. "Slide 3" or "Sheet: Q1 Data".
--   - paragraph_start/paragraph_end: the 0-based paragraph range a chunk
--     spans, the guaranteed last-resort citation anchor (e.g. "P12-18") for
--     DOCX/TXT/RTF/ODT/HTML and any format with neither a page nor a named
--     location. Always set by the chunker; NULL only for chunks written
--     before this migration.

ALTER TABLE document_chunks ADD COLUMN location_label TEXT;
ALTER TABLE document_chunks ADD COLUMN paragraph_start INTEGER;
ALTER TABLE document_chunks ADD COLUMN paragraph_end INTEGER;
