/**
 * Model availability, recommendations and install: the calls the chat and the
 * Aurora need to get from "no model" to "answering", shared with the Models
 * page so the install request is built in exactly one place.
 */
import { getApiBaseSync } from './backendUrl';
import { getHfToken } from './hfToken';

/** Mirrors `ReadinessState` in crates/offline-intelligence/src/api/model_api.rs. */
export type ReadinessState = 'ready' | 'loading' | 'restarting' | 'not_loaded' | 'none_installed' | 'sleeping';

/** A sleeping model is unloaded only to save memory: sending a question
 *  loads it, so it counts as ready to ask. */
export function canAsk(readiness: Readiness): boolean {
    return readiness.state === 'ready' || readiness.state === 'sleeping';
}

let lastPrewarm = 0;

/** The user is about to ask: start loading a sleeping model now so the wait
 *  overlaps their typing. Fire and forget, at most once every 15 s. */
export function prewarmModel(): void {
    const now = Date.now();
    if (now - lastPrewarm < 15_000) return;
    lastPrewarm = now;
    void fetch(`${getApiBaseSync()}/models/prewarm`, { method: 'POST' }).catch(() => {
        /* Backend unreachable; the real request reports it. */
    });
}

/** Tell the backend whether the app window has focus (shorter idle unload
 *  while it does not). */
export function reportWindowFocus(focused: boolean): void {
    void fetch(`${getApiBaseSync()}/runtime/focus`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ focused }),
    }).catch(() => {
        /* Best effort. */
    });
}

export interface Readiness {
    state: ReadinessState;
    /** Installed chat models on disk. */
    installed: number;
    loading_model?: string;
}

/** The subset of the registry's ModelInfo the install flow and cards use. */
export interface CatalogModel {
    id: string;
    name: string;
    description?: string;
    author?: string;
    status: string;
    size_bytes: number;
    format: string;
    download_source?: string;
    filename?: string;
    parameters?: string;
    context_length?: number;
    compatibility_score?: number;
    tags?: string[];
}

/** Fired after something changes which model is available or active, so
 *  every surface that shows model state can refresh from the backend. */
export const MODELS_CHANGED_EVENT = 'oca-models-changed';

export function announceModelsChanged(): void {
    window.dispatchEvent(new CustomEvent(MODELS_CHANGED_EVENT));
}

/**
 * Whether a question can be answered right now. `purpose` is set when the
 * user is about to send, so the backend records an unanswerable request in
 * its log without a failing request ever being made.
 */
export async function getReadiness(purpose?: 'chat' | 'agent'): Promise<Readiness> {
    const query = purpose ? `?for=${purpose}` : '';
    const response = await fetch(`${getApiBaseSync()}/models/readiness${query}`);
    if (!response.ok) throw new Error(`Readiness check failed (HTTP ${response.status})`);
    return response.json();
}

/** Hardware-ranked recommendations, best first, not yet installed. */
export async function getRecommendedModels(limit: number): Promise<CatalogModel[]> {
    // Ask for extra: installed and in-progress entries are filtered out here.
    const response = await fetch(`${getApiBaseSync()}/models/recommendations?limit=${limit + 6}`);
    if (!response.ok) throw new Error(`Recommendations failed (HTTP ${response.status})`);
    const models: CatalogModel[] = await response.json();
    return models.filter(m => m.status === 'Available').slice(0, limit);
}

export function buildSourcePayload(model: Pick<CatalogModel, 'id' | 'download_source' | 'filename'>) {
    const source = model.download_source || 'huggingface';
    if (source === 'ollama') {
        const modelName = model.id.startsWith('ollama:') ? model.id.slice(7) : model.id;
        return { type: 'Ollama', model_name: modelName };
    }
    // Use filename from model info if available, otherwise construct a guess
    const filename = model.filename || (() => {
        const parts = model.id.split('/');
        return parts.length > 1
            ? `${parts[parts.length - 1].toLowerCase().replace(/-gguf$/i, '')}.Q4_K_M.gguf`
            : `${model.id}.gguf`;
    })();
    return { type: 'HuggingFace', repo_id: model.id, filename };
}

export type InstallResult =
    | { ok: true }
    /** The model needs a Hugging Face token or access approval: the Models
     *  page owns that flow. */
    | { ok: false; needsModelsPage: true; message: string }
    | { ok: false; needsModelsPage: false; message: string };

