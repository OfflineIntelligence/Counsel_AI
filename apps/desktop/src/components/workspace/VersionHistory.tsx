/**
 * Version history — the durable, cross-session undo, shown as a tray.
 *
 * Deliberately presented as a different thing from Ctrl+Z. Editor undo is local
 * and in-memory; this is "go back to a saved state". Editing an old version
 * writes it as a NEW version rather than deleting the ones after it, so nothing
 * here can lose work. Version 1 is always the file exactly as it was opened,
 * and (with the current version) cannot be deleted.
 */

import { useCallback, useEffect, useState } from 'react'
import { Pencil, Plus, Trash2, X } from 'lucide-react'

import { checkpoint, deleteVersion, listVersions, type VersionSummary } from '../../api/drafts'
import { useErrorNotification } from '../../contexts/NotificationContext'
import { useConfirm } from '../../contexts/confirm'
import { useNameDialog } from './NameDialog'

interface Props {
  draftId: number
  /** Bumped by the parent after every save, to refetch the list. */
  refreshKey: number
  /** Edit this version: it is restored as the newest one. */
  onRestore: (versionNo: number) => void
  onClose: () => void
}

/** Minimal date: the time today, otherwise the day ("Oct 27"). */
function when(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return iso
  const today = new Date()
  if (date.toDateString() === today.toDateString()) {
    return date.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })
  }
  return date.toLocaleDateString([], { month: 'short', day: 'numeric' })
}

export default function VersionHistory({ draftId, refreshKey, onRestore, onClose }: Props) {
  const confirm = useConfirm()
  const [nameDialog, askName] = useNameDialog()
  const [versions, setVersions] = useState<VersionSummary[]>([])
  const [error, setError] = useState<string | null>(null)
  useErrorNotification(error, "Couldn't load version history", () => setError(null))

  const load = useCallback(async () => {
    try {
      setVersions(await listVersions(draftId))
      setError(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }, [draftId])

  useEffect(() => {
    void load()
  }, [load, refreshKey])

  const addCheckpoint = useCallback(async () => {
    const label = await askName({
      title: 'Name this version',
      label: 'Name',
      initial: 'Before client review',
      confirmLabel: 'Save version',
    })
    if (!label) return
    try {
      await checkpoint(draftId, label)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }, [draftId, load, askName])

  const edit = async (v: VersionSummary) => {
    const ok = await confirm({
      title: `Edit version ${v.version_no}?`,
      message: 'It opens as a new version, so nothing is lost.',
      confirmLabel: 'Edit',
      danger: false,
    })
    if (ok) onRestore(v.version_no)
  }

  const remove = async (v: VersionSummary) => {
    const ok = await confirm({
      title: `Delete version ${v.version_no}?`,
      message: 'This version is removed from the history.',
      confirmLabel: 'Delete',
      danger: true,
    })
    if (!ok) return
    try {
      await deleteVersion(draftId, v.version_no)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }

  return (
    <aside className="dr-history-tray" aria-label="Version history">
      {nameDialog}
      <div className="dr-history__head">
        <span>History</span>
        <button type="button" className="dr-history__icon" onClick={() => void addCheckpoint()} title="Save a checkpoint" aria-label="Save a checkpoint">
          <Plus size={15} strokeWidth={1.75} />
        </button>
        <button type="button" className="dr-history__icon" onClick={onClose} title="Close" aria-label="Close history">
          <X size={15} strokeWidth={1.75} />
        </button>
      </div>

      <ul className="dr-history__list">
        {versions.map(v => (
          <li key={v.version_no} className={`dr-history__card${v.is_current ? ' is-current' : ''}`}>
            <div className="dr-history__text">
              <span className="dr-history__label">
                v{v.version_no} · {v.label ?? (v.version_no === 1 ? 'Original' : 'Autosave')}
              </span>
              <span className="dr-history__date">{v.is_current ? `Current · ${when(v.created_at)}` : when(v.created_at)}</span>
            </div>
            {!v.is_current && (
              <button type="button" className="dr-history__icon" onClick={() => void edit(v)} title="Edit this version" aria-label={`Edit version ${v.version_no}`}>
                <Pencil size={14} strokeWidth={1.75} />
              </button>
            )}
            {!v.is_current && v.version_no !== 1 && (
              <button type="button" className="dr-history__icon" onClick={() => void remove(v)} title="Delete this version" aria-label={`Delete version ${v.version_no}`}>
                <Trash2 size={14} strokeWidth={1.75} />
              </button>
            )}
          </li>
        ))}
      </ul>
      {versions.length === 0 && !error && <p className="dr-history__empty">No versions yet.</p>}
    </aside>
  )
}
