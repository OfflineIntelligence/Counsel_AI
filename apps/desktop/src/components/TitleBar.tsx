/**
 * The window chrome for the undecorated window (src-tauri/src/main.rs).
 *
 * Two pieces, both fixed to the top of the window:
 *  - a drag strip across the page band (hidden on the chat page, whose own
 *    top bar is the drag handle there), and
 *  - the window lights, top-right on every page.
 *
 * The lights are three plain coloured dots — close, minimise, maximise — and
 * they are the same three dots at all times. They used to shrink to a third of
 * their size and grey out into a ⋯ at rest, blooming into colour on hover,
 * which was a nice trick and a bad control: the target moved, the colour that
 * identifies each button was absent exactly when you were looking for it, and
 * a row of dots that changes size as the pointer nears reads as decoration
 * rather than as the buttons that close your work.
 *
 * Nothing is drawn inside them. A × or a − at 12px is a smudge, and the
 * position and the colour already say which is which — that is the whole
 * convention these dots borrow. What each one does is said in words instead,
 * by the tooltip, which is the app's own and not the browser's.
 */

import { getCurrentWindow } from '@tauri-apps/api/window'

function inTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

type WindowAction = 'close' | 'minimize' | 'toggleMaximize'

/**
 * Left to right: Minimize (amber), Maximize (green), Close (red).
 *
 * `label` is both the accessible name and the tooltip's text. One string, so
 * the two can never say different things.
 */
const LIGHTS: Array<{ key: WindowAction; label: string }> = [
  { key: 'minimize', label: 'Minimize' },
  { key: 'toggleMaximize', label: 'Maximize' },
  { key: 'close', label: 'Close' },
]

export default function TitleBar() {
  const act = (action: WindowAction) => {
    if (!inTauri()) return
    void getCurrentWindow()[action]()
  }

  return (
    <>
      <div className="win-titlebar" data-tauri-drag-region />
      <div className="win-lights" role="group" aria-label="Window controls">
        {LIGHTS.map(l => (
          <button
            key={l.key}
            type="button"
            className={`win-light is-${l.key}`}
            aria-label={l.label}
            /* Deliberately NOT `title`. The browser's own tooltip cannot be
               styled at all — it arrives as a grey OS chip in a font nothing
               else here uses — so the label is carried in a data attribute and
               drawn by the stylesheet instead. See `.win-light::after`. */
            data-label={l.label}
            onClick={() => act(l.key)}
          />
        ))}
      </div>
    </>
  )
}
