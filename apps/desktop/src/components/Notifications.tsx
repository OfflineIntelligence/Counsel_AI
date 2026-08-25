// Toast notifications — bottom-right stack.
//
// Rebuilt on the `.ui-toast` primitive. Three things changed beyond styling,
// each of which was a real defect rather than a matter of taste:
//
// 1. THEME. The container was a hardcoded `rgba(47,47,47,0.75)` with a 20px
//    backdrop-blur — dark in BOTH themes — while its text used --text-primary.
//    In light mode that put near-black text on a dark slab.
//
// 2. RENDER COST. The countdown bar was driven by a 50ms setInterval calling
//    setProgress, i.e. 20 React renders per second per visible toast. It is now
//    a CSS animation reading --toast-duration: zero renders, and it pauses on
//    hover, which is the only reason a visible countdown is worth having.
//
// 3. THE EXIT ANIMATION NEVER RAN. Dismissal was owned in two places:
//    NotificationContext set a `setTimeout(duration)` that removed the toast
//    outright, while this component ran its own timer that set an exiting flag
//    and removed it 300ms later. The context's timeout always won the race, so
//    the toast was unmounted before the transition it had just started could
//    play. Lifecycle now lives here alone — see NotificationContext.

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { useNotifications, type Notification } from '../contexts/NotificationContext';
import { X, CheckCircle, AlertCircle, AlertTriangle, Info, Download } from 'lucide-react';

/** How long the exit animation runs. Must match `ui-toast-out`'s --dur-2. */
const EXIT_MS = 180;

const TOAST_ICON = {
  success: CheckCircle,
  error: AlertCircle,
  warning: AlertTriangle,
  info: Info,
  download: Download,
} as const;

const Toast: React.FC<{
  notification: Notification;
  onDismiss: (id: string) => void;
}> = ({ notification, onDismiss }) => {
  const [isExiting, setIsExiting] = useState(false);
  const exitTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const dismissTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const { id, type, title, message, duration, timestamp } = notification;
  const autoDismiss = typeof duration === 'number' && duration > 0;

  const close = useCallback(() => {
    // Guard against a second call from the timer racing the click handler,
    // which would restart the exit animation midway through it.
    if (exitTimer.current) return;
    setIsExiting(true);
    exitTimer.current = setTimeout(() => onDismiss(id), EXIT_MS);
  }, [id, onDismiss]);

  // Auto-dismiss. A single timeout rather than a ticking interval — the visual
  // countdown is CSS, so nothing here needs to know the elapsed time.
  //
  // Deliberately NOT paused on hover in JS. The CSS bar pauses, which is the
  // part the user can see; keeping the JS timer simple avoids having to
  // reconcile two notions of "remaining time" that can disagree.
  useEffect(() => {
    if (!autoDismiss) return;
    dismissTimer.current = setTimeout(close, duration);
    return () => {
      if (dismissTimer.current) clearTimeout(dismissTimer.current);
    };
  }, [autoDismiss, duration, close]);

  useEffect(
    () => () => {
      if (exitTimer.current) clearTimeout(exitTimer.current);
    },
    [],
  );

  const Icon = TOAST_ICON[type] ?? Info;

  return (
    <div
      className={`ui-toast ui-toast--${type}${isExiting ? ' is-exiting' : ''}`}
      // An error is interruptive and should be announced immediately; the rest
      // are polite so they do not cut across whatever is being read.
      role={type === 'error' ? 'alert' : 'status'}
      aria-live={type === 'error' ? 'assertive' : 'polite'}
      style={autoDismiss ? ({ '--toast-duration': `${duration}ms` } as React.CSSProperties) : undefined}
    >
      <span className="ui-toast__icon" aria-hidden="true">
        <Icon size={15} strokeWidth={2} />
      </span>

      <div className="ui-toast__body">
        <div className="ui-toast__title">{title}</div>
        {/* title attribute carries the full text, since the CSS clamps to
            three lines rather than letting one toast grow without bound. */}
        {message && (
          <div className="ui-toast__message" title={message}>
            {message}
          </div>
        )}
        <div className="ui-toast__meta">
          {timestamp.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}
        </div>
      </div>

      {/* Dismiss is an explicit button rather than a click handler on the whole
          card. The card was previously clickable-to-dismiss with no affordance
          saying so, which meant selecting the message text dismissed it. */}
      <button
        type="button"
        className="ui-iconbtn ui-iconbtn--sm"
        onClick={close}
        aria-label="Dismiss notification"
      >
        <X size={13} strokeWidth={2} />
      </button>

      {autoDismiss && (
        <div className="ui-toast__timer" aria-hidden="true">
          <div className="ui-toast__timer-fill" />
        </div>
      )}
    </div>
  );
};

const NotificationsContainer: React.FC = () => {
  const { notifications, removeNotification } = useNotifications();

  if (notifications.length === 0) return null;

  return (
    <div className="ui-toast-region">
      {notifications.map((notification) => (
        <Toast
          key={notification.id}
          notification={notification}
          onDismiss={removeNotification}
        />
      ))}
    </div>
  );
};

export default NotificationsContainer;
