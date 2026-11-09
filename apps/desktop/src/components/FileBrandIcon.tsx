/**
 * The file icon — ONE drawing for every file the app shows.
 *
 * Workspace, Vault, Library, drafts, the chat composer, sent-message chips,
 * pickers: all of them draw a file with this, so the same document looks the
 * same wherever it appears.
 *
 * Drawn like the company logos in the Models panel: flat vector marks, no
 * shadow, no gradient, a hairline edge that stays one pixel at every size.
 *
 * A white page with a folded corner, a band of the
 * format's colour across its top (Word blue, PDF red, Excel green,
 * PowerPoint orange…), and under it a few quiet lines in that format's
 * layout: paragraphs, columns, a grid, a slide, a photo. No lettering.
 * Small icons (< 24px) draw a taller band and fewer, bolder lines.
 */

import { useId } from 'react';
import './file-brand.css';

/** The only two file-icon sizes in the app, in px. */
// eslint-disable-next-line react-refresh/only-export-components
export const FILE_ICON = {
  /** Grid view tiles: Vault, Library, Workspace. */
  grid: 44,
  /** List rows, pickers, chat attachments and sources. */
  list: 20,
} as const;

/** The type name for a format: always three letters or fewer (DOC, XLS,
 *  PPT, PDF, TXT, CSV, JPG, PNG…). Families with a long modern extension
 *  use the familiar three-letter name. */
// eslint-disable-next-line react-refresh/only-export-components
export function labelFor(format: string): string {
  const f = format.toLowerCase().replace(/^\./, '');
  const family: Record<string, string> = {
    doc: 'DOC', docx: 'DOC', dotx: 'DOC', odt: 'DOC', rtf: 'DOC',
    xls: 'XLS', xlsx: 'XLS', xlsm: 'XLS', ods: 'XLS',
    ppt: 'PPT', pptx: 'PPT', potx: 'PPT', odp: 'PPT',
    jpg: 'JPG', jpeg: 'JPG',
    tif: 'TIF', tiff: 'TIF',
    md: 'MD', markdown: 'MD',
  };
  if (family[f]) return family[f];
  return f.replace(/[^a-z0-9]/g, '').toUpperCase().slice(0, 3);
}

/** The distinct type labels in a set of file names, in first-seen order,
 *  e.g. ["PDF", "DOC"] — what an attachment stack shows at rest. */
// eslint-disable-next-line react-refresh/only-export-components
export function typeLabelsOf(names: string[]): string[] {
  const seen: string[] = [];
  for (const n of names) {
    const dot = n.lastIndexOf('.');
    const label = labelFor(dot > 0 ? n.slice(dot + 1) : '') || 'FILE';
    if (!seen.includes(label)) seen.push(label);
  }
  return seen;
}

/** The type names a capsule shows for a set of files: lower-case words in
 *  first-seen order, at most three (pdf, doc, excel, text, png, jpg…). */
// eslint-disable-next-line react-refresh/only-export-components
export function typeWordsOf(names: string[]): string[] {
  const word: Record<string, string> = { XLS: 'excel', TXT: 'text' };
  const out: string[] = [];
  for (const label of typeLabelsOf(names)) {
    const w = word[label] ?? label.toLowerCase();
    if (!out.includes(w)) out.push(w);
  }
  return out.slice(0, 3);
}

/** Extension of a file name, lower-case, without the dot ('' if none). */
// eslint-disable-next-line react-refresh/only-export-components
export function formatOf(name: string): string {
  const dot = name.lastIndexOf('.');
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : '';
}

/** How a format is drawn: its colour pair, its badge mark, and the kind of
 *  content sketched on the page. */
type Kind = 'word' | 'excel' | 'ppt' | 'pdf' | 'image' | 'text' | 'csv' | 'md' | 'zip' | 'file';

