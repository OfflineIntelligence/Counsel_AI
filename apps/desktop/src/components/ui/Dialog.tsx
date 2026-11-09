// Dialog — the app's one modal shape.
//
// Two weights, both from `.ui-dialog` in styles/surfaces.css:
//
//   <Dialog>          title, body, actions. Confirmations, forms, pickers.
//   <AnnounceDialog>  a hero masthead over a feature list. The "what's new"
//                     shape, built on Dialog rather than beside it.
//
// Three things this owns that a bare <div> would get wrong, and which are the
// reason every modal in the app should route through it:
//
// 1. THE EXIT ACTUALLY PLAYS. Closing sets `is-leaving`, waits the animation
//    out, and only then calls onClose. The existing toast stack had exactly
//    this bug before it was fixed — the owner unmounted the node before the
//    transition it had started could run, so the exit was dead code that
//    looked fine in review. Anything that animates out must own its own
//    unmount timing.
//
// 2. FOCUS. Focus moves into the dialog on open and returns to whatever was
//    focused before on close. Without the restore, dismissing a dialog drops
//    the caret at the top of the document and a keyboard user starts over.
//
// 3. ESCAPE AND SCRIM. Both route through the same guarded close, so a
//    dialog mid-exit cannot be closed twice, and `dismissible={false}` shuts
//    off both at once rather than at two call sites that can disagree.
//
// Not included, deliberately: a focus TRAP. A real trap needs to handle
// portals, iframes and shadow roots to be correct, and a half-built one is
// worse than none because it silently strands the caret. Tab can leave the
// dialog; the scrim still blocks the pointer.

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { X } from 'lucide-react';
import { ANIM_MS } from '../../styles/motion';


export interface DialogProps {
    open: boolean;
    onClose: () => void;
    /** Rendered above the title, in small caps — "NEW", "STEP 2 OF 4". */
    eyebrow?: React.ReactNode;
    title?: React.ReactNode;
    subtitle?: React.ReactNode;
    /** The hero masthead. Pass <DialogHero> for the announcement shape. */
    hero?: React.ReactNode;
    /** Footer content. Actions inside it belong in <DialogActions>. */
    footer?: React.ReactNode;
    size?: 'sm' | 'md' | 'lg';
    /** False removes the close button, Escape and scrim-click. For a dialog
     *  the user must answer rather than dismiss. */
    dismissible?: boolean;
    children?: React.ReactNode;
    /** Extra classes on the dialog element, for a one-off layout need. */
    className?: string;
    labelledBy?: string;
}

