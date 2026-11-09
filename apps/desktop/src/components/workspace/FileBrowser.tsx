/**
 * The workspace file browser — folders and documents, as tiles or a list.
 *
 * # What it owns and what it does not
 *
 * Nothing here talks to the API. Every mutation is a callback to
 * `DraftWorkspace`, which owns the lists and is the only place that knows how
 * to refresh them. That keeps one source of truth for "what is in this
 * workspace" and makes this component trivially renderable in isolation.
 *
 * # Navigation
 *
 * Going UP is the "Up" item's job, which is why it is always the first item
 * inside a folder and why it is also a drop target: dragging onto it is the
 * way to move something out of the folder you are in.
 */

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type DragEvent, type ReactNode } from 'react'
import {
  Check,
  ChevronDown,
  CornerLeftUp,
  FolderOpen,
  LayoutGrid,
  List,
  MoreHorizontal,
  MoreVertical,
  Star,
  Upload,
} from 'lucide-react'

import {
  DRAFT_STATUSES,
  DRAFT_STATUS_LABELS,
  type DraftStatus,
  type DraftSummary,
  type FolderSummary,
} from '../../api/drafts'
import { createPortal } from 'react-dom'
import { FileBrandTile, FileGlyph, FolderGlyph, FILE_ICON } from '../FileBrandIcon'
import type { Selection } from './useSelection'
import VaultPane, { preloadVault, type VaultEntry } from './VaultPane'
import SelectAll from './SelectAll'
import { useConfirm } from '../../contexts/confirm'
import { useNameDialog } from './NameDialog'

/** How long "From Vault" mode waits, untouched, before returning to the workspace. */
const VAULT_IDLE_MS = 60_000

function when(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return ''
  const minutes = Math.round((Date.now() - date.getTime()) / 60000)
  if (minutes < 1) return 'just now'
  if (minutes < 60) return `${minutes} min ago`
  if (minutes < 60 * 24) return `${Math.round(minutes / 60)} h ago`
  return date.toLocaleDateString()
}

function longDate(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return ''
  return date.toLocaleDateString('en-US', { month: 'long', day: 'numeric', year: 'numeric' })
}

/** The status wording users see. "In draft" reads as a stage beside "In review". */
function statusLabel(status: DraftStatus): string {
  return status === 'draft' ? 'In draft' : DRAFT_STATUS_LABELS[status]
}

interface Props {
  folders: FolderSummary[]
  drafts: DraftSummary[]
  folderId: number | null
  busy: boolean
  onOpenFolder: (id: number | null) => void
  onOpenDraft: (id: number) => void
  onRenameFolder: (id: number, name: string) => void
  onDeleteFolder: (id: number) => void
  onRenameDraft: (id: number, title: string) => void
  onDeleteDraft: (id: number) => void
  onSetDraftStatus: (id: number, status: DraftStatus) => void
  onSetDraftStarred: (id: number, starred: boolean) => void
  onDownloadDraft: (id: number) => void
  onDownloadFolder: (id: number) => void
  onMoveDraft: (draftId: number, folderId: number | null) => void
  onMoveFolder: (id: number, parentId: number | null) => void
  selection: Selection
  /** Every item in display order, so the view and the ordering agree. */
  onOrderChange: (items: { kind: 'draft' | 'folder'; id: number }[]) => void
  toolbarEnd?: ReactNode
  /** Browse the Vault in place of the workspace grid. */
  vaultMode?: boolean
  onExitVault?: () => void
  onImportFromVault?: (entries: VaultEntry[]) => Promise<void>
  /** Opens the file picker; the empty state's "Import Files". */
  onImportFiles?: () => void
}

type SortKey = 'modified' | 'newest' | 'oldest'
type DateKey = 'any' | 'today' | '7d' | '30d' | '365d'
type ViewMode = 'grid' | 'list'

const SORT_OPTIONS: [SortKey, string][] = [
  ['modified', 'Last modified'],
  ['newest', 'Newest first'],
  ['oldest', 'Oldest first'],
]

const DATE_OPTIONS: [DateKey, string][] = [
  ['today', 'Today'],
  ['7d', 'Last week'],
  ['30d', 'Last month'],
  ['365d', 'Last year'],
]

const DAY = 24 * 60 * 60 * 1000
const VIEW_KEY = 'ws-file-view'

function withinDate(iso: string, key: DateKey): boolean {
  if (key === 'any') return true
  const t = Date.parse(iso)
  if (Number.isNaN(t)) return false
  if (key === 'today') {
    const start = new Date()
    start.setHours(0, 0, 0, 0)
    return t >= start.getTime()
  }
  const days = key === '7d' ? 7 : key === '30d' ? 30 : 365
  return t >= Date.now() - days * DAY
}

