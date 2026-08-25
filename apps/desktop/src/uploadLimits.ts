/**
 * How much a single upload may carry — the single frontend definition.
 *
 * Both entry points that send files to the backend share this budget:
 *   - the chat paperclip  (ChatWindow → POST /documents/attach)
 *   - Local Storage       (LocalFilesPanel → POST /files/upload)
 *
 * Previously only the paperclip enforced a limit, and it enforced the wrong
 * one. Local Storage enforced none at all, so selecting a folder larger than
 * the server's body limit produced a raw `Upload failed: 413 - <axum body>`
 * in the UI. One limit, two entry points, one message.
 *
 * # Where the number comes from
 *
 * `MAX_UPLOAD_TOTAL_BYTES` is DERIVED from the server's real limit rather than
 * picked. The authority is `DefaultBodyLimit::max(50 * 1024 * 1024)` in
 * `crates/offline-intelligence/src/thread_server.rs`, applied as a router-wide
 * layer, so it governs every upload route. If that layer changes,
 * `SERVER_BODY_LIMIT_BYTES` below must change with it — it is a mirror, not an
 * independent choice.
 *
 * # Why the old paperclip cap was wrong
 *
 * It was 30 MB, justified in a comment as "headroom under the 50MB body limit
 * for base64 (~33% inflation) + JSON overhead". That reasoning described a
 * transport the app no longer uses: attachments moved to multipart
 * (`api/attachments.ts`), and the send now carries only `{name, document_id}`
 * rather than a base64 payload. Multipart framing costs a boundary plus a
 * couple of headers per part — on the order of a few hundred bytes per file,
 * not a third of the payload — so ~18 MB of the user's allowance was being
 * withheld for an inflation that no longer happens.
 */

/**
 * The server's request-body ceiling. Mirrors `DefaultBodyLimit::max(...)` in
 * thread_server.rs. Exceeding it yields HTTP 413 before any handler runs, so
 * the client must gate below it to say anything useful about the failure.
 */
export const SERVER_BODY_LIMIT_BYTES = 50 * 1024 * 1024

/**
 * Reserve held back from the server limit for multipart framing.
 *
 * Each part contributes a boundary delimiter, a `Content-Disposition` header
 * carrying the filename, a `Content-Type`, and CRLFs — a few hundred bytes,
 * scaling with the number of files rather than their size. A folder upload of
 * several hundred small files is therefore the worst case, and even that lands
 * well inside a flat 1 MB. Kept flat and generous instead of computed per file:
 * a budget the user can predict ("about 49 MB") is worth more than one that
 * shifts by a few kilobytes depending on how long their filenames are.
 */
const MULTIPART_FRAMING_RESERVE_BYTES = 1024 * 1024

/** Largest total file size one upload request may carry. */
export const MAX_UPLOAD_TOTAL_BYTES =
  SERVER_BODY_LIMIT_BYTES - MULTIPART_FRAMING_RESERVE_BYTES

/** Sum the sizes of a batch. Works for both `File` and Tauri stat results. */
export function totalBytesOf(files: readonly { size: number }[]): number {
  return files.reduce((sum, f) => sum + f.size, 0)
}

/** Whether a batch of this total size would be rejected by the server. */
export function exceedsUploadBudget(totalBytes: number): boolean {
  return totalBytes > MAX_UPLOAD_TOTAL_BYTES
}

/**
 * Whole megabytes, for limit messages.
 *
 * Deliberately fixed-unit rather than adaptive: a message comparing a total
 * against a limit reads far better when both sides use the same unit
 * ("62 MB, over the 49 MB limit") than when one auto-scales to GB. The
 * adaptive formatters in ChatWindow and ModelsPanel serve a different purpose
 * (labelling one file or one download) and are left alone.
 */
export function formatMegabytes(bytes: number): string {
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

/**
 * The one message shown when a batch is too large, wherever it happens.
 *
 * Says three things, because a limit message that omits any of them sends the
 * user guessing: what they selected, what the ceiling is, and what to do about
 * it. The remedy is true at both entry points — a paperclip batch and a Local
 * Storage batch are each persisted as they complete, so splitting one up loses
 * nothing.
 */
export function uploadTooLargeMessage(totalBytes: number): string {
  return (
    `These files total ${formatMegabytes(totalBytes)}, over the ` +
    `${formatMegabytes(MAX_UPLOAD_TOTAL_BYTES)} limit for a single upload. ` +
    `Add them in smaller batches instead — each batch is saved as it ` +
    `completes, so nothing is lost by splitting them up.`
  )
}
