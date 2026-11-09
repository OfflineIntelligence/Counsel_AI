import React, { useEffect, useRef, useState } from 'react';
import * as pdfjsLib from 'pdfjs-dist';
// Vite bundles the worker as a served asset; pdfjs loads it in a Worker
// thread so rendering never blocks the UI. Fully offline - no CDN fetch.
import pdfWorkerUrl from 'pdfjs-dist/build/pdf.worker.min.mjs?url';
import type { TextItem } from 'pdfjs-dist/types/src/display/api';
import mammoth from 'mammoth';
import DOMPurify from 'dompurify';
import { getApiBaseSync } from '../api/backendUrl';
import './DocumentViewer.css';

pdfjsLib.GlobalWorkerOptions.workerSrc = pdfWorkerUrl;

interface DocumentViewerProps {
  documentId: number;
  filename: string;
  onClose: () => void;
  /** A passage to find, highlight and scroll to (a cited source). */
  highlight?: string;
  /** The page the passage is expected on, searched first in a PDF. */
  page?: number | null;
  /** Render in place (e.g. under a list row) instead of as a modal. */
  inline?: boolean;
}

/** Lower-case and collapse whitespace, so text matches across reflows. */
function norm(s: string): string {
  return s.replace(/\s+/g, ' ').trim().toLowerCase();
}

/** The part of a passage to search for: long enough to be unique, short
 *  enough to survive extraction differences at its tail. */
function needleOf(passage: string): string {
  return norm(passage).slice(0, 90);
}

/** Wrap the first occurrence of `passage` inside `root` in <mark> elements
 *  (one per text node it spans) and return the first mark, or null. */
function markInDom(root: HTMLElement, passage: string): HTMLElement | null {
  const needle = needleOf(passage);
  if (needle.length < 12) return null;
  const nodes: Text[] = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) nodes.push(n as Text);
  // Normalised text with a map back to (node, offset) for every char.
  let flat = '';
  const map: { node: Text; offset: number }[] = [];
  let lastSpace = true;
  for (const node of nodes) {
    const t = node.data;
    for (let i = 0; i < t.length; i++) {
      const ch = t[i];
      if (/\s/.test(ch)) {
        if (lastSpace) continue;
        flat += ' ';
        lastSpace = true;
      } else {
        flat += ch.toLowerCase();
        lastSpace = false;
      }
      map.push({ node, offset: i });
    }
  }
  const at = flat.indexOf(needle);
  if (at < 0) return null;
  const end = Math.min(flat.length, at + norm(passage).length) - 1;
  // Group the span by text node, then wrap each piece.
  const pieces = new Map<Text, [number, number]>();
  for (let i = at; i <= end; i++) {
    const { node, offset } = map[i];
    const r = pieces.get(node);
    pieces.set(node, r ? [r[0], offset] : [offset, offset]);
  }
  let first: HTMLElement | null = null;
  for (const [node, [a, b]] of pieces) {
    const range = document.createRange();
    range.setStart(node, a);
    range.setEnd(node, b + 1);
    const mark = document.createElement('mark');
    mark.className = 'doc-hl';
    range.surroundContents(mark);
    first ??= mark;
  }
  return first;
}

type ViewerKind = 'pdf' | 'docx' | 'image' | 'text';

