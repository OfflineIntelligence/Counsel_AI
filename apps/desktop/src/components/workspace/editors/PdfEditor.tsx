/**
 * PDF review surface: pdfium-rendered pages with an annotation overlay.
 *
 * Pages are `<img>` elements served by the backend — which is why the CSP had
 * to allow `http://127.0.0.1:*` in `img-src`. Rendering happens in native
 * pdfium, and the backend caches each page by (version, page, scale).
 *
 * # Coordinates
 *
 * PDF space has its origin at the BOTTOM-left and measures in points; the DOM
 * has its origin at the top-left and measures in CSS pixels at the current
 * zoom. Every rectangle crossing that boundary is converted in one of the two
 * helpers below, and nowhere else.
 *
 * # Bookmarks
 *
 * pdfium can read a document's outline but not write one, so a bookmark added
 * here is a sticky note whose text starts with "Bookmark:". It is a real PDF
 * annotation — every viewer shows it — and the side column lists it beside the
 * document's own outline.
 */

import { useCallback, useMemo, useRef, useState } from 'react'
import {
  Bookmark,
  Highlighter,
  MessageSquare,
  Minus,
  MousePointer2,
  Plus,
  RotateCw,
  Square,
  Strikethrough,
  Trash2,
  Type,
  Underline,
  Waves,
} from 'lucide-react'

import { pageImageUrl, type DraftPatch, type PdfPageModel } from '../../../api/drafts'
import { BOOKMARK_PREFIX } from '../draftText'
import { Group, PromptDialog, Ribbon, Tool } from './ribbon'
import { useConfirm } from '../../../contexts/confirm'

interface Props {
  draftId: number
  version: number
  pages: PdfPageModel[]
  onPatch: (...patches: DraftPatch[]) => void
  onCommit: (...patches: DraftPatch[]) => Promise<void>
  onError?: (message: string) => void
}

/** Pages are rendered once at this scale and displayed at any zoom. */
const RENDER_SCALE = 1.5

type PdfTool = 'select' | 'highlight' | 'underline' | 'strikeout' | 'squiggly' | 'square' | 'note' | 'freetext' | 'bookmark'

const INKS: { name: string; rgb: [number, number, number] }[] = [
  { name: 'Yellow', rgb: [255, 235, 59] },
  { name: 'Green', rgb: [118, 221, 118] },
  { name: 'Blue', rgb: [110, 180, 255] },
  { name: 'Pink', rgb: [255, 128, 190] },
  { name: 'Orange', rgb: [255, 170, 70] },
  { name: 'Red', rgb: [220, 50, 40] },
]


const HINTS: Record<PdfTool, string> = {
  select: 'Click an annotation to remove it',
  highlight: 'Drag over text to highlight',
  underline: 'Drag over text to underline',
  strikeout: 'Drag over text to strike through',
  squiggly: 'Drag over text for a squiggly underline',
  square: 'Drag to draw a box',
  note: 'Drag a region, then type a comment',
  freetext: 'Drag a box, then type text onto the page',
  bookmark: 'Click where the bookmark should point',
}

function toScreen(rect: [number, number, number, number], pageHeight: number, zoom: number) {
  const [left, bottom, right, top] = rect
  return {
    left: left * zoom,
    top: (pageHeight - top) * zoom,
    width: Math.max((right - left) * zoom, 2),
    height: Math.max((top - bottom) * zoom, 2),
  }
}

function toPdf(box: { left: number; top: number; width: number; height: number }, pageHeight: number, zoom: number): [number, number, number, number] {
  return [box.left / zoom, pageHeight - (box.top + box.height) / zoom, (box.left + box.width) / zoom, pageHeight - box.top / zoom]
}

type Pending = { page: PdfPageModel; kind: 'note' | 'freetext' | 'bookmark'; rect: [number, number, number, number] }

