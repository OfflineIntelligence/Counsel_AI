/**
 * The redline.
 *
 * # Two modes, because they answer different questions
 *
 * Side by side answers "what did each version say"; inline answers "what
 * changed". Lawyers use both — the first when reading a returned draft, the
 * second when writing the note that explains it — so neither is a setting
 * buried somewhere, they are one toggle in the header.
 *
 * # Why the panes do not scroll together
 *
 * Synchronised scrolling sounds right and is wrong here: the two sides have
 * different lengths wherever text was added or removed, so locking them drifts
 * apart exactly where the reader is looking. Instead both sides of a change sit
 * in the SAME row, so they cannot drift by construction.
 *
 * # Navigating changes
 *
 * Every change has an index. The list, the previous/next buttons and the
 * keyboard (J / K, or Alt+↓ / Alt+↑) all move one "current change", which is
 * scrolled into view and outlined in the document.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { ArrowLeft, ChevronDown, ChevronUp, Columns2, FileDiff } from 'lucide-react'

import {
  compareDocuments,
  listVersions,
  type CompareResult,
  type DiffOp,
  type DocRef,
  type VersionSummary,
  type WordOp,
} from '../../api/drafts'

interface Props {
  left: DocRef
  right: DocRef
  onClose: () => void
  /** Pick different versions to compare; offered when both sides are one draft. */
  onChange?: (left: DocRef, right: DocRef) => void
}

type Mode = 'split' | 'inline'
type Filter = 'all' | 'insert' | 'delete' | 'change'

