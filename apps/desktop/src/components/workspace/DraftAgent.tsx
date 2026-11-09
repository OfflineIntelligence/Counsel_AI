/**
 * The Aurora, inside the drafts page.
 *
 * In a draft the composer at the foot of the page IS Aurora: the side
 * panel never opens here. The bar is always there, so Aurora is always one
 * keystroke away, and it answers anything — about the open document or not,
 * because the same model and the same workspace knowledge sit behind it.
 *
 * Answers rise ABOVE the bar, floating over the page: the document never
 * resizes or shifts to make room, which is the whole reason the panel is not
 * used here.
 *
 * The conversation is the draft's own Aurora thread, the same one the panel
 * used to keep (`draft-assistant-<id>`), so history survives reopening the
 * document and the backend still treats it as Aurora — its own share of
 * the model's context window, plus the workspace brief.
 */
import { splitThinking } from '../MessageContent'
import { useCallback, useEffect, useRef, useState } from 'react'
import { ArrowUp, Check, Copy, CornerDownLeft, Square, Trash2, X } from 'lucide-react'

import { ModelNotReadyError, deleteConversation, fetchConversation, streamChat, type Message } from '../../api/chat'
import { announceModelsChanged, canAsk, getReadiness, prewarmModel, type Readiness } from '../../api/models'
import { systemPrompt } from '../assistant/agentPrompt'
import ThinkingStatus from '../ThinkingStatus'
import AssistantMark from '../assistant/AssistantMark'
import ModelCapsule from '../ModelCapsule'
import ModelSetupPrompt from '../ModelSetupPrompt'
import { useAssistant, type AssistantPage } from '../../contexts/AssistantContext'
import { useModelSelection } from '../../contexts/ModelSelectionContext'
import { useIdleDismiss } from '../../utils/useIdleDismiss'

interface Turn {
  role: 'user' | 'assistant'
  content: string
  error?: boolean
}

/** Focus the field once the reveal has mostly settled. */
const REVEAL_FOCUS_MS = 320

