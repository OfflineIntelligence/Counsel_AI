/**
 * Document workspace client.
 *
 * The types here mirror `document_workspace::view_model` in Rust exactly. The
 * `addr` on every node is the contract: the editor never sends a document, only
 * "replace the text of THIS node", and Rust splices that one node in the
 * original file. Anything the editor cannot render is never addressed, and so
 * can never be damaged by an edit.
 */

import { getApiBaseSync } from './backendUrl'
import { fetchWithNetworkRetry } from './fetchWithTimeout'

// ---------------------------------------------------------------- view models

export interface DocxRun {
  addr: string
  text: string
  bold: boolean
  italic: boolean
  underline: boolean
}

export interface DocxCellRef {
  table: number
  row: number
  col: number
}

export interface DocxBlock {
  addr: string
  style: string | null
  num_id: string | null
  cell: DocxCellRef | null
  runs: DocxRun[]
}

export interface DocxStyle {
  id: string
  name: string
  bold: boolean
  size_half_points: number | null
  color: string | null
}

export interface XlsxCell {
  addr: string
  value: string
  formula: string | null
  style: number | null
  numeric: boolean
}

export interface XlsxSheet {
  index: number
  name: string
  part: string
  rows: number
  cols: number
  cells: Record<string, XlsxCell>
  merges: string[]
}

export interface PptxParagraph {
  addr: string
  text: string
  level: number
}

export interface PptxShape {
  addr: string
  kind: string
  name: string
  placeholder: string | null
  bbox: { x: number; y: number; cx: number; cy: number } | null
  paragraphs: PptxParagraph[]
  image_rel: string | null
}

export interface PptxSlide {
  addr: string
  index: number
  part: string
  width: number
  height: number
  shapes: PptxShape[]
}

export interface PdfAnnotation {
  index: number
  kind: string
  rect: [number, number, number, number]
  contents: string | null
}

export interface PdfPageModel {
  index: number
  width: number
  height: number
  annotations: PdfAnnotation[]
}

export type DraftViewModel =
  | { format: 'docx'; version: number; blocks: DocxBlock[]; styles: DocxStyle[] }
  | { format: 'xlsx'; version: number; sheets: XlsxSheet[] }
  | { format: 'pptx'; version: number; slides: PptxSlide[] }
  | { format: 'pdf'; version: number; pages: PdfPageModel[] }
  | {
      format: 'txt'
      version: number
      content: string
      encoding: string
      line_ending: string
      bom: boolean
    }

// ---------------------------------------------------------------- patches

export type DraftPatch =
  | { op: 'SetRunText'; addr: string; text: string }
  | { op: 'SetRunFormat'; addr: string; bold?: boolean; italic?: boolean; underline?: boolean }
  | { op: 'SetParagraphStyle'; addr: string; style: string | null }
  | { op: 'InsertParagraphAfter'; addr: string; text: string }
  | { op: 'DeleteParagraph'; addr: string }
  | { op: 'SetCellValue'; addr: string; value: string }
  | { op: 'ClearCell'; addr: string }
  | { op: 'SetShapeText'; addr: string; text: string }
  | { op: 'SetShapeBox'; addr: string; x: number; y: number; cx: number; cy: number }
  | { op: 'DeleteShape'; addr: string }
  | {
      op: 'AddAnnotation'
      page: number
      kind: string
      rect: [number, number, number, number]
      contents?: string | null
      color?: [number, number, number] | null
    }
  | { op: 'DeleteAnnotation'; page: number; index: number }
  | { op: 'DeletePage'; page: number }
  /** Clockwise, relative to the page's current rotation. Quarter turns only. */
  | { op: 'RotatePage'; page: number; degrees: number }
  | { op: 'SetText'; content: string }

/**
 * The address a patch targets, for de-duplication in the queue.
 *
 * Patches that name a node collapse to the latest one — typing a word produces
 * one patch per keystroke and only the final text matters. Patches with no
 * address (annotations) never collapse, because each one is a distinct act.
 */
export function patchKey(patch: DraftPatch): string | null {
  if ('addr' in patch) return `${patch.op}:${patch.addr}`
  if (patch.op === 'SetText') return 'SetText'
  return null
}

// ---------------------------------------------------------------- summaries

export interface DraftSummary {
  id: number
  title: string
  format: string
  origin_kind: string
  source_document_id: number | null
  current_version: number
  created_at: string
  updated_at: string
}

export interface VersionSummary {
  version_no: number
  byte_size: number
  label: string | null
  created_at: string
  is_current: boolean
}

export interface PatchResult {
  version: number
  model: DraftViewModel
}