export default function CompareView({ left, right, onClose, onChange }: Props) {
  const [result, setResult] = useState<CompareResult | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [mode, setMode] = useState<Mode>('split')
  const [filter, setFilter] = useState<Filter>('all')
  const [current, setCurrent] = useState(0)
  const [versions, setVersions] = useState<VersionSummary[]>([])
  const body = useRef<HTMLDivElement | null>(null)

  const sameDraft = left.id === right.id

  // No reset of result/error here: the parent keys this component by the two
  // refs, so changing what is compared remounts it with fresh state.
  useEffect(() => {
    let cancelled = false
    compareDocuments(left, right)
      .then(r => !cancelled && setResult(r))
      .catch(e => !cancelled && setError(e instanceof Error ? e.message : String(e)))
    return () => {
      cancelled = true
    }
  }, [left, right])

  useEffect(() => {
    if (!sameDraft) return
    let live = true
    listVersions(left.id)
      .then(v => live && setVersions(v))
      .catch(() => live && setVersions([]))
    return () => {
      live = false
    }
  }, [left.id, sameDraft])

  /** Indices into `result.ops` of every change, in document order. */
  const changeIndices = useMemo(
    () => (result?.ops ?? []).map((op, i) => (op.op === 'equal' ? -1 : i)).filter(i => i >= 0),
    [result],
  )

  const listed = useMemo(
    () => changeIndices.filter(i => filter === 'all' || result?.ops[i].op === filter),
    [changeIndices, filter, result],
  )

  const goTo = useCallback(
    (position: number) => {
      if (listed.length === 0) return
      const next = ((position % listed.length) + listed.length) % listed.length
      setCurrent(next)
      body.current?.querySelector(`[data-op="${listed[next]}"]`)?.scrollIntoView({ behavior: 'smooth', block: 'center' })
    },
    [listed],
  )

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement
      if (target.closest('input, select, textarea')) return
      if (e.key === 'j' || (e.altKey && e.key === 'ArrowDown')) {
        e.preventDefault()
        goTo(current + 1)
      } else if (e.key === 'k' || (e.altKey && e.key === 'ArrowUp')) {
        e.preventDefault()
        goTo(current - 1)
      } else if (e.key === 'Escape') {
        onClose()
      }
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [current, goTo, onClose])

  const activeOp = listed[current] ?? -1
  const versionOptions = versions.slice().sort((a, b) => a.version_no - b.version_no)
  const leftVersion = result?.left.version ?? left.version
  const rightVersion = result?.right.version ?? right.version

  return (
    <div className="ws-compare">
      <header className="ws-header">
        <div className="ws-header-title">
          <button type="button" className="ws-tool ws-back" onClick={onClose} title="Close comparison (Esc)">
            <ArrowLeft size={14} strokeWidth={1.75} />
            Back
          </button>
          <h1>{result ? (sameDraft ? result.left.title : `${result.left.title} ↔ ${result.right.title}`) : 'Comparing…'}</h1>
          {sameDraft && onChange && versionOptions.length > 1 && (
            <span className="cmp-versions">
              <select
                className="ws-header-select"
                value={leftVersion ?? ''}
                aria-label="Earlier version"
                onChange={e => onChange({ ...left, version: Number(e.target.value) }, right)}
              >
                {versionOptions.map(v => (
                  <option key={v.version_no} value={v.version_no}>
                    v{v.version_no}
                    {v.label ? ` · ${v.label}` : ''}
                  </option>
                ))}
              </select>
              <span className="ws-hint">→</span>
              <select
                className="ws-header-select"
                value={rightVersion ?? ''}
                aria-label="Later version"
                onChange={e => onChange(left, { ...right, version: Number(e.target.value) })}
              >
                {versionOptions.map(v => (
                  <option key={v.version_no} value={v.version_no}>
                    v{v.version_no}
                    {v.is_current ? ' · current' : v.label ? ` · ${v.label}` : ''}
                  </option>
                ))}
              </select>
            </span>
          )}
          {result && !sameDraft && (
            <span className="ws-hint">
              v{result.left.version} → v{result.right.version}
            </span>
          )}
        </div>

        <div className="ws-header-actions">
          {result && !result.summary.identical && (
            <>
              <span className="ws-diff-counts">
                <span className="ws-diff-count ws-diff-count-ins">+{result.summary.insertions}</span>
                <span className="ws-diff-count ws-diff-count-del">−{result.summary.deletions}</span>
                <span className="ws-diff-count">{result.summary.changes} edited</span>
              </span>
              <span className="cmp-nav">
                <button type="button" className="ws-tool ws-tool-icon" onClick={() => goTo(current - 1)} disabled={!listed.length} title="Previous change (K)">
                  <ChevronUp size={15} strokeWidth={1.75} />
                </button>
                <span className="ws-hint">
                  {listed.length ? `${Math.min(current, listed.length - 1) + 1} of ${listed.length}` : '0 of 0'}
                </span>
                <button type="button" className="ws-tool ws-tool-icon" onClick={() => goTo(current + 1)} disabled={!listed.length} title="Next change (J)">
                  <ChevronDown size={15} strokeWidth={1.75} />
                </button>
              </span>
            </>
          )}
          <div className="ws-segment" role="group" aria-label="Comparison layout">
            <button
              type="button"
              className={`ws-tool ws-tool-icon${mode === 'split' ? ' ws-tool-active' : ''}`}
              onClick={() => setMode('split')}
              aria-pressed={mode === 'split'}
              title="Side by side"
            >
              <Columns2 size={15} strokeWidth={1.75} />
            </button>
            <button
              type="button"
              className={`ws-tool ws-tool-icon${mode === 'inline' ? ' ws-tool-active' : ''}`}
              onClick={() => setMode('inline')}
              aria-pressed={mode === 'inline'}
              title="Inline redline"
            >
              <FileDiff size={15} strokeWidth={1.75} />
            </button>
          </div>
        </div>
      </header>

      <div className="ws-compare-body" ref={body}>
        {!result && !error && <Skeleton />}

        {error && (
          <div className="cmp-message">
            <h2>These documents could not be compared</h2>
            <p>{error}</p>
            <button type="button" className="ws-tool" onClick={onClose}>
              Back to the draft
            </button>
          </div>
        )}

        {result?.summary.identical && (
          <div className="cmp-message">
            <h2>No differences</h2>
            <p>These two versions have exactly the same text.</p>
          </div>
        )}

        {result && !result.summary.identical && (
          <div className="ws-surface-host">
            {mode === 'split' && (
              <div className="cmp-columns">
                <span>
                  {result.left.title} · v{result.left.version}
                </span>
                <span>
                  {result.right.title} · v{result.right.version}
                </span>
              </div>
            )}
            <div className={`ws-surface ws-diff ws-diff-${mode}`}>
              {result.ops.map((op, i) => (
                <Row key={i} index={i} op={op} mode={mode} active={i === activeOp} />
              ))}
            </div>
            <div className="cmp-legend">
              <span className="cmp-key cmp-key-ins">Added</span>
              <span className="cmp-key cmp-key-del">Removed</span>
              <span className="cmp-key cmp-key-chg">Edited</span>
            </div>
          </div>
        )}
      </div>

      {result && changeIndices.length > 0 && (
        <aside className="ws-context ws-diff-list">
          <div className="ws-versions">
            <div className="ws-panel-head">
              <h3>
                {changeIndices.length} change{changeIndices.length === 1 ? '' : 's'}
              </h3>
            </div>
            <div className="cmp-filters" role="tablist">
              {(
                [
                  ['all', 'All', changeIndices.length],
                  ['insert', 'Added', result.summary.insertions],
                  ['delete', 'Removed', result.summary.deletions],
                  ['change', 'Edited', result.summary.changes],
                ] as const
              ).map(([id, text, count]) => (
                <button
                  key={id}
                  type="button"
                  role="tab"
                  aria-selected={filter === id}
                  className={`cmp-filter${filter === id ? ' is-active' : ''}`}
                  onClick={() => {
                    setFilter(id)
                    setCurrent(0)
                  }}
                >
                  {text} <span>{count}</span>
                </button>
              ))}
            </div>
            <ul className="ws-version-list">
              {listed.map((opIndex, position) => {
                const op = result.ops[opIndex]
                return (
                  <li key={opIndex}>
                    <button
                      type="button"
                      className={`ws-diff-entry cmp-entry${opIndex === activeOp ? ' is-active' : ''}`}
                      onClick={() => goTo(position)}
                    >
                      <span className={`ws-diff-badge ws-diff-badge-${op.op}`}>{label(op)}</span>
                      <span className="ws-diff-entry-text">{preview(op)}</span>
                    </button>
                  </li>
                )
              })}
              {listed.length === 0 && <li className="ws-panel-empty">Nothing of this kind.</li>}
            </ul>
          </div>
        </aside>
      )}
    </div>
  )
}

