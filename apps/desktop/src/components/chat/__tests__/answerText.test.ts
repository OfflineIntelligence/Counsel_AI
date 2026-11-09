import { describe, it, expect } from 'vitest'
import { stripAll, stripEmbeddedBlocks } from '../answerText'

/**
 * Mirrors the backend's `rag::echo_guard` tests: the frontend cut is kept in
 * step with it line for line, so the live stream (rendered long before
 * anything is saved) hides exactly what the persisted answer hides.
 */
const pdfBlock = [
  '[DOCUMENT EXCERPTS]',
  'Retrieved from the attached documents for this question. Cite the tag heading each excerpt.',
  '',
  '--- [IDMS.pdf, p.3, 4.2 Text, Style, and Image Recognition] ---',
  '4.2 Text, Style, and Image Recognition',
  'Beyond extracting raw text, the system understands text styles.',
  '',
  '--- [IDMS.pdf, pp.3-4, 4.3 Digitization] ---',
  '4.3 Digitization',
  'The system converts image-based documents.',
].join('\n')

describe('stripEmbeddedBlocks', () => {
  it('strips a mid-answer reprint and keeps surrounding prose', () => {
    const text = `What is the compensation structure?\n\n${pdfBlock}\n\nIn short, it runs offline.`
    expect(stripAll(text, [pdfBlock])).toBe(
      'What is the compensation structure?\n\nIn short, it runs offline.',
    )
  })

  it('strips a mid-answer reprint without blocks to hand', () => {
    const text = `What is the compensation structure?\n\n${pdfBlock}\n\nIn short, it runs offline.`
    expect(stripAll(text, [])).toBe(
      'What is the compensation structure?\n\nIn short, it runs offline.',
    )
  })

  it('leaves a heading with no tag alone', () => {
    const a = 'I checked the [DOCUMENT EXCERPTS] idea and it does not apply.\n\nHere is why.'
    expect(stripEmbeddedBlocks(a, [pdfBlock])).toBe(a)
  })

  it('leaves an ordinary answer alone', () => {
    const a = 'The system recognises headings and bold text [IDMS.pdf, p.3].'
    expect(stripAll(a, [pdfBlock])).toBe(a)
  })
})
