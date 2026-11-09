// The "no model yet" prompt for the main chat: a small card at the composer's top-right with the download suggestions
// (ModelSetupPrompt, compact). It opens when a question could not be sent for
// lack of a model; outside click or Esc closes it. There is no pill: the
// model capsule inside the chat box shows the state in the meantime.

import { useEffect, useRef, useState } from 'react';
import ModelSetupPrompt from '../ModelSetupPrompt';
import type { Readiness } from '../../api/models';

export default function ModelChip({
  readiness,
  onOpenModels,
  onReady,
}: {
  readiness: Readiness;
  onOpenModels?: () => void;
  onReady: () => void;
}) {
  const [open, setOpen] = useState(true);
  const ref = useRef<HTMLDivElement | null>(null);

  // A new prompt (another question that could not be sent) shows it again.
  const [shownFor, setShownFor] = useState(readiness);
  if (shownFor !== readiness) {
    setShownFor(readiness);
    setOpen(true);
  }

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setOpen(false); };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  // No pill: while the card is closed, the model capsule inside the chat box
  // carries the state (no model / downloading / queued).
  if (!open) return null;
  return (
    <div ref={ref} className="model-chip is-open">
      <div className="src-card-pop model-chip__card" role="dialog" aria-label="Get a model">
        <div className="model-chip__body">
          <ModelSetupPrompt
            readiness={readiness}
            surface="chat"
            compact
            onOpenModels={() => {
              setOpen(false);
              onOpenModels?.();
            }}
            onReady={onReady}
          />
        </div>
      </div>
    </div>
  );
}
