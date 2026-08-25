/**
 * Slide editor: a rail of thumbnails and an SVG-backed canvas.
 *
 * # Honest rendering
 *
 * Text boxes, placeholders and geometry render; pictures, charts and diagrams
 * render as a labelled frame in their real position and size. That is a display
 * limitation and nothing more — a shape this editor draws as a frame is a shape
 * it never addresses, so the file keeps it whole. The frames say what they are
 * rather than pretending to be empty.
 *
 * # Units
 *
 * DrawingML measures in EMU (914,400 per inch). The canvas is laid out in
 * percentages of the slide box, so it scales to any panel width without a
 * pixel-per-EMU constant to get wrong.
 */

import { useCallback, useEffect, useRef, useState } from 'react'

import type { DraftPatch, PptxShape, PptxSlide } from '../../../api/drafts'

interface Props {
  slides: PptxSlide[]
  onPatch: (...patches: DraftPatch[]) => void
}

/** Shapes we can draw properly, as opposed to frame with a label. */
function isTextual(shape: PptxShape): boolean {
  return shape.kind === 'sp' && shape.paragraphs.length > 0
}

function frameLabel(shape: PptxShape): string {
  switch (shape.kind) {
    case 'pic':
      return 'Picture'
    case 'graphicFrame':
      return 'Chart or table'
    case 'grpSp':
      return 'Grouped shapes'
    default:
      return shape.name
  }
}

function ShapeBox({
  slide,
  shape,
  selected,
  onSelect,
  onPatch,
}: {
  slide: PptxSlide
  shape: PptxShape
  selected: boolean
  onSelect: () => void
  onPatch: (...patches: DraftPatch[]) => void
}) {
  const bbox = shape.bbox
  // A shape with no transform inherits its box from the slide layout, which we
  // do not read. Rather than stacking everything at the origin, such shapes are
  // laid out down the middle so they are at least findable and editable.
  const style = bbox
    ? {
        left: `${(bbox.x / slide.width) * 100}%`,
        top: `${(bbox.y / slide.height) * 100}%`,
        width: `${(bbox.cx / slide.width) * 100}%`,
        height: `${(bbox.cy / slide.height) * 100}%`,
      }
    : { left: '10%', top: '10%', width: '80%', height: '15%' }

  return (
    <div
      className={
        'ws-shape' +
        (selected ? ' ws-shape-selected' : '') +
        (isTextual(shape) ? '' : ' ws-shape-frame') +
        (bbox ? '' : ' ws-shape-inherited')
      }
      style={style}
      onMouseDown={onSelect}
      title={bbox ? shape.name : `${shape.name} — position inherited from the slide layout`}
    >
      {isTextual(shape) ? (
        shape.paragraphs.map(paragraph => (
          <div
            key={paragraph.addr}
            className="ws-shape-text"
            style={{ paddingLeft: `${paragraph.level * 1.2}em` }}
            contentEditable
            suppressContentEditableWarning
            spellCheck={false}
            // Uncontrolled on purpose: re-rendering a focused contentEditable
            // moves the caret to the start. The value is committed on blur.
            onBlur={e => {
              const text = e.currentTarget.textContent ?? ''
              if (text !== paragraph.text) {
                onPatch({ op: 'SetShapeText', addr: paragraph.addr, text })
              }
            }}
            onPaste={e => {
              e.preventDefault()
              document.execCommand('insertText', false, e.clipboardData.getData('text/plain'))
            }}
          >
            {paragraph.text}
          </div>
        ))
      ) : (
        <span className="ws-shape-frame-label">{frameLabel(shape)}</span>
      )}
    </div>
  )
}