/** Start downloading `model`. The backend activates it when it finishes if
 *  no other model is loaded. */
export async function installModel(model: CatalogModel): Promise<InstallResult> {
    const hfToken = getHfToken();
    const response = await fetch(`${getApiBaseSync()}/models/install`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            model_id: model.id,
            model_name: model.name,
            source: buildSourcePayload(model),
            description: model.description,
            size_bytes: model.size_bytes,
            format: model.format,
            hf_token: hfToken || undefined,
        }),
    });
    if (response.ok) {
        announceModelsChanged();
        return { ok: true };
    }
    const text = await response.text().catch(() => '');
    if (text.includes('401') || text.includes('Unauthorized') || text.includes('gated')) {
        return { ok: false, needsModelsPage: true, message: 'This model needs a Hugging Face token.' };
    }
    return { ok: false, needsModelsPage: false, message: text || `Download failed (HTTP ${response.status})` };
}

/** Choose an installed model without loading it: any running model is
 *  unloaded, and the chosen one loads on the next question (or prewarm). */
export async function chooseModel(modelId: string): Promise<{ ok: boolean; message?: string }> {
    const response = await fetch(`${getApiBaseSync()}/models/switch`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model_id: modelId, lazy: true }),
    });
    if (response.ok) return { ok: true };
    const body = await response.json().catch(() => ({} as { detail?: string; error?: string }));
    return { ok: false, message: body.detail || body.error || `Could not switch the model (HTTP ${response.status})` };
}

/** Load an installed model. */
export async function activateModel(modelId: string): Promise<{ ok: boolean; message?: string }> {
    const response = await fetch(`${getApiBaseSync()}/models/switch`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model_id: modelId }),
    });
    if (response.ok) {
        announceModelsChanged();
        return { ok: true };
    }
    const body = await response.json().catch(() => ({} as { detail?: string; error?: string }));
    return { ok: false, message: body.detail || body.error || `Could not load the model (HTTP ${response.status})` };
}

export function formatBytes(bytes: number): string {
    if (!bytes) return '';
    const gb = bytes / 1024 ** 3;
    return gb >= 1 ? `${gb.toFixed(1)} GB` : `${Math.round(bytes / 1024 ** 2)} MB`;
}

/**
 * The backend's `sanitize_model_id` (model_management/storage.rs): downloads
 * are tracked under this form, while the catalog lists the raw id
 * ("author/repo"), so matching one to the other needs both. Ids longer than
 * 200 characters are hashed by the backend and cannot be reproduced here;
 * callers also match on the model name for those.
 */
export function sanitizeModelId(id: string): string {
    let s = id.replace(/[:/\\<>"|?*]/g, '_').replace(/\.+$/, '').replace(/\s+$/, '');
    if (/^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$/.test(s.toUpperCase())) s += '_model';
    return s;
}

/** An install that was under way when the app last closed. */
export interface InterruptedDownload {
    model_id: string;
    model_name: string;
    size_bytes: number;
    bytes_on_disk: number;
    started_at: string;
    source: Record<string, unknown>;
    description?: string;
    format: string;
}

export async function getInterruptedDownloads(): Promise<InterruptedDownload[]> {
    const response = await fetch(`${getApiBaseSync()}/models/downloads/interrupted`);
    if (!response.ok) return [];
    return response.json();
}

/** Continue an interrupted install from the partial files on disk. */
export async function resumeInterruptedDownload(d: InterruptedDownload): Promise<InstallResult> {
    const hfToken = getHfToken();
    const response = await fetch(`${getApiBaseSync()}/models/install`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            model_id: d.model_id,
            model_name: d.model_name,
            source: d.source,
            description: d.description,
            size_bytes: d.size_bytes,
            format: d.format,
            hf_token: hfToken || undefined,
            resume: true,
        }),
    });
    if (response.ok) {
        announceModelsChanged();
        return { ok: true };
    }
    const text = await response.text().catch(() => '');
    return { ok: false, needsModelsPage: false, message: text || `Resume failed (HTTP ${response.status})` };
}

/** Forget an interrupted install and delete its partial files. */
export async function discardInterruptedDownload(modelId: string): Promise<void> {
    await fetch(`${getApiBaseSync()}/models/downloads/interrupted/discard?model_id=${encodeURIComponent(modelId)}`, {
        method: 'POST',
    });
    announceModelsChanged();
}
