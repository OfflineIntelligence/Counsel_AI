/**
 * The library — precedents, templates and clauses.
 *
 * Lawyers do not draft from nothing; they start from the last one that worked.
 * This is the client for that, and it deliberately keeps the two shapes apart
 * because they are used differently:
 *
 * - a **library item** is a FILE. You open it, and it becomes a new draft.
 * - a **clause** is TEXT. You never open it; you drop it into the document you
 *   are already writing, which is why its body arrives in the row rather than
 *   behind a second fetch.
 *
 * Starting from a precedent COPIES it. Editing the new draft can never reach
 * back into the firm's precedent bank, and deleting the matter it was drawn
 * from can never empty that bank out.
 */

import { fetchWithNetworkRetry } from './fetchWithTimeout'
import { base, json, ok } from './http'
import type { DraftSummary } from './drafts'

export const LIBRARY_KINDS = ['template', 'precedent'] as const
export type LibraryKind = (typeof LIBRARY_KINDS)[number]

export const LIBRARY_KIND_LABELS: Record<LibraryKind | 'knowledge', string> = {
  template: 'Template',
  precedent: 'Precedent',
  knowledge: 'Knowledge',
}

export interface LibraryItem {
  id: number
  title: string
  /** 'knowledge' marks a reference document shipped with the app. */
  kind: LibraryKind | 'knowledge'
  /** Shipped with the app (the knowledge collection) rather than added by the firm. */
  bundled: boolean
  format: string
  byte_size: number
  /** Which draft it was promoted from, if any. Provenance only. */
  source_draft_id: number | null
  jurisdiction: string | null
  practice_area: string | null
  language: string | null
  tags: string[]
  created_at: string
  updated_at: string
}

export interface Clause {
  id: number
  title: string
  body: string
  jurisdiction: string | null
  created_at: string
  updated_at: string
}

/** The classifying metadata, shared by uploading and promoting. */
export interface LibraryMeta {
  title?: string
  kind?: LibraryKind
  jurisdiction?: string
  practice_area?: string
  language?: string
  tags?: string[]
}

export async function listLibrary(): Promise<LibraryItem[]> {
  return json(await fetchWithNetworkRetry(`${base()}/library`))
}

/** Add a house template or precedent by uploading the file itself. */
export async function addToLibrary(file: File, meta: LibraryMeta = {}): Promise<LibraryItem> {
  const form = new FormData()
  if (meta.title) form.append('title', meta.title)
  if (meta.kind) form.append('kind', meta.kind)
  if (meta.jurisdiction) form.append('jurisdiction', meta.jurisdiction)
  if (meta.practice_area) form.append('practice_area', meta.practice_area)
  if (meta.language) form.append('language', meta.language)
  // One comma-joined field rather than repeated ones: the server accepts
  // either, and this keeps the request readable when something goes wrong.
  if (meta.tags?.length) form.append('tags', meta.tags.join(','))
  // The file goes last so the server has the metadata before the bytes.
  form.append('file', file, file.name)
  return json(await fetchWithNetworkRetry(`${base()}/library`, { method: 'POST', body: form }))
}

export async function removeFromLibrary(id: number): Promise<void> {
  await ok(await fetchWithNetworkRetry(`${base()}/library/${id}`, { method: 'DELETE' }))
}

/** A library item's text, for the AI Assistant. */
export async function getLibraryItemText(
  id: number,
): Promise<{ id: number; title: string; format: string; text: string; truncated: boolean }> {
  return json(await fetchWithNetworkRetry(`${base()}/library/${id}/text`))
}

/** The most common way a document begins. Returns the new, independent draft. */
export async function newDraftFromLibrary(
  id: number,
  options: { title?: string; matterId?: number | null } = {},
): Promise<DraftSummary> {
  return json(
    await fetchWithNetworkRetry(`${base()}/library/${id}/new-draft`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ title: options.title, matter_id: options.matterId ?? null }),
    }),
  )
}

/** Promote a draft as it now reads — not as it was imported. */
export async function promoteDraftToLibrary(
  draftId: number,
  meta: LibraryMeta = {},
): Promise<LibraryItem> {
  return json(
    await fetchWithNetworkRetry(`${base()}/drafts/${draftId}/to-library`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(meta),
    }),
  )
}

export async function listClauses(): Promise<Clause[]> {
  return json(await fetchWithNetworkRetry(`${base()}/clauses`))
}

export async function createClause(
  title: string,
  body: string,
  jurisdiction?: string | null,
): Promise<Clause> {
  return json(
    await fetchWithNetworkRetry(`${base()}/clauses`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ title, body, jurisdiction: jurisdiction ?? null }),
    }),
  )
}

export async function deleteClause(id: number): Promise<void> {
  await ok(await fetchWithNetworkRetry(`${base()}/clauses/${id}`, { method: 'DELETE' }))
}
