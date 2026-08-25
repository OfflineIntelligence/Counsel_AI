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
  createBlank,
  createFromUpload,
  deleteDraft,
  downloadDraft,
  getContent,
  listDrafts,
  publishToVault,
  renameDraft,
  restoreVersion,
  type DraftPatch,
  type DraftSummary,
  type DraftViewModel,
} from '../../api/drafts'
import {
  EDITABLE_ACCEPT,
  isEditableFormat,
  MAX_EDITABLE_BYTES,
  tooLargeToEditMessage,
} from '../../editableFormats'
import DocxEditor from './editors/DocxEditor'
import PdfEditor from './editors/PdfEditor'
import PptxEditor from './editors/PptxEditor'
import TextEditor from './editors/TextEditor'
import XlsxEditor from './editors/XlsxEditor'
import { usePatchQueue, type SaveStatus } from './usePatchQueue'
import VaultPicker from './VaultPicker'
import VersionHistory from './VersionHistory'
import './workspace.css'

interface Props {
  onClose: () => void
}

const FORMAT_GLYPH: Record<string, string> = {
  docx: 'W',
  xlsx: 'X',
  pptx: 'P',
  pdf: 'PDF',
  txt: 'TXT',
}

function statusText(status: SaveStatus, hasPending: boolean): string {
  switch (status.kind) {
    case 'saving':
      return 'Saving…'
    case 'pending':
      return 'Unsaved changes'
    case 'saved':
      return hasPending ? 'Unsaved changes' : 'Saved'
    case 'error':
      return 'Not saved'
    default:
      return hasPending ? 'Unsaved changes' : ''
  }
}

