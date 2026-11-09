import React, { useMemo } from 'react';
import AppMark from './AppMark';
import RailIcon from './RailIcon';
import { useAuth } from '../contexts/AuthContext';

type Conversation = {
  id: string;
  title: string;
  createdAt: Date | string;
  pinned?: boolean;
  saved?: boolean;
  messages?: unknown[];
};

interface SidebarProps {
  conversations?: Conversation[];
  chats?: Conversation[];
  selectedChatId?: string | null;
  activeChatId?: string | null;
  onNewChat?: () => void;
  onSelectChat?: (chatId: string) => void;
  onDeleteChat?: (chatId: string) => void;
  onPinChat?: (chatId: string) => void;
  onSaveChat?: (chatId: string) => void;
  onOpenModels?: () => void;
  onOpenSettings?: () => void;
  onOpenHelp?: () => void;
  /** The user button above Settings: set up, lock or remove the login. */
  onOpenAccount?: () => void;
  onOpenLocalFiles?: () => void;
  onOpenDraft?: () => void;
  onOpenLibrary?: () => void;
  /** Top-level Workspace folders shown under Spaces. */
  spaces?: { id: number; name: string }[];
  onOpenSpace?: (folderId: number) => void;
  /** Previous Workspace drafts, listed while the Workspace is active. */
  recentDrafts?: { id: number; title: string }[];
  onOpenRecentDraft?: (draftId: number) => void;
  activeView?: string;
  /** Whether the app-wide AI Assistant panel is showing. */
  assistantOpen?: boolean;
  onToggleAssistant?: () => void;
  isOpen: boolean;
  onToggle: () => void;

  /* Five props that nothing here reads have been removed rather than left as
     optional-and-ignored: onOpenActivity, onOpenNotifications and
     onOpenMetrics (no caller ever passed them -- the pages they named are not
     in the rail), onOpenHome (App passed it, but it was a duplicate of
     onNewChat, which is what the New Chat button actually calls), and userName
     (passed, and only ever used to compute `initials` for an avatar this rail
     does not draw).

     An optional prop a component ignores is worse than no prop: a caller
     wiring it up gets no error and no behaviour. */
}

/* A `LoginGlyph` component stood here -- a hand-drawn person-in-a-square for a
   "Log in" rail entry that no longer exists. Signing in is reached from
   Settings. */

/**
 * A rail label that reveals word by word as the sidebar opens — "New" then
 * "Chat" — instead of popping in and wrapping mid-animation. Every button
 * already carries an aria-label, so the words are pure visuals: positions
 * never move, rows never change height, and the rail's own clipping does the
 * hiding while the words fade/slide in place.
 */
const RailLabel: React.FC<{ text: string }> = ({ text }) => {
  const words = text.split(' ');
  return (
    <span>
      {words.map((w, i) => (
        <React.Fragment key={i}>
          {i > 0 ? ' ' : null}
          <span className="rail-word" style={{ '--i': i } as React.CSSProperties}>
            {w}
          </span>
        </React.Fragment>
      ))}
    </span>
  );
};

