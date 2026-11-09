/**
 * The three lines every workspace API module would otherwise repeat.
 *
 * These lived inside `drafts.ts` until matters and the library needed them
 * too. Copying them would have been worse than moving them: `raise` is the
 * only reason the workspace can show a user a sentence they can act on instead
 * of a status code, and a second copy is a second place for that to drift.
 */

import { getApiBaseSync } from './backendUrl'

/**
 * Raise the backend's structured error rather than a status code.
 *
 * The backend always answers with `{ error, detail, action }`. Throwing the
 * detail plus the action is what lets the workspace show a user something they
 * can act on instead of "Request failed (422)".
 */
export async function raise(response: Response): Promise<never> {
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

export async function json<T>(response: Response): Promise<T> {
  if (!response.ok) await raise(response)
  return (await response.json()) as T
}

/** Throw on failure for calls that answer with no body. */
export async function ok(response: Response): Promise<void> {
  if (!response.ok) await raise(response)
}

export const base = (): string => getApiBaseSync()
