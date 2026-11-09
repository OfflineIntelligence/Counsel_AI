// The model-related dialogs: HuggingFace token, gated-model access, Ollama.
//
// ── Why this file exists ────────────────────────────────────────────────────
// These three were six modals built with `document.createElement` +
// `innerHTML` + `document.body.appendChild`, each with its own inline styles,
// its own hand-wired Escape listener, its own hover handlers assigning
// `style.backgroundColor` on mouseenter, and `background: white; color: black`
// with an explicit `data-theme="light"` on the card. In dark mode they were
// white cards on a dark app. None of them managed focus, none of them restored
// it, none of them animated, and two of them reached for `alert()` and
// `prompt()`.
//
// `components/ui/Dialog.tsx` already owns every one of those concerns, and
// owns them correctly. So the dialogs are React now, and all of the above is
// deleted rather than ported.
//
// The imperative `show*` entry points keep their exact signatures and publish
// a request on the bus in ./modelDialogBus; `<ModelDialogHost/>` is mounted
// once at the app root (App.tsx, beside NotificationsContainer) and renders
// it. So call sites do not change, the dialogs are ordinary React, and there
// is one host rather than six detached DOM subtrees that each had to remember
// to clean up after themselves. The bus module explains why it is a bus.

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { open } from '@tauri-apps/plugin-shell';
import { AlertTriangle, ExternalLink, RefreshCw, Server } from 'lucide-react';
import { Banner, Dialog, DialogActions } from '../ui';
import {
    subscribeModelDialogs,
    type ModelDialogEvent,
    type ModelDialogRequest,
} from './modelDialogBus';
import { setHfToken as storeHfToken } from '../../api/hfToken';
import { saveApiKey as persistApiKey } from '../../api/apiKeys';
import { useNotificationHelpers } from '../../contexts/NotificationContext';

/* ═══════════════════════════════════════════════════════════════════════════
   HUGGINGFACE TOKEN — two steps in one dialog
   ═══════════════════════════════════════════════════════════════════════════
   Previously two separate detached modals, where "I have a token" destroyed
   the first and built the second. One Dialog with a `step` is the same flow
   without the teardown: Escape, focus and the exit animation keep working
   across the step change, and going back is possible, which it was not.
   ─────────────────────────────────────────────────────────────────────────── */

