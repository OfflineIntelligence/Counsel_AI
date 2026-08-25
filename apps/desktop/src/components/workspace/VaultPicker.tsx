/**
 * Open a document from the Vault as a new draft.
 *
 * This is the primary way a draft gets created — the workspace exists to edit
 * documents the user already has, not mainly to make new ones.
 *
 * # The copy is the whole point
 *
 * Choosing a file here copies its bytes into the draft store. The Vault entry
 * is never opened for writing, never mutated, and is not linked to the draft by
 * a foreign key. Deleting the original later cannot damage the draft, and
 * editing the draft cannot damage the original. The dialog says so, because a
 * user about to edit a signed engagement letter deserves to know which one they
 * are about to change.
 */

import { useCallback, useEffect, useMemo, useState } from 'react'

import { getApiBaseSync } from '../../api/backendUrl'
import { createFromVault, formatBytes, type DraftSummary } from '../../api/drafts'
import { fetchWithNetworkRetry } from '../../api/fetchWithTimeout'
import { extensionOf, isEditableFormat, MAX_EDITABLE_BYTES } from '../../editableFormats'

/** The subset of `DocumentRecord` this picker needs. */
interface VaultDocument {
  id: number
  original_filename: string
  size_bytes: number
  extraction_status: string
  last_referenced_at: string
}

interface Props {
  onOpened: (draft: DraftSummary) => void
  onCancel: () => void
}

export default function VaultPicker({ onOpened, onCancel }: Props) {
  const [documents, setDocuments] = useState<VaultDocument[]>([])
  const [query, setQuery] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busyId, setBusyId] = useState<number | null>(null)
  const [loading, setLoading] = useState(true)

  useEffect(() => {
    let cancelled = false
    void (async () => {
      try {
        const response = await fetchWithNetworkRetry(`${getApiBaseSync()}/documents?limit=500`)
        if (!response.ok) throw new Error(`The Vault could not be listed (${response.status}).`)
        const body = (await response.json()) as VaultDocument[]
        if (!cancelled) setDocuments(body)
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e))
      } finally {
        if (!cancelled) setLoading(false)
      }
    })()
    return () => {
      cancelled = true
    }
  }, [])

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase()
    return documents
      // Unsupported types are hidden rather than shown and then refused: the
      // server would reject them anyway, and offering a button that cannot
      // work is worse than not offering it.
      .filter(d => isEditableFormat(d.original_filename))
      // Too large to open is also 'cannot be edited' — hiding it beats
      // offering a row that fails the moment it is clicked.
      .filter(d => d.size_bytes <= MAX_EDITABLE_BYTES)
      .filter(d => !needle || d.original_filename.toLowerCase().includes(needle))
  }, [documents, query])

  const open = useCallback(
    async (document: VaultDocument) => {
      setBusyId(document.id)
      setError(null)
      try {
        onOpened(await createFromVault(document.id))
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e))
      } finally {
        setBusyId(null)
      }
    },
    [onOpened],
  )

  return (
    <div className="ws-modal-backdrop" onClick={onCancel}>
      <div
        className="ws-modal"
        role="dialog"
        aria-label="Open a document from the Vault"
        onClick={e => e.stopPropagation()}
      >
        <div className="ws-panel-head">
          <h3>Open from the Vault</h3>
          <button type="button" className="ws-close" onClick={onCancel} aria-label="Cancel">
            ✕
          </button>
        </div>

        <p className="ws-modal-note">
          The file is copied into a new draft. Your Vault copy is not changed by anything
          you do here.
        </p>

        <input
          className="ws-formula-bar"
          placeholder="Search the Vault"
          value={query}
          autoFocus
          onChange={e => setQuery(e.target.value)}
        />

        {error && <div className="ws-panel-error">{error}</div>}

        <ul className="ws-vault-list">
          {visible.map(document => (
            <li key={document.id}>
              <button
                type="button"
                className="ws-vault-row"
                disabled={busyId !== null}
                onClick={() => void open(document)}
              >
                <span className={`ws-draft-glyph ws-draft-glyph-${extensionOf(document.original_filename)}`}>
                  {extensionOf(document.original_filename).toUpperCase().slice(0, 3)}
                </span>
                <span className="ws-draft-body">
                  <span className="ws-draft-title">{document.original_filename}</span>
                  <span className="ws-draft-meta">
                    {formatBytes(document.size_bytes)}
                    {document.extraction_status !== 'ok' && ' • text could not be read'}
                  </span>
                </span>
                {busyId === document.id && <span className="ws-draft-meta">Opening…</span>}
              </button>
            </li>
          ))}
        </ul>

        {!loading && visible.length === 0 && (
          <p className="ws-panel-empty">
            {documents.length === 0
              ? 'The Vault is empty.'
              : query
                ? 'Nothing in the Vault matches that.'
                : 'Nothing in the Vault can be edited yet. Word, Excel, PowerPoint, PDF and text files can.'}
          </p>
        )}
        {loading && <p className="ws-panel-empty">Reading the Vault…</p>}
      </div>
    </div>
  )
}
