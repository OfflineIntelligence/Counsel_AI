/**
 * The library — precedents, templates and clauses.
 *
 * Lawyers do not draft from nothing. They start from the last one that worked,
 * which is why "new from precedent" is the most common way a document begins.
 *
 * # Two lists, because they are used differently
 *
 * A **precedent or template** is a FILE. You open it, and it becomes a new
 * draft — a copy, so editing it can never reach back into the firm's bank.
 *
 * A **clause** is TEXT. You never open it; you copy it into the document you
 * are already writing. That is why its whole body is on the card and the only
 * action is Copy.
 */

import { Fragment, useEffect, useMemo, useRef, useState } from 'react'
import '../DocumentViewer.css'
import { Copy, FilePlus2, LayoutGrid, List, Plus, Scale, Trash2, Upload, X } from 'lucide-react'

import {
  addToLibrary,
  createClause,
  deleteClause,
  listClauses,
  listLibrary,
  getLibraryItemText,
  newDraftFromLibrary,
  removeFromLibrary,
  LIBRARY_KIND_LABELS,
  type Clause,
  type LibraryItem,
  type LibraryKind,
} from '../../api/library'
import type { MatterSummary } from '../../api/matters'
import { EmptyPlace, fileTypeGroup, fileTypeName, ItemMenu, TypeFilter, type MenuEntry } from './FileBrowser'
import { FileBrandTile, FILE_ICON } from '../FileBrandIcon'
import UploadMenu from './UploadMenu'
import { pickFolders } from '../../utils/pickFolders'
import { useErrorNotification } from '../../contexts/NotificationContext'
import { useAssistant, useAssistantPage } from '../../contexts/AssistantContext'
import { useConfirm } from '../../contexts/confirm'

type Tab = 'documents' | 'clauses'

const LIBRARY_VIEW_KEY = 'ws-library-view'

function longDate(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return ''
  return date.toLocaleDateString('en-US', { month: 'long', day: 'numeric', year: 'numeric' })
}

/**
 * Precedents, templates and knowledge documents as tiles or a list, drawn
 * with the Workspace file browser's styles so both pages look the same.
 */
