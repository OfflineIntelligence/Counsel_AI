/**
 * The agent as a top-right search bar — chat view only.
 *
 * The sidebar's Aurora button toggles the same `AssistantContext.open` the
 * old panel used. Here that state drives a collapsed magnifier instead: it
 * expands leftward into a search bar (the icon rides left as the field
 * grows), and asking expands an answer space below it. Agent space and
 * Aurora space are one and the same surface: user prompts render as
 * rectangles, agent answers as sharp-cornered squares.
 *
 * The thread is the page's own (`page.thread ?? global`), shared with the
 * panel on other views, so history follows the user there and back. The
 * component stays mounted with the chat window, so collapsing mid-stream
 * never aborts the answer — reopening shows it in flight.
 */

import { splitThinking } from '../MessageContent'
import { useCallback, useEffect, useRef, useState } from 'react'
import { ArrowUp, Check, Copy, Square, X } from 'lucide-react'

import {
  DRAFT_ASSISTANT_SESSION_PREFIX,
  ModelNotReadyError,
  fetchConversation,
  streamChat,
  type Message,
} from '../../api/chat'
import { announceModelsChanged, canAsk, getReadiness, prewarmModel, type Readiness } from '../../api/models'
import { useAssistant } from '../../contexts/AssistantContext'
import ModelSetupPrompt from '../ModelSetupPrompt'
import { systemPrompt } from './agentPrompt'
import ThinkingStatus from '../ThinkingStatus'
import AppMark from '../AppMark'
import { useIdleDismiss } from '../../utils/useIdleDismiss'
import './agent-search.css'

interface Turn {
  role: 'user' | 'assistant'
  content: string
  error?: boolean
}

