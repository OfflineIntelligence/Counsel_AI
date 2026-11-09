// The UI kit — typed wrappers over the composite primitives in
// styles/surfaces.css.
//
// The point of this file is not to save typing. It is that a class name is
// not an API: `.ui-status--success` says nothing about which values are
// legal, and the day the status ramp changes, nothing points at the twelve
// call sites that need updating. A `StatusTone` union does both.
//
// Every component here is presentational and uncontrolled. None of them
// fetches, none of them holds state that outlives a render, and none of them
// knows what a matter or a document is. That is what keeps them reusable
// across the home state, the assistant panel and message rendering without
// any of those three leaking into the other two.

import React from 'react';
import { Check, ChevronRight, Loader2, X } from 'lucide-react';

export { Dialog, DialogActions, DialogHero, DialogDots, FeatureList } from './Dialog';
export type { DialogProps, DialogHeroProps, Feature } from './Dialog';

/* ═══════════════════════════════════════════════════════════════════════════
   STATUS
   ═══════════════════════════════════════════════════════════════════════════ */

export type StatusTone = 'neutral' | 'success' | 'warning' | 'danger';

export const StatusPill: React.FC<{
    tone?: StatusTone;
    /** Pulses the dot. For work genuinely in flight — not for "important". */
    active?: boolean;
    children: React.ReactNode;
}> = ({ tone = 'neutral', active = false, children }) => (
    <span
        className={`ui-status${tone === 'neutral' ? '' : ` ui-status--${tone}`}${active ? ' ui-status--active' : ''}`}
    >
        {children}
    </span>
);

/* ═══════════════════════════════════════════════════════════════════════════
   BANNER — a standing condition, inline in the page
   ═══════════════════════════════════════════════════════════════════════════
   Not a toast. A toast reports something that just happened and leaves; a
   banner states a condition that is still true and stays until it is not.
   ─────────────────────────────────────────────────────────────────────────── */

export const Banner: React.FC<{
    tone?: StatusTone;
    icon?: React.ReactNode;
    /** De-emphasised tail of the sentence — "…since your last visit". */
    muted?: React.ReactNode;
    actions?: React.ReactNode;
    onDismiss?: () => void;
    children: React.ReactNode;
}> = ({ tone = 'neutral', icon, muted, actions, onDismiss, children }) => (
    <div className={`ui-banner${tone === 'neutral' ? '' : ` ui-banner--${tone}`}`} role="status">
        {icon && (
            <span className="ui-banner__icon" aria-hidden="true">
                {icon}
            </span>
        )}
        <span className="ui-banner__text">
            {children}
            {muted && <span className="ui-banner__muted"> {muted}</span>}
        </span>
        {(actions || onDismiss) && (
            <span className="ui-banner__actions">
                {actions}
                {onDismiss && (
                    <button type="button" className="ui-iconbtn ui-iconbtn--sm" onClick={onDismiss} aria-label="Dismiss">
                        <X size={14} strokeWidth={1.75} />
                    </button>
                )}
            </span>
        )}
    </div>
);

/* ═══════════════════════════════════════════════════════════════════════════
   SEGMENTED — a filter over one list
   ═══════════════════════════════════════════════════════════════════════════ */

export interface SegmentOption<T extends string> {
    value: T;
    label: string;
    /** Shown beside the label. Zero is rendered — a "0" is information. */
    count?: number;
}

export function Segmented<T extends string>({
    options,
    value,
    onChange,
    label,
}: {
    options: SegmentOption<T>[];
    value: T;
    onChange: (next: T) => void;
    /** Accessible name for the group, e.g. "Filter reviews". */
    label?: string;
}) {
    return (
        <div className="ui-segmented" role="tablist" aria-label={label}>
            {options.map((opt) => (
                <button
                    key={opt.value}
                    type="button"
                    role="tab"
                    aria-selected={opt.value === value}
                    className="ui-segmented__item"
                    onClick={() => onChange(opt.value)}
                >
                    {opt.label}
                    {opt.count != null && <span className="ui-segmented__count">{opt.count}</span>}
                </button>
            ))}
        </div>
    );
}

/* ═══════════════════════════════════════════════════════════════════════════
   STEPS — a run's progress
   ═══════════════════════════════════════════════════════════════════════════ */

export type StepState = 'done' | 'active' | 'pending';

export interface Step {
    id: string;
    label: string;
    state: StepState;
}

const STEP_MARKER: Record<StepState, React.ReactNode> = {
    // The spin is CSS, on the svg — see `.ui-step--active .ui-step__marker`.
    active: <Loader2 size={14} strokeWidth={2} />,
    done: <Check size={14} strokeWidth={2.25} />,
    pending: <Circle />,
};

/** An empty ring. Drawn rather than imported: lucide's Circle at 14px renders
 *  heavier than the check beside it, and the two must read as one family. */
function Circle() {
    return (
        <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
            <circle cx="7" cy="7" r="5.25" fill="none" stroke="currentColor" strokeWidth="1.25" />
        </svg>
    );
}