const HfTokenDialog: React.FC<{
    open: boolean;
    onClose: () => void;
    req: Extract<ModelDialogRequest, { kind: 'hf-token' }>;
}> = ({ open: isOpen, onClose, req }) => {
    // No reset effect: the host keys this component on the request's serial
    // number, so every new request mounts a fresh one and these initialisers
    // ARE the reset. Resetting on close instead would visibly rewind the
    // dialog mid-exit, since it stays mounted through its exit animation.
    const [step, setStep] = useState<'intro' | 'input'>('intro');
    const [token, setToken] = useState('');
    const [invalid, setInvalid] = useState(false);
    const inputRef = useRef<HTMLInputElement>(null);
    const { showSuccess } = useNotificationHelpers();

    useEffect(() => {
        if (step === 'input') inputRef.current?.focus();
    }, [step]);

    const save = useCallback(() => {
        const t = token.trim();
        if (!t) {
            setInvalid(true);
            return;
        }
        storeHfToken(t);
        // Fire-and-forget into the encrypted backend store, as before.
        persistApiKey('huggingface', t).catch(console.error);
        req.setApiKey?.('huggingface', t);
        req.onHfTokenChange?.(t);
        showSuccess('Hugging Face token saved', 'Gated models can now be downloaded.');
        onClose();
        req.onComplete?.();
    }, [token, req, onClose, showSuccess]);

    return (
        <Dialog
            open={isOpen}
            onClose={onClose}
            size="sm"
            eyebrow={step === 'input' ? 'Step 2 of 2' : undefined}
            title={step === 'intro' ? 'HuggingFace token needed' : 'Paste your token'}
            subtitle={
                step === 'intro'
                    ? undefined
                    : 'Tokens begin with hf_. It is never sent anywhere except HuggingFace.'
            }
            footer={
                <DialogActions>
                    {step === 'intro' ? (
                        <>
                            <button
                                type="button"
                                className="ui-btn ui-btn--secondary"
                                onClick={() => {
                                    void open('https://huggingface.co/settings/tokens');
                                }}
                            >
                                <ExternalLink size={14} strokeWidth={1.75} />
                                Get a token
                            </button>
                            <button type="button" className="ui-btn ui-btn--primary" onClick={() => setStep('input')}>
                                I have a token
                            </button>
                        </>
                    ) : (
                        <>
                            <button type="button" className="ui-btn ui-btn--ghost" onClick={() => setStep('intro')}>
                                Back
                            </button>
                            <button
                                type="button"
                                className="ui-btn ui-btn--primary"
                                onClick={save}
                                disabled={!token.trim()}
                            >
                                Save token
                            </button>
                        </>
                    )}
                </DialogActions>
            }
        >
            {step === 'intro' ? null : (
                <input
                    ref={inputRef}
                    type="password"
                    className="ui-input"
                    // The browser's own credential manager has nothing useful to
                    // offer for an API token and gets in the way of pasting one.
                    autoComplete="off"
                    spellCheck={false}
                    placeholder="hf_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
                    aria-label="HuggingFace token"
                    aria-invalid={invalid || undefined}
                    value={token}
                    onChange={(e) => {
                        setToken(e.target.value);
                        setInvalid(false);
                    }}
                    onKeyDown={(e) => {
                        if (e.key === 'Enter') save();
                    }}
                />
            )}
        </Dialog>
    );
};

/* ═══════════════════════════════════════════════════════════════════════════
   GATED MODEL
   ═══════════════════════════════════════════════════════════════════════════ */

const GatedDialog: React.FC<{
    open: boolean;
    onClose: () => void;
    req: Extract<ModelDialogRequest, { kind: 'gated' }>;
}> = ({ open: isOpen, onClose, req }) => (
    <Dialog
        open={isOpen}
        onClose={onClose}
        size="sm"
        title="Model access required"
        subtitle="This model needs explicit approval from HuggingFace before it can be downloaded."
        footer={
            <DialogActions>
                <button
                    type="button"
                    className="ui-btn ui-btn--secondary"
                    onClick={() => {
                        void open(`https://huggingface.co/${req.repoId}`);
                    }}
                >
                    <ExternalLink size={14} strokeWidth={1.75} />
                    Visit model page
                </button>
                <button
                    type="button"
                    className="ui-btn ui-btn--primary"
                    onClick={() => {
                        onClose();
                        req.onRetry?.();
                    }}
                >
                    <RefreshCw size={14} strokeWidth={1.75} />
                    Retry download
                </button>
            </DialogActions>
        }
    >
        {/* The repo id used to be interpolated straight into an innerHTML
            string. It comes from a regex over a server error message, so it was
            also the one place in this app where remote text reached the DOM as
            markup. React escapes it. */}
        <Banner
            tone="warning"
            icon={<AlertTriangle size={14} strokeWidth={1.75} />}
            muted="Open the model page and click “Request access”, then retry."
        >
            <code>{req.repoId}</code>
        </Banner>
    </Dialog>
);

/* ═══════════════════════════════════════════════════════════════════════════
   OLLAMA
   ═══════════════════════════════════════════════════════════════════════════
   The server URL was collected with `prompt()`. It is a field now, which also
   means it can be validated before it is stored — the old flow accepted any
   non-empty string and only failed on the next app start.
   ─────────────────────────────────────────────────────────────────────────── */

const OLLAMA_URL_KEY = 'offline-intelligence-ollama-url';
const OLLAMA_DEFAULT_URL = 'http://localhost:11434';

