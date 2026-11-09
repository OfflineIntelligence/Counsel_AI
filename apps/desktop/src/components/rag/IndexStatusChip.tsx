import React, { useEffect, useState } from 'react'
import { StatusPill } from '../ui'
import { getDocumentIndexState, type DocumentIndexState } from '../../api/rag'

/**
 * How searchable a document is: readable by keyword/page as soon as it is
 * chunked, fully searchable by meaning once embedded. Polls while in progress.
 */
export const IndexStatusChip: React.FC<{ documentId: number; initial?: DocumentIndexState | null; dot?: boolean }> = ({
  documentId,
  initial,
  dot,
}) => {
  const [state, setState] = useState<DocumentIndexState | null>(initial ?? null)

  useEffect(() => {
    let cancelled = false
    let timer: number | undefined
    const poll = async () => {
      try {
        const s = await getDocumentIndexState(documentId)
        if (cancelled) return
        setState(s)
        if (s.status !== 'embedded' && s.status !== 'failed') {
          // "chunked" can last while the embedding model is not installed; check less often.
          timer = window.setTimeout(poll, s.status === 'chunked' ? 10000 : 2000)
        }
      } catch {
        if (!cancelled) timer = window.setTimeout(poll, 5000)
      }
    }
    void poll()
    return () => {
      cancelled = true
      if (timer) window.clearTimeout(timer)
    }
  }, [documentId])

  if (!state) return null
  // Compact form: a coloured dot only — green when searchable, dark grey otherwise.
  if (dot) {
    const ok = state.status === 'embedded'
    const label = ok ? 'Searchable' : 'Not searchable yet'
    return <span className={`index-dot${ok ? ' is-ok' : ''}`} title={label} aria-label={label} role="img" />
  }
  switch (state.status) {
    case 'embedded':
      return <StatusPill tone="success">Searchable</StatusPill>
    case 'failed':
      return <StatusPill tone="danger">{state.error ? 'Not searchable' : 'Unreadable'}</StatusPill>
    case 'embedding': {
      const total = state.chunk_count ?? 0
      const done = state.embedded_count ?? 0
      const pct = total ? Math.floor((done / total) * 100) : 0
      return <StatusPill active>Indexing {pct}%</StatusPill>
    }
    case 'chunked':
      return <StatusPill active>Keyword search ready</StatusPill>
    default:
      return <StatusPill active>Indexing</StatusPill>
  }
}

export default IndexStatusChip
