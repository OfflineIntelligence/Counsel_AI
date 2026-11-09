/**
 * Choose one or more folders and read every file inside them.
 *
 * The browser's folder input (`webkitdirectory`) only allows ONE folder per
 * pick. Tauri's dialog allows several, and grants read access to what was
 * picked; the files are then read and returned as `File` objects whose
 * `webkitRelativePath` is set to "Folder/sub/file.pdf", exactly the shape the
 * browser input produces, so the existing folder-upload code handles them.
 *
 * Returns `null` if the dialog was cancelled or is unavailable (not running in
 * Tauri), so callers can fall back to the browser input.
 */

import { open } from '@tauri-apps/plugin-dialog'
import { readDir, readFile } from '@tauri-apps/plugin-fs'
import { basename, join } from '@tauri-apps/api/path'

function inTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

async function walk(dir: string, relative: string, out: File[]): Promise<void> {
  const entries = await readDir(dir)
  for (const entry of entries) {
    const path = await join(dir, entry.name)
    const rel = `${relative}/${entry.name}`
    if (entry.isDirectory) {
      await walk(path, rel, out)
    } else if (entry.isFile) {
      const bytes = await readFile(path)
      const file = new File([bytes], entry.name)
      Object.defineProperty(file, 'webkitRelativePath', { value: rel })
      out.push(file)
    }
  }
}

export async function pickFolders(): Promise<File[] | null> {
  if (!inTauri()) return null
  const picked = await open({ directory: true, multiple: true, recursive: true, title: 'Choose folders' })
  if (!picked) return []
  const roots = Array.isArray(picked) ? picked : [picked]
  const files: File[] = []
  for (const root of roots) {
    await walk(root, await basename(root), files)
  }
  return files
}
