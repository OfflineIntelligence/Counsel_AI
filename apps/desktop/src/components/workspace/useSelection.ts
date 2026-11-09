/**
 * Multi-select over the file grid.
 *
 * # Why this exists before the features that use it
 *
 * Compare needs two items. Bundling needs many. A review table needs many.
 * Bulk move needs many. A tile could previously only be *opened*, so none of
 * those had anywhere to begin. This is the primitive all of them start from,
 * which is why it is built once, first, rather than four times badly.
 *
 * # Identity
 *
 * Items are keyed `draft:12` / `folder:3` rather than by bare id, because a
 * folder and a draft can share an id and selecting one would silently select
 * the other.
 */

import { useCallback, useMemo, useRef, useState } from 'react'

export type ItemKind = 'draft' | 'folder'

export interface SelectionItem {
  kind: ItemKind
  id: number
}

const keyOf = (kind: ItemKind, id: number) => `${kind}:${id}`

function parseKey(key: string): SelectionItem {
  const [kind, id] = key.split(':')
  return { kind: kind as ItemKind, id: Number(id) }
}

export interface Selection {
  count: number
  items: SelectionItem[]
  drafts: number[]
  folders: number[]
  isSelected: (kind: ItemKind, id: number) => boolean
  /** Toggle one item. `range` extends from the last toggled item (shift-click). */
  toggle: (kind: ItemKind, id: number, range?: boolean) => void
  selectAll: () => void
  clear: () => void
}

/**
 * @param ordered Every selectable item in the order it is displayed. Needed so
 *   a range selection follows what the user can see rather than insertion
 *   order — shift-clicking across a grid must select what lies between the two
 *   tiles on screen.
 */
export function useSelection(ordered: SelectionItem[]): Selection {
  const [keys, setKeys] = useState<Set<string>>(() => new Set())
  const anchor = useRef<string | null>(null)

  const orderedKeys = useMemo(
    () => ordered.map(i => keyOf(i.kind, i.id)),
    [ordered],
  )

  // Drop keys for items that have gone — a deleted or moved-away document
  // must not stay counted in "3 selected" while being invisible.
  const live = useMemo(() => {
    const present = new Set(orderedKeys)
    const kept = new Set<string>()
    keys.forEach(k => {
      if (present.has(k)) kept.add(k)
    })
    return kept
  }, [keys, orderedKeys])

  const toggle = useCallback(
    (kind: ItemKind, id: number, range = false) => {
      const key = keyOf(kind, id)
      setKeys(prev => {
        const next = new Set(prev)

        if (range && anchor.current && anchor.current !== key) {
          const from = orderedKeys.indexOf(anchor.current)
          const to = orderedKeys.indexOf(key)
          if (from !== -1 && to !== -1) {
            const [lo, hi] = from < to ? [from, to] : [to, from]
            // A range ADDS; it never clears what is already selected, so two
            // separate ranges can be built up without starting over.
            for (let i = lo; i <= hi; i++) next.add(orderedKeys[i])
            return next
          }
        }

        if (next.has(key)) next.delete(key)
        else next.add(key)
        return next
      })
      anchor.current = key
    },
    [orderedKeys],
  )

  const selectAll = useCallback(() => {
    setKeys(new Set(orderedKeys))
  }, [orderedKeys])

  const clear = useCallback(() => {
    setKeys(new Set())
    anchor.current = null
  }, [])

  const items = useMemo(() => [...live].map(parseKey), [live])
  const drafts = useMemo(() => items.filter(i => i.kind === 'draft').map(i => i.id), [items])
  const folders = useMemo(() => items.filter(i => i.kind === 'folder').map(i => i.id), [items])

  return {
    count: live.size,
    items,
    drafts,
    folders,
    isSelected: (kind, id) => live.has(keyOf(kind, id)),
    toggle,
    selectAll,
    clear,
  }
}
