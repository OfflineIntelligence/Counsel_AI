/**
 * Matters — the level above documents.
 *
 * A firm does not think in files, it thinks in matters: a client, an
 * engagement, a court reference. Folders are how a matter is arranged inside;
 * the matter is what the work is *for*.
 *
 * # Why a card and not a row
 *
 * The four things someone needs to recognise a matter — who it is for, where
 * it is running, how much is in it, when it last moved — do not fit a list row
 * without abbreviating all four into uselessness. The card shows them whole.
 *
 * # What this view does not do
 *
 * It does not own the documents. Picking a matter hands its id back up and the
 * ordinary file browser draws the contents, so there is exactly one grid in
 * the workspace and it behaves the same everywhere.
 */

import { useMemo, useState } from 'react'
import { Briefcase, FileSearch, FolderOpen, Plus, Trash2, X } from 'lucide-react'

import type { DraftSummary } from '../../api/drafts'
import {
  MATTER_STATUSES,
  MATTER_STATUS_LABELS,
  type MatterFields,
  type MatterStatus,
  type MatterSummary,
} from '../../api/matters'
import MatterDocumentsDialog from '../rag/MatterDocumentsDialog'
import { useConfirm } from '../../contexts/confirm'

function when(iso: string | null): string {
  if (!iso) return '-'
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return '-'
  return date.toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric' })
}

interface Props {
  matters: MatterSummary[]
  drafts: DraftSummary[]
  busy: boolean
  onOpen: (id: number | null) => void
  onCreate: (fields: MatterFields) => void
  onUpdate: (id: number, fields: MatterFields) => void
  onDelete: (id: number) => void
}

export default function MattersView({
  matters,
  drafts,
  busy,
  onOpen,
  onCreate,
  onUpdate,
  onDelete,
}: Props) {
  const confirm = useConfirm()
  const [composing, setComposing] = useState(false)
  const [documentsFor, setDocumentsFor] = useState<MatterSummary | null>(null)

  // Counted here rather than asked of the server: the drafts are already
  // loaded, and a second endpoint would only be a second thing to keep in
  // step with them.
  const counts = useMemo(() => {
    const map = new Map<number, number>()
    let unfiled = 0
    for (const d of drafts) {
      if (d.matter_id == null) unfiled += 1
      else map.set(d.matter_id, (map.get(d.matter_id) ?? 0) + 1)
    }
    return { map, unfiled }
  }, [drafts])

  const lastActivity = useMemo(() => {
    const map = new Map<number, string>()
    for (const d of drafts) {
      if (d.matter_id == null) continue
      const seen = map.get(d.matter_id)
      if (!seen || d.updated_at > seen) map.set(d.matter_id, d.updated_at)
    }
    return map
  }, [drafts])

  return (
    <div className="ws-matters">
      <div className="ws-matters-head">
        <button
          type="button"
          className="ws-tool ws-tool-primary"
          onClick={() => setComposing(true)}
          disabled={busy}
        >
          <Plus size={14} strokeWidth={2} />
          New matter
        </button>
      </div>

      {composing && (
        <MatterForm
          busy={busy}
          onCancel={() => setComposing(false)}
          onSubmit={fields => {
            onCreate(fields)
            setComposing(false)
          }}
        />
      )}

      <div className="ws-matter-grid">
        {/* Always present, and never removable: documents have to be allowed
            to exist before anyone has decided what they belong to. */}
        <button
          type="button"
          className="ws-matter-card ws-matter-unfiled"
          onClick={() => onOpen(null)}
        >
          <span className="ws-matter-icon">
            <FolderOpen size={18} strokeWidth={1.5} />
          </span>
          <span className="ws-matter-name">Unfiled</span>
          <span className="ws-matter-client">Not yet assigned to a matter</span>
          <span className="ws-matter-foot">
            <span>{counts.unfiled} document{counts.unfiled === 1 ? '' : 's'}</span>
          </span>
        </button>

        {matters.map(m => (
          <div key={m.id} className="ws-matter-card-wrap">
            <button type="button" className="ws-matter-card" onClick={() => onOpen(m.id)}>
              <span className="ws-matter-icon">
                <Briefcase size={18} strokeWidth={1.5} />
              </span>
              <span className="ws-matter-name">{m.name}</span>
              <span className="ws-matter-client">{m.client || 'No client recorded'}</span>

              <span className="ws-matter-meta">
                {m.matter_number && <span className="ws-matter-ref">{m.matter_number}</span>}
                {m.jurisdiction && <span className="ws-chip">{m.jurisdiction}</span>}
                {m.practice_area && <span className="ws-chip">{m.practice_area}</span>}
              </span>

              <span className="ws-matter-foot">
                <span>
                  {counts.map.get(m.id) ?? 0} document
                  {(counts.map.get(m.id) ?? 0) === 1 ? '' : 's'}
                </span>
                <span className="ws-matter-when">
                  {/* The matter's own timestamp is the fallback: a matter with
                      nothing in it yet still has a day it was opened. */}
                  {when(lastActivity.get(m.id) ?? m.updated_at)}
                </span>
              </span>
            </button>

            <div className="ws-matter-actions">
              <select
                className="ws-matter-status"
                value={m.status}
                disabled={busy}
                aria-label={`Status of ${m.name}`}
                onChange={e => onUpdate(m.id, { status: e.target.value as MatterStatus })}
              >
                {MATTER_STATUSES.map(s => (
                  <option key={s} value={s}>
                    {MATTER_STATUS_LABELS[s]}
                  </option>
                ))}
              </select>
              <button
                type="button"
                className="ws-icon-btn"
                title="Documents that this matter's chats answer from"
                aria-label={`Search documents of ${m.name}`}
                onClick={() => setDocumentsFor(m)}
              >
                <FileSearch size={14} strokeWidth={1.75} />
              </button>
              <button
                type="button"
                className="ws-icon-btn ws-icon-danger"
                disabled={busy}
                title="Remove this matter. Its documents move back to Unfiled"
                aria-label={`Remove matter ${m.name}`}
                onClick={() => {
                  const n = counts.map.get(m.id) ?? 0
                  void (async () => {
                    const ok = await confirm({
                      title: `Remove "${m.name}"?`,
                      message: n
                        ? `Its ${n} document${n === 1 ? '' : 's'} move back to Unfiled. Nothing is deleted.`
                        : 'Nothing is deleted.',
                      confirmLabel: 'Remove',
                    })
                    if (ok) onDelete(m.id)
                  })()
                }}
              >
                <Trash2 size={14} strokeWidth={1.75} />
              </button>
            </div>
          </div>
        ))}
      </div>

      {documentsFor && (
        <MatterDocumentsDialog
          open
          matterId={documentsFor.id}
          matterName={documentsFor.name}
          onClose={() => setDocumentsFor(null)}
        />
      )}
    </div>
  )
}

