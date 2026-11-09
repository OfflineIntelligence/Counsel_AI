/**
 * "On Computer | On Cloud": where every answer in the app comes from.
 *
 * One switch for the whole app. It is held by the backend (api/cloud.ts), and
 * every model call there — chat, agents, drafting, document intelligence —
 * goes through it, so they always agree.
 *
 * The cloud panel has two tabs:
 *   Models — the menu by model maker (Anthropic, OpenAI, Google, Meta,
 *            NVIDIA, DeepSeek…), each reachable through whichever connected
 *            key can serve it (api/cloudCatalog.ts).
 *   Keys   — connect a provider with your own API key; a connected
 *            provider's full live model list is here too.
 *
 * What leaves the machine is the panel's first line, not a footnote.
 */
import React, { useEffect, useMemo, useRef, useState } from 'react';
import { open as openExternal } from '@tauri-apps/plugin-shell';
import { Monitor, Cloud, ChevronDown, Check, ExternalLink, Search } from 'lucide-react';
import {
    PROVIDER_INFO,
    connectProvider,
    disconnectProvider,
    listModels,
    listProviders,
    useAnswerSource,
    type CloudProvider,
} from '../../api/cloud';
import { CATALOG, type CatalogModel } from '../../api/cloudCatalog';
import { useModelSelection } from '../../contexts/ModelSelectionContext';
import { getRecommendedModels, installModel, formatBytes, type CatalogModel as LocalCatalogModel } from '../../api/models';
import { useDownloads } from '../../api/modelSync';
import { getHfToken, setHfToken, clearHfToken, HF_TOKEN_EVENT } from '../../api/hfToken';
import { saveApiKey as persistApiKey } from '../../api/apiKeys';
import { useAuth } from '../../contexts/AuthContext';

/** Room for a pop-up above the toggle without covering the chat home's
 *  cards: from the cards' bottom edge to the toggle, less a margin. With no
 *  cards on screen (a conversation is open) the panel keeps its own cap. */
function useRoomAbove(anchor: React.RefObject<HTMLElement | null>, open: boolean): number | undefined {
    const [room, setRoom] = useState<number | undefined>(undefined);
    useEffect(() => {
        if (!open) return;
        const measure = () => {
            const top = anchor.current?.getBoundingClientRect().top;
            const cards = document.querySelector('.home__list')?.getBoundingClientRect();
            setRoom(top !== undefined && cards ? Math.min(260, Math.max(140, Math.floor(top - cards.bottom - 16))) : undefined);
        };
        measure();
        window.addEventListener('resize', measure);
        return () => window.removeEventListener('resize', measure);
    }, [anchor, open]);
    return room;
}

