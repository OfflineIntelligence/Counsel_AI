/**
 * The drafting ribbon: one small kit shared by every editor.
 *
 * Word, PowerPoint and Excel each get the same controls — a tab strip, icon
 * tools, font and size pickers, colour palettes, a table grid, a symbol grid,
 * an equation dialog — so a font picker behaves identically whether it is
 * formatting a paragraph, a slide title or a cell. Editors own what a control
 * DOES; this file owns only how it looks and behaves.
 *
 * Every tool suppresses `mousedown` so clicking it never steals the caret or
 * selection from the document it is about to act on.
 */

import {
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react'
import { ChevronDown, History, Redo2, Save, Undo2, X } from 'lucide-react'

import { FONTS, HIGHLIGHT_HEX, HIGHLIGHTS, PALETTE, RibbonActionsContext, SIZES, SYMBOLS } from './ribbonData'
import './editors.css'

// ------------------------------------------------------------------- helpers

const keepSelection = (e: React.MouseEvent) => e.preventDefault()

/** Close a popover on an outside pointer-down or Escape. */
function useDismiss(open: boolean, ref: React.RefObject<HTMLElement | null>, close: () => void) {
  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) close()
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close()
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [open, ref, close])
}

// --------------------------------------------------------------------- shell

export interface RibbonTab {
  id: string
  label: string
  /** A contextual tab (Table, Picture) is tinted and appears only when relevant. */
  contextual?: boolean
}

export function Ribbon({
  tabs,
  active,
  onTab,
  children,
  aside,
}: {
  tabs: RibbonTab[]
  active: string
  onTab: (id: string) => void
  children: ReactNode
  aside?: ReactNode
}) {
  const actions = useContext(RibbonActionsContext)
  return (
    <div className="rb" role="toolbar" aria-label="Editing tools" onMouseDown={e => {
      // Clicks on the ribbon's empty space must not blur the document either.
      if ((e.target as HTMLElement).closest('input, select, textarea')) return
      e.preventDefault()
    }}>
      <div className="rb-tabs" role="tablist">
        {tabs.map(t => (
          <button
            key={t.id}
            type="button"
            role="tab"
            aria-selected={t.id === active}
            className={`rb-tab${t.id === active ? ' is-active' : ''}${t.contextual ? ' is-contextual' : ''}`}
            onMouseDown={keepSelection}
            onClick={() => onTab(t.id)}
          >
            {t.label}
          </button>
        ))}
        <div className="rb-aside">
          {aside}
          {actions && (
            <span className="rb-doc-actions">
              {/* Undo and redo lead the cluster: they act on whatever the last
                  tool did, so they must be in reach from every tab, not just
                  Home. */}
              <button
                type="button"
                className="rb-tool"
                onMouseDown={keepSelection}
                onClick={actions.onUndo}
                disabled={!actions.canUndo}
                title="Undo (Ctrl+Z)"
                aria-label="Undo"
              >
                <Undo2 size={15} strokeWidth={1.75} />
              </button>
              <button
                type="button"
                className="rb-tool"
                onMouseDown={keepSelection}
                onClick={actions.onRedo}
                disabled={!actions.canRedo}
                title="Redo (Ctrl+Y)"
                aria-label="Redo"
              >
                <Redo2 size={15} strokeWidth={1.75} />
              </button>
              <span className="rb-doc-divider" aria-hidden />
              <button
                type="button"
                className={`rb-tool has-label${actions.historyOpen ? ' is-active' : ''}`}
                onMouseDown={keepSelection}
                onClick={actions.onHistory}
                aria-pressed={actions.historyOpen}
                title="Version history"
              >
                <History size={15} strokeWidth={1.75} />
                <span>History</span>
              </button>
              <button
                type="button"
                className="rb-save"
                onMouseDown={keepSelection}
                onClick={actions.onSave}
                disabled={actions.saving}
                title="Save and cut a version (Ctrl+S)"
              >
                <Save size={14} strokeWidth={1.75} />
                <span>{actions.saving ? 'Saving…' : 'Save'}</span>
              </button>
            </span>
          )}
        </div>
      </div>
      <div className="rb-row">{children}</div>
    </div>
  )
}

