import type React from 'react'
import { createContext, useContext } from 'react'

/** Options for an app-styled confirmation (see ConfirmProvider). */
export interface ConfirmOptions {
  title: string
  message: React.ReactNode
  confirmLabel?: string
  cancelLabel?: string
  /** Destructive actions get the danger button. Defaults to true. */
  danger?: boolean
}

export type ConfirmFn = (options: ConfirmOptions) => Promise<boolean>

export const ConfirmContext = createContext<ConfirmFn | null>(null)

/**
 * Returns the app confirmation. Outside the provider (isolated component
 * tests) it falls back to `window.confirm` so behaviour stays testable.
 */
export function useConfirm(): ConfirmFn {
  const ctx = useContext(ConfirmContext)
  return (
    ctx ??
    (async (o: ConfirmOptions) =>
      window.confirm(typeof o.message === 'string' ? `${o.title}\n\n${o.message}` : o.title))
  )
}