export default function AnswerSourceToggle() {
    const [source, setSource] = useAnswerSource();
    // Which pop-up is open: the local models (On Computer) or the cloud panel.
    const [panel, setPanel] = useState<'local' | 'cloud' | null>(null);
    const wrap = useRef<HTMLDivElement>(null);
    const room = useRoomAbove(wrap, panel !== null);

    // The model capsule beside Send opens this same pop-up (see ModelCapsule).
    useEffect(() => {
        const onOpen = (e: Event) => {
            const mode = (e as CustomEvent<'local' | 'cloud'>).detail;
            setPanel(p => (p === mode ? null : mode));
        };
        window.addEventListener('oca-open-source-panel', onOpen);
        return () => window.removeEventListener('oca-open-source-panel', onOpen);
    }, []);

    // Tell the model capsule which pop-up is showing, so it shows the same side.
    useEffect(() => {
        window.dispatchEvent(new CustomEvent('oca-source-panel-changed', { detail: panel }));
    }, [panel]);
    useEffect(() => () => {
        window.dispatchEvent(new CustomEvent('oca-source-panel-changed', { detail: null }));
    }, []);

    useEffect(() => {
        if (!panel) return;
        const onDown = (e: MouseEvent) => {
            const t = e.target as Element;
            if (!wrap.current?.contains(t) && !t.closest?.('.model-capsule')) setPanel(null);
        };
        const onKey = (e: KeyboardEvent) => e.key === 'Escape' && setPanel(null);
        document.addEventListener('mousedown', onDown);
        document.addEventListener('keydown', onKey);
        return () => {
            document.removeEventListener('mousedown', onDown);
            document.removeEventListener('keydown', onKey);
        };
    }, [panel]);

    // Both labels always show; the lit one follows the open pop-up, else the
    // backend's answer source.
    const cloud = panel ? panel === 'cloud' : source.mode === 'cloud';
    return (
        <div ref={wrap} className="answer-source">
            <div className="answer-source__seg" role="radiogroup" aria-label="Where answers come from">
                <button
                    type="button"
                    role="radio"
                    aria-checked={!cloud}
                    aria-label="On Computer"
                    className={`answer-source__opt${!cloud ? ' is-on' : ''}`}
                    onClick={() => {
                        setSource({ mode: 'local' });
                        setPanel(p => (p === 'local' ? null : 'local'));
                    }}
                    title="Answer with a model downloaded to this computer. Nothing leaves your machine."
                >
                    <Monitor size={13} strokeWidth={1.75} />
                    On Computer
                    {!cloud && <ChevronDown size={12} strokeWidth={2} />}
                </button>
                <button
                    type="button"
                    role="radio"
                    aria-checked={cloud}
                    aria-label="On Cloud"
                    className={`answer-source__opt${cloud ? ' is-on' : ''}`}
                    onClick={() => setPanel(p => (p === 'cloud' ? null : 'cloud'))}
                    title={source.mode === 'cloud' ? `${source.providerName} · ${source.model}` : 'Answer with a cloud model, using your own API key'}
                >
                    <Cloud size={13} strokeWidth={1.75} style={{ transform: "scaleX(-1)" }} />
                    On Cloud
                    {cloud && <ChevronDown size={12} strokeWidth={2} />}
                </button>
            </div>
            {panel === 'cloud' && <CloudPanel room={room} onDone={() => setPanel(null)} />}
            {panel === 'local' && <LocalPanel room={room} onDone={() => setPanel(null)} />}
        </div>
    );
}

function CloudPanel({ onDone, room }: { onDone: () => void; room?: number }) {
    const [source, setSource] = useAnswerSource();
    const [providers, setProviders] = useState<CloudProvider[] | null>(null);
    const [tab, setTab] = useState<'models' | 'keys'>('models');
    const [picked, setPicked] = useState<string | null>(source.mode === 'cloud' ? source.provider : null);
    const [query, setQuery] = useState('');
    const [error, setError] = useState<string | null>(null);

    const refresh = () =>
        listProviders()
            .then(p => {
                setProviders(p);
                // Nothing connected yet: start where the user has to start.
                if (!p.some(x => x.connected)) setTab('keys');
            })
            .catch(() => setError('Could not reach Counsel’s engine. Is it running?'));
    useEffect(() => {
        void refresh();
    }, []);

    const connected = useMemo(() => new Set((providers ?? []).filter(p => p.connected).map(p => p.id)), [providers]);
    const nameOf = (id: string) => providers?.find(p => p.id === id)?.name ?? id;

    const use = (provider: string, model: string, label?: string) => {
        setSource({ mode: 'cloud', provider, providerName: nameOf(provider), model, label });
        onDone();
    };

    /** Send the user to connect the model's best route. */
    const connectFor = (m: CatalogModel) => {
        setPicked(m.routes[0].provider);
        setTab('keys');
    };

    return (
        <div className="cloud-panel" role="dialog" aria-label="Answer with a cloud model" style={room ? { maxHeight: room } : undefined}>
            <div className="cloud-panel__head">
                <div className="cloud-panel__title">
                    <strong title="Your question, the conversation and attached documents go to the provider that serves the model, under your own key. On Computer keeps everything on this machine.">Cloud model</strong>
                    <div className="cloud-panel__tabs" role="tablist">
                        <button type="button" role="tab" aria-selected={tab === 'models'} className={tab === 'models' ? 'is-on' : ''} onClick={() => setTab('models')}>
                            Models
                        </button>
                        <button type="button" role="tab" aria-selected={tab === 'keys'} className={tab === 'keys' ? 'is-on' : ''} onClick={() => setTab('keys')}>
                            Keys{connected.size > 0 && <span className="cloud-panel__count">{connected.size}</span>}
                        </button>
                    </div>
                </div>
            </div>

            {tab === 'models' ? (
                <ModelsTab
                    query={query}
                    setQuery={setQuery}
                    connected={connected}
                    nameOf={nameOf}
                    source={source}
                    onUse={use}
                    onConnect={connectFor}
                />
            ) : (
                <KeysTab
                    providers={providers}
                    picked={picked}
                    setPicked={setPicked}
                    refresh={refresh}
                    source={source}
                    onUse={use}
                />
            )}
            {error && <p className="cloud-panel__error">{error}</p>}
        </div>
    );
}

