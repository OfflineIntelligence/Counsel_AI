/**
 * The editing loop: collect edits, debounce them, send them, adopt the result.
 *
 * # Why a queue rather than a save button
 *
 * Every keystroke produces a patch. Sending each one would be one HTTP round
 * trip per character; sending none until the user presses save loses work. So
 * patches accumulate, collapse by address, and flush after a short idle.
 *
 * # Why it survives a crash
 *
 * Unsent patches are mirrored to localStorage keyed by draft id, and replayed
 * on next open once the base version is confirmed to still match. An app killed
 * mid-sentence loses at most the debounce window — and if the version HAS moved
 * on, the replay is discarded rather than applied blind, because reapplying an
 * old edit to a newer document is worse than losing it.
 */

import { useCallback, useEffect, useRef, useState } from 'react'

import {
  applyPatches,
  patchKey,
  StaleVersionError,
  type DraftPatch,
  type DraftViewModel,
} from '../../api/drafts'

const DEBOUNCE_MS = 800
const STORAGE_PREFIX = 'oca.draft.pending.'

export type SaveStatus =
  | { kind: 'idle' }
  | { kind: 'pending' }
  | { kind: 'saving' }
  | { kind: 'saved'; at: number }
  | { kind: 'error'; message: string }

interface Options {
  draftId: number
  model: DraftViewModel | null
  /** Called with the refreshed model after every successful flush. */
  onModel: (model: DraftViewModel) => void
}

interface StoredQueue {
  baseVersion: number
  patches: DraftPatch[]
}

function storageKey(draftId: number) {
  return `${STORAGE_PREFIX}${draftId}`
}

function readStored(draftId: number): StoredQueue | null {
  try {
    const raw = localStorage.getItem(storageKey(draftId))
    return raw ? (JSON.parse(raw) as StoredQueue) : null
  } catch {
    // A corrupt or unreadable entry must not stop the draft from opening.
    return null
  }
}

/**
 * Collapse patches that target the same node, keeping the last.
 *
 * Typing "Agreement" queues nine SetRunText patches for one address and only
 * the ninth matters. Order is otherwise preserved, because an insert followed
 * by a delete is not the same as the reverse.
 */
export function collapse(patches: DraftPatch[]): DraftPatch[] {
  const out: DraftPatch[] = []
  const seenAt = new Map<string, number>()

  for (const patch of patches) {
    const key = patchKey(patch)
    if (key === null) {
      out.push(patch)
      continue
    }
    const existing = seenAt.get(key)
    if (existing !== undefined) {
      out[existing] = patch
    } else {
      seenAt.set(key, out.length)
      out.push(patch)
    }
  }
  return out
}

