/**
 * Plain-text readings of a draft's view model.
 *
 * The side column needs the document as words — to count them, to outline
 * them, and to hand them to the assistant as context. Each format keeps its
 * text in a different shape, so the flattening lives here once rather than in
 * every panel that needs it.
 */

import type { DocxBlock, DraftViewModel } from '../../api/drafts'

export function documentText(model: DraftViewModel | null): string {
  if (!model) return ''
  switch (model.format) {
    case 'txt':
      return model.content
    case 'docx':
      return model.blocks.map(b => b.runs.map(r => r.text).join('')).join('\n')
    case 'xlsx':
      return model.sheets
        .map(
          sheet =>
            `# ${sheet.name}\n` +
            Object.entries(sheet.cells)
              .map(([ref, cell]) => `${ref}: ${cell.formula ? `=${cell.formula} → ` : ''}${cell.value}`)
              .join('\n'),
        )
        .join('\n\n')
    case 'pptx':
      return model.slides
        .map(
          slide =>
            `# Slide ${slide.index + 1}\n` +
            slide.shapes.flatMap(s => s.paragraphs.map(p => p.text)).filter(Boolean).join('\n'),
        )
        .join('\n\n')
    default:
      return ''
  }
}

/** A user bookmark in a PDF is a note annotation whose text starts with this. */
export const BOOKMARK_PREFIX = 'Bookmark: '

export type SideTab =
  | 'outline'
  | 'clauses'
  | 'cell'
  | 'slide'
  | 'details'
  | 'history'
  | 'pages'
  | 'bookmarks'
  | 'annotations'

/** Which side-column tabs a format offers, in the order they appear. */
export function tabsFor(format: string): SideTab[] {
  switch (format) {
    case 'docx':
      return ['outline', 'clauses', 'details']
    case 'txt':
      return ['clauses', 'details']
    case 'xlsx':
      return ['cell', 'details']
    case 'pptx':
      return ['slide', 'details']
    case 'pdf':
      return ['pages', 'bookmarks', 'annotations', 'details']
    default:
      return ['details']
  }
}

export interface XlsxSelection {
  sheet: number
  ref: string
}

export interface PptxSelection {
  slide: number
  shape: string | null
}

export interface TextStats {
  words: number
  characters: number
  paragraphs: number
  readingMinutes: number
}

export function textStats(text: string): TextStats {
  const words = text.trim() ? text.trim().split(/\s+/).length : 0
  return {
    words,
    characters: text.length,
    paragraphs: text.split(/\n\s*\n|\n/).filter(p => p.trim()).length,
    // 230 wpm is the usual figure for careful reading of dense prose.
    readingMinutes: Math.max(1, Math.round(words / 230)),
  }
}

export interface OutlineEntry {
  addr: string
  text: string
  level: number
}

/** Headings in document order, with their level taken from the style id. */
export function docxOutline(blocks: DocxBlock[]): OutlineEntry[] {
  return blocks
    .filter(b => !b.cell && /^(heading|title)/i.test(b.style ?? ''))
    .map(b => {
      const style = b.style ?? ''
      const level = /^title/i.test(style) ? 0 : Number(/(\d+)/.exec(style)?.[1] ?? 1)
      return { addr: b.addr, text: b.runs.map(r => r.text).join('').trim(), level }
    })
    .filter(e => e.text)
}
