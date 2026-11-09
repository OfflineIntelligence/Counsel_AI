/**
 * The app's own typing rhythm.
 *
 * A model streams at whatever speed it happens to run: a small one bursts,
 * a large one stutters, and switching models changed how the whole product
 * felt. This sits between the network and every surface that shows an
 * answer: tokens land in a buffer as fast as they arrive, and leave it whole
 * words at a time, at a pace we set.
 *
 * Three rules keep it fast and honest:
 * - It follows the engine: the display stays at most about LAG_SECONDS
 *   behind what has arrived, so a fast model is shown at the model's speed
 *   (measured: gemma-2-2b on an RTX 3050 Ti makes ~85 tokens, ~340
 *   characters, a second; the old fixed 45 characters a second added 5-9 s
 *   to every answer).
 * - When the answer has finished arriving, what is left is shown within
 *   LAG_SECONDS at a constant rate, not a rate that falls with the backlog.
 * - It cannot show what has not arrived. A model slower than the floor is
 *   shown at the model's speed.
 */

export const DISPLAY_PACE = {
    /** Lowest speed for a small backlog, in characters per second (about
     *  the measured engine speed), so the last words do not trickle. */
    charsPerSecond: 360,
    /** Longest the display trails the text that has arrived. */
    catchUpSeconds: 0.25,
    /** How often the display advances. ~30 updates a second is smooth to the
     *  eye without re-rendering markdown at frame rate. */
    tickMs: 33,
    /** A "word" longer than this (a URL, a run of dashes) is not held back
     *  waiting for the space that ends it. */
    maxHeldWord: 24,
} as const;

const now = () => (typeof performance !== 'undefined' ? performance.now() : Date.now());
const sleep = (ms: number) => new Promise<void>(resolve => setTimeout(resolve, ms));
const abortError = () => new DOMException('The operation was aborted.', 'AbortError');

/** Length of the next whole word plus the whitespace after it; 0 while that
 *  word may still be growing. */
function nextWordLength(text: string, finished: boolean): number {
    const match = /^\s*\S+\s+/.exec(text);
    if (match) return match[0].length;
    if (finished || text.length >= DISPLAY_PACE.maxHeldWord) return text.length;
    // Only whitespace so far: release it with the word that follows.
    return 0;
}

export async function* paceText(
    source: AsyncIterable<string>,
    signal?: AbortSignal,
): AsyncGenerator<string, void, unknown> {
    let pending = '';
    let finished = false;
    let failure: unknown = null;
    let wake: (() => void) | null = null;
    const notify = () => {
        wake?.();
        wake = null;
    };
    const waitForInput = () => new Promise<void>(resolve => (wake = resolve));

    signal?.addEventListener('abort', notify, { once: true });

    // Producer: drain the network as fast as it delivers.
    void (async () => {
        try {
            for await (const chunk of source) {
                pending += chunk;
                notify();
            }
        } catch (e) {
            failure = e;
        } finally {
            finished = true;
            notify();
        }
    })();

    let budget = 0;
    let last = now();
    // Constant rate for what is left once the answer has fully arrived.
    let drainRate = 0;
    try {
        while (true) {
            if (signal?.aborted) throw abortError();
            // A failure ends the answer at once: callers replace it with the
            // error, so finishing the reveal first would only delay that.
            if (failure) throw failure;

            if (nextWordLength(pending, finished) === 0) {
                if (finished) return;
                await waitForInput();
                // After a stall the first word shows at once, not a tick late.
                last = now();
                budget = Math.max(budget, nextWordLength(pending, finished));
                continue;
            }

            const t = now();
            if (finished && drainRate === 0) drainRate = pending.length / DISPLAY_PACE.catchUpSeconds;
            const rate = Math.max(
                DISPLAY_PACE.charsPerSecond,
                pending.length / DISPLAY_PACE.catchUpSeconds,
                drainRate,
            );
            // Capped at two ticks' worth so a long pause never banks a burst,
            // but never below the next word, or a long word could never be
            // afforded.
            const cap = Math.max(rate * (2 * DISPLAY_PACE.tickMs) / 1000, nextWordLength(pending, finished));
            budget = Math.min(budget + ((t - last) / 1000) * rate, cap);
            last = t;

            let out = '';
            for (let n = nextWordLength(pending, finished); n > 0 && budget >= n; n = nextWordLength(pending, finished)) {
                budget -= n;
                out += pending.slice(0, n);
                pending = pending.slice(n);
            }
            if (out) yield out;

            await sleep(DISPLAY_PACE.tickMs);
        }
    } finally {
        signal?.removeEventListener('abort', notify);
    }
}
