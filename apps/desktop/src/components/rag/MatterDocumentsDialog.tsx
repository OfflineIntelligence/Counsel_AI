import React, { useCallback, useEffect, useState } from 'react'
import { Dialog } from '../ui'
import {
  addMatterDocument,
  listMatterDocuments,
  removeMatterDocument,
  searchDocuments,
  type MatterDocument,
  type StoredDocument,
} from '../../api/rag'
import IndexStatusChip from './IndexStatusChip'
import './rag.css'

/**
 * The documents a matter's chats may retrieve from. Documents join a matter
 * automatically when attached in one of its chats; this dialog also lets any
 * stored document be added or removed by hand.
 */
export const MatterDocumentsDialog: React.FC<{
  matterId: number
  matterName: string
  open: boolean
  onClose: () => void
}> = ({ matterId, matterName, open, onClose }) => {
  const [linked, setLinked] = useState<MatterDocument[]>([])
  const [query, setQuery] = useState('')
  const [results, setResults] = useState<StoredDocument[]>([])
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const refresh = useCallback(async () => {
    try {
      setLinked(await listMatterDocuments(matterId))
      setError(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }, [matterId])

  useEffect(() => {
    if (!open) return
    const id = window.setTimeout(refresh, 0)
    return () => window.clearTimeout(id)
  }, [open, refresh])

  useEffect(() => {
    if (!open) return
    const id = window.setTimeout(() => {
      searchDocuments(query)
        .then(setResults)
        .catch(() => setResults([]))
    }, 250)
    return () => window.clearTimeout(id)
  }, [open, query])

  const run = async (action: () => Promise<void>) => {
    setBusy(true)
    try {
      await action()
      await refresh()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const linkedIds = new Set(linked.map(d => d.id))
  const candidates = results.filter(d => !linkedIds.has(d.id))

  return (
    <Dialog
      open={open}
      onClose={onClose}
      size="lg"
      title={`Documents in ${matterName}`}
      subtitle="Chats filed under this matter answer from these documents. Documents attached in the matter's chats are added automatically."
    >
      <div className="rag-docs">
        {error && <div className="rag-docs__error" role="alert">{error}</div>}

        <section aria-label="Documents in this matter">
          <h3 className="rag-docs__heading">In this matter ({linked.length})</h3>
          {linked.length === 0 ? (
            <p className="rag-docs__empty">No documents yet.</p>
          ) : (
            <ul className="rag-docs__list">
              {linked.map(d => (
                <li key={d.id} className="rag-docs__row">
                  <span className="rag-docs__name" title={d.original_filename}>{d.original_filename}</span>
                  <IndexStatusChip documentId={d.id} initial={d.index} />
                  <button
                    type="button"
                    className="ui-btn ui-btn--ghost"
                    disabled={busy}
                    onClick={() => run(() => removeMatterDocument(matterId, d.id))}
                  >
                    Remove
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section aria-label="Add documents">
          <h3 className="rag-docs__heading">Add stored documents</h3>
          <input
            className="rag-docs__search"
            type="search"
            placeholder="Search by file name"
            value={query}
            onChange={e => setQuery(e.target.value)}
            aria-label="Search stored documents by file name"
          />
          {candidates.length === 0 ? (
            <p className="rag-docs__empty">{query ? 'No matching documents.' : 'Every recent document is already in this matter.'}</p>
          ) : (
            <ul className="rag-docs__list">
              {candidates.map(d => (
                <li key={d.id} className="rag-docs__row">
                  <span className="rag-docs__name" title={d.original_filename}>{d.original_filename}</span>
                  <button
                    type="button"
                    className="ui-btn ui-btn--secondary"
                    disabled={busy}
                    onClick={() => run(() => addMatterDocument(matterId, d.id))}
                  >
                    Add
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>
    </Dialog>
  )
}

export default MatterDocumentsDialog
