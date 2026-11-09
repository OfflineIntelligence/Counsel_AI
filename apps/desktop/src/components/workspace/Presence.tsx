/**
 * Shows its child with a fade-and-drop in, and keeps it on screen long enough
 * to fade back out when `open` turns false.
 *
 * Only opacity and transform move, through the Web Animations API, so the
 * browser runs it on the compositor with no layout or repaint per frame.
 */

import { cloneElement, useLayoutEffect, useRef, useState, type ReactElement, type Ref } from 'react'
import { ANIM_MS, SPRING_SMOOTH } from '../../styles/motion'

const DURATION = ANIM_MS
const EASE = SPRING_SMOOTH
const HIDDEN = { opacity: 0, transform: 'translateY(-6px) scale(0.98)' }
const SHOWN = { opacity: 1, transform: 'none' }

function reduceMotion(): boolean {
  return window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false
}

export default function Presence({
  open,
  children,
}: {
  open: boolean
  children: ReactElement<{ ref?: Ref<HTMLElement> }>
}) {
  const [mounted, setMounted] = useState(open)
  const node = useRef<HTMLElement | null>(null)

  if (open && !mounted) setMounted(true)

  useLayoutEffect(() => {
    const el = node.current
    if (!el) return
    if (reduceMotion()) {
      if (!open) setMounted(false)
      return
    }
    const from = el.getAnimations()[0]
    from?.cancel()
    const animation = el.animate(open ? [HIDDEN, SHOWN] : [SHOWN, HIDDEN], {
      duration: DURATION,
      easing: EASE,
      fill: 'forwards',
    })
    if (!open) animation.onfinish = () => setMounted(false)
    return () => {
      animation.onfinish = null
    }
  }, [open, mounted])

  if (!mounted) return null
  // While fading out it can no longer be clicked.
  return cloneElement(children, {
    ref: node as Ref<HTMLElement>,
    'aria-hidden': open ? undefined : true,
    style: open ? undefined : { pointerEvents: 'none' },
  } as Record<string, unknown>)
}