function compareBy<T extends { created_at: string; updated_at: string }>(sort: SortKey) {
  return (a: T, b: T) => {
    switch (sort) {
      case 'newest':
        return Date.parse(b.created_at) - Date.parse(a.created_at)
      case 'oldest':
        return Date.parse(a.created_at) - Date.parse(b.created_at)
      default:
        return Date.parse(b.updated_at) - Date.parse(a.updated_at)
    }
  }
}

/** List view only: tiles and the grid/list switch were removed. */
function readView(): ViewMode {
  return 'list'
}

/** What is currently being dragged. */
type Dragged = { kind: 'draft' | 'folder'; id: number } | null

/** Sentinel for the "Up" item, which has no folder id of its own. */
const UP = -1

export default function FileBrowser({
  folders,
  drafts,
  folderId,
  busy,
  onOpenFolder,
  onOpenDraft,
  onRenameFolder,
  onDeleteFolder,
  onRenameDraft,
  onDeleteDraft,
  onSetDraftStatus,
  onSetDraftStarred,
  onDownloadDraft,
  onDownloadFolder,
  onMoveDraft,
  onMoveFolder,
  selection,
  onOrderChange,
  toolbarEnd,
  vaultMode = false,
  onExitVault,
  onImportFiles,
  onImportFromVault,
}: Props) {
  const confirm = useConfirm()
  // Renaming asked through `window.prompt`, which on Tauri opens an OS box
  // captioned "tauri.localhost says". Same dialog as everywhere else now.
  const [nameDialog, askName] = useNameDialog()
  const rootRef = useRef<HTMLDivElement | null>(null)
  const collapseRef = useRef<HTMLDivElement | null>(null)
  const [collapseWidth, setCollapseWidth] = useState<number | null>(null)

  // Upload and Folder collapse by animating max-width. A generous guess (e.g.
  // 420px) against a ~250px real width makes the reopening finish early and
  // the closing start late, so the two directions feel different speeds.
  // Measuring the real width makes both run the full duration.
  useLayoutEffect(() => {
    const el = collapseRef.current
    if (el && !vaultMode) setCollapseWidth(el.getBoundingClientRect().width)
  }, [vaultMode, toolbarEnd])
  // Fetch the Vault listing ahead of time, so "From Vault" opens straight onto
  // files instead of a loading pause.
  useEffect(() => {
    preloadVault().catch(() => {})
  }, [])

  const [vaultSelected, setVaultSelected] = useState<Map<number, VaultEntry>>(new Map())
  const [importing, setImporting] = useState(false)

  useEffect(() => {
    if (!vaultMode) {
      setVaultSelected(new Map())
      return
    }
    const exit = () => onExitVault?.()
    let idle = window.setTimeout(exit, VAULT_IDLE_MS)
    const touch = () => {
      window.clearTimeout(idle)
      idle = window.setTimeout(exit, VAULT_IDLE_MS)
    }
    const onPointer = (e: MouseEvent) => {
      touch()
      const box = rootRef.current?.closest('.ws-files-box') ?? rootRef.current
      if (box && !box.contains(e.target as Node)) exit()
    }
    const onKey = (e: KeyboardEvent) => {
      touch()
      if (e.key === 'Escape') exit()
    }
    const activity = ['mousemove', 'wheel', 'touchstart'] as const
    document.addEventListener('mousedown', onPointer)
    document.addEventListener('keydown', onKey)
    activity.forEach(name => window.addEventListener(name, touch, { passive: true }))
    return () => {
      window.clearTimeout(idle)
      document.removeEventListener('mousedown', onPointer)
      document.removeEventListener('keydown', onKey)
      activity.forEach(name => window.removeEventListener(name, touch))
    }
  }, [vaultMode, onExitVault])

  const toggleVaultEntry = (entry: VaultEntry) =>
    setVaultSelected(prev => {
      const next = new Map(prev)
      if (next.has(entry.id)) next.delete(entry.id)
      else next.set(entry.id, entry)
      return next
    })

  const uploadToWorkspace = async () => {
    if (!onImportFromVault || vaultSelected.size === 0 || importing) return
    setImporting(true)
    try {
      await onImportFromVault([...vaultSelected.values()])
    } finally {
      setImporting(false)
    }
  }

  const [dropTarget, setDropTarget] = useState<number | null>(null)
  const dragged = useRef<Dragged>(null)
  const [sort, setSort] = useState<SortKey>('modified')
  const [statusFilter, setStatusFilter] = useState<DraftStatus | 'all'>('all')
  const [dateFilter, setDateFilter] = useState<DateKey>('any')
  const [view, setView] = useState<ViewMode>(readView)

  const changeView = (next: ViewMode) => {
    setView(next)
    try {
      localStorage.setItem(VIEW_KEY, next)
    } catch {
      /* the choice simply is not remembered */
    }
  }

  const childFolders = useMemo(
    () =>
      folders
        .filter(f => f.parent_id === folderId)
        .sort(compareBy<FolderSummary>(sort)),
    [folders, folderId, sort],
  )

  const childDrafts = useMemo(
    () =>
      drafts
        .filter(d => (d.folder_id ?? null) === folderId)
        .filter(d => statusFilter === 'all' || d.status === statusFilter)
        .filter(d => withinDate(d.updated_at, dateFilter))
        .sort(compareBy<DraftSummary>(sort))
        .sort((a, b) => Number(b.starred) - Number(a.starred)),
    [drafts, folderId, sort, statusFilter, dateFilter],
  )

  /** Where "Up" goes: the current folder's parent, or the root. */
  const parentId = useMemo(
    () => (folderId === null ? null : (folders.find(f => f.id === folderId)?.parent_id ?? null)),
    [folders, folderId],
  )

  useEffect(() => {
    onOrderChange([
      ...childFolders.map(f => ({ kind: 'folder' as const, id: f.id })),
      ...childDrafts.map(d => ({ kind: 'draft' as const, id: d.id })),
    ])
  }, [childFolders, childDrafts, onOrderChange])

  const beginDrag = useCallback((kind: 'draft' | 'folder', id: number) => {
    dragged.current = { kind, id }
  }, [])

  /** Apply a drop onto `target`, which is a folder id or the root. */
  const drop = useCallback(
    (target: number | null) => {
      const item = dragged.current
      dragged.current = null
      setDropTarget(null)
      if (!item) return
      if (item.kind === 'draft') {
        onMoveDraft(item.id, target)
      } else if (item.id !== target) {
        onMoveFolder(item.id, target)
      }
    },
    [onMoveDraft, onMoveFolder],
  )

  const dropProps = (target: number, destination: number | null) => ({
    onDragOver: (e: DragEvent) => {
      e.preventDefault()
      setDropTarget(target)
    },
    onDragLeave: () => setDropTarget(null),
    onDrop: (e: DragEvent) => {
      e.preventDefault()
      drop(destination)
    },
  })

  const renameFolder = async (folder: FolderSummary) => {
    const name = await askName({
      title: 'Rename folder',
      label: 'Name',
      initial: folder.name,
      confirmLabel: 'Rename',
    })
    if (name && name !== folder.name) onRenameFolder(folder.id, name)
  }

  const deleteFolder = async (folder: FolderSummary) => {
    const ok = await confirm({
      title: 'Delete folder?',
      message: `Delete "${folder.name}"? Documents inside are kept and moved back to Workspace.`,
      confirmLabel: 'Delete folder',
    })
    if (ok) onDeleteFolder(folder.id)
  }

  const renameDraft = async (draft: DraftSummary) => {
    const title = await askName({
      title: 'Rename document',
      label: 'Name',
      initial: draft.title,
      confirmLabel: 'Rename',
    })
    if (title && title !== draft.title) onRenameDraft(draft.id, title)
  }

  const deleteDraft = async (draft: DraftSummary) => {
    const ok = await confirm({
      title: 'Delete document?',
      message: `Delete "${draft.title}" and all of its versions? It will no longer be available to chats or document search. This cannot be undone.`,
      confirmLabel: 'Delete',
    })
    if (ok) onDeleteDraft(draft.id)
  }

  const folderMenu = (folder: FolderSummary): MenuEntry[] => [
    { label: 'Open', onSelect: () => onOpenFolder(folder.id) },
    { label: 'Rename', onSelect: () => void renameFolder(folder) },
    { label: 'Download', onSelect: () => onDownloadFolder(folder.id) },
    'separator',
    { label: 'Delete', onSelect: () => deleteFolder(folder) },
  ]

  const draftMenu = (draft: DraftSummary): MenuEntry[] => [
    { label: 'Edit', onSelect: () => onOpenDraft(draft.id) },
    { label: 'Rename', onSelect: () => void renameDraft(draft) },
    { label: 'Download', onSelect: () => onDownloadDraft(draft.id) },
    {
      label: draft.starred ? 'Unmark as important' : 'Mark as important',
      onSelect: () => onSetDraftStarred(draft.id, !draft.starred),
    },
    'separator',
    ...DRAFT_STATUSES.map(s => ({
      label: statusLabel(s),
      checked: draft.status === s,
      onSelect: () => {
        if (draft.status !== s) onSetDraftStatus(draft.id, s)
      },
    })),
    'separator',
    { label: 'Delete', onSelect: () => deleteDraft(draft) },
  ]

  return (
    <div className="ws-browser" ref={rootRef}>
      {nameDialog}
      <div className={`ws-files-toolbar${vaultMode ? ' is-vault' : ''}`}>
        <div className="ws-toolbar-start">
          <div className="ws-filters ws-swap ws-swap-filters" aria-hidden={vaultMode}>
            <SelectAll
              total={childFolders.length + childDrafts.length}
              selected={selection.count}
              onSelectAll={selection.selectAll}
              onClear={selection.clear}
            />
            <FilterSelect label="Sort" value={sort} options={SORT_OPTIONS} onChange={setSort} />
            <FilterSelect
              label="Status"
              value={statusFilter}
              placeholder="Status"
              clearValue="all"
              options={DRAFT_STATUSES.map(s => [s, statusLabel(s)] as [DraftStatus | 'all', string])}
              onChange={setStatusFilter}
            />
            <FilterSelect
              label="Date modified"
              value={dateFilter}
              placeholder="Date"
              clearValue="any"
              options={DATE_OPTIONS}
              onChange={setDateFilter}
            />
          </div>
          <div className="ws-swap ws-swap-vault" aria-hidden={!vaultMode}>
            <button
              type="button"
              className="ws-upload-to-workspace"
              tabIndex={vaultMode ? 0 : -1}
              disabled={!vaultMode || vaultSelected.size === 0 || importing}
              title={vaultSelected.size === 0 ? 'Select files or folders to add' : undefined}
              onClick={() => void uploadToWorkspace()}
            >
              {importing ? 'Uploading…' : 'Upload to Workspace'}
            </button>
          </div>
        </div>
        <div
          className="ws-toolbar-end"
          style={collapseWidth ? ({ '--ws-collapse-w': `${collapseWidth}px` } as CSSProperties) : undefined}
        >
          <div
            className="ws-toolbar-collapse"
            ref={collapseRef}
            aria-hidden={vaultMode}
          >
            {toolbarEnd}
          </div>
        </div>
      </div>

      {vaultMode ? (
        <VaultPane view={view} selected={vaultSelected} onToggle={toggleVaultEntry} />
      ) : folderId === null && childFolders.length + childDrafts.length === 0 ? (
        // An empty panel said nothing. Say what would fill it — or, when a
        // filter is what emptied it, say that instead.
        statusFilter === 'all' && dateFilter === 'any' ? (
          <EmptyPlace place="Workspace" onImport={onImportFiles} />
        ) : (
          <div className="ws-empty">
            <span className="ws-empty__glyph" aria-hidden="true">
              <FolderOpen size={20} strokeWidth={1.5} />
            </span>
            <span className="ws-empty__title">No documents match these filters</span>
            <span className="ws-empty__text">Change the status or date filter to see more.</span>
          </div>
        )
      ) : view === 'grid' ? (
        <div className="ws-grid-items ws-tiles ws-mode-in">
          {folderId !== null && (
            <button
              type="button"
              className={`ws-tile ws-tile-up${dropTarget === UP ? ' ws-tile-drop' : ''}`}
              onClick={() => onOpenFolder(parentId)}
              {...dropProps(UP, parentId)}
              title="Up one level"
            >
              <span className="ws-tile-icon">
                <CornerLeftUp size={20} strokeWidth={1.75} />
              </span>
              <span className="ws-tile-text">
                <span className="ws-tile-name">Up one level</span>
              </span>
            </button>
          )}

          {childFolders.map(folder => {
            const selected = selection.isSelected('folder', folder.id)
            return (
              <div
                key={`f${folder.id}`}
                className={`ws-tile${dropTarget === folder.id ? ' ws-tile-drop' : ''}${selected ? ' ws-tile-selected' : ''}`}
                draggable={!busy}
                onDragStart={() => beginDrag('folder', folder.id)}
                {...dropProps(folder.id, folder.id)}
                onDoubleClick={() => onOpenFolder(folder.id)}
              >
                <div className="ws-tile-top">
                  <Check2
                    on={selected}
                    label={`Select ${folder.name}`}
                    onToggle={range => selection.toggle('folder', folder.id, range)}
                  />
                  <ItemMenu label={folder.name} vertical={false} entries={folderMenu(folder)} />
                </div>
                <button
                  type="button"
                  className="ws-tile-open"
                  onClick={() => onOpenFolder(folder.id)}
                  title={`Open ${folder.name}`}
                >
                  <span className="ws-tile-icon ws-icon-folder">
                    {dropTarget === folder.id ? (
                      <FolderGlyph size={FILE_ICON.grid} />
                    ) : (
                      <FolderGlyph size={FILE_ICON.grid} />
                    )}
                  </span>
                  <span className="ws-tile-text">
                    <span className="ws-tile-name">{folder.name}</span>
                    <span className="ws-tile-meta">Folder · {when(folder.updated_at)}</span>
                  </span>
                </button>
              </div>
            )
          })}

          {childDrafts.map(draft => {
            const selected = selection.isSelected('draft', draft.id)
            return (
              <div
                key={`d${draft.id}`}
                className={`ws-tile${selected ? ' ws-tile-selected' : ''}`}
                draggable={!busy}
                onDragStart={() => beginDrag('draft', draft.id)}
                onDoubleClick={() => onOpenDraft(draft.id)}
              >
                <div className="ws-tile-top">
                  <Check2
                    on={selected}
                    label={`Select ${draft.title}`}
                    onToggle={range => selection.toggle('draft', draft.id, range)}
                  />
                  <ItemMenu label={draft.title} vertical={false} entries={draftMenu(draft)} />
                </div>
                <button
                  type="button"
                  className="ws-tile-open"
                  onClick={() => onOpenDraft(draft.id)}
                  title={`Open ${draft.title}`}
                >
                  <span className="ws-tile-icon ws-tile-icon-file">
                    <FileBrandTile format={draft.format} size={FILE_ICON.grid} />
                  </span>
                  <span className="ws-tile-text">
                    <span className="ws-tile-name">
                      {draft.starred && <Star size={12} className="ws-star" aria-label="Important" />}
                      {draft.title}
                    </span>
                    <span className="ws-tile-meta">
                      {draft.format.toUpperCase()} · {when(draft.updated_at)}
                    </span>
                  </span>
                </button>
                <StatusChip status={draft.status} />
              </div>
            )
          })}
        </div>
      ) : (
        <div className="ws-file-list ws-mode-in" role="table" aria-label="Documents and folders">
          <div className="ws-list-head" role="row">
            <span role="columnheader">
              <SelectAll
                compact
                total={childFolders.length + childDrafts.length}
                selected={selection.count}
                onSelectAll={selection.selectAll}
                onClear={selection.clear}
              />
            </span>
            <span role="columnheader">Name</span>
            <span role="columnheader">Status</span>
            <span role="columnheader">Type</span>
            <span role="columnheader">Last modified</span>
            <span role="columnheader" />
          </div>

          <div className="ws-list-body">
            {folderId !== null && (
              <div
                role="row"
                className={`ws-list-row ws-list-up${dropTarget === UP ? ' ws-list-drop' : ''}`}
                onClick={() => onOpenFolder(parentId)}
                {...dropProps(UP, parentId)}
              >
                <span />
                <span className="ws-list-name">
                  <CornerLeftUp size={16} strokeWidth={1.75} className="ws-list-icon" />
                  Up one level
                </span>
                <span />
                <span />
                <span />
                <span />
              </div>
            )}

            {childFolders.map(folder => {
              const selected = selection.isSelected('folder', folder.id)
              return (
                <div
                  key={`f${folder.id}`}
                  role="row"
                  className={`ws-list-row${dropTarget === folder.id ? ' ws-list-drop' : ''}${selected ? ' ws-list-selected' : ''}`}
                  draggable={!busy}
                  onDragStart={() => beginDrag('folder', folder.id)}
                  {...dropProps(folder.id, folder.id)}
                  onDoubleClick={() => onOpenFolder(folder.id)}
                >
                  <span>
                    <Check2
                      on={selected}
                      label={`Select ${folder.name}`}
                      onToggle={range => selection.toggle('folder', folder.id, range)}
                    />
                  </span>
                  <button
                    type="button"
                    className="ws-list-name"
                    onClick={() => onOpenFolder(folder.id)}
                    title={`Open ${folder.name}`}
                  >
                    <span className="ws-list-icon ws-icon-folder"><FolderGlyph size={FILE_ICON.list} /></span>
                    <span className="ws-list-title">{folder.name}</span>
                  </button>
                  <span />
                  <span className="ws-list-cell">Folder</span>
                  <span className="ws-list-cell">{longDate(folder.updated_at)}</span>
                  <span className="ws-list-actions">
                    <ItemMenu label={folder.name} vertical entries={folderMenu(folder)} />
                  </span>
                </div>
              )
            })}

            {childDrafts.map(draft => {
              const selected = selection.isSelected('draft', draft.id)
              return (
                <div
                  key={`d${draft.id}`}
                  role="row"
                  className={`ws-list-row${selected ? ' ws-list-selected' : ''}`}
                  draggable={!busy}
                  onDragStart={() => beginDrag('draft', draft.id)}
                  onDoubleClick={() => onOpenDraft(draft.id)}
                >
                  <span>
                    <Check2
                      on={selected}
                      label={`Select ${draft.title}`}
                      onToggle={range => selection.toggle('draft', draft.id, range)}
                    />
                  </span>
                  <button
                    type="button"
                    className="ws-list-name"
                    onClick={() => onOpenDraft(draft.id)}
                    title={`Open ${draft.title}`}
                  >
                    <span className="ws-list-icon ws-list-icon-file">
                      <FileBrandTile format={draft.format} size={FILE_ICON.list} />
                    </span>
                    <span className="ws-list-title">{draft.title}</span>
                    {draft.starred && <Star size={12} className="ws-star" aria-label="Important" />}
                  </button>
                  <span>
                    <StatusChip status={draft.status} always />
                  </span>
                  <span className="ws-list-cell">{fileTypeName(draft.format)}</span>
                  <span className="ws-list-cell">{longDate(draft.updated_at)}</span>
                  <span className="ws-list-actions">
                    <ItemMenu label={draft.title} vertical entries={draftMenu(draft)} />
                  </span>
                </div>
              )
            })}
          </div>
        </div>
      )}
    </div>
  )
}

