import { describe, it, expect } from 'vitest';
import { paceText, DISPLAY_PACE } from '../pacedStream';

async function* burst(chunks: string[], gapMs = 0) {
  for (const c of chunks) {
    if (gapMs) await new Promise(r => setTimeout(r, gapMs));
    yield c;
  }
}

async function collect(source: AsyncIterable<string>, signal?: AbortSignal) {
  const out: string[] = [];
  for await (const piece of paceText(source, signal)) out.push(piece);
  return out;
}

describe('paceText', () => {
  it('shows exactly what the model sent, in whole words', async () => {
    const text = 'The quick brown fox jumps over the lazy dog.';
    const pieces = await collect(burst(['The qu', 'ick bro', 'wn fox jumps over the lazy dog.']));
    expect(pieces.join('')).toBe(text);
    // Every piece but the last ends on a word boundary: no half words.
    pieces.slice(0, -1).forEach(p => expect(p).toMatch(/\s$/));
  });

  it('smooths a burst over a few frames without holding it back', async () => {
    const text = 'word '.repeat(200); // 1,000 chars, delivered instantly
    const start = performance.now();
    const pieces = await collect(burst([text]));
    const elapsed = performance.now() - start;
    expect(pieces.join('')).toBe(text);
    expect(pieces.length).toBeGreaterThan(3);
    // A finished answer is fully shown within catchUpSeconds (plus timer slack).
    expect(elapsed).toBeLessThan(DISPLAY_PACE.catchUpSeconds * 1000 + 400);
  });

  it('keeps up with a model faster than the floor', async () => {
    // ~340 chars/s in 34-char chunks every 100 ms, like the measured engine.
    const chunks = Array.from({ length: 10 }, () => 'abcdef ghijk lmnop qrstu vwxyz 12 ');
    const start = performance.now();
    await collect(burst(chunks, 100));
    const elapsed = performance.now() - start;
    // Arrival takes ~1 s; the display may trail it by catchUpSeconds at most.
    expect(elapsed).toBeLessThan(1000 + DISPLAY_PACE.catchUpSeconds * 1000 + 400);
  });

  it('stops promptly when aborted', async () => {
    const controller = new AbortController();
    const run = collect(burst(['word '.repeat(200)]), controller.signal);
    setTimeout(() => controller.abort(), 100);
    await expect(run).rejects.toMatchObject({ name: 'AbortError' });
  });

  it('surfaces a failure from the stream', async () => {
    async function* broken() {
      yield 'Hello ';
      throw new Error('engine went away');
    }
    await expect(collect(broken())).rejects.toThrow('engine went away');
  });
});
