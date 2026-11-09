import React, { useCallback, useEffect, useState } from 'react'
import { Search } from 'lucide-react'
import { StatusPill } from '../ui'
import { getEmbeddingStatus, installEmbeddingModel, type EmbeddingStatus } from '../../api/rag'
import { MODELS_CHANGED_EVENT, announceModelsChanged } from '../../api/models'
import './rag.css'

function mb(bytes: number): string {
  return `${Math.round(bytes / (1024 * 1024))} MB`
}

/**
 * State of the document-search (embedding) model and the retrieval index,
 * shown as a standing condition at the top of the Models page.
 */
export const EmbeddingModelBanner: React.FC = () => {
  const [status, setStatus] = useState<EmbeddingStatus | null>(null)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      setStatus(await getEmbeddingStatus())
      setError(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }, [])

  // Any model change elsewhere (another surface, a finished download) refreshes this too.
  useEffect(() => {
    const onChanged = () => void refresh()
    window.addEventListener(MODELS_CHANGED_EVENT, onChanged)
    return () => window.removeEventListener(MODELS_CHANGED_EVENT, onChanged)
  }, [refresh])

  const busy =
    !!status &&
    (status.download.state === 'downloading' ||
      status.download.state === 'verifying' ||
      (status.installed && status.index.chunks_embedded < status.index.chunks) ||
      status.index.documents_chunked < status.index.documents)

  useEffect(() => {
    const first = window.setTimeout(refresh, 0)
    const id = window.setInterval(refresh, busy ? 2000 : 30000)
    return () => {
      window.clearTimeout(first)
      window.clearInterval(id)
    }
  }, [refresh, busy])

  if (error && !status) return null
  if (!status) return null

  const { download, index, installed, model } = status

  // One shape for every state: what it is, how it is doing, the two figures
  // that matter, and the action (if any) on the right. A single sentence with
  // everything strung together after a dot could not carry the figures, the
  // progress or the action, so each state used to look like a different
  // component.
  const card = (
    tone: 'ready' | 'busy' | 'off' | 'failed',
    pill: React.ReactNode,
    detail: string,
    facts: { key: string; val: string }[],
    action?: React.ReactNode,
    progress?: number,
  ) => (
    <section className={`rag-status rag-status--${tone}`} aria-label="Document search">
      <div className="rag-status__main">
        <span className="rag-status__icon" aria-hidden="true">
          <Search size={16} strokeWidth={1.75} />
        </span>
        <div className="rag-status__text">
          <span className="rag-status__title">
            Document search
            {pill}
          </span>
          <span className="rag-status__detail">{detail}</span>
        </div>
        {facts.length > 0 && (
          <dl className="rag-status__facts">
            {facts.map(f => (
              <div key={f.key} className="rag-status__fact">
                <dt>{f.key}</dt>
                <dd>{f.val}</dd>
              </div>
            ))}
          </dl>
        )}
        {action && <div className="rag-status__action">{action}</div>}
      </div>
      {progress !== undefined && (
        <div
          className="ui-progress ui-progress--thin is-active rag-status__progress"
          style={{ '--progress': progress } as React.CSSProperties}
          role="progressbar"
          aria-valuenow={Math.round(progress)}
          aria-valuemin={0}
          aria-valuemax={100}
        >
          <div className="ui-progress__fill" />
        </div>
      )}
    </section>
  )

  if (download.state === 'downloading' || download.state === 'verifying') {
    const pct = download.total_bytes
      ? Math.min(100, Math.floor((download.downloaded_bytes / download.total_bytes) * 100))
      : 0
    return card(
      'busy',
      <StatusPill>{download.state === 'verifying' ? 'Verifying' : `Downloading ${pct}%`}</StatusPill>,
      model.display_name,
      [{ key: 'Downloaded', val: `${mb(download.downloaded_bytes)} of ${mb(download.total_bytes)}` }],
      undefined,
      pct,
    )
  }

  if (!installed) {
    const failed = download.state === 'failed'
    return card(
      failed ? 'failed' : 'off',
      <StatusPill tone={failed ? 'danger' : 'warning'}>{failed ? 'Download failed' : 'Not installed'}</StatusPill>,
      failed && download.error
        ? download.error
        : 'Document questions use keyword search until this model is installed.',
      [{ key: 'Download size', val: mb(model.size_bytes) }],
      <button type="button" className="ui-btn ui-btn--secondary" onClick={() => installEmbeddingModel().then(refresh).then(announceModelsChanged)}>
        {failed ? 'Retry download' : 'Download'}
      </button>,
    )
  }

  const embeddedPct = index.chunks ? Math.floor((index.chunks_embedded / index.chunks) * 100) : 100
  if (busy) {
    return card(
      'busy',
      <StatusPill>{`Indexing ${embeddedPct}%`}</StatusPill>,
      model.display_name,
      [
        { key: 'Documents', val: index.documents.toLocaleString() },
        { key: 'Passages', val: `${index.chunks_embedded.toLocaleString()} of ${index.chunks.toLocaleString()}` },
      ],
      undefined,
      embeddedPct,
    )
  }

  return card(
    'ready',
    <StatusPill tone="success">Ready</StatusPill>,
    model.display_name,
    [
      { key: 'Documents', val: index.documents.toLocaleString() },
      { key: 'Passages indexed', val: index.chunks.toLocaleString() },
    ],
  )
}

export default EmbeddingModelBanner
