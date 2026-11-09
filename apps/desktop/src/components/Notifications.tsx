// The island — notifications AND Aurora, in one capsule.
//
// One black capsule that lives in the window band, centred over the page. It
// has three shapes and morphs between them:
//
//   idle          A small resting pill ("Aurora", with the app mark). Always
//                 present; clicking it opens Aurora.
//   notification  One row, 28px tall, the same size as before: status glyph
//                 on the left, close on the right, message between. Several
//                 notifications queue in the capsule, oldest first.
//   agent         Aurora. The pill widens into a search-like capsule (attach,
//                 question, send). Once something is asked, a separate answer
//                 bar eases in beneath it and grows with the reply, up to
//                 about a third of the window. The page dims behind, so
//                 neither sits on live content. Esc or a click on the dimmed
//                 page folds it back. (Styling: notification-island.css.)
//
// Notifications keep working while Aurora is open: they appear as their own
// small row above the capsule.
//
// Lifecycle rules carried over from the toast this replaced: dismissal is
// owned here (only the component knows when its exit has finished), and every
// countdown is CSS, so nothing re-renders per tick.

import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useNotifications, type Notification } from '../contexts/NotificationContext';
import { useAssistant } from '../contexts/AssistantContext';
import { X, Check, AlertTriangle, AlertCircle, Info, ArrowDown, HelpCircle } from 'lucide-react';
import AssistantPanel from './assistant/AssistantPanel';
import AppMark from './AppMark';
import './notification-island.css';

// One glyph family, one stroke, one size, so every status sits in the well
// at the same optical weight.
const GLYPH = {
  success: Check,
  error: AlertCircle,
  warning: AlertTriangle,
  info: Info,
  download: ArrowDown,
  feedback: HelpCircle,
} as const;

/** A notification with no auto-dismiss folds back to idle after this long. */
const PERSISTENT_EXPANDED_MS = 4000;
/** The content fades out for this long before the queue advances. */
const CONTENT_OUT_MS = 140;

const DOWNLOADS_ID = 'downloads';

/** Model downloads ride in the capsule as a persistent notification of their own. */
export interface DownloadNote {
  title: string;
  message: string;
  onView: () => void;
  onDismiss: () => void;
}

