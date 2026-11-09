/**
 * The Vault, browsed from inside the Workspace files box.
 *
 * Shown in place of the workspace grid while "From Vault" mode is on. It only
 * lists and selects; copying the selection into the workspace is the parent's
 * job, because the parent knows which workspace folder the user is in.
 */

import { useEffect, useMemo, useState } from 'react'
import { Check, CornerLeftUp } from 'lucide-react'

import { getApiBaseSync } from '../../api/backendUrl'
import { fetchWithNetworkRetry } from '../../api/fetchWithTimeout'
import { isEditableFormat, MAX_EDITABLE_BYTES } from '../../editableFormats'
import { fileTypeName } from './FileBrowser'
import { FileBrandTile, FolderGlyph, FILE_ICON } from '../FileBrandIcon'

export interface VaultEntry {
  id: number
  name: string
  isDirectory: boolean
  size: number
  modified: string
  children?: VaultEntry[]
}

/** Whether a vault file can become a workspace draft at all. */
export function canImport(entry: VaultEntry): boolean {
  return !entry.isDirectory && isEditableFormat(entry.name) && entry.size <= MAX_EDITABLE_BYTES
}

/** The vault's document id for a stored file, created on demand by the backend. */
export async function resolveVaultDocument(localFileId: number): Promise<number> {
  const r = await fetchWithNetworkRetry(`${getApiBaseSync()}/documents/by-local-file/${localFileId}`)
  if (!r.ok) throw new Error((await r.text()) || `The Vault file could not be read (${r.status}).`)
  const doc = (await r.json()) as { id: number }
  return doc.id
}

function extensionOf(name: string): string {
  const ext = name.split('.').pop()?.toLowerCase() ?? ''
  return ext && ext !== name.toLowerCase() ? ext : 'file'
}

function formatSize(bytes: number): string {
  if (!bytes) return '-'
  const units = ['B', 'KB', 'MB', 'GB']
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  return `${parseFloat((bytes / 1024 ** i).toFixed(1))} ${units[i]}`
}

function longDate(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return '-'
  return date.toLocaleDateString('en-US', { month: 'long', day: 'numeric', year: 'numeric' })
}

function findFolder(entries: VaultEntry[], id: number): VaultEntry | null {
  for (const entry of entries) {
    if (entry.id === id && entry.isDirectory) return entry
    const hit = findFolder(entry.children ?? [], id)
    if (hit) return hit
  }
  return null
}

interface Props {
  view: 'grid' | 'list'
  selected: Map<number, VaultEntry>
  onToggle: (entry: VaultEntry) => void
}

/**
 * The last Vault listing, kept between visits so "From Vault" can show files
 * immediately instead of pausing on a loading message mid-animation.
 */
let cachedTree: VaultEntry[] | null = null
let inflight: Promise<VaultEntry[]> | null = null

async function fetchVaultTree(): Promise<VaultEntry[]> {
  const r = await fetchWithNetworkRetry(`${getApiBaseSync()}/files`)
  if (!r.ok) throw new Error(`The Vault could not be listed (${r.status}).`)
  const body = await r.json()
  return Array.isArray(body) ? (body as VaultEntry[]) : []
}

/** Load (or refresh) the Vault listing in the background. */
export function preloadVault(): Promise<VaultEntry[]> {
  if (!inflight) {
    inflight = fetchVaultTree()
      .then(tree => {
        cachedTree = tree
        return tree
      })
      .finally(() => {
        inflight = null
      })
  }
  return inflight
}