const FILE_TYPE_NAMES: Record<string, string> = {
  pdf: 'PDF Document',
  doc: 'Word Document',
  docx: 'Word Document',
  ppt: 'PowerPoint Presentation',
  pptx: 'PowerPoint Presentation',
  xls: 'Excel Workbook',
  xlsx: 'Excel Workbook',
  csv: 'CSV File',
  txt: 'Text Document',
  md: 'Markdown Document',
  rtf: 'Rich Text Document',
  png: 'PNG Image',
  jpg: 'JPEG Image',
  jpeg: 'JPEG Image',
}

/** The name people use for a file format, e.g. "Word Document" for docx. */
export function fileTypeName(format: string): string {
  const key = format.toLowerCase()
  return FILE_TYPE_NAMES[key] ?? `${key.toUpperCase()} File`
}

/**
 * A file's icon in the chat composer and sent-message chips. Drawn by the
 * shared FileGlyph (components/FileBrandIcon.tsx), the same minimal page the
 * Workspace, Vault and Library use, so one document looks the same
 * everywhere. `width` is kept for the existing callers; the glyph's square
 * box is sized to match the old page's height.
 */
export function FileTypeIcon({
  format,
  width = 32,
  showLabel,
}: {
  format: string
  width?: number
  showLabel?: boolean
}) {
  return <FileGlyph format={format} size={FILE_ICON.list} showLabel={showLabel} />
}