const OllamaDialog: React.FC<{ open: boolean; onClose: () => void }> = ({ open: isOpen, onClose }) => {
    // Read once, in the initialiser, for the same reason as the HF dialog:
    // the host remounts this per request, so there is nothing to re-sync.
    const [url, setUrl] = useState(() => {
        try {
            return localStorage.getItem(OLLAMA_URL_KEY) || OLLAMA_DEFAULT_URL;
        } catch {
            // Storage can throw outright when site data is blocked.
            return OLLAMA_DEFAULT_URL;
        }
    });
    const [error, setError] = useState<string | null>(null);
    const { showSuccess } = useNotificationHelpers();

    const connect = useCallback(() => {
        const v = url.trim();
        let parsed: URL;
        try {
            parsed = new URL(v);
        } catch {
            setError('That is not a valid URL. It should look like http://localhost:11434');
            return;
        }
        if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
            setError('The URL must start with http:// or https://');
            return;
        }
        try {
            localStorage.setItem(OLLAMA_URL_KEY, v);
        } catch {
            setError('This computer is not allowing Counsel to save settings.');
            return;
        }
        showSuccess('Ollama server saved', 'Restart Counsel for the change to take effect.');
        onClose();
    }, [url, onClose, showSuccess]);

    return (
        <Dialog
            open={isOpen}
            onClose={onClose}
            size="sm"
            title="Ollama server required"
            subtitle="Point Counsel at your Ollama server to use the models it hosts."
            footer={
                <DialogActions>
                    <button
                        type="button"
                        className="ui-btn ui-btn--secondary"
                        onClick={() => {
                            void open('https://ollama.com/download');
                        }}
                    >
                        <ExternalLink size={14} strokeWidth={1.75} />
                        Setup guide
                    </button>
                    <button type="button" className="ui-btn ui-btn--primary" onClick={connect} disabled={!url.trim()}>
                        <Server size={14} strokeWidth={1.75} />
                        Connect
                    </button>
                </DialogActions>
            }
        >
            <input
                type="url"
                className="ui-input"
                inputMode="url"
                spellCheck={false}
                placeholder={OLLAMA_DEFAULT_URL}
                aria-label="Ollama server URL"
                aria-invalid={!!error || undefined}
                aria-describedby={error ? 'ollama-url-error' : undefined}
                value={url}
                onChange={(e) => {
                    setUrl(e.target.value);
                    setError(null);
                }}
                onKeyDown={(e) => {
                    if (e.key === 'Enter') connect();
                }}
            />
            {error && (
                <p id="ollama-url-error" className="ui-dialog__error" role="alert">
                    {error}
                </p>
            )}
        </Dialog>
    );
};

/* ═══════════════════════════════════════════════════════════════════════════
   THE HOST — mounted once, at the app root
   ═══════════════════════════════════════════════════════════════════════════ */

export const ModelDialogHost: React.FC = () => {
    const [event, setEvent] = useState<ModelDialogEvent | null>(null);
    const [isOpen, setIsOpen] = useState(false);

    useEffect(
        () =>
            subscribeModelDialogs((e) => {
                setEvent(e);
                setIsOpen(true);
            }),
        [],
    );

    // The event is held after close so the dialog still has content to render
    // while its exit animation plays; Dialog decides when to unmount itself.
    const close = useCallback(() => setIsOpen(false), []);

    if (!event) return null;
    const { id, req } = event;

    // `key={id}` is what makes each request a fresh dialog: none of these
    // needs a reset effect, because none of them is ever reused.
    if (req.kind === 'hf-token') {
        return <HfTokenDialog key={id} open={isOpen} onClose={close} req={req} />;
    }
    if (req.kind === 'gated') {
        return <GatedDialog key={id} open={isOpen} onClose={close} req={req} />;
    }
    return <OllamaDialog key={id} open={isOpen} onClose={close} />;
};

export default ModelDialogHost;