export default function DraftAgent({
  thread,
  page,
  placeholder,
}: {
  thread: string
  page: AssistantPage
  placeholder: string
}) {
  const { registerInlineHost } = useAssistant()
  // "View all models" goes to the Models page, through the same shared
  // selection the capsule in the bar reads.
  const openModels = useModelSelection()?.openModels
  const [turns, setTurns] = useState<Turn[]>([])
  const [input, setInput] = useState('')
  const [streaming, setStreaming] = useState(false)
  const [modelPrompt, setModelPrompt] = useState<Readiness | null>(null)
  // The bar rests hidden; the rail's Aurora button reveals it, and a minute
  // without use fades it away again.
  const [visible, setVisible] = useState(false)
  const root = useRef<HTMLDivElement | null>(null)
  const [copied, setCopied] = useState<number | null>(null)
  const [open, setOpen] = useState(false)

  const field = useRef<HTMLInputElement | null>(null)
  const scroller = useRef<HTMLDivElement | null>(null)
  // Stick-to-bottom, as in the main chat: a manual scroll-up unsticks so a
  // streaming answer never drags the view back down; reaching the bottom
  // re-sticks.
  const stickToBottomRef = useRef(true)
  const handleScroll = () => {
    const el = scroller.current
    if (!el) return
    stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80
  }
  const abort = useRef<AbortController | null>(null)
  const pageRef = useRef(page)
  const turnsRef = useRef(turns)
  useEffect(() => {
    pageRef.current = page
    turnsRef.current = turns
  }, [page, turns])

  // This draft's history. allowMissing: a thread is saved on its first
  // message, so opening one before that is empty, not an error.
  //
  // Nothing is reset here: the parent keys this component by thread, so
  // switching documents remounts it with empty state rather than clearing it
  // in an effect (which would render once with the previous draft's answers).
  useEffect(() => {
    let live = true
    fetchConversation(thread, { allowMissing: true })
      .then(conv => {
        if (!live || !conv) return
        const restored = conv.messages
          .filter((m): m is Message & { role: 'user' | 'assistant' } => m.role !== 'system')
          .map(m => ({ role: m.role, content: m.content }))
        if (restored.length > 0) setTurns(restored)
      })
      .catch(() => {
        /* No history yet, or the backend is down; start fresh. */
      })
    return () => {
      live = false
    }
  }, [thread])

  useEffect(() => {
    if (stickToBottomRef.current) scroller.current?.scrollTo({ top: scroller.current.scrollHeight })
  }, [turns, open])

  useEffect(() => () => abort.current?.abort(), [])

  const send = useCallback(
    async (question: string) => {
      const q = question.trim()
      if (!q || streaming) return

      // Ask before sending, so "no model" is a prompt to download one rather
      // than a failed request; the question stays in the box.
      try {
        const readiness = await getReadiness('agent')
        if (!canAsk(readiness)) {
          setInput(q)
          setModelPrompt(readiness)
          setOpen(true)
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
      setOpen(true)
      setTurns(t => [...t, { role: 'user', content: q }, { role: 'assistant', content: '' }])
      // The user's own question re-sticks: the answer streams beneath them
      // unless they scroll up to read earlier turns.
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
          // The model went away between the check and the send; nothing was
          // stored, so the turn is withdrawn and the question restored.
          setTurns(t => t.slice(0, -2))
          setInput(q)
          setModelPrompt(e.readiness)
        } else if (!controller.signal.aborted) {
          setTurns(t => {
            const next = t.slice()
            next[next.length - 1] = {
              role: 'assistant',
              content: e instanceof Error ? e.message : String(e),
              error: true,
            }
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

  // The Aurora button in the rail, and anything else that would have opened the
  // panel, lands here: the bar glows and takes focus. With text, it asks.
  useEffect(() => {
    return registerInlineHost((text?: string) => {
      setVisible(true)
      prewarmModel()
      window.setTimeout(() => field.current?.focus(), REVEAL_FOCUS_MS)
      if (text) void send(text)
    })
  }, [registerInlineHost, send])

  useIdleDismiss(root, visible, streaming || input.trim().length > 0, () => {
    setVisible(false)
    setOpen(false)
    field.current?.blur()
  })

  const clear = async () => {
    abort.current?.abort()
    setTurns([])
    setOpen(false)
    try {
      await deleteConversation(thread)
    } catch {
      /* A thread that was never persisted has nothing to delete. */
    }
  }

  const showThread = open && (turns.length > 0 || modelPrompt !== null)

  return (
    <div
      ref={root}
      className={`dr-agent${visible ? ' is-visible' : ''}`}
      aria-hidden={!visible}
    >
      {showThread && (
        <section className="dr-agent__thread" aria-label="LARA">
          <header className="dr-agent__head">
            <span className="dr-agent__title">LARA</span>
            {turns.length > 0 && !streaming && (
              <button type="button" className="dr-mini" onClick={() => void clear()}>
                <Trash2 size={12} strokeWidth={2} />
                Clear
              </button>
            )}
            <button
              type="button"
              className="ui-iconbtn ui-iconbtn--sm"
              onClick={() => setOpen(false)}
              aria-label="Hide answers"
              title="Hide"
            >
              <X size={14} strokeWidth={2} />
            </button>
          </header>

          <div className="dr-agent__scroll" ref={scroller} onScroll={handleScroll}>
            {turns.map((turn, i) => {
              const live = streaming && i === turns.length - 1
              return (
                <div key={i} className={`dr-turn dr-turn-${turn.role}${turn.error ? ' dr-turn-error' : ''}`}>
                  <div className="dr-turn-body">{splitThinking(turn.content).answer || (live ? <ThinkingStatus /> : '')}</div>
                  {turn.role === 'assistant' && turn.content && !turn.error && !live && (
                    <div className="dr-turn-actions">
                      {page.onInsertText && (
                        <button type="button" className="dr-mini" onClick={() => page.onInsertText?.(turn.content.trim())}>
                          <CornerDownLeft size={12} strokeWidth={2} />
                          Insert
                        </button>
                      )}
                      <button
                        type="button"
                        className="dr-mini"
                        onClick={() => {
                          void navigator.clipboard?.writeText(turn.content)
                          setCopied(i)
                          window.setTimeout(() => setCopied(c => (c === i ? null : c)), 1600)
                        }}
                      >
                        {copied === i ? <Check size={12} strokeWidth={2} /> : <Copy size={12} strokeWidth={2} />}
                        {copied === i ? 'Copied' : 'Copy'}
                      </button>
                    </div>
                  )}
                </div>
              )
            })}

            {modelPrompt && (
              <ModelSetupPrompt
                readiness={modelPrompt}
                surface="agent"
                onOpenModels={openModels}
                onReady={() => {
                  setModelPrompt(null)
                  announceModelsChanged()
                  field.current?.focus()
                }}
              />
            )}
          </div>
        </section>
      )}

      <form
        className="dr-composer"
        onSubmit={e => {
          e.preventDefault()
          void send(input)
        }}
      >
        <span className="dr-composer-mark">
          <AssistantMark size={13} />
        </span>
        <input
          ref={field}
          value={input}
          onChange={e => {
            setInput(e.target.value)
            if (e.target.value.trim()) prewarmModel()
          }}
          onFocus={() => {
            if (turns.length > 0) setOpen(true)
          }}
          onKeyDown={e => {
            if (e.key === 'Enter' && !e.nativeEvent.isComposing) {
              e.preventDefault()
              void send(input)
            }
          }}
          placeholder={placeholder}
          aria-label="Ask LARA"
        />
        <ModelCapsule className="dr-composer-model" />
        {streaming ? (
          <button
            type="button"
            className="dr-send dr-send--stop"
            onClick={() => abort.current?.abort()}
            aria-label="Stop"
            title="Stop"
          >
            <Square size={12} strokeWidth={2} />
          </button>
        ) : (
          <button type="submit" className="dr-send" disabled={!input.trim()} aria-label="Ask">
            <ArrowUp size={15} strokeWidth={2} />
          </button>
        )}
      </form>
    </div>
  )
}
