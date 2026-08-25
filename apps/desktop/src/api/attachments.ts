import { getApiBaseSync } from './backendUrl'
import { fetchWithNetworkRetry } from './fetchWithTimeout'
import { SUPPORTED_LIST_LABEL } from '../supportedFormats'

/**
 * Attach-time document processing.
 *
 * A picked file is sent to the backend the moment it is chosen, not when the
 * message is sent. The backend extracts it immediately (through the per-format
 * extraction lanes) and returns a `document_id`, so by the time the user
 * finishes typing, the content is usually already in the database and the send
 * carries an id rather than megabytes of base64.
 *
 * The old flow read the file and base64-encoded it inside the send handler,
 * which put every extraction — including OCR of a scanned PDF — squarely
 * between pressing send and the first token.
 */

export interface AttachedDocument {
  documentId: number
  filename: string
  /** 'ok' | 'failed' | 'partial', as classified by the backend extractor. */
  extractionStatus: string
  extractionError?: string | null
  charCount: number
  /** True when the backend recognised the content and skipped re-extraction. */
  reused: boolean
  /**
   * Which engine read the document: 'vision_model' | 'windows_ocr' | 'native'.
   * Images read with 'windows_ocr' get a UI notice — basic OCR cannot read
   * handwriting; an active vision model can.
   */
  extractionEngine: string
}

export interface AttachResponse {
  documents: AttachedDocument[]
  /** Filenames the server refused, by type. Named so they can be shown. */
  rejected: string[]
}

/**
 * POST the attach form, retrying TRANSIENT network failures.
 *
 * Why this exists (seen in production, 2026-08-08): when Windows suspends the
 * WebView's network stack — laptop sleep, lid close, power saving — every
 * in-flight fetch dies with `TypeError: Failed to fetch`
 * (net::ERR_NETWORK_IO_SUSPENDED). Without retries, one such moment
 * permanently marks the attachment chip "failed" even though nothing is wrong
 * with the file, the backend, or the pipeline. Retrying is SAFE here: the
 * backend deduplicates by content hash, so a request that actually landed
 * before the response was lost simply resolves to the same document.
 *
 * Only network-level failures (fetch throwing) are retried. HTTP error
 * responses are real answers from the backend and are returned as-is.
 */
async function postAttachForm(form: FormData): Promise<Response> {
  return fetchWithNetworkRetry(`${getApiBaseSync()}/documents/attach`, {
    method: 'POST',
    body: form,
  })
}

/**
 * Upload and process one or more files immediately.
 *
 * Sent as multipart rather than base64 JSON: base64 inflates by ~33%, and this
 * path exists to make attaching large scans cheaper, not more expensive.
 */
export async function processAttachments(
  files: { name: string; bytes: Uint8Array }[],
): Promise<AttachResponse> {
  if (files.length === 0) return { documents: [], rejected: [] }

  const form = new FormData()
  for (const file of files) {
    // Copy into a fresh ArrayBuffer: a Uint8Array from Tauri's readFile may be
    // a view over a larger buffer, and Blob would otherwise capture all of it.
    const copy = new Uint8Array(file.bytes.length)
    copy.set(file.bytes)
    form.append('files', new Blob([copy]), file.name)
  }

  const response = await postAttachForm(form)

  if (response.status === 415) {
    throw new Error(
      `None of those files are a supported type. Supported: ${SUPPORTED_LIST_LABEL}`,
    )
  }
  if (!response.ok) {
    const detail = await response.text().catch(() => '')
    throw new Error(
      `Could not process the attachment${detail ? `: ${detail}` : ` (HTTP ${response.status})`}`,
    )
  }

  const json = await response.json()
  return {
    documents: (json.documents ?? []).map((d: Record<string, unknown>) => ({
      documentId: d.document_id as number,
      filename: d.filename as string,
      extractionStatus: d.extraction_status as string,
      extractionError: (d.extraction_error as string | null) ?? null,
      charCount: (d.char_count as number) ?? 0,
      reused: Boolean(d.reused),
      extractionEngine: (d.extraction_engine as string) ?? 'native',
    })),
    rejected: json.rejected ?? [],
  }
}
