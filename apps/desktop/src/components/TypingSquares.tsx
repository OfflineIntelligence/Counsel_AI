/**
 * Small coloured squares that pop in and fade out across the chat window
 * while the user types in the composer. It sits behind the chat's content, so
 * cards, the composer and the title cover it; the squares show only on the
 * open background. Listens for the `oca-composer-typing` window event.
 */
import { useEffect, useRef } from 'react'

const COLORS = ['#ff5f57', '#febc2e', '#28c840', '#0a84ff', '#bf5af2', '#ff9f0a', '#64d2ff', '#ff375f']

export default function TypingSquares() {
  const layer = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (window.matchMedia?.('(prefers-reduced-motion: reduce)').matches) return
    const onType = () => {
      const host = layer.current
      if (!host) return
      const count = 2 + Math.floor(Math.random() * 3)
      for (let i = 0; i < count; i++) {
        if (host.childElementCount > 80) return
        const sq = document.createElement('span')
        const size = 5 + Math.random() * 9
        sq.className = 'typing-square'
        sq.style.left = `${Math.random() * 100}%`
        sq.style.top = `${Math.random() * 100}%`
        sq.style.width = `${size}px`
        sq.style.height = `${size}px`
        sq.style.background = COLORS[Math.floor(Math.random() * COLORS.length)]
        sq.style.animationDuration = `${700 + Math.random() * 900}ms`
        sq.addEventListener('animationend', () => sq.remove())
        host.appendChild(sq)
      }
    }
    window.addEventListener('oca-composer-typing', onType)
    return () => window.removeEventListener('oca-composer-typing', onType)
  }, [])

  return <div ref={layer} className="typing-squares" aria-hidden="true" />
}
