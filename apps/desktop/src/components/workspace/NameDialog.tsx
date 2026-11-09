/**
 * A small themed dialog that asks for a name, in place of the browser's
 * `window.prompt` (which shows "tauri.localhost says" in the system style).
 *
 * Usage: `const [dialog, askName] = useNameDialog()`, render `{dialog}`, then
 * `const name = await askName({ title: 'Create folder', initial: 'New folder' })`.
 * Resolves to the trimmed name, or `null` if cancelled.
 */

import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'

import Presence from './Presence'
import './workspace.css'

interface Ask {
  title: string
  label?: string
  initial?: string
  confirmLabel?: string
}

interface Pending extends Ask {
  resolve: (value: string | null) => void
}

export function useNameDialog(): [ReactNode, (ask: Ask) => Promise<string | null>] {
  const [pending, setPending] = useState<Pending | null>(null)
  const [open, setOpen] = useState(false)

  const askName = useCallback(
    (ask: Ask) =>
      new Promise<string | null>(resolve => {
        setPending({ ...ask, resolve })
        setOpen(true)
      }),
    [],
  )

  const finish = (value: string | null) => {
    pending?.resolve(value)
    setOpen(false)
  }

  const dialog = pending ? (
    <Presence open={open}>
      <div
        className="name-dialog-backdrop"
        onMouseDown={e => {
          if (e.target === e.currentTarget) finish(null)
        }}
      >
        <NameDialogBody key={pending.title + (pending.initial ?? '')} ask={pending} onDone={finish} />
      </div>
    </Presence>
  ) : null

  return [dialog, askName]
}

function NameDialogBody({ ask, onDone }: { ask: Ask; onDone: (v: string | null) => void }) {
  const [value, setValue] = useState(ask.initial ?? '')
  const input = useRef<HTMLInputElement | null>(null)

  useEffect(() => {
    input.current?.focus()
    input.current?.select()
  }, [])

  const submit = () => {
    const name = value.trim()
    if (name) onDone(name)
  }

  return (
    <div className="name-dialog" role="dialog" aria-modal="true" aria-label={ask.title}>
      <h2 className="name-dialog__title">{ask.title}</h2>
      {ask.label && <p className="name-dialog__label">{ask.label}</p>}
      <input
        ref={input}
        className="name-dialog__input"
        value={value}
        onChange={e => setValue(e.target.value)}
        onKeyDown={e => {
          if (e.key === 'Enter') submit()
          if (e.key === 'Escape') onDone(null)
        }}
        aria-label={ask.label ?? ask.title}
      />
      <div className="name-dialog__actions">
        <button type="button" className="name-dialog__btn" onClick={() => onDone(null)}>
          Cancel
        </button>
        <button
          type="button"
          className="name-dialog__btn name-dialog__btn--primary"
          onClick={submit}
          disabled={!value.trim()}
        >
          {ask.confirmLabel ?? 'Create'}
        </button>
      </div>
    </div>
  )
}