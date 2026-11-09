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
 * # Two kinds of edit
 *
 * Typing is a text edit: it becomes `SetRunText` and rides the debounced queue.
 * Everything else — Enter, Backspace across paragraphs, formatting a selection,
 * inserting a table — moves addresses, so it goes through `onCommit`, which
 * sends it alone and resolves once the new model is in. The caret is then put
 * back by character offset, which survives the runs being re-split.
 *
 * # The caret rule
 *
 * A focused `contentEditable` is only re-rendered when doing so cannot lose
 * typing: when the model's text for it matches what is on screen (formatting
 * changed, text did not), or after a commit the editor itself made.
 */

import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import {
  AlignCenter,
  AlignJustify,
  AlignLeft,
  AlignRight,
  ArrowDownToLine,
  ArrowLeftToLine,
  ArrowRightToLine,
  ArrowUpToLine,
  Baseline,
  Bold,
  CalendarDays,
  Eraser,
  Highlighter,
  ImagePlus,
  IndentDecrease,
  IndentIncrease,
  Italic,
  List,
  ListOrdered,
  Minus,
  Omega,
  Pilcrow,
  Plus,
  Radical,
  Rows3,
  SeparatorHorizontal,
  Strikethrough,
  Subscript,
  Superscript,
  Table2,
  Trash2,
  Type,
  Underline,
} from 'lucide-react'

import type { DocxBlock, DocxDefaults, DocxRun, DocxStyle, DraftPatch, TextFormat } from '../../../api/drafts'
import {
  ColorPicker,
  EquationDialog,
  FontPicker,
  Group,
  HighlightPicker,
  MathView,
  Popover,
  Ribbon,
  SizePicker,
  SymbolPicker,
  TableGridPicker,
  Tool,
} from './ribbon'
import { HIGHLIGHT_HEX, pickImage, readImage, EMU_PER_PX } from './ribbonData'

interface Props {
  blocks: DocxBlock[]
  styles: DocxStyle[]
  defaults?: DocxDefaults
  onPatch: (...patches: DraftPatch[]) => void
  onCommit: (...patches: DraftPatch[]) => Promise<void>
  /** Text from the side column, to insert at the caret. */
  insert?: { text: string; nonce: number } | null
  onError?: (message: string) => void
}

const ZWSP = '\u200b'

// ------------------------------------------------------------------ caret math

/** Text nodes of a paragraph that count toward offsets (not pictures/maths). */
function textNodes(root: HTMLElement): Text[] {
  const out: Text[] = []
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
    acceptNode: node => {
      const locked = node.parentElement?.closest('[contenteditable="false"]')
      return locked && root.contains(locked) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT
    },
  })
  for (let n = walker.nextNode(); n; n = walker.nextNode()) out.push(n as Text)
  return out
}

function visibleLength(text: string, upTo = text.length): number {
  let n = 0
  for (let i = 0; i < upTo; i++) if (text[i] !== ZWSP) n++
  return n
}

/** Character offsets of the selection within `root`, ZWSP excluded. */
function selectionOffsets(root: HTMLElement): { start: number; end: number } | null {
  const sel = window.getSelection()
  if (!sel || sel.rangeCount === 0) return null
  const range = sel.getRangeAt(0)
  if (!root.contains(range.startContainer) || !root.contains(range.endContainer)) return null
  // Count every text node that lies before the point: the prefix range from
  // the start of the paragraph to the point intersects exactly those.
  const measure = (container: Node, offset: number) => {
    const prefix = document.createRange()
    prefix.selectNodeContents(root)
    prefix.setEnd(container, offset)
    let total = 0
    for (const t of textNodes(root)) {
      if (t === container) return total + visibleLength(t.data, offset)
      if (!prefix.intersectsNode(t)) break
      total += visibleLength(t.data)
    }
    return total
  }
  const start = measure(range.startContainer, range.startOffset)
  const end = measure(range.endContainer, range.endOffset)
  return { start: Math.min(start, end), end: Math.max(start, end) }
}

function placeCaret(root: HTMLElement, start: number, end = start) {
  const nodes = textNodes(root)
  const locate = (target: number): [Node, number] => {
    let total = 0
    for (const t of nodes) {
      const len = visibleLength(t.data)
      if (target <= total + len) {
        // Map the visible offset back to a raw offset, skipping ZWSPs.
        let seen = 0
        for (let i = 0; i <= t.data.length; i++) {
          if (seen === target - total) {
            // Step past a leading zero-width space so typing lands in the run.
            while (i < t.data.length && t.data[i] === ZWSP) i++
            return [t, i]
          }
          if (t.data[i] !== ZWSP) seen++
        }
        return [t, t.data.length]
      }
      total += len
    }
    const last = nodes[nodes.length - 1]
    return last ? [last, last.data.length] : [root, root.childNodes.length]
  }
  const sel = window.getSelection()
  if (!sel) return
  const range = document.createRange()
  const [sn, so] = locate(start)
  const [en, eo] = locate(end)
  range.setStart(sn, so)
  range.setEnd(en, eo)
  sel.removeAllRanges()
  sel.addRange(range)
}

function blockText(block: DocxBlock): string {
  return block.runs.map(r => r.text).join('')
}

// ------------------------------------------------------------------ rendering

