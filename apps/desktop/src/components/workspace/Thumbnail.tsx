/**
 * A document's first page, drawn small.
 *
 * # Why this renders a page rather than picking an icon
 *
 * An icon tells you a file is a Word document. It does not tell you WHICH Word
 * document, which is the only question a file grid exists to answer. So each
 * tile draws the document's real opening lines, its real headings, its real
 * top-left cells.
 *
 * # Why it is a scaled page and not a separate "small" layout
 *
 * The miniature is the page at FULL size — 816px wide, letter proportions, the
 * same margins the editor uses — inside a fixed box with a CSS transform. A
 * second hand-tuned small layout would drift from the real one every time the
 * page changed, and proportions are most of what makes a thumbnail readable as
 * a document. Scaling means the tile is always the page.
 *
 * # Loading
 *
 * Previews are fetched only once a tile is near the viewport. Building one
 * parses the whole document server-side, so a folder of fifty files must not
 * parse fifty documents to draw a screen that shows eight.
 */

import { useEffect, useRef, useState } from 'react'

import {
  getPreview,
  pageImageUrl,
  type DraftPreview,
  type DraftSummary,
} from '../../api/drafts'

/** Letter at 96dpi, matching `--paper-width` in workspace.css. */
const PAGE_WIDTH = 816
const PAGE_HEIGHT = 1056

/** Short marks, because "DOCX" does not fit a 30px glyph and "W" does. */
const FORMAT_GLYPH: Record<string, string> = {
  docx: 'W',
  xlsx: 'X',
  pptx: 'P',
  pdf: 'PDF',
  txt: 'TXT',
}

interface Props {
  draft: DraftSummary
}

export default function Thumbnail({ draft }: Props) {
  const [preview, setPreview] = useState<DraftPreview | null>(null)
  const host = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    const el = host.current
    if (!el) return

    let cancelled = false
    const load = () => {
      void getPreview(draft.id).then(p => {
        if (!cancelled) setPreview(p)
      })
    }

    // A browser without IntersectionObserver (or a test environment) still
    // gets its preview — just eagerly.
    if (typeof IntersectionObserver === 'undefined') {
      load()
      return () => {
        cancelled = true
      }
    }

    const observer = new IntersectionObserver(
      entries => {
        if (entries.some(e => e.isIntersecting)) {
          observer.disconnect()
          load()
        }
      },
      // Start a little before the tile is actually on screen, so scrolling
      // does not reveal a column of empty boxes filling in behind the user.
      { rootMargin: '300px' },
    )
    observer.observe(el)
    return () => {
      cancelled = true
      observer.disconnect()
    }
    // The version matters: a saved edit must invalidate the thumbnail.
  }, [draft.id, draft.current_version])

  return (
    <div className="ws-thumb" ref={host}>
      {preview ? <Rendered draft={draft} preview={preview} /> : <div className="ws-thumb-idle" />}
    </div>
  )
}

function Rendered({ draft, preview }: { draft: DraftSummary; preview: DraftPreview }) {
  switch (preview.kind) {
    case 'text':
      return (
        <Scaled width={PAGE_WIDTH} height={PAGE_HEIGHT}>
          <div className="ws-thumb-page">
            {preview.lines.map((line, i) => (
              <p
                key={i}
                className={`ws-thumb-line${line.heading ? ' ws-thumb-line-heading' : ''}`}
              >
                {line.text}
              </p>
            ))}
          </div>
        </Scaled>
      )

    case 'grid':
      return (
        <Scaled width={PAGE_WIDTH} height={PAGE_HEIGHT}>
          <div className="ws-thumb-sheet">
            <table>
              <tbody>
                {preview.rows.map((row, r) => (
                  <tr key={r}>
                    {row.map((cell, c) => (
                      <td key={c}>{cell}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </Scaled>
      )

    case 'slide':
      // 16:9, because a slide is not a page and showing it as one would
      // misrepresent the document.
      return (
        <Scaled width={1280} height={720}>
          <div className="ws-thumb-slide">
            {preview.lines.map((line, i) => (
              <p
                key={i}
                className={`ws-thumb-line${line.heading ? ' ws-thumb-line-heading' : ''}`}
              >
                {line.text}
              </p>
            ))}
          </div>
        </Scaled>
      )

    case 'raster':
      // The only true render: pdfium already produces a real page image.
      return (
        <img
          className="ws-thumb-raster"
          src={pageImageUrl(draft.id, 0, draft.current_version, 0.4)}
          alt=""
          loading="lazy"
        />
      )

    case 'empty':
    default:
      return (
        <div className="ws-thumb-blank">
          <span className={`ws-draft-glyph ws-draft-glyph-${draft.format}`}>
            {FORMAT_GLYPH[draft.format] ?? draft.format.slice(0, 3).toUpperCase()}
          </span>
        </div>
      )
  }
}

/**
 * Draw `children` at their natural size, scaled to fit the tile.
 *
 * The scale is measured rather than hardcoded so the tile size can change in
 * CSS alone. `transform` (not zoom, not a font-size cascade) because it scales
 * the whole box model uniformly — margins, table borders and line height all
 * shrink together, which is exactly what keeps the miniature a faithful
 * reduction rather than a squashed re-layout.
 */
function Scaled({
  width,
  height,
  children,
}: {
  width: number
  height: number
  children: React.ReactNode
}) {
  const box = useRef<HTMLDivElement | null>(null)
  const [scale, setScale] = useState(0)

  useEffect(() => {
    const el = box.current
    if (!el) return
    const measure = () => {
      const w = el.clientWidth
      if (w > 0) setScale(w / width)
    }
    measure()
    if (typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(measure)
    ro.observe(el)
    return () => ro.disconnect()
  }, [width])

  return (
    <div className="ws-thumb-scaler" ref={box}>
      {scale > 0 && (
        // The frame takes the REDUCED height, so a shape shorter than the tile
        // — a 16:9 slide in a portrait tile — is centred rather than hanging
        // from the top edge with dead space beneath it. A transformed element
        // keeps its pre-transform layout box, so without this the frame would
        // always be the full unscaled height.
        <div className="ws-thumb-frame" style={{ height: Math.round(height * scale) }}>
          <div
            className="ws-thumb-stage"
            style={{
              width,
              height,
              transform: `scale(${scale})`,
              transformOrigin: 'top left',
            }}
          >
            {children}
          </div>
        </div>
      )}
    </div>
  )
}
