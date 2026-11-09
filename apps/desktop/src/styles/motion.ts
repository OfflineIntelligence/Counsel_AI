// JavaScript side of the motion tokens in tokens.css.
//
// Anything that waits for a CSS animation to finish (unmounting after an
// exit, a Web Animations API call) reads its duration and curve from here, so
// the two can only change together. Keep these in step with --dur-anim and
// --spring-* in tokens.css.

/** Must equal --dur-anim. */
export const ANIM_MS = 420;

/** Critically damped spring; same samples as --spring-smooth. */
export const SPRING_SMOOTH =
  'linear(0, 0.03 2%, 0.12 5%, 0.33 11%, 0.56 18%, 0.74 26%, 0.86 34%, 0.93 43%, 0.97 53%, 0.99 66%, 1)';

/** ~4% overshoot; same samples as --spring-bouncy. */
export const SPRING_BOUNCY =
  'linear(0, 0.02 1.5%, 0.09 3.5%, 0.3 8%, 0.58 14%, 0.8 20%, 0.93 26%, 1 32%, 1.035 38%, 1.045 44%, 1.04 50%, 1.025 58%, 1.01 68%, 1.002 80%, 1)';

export function prefersReducedMotion(): boolean {
  return typeof window !== 'undefined' &&
    (window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false);
}
