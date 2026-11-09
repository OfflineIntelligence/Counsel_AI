/**
 * Slide editor: a rail of live miniatures, a scaled canvas, and a ribbon.
 *
 * # Units
 *
 * DrawingML measures in EMU (914,400 per inch; 12,700 per point). The canvas
 * is laid out in percentages of the slide box and text is sized from the
 * canvas's measured width, so a 44pt title is 44pt of a real slide at any
 * window size.
 *
 * # Selecting versus typing
 *
 * One click selects a shape — it can then be dragged, resized, filled or
 * deleted, and formatting applies to all of its text. Clicking the selected
 * shape again (or double-clicking) enters its text, where formatting applies
 * to the paragraph under the caret. That is PowerPoint's own model, and it is
 * the only way a drag and a text selection can share one surface.
 */

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import {
  AlignCenter,
  AlignJustify,
  AlignLeft,
  AlignRight,
  ArrowDown,
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  Baseline,
  Bold,
  Copy,
  Highlighter,
  ImagePlus,
  Italic,
  List,
  ListOrdered,
  Maximize2,
  Minimize2,
  Omega,
  PaintBucket,
  Plus,
  Shapes,
  Strikethrough,
  Table2,
  Trash2,
  Type,
  Underline,
} from 'lucide-react'

import type { DraftPatch, PptxParagraph, PptxShape, PptxSlide, TextFormat } from '../../../api/drafts'
import {
  ColorPicker,
  FontPicker,
  Group,
  Popover,
  Ribbon,
  SizePicker,
  SymbolPicker,
  TableGridPicker,
  Tool,
} from './ribbon'
import { useConfirm } from '../../../contexts/confirm'
import { pickImage, readImage } from './ribbonData'

interface Props {
  slides: PptxSlide[]
  onPatch: (...patches: DraftPatch[]) => void
  onCommit: (...patches: DraftPatch[]) => Promise<void>
  /** Reported so the side column can inspect the current slide and shape. */
  onSelectionChange?: (slide: number, shape: string | null) => void
  onError?: (message: string) => void
}

const EMU_PER_PT = 12_700
const STEP = 91_440 // a tenth of an inch

const GEOMETRIES: { id: string; label: string; css: CSSProperties }[] = [
  { id: 'rect', label: 'Rectangle', css: {} },
  { id: 'roundRect', label: 'Rounded rectangle', css: { borderRadius: '14%' } },
  { id: 'ellipse', label: 'Oval', css: { borderRadius: '50%' } },
  { id: 'triangle', label: 'Triangle', css: { clipPath: 'polygon(50% 0, 100% 100%, 0 100%)' } },
  { id: 'rightArrow', label: 'Arrow', css: { clipPath: 'polygon(0 30%, 62% 30%, 62% 0, 100% 50%, 62% 100%, 62% 70%, 0 70%)' } },
  { id: 'chevron', label: 'Chevron', css: { clipPath: 'polygon(0 0, 75% 0, 100% 50%, 75% 100%, 0 100%, 25% 50%)' } },
  { id: 'hexagon', label: 'Hexagon', css: { clipPath: 'polygon(25% 0, 75% 0, 100% 50%, 75% 100%, 25% 100%, 0 50%)' } },
  { id: 'star5', label: 'Star', css: { clipPath: 'polygon(50% 0, 61% 35%, 98% 35%, 68% 57%, 79% 91%, 50% 70%, 21% 91%, 32% 57%, 2% 35%, 39% 35%)' } },
  { id: 'line', label: 'Line', css: {} },
]
const GEOMETRY_CSS = Object.fromEntries(GEOMETRIES.map(g => [g.id, g.css]))

function defaultSize(shape: PptxShape): number {
  if (shape.placeholder === 'title' || shape.placeholder === 'ctrTitle') return 40
  if (shape.placeholder === 'subTitle') return 24
  if (shape.placeholder) return 22
  if (shape.table) return 14
  return 18
}

