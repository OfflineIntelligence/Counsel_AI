/**
 * Removing prompt scaffolding that a model printed straight back.
 *
 * Retrieval folds blocks into the user's message so the model can read them:
 * document excerpts, matter material, earlier conversations, workspace
 * matches. A capable model reads them and answers. A small one often prints
 * them back first, so the reply opens with `[DOCUMENT EXCERPTS]`, a guidance
 * line written for the model and every `--- [file, p.3, ...] ---` tag before
 * any answer at all — the user is handed back the prompt.
 *
 * This is the same cut the backend makes in `rag::echo_guard`, kept in step
 * with it line for line, and it exists here for the two cases the backend
 * cannot reach: the LIVE stream, rendered token by token long before anything
 * is saved, and chats already stored with a reprint in them.
 *
 * `stripScaffold` is the exact pass. It is given the blocks that were actually
 * folded in — the backend sends them ahead of the answer — and removes leading
 * lines that are literally those lines. It knows nothing about documents,
 * pages, slides or sheets, so a PDF, a Word file, a deck, a workbook and a text
 * file are all handled the same way, as are the blocks that have no document in
 * them at all.
 *
 * `stripReprintedHeadings` is the fallback for when no blocks arrived, which is
 * every chat saved before this existed.
 *
 * `stripEmbeddedBlocks` removes reprinted blocks ANYWHERE in the answer, not
 * just at its start: a small model sometimes restates the question first and
 * echoes the context after it. A span is excised only when it opens with a
 * block heading AND contains at least one `--- [...] ---` tag, so a quoted
 * sentence or a mere mention of a heading is never touched.
 *
 * Both only cut from the START, and only while every line so far is
 * scaffolding. An answer that mentions a document, or quotes a line from one
 * and then says something about it, keeps everything from its first real
 * sentence on.
 */

/**
 * Every heading the backend puts at the top of a folded block.
 *
 * Matched as PREFIXES: three carry a trailing clause explaining why the block
 * was retrieved, and that wording is not worth pinning here.
 */
const BLOCK_HEADINGS = [
    '[DOCUMENT EXCERPTS]',
    "[POSSIBLY RELEVANT MATERIAL FROM THIS MATTER'S DOCUMENTS]",
    '[MATERIAL FROM EARLIER',
    '[YOUR EARLIER CONVERSATIONS',
    '[Workspace matches for this question]',
    '[Note: no attached document is relevant',
    '[EARLIER IN THIS CONVERSATION',
    '[CONVERSATION SUMMARY',
    '[POSSIBLY RELATED FROM YOUR OTHER CONVERSATIONS',
];

const isHeading = (line: string) => {
    const t = line.trim();
    return BLOCK_HEADINGS.some(h => t.startsWith(h));
};

/**
 * One excerpt's tag line, e.g. `--- [SPA.pdf, p.47, Section 12.3] ---` or, for
 * a document with other versions in scope,
 * `--- [SPA.pdf, p.47] (v2, uploaded 3 May) ---`.
 *
 * The tail is NOT required to be `] ---`: the version label goes after the
 * closing bracket, so testing for that missed every versioned document.
 */
const isTagLine = (line: string) => {
    const t = line.trim();
    return t.startsWith('--- [') && t.endsWith('---') && t.length > 8;
};

const FORMATTER_LINES = [
    '[excerpt shortened to fit]',
    '[Further retrieved context omitted to fit the context window.]',
];

const isFormatterLine = (line: string) => FORMATTER_LINES.includes(line.trim());

/** Remove a reprint of the blocks that were folded into this turn. */
export function stripScaffold(answer: string, blocks: string[]): string {
    const trimmed = answer.trimStart();
    const lines = trimmed.split('\n');
    if (lines.length === 0 || !isHeading(lines[0])) return answer;

    // Every non-blank line the model was shown. A set, not a substring search:
    // a line either was in the prompt or it was not, and "block contains line"
    // would let one long block swallow real prose that appeared inside it.
    const scaffold = new Set<string>();
    for (const block of blocks) {
        for (const line of block.split('\n')) {
            const t = line.trim();
            if (t) scaffold.add(t);
        }
    }

    let i = 0;
    while (i < lines.length) {
        const t = lines[i].trim();
        if (!t || isHeading(t) || isTagLine(t) || isFormatterLine(t) || scaffold.has(t)) {
            i += 1;
            continue;
        }
        break;
    }

    // Consuming the heading and nothing else is not a cut, it is damage: it
    // means there was nothing to match against, or the model paraphrased rather
    // than copied. Returning the answer whole leaves the heading in place for
    // the fallback, which recognises a reprint BY that heading.
    if (i <= 1) return answer;

    return lines.slice(i).join('\n').trim();
}