const KIND_OF: Record<string, Kind> = {
  DOC: 'word', XLS: 'excel', PPT: 'ppt', PDF: 'pdf',
  PNG: 'image', JPG: 'image', GIF: 'image', WEB: 'image', TIF: 'image', BMP: 'image', HEI: 'image', SVG: 'image',
  TXT: 'text', LOG: 'text', CSV: 'csv', TSV: 'csv', MD: 'md', ZIP: 'zip', RAR: 'zip', '7Z': 'zip',
};

/** Band colours. Word, Excel, PowerPoint: Microsoft Office brand colours;
 *  PDF: Adobe red. Solid, as the brands use them. */
const LOOK: Record<Kind, { from: string; to: string; mark: string }> = {
  word: { from: '#2B579A', to: '#2B579A', mark: 'W' },
  excel: { from: '#217346', to: '#217346', mark: 'X' },
  ppt: { from: '#D24726', to: '#D24726', mark: 'P' },
  pdf: { from: '#FA0F00', to: '#FA0F00', mark: 'PDF' },
  image: { from: '#3BB3C3', to: '#1A7F8E', mark: '' },
  text: { from: '#8E8E93', to: '#5B5B60', mark: 'TXT' },
  csv: { from: '#217346', to: '#217346', mark: 'CSV' },
  md: { from: '#636366', to: '#3A3A3C', mark: 'MD' },
  zip: { from: '#E0A030', to: '#B7791F', mark: 'ZIP' },
  file: { from: '#8E8E93', to: '#636366', mark: '' },
};

const INK = 'var(--fg-page-ink)';

/*
 * One grid for every icon, in a 32-unit box, symmetric about x = 16:
 *   page     x 7–25, y 3–29, corner folded at x 20 / y 8
 *   band     the top of the page, y 3–11, in the format's colour
 *   content  x 10–22, y 14–26, quiet lines in the format's own layout
 * No lettering: the band's colour and the layout under it say what it is.
 */

const PAGE = 'M8.6 3H20L25 8V27.4A1.6 1.6 0 0 1 23.4 29H8.6A1.6 1.6 0 0 1 7 27.4V4.6A1.6 1.6 0 0 1 8.6 3Z';

/** A line: x, y, width. */
function L({ x, y, w, h = 1.2, o = 0.2, c = INK }: { x: number; y: number; w: number; h?: number; o?: number; c?: string }) {
  return <rect x={x} y={y} width={w} height={h} rx={h / 2} fill={c} opacity={o} />;
}

/** What sits under the band. Small icons: two or three bold strokes. */
function PageContent({ kind, accent, small }: { kind: Kind; accent: string; small: boolean }) {
  const h = small ? 1.8 : 1.2;
  const step = small ? 3.8 : 2.4;
  const lines = (widths: number[], x = 10, y0 = 14.2) =>
    widths.map((w, i) => <L key={i} x={x} y={y0 + i * step} w={w} h={h} o={small ? 0.3 : 0.2} />);

  switch (kind) {
    case 'excel':
    case 'csv': {
      // A grid: three columns, ruled rows.
      const rows = small ? [14, 18, 22] : [14, 16.6, 19.2, 21.8, 24.4];
      return (
        <g>
          {rows.map(y => (
            <g key={y}>
              {[10, 14.2, 18.4].map(x => (
                <rect key={x} x={x} y={y} width="3.6" height={small ? 2.6 : 1.8} rx="0.3" fill={kind === 'excel' && x === 18.4 ? accent : INK} opacity={kind === 'excel' && x === 18.4 ? 0.35 : 0.13} />
              ))}
            </g>
          ))}
        </g>
      );
    }
    case 'ppt':
      // A slide, and its notes under it.
      return (
        <g>
          <rect x="10" y="14" width="12" height={small ? 7.5 : 7} rx="0.8" fill="none" stroke={accent} strokeOpacity="0.55" strokeWidth={small ? 1 : 0.6} />
          <L x={11.6} y={15.6} w={5} h={small ? 1.6 : 1} o={0.7} c={accent} />
          {!small && lines([12, 8], 10, 22.6)}
        </g>
      );
    case 'image':
      // A photo filling the page below the band.
      return (
        <g>
          <circle cx="18.6" cy="16" r={small ? 1.8 : 1.5} fill="#F5B83D" />
          <path d="M10 24.6l4.2-5.4 3 3.2 2-2 2.8 3.1v1.2a1 1 0 0 1-1 1H11a1 1 0 0 1-1-1z" fill={accent} opacity="0.75" />
        </g>
      );
    case 'zip':
      // A zipper running down from the band.
      return (
        <g fill={accent} opacity="0.8">
          {(small ? [12.4, 15.8, 19.2] : [12.2, 14.2, 16.2, 18.2, 20.2]).map((y, i) => (
            <rect key={y} x={i % 2 ? 16 : 14.4} y={y} width="1.6" height={small ? 2.2 : 1.3} rx="0.3" />
          ))}
          <rect x="14" y={small ? 22.6 : 22.4} width="4" height="3.6" rx="0.9" fill="none" stroke={accent} strokeWidth={small ? 1.1 : 0.8} />
        </g>
      );
    case 'pdf':
      // Two columns of text over a full-width line.
      return (
        <g>
          {small ? lines([12, 12, 8]) : (
            <>
              {lines([5.4, 5.4, 5.4, 4], 10)}
              {lines([5.4, 5.4, 5.4, 3], 16.6)}
            </>
          )}
        </g>
      );
    case 'text':
    case 'md':
      return <g>{lines(small ? [12, 9, 11] : [12, 10, 12, 7, 11])}</g>;
    default:
      // Word and anything else: justified paragraphs.
      return <g>{lines(small ? [12, 12, 8] : [12, 12, 12, 12, 7])}</g>;
  }
}

