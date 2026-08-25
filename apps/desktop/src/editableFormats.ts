/**
 * The document workspace's format policy, in one place.
 *
 * Deliberately NARROWER than `supportedFormats.ts`. That list governs what can
 * be attached and read; this one governs what can be OPENED IN AN EDITOR, and
 * the difference matters: legacy binary Office files (.doc/.xls/.ppt) are
 * accepted as attachments and refused by the extractors with a "save as .docx"
 * message, but they must never reach an editor that cannot round-trip them.
 *
 * The Rust counterpart is `document_workspace::EDITABLE_FORMATS`, and
 * `editable_formats_match_the_frontend_list` keeps the two identical by parsing
 * this file. Both must list the same extensions in the same order.
 *
 * The server is still the real gate — this list only stops the UI from offering
 * a file the backend would refuse.
 */

export const EDITABLE_EXTENSIONS = ['docx', 'xlsx', 'pptx', 'pdf', 'txt']

/** `accept` attribute for a file input. */
export const EDITABLE_ACCEPT = EDITABLE_EXTENSIONS.map(e => `.${e}`).join(',')

export function extensionOf(filename: string): string {
  const at = filename.lastIndexOf('.')
  return at === -1 ? '' : filename.slice(at + 1).toLowerCase()
}

export function isEditableFormat(filename: string): boolean {
  return EDITABLE_EXTENSIONS.includes(extensionOf(filename))
}

/**
 * Largest file the workspace will open, mirroring `MAX_EDITABLE_BYTES` in Rust.
 *
 * Sits below the 50 MB server body limit on purpose, so a draft can always be
 * uploaded and downloaded. Checked here as well as on the server so an
 * oversized file gets a sentence the user can act on, rather than the bare 413
 * `DefaultBodyLimit` produces before any handler runs.
 */
export const MAX_EDITABLE_BYTES = 40 * 1024 * 1024

export function tooLargeToEditMessage(filename: string, bytes: number): string {
  return (
    `"${filename}" is ${(bytes / (1024 * 1024)).toFixed(1)} MB, beyond the ` +
    `${MAX_EDITABLE_BYTES / (1024 * 1024)} MB the workspace can edit. ` +
    'It can still be read in the document viewer.'
  )
}