/** Remove a reprint using only the headings the backend writes. */export function stripReprintedHeadings(answer: string): string {
    const trimmed = answer.trimStart();
    const lines = trimmed.split('\n');
    if (lines.length === 0 || !isHeading(lines[0])) return answer;

    let lastTag = -1;
    lines.forEach((l, i) => {
        if (isTagLine(l)) lastTag = i;
    });

    let start: number;
    if (lastTag === -1) {
        // A heading with no excerpts under it — the earlier-conversations and
        // workspace blocks, and a mid-stream reprint caught before its first
        // tag has arrived. Such a block is its heading plus its guidance, which
        // run to the first blank line.
        const blank = lines.findIndex(l => !l.trim());
        start = blank === -1 ? lines.length : blank + 1;
    } else {
        let i = lastTag + 1;
        while (i < lines.length) {
            if (!lines[i].trim()) {
                const next = lines.slice(i + 1).find(l => l.trim());
                // A blank line followed by another excerpt or block: the run
                // continues.
                if (!(next && (isTagLine(next) || isHeading(next)))) {
                    i += 1;
                    break;
                }
            }
            i += 1;
        }
        start = i;
    }

    return lines.slice(Math.min(start, lines.length)).join('\n').trim();
}

/** Both passes: the exact one, then the headings one for anything it left. */
export function stripAll(answer: string, blocks: string[] = []): string {
    return stripEmbeddedBlocks(stripReprintedHeadings(stripScaffold(answer, blocks)), blocks);
}

/**
 * End index (exclusive) of the reprinted span opening at heading `h`, or
 * null when the heading is not followed by a tag — prose that merely
 * mentions a block name.
 */
function embeddedSpanEnd(lines: string[], h: number, scaffold: Set<string>): number | null {
    let j = h + 1;
    let seenTag = false;
    // Guidance lines of unknown wording, only spendable with no blocks to
    // check against. Every real block carries one or two; a heading followed
    // by prose and no tag soon after is not a reprint.
    let guidanceLeft = 2;
    while (j < lines.length) {
        const t = lines[j].trim();
        if (!t || isFormatterLine(t)) {
            j += 1;
            continue;
        }
        if (isHeading(t)) break;
        if (isTagLine(t)) {
            seenTag = true;
            j += 1;
            break;
        }
        if (scaffold.has(t)) {
            j += 1;
            continue;
        }
        if (scaffold.size === 0 && guidanceLeft > 0) {
            guidanceLeft -= 1;
            j += 1;
            continue;
        }
        break;
    }
    if (!seenTag) return null;
    for (;;) {
        if (j >= lines.length) break;
        const t = lines[j].trim();
        if (isTagLine(t) || isFormatterLine(t) || scaffold.has(t)) {
            j += 1;
            continue;
        }
        if (isHeading(t)) break;
        if (!t) {
            if (scaffold.size === 0) {
                // No blocks to match against: a blank ends the body unless
                // another excerpt or block follows it.
                const next = lines.slice(j + 1).find(l => l.trim());
                if (next && (isTagLine(next) || isHeading(next))) {
                    j += 1;
                    continue;
                }
                break;
            }
            j += 1;
            continue;
        }
        if (scaffold.size === 0) {
            j += 1;
            continue;
        }
        break;
    }
    return j;
}

/**
 * Remove reprinted excerpt blocks ANYWHERE in the answer. Kept in step with
 * the backend's `rag::echo_guard::strip_embedded_blocks`, line for line.
 */
export function stripEmbeddedBlocks(answer: string, blocks: string[] = []): string {
    const scaffold = new Set<string>();
    for (const block of blocks) {
        for (const line of block.split('\n')) {
            const t = line.trim();
            if (t) scaffold.add(t);
        }
    }
    const lines = answer.split('\n');
    const out: string[] = [];
    let i = 0;
    let cutAny = false;
    while (i < lines.length) {
        if (isHeading(lines[i])) {
            const end = embeddedSpanEnd(lines, i, scaffold);
            if (end !== null) {
                cutAny = true;
                i = end;
                continue;
            }
        }
        out.push(lines[i]);
        i += 1;
    }
    if (!cutAny) return answer;
    // Excising a span can stack three or more newlines where a block sat
    // between paragraphs. Squeeze those back to one blank line.
    const collapsed: string[] = [];
    let blanks = 0;
    for (const line of out.join('\n').split('\n')) {
        if (!line.trim()) {
            blanks += 1;
            if (blanks <= 1) collapsed.push('');
        } else {
            blanks = 0;
            collapsed.push(line);
        }
    }
    return collapsed.join('\n').trim();
}
