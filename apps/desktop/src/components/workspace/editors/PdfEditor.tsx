/**
 * PDF review surface: pdfium-rendered pages with an annotation overlay.
 *
 * Pages are `<img>` elements served by the backend — which is why the CSP had
 * to allow `http://127.0.0.1:*` in `img-src`. Rendering happens in native
 * pdfium rather than in WASM in the WebView, and the backend caches each page
 * by (version, page, scale) so scrolling never re-renders.
 *
 * # Coordinates
 *
 * PDF space has its origin at the BOTTOM-left and measures in points; the DOM
 * has its origin at the top-left and measures in CSS pixels. Every rectangle
 * crossing that boundary is converted in one of the two helpers below, and
 * nowhere else — getting this wrong puts highlights on the wrong half of the
 * page, which is exactly the kind of bug that looks like a rendering fault.
 */

import { useCallback, useMemo, useRef, useState } from 'react'

import { pageImageUrl, type DraftPatch, type PdfPageModel } from '../../../api/drafts'

interface Props {
  draftId: number
  version: number
  pages: PdfPageModel[]
  onPatch: (...patches: DraftPatch[]) => void
}

const SCALE = 1.5

type Tool = 'select' | 'highlight' | 'strikeout' | 'underline' | 'square' | 'note'

const TOOLS: { id: Tool; label: string; hint: string }[] = [
  { id: 'select', label: 'Select', hint: 'Click an annotation to remove it' },
  { id: 'highlight', label: 'Highlight', hint: 'Drag over text to highlight' },
  { id: 'strikeout', label: 'Strike', hint: 'Drag over text to strike through' },
  { id: 'underline', label: 'Underline', hint: 'Drag over text to underline' },
  { id: 'square', label: 'Box', hint: 'Drag to draw a box' },
  { id: 'note', label: 'Note', hint: 'Drag a region, then type a comment' },
]

/** PDF points (origin bottom-left) → CSS pixels within the rendered page. */
function toScreen(rect: [number, number, number, number], pageHeight: number) {
  const [left, bottom, right, top] = rect
  return {
    left: left * SCALE,
    top: (pageHeight - top) * SCALE,
    width: Math.max((right - left) * SCALE, 2),
    height: Math.max((top - bottom) * SCALE, 2),
  }
}

/** CSS pixels within the rendered page → PDF points (origin bottom-left). */
function toPdf(
  box: { left: number; top: number; width: number; height: number },
  pageHeight: number,
): [number, number, number, number] {
  const left = box.left / SCALE
  const right = (box.left + box.width) / SCALE
  const top = pageHeight - box.top / SCALE
  const bottom = pageHeight - (box.top + box.height) / SCALE
  return [left, bottom, right, top]
}

