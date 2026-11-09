/**
 * Spreadsheet editor.
 *
 * A plain table rather than a virtualised grid component: the grid libraries
 * worth using are web components that fight React's controlled inputs, and the
 * bound on what the workspace opens (see `MAX_EDITABLE_BYTES`) keeps sheets in
 * a range a table handles comfortably.
 *
 * # Formulas
 *
 * A cell showing a formula displays Excel's own cached result and shows the
 * expression in the formula bar. We do not evaluate it: writing a value we
 * computed ourselves next to a formula Excel will recompute differently is how
 * a spreadsheet ends up quietly lying about its own totals. On save the cached
 * value is dropped so Excel recalculates for real.
 *
 * # Ranges
 *
 * Click selects a cell, Shift+click or drag extends to a range, and every
 * formatting control applies to the whole range — one `SetCellFormat` per cell,
 * which the backend folds into shared styles.
 */

import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import {
  AlignCenter,
  AlignLeft,
  AlignRight,
  Baseline,
  Bold,
  CalendarDays,
  Clock,
  Eraser,
  Italic,
  Omega,
  PaintBucket,
  Sigma,
  Strikethrough,
  Underline,
  WrapText,
} from 'lucide-react'

import type { DraftPatch, TextFormat, XlsxCell, XlsxSheet } from '../../../api/drafts'
import { ColorPicker, FontPicker, Group, Popover, Ribbon, SizePicker, SymbolPicker, Tool } from './ribbon'

interface Props {
  sheets: XlsxSheet[]
  onPatch: (...patches: DraftPatch[]) => void
  onCommit: (...patches: DraftPatch[]) => Promise<void>
  /** Reported so the side column can inspect the cell under the cursor. */
  onSelectionChange?: (sheet: number, ref: string) => void
  /** A value to write into the selected cell. */
  insert?: { text: string; nonce: number } | null
  onError?: (message: string) => void
}

/** Column index to spreadsheet letters: 0 → A, 26 → AA. */
function columnName(index: number): string {
  let name = ''
  let n = index + 1
  while (n > 0) {
    const remainder = (n - 1) % 26
    name = String.fromCharCode(65 + remainder) + name
    n = Math.floor((n - 1) / 26)
  }
  return name
}

function parseRef(reference: string): { col: number; row: number } | null {
  const m = /^([A-Z]+)(\d+)$/.exec(reference)
  if (!m) return null
  let col = 0
  for (const ch of m[1]) col = col * 26 + (ch.charCodeAt(0) - 64)
  return { col: col - 1, row: Number(m[2]) }
}

const MIN_ROWS = 40
const MIN_COLS = 12
const PADDING = 6

const NUMBER_FORMATS: { label: string; code: string; sample: string }[] = [
  { label: 'General', code: 'General', sample: '1234.5' },
  { label: 'Number', code: '#,##0.00', sample: '1,234.50' },
  { label: 'Whole number', code: '#,##0', sample: '1,235' },
  { label: 'Currency', code: '$#,##0.00', sample: '$1,234.50' },
  { label: 'Euro', code: '€#,##0.00', sample: '€1,234.50' },
  { label: 'Percentage', code: '0%', sample: '12%' },
  { label: 'Percentage (2 dp)', code: '0.00%', sample: '12.35%' },
  { label: 'Date', code: 'm/d/yyyy', sample: '3/14/2026' },
  { label: 'Long date', code: 'd mmmm yyyy', sample: '14 March 2026' },
  { label: 'Text', code: '@', sample: 'as typed' },
]

const FUNCTIONS: { name: string; template: string; hint: string }[] = [
  { name: 'SUM', template: '=SUM()', hint: 'Adds numbers' },
  { name: 'AVERAGE', template: '=AVERAGE()', hint: 'Mean of numbers' },
  { name: 'COUNT', template: '=COUNT()', hint: 'Counts numbers' },
  { name: 'MIN', template: '=MIN()', hint: 'Smallest value' },
  { name: 'MAX', template: '=MAX()', hint: 'Largest value' },
  { name: 'ROUND', template: '=ROUND(, 2)', hint: 'Rounds to digits' },
  { name: 'IF', template: '=IF(, , )', hint: 'Test a condition' },
  { name: 'SUMIF', template: '=SUMIF(, , )', hint: 'Adds where a test holds' },
  { name: 'VLOOKUP', template: '=VLOOKUP(, , , FALSE)', hint: 'Finds a row' },
  { name: 'CONCAT', template: '=CONCAT(, )', hint: 'Joins text' },
  { name: 'TODAY', template: '=TODAY()', hint: "Today's date" },
  { name: 'DATEDIF', template: '=DATEDIF(, , "d")', hint: 'Days between dates' },
]