export default function DraftWorkspace({ onClose }: Props) {
  const [drafts, setDrafts] = useState<DraftSummary[]>([])
  const [activeId, setActiveId] = useState<number | null>(null)
  const [model, setModel] = useState<DraftViewModel | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [showVersions, setShowVersions] = useState(true)
  const [versionKey, setVersionKey] = useState(0)
  const [showVaultPicker, setShowVaultPicker] = useState(false)
  const filePicker = useRef<HTMLInputElement | null>(null)

  const active = useMemo(
    () => drafts.find(d => d.id === activeId) ?? null,
    [drafts, activeId],
  )

  const refreshList = useCallback(async () => {
    try {
      setDrafts(await listDrafts())
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e))
    }
  }, [])

  useEffect(() => {
    void refreshList()
  }, [refreshList])

  const adoptModel = useCallback((next: DraftViewModel) => {
    setModel(next)
    // The rail shows the current version and the list is ordered by recency,
    // so it has to follow every save.
    setVersionKey(k => k + 1)
    void listDrafts().then(setDrafts).catch(() => {})
  }, [])

  const { status, push, flush, hasPending, discard } = usePatchQueue({
    draftId: activeId ?? 0,
    model,
    onModel: adoptModel,
  })

  const openDraft = useCallback(
    async (id: number) => {
      // Never switch away with work in flight — the queue is keyed by draft id,
      // and leaving edits behind would strand them against the wrong document.
      if (hasPending()) await flush()
      setActiveId(id)
      setModel(null)
      setLoadError(null)
      try {
        setModel(await getContent(id))
      } catch (e) {
        setLoadError(e instanceof Error ? e.message : String(e))
      }
    },
    [flush, hasPending],
  )

  const onPatch = useCallback((...patches: DraftPatch[]) => push(...patches), [push])

  // Ctrl+S saves and cuts a version, matching every other editor the user has.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') {
        e.preventDefault()
        void flush()
      }
    }
    document.addEventListener('keydown', onKeyDown)
    return () => document.removeEventListener('keydown', onKeyDown)
  }, [flush])

  const handleUpload = useCallback(
    async (file: File) => {
      // Checked here as well as on the server. An oversized file would
      // otherwise be refused by the 50 MB body limit BEFORE any handler runs,
      // which produces a bare 413 with no message the user can act on.
      if (file.size > MAX_EDITABLE_BYTES) {
        setLoadError(tooLargeToEditMessage(file.name, file.size))
        return
      }
      if (!isEditableFormat(file.name)) {
        setLoadError(
          `"${file.name}" cannot be edited here. Word, Excel, PowerPoint, PDF and text files can.`,
        )
        return
      }

      setBusy(true)
      setLoadError(null)
      try {
        const draft = await createFromUpload(file)
        await refreshList()
        await openDraft(draft.id)
      } catch (e) {
        setLoadError(e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
      }
    },
    [refreshList, openDraft],
  )

  const handleNewText = useCallback(async () => {
    const title = window.prompt('Name the new document', 'Untitled note')
    if (!title) return
    setBusy(true)
    try {
      const draft = await createBlank(title, 'txt')
      await refreshList()
      await openDraft(draft.id)
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }, [refreshList, openDraft])

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
  }, [active, flush, hasPending])

  const handlePublish = useCallback(async () => {
    if (!active) return
    if (hasPending()) await flush()
    if (!window.confirm(`Add "${active.title}" to the Vault as a new document?`)) return
    try {
      await publishToVault(active.id)
      setLoadError(null)
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e))
    }
  }, [active, flush, hasPending])

  const handleRename = useCallback(async () => {
    if (!active) return
    const title = window.prompt('Rename draft', active.title)
    if (!title || title === active.title) return
    try {
      await renameDraft(active.id, title)
      await refreshList()
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e))
    }
  }, [active, refreshList])

  const handleDelete = useCallback(async () => {
    if (!active) return
    if (
      !window.confirm(
        `Delete "${active.title}" and all of its versions? This cannot be undone.`,
      )
    ) {
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
  }, [active, refreshList, discard])

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
    [active, adoptModel],
  )

  const editor = () => {
    if (!model || !active) return null
    switch (model.format) {
      case 'docx':
        return <DocxEditor blocks={model.blocks} styles={model.styles} onPatch={onPatch} />
      case 'xlsx':
        return <XlsxEditor sheets={model.sheets} onPatch={onPatch} />
      case 'pptx':
        return <PptxEditor slides={model.slides} onPatch={onPatch} />
      case 'pdf':
        return (
          <PdfEditor
            draftId={active.id}
            version={model.version}
            pages={model.pages}
            onPatch={onPatch}
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
          />
        )
      default:
        return <div className="ws-empty">This format has no editor yet.</div>
    }
  }

  return (
    <div className="ws-root">
      {showVaultPicker && (
        <VaultPicker
          onCancel={() => setShowVaultPicker(false)}
          onOpened={draft => {
            setShowVaultPicker(false)
            void refreshList().then(() => openDraft(draft.id))
          }}
        />
      )}
      <aside className="ws-rail">
        <div className="ws-rail-head">
          <h2>Drafts</h2>
          <button type="button" className="ws-close" onClick={onClose} aria-label="Close workspace">
            ✕
          </button>
        </div>

        <div className="ws-rail-actions">
          <button type="button" className="ws-tool" onClick={() => setShowVaultPicker(true)} disabled={busy}>
            From Vault
          </button>
          <button type="button" className="ws-tool" onClick={() => filePicker.current?.click()} disabled={busy}>
            Open a file
          </button>
          <button type="button" className="ws-tool" onClick={handleNewText} disabled={busy}>
            New note
          </button>
          <input
            ref={filePicker}
            type="file"
            accept={EDITABLE_ACCEPT}
            hidden
            onChange={e => {
              const file = e.target.files?.[0]
              // Reset so picking the same file twice in a row still fires.
              e.target.value = ''
              if (file) void handleUpload(file)
            }}
          />
        </div>

        <ul className="ws-draft-list">
          {drafts.map(draft => (
            <li key={draft.id}>
              <button
                type="button"
                className={`ws-draft${draft.id === activeId ? ' ws-draft-active' : ''}`}
                onClick={() => void openDraft(draft.id)}
              >
                <span className={`ws-draft-glyph ws-draft-glyph-${draft.format}`}>
                  {FORMAT_GLYPH[draft.format] ?? '?'}
                </span>
                <span className="ws-draft-body">
                  <span className="ws-draft-title">{draft.title}</span>
                  <span className="ws-draft-meta">
                    v{draft.current_version}
                    {draft.id === activeId && status.kind === 'pending' ? ' • unsaved' : ''}
                  </span>
                </span>
              </button>
            </li>
          ))}
        </ul>

        {drafts.length === 0 && (
          <p className="ws-panel-empty">
            No drafts yet. Open a file to start — the original is copied, never changed.
          </p>
        )}
      </aside>

      <main className="ws-main">
        {active ? (
          <>
            <header className="ws-header">
              <div className="ws-header-title">
                <h1>{active.title}</h1>
                <span className="ws-header-format">{active.format.toUpperCase()}</span>
              </div>
              <div className="ws-header-actions">
                <span
                  className={`ws-status ws-status-${status.kind}`}
                  title={status.kind === 'error' ? status.message : undefined}
                >
                  {statusText(status, hasPending())}
                </span>
                <button type="button" className="ws-tool" onClick={() => void flush()}>
                  Save
                </button>
                <button type="button" className="ws-tool" onClick={handleSaveAs}>
                  Save a copy…
                </button>
                <button type="button" className="ws-tool" onClick={handlePublish} title="Add the current version to the Vault">
                  Add to Vault
                </button>
                <button type="button" className="ws-tool" onClick={handleRename}>
                  Rename
                </button>
                <button type="button" className="ws-tool ws-tool-danger" onClick={handleDelete}>
                  Delete
                </button>
                <button
                  type="button"
                  className="ws-tool"
                  onClick={() => setShowVersions(v => !v)}
                  aria-pressed={showVersions}
                >
                  History
                </button>
              </div>
            </header>

            {status.kind === 'error' && <div className="ws-banner-error">{status.message}</div>}
            {loadError && <div className="ws-banner-error">{loadError}</div>}

            <div className="ws-body">
              <div className="ws-editor-host">
                {model ? editor() : <div className="ws-empty">Opening…</div>}
              </div>
              {showVersions && (
                <aside className="ws-context">
                  <VersionHistory
                    draftId={active.id}
                    refreshKey={versionKey}
                    onRestore={v => void handleRestore(v)}
                  />
                </aside>
              )}
            </div>
          </>
        ) : (
          <div className="ws-placeholder">
            <h1>Document workspace</h1>
            <p>
              Open a Word, Excel, PowerPoint, PDF or text file to edit it. The file you open
              is copied first — the original in your Vault is never modified, and every save
              keeps the previous version.
            </p>
            {loadError && <div className="ws-banner-error">{loadError}</div>}
          </div>
        )}
      </main>
    </div>
  )
}