function ItemCollection({
  items,
  view,
  busy,
  selectedId,
  onSelect,
  onAsk,
  onStart,
  onRemove,
  previewText,
}: {
  items: LibraryItem[]
  view: 'grid' | 'list'
  busy: boolean
  /** The item the AI Assistant is reading. Click selects; double-click starts a draft. */
  selectedId: number | null
  onSelect: (item: LibraryItem) => void
  onAsk: (item: LibraryItem) => void
  onStart: (item: LibraryItem) => void
  onRemove: (item: LibraryItem) => void
  /** The selected item's text, shown right under its row; null while loading. */
  previewText?: string | null
}) {
  const menu = (item: LibraryItem): MenuEntry[] => [
    { label: 'New document from this', onSelect: () => !busy && onStart(item) },
    { label: 'Ask LARA about this', onSelect: () => onAsk(item) },
    'separator',
    { label: item.bundled ? 'Delete from this computer' : 'Remove from Library', onSelect: () => onRemove(item) },
  ]
  const kindLabel = (item: LibraryItem) => LIBRARY_KIND_LABELS[item.kind] ?? item.kind

  if (view === 'grid') {
    return (
      <div className="ws-grid-items ws-tiles ws-library-tiles">
        {items.map(item => (
          <div key={item.id} className={`ws-tile${selectedId === item.id ? ' ws-tile-selected' : ''}`} onDoubleClick={() => !busy && onStart(item)}>
            <div className="ws-tile-top">
              {item.bundled ? (
                <span className="ws-library-included" title="Included with Offline Counsel AI">
                  Included
                </span>
              ) : (
                <span />
              )}
              <ItemMenu label={item.title} vertical={false} entries={menu(item)} />
            </div>
            <button
              type="button"
              className="ws-tile-open"
              onClick={() => onSelect(item)}
              aria-pressed={selectedId === item.id}
              title={`Select ${item.title} · double-click for a new document`}
            >
              <span className="ws-tile-icon ws-tile-icon-file">
                <FileBrandTile format={item.format} size={FILE_ICON.grid} />
              </span>
              <span className="ws-tile-text">
                <span className="ws-tile-name">{item.title}</span>
                <span className="ws-tile-meta">
                  {kindLabel(item)} · {item.format.toUpperCase()}
                  {item.jurisdiction ? ` · ${item.jurisdiction}` : ''}
                </span>
              </span>
            </button>
          </div>
        ))}
      </div>
    )
  }

  return (
    <div className="ws-file-list ws-library-list" role="table" aria-label="Library">
      <div className="ws-list-head" role="row">
        <span role="columnheader" />
        <span role="columnheader">Name</span>
        <span role="columnheader">Kind</span>
        <span role="columnheader">Type</span>
        <span role="columnheader">Last modified</span>
        <span role="columnheader" />
      </div>
      <div className="ws-list-body">
        {items.map(item => (
          <Fragment key={item.id}>
          <div role="row" className={`ws-list-row${selectedId === item.id ? ' ws-list-selected' : ''}`} onDoubleClick={() => !busy && onStart(item)}>
            <span />
            <button
              type="button"
              className="ws-list-name"
              onClick={() => onSelect(item)}
              aria-pressed={selectedId === item.id}
              title={`Select ${item.title} · double-click for a new document`}
            >
              <span className="ws-list-icon ws-list-icon-file">
                <FileBrandTile format={item.format} size={FILE_ICON.list} />
              </span>
              <span className="ws-list-title">{item.title}</span>
              {item.bundled && <span className="ws-library-included">Included</span>}
            </button>
            <span className="ws-list-cell">{kindLabel(item)}</span>
            <span className="ws-list-cell">{fileTypeName(item.format)}</span>
            <span className="ws-list-cell">{longDate(item.updated_at)}</span>
            <span className="ws-list-actions">
              <ItemMenu label={item.title} vertical entries={menu(item)} />
            </span>
          </div>
          {/* The preview opens right under its row, as wide as the list; the
              rest of the list continues below it. */}
          {selectedId === item.id && (
            <div className="document-viewer-inline library-inline-preview" role="region" aria-label={`Preview of ${item.title}`}>
              <div className="document-viewer-header">
                <h2 className="modal-title" title={item.title}>{item.title}</h2>
              </div>
              <div className="document-viewer-body">
                {previewText == null ? (
                  <div className="document-viewer-status">Loading preview…</div>
                ) : previewText.trim() ? (
                  <pre className="text-preview">{previewText}</pre>
                ) : (
                  <div className="document-viewer-status">No text to preview in this document.</div>
                )}
              </div>
            </div>
          )}
          </Fragment>
        ))}
      </div>
    </div>
  )
}

interface Props {
  busy: boolean
  matters: MatterSummary[]
  /** Run a server change, surface its message, and resync the workspace. */
  run: (work: () => Promise<unknown>) => void
  /** Open the draft that a precedent just produced. */
  onOpenDraft: (id: number) => void
  /** Search text from the page's search bar; when given, the built-in filter field is hidden. */
  query?: string
  /** An item to select and scroll to (a source opened from chat). */
  focusItemId?: number | null
  onFocusHandled?: () => void
}

