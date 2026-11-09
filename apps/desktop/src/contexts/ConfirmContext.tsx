import React, { useCallback, useRef, useState } from 'react'
import { Dialog, DialogActions } from '../components/ui'
import { ConfirmContext, type ConfirmFn, type ConfirmOptions } from './confirm'
import '../components/rag/rag.css'

/**
 * App-styled confirmation, replacing the browser's `window.confirm` for
 * destructive actions. `await confirm({...})` (from `useConfirm`) resolves
 * true only when the user presses the confirm button; closing, Escape or
 * Cancel resolve false.
 */
export const ConfirmProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const [options, setOptions] = useState<ConfirmOptions | null>(null)
  const [open, setOpen] = useState(false)
  const resolver = useRef<((value: boolean) => void) | null>(null)

  const settle = useCallback((value: boolean) => {
    const resolve = resolver.current
    resolver.current = null
    setOpen(false)
    resolve?.(value)
  }, [])

  const confirm = useCallback<ConfirmFn>(opts => {
    // A second request while one is open cancels the first.
    resolver.current?.(false)
    return new Promise<boolean>(resolve => {
      resolver.current = resolve
      setOptions(opts)
      setOpen(true)
    })
  }, [])

  const danger = options?.danger ?? true

  return (
    <ConfirmContext.Provider value={confirm}>
      {children}
      {options && (
        <Dialog
          open={open}
          onClose={() => settle(false)}
          size="sm"
          className="confirm-dialog"
          title={options.title}
          footer={
            <DialogActions>
              <button type="button" className="ui-btn ui-btn--secondary" onClick={() => settle(false)}>
                {options.cancelLabel ?? 'Cancel'}
              </button>
              <button
                type="button"
                className={`ui-btn ${danger ? 'ui-btn--danger' : 'ui-btn--primary'}`}
                onClick={() => settle(true)}
              >
                {options.confirmLabel ?? 'Delete'}
              </button>
            </DialogActions>
          }
        >
          <div className="confirm-dialog__message">{options.message}</div>
        </Dialog>
      )}
    </ConfirmContext.Provider>
  )
}
