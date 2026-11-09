/**
 * The app's mark: an OUTLINED triangle.
 *
 * One definition, shared by the window title bar and the empty-chat heading,
 * so the two can never drift apart. It is deliberately not `AssistantMark` —
 * that glyph is the SOLID triangle the sidebar and the assistant panel use to
 * stand for the AI Assistant feature - that name now resolves here too, so
 * this is the one triangle in the app.
 *
 * Scaling is uniform: the stroke rides the viewBox, so the mark at heading
 * size is the title bar's mark enlarged, not a heavier drawing of it.
 */
export default function AppMark({ size = 14, className }: { size?: number; className?: string }) {
  return (
    <svg className={className} width={size} height={size} viewBox="0 0 14 14" aria-hidden="true">
      <path
        d="M7 1.6 12.6 12H1.4L7 1.6Z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.1"
        strokeLinejoin="round"
      />
    </svg>
  )
}
