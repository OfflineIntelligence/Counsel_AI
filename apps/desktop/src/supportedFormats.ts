/**
 * The file types this product accepts — the single frontend definition.
 *
 * MUST stay identical to `SUPPORTED_ATTACHMENT_EXTENSIONS` in
 * `crates/offline-intelligence/src/utils/file_processor.rs`. The Rust test
 * `supported_formats_match_the_frontend_list` reads THIS file and fails the
 * build if the two lists diverge, so a one-sided edit cannot ship.
 *
 * Why a narrow list at all: the backend extractor can technically parse RTF,
 * ODT, HTML, CSV, Markdown and many source-code types, and falls back to a
 * plain text decode for anything it does not recognise. Without this gate a
 * user could store a .zip and be shown mojibake as though it were document
 * content. Every type below is one the product reads with a real engine
 * (pdfium, Windows OCR, calamine, quick-xml).
 *
 * The picker is a convenience, not a security boundary — the upload API
 * enforces the same list server-side, so drag-and-drop and any direct caller
 * are gated too.
 */
export const SUPPORTED_EXTENSIONS = [
  'pdf',
  'doc',
  'docx',
  'xls',
  'xlsx',
  'ppt',
  'pptx',
  'txt',
  'png',
  'jpg',
  'jpeg',
] as const

export type SupportedExtension = (typeof SUPPORTED_EXTENSIONS)[number]

/** `accept` attribute value for `<input type="file">`, e.g. ".pdf,.doc,..." */
export const FILE_INPUT_ACCEPT = SUPPORTED_EXTENSIONS.map((e) => `.${e}`).join(',')

/** Extensions in the shape the Tauri native dialog expects (no dots). */
export const DIALOG_EXTENSIONS: string[] = [...SUPPORTED_EXTENSIONS]

/** Lowercased extension of a filename, or '' when it has none. */
export function extensionOf(filename: string): string {
  const dot = filename.lastIndexOf('.')
  if (dot <= 0 || dot === filename.length - 1) return ''
  return filename.slice(dot + 1).toLowerCase()
}

/** Whether this filename is a type the product accepts. */
export function isSupportedFile(filename: string): boolean {
  return (SUPPORTED_EXTENSIONS as readonly string[]).includes(extensionOf(filename))
}

/** Human-readable list for user-facing messages, e.g. "PDF, DOC, DOCX, …". */
export const SUPPORTED_LIST_LABEL = SUPPORTED_EXTENSIONS.map((e) => e.toUpperCase()).join(', ')

/**
 * Split a batch into accepted and rejected files.
 *
 * Returned as a partition rather than a filter so callers can TELL the user
 * what was skipped and why. Silently dropping files is the failure mode this
 * exists to prevent: a user who drags in a folder and sees "3 uploaded" with
 * no mention of the 5 that were ignored has been misinformed.
 */
export function partitionSupported<T extends { name: string }>(
  files: T[],
): { accepted: T[]; rejected: T[] } {
  const accepted: T[] = []
  const rejected: T[] = []
  for (const f of files) {
    if (isSupportedFile(f.name)) accepted.push(f)
    else rejected.push(f)
  }
  return { accepted, rejected }
}
