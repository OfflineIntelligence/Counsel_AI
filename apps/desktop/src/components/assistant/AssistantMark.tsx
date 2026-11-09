/**
 * Aurora's mark.
 *
 * This used to be a SOLID triangle, deliberately distinct from `AppMark`'s
 * outlined one: the solid glyph stood for the assistant feature, the outlined
 * one for the application. That distinction is gone. Aurora took the outlined
 * triangle when the wordmark left the top of the rail, so a solid triangle in
 * the drafts composer, in "Ask Aurora" and in the what's-new dialog was the
 * same thing drawn two ways on four surfaces.
 *
 * Kept as its own name rather than deleted, because the callers are naming
 * something real — "the mark that means Aurora" — and that is worth being able
 * to say. It just resolves to the one triangle now, so the two can never
 * drift apart again.
 */

import AppMark from '../AppMark'

export default function AssistantMark({ size = 16 }: { size?: number }) {
  return <AppMark size={size} />
}