function paragraphCss(p: PptxParagraph, shape: PptxShape, ptToPx: number): CSSProperties {
  return {
    fontWeight: p.bold || shape.placeholder === 'title' || shape.placeholder === 'ctrTitle' ? 700 : undefined,
    fontStyle: p.italic ? 'italic' : undefined,
    textDecoration: [p.underline ? 'underline' : '', p.strike ? 'line-through' : ''].filter(Boolean).join(' ') || undefined,
    fontSize: `${(p.size ?? defaultSize(shape)) * ptToPx}px`,
    fontFamily: p.font ? `"${p.font}", var(--font-sans)` : undefined,
    color: p.color ? `#${p.color}` : undefined,
    textAlign: p.align === 'justify' ? 'justify' : p.align ?? undefined,
    paddingLeft: p.list ? `${(p.level + 1) * 1.1}em` : p.level ? `${p.level * 1.1}em` : undefined,
  }
}

// ---------------------------------------------------------------- paragraph

function ShapeText({
  paragraph,
  shape,
  ptToPx,
  marker,
  editable,
  onPatch,
  onEnter,
  onFocus,
}: {
  paragraph: PptxParagraph
  shape: PptxShape
  ptToPx: number
  marker: string | null
  editable: boolean
  onPatch: (...patches: DraftPatch[]) => void
  onEnter: (paragraph: PptxParagraph, before: string, after: string) => void
  onFocus: (addr: string) => void
}) {
  const ref = useRef<HTMLDivElement | null>(null)
  const sent = useRef(paragraph.text)

  useEffect(() => {
    const el = ref.current
    if (!el) return
    const focused = document.activeElement === el
    if (!focused || el.textContent === paragraph.text) {
      if (el.textContent !== paragraph.text) el.textContent = paragraph.text
    }
    sent.current = paragraph.text
  }, [paragraph.text])

  return (
    <div
      ref={ref}
      className={`ws-shape-text px-text${paragraph.list ? ' px-listed' : ''}`}
      data-para={paragraph.addr}
      data-marker={marker ?? undefined}
      style={paragraphCss(paragraph, shape, ptToPx)}
      contentEditable={editable}
      suppressContentEditableWarning
      spellCheck
      onFocus={() => onFocus(paragraph.addr)}
      onInput={e => {
        const text = e.currentTarget.textContent ?? ''
        if (text !== sent.current) {
          sent.current = text
          onPatch({ op: 'SetShapeText', addr: paragraph.addr, text })
        }
      }}
      onKeyDown={e => {
        if (e.key === 'Enter') {
          e.preventDefault()
          const el = e.currentTarget
          const sel = window.getSelection()
          const text = el.textContent ?? ''
          let at = text.length
          if (sel && sel.rangeCount && el.contains(sel.anchorNode)) {
            const r = document.createRange()
            r.selectNodeContents(el)
            r.setEnd(sel.getRangeAt(0).startContainer, sel.getRangeAt(0).startOffset)
            at = r.toString().length
          }
          onEnter(paragraph, text.slice(0, at), text.slice(at))
        }
        // Keys that act on the shape must not escape while typing — except
        // Undo/Redo, which belong to the whole document.
        const k = e.key.toLowerCase()
        if (!((e.ctrlKey || e.metaKey) && (k === 'z' || k === 'y'))) e.stopPropagation()
      }}
      onPaste={e => {
        e.preventDefault()
        document.execCommand('insertText', false, e.clipboardData.getData('text/plain').replace(/\s*\n\s*/g, ' '))
      }}
    />
  )
}

// --------------------------------------------------------------------- shape

type Drag = { addr: string; mode: 'move' | 'nw' | 'ne' | 'sw' | 'se'; x0: number; y0: number; box: NonNullable<PptxShape['bbox']> }

