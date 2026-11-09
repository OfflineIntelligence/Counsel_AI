/**
 * Document workspace client.
 *
 * The types here mirror `document_workspace::view_model` in Rust exactly. The
 * `addr` on every node is the contract: the editor never sends a document, only
 * "replace the text of THIS node", and Rust splices that one node in the
 * original file. Anything the editor cannot render is never addressed, and so
 * can never be damaged by an edit.
 */

import { fetchWithNetworkRetry } from './fetchWithTimeout'
import { base, json, raise } from './http'

// ---------------------------------------------------------------- view models

export interface DocxInlineImage {
  data_url: string
  width_emu: number
  height_emu: number
}

export interface DocxRun {
  addr: string
  text: string
  bold: boolean
  italic: boolean
  underline: boolean
  strike?: boolean
  font?: string | null
  /** Points. */
  size?: number | null
  /** `RRGGBB`. */
  color?: string | null
  /** A Word highlight name, e.g. `yellow`. */
  highlight?: string | null
  vert_align?: 'superscript' | 'subscript' | null
  math?: boolean
  image?: DocxInlineImage | null
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
  /** `bullet` or `number` for a list item. */
  list?: 'bullet' | 'number' | null
  level?: number
  align?: 'left' | 'center' | 'right' | 'justify' | null
  line_spacing?: number | null
  cell: DocxCellRef | null
  /** Linear notation of a display equation, when the paragraph is one. */
  equation?: string | null
  page_break?: boolean
  runs: DocxRun[]
}

export interface DocxStyle {
  id: string
  name: string
  bold: boolean
  italic?: boolean
  size_half_points: number | null
  color: string | null
  font?: string | null
  kind?: string | null
}

export interface DocxDefaults {
  font: string | null
  size: number | null
}

export interface XlsxCellFormat {
  bold: boolean
  italic: boolean
  underline: boolean
  strike: boolean
  font: string | null
  size: number | null
  color: string | null
  fill: string | null
  align: string | null
  wrap: boolean
  num_fmt: string | null
}

export interface XlsxCell {
  addr: string
  value: string
  formula: string | null
  style: number | null
  numeric: boolean
  format?: XlsxCellFormat | null
}

export interface XlsxSheet {
  index: number
  name: string
  part: string
  rows: number
  cols: number
  cells: Record<string, XlsxCell>
  merges: string[]
  columns?: { min: number; max: number; width: number }[]
}

export interface PptxParagraph {
  addr: string
  text: string
  level: number
  align?: 'left' | 'center' | 'right' | 'justify' | null
  list?: 'bullet' | 'number' | null
  bold?: boolean
  italic?: boolean
  underline?: boolean
  strike?: boolean
  size?: number | null
  font?: string | null
  color?: string | null
}

export interface PptxShape {
  addr: string
  kind: string
  name: string
  placeholder: string | null
  bbox: { x: number; y: number; cx: number; cy: number } | null
  paragraphs: PptxParagraph[]
  image_rel: string | null
  image?: string | null
  fill?: string | null
  geometry?: string | null
  /** Rows of cells; each cell lists indices into `paragraphs`. */
  table?: number[][][] | null
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
  color?: [number, number, number] | null
}

export interface PdfPageModel {
  index: number
  width: number
  height: number
  annotations: PdfAnnotation[]
}

export interface PdfBookmark {
  title: string
  page: number | null
  level: number
}

export type DraftViewModel =
  | { format: 'docx'; version: number; blocks: DocxBlock[]; styles: DocxStyle[]; defaults?: DocxDefaults }
  | { format: 'xlsx'; version: number; sheets: XlsxSheet[] }
  | { format: 'pptx'; version: number; slides: PptxSlide[] }
  | { format: 'pdf'; version: number; pages: PdfPageModel[]; bookmarks?: PdfBookmark[] }
  | {
      format: 'txt'
      version: number
      content: string
      encoding: string
      line_ending: string
      bom: boolean
    }

// ---------------------------------------------------------------- patches

/** Character formatting; every field optional, empty string clears. */
export interface TextFormat {
  bold?: boolean
  italic?: boolean
  underline?: boolean
  strike?: boolean
  font?: string
  /** Points; 0 clears. */
  size?: number
  /** `RRGGBB`; empty clears. */
  color?: string
  /** Word: highlight name. Slides: `RRGGBB`. Empty clears. */
  highlight?: string
  vert_align?: 'superscript' | 'subscript' | 'baseline'
}

