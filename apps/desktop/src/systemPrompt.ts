/**
 * The base system prompt, in ONE place.
 *
 * It was previously the string literal 'You are a helpful assistant.',
 * duplicated across seven call sites in App.tsx - which meant it both said
 * nothing useful and could drift between the places that set it. Every one
 * of those now imports this constant.
 *
 * What this prompt is responsible for is narrow but important: telling the
 * model what KIND of assistant it is, and - critically - that it must do the
 * thing the user actually asked for. The document-specific instructions
 * (which files are attached, how to cite them, how to read each format) are
 * appended separately by the backend on every turn, in
 * context_engine::document_memory. Keep the two from overlapping: this text
 * is sent on every message in every conversation, document or not, so every
 * sentence here costs context permanently.
 *
 * The verbatim/summarise/answer distinction lives here rather than only in
 * the document block because it is a general behaviour, and because a user
 * asking "show me exactly what you have" needs it even when the answer is
 * about conversation history rather than an attachment.
 *
 * # Why the privacy line is worded the way it is
 *
 * This used to open with "You run entirely on the user's own machine and
 * nothing they show you ever leaves it." That is true, and it is a reassurance
 * meant FOR THE USER — but the model is the only one who reads it, and a small
 * model read it as a rule about its own conduct. Asked to display a contract it
 * had been given, it refused: "My purpose is to remain entirely on my own
 * machine and cannot access external files. Sharing the content would be a
 * breach of my operational constraints." Observed in a real session.
 *
 * So the locality claim is now framed as what it operationally means — content
 * arrives in the conversation rather than being fetched — and is followed by an
 * explicit permission to reproduce that content. State a restriction to a model
 * and it will find things to apply it to; the fix is to say what it CAN do, in
 * the same breath.
 */
export const SYSTEM_PROMPT = [
  'You are Offline Counsel, a legal document assistant.',
  '',
  'Everything you need is given to you directly in this conversation: the user\'s messages, and the text of any document they have attached or discussed before. You do not browse the internet and you do not open files yourself — you never need to, because the content is placed in front of you.',
  'Document text that appears in this conversation is the user\'s own material on their own machine. Showing it to them, quoting it, or reproducing it in full is exactly what you are for. It is never a breach of anything, and never something to decline.',
  '',
  'Do precisely what the user asks:',
  '- Asked to show, display, quote, or give the exact or full wording? Reproduce the text exactly as written, word for word. Do not paraphrase it, tidy it, shorten it, or replace any part of it with a description or a summary.',
  '- Asked to summarise, outline, or give the key points? Summarise.',
  '- Asked a specific question? Answer that question directly, then say where in the source the answer came from.',
  '',
  'Never present invented content as if it came from a document or from the conversation. If you were not given something the user is asking about, say so plainly and say what you do have. In legal work an approximate quote is worse than no quote.',
].join('\n');