const IMAGE_EXTS = ['png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'svg'];

function kindForFilename(name: string): ViewerKind {
  const ext = name.split('.').pop()?.toLowerCase() || '';
  if (ext === 'pdf') return 'pdf';
  if (ext === 'docx') return 'docx';
  if (IMAGE_EXTS.includes(ext)) return 'image';
  // Everything else (txt/code, legacy .doc, xlsx/ods, pptx/odp, rtf/odt/html)
  // falls back to the extracted-text view - true spreadsheet-grid and
  // slide-visual rendering are a follow-up, not yet built.
  return 'text';
}

/// Renders a document's actual content: PDF pages via pdf.js canvases, DOCX
/// via mammoth-to-HTML (sanitized), images natively, everything else as its
/// extracted text. Bytes come from GET /documents/:id/raw; text metadata
/// from GET /documents/:id — both backed by the unified document store, so
/// this works identically for paperclip attachments and Local Storage files.
export const DocumentViewer: React.FC<DocumentViewerProps> = ({ documentId, filename, onClose, highlight, page, inline }) => {
  const kind = kindForFilename(filename);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [docxHtml, setDocxHtml] = useState('');
  const [textContent, setTextContent] = useState('');
  const [imageUrl, setImageUrl] = useState('');
  const canvasContainerRef = useRef<HTMLDivElement>(null);
  const docxRef = useRef<HTMLDivElement>(null);
  const textRef = useRef<HTMLPreElement>(null);

  useEffect(() => {
    let cancelled = false;
    let objectUrl: string | null = null;
    setLoading(true);
    setError(null);

    const apiBase = getApiBaseSync();
    const rawUrl = `${apiBase}/documents/${documentId}/raw`;

    async function fetchRawBytes(): Promise<ArrayBuffer> {
      const resp = await fetch(rawUrl);
      if (!resp.ok) {
        throw new Error(await resp.text());
      }
      return resp.arrayBuffer();
    }

    async function load() {
      try {
        if (kind === 'pdf') {
          const buf = await fetchRawBytes();
          const pdf = await pdfjsLib.getDocument({ data: buf }).promise;
          if (cancelled) return;
          const container = canvasContainerRef.current;
          if (!container) return;
          container.innerHTML = '';
          const pages: { wrap: HTMLDivElement; pdfPage: pdfjsLib.PDFPageProxy; viewport: pdfjsLib.PageViewport }[] = [];
          for (let pageNum = 1; pageNum <= pdf.numPages; pageNum++) {
            const pdfPage = await pdf.getPage(pageNum);
            const viewport = pdfPage.getViewport({ scale: 1.3 });
            const wrap = document.createElement('div');
            wrap.className = 'pdf-page';
            const canvas = document.createElement('canvas');
            canvas.width = viewport.width;
            canvas.height = viewport.height;
            canvas.className = 'pdf-page-canvas';
            wrap.appendChild(canvas);
            container.appendChild(wrap);
            const ctx = canvas.getContext('2d');
            if (ctx) {
              await pdfPage.render({ canvas, canvasContext: ctx, viewport }).promise;
            }
            pages.push({ wrap, pdfPage, viewport });
            if (cancelled) return;
          }
          // Highlight the cited passage: the expected page first, then the
          // rest, stopping at the first page that contains it.
          if (highlight) {
            const needle = needleOf(highlight);
            const order = pages.map((_, i) => i);
            if (page && page >= 1 && page <= pages.length) {
              order.splice(order.indexOf(page - 1), 1);
              order.unshift(page - 1);
            }
            for (const i of order) {
              const { wrap, pdfPage, viewport } = pages[i];
              const content = await pdfPage.getTextContent();
              if (cancelled) return;
              const items = content.items.filter((it): it is TextItem => 'str' in it);
              let flat = '';
              const spans: { from: number; to: number; item: TextItem }[] = [];
              for (const item of items) {
                const t = norm(item.str);
                if (!t) continue;
                if (flat) flat += ' ';
                spans.push({ from: flat.length, to: flat.length + t.length, item });
                flat += t;
              }
              const at = needle.length >= 12 ? flat.indexOf(needle) : -1;
              if (at < 0) continue;
              const end = at + norm(highlight).length;
              let firstBox: HTMLDivElement | null = null;
              for (const sp of spans) {
                if (sp.to <= at || sp.from >= end) continue;
                const tx = pdfjsLib.Util.transform(viewport.transform, sp.item.transform);
                const h = Math.hypot(tx[2], tx[3]);
                const box = document.createElement('div');
                box.className = 'pdf-hl';
                box.style.left = `${tx[4]}px`;
                box.style.top = `${tx[5] - h}px`;
                box.style.width = `${sp.item.width * viewport.scale}px`;
                box.style.height = `${h * 1.15}px`;
                wrap.appendChild(box);
                firstBox ??= box;
              }
              firstBox?.scrollIntoView({ block: 'center' });
              break;
            }
          }
        } else if (kind === 'docx') {
          const buf = await fetchRawBytes();
          const result = await mammoth.convertToHtml({ arrayBuffer: buf });
          if (cancelled) return;
          setDocxHtml(DOMPurify.sanitize(result.value));
        } else if (kind === 'image') {
          const resp = await fetch(rawUrl);
          if (!resp.ok) throw new Error(await resp.text());
          const blob = await resp.blob();
          objectUrl = URL.createObjectURL(blob);
          if (cancelled) return;
          setImageUrl(objectUrl);
        } else {
          const metaResp = await fetch(`${apiBase}/documents/${documentId}`);
          if (!metaResp.ok) throw new Error(await metaResp.text());
          const meta = await metaResp.json();
          if (cancelled) return;
          setTextContent(meta.extracted_text || '(no extracted text available)');
        }
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    load();
    return () => {
      cancelled = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [documentId, filename, kind, highlight, page]);

  // Word and text views: once the content is in the DOM, mark the cited
  // passage and bring it into view.
  useEffect(() => {
    if (!highlight || loading) return;
    const root = kind === 'docx' ? docxRef.current : kind === 'text' ? textRef.current : null;
    if (!root) return;
    const mark = markInDom(root, highlight);
    mark?.scrollIntoView({ block: 'center' });
  }, [highlight, loading, kind, docxHtml, textContent]);

  // Escape closes, as the backdrop click does: the keyboard way out.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [onClose]);

  const content = (
    <>
        <div className="document-viewer-header">
          <h2 className="modal-title" title={filename}>{filename}</h2>
          <button type="button" className="document-viewer-close" onClick={onClose} aria-label="Close preview">
            <svg width="18" height="18" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
            </svg>
          </button>
        </div>
        <div className="document-viewer-body">
          {loading && <div className="document-viewer-status">Loading preview…</div>}
          {error && (
            <div className="document-viewer-status document-viewer-error">
              Could not preview this file: {error}
            </div>
          )}
          {!error && kind === 'pdf' && <div ref={canvasContainerRef} className="pdf-canvas-container" />}
          {!error && kind === 'docx' && (
            <div ref={docxRef} className="docx-preview" dangerouslySetInnerHTML={{ __html: docxHtml }} />
          )}
          {!error && kind === 'image' && imageUrl && (
            <img src={imageUrl} alt={filename} className="image-preview" />
          )}
          {!error && kind === 'text' && <pre ref={textRef} className="text-preview">{textContent}</pre>}
        </div>
    </>
  );

  if (inline) {
    return (
      <div className="document-viewer-inline" role="region" aria-label={`Preview of ${filename}`}>
        {content}
      </div>
    );
  }

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div
        className="modal document-viewer-modal"
        role="dialog"
        aria-modal="true"
        aria-label={filename}
        onClick={(e) => e.stopPropagation()}
      >
        {content}
      </div>
    </div>
  );
};
