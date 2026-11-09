// The what's-new announcement.
//
// This is the Qoder-style product-tour card translated into Counsel's own
// material. The reference leans on a green gradient wash; this system spends
// colour only on STATUS, so the hero is achromatic — an ink wash composed
// from `--accent-quiet` over a masked dot grid, which inverts with the theme
// for free because both are built from theme tokens rather than fixed rgba.
//
// # Adding an announcement
//
// Append to ANNOUNCEMENTS. The `id` is a database key, so it is permanent:
// changing it re-shows the dialog to everyone who already dismissed the old
// one. Only the newest UNSEEN entry is shown — a user returning after three
// releases gets the current one, not a queue of three modals.
//
// # Why it does not fire on first run
//
// `suppressed` is what a fresh install passes in. Someone who has never used
// Counsel does not need to be told what changed since a version they never
// had, and a what's-new modal over an empty first screen is the single most
// disposable thing the app could show. It is recorded as seen anyway, so it
// does not ambush them on their second launch either.

import React, { useEffect, useState } from 'react'
import { FileSearch, ListChecks, PanelRight } from 'lucide-react'

import { hasSeenAnnouncement, markAnnouncementSeen } from '../api/announcements'
import { Dialog, DialogActions, DialogHero, FeatureList, type Feature } from './ui'
import AssistantMark from './assistant/AssistantMark'

interface Announcement {
    /** Permanent. `[a-z0-9_-]`, 64 chars max — see the note above. */
    id: string
    eyebrow: string
    title: string
    subtitle?: string
    /** The sample exchange shown floating in the hero. */
    heroLead?: string
    heroReply?: string
    features: Feature[]
}

const ANNOUNCEMENTS: Announcement[] = [
    {
        id: 'assistant-panel-2026-09',
        eyebrow: 'New',
        title: 'A deeper AI Assistant',
        subtitle: 'The panel now shows what it can actually read, and keeps every thread you start.',
        heroLead: 'What does clause 9 say?',
        heroReply: 'Reading the whole document.',
        features: [
            {
                icon: <PanelRight size={17} strokeWidth={1.75} />,
                title: 'Chat, Context and History',
                description:
                    'Three views in one panel. Switch without losing an answer that is still being written.',
            },
            {
                icon: <FileSearch size={17} strokeWidth={1.75} />,
                title: 'See exactly what is in context',
                description:
                    'Context tells you whether the whole document reached the model, or only the first part of it.',
            },
            {
                icon: <ListChecks size={17} strokeWidth={1.75} />,
                title: 'Pick up any past thread',
                description:
                    'Assistant conversations are kept and listed, separate from your main discussions.',
            },
        ],
    },
]

export const WhatsNewDialog: React.FC<{
    /** True on a machine with nothing set up yet. See the note above. */
    suppressed?: boolean
}> = ({ suppressed = false }) => {
    const [current, setCurrent] = useState<Announcement | null>(null)
    const [open, setOpen] = useState(false)

    useEffect(() => {
        let alive = true

        void (async () => {
            // Newest first, so a user behind several releases sees only the
            // most recent thing they have not been told about.
            for (const a of [...ANNOUNCEMENTS].reverse()) {
                const seen = await hasSeenAnnouncement(a.id)
                if (!alive) return
                if (seen) continue

                if (suppressed) {
                    // Record it without showing it, so this never appears on
                    // a later launch as if it were news.
                    void markAnnouncementSeen(a.id)
                    return
                }
                setCurrent(a)
                setOpen(true)
                return
            }
        })()

        return () => {
            alive = false
        }
    }, [suppressed])

    if (!current) return null

    const dismiss = () => {
        setOpen(false)
        // Recorded on dismissal rather than on display: a dialog the user
        // never got to read — because the app was closed, or it was opened
        // behind another window — has not actually been seen.
        void markAnnouncementSeen(current.id)
    }

    return (
        <Dialog
            open={open}
            onClose={dismiss}
            labelledBy="whats-new-title"
            eyebrow={current.eyebrow}
            title={current.title}
            subtitle={current.subtitle}
            hero={
                <DialogHero
                    mark={<AssistantMark size={26} />}
                    lead={current.heroLead}
                    reply={current.heroReply}
                />
            }
            footer={
                <DialogActions>
                    <button type="button" className="ui-btn ui-btn--primary" onClick={dismiss}>
                        Got it
                    </button>
                </DialogActions>
            }
        >
            <FeatureList features={current.features} />
        </Dialog>
    )
}

export default WhatsNewDialog
