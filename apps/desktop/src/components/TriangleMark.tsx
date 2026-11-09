/**
 * The app's mark, in motion: the same outlined triangle as AppMark, used to
 * show what an answer is doing, so every state of an answer is drawn in
 * the logo's own shape.
 *
 * - thinking: a short light traces the outline,
 * - reading:  the three corners light in turn (sources are being read),
 * - done:     the outline fills solid once (the solid triangle is the
 *             assistant's mark) and settles back.
 *
 * Geometry and stroke are AppMark's, so the moving mark is the static one
 * animated, not a second drawing of it. Styles: App.css, "Triangle mark".
 */
export type TriangleState = 'thinking' | 'reading' | 'done'

const OUTLINE = 'M7 1.6 12.6 12H1.4L7 1.6Z'

export default function TriangleMark({ state, size = 18 }: { state: TriangleState; size?: number }) {
  return (
    <svg className={`tri-mark tri-mark--${state}`} width={size} height={size} viewBox="0 0 14 14" aria-hidden="true">
      <path className="tri-mark__base" d={OUTLINE} />
      {state === 'thinking' && <path className="tri-mark__trace" d={OUTLINE} pathLength={34} />}
      {state === 'reading' && (
        <>
          <circle className="tri-mark__corner" cx="7" cy="1.9" r="1.15" />
          <circle className="tri-mark__corner" cx="12.3" cy="11.7" r="1.15" />
          <circle className="tri-mark__corner" cx="1.7" cy="11.7" r="1.15" />
        </>
      )}
    </svg>
  )
}