/**
 * Only the name is required.
 *
 * A matter is usually opened the moment a client calls, before anyone knows
 * the court, the reference or the practice area. Demanding them up front would
 * make the honest answer — "not yet" — impossible to record.
 */
function MatterForm({
  busy,
  onSubmit,
  onCancel,
}: {
  busy: boolean
  onSubmit: (fields: MatterFields) => void
  onCancel: () => void
}) {
  const [fields, setFields] = useState<MatterFields>({ name: '' })
  const set = (key: keyof MatterFields) => (value: string) =>
    setFields(f => ({ ...f, [key]: value }))

  const name = (fields.name ?? '').trim()

  return (
    <form
      className="ws-matter-form"
      onSubmit={e => {
        e.preventDefault()
        if (!name) return
        // Blank optional fields are sent as absent, not as empty strings: an
        // empty jurisdiction is "unknown", and storing "" for it would make
        // every later filter have to special-case the difference.
        const out: MatterFields = { name }
        for (const key of ['client', 'matter_number', 'jurisdiction', 'practice_area'] as const) {
          const value = (fields[key] ?? '').toString().trim()
          if (value) out[key] = value
        }
        onSubmit(out)
      }}
    >
      <div className="ws-matter-form-head">
        <h2>New matter</h2>
        <button type="button" className="ws-close" onClick={onCancel} aria-label="Cancel">
          <X size={14} strokeWidth={1.75} />
        </button>
      </div>

      <div className="ws-matter-form-grid">
        <label className="ws-field">
          <span>Matter name</span>
          <input
            autoFocus
            value={fields.name ?? ''}
            onChange={e => set('name')(e.target.value)}
            placeholder="Acme Ltd v Brown"
          />
        </label>
        <label className="ws-field">
          <span>Client</span>
          <input
            value={fields.client ?? ''}
            onChange={e => set('client')(e.target.value)}
            placeholder="Acme Ltd"
          />
        </label>
        <label className="ws-field">
          <span>Reference</span>
          <input
            value={fields.matter_number ?? ''}
            onChange={e => set('matter_number')(e.target.value)}
            placeholder="2026-0114"
          />
        </label>
        <label className="ws-field">
          <span>Jurisdiction</span>
          <input
            value={fields.jurisdiction ?? ''}
            onChange={e => set('jurisdiction')(e.target.value)}
            placeholder="England & Wales"
          />
        </label>
        <label className="ws-field">
          <span>Practice area</span>
          <input
            value={fields.practice_area ?? ''}
            onChange={e => set('practice_area')(e.target.value)}
            placeholder="Commercial litigation"
          />
        </label>
      </div>

      <div className="ws-matter-form-foot">
        <button type="button" className="ws-tool" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className="ws-tool ws-tool-primary" disabled={busy || !name}>
          Create matter
        </button>
      </div>
    </form>
  )
}