function Page({
  draftId,
  version,
  page,
  tool,
  onPatch,
}: {
  draftId: number
  version: number
  page: PdfPageModel
  tool: Tool
  onPatch: (...patches: DraftPatch[]) => void
}) {
  const surface = useRef<HTMLDivElement | null>(null)
  const [drag, setDrag] = useState<{ x0: number; y0: number; x1: number; y1: number } | null>(null)
  const [failed, setFailed] = useState(false)

  const box = useMemo(() => {
    if (!drag) return null
    return {
      left: Math.min(drag.x0, drag.x1),
      top: Math.min(drag.y0, drag.y1),
      width: Math.abs(drag.x1 - drag.x0),
      height: Math.abs(drag.y1 - drag.y0),
    }
  }, [drag])

  const relative = useCallback((e: React.MouseEvent) => {
    const rect = surface.current?.getBoundingClientRect()
    if (!rect) return { x: 0, y: 0 }
    return { x: e.clientX - rect.left, y: e.clientY - rect.top }
  }, [])

  const finish = useCallback(() => {
    if (!box || tool === 'select') {
      setDrag(null)
      return
    }
    // A stray click is not an annotation.
    if (box.width < 4 || box.height < 4) {
      setDrag(null)
      return
    }

    let contents: string | null = null
    if (tool === 'note') {
      contents = window.prompt('Note text')
      if (contents === null) {
        setDrag(null)
        return
      }
    }

    onPatch({
      op: 'AddAnnotation',
      page: page.index,
      kind: tool,
      rect: toPdf(box, page.height),
      contents,
    })
    setDrag(null)
  }, [box, tool, page.index, page.height, onPatch])

  return (
    <div className="ws-pdf-page">
      <div className="ws-pdf-page-label">
        <span>Page {page.index + 1}</span>
        <button
          type="button"
          className="ws-tool"
          title="Rotate this page a quarter turn clockwise"
          onClick={() => onPatch({ op: 'RotatePage', page: page.index, degrees: 90 })}
        >
          ↻
        </button>
        <button
          type="button"
          className="ws-tool ws-tool-danger"
          title="Delete this page"
          onClick={() => {
            if (window.confirm(`Delete page ${page.index + 1}? Earlier versions keep it.`)) {
              onPatch({ op: 'DeletePage', page: page.index })
            }
          }}
        >
          Delete page
        </button>
      </div>
      <div
        ref={surface}
        className={`ws-pdf-surface${tool !== 'select' ? ' ws-pdf-surface-drawing' : ''}`}
        style={{ width: page.width * SCALE, height: page.height * SCALE }}
        onMouseDown={e => {
          if (tool === 'select') return
          const { x, y } = relative(e)
          setDrag({ x0: x, y0: y, x1: x, y1: y })
        }}
        onMouseMove={e => {
          if (!drag) return
          const { x, y } = relative(e)
          setDrag(d => (d ? { ...d, x1: x, y1: y } : null))
        }}
        onMouseUp={finish}
        onMouseLeave={() => drag && finish()}
      >
        {failed ? (
          <div className="ws-pdf-page-failed">
            This page could not be rendered. The PDF engine may be busy reading another
            document — try again in a moment.
          </div>
        ) : (
          <img
            className="ws-pdf-image"
            src={pageImageUrl(draftId, page.index, version, SCALE)}
            alt={`Page ${page.index + 1}`}
            draggable={false}
            // Lazy so a 200-page brief does not ask pdfium for 200 renders at
            // once — pdfium is serialised process-wide, so that would queue
            // every page behind the first.
            loading="lazy"
            onError={() => setFailed(true)}
          />
        )}

        {page.annotations.map(annotation => {
          const position = toScreen(annotation.rect, page.height)
          return (
            <div
              key={annotation.index}
              className={`ws-pdf-annotation ws-pdf-annotation-${annotation.kind.toLowerCase()}`}
              style={position}
              title={
                annotation.contents
                  ? `${annotation.kind}: ${annotation.contents}`
                  : `${annotation.kind} — click to remove`
              }
              onClick={() => {
                if (tool !== 'select') return
                if (window.confirm('Remove this annotation?')) {
                  onPatch({ op: 'DeleteAnnotation', page: page.index, index: annotation.index })
                }
              }}
            />
          )
        })}

        {box && tool !== 'select' && <div className="ws-pdf-marquee" style={box} />}
      </div>
    </div>
  )
}

export default function PdfEditor({ draftId, version, pages, onPatch }: Props) {
  const [tool, setTool] = useState<Tool>('select')
  const active = TOOLS.find(t => t.id === tool)

  return (
    <div className="ws-pdf">
      <div className="ws-toolbar">
        {TOOLS.map(t => (
          <button
            key={t.id}
            type="button"
            className={`ws-tool${tool === t.id ? ' ws-tool-active' : ''}`}
            onClick={() => setTool(t.id)}
            title={t.hint}
          >
            {t.label}
          </button>
        ))}
        <span className="ws-tool-divider" />
        <span className="ws-hint">{active?.hint}</span>
        <span className="ws-tool-divider" />
        <span className="ws-hint" title="A PDF has no paragraph model, so text cannot reflow">
          text is not editable in PDFs
        </span>
      </div>

      <div className="ws-pdf-scroll">
        {pages.map(page => (
          <Page
            key={page.index}
            draftId={draftId}
            version={version}
            page={page}
            tool={tool}
            onPatch={onPatch}
          />
        ))}
      </div>
    </div>
  )
}
