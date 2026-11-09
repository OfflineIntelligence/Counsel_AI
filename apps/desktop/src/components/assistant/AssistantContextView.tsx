// The History view of the Aurora panel.
//
// WHY IT EXISTS
//
// "What did I ask about this before?" Every thread used to be reachable only
// by being on the page that owned it, and "New conversation" deleted the old
// one outright. History lists Aurora's own threads from the backend and
// switches between them.
//
// It reads /conversations live, so it cannot drift out of step with what is
// actually stored.
//
// This file also held a Context view (how much of the open document fits the
// model's window) and a Legal operations panel (standing instructions, stored
// in this browser and never run by anything). Both are gone: the panel is the
// conversation and the way back to an earlier one, and a form that promised a
// feature it did not have was costing a third of its header to say so.

import { useEffect, useState } from 'react'
import { Clock, MessageSquare, Trash2 } from 'lucide-react'

import { DRAFT_ASSISTANT_SESSION_PREFIX, fetchConversations, type ConversationSummary } from '../../api/chat'
import { EmptyState } from '../ui'

export function HistoryView({
    activeThread,
    onSelect,
    onDelete,
}: {
    activeThread: string
    onSelect: (thread: string) => void
    onDelete: (thread: string) => void
}) {
    const [threads, setThreads] = useState<ConversationSummary[] | null>(null)

    useEffect(() => {
        let alive = true
        void fetchConversations()
            .then(all => {
                if (!alive) return
                // Assistant threads carry the drafting-assistant prefix, which
                // is exactly what keeps them out of the main chat list. Here
                // that prefix is the filter rather than something to hide.
                setThreads(all.filter(c => c.id.startsWith(DRAFT_ASSISTANT_SESSION_PREFIX)))
            })
            .catch(() => alive && setThreads([]))
        return () => {
            alive = false
        }
    }, [activeThread])

    if (threads == null) return <div className="ai-view" />

    if (threads.length === 0) {
        return (
            <div className="ai-view">
                <EmptyState
                    compact
                    icon={<MessageSquare size={18} strokeWidth={1.75} />}
                    title="No past conversations"
                />
            </div>
        )
    }

    return (
        <div className="ai-view">
            <ul className="ai-threads">
                {threads.map(t => {
                    const isActive = t.id === activeThread
                    return (
                        <li key={t.id} className={`ai-thread${isActive ? ' is-active' : ''}`}>
                            <button
                                type="button"
                                className="ai-thread__open"
                                onClick={() => onSelect(t.id)}
                                aria-current={isActive || undefined}
                            >
                                <span className="ai-thread__title">{t.title || 'Untitled thread'}</span>
                                <span className="ai-thread__meta">
                                    <Clock size={11} strokeWidth={1.75} aria-hidden="true" />
                                    {t.message_count} {t.message_count === 1 ? 'message' : 'messages'}
                                </span>
                            </button>
                            {/* Deleting the thread you are in would leave the
                                conversation on screen with no store behind it,
                                so the active row keeps its history instead. */}
                            {!isActive && (
                                <button
                                    type="button"
                                    className="ui-iconbtn ui-iconbtn--sm ui-iconbtn--danger ai-thread__del"
                                    onClick={() => onDelete(t.id)}
                                    aria-label={`Delete ${t.title || 'thread'}`}
                                >
                                    <Trash2 size={13} strokeWidth={1.75} />
                                </button>
                            )}
                        </li>
                    )
                })}
            </ul>
        </div>
    )
}