const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
const escAttr = (s: string) => esc(s).replace(/"/g, '&quot;')

function runStyle(run: DocxRun): string {
  const css: string[] = []
  if (run.bold) css.push('font-weight:700')
  if (run.italic) css.push('font-style:italic')
  const deco = [run.underline ? 'underline' : '', run.strike ? 'line-through' : ''].filter(Boolean).join(' ')
  if (deco) css.push(`text-decoration:${deco}`)
  if (run.font) css.push(`font-family:"${run.font.replace(/"/g, '')}",var(--dx-font)`)
  if (run.size) css.push(`font-size:${run.size}pt`)
  if (run.color) css.push(`color:#${run.color}`)
  if (run.highlight && HIGHLIGHT_HEX[run.highlight]) css.push(`background:#${HIGHLIGHT_HEX[run.highlight]}`)
  if (run.vert_align === 'superscript') css.push('vertical-align:super;font-size:0.7em')
  if (run.vert_align === 'subscript') css.push('vertical-align:sub;font-size:0.7em')
  return css.join(';')
}

function runHtml(block: DocxBlock): string {
  if (block.runs.length === 0) return ''
  return block.runs
    .map(run => {
      if (run.image) {
        const w = Math.round(run.image.width_emu / EMU_PER_PX)
        return `<span data-addr="${run.addr}" class="dx-img" contenteditable="false"><img src="${escAttr(run.image.data_url)}" style="width:${w}px" draggable="false" alt=""/></span>`
      }
      const cls = run.math ? ' class="dx-math"' : ''
      return `<span data-addr="${run.addr}"${cls} style="${runStyle(run)}">${esc(run.text) || ZWSP}</span>`
    })
    .join('')
}

function prettyStyleName(style: DocxStyle): string {
  const n = style.name.replace(/^heading (\d)$/i, 'Heading $1')
  return n.charAt(0).toUpperCase() + n.slice(1)
}

// ---------------------------------------------------------------- paragraph

interface ParagraphProps {
  block: DocxBlock
  index: number
  marker: string | null
  styleInfo: DocxStyle | undefined
  epoch: number
  onPatch: (...patches: DraftPatch[]) => void
  onKey: (e: React.KeyboardEvent<HTMLDivElement>, index: number, el: HTMLDivElement) => void
  onEmptyInput: (index: number, text: string) => void
}

function Paragraph({ block, index, marker, styleInfo, epoch, onPatch, onKey, onEmptyInput }: ParagraphProps) {
  const ref = useRef<HTMLDivElement | null>(null)
  const composing = useRef(false)
  const lastSent = useRef<Record<string, string>>({})
  const renderedEpoch = useRef(-1)

  useEffect(() => {
    const el = ref.current
    if (!el) return
    lastSent.current = Object.fromEntries(block.runs.map(r => [r.addr, r.text]))
    const html = runHtml(block)
    const focused = document.activeElement === el
    const domText = textNodes(el).map(t => t.data).join('').replace(/\u200b/g, '')
    const safe = !focused || epoch !== renderedEpoch.current || domText === blockText(block)
    if (!safe || composing.current) return
    renderedEpoch.current = epoch
    if (el.innerHTML === html) return
    const offsets = focused ? selectionOffsets(el) : null
    el.innerHTML = html
    if (focused && offsets) placeCaret(el, offsets.start, offsets.end)
  }, [block, epoch])

  const handleInput = useCallback(() => {
    const el = ref.current
    if (!el || composing.current) return

    const spans = Array.from(el.querySelectorAll<HTMLElement>('[data-addr]')).filter(s => !s.classList.contains('dx-img'))
    if (spans.length === 0) {
      // Typing into a paragraph that had no runs at all.
      const text = (el.textContent ?? '').replace(/\u200b/g, '')
      if (text) onEmptyInput(index, text)
      return
    }

    // Text the browser put OUTSIDE any run (at a paragraph edge) belongs to the
    // nearest run; otherwise it would be on screen and never saved.
    const texts = new Map<string, string>(spans.map(s => [s.dataset.addr!, '']))
    let current: string | null = null
    let leading = ''
    const walk = (node: Node) => {
      for (const child of Array.from(node.childNodes)) {
        if (child instanceof HTMLElement && child.dataset.addr) {
          if (child.classList.contains('dx-img')) continue
          current = child.dataset.addr
          texts.set(current, (texts.get(current) ?? '') + (child.textContent ?? ''))
        } else if (child.nodeType === Node.TEXT_NODE) {
          if (current) texts.set(current, texts.get(current) + (child.textContent ?? ''))
          else leading += child.textContent ?? ''
        } else if (child instanceof HTMLElement) {
          walk(child)
        }
      }
    }
    walk(el)
    if (leading) {
      const first = spans[0].dataset.addr!
      texts.set(first, leading + texts.get(first))
    }

    const patches: DraftPatch[] = []
    for (const [addr, raw] of texts) {
      const text = raw.replace(/\u200b/g, '')
      if (lastSent.current[addr] !== text) {
        lastSent.current[addr] = text
        patches.push({ op: 'SetRunText', addr, text })
      }
    }
    for (const run of block.runs) {
      if (!run.image && !texts.has(run.addr) && lastSent.current[run.addr] !== '') {
        lastSent.current[run.addr] = ''
        patches.push({ op: 'SetRunText', addr: run.addr, text: '' })
      }
    }
    if (patches.length > 0) onPatch(...patches)
  }, [block.runs, index, onEmptyInput, onPatch])

  const isHeading = /^(heading|title)/i.test(block.style ?? '')
  const level = block.level ?? 0
  const style: CSSProperties = {
    fontWeight: styleInfo?.bold ? 700 : undefined,
    fontStyle: styleInfo?.italic ? 'italic' : undefined,
    fontSize: styleInfo?.size_half_points ? `${styleInfo.size_half_points / 2}pt` : undefined,
    fontFamily: styleInfo?.font ? `"${styleInfo.font}", var(--dx-font)` : undefined,
    color: styleInfo?.color ? `#${styleInfo.color}` : undefined,
    textAlign: block.align === 'justify' ? 'justify' : block.align ?? undefined,
    lineHeight: block.line_spacing ? String(Math.max(1, block.line_spacing * 1.25)) : undefined,
    paddingLeft: block.list ? `${24 + level * 24}px` : level ? `${level * 48}px` : undefined,
  }

  return (
    <div
      ref={ref}
      className={`ws-paragraph dx-p${isHeading ? ' ws-paragraph-heading' : ''}${block.list ? ' dx-listed' : ''}`}
      contentEditable
      suppressContentEditableWarning
      spellCheck
      data-block={block.addr}
      data-index={index}
      data-marker={marker ?? undefined}
      data-style={block.style ?? undefined}
      style={style}
      onInput={handleInput}
      onCompositionStart={() => {
        composing.current = true
      }}
      onCompositionEnd={() => {
        composing.current = false
        handleInput()
      }}
      onKeyDown={e => ref.current && onKey(e, index, ref.current)}
      // Paste as plain text: pasted HTML would bring spans with foreign
      // `data-addr` attributes into the paragraph and corrupt the mapping.
      onPaste={e => {
        e.preventDefault()
        const text = e.clipboardData.getData('text/plain').replace(/\r\n?/g, '\n')
        document.execCommand('insertText', false, text.replace(/\n+/g, ' '))
      }}
    />
  )
}

// ------------------------------------------------------------------- editor

type Row =
  | { kind: 'paragraph'; block: DocxBlock; index: number }
  | { kind: 'table'; table: number; rows: { block: DocxBlock; index: number }[][][] }

/** Group consecutive table-cell paragraphs back into tables. */
function groupRows(blocks: DocxBlock[]): Row[] {
  const out: Row[] = []
  let current: { table: number; rows: { block: DocxBlock; index: number }[][][] } | null = null
  blocks.forEach((block, index) => {
    if (!block.cell) {
      if (current) {
        out.push({ kind: 'table', ...current })
        current = null
      }
      out.push({ kind: 'paragraph', block, index })
      return
    }
    const { table, row, col } = block.cell
    if (!current || current.table !== table) {
      if (current) out.push({ kind: 'table', ...current })
      current = { table, rows: [] }
    }
    while (current.rows.length <= row) current.rows.push([])
    const cells = current.rows[row]
    while (cells.length <= col) cells.push([])
    cells[col].push({ block, index })
  })
  if (current) out.push({ kind: 'table', ...(current as { table: number; rows: { block: DocxBlock; index: number }[][][] }) })
  return out
}

/** List markers, numbered the way Word numbers them. */
function listMarkers(blocks: DocxBlock[]): (string | null)[] {
  const counters = new Map<string, number[]>()
  const roman = (n: number) => {
    const map: [number, string][] = [[10, 'x'], [9, 'ix'], [5, 'v'], [4, 'iv'], [1, 'i']]
    let out = ''
    for (const [v, s] of map) while (n >= v) { out += s; n -= v }
    return out
  }
  return blocks.map(b => {
    if (!b.list) return null
    const level = b.level ?? 0
    if (b.list === 'bullet') return ['•', '◦', '▪'][level % 3]
    const key = b.num_id ?? 'n'
    const levels = counters.get(key) ?? []
    levels[level] = (levels[level] ?? 0) + 1
    levels.length = level + 1
    counters.set(key, levels)
    const n = levels[level]
    return [`${n}.`, `${String.fromCharCode(96 + ((n - 1) % 26) + 1)}.`, `${roman(n)}.`][level % 3]
  })
}

interface Sel {
  index: number
  start: number
  end: number
}

export default function DocxEditor({ blocks, styles, defaults, onPatch, onCommit, insert, onError }: Props) {
  const [tab, setTab] = useState('home')
  const [epoch, setEpoch] = useState(0)
  const [sel, setSel] = useState<Sel | null>(null)
  const [image, setImage] = useState<string | null>(null)
  const [equation, setEquation] = useState<{ index: number; text: string; mode: 'insert' | 'edit' } | null>(null)
  const [zoom, setZoom] = useState(100)
  const [busy, setBusy] = useState(false)
  const selRef = useRef<Sel | null>(null)
  const pendingCaret = useRef<Sel | null>(null)
  const page = useRef<HTMLDivElement | null>(null)

  const rows = useMemo(() => groupRows(blocks), [blocks])
  const markers = useMemo(() => listMarkers(blocks), [blocks])
  const styleById = useMemo(() => new Map(styles.map(s => [s.id, s])), [styles])
  const paragraphStyles = useMemo(() => {
    // The built-ins are always offered; the backend adds a definition to the
    // document the first time one is used, so they work in any file.
    const builtins: DocxStyle[] = [
      ['Normal', 'Normal'],
      ['Title', 'Title'],
      ['Subtitle', 'Subtitle'],
      ['Heading1', 'heading 1'],
      ['Heading2', 'heading 2'],
      ['Heading3', 'heading 3'],
      ['Quote', 'Quote'],
    ].map(([id, name]) => ({ id, name, bold: false, size_half_points: null, color: null, kind: 'paragraph' }))
    const own = styles.filter(s => (s.kind ?? 'paragraph') === 'paragraph')
    const wanted = [...own, ...builtins.filter(b => !own.some(s => s.id === b.id))]
    const rank = (s: DocxStyle) => {
      const id = s.id.toLowerCase()
      if (id === 'normal') return 0
      if (id === 'title') return 1
      if (id === 'subtitle') return 2
      const h = /^heading(\d)$/.exec(id)
      if (h) return 2 + Number(h[1])
      if (id.includes('quote')) return 20
      return 30
    }
    return wanted.sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name)).slice(0, 18)
  }, [styles])

  const paragraphEl = useCallback((index: number) => page.current?.querySelector<HTMLDivElement>(`[data-index="${index}"]`) ?? null, [])

  // Keep the last in-document selection, so ribbon controls that take focus
  // (font box, dialogs) still know what they apply to.
  useEffect(() => {
    const onChange = () => {
      const s = window.getSelection()
      const node = s?.anchorNode
      const el = (node instanceof HTMLElement ? node : node?.parentElement)?.closest<HTMLDivElement>('[data-index]')
      if (!el || !page.current?.contains(el)) return
      const offsets = selectionOffsets(el)
      if (!offsets) return
      const next = { index: Number(el.dataset.index), ...offsets }
      selRef.current = next
      setSel(prev => (prev && prev.index === next.index && prev.start === next.start && prev.end === next.end ? prev : next))
    }
    document.addEventListener('selectionchange', onChange)
    return () => document.removeEventListener('selectionchange', onChange)
  }, [])

  // Put the caret back once a commit's model has rendered.
  useEffect(() => {
    const target = pendingCaret.current
    if (!target) return
    const el = paragraphEl(Math.min(target.index, blocks.length - 1))
    if (!el) return
    pendingCaret.current = null
    el.focus({ preventScroll: true })
    placeCaret(el, target.start, target.end)
    el.scrollIntoView({ block: 'nearest' })
  }, [blocks, epoch, paragraphEl])

  const run = useCallback(
    async (patches: DraftPatch[], caret: Sel | null) => {
      if (patches.length === 0) return
      setBusy(true)
      try {
        pendingCaret.current = caret
        await onCommit(...patches)
        setEpoch(e => e + 1)
      } catch (e) {
        onError?.(e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
      }
    },
    [onCommit, onError],
  )

  const current = sel && blocks[sel.index] ? sel : null
  const block = current ? blocks[current.index] : null
  const anchorAddr = block?.addr ?? blocks.filter(b => !b.cell).at(-1)?.addr ?? null

  /** The run under the start of the selection, for showing tool states. */
  const activeRun = useMemo(() => {
    if (!block || !current) return null
    let cursor = 0
    for (const r of block.runs) {
      const len = r.text.length
      if (current.start < cursor + len || (current.start === cursor + len && current.start === current.end && len > 0)) return r
      cursor += len
    }
    return block.runs.at(-1) ?? null
  }, [block, current])

  const styleOf = block?.style ? styleById.get(block.style) : undefined
  const effectiveFont = activeRun?.font ?? styleOf?.font ?? defaults?.font ?? 'Calibri'
  const effectiveSize = activeRun?.size ?? (styleOf?.size_half_points ? styleOf.size_half_points / 2 : defaults?.size ?? 11)

  // ---------------------------------------------------------------- formatting

  const formatSelection = useCallback(
    (format: TextFormat) => {
      const s = selRef.current
      const b = s ? blocks[s.index] : null
      if (!s || !b) return
      let { start, end } = s
      if (start === end) {
        // Like Word: a caret inside a word formats that word.
        const text = blockText(b)
        const isWord = (c: string | undefined) => !!c && /[\p{L}\p{N}_'’-]/u.test(c)
        while (start > 0 && isWord(text[start - 1])) start--
        while (end < text.length && isWord(text[end])) end++
        if (start === end) {
          // An empty paragraph: format what will be typed into its run.
          const r = b.runs.find(x => !x.image)
          if (r) void run([{ op: 'SetRunFormat', addr: r.addr, ...format }], s)
          return
        }
      }
      void run([{ op: 'FormatRange', addr: b.addr, start, end, ...format }], s)
    },
    [blocks, run],
  )

  const toggle = (key: 'bold' | 'italic' | 'underline' | 'strike') => formatSelection({ [key]: !activeRun?.[key] })

  const paragraphFormat = useCallback(
    (fields: Omit<Extract<DraftPatch, { op: 'SetParagraphFormat' }>, 'op' | 'addr'>) => {
      const s = selRef.current
      const b = s ? blocks[s.index] : null
      if (!s || !b) return
      void run([{ op: 'SetParagraphFormat', addr: b.addr, ...fields }], s)
    },
    [blocks, run],
  )

  const setStyle = (styleId: string) => {
    const s = selRef.current
    const b = s ? blocks[s.index] : null
    if (!s || !b) return
    void run([{ op: 'SetParagraphStyle', addr: b.addr, style: styleId === 'Normal' || !styleId ? null : styleId }], s)
  }

  // ------------------------------------------------------------------ inserts

  const insertText = useCallback(
    (text: string) => {
      const s = selRef.current
      const el = s ? paragraphEl(s.index) : null
      if (!s || !el) {
        onError?.('Click into the document where the text should go first.')
        return
      }
      if (text.includes('\n')) {
        const b = blocks[s.index]
        const parts = text.split(/\n+/).map(p => p.trim()).filter(Boolean)
        void run(parts.reverse().map(p => ({ op: 'InsertParagraphAfter', addr: b.addr, text: p }) as DraftPatch), { index: s.index + parts.length, start: 0, end: 0 })
        return
      }
      el.focus({ preventScroll: true })
      placeCaret(el, s.start, s.end)
      document.execCommand('insertText', false, text)
    },
    [blocks, onError, paragraphEl, run],
  )

  const lastInsert = useRef<number | null>(null)
  useEffect(() => {
    if (!insert || insert.nonce === lastInsert.current) return
    lastInsert.current = insert.nonce
    insertText(insert.text)
  }, [insert, insertText])

  const insertAfterCurrent = (patch: DraftPatch, caretOffset = 1) => {
    const s = selRef.current
    void run([patch], s ? { index: s.index + caretOffset, start: 0, end: 0 } : null)
  }

  // ----------------------------------------------------------------- keyboard

  const onKey = useCallback(
    (e: React.KeyboardEvent<HTMLDivElement>, index: number, el: HTMLDivElement) => {
      const b = blocks[index]
      if (!b) return
      const offsets = selectionOffsets(el) ?? { start: 0, end: 0 }
      const text = blockText(b)
      const domLen = textNodes(el).reduce((n, t) => n + visibleLength(t.data), 0)
      const mod = e.ctrlKey || e.metaKey
      const sameCell = (other: DocxBlock | undefined) =>
        !!other && (other.cell?.table ?? -1) === (b.cell?.table ?? -1) && other.cell?.row === b.cell?.row && other.cell?.col === b.cell?.col

      if (mod && ['b', 'i', 'u'].includes(e.key.toLowerCase())) {
        e.preventDefault()
        toggle(e.key.toLowerCase() === 'b' ? 'bold' : e.key.toLowerCase() === 'i' ? 'italic' : 'underline')
        return
      }
      if (mod && ['l', 'e', 'r', 'j'].includes(e.key.toLowerCase())) {
        e.preventDefault()
        const map = { l: 'left', e: 'center', r: 'right', j: 'justify' } as const
        paragraphFormat({ align: map[e.key.toLowerCase() as 'l' | 'e' | 'r' | 'j'] })
        return
      }

      if (e.key === 'Enter') {
        e.preventDefault()
        if (b.list && domLen === 0) {
          // Enter on an empty list item ends the list, as in Word.
          void run([{ op: 'SetParagraphFormat', addr: b.addr, list: 'none' }], { index, start: 0, end: 0 })
          return
        }
        if (offsets.start !== offsets.end) document.execCommand('delete')
        void run([{ op: 'SplitParagraph', addr: b.addr, offset: offsets.start }], { index: index + 1, start: 0, end: 0 })
        return
      }

      if (e.key === 'Backspace' && offsets.start === 0 && offsets.end === 0) {
        if (b.list) {
          e.preventDefault()
          void run([{ op: 'SetParagraphFormat', addr: b.addr, list: 'none' }], { index, start: 0, end: 0 })
          return
        }
        const prev = blocks[index - 1]
        if (prev && sameCell(prev) && !prev.equation) {
          e.preventDefault()
          const prevLen = blockText(prev).length
          void run([{ op: 'MergeParagraphWithPrevious', addr: b.addr }], { index: index - 1, start: prevLen, end: prevLen })
        }
        return
      }

      if (e.key === 'Delete' && offsets.start === domLen && offsets.end === domLen) {
        const next = blocks[index + 1]
        if (next && sameCell(next) && !next.equation) {
          e.preventDefault()
          void run([{ op: 'MergeParagraphWithPrevious', addr: next.addr }], { index, start: text.length, end: text.length })
        }
        return
      }

      if (e.key === 'Tab') {
        e.preventDefault()
        if (b.cell) {
          // Tab walks table cells.
          const step = e.shiftKey ? -1 : 1
          for (let i = index + step; i >= 0 && i < blocks.length; i += step) {
            const o = blocks[i]
            if (!o.cell) break
            if (!sameCell(o)) {
              const target = paragraphEl(i)
              target?.focus()
              if (target) placeCaret(target, 0)
              break
            }
          }
          return
        }
        void run([{ op: 'SetParagraphFormat', addr: b.addr, indent: e.shiftKey ? -1 : 1 }], offsets && { index, ...offsets })
        return
      }

      // The caret cannot cross between contentEditables on its own.
      const moveTo = (target: number, atEnd: boolean) => {
        const t = paragraphEl(target)
        if (!t) return
        e.preventDefault()
        t.focus({ preventScroll: false })
        const len = textNodes(t).reduce((n, x) => n + visibleLength(x.data), 0)
        placeCaret(t, atEnd ? len : 0)
      }
      const caretRect = () => window.getSelection()?.getRangeAt(0).getBoundingClientRect()
      if (e.key === 'ArrowUp' && !e.shiftKey) {
        const r = caretRect()
        const top = el.getBoundingClientRect().top
        if (!r || r.top === 0 || r.top - top < parseFloat(getComputedStyle(el).lineHeight || '20') * 0.9) moveTo(index - 1, true)
      } else if (e.key === 'ArrowDown' && !e.shiftKey) {
        const r = caretRect()
        const bottom = el.getBoundingClientRect().bottom
        if (!r || r.bottom === 0 || bottom - r.bottom < parseFloat(getComputedStyle(el).lineHeight || '20') * 0.9) moveTo(index + 1, false)
      } else if (e.key === 'ArrowLeft' && !e.shiftKey && offsets.start === 0 && offsets.end === 0) {
        moveTo(index - 1, true)
      } else if (e.key === 'ArrowRight' && !e.shiftKey && offsets.start === domLen) {
        moveTo(index + 1, false)
      }
    },
    // toggle is recreated each render from activeRun; it is read at call time.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [blocks, paragraphEl, paragraphFormat, run, activeRun],
  )

  const onEmptyInput = useCallback(
    (index: number, text: string) => {
      const b = blocks[index]
      if (b) void run([{ op: 'SetParagraphText', addr: b.addr, text }], { index, start: text.length, end: text.length })
    },
    [blocks, run],
  )

  // ------------------------------------------------------------------ images

  const selectedImage = useMemo(() => {
    if (!image) return null
    for (const b of blocks) {
      const r = b.runs.find(x => x.addr === image && x.image)
      if (r?.image) return r
    }
    return null
  }, [image, blocks])

  const resizeImage = (factor: number) => {
    if (!selectedImage?.image) return
    const { width_emu: w, height_emu: h } = selectedImage.image
    void run([{ op: 'SetImageSize', addr: selectedImage.addr, width_emu: Math.round(w * factor), height_emu: Math.round(h * factor) }], null)
  }

  const inTable = !!block?.cell
  const tabs = [
    { id: 'home', label: 'Home' },
    { id: 'insert', label: 'Insert' },
    { id: 'layout', label: 'Layout' },
    ...(inTable ? [{ id: 'table', label: 'Table', contextual: true }] : []),
    ...(selectedImage ? [{ id: 'picture', label: 'Picture', contextual: true }] : []),
  ]
  const activeTab = tabs.some(t => t.id === tab) ? tab : 'home'
  const noSel = !block

  // Clicking a picture selects it; the Picture tab follows.
  const onPageClick = (e: React.MouseEvent) => {
    const img = (e.target as HTMLElement).closest<HTMLElement>('.dx-img')
    if (img?.dataset.addr) {
      setImage(img.dataset.addr)
      setTab('picture')
    } else if (image) {
      setImage(null)
    }
  }

  return (
    <div className="ws-docx ws-surface-host dx">
      <Ribbon
        tabs={tabs}
        active={activeTab}
        onTab={setTab}
        aside={
          <div className="rb-zoom">
            <Tool icon={<Minus size={13} />} title="Zoom out" onClick={() => setZoom(z => Math.max(50, z - 10))} />
            <span className="rb-zoom-value">{zoom}%</span>
            <Tool icon={<Plus size={13} />} title="Zoom in" onClick={() => setZoom(z => Math.min(200, z + 10))} />
          </div>
        }
      >
        {activeTab === 'home' && (
          <>
            <Group label="Style">
              <select
                className="rb-select rb-style-select"
                value={block?.style ?? 'Normal'}
                disabled={noSel}
                onChange={e => setStyle(e.target.value)}
                title="Paragraph style"
              >
                {!paragraphStyles.some(s => s.id === 'Normal') && <option value="Normal">Normal</option>}
                {paragraphStyles.map(s => (
                  <option key={s.id} value={s.id}>
                    {prettyStyleName(s)}
                  </option>
                ))}
              </select>
            </Group>
            <Group label="Font">
              <FontPicker value={effectiveFont} disabled={noSel} onPick={font => formatSelection({ font })} />
              <SizePicker value={effectiveSize} disabled={noSel} onPick={size => formatSelection({ size })} />
              <Tool icon={<Type size={15} />} label="+" title="Grow font" disabled={noSel} onClick={() => formatSelection({ size: Math.min(96, effectiveSize + (effectiveSize < 12 ? 1 : 2)) })} />
              <Tool icon={<Type size={12} />} label="−" title="Shrink font" disabled={noSel} onClick={() => formatSelection({ size: Math.max(6, effectiveSize - (effectiveSize <= 12 ? 1 : 2)) })} />
            </Group>
            <Group label="Character">
              <Tool icon={<Bold size={15} strokeWidth={2.25} />} title="Bold (Ctrl+B)" active={!!activeRun?.bold} disabled={noSel} onClick={() => toggle('bold')} />
              <Tool icon={<Italic size={15} />} title="Italic (Ctrl+I)" active={!!activeRun?.italic} disabled={noSel} onClick={() => toggle('italic')} />
              <Tool icon={<Underline size={15} />} title="Underline (Ctrl+U)" active={!!activeRun?.underline} disabled={noSel} onClick={() => toggle('underline')} />
              <Tool icon={<Strikethrough size={15} />} title="Strikethrough" active={!!activeRun?.strike} disabled={noSel} onClick={() => toggle('strike')} />
              <Tool icon={<Superscript size={15} />} title="Superscript" active={activeRun?.vert_align === 'superscript'} disabled={noSel} onClick={() => formatSelection({ vert_align: activeRun?.vert_align === 'superscript' ? 'baseline' : 'superscript' })} />
              <Tool icon={<Subscript size={15} />} title="Subscript" active={activeRun?.vert_align === 'subscript'} disabled={noSel} onClick={() => formatSelection({ vert_align: activeRun?.vert_align === 'subscript' ? 'baseline' : 'subscript' })} />
              <ColorPicker icon={<Baseline size={15} />} title="Font colour" value={activeRun?.color ?? null} disabled={noSel} onPick={color => formatSelection({ color })} />
              <HighlightPicker icon={<Highlighter size={15} />} value={activeRun?.highlight ?? null} disabled={noSel} onPick={highlight => formatSelection({ highlight })} />
              <Tool
                icon={<Eraser size={15} />}
                title="Clear formatting"
                disabled={noSel}
                onClick={() => formatSelection({ bold: false, italic: false, underline: false, strike: false, font: '', size: 0, color: '', highlight: '', vert_align: 'baseline' })}
              />
            </Group>
            <Group label="Paragraph">
              <Tool icon={<List size={15} />} title="Bullets" active={block?.list === 'bullet'} disabled={noSel} onClick={() => paragraphFormat({ list: block?.list === 'bullet' ? 'none' : 'bullet' })} />
              <Tool icon={<ListOrdered size={15} />} title="Numbering" active={block?.list === 'number'} disabled={noSel} onClick={() => paragraphFormat({ list: block?.list === 'number' ? 'none' : 'number' })} />
              <Tool icon={<IndentDecrease size={15} />} title="Decrease indent (Shift+Tab)" disabled={noSel} onClick={() => paragraphFormat({ indent: -1 })} />
              <Tool icon={<IndentIncrease size={15} />} title="Increase indent (Tab)" disabled={noSel} onClick={() => paragraphFormat({ indent: 1 })} />
            </Group>
            <Group label="Alignment">
              <Tool icon={<AlignLeft size={15} />} title="Align left (Ctrl+L)" active={!!block && (!block.align || block.align === 'left')} disabled={noSel} onClick={() => paragraphFormat({ align: 'left' })} />
              <Tool icon={<AlignCenter size={15} />} title="Centre (Ctrl+E)" active={block?.align === 'center'} disabled={noSel} onClick={() => paragraphFormat({ align: 'center' })} />
              <Tool icon={<AlignRight size={15} />} title="Align right (Ctrl+R)" active={block?.align === 'right'} disabled={noSel} onClick={() => paragraphFormat({ align: 'right' })} />
              <Tool icon={<AlignJustify size={15} />} title="Justify (Ctrl+J)" active={block?.align === 'justify'} disabled={noSel} onClick={() => paragraphFormat({ align: 'justify' })} />
              <Popover title="Line spacing" disabled={noSel} trigger={<Rows3 size={15} />}>
                {close => (
                  <div className="rb-menu">
                    {[1, 1.15, 1.5, 2, 2.5, 3].map(v => (
                      <button
                        key={v}
                        type="button"
                        className={`rb-list-item${block?.line_spacing === v ? ' is-active' : ''}`}
                        onClick={() => {
                          paragraphFormat({ line_spacing: v })
                          close()
                        }}
                      >
                        {v.toFixed(v % 1 ? 2 : 1)}
                      </button>
                    ))}
                  </div>
                )}
              </Popover>
            </Group>
          </>
        )}

        {activeTab === 'insert' && (
          <>
            <Group label="Tables">
              <TableGridPicker
                disabled={!anchorAddr || busy}
                trigger={<><Table2 size={15} /><span>Table</span></>}
                onPick={(r, c) => anchorAddr && insertAfterCurrent({ op: 'InsertTable', addr: anchorAddr, rows: r, cols: c })}
              />
            </Group>
            <Group label="Illustrations">
              <Tool
                icon={<ImagePlus size={15} />}
                label="Picture"
                title="Insert a picture"
                disabled={!anchorAddr || busy}
                onClick={() => {
                  void (async () => {
                    const file = await pickImage()
                    if (!file || !anchorAddr) return
                    try {
                      const img = await readImage(file)
                      insertAfterCurrent({ op: 'InsertImage', addr: anchorAddr, ...img })
                    } catch (err) {
                      onError?.(err instanceof Error ? err.message : String(err))
                    }
                  })()
                }}
              />
            </Group>
            <Group label="Symbols">
              <Tool icon={<Radical size={15} />} label="Equation" title="Insert an equation" disabled={!anchorAddr || busy} onClick={() => setEquation({ index: selRef.current?.index ?? blocks.length - 1, text: '', mode: 'insert' })} />
              <SymbolPicker disabled={noSel} trigger={<><Omega size={15} /><span>Symbol</span></>} onPick={insertText} />
            </Group>
            <Group label="Text">
              <Tool
                icon={<CalendarDays size={15} />}
                label="Date"
                title="Insert today's date"
                disabled={noSel}
                onClick={() => insertText(new Date().toLocaleDateString(undefined, { day: 'numeric', month: 'long', year: 'numeric' }))}
              />
              <Tool icon={<Pilcrow size={15} />} label="Paragraph" title="Insert a paragraph below" disabled={!anchorAddr || busy} onClick={() => anchorAddr && insertAfterCurrent({ op: 'InsertParagraphAfter', addr: anchorAddr, text: '' })} />
            </Group>
          </>
        )}

        {activeTab === 'layout' && (
          <>
            <Group label="Pages">
              <Tool icon={<SeparatorHorizontal size={15} />} label="Page break" title="Start a new page below" disabled={!anchorAddr || busy} onClick={() => anchorAddr && insertAfterCurrent({ op: 'InsertPageBreak', addr: anchorAddr }, 2)} />
            </Group>
            <Group label="Paragraph">
              <Tool icon={<IndentDecrease size={15} />} label="Outdent" title="Decrease indent" disabled={noSel} onClick={() => paragraphFormat({ indent: -1 })} />
              <Tool icon={<IndentIncrease size={15} />} label="Indent" title="Increase indent" disabled={noSel} onClick={() => paragraphFormat({ indent: 1 })} />
            </Group>
            <Group label="Remove">
              <Tool icon={<Trash2 size={15} />} label="Delete paragraph" title="Delete this paragraph" danger disabled={noSel || busy} onClick={() => block && current && void run([{ op: 'DeleteParagraph', addr: block.addr }], { index: Math.max(0, current.index - 1), start: 0, end: 0 })} />
            </Group>
          </>
        )}

        {activeTab === 'table' && block?.cell && (
          <>
            <Group label="Rows">
              <Tool icon={<ArrowUpToLine size={15} />} label="Above" title="Insert row above" disabled={busy} onClick={() => void run([{ op: 'InsertTableRow', addr: block.addr, below: false }], current)} />
              <Tool icon={<ArrowDownToLine size={15} />} label="Below" title="Insert row below" disabled={busy} onClick={() => void run([{ op: 'InsertTableRow', addr: block.addr, below: true }], current)} />
            </Group>
            <Group label="Columns">
              <Tool icon={<ArrowLeftToLine size={15} />} label="Left" title="Insert column left" disabled={busy} onClick={() => void run([{ op: 'InsertTableColumn', addr: block.addr, right: false }], current)} />
              <Tool icon={<ArrowRightToLine size={15} />} label="Right" title="Insert column right" disabled={busy} onClick={() => void run([{ op: 'InsertTableColumn', addr: block.addr, right: true }], current)} />
            </Group>
            <Group label="Delete">
              <Tool icon={<Trash2 size={15} />} label="Row" title="Delete row" danger disabled={busy} onClick={() => void run([{ op: 'DeleteTableRow', addr: block.addr }], null)} />
              <Tool icon={<Trash2 size={15} />} label="Column" title="Delete column" danger disabled={busy} onClick={() => void run([{ op: 'DeleteTableColumn', addr: block.addr }], null)} />
              <Tool icon={<Trash2 size={15} />} label="Table" title="Delete table" danger disabled={busy} onClick={() => void run([{ op: 'DeleteTable', addr: block.addr }], null)} />
            </Group>
          </>
        )}

        {activeTab === 'picture' && selectedImage?.image && (
          <>
            <Group label="Size">
              <Tool icon={<Minus size={15} />} label="Smaller" title="Make the picture smaller" disabled={busy} onClick={() => resizeImage(0.85)} />
              <Tool icon={<Plus size={15} />} label="Larger" title="Make the picture larger" disabled={busy} onClick={() => resizeImage(1.15)} />
              <span className="rb-hint">
                {(selectedImage.image.width_emu / 914400).toFixed(2)}″ × {(selectedImage.image.height_emu / 914400).toFixed(2)}″
              </span>
            </Group>
            <Group label="Remove">
              <Tool
                icon={<Trash2 size={15} />}
                label="Delete picture"
                title="Delete this picture"
                danger
                disabled={busy}
                onClick={() => {
                  void run([{ op: 'DeleteRun', addr: selectedImage.addr }], null)
                  setImage(null)
                }}
              />
            </Group>
          </>
        )}
      </Ribbon>

      <div className="ws-surface dx-surface" style={{ zoom: zoom / 100 } as CSSProperties} onMouseDown={onPageClick}>
        <div
          className="ws-page dx-page"
          ref={page}
          style={{
            ['--dx-font' as string]: `"${defaults?.font ?? 'Calibri'}", Carlito, Arial, sans-serif`,
            fontSize: defaults?.size ? `${defaults.size}pt` : undefined,
          }}
        >
          {rows.map((row, i) => {
            if (row.kind === 'paragraph') {
              const b = row.block
              if (b.equation != null) {
                return (
                  <div key={b.addr} className="dx-equation" data-index={row.index} style={{ textAlign: b.align === 'left' ? 'left' : 'center' }}>
                    <button type="button" className="dx-equation-body" title="Edit equation" onClick={() => setEquation({ index: row.index, text: b.equation ?? '', mode: 'edit' })}>
                      <MathView text={b.equation} />
                    </button>
                    <button type="button" className="dx-block-remove" title="Delete equation" onClick={() => void run([{ op: 'DeleteParagraph', addr: b.addr }], null)}>
                      <Trash2 size={13} />
                    </button>
                  </div>
                )
              }
              if (b.page_break && b.runs.every(r => !r.text)) {
                return (
                  <div key={b.addr} className="dx-page-break" data-index={row.index}>
                    <span>Page break</span>
                    <button type="button" className="dx-block-remove" title="Remove page break" onClick={() => void run([{ op: 'DeleteParagraph', addr: b.addr }], null)}>
                      <Trash2 size={13} />
                    </button>
                  </div>
                )
              }
              return (
                <Paragraph
                  key={b.addr}
                  block={b}
                  index={row.index}
                  marker={markers[row.index]}
                  styleInfo={b.style ? styleById.get(b.style) : undefined}
                  epoch={epoch}
                  onPatch={onPatch}
                  onKey={onKey}
                  onEmptyInput={onEmptyInput}
                />
              )
            }
            return (
              <table className="ws-table dx-table" key={`table-${row.table}-${i}`}>
                <tbody>
                  {row.rows.map((cells, r) => (
                    <tr key={r}>
                      {cells.map((paras, c) => (
                        <td key={c} className={current && paras.some(p => p.index === current.index) ? 'is-active' : undefined}>
                          {paras.map(({ block: b, index }) => (
                            <Paragraph
                              key={b.addr}
                              block={b}
                              index={index}
                              marker={markers[index]}
                              styleInfo={b.style ? styleById.get(b.style) : undefined}
                              epoch={epoch}
                              onPatch={onPatch}
                              onKey={onKey}
                              onEmptyInput={onEmptyInput}
                            />
                          ))}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            )
          })}
        </div>
      </div>

      {equation && (
        <EquationDialog
          title={equation.mode === 'edit' ? 'Edit equation' : 'Insert equation'}
          initial={equation.text}
          onClose={() => setEquation(null)}
          onSubmit={text => {
            const target = blocks[equation.index]
            setEquation(null)
            if (!target) return
            void run(
              [equation.mode === 'edit' ? { op: 'SetEquation', addr: target.addr, text } : { op: 'InsertEquation', addr: target.addr, text }],
              equation.mode === 'edit' ? null : { index: equation.index + 2, start: 0, end: 0 },
            )
          }}
        />
      )}
    </div>
  )
}
