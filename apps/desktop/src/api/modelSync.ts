/**
 * One watcher for every model download in the app.
 *
 * The backend owns the truth (chat-model downloads at /models/downloads,
 * embedding models at /embeddings/models). This module polls it ONCE, shares
 * the result with every surface (chat capsule, setup card, Models page, the
 * download bubble, Aurora), and fires MODELS_CHANGED_EVENT whenever a download
 * changes state -- finished, paused, cancelled, failed, or an embedding model
 * installed or selected -- so every surface refetches the same moment, no
 * matter which one started the download.
 *
 * Polling is quick while something is downloading, slow when idle, slower
 * still while the window is hidden, and stops when nothing is subscribed.
 */
import { useSyncExternalStore } from 'react';

import { getApiBaseSync } from './backendUrl';
import { MODELS_CHANGED_EVENT, announceModelsChanged } from './models';

/** A chat-model download as the backend reports it. Fields beyond these are
 *  passed through untouched. */
export interface DownloadRow {
    download_id: string;
    model_name: string;
    status: string;
    percentage: number;
    [key: string]: any;
}

const ACTIVE = new Set(['Downloading', 'Starting', 'Queued']);
const FAST_MS = 1000;
const IDLE_MS = 4000;
const HIDDEN_MS = 10000;

let rows: DownloadRow[] = [];
let embeddingBusy = false;
let embeddingKey = '';
let seen = new Map<string, string>();
let primed = false;

const listeners = new Set<() => void>();
let timer: number | undefined;
let running: Promise<DownloadRow[]> | null = null;

function emit(): void {
    listeners.forEach(l => l());
}

async function fetchJson(path: string): Promise<any | null> {
    try {
        const r = await fetch(`${getApiBaseSync()}${path}`);
        return r.ok ? await r.json() : null;
    } catch {
        return null; // backend briefly busy: keep the last state
    }
}

async function pass(): Promise<DownloadRow[]> {
    const [dl, emb] = await Promise.all([fetchJson('/models/downloads'), fetchJson('/embeddings/models')]);
    let changed = false;

    if (Array.isArray(dl)) {
        const now = new Map<string, string>();
        for (const d of dl as DownloadRow[]) now.set(d.download_id, d.status);
        if (primed) {
            // A status that moved, or an in-flight download that vanished.
            for (const [id, status] of now) if (seen.has(id) && seen.get(id) !== status) changed = true;
            for (const [id, status] of seen) if (!now.has(id) && ACTIVE.has(status)) changed = true;
        }
        seen = now;
        rows = dl;
        primed = true;
        emit();
    }

    if (emb && Array.isArray(emb.models)) {
        const key = emb.models
            .map((m: any) => `${m.id}:${m.installed}:${m.active}:${m.download?.state}`)
            .join('|');
        if (embeddingKey && key !== embeddingKey) changed = true;
        embeddingKey = key;
        embeddingBusy = emb.models.some((m: any) => m.download?.state === 'downloading' || m.download?.state === 'verifying');
    }

    if (changed) announceModelsChanged();
    return rows;
}

/** Look at the backend now (shares one request if one is already running). */
export function refreshDownloads(): Promise<DownloadRow[]> {
    if (!running) {
        running = pass().finally(() => {
            running = null;
            schedule();
        });
    }
    return running;
}

function schedule(): void {
    window.clearTimeout(timer);
    if (listeners.size === 0) return;
    const active = embeddingBusy || rows.some(d => ACTIVE.has(d.status));
    const wait = document.hidden ? HIDDEN_MS : active ? FAST_MS : IDLE_MS;
    timer = window.setTimeout(() => void refreshDownloads(), wait);
}

const onChanged = () => void refreshDownloads();

function subscribe(listener: () => void): () => void {
    if (listeners.size === 0) {
        window.addEventListener(MODELS_CHANGED_EVENT, onChanged);
        document.addEventListener('visibilitychange', onChanged);
        void refreshDownloads();
    }
    listeners.add(listener);
    return () => {
        listeners.delete(listener);
        if (listeners.size === 0) {
            window.removeEventListener(MODELS_CHANGED_EVENT, onChanged);
            document.removeEventListener('visibilitychange', onChanged);
            window.clearTimeout(timer);
        }
    };
}

/** Every chat-model download, live and shared by all callers. */
export function useDownloads(): DownloadRow[] {
    return useSyncExternalStore(subscribe, () => rows);
}

/** The latest snapshot, for code that is not a component. */
export function getDownloads(): DownloadRow[] {
    return rows;
}