type Box = { x: number; y: number; cx: number; cy: number }

export type DraftPatch =
  // ---- docx
  | { op: 'SetRunText'; addr: string; text: string }
  | ({ op: 'SetRunFormat'; addr: string } & TextFormat)
  | ({ op: 'FormatRange'; addr: string; start: number; end: number } & TextFormat)
  | { op: 'SetParagraphStyle'; addr: string; style: string | null }
  | {
      op: 'SetParagraphFormat'
      addr: string
      align?: 'left' | 'center' | 'right' | 'justify'
      list?: 'bullet' | 'number' | 'none'
      indent?: number
      line_spacing?: number
    }
  | { op: 'SetParagraphText'; addr: string; text: string }
  | { op: 'InsertParagraphAfter'; addr: string; text: string }
  | { op: 'SplitParagraph'; addr: string; offset: number }
  | { op: 'MergeParagraphWithPrevious'; addr: string }
  | { op: 'DeleteParagraph'; addr: string }
  | { op: 'DeleteRun'; addr: string }
  | { op: 'InsertTable'; addr: string; rows: number; cols: number }
  | { op: 'InsertTableRow'; addr: string; below: boolean }
  | { op: 'DeleteTableRow'; addr: string }
  | { op: 'InsertTableColumn'; addr: string; right: boolean }
  | { op: 'DeleteTableColumn'; addr: string }
  | { op: 'DeleteTable'; addr: string }
  | {
      op: 'InsertImage'
      addr: string
      data_base64: string
      file_name: string
      width_emu: number
      height_emu: number
      x?: number
      y?: number
    }
  | { op: 'SetImageSize'; addr: string; width_emu: number; height_emu: number }
  | { op: 'InsertEquation'; addr: string; text: string }
  | { op: 'SetEquation'; addr: string; text: string }
  | { op: 'InsertPageBreak'; addr: string }
  // ---- xlsx
  | { op: 'SetCellValue'; addr: string; value: string }
  | { op: 'ClearCell'; addr: string }
  | ({
      op: 'SetCellFormat'
      addr: string
      fill?: string
      align?: 'left' | 'center' | 'right' | 'general'
      wrap?: boolean
      num_fmt?: string
    } & TextFormat)
  // ---- pptx
  | { op: 'SetShapeText'; addr: string; text: string }
  | ({
      op: 'SetShapeTextFormat'
      addr: string
      align?: 'left' | 'center' | 'right' | 'justify'
      list?: 'bullet' | 'number' | 'none'
    } & TextFormat)
  | { op: 'InsertShapeParagraph'; addr: string; text: string }
  | { op: 'SetShapeFill'; addr: string; color: string }
  | ({ op: 'SetShapeBox'; addr: string } & Box)
  | { op: 'DeleteShape'; addr: string }
  | ({ op: 'InsertTextBox'; addr: string; text: string } & Box)
  | ({ op: 'InsertShape'; addr: string; geometry: string; fill?: string } & Box)
  | ({ op: 'InsertSlideTable'; addr: string; rows: number; cols: number } & Box)
  | { op: 'AddSlide'; after: number }
  | { op: 'DuplicateSlide'; slide: number }
  | { op: 'DeleteSlide'; slide: number }
  // ---- pdf
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
 * Only "set the state of THIS node" patches collapse — typing a word produces
 * one patch per keystroke and only the final text matters. Everything that
 * creates, removes or splits something is a distinct act and never collapses:
 * two "insert row below" clicks are two rows, not one.
 */
const COLLAPSIBLE = new Set<DraftPatch['op']>([
  'SetRunText',
  'SetRunFormat',
  'SetParagraphStyle',
  'SetParagraphFormat',
  'SetCellValue',
  'SetCellFormat',
  'SetShapeText',
  'SetShapeTextFormat',
  'SetShapeFill',
  'SetShapeBox',
  'SetImageSize',
  'SetEquation',
])

/** Format patches merge rather than replace: bold then italic is both. */
export const MERGEABLE = new Set<DraftPatch['op']>([
  'SetRunFormat',
  'SetParagraphFormat',
  'SetCellFormat',
  'SetShapeTextFormat',
])

export function patchKey(patch: DraftPatch): string | null {
  if (patch.op === 'SetText') return 'SetText'
  if ('addr' in patch && COLLAPSIBLE.has(patch.op)) return `${patch.op}:${patch.addr}`
  return null
}

