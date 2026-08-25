/**
 * Word editor.
 *
 * # Why this is not built on a rich-text framework
 *
 * ProseMirror (and everything on top of it) owns its own document model. Using
 * one would mean maintaining a two-way mapping between its nodes and our OOXML
 * addresses, and that mapping breaks precisely where editing gets interesting:
 * splits, merges, paste. Our view model is ALREADY a list of paragraphs made of
 * runs — rendering it directly, one `contentEditable` paragraph containing one
 * `<span data-addr>` per run, keeps the address the single source of truth on
 * both sides.
 *
 * # The caret rule
 *
 * A `contentEditable` element must never be re-rendered by React while the user
 * is typing in it: React would replace the DOM node and the caret would jump to
 * the start. So paragraph content is written imperatively through a ref, and
 * only when that paragraph is NOT focused.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'

import type { DocxBlock, DocxStyle, DraftPatch } from '../../../api/drafts'

interface Props {
  blocks: DocxBlock[]
  styles: DocxStyle[]
  onPatch: (...patches: DraftPatch[]) => void
}

/** Blocks grouped for rendering: loose paragraphs and whole tables. */
type Row =
  | { kind: 'paragraph'; block: DocxBlock }
  | { kind: 'table'; table: number; rows: DocxBlock[][] }

/**
 * Group consecutive table-cell paragraphs back into tables.
 *
 * The backend addresses every paragraph in one flat sequence — deliberately, so
 * a cell needs no special patching — which means the grid has to be rebuilt
 * here from each block's `cell` reference.
 */
function groupRows(blocks: DocxBlock[]): Row[] {
  const out: Row[] = []
  let current: { table: number; rows: DocxBlock[][] } | null = null

  for (const block of blocks) {
    if (!block.cell) {
      if (current) {
        out.push({ kind: 'table', ...current })
        current = null
      }
      out.push({ kind: 'paragraph', block })
      continue
    }

    const { table, row, col } = block.cell
    if (!current || current.table !== table) {
      if (current) out.push({ kind: 'table', ...current })
      current = { table, rows: [] }
    }
    while (current.rows.length <= row) current.rows.push([])
    // Several paragraphs can share a cell; they stack inside it.
    const cells = current.rows[row]
    while (cells.length <= col) cells.push(null as unknown as DocxBlock)
    cells[col] = block
  }
  if (current) out.push({ kind: 'table', ...current })
  return out
}

function runHtml(block: DocxBlock): string {
  if (block.runs.length === 0) return ''
  return block.runs
    .map(run => {
      const style = [
        run.bold ? 'font-weight:600' : '',
        run.italic ? 'font-style:italic' : '',
        run.underline ? 'text-decoration:underline' : '',
      ]
        .filter(Boolean)
        .join(';')
      // textContent is set separately; escaping here keeps markup in the
      // document from becoming markup in the editor.
      const escaped = run.text
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
      return `<span data-addr="${run.addr}" style="${style}">${escaped || '​'}</span>`
    })
    .join('')
}

function Paragraph({
  block,
  styles,
  onPatch,
  onFocusBlock,
}: {
  block: DocxBlock
  styles: DocxStyle[]
  onPatch: (...patches: DraftPatch[]) => void
  onFocusBlock: (addr: string | null) => void
}) {
  const ref = useRef<HTMLDivElement | null>(null)
  const focused = useRef(false)
  // The text we last sent, per run, so an input event only emits patches for
  // runs that actually changed.
  const lastSent = useRef<Record<string, string>>({})

  useEffect(() => {
    lastSent.current = Object.fromEntries(block.runs.map(r => [r.addr, r.text]))
    if (!focused.current && ref.current) {
      ref.current.innerHTML = runHtml(block)
    }
  }, [block])

  const handleInput = useCallback(() => {
    const el = ref.current
    if (!el) return

    const patches: DraftPatch[] = []
    const seen = new Set<string>()

    el.querySelectorAll<HTMLElement>('[data-addr]').forEach(span => {
      const addr = span.dataset.addr
      if (!addr) return
      seen.add(addr)
      // The zero-width space is a placeholder for an empty run so the span
      // stays selectable; it is never part of the document's text.
      const text = (span.textContent ?? '').replace(/​/g, '')
      if (lastSent.current[addr] !== text) {
        lastSent.current[addr] = text
        patches.push({ op: 'SetRunText', addr, text })
      }
    })

    // A run whose span the browser removed (the user selected it all and
    // deleted) becomes empty rather than being silently forgotten.
    for (const run of block.runs) {
      if (!seen.has(run.addr) && lastSent.current[run.addr] !== '') {
        lastSent.current[run.addr] = ''
        patches.push({ op: 'SetRunText', addr: run.addr, text: '' })
      }
    }

    if (patches.length > 0) onPatch(...patches)
  }, [block.runs, onPatch])

  const styleInfo = useMemo(
    () => styles.find(s => s.id === block.style),
    [styles, block.style],
  )

  const isHeading = (block.style ?? '').toLowerCase().startsWith('heading')

  return (
    <div
      ref={ref}
      className={`ws-paragraph${isHeading ? ' ws-paragraph-heading' : ''}${block.num_id ? ' ws-paragraph-listed' : ''}`}
      contentEditable
      suppressContentEditableWarning
      spellCheck={false}
      data-block={block.addr}
      style={{
        fontWeight: styleInfo?.bold ? 600 : undefined,
        fontSize: styleInfo?.size_half_points
          ? `${styleInfo.size_half_points / 2}pt`
          : undefined,
        color: styleInfo?.color ? `#${styleInfo.color}` : undefined,
      }}
      onFocus={() => {
        focused.current = true
        onFocusBlock(block.addr)
      }}
      onBlur={() => {
        focused.current = false
        onFocusBlock(null)
      }}
      onInput={handleInput}
      // Paste as plain text: pasted HTML would bring spans with foreign
      // `data-addr` attributes into the paragraph and corrupt the mapping.
      onPaste={e => {
        e.preventDefault()
        const text = e.clipboardData.getData('text/plain')
        document.execCommand('insertText', false, text)
      }}
    />
  )
}

