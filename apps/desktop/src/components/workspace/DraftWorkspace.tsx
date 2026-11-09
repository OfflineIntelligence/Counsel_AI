/**
 * Document workspace shell: draft rail, editor host, context panel.
 *
 * Everything here is arranged around one loop. The editor renders the view
 * model, an edit becomes a typed patch addressed to one node, the patch queue
 * batches and sends it, and the response replaces the model. The shell owns the
 * model and the status; the editors own nothing but the caret.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  ArrowLeft,
  ArrowUp,
  Columns2,
  PanelRight,
  CornerLeftUp,
  FilePlus2,
  FileUp,
  ChevronDown,
  FileSpreadsheet,
  FileText,
  FolderPlus,
  Plus,
  Presentation,
  Search,
  SquareArrowOutUpRight,
  Trash2,
  Upload,
  X,
} from 'lucide-react'

import {
  createBlank,
  createFolder,
  createFromUpload,
  createFromVault,
  deleteDraft,
  deleteFolder,
  downloadDraft,
  getContent,
  moveDraft,
  moveFolder,
  publishToVault,
  renameDraft,
  renameFolder,
  restoreVersion,
  type DraftPatch,
  type DraftSummary,
  type DraftViewModel,
  searchWorkspace,
  splitSnippet,
  type DocRef,
  type SearchResponse,
  assignMatter,
  setDraftStatus,
  setDraftStarred,
} from '../../api/drafts'
import { promoteDraftToLibrary } from '../../api/library'
import { useErrorNotification, useNotifications } from '../../contexts/NotificationContext'
import VersionHistory from './VersionHistory'
import {
  EDITABLE_ACCEPT,
  isEditableFormat,
  MAX_EDITABLE_BYTES,
  tooLargeToEditMessage,
} from '../../editableFormats'
import DocxEditor from './editors/DocxEditor'
import CompareView from './CompareView'
import FileBrowser from './FileBrowser'
import PdfEditor from './editors/PdfEditor'
import PptxEditor from './editors/PptxEditor'
import TextEditor from './editors/TextEditor'
import XlsxEditor from './editors/XlsxEditor'
import { usePatchQueue, type SaveStatus } from './usePatchQueue'
import VaultPicker from './VaultPicker'
import UploadMenu from './UploadMenu'
import Presence from './Presence'
import { useNameDialog } from './NameDialog'
import { pickFolders } from '../../utils/pickFolders'
import { canImport, resolveVaultDocument, type VaultEntry } from './VaultPane'
import { useSelection, type SelectionItem } from './useSelection'
import { useWorkspaceData } from './useWorkspaceData'
import DraftSidePanel, { type DraftDocActions } from './DraftSidePanel'
import { documentText, type PptxSelection, type SideTab, type XlsxSelection } from './draftText'
import { RibbonActionsContext } from './editors/ribbonData'
import { DRAFT_ASSISTANT_SESSION_PREFIX } from '../../api/chat'
import { useAssistantPage } from '../../contexts/AssistantContext'
import './workspace.css'
import './drafting.css'
import { useConfirm } from '../../contexts/confirm'

/**
 * No props: the Workspace is a top-level view, left by choosing another
 * destination in the app rail. It had an `onClose` while a Back button existed
 * to call it; nothing calls it now, and a prop that is only ever ignored is
 * worse than no prop.
 */

/**
 * The header's right cluster while a selection exists.
 *
 * It REPLACES the import capsule rather than sitting beside it: a selection is
 * a mode, and offering "import a document" mid-selection invites an action that
 * would only clear it. Compare states why it is unavailable rather than just
 * greying out, because "why can't I click this" is otherwise unanswerable.
 */
function SelectionBar({
  selection,
  onCompare,
  onMoveToRoot,
  onDelete,
}: {
  selection: ReturnType<typeof useSelection>
  onCompare: () => void
  onMoveToRoot: () => void
  onDelete: () => void
}) {
  const canCompare = selection.drafts.length === 2 && selection.folders.length === 0
  const compareHint =
    selection.folders.length > 0
      ? 'Folders cannot be compared'
      : selection.drafts.length < 2
        ? 'Select two documents to compare'
        : 'Select exactly two documents'

  return (
    <>
      <span className="ws-selection-count">{selection.count} selected</span>

      <button
        type="button"
        className="ws-tool ws-tool-primary"
        onClick={onCompare}
        disabled={!canCompare}
        title={canCompare ? 'Compare the two selected documents' : compareHint}
      >
        <Columns2 size={14} strokeWidth={1.75} />
        Compare
      </button>

      <button
        type="button"
        className="ws-tool"
        onClick={onMoveToRoot}
        disabled={selection.drafts.length === 0}
        title="Move the selected documents back to the top level"
      >
        <CornerLeftUp size={14} strokeWidth={1.75} />
        Move out
      </button>

      <button type="button" className="ws-tool ws-tool-danger" onClick={onDelete}>
        <Trash2 size={14} strokeWidth={1.75} />
        Delete
      </button>

      <button
        type="button"
        className="ws-close"
        onClick={() => selection.clear()}
        aria-label="Clear selection"
        title="Clear selection (Esc)"
      >
        <X size={16} strokeWidth={1.75} />
      </button>
    </>
  )
}

/**
 * Content search results.
 *
 * Replaces the grid rather than sitting under it: two lists of documents, one
 * of which does not answer the question just asked, is harder to read than one.
 */