/** Patches that move addresses; the editor sends these on their own. */
export const STRUCTURAL = new Set<DraftPatch['op']>([
  'SplitParagraph',
  'MergeParagraphWithPrevious',
  'InsertParagraphAfter',
  'DeleteParagraph',
  'DeleteRun',
  'InsertTable',
  'InsertTableRow',
  'DeleteTableRow',
  'InsertTableColumn',
  'DeleteTableColumn',
  'DeleteTable',
  'InsertImage',
  'InsertEquation',
  'InsertPageBreak',
  'FormatRange',
  'SetParagraphText',
  'InsertShapeParagraph',
  'DeleteShape',
  'InsertTextBox',
  'InsertShape',
  'InsertSlideTable',
  'AddSlide',
  'DuplicateSlide',
  'DeleteSlide',
  'AddAnnotation',
  'DeleteAnnotation',
  'DeletePage',
  'RotatePage',
])

// ---------------------------------------------------------------- summaries

export interface DraftSummary {
  id: number
  title: string
  format: string
  origin_kind: string
  source_document_id: number | null
  current_version: number
  /** The folder this draft is in; null is the root. */
  folder_id: number | null
  /** The matter this draft is filed under; null is Unfiled. */
  matter_id: number | null
  /** draft | in_review | completed */
  status: DraftStatus
  starred: boolean
  created_at: string
  updated_at: string
}

/**
 * Where a document is in its life, not what it is.
 *
 * The order is the order they happen in, and the UI depends on that: a status
 * chip reads as progress only if 'completed' sits after 'in_review'.
 */
export const DRAFT_STATUSES = ['draft', 'in_review', 'completed'] as const
export type DraftStatus = (typeof DRAFT_STATUSES)[number]

export const DRAFT_STATUS_LABELS: Record<DraftStatus, string> = {
  draft: 'Draft',
  in_review: 'In review',
  completed: 'Completed',
}

export interface FolderSummary {
  id: number
  name: string
  parent_id: number | null
  created_at: string
  updated_at: string
}

/**
 * A document's first page, reduced to what a thumbnail can draw.
 *
 * Only PDF is a real raster — pdfium is the backend's sole renderer. The other
 * formats return their genuine first-page CONTENT and the tile draws a
 * miniature page from it, which is what actually lets someone recognise their
 * document in a grid.
 */
export type DraftPreview =
  | { kind: 'text'; lines: PreviewLine[] }
  | { kind: 'grid'; sheet: string; rows: string[][] }
  | { kind: 'slide'; lines: PreviewLine[]; total: number }
  | { kind: 'raster'; pages: number }
  | { kind: 'empty' }

export interface PreviewLine {
  text: string
  heading: boolean
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
  /** Edits Undo can take back and Redo can reapply. */
  undo_depth?: number
  redo_depth?: number
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



// ---------------------------------------------------------------- calls

export async function listDrafts(): Promise<DraftSummary[]> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts`)
  const body = await json<{ drafts: DraftSummary[] }>(r)
  return body.drafts
}

// ---------------------------------------------------------------- search

export interface SearchHit {
  draft_id: number
  title: string
  /** Body excerpt; matched runs are wrapped in MARK_OPEN/MARK_CLOSE. */
  snippet: string
  rank: number
}

export interface SearchResponse {
  hits: SearchHit[]
  /** Documents not yet indexed, so the UI can admit results are partial. */
  pending_index: number
}

/**
 * The control characters the backend wraps matches in.
 *
 * Not `<mark>`: rendering server-sent HTML would mean `dangerouslySetInnerHTML`
 * over document text, turning every file in the workspace into an injection
 * surface in order to achieve bold. These two bytes cannot occur in extracted
 * document text, so they are safe to use as delimiters.
 */
export const MARK_OPEN = ''
export const MARK_CLOSE = ''

/** Split a snippet into plain runs the caller renders as real elements. */
export function splitSnippet(snippet: string): { text: string; match: boolean }[] {
  const out: { text: string; match: boolean }[] = []
  for (const chunk of snippet.split(MARK_OPEN)) {
    const [matched, ...rest] = chunk.split(MARK_CLOSE)
    if (rest.length === 0) {
      // No closing marker, so this run was never inside a match.
      if (matched) out.push({ text: matched, match: false })
      continue
    }
    if (matched) out.push({ text: matched, match: true })
    const tail = rest.join(MARK_CLOSE)
    if (tail) out.push({ text: tail, match: false })
  }
  return out
}

export async function searchWorkspace(query: string): Promise<SearchResponse> {
  const r = await fetchWithNetworkRetry(
    `${base()}/workspace/search?q=${encodeURIComponent(query)}`,
  )
  return json(r)
}

// ---------------------------------------------------------------- compare

/** One side of a comparison. A draft's Vault original is its version 1. */
export type DocRef = { kind: 'draft'; id: number; version?: number }

export type WordOp = { op: 'equal' | 'insert' | 'delete'; text: string }

export type DiffOp =
  | { op: 'equal'; text: string }
  | { op: 'insert'; text: string; addr: string }
  | { op: 'delete'; text: string; addr: string }
  | {
      op: 'change'
      left: string
      right: string
      left_addr: string
      right_addr: string
      words: WordOp[]
    }

export interface DiffSummary {
  insertions: number
  deletions: number
  changes: number
  identical: boolean
}

export interface CompareResult {
  ops: DiffOp[]
  summary: DiffSummary
  left: { title: string; version: number }
  right: { title: string; version: number }
}

export async function compareDocuments(left: DocRef, right: DocRef): Promise<CompareResult> {
  const r = await fetchWithNetworkRetry(`${base()}/compare`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ left, right }),
  })
  return json(r)
}

// ---------------------------------------------------------------- folders

export async function listFolders(): Promise<FolderSummary[]> {
  const r = await fetchWithNetworkRetry(`${base()}/draft-folders`)
  const body = await json<{ folders: FolderSummary[] }>(r)
  return body.folders
}

export async function createFolder(
  name: string,
  parentId: number | null,
): Promise<FolderSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/draft-folders`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name, parent_id: parentId }),
  })
  return json(r)
}

