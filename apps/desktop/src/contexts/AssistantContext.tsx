/**
 * The AI Assistant, available on every page.
 *
 * One panel lives at the app root; any page can tell it what the user is
 * looking at. The app registers a plain description of each page ("Library",
 * "Vault"); a page with something richer to offer — an open draft, a selected
 * precedent — registers over it while mounted, and the panel reads whichever
 * is most specific.
 *
 * # Threads
 *
 * One conversation follows the user between pages. Inside a draft the thread
 * belongs to that document instead, so reopening a contract brings back what
 * was asked about it. Both kinds use the drafting-assistant session prefix,
 * which keeps them out of the main chat list.
 */

import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'

export interface AssistantPage {
  /** Short page name shown in the panel, e.g. "Library". */
  label: string
  /** What is open or selected, e.g. a document title. */
  title?: string
  /** Text the assistant may read to answer, already trimmed by the page. */
  text?: string
  /** A hint about the text, e.g. "DOCX". */
  format?: string
  /** Conversation key. Absent means the global thread. */
  thread?: string
  /** Where "Insert" puts an answer; absent hides the button. */
  onInsertText?: (text: string) => void
  /** Starter questions for this page. */
  suggestions?: string[]
}

interface AssistantState {
  open: boolean
  setOpen: (open: boolean) => void
  toggle: () => void
  /** Open the panel and send a question. */
  ask: (text: string) => void
  /** A question waiting for the panel to send; the nonce makes a repeat re-send. */
  pending: { text: string; nonce: number } | null
  page: AssistantPage
  /** Register richer context over the app's default; returns an unregister. */
  register: (page: AssistantPage) => () => void
  /**
   * Claim Aurora for this page: the panel stops opening, and everything
   * that would have opened it (the rail button, `ask`, `setOpen(true)`) calls
   * `focus` instead — the drafts composer uses this to glow and take the
   * question itself. Returns an unregister.
   */
  registerInlineHost: (focus: (text?: string) => void) => () => void
  /** True while a page hosts Aurora, so the panel can stay out of the way. */
  inlineHosted: boolean
  setDefaultPage: (page: AssistantPage) => void
}

const AssistantContext = createContext<AssistantState | null>(null)

const OPEN_KEY = 'oca.assistant.open'

export function AssistantProvider({ children }: { children: ReactNode }) {
  const [open, setOpenState] = useState(() => {
    try {
      return localStorage.getItem(OPEN_KEY) === '1'
    } catch {
      return false
    }
  })
  const [pending, setPending] = useState<{ text: string; nonce: number } | null>(null)
  const [defaultPage, setDefaultPage] = useState<AssistantPage>({ label: 'Counsel' })
  const [stack, setStack] = useState<{ id: number; page: AssistantPage }[]>([])
  const nextId = useRef(1)

  // A page hosting Aurora (the drafts composer). Kept as a ref for the
  // callbacks and as state for what renders.
  const inlineHost = useRef<((text?: string) => void) | null>(null)
  const [inlineHosted, setInlineHosted] = useState(false)

  const registerInlineHost = useCallback((focus: (text?: string) => void) => {
    inlineHost.current = focus
    setInlineHosted(true)
    return () => {
      if (inlineHost.current === focus) {
        inlineHost.current = null
        setInlineHosted(false)
      }
    }
  }, [])

  const setOpen = useCallback((value: boolean) => {
    // While a page hosts Aurora, opening the panel means drawing attention
    // to that page's own composer instead.
    if (value && inlineHost.current) {
      inlineHost.current()
      return
    }
    setOpenState(value)
    try {
      localStorage.setItem(OPEN_KEY, value ? '1' : '0')
    } catch {
      /* not remembered; still applied */
    }
  }, [])

  const toggle = useCallback(() => setOpen(!open), [open, setOpen])

  const ask = useCallback(
    (text: string) => {
      if (!text.trim()) return
      if (inlineHost.current) {
        inlineHost.current(text.trim())
        return
      }
      setOpen(true)
      setPending({ text: text.trim(), nonce: Date.now() })
    },
    [setOpen],
  )

  const register = useCallback((page: AssistantPage) => {
    const id = nextId.current++
    setStack(s => [...s.filter(e => e.id !== id), { id, page }])
    return () => setStack(s => s.filter(e => e.id !== id))
  }, [])

  const page = stack.length ? stack[stack.length - 1].page : defaultPage

  const value = useMemo<AssistantState>(
    () => ({ open, setOpen, toggle, ask, pending, page, register, setDefaultPage, registerInlineHost, inlineHosted }),
    [open, setOpen, toggle, ask, pending, page, register, registerInlineHost, inlineHosted],
  )

  return <AssistantContext.Provider value={value}>{children}</AssistantContext.Provider>
}

export function useAssistant(): AssistantState {
  const ctx = useContext(AssistantContext)
  if (!ctx) throw new Error('useAssistant must be used inside <AssistantProvider>')
  return ctx
}

/**
 * Tell the assistant what this page shows, for as long as it is mounted.
 *
 * Pass `null` to register nothing (e.g. while no document is open). The page
 * object may change on every render; only its content is compared.
 */
export function useAssistantPage(page: AssistantPage | null) {
  const ctx = useContext(AssistantContext)
  const register = ctx?.register
  const latest = useRef(page)
  useEffect(() => {
    latest.current = page
  })

  // Re-register only when what the assistant would read actually changes.
  const signature = page
    ? JSON.stringify([page.label, page.title, page.format, page.thread, page.text?.length, page.text?.slice(0, 200), !!page.onInsertText])
    : null

  useEffect(() => {
    if (!register || !signature || !latest.current) return
    const current = latest.current
    return register({
      ...current,
      // Always call the newest handler, not the one captured at registration.
      onInsertText: current.onInsertText ? (t: string) => latest.current?.onInsertText?.(t) : undefined,
    })
  }, [register, signature])
}
