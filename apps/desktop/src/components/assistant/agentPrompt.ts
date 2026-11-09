/**
 * The Aurora's system prompt, shared by the panel and the drafts composer so
 * both ask in the same voice with the same budget.
 */
import type { AssistantPage } from '../../contexts/AssistantContext'

/** Enough page context to be useful without blowing a small model's window.
 *  The Context view reports this exact budget — a panel that claims a
 *  different limit than the prompt builder uses is worse than silence. */
export const CONTEXT_CHARS = 12_000

export function systemPrompt(page: AssistantPage): string {
  const base =
    'You are Counsel, a careful legal assistant inside an offline legal workspace. ' +
    'Answer concisely and plainly. When asked to write or rewrite text, reply with only that text, no preamble.'
  const where = `The user is on the ${page.label} page${page.title ? `, looking at "${page.title}"` : ''}${page.format ? ` (${page.format})` : ''}.`
  if (!page.text) return `${base}

${where}`
  const text = page.text.length > CONTEXT_CHARS ? `${page.text.slice(0, CONTEXT_CHARS)}
[…truncated]` : page.text
  return `${base}

${where}

--- CONTENT ---
${text}`
}
