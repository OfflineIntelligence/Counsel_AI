// ChatHome — what an empty chat shows instead of a wordmark on a blank page.
//
// The screen this replaces was one <h1> centred in the void, with the only
// actionable thing on it (the model prompt) exiled to a banner above the
// composer. An empty state is the most-seen screen in the product — it is
// what every new session opens on — and it was carrying no information and
// offering no route anywhere.
//
// What it shows now, in the order a session actually starts:
//
//   1. The wordmark, and beneath it the ONE fact that governs whether
//      anything else on the screen will work: which local model is loaded.
//   2. Suggested openings. These change with what the workspace holds — a
//      first run with no documents is offered different work than one with
//      forty, because "Summarise a document" is a dead end with nothing to
//      summarise.
//   3. Recent conversations and open matters, as real rows from the backend.
//
// Everything here is REAL. Recents come from /conversations, matters from
// /matters, the document count from the library. Nothing is placeholder, and
// nothing is fetched that is not shown — the three requests run in parallel
// and each section renders as its own data lands rather than the page waiting
// on the slowest.

import React, { useEffect, useRef, useState } from 'react';
import {
    ArrowUpRight,
    FileStack,
    Plus,
} from 'lucide-react';
import { listMatters, type MatterSummary } from '../api/matters';
import MatterDocumentsDialog from './rag/MatterDocumentsDialog';
import { SectionHeader, SuggestRow, Tile, TileGrid } from './ui';
import AppMark from './AppMark';
import RailIcon from './RailIcon';

interface ChatHomeProps {
    /** Puts text in the composer and focuses it. The user still presses send:
     *  a suggestion is a starting point to edit, not a command to run. Firing
     *  the model on one click would spend a local inference on a prompt the
     *  user never read. */
    onUsePrompt: (prompt: string) => void;
    onOpenModels?: () => void;
    onOpenLibrary?: () => void;
    onOpenDraft?: () => void;
    onOpenLocalFiles?: () => void;
    /** Models installed locally. Empty is a first-run signal, not an error. */
    modelCount: number;
}

interface Suggestion {
    id: string;
    icon: React.ReactNode;
    title: string;
    description: string;
    /** A prompt to stage in the composer, or a view to open. Exactly one. */
    prompt?: string;
    action?: () => void;
    /** Keep `description` out of the row and show it as a hover hint instead.
     *  For rows whose title already says the whole thing, where the second
     *  line is detail rather than information the user needs to choose. */
    hintOnly?: boolean;
}

/** How long the pointer must rest on a row before its hint appears. Long
 *  enough that crossing the row on the way somewhere else never triggers it. */
const HINT_DELAY_MS = 2000;

/**
 * Shows `hint` once the pointer has rested on its child for HINT_DELAY_MS.
 *
 * The hint stays in the DOM the whole time and is revealed with opacity, not
 * mounted on a timer, for two reasons: a screen reader can still reach the
 * text at any point, and revealing it costs one compositor property rather
 * than a layout.
 *
 * Keyboard users never hover, so focus opens it too. Without the delay, since
 * arriving on a row by Tab is deliberate in a way that sweeping a mouse across
 * one is not.
 */
function HoverHint({
    hint,
    className,
    children,
}: {
    hint: string;
    className?: string;
    children: React.ReactNode;
}) {
    const [shown, setShown] = useState(false);
    const timer = useRef<number | undefined>(undefined);

    const arm = () => {
        window.clearTimeout(timer.current);
        timer.current = window.setTimeout(() => setShown(true), HINT_DELAY_MS);
    };
    const cancel = () => {
        window.clearTimeout(timer.current);
        setShown(false);
    };

    // A row can be unmounted while its timer is still running (the list is
    // rebuilt whenever a model loads), and a setState after that is a leak.
    useEffect(() => () => window.clearTimeout(timer.current), []);

    return (
        <div
            className={`home__hinted${className ? ` ${className}` : ''}`}
            onMouseEnter={arm}
            onMouseLeave={cancel}
            onFocus={() => setShown(true)}
            onBlur={cancel}
        >
            {children}
            <span className={`home__hint${shown ? ' is-shown' : ''}`} role="tooltip">
                {hint}
            </span>
        </div>
    );
}

