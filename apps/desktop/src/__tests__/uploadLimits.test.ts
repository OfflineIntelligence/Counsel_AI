import { describe, it, expect } from 'vitest'
import {
  MAX_UPLOAD_TOTAL_BYTES,
  SERVER_BODY_LIMIT_BYTES,
  exceedsUploadBudget,
  formatMegabytes,
  totalBytesOf,
  uploadTooLargeMessage,
} from '../uploadLimits'

/**
 * The Rust side already pins SERVER_BODY_LIMIT_BYTES against the router's real
 * DefaultBodyLimit (`frontend_upload_limit_mirrors_the_server_limit` in
 * thread_server.rs). These tests cover what that one cannot: that the budget
 * derived from it is actually usable, and that the shared message tells the
 * user something true.
 */
describe('uploadLimits', () => {
  it('leaves headroom under the server limit rather than matching it', () => {
    // A budget equal to the server limit would let a batch through that the
    // multipart framing then pushes over — the 413 this module exists to
    // prevent, just harder to reproduce.
    expect(MAX_UPLOAD_TOTAL_BYTES).toBeLessThan(SERVER_BODY_LIMIT_BYTES)
    // ...but the reserve must not be so large it becomes the real limit. The
    // old 30MB cap wasted ~40% of the allowance; anything under 5% is fine.
    const reserveFraction =
      (SERVER_BODY_LIMIT_BYTES - MAX_UPLOAD_TOTAL_BYTES) / SERVER_BODY_LIMIT_BYTES
    expect(reserveFraction).toBeLessThan(0.05)
  })

  it('is more generous than the base64-era cap it replaced', () => {
    // Regression guard on the actual fix: the previous limit was 30MB, sized
    // for a ~33% base64 inflation that multipart does not incur.
    expect(MAX_UPLOAD_TOTAL_BYTES).toBeGreaterThan(30 * 1024 * 1024)
  })

  it('accepts a batch exactly at the budget and rejects one byte more', () => {
    expect(exceedsUploadBudget(MAX_UPLOAD_TOTAL_BYTES)).toBe(false)
    expect(exceedsUploadBudget(MAX_UPLOAD_TOTAL_BYTES + 1)).toBe(true)
  })

  it('accepts an empty batch', () => {
    expect(totalBytesOf([])).toBe(0)
    expect(exceedsUploadBudget(0)).toBe(false)
  })

  it('sums a batch regardless of what other fields the items carry', () => {
    expect(totalBytesOf([{ size: 10 }, { size: 32 }])).toBe(42)

    // Called with browser `File` objects in LocalFilesPanel and with Tauri
    // stat results in ChatWindow, so it must key only on `size`. Declared as
    // typed variables rather than inline literals because that is how both
    // callers actually pass them — an inline literal would additionally be
    // subject to TS's excess-property check, which is not the property under
    // test here.
    const paperclipFile = { name: 'a.pdf', path: 'C:/a.pdf', size: 5, status: 'ready' }
    const localStorageFile = { name: 'b.docx', size: 37, type: 'application/msword' }
    expect(totalBytesOf([paperclipFile, localStorageFile])).toBe(42)
  })

  it('states the total, the limit, and the remedy in one message', () => {
    const oversize = MAX_UPLOAD_TOTAL_BYTES + 12 * 1024 * 1024
    const message = uploadTooLargeMessage(oversize)

    // What they selected, and what the ceiling is — a limit message missing
    // either one leaves the user guessing how much to remove.
    expect(message).toContain(formatMegabytes(oversize))
    expect(message).toContain(formatMegabytes(MAX_UPLOAD_TOTAL_BYTES))
    // And what to do about it.
    expect(message).toMatch(/smaller batches/i)
  })

  it('compares both sides of the message in the same unit', () => {
    // The reason formatMegabytes is fixed-unit: an adaptive formatter would
    // render a large total as GB and the limit as MB, which reads as a
    // contradiction ("0.5 GB is over the 49 MB limit" invites a double-take).
    const message = uploadTooLargeMessage(3 * 1024 * 1024 * 1024)
    expect(message).not.toMatch(/\bGB\b|\bKB\b/)
    expect(message.match(/ MB\b/g)?.length).toBe(2)
  })

  it('formats megabytes to one decimal place', () => {
    expect(formatMegabytes(0)).toBe('0.0 MB')
    expect(formatMegabytes(1024 * 1024)).toBe('1.0 MB')
    expect(formatMegabytes(1.5 * 1024 * 1024)).toBe('1.5 MB')
  })
})