function SearchResults({
  results,
  query,
  searching,
  onOpen,
}: {
  results: SearchResponse
  query: string
  searching: boolean
  onOpen: (id: number) => void
}) {
  if (searching && results.hits.length === 0) {
    return <p className="ws-panel-empty">Searching…</p>
  }

  if (results.hits.length === 0) {
    return (
      <div className="ws-search-empty">
        <p className="ws-panel-empty">
          No documents contain <strong>{query}</strong>.
        </p>
        {results.pending_index > 0 && (
          <p className="ws-search-pending">
            {results.pending_index} older document
            {results.pending_index === 1 ? ' is' : 's are'} still being indexed. Search again in a
            moment.
          </p>
        )}
      </div>
    )
  }

  return (
    <div className="ws-search-results">
      <p className="ws-search-summary">
        {results.hits.length} result{results.hits.length === 1 ? '' : 's'}
        {results.pending_index > 0 && ` · ${results.pending_index} still being indexed`}
      </p>
      <ul className="ws-search-list">
        {results.hits.map(hit => (
          <li key={hit.draft_id}>
            <button type="button" className="ws-search-hit" onClick={() => onOpen(hit.draft_id)}>
              <span className="ws-search-hit-title">{hit.title}</span>
              <span className="ws-search-hit-snippet">
                {/* Real elements from delimited runs — never HTML from the
                    server, which would make every document body an injection
                    surface in exchange for bold text. */}
                {splitSnippet(hit.snippet).map((run, i) =>
                  run.match ? (
                    <mark key={i}>{run.text}</mark>
                  ) : (
                    <span key={i}>{run.text}</span>
                  ),
                )}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  )
}

/** The blank documents the backend can actually synthesise. */
function safeName(name: string): string {
  const cleaned = name.replace(/[<>:"/\\|?*\u0000-\u001f]/g, '_').replace(/[. ]+$/, '').trim()
  return cleaned || 'Untitled'
}

function fileNameFor(title: string, format: string): string {
  const ext = `.${format.toLowerCase()}`
  const base = safeName(title)
  return base.toLowerCase().endsWith(ext) ? base : `${base}${ext}`
}

const NEW_FORMATS = [
  { format: 'txt' as const, label: 'Text document', Icon: FileText },
  { format: 'docx' as const, label: 'Word document', Icon: FilePlus2 },
  { format: 'xlsx' as const, label: 'Excel workbook', Icon: FileSpreadsheet },
  { format: 'pptx' as const, label: 'PowerPoint deck', Icon: Presentation },
]

const ASSISTANT_SUGGESTIONS: Record<string, string[]> = {
  docx: ['Summarise this document', 'List the key obligations', 'Suggest a stronger title'],
  txt: ['Summarise this document', 'Tighten the wording', 'Draft a closing paragraph'],
  xlsx: ['Explain what this sheet tracks', 'Spot anomalies in the figures'],
  pptx: ['Summarise the deck', 'Suggest a clearer title for each slide'],
  pdf: ['What should I look for when reviewing this PDF?', 'How do I annotate for a client?'],
}

/**
 * The document title, edited in place.
 *
 * A Colab-style field: it reads as the heading until you click it, then it is
 * simply an input holding the same text at the same size, so nothing jumps.
 * Enter or blur commits, Escape abandons. The committed value is optimistic —
 * the heading shows what you typed while the rename is in flight, because a
 * title that snaps back to the old one for a moment reads as a failure.
 */
function TitleField({
  title,
  onRename,
}: {
  title: string
  onRename: (next: string) => Promise<void>
}) {
  const [editing, setEditing] = useState(false)
  const [value, setValue] = useState(title)
  const [pending, setPending] = useState<string | null>(null)
  const field = useRef<HTMLInputElement | null>(null)

  // A rename from anywhere else (the side panel, another window) wins once it
  // lands; while editing, what is being typed wins instead. Adjusted during
  // render rather than in an effect, so the field never paints one frame of
  // the stale title first.
  const [seen, setSeen] = useState(title)
  if (title !== seen) {
    setSeen(title)
    if (!editing) {
      setValue(title)
      setPending(null)
    }
  }

  useEffect(() => {
    if (!editing) return
    const el = field.current
    if (!el) return
    el.focus()
    el.select()
  }, [editing])

  const commit = () => {
    setEditing(false)
    const next = value.trim()
    if (!next || next === title) {
      setValue(title)
      return
    }
    setPending(next)
    void onRename(next).finally(() => setPending(null))
  }

  if (editing) {
    return (
      <input
        ref={field}
        className="dr-title-input"
        value={value}
        aria-label="Document title"
        onChange={e => setValue(e.target.value)}
        onBlur={commit}
        onKeyDown={e => {
          if (e.key === 'Enter') {
            e.preventDefault()
            commit()
          }
          if (e.key === 'Escape') {
            e.preventDefault()
            setValue(title)
            setEditing(false)
          }
        }}
      />
    )
  }

  const shown = pending ?? title
  return (
    <button
      type="button"
      className="dr-title-button"
      onClick={() => setEditing(true)}
      title={`${shown} — click to rename`}
    >
      <h1>{shown}</h1>
    </button>
  )
}

function statusText(status: SaveStatus, hasPending: boolean): string {
  switch (status.kind) {
    case 'saving':
      return 'Saving…'
    case 'pending':
      return 'Unsaved changes'
    case 'saved':
      return hasPending ? 'Saving…' : ''
    case 'error':
      return 'Not saved'
    default:
      return hasPending ? 'Saving…' : ''
  }
}

export default function DraftWorkspace({
  openDraftId,
  onDraftOpened,
  openFolderId,
  onFolderOpened,
}: {
  /** A document to open as soon as the Workspace shows, e.g. chosen on the Library page. */
  openDraftId?: number | null
  onDraftOpened?: () => void
  /** A folder to show as soon as the Workspace appears, e.g. a Space picked in the sidebar. */
  openFolderId?: number | null
  onFolderOpened?: () => void
} = {}) {
  const confirm = useConfirm()
  // Server state lives in the hook; this component owns only interaction
  // state from here down. `run` and `refreshList` keep their old names so the
  // call sites read the same as before.
  const {
    drafts,
    folders,
    matters,
    busy,
    error: loadError,
    setError: setLoadError,
    refresh: refreshList,
    mutate: run,
  } = useWorkspaceData()
  useErrorNotification(loadError, "Couldn't open draft", () => setLoadError(null))


  /**
   * The matter the document list is scoped to.
   *
   * `undefined` is every matter, `null` is Unfiled, a number is one matter.
   * Three states rather than two because "not filed anywhere" is a real answer
   * a user needs to be able to look at, not the absence of a filter.
   */
  const [matterScope, setMatterScope] = useState<number | null | undefined>(undefined)

  /** The folder being browsed. `null` is the root. */
  const [folderId, setFolderId] = useState<number | null>(null)
  const [query, setQuery] = useState('')
  const [results, setResults] = useState<SearchResponse | null>(null)
  const [searching, setSearching] = useState(false)
  const [ordered, setOrdered] = useState<SelectionItem[]>([])
  const [comparing, setComparing] = useState<{ left: DocRef; right: DocRef } | null>(null)
  const [newMenuOpen, setNewMenuOpen] = useState(false)
  const newMenuAnchor = useRef<HTMLDivElement | null>(null)
  const [activeId, setActiveId] = useState<number | null>(null)
  const [model, setModel] = useState<DraftViewModel | null>(null)
  // The document tools column (outline, clauses, history…). Closed by default:
  // the AI Assistant has its own app-wide panel now.
  const [sideOpen, setSideOpen] = useState(false)
  const [sideTab, setSideTab] = useState<SideTab>('outline')
  const [insertion, setInsertion] = useState<{ text: string; nonce: number } | null>(null)
  const [xlsxSelection, setXlsxSelection] = useState<XlsxSelection | null>(null)
  const [pptxSelection, setPptxSelection] = useState<PptxSelection | null>(null)
  const [versionKey, setVersionKey] = useState(0)
  // The version history is a tray beside the document, not a side-panel tab.
  const [historyOpen, setHistoryOpen] = useState(false)
  const [showVaultPicker, setShowVaultPicker] = useState(false)
  const filePicker = useRef<HTMLInputElement | null>(null)
  const folderPicker = useRef<HTMLInputElement | null>(null)

  const active = useMemo(
    () => drafts.find(d => d.id === activeId) ?? null,
    [drafts, activeId],
  )

  /**
   * The documents the grid is showing.
   *
   * Scoping to a matter flattens the view deliberately: folders are how ONE
   * matter is arranged inside, so showing the folder tree of every matter
   * beside one matter's documents would be showing two organising schemes at
   * once. Inside a matter you see its documents; the folder tree is the
   * unscoped view's job.
   */
  const scopedDrafts = useMemo(
    () =>
      matterScope === undefined
        ? drafts
        : drafts.filter(d => (matterScope === null ? d.matter_id == null : d.matter_id === matterScope)),
    [drafts, matterScope],
  )

  const scopedFolders = matterScope === undefined ? folders : []

  const scopeName =
    matterScope === undefined
      ? null
      : matterScope === null
        ? 'Unfiled'
        : (matters.find(m => m.id === matterScope)?.name ?? 'Matter')

  const adoptModel = useCallback((next: DraftViewModel) => {
    setModel(next)
    // The rail shows the current version and the list is ordered by recency,
    // so it has to follow every save.
    setVersionKey(k => k + 1)
    void refreshList()
  }, [refreshList])

  const { status, push, flush, commit, hasPending, discard, undo, redo, canUndo, canRedo } = usePatchQueue({
    draftId: activeId ?? 0,
    model,
    onModel: adoptModel,
  })
  // Every autosave says so in the notification capsule, once, for three
  // seconds. The tag keeps saves that land while it is showing from stacking.
  const { addNotification } = useNotifications()
  const savedAt = status.kind === 'saved' ? status.at : null
  useEffect(() => {
    if (savedAt === null) return
    addNotification({ type: 'success', title: 'Auto save', message: '', duration: 3000, tag: 'autosave' })
  }, [savedAt, addNotification])


  /**
   * Undo or redo, after taking focus out of the page.
   *
   * Editors never overwrite a focused field (that would eat typing), so the
   * field has to be let go first or the restored text would not appear.
   */
  const stepHistory = useCallback(
    (direction: 'undo' | 'redo') => {
      const el = document.activeElement
      if (el instanceof HTMLElement && el.closest('.ws-editor-host')) el.blur()
      void (direction === 'undo' ? undo() : redo())
    },
    [undo, redo],
  )

  const openDraft = useCallback(
    async (id: number) => {
      // Never switch away with work in flight — the queue is keyed by draft id,
      // and leaving edits behind would strand them against the wrong document.
      if (hasPending()) await flush()
      setActiveId(id)
      setModel(null)
      setLoadError(null)
      // A pending insert or question belongs to the document being left.
      setInsertion(null)
      try {
        setModel(await getContent(id))
      } catch (e) {
        setLoadError(e instanceof Error ? e.message : String(e))
      }
    },
    [flush, hasPending, setLoadError],
  )

  useEffect(() => {
    if (openFolderId == null) return
    setFolderId(openFolderId)
    onFolderOpened?.()
  }, [openFolderId, onFolderOpened])

  useEffect(() => {
    if (openDraftId == null) return
    void openDraft(openDraftId)
    onDraftOpened?.()
  }, [openDraftId, openDraft, onDraftOpened])

  const onPatch = useCallback((...patches: DraftPatch[]) => push(...patches), [push])

  /**
   * Leave the editor and return to the drafts list.
   *
   * Flushes first, for the same reason switching drafts does: the patch queue
   * is keyed by draft id, and a debounce still in flight would otherwise land
   * after the id has been cleared and be dropped.
   */
  const closeDraft = useCallback(async () => {
    if (hasPending()) await flush()
    setActiveId(null)
    setModel(null)
    setLoadError(null)
    void refreshList()
  }, [flush, hasPending, refreshList, setLoadError])

  // Tell the app shell when a document is open: the sidebar stays shut on this
  // screen, and clicking the rail twice sends 'oca-draft-back' to leave it.
  const editing = active !== null && active !== undefined
  useEffect(() => {
    if (!editing) return
    window.dispatchEvent(new CustomEvent('oca-draft-editor', { detail: true }))
    return () => {
      window.dispatchEvent(new CustomEvent('oca-draft-editor', { detail: false }))
    }
  }, [editing])
  useEffect(() => {
    const onBack = () => void closeDraft()
    window.addEventListener('oca-draft-back', onBack)
    return () => window.removeEventListener('oca-draft-back', onBack)
  }, [closeDraft])

  const selection = useSelection(ordered)

  /**
   * Run a content search.
   *
   * Deliberately on submit, not on every keystroke: each query can trigger a
   * backfill pass that parses documents, so firing one per character would put
   * real work behind a fast typist.
   */
  const runSearch = useCallback(async () => {
    const q = query.trim()
    if (!q) {
      setResults(null)
      return
    }
    setSearching(true)
    try {
      setResults(await searchWorkspace(q))
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e))
    } finally {
      setSearching(false)
    }
  }, [query, setLoadError])

  const clearSearch = useCallback(() => {
    setQuery('')
    setResults(null)
  }, [])

  // Results as you type, like every other page's search bar, but only once
  // typing pauses, so a fast typist sends one search rather than one per key.
  useEffect(() => {
    if (!query.trim()) return
    const timer = window.setTimeout(() => void runSearch(), 400)
    return () => window.clearTimeout(timer)
  }, [query, runSearch])

  /** Compare the two selected documents, oldest-modified on the left. */
  const compareSelected = useCallback(() => {
    if (selection.drafts.length !== 2) return
    const [a, b] = selection.drafts
      .map(id => drafts.find(d => d.id === id))
      .filter((d): d is DraftSummary => Boolean(d))
      // Older on the left, so the redline reads left-to-right as time passes.
      .sort((x, y) => Date.parse(x.updated_at) - Date.parse(y.updated_at))
    if (!a || !b) return
    setComparing({ left: { kind: 'draft', id: a.id }, right: { kind: 'draft', id: b.id } })
  }, [selection.drafts, drafts])

  /** Compare a draft's current version against the version before it. */
  const compareWithPrevious = useCallback(() => {
    if (!active || active.current_version < 2) return
    setComparing({
      left: { kind: 'draft', id: active.id, version: active.current_version - 1 },
      right: { kind: 'draft', id: active.id, version: active.current_version },
    })
  }, [active])

  /**
   * Selection keyboard: Ctrl/Cmd+A selects everything here, Escape clears.
   *
   * Bound only on the landing page — inside the editor Ctrl+A belongs to the
   * text, and stealing it there would be actively hostile. The guard on an
   * open draft, a search, and a focused field is what keeps it from firing
   * while someone is selecting text in the search box.
   */
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && selection.count > 0) {
        selection.clear()
        return
      }

      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'a') {
        if (activeId !== null || results) return
        const el = document.activeElement
        const typing =
          el instanceof HTMLInputElement ||
          el instanceof HTMLTextAreaElement ||
          (el instanceof HTMLElement && el.isContentEditable)
        if (typing) return

        e.preventDefault()
        selection.selectAll()
      }
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [selection, activeId, results])

  // The New-document menu closes on an outside click or Escape. `mousedown`
  // rather than `click`, so the menu is gone before the thing underneath it
  // reacts.
  useEffect(() => {
    if (!newMenuOpen) return
    const onPointer = (e: MouseEvent) => {
      if (!newMenuAnchor.current?.contains(e.target as Node)) setNewMenuOpen(false)
    }
    const onEsc = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setNewMenuOpen(false)
    }
    document.addEventListener('mousedown', onPointer)
    document.addEventListener('keydown', onEsc)
    return () => {
      document.removeEventListener('mousedown', onPointer)
      document.removeEventListener('keydown', onEsc)
    }
  }, [newMenuOpen])

  // Ctrl+S saves and cuts a version, matching every other editor the user has.
  // Ctrl+Z / Ctrl+Y (and Ctrl+Shift+Z) undo and redo saved edits.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return
      const key = e.key.toLowerCase()
      if (key === 's') {
        e.preventDefault()
        void flush()
        return
      }
      if (activeId === null || (key !== 'z' && key !== 'y')) return
      // Small inputs (font box, formula bar, find, dialogs) keep their own
      // native undo; the document itself uses the document's history.
      const target = e.target as HTMLElement | null
      if (target instanceof HTMLInputElement || target?.closest('.rb-modal, .dr-side')) return
      e.preventDefault()
      stepHistory(key === 'y' || e.shiftKey ? 'redo' : 'undo')
    }
    document.addEventListener('keydown', onKeyDown)
    return () => document.removeEventListener('keydown', onKeyDown)
  }, [flush, activeId, stepHistory])

  const handleUpload = useCallback(
    async (files: File[]) => {
      const problems: string[] = []
      const accepted = files.filter(file => {
        // Checked here as well as on the server. An oversized file would
        // otherwise be refused by the 50 MB body limit BEFORE any handler runs,
        // which produces a bare 413 with no message the user can act on.
        if (file.size > MAX_EDITABLE_BYTES) {
          problems.push(tooLargeToEditMessage(file.name, file.size))
          return false
        }
        if (!isEditableFormat(file.name)) {
          problems.push(
            `"${file.name}" cannot be edited here. Word, Excel, PowerPoint, PDF and text files can.`,
          )
          return false
        }
        return true
      })
      if (accepted.length === 0) {
        if (problems.length > 0) setLoadError(problems.join(' '))
        return
      }

      await run(async () => {
        for (const file of accepted) {
          const draft = await createFromUpload(file)
          // An import lands where the user is looking, not at the root.
          if (folderId !== null) await moveDraft(draft.id, folderId)
        }
      })
      if (problems.length > 0) setLoadError(problems.join(' '))
    },
    [folderId, run, setLoadError],
  )

  /**
   * Upload a whole folder from this computer into the folder being browsed,
   * recreating its subfolders. Files the editor cannot open are skipped and
   * listed afterwards.
   */
  const handleUploadFolder = useCallback(
    async (files: File[]) => {
      const problems: string[] = []
      await run(async () => {
        const created = new Map<string, number | null>([['', folderId]])
        const folderFor = async (dir: string): Promise<number | null> => {
          if (created.has(dir)) return created.get(dir)!
          const cut = dir.lastIndexOf('/')
          const parent = await folderFor(cut === -1 ? '' : dir.slice(0, cut))
          const folder = await createFolder(dir.slice(cut + 1), parent)
          created.set(dir, folder.id)
          return folder.id
        }
        for (const file of files) {
          const relative = (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name
          if (file.size > MAX_EDITABLE_BYTES) {
            problems.push(tooLargeToEditMessage(file.name, file.size))
            continue
          }
          if (!isEditableFormat(file.name)) {
            problems.push(`"${file.name}"`)
            continue
          }
          const dir = relative.includes('/') ? relative.slice(0, relative.lastIndexOf('/')) : ''
          const target = await folderFor(dir)
          const draft = await createFromUpload(file)
          if (target !== null) await moveDraft(draft.id, target)
        }
      })
      if (problems.length > 0) {
        setLoadError(
          `${problems.length} file${problems.length === 1 ? ' was' : 's were'} not added (only Word, Excel, PowerPoint, PDF and text files up to the size limit can be edited): ${problems.join(', ')}`,
        )
      }
    },
    [folderId, run, setLoadError],
  )

  const [nameDialog, askName] = useNameDialog()

  /**
   * Start a blank document in the folder currently being browsed.
   *
   * Creation and placement are two calls because the create endpoints predate
   * folders and take no folder. Doing the move here rather than widening four
   * separate creation routes keeps the rule in one place: a new document lands
   * where the user is looking.
   */
  const handleNewDraft = useCallback(
    async (format: 'txt' | 'docx' | 'xlsx' | 'pptx', label: string) => {
      const title = await askName({ title: `New ${label}`, initial: `Untitled ${label.toLowerCase()}` })
      if (!title) return

      // `run` owns busy and error and resyncs afterwards; the id is captured so
      // the new document can be opened once the list has caught up with it.
      let created: number | null = null
      await run(async () => {
        const draft = await createBlank(title, format)
        if (folderId !== null) await moveDraft(draft.id, folderId)
        created = draft.id
      })
      if (created !== null) await openDraft(created)
    },
    [askName, folderId, run, openDraft],
  )

  const handleNewFolder = useCallback(async () => {
    const name = await askName({ title: 'Create Folder', initial: 'New folder' })
    if (!name) return
    void run(() => createFolder(name, folderId))
  }, [askName, folderId, run])

  /**
   * Save a copy through the OS dialog.
   *
   * Needs `dialog:allow-save` and `fs:allow-write-file` in the Tauri
   * capabilities — without them this fails as a caught console error and looks
   * to the user like nothing happened at all.
   */
  const handleSaveAs = useCallback(async () => {
    if (!active) return
    if (hasPending()) await flush()
    try {
      const { save } = await import('@tauri-apps/plugin-dialog')
      const path = await save({ defaultPath: `${active.title}.${active.format}` })
      if (!path) return
      const blob = await downloadDraft(active.id)
      const bytes = new Uint8Array(await blob.arrayBuffer())
      const { writeFile } = await import('@tauri-apps/plugin-fs')
      await writeFile(path, bytes)
    } catch (e) {
      setLoadError(
        `The file could not be saved: ${e instanceof Error ? e.message : String(e)}`,
      )
    }
  }, [active, flush, hasPending, setLoadError])

  const [vaultMode, setVaultMode] = useState(false)
  const exitVault = useCallback(() => setVaultMode(false), [])

  /**
   * Copy what was picked in "From Vault" mode into the folder being browsed.
   * A picked folder comes across whole, as a workspace folder of the same
   * name; anything inside a picked folder is not copied a second time.
   */
  const handleImportFromVault = useCallback(
    async (entries: VaultEntry[]) => {
      const contains = (parent: VaultEntry, id: number): boolean =>
        (parent.children ?? []).some(c => c.id === id || contains(c, id))
      const roots = entries.filter(
        e => !entries.some(p => p.isDirectory && p.id !== e.id && contains(p, e.id)),
      )

      let skipped = 0
      const copyInto = async (entry: VaultEntry, target: number | null): Promise<void> => {
        if (entry.isDirectory) {
          const folder = await createFolder(entry.name, target)
          for (const child of entry.children ?? []) await copyInto(child, folder.id)
        } else if (canImport(entry)) {
          const draft = await createFromVault(await resolveVaultDocument(entry.id))
          if (target !== null) await moveDraft(draft.id, target)
        } else {
          skipped += 1
        }
      }

      await run(async () => {
        for (const entry of roots) await copyInto(entry, folderId)
      })
      setVaultMode(false)
      if (skipped > 0) {
        setLoadError(
          `${skipped} file${skipped === 1 ? ' was' : 's were'} not added: only Word, Excel, PowerPoint, PDF and text files can be edited here.`,
        )
      }
    },
    [folderId, run, setLoadError],
  )

  const handleDownloadDraft = useCallback(
    async (id: number) => {
      const draft = drafts.find(d => d.id === id)
      if (!draft) return
      try {
        const { save } = await import('@tauri-apps/plugin-dialog')
        const path = await save({ defaultPath: fileNameFor(draft.title, draft.format) })
        if (!path) return
        const blob = await downloadDraft(draft.id)
        const { writeFile } = await import('@tauri-apps/plugin-fs')
        await writeFile(path, new Uint8Array(await blob.arrayBuffer()))
      } catch (e) {
        setLoadError(`The file could not be downloaded: ${e instanceof Error ? e.message : String(e)}`)
      }
    },
    [drafts, setLoadError],
  )

  const handleDownloadFolder = useCallback(
    async (id: number) => {
      const root = folders.find(f => f.id === id)
      if (!root) return
      try {
        const { open } = await import('@tauri-apps/plugin-dialog')
        const picked = await open({ directory: true, recursive: true, title: `Download "${root.name}" to` })
        if (!picked || Array.isArray(picked)) return
        const { mkdir, writeFile } = await import('@tauri-apps/plugin-fs')
        const { join } = await import('@tauri-apps/api/path')

        const writeTree = async (folderId: number, name: string, parentPath: string) => {
          const dir = await join(parentPath, safeName(name))
          await mkdir(dir, { recursive: true })
          for (const draft of drafts.filter(d => (d.folder_id ?? null) === folderId)) {
            const blob = await downloadDraft(draft.id)
            await writeFile(
              await join(dir, fileNameFor(draft.title, draft.format)),
              new Uint8Array(await blob.arrayBuffer()),
            )
          }
          for (const child of folders.filter(f => f.parent_id === folderId)) {
            await writeTree(child.id, child.name, dir)
          }
        }

        await writeTree(root.id, root.name, picked)
      } catch (e) {
        setLoadError(`The folder could not be downloaded: ${e instanceof Error ? e.message : String(e)}`)
      }
    },
    [drafts, folders, setLoadError],
  )

  const handlePublish = useCallback(async () => {
    if (!active) return
    if (hasPending()) await flush()
    const ok = await confirm({
      title: `Add "${active.title}" to the Vault?`,
      message: 'A copy is saved as a new Vault document.',
      confirmLabel: 'Add',
      danger: false,
    })
    if (!ok) return
    try {
      await publishToVault(active.id)
      setLoadError(null)
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e))
    }
  }, [active, flush, hasPending, setLoadError])

  /**
   * Promote the document as it now reads.
   *
   * Pending edits are flushed first, because "save this to the library" means
   * the version on screen — promoting the last saved one would quietly bank a
   * document the user had already moved past.
   */
  const handlePromote = useCallback(async () => {
    if (!active) return
    if (hasPending()) await flush()
    const title = await askName({
      title: 'Save to library',
      label: 'Name',
      initial: active.title,
      confirmLabel: 'Save',
    })
    if (!title) return
    // Always a precedent: a document someone wrote and is now banking IS "one
    // that worked". Templates are blank house forms, which arrive through the
    // Library view's own Add control where the kind is an explicit choice.
    try {
      await promoteDraftToLibrary(active.id, { title, kind: 'precedent' })
      setLoadError(null)
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e))
    }
  }, [active, flush, hasPending, setLoadError])

  /** One rename path, used by the title field and by the panel's Rename. */
  const renameTo = useCallback(
    async (title: string) => {
      if (!active) return
      const next = title.trim()
      if (!next || next === active.title) return
      try {
        await renameDraft(active.id, next)
        await refreshList()
      } catch (e) {
        setLoadError(e instanceof Error ? e.message : String(e))
      }
    },
    [active, refreshList, setLoadError],
  )

  const handleRename = useCallback(async () => {
    if (!active) return
    const title = await askName({
      title: 'Rename document',
      label: 'Name',
      initial: active.title,
      confirmLabel: 'Rename',
    })
    if (title !== null) await renameTo(title)
  }, [active, renameTo, askName])

  const handleDelete = useCallback(async () => {
    if (!active) return
    const ok = await confirm({
      title: `Delete "${active.title}"?`,
      // The title asks the question; this says the one thing the title cannot,
      // which is that there is no way back. Naming the document again here, and
      // listing every consequence, made a two-word decision look like a form.
      message: 'This cannot be undone.',
      confirmLabel: 'Delete',
    })
    if (!ok) {
      return
    }
    try {
      // Before the request, so a debounce already in flight cannot fire against
      // a draft that is about to stop existing.
      discard()
      await deleteDraft(active.id)
      setActiveId(null)
      setModel(null)
      await refreshList()
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e))
    }
  }, [active, refreshList, discard, setLoadError, confirm])

  // ---------------------------------------------------------- browser actions

  const handleBrowserRenameDraft = useCallback(
    (id: number, title: string) => void run(() => renameDraft(id, title)),
    [run],
  )

  const handleBrowserDeleteDraft = useCallback(
    (id: number) => {
      // A pending debounce for THIS draft would otherwise fire against a row
      // that is about to stop existing.
      if (id === activeId) discard()
      void run(async () => {
        await deleteDraft(id)
        if (id === activeId) {
          setActiveId(null)
          setModel(null)
        }
      })
    },
    [activeId, discard, run],
  )

  const handleDeleteFolder = useCallback(
    (id: number) => {
      // Deleting a folder takes its whole subtree. Standing anywhere inside
      // that subtree — not just in the folder itself — leaves the view pointed
      // at something that no longer exists, with a breadcrumb that cannot be
      // rebuilt. Walk up from where we are and step back to the root if the
      // doomed folder is on the path.
      const byId = new Map(folders.map(f => [f.id, f]))
      let cursor = folderId
      let inside = false
      for (let i = 0; cursor != null && i <= folders.length; i++) {
        if (cursor === id) {
          inside = true
          break
        }
        cursor = byId.get(cursor)?.parent_id ?? null
      }

      void run(async () => {
        await deleteFolder(id)
        if (inside) setFolderId(null)
      })
    },
    [folderId, folders, run],
  )

  const handleRestore = useCallback(
    async (versionNo: number) => {
      if (!active) return
      try {
        const result = await restoreVersion(active.id, versionNo)
        adoptModel(result.model)
      } catch (e) {
        setLoadError(e instanceof Error ? e.message : String(e))
      }
    },
    [active, adoptModel, setLoadError],
  )

  /**
   * Put text from the side column into the document, wherever that means for
   * the format: at the caret in a text file, after the last-focused paragraph
   * in Word, into the selected cell in a workbook.
   */
  // Every editor takes the text and puts it where its own caret is.
  const insertText = useCallback((text: string) => {
    if (text) setInsertion({ text, nonce: Date.now() })
  }, [])

  const onCommit = useCallback((...patches: DraftPatch[]) => commit(...patches), [commit])
  const onEditorError = useCallback((message: string) => setLoadError(message), [setLoadError])

  const jumpTo = useCallback((addr: string) => {
    const el = document.querySelector<HTMLElement>(`[data-block="${CSS.escape(addr)}"]`)
    if (!el) return
    el.scrollIntoView({ behavior: 'smooth', block: 'center' })
    el.focus({ preventScroll: true })
  }, [])

  const onXlsxSelect = useCallback((sheet: number, ref: string) => setXlsxSelection({ sheet, ref }), [])
  const onPptxSelect = useCallback((slide: number, shape: string | null) => setPptxSelection({ slide, shape }), [])

  const openSide = useCallback((tab: SideTab) => {
    setSideTab(tab)
    setSideOpen(true)
  }, [])

  // The open document is what Aurora reads while a draft is open, and each
  // document keeps its own conversation. Built once: the composer answers from
  // it, and it is also registered so anything else asking about this page
  // (the rail button) lands on the same context.
  const documentBody = useMemo(() => documentText(model), [model])
  const assistantPage = useMemo(
    () => ({
      label: 'Draft',
      title: active?.title,
      format: active?.format.toUpperCase(),
      text: documentBody || undefined,
      thread: active ? `${DRAFT_ASSISTANT_SESSION_PREFIX}${active.id}` : undefined,
      onInsertText: model && (model.format === 'pptx' || model.format === 'pdf') ? undefined : insertText,
      suggestions: model ? ASSISTANT_SUGGESTIONS[model.format] : undefined,
    }),
    [active, model, documentBody, insertText],
  )
  useAssistantPage(active && model ? assistantPage : null)

  // Undo, redo, History and Save live in the ribbon of every editor.
  const ribbonActions = useMemo(
    () => ({
      onSave: () => void flush(),
      onHistory: () => setHistoryOpen(o => !o),
      historyOpen,
      saving: status.kind === 'saving',
      onUndo: () => stepHistory('undo'),
      onRedo: () => stepHistory('redo'),
      canUndo: canUndo && !!model,
      canRedo: canRedo && !!model,
    }),
    [flush, historyOpen, status.kind, stepHistory, canUndo, canRedo, model],
  )

  /**
   * Filing, status and the document's verbs, handed to the tools panel.
   *
   * These used to be a ⋯ menu in the header. They are not editing tools and
   * they do not belong above the ribbon, so the panel owns them now and the
   * header is left with the three things it is actually for: where you came
   * from, what this is called, and whether it is saved.
   */
  const docActions = useMemo<DraftDocActions | undefined>(() => {
    if (!active) return undefined
    return {
      matters,
      busy,
      onMatter: id => void run(() => assignMatter(active.id, id)),
      onStatus: st => void run(() => setDraftStatus(active.id, st)),
      onCompare: active.current_version > 1 ? compareWithPrevious : undefined,
      compareLabel:
        active.current_version > 1
          ? `Compare v${active.current_version - 1} with v${active.current_version}`
          : undefined,
      onSaveAs: () => void handleSaveAs(),
      onPublish: () => void handlePublish(),
      onPromote: () => void handlePromote(),
      onRename: () => void handleRename(),
      onDelete: () => void handleDelete(),
    }
  }, [
    active,
    matters,
    busy,
    run,
    compareWithPrevious,
    handleSaveAs,
    handlePublish,
    handlePromote,
    handleRename,
    handleDelete,
  ])

  const editor = () => {
    if (!model || !active) return null
    switch (model.format) {
      case 'docx':
        return (
          <DocxEditor
            blocks={model.blocks}
            styles={model.styles}
            defaults={model.defaults}
            onPatch={onPatch}
            onCommit={onCommit}
            insert={insertion}
            onError={onEditorError}
          />
        )
      case 'xlsx':
        return (
          <XlsxEditor
            sheets={model.sheets}
            onPatch={onPatch}
            onCommit={onCommit}
            onSelectionChange={onXlsxSelect}
            insert={insertion}
            onError={onEditorError}
          />
        )
      case 'pptx':
        return (
          <PptxEditor
            slides={model.slides}
            onPatch={onPatch}
            onCommit={onCommit}
            onSelectionChange={onPptxSelect}
            onError={onEditorError}
          />
        )
      case 'pdf':
        return (
          <PdfEditor
            draftId={active.id}
            version={model.version}
            pages={model.pages}
            onPatch={onPatch}
            onCommit={onCommit}
            onError={onEditorError}
          />
        )
      case 'txt':
        return (
          <TextEditor
            content={model.content}
            encoding={model.encoding}
            lineEnding={model.line_ending}
            bom={model.bom}
            onPatch={onPatch}
            insert={insertion}
          />
        )
      default:
        return <div className="ws-empty">This format has no editor yet.</div>
    }
  }

  // A comparison takes over the whole view: it is a reading task, and framing
  // it inside the browser would leave two documents competing for the width the
  // redline needs.
  if (comparing) {
    return (
      <div className="ws-root">
      {nameDialog}
        <main className="ws-main">
          <CompareView
            // Remount when the pair changes, so the view never shows one
            // comparison's result while another is loading.
            key={`${comparing.left.id}:${comparing.left.version ?? 'cur'}-${comparing.right.id}:${comparing.right.version ?? 'cur'}`}
            left={comparing.left}
            right={comparing.right}
            onClose={() => setComparing(null)}
            onChange={(left, right) => setComparing({ left, right })}
          />
        </main>
      </div>
    )
  }

  const createFolderButton = (
    <div className="ws-toolbar-actions">
      <UploadMenu
        className="ws-upload-btn"
        disabled={busy}
        onAddFiles={() => filePicker.current?.click()}
        onAddFolder={() => {
          void (async () => {
            try {
              const files = await pickFolders()
              if (files === null) folderPicker.current?.click()
              else if (files.length > 0) await handleUploadFolder(files)
            } catch (e) {
              setLoadError(`Folders could not be read: ${e instanceof Error ? e.message : String(e)}`)
            }
          })()
        }}
        onCreateFolder={() => void handleNewFolder()}
      />
    </div>
  )

  return (
    <div className="ws-root">
      {nameDialog}
      {showVaultPicker && (
        <VaultPicker
          onCancel={() => setShowVaultPicker(false)}
          onOpened={draft => {
            setShowVaultPicker(false)
            void (async () => {
              if (folderId !== null) await moveDraft(draft.id, folderId)
              await refreshList()
            })()
          }}
        />
      )}
      {/* Outside both branches: the picker must survive the switch from the
          landing page to the editor, which happens while its change event is
          still in flight. */}
      <input
        ref={filePicker}
        type="file"
        accept={EDITABLE_ACCEPT}
        multiple
        hidden
        onChange={e => {
          const files = Array.from(e.target.files ?? [])
          // Reset so picking the same files twice in a row still fires.
          e.target.value = ''
          if (files.length > 0) void handleUpload(files)
        }}
      />
      {/* Folder picker: no accept filter, since browsers ignore it for a
          directory; unsupported files are skipped and reported instead. */}
      <input
        ref={folderPicker}
        type="file"
        multiple
        hidden
        {...({ webkitdirectory: '', directory: '' } as React.InputHTMLAttributes<HTMLInputElement>)}
        onChange={e => {
          const files = Array.from(e.target.files ?? [])
          e.target.value = ''
          if (files.length > 0) void handleUploadFolder(files)
        }}
      />

      <main className="ws-main">
        {active ? (
          <>
            <header className="ws-header dr-header">
              {/* The only route back to the list, pinned to the left edge so
                  it never moves with the title. */}
              <button
                type="button"
                className="ws-tool ws-back"
                onClick={() => void closeDraft()}
                title="Back to drafts"
              >
                <ArrowLeft size={15} strokeWidth={1.75} />
                Back
              </button>
              {/* Badge and title in a fixed-width centre slot: renaming swaps
                  the label for a field of the same size, so nothing shifts. */}
              <div className="ws-header-title dr-header-center">
                <span className={`dr-format-badge dr-format-${active.format}`}>
                  {active.format.toUpperCase()}
                </span>
                <div className="dr-title-stack">
                  <TitleField title={active.title} onRename={renameTo} />
                  {/* One caption, and it says what the document does rather
                      than where it is filed: saving is the thing you need to
                      trust, filing lives in the tools panel. */}
                  {statusText(status, hasPending()) && <span
                    className={`dr-save${
                      status.kind === 'error'
                        ? ' dr-save-error'
                        : status.kind === 'saving' || hasPending()
                          ? ' dr-save-saving'
                          : ''
                    }`}
                    title={status.kind === 'error' ? status.message : 'Changes are saved as you work'}
                  >
                    {statusText(status, hasPending())}
                  </span>}
                </div>
              </div>
            </header>


            <div className={`ws-body dr-body${sideOpen && model ? ' has-side' : ''}`}>
              <div
                className={`ws-editor-host dr-editor-host dr-host-${active.format}`}
              >
                {model ? (
                  <RibbonActionsContext.Provider value={ribbonActions}>{editor()}</RibbonActionsContext.Provider>
                ) : (
                  <div className="dr-opening">
                    <span className="dr-opening-page" />
                    <span className="ws-hint">Opening document…</span>
                  </div>
                )}

                {/* Aurora opens as the side panel here, as on every page: it
                    reads this draft (useAssistantPage above) and can insert. */}

                {/* The tools panel opens from the corner it opens INTO, on the
                    composer's own horizon — the two float over the page as a
                    pair rather than one sitting in a header band far from the
                    column it controls. */}
                {model && (
                  <button
                    type="button"
                    className={`dr-tools-fab${sideOpen ? ' is-on' : ''}`}
                    onClick={() => setSideOpen(o => !o)}
                    aria-pressed={sideOpen}
                    title={sideOpen ? 'Hide document tools' : 'Show document tools'}
                    aria-label={sideOpen ? 'Hide document tools' : 'Show document tools'}
                  >
                    <PanelRight size={17} strokeWidth={1.75} />
                  </button>
                )}
              </div>
              {model && (
                <Presence open={historyOpen}>
                  <VersionHistory
                    draftId={active.id}
                    refreshKey={versionKey}
                    onRestore={v => void handleRestore(v)}
                    onClose={() => setHistoryOpen(false)}
                  />
                </Presence>
              )}
              {model && (
                <Presence open={sideOpen}>
                  <div className="dr-side-wrap">
                    <DraftSidePanel
                      draft={active}
                      model={model}
                      tab={sideTab}
                      onTab={setSideTab}
                      onClose={() => setSideOpen(false)}
                      versionKey={versionKey}
                      onRestore={v => void handleRestore(v)}
                      onCompareVersion={v => {
                        void (async () => {
                          // Compare against what is on screen, so save first.
                          if (hasPending()) await flush()
                          setComparing({
                            left: { kind: 'draft', id: active.id, version: v },
                            right: { kind: 'draft', id: active.id },
                          })
                        })()
                      }}
                      onInsertText={model.format === 'pptx' || model.format === 'pdf' ? undefined : insertText}
                      onJumpTo={jumpTo}
                      xlsxSelection={xlsxSelection}
                      pptxSelection={pptxSelection}
                      onPatch={onPatch}
                      actions={docActions}
                    />
                  </div>
                </Presence>
              )}
            </div>
          </>
        ) : (
          <div className="ws-landing">
            {/* Title centred at the top, back arrow on its left, tools on the
                right. A three-column grid rather than space-between, because
                with flex the title would only look centred while both sides
                happened to be the same width. */}
            {/* The title stands alone now that the actions have moved below
                the search field, so it needs no columns to be centred. */}
            <header className="ws-landing-head">
              <h1 className="ws-landing-title">Workspace</h1>
            </header>

            {scopeName && (
              /* Scoping is a state the user has to be able to see and leave;
                 without this line a matter with two documents in it looks
                 identical to a workspace with two documents in it. */
              <div className="ws-scope">
                <span className="ws-scope-label">{scopeName}</span>
                <button
                  type="button"
                  className="ws-scope-clear"
                  onClick={() => setMatterScope(undefined)}
                >
                  Show all documents
                </button>
              </div>
            )}

            <div className="ws-searchbar">
              <Search size={15} strokeWidth={2.25} className="ws-search-icon" />
              <input
                className="ws-search-input"
                type="search"
                value={query}
                placeholder="Search workspace"
                aria-label="Search workspace"
                onChange={e => {
                  setQuery(e.target.value)
                  // Clearing the field restores the grid immediately; results
                  // for a query nobody is looking at any more are noise.
                  if (!e.target.value.trim()) setResults(null)
                }}
                onKeyDown={e => {
                  if (e.key === 'Enter') void runSearch()
                  if (e.key === 'Escape') clearSearch()
                }}
              />
              {query && (
                <button
                  type="button"
                  className="ws-close"
                  onClick={clearSearch}
                  aria-label="Clear search"
                >
                  <X size={14} strokeWidth={1.75} />
                </button>
              )}
            </div>

            {/* Below the search field: the actions read as "what you can add"
                once you have failed to find it, which is the order the page is
                actually used in. */}
            <div className="ws-landing-actions">
                {selection.count > 0 ? (
                  <SelectionBar
                    selection={selection}
                    onCompare={compareSelected}
                    onMoveToRoot={() => {
                      void run(async () => {
                        for (const id of selection.drafts) await moveDraft(id, null)
                      })
                      selection.clear()
                    }}
                    onDelete={async () => {
                      const n = selection.count
                      const ok = await confirm({
                        title: `Delete ${n} item${n === 1 ? '' : 's'}?`,
                        message: `Delete ${n} selected item${n === 1 ? '' : 's'}? Deleted documents will no longer be available to chats or document search. This cannot be undone.`,
                        confirmLabel: 'Delete',
                      })
                      if (!ok) return
                      void run(async () => {
                        for (const id of selection.drafts) await deleteDraft(id)
                        for (const id of selection.folders) await deleteFolder(id)
                      })
                      selection.clear()
                    }}
                  />
                ) : null}
            </div>

            {selection.count === 0 && (
              <div className="ws-cards">
                {/* The two cards share one anatomy — glyph well, title, one
                    line of description, actions — so they read as a pair. */}
                <div className="ws-card ws-action-card ws-import-card">
                  <span className="ws-action-card__glyph" aria-hidden="true">
                    <FileUp size={16} strokeWidth={1.75} />
                  </span>
                  <span className="ws-action-card__copy">
                    <span className="ws-card-title">Add documents</span>
                    <span className="ws-card-text">Bring files in from this computer or your Vault.</span>
                  </span>
                  <div className="ws-card-buttons">
                    <button
                      type="button"
                      className="ws-pill"
                      onClick={() => filePicker.current?.click()}
                      disabled={busy}
                      title="Upload a file from this computer"
                    >
                      <Upload size={14} strokeWidth={1.75} />
                      Upload
                    </button>
                    <button
                      type="button"
                      className="ws-pill"
                      onClick={() => {
                        clearSearch()
                        selection.clear()
                        setVaultMode(true)
                      }}
                      disabled={busy}
                      title="Pick documents from your Vault"
                    >
                      <SquareArrowOutUpRight size={14} strokeWidth={1.75} />
                      From Vault
                    </button>
                  </div>
                </div>

                <div className="ws-draft-anchor" ref={newMenuAnchor}>
                  <button
                    type="button"
                    className="ws-card ws-action-card ws-draft-box"
                    onClick={() => setNewMenuOpen(o => !o)}
                    disabled={busy}
                    aria-haspopup="menu"
                    aria-expanded={newMenuOpen}
                    title="Start a new blank document here"
                  >
                    <span className="ws-action-card__glyph" aria-hidden="true">
                      <FilePlus2 size={16} strokeWidth={1.75} />
                    </span>
                    <span className="ws-action-card__copy">
                      <span className="ws-card-title">New draft</span>
                      <span className="ws-card-text">Start a document, contract or bylaw.</span>
                    </span>
                    <span className="ws-card-buttons">
                      <span className="ws-pill">
                        Choose format
                        <ChevronDown size={14} strokeWidth={1.75} />
                      </span>
                    </span>
                  </button>
                  <Presence open={newMenuOpen}>
                  <div className="ws-filter-menu ws-draft-menu" role="menu">
                    {NEW_FORMATS.map(({ format, label }) => (
                      <button
                        key={format}
                        type="button"
                        role="menuitem"
                        className="ws-filter-option"
                        onClick={() => {
                          setNewMenuOpen(false)
                          void handleNewDraft(format, label)
                        }}
                      >
                        {label}
                      </button>
                    ))}
                  </div>
                  </Presence>
                </div>
              </div>
            )}

            <section className="ws-files-box" aria-label="Documents and folders">
            {results && <div className="ws-files-head">{createFolderButton}</div>}
            {results && (
              <SearchResults
                results={results}
                query={query}
                searching={searching}
                onOpen={id => void openDraft(id)}
              />
            )}

            {!results && (
            <FileBrowser
              onImportFiles={() => filePicker.current?.click()}
              folders={scopedFolders}
              drafts={scopedDrafts}
              folderId={folderId}
              busy={busy}
              onOpenFolder={setFolderId}
              onOpenDraft={id => void openDraft(id)}
              onRenameFolder={(id, name) => void run(() => renameFolder(id, name))}
              onDeleteFolder={handleDeleteFolder}
              onRenameDraft={handleBrowserRenameDraft}
              onDeleteDraft={handleBrowserDeleteDraft}
              onSetDraftStatus={(id, s) => void run(() => setDraftStatus(id, s))}
              onSetDraftStarred={(id, starred) => void run(() => setDraftStarred(id, starred))}
              onDownloadDraft={id => void handleDownloadDraft(id)}
              onDownloadFolder={id => void handleDownloadFolder(id)}
              onMoveDraft={(id, target) => void run(() => moveDraft(id, target))}
              onMoveFolder={(id, parent) => void run(() => moveFolder(id, parent))}
              selection={selection}
              onOrderChange={setOrdered}
              toolbarEnd={createFolderButton}
              vaultMode={vaultMode}
              onExitVault={exitVault}
              onImportFromVault={handleImportFromVault}
            />
            )}
            </section>

          </div>
        )}
      </main>
    </div>
  )
}
