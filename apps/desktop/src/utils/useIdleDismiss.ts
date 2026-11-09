import { useEffect, useRef, type RefObject } from 'react'

/** How long Aurora may sit untouched before it fades away. */
export const AGENT_IDLE_MS = 60_000

/**
 * Calls `onIdle` once `ms` pass with no interaction inside `ref`.
 *
 * Pointer, keyboard, wheel, focus and DOM changes (an answer streaming in)
 * all count as use. While `busy` is true the timer never fires.
 */
export function useIdleDismiss(
  ref: RefObject<HTMLElement | null>,
  active: boolean,
  busy: boolean,
  onIdle: () => void,
  ms: number = AGENT_IDLE_MS,
) {
  const onIdleRef = useRef(onIdle)
  useEffect(() => {
    onIdleRef.current = onIdle
  })

  useEffect(() => {
    if (!active || busy) return
    const el = ref.current
    let timer = window.setTimeout(() => onIdleRef.current(), ms)
    const reset = () => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => onIdleRef.current(), ms)
    }
    const events = ['pointerdown', 'pointermove', 'keydown', 'wheel', 'focusin', 'input'] as const
    events.forEach(e => el?.addEventListener(e, reset, { passive: true }))
    const observer = el ? new MutationObserver(reset) : null
    if (el && observer) observer.observe(el, { childList: true, subtree: true, characterData: true })
    return () => {
      window.clearTimeout(timer)
      events.forEach(e => el?.removeEventListener(e, reset))
      observer?.disconnect()
    }
  }, [ref, active, busy, ms])
}
