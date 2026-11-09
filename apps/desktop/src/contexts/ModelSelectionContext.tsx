/**
 * Which local model the app is talking to — one selection, shared.
 *
 * The chat composer and the drafts composer both carry a model capsule, and
 * they are the same choice: switching in one switches the other, because
 * there is one engine with one model loaded. The state itself lives in App
 * (it is also persisted and refreshed when models change); this context is
 * how the two capsules reach it without threading props through every page.
 */
import { createContext, useContext, type ReactNode } from 'react'

export interface SelectableModel {
  id: string
  name: string
}

export interface ModelSelection {
  /** Installed models, plus the running one when it is not in the registry. */
  models: SelectableModel[]
  selected: { id: string; name: string; source: 'local' } | null
  select: (model: { id: string; name: string; source: 'local' } | null) => void
  /** Opens the Models page ("View All Models"). */
  openModels: () => void
}

const ModelSelectionContext = createContext<ModelSelection | null>(null)

export function ModelSelectionProvider({ value, children }: { value: ModelSelection; children: ReactNode }) {
  return <ModelSelectionContext.Provider value={value}>{children}</ModelSelectionContext.Provider>
}

/** Null outside the provider, so a component can render without the capsule. */
export function useModelSelection(): ModelSelection | null {
  return useContext(ModelSelectionContext)
}
