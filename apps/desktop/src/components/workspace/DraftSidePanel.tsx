/**
 * The drafting side column.
 *
 * One column, several tabs, and the tabs depend on what is being drafted: a
 * Word document gets an outline, a workbook gets a cell inspector, a deck gets
 * a slide inspector. History and Details are common to all of them. The AI Assistant is app-wide (components/assistant).
 *
 * Nothing in here edits the document directly. Every change goes back up
 * through `onInsertText` / `onJumpTo`, so the patch queue stays the one path by
 * which a document is modified.
 */

import { useEffect, useMemo, useState } from 'react'
import {
  Bookmark as BookmarkIcon,
  BookMarked,
  BookText,
  Check,
  Columns2,
  FileStack,
  Library,
  MessageSquare,
  PencilLine,
  SquareArrowOutUpRight,
  Trash2,
  CornerDownLeft,
  Grid3x3,
  History,
  Info,
  ListTree,
  RectangleHorizontal,
  Search,
  Sparkles,
  X,
} from 'lucide-react'

import {
  DRAFT_STATUSES,
  DRAFT_STATUS_LABELS,
  formatBytes,
  pageImageUrl,
  type DraftPatch,
  type DraftSummary,
  type DraftStatus,
  type DraftViewModel,
} from '../../api/drafts'
import type { MatterSummary } from '../../api/matters'
import { listClauses, type Clause } from '../../api/library'
import {
  BOOKMARK_PREFIX,
  docxOutline,
  documentText,
  tabsFor,
  textStats,
  type PptxSelection,
  type SideTab,
  type XlsxSelection,
} from './draftText'

const TAB_META: Record<SideTab, { label: string; Icon: typeof Sparkles }> = {
  outline: { label: 'Outline', Icon: ListTree },
  clauses: { label: 'Clauses', Icon: BookText },
  cell: { label: 'Cell', Icon: Grid3x3 },
  slide: { label: 'Slide', Icon: RectangleHorizontal },
  details: { label: 'Details', Icon: Info },
  history: { label: 'History', Icon: History },
  pages: { label: 'Pages', Icon: FileStack },
  bookmarks: { label: 'Bookmarks', Icon: BookmarkIcon },
  annotations: { label: 'Annotations', Icon: MessageSquare },
}

/** Scroll the editor to a PDF page. */
function jumpToPage(page: number) {
  document.querySelector(`[data-page="${page}"]`)?.scrollIntoView({ behavior: 'smooth', block: 'start' })
}

function PagesPanel({ draft, model }: Pick<Props, 'draft' | 'model'>) {
  if (model.format !== 'pdf') return null
  return (
    <div className="dr-pad dr-thumbs">
      {model.pages.map(p => (
        <button key={p.index} type="button" className="dr-page-thumb" onClick={() => jumpToPage(p.index)}>
          <img src={pageImageUrl(draft.id, p.index, model.version, 0.35)} alt="" loading="lazy" style={{ aspectRatio: `${p.width} / ${p.height}` }} />
          <span>
            {p.index + 1}
            {p.annotations.length > 0 && <em> · {p.annotations.length}</em>}
          </span>
        </button>
      ))}
    </div>
  )
}

