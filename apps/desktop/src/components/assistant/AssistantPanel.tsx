/**
 * The AI Assistant panel — one instance at the app root, on every page.
 *
 * It floats at the right edge over the page. The app shifts its own content
 * over by the panel's width while it is open (see `.app-root[data-assistant]`),
 * so buttons, search fields and headers are never hidden beneath it.
 *
 * What it reads comes from `AssistantContext`: the open document inside a
 * draft, a description of the page everywhere else.
 */

import { splitThinking } from '../MessageContent'
import { useCallback, useEffect, useRef, useState } from 'react'
import { ArrowRight, Check, Copy, CornerDownLeft, History, Mic, RotateCcw, Square, X } from 'lucide-react'

import { open as openNativeFileDialog } from '@tauri-apps/plugin-dialog'
import { readFile } from '@tauri-apps/plugin-fs'

import {
  DRAFT_ASSISTANT_SESSION_PREFIX,
  ModelNotReadyError,
  deleteConversation,
  streamChat,
  type ChatAttachment,
  type Message,
} from '../../api/chat'
import { processAttachments } from '../../api/attachments'
import { DIALOG_EXTENSIONS, partitionSupported } from '../../supportedFormats'
import { useAssistant, type AssistantPage } from '../../contexts/AssistantContext'
import AppMark from '../AppMark'
import { useIdleDismiss } from '../../utils/useIdleDismiss'
import ModelSetupPrompt from '../ModelSetupPrompt'
import { announceModelsChanged, canAsk, getReadiness, prewarmModel, type Readiness } from '../../api/models'
import ThinkingStatus from '../ThinkingStatus'
import { CONTEXT_CHARS, systemPrompt } from './agentPrompt'

export { CONTEXT_CHARS }

import { HistoryView } from './AssistantContextView'
import './assistant.css'
import { ANIM_MS } from '../../styles/motion'
import PaperclipIcon from '../PaperclipIcon'
import { FileGlyph, formatOf, FILE_ICON } from '../FileBrandIcon'
import { useNotificationHelpers } from '../../contexts/NotificationContext'

const DEFAULT_SUGGESTIONS = ['What can you help me with here?', 'Draft a short client email']

/** Files Aurora takes per question. The main chat takes 16; Aurora is a
 *  quick side conversation, so it keeps to six. */
const AURORA_MAX_FILES = 6

interface Turn {
  role: 'user' | 'assistant'
  content: string
  error?: boolean
}

/** A document picked for the next question. `documentId` only exists once the
 *  backend has read it, which is what 'ready' means. */
interface Staged {
  name: string
  status: 'processing' | 'ready' | 'failed'
  documentId?: number
}