/** The file icon. `size` is the side of its square box, in px. */
export function FileGlyph({
  format,
  size = 32,
}: {
  format: string;
  size?: number;
  /** Ignored: icons carry no lettering. Kept for existing callers. */
  showLabel?: boolean;
}) {
  const uid = useId().replace(/:/g, '');
  const label = labelFor(format);
  const kind: Kind = KIND_OF[label] ?? 'file';
  const look = LOOK[kind];
  const small = size < 24;
  const clip = `fgc${uid}`;

  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 32 32"
      aria-hidden="true"
      className={`file-glyph file-glyph--${kind}${small ? ' file-glyph--small' : ''}`}
      role="presentation"
    >
      <defs>
        <clipPath id={clip}>
          <path d={PAGE} />
        </clipPath>
      </defs>

      {/* The page, centred. */}
      <path className="file-glyph__page" d={PAGE} />
      {/* The coloured band across the top of the page. */}
      <rect x="7" y="3" width="18" height={small ? 9 : 8} fill={look.to} clipPath={`url(#${clip})`} />
      {/* The folded corner: a flat, lighter tint of the band. */}
      <path d="M20 3V6.6A1.4 1.4 0 0 0 21.4 8H25Z" fill="#ffffff" fillOpacity="0.45" />
      <path className="file-glyph__edge" d={PAGE} />

      <PageContent kind={kind} accent={look.to} small={small} />
    </svg>
  );
}

/** The folder glyph: a flat folder in two tones, in the same hand as the
 *  file icon. */
export function FolderGlyph({ size = 32 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 32 32" aria-hidden="true" className="file-glyph" role="presentation">
      <path d="M3.5 8A2 2 0 0 1 5.5 6h6.2l2.4 2.6h12.4a2 2 0 0 1 2 2V25a2 2 0 0 1-2 2H5.5a2 2 0 0 1-2-2z" fill="#3B87DD" />
      <path d="M3.5 13.4a2 2 0 0 1 2-2h21a2 2 0 0 1 2 2V25a2 2 0 0 1-2 2H5.5a2 2 0 0 1-2-2z" fill="#5BA2F0" />
    </svg>
  );
}

/** Kept under its old name so every existing call site — Workspace, Vault,
 *  Library tiles and lists — draws the shared icon. */
export function FileBrandTile({ format, size = 40 }: { format: string; size?: number }) {
  return <FileGlyph format={format} size={size} />;
}