export function usePatchQueue({ draftId, model, onModel }: Options) {
  const [status, setStatus] = useState<SaveStatus>({ kind: 'idle' })

  const queue = useRef<DraftPatch[]>([])
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const inFlight = useRef(false)
  // Kept in a ref as well as in `model` so a flush scheduled before a re-render
  // still sends the version the server actually has.
  const version = useRef<number>(model?.version ?? 0)

  useEffect(() => {
    if (model) version.current = model.version
  }, [model])

  const persist = useCallback(() => {
    try {
      if (queue.current.length === 0) {
        localStorage.removeItem(storageKey(draftId))
      } else {
        const stored: StoredQueue = { baseVersion: version.current, patches: queue.current }
        localStorage.setItem(storageKey(draftId), JSON.stringify(stored))
      }
    } catch {
      // Private mode or a full quota. The edit still goes to the server on the
      // normal path; only crash recovery is lost, so this must not throw.
    }
  }, [draftId])

  const send = useCallback(
    async (explicitSave: boolean, label?: string) => {
      if (inFlight.current) return
      const batch = collapse(queue.current)
      if (batch.length === 0 && !explicitSave) {
        setStatus({ kind: 'idle' })
        return
      }

      inFlight.current = true
      queue.current = []
      persist()
      setStatus({ kind: 'saving' })

      try {
        const result = await applyPatches(draftId, batch, version.current, {
          explicitSave,
          label,
        })
        version.current = result.version
        onModel(result.model)
        setStatus({ kind: 'saved', at: Date.now() })
      } catch (error) {
        if (error instanceof StaleVersionError) {
          // Someone else moved the document on. Adopt what the server has
          // rather than replaying edits against a document that no longer
          // matches them — the addresses in those patches may now point at
          // completely different content.
          version.current = error.currentVersion
          if (error.model) onModel(error.model)
          setStatus({
            kind: 'error',
            message:
              'This draft was changed elsewhere, so your last edit was not applied. ' +
              'The current version is now shown.',
          })
        } else {
          // Put the batch back so the next flush retries it. This is the case
          // where the network dropped and the request never reached the server.
          queue.current = [...batch, ...queue.current]
          persist()
          setStatus({
            kind: 'error',
            message: error instanceof Error ? error.message : String(error),
          })
        }
      } finally {
        inFlight.current = false
      }
    },
    [draftId, onModel, persist],
  )

  /** Queue an edit. Flushes after the idle window. */
  const push = useCallback(
    (...patches: DraftPatch[]) => {
      if (patches.length === 0) return
      queue.current.push(...patches)
      persist()
      setStatus({ kind: 'pending' })

      if (timer.current) clearTimeout(timer.current)
      timer.current = setTimeout(() => void send(false), DEBOUNCE_MS)
    },
    [persist, send],
  )

  /** Send everything now and cut a version. Ctrl-S, closing, switching sheet. */
  const flush = useCallback(
    async (label?: string) => {
      if (timer.current) {
        clearTimeout(timer.current)
        timer.current = null
      }
      await send(true, label)
    },
    [send],
  )

  /** Whether anything is waiting to be sent — for the unsaved dot and close guard. */
  const hasPending = useCallback(() => queue.current.length > 0, [])

  /**
   * Throw away everything queued, without sending it.
   *
   * Exactly one caller: deleting the draft being edited. Without this, edits
   * queued moments before the delete would keep their debounce timer, fire
   * after `activeId` had been cleared, and POST to `/drafts/0/patch` — a 404
   * reported to the user as a failed save for a document that no longer
   * exists. Everywhere else, discarding a user's edits would be wrong.
   */
  const discard = useCallback(() => {
    if (timer.current) {
      clearTimeout(timer.current)
      timer.current = null
    }
    queue.current = []
    persist()
    setStatus({ kind: 'idle' })
  }, [persist])

  // Replay anything a crash left behind.
  useEffect(() => {
    const stored = readStored(draftId)
    if (!stored || stored.patches.length === 0) return

    if (model && stored.baseVersion !== model.version) {
      console.warn(
        `[drafts] discarding ${stored.patches.length} recovered edit(s) for draft ${draftId}: ` +
          `they were made against version ${stored.baseVersion}, but the draft is now at ${model.version}`,
      )
      try {
        localStorage.removeItem(storageKey(draftId))
      } catch {
        /* nothing more we can do, and it must not break opening the draft */
      }
      return
    }
    if (model) {
      console.info(`[drafts] replaying ${stored.patches.length} recovered edit(s)`)
      queue.current.push(...stored.patches)
      void send(true, 'Recovered')
    }
    // Deliberately keyed on the draft and on having a model at all: this must
    // run once per opened draft, not on every model refresh a save produces.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draftId, model !== null])

  // Flush on unmount so navigating away does not strand the last edit.
  useEffect(() => {
    return () => {
      if (timer.current) clearTimeout(timer.current)
      if (queue.current.length > 0) void send(true)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // A browser-level guard for the case the app is closed with work in flight.
  useEffect(() => {
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      if (queue.current.length > 0) {
        e.preventDefault()
        e.returnValue = ''
      }
    }
    window.addEventListener('beforeunload', onBeforeUnload)
    return () => window.removeEventListener('beforeunload', onBeforeUnload)
  }, [])

  return { status, push, flush, hasPending, discard }
}
