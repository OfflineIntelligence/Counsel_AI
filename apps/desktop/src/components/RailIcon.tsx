/**
 * The app's icon family: one set, drawn on a 24-unit grid with a single 1.5
 * stroke, round caps and joins, and no fills. Each glyph is centred and, where
 * the idea allows, mirror-symmetric, so a column of them reads as calm and
 * even. Colour comes from the text, through `currentColor`.
 *
 * These lived inside Sidebar.tsx, which was fine while the rail was the only
 * thing drawing them. It is not any more: the chat home offers "Add Knowledge
 * to your library" and "Add documents to your vault", and those cards have to
 * carry the SAME marks as the rail entries they lead to, or the two drift the
 * first time either is touched. One definition, imported by both.
 */

import React from 'react';

export type RailIconName =
  | 'discussion' | 'assistant' | 'workspace' | 'library' | 'vault' | 'models'
  | 'settings' | 'help' | 'login' | 'bell';

const RAIL_ICON_PATHS: Record<RailIconName, React.ReactNode> = {
  // A plain speech bubble: a conversation.
  discussion: <path d="M6 4h12a3 3 0 0 1 3 3v8a3 3 0 0 1-3 3h-6.2L7 21.2V18H6a3 3 0 0 1-3-3V7a3 3 0 0 1 3-3z" />,
  // The app's triangle, as in the window's title bar.
  assistant: <path d="M12 3.5 21 19.5H3L12 3.5z" />,
  // Four equal tiles.
  workspace: (
    <>
      <rect x="4" y="4" width="6.5" height="6.5" rx="1.5" />
      <rect x="13.5" y="4" width="6.5" height="6.5" rx="1.5" />
      <rect x="4" y="13.5" width="6.5" height="6.5" rx="1.5" />
      <rect x="13.5" y="13.5" width="6.5" height="6.5" rx="1.5" />
    </>
  ),
  // An open book: two pages mirrored about the spine.
  library: (
    <>
      <path d="M11.25 6.4C9.5 5.3 7.1 4.9 4 5.05V18c3.1-.15 5.5.25 7.25 1.3V6.4z" />
      <path d="M12.75 6.4c1.75-1.1 4.15-1.5 7.25-1.35V18c-3.1-.15-5.5.25-7.25 1.3V6.4z" />
    </>
  ),
  // A closed padlock.
  vault: (
    <>
      <rect x="5" y="10.5" width="14" height="10" rx="2" />
      <path className="rail-line" d="M8 10.5V8a4 4 0 0 1 8 0v2.5" />
    </>
  ),
  // An isometric cube, its edges cut into the fill.
  models: (
    <>
      <path d="M12 3l8 4.5v9L12 21l-8-4.5v-9L12 3z" />
      <path className="rail-cut" d="M4 7.5l8 4.5 8-4.5M12 12v9" />
    </>
  ),
  // The standard settings gear: eight rounded teeth around a centre hole.
  settings: (
    <path
      className="rail-solid"
      fillRule="evenodd"
      d="M19.14 12.94c.04-.3.06-.61.06-.94 0-.32-.02-.64-.07-.94l2.03-1.58a.49.49 0 0 0 .12-.61l-1.92-3.32a.49.49 0 0 0-.59-.22l-2.39.96c-.5-.38-1.03-.7-1.62-.94l-.36-2.54a.48.48 0 0 0-.48-.41h-3.84a.48.48 0 0 0-.47.41l-.36 2.54c-.59.24-1.13.57-1.62.94l-2.39-.96a.48.48 0 0 0-.59.22L2.74 8.87a.47.47 0 0 0 .12.61l2.03 1.58c-.05.3-.09.63-.09.94s.02.64.07.94l-2.03 1.58a.49.49 0 0 0-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.05.24.24.41.48.41h3.84c.24 0 .44-.17.47-.41l.36-2.54c.59-.24 1.13-.56 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32a.49.49 0 0 0-.12-.61l-2.01-1.58zM12 15.6a3.6 3.6 0 1 1 0-7.2 3.6 3.6 0 0 1 0 7.2z"
    />
  ),  // A filled disc with a question mark cut out.
  help: (
    <>
      <circle cx="12" cy="12" r="9" />
      <path className="rail-cut" d="M9.75 9.5a2.25 2.25 0 1 1 3.4 1.95c-.7.4-1.15.95-1.15 1.8v.25M12 16.5h.01" />
    </>
  ),
  // A person: head and shoulders.
  login: (
    <>
      <circle cx="12" cy="8.5" r="3.75" />
      <path d="M4.5 20.5c.8-3.9 3.7-6 7.5-6s6.7 2.1 7.5 6h-15z" />
    </>
  ),
  // A bell, mirrored about its centre line.
  bell: (
    <>
      <path d="M6.5 16.5V11a5.5 5.5 0 0 1 11 0v5.5l1.5 1.5H5l1.5-1.5z" />
      <path d="M10 20a2 2 0 0 0 4 0h-4z" />
    </>
  ),
};
export default function RailIcon({ name }: { name: RailIconName }) {
  return (
    <svg
      className="rail-icon"
      width="20"
      height="20"
      viewBox="0 0 24 24"
      fill="currentColor"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {RAIL_ICON_PATHS[name]}
    </svg>
  );
}