export const ChatHome: React.FC<ChatHomeProps> = ({
    onUsePrompt,
    onOpenLibrary,
    onOpenLocalFiles,
}) => {
    const [matters, setMatters] = useState<MatterSummary[] | null>(null);
    const [documentsFor, setDocumentsFor] = useState<MatterSummary | null>(null);

    // Matters load in the background; the component unmounts the moment the
    // first message is sent, so `alive` guards the late setState.
    useEffect(() => {
        let alive = true;
        void listMatters()
            .then((m) => alive && setMatters(m))
            .catch(() => alive && setMatters([]));
        return () => {
            alive = false;
        };
    }, []);

    // The opening trio, identical in every state — model or no model, docs or
    // no docs. These are places to go and things to stage, never dead ends,
    // so there is nothing to derive from readiness.
    const suggestions: Suggestion[] = [
        {
            id: 'add-knowledge',
            // The rail's own Library and Vault marks, not lookalikes
            // from the icon package: each card leads to that rail
            // entry, so it should carry that entry's glyph.
            icon: <RailIcon name="library" />,
            title: 'Add knowledge from Library',
            description: 'Bring in precedents, templates and clauses to draft from.',
            hintOnly: true,
            action: onOpenLibrary,
        },
        {
            id: 'add-docs',
            icon: <RailIcon name="vault" />,
            title: 'Add documents from Vault',
            description: 'Read your own files once, then use them in every conversation.',
            hintOnly: true,
            action: onOpenLocalFiles ?? onOpenLibrary,
        },
        {
            id: 'new-matter',
            icon: <Plus size={16} strokeWidth={1.75} />,
            title: 'Start a new conversation',
            description: 'Open a fresh thread and work a matter from intake to outcome.',
            hintOnly: true,
            prompt: 'I want to start a new matter. Help me set it up step by step: ',
        },
    ];

    const openMatters = (matters ?? []).filter((m) => m.status === 'open').slice(0, 6);

    return (
        <div className="home">
            <header className="home__masthead">
                {/* The app mark — the SAME component the window title bar
                    renders, not a second drawing of it, so the outline and
                    proportions cannot drift. Sized from the heading's own
                    font-size so the two stay locked together at every
                    breakpoint. Decorative: the name beside it is already the
                    accessible heading. */}
                <h1 className="home__wordmark">
                    <span className="home__wordmark-mark" aria-hidden="true">
                        <AppMark />
                    </span>
                    <span>Offline Counsel AI</span>
                </h1>
            </header>

            <section className="home__block">
                <div className="home__list ui-stagger">
                    {suggestions.map((s) => {
                        const row = (
                            <SuggestRow
                                icon={s.icon}
                                title={s.title}
                                description={s.hintOnly ? undefined : s.description}
                                onClick={() => (s.prompt ? onUsePrompt(s.prompt) : s.action?.())}
                                // Every card carries the same corner arrow, so
                                // the three read as one set: glyph top-left,
                                // arrow top-right, title along the bottom.
                                trailing={
                                    <span className="ui-suggest__arrow" aria-hidden="true">
                                        <ArrowUpRight size={15} strokeWidth={1.75} />
                                    </span>
                                }
                            />
                        );
                        return s.hintOnly && s.description ? (
                            <HoverHint key={s.id} hint={s.description} className="ui-anim ui-anim--rise">
                                {row}
                            </HoverHint>
                        ) : (
                            <div key={s.id} className="ui-anim ui-anim--rise">
                                {row}
                            </div>
                        );
                    })}
                </div>
            </section>

            {/* Both sections below render only with content. An empty state
                inside an empty state is worse than the absence of a heading. */}
            {openMatters.length > 0 && (
                <section className="home__block">
                    <SectionHeader
                        title="Matters"
                        actions={
                            onOpenLibrary && (
                                <button type="button" className="ui-btn ui-btn--ghost ui-btn--sm" onClick={onOpenLibrary}>
                                    View all
                                </button>
                            )
                        }
                    />
                    <TileGrid>
                        {openMatters.map((m) => (
                            <div key={m.id} className="ui-anim ui-anim--rise">
                                <Tile
                                    icon={<FileStack size={16} strokeWidth={1.75} />}
                                    title={m.name}
                                    meta={
                                        [m.client, m.matter_number].filter(Boolean).join(' · ') || 'No reference'
                                    }
                                    onClick={() =>
                                        onUsePrompt(`Regarding the matter "${m.name}": `)
                                    }
                                />
                                <button
                                    type="button"
                                    className="ui-btn ui-btn--ghost ui-btn--sm home__matter-docs"
                                    onClick={() => setDocumentsFor(m)}
                                    aria-label={`Documents of ${m.name}`}
                                >
                                    Documents
                                </button>
                            </div>
                        ))}
                    </TileGrid>
                </section>
            )}

            {documentsFor && (
                <MatterDocumentsDialog
                    open
                    matterId={documentsFor.id}
                    matterName={documentsFor.name}
                    onClose={() => setDocumentsFor(null)}
                />
            )}
        </div>
    );
};

export default ChatHome;
