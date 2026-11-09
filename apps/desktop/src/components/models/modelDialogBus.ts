// The request bus behind the model dialogs.
//
// Split out of ModelDialogs.tsx rather than living beside the components,
// because a module that exports both components and plain functions defeats
// Fast Refresh for the whole file — an edit to one of these functions would
// force a full reload instead of a hot swap.
//
// ── Why a bus at all ────────────────────────────────────────────────────────
// `showHuggingFaceApiKeyModal` is an imperative function called from three
// places, one of them SettingsPanel — a sibling of ModelsPanel, not a child,
// and often the only one of the two mounted. Lifting the dialog's open state
// would mean threading it through App.tsx into both panels and keeping them in
// step. Publishing a request instead keeps those call sites exactly as they
// were while the dialogs themselves become ordinary React.

export type ModelDialogRequest =
    | {
        kind: 'hf-token';
        onHfTokenChange?: (token: string) => void;
        setApiKey?: (provider: 'huggingface', key: string) => void;
        onComplete?: () => void;
    }
    | { kind: 'gated'; repoId: string; onRetry?: () => void }
    | { kind: 'ollama' };

/** What the host receives: the request plus a serial number. The serial is the
 *  host's React `key`, so each new request mounts a fresh dialog with fresh
 *  state instead of the previous one having to reset itself. */
export interface ModelDialogEvent {
    id: number;
    req: ModelDialogRequest;
}

type Listener = (e: ModelDialogEvent) => void;

// A set, not a single slot: under StrictMode the host mounts, unmounts and
// remounts, and a lone slot written by the first mount would be overwritten by
// the second and then cleared by the first one's cleanup.
const listeners = new Set<Listener>();

// Queued while no host is mounted yet. The first call can land during the
// app's first paint, and dropping it silently is a cold-start-only bug.
let pending: ModelDialogEvent | null = null;
let serial = 0;

function publish(req: ModelDialogRequest) {
    const e: ModelDialogEvent = { id: ++serial, req };
    if (listeners.size === 0) {
        pending = e;
        return;
    }
    listeners.forEach((l) => l(e));
}

/** Subscribe. Returns an unsubscribe. Replays a request that arrived before
 *  any host existed. */
export function subscribeModelDialogs(listener: Listener): () => void {
    listeners.add(listener);
    if (pending) {
        const p = pending;
        pending = null;
        listener(p);
    }
    return () => {
        listeners.delete(listener);
    };
}

/**
 * Ask for the HuggingFace token dialog. Signature unchanged from the
 * imperative version this replaces, so its three call sites are untouched.
 */
export function showHuggingFaceApiKeyModal(
    onHfTokenChange?: (token: string) => void,
    setApiKey?: (provider: 'huggingface', key: string) => void,
    onComplete?: () => void,
) {
    publish({ kind: 'hf-token', onHfTokenChange, setApiKey, onComplete });
}

/** Ask for the gated-model dialog. Signature unchanged. */
export function showGatedModelModal(repoId: string, onRetry?: () => void) {
    publish({ kind: 'gated', repoId, onRetry });
}

/** Ask for the Ollama server dialog. */
export function showOllamaModal() {
    publish({ kind: 'ollama' });
}