export const Dialog: React.FC<DialogProps> = ({
    open,
    onClose,
    eyebrow,
    title,
    subtitle,
    hero,
    footer,
    size = 'md',
    dismissible = true,
    children,
    className = '',
    labelledBy,
}) => {
    // The exit animation, as a single derived state machine.
    //
    // `open` is the parent's intent; `closing` is this component finishing an
    // exit the parent has already stopped rendering for. The node stays in
    // the tree while `closing` is true — an element must still be rendered in
    // order to animate away — and that is the whole reason this state exists.
    //
    // The open→closed transition is detected DURING RENDER against a ref, not
    // in an effect. Both work, but a setState in an effect body schedules a
    // second render pass after the browser has already been handed the first,
    // so the exit starts a frame late and, on a slow frame, visibly snaps.
    // A render-phase update is re-run before anything is committed.
    // The previous `open` is held in STATE rather than a ref. A ref would be
    // the obvious tool, but mutating one during render is genuinely unsafe
    // under concurrent rendering — a render can be thrown away and re-run,
    // and the ref would keep the write from the discarded attempt. State
    // adjusted during render is React's supported form of exactly this.
    const [closing, setClosing] = useState(false);
    const [prevOpen, setPrevOpen] = useState(open);
    if (prevOpen !== open) {
        setPrevOpen(open);
        // Closing: start the exit. Opening: cancel any exit still in flight,
        // so re-opening mid-animation does not leave `is-leaving` stuck on.
        setClosing(!open);
    }

    const panelRef = useRef<HTMLDivElement>(null);
    const restoreFocusTo = useRef<HTMLElement | null>(null);

    // Every route out of the dialog — the close button, Escape, the scrim —
    // goes through the parent, which flips `open`. That keeps one source of
    // truth: the dialog never hides itself while the parent still believes it
    // is open, which is the usual way these two drift apart.
    const beginClose = useCallback(() => {
        if (!open) return; // already on the way out
        onClose();
    }, [open, onClose]);

    // Retire the node once the exit has played out. setState here is in a
    // timeout callback, not the effect body — the effect subscribes to a
    // timer and reacts to it, which is what effects are for.
    useEffect(() => {
        if (!closing) return;
        const t = window.setTimeout(() => setClosing(false), ANIM_MS);
        return () => window.clearTimeout(t);
    }, [closing]);

    // Remember where focus was before the dialog took it.
    useEffect(() => {
        if (open) restoreFocusTo.current = document.activeElement as HTMLElement | null;
    }, [open]);

    const mounted = open || closing;
    const leaving = closing;

    // Move focus into the panel once it exists, and restore it on unmount.
    useEffect(() => {
        if (!open) return;
        const node = panelRef.current;
        if (!node) return;
        // Prefer the first genuinely interactive element; fall back to the
        // panel itself, which carries tabIndex={-1} for exactly this.
        const target = node.querySelector<HTMLElement>(
            'button:not([disabled]), [href], input:not([disabled]), select, textarea, [tabindex]:not([tabindex="-1"])',
        );
        (target ?? node).focus({ preventScroll: true });
        return () => {
            restoreFocusTo.current?.focus?.({ preventScroll: true });
        };
    }, [open]);

    // Escape closes. Bound on the document rather than the panel so it works
    // before focus has landed inside.
    useEffect(() => {
        if (!open || !dismissible) return;
        const onKey = (e: KeyboardEvent) => {
            if (e.key === 'Escape') {
                e.stopPropagation();
                beginClose();
            }
        };
        document.addEventListener('keydown', onKey, true);
        return () => document.removeEventListener('keydown', onKey, true);
    }, [open, dismissible, beginClose]);

    if (!mounted) return null;

    const leavingCls = leaving ? ' is-leaving' : '';
    const sizeCls = size === 'md' ? '' : ` ui-dialog--${size}`;
    const heroCls = hero ? ' ui-dialog--hero' : '';

    return (
        <div
            className={`ui-dialog-scrim${leavingCls}`}
            // Only a click that both starts and ends on the scrim dismisses.
            // A drag that began inside the dialog (selecting text, dragging a
            // slider) ends up here as a click on the scrim and would
            // otherwise close the dialog out from under the user.
            onMouseDown={(e) => {
                if (dismissible && e.target === e.currentTarget) beginClose();
            }}
        >
            <div
                ref={panelRef}
                role="dialog"
                aria-modal="true"
                aria-labelledby={labelledBy}
                tabIndex={-1}
                className={`ui-dialog${sizeCls}${heroCls}${leavingCls} ${className}`.trim()}
            >
                {dismissible && (
                    <button type="button" className="ui-dialog__close" onClick={beginClose} aria-label="Close">
                        <X size={15} strokeWidth={1.75} />
                    </button>
                )}

                {hero}

                {(eyebrow || title || subtitle) && (
                    <div className="ui-dialog__head">
                        {eyebrow && <span className="ui-dialog__eyebrow">{eyebrow}</span>}
                        {title && (
                            <h2 className="ui-dialog__title" id={labelledBy}>
                                {title}
                            </h2>
                        )}
                        {subtitle && <p className="ui-dialog__subtitle">{subtitle}</p>}
                    </div>
                )}

                {children != null && <div className="ui-dialog__body">{children}</div>}

                {footer && <div className="ui-dialog__foot">{footer}</div>}
            </div>
        </div>
    );
};

/** The right-aligned action cluster in a footer. Anything placed in the
 *  footer BEFORE this stays left (a page counter, a "don't show again"). */
export const DialogActions: React.FC<{ children: React.ReactNode }> = ({ children }) => (
    <div className="ui-dialog__foot-actions">{children}</div>
);

export interface DialogHeroProps {
    /** The centred mark. Defaults to nothing so a caller can pass the app's
     *  own glyph rather than this file importing a brand asset. */
    mark?: React.ReactNode;
    /** The two floating caption bubbles, as in the reference: a sample
     *  exchange SHOWN rather than described. Both are optional — one alone
     *  reads fine, and passing neither leaves a clean plate. */
    lead?: React.ReactNode;
    reply?: React.ReactNode;
}

export const DialogHero: React.FC<DialogHeroProps> = ({ mark, lead, reply }) => (
    <div className="ui-dialog__hero">
        {lead && <div className="ui-dialog__hero-bubble ui-dialog__hero-bubble--lead">{lead}</div>}
        {mark && <div className="ui-dialog__hero-mark">{mark}</div>}
        {reply && <div className="ui-dialog__hero-bubble ui-dialog__hero-bubble--reply">{reply}</div>}
    </div>
);

export interface Feature {
    icon: React.ReactNode;
    title: string;
    description: string;
}

/** The body of an announcement: icon, title, one sentence. Three or four
 *  entries. Past that it stops being an announcement and becomes docs. */
export const FeatureList: React.FC<{ features: Feature[] }> = ({ features }) => (
    <div className="ui-featurelist">
        {features.map((f) => (
            <div className="ui-feature" key={f.title}>
                <span className="ui-feature__icon" aria-hidden="true">
                    {f.icon}
                </span>
                <div>
                    <div className="ui-feature__title">{f.title}</div>
                    <p className="ui-feature__desc">{f.description}</p>
                </div>
            </div>
        ))}
    </div>
);

/** Page dots for a multi-step dialog. Presentational — the caller owns the
 *  index, because it also owns what Next does. */
export const DialogDots: React.FC<{ count: number; index: number }> = ({ count, index }) => (
    <div className="ui-dots" role="presentation">
        {Array.from({ length: count }, (_, i) => (
            <span key={i} className={`ui-dots__dot${i === index ? ' is-active' : ''}`} />
        ))}
    </div>
);