export default function LibraryView({ busy, matters, run, onOpenDraft, query, focusItemId, onFocusHandled }: Props) {
  const confirm = useConfirm()
  const [tab, setTab] = useState<Tab>('documents')
  const [items, setItems] = useState<LibraryItem[]>([])
  const [clauses, setClauses] = useState<Clause[]>([])
  const [error, setError] = useState<string | null>(null)
  useErrorNotification(error, "Couldn't load Library", () => setError(null))
  const [ownFilter, setFilter] = useState('')
  const filter = query ?? ownFilter
  // List view only: tiles and the grid/list switch were removed.
  const view = 'list' as 'grid' | 'list'

  // The library is its own server state — nothing else in the workspace reads
  // it, so loading it here keeps it out of the shared refresh.
  const reload = useMemo(
    () => async () => {
      try {
        const [nextItems, nextClauses] = await Promise.all([listLibrary(), listClauses()])
        setItems(nextItems)
        setClauses(nextClauses)
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e))
      }
    },
    [],
  )

  useEffect(() => {
    void reload()
  }, [reload])

  /** Every library change resyncs this view; only new drafts touch the rest. */
  const change = (work: () => Promise<unknown>) => {
    setError(null)
    void (async () => {
      try {
        await work()
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e))
      } finally {
        await reload()
      }
    })()
  }

  // ---- the selected item, which the AI Assistant reads
  const [selectedId, setSelectedId] = useState<number | null>(null)
  useEffect(() => {
    if (focusItemId == null || !items.some(i => i.id === focusItemId)) return
    setTab('documents')
    setSelectedId(focusItemId)
    onFocusHandled?.()
    requestAnimationFrame(() =>
      document.querySelector('.ws-tile-selected, .ws-list-selected')?.scrollIntoView({ block: 'center', behavior: 'smooth' }),
    )
  }, [focusItemId, items, onFocusHandled])
  const [selectedText, setSelectedText] = useState<{ id: number; text: string } | null>(null)
  const { ask } = useAssistant()
  const selected = items.find(i => i.id === selectedId) ?? null

  useEffect(() => {
    if (selectedId === null) return
    let live = true
    getLibraryItemText(selectedId)
      .then(r => live && setSelectedText({ id: r.id, text: r.text }))
      // Unreadable text still leaves the title and details for the assistant.
      .catch(() => live && setSelectedText({ id: selectedId, text: '' }))
    return () => {
      live = false
    }
  }, [selectedId])

  useEffect(() => {
    if (selectedId === null) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !(e.target as HTMLElement).closest('input, textarea, .ai-panel')) setSelectedId(null)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [selectedId])

  const selectedBody = selected && selectedText?.id === selected.id ? selectedText.text : ''
  useAssistantPage(
    selected
      ? {
          label: 'Library',
          title: selected.title,
          format: [LIBRARY_KIND_LABELS[selected.kind] ?? selected.kind, selected.format.toUpperCase(), selected.jurisdiction]
            .filter(Boolean)
            .join(' · '),
          text: selectedBody || undefined,
          suggestions: ['Summarise this precedent', 'When would I use this?', 'List the clauses I should adapt'],
        }
      : null,
  )

  const needle = filter.trim().toLowerCase()
  // File-type filter (PDF, Word, Excel…): empty means every type.
  const [types, setTypes] = useState<string[]>([])
  const typeCounts = useMemo(() => {
    const counts: Record<string, number> = {}
    for (const i of items) {
      const g = fileTypeGroup(i.format)
      counts[g] = (counts[g] ?? 0) + 1
    }
    return counts
  }, [items])
  const shownItems = items.filter(
    i =>
      (types.length === 0 || types.includes(fileTypeGroup(i.format))) &&
      (!needle ||
        i.title.toLowerCase().includes(needle) ||
        i.tags.some(t => t.includes(needle)) ||
        (i.jurisdiction ?? '').toLowerCase().includes(needle) ||
        (i.practice_area ?? '').toLowerCase().includes(needle)),
  )
  const shownClauses = needle
    ? clauses.filter(
        c =>
          c.title.toLowerCase().includes(needle) ||
          c.body.toLowerCase().includes(needle) ||
          (c.jurisdiction ?? '').toLowerCase().includes(needle),
      )
    : clauses

  const pickFiles = useRef<HTMLInputElement | null>(null)
  const pickFolder = useRef<HTMLInputElement | null>(null)

  /** Add every picked file as a precedent; a folder's files come in flat. */
  const addFiles = (files: File[]) => {
    if (files.length === 0) return
    change(async () => {
      for (const file of files) await addToLibrary(file, { kind: 'precedent' })
    })
  }

  const addFolder = () => {
    void (async () => {
      try {
        const files = await pickFolders()
        if (files === null) pickFolder.current?.click()
        else addFiles(files)
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e))
      }
    })()
  }

  return (
    <div className="ws-library">
      <input
        ref={pickFiles}
        type="file"
        multiple
        hidden
        onChange={e => {
          const files = Array.from(e.target.files ?? [])
          e.target.value = ''
          addFiles(files)
        }}
      />
      <input
        ref={pickFolder}
        type="file"
        multiple
        hidden
        {...({ webkitdirectory: '', directory: '' } as React.InputHTMLAttributes<HTMLInputElement>)}
        onChange={e => {
          const files = Array.from(e.target.files ?? [])
          e.target.value = ''
          addFiles(files)
        }}
      />

      <div className="ws-files-toolbar">
        <div className="ws-toolbar-start">
          {items.length > 0 && <TypeFilter value={types} onChange={setTypes} counts={typeCounts} />}
        </div>
        <div className="ws-toolbar-end">
          <div className="ws-toolbar-actions ws-library-actions">
            <UploadMenu
              className="ws-upload-btn"
              disabled={busy}
              onAddFiles={() => pickFiles.current?.click()}
              onAddFolder={addFolder}
            />
          </div>
        </div>
      </div>

      {shownItems.length === 0 ? (
        (needle || types.length > 0) && items.length > 0 ? (
          <p className="ws-panel-empty">Nothing matches that.</p>
        ) : items.length === 0 ? (
          <EmptyPlace place="Library" onImport={() => pickFiles.current?.click()} />
        ) : null
      ) : (
        <ItemCollection
          items={shownItems}
          view={view}
          busy={busy}
          selectedId={selectedId}
          previewText={selectedId != null && selectedText?.id === selectedId ? selectedText.text : null}
          onSelect={item => setSelectedId(id => (id === item.id ? null : item.id))}
          onAsk={item => {
            void (async () => {
              // Read the item before asking, so the question goes out with it.
              const r = await getLibraryItemText(item.id).catch(() => null)
              setSelectedId(item.id)
              setSelectedText({ id: item.id, text: r?.text ?? '' })
              // Let the selection register with the assistant first.
              window.setTimeout(() => ask(`Summarise “${item.title}” and tell me when I would use it.`), 60)
            })()
          }}
          onStart={item =>
            // The new draft belongs to the workspace, not to this view,
            // so it goes through `run` — that is what refreshes the
            // document list the user lands back in.
            run(async () => {
              const draft = await newDraftFromLibrary(item.id, { matterId: null })
              onOpenDraft(draft.id)
            })
          }
          onRemove={async item => {
            const message = item.bundled
              ? `"${item.title}" is included with Offline Counsel AI. Delete it from this computer? It will no longer appear in the Library or be available in chat, and app updates will not restore it.`
              : `Remove "${item.title}" from the library? It will no longer be available to chats or document search. This cannot be undone.`
            const ok = await confirm({
              title: item.bundled ? 'Delete included document?' : 'Remove from library?',
              message,
              confirmLabel: item.bundled ? 'Delete' : 'Remove',
            })
            if (ok) change(() => removeFromLibrary(item.id))
          }}
        />
      )}
    </div>
  )
}
function ItemCard({
  item,
  busy,
  matters,
  onStart,
  onRemove,
}: {
  item: LibraryItem
  busy: boolean
  matters: MatterSummary[]
  onStart: (matterId: number | null) => void
  onRemove: () => void
}) {
  const [matterId, setMatterId] = useState<number | null>(null)

  return (
    <article className="ws-library-card">
      <header className="ws-library-card-head">
        <span className="ws-library-kind">{LIBRARY_KIND_LABELS[item.kind] ?? item.kind}</span>
        <span className="ws-library-format">{item.format.toUpperCase()}</span>
        {item.bundled && (
          <span className="ws-library-included" title="Included with Offline Counsel AI">
            Included
          </span>
        )}
        <button
          type="button"
          className="ws-icon-btn ws-icon-danger"
          disabled={busy}
          title="Remove from the library"
          aria-label={`Remove ${item.title}`}
          onClick={onRemove}
        >
          <Trash2 size={14} strokeWidth={1.75} />
        </button>
      </header>

      <h3 className="ws-library-title">{item.title}</h3>

      <div className="ws-library-meta">
        {item.jurisdiction && <span className="ws-chip">{item.jurisdiction}</span>}
        {item.practice_area && <span className="ws-chip">{item.practice_area}</span>}
        {item.tags.map(t => (
          <span key={t} className="ws-chip ws-chip-tag">
            {t}
          </span>
        ))}
      </div>

      <footer className="ws-library-card-foot">
        {matters.length > 0 && (
          <select
            className="ws-library-matter"
            value={matterId ?? ''}
            aria-label="File the new document under a matter"
            onChange={e => setMatterId(e.target.value ? Number(e.target.value) : null)}
          >
            <option value="">Unfiled</option>
            {matters.map(m => (
              <option key={m.id} value={m.id}>
                {m.name}
              </option>
            ))}
          </select>
        )}
        <button
          type="button"
          className="ws-tool ws-tool-primary"
          disabled={busy}
          onClick={() => onStart(matterId)}
        >
          <FilePlus2 size={14} strokeWidth={1.75} />
          New from this
        </button>
      </footer>
    </article>
  )
}

