-- Migration 008: Provenance anchors on document_chunks.
--
-- Chunking became structure-aware (utils::doc_context::chunk_text): each
-- chunk now knows the PDF page(s) it was drawn from (when the source
-- extraction tagged pages) and the nearest legal heading/clause in effect
-- (e.g. "Section 4.2(b)"). This is what lets retrieved excerpts be cited
-- back to the user as "p.12, Section 4.2(b)" instead of an anonymous blob.
-- All nullable: unstructured text (plain .txt, DOCX without headings) or
-- documents chunked before this migration simply carry no anchor.

ALTER TABLE document_chunks ADD COLUMN page_start INTEGER;
ALTER TABLE document_chunks ADD COLUMN page_end INTEGER;
ALTER TABLE document_chunks ADD COLUMN section_label TEXT;