function ModelsTab({
    query,
    setQuery,
    connected,
    nameOf,
    source,
    onUse,
    onConnect,
}: {
    query: string;
    setQuery: (q: string) => void;
    connected: Set<string>;
    nameOf: (id: string) => string;
    source: ReturnType<typeof useAnswerSource>[0];
    onUse: (provider: string, model: string, label?: string) => void;
    onConnect: (m: CatalogModel) => void;
}) {
    const q = query.trim().toLowerCase();
    const groups = CATALOG.map(g => ({
        ...g,
        models: g.models.filter(m => !q || `${g.vendor} ${m.name} ${m.note}`.toLowerCase().includes(q)),
    })).filter(g => g.models.length);

    return (
        <div className="cloud-models">
            <label className="cloud-models__search">
                <Search size={13} />
                <input type="search" placeholder="Search models or makers" value={query} onChange={e => setQuery(e.target.value)} autoFocus />
            </label>
            <div className="cloud-models__list">
                {groups.map(g => (
                    <section key={g.vendor} className="cloud-models__group">
                        <h4>{g.vendor}</h4>
                        {g.models.map(m => {
                            const route = m.routes.find(r => connected.has(r.provider));
                            const on = source.mode === 'cloud' && route && source.provider === route.provider && source.model === route.id;
                            return (
                                <button
                                    key={m.name}
                                    type="button"
                                    className={`cloud-model${on ? ' is-on' : ''}${route ? '' : ' is-locked'}`}
                                    onClick={() => (route ? onUse(route.provider, route.id, m.name) : onConnect(m))}
                                >
                                    <span className="cloud-model__text">
                                        <span className="cloud-model__name">
                                            {m.name}
                                            {m.reasoning && <span className="cloud-model__tag">Reasoning</span>}
                                        </span>
                                        <span className="cloud-model__note">{m.note}</span>
                                    </span>
                                    <span className="cloud-model__route">
                                        {on ? <Check size={14} /> : route ? `via ${nameOf(route.provider)}` : 'Connect a key'}
                                    </span>
                                </button>
                            );
                        })}
                    </section>
                ))}
                {!groups.length && <p className="cloud-panel__muted">No model matches. Connected providers list every model they serve under Keys.</p>}
            </div>
        </div>
    );
}

