/**
 * Spreadsheet editor.
 *
 * A plain table rather than a virtualised grid component: the grid libraries
 * worth using are web components that fight React's controlled inputs, and the
 * bound on what the workspace opens (see `MAX_EDITABLE_BYTES`) keeps sheets in
 * a range a windowed table handles comfortably.
 *
 * # Formulas
 *
 * A cell showing a formula displays Excel's own cached result and shows the
 * expression in the formula bar. We do not evaluate it: writing a value we
 * computed ourselves next to a formula Excel will recompute differently is how
 * a spreadsheet ends up quietly lying about its own totals. On save the cached
 * value is dropped so Excel recalculates for real.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'

import type { DraftPatch, XlsxSheet } from '../../../api/drafts'

interface Props {
  sheets: XlsxSheet[]
  onPatch: (...patches: DraftPatch[]) => void
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

/** Always show a workable grid, with room past the last used cell. */
const MIN_ROWS = 24
const MIN_COLS = 8
const PADDING = 4

export default function XlsxEditor({ sheets, onPatch }: Props) {
  const [active, setActive] = useState(0)
  const [selected, setSelected] = useState('A1')
  const [editing, setEditing] = useState<string | null>(null)
  const [buffer, setBuffer] = useState('')
  const formulaBar = useRef<HTMLInputElement | null>(null)

  const sheet = sheets[Math.min(active, sheets.length - 1)]

  const dimensions = useMemo(() => {
    if (!sheet) return { rows: MIN_ROWS, cols: MIN_COLS }
    return {
      rows: Math.max(sheet.rows + PADDING, MIN_ROWS),
      cols: Math.max(sheet.cols + PADDING, MIN_COLS),
    }
  }, [sheet])

  const cellAt = useCallback(
    (reference: string) => sheet?.cells[reference] ?? null,
    [sheet],
  )

  /** What belongs in an editing box: the formula if there is one, else the value. */
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

  const commit = useCallback(
    (reference: string, value: string) => {
      if (!sheet) return
      const addr = `sheet[${sheet.index}]/${reference}`
      if (value === editableText(reference)) {
        setEditing(null)
        return
      }
      onPatch(
        value === ''
          ? { op: 'ClearCell', addr }
          : { op: 'SetCellValue', addr, value },
      )
      setEditing(null)
    },
    [sheet, editableText, onPatch],
  )

  const move = useCallback(
    (reference: string, dRow: number, dCol: number) => {
      const match = /^([A-Z]+)(\d+)$/.exec(reference)
      if (!match) return
      let col = 0
      for (const ch of match[1]) col = col * 26 + (ch.charCodeAt(0) - 64)
      col = col - 1 + dCol
      const row = Number(match[2]) + dRow
      if (col < 0 || row < 1) return
      setSelected(`${columnName(col)}${row}`)
    },
    [],
  )

  const onGridKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (editing !== null) return
      switch (e.key) {
        case 'ArrowUp':
          e.preventDefault()
          move(selected, -1, 0)
          break
        case 'ArrowDown':
        case 'Enter':
          e.preventDefault()
          move(selected, 1, 0)
          break
        case 'ArrowLeft':
          e.preventDefault()
          move(selected, 0, -1)
          break
        case 'ArrowRight':
        case 'Tab':
          e.preventDefault()
          move(selected, 0, 1)
          break
        case 'Delete':
        case 'Backspace':
          e.preventDefault()
          if (sheet && cellAt(selected)) {
            onPatch({ op: 'ClearCell', addr: `sheet[${sheet.index}]/${selected}` })
          }
          break
        case 'F2':
          e.preventDefault()
          setEditing(selected)
          setBuffer(editableText(selected))
          break
        default:
          // Typing a printable character starts an edit, replacing the cell —
          // the behaviour every spreadsheet has.
          if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
            setEditing(selected)
            setBuffer(e.key)
            e.preventDefault()
          }
      }
    },
    [editing, selected, move, sheet, cellAt, onPatch, editableText],
  )

  if (!sheet) {
    return <div className="ws-empty">This workbook has no sheets.</div>
  }

  const selectedCell = cellAt(selected)

  return (
    <div className="ws-xlsx">
      <div className="ws-toolbar">
        <span className="ws-cell-ref">{selected}</span>
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
              commit(selected, buffer)
              formulaBar.current?.blur()
            } else if (e.key === 'Escape') {
              setEditing(null)
              setBuffer(editableText(selected))
            }
          }}
          onBlur={() => editing !== null && commit(selected, buffer)}
        />
        {selectedCell?.formula && (
          <span className="ws-hint" title="Excel recalculates this when the file is opened">
            showing last calculated value
          </span>
        )}
      </div>

      {/* tabIndex makes the grid itself focusable, which is what lets arrow
          keys move the selection without a cell input being focused. */}
      <div className="ws-grid-scroll" tabIndex={0} onKeyDown={onGridKeyDown}>
        <table className="ws-grid">
          <thead>
            <tr>
              <th className="ws-grid-corner" />
              {Array.from({ length: dimensions.cols }, (_, c) => (
                <th key={c} className="ws-grid-head">
                  {columnName(c)}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {Array.from({ length: dimensions.rows }, (_, r) => {
              const rowNumber = r + 1
              return (
                <tr key={rowNumber}>
                  <th className="ws-grid-head">{rowNumber}</th>
                  {Array.from({ length: dimensions.cols }, (_, c) => {
                    const reference = `${columnName(c)}${rowNumber}`
                    const cell = cellAt(reference)
                    const isSelected = reference === selected
                    const isEditing = editing === reference

                    return (
                      <td
                        key={reference}
                        className={
                          'ws-grid-cell' +
                          (isSelected ? ' ws-grid-cell-selected' : '') +
                          (cell?.numeric ? ' ws-grid-cell-numeric' : '') +
                          (cell?.formula ? ' ws-grid-cell-formula' : '')
                        }
                        onClick={() => {
                          setSelected(reference)
                          setEditing(null)
                        }}
                        onDoubleClick={() => {
                          setSelected(reference)
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
                            onBlur={() => commit(reference, buffer)}
                            onKeyDown={e => {
                              if (e.key === 'Enter') {
                                commit(reference, buffer)
                                move(reference, 1, 0)
                              } else if (e.key === 'Escape') {
                                setEditing(null)
                              } else if (e.key === 'Tab') {
                                e.preventDefault()
                                commit(reference, buffer)
                                move(reference, 0, 1)
                              }
                            }}
                          />
                        ) : (
                          cell?.value ?? ''
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

      {sheets.length > 1 && (
        <div className="ws-sheet-tabs">
          {sheets.map(s => (
            <button
              key={s.index}
              type="button"
              className={`ws-sheet-tab${s.index === sheet.index ? ' ws-sheet-tab-active' : ''}`}
              onClick={() => {
                setActive(s.index)
                setSelected('A1')
                setEditing(null)
              }}
            >
              {s.name}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}
