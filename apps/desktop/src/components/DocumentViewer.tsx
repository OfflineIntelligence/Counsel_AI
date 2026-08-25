import React, { useEffect, useRef, useState } from 'react';
import * as pdfjsLib from 'pdfjs-dist';
// Vite bundles the worker as a served asset; pdfjs loads it in a Worker
// thread so rendering never blocks the UI. Fully offline - no CDN fetch.
import pdfWorkerUrl from 'pdfjs-dist/build/pdf.worker.min.mjs?url';
import mammoth from 'mammoth';
import DOMPurify from 'dompurify';
import { getApiBaseSync } from '../api/backendUrl';
import './DocumentViewer.css';

pdfjsLib.GlobalWorkerOptions.workerSrc = pdfWorkerUrl;

interface DocumentViewerProps {
  documentId: number;
  filename: string;
  onClose: () => void;
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
export const DocumentViewer: React.FC<DocumentViewerProps> = ({ documentId, filename, onClose }) => {
  const kind = kindForFilename(filename);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [docxHtml, setDocxHtml] = useState('');
  const [textContent, setTextContent] = useState('');
  const [imageUrl, setImageUrl] = useState('');
  const canvasContainerRef = useRef<HTMLDivElement>(null);

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
          for (let pageNum = 1; pageNum <= pdf.numPages; pageNum++) {
            const page = await pdf.getPage(pageNum);
            const viewport = page.getViewport({ scale: 1.3 });
            const canvas = document.createElement('canvas');
            canvas.width = viewport.width;
            canvas.height = viewport.height;
            canvas.className = 'pdf-page-canvas';
            container.appendChild(canvas);
            const ctx = canvas.getContext('2d');
            if (ctx) {
              await page.render({ canvas, canvasContext: ctx, viewport }).promise;
            }
            if (cancelled) return;
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
  }, [documentId, filename, kind]);

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal document-viewer-modal" onClick={(e) => e.stopPropagation()}>
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
            <div className="docx-preview" dangerouslySetInnerHTML={{ __html: docxHtml }} />
          )}
          {!error && kind === 'image' && imageUrl && (
            <img src={imageUrl} alt={filename} className="image-preview" />
          )}
          {!error && kind === 'text' && <pre className="text-preview">{textContent}</pre>}
        </div>
      </div>
    </div>
  );
};