export function Group({ children, label }: { children: ReactNode; label?: string }) {
  return (
    <div className="rb-group" aria-label={label}>
      {children}
    </div>
  )
}

export function Tool({
  icon,
  label,
  title,
  active,
  disabled,
  danger,
  onClick,
  wide,
}: {
  icon?: ReactNode
  label?: string
  title: string
  active?: boolean
  disabled?: boolean
  danger?: boolean
  wide?: boolean
  onClick: () => void
}) {
  return (
    <button
      type="button"
      className={`rb-tool${active ? ' is-active' : ''}${danger ? ' is-danger' : ''}${label && !wide ? ' has-label' : ''}${wide ? ' is-wide' : ''}`}
      title={title}
      aria-label={title}
      aria-pressed={active}
      disabled={disabled}
      onMouseDown={keepSelection}
      onClick={onClick}
    >
      {icon}
      {label && <span>{label}</span>}
    </button>
  )
}

// ------------------------------------------------------------------ popovers

export function Popover({
  trigger,
  title,
  disabled,
  children,
  wide,
  align = 'left',
}: {
  trigger: ReactNode
  title: string
  disabled?: boolean
  children: (close: () => void) => ReactNode
  wide?: boolean
  align?: 'left' | 'right'
}) {
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLDivElement | null>(null)
  const panel = useRef<HTMLDivElement | null>(null)
  const [shift, setShift] = useState(0)
  const close = useMemo(() => () => setOpen(false), [])
  useDismiss(open, ref, close)

  // Keep the panel inside the window on the right edge.
  useLayoutEffect(() => {
    if (!open || !panel.current) return
    const rect = panel.current.getBoundingClientRect()
    const overflow = rect.right - (window.innerWidth - 8)
    setShift(overflow > 0 ? -overflow : 0)
  }, [open])

  return (
    <div className="rb-pop" ref={ref}>
      <button
        type="button"
        className={`rb-tool rb-pop-trigger${open ? ' is-open' : ''}`}
        title={title}
        aria-label={title}
        aria-haspopup="dialog"
        aria-expanded={open}
        disabled={disabled}
        onMouseDown={keepSelection}
        onClick={() => setOpen(o => !o)}
      >
        {trigger}
        <ChevronDown size={11} strokeWidth={2} className="rb-caret" />
      </button>
      {open && (
        <div
          ref={panel}
          className={`rb-pop-panel${wide ? ' is-wide' : ''}${align === 'right' ? ' is-right' : ''}`}
          style={{ transform: shift ? `translateX(${shift}px)` : undefined }}
          onMouseDown={e => {
            if (!(e.target as HTMLElement).closest('input, textarea, select')) e.preventDefault()
          }}
        >
          {children(close)}
        </div>
      )}
    </div>
  )
}

