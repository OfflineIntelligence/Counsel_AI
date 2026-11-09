/**
 * What an answer was read from, under the answer.
 *
 * This is the themed replacement for a block of tagged plain text. The same
 * passages used to reach the user only when a small model recited its own
 * prompt back — `[DOCUMENT EXCERPTS]`, a guidance line written for the model,
 * and a `--- [file, p.3, heading] ---` tag over each one. That is a debug
 * dump: it is addressed to the model, it is longer than the answer, and it
 * buries the reply it is supposed to support.
 *
 * So the passages come here as data instead, and the rules are:
 *
 *   - CLOSED BY DEFAULT. The answer is the thing the user asked for. Sources
 *     are for checking it, which is a second, deliberate act — and it is what
 *     makes three retrieved passages stop being "excess output".
 *   - The closed state still answers "what did it read?" — the file names are
 *     on the summary line, so the common question needs no click at all.
 *   - One card per passage, and the card leads with WHERE: file, page, and the
 *     section heading. That is what a citation is checked against.
 *   - Excerpt text is clamped to a few lines. A passage is prompt-sized, not
 *     reading-sized; anyone who wants the whole thing wants the document.
 */

import { useEffect, useRef, useState } from 'react';
import { FileText } from 'lucide-react';
import { FileGlyph, formatOf, FILE_ICON } from '../FileBrandIcon';
import { requestOpenSource } from '../../utils/openSource';
import { open as openExternal } from '@tauri-apps/plugin-shell';

import type { AnswerSource } from '../../api/chat';

/** "p.3", "pp.3-4", or nothing when the format has no pages. */
function pageLabel(s: AnswerSource): string | null {
    const { page_start: a, page_end: b } = s;
    if (a == null) return null;
    if (b == null || b === a) return `p.${a}`;
    return `pp.${a}-${b}`;
}

/** The passage's own "where", without the filename already shown beside it. */
function whereLabel(s: AnswerSource): string | null {
    const parts = [pageLabel(s), s.section_label?.trim() || null].filter(Boolean);
    return parts.length > 0 ? parts.join(' · ') : null;
}

/** A web address carried by a source, if any (none of today's document
 *  sources have one, but a cited page or link is shown as a link). */
function linkOf(s: AnswerSource): string | null {
    const m = `${s.citation} ${s.filename}`.match(/https?:\/\/\S+/);
    return m ? m[0].replace(/[),.;]+$/, '') : null;
}

/**
 * The sources pill, on the answer's action row.
 *
 * It lives in the strip left of the answer — the strip the sidebar slides
 * over when it opens — on the same line as Copy / Retry / Ask, so opening the
 * sidebar covers the sources and nothing else. It is a soft pill, the same
 * shape as the island, reading "3 sources".
 *
 * Clicking it opens a small card above it: one entry per source, scrolling
 * inside the card when there are many. An entry leads with WHERE (file and
 * page, or the link), then the cited line itself — the passage the answer
 * drew on. A document source can open that line in context inside the card.
 * Outside click or Esc closes it.
 */
export default function SourcesPanel({
    sources,
}: {
    sources: AnswerSource[];
}) {
    const [open, setOpen] = useState(false);
    /** Clicking a source takes you to it: a web source opens in the browser;
     *  a document goes to the screen it lives on — Vault, Workspace or
     *  Library — and opens there (App resolves where). */
    const go = (s: AnswerSource) => {
        const link = linkOf(s);
        if (link) {
            if ('__TAURI_INTERNALS__' in window) void openExternal(link);
            else window.open(link, '_blank', 'noopener');
        } else {
            requestOpenSource({ documentId: s.document_id, filename: s.filename, highlight: s.text, page: s.page_start });
        }
        setOpen(false);
    };
    const ref = useRef<HTMLElement | null>(null);
    // Where the card fits: above the pill when there is room in the
    // conversation's visible area, otherwise below it — and never taller
    // than that room, so it scrolls inside rather than running off screen.
    const [place, setPlace] = useState<{ below: boolean; max: number }>({ below: false, max: 300 });
    const toggle = () => {
        if (!open && ref.current) {
            const pill = ref.current.getBoundingClientRect();
            const area = (ref.current.closest('.chat-messages') ?? document.body).getBoundingClientRect();
            const above = pill.top - area.top - 12;
            const below = area.bottom - pill.bottom - 12;
            const goBelow = above < 220 && below > above;
            setPlace({ below: goBelow, max: Math.max(160, Math.min(300, goBelow ? below : above)) });
        }
        setOpen(o => !o);
    };

    useEffect(() => {
        if (!open) return;
        const onDown = (e: MouseEvent) => {
            if (!ref.current?.contains(e.target as Node)) setOpen(false);
        };
        const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setOpen(false); };
        document.addEventListener('mousedown', onDown);
        document.addEventListener('keydown', onKey);
        return () => {
            document.removeEventListener('mousedown', onDown);
            document.removeEventListener('keydown', onKey);
        };
    }, [open]);

    if (sources.length === 0) return null;
    const count = `${sources.length} ${sources.length === 1 ? 'source' : 'sources'}`;

    return (
        <section ref={ref} className={`src-stack${open ? ' is-open' : ''}`} aria-label="Sources for this answer">
            {open && (
                <div
                    className={`src-card-pop${place.below ? ' is-below' : ''}`}
                    role="dialog"
                    aria-label={count}
                    style={{ maxHeight: place.max }}
                >
                    <div className="src-card-pop__head">{count}</div>
                    <ol className="src-card-pop__list">
                        {sources.map((s, i) => {
                            const where = whereLabel(s);
                            const link = linkOf(s);
                            return (
                                <li key={`${s.document_id}-${i}`}>
                                    <button
                                        type="button"
                                        className="src-entry"
                                        onClick={() => go(s)}
                                        title={link ? `Open ${link}` : `Go to ${s.filename}`}
                                    >
                                        <span className="src-entry__head">
                                            <FileGlyph format={formatOf(s.filename)} size={FILE_ICON.list} />
                                            <span className="src-entry__name">{s.filename}</span>
                                            {where && <span className="src-entry__where">{where}</span>}
                                            <span className="src-entry__go" aria-hidden="true">{link ? '↗' : '›'}</span>
                                        </span>
                                        <span className="src-entry__line">{s.text.trim()}</span>
                                    </button>
                                </li>
                            );
                        })}
                    </ol>
                </div>
            )}
            <button
                type="button"
                className="src-pill"
                aria-expanded={open}
                aria-haspopup="dialog"
                onClick={toggle}
            >
                <FileText size={13} strokeWidth={1.75} aria-hidden="true" />
                {count}
            </button>
        </section>
    );
}