export default function DocxEditor({ blocks, styles, onPatch }: Props) {
  const [activeBlock, setActiveBlock] = useState<string | null>(null)
  const rows = useMemo(() => groupRows(blocks), [blocks])

  const activeRunAddr = useCallback(() => {
    const selection = window.getSelection()
    const node = selection?.anchorNode
    if (!node) return null
    const element = node.nodeType === Node.TEXT_NODE ? node.parentElement : (node as HTMLElement)
    return element?.closest<HTMLElement>('[data-addr]')?.dataset.addr ?? null
  }, [])

  const applyFormat = useCallback(
    (property: 'bold' | 'italic' | 'underline') => {
      const addr = activeRunAddr()
      if (!addr) return
      const run = blocks.flatMap(b => b.runs).find(r => r.addr === addr)
      if (!run) return
      onPatch({ op: 'SetRunFormat', addr, [property]: !run[property] } as DraftPatch)
    },
    [activeRunAddr, blocks, onPatch],
  )

  const applyStyle = useCallback(
    (style: string) => {
      if (!activeBlock) return
      onPatch({ op: 'SetParagraphStyle', addr: activeBlock, style: style || null })
    },
    [activeBlock, onPatch],
  )

  // Keyboard shortcuts are bound here rather than relying on the browser's
  // native contenteditable bold/italic, which would style the DOM without ever
  // producing a patch — the document would look changed and not be.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return
      const key = e.key.toLowerCase()
      if (key === 'b' || key === 'i' || key === 'u') {
        e.preventDefault()
        applyFormat(key === 'b' ? 'bold' : key === 'i' ? 'italic' : 'underline')
      }
    }
    document.addEventListener('keydown', onKeyDown)
    return () => document.removeEventListener('keydown', onKeyDown)
  }, [applyFormat])

  const namedStyles = useMemo(
    () => styles.filter(s => s.id !== 'Normal').slice(0, 12),
    [styles],
  )

  return (
    <div className="ws-docx">
      <div className="ws-toolbar">
        <button type="button" className="ws-tool" onClick={() => applyFormat('bold')} title="Bold (Ctrl+B)">
          <b>B</b>
        </button>
        <button type="button" className="ws-tool" onClick={() => applyFormat('italic')} title="Italic (Ctrl+I)">
          <i>I</i>
        </button>
        <button type="button" className="ws-tool" onClick={() => applyFormat('underline')} title="Underline (Ctrl+U)">
          <u>U</u>
        </button>
        <span className="ws-tool-divider" />
        <select
          className="ws-select"
          value={blocks.find(b => b.addr === activeBlock)?.style ?? ''}
          disabled={!activeBlock}
          onChange={e => applyStyle(e.target.value)}
          title="Paragraph style"
        >
          <option value="">Normal</option>
          {namedStyles.map(s => (
            <option key={s.id} value={s.id}>
              {s.name}
            </option>
          ))}
        </select>
        <span className="ws-tool-divider" />
        <button
          type="button"
          className="ws-tool"
          disabled={!activeBlock}
          onClick={() => activeBlock && onPatch({ op: 'InsertParagraphAfter', addr: activeBlock, text: '' })}
          title="Insert a paragraph below"
        >
          + ¶
        </button>
        <button
          type="button"
          className="ws-tool ws-tool-danger"
          disabled={!activeBlock}
          onClick={() => activeBlock && onPatch({ op: 'DeleteParagraph', addr: activeBlock })}
          title="Delete this paragraph"
        >
          − ¶
        </button>
      </div>

      <div className="ws-page">
        {rows.map((row, i) =>
          row.kind === 'paragraph' ? (
            <Paragraph
              key={row.block.addr}
              block={row.block}
              styles={styles}
              onPatch={onPatch}
              onFocusBlock={setActiveBlock}
            />
          ) : (
            <table className="ws-table" key={`table-${row.table}-${i}`}>
              <tbody>
                {row.rows.map((cells, r) => (
                  <tr key={r}>
                    {cells.map((block, c) => (
                      <td key={c}>
                        {block ? (
                          <Paragraph
                            block={block}
                            styles={styles}
                            onPatch={onPatch}
                            onFocusBlock={setActiveBlock}
                          />
                        ) : null}
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          ),
        )}
      </div>
    </div>
  )
}
