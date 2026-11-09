/**
 * Plain text editor.
 *
 * The editor always works in `\n`; the file's real encoding, BOM and line
 * endings are detected on open and restored on save by the backend.
 *
 * # What a plain-text file can hold
 *
 * A `.txt` stores characters and nothing else — no bold, no fonts, no sizes.
 * The ribbon is honest about that: the font, size and wrap controls sit under
 * "View" and only change how the text is shown here, and every tool that
 * changes the FILE (lists, indent, case, find and replace, symbols, a text
 * table) does so by editing the characters themselves, which is exactly what
 * the file will contain when it is opened anywhere else.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  ALargeSmall,
  CalendarDays,
  CaseLower,
  CaseSensitive,
  CaseUpper,
  IndentDecrease,
  IndentIncrease,
  List,
  ListOrdered,
  Omega,
  Replace,
  Search,
  SeparatorHorizontal,
  Table2,
  WrapText,
  X,
} from 'lucide-react'

import type { DraftPatch } from '../../../api/drafts'
import {
  Group,
  Popover,
  Ribbon,
  SymbolPicker,
  TableGridPicker,
  Tool,
} from './ribbon'
import { FONTS, SIZES } from './ribbonData'

interface Props {
  content: string
  encoding: string
  lineEnding: string
  bom: boolean
  onPatch: (...patches: DraftPatch[]) => void
  /** Text to splice in at the caret, e.g. a clause or an assistant reply. */
  insert?: { text: string; nonce: number } | null
}

interface ViewPrefs {
  font: string
  size: number
  wrap: boolean
  mono: boolean
}

const PREFS_KEY = 'oca.text.view'

function loadPrefs(): ViewPrefs {
  try {
    const raw = localStorage.getItem(PREFS_KEY)
    if (raw) return { font: 'Calibri', size: 11, wrap: true, mono: false, ...JSON.parse(raw) }
  } catch {
    /* private mode: defaults are fine */
  }
  return { font: 'Calibri', size: 11, wrap: true, mono: false }
}

function escapeRegex(s: string) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

