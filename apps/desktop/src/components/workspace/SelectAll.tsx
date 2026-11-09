/**
 * "Select all" as a checkbox with its label, for the files toolbars. Shows a
 * dash when only some items are selected; clicking selects everything, or
 * clears the selection when everything is already selected.
 */

import { Check, Minus } from 'lucide-react'

export default function SelectAll({
  total,
  selected,
  onSelectAll,
  onClear,
  compact = false,
}: {
  total: number
  selected: number
  onSelectAll: () => void
  onClear: () => void
  /** Checkbox only, for a list header cell. */
  compact?: boolean
}) {
  const all = total > 0 && selected >= total
  const some = selected > 0 && !all
  const box = (
    <span className={`ws-check${all || some ? ' ws-check-on' : ''}`} aria-hidden="true">
      {all && <Check size={12} strokeWidth={3} />}
      {some && <Minus size={12} strokeWidth={3} />}
    </span>
  )
  return (
    <button
      type="button"
      className={`ws-select-all${compact ? ' ws-select-all--compact' : ''}`}
      role="checkbox"
      aria-checked={all ? 'true' : some ? 'mixed' : 'false'}
      aria-label="Select all"
      disabled={total === 0}
      onClick={e => {
        e.stopPropagation()
        if (all) onClear()
        else onSelectAll()
      }}
    >
      {box}
      {!compact && <span>Select all</span>}
    </button>
  )
}