function AddItem({
  busy,
  onAdd,
}: {
  busy: boolean
  onAdd: (file: File, kind: LibraryKind) => void
}) {
  const [kind, setKind] = useState<LibraryKind>('precedent')
  const [picker, setPicker] = useState<HTMLInputElement | null>(null)

  return (
    <div className="ws-library-add">
      <select
        className="ws-library-kind-select"
        value={kind}
        aria-label="What kind of document this is"
        onChange={e => setKind(e.target.value as LibraryKind)}
      >
        <option value="precedent">Precedent</option>
        <option value="template">Template</option>
      </select>
      <input
        ref={setPicker}
        type="file"
        hidden
        accept=".docx,.xlsx,.pptx,.txt,.md"
        onChange={e => {
          const file = e.target.files?.[0]
          // Cleared so picking the same file twice in a row still fires.
          e.target.value = ''
          if (file) onAdd(file, kind)
        }}
      />
      <button
        type="button"
        className="ws-tool"
        disabled={busy}
        onClick={() => picker?.click()}
        title="Add a file to the library"
      >
        <Upload size={14} strokeWidth={1.75} />
        Add
      </button>
    </div>
  )
}

function AddClause({
  busy,
  onAdd,
}: {
  busy: boolean
  onAdd: (title: string, body: string, jurisdiction: string | null) => void
}) {
  const [open, setOpen] = useState(false)
  const [title, setTitle] = useState('')
  const [body, setBody] = useState('')
  const [jurisdiction, setJurisdiction] = useState('')

  if (!open) {
    return (
      <button type="button" className="ws-tool" disabled={busy} onClick={() => setOpen(true)}>
        <Plus size={14} strokeWidth={2} />
        New clause
      </button>
    )
  }

  return (
    <form
      className="ws-clause-form"
      onSubmit={e => {
        e.preventDefault()
        if (!title.trim() || !body.trim()) return
        onAdd(title.trim(), body.trim(), jurisdiction.trim() || null)
        setTitle('')
        setBody('')
        setJurisdiction('')
        setOpen(false)
      }}
    >
      <div className="ws-clause-form-head">
        <Scale size={14} strokeWidth={1.75} />
        <h2>New clause</h2>
        <button type="button" className="ws-close" onClick={() => setOpen(false)} aria-label="Cancel">
          <X size={14} strokeWidth={1.75} />
        </button>
      </div>
      <label className="ws-field">
        <span>Name</span>
        <input
          autoFocus
          value={title}
          onChange={e => setTitle(e.target.value)}
          placeholder="Governing law: England &amp; Wales"
        />
      </label>
      <label className="ws-field">
        <span>Jurisdiction</span>
        <input
          value={jurisdiction}
          onChange={e => setJurisdiction(e.target.value)}
          placeholder="Optional"
        />
      </label>
      <label className="ws-field">
        <span>Text</span>
        <textarea
          rows={5}
          value={body}
          onChange={e => setBody(e.target.value)}
          placeholder="Paste the provision exactly as it should appear."
        />
      </label>
      <div className="ws-clause-form-foot">
        <button
          type="submit"
          className="ws-tool ws-tool-primary"
          disabled={busy || !title.trim() || !body.trim()}
        >
          Save clause
        </button>
      </div>
    </form>
  )
}