export async function renameFolder(id: number, name: string): Promise<FolderSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/draft-folders/${id}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name }),
  })
  return json(r)
}

/**
 * Re-parent a folder. `null` moves it to the root.
 *
 * `parent_id` is always SENT, including as null — the backend distinguishes
 * present-and-null ("move to the root") from absent ("leave it where it is"),
 * so omitting the key would silently do nothing.
 */
export async function moveFolder(
  id: number,
  parentId: number | null,
): Promise<FolderSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/draft-folders/${id}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ parent_id: parentId }),
  })
  return json(r)
}

/** Deletes the folder and its subfolders. Drafts inside return to the root. */
export async function deleteFolder(
  id: number,
): Promise<{ deleted: number; drafts_moved_to_root: number }> {
  const r = await fetchWithNetworkRetry(`${base()}/draft-folders/${id}`, {
    method: 'DELETE',
  })
  return json(r)
}

export async function moveDraft(id: number, folderId: number | null): Promise<DraftSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/folder`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ folder_id: folderId }),
  })
  return json(r)
}

/**
 * A draft's thumbnail content.
 *
 * Never throws: a preview is decoration, and one unreadable document must not
 * take the whole grid down with it.
 */
export async function getPreview(id: number): Promise<DraftPreview> {
  try {
    const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/preview`)
    if (!r.ok) return { kind: 'empty' }
    return (await r.json()) as DraftPreview
  } catch {
    return { kind: 'empty' }
  }
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

/** Remove one saved version. The current version and the original are refused. */
export async function deleteVersion(id: number, versionNo: number): Promise<void> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/versions/${versionNo}`, { method: 'DELETE' })
  await json(r)
}

export async function restoreVersion(id: number, versionNo: number): Promise<PatchResult> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/versions/${versionNo}/restore`, {
    method: 'POST',
  })
  return json(r)
}

/** Step one edit back or forward. With nothing to step to, the model is returned unchanged. */
export async function stepHistory(id: number, direction: 'undo' | 'redo', baseVersion: number): Promise<PatchResult> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/${direction}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ base_version: baseVersion }),
  })
  return json(r)
}

export async function getUndoState(id: number): Promise<{ undo_depth: number; redo_depth: number }> {
  return json(await fetchWithNetworkRetry(`${base()}/drafts/${id}/undo`))
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

/** Move a document into a matter, or out of every matter with null. */
export async function assignMatter(id: number, matterId: number | null): Promise<DraftSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/matter`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ matter_id: matterId }),
  })
  return json(r)
}

export async function setDraftStatus(id: number, status: DraftStatus): Promise<DraftSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/status`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ status }),
  })
  return json(r)
}

export async function setDraftStarred(id: number, starred: boolean): Promise<DraftSummary> {
  const r = await fetchWithNetworkRetry(`${base()}/drafts/${id}/star`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ starred }),
  })
  return json(r)
}
