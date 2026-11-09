/**
 * The Library as its own page, reached from the sidebar.
 *
 * It used to be a tab inside the Workspace. It shares the Workspace's data
 * hook and landing styles, so it looks and behaves like the other pages;
 * opening a document from here hands it to the Workspace to edit.
 */

import { useState } from 'react'
import { Search, X } from 'lucide-react'

import LibraryView from './LibraryView'
import { useWorkspaceData } from './useWorkspaceData'
import { useErrorNotification } from '../../contexts/NotificationContext'
import './workspace.css'

export default function LibraryPanel({
  onOpenDraft,
  focusItemId,
  onFocusHandled,
}: {
  onOpenDraft: (draftId: number) => void
  focusItemId?: number | null
  onFocusHandled?: () => void
}) {
  const { matters, busy, error, setError, mutate: run } = useWorkspaceData()
  useErrorNotification(error, "Couldn't load Library", () => setError(null))
  const [query, setQuery] = useState('')

  return (
    <div className="ws-root">
      <main className="ws-main">
        <div className="ws-landing library-landing">
          <header className="ws-landing-head">
            <h1 className="ws-landing-title">Library</h1>
          </header>

          <div className="ws-searchbar">
            <Search size={15} strokeWidth={2.25} className="ws-search-icon" />
            <input
              className="ws-search-input"
              type="search"
              value={query}
              placeholder="Search library"
              aria-label="Search library"
              onChange={e => setQuery(e.target.value)}
              onKeyDown={e => {
                if (e.key === 'Escape') setQuery('')
              }}
            />
            {query && (
              <button type="button" className="ws-close" onClick={() => setQuery('')} aria-label="Clear search">
                <X size={14} strokeWidth={1.75} />
              </button>
            )}
          </div>

          <section className="ws-files-box library-box" aria-label="Library">
            <LibraryView
              busy={busy}
              matters={matters}
              run={work => void run(work)}
              onOpenDraft={onOpenDraft}
              query={query}
              focusItemId={focusItemId}
              onFocusHandled={onFocusHandled}
            />
          </section>

        </div>
      </main>
    </div>
  )
}
