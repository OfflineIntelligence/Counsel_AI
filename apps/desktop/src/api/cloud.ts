/**
 * Where answers come from: this computer (a downloaded model) or a cloud
 * provider the user connected with their own API key.
 *
 * The backend does the talking (it keeps the key encrypted and runs the same
 * retrieval, attachments and history for both), so a cloud answer is a normal
 * /generate/stream request with a `cloud` field. This module only keeps the
 * user's choice and wraps the /cloud endpoints.
 */
import { useEffect, useState } from 'react';
import { getApiBaseSync } from './backendUrl';
import { deleteApiKey, saveApiKey } from './apiKeys';

export interface CloudProvider {
    id: string;
    name: string;
    connected: boolean;
}

/** Where to get a key, and a model to start on if the list cannot load. */
export const PROVIDER_INFO: Record<string, { keyUrl: string; hint: string; fallbackModels: string[] }> = {
    openai: { keyUrl: 'https://platform.openai.com/api-keys', hint: 'sk-…', fallbackModels: ['gpt-4o', 'gpt-4o-mini'] },
    anthropic: { keyUrl: 'https://console.anthropic.com/settings/keys', hint: 'sk-ant-…', fallbackModels: ['claude-sonnet-4-5'] },
    gemini: { keyUrl: 'https://aistudio.google.com/apikey', hint: 'AIza…', fallbackModels: ['gemini-2.5-pro', 'gemini-2.5-flash'] },
    deepseek: { keyUrl: 'https://platform.deepseek.com/api_keys', hint: 'sk-…', fallbackModels: ['deepseek-chat', 'deepseek-reasoner'] },
    moonshot: { keyUrl: 'https://platform.moonshot.ai/console/api-keys', hint: 'sk-…', fallbackModels: ['kimi-k2-0905-preview'] },
    perplexity: { keyUrl: 'https://www.perplexity.ai/settings/api', hint: 'pplx-…', fallbackModels: ['sonar', 'sonar-pro', 'sonar-reasoning'] },
    mistral: { keyUrl: 'https://console.mistral.ai/api-keys', hint: 'Key', fallbackModels: ['mistral-large-latest'] },
    xai: { keyUrl: 'https://console.x.ai', hint: 'xai-…', fallbackModels: ['grok-4'] },
    groq: { keyUrl: 'https://console.groq.com/keys', hint: 'gsk_…', fallbackModels: ['llama-3.3-70b-versatile'] },
    nvidia: { keyUrl: 'https://build.nvidia.com', hint: 'nvapi-…', fallbackModels: ['meta/llama-3.3-70b-instruct'] },
    together: { keyUrl: 'https://api.together.xyz/settings/api-keys', hint: 'Key', fallbackModels: ['meta-llama/Llama-3.3-70B-Instruct-Turbo'] },
    openrouter: { keyUrl: 'https://openrouter.ai/keys', hint: 'sk-or-…', fallbackModels: ['openrouter/auto'] },
};

/** `label` is the model's friendly name from the catalog, when it has one. */
export type AnswerSource =
    | { mode: 'local' }
    | { mode: 'cloud'; provider: string; providerName: string; model: string; label?: string };

const KEY = 'oca.answerSource';
const EVENT = 'oca-answer-source';

export function getAnswerSource(): AnswerSource {
    try {
        const raw = localStorage.getItem(KEY);
        if (raw) {
            const v = JSON.parse(raw) as AnswerSource;
            if (v.mode === 'local' || (v.mode === 'cloud' && v.provider && v.model)) return v;
        }
    } catch {
        /* fall through */
    }
    return { mode: 'local' };
}

/**
 * Tell the backend. It is the single source of truth: every model call it
 * makes (chat, agents, drafting, document intelligence) follows this switch.
 * Resolves false when the backend refused (e.g. the key was removed).
 */
async function pushToBackend(v: AnswerSource): Promise<boolean> {
    const body = v.mode === 'cloud' ? { provider: v.provider, model: v.model } : {};
    const r = await fetch(`${getApiBaseSync()}/cloud/active`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    });
    // Not signed in yet: keep the choice; it is sent again after sign-in.
    if (r.status === 401) return true;
    return r.ok;
}

export function setAnswerSource(v: AnswerSource) {
    try {
        localStorage.setItem(KEY, JSON.stringify(v));
    } catch {
        /* applied for this session only */
    }
    window.dispatchEvent(new Event(EVENT));
    void pushToBackend(v)
        .then(ok => {
            if (!ok && v.mode === 'cloud') setAnswerSource({ mode: 'local' });
        })
        .catch(() => {
            /* Backend not up yet: syncAnswerSource re-sends on start. */
        });
}

/** On app start (the backend forgets on restart): re-send the saved choice,
 *  retrying while the backend boots. */
export async function syncAnswerSource(attempts = 20): Promise<void> {
    const v = getAnswerSource();
    for (let i = 0; i < attempts; i++) {
        try {
            const ok = await pushToBackend(v);
            if (!ok && v.mode === 'cloud') setAnswerSource({ mode: 'local' });
            return;
        } catch {
            await new Promise(r => setTimeout(r, 1500));
        }
    }
}

/** The choice, kept in sync across every composer that shows it. */
export function useAnswerSource(): [AnswerSource, (v: AnswerSource) => void] {
    const [v, setV] = useState(getAnswerSource);
    useEffect(() => {
        const on = () => setV(getAnswerSource());
        window.addEventListener(EVENT, on);
        return () => window.removeEventListener(EVENT, on);
    }, []);
    return [v, setAnswerSource];
}

/** The `cloud` field for a /generate/stream request, or undefined for local. */
export function cloudTarget(): { provider: string; model: string } | undefined {
    const v = getAnswerSource();
    return v.mode === 'cloud' ? { provider: v.provider, model: v.model } : undefined;
}

export async function listProviders(): Promise<CloudProvider[]> {
    const r = await fetch(`${getApiBaseSync()}/cloud/providers`);
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    return r.json();
}

/** The provider's models, fetched with the saved key. Throws with the
 *  provider's own reason when the key is rejected. */
export async function listModels(provider: string): Promise<string[]> {
    const r = await fetch(`${getApiBaseSync()}/cloud/models?provider=${encodeURIComponent(provider)}`);
    const body = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(body.error || `HTTP ${r.status}`);
    return body.models ?? [];
}

export async function connectProvider(provider: string, key: string): Promise<boolean> {
    return saveApiKey(provider, key.trim());
}

export async function disconnectProvider(provider: string): Promise<boolean> {
    const v = getAnswerSource();
    if (v.mode === 'cloud' && v.provider === provider) setAnswerSource({ mode: 'local' });
    return deleteApiKey(provider);
}