function label(op: DiffOp): string {
  if (op.op === 'insert') return 'Added'
  if (op.op === 'delete') return 'Removed'
  return 'Edited'
}

function preview(op: DiffOp): string {
  const text = op.op === 'change' ? op.right : op.op === 'equal' ? '' : op.text
  return text.length > 90 ? `${text.slice(0, 90)}…` : text || '(empty line)'
}

/** One paragraph, rendered per mode. */
function Row({ op, mode, index, active }: { op: DiffOp; mode: Mode; index: number; active: boolean }) {
  const cls = active ? ' is-current' : ''
  if (op.op === 'equal') {
    // Unchanged text stays legible rather than being hidden: a redline with the
    // surrounding clause removed cannot be read as a document.
    return mode === 'split' ? (
      <div className="ws-diff-row">
        <p className="ws-diff-cell">{op.text}</p>
        <p className="ws-diff-cell">{op.text}</p>
      </div>
    ) : (
      <p className="ws-diff-para">{op.text}</p>
    )
  }

  if (op.op === 'insert') {
    return mode === 'split' ? (
      <div className={`ws-diff-row${cls}`} data-op={index}>
        <p className="ws-diff-cell ws-diff-cell-empty" aria-hidden="true" />
        <p className="ws-diff-cell ws-diff-ins">{op.text}</p>
      </div>
    ) : (
      <p className={`ws-diff-para ws-diff-ins${cls}`} data-op={index}>
        {op.text}
      </p>
    )
  }

  if (op.op === 'delete') {
    return mode === 'split' ? (
      <div className={`ws-diff-row${cls}`} data-op={index}>
        <p className="ws-diff-cell ws-diff-del">{op.text}</p>
        <p className="ws-diff-cell ws-diff-cell-empty" aria-hidden="true" />
      </div>
    ) : (
      <p className={`ws-diff-para ws-diff-del${cls}`} data-op={index}>
        {op.text}
      </p>
    )
  }

  // Changed: the words carry the detail, which is the only reason this view is
  // worth more than a list of removed and added paragraphs.
  return mode === 'split' ? (
    <div className={`ws-diff-row${cls}`} data-op={index}>
      <p className="ws-diff-cell ws-diff-touched">
        <Words words={op.words} side="left" />
      </p>
      <p className="ws-diff-cell ws-diff-touched">
        <Words words={op.words} side="right" />
      </p>
    </div>
  ) : (
    <p className={`ws-diff-para ws-diff-touched${cls}`} data-op={index}>
      <Words words={op.words} side="both" />
    </p>
  )
}

/**
 * Word runs for one side.
 *
 * The left pane shows the document as it was, so insertions are omitted there;
 * the right shows it as it is, so deletions are omitted. Inline shows both,
 * which is what makes it a redline.
 */
function Words({ words, side }: { words: WordOp[]; side: 'left' | 'right' | 'both' }) {
  return (
    <>
      {words.map((w, i) => {
        if (side === 'left' && w.op === 'insert') return null
        if (side === 'right' && w.op === 'delete') return null
        if (w.op === 'equal') return <span key={i}>{w.text}</span>
        return (
          <span key={i} className={w.op === 'insert' ? 'ws-word-ins' : 'ws-word-del'}>
            {w.text}
          </span>
        )
      })}
    </>
  )
}

function Skeleton() {
  return (
    <div className="ws-surface-host">
      <div className="ws-surface ws-diff-skeleton" aria-label="Comparing documents">
        {Array.from({ length: 7 }).map((_, i) => (
          <span key={i} className="ws-skeleton-line" style={{ width: `${88 - (i % 4) * 14}%` }} />
        ))}
      </div>
    </div>
  )
}
