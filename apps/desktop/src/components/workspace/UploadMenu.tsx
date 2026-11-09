/**
 * The Upload button and its menu, shared by the Workspace and the Vault.
 *
 * The menu opens beside the files box, in the empty space to its right, with
 * its first row level with the button — the same placement as the list ⋮
 * menu. It is pinned to the window so the scrolling box cannot clip it.
 */

import { useEffect, useRef, useState } from 'react'
import { Upload } from 'lucide-react'

interface Props {
  onAddFiles: () => void
  onAddFolder: () => void
  /** Shown as 'Create Folder' when given. */
  onCreateFolder?: () => void
  disabled?: boolean
  className?: string
}

export default function UploadMenu({ onAddFiles, onAddFolder, onCreateFolder, disabled, className = '' }: Props) {
  const [at, setAt] = useState<{ top: number; left: number } | null>(null)
  const anchor = useRef<HTMLDivElement | null>(null)
  const open = at !== null

  useEffect(() => {
    if (!open) return
    const close = () => setAt(null)
    const onPointer = (e: MouseEvent) => {
      if (!anchor.current?.contains(e.target as Node)) close()
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close()
    }
    document.addEventListener('mousedown', onPointer)
    document.addEventListener('keydown', onKey)
    document.addEventListener('scroll', close, true)
    window.addEventListener('resize', close)
    return () => {
      document.removeEventListener('mousedown', onPointer)
      document.removeEventListener('keydown', onKey)
      document.removeEventListener('scroll', close, true)
      window.removeEventListener('resize', close)
    }
  }, [open])

  const toggle = () => {
    if (open || !anchor.current) {
      setAt(null)
      return
    }
    const button = anchor.current.getBoundingClientRect()
    const box = anchor.current.closest('.ws-files-box')?.getBoundingClientRect()
    const menuWidth = 220
    const left = Math.min((box?.right ?? button.right) + 8, window.innerWidth - menuWidth - 8)
    setAt({ top: button.top, left })
  }

  const choose = (action: () => void) => {
    setAt(null)
    action()
  }

  return (
    <div className="ws-upload-menu" ref={anchor}>
      <button
        type="button"
        className={`ws-pill ws-create-folder ${className}`}
        onClick={toggle}
        disabled={disabled}
        aria-haspopup="menu"
        aria-expanded={open}
        title="Upload"
      >
        <Upload size={16} strokeWidth={1.75} />
        Upload
      </button>
      {open && (
        <div
          className="ws-filter-menu ws-actions-menu ws-actions-menu--fixed ws-upload-menu__list"
          style={{ top: at.top, left: at.left }}
          role="menu"
        >
          <button type="button" role="menuitem" className="ws-filter-option" onClick={() => choose(onAddFiles)}>
            Add Files or Photos
          </button>
          <button type="button" role="menuitem" className="ws-filter-option" onClick={() => choose(onAddFolder)}>
            Add Folder
          </button>
          {onCreateFolder && (
            <button type="button" role="menuitem" className="ws-filter-option" onClick={() => choose(onCreateFolder)}>
              Create Folder
            </button>
          )}
          <div className="ws-menu-divider" role="separator" />
          <button
            type="button"
            role="menuitem"
            className="ws-filter-option ws-upload-menu__soon"
            aria-disabled="true"
            title="Slash commands for legal work are coming soon"
            onClick={() => setAt(null)}
          >
            Slash commands
            <span className="ws-soon">Soon</span>
          </button>
        </div>
      )}
    </div>
  )
}
