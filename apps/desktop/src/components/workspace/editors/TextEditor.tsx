/**
 * Plain text editor.
 *
 * The editor always works in `\n`; the file's real encoding, BOM and line
 * endings are detected on open and restored on save by the backend. They are
 * shown in the status strip rather than hidden, because "this is a
 * Windows-1252, CRLF file" is exactly the kind of thing that matters when the
 * text came out of a case management system and has to go back into one.
 */

import { useEffect, useRef, useState } from 'react'

import type { DraftPatch } from '../../../api/drafts'

interface Props {
  content: string
  encoding: string
  lineEnding: string
  bom: boolean
  onPatch: (...patches: DraftPatch[]) => void
}

export default function TextEditor({ content, encoding, lineEnding, bom, onPatch }: Props) {
  const [value, setValue] = useState(content)
  const dirty = useRef(false)

  // Adopt content from the server ONLY when the user is not mid-edit. A save
  // returns a refreshed model, and overwriting the textarea with it would undo
  // whatever was typed while the request was in flight.
  useEffect(() => {
    if (!dirty.current) setValue(content)
  }, [content])

  const lines = value === '' ? 0 : value.split('\n').length

  return (
    <div className="ws-text">
      <textarea
        className="ws-textarea"
        value={value}
        spellCheck={false}
        onChange={e => {
          dirty.current = true
          setValue(e.target.value)
          onPatch({ op: 'SetText', content: e.target.value })
        }}
        onBlur={() => {
          dirty.current = false
        }}
      />
      <div className="ws-text-status">
        <span>{encoding}</span>
        <span>{lineEnding} line endings</span>
        {bom && <span>BOM</span>}
        <span>
          {lines} line{lines === 1 ? '' : 's'}
        </span>
        <span>{value.length} characters</span>
      </div>
    </div>
  )
}
