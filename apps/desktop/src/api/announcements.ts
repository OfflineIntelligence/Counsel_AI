/**
 * Which in-app announcements this machine has already dismissed.
 *
 * Backed by the `app_settings` table through `/announcements`, not by
 * `localStorage` — see the module docs on `announcements_api.rs` for why a
 * webview-local flag was the wrong home for this.
 *
 * # Failure is not "unseen"
 *
 * Both calls swallow their errors, and the direction of each fallback is
 * chosen rather than incidental:
 *
 * - A failed READ reports `true` (treat as seen), so a backend that is still
 *   starting cannot flash a what's-new dialog at someone who dismissed it
 *   months ago. The cost of being wrong is that a genuinely new announcement
 *   waits until the next launch, which nobody notices.
 * - A failed WRITE is swallowed too, because the dialog has already closed by
 *   then. Blocking the UI on a preference write, or raising a toast about it,
 *   would be louder than the thing being recorded.
 *
 * This is the opposite of the server's own fallback, which reports `false` on
 * a read error. That is deliberate: the server cannot tell a cold start from
 * a corrupt row, so it answers honestly and lets the client decide how to be
 * wrong.
 */

import { fetchWithNetworkRetry } from './fetchWithTimeout'
import { base } from './http'

/** Ids are `[a-z0-9_-]`, max 64 chars — the server rejects anything else. */
export type AnnouncementId = string

export async function hasSeenAnnouncement(id: AnnouncementId): Promise<boolean> {
  try {
    const r = await fetchWithNetworkRetry(`${base()}/announcements/${encodeURIComponent(id)}/seen`)
    if (!r.ok) return true
    const body = (await r.json()) as { seen?: boolean }
    return body.seen === true
  } catch {
    return true
  }
}

export async function markAnnouncementSeen(id: AnnouncementId): Promise<void> {
  try {
    await fetchWithNetworkRetry(`${base()}/announcements/seen`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id }),
    })
  } catch {
    /* Recorded next time. See the module note above. */
  }
}