function ShapeView({
  slide,
  shape,
  selected,
  editing,
  ptToPx,
  live,
  onPointerDown,
  onEnterText,
  onPatch,
  onEnter,
  onFocusPara,
}: {
  slide: PptxSlide
  shape: PptxShape
  selected: boolean
  editing: boolean
  ptToPx: number
  live: PptxShape['bbox'] | null
  onPointerDown: (e: React.PointerEvent, shape: PptxShape, mode: Drag['mode']) => void
  onEnterText: (shape: PptxShape, e: React.MouseEvent) => void
  onPatch: (...patches: DraftPatch[]) => void
  onEnter: (paragraph: PptxParagraph, before: string, after: string) => void
  onFocusPara: (addr: string) => void
}) {
  const bbox = live ?? shape.bbox
  const style: CSSProperties = bbox
    ? {
        left: `${(bbox.x / slide.width) * 100}%`,
        top: `${(bbox.y / slide.height) * 100}%`,
        width: `${(bbox.cx / slide.width) * 100}%`,
        height: `${(bbox.cy / slide.height) * 100}%`,
      }
    : { left: '8%', top: '8%', width: '84%', height: '16%' }

  const isLine = shape.geometry === 'line'
  const fill = shape.fill ? `#${shape.fill}` : undefined
  const markers = useMemo(() => {
    // Numbering restarts after any paragraph that is not a numbered item.
    return shape.paragraphs.reduce<{ out: (string | null)[]; n: number }>(
      (acc, p) => {
        if (p.list === 'bullet') return { out: [...acc.out, '•'], n: acc.n }
        if (p.list === 'number') return { out: [...acc.out, `${acc.n + 1}.`], n: acc.n + 1 }
        return { out: [...acc.out, null], n: 0 }
      },
      { out: [], n: 0 },
    ).out
  }, [shape.paragraphs])

  const text = (indices: number[]) =>
    indices.map(i => {
      const p = shape.paragraphs[i]
      return p ? (
        <ShapeText key={p.addr} paragraph={p} shape={shape} ptToPx={ptToPx} marker={markers[i]} editable={editing} onPatch={onPatch} onEnter={onEnter} onFocus={onFocusPara} />
      ) : null
    })

  return (
    <div
      className={
        'ws-shape px-shape' +
        (selected ? ' ws-shape-selected' : '') +
        (editing ? ' is-editing' : '') +
        (shape.kind === 'pic' || (!shape.paragraphs.length && !shape.fill && !shape.table && !isLine) ? ' ws-shape-frame' : '') +
        (bbox ? '' : ' ws-shape-inherited')
      }
      data-shape={shape.addr}
      style={style}
      onPointerDown={e => onPointerDown(e, shape, 'move')}
      onDoubleClick={e => onEnterText(shape, e)}
      title={bbox ? undefined : `${shape.name}: position inherited from the slide layout`}
    >
      {shape.kind === 'pic' && shape.image ? (
        <img className="px-image" src={shape.image} alt={shape.name} draggable={false} />
      ) : shape.table ? (
        <table className="px-table">
          <tbody>
            {shape.table.map((row, r) => (
              <tr key={r}>
                {row.map((cell, c) => (
                  <td key={c} className={r === 0 ? 'is-head' : undefined}>
                    {text(cell)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      ) : isLine ? (
        <div className="px-line" style={{ borderTopColor: fill ?? '#4472C4' }} />
      ) : (
        <div className="px-body" style={{ background: fill, ...(shape.geometry ? GEOMETRY_CSS[shape.geometry] : {}) }}>
          {shape.paragraphs.length ? text(shape.paragraphs.map((_, i) => i)) : shape.kind !== 'sp' && <span className="ws-shape-frame-label">{shape.kind === 'graphicFrame' ? 'Chart or diagram' : shape.name}</span>}
        </div>
      )}

      {selected && bbox && !editing &&
        (['nw', 'ne', 'sw', 'se'] as const).map(h => (
          <span key={h} className={`px-handle px-handle-${h}`} onPointerDown={e => onPointerDown(e, shape, h)} />
        ))}
    </div>
  )
}

function SlideMiniature({ slide }: { slide: PptxSlide }) {
  return (
    <span className="dr-mini-slide" style={{ aspectRatio: `${slide.width} / ${slide.height}` }}>
      {slide.shapes.map(shape =>
        shape.bbox ? (
          <span
            key={shape.addr}
            className={`dr-mini-shape${shape.kind === 'pic' ? ' is-frame' : ''}`}
            style={{
              left: `${(shape.bbox.x / slide.width) * 100}%`,
              top: `${(shape.bbox.y / slide.height) * 100}%`,
              width: `${(shape.bbox.cx / slide.width) * 100}%`,
              height: `${(shape.bbox.cy / slide.height) * 100}%`,
              background: shape.fill ? `#${shape.fill}` : undefined,
              ...(shape.geometry ? GEOMETRY_CSS[shape.geometry] : {}),
            }}
          >
            {shape.image ? <img src={shape.image} alt="" /> : shape.paragraphs.map(p => p.text).join(' ')}
          </span>
        ) : null,
      )}
    </span>
  )
}

// -------------------------------------------------------------------- editor

export default function PptxEditor({ slides, onPatch, onCommit, onSelectionChange, onError }: Props) {
  const confirm = useConfirm()
  const [current, setCurrent] = useState(0)
  const [selected, setSelected] = useState<string | null>(null)
  const [editing, setEditing] = useState<string | null>(null)
  const [paraAddr, setParaAddr] = useState<string | null>(null)
  const [tab, setTab] = useState('home')
  const [drag, setDrag] = useState<Drag | null>(null)
  const [live, setLive] = useState<PptxShape['bbox'] | null>(null)
  const [busy, setBusy] = useState(false)
  const [canvasWidth, setCanvasWidth] = useState(900)
  const canvas = useRef<HTMLDivElement | null>(null)
  const focusAfter = useRef<{ shape: string; para: number } | null>(null)

  const slide = slides[Math.min(current, slides.length - 1)]

  useEffect(() => {
    setSelected(null)
    setEditing(null)
  }, [current])

  useEffect(() => {
    if (slide) onSelectionChange?.(slide.index, selected)
  }, [slide, selected, onSelectionChange])

  useLayoutEffect(() => {
    const el = canvas.current
    if (!el) return
    const ro = new ResizeObserver(() => setCanvasWidth(el.clientWidth))
    ro.observe(el)
    setCanvasWidth(el.clientWidth)
    return () => ro.disconnect()
  }, [slide?.index])

  // Focus the paragraph a commit created (Enter in a text box).
  useEffect(() => {
    const target = focusAfter.current
    if (!target) return
    const el = canvas.current?.querySelector<HTMLElement>(`[data-para="${target.shape}/p[${target.para}]"]`)
    if (!el) return
    focusAfter.current = null
    el.focus()
    const sel = window.getSelection()
    sel?.selectAllChildren(el)
    sel?.collapseToStart()
  }, [slides])

  const commit = useCallback(
    async (...patches: DraftPatch[]) => {
      setBusy(true)
      try {
        await onCommit(...patches)
      } catch (e) {
        onError?.(e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
      }
    },
    [onCommit, onError],
  )

  if (!slide) {
    return <div className="ws-empty">This presentation has no slides.</div>
  }

  const ptToPx = (canvasWidth / slide.width) * EMU_PER_PT
  const shape = slide.shapes.find(s => s.addr === selected) ?? null
  const paragraph = shape?.paragraphs.find(p => p.addr === paraAddr) ?? (editing ? null : shape?.paragraphs[0]) ?? null
  const target = editing && paraAddr?.startsWith(`${selected}/`) ? paraAddr : selected

  const format = (fields: TextFormat & { align?: 'left' | 'center' | 'right' | 'justify'; list?: 'bullet' | 'number' | 'none' }) => {
    if (!target) return
    void commit({ op: 'SetShapeTextFormat', addr: target, ...fields })
  }

  const emuPoint = (clientX: number, clientY: number) => {
    const rect = canvas.current!.getBoundingClientRect()
    return { x: ((clientX - rect.left) / rect.width) * slide.width, y: ((clientY - rect.top) / rect.height) * slide.height }
  }

  const onPointerDown = (e: React.PointerEvent, s: PptxShape, mode: Drag['mode']) => {
    if (editing === s.addr && mode === 'move') return // typing, not dragging
    e.stopPropagation()
    if (mode === 'move' && selected === s.addr && s.paragraphs.length && !e.shiftKey) {
      // A second click on a selected text shape enters its text.
      const point = { x: e.clientX, y: e.clientY }
      setEditing(s.addr)
      requestAnimationFrame(() => {
        const range = (document as Document & { caretRangeFromPoint?: (x: number, y: number) => Range | null }).caretRangeFromPoint?.(point.x, point.y)
        const host = range?.startContainer.parentElement?.closest<HTMLElement>('[data-para]')
        const el = host ?? canvas.current?.querySelector<HTMLElement>(`[data-para^="${s.addr}/"]`)
        el?.focus()
        if (range && host) {
          const sel = window.getSelection()
          sel?.removeAllRanges()
          sel?.addRange(range)
        }
      })
      return
    }
    setSelected(s.addr)
    setEditing(null)
    if (!s.bbox) return
    e.preventDefault()
    ;(e.currentTarget as HTMLElement).setPointerCapture?.(e.pointerId)
    const p = emuPoint(e.clientX, e.clientY)
    setDrag({ addr: s.addr, mode, x0: p.x, y0: p.y, box: s.bbox })
  }

  const onPointerMove = (e: React.PointerEvent) => {
    if (!drag) return
    const p = emuPoint(e.clientX, e.clientY)
    const dx = p.x - drag.x0
    const dy = p.y - drag.y0
    const b = drag.box
    const min = 91_440
    let next = { ...b }
    if (drag.mode === 'move') next = { ...b, x: b.x + dx, y: b.y + dy }
    if (drag.mode === 'se') next = { ...b, cx: Math.max(min, b.cx + dx), cy: Math.max(min, b.cy + dy) }
    if (drag.mode === 'sw') next = { x: Math.min(b.x + dx, b.x + b.cx - min), y: b.y, cx: Math.max(min, b.cx - dx), cy: Math.max(min, b.cy + dy) }
    if (drag.mode === 'ne') next = { x: b.x, y: Math.min(b.y + dy, b.y + b.cy - min), cx: Math.max(min, b.cx + dx), cy: Math.max(min, b.cy - dy) }
    if (drag.mode === 'nw') next = { x: Math.min(b.x + dx, b.x + b.cx - min), y: Math.min(b.y + dy, b.y + b.cy - min), cx: Math.max(min, b.cx - dx), cy: Math.max(min, b.cy - dy) }
    setLive({ x: Math.round(next.x), y: Math.round(next.y), cx: Math.round(next.cx), cy: Math.round(next.cy) })
  }

  const onPointerUp = () => {
    if (drag && live) {
      const moved = live.x !== drag.box.x || live.y !== drag.box.y || live.cx !== drag.box.cx || live.cy !== drag.box.cy
      if (moved) void commit({ op: 'SetShapeBox', addr: drag.addr, ...live })
    }
    setDrag(null)
    // Keep the live box until the model catches up, so the shape does not snap back.
    setTimeout(() => setLive(null), 50)
  }

  const nudge = (dx: number, dy: number) => {
    if (!shape?.bbox) return
    const { x, y, cx, cy } = shape.bbox
    void commit({ op: 'SetShapeBox', addr: shape.addr, x: x + dx, y: y + dy, cx, cy })
  }
  const scale = (factor: number) => {
    if (!shape?.bbox) return
    const { x, y, cx, cy } = shape.bbox
    void commit({ op: 'SetShapeBox', addr: shape.addr, x, y, cx: Math.max(Math.round(cx * factor), 1), cy: Math.max(Math.round(cy * factor), 1) })
  }

  const centreBox = (cxFrac: number, cyFrac: number) => {
    const cx = Math.round(slide.width * cxFrac)
    const cy = Math.round(slide.height * cyFrac)
    return { x: Math.round((slide.width - cx) / 2), y: Math.round((slide.height - cy) / 2), cx, cy }
  }

  const onEnter = (p: PptxParagraph, before: string, after: string) => {
    const shapeAddr = p.addr.replace(/\/p\[\d+\]$/, '')
    const ord = Number(/\/p\[(\d+)\]$/.exec(p.addr)?.[1] ?? 0)
    focusAfter.current = { shape: shapeAddr, para: ord + 1 }
    void commit({ op: 'SetShapeText', addr: p.addr, text: before }, { op: 'InsertShapeParagraph', addr: p.addr, text: after })
  }

  const insertSymbol = (s: string) => {
    const el = paraAddr ? canvas.current?.querySelector<HTMLElement>(`[data-para="${paraAddr}"]`) : null
    if (!el || !editing) {
      onError?.('Click into a text box first, then insert the symbol.')
      return
    }
    el.focus()
    document.execCommand('insertText', false, s)
  }

  const deleteShape = () => {
    if (!shape) return
    void commit({ op: 'DeleteShape', addr: shape.addr })
    setSelected(null)
    setEditing(null)
  }

  const tabs = [
    { id: 'home', label: 'Home' },
    { id: 'insert', label: 'Insert' },
    { id: 'arrange', label: 'Arrange' },
  ]
  const none = !shape

  return (
    <div
      className="ws-pptx px"
      tabIndex={-1}
      onKeyDown={e => {
        if (!shape || editing) return
        if (e.key === 'Delete' || e.key === 'Backspace') {
          e.preventDefault()
          deleteShape()
        }
        const step = e.shiftKey ? STEP * 10 : STEP
        if (e.key === 'ArrowLeft') nudge(-step, 0)
        if (e.key === 'ArrowRight') nudge(step, 0)
        if (e.key === 'ArrowUp') nudge(0, -step)
        if (e.key === 'ArrowDown') nudge(0, step)
        if (e.key === 'Escape') setSelected(null)
      }}
    >
      <Ribbon tabs={tabs} active={tab} onTab={setTab} aside={<span className="rb-hint">Slide {slide.index + 1} of {slides.length}</span>}>
        {tab === 'home' && (
          <>
            <Group label="Slides">
              <Tool icon={<Plus size={15} />} label="New slide" title="Add a slide after this one" disabled={busy} onClick={() => void commit({ op: 'AddSlide', after: slide.index }).then(() => setCurrent(slide.index + 1))} />
            </Group>
            <Group label="Font">
              <FontPicker value={paragraph?.font ?? 'Calibri'} disabled={none} onPick={font => format({ font })} />
              <SizePicker value={paragraph?.size ?? (shape ? defaultSize(shape) : 18)} disabled={none} onPick={size => format({ size })} />
              <Tool icon={<Type size={15} />} label="+" title="Grow font" disabled={none} onClick={() => format({ size: Math.min(400, (paragraph?.size ?? (shape ? defaultSize(shape) : 18)) + 2) })} />
              <Tool icon={<Type size={12} />} label="−" title="Shrink font" disabled={none} onClick={() => format({ size: Math.max(6, (paragraph?.size ?? (shape ? defaultSize(shape) : 18)) - 2) })} />
            </Group>
            <Group label="Character">
              <Tool icon={<Bold size={15} strokeWidth={2.25} />} title="Bold" active={!!paragraph?.bold} disabled={none} onClick={() => format({ bold: !paragraph?.bold })} />
              <Tool icon={<Italic size={15} />} title="Italic" active={!!paragraph?.italic} disabled={none} onClick={() => format({ italic: !paragraph?.italic })} />
              <Tool icon={<Underline size={15} />} title="Underline" active={!!paragraph?.underline} disabled={none} onClick={() => format({ underline: !paragraph?.underline })} />
              <Tool icon={<Strikethrough size={15} />} title="Strikethrough" active={!!paragraph?.strike} disabled={none} onClick={() => format({ strike: !paragraph?.strike })} />
              <ColorPicker icon={<Baseline size={15} />} title="Font colour" value={paragraph?.color ?? null} disabled={none} onPick={color => format({ color })} />
              <ColorPicker icon={<Highlighter size={15} />} title="Text highlight" value={null} disabled={none} clearLabel="No highlight" onPick={highlight => format({ highlight })} />
            </Group>
            <Group label="Paragraph">
              <Tool icon={<List size={15} />} title="Bullets" active={paragraph?.list === 'bullet'} disabled={none} onClick={() => format({ list: paragraph?.list === 'bullet' ? 'none' : 'bullet' })} />
              <Tool icon={<ListOrdered size={15} />} title="Numbering" active={paragraph?.list === 'number'} disabled={none} onClick={() => format({ list: paragraph?.list === 'number' ? 'none' : 'number' })} />
              <Tool icon={<AlignLeft size={15} />} title="Align left" active={paragraph?.align === 'left'} disabled={none} onClick={() => format({ align: 'left' })} />
              <Tool icon={<AlignCenter size={15} />} title="Centre" active={paragraph?.align === 'center'} disabled={none} onClick={() => format({ align: 'center' })} />
              <Tool icon={<AlignRight size={15} />} title="Align right" active={paragraph?.align === 'right'} disabled={none} onClick={() => format({ align: 'right' })} />
              <Tool icon={<AlignJustify size={15} />} title="Justify" active={paragraph?.align === 'justify'} disabled={none} onClick={() => format({ align: 'justify' })} />
            </Group>
            <Group label="Shape">
              <ColorPicker icon={<PaintBucket size={15} />} title="Shape fill" value={shape?.fill ?? null} disabled={none || shape?.kind !== 'sp'} clearLabel="No fill" onPick={color => shape && void commit({ op: 'SetShapeFill', addr: shape.addr, color })} />
            </Group>
          </>
        )}

        {tab === 'insert' && (
          <>
            <Group label="Slides">
              <Tool icon={<Plus size={15} />} label="New slide" title="Add a slide after this one" disabled={busy} onClick={() => void commit({ op: 'AddSlide', after: slide.index }).then(() => setCurrent(slide.index + 1))} />
              <Tool icon={<Copy size={15} />} label="Duplicate" title="Duplicate this slide" disabled={busy} onClick={() => void commit({ op: 'DuplicateSlide', slide: slide.index }).then(() => setCurrent(slide.index + 1))} />
            </Group>
            <Group label="Text">
              <Tool icon={<Type size={15} />} label="Text box" title="Insert a text box" disabled={busy} onClick={() => void commit({ op: 'InsertTextBox', addr: slide.addr, text: 'Text', ...centreBox(0.4, 0.12) })} />
              <SymbolPicker trigger={<><Omega size={15} /><span>Symbol</span></>} onPick={insertSymbol} />
            </Group>
            <Group label="Illustrations">
              <Popover title="Shapes" disabled={busy} trigger={<><Shapes size={15} /><span>Shapes</span></>}>
                {close => (
                  <div className="rb-shapes">
                    {GEOMETRIES.map(g => (
                      <button
                        key={g.id}
                        type="button"
                        className="rb-shape-option"
                        title={g.label}
                        onClick={() => {
                          close()
                          const box = g.id === 'line' ? { ...centreBox(0.3, 0.01), cy: 12_700 } : centreBox(0.2, 0.3)
                          void commit({ op: 'InsertShape', addr: slide.addr, geometry: g.id, fill: '4472C4', ...box })
                        }}
                      >
                        {g.id === 'line' ? <span className="rb-shape-line" /> : <span className="rb-shape-swatch" style={g.css} />}
                      </button>
                    ))}
                  </div>
                )}
              </Popover>
              <TableGridPicker
                disabled={busy}
                trigger={<><Table2 size={15} /><span>Table</span></>}
                onPick={(rows, cols) => void commit({ op: 'InsertSlideTable', addr: slide.addr, rows, cols, ...centreBox(0.7, Math.min(0.7, 0.08 * rows)) })}
              />
              <Tool
                icon={<ImagePlus size={15} />}
                label="Picture"
                title="Insert a picture"
                disabled={busy}
                onClick={() => {
                  void (async () => {
                    const file = await pickImage()
                    if (!file) return
                    try {
                      const img = await readImage(file)
                      // Fit within half the slide, keeping the aspect ratio.
                      const maxW = slide.width * 0.5
                      const maxH = slide.height * 0.6
                      const k = Math.min(1, maxW / img.width_emu, maxH / img.height_emu)
                      const w = Math.round(img.width_emu * k)
                      const h = Math.round(img.height_emu * k)
                      await commit({ op: 'InsertImage', addr: slide.addr, data_base64: img.data_base64, file_name: img.file_name, width_emu: w, height_emu: h, x: Math.round((slide.width - w) / 2), y: Math.round((slide.height - h) / 2) })
                    } catch (err) {
                      onError?.(err instanceof Error ? err.message : String(err))
                    }
                  })()
                }}
              />
            </Group>
          </>
        )}

        {tab === 'arrange' && (
          <>
            <Group label="Position">
              <Tool icon={<ArrowLeft size={15} />} title="Move left" disabled={!shape?.bbox} onClick={() => nudge(-STEP, 0)} />
              <Tool icon={<ArrowRight size={15} />} title="Move right" disabled={!shape?.bbox} onClick={() => nudge(STEP, 0)} />
              <Tool icon={<ArrowUp size={15} />} title="Move up" disabled={!shape?.bbox} onClick={() => nudge(0, -STEP)} />
              <Tool icon={<ArrowDown size={15} />} title="Move down" disabled={!shape?.bbox} onClick={() => nudge(0, STEP)} />
            </Group>
            <Group label="Size">
              <Tool icon={<Maximize2 size={15} />} label="Larger" title="Larger" disabled={!shape?.bbox} onClick={() => scale(1.1)} />
              <Tool icon={<Minimize2 size={15} />} label="Smaller" title="Smaller" disabled={!shape?.bbox} onClick={() => scale(0.9)} />
            </Group>
            <Group label="Remove">
              <Tool icon={<Trash2 size={15} />} label="Delete shape" title="Delete the selected shape" danger disabled={none || busy} onClick={deleteShape} />
              <Tool
                icon={<Trash2 size={15} />}
                label="Delete slide"
                title="Delete this slide"
                danger
                disabled={slides.length <= 1 || busy}
                onClick={() => {
                  void (async () => {
                    const ok = await confirm({
                      title: `Delete slide ${slide.index + 1}?`,
                      message: 'Earlier versions keep it.',
                      confirmLabel: 'Delete slide',
                    })
                    if (ok) {
                      await commit({ op: 'DeleteSlide', slide: slide.index })
                      setCurrent(c => Math.max(0, c - 1))
                    }
                  })()
                }}
              />
            </Group>
          </>
        )}
      </Ribbon>

      <div className="ws-pptx-body">
        <div className="ws-slide-rail">
          {slides.map(s => (
            <div key={s.addr} className="px-thumb-wrap">
              <button type="button" className={`ws-slide-thumb${s.index === slide.index ? ' ws-slide-thumb-active' : ''}`} onClick={() => setCurrent(s.index)}>
                <span className="ws-slide-thumb-number">{s.index + 1}</span>
                <span className="dr-thumb-col">
                  <SlideMiniature slide={s} />
                </span>
              </button>
            </div>
          ))}
          <button type="button" className="px-add-slide" disabled={busy} onClick={() => void commit({ op: 'AddSlide', after: slides.length - 1 }).then(() => setCurrent(slides.length))}>
            <Plus size={14} /> New slide
          </button>
        </div>

        <div className="ws-slide-stage" onPointerMove={onPointerMove} onPointerUp={onPointerUp}>
          <div
            ref={canvas}
            className="ws-slide-canvas"
            style={{ aspectRatio: `${slide.width} / ${slide.height}` }}
            onPointerDown={e => {
              if (e.target === canvas.current) {
                setSelected(null)
                setEditing(null)
              }
            }}
          >
            {slide.shapes.map(s => (
              <ShapeView
                key={s.addr}
                slide={slide}
                shape={s}
                selected={s.addr === selected}
                editing={s.addr === editing}
                ptToPx={ptToPx}
                live={drag?.addr === s.addr || (live && selected === s.addr) ? live : null}
                onPointerDown={onPointerDown}
                onEnterText={(sh, e) => {
                  if (!sh.paragraphs.length) return
                  setSelected(sh.addr)
                  setEditing(sh.addr)
                  const range = (document as Document & { caretRangeFromPoint?: (x: number, y: number) => Range | null }).caretRangeFromPoint?.(e.clientX, e.clientY)
                  requestAnimationFrame(() => {
                    const host = range?.startContainer.parentElement?.closest<HTMLElement>('[data-para]')
                    ;(host ?? canvas.current?.querySelector<HTMLElement>(`[data-para^="${sh.addr}/"]`))?.focus()
                  })
                }}
                onPatch={onPatch}
                onEnter={onEnter}
                onFocusPara={addr => {
                  setParaAddr(addr)
                  const owner = addr.replace(/\/p\[\d+\]$/, '')
                  setSelected(owner)
                  setEditing(owner)
                }}
              />
            ))}
          </div>
          <span className="px-stage-hint ws-hint">
            {editing ? 'Typing · Esc or click outside to finish' : shape ? 'Drag to move · corners to resize · click again to type · Delete removes' : 'Click a shape to select it'}
          </span>
        </div>
      </div>
    </div>
  )
}