/** A 409 from the patch endpoint, carrying the document as it now is. */
export class StaleVersionError extends Error {
  constructor(
    public readonly currentVersion: number,
    public readonly model: DraftViewModel | null,
  ) {
    super(
      `This draft moved to version ${currentVersion} while you were editing. ` +
        'Your last edit was not applied.',
    )
    this.name = 'StaleVersionError'
  }
}

/**
 * Raise the backend's structured error rather than a status code.
 *
 * The backend always answers with `{ error, detail, action }`. Throwing the
 * detail plus the action is what lets the workspace show a user something they
 * can act on instead of "Request failed (422)".
 */
async function raise(response: Response): Promise<never> {
  let detail = `Request failed (${response.status})`
  let action = ''
  try {
    const body = await response.json()
    if (body?.detail) detail = String(body.detail)
    if (body?.action) action = String(body.action)
  } catch {
    // A non-JSON error body means the failure happened below our handlers;
    // the status text is then the most honest thing we have.
    if (response.statusText) detail = response.statusText
  }
  throw new Error(action ? `${detail} ${action}` : detail)
}

async function json<T>(response: Response): Promise<T> {
  if (!response.ok) await raise(response)
  return (await response.json()) as T
}

const base = () => getApiBaseSync()

// ---------------------------------------------------------------- calls

export async function listDrafts(): Promise<DraftSummary[]> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts`)
  const body = await json<{ drafts: DraftSummary[] }>(r)
  return body.drafts
}


// NOTE: `GET /drafts/:id` has no client here on purpose. Every screen that
// needs a draft's metadata already has it from `listDrafts`, and the route is
// covered by the API tests. Add a wrapper when something actually needs one.

export async function createFromVault(documentId: number, title?: string): Promise<DraftSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ document_id: documentId, title }),
  })
  return json(r)
}

export async function createFromUpload(file: File): Promise<DraftSummary> {
  const form = new FormData()
  form.append('file', file, file.name)
  // Safe to retry: creation is not deduplicated, but a retry only happens when
  // fetch itself threw, which means the request never reached the server.
  const r = await fetchWithNetworkRetry(`${base()}/drafts/upload`, { method: 'POST', body: form })
  return json(r)
}

export async function createBlank(title: string, format: string): Promise<DraftSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/blank`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ title, format }),
  })
  return json(r)
}

export async function renameDraft(id: number, title: string): Promise<void> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ title }),
  })
  if (!r.ok) await raise(r)
}

export async function deleteDraft(id: number): Promise<void> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}`, { method: 'DELETE' })
  if (!r.ok) await raise(r)
}

export async function getContent(id: number): Promise<DraftViewModel> {
  return json(await fetchWithNetworkRetry(`${base()}/drafts/${id}/content`))
}

/**
 * Apply queued edits.
 *
 * Retried on network failure, which is safe precisely because of
 * `base_version`: a retry whose first attempt actually landed comes back as a
 * 409 rather than applying the same edit twice.
 */
export async function applyPatches(
  id: number,
  patches: DraftPatch[],
  baseVersion: number,
  options: { explicitSave?: boolean; label?: string } = {},
): Promise<PatchResult> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/patch`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      patches,
      base_version: baseVersion,
      explicit_save: options.explicitSave ?? false,
      label: options.label ?? null,
    }),
  })

  if (r.status === 409) {
    const body = await r.json().catch(() => null)
    throw new StaleVersionError(body?.current_version ?? baseVersion, body?.model ?? null)
  }
  return json(r)
}

export async function listVersions(id: number): Promise<VersionSummary[]> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/versions`)
  const body = await json<{ versions: VersionSummary[] }>(r)
  return body.versions
}

export async function checkpoint(id: number, label: string): Promise<number> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/versions`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ title: label }),
  })
  const body = await json<{ version: number }>(r)
  return body.version
}

export async function restoreVersion(id: number, versionNo: number): Promise<PatchResult> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/versions/${versionNo}/restore`, {
    method: 'POST',
  })
  return json(r)
}

export async function publishToVault(id: number): Promise<number> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/publish`, { method: 'POST' })
  const body = await json<{ document_id: number }>(r)
  return body.document_id
}

export async function downloadDraft(id: number): Promise<Blob> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/raw`)
  if (!r.ok) await raise(r)
  return r.blob()
}

/**
 * URL of a rendered PDF page.
 *
 * Carries the version so the browser's own cache is invalidated by a save
 * rather than by us having to bust it. The CSP allows `http://127.0.0.1:*` in
 * `img-src` specifically so this can be used as an `<img src>`.
 */
export function pageImageUrl(id: number, page: number, version: number, scale = 1.5): string {
  return `${base()}/drafts/${id}/page/${page}?scale=${scale}&v=${version}`
}

/** Human file-size, matching how the storage panel phrases it. */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}
