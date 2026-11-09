/**
 * What a not-yet-arrived answer looks like: the app's triangle, in motion,
 * beside a shimmering status word, set in the same size as the streaming
 * prose around it (no font-size of its own — it inherits).
 *
 * One component for every surface (chat, agent search, panel, drafts) so the
 * loading state reads as one product, not four approximations.
 *
 * The word names the phase the answer is in. A caller that knows the phase
 * passes it; on its own, a long wait moves the word on so a slow model never
 * looks stuck. Each change cross-fades rather than swapping.
 */
import { useEffect, useState } from 'react';
import TriangleMark from './TriangleMark';

export type ThinkingPhase = 'thinking' | 'reading';

const PHASE_LABEL: Record<ThinkingPhase, string> = {
  thinking: 'Thinking',
  reading: 'Reading your sources',
};

/** How long each word holds before a silent wait moves it on. */
const PATIENCE: { after: number; label: string }[] = [
  { after: 8000, label: 'Still thinking' },
  { after: 20000, label: 'Taking a closer look' },
];

/** Animations are held while the app is in the background (the flag App.tsx
 *  keeps on <html>), so a hidden window is not drawing frames. */
function useAppActive(): boolean {
  const read = () => document.documentElement.dataset.appActive !== 'false';
  const [active, setActive] = useState(read);
  useEffect(() => {
    const observer = new MutationObserver(() => setActive(read()));
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-app-active'] });
    return () => observer.disconnect();
  }, []);
  return active;
}

/** The given label, moved on by PATIENCE while it stays unchanged. */
function usePatientLabel(label: string): string {
  const [waited, setWaited] = useState({ label, steps: 0 });
  // A new phase starts its patience over (adjusted during render, not in an
  // effect, so the old word never shows for a frame).
  if (waited.label !== label) setWaited({ label, steps: 0 });
  useEffect(() => {
    const timers = PATIENCE.map((step, i) =>
      setTimeout(() => setWaited(w => (w.label === label ? { label, steps: i + 1 } : w)), step.after),
    );
    return () => timers.forEach(clearTimeout);
  }, [label]);
  return waited.steps > 0 && waited.label === label ? PATIENCE[waited.steps - 1].label : label;
}

export default function ThinkingStatus({ phase = 'thinking' }: { phase?: ThinkingPhase }) {
  const active = useAppActive();
  const shown = usePatientLabel(PHASE_LABEL[phase]);
  return (
    <span className={`thinking-status${active ? '' : ' is-paused'}`} role="status" aria-label={shown}>
      <TriangleMark state={phase} size={18} />
      <span key={shown} className="thinking-status__text" aria-hidden="true">
        {shown}…
      </span>
    </span>
  );
}
