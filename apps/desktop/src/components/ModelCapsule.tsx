/**
 * The model capsule: which model answers, and a menu to change it.
 *
 * One component for the chat composer and the drafts composer, reading the
 * shared selection (ModelSelectionContext), so the two are always showing —
 * and changing — the same thing.
 */
import { useEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'

import { useModelSelection } from '../contexts/ModelSelectionContext'
import { useDownloads } from '../api/modelSync'
import { useAnswerSource } from '../api/cloud'
import { getApiBaseSync } from '../api/backendUrl'
import { MODELS_CHANGED_EVENT } from '../api/models'

/** A download in flight, as the capsule reports it. */
type Pending = { state: 'downloading'; pct: number } | { state: 'queued' } | null

/** While no model is in use, read the shared download list so the capsule
 *  can say "Downloading 34%" or "Queued". */
function usePendingDownload(active: boolean): Pending {
  const list = useDownloads()
  if (!active) return null
  const dl = list.find(d => d.status === 'Downloading' || d.status === 'Starting')
  const q = list.find(d => d.status === 'Queued')
  return dl ? { state: 'downloading', pct: Math.round(dl.percentage ?? 0) } : q ? { state: 'queued' } : null
}

/** How many tokens the model now answering can take in (GET /context/window).
 *  Re-read when the answer source or the models change. */
function useContextWindow(sourceKey: string): number | null {
  const [win, setWin] = useState<number | null>(null)
  useEffect(() => {
    let live = true
    const load = () =>
      fetch(`${getApiBaseSync()}/context/window`)
        .then(r => (r.ok ? r.json() : null))
        .then((j: { window?: number } | null) => live && setWin(j?.window ?? null))
        .catch(() => {})
    void load()
    window.addEventListener(MODELS_CHANGED_EVENT, load)
    return () => {
      live = false
      window.removeEventListener(MODELS_CHANGED_EVENT, load)
    }
  }, [sourceKey])
  return win
}

/** A small ring: one full turn is the whole (context limit, or download). */
function Gauge({ value, title }: { value: number; title: string }) {
  const r = 6
  const c = 2 * Math.PI * r
  const v = Math.max(0, Math.min(1, value))
  return (
    <svg className="model-gauge" width="16" height="16" viewBox="0 0 16 16" role="img" aria-label={title}>
      <title>{title}</title>
      <circle className="model-gauge__track" cx="8" cy="8" r={r} />
      <circle
        className="model-gauge__fill"
        cx="8"
        cy="8"
        r={r}
        strokeDasharray={`${c * v} ${c}`}
        transform="rotate(-90 8 8)"
      />
    </svg>
  )
}

export default function ModelCapsule({ className, contextUsed }: { className?: string; contextUsed?: number }) {
  const selection = useModelSelection()
  const [open, setOpen] = useState(false)
  const pending = usePendingDownload(!!selection && !selection.selected)
  const anchor = useRef<HTMLDivElement | null>(null)
  const solo = useRef<HTMLButtonElement | null>(null)
  const [source] = useAnswerSource()
  // The side the On Computer / On Cloud pop-up is showing, if open: the
  // capsule follows it so the pill, pop-up and capsule always agree.
  const [viewing, setViewing] = useState<'local' | 'cloud' | null>(null)
  useEffect(() => {
    const onChange = (e: Event) => setViewing((e as CustomEvent<'local' | 'cloud' | null>).detail)
    window.addEventListener('oca-source-panel-changed', onChange)
    return () => window.removeEventListener('oca-source-panel-changed', onChange)
  }, [])
  const mode = viewing ?? source.mode
  const contextWindow = useContextWindow(source.mode === 'cloud' ? `cloud:${source.provider}:${source.model}` : `local:${selection?.selected?.id ?? ''}`)
  const fmt = (n: number) => (n >= 1000 ? `${Math.round(n / 1000)}k` : String(n))
  const contextGauge =
    contextUsed !== undefined && contextWindow ? (
      <Gauge
        value={contextUsed / contextWindow}
        title={`About ${fmt(contextUsed)} of ${fmt(contextWindow)} tokens of context used`}
      />
    ) : null

  useEffect(() => {
    if (!open) return
    const onPointer = (e: MouseEvent) => {
      const t = e.target as Node
      if (!anchor.current?.contains(t) && !solo.current?.contains(t)) setOpen(false)
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

  // The model in use first, then one other installed model. Two is the cap on
  // purpose: the menu is a switcher for the model you just came from, and
  // "View All Models" carries anyone who wants the full set.
  const entries = useMemo(() => {
    if (!selection) return []
    const inUse = selection.selected ? selection.models.find(m => m.id === selection.selected!.id) : undefined
    const head = inUse ?? (selection.selected ? { id: selection.selected.id, name: selection.selected.name } : undefined)
    const rest = selection.models.filter(m => m.id !== head?.id)
    return [...(head ? [head] : []), ...rest].slice(0, 2)
  }, [selection])

  // Where the chat composer's On Computer / On Cloud capsule is on screen,
  // this capsule opens THAT pop-up (local models or cloud, matching the
  // current mode) instead of a menu of its own: one place to choose a model.
  // Composers without it (drafts) keep their own menu.
  const openSourcePanel = () => {
    if (!document.querySelector('.answer-source')) return false
    window.dispatchEvent(new CustomEvent('oca-open-source-panel', { detail: mode }))
    return true
  }

  // On Cloud, every composer shows the same cloud model: one truth. While the
  // cloud pop-up is open with no cloud model in use yet, say so.
  if (mode === 'cloud') {
    const inUse = source.mode === 'cloud'
    return (
      <div className={className ?? 'input-model-picker'}>
        <button
          type="button"
          className="model-capsule model-capsule--cloud"
          title={inUse ? `On Cloud · ${source.providerName} · ${source.model}` : 'Choose a cloud model'}
          onClick={openSourcePanel}
          aria-expanded={viewing === 'cloud'}
        >
          {inUse && (contextGauge ?? <span className="model-capsule__cloud-dot" aria-hidden="true" />)}
          {inUse ? source.label ?? source.model : 'Choose a cloud model'}
        </button>
      </div>
    )
  }

  if (!selection) return null

  const { models, selected, select, openModels } = selection
  // No light at all when nothing is installed: a fresh install has no models
  // yet, which is not a fault. Yellow once something is on disk, green when
  // one is chosen.
  const dot: 'none' | 'idle' | 'live' = selected ? 'live' : models.length > 0 ? 'idle' : 'none'

  // With nothing to switch between, the capsule offers one quiet action at the
  // composer's top-right instead of a menu. The drafts composer has no such
  // corner, so it keeps the menu.
  const corner = entries.length === 0 ? anchor.current?.closest('.chat-input-form') ?? null : null

  return (
    <div className={className ?? 'input-model-picker'}>
      <div className="model-menu-anchor" ref={anchor}>
        <button
          type="button"
          className="model-capsule"
          onClick={() => {
            if (!openSourcePanel()) setOpen(o => !o)
          }}
          title={selected ? `${selected.name}. Click to change model` : 'Click to select a model'}
          aria-haspopup="menu"
          aria-expanded={open || viewing === 'local'}
        >
          {pending?.state === 'downloading' ? (
            <Gauge value={pending.pct / 100} title={`Downloading ${pending.pct}%`} />
          ) : selected && contextGauge ? (
            contextGauge
          ) : (
            dot !== 'none' && <span className={`model-capsule__dot model-capsule__dot--${dot}`} aria-hidden="true" />
          )}
          <span className="model-capsule__name">
            {selected
              ? selected.name
              : pending?.state === 'downloading'
                ? `Downloading ${pending.pct}%`
                : pending?.state === 'queued'
                  ? 'Queued'
                  : models.length > 0
                    ? 'Choose a model'
                    : 'No model'}
          </span>
          <svg
            className={`model-capsule__caret${open || viewing === 'local' ? ' is-open' : ''}`}
            width="10"
            height="10"
            viewBox="0 0 12 12"
            fill="none"
            aria-hidden="true"
          >
            <path d="M2.5 7.5 6 4l3.5 3.5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>

        {open && !corner && (
          <div className="model-menu" role="menu">
            {entries.length > 0 && (
              <>
                <div className="model-menu__label">Installed</div>
                {entries.map(model => {
                  const inUse = selected?.id === model.id
                  return (
                    <button
                      key={model.id}
                      type="button"
                      role="menuitem"
                      className={`model-menu__item${inUse ? ' is-active' : ''}`}
                      onClick={() => {
                        if (!inUse) select({ id: model.id, name: model.name, source: 'local' })
                        setOpen(false)
                      }}
                    >
                      <span className={`model-menu__dot${inUse ? '' : ' model-menu__dot--idle'}`} aria-hidden="true" />
                      <span className="model-menu__name">{model.name}</span>
                      {inUse && <span className="model-menu__tag">In use</span>}
                    </button>
                  )
                })}
              </>
            )}

            <button
              type="button"
              role="menuitem"
              className="model-menu__item model-menu__item--all"
              onClick={() => {
                setOpen(false)
                openModels()
              }}
            >
              <span className="model-menu__name">View All Models</span>
            </button>
          </div>
        )}
      </div>

      {open && corner && createPortal(
        <button
          ref={solo}
          type="button"
          className="model-browse-btn"
          onClick={() => {
            setOpen(false)
            openModels()
          }}
        >
          Browse all models
        </button>,
        corner,
      )}
    </div>
  )
}