/** A searchable font list, each face shown in itself. */
export function FontPicker({ value, onPick, disabled }: { value: string; onPick: (font: string) => void; disabled?: boolean }) {
  const [open, setOpen] = useState(false)
  const [draft, setDraft] = useState(value)
  const ref = useRef<HTMLDivElement | null>(null)
  const close = useMemo(() => () => setOpen(false), [])
  useDismiss(open, ref, close)

  useEffect(() => {
    if (!open) setDraft(value)
  }, [value, open])

  // Until the user types, every face is listed; typing filters.
  const query = draft === value ? '' : draft.trim().toLowerCase()
  const shown = FONTS.filter(f => !query || f.toLowerCase().includes(query))

  return (
    <div className="rb-combo rb-combo-font" ref={ref}>
      <input
        className="rb-combo-input"
        value={draft}
        disabled={disabled}
        aria-label="Font"
        title="Font"
        spellCheck={false}
        onFocus={e => {
          setOpen(true)
          e.currentTarget.select()
        }}
        onChange={e => setDraft(e.target.value)}
        onKeyDown={e => {
          if (e.key === 'Enter' && draft.trim()) {
            onPick(draft.trim())
            setOpen(false)
            e.currentTarget.blur()
          }
        }}
        style={{ fontFamily: value ? `"${value}", var(--font-sans)` : undefined }}
      />
      <button type="button" className="rb-combo-caret" disabled={disabled} onMouseDown={keepSelection} onClick={() => setOpen(o => !o)} aria-label="Fonts">
        <ChevronDown size={11} strokeWidth={2} />
      </button>
      {open && (
        <div className="rb-pop-panel rb-list" onMouseDown={keepSelection}>
          {shown.map(f => (
            <button
              key={f}
              type="button"
              className={`rb-list-item${f === value ? ' is-active' : ''}`}
              style={{ fontFamily: `"${f}", var(--font-sans)` }}
              onClick={() => {
                onPick(f)
                setOpen(false)
              }}
            >
              {f}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

export function SizePicker({ value, onPick, disabled }: { value: number | null; onPick: (size: number) => void; disabled?: boolean }) {
  const [open, setOpen] = useState(false)
  const [draft, setDraft] = useState(value ? String(value) : '')
  const ref = useRef<HTMLDivElement | null>(null)
  const close = useMemo(() => () => setOpen(false), [])
  useDismiss(open, ref, close)

  useEffect(() => {
    setDraft(value ? String(value) : '')
  }, [value])

  const commit = (raw: string) => {
    const n = Number(raw)
    if (Number.isFinite(n) && n >= 1 && n <= 1638) onPick(Math.round(n * 2) / 2)
  }

  return (
    <div className="rb-combo rb-combo-size" ref={ref}>
      <input
        className="rb-combo-input"
        value={draft}
        disabled={disabled}
        aria-label="Font size"
        title="Font size"
        inputMode="decimal"
        onFocus={e => e.currentTarget.select()}
        onChange={e => setDraft(e.target.value)}
        onKeyDown={e => {
          if (e.key === 'Enter') {
            commit(draft)
            e.currentTarget.blur()
          }
        }}
      />
      <button type="button" className="rb-combo-caret" disabled={disabled} onMouseDown={keepSelection} onClick={() => setOpen(o => !o)} aria-label="Sizes">
        <ChevronDown size={11} strokeWidth={2} />
      </button>
      {open && (
        <div className="rb-pop-panel rb-list rb-list-narrow" onMouseDown={keepSelection}>
          {SIZES.map(s => (
            <button
              key={s}
              type="button"
              className={`rb-list-item${s === value ? ' is-active' : ''}`}
              onClick={() => {
                onPick(s)
                setOpen(false)
              }}
            >
              {s}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

/** Icon with a colour bar beneath it, and a palette. */
export function ColorPicker({
  icon,
  title,
  value,
  onPick,
  disabled,
  clearLabel = 'Automatic',
}: {
  icon: ReactNode
  title: string
  /** `RRGGBB`, or null for automatic / none. */
  value: string | null
  onPick: (hex: string) => void
  disabled?: boolean
  clearLabel?: string
}) {
  return (
    <Popover
      title={title}
      disabled={disabled}
      trigger={
        <span className="rb-color-trigger">
          {icon}
          <span className="rb-color-bar" style={{ background: value ? `#${value}` : 'transparent' }} />
        </span>
      }
    >
      {close => (
        <div className="rb-palette">
          <button
            type="button"
            className="rb-palette-clear"
            onClick={() => {
              onPick('')
              close()
            }}
          >
            <X size={12} strokeWidth={2} /> {clearLabel}
          </button>
          {PALETTE.map((row, i) => (
            <div key={i} className="rb-palette-row">
              {row.map(hex => (
                <button
                  key={hex}
                  type="button"
                  className={`rb-swatch${hex === value ? ' is-active' : ''}`}
                  style={{ background: `#${hex}` }}
                  title={`#${hex}`}
                  onClick={() => {
                    onPick(hex)
                    close()
                  }}
                />
              ))}
            </div>
          ))}
          <label className="rb-palette-custom">
            Custom
            <input
              type="color"
              value={`#${value ?? '000000'}`}
              onChange={e => onPick(e.target.value.slice(1).toUpperCase())}
            />
          </label>
        </div>
      )}
    </Popover>
  )
}

/** Word's fixed highlight set. */
export function HighlightPicker({
  icon,
  value,
  onPick,
  disabled,
}: {
  icon: ReactNode
  value: string | null
  onPick: (name: string) => void
  disabled?: boolean
}) {
  return (
    <Popover
      title="Highlight"
      disabled={disabled}
      trigger={
        <span className="rb-color-trigger">
          {icon}
          <span className="rb-color-bar" style={{ background: value ? `#${HIGHLIGHT_HEX[value] ?? 'FFFF00'}` : '#FFFF00' }} />
        </span>
      }
    >
      {close => (
        <div className="rb-palette">
          <div className="rb-palette-row rb-palette-wrap">
            {HIGHLIGHTS.map(h => (
              <button
                key={h.name}
                type="button"
                className={`rb-swatch${h.name === value ? ' is-active' : ''}`}
                style={{ background: `#${h.hex}` }}
                title={h.name}
                onClick={() => {
                  onPick(h.name)
                  close()
                }}
              />
            ))}
          </div>
          <button
            type="button"
            className="rb-palette-clear"
            onClick={() => {
              onPick('')
              close()
            }}
          >
            <X size={12} strokeWidth={2} /> No highlight
          </button>
        </div>
      )}
    </Popover>
  )
}

export function TableGridPicker({ onPick, disabled, trigger }: { onPick: (rows: number, cols: number) => void; disabled?: boolean; trigger: ReactNode }) {
  const [hover, setHover] = useState<[number, number]>([0, 0])
  const N = 8
  return (
    <Popover title="Insert table" disabled={disabled} trigger={trigger}>
      {close => (
        <div className="rb-grid-picker" onMouseLeave={() => setHover([0, 0])}>
          <div className="rb-grid-cells" style={{ gridTemplateColumns: `repeat(${N}, 18px)` }}>
            {Array.from({ length: N * N }, (_, i) => {
              const r = Math.floor(i / N) + 1
              const c = (i % N) + 1
              const on = r <= hover[0] && c <= hover[1]
              return (
                <button
                  key={i}
                  type="button"
                  className={`rb-grid-cell${on ? ' is-on' : ''}`}
                  onMouseEnter={() => setHover([r, c])}
                  onClick={() => {
                    onPick(r, c)
                    close()
                  }}
                  aria-label={`${r} by ${c} table`}
                />
              )
            })}
          </div>
          <span className="rb-grid-label">{hover[0] ? `${hover[1]} × ${hover[0]} table` : 'Choose a size'}</span>
        </div>
      )}
    </Popover>
  )
}

export function SymbolPicker({ onPick, disabled, trigger }: { onPick: (symbol: string) => void; disabled?: boolean; trigger: ReactNode }) {
  return (
    <Popover title="Insert symbol" disabled={disabled} trigger={trigger} wide>
      {close => (
        <div className="rb-symbols">
          {SYMBOLS.map(group => (
            <div key={group.group}>
              <span className="rb-symbols-label">{group.group}</span>
              <div className="rb-symbols-grid">
                {group.items.map(s => (
                  <button
                    key={s}
                    type="button"
                    className="rb-symbol"
                    onClick={() => {
                      onPick(s)
                      close()
                    }}
                  >
                    {s}
                  </button>
                ))}
              </div>
            </div>
          ))}
        </div>
      )}
    </Popover>
  )
}

// ------------------------------------------------------------------ equations

const MATH_SYMBOLS: Record<string, string> = {
  alpha: 'α', beta: 'β', gamma: 'γ', delta: 'δ', epsilon: 'ε', theta: 'θ', lambda: 'λ', mu: 'μ', pi: 'π',
  rho: 'ρ', sigma: 'σ', tau: 'τ', phi: 'φ', omega: 'ω', Gamma: 'Γ', Delta: 'Δ', Theta: 'Θ', Pi: 'Π',
  Sigma: 'Σ', Phi: 'Φ', Omega: 'Ω', sum: '∑', prod: '∏', int: '∫', partial: '∂', infty: '∞', pm: '±',
  times: '×', div: '÷', cdot: '·', le: '≤', leq: '≤', ge: '≥', geq: '≥', ne: '≠', neq: '≠', approx: '≈',
  in: '∈', rightarrow: '→', Rightarrow: '⇒', forall: '∀', exists: '∃', nabla: '∇', degree: '°',
}

type MathNode = string | { kind: 'sup' | 'sub'; base: MathNode[]; script: MathNode[] } | { kind: 'frac'; num: MathNode[]; den: MathNode[] } | { kind: 'sqrt'; deg: MathNode[] | null; body: MathNode[] }

function parseMath(src: string): MathNode[] {
  let i = 0
  const group = (stop?: string): MathNode[] => {
    const out: MathNode[] = []
    while (i < src.length && src[i] !== stop) {
      const c = src[i]
      if (c === '{') {
        i++
        const g = group('}')
        i++
        out.push(...(g.length === 1 ? g : [{ kind: 'sub', base: g, script: [] } as MathNode]))
      } else if (c === '^' || c === '_') {
        i++
        const script = arg()
        const base = out.length ? [out.pop()!] : []
        out.push({ kind: c === '^' ? 'sup' : 'sub', base, script })
      } else if (c === '\\') {
        i++
        const name = /^[A-Za-z]*/.exec(src.slice(i))?.[0] ?? ''
        i += name.length
        if (name === 'frac') out.push({ kind: 'frac', num: arg(), den: arg() })
        else if (name === 'sqrt') {
          let deg: MathNode[] | null = null
          if (src[i] === '[') {
            i++
            deg = group(']')
            i++
          }
          out.push({ kind: 'sqrt', deg, body: arg() })
        } else out.push(MATH_SYMBOLS[name] ?? `\\${name}`)
      } else {
        out.push(c)
        i++
      }
    }
    return out
  }
  const arg = (): MathNode[] => {
    if (src[i] === '{') {
      i++
      const g = group('}')
      i++
      return g
    }
    if (src[i] === '\\') {
      const start = i
      i++
      const name = /^[A-Za-z]*/.exec(src.slice(i))?.[0] ?? ''
      i += name.length
      return MATH_SYMBOLS[name] ? [MATH_SYMBOLS[name]] : [src.slice(start, i)]
    }
    return i < src.length ? [src[i++]] : []
  }
  const nodes: MathNode[] = []
  while (i < src.length) {
    nodes.push(...group())
    if (i < src.length) nodes.push(src[i++])
  }
  return nodes
}

function renderMath(nodes: MathNode[]): ReactNode[] {
  return nodes.map((n, k) => {
    if (typeof n === 'string') return <span key={k} className={/[A-Za-z]/.test(n) ? 'mx-var' : undefined}>{n}</span>
    if (n.kind === 'frac')
      return (
        <span key={k} className="mx-frac">
          <span className="mx-num">{renderMath(n.num)}</span>
          <span className="mx-den">{renderMath(n.den)}</span>
        </span>
      )
    if (n.kind === 'sqrt')
      return (
        <span key={k} className="mx-sqrt">
          {n.deg && <sup className="mx-deg">{renderMath(n.deg)}</sup>}
          <span className="mx-radic">√</span>
          <span className="mx-radicand">{renderMath(n.body)}</span>
        </span>
      )
    const Tag = n.kind === 'sup' ? 'sup' : 'sub'
    return (
      <span key={k}>
        {renderMath(n.base)}
        {n.script.length > 0 && <Tag>{renderMath(n.script)}</Tag>}
      </span>
    )
  })
}

/** Renders the linear notation the backend reads and writes. */
export function MathView({ text }: { text: string }) {
  const nodes = useMemo(() => parseMath(text), [text])
  return <span className="mx">{renderMath(nodes)}</span>
}

const EQUATION_TEMPLATES: { label: ReactNode; insert: string; title: string }[] = [
  { label: <MathView text="x^{2}" />, insert: '^{}', title: 'Superscript' },
  { label: <MathView text="x_{i}" />, insert: '_{}', title: 'Subscript' },
  { label: <MathView text="\frac{a}{b}" />, insert: '\\frac{}{}', title: 'Fraction' },
  { label: <MathView text="\sqrt{x}" />, insert: '\\sqrt{}', title: 'Square root' },
  { label: <MathView text="\sqrt[n]{x}" />, insert: '\\sqrt[]{}', title: 'Nth root' },
  { label: '∑', insert: '\\sum ', title: 'Sum' },
  { label: '∫', insert: '\\int ', title: 'Integral' },
  { label: '±', insert: '\\pm ', title: 'Plus or minus' },
  { label: '×', insert: '\\times ', title: 'Times' },
  { label: '≤', insert: '\\le ', title: 'Less or equal' },
  { label: '≥', insert: '\\ge ', title: 'Greater or equal' },
  { label: '≠', insert: '\\ne ', title: 'Not equal' },
  { label: '∞', insert: '\\infty ', title: 'Infinity' },
  { label: 'π', insert: '\\pi ', title: 'Pi' },
  { label: 'α', insert: '\\alpha ', title: 'Alpha' },
  { label: 'Δ', insert: '\\Delta ', title: 'Delta' },
]

export function EquationDialog({
  initial,
  title,
  onSubmit,
  onClose,
}: {
  initial: string
  title: string
  onSubmit: (text: string) => void
  onClose: () => void
}) {
  const [text, setText] = useState(initial)
  const area = useRef<HTMLTextAreaElement | null>(null)
  useEffect(() => {
    area.current?.focus()
    area.current?.setSelectionRange(initial.length, initial.length)
  }, [initial])

  const insert = (snippet: string) => {
    const el = area.current
    if (!el) return
    const { selectionStart: s, selectionEnd: e } = el
    const next = text.slice(0, s) + snippet + text.slice(e)
    setText(next)
    // Place the caret inside the first empty group of the template.
    const hole = snippet.indexOf('{}')
    const caret = s + (hole >= 0 ? hole + 1 : snippet.length)
    requestAnimationFrame(() => {
      el.focus()
      el.setSelectionRange(caret, caret)
    })
  }

  return (
    <div className="rb-modal-backdrop" onMouseDown={e => e.target === e.currentTarget && onClose()}>
      <div className="rb-modal" role="dialog" aria-label={title}>
        <div className="rb-modal-head">
          <h3>{title}</h3>
          <button type="button" className="ws-close" onClick={onClose} aria-label="Close">
            <X size={15} strokeWidth={1.75} />
          </button>
        </div>
        <div className="rb-eq-templates">
          {EQUATION_TEMPLATES.map((t, i) => (
            <button key={i} type="button" className="rb-eq-template" title={t.title} onClick={() => insert(t.insert)}>
              {t.label}
            </button>
          ))}
        </div>
        <textarea
          ref={area}
          className="rb-eq-input"
          value={text}
          rows={3}
          spellCheck={false}
          placeholder="e.g.  A = \pi r^{2}   or   \frac{a+b}{2}"
          onChange={e => setText(e.target.value)}
          onKeyDown={e => {
            if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && text.trim()) onSubmit(text.trim())
          }}
        />
        <div className="rb-eq-preview">{text.trim() ? <MathView text={text} /> : <span className="ws-hint">Preview</span>}</div>
        <div className="rb-modal-foot">
          <span className="ws-hint">Saved as a real Word equation · Ctrl+Enter to insert</span>
          <button type="button" className="ws-tool" onClick={onClose}>
            Cancel
          </button>
          <button type="button" className="ws-tool ws-tool-primary" disabled={!text.trim()} onClick={() => onSubmit(text.trim())}>
            {initial ? 'Update' : 'Insert'}
          </button>
        </div>
      </div>
    </div>
  )
}

/** A one-field prompt in the ribbon's own style, replacing `window.prompt`. */
export function PromptDialog({
  title,
  label,
  initial,
  confirm,
  onSubmit,
  onClose,
}: {
  title: string
  label: string
  initial: string
  confirm: string
  onSubmit: (value: string) => void
  onClose: () => void
}) {
  const [value, setValue] = useState(initial)
  return (
    <div className="rb-modal-backdrop" onMouseDown={e => e.target === e.currentTarget && onClose()}>
      <form
        className="rb-modal rb-modal-small"
        onSubmit={e => {
          e.preventDefault()
          if (value.trim()) onSubmit(value.trim())
        }}
      >
        <div className="rb-modal-head">
          <h3>{title}</h3>
        </div>
        <label className="rb-field">
          <span>{label}</span>
          <input autoFocus value={value} onChange={e => setValue(e.target.value)} onFocus={e => e.currentTarget.select()} />
        </label>
        <div className="rb-modal-foot">
          <button type="button" className="ws-tool" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="ws-tool ws-tool-primary" disabled={!value.trim()}>
            {confirm}
          </button>
        </div>
      </form>
    </div>
  )
}
