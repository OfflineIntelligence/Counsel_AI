/** Shared constants and helpers for the drafting ribbon. */

import { createContext } from 'react'

/**
 * Document-level actions every ribbon shows beside its tabs — undo, redo,
 * History and Save — supplied once by the workspace rather than by each
 * editor.
 */
export interface RibbonActions {
  onSave: () => void
  onHistory: () => void
  historyOpen: boolean
  saving: boolean
  /**
   * Undo and redo, in the tab strip rather than the page header.
   *
   * They belong with the other document-level actions and, unlike a Home-tab
   * tool, they must stay reachable from Insert, Layout or a contextual tab —
   * the moment you most want to undo is right after the tool you just used.
   */
  onUndo: () => void
  onRedo: () => void
  canUndo: boolean
  canRedo: boolean
}

export const RibbonActionsContext = createContext<RibbonActions | null>(null)

// ------------------------------------------------------------------ constants

export const FONTS = [
  'Calibri',
  'Aptos',
  'Arial',
  'Helvetica',
  'Segoe UI',
  'Verdana',
  'Tahoma',
  'Times New Roman',
  'Georgia',
  'Garamond',
  'Cambria',
  'Book Antiqua',
  'Palatino Linotype',
  'Century Gothic',
  'Courier New',
  'Consolas',
]

export const SIZES = [8, 9, 10, 10.5, 11, 12, 14, 16, 18, 20, 24, 28, 32, 36, 40, 48, 54, 60, 72]

/** A Word-like palette: theme tints on top, standard colours beneath. */
export const PALETTE: string[][] = [
  ['000000', '404040', '595959', '7F7F7F', 'A6A6A6', 'BFBFBF', 'D9D9D9', 'F2F2F2', 'FFFFFF', '1F2A44'],
  ['C00000', 'FF0000', 'FFC000', 'FFFF00', '92D050', '00B050', '00B0F0', '0070C0', '002060', '7030A0'],
  ['F4CCCC', 'FCE5CD', 'FFF2CC', 'D9EAD3', 'D0E0E3', 'CFE2F3', 'D9D2E9', 'EAD1DC', 'E7E6E6', 'DEEAF6'],
]

/** The only highlight colours a Word document can store. */
export const HIGHLIGHTS: { name: string; hex: string }[] = [
  { name: 'yellow', hex: 'FFFF00' },
  { name: 'green', hex: '00FF00' },
  { name: 'cyan', hex: '00FFFF' },
  { name: 'magenta', hex: 'FF00FF' },
  { name: 'blue', hex: '0000FF' },
  { name: 'red', hex: 'FF0000' },
  { name: 'darkBlue', hex: '000080' },
  { name: 'darkCyan', hex: '008080' },
  { name: 'darkGreen', hex: '008000' },
  { name: 'darkMagenta', hex: '800080' },
  { name: 'darkRed', hex: '800000' },
  { name: 'darkYellow', hex: '808000' },
  { name: 'darkGray', hex: '808080' },
  { name: 'lightGray', hex: 'C0C0C0' },
  { name: 'black', hex: '000000' },
]

export const HIGHLIGHT_HEX: Record<string, string> = Object.fromEntries(HIGHLIGHTS.map(h => [h.name, h.hex]))

export const SYMBOLS: { group: string; items: string[] }[] = [
  { group: 'Legal', items: ['§', '¶', '©', '®', '™', '℠', '†', '‡', '№', '•', '…', '—', '–', '“', '”', '‘', '’', '«', '»'] },
  { group: 'Currency', items: ['€', '£', '¥', '₹', '¢', '₩', '₽', '₿'] },
  { group: 'Maths', items: ['±', '×', '÷', '≤', '≥', '≠', '≈', '≡', '∞', '√', '∑', '∏', '∫', '∂', '∆', '∇', '∈', '∉', '∩', '∪', '⊂', '⊃', '∀', '∃', '°', '‰', '½', '¼', '¾', '²', '³'] },
  { group: 'Greek', items: ['α', 'β', 'γ', 'δ', 'ε', 'θ', 'λ', 'μ', 'π', 'σ', 'τ', 'φ', 'ω', 'Γ', 'Δ', 'Θ', 'Λ', 'Π', 'Σ', 'Φ', 'Ω'] },
  { group: 'Arrows & marks', items: ['→', '←', '↑', '↓', '↔', '⇒', '⇔', '✓', '✗', '★', '☐', '☑', '■', '□', '▲', '►'] },
]

/** EMU per CSS pixel at 96 dpi. */
export const EMU_PER_PX = 9525



/** Read a picked image as base64 with its natural size in EMU. */
export function readImage(file: File): Promise<{ data_base64: string; width_emu: number; height_emu: number; file_name: string }> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onerror = () => reject(new Error(`"${file.name}" could not be read.`))
    reader.onload = () => {
      const url = String(reader.result)
      const img = new Image()
      img.onload = () =>
        resolve({
          data_base64: url.split(',')[1] ?? '',
          width_emu: Math.max(1, img.naturalWidth) * EMU_PER_PX,
          height_emu: Math.max(1, img.naturalHeight) * EMU_PER_PX,
          file_name: file.name,
        })
      img.onerror = () => reject(new Error(`"${file.name}" is not a picture this editor can place.`))
      img.src = url
    }
    reader.readAsDataURL(file)
  })
}

/** Open the OS picker for one image. */
export function pickImage(): Promise<File | null> {
  return new Promise(resolve => {
    const input = document.createElement('input')
    input.type = 'file'
    input.accept = 'image/png,image/jpeg,image/gif,image/bmp'
    input.onchange = () => resolve(input.files?.[0] ?? null)
    input.click()
  })
}

