/**
 * Shown in place of an answer when no model can answer yet — in the chat and
 * in Aurora alike.
 *
 * - No model downloaded: the top three models recommended for this machine,
 *   each one click to download, plus "Browse models" with more of them and a
 *   way into the full Models page. The backend activates the download when it
 *   finishes, so downloading is the only step.
 * - Downloaded but none loaded: the installed models, one click to load.
 * - Loading: says so, and waits.
 *
 * While a download or load is under way it polls the backend (every 2s, only
 * while something is in flight) and calls `onReady` the moment a model can
 * answer, so the question the user typed can simply be sent again.
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { ArrowDown, ChevronDown, Download } from 'lucide-react';

import { getApiBaseSync } from '../api/backendUrl';
import { refreshDownloads } from '../api/modelSync';
import {
    MODELS_CHANGED_EVENT,
    activateModel,
    canAsk,
    formatBytes,
    getReadiness,
    getRecommendedModels,
    installModel,
    type CatalogModel,
    type Readiness,
} from '../api/models';

const TOP = 3;
const MORE = 7;
const POLL_MS = 2000;

interface DownloadRow {
    /** Tracked under the backend's sanitized id, so rows are matched by the
     *  name the install request sent, which is carried through unchanged. */
    model_name: string;
    status: string;
    percentage: number;
}

const ACTIVE = new Set(['Queued', 'Starting', 'Downloading']);

interface Props {
    readiness: Readiness;
    /** Where the prompt is shown; tunes the wording only. */
    surface: 'chat' | 'agent';
    onOpenModels?: () => void;
    /** A model is loaded and answering is possible. */
    onReady: () => void;
    /** The small card beside the composer: "Browse models" goes straight to
     *  the Models page instead of opening its list, and the download control
     *  is an arrow glyph. */
    compact?: boolean;
}