function Page({
  draftId,
  version,
  page,
  tool,
  ink,
  zoom,
  onPatch,
  onRequestText,
}: {
  draftId: number
  version: number
  page: PdfPageModel
  tool: PdfTool
  ink: [number, number, number]
  zoom: number
  onPatch: (...patches: DraftPatch[]) => void
  onRequestText: (p: Pending) => void
}) {
  const confirm = useConfirm()
  const surface = useRef<HTMLDivElement | null>(null)
  const [drag, setDrag] = useState<{ x0: number; y0: number; x1: number; y1: number } | null>(null)
  const [failed, setFailed] = useState(false)

  const box = useMemo(() => {
    if (!drag) return null
    return { left: Math.min(drag.x0, drag.x1), top: Math.min(drag.y0, drag.y1), width: Math.abs(drag.x1 - drag.x0), height: Math.abs(drag.y1 - drag.y0) }
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
    if (tool === 'bookmark') {
      const rect = toPdf({ left: box.left, top: box.top, width: 20, height: 20 }, page.height, zoom)
      onRequestText({ page, kind: 'bookmark', rect })
      setDrag(null)
      return
    }
    if (box.width < 4 || box.height < 4) {
      setDrag(null)
      return
    }
    const rect = toPdf(box, page.height, zoom)
    if (tool === 'note' || tool === 'freetext') {
      onRequestText({ page, kind: tool, rect })
    } else {
      onPatch({ op: 'AddAnnotation', page: page.index, kind: tool, rect, contents: null, color: ink })
    }
    setDrag(null)
  }, [box, tool, page, zoom, ink, onPatch, onRequestText])

  return (
    <div className="ws-pdf-page" data-page={page.index}>
      <div className="ws-pdf-page-label">
        <span>Page {page.index + 1}</span>
        <button type="button" className="ws-tool" title="Rotate this page a quarter turn clockwise" onClick={() => onPatch({ op: 'RotatePage', page: page.index, degrees: 90 })}>
          <RotateCw size={12} />
        </button>
        <button
          type="button"
          className="ws-tool ws-tool-danger"
          title="Delete this page"
          onClick={() => {
            void (async () => {
              const ok = await confirm({
                title: `Delete page ${page.index + 1}?`,
                message: 'Earlier versions keep it.',
                confirmLabel: 'Delete page',
              })
              if (ok) onPatch({ op: 'DeletePage', page: page.index })
            })()
          }}
        >
          <Trash2 size={12} />
        </button>
      </div>
      <div
        ref={surface}
        className={`ws-pdf-surface${tool !== 'select' ? ' ws-pdf-surface-drawing' : ''}`}
        style={{ width: page.width * zoom, height: page.height * zoom }}
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
          <div className="ws-pdf-page-failed">This page could not be rendered. The PDF engine may be busy. Try again in a moment.</div>
        ) : (
          <img
            className="ws-pdf-image"
            src={pageImageUrl(draftId, page.index, version, RENDER_SCALE)}
            alt={`Page ${page.index + 1}`}
            draggable={false}
            loading="lazy"
            onError={() => setFailed(true)}
          />
        )}

        {page.annotations.map(annotation => {
          const position = toScreen(annotation.rect, page.height, zoom)
          const kind = annotation.kind.toLowerCase()
          const isBookmark = annotation.contents?.startsWith(BOOKMARK_PREFIX)
          const [r, g, b] = annotation.color ?? [255, 235, 59]
          const tint =
            kind === 'highlight'
              ? { background: `rgba(${r},${g},${b},0.42)` }
              : kind === 'underline' || kind === 'squiggly'
                ? { borderBottom: `2px ${kind === 'squiggly' ? 'dotted' : 'solid'} rgb(${r},${g},${b})` }
                : kind === 'strikeout'
                  ? { borderTopColor: `rgb(${r},${g},${b})` }
                  : kind === 'square'
                    ? { borderColor: `rgb(${r},${g},${b})` }
                    : undefined
          return (
            <div
              key={annotation.index}
              className={`ws-pdf-annotation ws-pdf-annotation-${kind}${isBookmark ? ' is-bookmark' : ''}`}
              style={{ ...position, ...tint }}
              data-annotation={`${page.index}:${annotation.index}`}
              title={annotation.contents ? `${isBookmark ? 'Bookmark' : annotation.kind}: ${annotation.contents.replace(BOOKMARK_PREFIX, '')}` : `${annotation.kind}, click to remove`}
              onClick={() => {
                if (tool !== 'select') return
                void (async () => {
                  const ok = await confirm({
                    title: 'Remove this annotation?',
                    message: 'Earlier versions keep it.',
                    confirmLabel: 'Remove',
                  })
                  if (ok) onPatch({ op: 'DeleteAnnotation', page: page.index, index: annotation.index })
                })()
              }}
            >
              {isBookmark && <Bookmark size={14} fill="currentColor" />}
              {kind === 'freetext' && annotation.contents && <span className="pdf-freetext">{annotation.contents}</span>}
            </div>
          )
        })}

        {box && tool !== 'select' && tool !== 'bookmark' && <div className="ws-pdf-marquee" style={box} />}
      </div>
    </div>
  )
}

