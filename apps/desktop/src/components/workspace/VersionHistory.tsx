/**
 * Version history — the durable, cross-session undo.
 *
 * Deliberately presented as a different thing from Ctrl+Z. Editor undo is local
 * and in-memory; this is "go back to a saved state", and restoring writes a NEW
 * version rather than deleting the ones after it, so nothing here can lose
 * work. Version 1 is always the file exactly as it was opened.
 */

import { useCallback, useEffect, useState } from 'react'

import { checkpoint, formatBytes, listVersions, type VersionSummary } from '../../api/drafts'

interface Props {
  draftId: number
  /** Bumped by the parent after every save, to refetch the list. */
  refreshKey: number
  onRestore: (versionNo: number) => void
}

function when(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return iso
  const minutes = Math.round((Date.now() - date.getTime()) / 60000)
  if (minutes < 1) return 'just now'
  if (minutes < 60) return `${minutes} min ago`
  if (minutes < 60 * 24) return `${Math.round(minutes / 60)} h ago`
  return date.toLocaleDateString()
}

export default function VersionHistory({ draftId, refreshKey, onRestore }: Props) {
  const [versions, setVersions] = useState<VersionSummary[]>([])
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

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
    const label = window.prompt('Name this version', 'Before client review')
    if (!label) return
    setBusy(true)
    try {
      await checkpoint(draftId, label)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }, [draftId, load])

  return (
    <div className="ws-versions">
      <div className="ws-panel-head">
        <h3>Version history</h3>
        <button type="button" className="ws-tool" onClick={addCheckpoint} disabled={busy}>
          Save a checkpoint
        </button>
      </div>

      {error && <div className="ws-panel-error">{error}</div>}

      <ul className="ws-version-list">
        {versions.map(version => (
          <li
            key={version.version_no}
            className={`ws-version${version.is_current ? ' ws-version-current' : ''}`}
          >
            <div className="ws-version-main">
              <span className="ws-version-no">v{version.version_no}</span>
              <span className="ws-version-label">
                {version.label ?? (version.version_no === 1 ? 'Original' : 'Autosave')}
              </span>
            </div>
            <div className="ws-version-meta">
              <span>{when(version.created_at)}</span>
              <span>{formatBytes(version.byte_size)}</span>
            </div>
            {version.is_current ? (
              <span className="ws-version-badge">current</span>
            ) : (
              <button
                type="button"
                className="ws-tool ws-version-restore"
                onClick={() => {
                  // Worth a confirmation, but not a warning: restoring is
                  // additive, so the state being left behind is still there.
                  if (
                    window.confirm(
                      `Restore version ${version.version_no}? ` +
                        'This is saved as a new version, so nothing is lost.',
                    )
                  ) {
                    onRestore(version.version_no)
                  }
                }}
              >
                Restore
              </button>
            )}
          </li>
        ))}
      </ul>

      {versions.length === 0 && !error && (
        <p className="ws-panel-empty">No versions yet.</p>
      )}
    </div>
  )
}