export default function TextEditor({ content, encoding, lineEnding, bom, onPatch, insert }: Props) {
  const [value, setValue] = useState(content)
  const [tab, setTab] = useState('home')
  const [prefs, setPrefs] = useState<ViewPrefs>(loadPrefs)
  const [findOpen, setFindOpen] = useState(false)
  const [find, setFind] = useState('')
  const [replace, setReplace] = useState('')
  const [matchCase, setMatchCase] = useState(false)
  const [matchIndex, setMatchIndex] = useState(0)
  const dirty = useRef(false)
  const area = useRef<HTMLTextAreaElement | null>(null)
  const findInput = useRef<HTMLInputElement | null>(null)
  // Where the caret was when the textarea last lost focus. Ribbon controls and
  // the side column take focus, so the live selection is gone by the time they act.
  const caret = useRef<{ start: number; end: number } | null>(null)
  const valueRef = useRef(value)
  useEffect(() => {
    valueRef.current = value
  }, [value])

  useEffect(() => {
    try {
      localStorage.setItem(PREFS_KEY, JSON.stringify(prefs))
    } catch {
      /* not persisted; still applied */
    }
  }, [prefs])

  // Adopt content from the server ONLY when the user is not mid-edit.
  useEffect(() => {
    if (!dirty.current) setValue(content)
  }, [content])

  // The textarea grows to its content and the page scrolls around it.
  useEffect(() => {
    const el = area.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${el.scrollHeight}px`
  }, [value, prefs])

  /** Replace the whole text, keeping an undo step and saving. */
  const write = useCallback(
    (next: string, selStart: number, selEnd = selStart) => {
      const el = area.current
      if (el) {
        // Going through the browser's own editing command keeps Ctrl+Z working.
        el.focus()
        el.setSelectionRange(0, el.value.length)
        document.execCommand('insertText', false, next)
        el.setSelectionRange(selStart, selEnd)
      }
      dirty.current = false
      caret.current = { start: selStart, end: selEnd }
      setValue(next)
      onPatch({ op: 'SetText', content: next })
    },
    [onPatch],
  )

  const selection = () => {
    const el = area.current
    if (el && document.activeElement === el) return { start: el.selectionStart, end: el.selectionEnd }
    return caret.current ?? { start: valueRef.current.length, end: valueRef.current.length }
  }

  const insertAtCaret = useCallback(
    (text: string, block = false) => {
      const current = valueRef.current
      const at = selection()
      const before = current.slice(0, at.start)
      const after = current.slice(at.end)
      const lead = block && before && !before.endsWith('\n') ? '\n\n' : ''
      const trail = block && after && !after.startsWith('\n') ? '\n\n' : ''
      const end = before.length + lead.length + text.length
      write(`${before}${lead}${text}${trail}${after}`, end)
    },
    [write],
  )

  const lastNonce = useRef<number | null>(null)
  useEffect(() => {
    if (!insert || insert.nonce === lastNonce.current) return
    lastNonce.current = insert.nonce
    insertAtCaret(insert.text, true)
  }, [insert, insertAtCaret])

  /** Apply `fn` to each line touched by the selection. */
  const transformLines = (fn: (line: string, i: number) => string) => {
    const current = valueRef.current
    const { start, end } = selection()
    const lineStart = current.lastIndexOf('\n', start - 1) + 1
    const nextBreak = current.indexOf('\n', Math.max(end - (end > start && current[end - 1] === '\n' ? 1 : 0), start))
    const lineEnd = nextBreak === -1 ? current.length : nextBreak
    const block = current.slice(lineStart, lineEnd)
    const out = block.split('\n').map(fn).join('\n')
    write(current.slice(0, lineStart) + out + current.slice(lineEnd), lineStart, lineStart + out.length)
  }

  const transformSelection = (fn: (s: string) => string) => {
    const current = valueRef.current
    const { start, end } = selection()
    if (start === end) return
    const out = fn(current.slice(start, end))
    write(current.slice(0, start) + out + current.slice(end), start, start + out.length)
  }

  const listPrefix = /^(\s*)(?:[•\-*]\s+|\d+[.)]\s+)/

  // -------------------------------------------------------------- find/replace

  const matches = useMemo(() => {
    if (!find) return [] as number[]
    const re = new RegExp(escapeRegex(find), matchCase ? 'g' : 'gi')
    const out: number[] = []
    for (let m = re.exec(value); m && out.length < 5000; m = re.exec(value)) out.push(m.index)
    return out
  }, [find, matchCase, value])

  const goTo = (i: number) => {
    if (!matches.length) return
    const idx = ((i % matches.length) + matches.length) % matches.length
    setMatchIndex(idx)
    const el = area.current
    if (!el) return
    el.focus()
    el.setSelectionRange(matches[idx], matches[idx] + find.length)
    // Scroll the match into view: the host scrolls, not the textarea.
    const line = value.slice(0, matches[idx]).split('\n').length
    const lineHeight = parseFloat(getComputedStyle(el).lineHeight || '20')
    el.closest('.ws-surface-host')?.scrollTo({ top: Math.max(0, line * lineHeight - 200), behavior: 'smooth' })
    findInput.current?.focus()
  }

  const replaceOne = () => {
    if (!matches.length) return
    const idx = Math.min(matchIndex, matches.length - 1)
    const at = matches[idx]
    const current = valueRef.current
    write(current.slice(0, at) + replace + current.slice(at + find.length), at + replace.length)
  }

  const replaceAll = () => {
    if (!find) return
    const re = new RegExp(escapeRegex(find), matchCase ? 'g' : 'gi')
    write(valueRef.current.replace(re, () => replace), 0)
  }

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return
      const k = e.key.toLowerCase()
      if (k === 'f' || k === 'h') {
        e.preventDefault()
        setFindOpen(true)
        const el = area.current
        if (el && el.selectionStart !== el.selectionEnd) setFind(el.value.slice(el.selectionStart, el.selectionEnd))
        requestAnimationFrame(() => findInput.current?.select())
      }
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [])

  const lines = value === '' ? 0 : value.split('\n').length
  const words = value.trim() ? value.trim().split(/\s+/).length : 0

  return (
    <div className="ws-text ws-surface-host tx">
      <Ribbon
        tabs={[
          { id: 'home', label: 'Home' },
          { id: 'insert', label: 'Insert' },
          { id: 'view', label: 'View' },
        ]}
        active={tab}
        onTab={setTab}
        aside={<span className="rb-hint">Plain text · {encoding} · {lineEnding}{bom ? ' · BOM' : ''}</span>}
      >
        {tab === 'home' && (
          <>
            <Group label="Lists">
              <Tool icon={<List size={15} />} title="Bulleted lines" onClick={() => transformLines(l => (l.trim() ? (listPrefix.test(l) ? l.replace(listPrefix, '$1') : `• ${l}`) : l))} />
              <Tool
                icon={<ListOrdered size={15} />}
                title="Numbered lines"
                onClick={() => {
                  let n = 0
                  transformLines(l => (l.trim() ? `${++n}. ${l.replace(listPrefix, '$1')}` : l))
                }}
              />
              <Tool icon={<IndentDecrease size={15} />} title="Outdent lines" onClick={() => transformLines(l => l.replace(/^( {1,4}|\t)/, ''))} />
              <Tool icon={<IndentIncrease size={15} />} title="Indent lines" onClick={() => transformLines(l => `    ${l}`)} />
            </Group>
            <Group label="Case">
              <Tool icon={<CaseUpper size={16} />} title="UPPERCASE" onClick={() => transformSelection(s => s.toUpperCase())} />
              <Tool icon={<CaseLower size={16} />} title="lowercase" onClick={() => transformSelection(s => s.toLowerCase())} />
              <Tool icon={<CaseSensitive size={16} />} title="Title Case" onClick={() => transformSelection(s => s.toLowerCase().replace(/(^|\s)(\p{L})/gu, (_, a, b) => a + b.toUpperCase()))} />
            </Group>
            <Group label="Editing">
              <Tool
                icon={<Search size={15} />}
                label="Find"
                title="Find and replace (Ctrl+F)"
                active={findOpen}
                onClick={() => {
                  setFindOpen(o => !o)
                  requestAnimationFrame(() => findInput.current?.focus())
                }}
              />
            </Group>
          </>
        )}

        {tab === 'insert' && (
          <>
            <Group label="Text">
              <SymbolPicker trigger={<><Omega size={15} /><span>Symbol</span></>} onPick={s => insertAtCaret(s)} />
              <Tool icon={<CalendarDays size={15} />} label="Date" title="Insert today's date" onClick={() => insertAtCaret(new Date().toLocaleDateString(undefined, { day: 'numeric', month: 'long', year: 'numeric' }))} />
              <Tool icon={<SeparatorHorizontal size={15} />} label="Divider" title="Insert a divider line" onClick={() => insertAtCaret('─'.repeat(48), true)} />
            </Group>
            <Group label="Tables">
              <TableGridPicker
                trigger={<><Table2 size={15} /><span>Text table</span></>}
                onPick={(rows, cols) => {
                  const cell = ' '.repeat(12)
                  const line = `+${Array(cols).fill('-'.repeat(14)).join('+')}+`
                  const row = `| ${Array(cols).fill(cell).join(' | ')} |`
                  insertAtCaret([line, ...Array.from({ length: rows }, () => [row, line]).flat()].join('\n'), true)
                }}
              />
            </Group>
          </>
        )}

        {tab === 'view' && (
          <>
            <Group label="Display font (not saved in the file)">
              <select className="rb-select" value={prefs.font} onChange={e => setPrefs(p => ({ ...p, font: e.target.value }))} title="Display font">
                {FONTS.map(font => (
                  <option key={font} value={font}>
                    {font}
                  </option>
                ))}
              </select>
              <Popover title="Display size" trigger={<><ALargeSmall size={15} /><span>{prefs.size}</span></>}>
                {close => (
                  <div className="rb-menu">
                    {SIZES.filter(s => s >= 8 && s <= 28).map(s => (
                      <button key={s} type="button" className={`rb-list-item${s === prefs.size ? ' is-active' : ''}`} onClick={() => { setPrefs(p => ({ ...p, size: s })); close() }}>
                        {s}
                      </button>
                    ))}
                  </div>
                )}
              </Popover>
              <Tool icon={<span className="rb-glyph">{'{ }'}</span>} title="Monospace" active={prefs.mono} onClick={() => setPrefs(p => ({ ...p, mono: !p.mono }))} />
              <Tool icon={<WrapText size={15} />} label="Wrap" title="Wrap long lines" active={prefs.wrap} onClick={() => setPrefs(p => ({ ...p, wrap: !p.wrap }))} />
            </Group>
            <span className="rb-hint rb-note">A .txt file stores text only. These change how it looks here, not the file.</span>
          </>
        )}
      </Ribbon>

      {findOpen && (
        <div className="tx-find" role="search">
          <Search size={14} className="tx-find-icon" />
          <input
            ref={findInput}
            value={find}
            placeholder="Find"
            onChange={e => {
              setFind(e.target.value)
              setMatchIndex(0)
            }}
            onKeyDown={e => {
              if (e.key === 'Enter') goTo(matchIndex + (e.shiftKey ? -1 : 1))
              if (e.key === 'Escape') setFindOpen(false)
            }}
          />
          <span className="tx-find-count">{find ? `${matches.length ? Math.min(matchIndex, matches.length - 1) + 1 : 0} / ${matches.length}` : ''}</span>
          <Tool icon={<span className="rb-glyph">Aa</span>} title="Match case" active={matchCase} onClick={() => setMatchCase(m => !m)} />
          <Tool icon={<span className="rb-glyph">↑</span>} title="Previous (Shift+Enter)" disabled={!matches.length} onClick={() => goTo(matchIndex - 1)} />
          <Tool icon={<span className="rb-glyph">↓</span>} title="Next (Enter)" disabled={!matches.length} onClick={() => goTo(matchIndex + 1)} />
          <span className="tx-find-sep" />
          <Replace size={14} className="tx-find-icon" />
          <input value={replace} placeholder="Replace with" onChange={e => setReplace(e.target.value)} onKeyDown={e => e.key === 'Enter' && replaceOne()} />
          <Tool label="Replace" wide title="Replace this match" disabled={!matches.length} onClick={replaceOne} />
          <Tool label="All" wide title="Replace every match" disabled={!matches.length} onClick={replaceAll} />
          <Tool icon={<X size={14} />} title="Close" onClick={() => setFindOpen(false)} />
        </div>
      )}

      <div className="ws-surface">
        <div className="ws-page">
          <textarea
            ref={area}
            className="ws-textarea"
            value={value}
            spellCheck
            placeholder="Start writing…"
            wrap={prefs.wrap ? 'soft' : 'off'}
            style={{
              fontFamily: prefs.mono ? 'var(--font-mono)' : `"${prefs.font}", var(--font-sans)`,
              fontSize: `${prefs.size}pt`,
              whiteSpace: prefs.wrap ? 'pre-wrap' : 'pre',
              overflowX: prefs.wrap ? 'hidden' : 'auto',
            }}
            onChange={e => {
              dirty.current = true
              setValue(e.target.value)
              onPatch({ op: 'SetText', content: e.target.value })
            }}
            onKeyDown={e => {
              // Tab indents rather than leaving the page.
              if (e.key === 'Tab') {
                e.preventDefault()
                if (e.currentTarget.selectionStart !== e.currentTarget.selectionEnd) {
                  transformLines(l => (e.shiftKey ? l.replace(/^( {1,4}|\t)/, '') : `    ${l}`))
                } else if (!e.shiftKey) {
                  document.execCommand('insertText', false, '    ')
                }
              }
              // Enter continues a list, like every editor with lists.
              if (e.key === 'Enter' && !e.shiftKey) {
                const el = e.currentTarget
                const start = el.selectionStart
                const lineStart = el.value.lastIndexOf('\n', start - 1) + 1
                const line = el.value.slice(lineStart, start)
                const m = /^(\s*)([•\-*]|\d+[.)])(\s+)/.exec(line)
                if (m) {
                  e.preventDefault()
                  if (line.trim() === m[2]) {
                    // An empty item ends the list.
                    el.setSelectionRange(lineStart, start)
                    document.execCommand('insertText', false, '')
                    return
                  }
                  const num = /^\d+/.exec(m[2])
                  const nextMarker = num ? `${Number(num[0]) + 1}${m[2].slice(num[0].length)}` : m[2]
                  document.execCommand('insertText', false, `\n${m[1]}${nextMarker}${m[3]}`)
                }
              }
            }}
            onBlur={e => {
              caret.current = { start: e.currentTarget.selectionStart, end: e.currentTarget.selectionEnd }
              dirty.current = false
            }}
          />
        </div>
        <div className="ws-text-status">
          <span>
            {words.toLocaleString()} word{words === 1 ? '' : 's'}
          </span>
          <span>
            {lines} line{lines === 1 ? '' : 's'}
          </span>
          <span>{value.length.toLocaleString()} characters</span>
        </div>
      </div>
    </div>
  )
}