function BookmarksPanel({ model }: Pick<Props, 'model'>) {
  if (model.format !== 'pdf') return null
  const outline = model.bookmarks ?? []
  const mine = model.pages.flatMap(p =>
    p.annotations.filter(a => a.contents?.startsWith(BOOKMARK_PREFIX)).map(a => ({ page: p.index, title: a.contents!.slice(BOOKMARK_PREFIX.length) })),
  )
  return (
    <div className="dr-pad">
      <h4 className="dr-label" style={{ marginTop: 0 }}>Your bookmarks</h4>
      {mine.length === 0 ? (
        <p className="dr-empty">Use the Bookmark tool and click on a page to mark a place.</p>
      ) : (
        <ul className="dr-outline">
          {mine.map((b, i) => (
            <li key={i}>
              <button type="button" className="dr-outline-item" onClick={() => jumpToPage(b.page)}>
                <BookmarkIcon size={12} /> {b.title} <span className="dr-tag">p. {b.page + 1}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <h4 className="dr-label">Document outline</h4>
      {outline.length === 0 ? (
        <p className="dr-empty">This PDF has no built-in outline.</p>
      ) : (
        <ul className="dr-outline">
          {outline.map((b, i) => (
            <li key={i}>
              <button
                type="button"
                className="dr-outline-item"
                style={{ paddingLeft: 8 + b.level * 14 }}
                disabled={b.page == null}
                onClick={() => b.page != null && jumpToPage(b.page)}
              >
                {b.title || 'Untitled'}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

function AnnotationsPanel({ model, onPatch }: Pick<Props, 'model' | 'onPatch'>) {
  if (model.format !== 'pdf') return null
  const all = model.pages.flatMap(p => p.annotations.map(a => ({ page: p.index, a })))
  if (all.length === 0) return <p className="dr-empty dr-pad">No annotations yet. Pick a tool in the ribbon and drag on a page.</p>
  return (
    <ul className="dr-pad dr-annotations">
      {all.map(({ page, a }) => {
        const [r, g, b] = a.color ?? [255, 235, 59]
        return (
          <li key={`${page}-${a.index}`} className="dr-annotation">
            <span className="dr-annotation-dot" style={{ background: `rgb(${r},${g},${b})` }} />
            <button type="button" className="dr-annotation-body" onClick={() => jumpToPage(page)}>
              <span className="dr-annotation-kind">{a.contents?.startsWith(BOOKMARK_PREFIX) ? 'Bookmark' : a.kind}</span>
              {a.contents && <span className="dr-annotation-text">{a.contents.replace(BOOKMARK_PREFIX, '')}</span>}
              <span className="dr-tag">p. {page + 1}</span>
            </button>
            {onPatch && (
              <button type="button" className="ws-close" title="Remove" onClick={() => onPatch({ op: 'DeleteAnnotation', page, index: a.index })}>
                <Trash2 size={13} />
              </button>
            )}
          </li>
        )
      })}
    </ul>
  )
}


/**
 * Everything the page header used to carry.
 *
 * Filing, status and the document's own verbs are not editing tools, so they
 * do not belong in the band above the ribbon competing with the title. They
 * live here, in the Details tab, where the rest of "what this document is"
 * already is. Optional so CompareView and any other host can mount the panel
 * without them.
 */
export interface DraftDocActions {
  matters: MatterSummary[]
  busy: boolean
  onMatter: (id: number | null) => void
  onStatus: (status: DraftStatus) => void
  /** Absent on v1, where there is no previous version to compare against. */
  onCompare?: () => void
  compareLabel?: string
  onSaveAs: () => void
  onPublish: () => void
  onPromote: () => void
  onRename: () => void
  onDelete: () => void
}

interface Props {
  draft: DraftSummary
  model: DraftViewModel
  tab: SideTab
  onTab: (tab: SideTab) => void
  onClose: () => void
  versionKey: number
  onRestore: (version: number) => void
  /** Open the redline of this version against the current document. */
  onCompareVersion?: (version: number) => void
  /** Absent when the format has nowhere sensible to put inserted text. */
  onInsertText?: (text: string) => void
  onJumpTo: (addr: string) => void
  xlsxSelection: XlsxSelection | null
  pptxSelection: PptxSelection | null
  onPatch?: (...patches: DraftPatch[]) => void
  actions?: DraftDocActions
}

// -------------------------------------------------------------------- outline

function OutlinePanel({ model, onJumpTo }: Pick<Props, 'model' | 'onJumpTo'>) {
  const outline = useMemo(() => (model.format === 'docx' ? docxOutline(model.blocks) : []), [model])
  const stats = useMemo(() => textStats(documentText(model)), [model])

  return (
    <div className="dr-pad">
      <StatRow
        items={[
          ['Words', stats.words.toLocaleString()],
          ['Paragraphs', stats.paragraphs.toLocaleString()],
          ['Read', `${stats.readingMinutes} min`],
        ]}
      />
      <h4 className="dr-label">Headings</h4>
      {outline.length === 0 ? (
        <p className="dr-empty">No headings yet. Give a paragraph a Heading style and it appears here.</p>
      ) : (
        <ul className="dr-outline">
          {outline.map(entry => (
            <li key={entry.addr}>
              <button
                type="button"
                className="dr-outline-item"
                style={{ paddingLeft: `${8 + Math.max(0, entry.level - 1) * 14}px` }}
                onClick={() => onJumpTo(entry.addr)}
                title={entry.text}
              >
                {entry.text}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

// -------------------------------------------------------------------- clauses

function ClausesPanel({ onInsertText }: Pick<Props, 'onInsertText'>) {
  const [clauses, setClauses] = useState<Clause[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [filter, setFilter] = useState('')
  const [open, setOpen] = useState<number | null>(null)

  useEffect(() => {
    let live = true
    listClauses()
      .then(c => live && setClauses(c))
      .catch(e => live && setError(e instanceof Error ? e.message : String(e)))
    return () => {
      live = false
    }
  }, [])

  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase()
    return (clauses ?? []).filter(
      c => !q || c.title.toLowerCase().includes(q) || c.body.toLowerCase().includes(q),
    )
  }, [clauses, filter])

  return (
    <div className="dr-pad">
      <label className="dr-search">
        <Search size={13} strokeWidth={2} />
        <input value={filter} onChange={e => setFilter(e.target.value)} placeholder="Search clauses" />
      </label>
      {error && <p className="dr-empty">{error}</p>}
      {!error && clauses === null && <p className="dr-empty">Loading clauses…</p>}
      {clauses !== null && shown.length === 0 && (
        <p className="dr-empty">
          {clauses.length === 0 ? 'Your clause library is empty. Add clauses from the Library page.' : 'No clauses match.'}
        </p>
      )}
      <ul className="dr-clauses">
        {shown.map(c => (
          <li key={c.id} className={`dr-clause${open === c.id ? ' is-open' : ''}`}>
            <button type="button" className="dr-clause-head" onClick={() => setOpen(o => (o === c.id ? null : c.id))}>
              <span className="dr-clause-title">{c.title}</span>
              {c.jurisdiction && <span className="dr-tag">{c.jurisdiction}</span>}
            </button>
            <p className="dr-clause-body">{c.body}</p>
            {open === c.id && onInsertText && (
              <button type="button" className="dr-mini dr-mini-ink" onClick={() => onInsertText(c.body)}>
                <CornerDownLeft size={12} strokeWidth={2} />
                Insert at cursor
              </button>
            )}
          </li>
        ))}
      </ul>
    </div>
  )
}

// ----------------------------------------------------------------- inspectors

function StatRow({ items }: { items: [string, string][] }) {
  return (
    <div className="dr-stats">
      {items.map(([k, v]) => (
        <div key={k} className="dr-stat">
          <span className="dr-stat-value">{v}</span>
          <span className="dr-stat-key">{k}</span>
        </div>
      ))}
    </div>
  )
}

function Field({ k, v, mono }: { k: string; v: string; mono?: boolean }) {
  return (
    <div className="dr-field">
      <span className="dr-field-key">{k}</span>
      <span className={`dr-field-value${mono ? ' is-mono' : ''}`}>{v}</span>
    </div>
  )
}

function CellPanel({ model, selection }: { model: DraftViewModel; selection: XlsxSelection | null }) {
  if (model.format !== 'xlsx') return null
  const sheet = model.sheets.find(s => s.index === (selection?.sheet ?? 0)) ?? model.sheets[0]
  if (!sheet) return <p className="dr-empty">This workbook has no sheets.</p>
  const ref = selection?.ref ?? 'A1'
  const cell = sheet.cells[ref]

  const col = /^[A-Z]+/.exec(ref)?.[0] ?? 'A'
  const colValues = Object.entries(sheet.cells)
    .filter(([r, c]) => /^[A-Z]+/.exec(r)?.[0] === col && c.numeric)
    .map(([, c]) => Number(c.value))
    .filter(n => Number.isFinite(n))
  const sum = colValues.reduce((a, b) => a + b, 0)

  return (
    <div className="dr-pad">
      <div className="dr-cell-hero">
        <span className="dr-cell-ref">{ref}</span>
        <span className="dr-cell-sheet">{sheet.name}</span>
      </div>
      <Field k="Value" v={cell?.value || '-'} />
      <Field k="Formula" v={cell?.formula ? `=${cell.formula}` : '-'} mono />
      <Field k="Type" v={cell ? (cell.formula ? 'Formula' : cell.numeric ? 'Number' : 'Text') : 'Empty'} />

      <h4 className="dr-label">Column {col}</h4>
      <StatRow
        items={[
          ['Numbers', colValues.length.toLocaleString()],
          ['Sum', colValues.length ? sum.toLocaleString(undefined, { maximumFractionDigits: 2 }) : '-'],
          ['Average', colValues.length ? (sum / colValues.length).toLocaleString(undefined, { maximumFractionDigits: 2 }) : '-'],
        ]}
      />

      <h4 className="dr-label">Sheet</h4>
      <Field k="Used range" v={`${sheet.rows} rows × ${sheet.cols} columns`} />
      <Field k="Filled cells" v={Object.keys(sheet.cells).length.toLocaleString()} />
      <Field k="Formulas" v={Object.values(sheet.cells).filter(c => c.formula).length.toLocaleString()} />
      {sheet.merges.length > 0 && <Field k="Merged ranges" v={String(sheet.merges.length)} />}
    </div>
  )
}

const EMU_PER_INCH = 914_400
const inches = (emu: number) => `${(emu / EMU_PER_INCH).toFixed(2)}″`

function SlidePanel({ model, selection }: { model: DraftViewModel; selection: PptxSelection | null }) {
  if (model.format !== 'pptx') return null
  const slide = model.slides[selection?.slide ?? 0] ?? model.slides[0]
  if (!slide) return <p className="dr-empty">This presentation has no slides.</p>
  const shape = slide.shapes.find(s => s.addr === selection?.shape) ?? null
  const words = textStats(slide.shapes.flatMap(s => s.paragraphs.map(p => p.text)).join(' ')).words

  return (
    <div className="dr-pad">
      <StatRow
        items={[
          ['Slide', `${slide.index + 1} / ${model.slides.length}`],
          ['Shapes', String(slide.shapes.length)],
          ['Words', String(words)],
        ]}
      />
      <h4 className="dr-label">Selected shape</h4>
      {shape ? (
        <>
          <Field k="Name" v={shape.name} />
          <Field k="Kind" v={shape.placeholder ? `${shape.placeholder} placeholder` : shape.kind} />
          {shape.bbox ? (
            <>
              <Field k="Position" v={`${inches(shape.bbox.x)}, ${inches(shape.bbox.y)}`} mono />
              <Field k="Size" v={`${inches(shape.bbox.cx)} × ${inches(shape.bbox.cy)}`} mono />
            </>
          ) : (
            <Field k="Position" v="Inherited from layout" />
          )}
        </>
      ) : (
        <p className="dr-empty">Click a shape on the slide to inspect it.</p>
      )}
      <h4 className="dr-label">On this slide</h4>
      <ul className="dr-shapes">
        {slide.shapes.map(s => (
          <li key={s.addr} className={s.addr === shape?.addr ? 'is-active' : undefined}>
            <span className="dr-shape-name">{s.name}</span>
            <span className="dr-tag">{s.paragraphs.length ? 'Text' : s.kind === 'pic' ? 'Picture' : s.kind}</span>
          </li>
        ))}
      </ul>
    </div>
  )
}

/**
 * Filing, status and the document's verbs, at the foot of Details.
 *
 * Delete is last and is the only tinted control here: in the old ⋯ menu it sat
 * one item away from "Rename", and the two are not equally recoverable.
 */
function DocumentActions({ draft, actions }: { draft: DraftSummary; actions: DraftDocActions }) {
  return (
    <>
      {actions.matters.length > 0 && (
        <>
          <h4 className="dr-label">Filed under</h4>
          <select
            className="dr-side-select"
            value={draft.matter_id ?? ''}
            disabled={actions.busy}
            aria-label="Matter"
            onChange={e => actions.onMatter(e.target.value ? Number(e.target.value) : null)}
          >
            <option value="">Unfiled</option>
            {actions.matters.map(m => (
              <option key={m.id} value={m.id}>
                {m.name}
              </option>
            ))}
          </select>
        </>
      )}

      <h4 className="dr-label">Status</h4>
      <div className="dr-status-row" role="radiogroup" aria-label="Status">
        {DRAFT_STATUSES.map(st => (
          <button
            key={st}
            type="button"
            role="radio"
            aria-checked={draft.status === st}
            className={`dr-status-pick${draft.status === st ? ' is-on' : ''}`}
            disabled={actions.busy}
            onClick={() => draft.status !== st && actions.onStatus(st as DraftStatus)}
          >
            <span className={`dr-status-dot dr-status-${st}`} />
            {DRAFT_STATUS_LABELS[st]}
            {draft.status === st && <Check size={12} strokeWidth={2.25} className="dr-status-check" />}
          </button>
        ))}
      </div>

      <h4 className="dr-label">Actions</h4>
      <div className="dr-actions">
        {actions.onCompare && (
          <button type="button" className="dr-action" onClick={actions.onCompare} title={actions.compareLabel}>
            <Columns2 size={14} strokeWidth={1.75} />
            Compare with previous version
          </button>
        )}
        <button type="button" className="dr-action" onClick={actions.onSaveAs}>
          <SquareArrowOutUpRight size={14} strokeWidth={1.75} />
          Save a copy…
        </button>
        <button type="button" className="dr-action" onClick={actions.onPublish}>
          <Library size={14} strokeWidth={1.75} />
          Add to Vault
        </button>
        <button type="button" className="dr-action" onClick={actions.onPromote}>
          <BookMarked size={14} strokeWidth={1.75} />
          Save to library…
        </button>
        <button type="button" className="dr-action" onClick={actions.onRename}>
          <PencilLine size={14} strokeWidth={1.75} />
          Rename
        </button>
        <button type="button" className="dr-action dr-action-danger" onClick={actions.onDelete}>
          <Trash2 size={14} strokeWidth={1.75} />
          Delete
        </button>
      </div>
    </>
  )
}

function DetailsPanel({ draft, model, actions }: Pick<Props, 'draft' | 'model' | 'actions'>) {
  const stats = useMemo(() => textStats(documentText(model)), [model])
  const date = (iso: string) => {
    const d = new Date(iso)
    return Number.isNaN(d.getTime()) ? iso : d.toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' })
  }
  return (
    <div className="dr-pad">
      {model.format !== 'pdf' && (
        <StatRow
          items={[
            ['Words', stats.words.toLocaleString()],
            ['Characters', stats.characters.toLocaleString()],
            ['Read', `${stats.readingMinutes} min`],
          ]}
        />
      )}
      <h4 className="dr-label">Document</h4>
      <Field k="Format" v={draft.format.toUpperCase()} />
      <Field k="Version" v={`v${model.version}`} />
      <Field k="Created" v={date(draft.created_at)} />
      <Field k="Last edited" v={date(draft.updated_at)} />
      {model.format === 'txt' && (
        <>
          <h4 className="dr-label">File</h4>
          <Field k="Encoding" v={model.encoding} />
          <Field k="Line endings" v={model.line_ending} />
          <Field k="Byte order mark" v={model.bom ? 'Yes' : 'No'} />
          <Field k="Size" v={formatBytes(new Blob([model.content]).size)} />
        </>
      )}
      {model.format === 'xlsx' && (
        <>
          <h4 className="dr-label">Workbook</h4>
          <Field k="Sheets" v={model.sheets.map(s => s.name).join(', ')} />
        </>
      )}
      {model.format === 'pdf' && <Field k="Pages" v={String(model.pages.length)} />}
      {actions && <DocumentActions draft={draft} actions={actions} />}
    </div>
  )
}

// ---------------------------------------------------------------------- shell

export default function DraftSidePanel(props: Props) {
  const { draft, model, tab, onTab, onClose } = props
  const tabs = tabsFor(model.format)
  const current = tabs.includes(tab) ? tab : tabs[0]

  return (
    <aside className="dr-side" aria-label="Drafting tools">
      <div className="dr-side-head">
        <div className="dr-tabs" role="tablist">
          {tabs.map(t => {
            const { label, Icon } = TAB_META[t]
            return (
              <button
                key={t}
                type="button"
                role="tab"
                aria-selected={t === current}
                className={`dr-tab${t === current ? ' is-active' : ''}`}
                onClick={() => onTab(t)}
                title={label}
              >
                <Icon size={13} strokeWidth={1.9} />
                <span>{label}</span>
              </button>
            )
          })}
        </div>
        <button type="button" className="ws-close" onClick={onClose} aria-label="Close panel" title="Close panel">
          <X size={15} strokeWidth={1.75} />
        </button>
      </div>

      <div className="dr-side-body">
        {current === 'outline' && <OutlinePanel model={model} onJumpTo={props.onJumpTo} />}
        {current === 'clauses' && <ClausesPanel onInsertText={props.onInsertText} />}
        {current === 'cell' && <CellPanel model={model} selection={props.xlsxSelection} />}
        {current === 'slide' && <SlidePanel model={model} selection={props.pptxSelection} />}
        {current === 'details' && <DetailsPanel draft={draft} model={model} actions={props.actions} />}
        {current === 'pages' && <PagesPanel draft={draft} model={model} />}
        {current === 'bookmarks' && <BookmarksPanel model={model} />}
        {current === 'annotations' && <AnnotationsPanel model={model} onPatch={props.onPatch} />}
        {/* History is a tray beside the document now (VersionHistory). */}
      </div>
    </aside>
  )
}
