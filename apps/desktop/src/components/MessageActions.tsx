// The action bar under an assistant message.
//
// Before this, a finished answer was a dead end: the only way to do anything
// with it was to select the text by hand. Three actions cover what actually
// happens next to a legal answer, and nothing else is offered, because a row
// of eight icons under every message is noise on a long thread:
//
//   Copy        — take it somewhere else.
//   Retry       — the answer was wrong or truncated. Only on the LAST
//                 message: re-running an earlier turn would have to discard
//                 everything after it, and silently deleting a conversation's
//                 tail is not something a button should do without saying so.
//   Ask         — hand the answer to the assistant panel as its context, for
//                 "tighten this", "what am I missing", "turn this into an
//                 email". This is what makes the panel and the thread one
//                 product rather than two chat boxes in one window.
//
// The bar is visible on hover and focus, and permanently on the last message.
// Keeping it hidden at rest is what lets a long thread read as prose; keeping
// it always-on for the last message is what makes it discoverable at all.

import React, { useCallback, useState } from 'react';
import { Check, Copy, RotateCcw, Sparkles } from 'lucide-react';

interface MessageActionsProps {
    content: string;
    /** Retry is offered only for the final assistant turn — see above. */
    isLast: boolean;
    onRetry?: () => void;
    onAsk?: (content: string) => void;
}

export const MessageActions: React.FC<MessageActionsProps> = ({ content, isLast, onRetry, onAsk }) => {
    const [copied, setCopied] = useState(false);

    const copy = useCallback(() => {
        void navigator.clipboard
            ?.writeText(content)
            .then(() => {
                setCopied(true);
                // Long enough to read, short enough that the button is back to
                // its real label before the user reaches for it again.
                window.setTimeout(() => setCopied(false), 1600);
            })
            .catch(() => {
                /* Clipboard denied. The button simply does not confirm — a
                   toast for a failed copy is louder than the action itself. */
            });
    }, [content]);

    return (
        <div className={`msg-actions${isLast ? ' is-pinned' : ''}`}>
            <button type="button" className="msg-action" onClick={copy}>
                {copied ? <Check size={13} strokeWidth={2} /> : <Copy size={13} strokeWidth={1.75} />}
                {copied ? 'Copied' : 'Copy'}
            </button>

            {isLast && onRetry && (
                <button type="button" className="msg-action" onClick={onRetry}>
                    <RotateCcw size={13} strokeWidth={1.75} />
                    Retry
                </button>
            )}

            {onAsk && (
                <button type="button" className="msg-action" onClick={() => onAsk(content)}>
                    <Sparkles size={13} strokeWidth={1.75} />
                    Ask about this
                </button>
            )}
        </div>
    );
};

export default MessageActions;