export default function AgentSearch({ onOpenModels }: { onOpenModels?: () => void } = {}) {
  const { open: panelOpen, setOpen, page, pending, inlineHosted } = useAssistant()
  const open = panelOpen && !inlineHosted
  const thread = page.thread ?? `${DRAFT_ASSISTANT_SESSION_PREFIX}global`

  const [turns, setTurns] = useState<Turn[]>([])
  const [input, setInput] = useState('')
  const [streaming, setStreaming] = useState(false)
  const [copied, setCopied] = useState<number | null>(null)
  const [modelPrompt, setModelPrompt] = useState<Readiness | null>(null)
  const abort = useRef<AbortController | null>(null)
  const scroller = useRef<HTMLDivElement | null>(null)
  const field = useRef<HTMLInputElement | null>(null)
  const wrap = useRef<HTMLDivElement | null>(null)
  const pageRef = useRef(page)
  const turnsRef = useRef(turns)
  useEffect(() => {
    pageRef.current = page
    turnsRef.current = turns
  }, [page, turns])

  // A new thread starts empty; the restore below fills it back in.
  const [seenThread, setSeenThread] = useState(thread)
  if (seenThread !== thread) {
    setSeenThread(thread)
    setTurns([])
    setModelPrompt(null)
  }

  // Restore the thread's history from the backend (allowMissing: a thread is
  // saved on its first message, so opening one before that is empty, not an
  // error).
  useEffect(() => {
    let live = true
    fetchConversation(thread, { allowMissing: true })
      .then(conv => {
        if (!live || !conv) return
        const restored = conv.messages
          .filter((m): m is Message & { role: 'user' | 'assistant' } => m.role !== 'system')
          .map(m => ({ role: m.role, content: m.content }))
        setTurns(t => (t.length === 0 ? restored : t))
      })
      .catch(() => {
        /* No history yet, or the backend is down; start fresh. */
      })
    return () => {
      live = false
    }
  }, [thread])

  useEffect(() => () => abort.current?.abort(), [])

  // Follow the answer while stuck to the bottom; a manual scroll-up unsticks
  // so streaming never drags the view back down.
  const stickToBottomRef = useRef(true)
  useEffect(() => {
    if (stickToBottomRef.current) scroller.current?.scrollTo({ top: scroller.current.scrollHeight })
  }, [turns])
  const handleScroll = () => {
    const el = scroller.current
    if (!el) return
    stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80
  }

  // The control's measured height, published as --agent-h: the download
  // bubble docks below it, so the bar itself never moves. Re-published as
  // the answer space grows.
  useEffect(() => {
    const el = wrap.current
    if (!el) return
    const publish = () => {
      document.documentElement.style.setProperty('--agent-h', `${el.offsetHeight}px`)
    }
    publish()
    const ro = new ResizeObserver(publish)
    ro.observe(el)
    return () => {
      ro.disconnect()
      document.documentElement.style.removeProperty('--agent-h')
    }
  }, [open, turns.length, streaming, modelPrompt])
  // Focus the field as the bar finishes expanding.
  useEffect(() => {
    if (!open) return
    prewarmModel()
    const t = window.setTimeout(() => field.current?.focus(), 180)
    return () => window.clearTimeout(t)
  }, [open])

  const onModelReady = useCallback(() => {
    setModelPrompt(null)
    announceModelsChanged()
    field.current?.focus()
  }, [])

  const send = useCallback(
    async (question: string) => {
      const q = question.trim()
      if (!q || streaming) return
      try {
        const readiness = await getReadiness('agent')
        if (!canAsk(readiness)) {
          setInput(q)
          setModelPrompt(readiness)
          return
        }
      } catch {
        /* Backend unreachable: the send below reports it. */
      }
      setModelPrompt(null)
      const history: Message[] = turnsRef.current
        .filter(t => !t.error)
        .map(t => ({ role: t.role, content: t.content }))
      const messages: Message[] = [
        { role: 'system', content: systemPrompt(pageRef.current) },
        ...history,
        { role: 'user', content: q },
      ]
      setInput('')
      setTurns(t => [...t, { role: 'user', content: q }, { role: 'assistant', content: '' }])
      stickToBottomRef.current = true
      setStreaming(true)
      const controller = new AbortController()
      abort.current = controller
      try {
        for await (const chunk of streamChat(messages, thread, undefined, undefined, controller.signal)) {
          setTurns(t => {
            const next = t.slice()
            const last = next[next.length - 1]
            next[next.length - 1] = { ...last, content: last.content + chunk }
            return next
          })
        }
      } catch (e) {
        if (e instanceof ModelNotReadyError) {
          setTurns(t => t.slice(0, -2))
          setInput(q)
          setModelPrompt(e.readiness)
        } else if (!controller.signal.aborted) {
          setTurns(t => {
            const next = t.slice()
            next[next.length - 1] = { role: 'assistant', content: e instanceof Error ? e.message : String(e), error: true }
            return next
          })
        }
      } finally {
        setStreaming(false)
        abort.current = null
      }
    },
    [streaming, thread],
  )

  // A question asked from anywhere else (the chat composer's "Ask Aurora")
  // arrives here and opens the bar with it.
  const lastNonce = useRef<number | null>(null)
  useEffect(() => {
    if (!pending || pending.nonce === lastNonce.current) return
    lastNonce.current = pending.nonce
    setOpen(true)
    void send(pending.text)
  }, [pending, send, setOpen])

  // A minute without use collapses the bar back to its mark.
  useIdleDismiss(wrap, open, streaming || input.trim().length > 0, () => setOpen(false))

  const showSpace = open && (turns.length > 0 || streaming || modelPrompt !== null)

  return (
    <div ref={wrap} className={`agent-search${open ? ' is-open' : ''}`}>
      {/* The bar: a circle that grows leftward into a field. The icon sits at
          the field's left, so opening reads as the icon travelling left. */}
      <div className="agent-search__bar" role="search">
        {!open ? (
          <button
            type="button"
            className="agent-search__icon"
            aria-label="Ask LARA"
            title="Ask LARA"
            onClick={() => setOpen(true)}
          >
            <AppMark size={16} />
          </button>
        ) : (
          <>
            <span className="agent-search__icon is-static" aria-hidden="true">
              <AppMark size={16} />
            </span>
            <input
              ref={field}
              className="agent-search__field"
              value={input}
              placeholder="Ask LARA…"
              aria-label="Ask LARA"
              onChange={e => {
            setInput(e.target.value)
            if (e.target.value.trim()) prewarmModel()
          }}
              onKeyDown={e => {
                if (e.key === 'Enter') {
                  e.preventDefault()
                  void send(input)
                } else if (e.key === 'Escape') {
                  e.preventDefault()
                  setOpen(false)
                }
              }}
            />
            {streaming ? (
              <button type="button" className="agent-search__go" onClick={() => abort.current?.abort()} aria-label="Stop">
                <Square size={11} strokeWidth={0} fill="currentColor" />
              </button>
            ) : (
              <button
                type="button"
                className="agent-search__go"
                disabled={!input.trim()}
                aria-label="Send"
                onClick={() => void send(input)}
              >
                <ArrowUp size={15} strokeWidth={2} />
              </button>
            )}
            <button type="button" className="agent-search__close" onClick={() => setOpen(false)} aria-label="Close LARA search">
              <X size={15} strokeWidth={1.75} />
            </button>
          </>
        )}
      </div>

      {/* The answer space: one rectangle that grows downward per prompt.
          Prompts are rectangles, answers are squares. Kept mounted while
          open so a stream in flight is never unmounted by collapsing —
          closing only hides the bar; the thread state above survives. */}
      {showSpace && (
        <div className="agent-space" role="log" aria-label="LARA answers">
          <div className="agent-space__scroll" ref={scroller} onScroll={handleScroll}>
            {turns.map((turn, i) => {
              const live = streaming && i === turns.length - 1
              return turn.role === 'user' ? (
                <div key={i} className="agent-rect">
                  {turn.content}
                </div>
              ) : (
                <div key={i} className={`agent-square${turn.error ? ' is-error' : ''}`}>
                  {splitThinking(turn.content).answer || (live ? <ThinkingStatus /> : '')}
                  {turn.role === 'assistant' && turn.content && !turn.error && !live && (
                    <button
                      type="button"
                      className="agent-square__copy"
                      onClick={() => {
                        void navigator.clipboard?.writeText(turn.content)
                        setCopied(i)
                        window.setTimeout(() => setCopied(c => (c === i ? null : c)), 1600)
                      }}
                    >
                      {copied === i ? <Check size={12} strokeWidth={2} /> : <Copy size={12} strokeWidth={2} />}
                      {copied === i ? 'Copied' : 'Copy'}
                    </button>
                  )}
                </div>
              )
            })}
            {modelPrompt && (
              <ModelSetupPrompt readiness={modelPrompt} surface="agent" onOpenModels={onOpenModels} onReady={onModelReady} />
            )}
          </div>
        </div>
      )}
    </div>
  )
}
