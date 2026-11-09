/**
 * The Workspace's single source of truth for what exists on the server.
 *
 * # Why this is a hook and not state inside a view
 *
 * `DraftWorkspace` had grown to own every list, every mutation and every view's
 * state at once, and each new feature made it worse. Pulling server state out
 * here leaves the views owning only what is genuinely theirs — which tile is
 * selected, which pane is scrolled — so each one can be rendered in isolation
 * against stub data, which is how they get verified.
 *
 * # The one rule
 *
 * Anything that comes from or goes to the server lives here. Views call
 * `mutate` and never touch the API directly, so there is exactly one place
 * where a refresh can be forgotten.
 */

import { useCallback, useEffect, useState } from 'react'

import {
  listDrafts,
  listFolders,
  type DraftSummary,
  type FolderSummary,
} from '../../api/drafts'
import { listMatters, type MatterSummary } from '../../api/matters'

export interface WorkspaceData {
  drafts: DraftSummary[]
  folders: FolderSummary[]
  matters: MatterSummary[]
  busy: boolean
  error: string | null
  setError: (message: string | null) => void
  refresh: () => Promise<void>
  /** Run a server change, surface its message, and resync. */
  mutate: (work: () => Promise<unknown>) => Promise<void>
}

export function useWorkspaceData(): WorkspaceData {
  const [drafts, setDrafts] = useState<DraftSummary[]>([])
  const [folders, setFolders] = useState<FolderSummary[]>([])
  const [matters, setMatters] = useState<MatterSummary[]>([])
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      // In parallel: the two are independent, and the browser cannot draw a
      // correct tree until it has all three, so serialising them would only
      // make the folder structure arrive a beat after the files.
      const [nextDrafts, nextFolders, nextMatters] = await Promise.all([
        listDrafts(),
        listFolders(),
        listMatters(),
      ])
      setDrafts(nextDrafts)
      setFolders(nextFolders)
      setMatters(nextMatters)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }, [])

  const mutate = useCallback(
    async (work: () => Promise<unknown>) => {
      setBusy(true)
      setError(null)
      try {
        await work()
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e))
      } finally {
        // Resync even on failure. A refused move leaves the server right and
        // this list wrong, and showing the item where it is not would be a lie
        // the user has no way to detect.
        await refresh()
        setBusy(false)
      }
    },
    [refresh],
  )

  useEffect(() => {
    void refresh()
  }, [refresh])

  return { drafts, folders, matters, busy, error, setError, refresh, mutate }
}