function KeysTab({
    providers,
    picked,
    setPicked,
    refresh,
    source,
    onUse,
}: {
    providers: CloudProvider[] | null;
    picked: string | null;
    setPicked: (id: string) => void;
    refresh: () => Promise<void>;
    source: ReturnType<typeof useAnswerSource>[0];
    onUse: (provider: string, model: string) => void;
}) {
    const [key, setKey] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [models, setModels] = useState<string[] | null>(null);
    const [filter, setFilter] = useState('');

    const provider = providers?.find(p => p.id === picked) ?? null;
    const info = picked ? PROVIDER_INFO[picked] : undefined;

    // A connected provider: its live model list (this also proves the key).
    useEffect(() => {
        setModels(null);
        setFilter('');
        setError(null);
        if (!provider?.connected) return;
        let live = true;
        listModels(provider.id)
            .then(m => live && setModels(m.length ? m : info?.fallbackModels ?? []))
            .catch(e => {
                if (!live) return;
                setError(e instanceof Error ? e.message : String(e));
                setModels(info?.fallbackModels ?? []);
            });
        return () => {
            live = false;
        };
    }, [provider?.id, provider?.connected]); // eslint-disable-line react-hooks/exhaustive-deps

    const connect = async () => {
        if (!provider || !key.trim()) return;
        setBusy(true);
        setError(null);
        try {
            if (!(await connectProvider(provider.id, key))) throw new Error('The key could not be saved.');
            try {
                await listModels(provider.id);
            } catch (e) {
                // A rejected key is not kept.
                const msg = e instanceof Error ? e.message : String(e);
                if (/reject|401|403/i.test(msg)) {
                    await disconnectProvider(provider.id);
                    throw new Error(msg);
                }
            }
            setKey('');
            await refresh();
        } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
        } finally {
            setBusy(false);
        }
    };

    const shown = (models ?? []).filter(m => m.toLowerCase().includes(filter.toLowerCase()));

    return (
        <div className="cloud-panel__cols">
            <ul className="cloud-panel__providers" role="listbox" aria-label="Providers">
                {(providers ?? []).map(p => (
                    <li key={p.id}>
                        <button
                            type="button"
                            role="option"
                            aria-selected={p.id === picked}
                            className={`cloud-panel__provider${p.id === picked ? ' is-on' : ''}`}
                            onClick={() => setPicked(p.id)}
                        >
                            <span>{p.name}</span>
                            {p.connected && <span className="cloud-panel__dot" title="Connected" />}
                        </button>
                    </li>
                ))}
                {!providers && <li className="cloud-panel__muted">Loading…</li>}
            </ul>

            <div className="cloud-panel__detail">
                {!provider ? (
                    <p className="cloud-panel__muted">
                        Choose a provider. OpenRouter gives one key for models from every maker; a maker’s own key
                        (Anthropic, OpenAI, Google…) is used first when you have one.
                    </p>
                ) : !provider.connected ? (
                    <form
                        className="cloud-panel__connect"
                        onSubmit={e => {
                            e.preventDefault();
                            void connect();
                        }}
                    >
                        <label htmlFor="cloud-key">{provider.name} API key</label>
                        <input
                            id="cloud-key"
                            type="password"
                            autoComplete="off"
                            spellCheck={false}
                            placeholder={info?.hint ?? 'API key'}
                            value={key}
                            onChange={e => setKey(e.target.value)}
                            autoFocus
                        />
                        <span className="cloud-panel__muted">Stored encrypted on this computer.</span>
                        <div className="cloud-panel__row">
                            {info && (
                                <button
                                    type="button"
                                    className="cloud-panel__link"
                                    onClick={() => {
                                        if ('__TAURI_INTERNALS__' in window) void openExternal(info.keyUrl);
                                        else window.open(info.keyUrl, '_blank', 'noopener');
                                    }}
                                >
                                    Get a key <ExternalLink size={11} />
                                </button>
                            )}
                            <button type="submit" className="ui-btn ui-btn--primary cloud-panel__go" disabled={busy || !key.trim()}>
                                {busy ? 'Checking…' : 'Connect'}
                            </button>
                        </div>
                    </form>
                ) : (
                    <div className="cloud-panel__models">
                        <input type="search" placeholder={`All ${provider.name} models`} value={filter} onChange={e => setFilter(e.target.value)} />
                        <ul>
                            {models === null && <li className="cloud-panel__muted">Loading models…</li>}
                            {shown.map(m => {
                                const on = source.mode === 'cloud' && source.provider === provider.id && source.model === m;
                                return (
                                    <li key={m}>
                                        <button type="button" className={`cloud-panel__model${on ? ' is-on' : ''}`} onClick={() => onUse(provider.id, m)}>
                                            <span>{m}</span>
                                            {on && <Check size={13} />}
                                        </button>
                                    </li>
                                );
                            })}
                            {models && filter && !shown.length && (
                                <li>
                                    <button type="button" className="cloud-panel__model" onClick={() => onUse(provider.id, filter.trim())}>
                                        Use “{filter.trim()}”
                                    </button>
                                </li>
                            )}
                        </ul>
                        <button
                            type="button"
                            className="cloud-panel__link cloud-panel__disconnect"
                            onClick={() => void disconnectProvider(provider.id).then(refresh)}
                        >
                            Remove key
                        </button>
                    </div>
                )}
                {error && <p className="cloud-panel__error">{error}</p>}
            </div>
        </div>
    );
}

/** On Computer: pick an installed model, download a recommended one, and
 *  set the Hugging Face token gated models need. Same panel, same place and
 *  same size rules as the cloud one. */