export const Steps: React.FC<{ steps: Step[]; label?: string }> = ({ steps, label }) => (
    <ol className="ui-steps" aria-label={label}>
        {steps.map((s) => (
            <li key={s.id} className={`ui-step ui-step--${s.state}`} aria-current={s.state === 'active' || undefined}>
                <span className="ui-step__marker" aria-hidden="true">
                    {STEP_MARKER[s.state]}
                </span>
                <span className="ui-step__label">{s.label}</span>
            </li>
        ))}
    </ol>
);

/* ═══════════════════════════════════════════════════════════════════════════
   FILE CARD — a document as an object
   ═══════════════════════════════════════════════════════════════════════════ */

/** The glyph text is the extension, uppercased and clipped to three — DOCX
 *  and XLSX are indistinguishable at 22px if all four letters are forced in,
 *  and the first three are already unambiguous. */
function extensionOf(name: string): string {
    const dot = name.lastIndexOf('.');
    if (dot < 0 || dot === name.length - 1) return 'DOC';
    return name.slice(dot + 1, dot + 4).toUpperCase();
}

export const FileCard: React.FC<{
    name: string;
    /** Size, page count, "reused", whatever is the most useful second line. */
    meta?: React.ReactNode;
    onClick?: () => void;
    small?: boolean;
    trailing?: React.ReactNode;
}> = ({ name, meta, onClick, small = false, trailing }) => {
    // A card with nothing to do is not a button. Rendering it as one puts it
    // in the tab order and promises an interaction that never comes.
    const Tag = onClick ? 'button' : 'span';
    return (
        <Tag
            {...(onClick ? { type: 'button' as const, onClick } : {})}
            className={`ui-filecard${small ? ' ui-filecard--sm' : ''}`}
        >
            <span className="ui-filecard__glyph" aria-hidden="true">
                {extensionOf(name)}
            </span>
            <span className="ui-filecard__body">
                <span className="ui-filecard__name">{name}</span>
                {meta && <span className="ui-filecard__meta">{meta}</span>}
            </span>
            {trailing}
        </Tag>
    );
};

/* ═══════════════════════════════════════════════════════════════════════════
   SUGGESTION ROW — a proposed action
   ═══════════════════════════════════════════════════════════════════════════ */

export const SuggestRow: React.FC<{
    icon?: React.ReactNode;
    title: React.ReactNode;
    description?: React.ReactNode;
    onClick: () => void;
    /** Replaces the hover arrow — a shortcut hint, a count, a status. */
    trailing?: React.ReactNode;
}> = ({ icon, title, description, onClick, trailing }) => (
    <button type="button" className="ui-suggest" onClick={onClick}>
        {icon && (
            <span className="ui-suggest__icon" aria-hidden="true">
                {icon}
            </span>
        )}
        <span className="ui-suggest__body">
            <span className="ui-suggest__title">{title}</span>
            {description && <span className="ui-suggest__desc">{description}</span>}
        </span>
        {trailing ?? (
            <span className="ui-suggest__arrow" aria-hidden="true">
                <ChevronRight size={15} strokeWidth={1.75} />
            </span>
        )}
    </button>
);

/* ═══════════════════════════════════════════════════════════════════════════
   TILE — a navigable container
   ═══════════════════════════════════════════════════════════════════════════ */

export const Tile: React.FC<{
    icon?: React.ReactNode;
    title: React.ReactNode;
    meta?: React.ReactNode;
    onClick: () => void;
}> = ({ icon, title, meta, onClick }) => (
    <button type="button" className="ui-tile" onClick={onClick}>
        {icon && (
            <span className="ui-tile__icon" aria-hidden="true">
                {icon}
            </span>
        )}
        <span className="ui-tile__title">{title}</span>
        {meta && <span className="ui-tile__meta">{meta}</span>}
    </button>
);

export const TileGrid: React.FC<{ children: React.ReactNode }> = ({ children }) => (
    <div className="ui-tile-grid ui-stagger">{children}</div>
);

/* ═══════════════════════════════════════════════════════════════════════════
   EMPTY STATE / SECTION HEADER
   ═══════════════════════════════════════════════════════════════════════════ */

export const EmptyState: React.FC<{
    icon?: React.ReactNode;
    title: React.ReactNode;
    description?: React.ReactNode;
    action?: React.ReactNode;
    compact?: boolean;
}> = ({ icon, title, description, action, compact = false }) => (
    <div className={`ui-empty${compact ? ' ui-empty--sm' : ''}`}>
        {icon && (
            <span className="ui-empty__icon" aria-hidden="true">
                {icon}
            </span>
        )}
        <span className="ui-empty__title">{title}</span>
        {description && <p className="ui-empty__desc">{description}</p>}
        {action}
    </div>
);

export const SectionHeader: React.FC<{
    title: React.ReactNode;
    description?: React.ReactNode;
    actions?: React.ReactNode;
}> = ({ title, description, actions }) => (
    <div className="ui-section">
        <span className="ui-section__title">{title}</span>
        {description && <span className="ui-section__desc">{description}</span>}
        {actions && <span className="ui-section__actions">{actions}</span>}
    </div>
);

/** A key hint. `⌘K`, `Enter`, `Esc`. */
export const Kbd: React.FC<{ children: React.ReactNode }> = ({ children }) => (
    <kbd className="ui-kbd">{children}</kbd>
);