/** Excel date serial to a JS date (the 1900 system). */
function serialToDate(serial: number): Date {
  return new Date(Math.round((serial - 25569) * 86400 * 1000))
}

/** Display a numeric value the way its number format would. */
function formatValue(cell: XlsxCell): string {
  const code = cell.format?.num_fmt
  if (!code || !cell.numeric || code === 'General' || code === '@') return cell.value
  const n = Number(cell.value)
  if (!Number.isFinite(n)) return cell.value
  if (/[dmy]/i.test(code) && !/[#0]/.test(code)) {
    const d = serialToDate(n)
    if (/mmmm/.test(code)) return d.toLocaleDateString(undefined, { day: 'numeric', month: 'long', year: 'numeric', timeZone: 'UTC' })
    return d.toLocaleDateString(undefined, { timeZone: 'UTC' })
  }
  const percent = code.includes('%')
  const decimals = (/0\.(0+)/.exec(code)?.[1].length) ?? 0
  const grouped = code.includes(',')
  const value = percent ? n * 100 : n
  let out = value.toLocaleString(undefined, { minimumFractionDigits: decimals, maximumFractionDigits: decimals, useGrouping: grouped })
  const prefix = /^[^#0]*/.exec(code)?.[0].replace(/["\\]/g, '') ?? ''
  if (percent) out += '%'
  return `${prefix}${out}`
}

function cellCss(cell: XlsxCell | null): CSSProperties | undefined {
  const f = cell?.format
  if (!f) return undefined
  return {
    fontWeight: f.bold ? 700 : undefined,
    fontStyle: f.italic ? 'italic' : undefined,
    textDecoration: [f.underline ? 'underline' : '', f.strike ? 'line-through' : ''].filter(Boolean).join(' ') || undefined,
    fontFamily: f.font ? `"${f.font}", var(--font-sans)` : undefined,
    fontSize: f.size ? `${f.size}pt` : undefined,
    color: f.color ? `#${f.color}` : undefined,
    background: f.fill ? `#${f.fill}` : undefined,
    textAlign: (f.align as CSSProperties['textAlign']) ?? undefined,
    whiteSpace: f.wrap ? 'normal' : undefined,
  }
}

interface Range {
  anchor: string
  focus: string
}

function rangeCells(range: Range): string[] {
  const a = parseRef(range.anchor)
  const b = parseRef(range.focus)
  if (!a || !b) return [range.anchor]
  const out: string[] = []
  for (let r = Math.min(a.row, b.row); r <= Math.max(a.row, b.row); r++) {
    for (let c = Math.min(a.col, b.col); c <= Math.max(a.col, b.col); c++) out.push(`${columnName(c)}${r}`)
  }
  return out
}

function rangeLabel(range: Range): string {
  return range.anchor === range.focus ? range.anchor : `${range.anchor}:${range.focus}`
}

export default function XlsxEditor({ sheets, onPatch, onCommit, onSelectionChange, insert, onError }: Props) {
  const [active, setActive] = useState(0)
  const [range, setRange] = useState<Range>({ anchor: 'A1', focus: 'A1' })
  const [editing, setEditing] = useState<string | null>(null)
  const [buffer, setBuffer] = useState('')
  const [tab, setTab] = useState('home')
  const [dragging, setDragging] = useState(false)
  const formulaBar = useRef<HTMLInputElement | null>(null)
  const grid = useRef<HTMLDivElement | null>(null)

  const sheet = sheets[Math.min(active, sheets.length - 1)]
  const selected = range.focus

  useEffect(() => {
    if (sheet) onSelectionChange?.(sheet.index, selected)
  }, [sheet, selected, onSelectionChange])

  const dimensions = useMemo(() => {
    if (!sheet) return { rows: MIN_ROWS, cols: MIN_COLS }
    return { rows: Math.max(sheet.rows + PADDING, MIN_ROWS), cols: Math.max(sheet.cols + PADDING, MIN_COLS) }
  }, [sheet])

  const widths = useMemo(() => {
    const out: Record<number, number> = {}
    for (const c of sheet?.columns ?? []) {
      for (let i = c.min; i <= Math.min(c.max, 200); i++) out[i - 1] = Math.round(c.width * 7 + 5)
    }
    return out
  }, [sheet])

  const cellAt = useCallback((reference: string) => sheet?.cells[reference] ?? null, [sheet])

  const editableText = useCallback(
    (reference: string) => {
      const cell = cellAt(reference)
      if (!cell) return ''
      return cell.formula ? `=${cell.formula}` : cell.value
    },
    [cellAt],
  )

  useEffect(() => {
    if (editing === null) setBuffer(editableText(selected))
  }, [selected, editing, editableText])

  const lastInsert = useRef<number | null>(null)
  useEffect(() => {
    if (!insert || !sheet || insert.nonce === lastInsert.current) return
    lastInsert.current = insert.nonce
    onPatch({ op: 'SetCellValue', addr: `sheet[${sheet.index}]/${selected}`, value: insert.text })
  }, [insert, sheet, selected, onPatch])

  const commitValue = useCallback(
    (reference: string, value: string) => {
      if (!sheet) return
      const addr = `sheet[${sheet.index}]/${reference}`
      setEditing(null)
      if (value === editableText(reference)) return
      onPatch(value === '' ? { op: 'ClearCell', addr } : { op: 'SetCellValue', addr, value })
    },
    [sheet, editableText, onPatch],
  )

  const select = useCallback((reference: string, extend: boolean) => {
    setRange(r => (extend ? { anchor: r.anchor, focus: reference } : { anchor: reference, focus: reference }))
  }, [])

  const move = useCallback(
    (reference: string, dRow: number, dCol: number, extend = false) => {
      const at = parseRef(reference)
      if (!at) return
      const col = at.col + dCol
      const row = at.row + dRow
      if (col < 0 || row < 1) return
      select(`${columnName(col)}${row}`, extend)
    },
    [select],
  )

  const cells = useMemo(() => rangeCells(range), [range])
  const inRange = useMemo(() => new Set(cells), [cells])

  const applyFormat = useCallback(
    (fields: TextFormat & { fill?: string; align?: 'left' | 'center' | 'right' | 'general'; wrap?: boolean; num_fmt?: string }) => {
      if (!sheet) return
      if (cells.length > 2000) {
        onError?.('Select fewer than 2,000 cells to format at once.')
        return
      }
      void onCommit(...cells.map(ref => ({ op: 'SetCellFormat', addr: `sheet[${sheet.index}]/${ref}`, ...fields }) as DraftPatch)).catch(e =>
        onError?.(e instanceof Error ? e.message : String(e)),
      )
    },
    [cells, onCommit, onError, sheet],
  )

  const clearRange = useCallback(() => {
    if (!sheet) return
    const patches = cells.filter(ref => cellAt(ref)).map(ref => ({ op: 'ClearCell', addr: `sheet[${sheet.index}]/${ref}` }) as DraftPatch)
    if (patches.length) onPatch(...patches)
  }, [cells, cellAt, onPatch, sheet])

  /** Σ: sum the contiguous numbers above the cell (or to its left). */
  const autoSum = useCallback(() => {
    const at = parseRef(selected)
    if (!at || !sheet) return
    const numeric = (col: number, row: number) => sheet.cells[`${columnName(col)}${row}`]?.numeric
    let top = at.row - 1
    while (top >= 1 && numeric(at.col, top)) top--
    if (top < at.row - 1) {
      commitValue(selected, `=SUM(${columnName(at.col)}${top + 1}:${columnName(at.col)}${at.row - 1})`)
      return
    }
    let left = at.col - 1
    while (left >= 0 && numeric(left, at.row)) left--
    if (left < at.col - 1) {
      commitValue(selected, `=SUM(${columnName(left + 1)}${at.row}:${columnName(at.col - 1)}${at.row})`)
      return
    }
    setEditing(selected)
    setBuffer('=SUM()')
    requestAnimationFrame(() => {
      formulaBar.current?.focus()
      formulaBar.current?.setSelectionRange(5, 5)
    })
  }, [selected, sheet, commitValue])

  const startFormula = (template: string) => {
    setEditing(selected)
    const withRange = range.anchor !== range.focus ? template.replace('()', `(${rangeLabel(range)})`) : template
    setBuffer(withRange)
    requestAnimationFrame(() => {
      const el = formulaBar.current
      if (!el) return
      el.focus()
      const at = withRange.indexOf('(') + 1
      el.setSelectionRange(at, withRange.includes('()') ? at : withRange.length - 1)
    })
  }

  const onGridKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (editing !== null) return
      const mod = e.ctrlKey || e.metaKey
      if (mod && ['b', 'i', 'u'].includes(e.key.toLowerCase())) {
        e.preventDefault()
        const f = cellAt(selected)?.format
        const key = e.key.toLowerCase() === 'b' ? 'bold' : e.key.toLowerCase() === 'i' ? 'italic' : 'underline'
        applyFormat({ [key]: !f?.[key] })
        return
      }
      switch (e.key) {
        case 'ArrowUp':
          e.preventDefault()
          move(selected, -1, 0, e.shiftKey)
          break
        case 'ArrowDown':
          e.preventDefault()
          move(selected, 1, 0, e.shiftKey)
          break
        case 'Enter':
          e.preventDefault()
          move(selected, e.shiftKey ? -1 : 1, 0)
          break
        case 'ArrowLeft':
          e.preventDefault()
          move(selected, 0, -1, e.shiftKey)
          break
        case 'ArrowRight':
          e.preventDefault()
          move(selected, 0, 1, e.shiftKey)
          break
        case 'Tab':
          e.preventDefault()
          move(selected, 0, e.shiftKey ? -1 : 1)
          break
        case 'Delete':
        case 'Backspace':
          e.preventDefault()
          clearRange()
          break
        case 'F2':
          e.preventDefault()
          setEditing(selected)
          setBuffer(editableText(selected))
          break
        default:
          // Typing a printable character starts an edit, replacing the cell —
          // the behaviour every spreadsheet has.
          if (e.key.length === 1 && !mod && !e.altKey) {
            setEditing(selected)
            setBuffer(e.key)
            e.preventDefault()
          }
      }
    },
    [editing, selected, move, clearRange, editableText, cellAt, applyFormat],
  )

  useEffect(() => {
    if (!dragging) return
    const up = () => setDragging(false)
    window.addEventListener('mouseup', up)
    return () => window.removeEventListener('mouseup', up)
  }, [dragging])

  if (!sheet) {
    return <div className="ws-empty">This workbook has no sheets.</div>
  }

  const selectedCell = cellAt(selected)
  const f = selectedCell?.format ?? null
  const numbers = cells.map(ref => sheet.cells[ref]).filter((c): c is XlsxCell => !!c?.numeric).map(c => Number(c.value))
  const sum = numbers.reduce((a, b) => a + b, 0)

  return (
    <div className="ws-xlsx xl">
      <Ribbon
        tabs={[
          { id: 'home', label: 'Home' },
          { id: 'formulas', label: 'Formulas' },
          { id: 'insert', label: 'Insert' },
        ]}
        active={tab}
        onTab={setTab}
        aside={<span className="rb-hint">{rangeLabel(range)}</span>}
      >
        {tab === 'home' && (
          <>
            <Group label="Font">
              <FontPicker value={f?.font ?? 'Calibri'} onPick={font => applyFormat({ font })} />
              <SizePicker value={f?.size ?? 11} onPick={size => applyFormat({ size })} />
            </Group>
            <Group label="Character">
              <Tool icon={<Bold size={15} strokeWidth={2.25} />} title="Bold (Ctrl+B)" active={!!f?.bold} onClick={() => applyFormat({ bold: !f?.bold })} />
              <Tool icon={<Italic size={15} />} title="Italic (Ctrl+I)" active={!!f?.italic} onClick={() => applyFormat({ italic: !f?.italic })} />
              <Tool icon={<Underline size={15} />} title="Underline (Ctrl+U)" active={!!f?.underline} onClick={() => applyFormat({ underline: !f?.underline })} />
              <Tool icon={<Strikethrough size={15} />} title="Strikethrough" active={!!f?.strike} onClick={() => applyFormat({ strike: !f?.strike })} />
              <ColorPicker icon={<Baseline size={15} />} title="Font colour" value={f?.color ?? null} onPick={color => applyFormat({ color })} />
              <ColorPicker icon={<PaintBucket size={15} />} title="Fill colour" value={f?.fill ?? null} clearLabel="No fill" onPick={fill => applyFormat({ fill })} />
            </Group>
            <Group label="Alignment">
              <Tool icon={<AlignLeft size={15} />} title="Align left" active={f?.align === 'left'} onClick={() => applyFormat({ align: 'left' })} />
              <Tool icon={<AlignCenter size={15} />} title="Centre" active={f?.align === 'center'} onClick={() => applyFormat({ align: 'center' })} />
              <Tool icon={<AlignRight size={15} />} title="Align right" active={f?.align === 'right'} onClick={() => applyFormat({ align: 'right' })} />
              <Tool icon={<WrapText size={15} />} title="Wrap text" active={!!f?.wrap} onClick={() => applyFormat({ wrap: !f?.wrap })} />
            </Group>
            <Group label="Number">
              <Popover title="Number format" trigger={<span className="rb-numfmt">{NUMBER_FORMATS.find(n => n.code === (f?.num_fmt ?? 'General'))?.label ?? f?.num_fmt ?? 'General'}</span>}>
                {close => (
                  <div className="rb-menu rb-menu-wide">
                    {NUMBER_FORMATS.map(n => (
                      <button
                        key={n.code}
                        type="button"
                        className={`rb-list-item rb-list-split${(f?.num_fmt ?? 'General') === n.code ? ' is-active' : ''}`}
                        onClick={() => {
                          applyFormat({ num_fmt: n.code })
                          close()
                        }}
                      >
                        <span>{n.label}</span>
                        <span className="rb-list-meta">{n.sample}</span>
                      </button>
                    ))}
                  </div>
                )}
              </Popover>
              <Tool icon={<span className="rb-glyph">$</span>} title="Currency" onClick={() => applyFormat({ num_fmt: '$#,##0.00' })} />
              <Tool icon={<span className="rb-glyph">%</span>} title="Percentage" onClick={() => applyFormat({ num_fmt: '0%' })} />
              <Tool icon={<span className="rb-glyph">,</span>} title="Thousands separator" onClick={() => applyFormat({ num_fmt: '#,##0.00' })} />
            </Group>
            <Group label="Editing">
              <Tool icon={<Sigma size={15} />} label="AutoSum" title="Sum the numbers above or to the left" onClick={autoSum} />
              <Tool
                icon={<Eraser size={15} />}
                title="Clear formatting"
                onClick={() => applyFormat({ bold: false, italic: false, underline: false, strike: false, color: '', fill: '', align: 'general', wrap: false, num_fmt: 'General' })}
              />
            </Group>
          </>
        )}

        {tab === 'formulas' && (
          <>
            <Group label="AutoSum">
              <Tool icon={<Sigma size={15} />} label="AutoSum" title="Sum the numbers above or to the left" onClick={autoSum} />
            </Group>
            <Group label="Functions">
              {FUNCTIONS.map(fn => (
                <Tool key={fn.name} wide label={fn.name} title={`${fn.name}: ${fn.hint}`} onClick={() => startFormula(fn.template)} />
              ))}
            </Group>
          </>
        )}

        {tab === 'insert' && (
          <>
            <Group label="Values">
              <Tool icon={<CalendarDays size={15} />} label="Date" title="Today's date" onClick={() => commitValue(selected, new Date().toLocaleDateString())} />
              <Tool icon={<Clock size={15} />} label="Time" title="The current time" onClick={() => commitValue(selected, new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }))} />
              <SymbolPicker trigger={<><Omega size={15} /><span>Symbol</span></>} onPick={s => commitValue(selected, `${editableText(selected)}${s}`)} />
            </Group>
          </>
        )}
      </Ribbon>

      <div className="xl-card">
        <div className="ws-toolbar dr-sheetbar">
          <span className="ws-cell-ref">{rangeLabel(range)}</span>
          <span className="dr-fx">fx</span>
          <input
            ref={formulaBar}
            className="ws-formula-bar"
            value={editing === null ? editableText(selected) : buffer}
            placeholder="Value or =FORMULA()"
            onChange={e => {
              setEditing(selected)
              setBuffer(e.target.value)
            }}
            onKeyDown={e => {
              if (e.key === 'Enter') {
                commitValue(selected, buffer)
                grid.current?.focus()
                move(selected, 1, 0)
              } else if (e.key === 'Escape') {
                setEditing(null)
                setBuffer(editableText(selected))
                grid.current?.focus()
              }
            }}
            onBlur={() => editing !== null && commitValue(selected, buffer)}
          />
          {selectedCell?.formula && (
            <span className="ws-hint" title="Excel recalculates this when the file is opened">
              last calculated value
            </span>
          )}
        </div>

        {/* tabIndex makes the grid itself focusable, which is what lets arrow
            keys move the selection without a cell input being focused. */}
        <div className="ws-grid-scroll" tabIndex={0} ref={grid} onKeyDown={onGridKeyDown}>
          <table className="ws-grid">
            <colgroup>
              <col style={{ width: 44 }} />
              {Array.from({ length: dimensions.cols }, (_, c) => (
                <col key={c} style={{ width: widths[c] ?? 96 }} />
              ))}
            </colgroup>
            <thead>
              <tr>
                <th className="ws-grid-corner" />
                {Array.from({ length: dimensions.cols }, (_, c) => {
                  const name = columnName(c)
                  const on = cells.some(r => r.startsWith(name) && /^\d/.test(r.slice(name.length)))
                  return (
                    <th
                      key={c}
                      className={`ws-grid-head${on ? ' is-on' : ''}`}
                      onMouseDown={e => {
                        e.preventDefault()
                        setRange({ anchor: `${name}1`, focus: `${name}${dimensions.rows}` })
                        grid.current?.focus()
                      }}
                    >
                      {name}
                    </th>
                  )
                })}
              </tr>
            </thead>
            <tbody>
              {Array.from({ length: dimensions.rows }, (_, r) => {
                const rowNumber = r + 1
                const rowOn = cells.some(ref => parseRef(ref)?.row === rowNumber)
                return (
                  <tr key={rowNumber}>
                    <th
                      className={`ws-grid-head${rowOn ? ' is-on' : ''}`}
                      onMouseDown={e => {
                        e.preventDefault()
                        setRange({ anchor: `A${rowNumber}`, focus: `${columnName(dimensions.cols - 1)}${rowNumber}` })
                        grid.current?.focus()
                      }}
                    >
                      {rowNumber}
                    </th>
                    {Array.from({ length: dimensions.cols }, (_, c) => {
                      const reference = `${columnName(c)}${rowNumber}`
                      const cell = cellAt(reference)
                      const isEditing = editing === reference
                      return (
                        <td
                          key={reference}
                          className={
                            'ws-grid-cell' +
                            (reference === selected ? ' ws-grid-cell-selected' : '') +
                            (inRange.has(reference) && cells.length > 1 ? ' is-in-range' : '') +
                            (cell?.numeric ? ' ws-grid-cell-numeric' : '') +
                            (cell?.formula ? ' ws-grid-cell-formula' : '')
                          }
                          style={cellCss(cell)}
                          onMouseDown={e => {
                            if (isEditing) return
                            e.preventDefault()
                            if (editing !== null) commitValue(editing, buffer)
                            select(reference, e.shiftKey)
                            setDragging(true)
                            grid.current?.focus()
                          }}
                          onMouseEnter={() => dragging && select(reference, true)}
                          onDoubleClick={() => {
                            select(reference, false)
                            setEditing(reference)
                            setBuffer(editableText(reference))
                          }}
                          title={cell?.formula ? `=${cell.formula}` : undefined}
                        >
                          {isEditing ? (
                            <input
                              className="ws-grid-input"
                              autoFocus
                              value={buffer}
                              onChange={e => setBuffer(e.target.value)}
                              onBlur={() => commitValue(reference, buffer)}
                              onKeyDown={e => {
                                if (e.key === 'Enter') {
                                  commitValue(reference, buffer)
                                  move(reference, 1, 0)
                                  grid.current?.focus()
                                } else if (e.key === 'Escape') {
                                  setEditing(null)
                                  grid.current?.focus()
                                } else if (e.key === 'Tab') {
                                  e.preventDefault()
                                  commitValue(reference, buffer)
                                  move(reference, 0, 1)
                                  grid.current?.focus()
                                }
                              }}
                            />
                          ) : cell ? (
                            formatValue(cell)
                          ) : (
                            ''
                          )}
                        </td>
                      )
                    })}
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>

        <div className="ws-sheet-tabs">
          {sheets.map(s => (
            <button
              key={s.index}
              type="button"
              className={`ws-sheet-tab${s.index === sheet.index ? ' ws-sheet-tab-active' : ''}`}
              onClick={() => {
                setActive(s.index)
                setRange({ anchor: 'A1', focus: 'A1' })
                setEditing(null)
              }}
            >
              {s.name}
            </button>
          ))}
          {numbers.length > 1 && (
            <span className="xl-status">
              Average {(sum / numbers.length).toLocaleString(undefined, { maximumFractionDigits: 2 })} · Count {numbers.length} · Sum{' '}
              {sum.toLocaleString(undefined, { maximumFractionDigits: 2 })}
            </span>
          )}
        </div>
      </div>
    </div>
  )
}
