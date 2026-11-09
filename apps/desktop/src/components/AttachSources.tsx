// The folder button's attach sources, above the chat composer.
//
// Clicking the folder slides three small pills out beside it — Vault,
// Workspace, Library — each with the sidebar's own icon for that place.
// Picking one opens SourcePane: a panel from the right edge, over a dimmed
// chat, listing only that place's files, with a checkbox on each so several
// can be attached at once. Everything picked here joins the folder stack;
// the paperclip is for files from the computer.

import React, { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import RailIcon from './RailIcon';
import { FileGlyph, formatOf, FILE_ICON } from './FileBrandIcon';
import { listDrafts, type DraftSummary } from '../api/drafts';
import { listLibrary, type LibraryItem } from '../api/library';
import { getApiBaseSync } from '../api/backendUrl';

export type AttachSource = 'vault' | 'workspace' | 'library';

/** One file the pane can attach. `path` is the Vault folder it sits in. */
export interface SourceEntry { id: number; name: string; format: string; path?: string }

const SOURCES: { key: AttachSource; label: string }[] = [
  { key: 'vault', label: 'Vault' },
  { key: 'workspace', label: 'Workspace' },
  { key: 'library', label: 'Library' },
];

/** Closes on Escape, and on a mouse-down outside `ref` that is not on one of
 *  the `keep` selectors (controls that handle their own click). */
function useDismiss(ref: React.RefObject<HTMLElement | null>, onClose: () => void, keep: string[] = []) {
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      const t = e.target as Element;
      if (ref.current?.contains(t) || keep.some(k => t.closest?.(k))) return;
      onClose();
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
    // `keep` is a literal at each call site; its identity is not a reason to rebind.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ref, onClose]);
}

/** The folder button's menu: a small box above the composer's top-left,
 *  offering From Vault and From Library (Workspace is not offered here). */
export function SourcePills({ onPick, onClose }: { onPick: (s: AttachSource) => void; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(ref, onClose, ['.input-folder-btn']);
  return (
    <div ref={ref} className="attach-source-pills attach-source-menu" role="menu" aria-label="Attach from">
      {SOURCES.filter(s => s.key !== 'workspace').map((s, i) => (
        <button
          key={s.key}
          type="button"
          role="menuitem"
          className="attach-source-pill"
          style={{ animationDelay: `${i * 40}ms` }}
          onClick={() => onPick(s.key)}
        >
          <RailIcon name={s.key} />
          From {s.label}
        </button>
      ))}
    </div>
  );
}

function loadEntries(source: AttachSource): Promise<SourceEntry[]> {
  if (source === 'workspace') {
    return listDrafts().then((d: DraftSummary[]) =>
      d.map(x => ({ id: x.id, name: `${x.title}.${x.format}`, format: x.format })));
  }
  if (source === 'library') {
    return listLibrary().then((l: LibraryItem[]) =>
      l.map(x => ({ id: x.id, name: `${x.title}.${x.format}`, format: x.format })));
  }
  return fetch(`${getApiBaseSync()}/files/all`)
    .then(r => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
    .then((all: unknown) =>
      (Array.isArray(all) ? all : [])
        .filter((f: { is_directory?: boolean; isDirectory?: boolean }) => !f.is_directory && !f.isDirectory)
        .map((f: { id: number; name: string; path?: string }) => {
          // The folder it lives in, shown under its name; the root shows none.
          const folder = (f.path ?? '').replace(/\\/g, '/').split('/').slice(0, -1).join('/');
          return { id: f.id, name: f.name, format: formatOf(f.name), path: folder || undefined };
        }));
}

/**
 * One place's files, from the right edge of the window, over a dimmed chat.
 * Checkboxes pick; Attach hands the picked files back. At most `room` files
 * can be picked, the slots this message still has. Escape or a click outside
 * closes it without attaching.
 */
export function SourcePane({
  source,
  room,
  attached,
  onAttach,
  onClose,
}: {
  source: AttachSource;
  room: number;
  /** Names already in the folder stack: shown as attached, not pickable. */
  attached: string[];
  onAttach: (entries: SourceEntry[]) => void;
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(ref, onClose);

  // A modal pane holds the keyboard: Tab cycles inside it, and on close focus
  // goes back to the folder button that opened it.
  useEffect(() => {
    const pane = ref.current;
    const onTab = (e: KeyboardEvent) => {
      if (e.key !== 'Tab' || !pane) return;
      const stops = Array.from(pane.querySelectorAll<HTMLElement>('input:not(:disabled), button:not(:disabled)'));
      if (stops.length === 0) return;
      const first = stops[0];
      const last = stops[stops.length - 1];
      if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
      else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
    };
    document.addEventListener('keydown', onTab);
    return () => {
      document.removeEventListener('keydown', onTab);
      document.querySelector<HTMLElement>('.input-folder-btn')?.focus();
    };
  }, []);

  const [entries, setEntries] = useState<SourceEntry[] | null>(null);
  const [failed, setFailed] = useState(false);
  const [query, setQuery] = useState('');
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const place = SOURCES.find(s => s.key === source)!.label;

  useEffect(() => {
    let live = true;
    loadEntries(source)
      .then(e => { if (live) setEntries(e.sort((a, b) => a.name.localeCompare(b.name))); })
      .catch(() => { if (live) setFailed(true); });
    return () => { live = false; };
  }, [source]);

  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (entries ?? []).filter(e => `${e.name} ${e.path ?? ''}`.toLowerCase().includes(q));
  }, [entries, query]);
  const pickable = shown.filter(e => !attached.includes(e.name));
  const allShownPicked = pickable.length > 0 && pickable.every(e => picked.has(e.id));
  const full = picked.size >= room;

  const toggle = (id: number) =>
    setPicked(prev => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else if (next.size < room) next.add(id);
      return next;
    });

  const toggleAll = () =>
    setPicked(prev => {
      const next = new Set(prev);
      if (allShownPicked) {
        pickable.forEach(e => next.delete(e.id));
      } else {
        for (const e of pickable) {
          if (next.size >= room) break;
          next.add(e.id);
        }
      }
      return next;
    });

  return createPortal(
    <div className="source-pane-scrim">
      <aside ref={ref} className="source-pane" role="dialog" aria-modal="true" aria-label={`Attach from ${place}`}>
        <header className="source-pane__head">
          <span className="source-pane__title">
            <RailIcon name={source} />
            {place}
          </span>
        </header>

        <label className="source-pane__search">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" aria-hidden="true">
            <circle cx="11" cy="11" r="7" />
            <path d="M20 20l-3.5-3.5" />
          </svg>
          <input
            type="text"
            placeholder={`Search ${place.toLowerCase()}`}
            value={query}
            onChange={e => setQuery(e.target.value)}
            aria-label={`Search ${place}`}
            autoFocus
          />
        </label>

        <div className="source-pane__rows">
          {failed ? (
            <div className="source-pane__empty">Couldn't load your {place}.</div>
          ) : entries === null ? (
            <div className="source-pane__empty">Loading…</div>
          ) : shown.length === 0 ? (
            <div className="source-pane__empty">{entries.length === 0 ? `Nothing in your ${place} yet.` : 'No matches'}</div>
          ) : (
            shown.map(e => {
              const isAttached = attached.includes(e.name);
              const on = picked.has(e.id);
              return (
                <label
                  key={e.id}
                  className={`source-pane__row${on ? ' is-picked' : ''}${isAttached || (full && !on) ? ' is-off' : ''}`}
                >
                  <input
                    type="checkbox"
                    checked={on || isAttached}
                    disabled={isAttached || (full && !on)}
                    onChange={() => toggle(e.id)}
                  />
                  <FileGlyph format={e.format} size={FILE_ICON.list} />
                  <span className="source-pane__name">
                    <span className="source-pane__file">{e.name}</span>
                    {(isAttached || e.path) && (
                      <span className="source-pane__meta">{isAttached ? 'Already attached' : e.path}</span>
                    )}
                  </span>
                </label>
              );
            })
          )}
        </div>

        <footer className="source-pane__foot">
          {/* Select all, bottom-left; the count beside it. */}
          <label className="source-pane__check">
            <input type="checkbox" checked={allShownPicked} onChange={toggleAll} disabled={pickable.length === 0} />
            Select all
          </label>
          <span className="source-pane__count">
            {full ? `Limit reached (${room} more)` : `${picked.size} selected`}
          </span>
          <button type="button" className="source-pane__btn" onClick={onClose}>Cancel</button>
          <button
            type="button"
            className="source-pane__btn source-pane__btn--primary"
            disabled={picked.size === 0}
            onClick={() => onAttach((entries ?? []).filter(e => picked.has(e.id)))}
          >
            {picked.size > 0 ? `Attach ${picked.size}` : 'Attach'}
          </button>
        </footer>
      </aside>
    </div>,
    document.body,
  );
}