/**
 * The empty state for Vault, Workspace and Library: the same words and the
 * same button on all three, so an empty place always reads the same.
 */
export function EmptyPlace({ place, onImport }: { place: 'Vault' | 'Workspace' | 'Library'; onImport?: () => void }) {
  return (
    <div className="ws-empty">
      <span className="ws-empty__title">No files or folders inside {place}</span>
      {onImport && (
        <button type="button" className="ui-btn ui-btn--primary ui-btn--pill ws-empty__action" onClick={onImport}>
          <Upload size={14} strokeWidth={1.9} />
          Import Files
        </button>
      )}
    </div>
  )
}

/** A draft's status as a small chip. Plain drafts show nothing unless
 *  `always` is set (the list view's status column). */
function StatusChip({ status, always = false }: { status: DraftStatus; always?: boolean }) {
  if (status === 'draft' && !always) return null
  return (
    <span className={`ws-chip ws-chip-${status}`}>
      <span className="ws-chip-dot" aria-hidden="true" />
      {statusLabel(status)}
    </span>
  )
}

/** One row of an ItemMenu, or a divider between groups. */
export type MenuEntry =
  | 'separator'
  | { label: string; onSelect: () => void; checked?: boolean; danger?: boolean }

export function ItemMenu({
  label,
  vertical,
  entries,
}: {
  label: string
  vertical: boolean
  entries: MenuEntry[]
}) {
  const [open, setOpen] = useState(false)
  const [up, setUp] = useState(false)
  const [fixedAt, setFixedAt] = useState<{ top: number; left: number } | null>(null)
  const anchor = useRef<HTMLDivElement | null>(null)
  const menuRef = useRef<HTMLDivElement | null>(null)
  const [portalHost, setPortalHost] = useState<HTMLElement | null>(null)

  useEffect(() => {
    if (!open) return
    const onPointer = (e: MouseEvent) => {
      const target = e.target as Node
      if (!anchor.current?.contains(target) && !menuRef.current?.contains(target)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    // A menu pinned to the window would drift away from its row on scroll.
    const onScroll = () => setOpen(false)
    document.addEventListener('mousedown', onPointer)
    document.addEventListener('keydown', onKey)
    if (vertical) document.addEventListener('scroll', onScroll, true)
    return () => {
      document.removeEventListener('mousedown', onPointer)
      document.removeEventListener('keydown', onKey)
      document.removeEventListener('scroll', onScroll, true)
    }
  }, [open, vertical])

  const toggle = () => {
    if (!open && anchor.current) {
      const trigger = anchor.current.getBoundingClientRect()
      if (vertical) {
        // List view: opens in the free space to the right of the white box,
        // with its first row level with the dots.
        //
        // It is rendered into the page (a portal), not inside the row: the
        // list's fade-in sets `will-change: transform`, which makes the list
        // the reference frame for anything `position: fixed` inside it, so a
        // menu placed in window coordinates landed in the wrong place.
        const menuWidth = 190
        const entryCount = entries.filter(e => e !== 'separator').length
        const separators = entries.length - entryCount
        const menuHeight = 8 + entryCount * 31 + separators * 9
        const box = anchor.current.closest('.ws-files-box')?.getBoundingClientRect()
        const styles = getComputedStyle(document.documentElement)
        // The AI Assistant panel covers the window's right edge while open.
        const rightInset = parseFloat(styles.getPropertyValue('--app-right-inset')) || 0
        const topInset = parseFloat(styles.getPropertyValue('--window-band-h')) || 0
        const rightEdge = window.innerWidth - rightInset - 8
        const besideBox = (box?.right ?? trigger.right) + 12
        // Not enough free space beside the box: open to the left of the dots
        // instead, still inside the visible page.
        const left = besideBox + menuWidth <= rightEdge ? besideBox : Math.max(8, trigger.left - menuWidth - 8)
        // First option (4px padding, 30px row) centred on the dots.
        const top = trigger.top + trigger.height / 2 - (4 + 15)
        setFixedAt({
          left,
          top: Math.max(topInset + 8, Math.min(top, window.innerHeight - menuHeight - 8)),
        })
        setPortalHost((anchor.current.closest('.ws-landing') as HTMLElement | null) ?? document.body)
      } else {
        const scroller = anchor.current.closest('.ws-grid-items, .ws-list-body')
        const bottom = scroller ? scroller.getBoundingClientRect().bottom : window.innerHeight
        setUp(trigger.bottom + 300 > bottom)
      }
    }
    setOpen(o => !o)
  }

  return (
    <div className={`ws-item-menu${open ? ' is-open' : ''}`} ref={anchor}>
      <button
        type="button"
        className="ws-dots"
        aria-label={`Actions for ${label}`}
        aria-haspopup="menu"
        aria-expanded={open}
        title="More actions"
        onClick={e => {
          e.stopPropagation()
          toggle()
        }}
        onDoubleClick={e => e.stopPropagation()}
      >
        {vertical ? (
          <MoreVertical size={16} strokeWidth={2} />
        ) : (
          <MoreHorizontal size={16} strokeWidth={2} />
        )}
      </button>
      {open &&
        (() => {
          const menu = (
            <div
              ref={menuRef}
              className={`ws-filter-menu ws-actions-menu${vertical ? ' ws-actions-menu--fixed' : ' ws-actions-menu--side'}${!vertical && up ? ' is-up' : ''}`}
              style={vertical && fixedAt ? { top: fixedAt.top, left: fixedAt.left } : undefined}
              role="menu"
              onClick={e => e.stopPropagation()}
              onDoubleClick={e => e.stopPropagation()}
            >
              {entries.map((entry, i) =>
                entry === 'separator' ? (
                  <div key={`sep${i}`} className="ws-menu-divider" role="separator" />
                ) : (
                  <button
                    key={entry.label}
                    type="button"
                    role="menuitem"
                    className={`ws-filter-option${entry.checked ? ' is-on' : ''}${entry.danger ? ' is-danger' : ''}`}
                    onClick={() => {
                      setOpen(false)
                      entry.onSelect()
                    }}
                  >
                    {entry.label}
                    {entry.checked && <Check size={13} strokeWidth={2.25} />}
                  </button>
                ),
              )}
            </div>
          )
          return vertical && portalHost ? createPortal(menu, portalHost) : menu
        })()}
    </div>
  )
}

export function FilterSelect<T extends string>({
  label,
  value,
  options,
  onChange,
  placeholder,
  clearValue,
}: {
  label: string
  value: T
  options: [T, string][]
  onChange: (value: T) => void
  placeholder?: string
  clearValue?: T
}) {
  const [open, setOpen] = useState(false)
  const anchor = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    if (!open) return
    const onPointer = (e: MouseEvent) => {
      if (!anchor.current?.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onPointer)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onPointer)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  const current = options.find(([key]) => key === value)?.[1] ?? placeholder ?? ''

  return (
    <div className="ws-filter-anchor" ref={anchor}>
      <button
        type="button"
        className="ws-filter"
        aria-label={label}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen(o => !o)}
      >
        {current}
        <ChevronDown size={13} strokeWidth={2.25} />
      </button>
      {open && (
        <div className="ws-filter-menu" role="listbox" aria-label={label}>
          {options.map(([key, text]) => (
            <button
              key={key}
              type="button"
              role="option"
              aria-selected={key === value}
              className={`ws-filter-option${key === value ? ' is-on' : ''}`}
              onClick={() => {
                onChange(key === value && clearValue !== undefined ? clearValue : key)
                setOpen(false)
              }}
            >
              {text}
              {key === value && <Check size={13} strokeWidth={2.25} />}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

/**
 * An item's select control.
 *
 * A button rather than an <input type="checkbox"> because it has to carry the
 * shift key through to build a range, and because it sits inside an item that
 * is itself clickable — stopPropagation here is what keeps selecting a document
 * from also opening it.
 */
function Check2({
  on,
  label,
  onToggle,
}: {
  on: boolean
  label: string
  onToggle: (range: boolean) => void
}) {
  return (
    <button
      type="button"
      className={`ws-check${on ? ' ws-check-on' : ''}`}
      aria-pressed={on}
      aria-label={label}
      title={label}
      onClick={e => {
        e.stopPropagation()
        onToggle(e.shiftKey)
      }}
      onDoubleClick={e => e.stopPropagation()}
    >
      {on && <Check size={12} strokeWidth={3} />}
    </button>
  )
}

/** File-type groups for the type filter, in the order they are offered. */
export const FILE_TYPE_GROUPS: { key: string; label: string; glyph: string; exts: string[] }[] = [
  { key: 'pdf', label: 'PDF', glyph: 'pdf', exts: ['pdf'] },
  { key: 'word', label: 'Word', glyph: 'docx', exts: ['doc', 'docx', 'rtf', 'odt'] },
  { key: 'excel', label: 'Excel', glyph: 'xlsx', exts: ['xls', 'xlsx', 'csv', 'ods'] },
  { key: 'powerpoint', label: 'PowerPoint', glyph: 'pptx', exts: ['ppt', 'pptx', 'odp'] },
  { key: 'text', label: 'Text', glyph: 'txt', exts: ['txt', 'md', 'markdown'] },
  { key: 'image', label: 'Images', glyph: 'png', exts: ['png', 'jpg', 'jpeg', 'gif', 'bmp', 'tif', 'tiff', 'webp'] },
  { key: 'other', label: 'Other', glyph: 'file', exts: [] },
]

/** The type group a file belongs to, from its name or bare extension. */
export function fileTypeGroup(nameOrExt: string): string {
  const ext = (nameOrExt.includes('.') ? nameOrExt.split('.').pop() ?? '' : nameOrExt).toLowerCase()
  return FILE_TYPE_GROUPS.find(g => g.exts.includes(ext))?.key ?? 'other'
}

/**
 * "All types" capsule with a checkbox list of file types (PDF, Word, Excel…).
 * Only types that are present are offered, each with its count. An empty
 * `value` means every type. The dropdown is drawn as the capsule's own
 * surface, so the two read as one control.
 */
export function TypeFilter({
  value,
  onChange,
  counts,
}: {
  value: string[]
  onChange: (value: string[]) => void
  /** How many files of each group key are present. */
  counts: Record<string, number>
}) {
  const [open, setOpen] = useState(false)
  const anchor = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    if (!open) return
    const onPointer = (e: MouseEvent) => {
      if (!anchor.current?.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onPointer)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onPointer)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  const groups = FILE_TYPE_GROUPS.filter(g => (counts[g.key] ?? 0) > 0)
  const toggle = (key: string) => onChange(value.includes(key) ? value.filter(v => v !== key) : [...value, key])
  const labelOf = (key: string) => FILE_TYPE_GROUPS.find(g => g.key === key)?.label ?? key
  const label = value.length === 0 ? 'All types' : value.length === 1 ? labelOf(value[0]) : `${value.length} types`

  return (
    <div className="ws-filter-anchor" ref={anchor}>
      <button
        type="button"
        className="ws-filter"
        aria-label="Filter by file type"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen(o => !o)}
      >
        {label}
        <ChevronDown size={13} strokeWidth={2.25} />
      </button>
      {open && (
        <div className="ws-filter-menu ws-type-menu" role="listbox" aria-label="File type" aria-multiselectable="true">
          <button
            type="button"
            role="option"
            aria-selected={value.length === 0}
            className={`ws-filter-option ws-type-option${value.length === 0 ? ' is-on' : ''}`}
            onClick={() => onChange([])}
          >
            <span className={`ws-type-check${value.length === 0 ? ' is-on' : ''}`} aria-hidden="true">
              {value.length === 0 && <Check size={11} strokeWidth={3} />}
            </span>
            <span className="ws-type-name">All types</span>
          </button>
          {groups.map(g => {
            const on = value.includes(g.key)
            return (
              <button
                key={g.key}
                type="button"
                role="option"
                aria-selected={on}
                className={`ws-filter-option ws-type-option${on ? ' is-on' : ''}`}
                onClick={() => toggle(g.key)}
              >
                <span className={`ws-type-check${on ? ' is-on' : ''}`} aria-hidden="true">
                  {on && <Check size={11} strokeWidth={3} />}
                </span>
                <FileGlyph format={g.glyph} size={FILE_ICON.list} />
                <span className="ws-type-name">{g.label}</span>
                <span className="ws-type-count">{counts[g.key]}</span>
              </button>
            )
          })}
          {groups.length === 0 && <p className="ws-type-empty">No files yet.</p>}
        </div>
      )}
    </div>
  )
}