function LocalPanel({ onDone, room }: { onDone: () => void; room?: number }) {
    const selection = useModelSelection();
    const models = selection?.models ?? [];
    const selected = selection?.selected ?? null;
    const downloads = useDownloads();

    const [recs, setRecs] = useState<LocalCatalogModel[] | null>(null);
    useEffect(() => {
        let live = true;
        getRecommendedModels(4)
            .then(r => live && setRecs(r))
            .catch(() => live && setRecs([]));
        return () => {
            live = false;
        };
    }, []);
    const [error, setError] = useState('');
    const startDownload = async (m: LocalCatalogModel) => {
        setError('');
        const r = await installModel(m);
        if (!r.ok) setError(r.message);
    };
    const progressOf = (name: string) =>
        downloads.find(d => d.model_name === name && ['Downloading', 'Starting', 'Queued', 'Paused'].includes(d.status));

    // The Hugging Face token: saved as typed, shared with the Models page.
    const [token, setToken] = useState(getHfToken());
    const { setApiKey } = useAuth();
    useEffect(() => {
        const sync = () => setToken(getHfToken());
        window.addEventListener(HF_TOKEN_EVENT, sync);
        return () => window.removeEventListener(HF_TOKEN_EVENT, sync);
    }, []);

    return (
        <div className="cloud-panel local-panel" role="dialog" aria-label="Answer with a model on this computer" style={room ? { maxHeight: room } : undefined}>
            <div className="local-panel__cols">
                <section className="local-panel__col">
                    <h4 className="local-panel__h">On this computer</h4>
                    <ul className="local-panel__list">
                        {models.map(m => {
                            const on = selected?.id === m.id;
                            return (
                                <li key={m.id}>
                                    <button
                                        type="button"
                                        className={`cloud-model${on ? ' is-on' : ''}`}
                                        onClick={() => {
                                            if (!on) selection?.select({ id: m.id, name: m.name, source: 'local' });
                                            onDone();
                                        }}
                                    >
                                        <span className="cloud-model__text">
                                            <span className="cloud-model__name">{m.name}</span>
                                        </span>
                                        <span className="cloud-model__route">{on ? <Check size={14} /> : ''}</span>
                                    </button>
                                </li>
                            );
                        })}
                        {models.length === 0 && <li className="cloud-panel__muted">No models yet. Download one.</li>}
                    </ul>
                </section>

                <section className="local-panel__col">
                    <h4 className="local-panel__h">Download</h4>
                    <ul className="local-panel__list">
                        {recs === null && <li className="cloud-panel__muted">Loading…</li>}
                        {recs?.map(m => {
                            const dl = progressOf(m.name);
                            return (
                                <li key={m.id} className="local-panel__rec">
                                    <span className="cloud-model__text">
                                        <span className="cloud-model__name">{m.name}</span>
                                        <span className="cloud-model__note">{formatBytes(m.size_bytes)}</span>
                                    </span>
                                    {dl ? (
                                        <span className="local-panel__pct">{Math.round(dl.percentage ?? 0)}%</span>
                                    ) : (
                                        <button type="button" className="ui-btn ui-btn--secondary ui-btn--pill local-panel__get" onClick={() => void startDownload(m)}>
                                            Download
                                        </button>
                                    )}
                                </li>
                            );
                        })}
                        {recs?.length === 0 && <li className="cloud-panel__muted">Nothing to suggest right now.</li>}
                    </ul>
                    <label className="local-panel__token">
                        <span className={`models-key-dot${/^hf_[A-Za-z0-9]{30,}$/.test(token.trim()) ? ' is-set' : ''}`} aria-hidden="true" />
                        <input
                            type="password"
                            placeholder="Hugging Face token (for gated models)"
                            aria-label="Hugging Face access token"
                            value={token}
                            onChange={e => {
                                const v = e.target.value;
                                setToken(v);
                                if (v.trim()) {
                                    setHfToken(v.trim());
                                    setApiKey('huggingface', v.trim());
                                    // The encrypted key store: a signed-in route.
                                    persistApiKey('huggingface', v.trim()).catch(() => {});
                                } else {
                                    clearHfToken();
                                    setApiKey('huggingface', '');
                                }
                            }}
                        />
                    </label>
                    {error && <p className="cloud-panel__error">{error}</p>}
                </section>
            </div>
            <button
                type="button"
                className="cloud-panel__link local-panel__all"
                onClick={() => {
                    onDone();
                    selection?.openModels();
                }}
            >
                View all models
            </button>
        </div>
    );
}