export const Sidebar: React.FC<SidebarProps> = ({  conversations,
  chats,
  selectedChatId,
  activeChatId,
  onNewChat,
  onSelectChat,
  onDeleteChat,
  onPinChat,
  onSaveChat,
  onOpenModels,
  onOpenSettings,
  onOpenHelp,
  onOpenAccount,
  onOpenLocalFiles,
  onOpenDraft,
  onOpenLibrary,
  spaces = [],
  onOpenSpace,
  recentDrafts = [],
  onOpenRecentDraft,
  activeView,
  assistantOpen = false,
  onToggleAssistant,
  isOpen,
  onToggle,
}) => {
  /* ── What was removed from here, and why it mattered ──────────────────────
     Nine pieces of state, a ref, a memo and a document-level listener used to
     stand in this block. None of them was read by anything this component
     renders, and three of them were costing something every single render of
     the app's chrome:

     - `search` was read by the chat filter below, but `setSearch` was never
       called by anything: the search field it belonged to is gone (SearchModal
       owns search now). So the filter ran `title.toLowerCase().includes('')`
       over every conversation on every render to decide, always, to keep it.
     - `profileRef` was attached to no element, so it was permanently null, and
       a `mousedown` listener on the whole document ran on every click in the
       app to read that null ref and set `profilePopup`, which nothing read.
     - the `source` memo listed `activeView` as a dependency it does not use,
       so it recomputed on every navigation.

     The rest -- `initials`, `pinnedChats`, `otherChats`, `hoveredChat`,
     `showHistory`, `isLoggedIn`, `logout`, `user` (kept alive by a `const _ =
     user` line written to silence the warning about it) -- were simply
     computed and discarded. */
  const source = useMemo(() => conversations ?? chats ?? [], [conversations, chats]);
  const selected = selectedChatId ?? activeChatId ?? null;

  // Pinned first, then newest first.
  const sorted = useMemo(() => {
    const list = Array.isArray(source) ? [...source] : [];
    return list.sort((a, b) => {
      if (a.pinned && !b.pinned) return -1;
      if (!a.pinned && b.pinned) return 1;
      return new Date(b.createdAt).getTime() - new Date(a.createdAt).getTime();
    });
  }, [source]);

  /* This used to return a fragment wrapping the nav AND a LoginModal whose
     `isOpen={showLoginModal}` could never be true, because `setShowLoginModal`
     was never called -- a modal mounted permanently closed. Signing in is
     reached from Settings; the modal and the fragment are both gone, so the
     nav is the component.

     Always mounted. Closed it becomes an icon rail rather than being
     unmounted, so the destinations stay one click away. Its empty space opens
     and closes it; its icons always go straight to their page. */
  const { user: signedInUser } = useAuth();
  const lockProfile = signedInUser ? { username: signedInUser.name } : null;
  return (
      <nav
        className={`sidebar${isOpen ? '' : ' sidebar--rail'}`}
        aria-label="Chat navigation"
        role="navigation"
        onClick={e => {
          if (!(e.target as Element).closest('button, a, input, li, [role="menu"]')) onToggle()
        }}
      >
      {/* The brand row is empty, and deliberately still here.

          The mark it used to hold now stands for Aurora further down the rail,
          and the app's name is already on the window and reaches a screen
          reader through this nav's own label — so nothing is left to draw. But
          the row also set where every destination below it begins. Deleting
          the element would have slid the whole rail up by its height, which is
          a change to the sidebar nobody asked for, so the box stays and only
          its contents go. */}
      <div className="sidebar-brand" aria-hidden="true" />

      {/* New Chat */}
      <button className="sidebar-nav-btn" onClick={() => onNewChat?.()} aria-label="New chat">
        <RailIcon name="discussion" />
        <RailLabel text="New Chat" />
      </button>

      {/* The order below leads with the place work is DONE — the Workspace —
          and puts the material it draws on behind it (Library, then Vault),
          with the engine that runs it after those. Aurora sits last because it
          is not a page at all: it is a panel that opens over whichever of these
          is showing. */}

      {/* Workspace — where documents are organised and edited.
          Its icon was previously a byte-identical copy of New Chat's
          pencil, which made the two indistinguishable in the collapsed rail
          where the label is gone. This is a folder-and-document mark instead:
          the workspace holds files, which is what tells it apart from a chat. */}
      <button className="sidebar-nav-btn" onClick={onOpenDraft} type="button" aria-label="Workspace">
        <RailIcon name="workspace" />
        <RailLabel text="Workspace" />
      </button>

      {/* Library — precedents, templates and clauses to start documents from. */}
      <button className="sidebar-nav-btn" onClick={onOpenLibrary} type="button" aria-label="Library">
        <RailIcon name="library" />
        <RailLabel text="Library" />
      </button>

      {/* Vault — the document library. */}
      <button className="sidebar-nav-btn" onClick={onOpenLocalFiles} aria-label="Vault">
        <RailIcon name="vault" />
        <RailLabel text="Vault" />
      </button>

      {/* Models - Box icon */}
      <button className="sidebar-nav-btn" onClick={onOpenModels} aria-label="Manage models">
        <RailIcon name="models" />
        <RailLabel text="Models" />
      </button>

      {/* Aurora — opens the app-wide panel over whatever page is open.

          It carries AppMark, the outlined triangle that used to sit at the top
          of this rail, rather than the solid one from the icon family: the two
          triangles differed only in fill, which is not a difference you can
          see in a 20px glyph, and only one of them is now on screen.

          Rendered as a bare <svg>, NOT wrapped in a span. The collapsed rail
          hides a button's direct span children to drop the labels, so a mark
          inside a span disappeared the moment the sidebar closed — Aurora was
          the one rail entry with no icon there. As an svg it is treated
          exactly like every other rail icon, by the same rules. */}
      <button
        className={`sidebar-nav-btn${assistantOpen ? ' is-active' : ''}`}
        type="button"
        aria-label="Agent Lara"
        aria-pressed={assistantOpen}
        title="Agent Lara"
        onClick={() => onToggleAssistant?.()}
      >
        <AppMark size={20} className="rail-icon sidebar-nav-mark" />
        <RailLabel text="Agent Lara" />
      </button>

      {/* Only on the page they belong to, with no heading: previous chats
          while New Chat is active; spaces (Workspace folders) and
          previous drafts while the Workspace is active. Each row is a grey
          dot and a name. */}
      {activeView === 'chat' && sorted.length > 0 && (
        <ul className="sidebar-plain-list sidebar-context-list">
          {sorted.map(chat => (
            /* The row is an <li> holding a title button and three action
               buttons, rather than one big button. A <button> may not contain
               other buttons, so the old single-button row had nowhere to put
               these. The actions appear on hover or keyboard focus. */
            <li key={chat.id} className="sidebar-row">
              <button
                type="button"
                className={`sidebar-plain-row${chat.id === selected ? ' is-on' : ''}`}
                onClick={() => onSelectChat?.(chat.id)}
                title={chat.title}
              >
                <span className="sidebar-dot" aria-hidden="true" />
                <span className="sidebar-plain-label">{chat.title}</span>
              </button>

              <span className="sidebar-row-actions">
                {/* A pin. Filled once the chat is pinned, so the button
                    reports the state as well as changing it. */}
                <button
                  type="button"
                  className={`sidebar-icon-btn-xs${chat.pinned ? ' is-on' : ''}`}
                  aria-pressed={!!chat.pinned}
                  title={chat.pinned ? 'Unpin chat' : 'Pin chat'}
                  aria-label={chat.pinned ? 'Unpin chat' : 'Pin chat'}
                  onClick={e => {
                    e.stopPropagation();
                    onPinChat?.(chat.id);
                  }}
                >
                  <svg width="13" height="13" viewBox="0 0 24 24" fill={chat.pinned ? 'currentColor' : 'none'} stroke="currentColor" strokeWidth={1.8} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                    <path d="M15 3.5v5.2l2.6 2.6a1.5 1.5 0 0 1 .4 1V14h-5v6l-1 1.5-1-1.5v-6H6v-1.7c0-.38.14-.74.4-1L9 8.7V3.5h6z" />
                  </svg>
                </button>

                {/* A bookmark, for saving. */}
                <button
                  type="button"
                  className={`sidebar-icon-btn-xs${chat.saved ? ' is-on' : ''}`}
                  aria-pressed={!!chat.saved}
                  title={chat.saved ? 'Unsave chat' : 'Save chat'}
                  aria-label={chat.saved ? 'Unsave chat' : 'Save chat'}
                  onClick={e => {
                    e.stopPropagation();
                    onSaveChat?.(chat.id);
                  }}
                >
                  <svg width="13" height="13" viewBox="0 0 24 24" fill={chat.saved ? 'currentColor' : 'none'} stroke="currentColor" strokeWidth={1.8} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                    <path d="M6 4.5h12v16l-6-3.6-6 3.6v-16z" />
                  </svg>
                </button>

                {/* A waste bin. The only destructive one here, so it is the
                    only one that goes red. */}
                <button
                  type="button"
                  className="sidebar-icon-btn-xs sidebar-icon-btn-xs--danger"
                  title="Delete chat"
                  aria-label="Delete chat"
                  onClick={e => {
                    e.stopPropagation();
                    onDeleteChat?.(chat.id);
                  }}
                >
                  <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                    <path d="M4 6.5h16M9.5 6.5V4.8h5v1.7M6.5 6.5l.9 12.2a1.5 1.5 0 0 0 1.5 1.4h6.2a1.5 1.5 0 0 0 1.5-1.4l.9-12.2M10.3 10v6.5M13.7 10v6.5" />
                  </svg>
                </button>
              </span>
            </li>
          ))}
        </ul>
      )}

      {activeView === 'draft' && (spaces.length > 0 || recentDrafts.length > 0) && (
        <ul className="sidebar-plain-list sidebar-context-list">
          {spaces.map(space => (
            <li key={`s${space.id}`}>
              <button
                type="button"
                className="sidebar-plain-row"
                onClick={() => onOpenSpace?.(space.id)}
                title={space.name}
              >
                <span className="sidebar-dot" aria-hidden="true" />
                <span className="sidebar-plain-label">{space.name}</span>
              </button>
            </li>
          ))}
          {recentDrafts.map(draft => (
            <li key={`d${draft.id}`}>
              <button
                type="button"
                className="sidebar-plain-row"
                onClick={() => onOpenRecentDraft?.(draft.id)}
                title={draft.title}
              >
                <span className="sidebar-dot" aria-hidden="true" />
                <span className="sidebar-plain-label">{draft.title}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="sidebar-spacer" />

      {/* Bottom of the rail: Help & Feedback above Settings, stacked in the
          icon column whether the sidebar is open or closed. */}
      <div className="sidebar-bottom">
        <button
          className="sidebar-bottom-btn sidebar-login-btn"
          type="button"
          onClick={onOpenAccount}
          aria-label={lockProfile ? lockProfile.username : 'Sign in'}
          title={lockProfile ? lockProfile.username : 'Sign in'}
        >
          <RailIcon name="login" />
          <RailLabel text={lockProfile ? lockProfile.username : 'Sign in'} />
        </button>
        <button className="sidebar-bottom-btn" type="button" onClick={onOpenSettings} aria-label="Settings" title="Settings">
          <RailIcon name="settings" />
          <RailLabel text="Settings" />
        </button>
      </div>
    </nav>
  );
};

// A second, never-rendered chat row used to follow here: `ChatItem`, a ~75-line
// component with its own pin / save / delete icons and its own hover-reveal
// behaviour, drawn from `hoveredChat` state that nothing set. Nothing ever
// imported or rendered it. The live row is the inline `<li className=
// "sidebar-row">` above, whose actions appear on hover OR keyboard focus --
// which is the reason to prefer it: ChatItem gated its actions on
// `isHovered` alone, so they were unreachable from the keyboard.
//
// Two designs for one row, one of them dead, is worse than either on its own:
// it leaves no way to tell which is canonical.