export default function PdfEditor({ draftId, version, pages, onPatch, onCommit, onError }: Props) {
  const [tool, setTool] = useState<PdfTool>('select')
  const [ink, setInk] = useState<[number, number, number]>(INKS[0].rgb)
  const [zoom, setZoom] = useState(RENDER_SCALE)
  const [pending, setPending] = useState<Pending | null>(null)

  const tools: { id: PdfTool; label: string; icon: React.ReactNode }[] = [
    { id: 'select', label: 'Select', icon: <MousePointer2 size={15} /> },
    { id: 'highlight', label: 'Highlight', icon: <Highlighter size={15} /> },
    { id: 'underline', label: 'Underline', icon: <Underline size={15} /> },
    { id: 'strikeout', label: 'Strike', icon: <Strikethrough size={15} /> },
    { id: 'squiggly', label: 'Squiggly', icon: <Waves size={15} /> },
  ]
  const shapes: { id: PdfTool; label: string; icon: React.ReactNode }[] = [
    { id: 'square', label: 'Box', icon: <Square size={15} className="lucide-outline" /> },
    { id: 'note', label: 'Note', icon: <MessageSquare size={15} /> },
    { id: 'freetext', label: 'Text', icon: <Type size={15} /> },
    { id: 'bookmark', label: 'Bookmark', icon: <Bookmark size={15} /> },
  ]

  return (
    <div className="ws-pdf pdfx">
      <Ribbon
        tabs={[{ id: 'review', label: 'Review' }]}
        active="review"
        onTab={() => {}}
        aside={
          <div className="rb-zoom">
            <Tool icon={<Minus size={13} />} title="Zoom out" onClick={() => setZoom(z => Math.max(0.5, +(z - 0.25).toFixed(2)))} />
            <span className="rb-zoom-value">{Math.round((zoom / RENDER_SCALE) * 100)}%</span>
            <Tool icon={<Plus size={13} />} title="Zoom in" onClick={() => setZoom(z => Math.min(4, +(z + 0.25).toFixed(2)))} />
          </div>
        }
      >
        <Group label="Mark up">
          {tools.map(t => (
            <Tool key={t.id} icon={t.icon} label={t.label} title={HINTS[t.id]} active={tool === t.id} onClick={() => setTool(t.id)} />
          ))}
        </Group>
        <Group label="Ink">
          {INKS.map(i => (
            <button
              key={i.name}
              type="button"
              className={`rb-swatch rb-swatch-round${i.rgb === ink ? ' is-active' : ''}`}
              style={{ background: `rgb(${i.rgb.join(',')})` }}
              title={i.name}
              onMouseDown={e => e.preventDefault()}
              onClick={() => setInk(i.rgb)}
            />
          ))}
        </Group>
        <Group label="Add">
          {shapes.map(t => (
            <Tool key={t.id} icon={t.icon} label={t.label} title={HINTS[t.id]} active={tool === t.id} onClick={() => setTool(t.id)} />
          ))}
        </Group>
        <span className="rb-hint">{HINTS[tool]}</span>
      </Ribbon>

      <div className="ws-pdf-scroll">
        {pages.map(page => (
          <Page key={page.index} draftId={draftId} version={version} page={page} tool={tool} ink={ink} zoom={zoom} onPatch={onPatch} onRequestText={setPending} />
        ))}
      </div>

      {pending && (
        <PromptDialog
          title={pending.kind === 'bookmark' ? 'Add bookmark' : pending.kind === 'note' ? 'Add note' : 'Add text'}
          label={pending.kind === 'bookmark' ? 'Bookmark name' : pending.kind === 'note' ? 'Comment' : 'Text on the page'}
          initial={pending.kind === 'bookmark' ? `Page ${pending.page.index + 1}` : ''}
          confirm="Add"
          onClose={() => setPending(null)}
          onSubmit={text => {
            const p = pending
            setPending(null)
            void onCommit({
              op: 'AddAnnotation',
              page: p.page.index,
              kind: p.kind === 'freetext' ? 'freetext' : 'note',
              rect: p.rect,
              contents: p.kind === 'bookmark' ? `${BOOKMARK_PREFIX}${text}` : text,
              color: p.kind === 'bookmark' ? [110, 180, 255] : ink,
            }).catch(e => onError?.(e instanceof Error ? e.message : String(e)))
          }}
        />
      )}
    </div>
  )
}