export default function ModelSetupPrompt({ readiness: initial, surface, onOpenModels, onReady, compact = false }: Props) {
    const [readiness, setReadiness] = useState<Readiness>(initial);
    const [recommended, setRecommended] = useState<CatalogModel[]>([]);
    const [installed, setInstalled] = useState<CatalogModel[]>([]);
    const [downloads, setDownloads] = useState<Record<string, DownloadRow>>({});
    const [browseOpen, setBrowseOpen] = useState(false);
    const [message, setMessage] = useState<string | null>(null);
    const [busy, setBusy] = useState<string | null>(null);
    /** A download or load was started here; keep watching until a model is
     *  ready, across the gap between a download finishing and the backend
     *  starting to load it. */
    const [awaiting, setAwaiting] = useState(false);
    const browseRef = useRef<HTMLDivElement | null>(null);

    useEffect(() => setReadiness(initial), [initial]);

    // What to offer: recommendations always (Browse needs them too), the
    // installed list only when there is something to load.
    useEffect(() => {
        let live = true;
        getRecommendedModels(TOP + MORE)
            .then(list => live && setRecommended(list))
            .catch(() => live && setRecommended([]));
        if (readiness.state === 'not_loaded') {
            fetch(`${getApiBaseSync()}/models`)
                .then(r => (r.ok ? r.json() : []))
                .then((all: CatalogModel[]) => live && setInstalled(all.filter(m => m.status === 'Installed')))
                .catch(() => live && setInstalled([]));
        }
        return () => {
            live = false;
        };
    }, [readiness.state]);

    const refresh = useCallback(async () => {
        try {
            const [next, dl] = await Promise.all([
                getReadiness(),
                refreshDownloads(),
            ]);
            const rows: Record<string, DownloadRow> = {};
            for (const d of dl as DownloadRow[]) rows[d.model_name] = d;
            setDownloads(rows);
            setReadiness(next);
            if (canAsk(next)) onReady();
        } catch {
            /* The backend is briefly busy (a model is loading); the next tick retries. */
        }
    }, [onReady]);

    // Poll only while something is actually happening.
    const inFlight =
        readiness.state === 'loading' ||
        (awaiting && !canAsk(readiness)) ||
        Object.values(downloads).some(d => ACTIVE.has(d.status)) ||
        busy !== null;
    useEffect(() => {
        if (!inFlight) return;
        const id = setInterval(refresh, POLL_MS);
        return () => clearInterval(id);
    }, [inFlight, refresh]);

    useEffect(() => {
        const onChanged = () => void refresh();
        window.addEventListener(MODELS_CHANGED_EVENT, onChanged);
        return () => window.removeEventListener(MODELS_CHANGED_EVENT, onChanged);
    }, [refresh]);

    // Close the Browse menu on an outside click.
    useEffect(() => {
        if (!browseOpen) return;
        const onDown = (e: MouseEvent) => {
            if (browseRef.current && !browseRef.current.contains(e.target as Node)) setBrowseOpen(false);
        };
        document.addEventListener('mousedown', onDown);
        return () => document.removeEventListener('mousedown', onDown);
    }, [browseOpen]);

    const download = async (model: CatalogModel) => {
        setMessage(null);
        setBusy(model.id);
        try {
            const result = await installModel(model);
            if (result.ok) {
                setAwaiting(true);
                await refresh();
            } else if (result.needsModelsPage) {
                setMessage(`${result.message} Opening Models…`);
                onOpenModels?.();
            } else {
                setMessage(result.message);
            }
        } catch {
            setMessage('Could not start the download. Is the app backend running?');
        } finally {
            setBusy(null);
        }
    };

    const activate = async (model: CatalogModel) => {
        setMessage(null);
        setBusy(model.id);
        try {
            const result = await activateModel(model.id);
            if (result.ok) setAwaiting(true);
            else setMessage(result.message ?? 'Could not load the model.');
            await refresh();
        } finally {
            setBusy(null);
        }
    };

    const downloadLabel = (model: CatalogModel): string => {
        const d = downloads[model.name];
        if (d && ACTIVE.has(d.status)) {
            return `${Math.round(d.percentage || 0)}%`;
        }
        return busy === model.id ? 'Starting…' : 'Download';
    };
    const isDownloading = (model: CatalogModel) => {
        const d = downloads[model.name];
        return busy === model.id || (!!d && ACTIVE.has(d.status));
    };

    const what = surface === 'agent' ? 'LARA' : 'Counsel';

    if (readiness.state === 'loading' || readiness.state === 'restarting') {
        return (
            <section className="model-setup" aria-live="polite">
                <h3 className="model-setup__title">
                    {readiness.state === 'loading'
                        ? `Loading ${readiness.loading_model ?? 'your model'}…`
                        : 'The model engine is restarting…'}
                </h3>
                <p className="model-setup__lead">Your question will be ready to send in a moment.</p>
            </section>
        );
    }

    const top = recommended.slice(0, TOP);
    const more = recommended.slice(TOP);

    return (
        <section className="model-setup" aria-live="polite">
            <h3 className="model-setup__title">
                {readiness.state === 'not_loaded' ? 'Load a model to continue' : 'Download a model to get started'}
            </h3>
            <p className="model-setup__lead">
                {readiness.state === 'not_loaded'
                    ? `You have ${readiness.installed} model${readiness.installed === 1 ? '' : 's'} downloaded, but none is loaded. Load one and ${what} can answer.`
                    : `${what} runs entirely on this computer. Pick a model recommended for your hardware; it loads automatically when the download finishes.`}
            </p>

            {readiness.state === 'not_loaded' && installed.length > 0 && (
                <ul className="model-setup__list">
                    {installed.slice(0, TOP).map(m => (
                        <li key={m.id} className="model-setup__card">
                            <div className="model-setup__info">
                                <span className="model-setup__name">{m.name}</span>
                                <span className="model-setup__meta">
                                    {[m.parameters, formatBytes(m.size_bytes)].filter(Boolean).join(' · ')}
                                </span>
                            </div>
                            <button
                                type="button"
                                className="model-setup__action"
                                disabled={busy !== null}
                                onClick={() => activate(m)}
                            >
                                {busy === m.id ? 'Loading…' : 'Load'}
                            </button>
                        </li>
                    ))}
                </ul>
            )}

            {readiness.state === 'none_installed' && (
                <ul className="model-setup__list">
                    {top.length === 0 && <li className="model-setup__empty">Finding models for your hardware…</li>}
                    {top.map((m, i) => (
                        <li key={m.id} className="model-setup__card">
                            <div className="model-setup__info">
                                <span className="model-setup__name">
                                    {m.name}
                                    {i === 0 && <span className="model-setup__badge">Best fit</span>}
                                </span>
                                <span className="model-setup__meta">
                                    {[m.parameters, formatBytes(m.size_bytes), m.author].filter(Boolean).join(' · ')}
                                </span>
                            </div>
                            <button
                                type="button"
                                className="model-setup__action"
                                disabled={isDownloading(m)}
                                onClick={() => download(m)}
                            >
                                {!isDownloading(m) && (compact
                                    ? <ArrowDown size={14} strokeWidth={2.25} aria-hidden="true" />
                                    : <Download size={14} aria-hidden="true" />)}
                                {downloadLabel(m)}
                            </button>
                        </li>
                    ))}
                </ul>
            )}

            <div className="model-setup__footer">
                <div className="model-setup__browse" ref={browseRef}>
                    <button
                        type="button"
                        className="model-setup__browse-btn"
                        aria-haspopup="menu"
                        aria-expanded={browseOpen}
                        onClick={() => (compact ? onOpenModels?.() : setBrowseOpen(o => !o))}
                    >
                        Browse models
                        {!compact && <ChevronDown size={14} aria-hidden="true" className={browseOpen ? 'is-open' : undefined} />}
                    </button>
                    {browseOpen && (
                        <div className="model-setup__menu" role="menu">
                            {more.map(m => (
                                <button
                                    key={m.id}
                                    type="button"
                                    role="menuitem"
                                    className="model-setup__menu-item"
                                    disabled={isDownloading(m)}
                                    onClick={() => download(m)}
                                >
                                    <span className="model-setup__menu-name">{m.name}</span>
                                    <span className="model-setup__meta">
                                        {isDownloading(m)
                                            ? downloadLabel(m)
                                            : [m.parameters, formatBytes(m.size_bytes)].filter(Boolean).join(' · ')}
                                    </span>
                                </button>
                            ))}
                            <button
                                type="button"
                                role="menuitem"
                                className="model-setup__menu-item model-setup__menu-item--all"
                                onClick={() => {
                                    setBrowseOpen(false);
                                    onOpenModels?.();
                                }}
                            >
                                View all models
                            </button>
                        </div>
                    )}
                </div>
                {message && <p className="model-setup__message">{message}</p>}
            </div>
        </section>
    );
}