export default function VaultPane({ view, selected, onToggle }: Props) {
  const [tree, setTree] = useState<VaultEntry[]>(cachedTree ?? [])
  const [loading, setLoading] = useState(cachedTree === null)
  const [error, setError] = useState<string | null>(null)
  const [path, setPath] = useState<number[]>([])

  useEffect(() => {
    let cancelled = false
    // Always refresh, but quietly: with a cached listing already on screen,
    // new results replace it in place rather than through a loading state.
    preloadVault()
      .then(next => {
        if (!cancelled) setTree(next)
      })
      .catch(e => {
        if (!cancelled && cachedTree === null) setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  const current = path.length ? findFolder(tree, path[path.length - 1]) : null
  const entries = useMemo(() => {
    const list = current ? (current.children ?? []) : tree
    return [...list].sort((a, b) =>
      a.isDirectory === b.isDirectory
        ? a.name.localeCompare(b.name, undefined, { sensitivity: 'base' })
        : a.isDirectory
          ? -1
          : 1,
    )
  }, [current, tree])

  const openFolder = (entry: VaultEntry) => setPath(p => [...p, entry.id])
  const goUp = () => setPath(p => p.slice(0, -1))

  // No message while a first-ever load is in flight: an empty box that the
  // files fade into reads as one movement, a message then files reads as two.
  if (loading) return <div className="ws-vault-state" aria-busy="true" />

  if (error) return <div className="ws-vault-state ws-mode-in">{error}</div>
  if (tree.length === 0) return <div className="ws-vault-state ws-mode-in">No files or folders inside Vault</div>

  const check = (entry: VaultEntry, disabled: boolean) => {
    const on = selected.has(entry.id)
    return (
      <button
        type="button"
        className={`ws-check${on ? ' ws-check-on' : ''}`}
        aria-pressed={on}
        aria-label={`Select ${entry.name}`}
        disabled={disabled}
        title={disabled ? 'This file type cannot be added to the Workspace' : `Select ${entry.name}`}
        onClick={e => {
          e.stopPropagation()
          onToggle(entry)
        }}
      >
        {on && <Check size={12} strokeWidth={3} />}
      </button>
    )
  }

  const activate = (entry: VaultEntry) => {
    if (entry.isDirectory) openFolder(entry)
    else if (canImport(entry)) onToggle(entry)
  }

  if (view === 'grid') {
    return (
      <div className="ws-grid-items ws-tiles ws-mode-in" key={`g${path.join('-')}`}>
        {path.length > 0 && (
          <button type="button" className="ws-tile ws-tile-up" onClick={goUp} title="Up one level">
            <span className="ws-tile-icon">
              <CornerLeftUp size={20} strokeWidth={1.75} />
            </span>
            <span className="ws-tile-text">
              <span className="ws-tile-name">Up one level</span>
            </span>
          </button>
        )}
        {entries.map(entry => {
          const disabled = !entry.isDirectory && !canImport(entry)
          return (
            <div
              key={entry.id}
              className={`ws-tile${selected.has(entry.id) ? ' ws-tile-selected' : ''}${disabled ? ' ws-tile-disabled' : ''}`}
            >
              <div className="ws-tile-top">
                {check(entry, disabled)}
                <span className="ws-dots-placeholder" aria-hidden="true" />
              </div>
              <button
                type="button"
                className="ws-tile-open"
                onClick={() => activate(entry)}
                title={entry.isDirectory ? `Open ${entry.name}` : entry.name}
              >
                {entry.isDirectory ? (
                  <span className="ws-tile-icon ws-icon-folder">
                    <FolderGlyph size={FILE_ICON.grid} />
                  </span>
                ) : (
                  <span className="ws-tile-icon ws-tile-icon-file">
                    <FileBrandTile format={extensionOf(entry.name)} size={FILE_ICON.grid} />
                  </span>
                )}
                <span className="ws-tile-text">
                  <span className="ws-tile-name">{entry.name}</span>
                  <span className="ws-tile-meta">
                    {entry.isDirectory ? 'Folder' : `${extensionOf(entry.name).toUpperCase()} · ${formatSize(entry.size)}`}
                  </span>
                </span>
              </button>
            </div>
          )
        })}
      </div>
    )
  }

  return (
    <div className="ws-file-list ws-mode-in" role="table" aria-label="Vault" key={`l${path.join('-')}`}>
      <div className="ws-list-head" role="row">
        <span role="columnheader" />
        <span role="columnheader">Name</span>
        <span role="columnheader">Size</span>
        <span role="columnheader">Type</span>
        <span role="columnheader">Last modified</span>
        <span role="columnheader" />
      </div>
      <div className="ws-list-body">
        {path.length > 0 && (
          <div role="row" className="ws-list-row ws-list-up" onClick={goUp}>
            <span />
            <span className="ws-list-name">
              <CornerLeftUp size={16} strokeWidth={1.75} className="ws-list-icon" />
              Up one level
            </span>
            <span />
            <span />
            <span />
            <span />
          </div>
        )}
        {entries.map(entry => {
          const disabled = !entry.isDirectory && !canImport(entry)
          return (
            <div
              key={entry.id}
              role="row"
              className={`ws-list-row${selected.has(entry.id) ? ' ws-list-selected' : ''}${disabled ? ' ws-list-disabled' : ''}`}
            >
              <span>{check(entry, disabled)}</span>
              <button type="button" className="ws-list-name" onClick={() => activate(entry)} title={entry.name}>
                {entry.isDirectory ? (
                  <span className="ws-list-icon ws-icon-folder"><FolderGlyph size={FILE_ICON.list} /></span>
                ) : (
                  <span className="ws-list-icon ws-list-icon-file">
                    <FileBrandTile format={extensionOf(entry.name)} size={FILE_ICON.list} />
                  </span>
                )}
                <span className="ws-list-title">{entry.name}</span>
              </button>
              <span className="ws-list-cell">{entry.isDirectory ? '-' : formatSize(entry.size)}</span>
              <span className="ws-list-cell">{entry.isDirectory ? 'Folder' : fileTypeName(extensionOf(entry.name))}</span>
              <span className="ws-list-cell">{longDate(entry.modified)}</span>
              <span />
            </div>
          )
        })}
      </div>
    </div>
  )
}
