// "Take me to this source": one app-wide request, raised where a source is
// clicked (the sources card under an answer) and handled in App, which knows
// how to switch screens. Kept as a window event so the chat does not need a
// prop path through every layer to the app's navigation.

export interface OpenSourceRequest {
  documentId: number;
  filename: string;
  /** The cited passage, highlighted where the destination can show it. */
  highlight?: string;
  page?: number | null;
}

export const OPEN_SOURCE_EVENT = 'oca-open-source';

export function requestOpenSource(detail: OpenSourceRequest): void {
  window.dispatchEvent(new CustomEvent<OpenSourceRequest>(OPEN_SOURCE_EVENT, { detail }));
}

/** Lower-case words only, so "about-the-knowledge-library.txt" and
 *  "About the Knowledge Library" compare equal. */
export function nameKey(name: string): string {
  return name
    .replace(/\.[a-z0-9]{1,5}$/i, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, ' ')
    .trim();
}
