import React, { useCallback, useEffect, useState } from 'react'
import { StatusPill } from '../ui'
import { ChevronDown } from 'lucide-react'
import {
  cancelEmbeddingDownload,
  installEmbeddingModelById,
  listEmbeddingModels,
  pauseEmbeddingDownload,
  rebuildIndex,
  removeEmbeddingModel,
  selectEmbeddingModel,
  type EmbeddingModelEntry,
} from '../../api/rag'
import EmbeddingModelBanner from './EmbeddingModelBanner'
import { useConfirm } from '../../contexts/confirm'
import { MODELS_CHANGED_EVENT, announceModelsChanged } from '../../api/models'
import './rag.css'

function mb(bytes: number): string {
  return `${Math.round(bytes / (1024 * 1024))} MB`
}

/**
 * Document-search models on the Models page: status banner, the available
 * embedding models (download, pause, resume, cancel, use, remove) and a full
 * index rebuild.
 */
export const EmbeddingModelsSection: React.FC = () => {
  const [models, setModels] = useState<EmbeddingModelEntry[]>([])
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [expanded, setExpanded] = useState(false)
  const confirm = useConfirm()

  const refresh = useCallback(async () => {
    try {
      setModels((await listEmbeddingModels()).models)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }, [])

  const downloading = models.some(m => m.download.state === 'downloading' || m.download.state === 'verifying')

  useEffect(() => {
    const first = window.setTimeout(refresh, 0)
    const id = window.setInterval(refresh, downloading ? 1500 : 15000)
    return () => {
      window.clearTimeout(first)
      window.clearInterval(id)
    }
  }, [refresh, downloading])

  // Any model change elsewhere (another surface, a finished download) refreshes this too.
  useEffect(() => {
    const onChanged = () => void refresh()
    window.addEventListener(MODELS_CHANGED_EVENT, onChanged)
    return () => window.removeEventListener(MODELS_CHANGED_EVENT, onChanged)
  }, [refresh])

  const run = async (
    key: string,
    action: () => Promise<void>,
    ask?: { title: string; message: string; confirmLabel: string; danger?: boolean },
  ) => {
    if (ask && !(await confirm(ask))) return
    setBusy(key)
    setError(null)
    try {
      await action()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(null)
      await refresh()
      announceModelsChanged()
    }
  }

  return (
    <section className="rag-models" aria-label="Document search models">
      <EmbeddingModelBanner />

      <div className="rag-models__bar">
        {/* A disclosure, so it carries a disclosure chevron — as plain
            text it read as a stray label. */}
        <button type="button" className="ui-btn ui-btn--ghost rag-models__toggle" onClick={() => setExpanded(v => !v)} aria-expanded={expanded}>
          Document-search models
          <ChevronDown size={14} strokeWidth={2} className="rag-models__chevron" aria-hidden="true" />
        </button>
        {expanded && (
          <button
            type="button"
            className="ui-btn ui-btn--secondary"
            disabled={busy !== null}
            onClick={() =>
              run(
                'rebuild',
                rebuildIndex,
                {
                  title: 'Rebuild search index?',
                  message: 'Every document is re-indexed in the background. Documents stay available; search quality returns as indexing completes.',
                  confirmLabel: 'Rebuild',
                  danger: false,
                },
              )
            }
          >
            Rebuild search index
          </button>
        )}
      </div>

      {error && <div className="rag-docs__error" role="alert">{error}</div>}

      {expanded && (
        <ul className="rag-models__list">
          {models.map(m => {
            const d = m.download
            const pct = d.total_bytes ? Math.floor((d.downloaded_bytes / d.total_bytes) * 100) : 0
            const isBusy = busy === m.id
            return (
              <li key={m.id} className="rag-models__row">
                <div className="rag-models__info">
                  <span className="rag-models__name">
                    {m.display_name}
                    {m.active && <StatusPill tone="success">In use</StatusPill>}
                  </span>
                  <span className="rag-models__desc">
                    {m.description} · {mb(m.size_bytes)} · {m.license}
                  </span>
                  {(d.state === 'downloading' || d.state === 'verifying' || d.state === 'paused') && (
                    <>
                      <span className="rag-models__desc">
                        {d.state === 'verifying' ? 'Verifying…' : `${d.state === 'paused' ? 'Paused at' : 'Downloading'} ${pct}% (${mb(d.downloaded_bytes)} of ${mb(d.total_bytes)})`}
                      </span>
                      <div className="rag-progress" aria-hidden="true">
                        <div className="rag-progress__fill" style={{ transform: `scaleX(${pct / 100})` }} />
                      </div>
                    </>
                  )}
                  {d.state === 'failed' && d.error && <span className="rag-docs__error">{d.error}</span>}
                </div>

                <div className="rag-models__actions">
                  {!m.installed && (d.state === 'idle' || d.state === 'failed') && (
                    <button type="button" className="ui-btn ui-btn--secondary" disabled={isBusy || downloading}
                      onClick={() => run(m.id, () => installEmbeddingModelById(m.id))}>
                      Download
                    </button>
                  )}
                  {d.state === 'downloading' && (
                    <button type="button" className="ui-btn ui-btn--ghost" disabled={isBusy}
                      onClick={() => run(m.id, pauseEmbeddingDownload)}>
                      Pause
                    </button>
                  )}
                  {d.state === 'paused' && (
                    <button type="button" className="ui-btn ui-btn--secondary" disabled={isBusy || downloading}
                      onClick={() => run(m.id, () => installEmbeddingModelById(m.id))}>
                      Resume
                    </button>
                  )}
                  {(d.state === 'downloading' || d.state === 'paused') && (
                    <button type="button" className="ui-btn ui-btn--ghost" disabled={isBusy}
                      onClick={() => run(m.id, () => cancelEmbeddingDownload(m.id))}>
                      Cancel
                    </button>
                  )}
                  {m.installed && !m.active && (
                    <>
                      <button type="button" className="ui-btn ui-btn--secondary" disabled={isBusy}
                        onClick={() => run(m.id, () => selectEmbeddingModel(m.id),
                          { title: 'Change document-search model?', message: `Use ${m.display_name} for document search? Every document will be re-indexed in the background.`, confirmLabel: 'Use this model', danger: false })}>
                        Use this model
                      </button>
                      <button type="button" className="ui-btn ui-btn--ghost" disabled={isBusy}
                        onClick={() => run(m.id, () => removeEmbeddingModel(m.id), { title: 'Remove model?', message: `Remove ${m.display_name} from this computer? It can be downloaded again later.`, confirmLabel: 'Remove' })}>
                        Remove
                      </button>
                    </>
                  )}
                </div>
              </li>
            )
          })}
        </ul>
      )}
    </section>
  )
}

export default EmbeddingModelsSection