export default function PptxEditor({ slides, onPatch }: Props) {
  const [current, setCurrent] = useState(0)
  const [selected, setSelected] = useState<string | null>(null)
  const canvas = useRef<HTMLDivElement | null>(null)

  const slide = slides[Math.min(current, slides.length - 1)]

  // Re-selecting across a slide change would leave a stale address selected,
  // and the toolbar acting on a shape the user cannot see.
  useEffect(() => setSelected(null), [current])

  const selectedShape = slide?.shapes.find(s => s.addr === selected) ?? null

  const nudge = useCallback(
    (dx: number, dy: number) => {
      if (!selectedShape?.bbox) return
      const { x, y, cx, cy } = selectedShape.bbox
      onPatch({ op: 'SetShapeBox', addr: selectedShape.addr, x: x + dx, y: y + dy, cx, cy })
    },
    [selectedShape, onPatch],
  )

  const resize = useCallback(
    (factor: number) => {
      if (!selectedShape?.bbox) return
      const { x, y, cx, cy } = selectedShape.bbox
      onPatch({
        op: 'SetShapeBox',
        addr: selectedShape.addr,
        x,
        y,
        cx: Math.max(Math.round(cx * factor), 1),
        cy: Math.max(Math.round(cy * factor), 1),
      })
    },
    [selectedShape, onPatch],
  )

  if (!slide) {
    return <div className="ws-empty">This presentation has no slides.</div>
  }

  // A tenth of an inch, which is roughly the smallest move that reads as
  // deliberate on a projected slide.
  const STEP = 91_440

  return (
    <div className="ws-pptx">
      <div className="ws-toolbar">
        <span className="ws-hint">
          Slide {slide.index + 1} of {slides.length}
        </span>
        <span className="ws-tool-divider" />
        <button type="button" className="ws-tool" disabled={!selectedShape?.bbox} onClick={() => nudge(-STEP, 0)} title="Move left">
          ←
        </button>
        <button type="button" className="ws-tool" disabled={!selectedShape?.bbox} onClick={() => nudge(STEP, 0)} title="Move right">
          →
        </button>
        <button type="button" className="ws-tool" disabled={!selectedShape?.bbox} onClick={() => nudge(0, -STEP)} title="Move up">
          ↑
        </button>
        <button type="button" className="ws-tool" disabled={!selectedShape?.bbox} onClick={() => nudge(0, STEP)} title="Move down">
          ↓
        </button>
        <span className="ws-tool-divider" />
        <button type="button" className="ws-tool" disabled={!selectedShape?.bbox} onClick={() => resize(1.1)} title="Larger">
          ＋
        </button>
        <button type="button" className="ws-tool" disabled={!selectedShape?.bbox} onClick={() => resize(0.9)} title="Smaller">
          －
        </button>
        <span className="ws-tool-divider" />
        <button
          type="button"
          className="ws-tool ws-tool-danger"
          disabled={!selectedShape}
          onClick={() => {
            if (selectedShape && window.confirm(`Delete "${selectedShape.name}"?`)) {
              onPatch({ op: 'DeleteShape', addr: selectedShape.addr })
              setSelected(null)
            }
          }}
          title="Delete the selected shape"
        >
          Delete shape
        </button>
      </div>

      <div className="ws-pptx-body">
        <div className="ws-slide-rail">
          {slides.map(s => (
            <button
              key={s.addr}
              type="button"
              className={`ws-slide-thumb${s.index === slide.index ? ' ws-slide-thumb-active' : ''}`}
              onClick={() => setCurrent(s.index)}
            >
              <span className="ws-slide-thumb-number">{s.index + 1}</span>
              <span className="ws-slide-thumb-title">
                {s.shapes.find(sh => sh.paragraphs.length > 0)?.paragraphs[0]?.text || 'Untitled slide'}
              </span>
            </button>
          ))}
        </div>

        <div className="ws-slide-stage">
          <div
            ref={canvas}
            className="ws-slide-canvas"
            style={{ aspectRatio: `${slide.width} / ${slide.height}` }}
            onMouseDown={e => {
              // A click on the canvas background, not on a shape, deselects.
              if (e.target === canvas.current) setSelected(null)
            }}
          >
            {slide.shapes.map(shape => (
              <ShapeBox
                key={shape.addr}
                slide={slide}
                shape={shape}
                selected={shape.addr === selected}
                onSelect={() => setSelected(shape.addr)}
                onPatch={onPatch}
              />
            ))}
          </div>
        </div>
      </div>
    </div>
  )
}