const NotificationsContainer: React.FC<{ onOpenModels?: () => void; downloadNote?: DownloadNote | null }> = ({ onOpenModels, downloadNote }) => {
  const { notifications, removeNotification } = useNotifications();
  const { open: assistantOpen, setOpen: setAssistantOpen, inlineHosted } = useAssistant();
  // A page that hosts Aurora inline (the drafts composer) owns it there.
  const agentOpen = assistantOpen && !inlineHosted;

  // Real notifications go first; the download progress shows when none is waiting.
  const live: Notification | undefined =
    notifications[0] ??
    (downloadNote
      ? {
          id: DOWNLOADS_ID,
          type: 'download',
          title: downloadNote.title,
          message: downloadNote.message,
          duration: 0,
          timestamp: new Date(0),
          actions: [{ label: 'View', onClick: downloadNote.onView }],
        }
      : undefined);
  const waiting = Math.max(0, notifications.length - 1);
  const liveTitle = live?.title;
  const liveMessage = live?.message;

  // What is drawn — held apart from `live` so the last notification can fade
  // out before the capsule morphs back to idle.
  const [shown, setShown] = useState<Notification | null>(live ?? null);
  const [expanded, setExpanded] = useState(false);
  const [hovered, setHovered] = useState(false);
  const [dismissing, setDismissing] = useState(false);

  // ── Follow the head of the queue ────────────────────────────────────────
  // Setting state here is the point: the island is a small state machine
  // driven by an external queue, and each step must render before the next
  // timer fires so the size has something to transition from.
  /* eslint-disable react-hooks/set-state-in-effect */
  useEffect(() => {
    if (live) {
      setDismissing(false);
      setShown(live);
      setExpanded(false);
      const t = window.setTimeout(() => setExpanded(true), 180);
      return () => window.clearTimeout(t);
    }
    if (shown) {
      setExpanded(false);
      const t = window.setTimeout(() => setShown(null), CONTENT_OUT_MS);
      return () => window.clearTimeout(t);
    }
    // `shown` is deliberately not a dependency: this reacts to the queue head.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live?.id]);
  /* eslint-enable react-hooks/set-state-in-effect */

  // Progress changes without changing the id, so refresh what is drawn.
  /* eslint-disable react-hooks/set-state-in-effect */
  useEffect(() => {
    if (live?.id === DOWNLOADS_ID) setShown(prev => (prev?.id === DOWNLOADS_ID ? live : prev));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [liveTitle, liveMessage]);
  /* eslint-enable react-hooks/set-state-in-effect */

  const dismiss = useCallback(() => {
    if (!live || dismissing) return;
    if (live.id === DOWNLOADS_ID) {
      downloadNote?.onDismiss();
      return;
    }
    setDismissing(true);
    window.setTimeout(() => removeNotification(live.id), CONTENT_OUT_MS);
  }, [live, dismissing, removeNotification, downloadNote]);

  // ── Auto-dismiss, paused while hovered ──────────────────────────────────
  const remaining = useRef(0);
  const startedAt = useRef(0);
  useEffect(() => {
    remaining.current = live?.duration && live.duration > 0 ? live.duration : 0;
  }, [live?.id, live?.duration]);
  useEffect(() => {
    if (!live || !remaining.current || hovered || dismissing) return;
    startedAt.current = performance.now();
    const t = window.setTimeout(dismiss, remaining.current);
    return () => {
      window.clearTimeout(t);
      remaining.current = Math.max(0, remaining.current - (performance.now() - startedAt.current));
    };
  }, [live, hovered, dismissing, dismiss]);

  // ── Persistent notifications fold to their title after a while ──────────
  useEffect(() => {
    if (!live || (live.duration ?? 0) > 0 || !expanded || hovered) return;
    const t = window.setTimeout(() => setExpanded(false), PERSISTENT_EXPANDED_MS);
    return () => window.clearTimeout(t);
  }, [live, expanded, hovered]);

  // ── Esc folds Aurora back from anywhere in the sheet ────────────────────
  useEffect(() => {
    if (!agentOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setAssistantOpen(false);
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [agentOpen, setAssistantOpen]);

  // ── Measure the row so the capsule can widen to it ──────────────────────
  const contentRef = useRef<HTMLDivElement>(null);
  const messageRef = useRef<HTMLSpanElement>(null);
  const [width, setWidth] = useState<number | null>(null);
  const [overflow, setOverflow] = useState(0);
  useLayoutEffect(() => {
    const el = contentRef.current;
    if (!el) return;
    const measure = () => {
      setWidth(el.offsetWidth);
      const m = messageRef.current;
      setOverflow(m ? Math.max(0, m.scrollWidth - m.clientWidth) : 0);
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [shown?.id, expanded, agentOpen]);

  const open = !!shown && expanded;
  const autoDismiss = !!shown?.duration && shown.duration > 0;
  const Glyph = shown ? GLYPH[shown.type] ?? Info : Info;
  const isError = shown?.type === 'error';
  const mode = agentOpen ? 'agent' : shown ? 'notification' : 'idle';

  // The notification row, used on its own and as the top line of the sheet.
  const row = shown && (
    <div
      ref={agentOpen ? undefined : contentRef}
      key={shown.id}
      className={`island__content${agentOpen ? ' island__content--inline' : ''}${dismissing ? ' is-dismissing' : ''}`}
      role={isError ? 'alert' : 'status'}
    >
      <span className="island__well island__glyph" aria-hidden="true">
        <Glyph size={13} strokeWidth={2.5} />
      </span>
      <span className="island__title">{shown.title}</span>
      {(open || agentOpen) && shown.message && (
        <span
          ref={messageRef}
          className="island__message"
          data-marquee={(hovered && overflow > 0) || undefined}
          style={overflow > 0 ? ({
            '--marquee': `${overflow + 16}px`,
            // ~40px a second: slow enough to read while it moves.
            '--marquee-dur': `${Math.max(3, (overflow + 16) / 40)}s`,
          } as React.CSSProperties) : undefined}
        >
          <span>{shown.message}</span>
        </span>
      )}
      {(open || agentOpen) && shown.actions && shown.actions.length > 0 && (
        <span className="island__actions">
          {shown.actions.map(action => (
            <button
              key={action.label}
              type="button"
              className={`island__action${action.primary ? ' island__action--primary' : ''}`}
              onClick={() => {
                action.onClick();
                dismiss();
              }}
            >
              {action.label}
            </button>
          ))}
        </span>
      )}
      {waiting > 0 && (
        <span className="island__count" aria-label={`${waiting} more`}>+{waiting}</span>
      )}
      <button type="button" className="island__well island__close" onClick={dismiss} aria-label="Dismiss">
        <X size={12} strokeWidth={2.5} />
      </button>
      {autoDismiss && open && !agentOpen && (
        <span
          className="island__timer"
          aria-hidden="true"
          style={{ '--island-duration': `${shown.duration}ms` } as React.CSSProperties}
        />
      )}
    </div>
  );

  return (
    <>
      {/* The dimmed page behind the open sheet. It is what keeps the sheet
          from reading as sitting ON the page, and clicking it folds Aurora. */}
      <div
        className={`island-scrim${agentOpen ? ' is-on' : ''}`}
        onClick={() => setAssistantOpen(false)}
        aria-hidden="true"
      />

      {/* Always mounted: content inserted into a live region that appeared in
          the same frame is not reliably announced. */}
      <div className="island-region" aria-live={isError ? 'assertive' : 'polite'}>
        <div
          className={`island${shown ? ` island--${shown.type}` : ''}`}
          data-mode={mode}
          data-open={open || undefined}
          style={width && mode !== 'agent' ? ({ '--island-w': `${width}px` } as React.CSSProperties) : undefined}
          onMouseEnter={() => { setHovered(true); if (shown) setExpanded(true); }}
          onMouseLeave={() => setHovered(false)}
        >
          {mode === 'agent' ? (
            // Scoped to the dark tokens: the sheet is black in both themes,
            // so everything Aurora draws inside it resolves to its dark-mode
            // colours whichever theme the app is in.
            <div className="island__agent">
              {row}
              <AssistantPanel embedded onOpenModels={onOpenModels} />
            </div>
          ) : mode === 'notification' ? (
            row
          ) : (
            <button
              ref={contentRef as unknown as React.Ref<HTMLButtonElement>}
              type="button"
              className="island__content island__idle"
              onClick={() => setAssistantOpen(true)}
              aria-label="Open LARA"
              title="Ask LARA"
            >
              <span className="island__idle-mark" aria-hidden="true"><AppMark size={12} /></span>
              <span className="island__idle-label">LARA</span>
            </button>
          )}
        </div>
      </div>
    </>
  );
};

export default NotificationsContainer;