/** The conversation for one thread; remounted when the thread changes. */
function Conversation({
  page,
  thread,
  onOpenModels,
}: {
  page: AssistantPage
  thread: string
  onOpenModels?: () => void
}) {
  const { pending, open } = useAssistant()
  const [turns, setTurns] = useState<Turn[]>([])
  const [input, setInput] = useState('')
  const [streaming, setStreaming] = useState(false)
  const [copied, setCopied] = useState<number | null>(null)
  const abort = useRef<AbortController | null>(null)
  const scroller = useRef<HTMLDivElement | null>(null)
  // Stick-to-bottom, as in the main chat: a manual scroll-up unsticks so a
  // streaming answer never drags the view back down; reaching the bottom
  // re-sticks. A ref so per-token renders never re-render to decide this.
  const stickToBottomRef = useRef(true)
  const handleScroll = () => {
    const el = scroller.current
    if (!el) return
    stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80
  }
  const area = useRef<HTMLTextAreaElement | null>(null)
  const pageRef = useRef(page)
  const turnsRef = useRef(turns)
  const attachedRef = useRef<Staged[]>([])
  useEffect(() => {
    pageRef.current = page
    turnsRef.current = turns
  }, [page, turns])

  // Every opening starts on an empty conversation: the user types the
  // question (or picks a suggestion). The thread's earlier history is no
  // longer replayed into the panel.
  const lastNonceRef = useRef<number | null>(null)
  useEffect(() => {
    if (!open) return
    // Opened by `ask` with a question waiting: keep it, it is about to send.
    if (pending && pending.nonce !== lastNonceRef.current) return
    abort.current?.abort()
    setTurns([])
    setInput('')
  }, [open]) // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => () => abort.current?.abort(), [])

  useEffect(() => {
    // Follow only while stuck; a send re-sticks (see send below).
    if (stickToBottomRef.current) scroller.current?.scrollTo({ top: scroller.current.scrollHeight })
  }, [turns])

  useEffect(() => {
    area.current?.focus()
  }, [])

  // Documents staged for the next question. Each is extracted by the backend
  // the moment it is picked, so by the time the user finishes typing the send
  // carries an id rather than the bytes. This mirrors the main composer's
  // pipeline exactly, through the same processAttachments call.
  const [attached, setAttached] = useState<Staged[]>([])
  useEffect(() => {
    attachedRef.current = attached
  }, [attached])
  const { showWarning } = useNotificationHelpers()
  // Browser fallback for the paperclip (the desktop app uses the native dialog).
  const browserInput = useRef<HTMLInputElement | null>(null)

  const stage = useCallback(async (files: { name: string; bytes: Uint8Array }[]) => {
    // Anything the backend cannot read is dropped here rather than being sent
    // and failing later: the extension list is the same one the main composer
    // and the upload gate use.
    const { accepted: supported } = partitionSupported(files)
    if (supported.length === 0) return
    // At most AURORA_MAX_FILES staged at once; the rest are left out, and
    // the user is told why.
    const room = Math.max(0, AURORA_MAX_FILES - attachedRef.current.length)
    const accepted = supported.slice(0, room)
    if (accepted.length < supported.length) {
      showWarning(
        `Up to ${AURORA_MAX_FILES} files in LARA`,
        room === 0
          ? 'Remove a file to attach another, or attach up to 16 in the main chat.'
          : `Only the first ${room} were attached. The main chat takes up to 16.`,
      )
    }
    if (accepted.length === 0) return

    const pending: Staged[] = accepted.map(f => ({ name: f.name, status: 'processing' }))
    setAttached(a => [...a, ...pending])

    try {
      const result = await processAttachments(accepted)
      const byName = new Map<string, number[]>()
      for (const d of result.documents) {
        const list = byName.get(d.filename) ?? []
        list.push(d.documentId)
        byName.set(d.filename, list)
      }
      setAttached(a =>
        a.map(item => {
          if (item.status !== 'processing') return item
          const queue = byName.get(item.name)
          const id = queue?.shift()
          return id === undefined
            ? { ...item, status: 'failed' as const }
            : { ...item, status: 'ready' as const, documentId: id }
        }),
      )
    } catch {
      setAttached(a => a.map(item => (item.status === 'processing' ? { ...item, status: 'failed' } : item)))
    }
  }, [showWarning])

  // Both pickers swallow their own failures. The dialog throws when it is
  // cancelled in some Tauri versions, and always throws outside Tauri (the dev
  // preview in a plain browser). Neither is a condition the user needs told
  // about: nothing was staged, which is exactly what cancelling means.
  const attachFiles = useCallback(async () => {
    if (!('__TAURI_INTERNALS__' in window)) {
      browserInput.current?.click()
      return
    }
    try {
      const picked = await openNativeFileDialog({
        multiple: true,
        filters: [{ name: 'Documents & Images', extensions: DIALOG_EXTENSIONS }],
      })
      if (!picked) return
      const paths = Array.isArray(picked) ? picked : [picked]
      const files = await Promise.all(
        paths.map(async path => ({
          name: path.split(/[\\/]/).pop() ?? path,
          bytes: await readFile(path),
        })),
      )
      await stage(files)
    } catch {
      /* Cancelled, or no native dialog available. */
    }
  }, [stage])


  // Set when a question cannot be answered because no model is ready.
  const [modelPrompt, setModelPrompt] = useState<Readiness | null>(null)
  const onModelReady = useCallback(() => {
    setModelPrompt(null)
    announceModelsChanged()
    area.current?.focus()
  }, [])

  const send = useCallback(
    async (question: string) => {
      const q = question.trim()
      if (!q || streaming) return
      // Ask first, so "no model" is a prompt to get one rather than a failed
      // request; the question stays in the box.
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
      // Only documents the backend actually read. One still extracting would
      // send an id that does not exist yet.
      const docs: ChatAttachment[] = attachedRef.current
        .filter(a => a.status === 'ready' && a.documentId !== undefined)
        .map(a => ({ name: a.name, document_id: a.documentId }))
      const history: Message[] = turnsRef.current.filter(t => !t.error).map(t => ({ role: t.role, content: t.content }))
      const messages: Message[] = [{ role: 'system', content: systemPrompt(pageRef.current) }, ...history, { role: 'user', content: q }]

      setInput('')
      setAttached([])
      setTurns(t => [...t, { role: 'user', content: q }, { role: 'assistant', content: '' }])
      // The user's own question re-sticks: the answer streams beneath them
      // unless they scroll up to read earlier turns.
      stickToBottomRef.current = true
      setStreaming(true)
      const controller = new AbortController()
      abort.current = controller
      try {
        for await (const chunk of streamChat(messages, thread, undefined, docs.length ? docs : undefined, controller.signal)) {
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

  // A question asked from anywhere else (the page composer) arrives here.
  const lastNonce = lastNonceRef
  useEffect(() => {
    if (!pending || pending.nonce === lastNonce.current) return
    lastNonce.current = pending.nonce
    void send(pending.text)
  }, [pending, send])

  const reset = useCallback(async () => {
    abort.current?.abort()
    setTurns([])
    try {
      await deleteConversation(thread)
    } catch {
      /* A thread that was never persisted has nothing to delete. */
    }
  }, [thread])

  return (
    <div className="dr-assistant">
      <div className="dr-assistant-scroll" ref={scroller} onScroll={handleScroll}>
        {turns.length === 0 ? (
          <div className="dr-assistant-intro">
            {/* Empty state: black triangle on white. Unmounted as soon as
                the first turn streams in, so only messages remain. */}
            <div className="dr-assistant-empty-mark" aria-hidden="true">
              <AppMark size={64} className="dr-assistant-empty-triangle" />
            </div>
            {/* The lead line and nothing else. The canned openings that used
                to sit here were three more things to read before typing, on a
                panel whose whole job is the box at the bottom. */}
            <p className="dr-assistant-lead">
              {page.text ? `Ask about ${page.title ? `“${page.title}”` : 'what is open'}, or have something drafted for you.` : 'How can I help with your legal work?'}
            </p>
            {/* Two starters for the page Aurora was opened on. */}
            <div className="dr-assistant-suggestions">
              {(page.suggestions?.length ? page.suggestions : DEFAULT_SUGGESTIONS).slice(0, 2).map(q => (
                <button key={q} type="button" className="dr-assistant-suggestion" onClick={() => void send(q)}>
                  {q}
                </button>
              ))}
            </div>
          </div>
        ) : (
          turns.map((turn, i) => {
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
          })
        )}
        {modelPrompt && (
          <ModelSetupPrompt readiness={modelPrompt} surface="agent" onOpenModels={onOpenModels} onReady={onModelReady} />
        )}
      </div>

      {turns.length > 0 && !streaming && (
        <button type="button" className="dr-mini dr-new-thread" onClick={() => void reset()}>
          <RotateCcw size={12} strokeWidth={2} />
          New conversation
        </button>
      )}
      <form
        className="dr-assistant-input"
        onSubmit={e => {
          e.preventDefault()
          void send(input)
        }}
      >
        {/* Staged documents, above the box rather than inside it: a row of
            chips inside a two-line textarea leaves no room to type. */}
        {attached.length > 0 && (
          <div className="dr-attached">
            {attached.map((a, i) => (
              <span key={`${a.name}-${i}`} className={`dr-chip dr-chip--${a.status}`}>
                <FileGlyph format={formatOf(a.name)} size={FILE_ICON.list} />
                <span className="dr-chip__name">{a.name}</span>
                <button
                  type="button"
                  className="dr-chip__x"
                  aria-label={`Remove ${a.name}`}
                  onClick={() => setAttached(list => list.filter((_, j) => j !== i))}
                >
                  <X size={10} strokeWidth={2.25} />
                </button>
              </span>
            ))}
          </div>
        )}

        {/* In the island the app mark takes the paperclip's place (the attach
            button is hidden there by CSS). */}
        <span className="dr-island-mark" aria-hidden="true">
          <AppMark size={16} />
        </span>

        {/* Attach. Top-left of the box, opposite Send, because it acts on the
            message being composed rather than on the conversation. */}
        <div className="dr-attach">
          <button
            type="button"
            className="dr-attach__btn"
            aria-label="Attach files"
            title={`Attach files (up to ${AURORA_MAX_FILES})`}
            onClick={() => void attachFiles()}
          >
            <PaperclipIcon size={15} />
          </button>
          <input
            ref={browserInput}
            type="file"
            multiple
            accept={DIALOG_EXTENSIONS.map(e => `.${e}`).join(',')}
            style={{ display: 'none' }}
            onChange={e => {
              const list = Array.from(e.target.files ?? [])
              e.target.value = ''
              void Promise.all(list.map(async f => ({ name: f.name, bytes: new Uint8Array(await f.arrayBuffer()) }))).then(stage)
            }}
          />
        </div>

        <textarea
          ref={area}
          rows={2}
          value={input}
          placeholder={page.text ? `Ask about ${page.title ?? 'this'}…` : 'Ask anything…'}
          onChange={e => {
            setInput(e.target.value)
            if (e.target.value.trim()) prewarmModel()
          }}
          onKeyDown={e => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              void send(input)
            }
          }}
        />
        {/* Voice input. The button only for now: speech capture is not wired yet. */}
        <button
          type="button"
          className="dr-mic"
          aria-label="Ask by voice"
          title="Ask by voice"
        >
          <Mic size={15} strokeWidth={1.9} />
        </button>
        {streaming ? (
          <button type="button" className="dr-send" onClick={() => abort.current?.abort()} aria-label="Stop">
            <Square size={11} strokeWidth={0} fill="currentColor" />
          </button>
        ) : (
          <button type="submit" className="dr-send" disabled={!input.trim()} aria-label="Send">
            <ArrowRight size={15} strokeWidth={2} />
          </button>
        )}
      </form>
    </div>
  )
}

/** How long the panel takes to slide in or out. Mirrors --dur-anim in
 *  styles/tokens.css, the app's single animation cadence. */
const PANEL_SLIDE_MS = ANIM_MS + 220

type View = 'chat' | 'history'

/**
 * `embedded`: rendered INSIDE the notification island (components/
 * Notifications.tsx), which owns the shape, the expand/collapse motion and
 * the backdrop. The panel then skips its own fixed positioning and slide, and
 * is simply present while Aurora is open.
 */
export default function AssistantPanel({ onOpenModels, hidden = false, placement = 'side', embedded = false }: { onOpenModels?: () => void; hidden?: boolean; placement?: 'side' | 'bottom'; embedded?: boolean } = {}) {
  const { open: panelOpen, setOpen, page, inlineHosted } = useAssistant()
  // A page hosting Aurora (the drafts composer) owns it; the panel does
  // not appear there at all.
  const open = panelOpen && !inlineHosted
  const pageThread = page.thread ?? `${DRAFT_ASSISTANT_SESSION_PREFIX}global`

  const [view, setView] = useState<View>('chat')

  // A thread opened from History, which overrides the one the current page
  // owns. Cleared whenever the page's own thread changes: navigating to a
  // different document and still being shown last week's thread about another
  // one is the kind of stale context that produces confidently wrong answers.
  const [picked, setPicked] = useState<string | null>(null)
  const [prevPageThread, setPrevPageThread] = useState(pageThread)
  if (prevPageThread !== pageThread) {
    setPrevPageThread(pageThread)
    setPicked(null)
    setView('chat')
  }
  const thread = picked ?? pageThread

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && (e.target as HTMLElement).closest('.ai-panel')) setOpen(false)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [open, setOpen])

  // The panel stays mounted while it slides away, so the exit can play; it
  // is removed once the slide has finished. Must cover --dur-anim in
  // assistant.css: unmounting earlier would cut the animation short.
  const [mounted, setMounted] = useState(open)
  // Applied one frame AFTER mounting: a element inserted already in its final
  // position has nothing to transition from, so the entrance would not play.
  const [slidIn, setSlidIn] = useState(false)
  useEffect(() => {
    if (open) {
      prewarmModel()
      setMounted(true)
      const frame = requestAnimationFrame(() => setSlidIn(true))
      return () => cancelAnimationFrame(frame)
    }
    setSlidIn(false)
    const timer = window.setTimeout(() => setMounted(false), PANEL_SLIDE_MS)
    return () => window.clearTimeout(timer)
  }, [open])

  // A minute without use and Aurora dissolves away. Typing in the composer or
  // an answer streaming in (DOM changes) counts as use.
  const panelRef = useRef<HTMLElement | null>(null)
  useIdleDismiss(panelRef, open && !hidden && (embedded || mounted), false, () => setOpen(false))

  // In chat view the agent lives in the top-right search bar (AgentSearch),
  // so the panel stays out of the way there — same thread, one surface.
  if (hidden) return null

  if (embedded ? !open : !mounted) return null

  return (
    <aside
      ref={panelRef}
      className={embedded ? 'ai-panel ai-panel--island is-open' : `ai-panel${slidIn ? ' is-open' : ''}${placement === 'bottom' ? ' is-bottom' : ''}`}
      aria-label="LARA"
      aria-hidden={embedded ? false : !slidIn}
    >
      {/* Opening animation: a few thin threads of #4E4260 that ripple like a
          wave and drift left to right, then fade. Clipped to the panel; it
          re-runs each time the panel mounts. */}
      <span className="ai-panel-flow" aria-hidden="true">
        <svg className="ai-panel-waves" viewBox="0 0 1200 200" preserveAspectRatio="none">
          {[0, 1, 2, 3, 4].map(i => (
            <path
              key={i}
              className={`ai-wave ai-wave--${i}`}
              d={`M0 ${100 + (i - 2) * 9} C 150 ${40 + i * 12}, 300 ${160 - i * 12}, 450 ${100 + (i - 2) * 9} S 750 ${40 + i * 12}, 900 ${100 + (i - 2) * 9} S 1200 ${160 - i * 12}, 1350 ${100 + (i - 2) * 9}`}
            />
          ))}
        </svg>
      </span>
      <header className="ai-panel-head">
        {/* The app mark, the same component the title bar and the empty-chat
            heading render. The panel used to draw the SOLID triangle here,
            which made it the one place in the app showing a different logo. */}
        <span className="ai-panel-mark">
          <AppMark size={14} />
        </span>
        <div className="ai-panel-titles">
          <h2>LARA</h2>
        </div>

        {/* One side view, as an icon rather than a segmented control.

            There is no "Chat" tab: the composer is sitting right there at the
            bottom of the panel, so a tab labelled Chat named the thing the user
            was already looking at. And there is no "Legal operations" tab any
            more either — it collected standing instructions that nothing has
            ever run, so it was a form that promised a feature and then did
            nothing with it, taking a third of the panel's header to do so.

            What is left is the conversation and the way back to an earlier one.
            History toggles ON to its list and OFF to the conversation, which is
            the resting state. */}
        <button
          type="button"
          className={`ai-panel-tool${view === 'history' ? ' is-active' : ''}`}
          aria-label="History"
          aria-pressed={view === 'history'}
          title="History"
          onClick={() => setView(view === 'history' ? 'chat' : 'history')}
        >
          <History size={15} strokeWidth={1.75} />
        </button>

        <button type="button" className="ws-close" onClick={() => setOpen(false)} aria-label="Close LARA" title="Close (Esc)">
          <X size={15} strokeWidth={1.75} />
        </button>
      </header>

      {/* The conversation is kept MOUNTED behind the other views rather than
          swapped out. It holds a stream in flight and the turns so far, none
          of which is persisted until the turn completes — unmounting it to
          glance at Context would abort the answer being written. */}
      <div className="ai-panel-body" hidden={view !== 'chat'}>
        <Conversation key={thread} page={page} thread={thread} onOpenModels={onOpenModels} />
      </div>
      {view === 'history' && (
        <HistoryView
          activeThread={thread}
          onSelect={t => {
            setPicked(t)
            setView('chat')
          }}
          onDelete={t => {
            void deleteConversation(t).catch(() => {
              /* Already gone, or never persisted. The list refreshes either way. */
            })
            if (picked === t) setPicked(null)
          }}
        />
      )}
    </aside>
  )
}
