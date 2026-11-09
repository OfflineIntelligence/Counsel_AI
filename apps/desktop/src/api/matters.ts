/**
 * Matters — the level above documents.
 *
 * A law firm does not organise work by file, it organises by matter: a client,
 * an engagement, a court reference. Folders are how a matter is arranged
 * inside; the matter is what the work is *for*. Everything here mirrors
 * `memory_db::matters_store` and the `/matters` handlers exactly.
 *
 * # Clearing a field
 *
 * `update` distinguishes "leave this alone" from "empty this out", because a
 * matter that has lost its court reference must be able to say so. Omitting a
 * key leaves it; passing `null` clears it. That is why the update type spells
 * out `| null` rather than making everything optional.
 */

import { fetchWithNetworkRetry } from './fetchWithTimeout'
import { base, json, ok } from './http'

export const MATTER_STATUSES = ['open', 'closed', 'archived'] as const
export type MatterStatus = (typeof MATTER_STATUSES)[number]

export const MATTER_STATUS_LABELS: Record<MatterStatus, string> = {
  open: 'Open',
  closed: 'Closed',
  archived: 'Archived',
}

export interface MatterSummary {
  id: number
  name: string
  client: string | null
  matter_number: string | null
  jurisdiction: string | null
  language: string | null
  practice_area: string | null
  status: MatterStatus
  opened_at: string
  closed_at: string | null
  updated_at: string
}

/** The fields a matter can be created or changed with. */
export interface MatterFields {
  name?: string
  client?: string | null
  matter_number?: string | null
  jurisdiction?: string | null
  language?: string | null
  practice_area?: string | null
  status?: MatterStatus
}

export async function listMatters(): Promise<MatterSummary[]> {
  const r = await fetchWithNetworkRetry(`${base()}/matters`)
  const body = await json<{ matters: MatterSummary[] }>(r)
  return body.matters
}

export async function createMatter(fields: MatterFields): Promise<MatterSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/matters`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(fields),
  })
  return json(r)
}

export async function updateMatter(id: number, fields: MatterFields): Promise<MatterSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/matters/${id}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(fields),
  })
  return json(r)
}

/**
 * Close a matter down without losing anything.
 *
 * The documents are not deleted with it — the server moves them back to
 * Unfiled. A matter is a label on work, and removing the label must never
 * remove the work.
 */
export async function deleteMatter(id: number): Promise<void> {
  await ok(await fetchWithNetworkRetry(`${base()}/matters/${id}`, { method: 'DELETE' }))
}
