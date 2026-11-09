// Chat API: stream SSE responses from local backend (127.0.0.1:8888)

import type { Readiness } from './models';

export interface Message {
    role: 'system' | 'user' | 'assistant';
    content: string;
}

export interface ChatRequest {
    model: string;
    messages: Message[];
    max_tokens?: number;
    temperature?: number;
    stream?: boolean;
    attachments?: ChatAttachment[];
}

/**
 * One retrieved passage the answer was built on.
 *
 * These are the excerpts the backend folds into the model's prompt. They now
 * also come to the UI as data, ahead of the first token, so the answer can
 * show what it is reading from instead of the model reciting it back in prose.
 */
export interface AnswerSource {
    document_id: number;
    filename: string;
    page_start: number | null;
    page_end: number | null;
    section_label: string | null;
    /** The tag the model was told to cite, e.g. "SPA.pdf, p.47, Section 12.3". */
    citation: string;
    text: string;
    version_label: string | null;
}

// Temporary chat attachment (in-memory, not persisted)
export interface ChatAttachment {
    name: string;
    content_base64?: string;
    content_text?: string;
    mime_type?: string;
    // Absolute path on the user's machine, when captured via the native
    // file dialog (Tauri plugin-dialog). Recorded as document provenance.
    source_path?: string;
    // Set when attaching an EXISTING Local Storage file to this chat (the
    // "@filename" autocomplete or folder-icon picker), instead of a fresh
    // paperclip pick. The backend resolves this by id directly - do NOT
    // also set content_text/content_base64 alongside it: the backend
    // ignores them when local_file_id is present, and populating
    // content_text with pre-extracted text here is exactly the bug this
    // field exists to avoid (see stream_api::persist_inline_attachments).
    local_file_id?: number;
    // References an EXISTING document already known to the system by its own
    // documents.id, as opposed to local_file_id (which only exists for
    // Local-Storage-backed files). Used when re-referencing a paperclip-only
    // attachment (no local_file_id) from the @ picker's "attached in this
    // conversation" list - no bytes needed, content is already stored.
    document_id?: number;
}

// Chat persistence: API response types for conversation management
export interface ConversationSummary {
    id: string;
    title: string;
    created_at: string;
    last_accessed: string;
    message_count: number;
    pinned: boolean;
}

export interface ConversationsResponse {
    conversations: ConversationSummary[];
}

export interface ConversationDetailResponse {
    id: string;
    title: string;
    messages: Message[];
}

import { getApiBaseSync } from './backendUrl';
import { paceText } from './pacedStream';
import { cloudTarget } from './cloud';

// Check if backend is ready with retry logic
async function checkBackendReadiness(maxRetries = 3, delayMs = 500): Promise<boolean> {
    for (let i = 0; i < maxRetries; i++) {
        try {
            const response = await fetch(`${getApiBaseSync()}/healthz`, {
                method: 'GET',
                headers: { 'Accept': 'application/json' },
            });
            if (response.ok) {
                // Try to parse JSON response (new format)
                try {
                    const data = await response.json();
                    // Accept "ready" or "degraded" as backend ready
                    return data.status === 'ready' || data.status === 'degraded';
                } catch {
                    // Fallback: try text format for backward compatibility
                    const text = await response.text();
                    return text === 'OK';
                }
            }
        } catch (error) {
            console.warn(`Backend readiness check attempt ${i + 1}/${maxRetries} failed:`, error);
        }

        if (i < maxRetries - 1) {
            await new Promise(resolve => setTimeout(resolve, delayMs * Math.pow(2, i)));
        }
    }
    return false;
}

/** The backend refused because no model can answer (see /models/readiness).
 *  Callers show the model setup prompt instead of an error. */
export class ModelNotReadyError extends Error {
    readonly readiness: Readiness;
    constructor(readiness: Readiness, detail: string) {
        super(detail);
        this.name = 'ModelNotReadyError';
        this.readiness = readiness;
    }
}

/**
 * Stream an answer at the app's own pace (see pacedStream.ts), whatever the
 * model's speed. Every surface that shows an answer reads it through here,
 * so they all type with the same rhythm.
 */
export function streamChat(
    messages: Message[],
    sessionId?: string,
    modelId?: string,
    attachments?: ChatAttachment[],
    signal?: AbortSignal,
    onSources?: (sources: AnswerSource[]) => void,
    onScaffold?: (blocks: string[]) => void,
): AsyncGenerator<string, void, unknown> {
    return paceText(
        streamChatRaw(messages, sessionId, modelId, attachments, signal, onSources, onScaffold),
        signal,
    );
}

// Stream assistant tokens via Server-Sent Events (delta chunks), as fast as
// the model produces them.
async function* streamChatRaw(
    messages: Message[],
    sessionId?: string,
    modelId?: string,
    attachments?: ChatAttachment[],
    signal?: AbortSignal,
    /**
     * Called once, before the first token, with the passages this answer is
     * being built from. A callback rather than a second kind of yielded value:
     * every existing caller of this generator treats what it yields as text to
     * append, and widening that would break all of them to serve one.
     */
    onSources?: (sources: AnswerSource[]) => void,
    /**
     * Called once, before the first token, with the blocks that were folded
     * into the prompt. They are what a small model reprints instead of
     * answering, and having them lets that reprint be cut EXACTLY rather than
     * guessed at from its shape — the same for every document format, and for
     * the blocks that contain no document at all.
     */
    onScaffold?: (blocks: string[]) => void,
): AsyncGenerator<string, void, unknown> {
    // Enforce session ID requirement for persistence - prevents orphaned conversations
    if (!sessionId) {
        throw new Error('Session ID is required for chat. This is a bug - ChatWindow should have generated one.');
    }
    
    // Check backend readiness before making request
    if (!(await checkBackendReadiness())) {
        throw new Error('Backend is not ready. Please ensure the offline-intelligence service is running.');
    }

    // Build request body
    const requestBody: any = {
        model: modelId || 'local-llm',
        model_source: 'local',
        messages: messages,
        session_id: sessionId,
        max_tokens: 2000,
        stream: true,
        temperature: 0.7,
        attachments: attachments && attachments.length > 0 ? attachments : undefined,
        // On Cloud: the backend answers with the connected provider instead
        // of the local model (api/cloud.ts).
        cloud: cloudTarget(),
    };

    const response = await fetch(`${getApiBaseSync()}/generate/stream`, {
        method: 'POST',
        headers: {
            'Content-Type': 'application/json',
        },
        body: JSON.stringify(requestBody),
        signal,
    });

    if (!response.ok) {
        // The body is read for EVERY failing status, not just 503.
        //
        // This used to replace a 502 with "No model is currently loaded" and
        // throw the body away. A 502 is only ever produced AFTER the backend
        // has confirmed the engine is loaded and ready — it means the engine
        // was alive and rejected the request. So the substituted message named
        // a condition that could not be true, while the engine's own
        // explanation (a chat-template role error, a context overflow) was
        // discarded unread. Never invent a diagnosis over a real one.
        const body = (await response.text().catch(() => '')).trim();
        let errorMessage = body || `The request failed (HTTP ${response.status}).`;

        // Structured refusal: no model can answer. Not an error to report,
        // a state to act on — the caller shows the model setup prompt.
        if ((response.status === 503 || response.status === 429) && body.startsWith('{')) {
            try {
                const parsed = JSON.parse(body) as { error?: string; readiness?: Readiness; detail?: string };
                if (parsed.error === 'model_not_ready' && parsed.readiness) {
                    throw new ModelNotReadyError(parsed.readiness, parsed.detail ?? 'No model is ready.');
                }
                // Backend admission control (generation_queue.rs): show its sentence, not the JSON.
                if (parsed.error === 'generation_busy' && parsed.detail) {
                    errorMessage = parsed.detail;
                }
            } catch (e) {
                if (e instanceof ModelNotReadyError) throw e;
            }
        }

        if (response.status === 504 && !body) {
            errorMessage =
                'Gateway Timeout: The model took too long to respond. It may be too large for your hardware.';
        } else if (response.status === 404 && !body) {
            errorMessage = 'Not Found: Model or engine binary not found. Please check your installation.';
        }

        // Always logged verbatim, whatever we chose to show. When something
        // unexpected happens next, this is the line that explains it.
        console.error(
            `[chat] request failed: HTTP ${response.status}; engine/backend said: ${body || '(empty body)'}`,
        );

        throw new Error(errorMessage);
    }

    if (!response.body) {
        throw new Error('Response body is null');
    }

    const reader = response.body.getReader();
    const decoder = new TextDecoder('utf-8');
    let buffer = '';

    while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split('\n');

        // Keep the last incomplete line in the buffer
        buffer = lines.pop() || '';

        for (const line of lines) {
            const trimmed = line.trim();
            if (!trimmed || trimmed === '[DONE]') continue;

            if (trimmed.startsWith('data: ')) {
                const jsonStr = trimmed.slice(6);
                if (jsonStr === '[DONE]') continue;
                // eslint-disable-next-line @typescript-eslint/no-explicit-any
                let data: any;
                try {
                    data = JSON.parse(jsonStr);
                } catch (e) {
                    console.error('Error parsing SSE line:', e);
                    continue;
                }
                // The model failed mid-answer (a cloud provider's error, or the
                // engine's): say so, instead of ending the answer silently.
                if (data.error) {
                    const err = data.error as string | { message?: string };
                    throw new Error(typeof err === 'string' ? err : err.message || 'The model stopped with an error.');
                }
                try {

                    // The sources frame carries no `choices`, so a caller that
                    // passes no sink simply sees a chunk with no content.
                    if (Array.isArray(data.sources)) {
                        onSources?.(data.sources as AnswerSource[]);
                        continue;
                    }
                    if (Array.isArray(data.scaffold)) {
                        onScaffold?.(data.scaffold as string[]);
                        continue;
                    }

                    const content = data.choices?.[0]?.delta?.content;
                    if (content) {
                        yield content;
                    }
                } catch (e) {
                    console.error('Error parsing SSE line:', e);
                }
            }
        }
    }
}

// Chat persistence: Fetch all saved conversations for sidebar display
export async function fetchConversations(): Promise<ConversationSummary[]> {
    if (!(await checkBackendReadiness())) {
        throw new Error('Backend is not ready. Please ensure the offline-intelligence service is running.');
    }
    const response = await fetch(`${getApiBaseSync()}/conversations`);
    if (!response.ok) {
        throw new Error(`HTTP error! status: ${response.status}`);
    }
    const data: ConversationsResponse = await response.json();
    // The drafting assistant talks through the same streaming endpoint, which
    // persists every session. Those belong to their document, not the chat list.
    return data.conversations.filter(c => !c.id.startsWith(DRAFT_ASSISTANT_SESSION_PREFIX));
}

/** Session ids used by the drafting side panel's assistant, one per draft. */
export const DRAFT_ASSISTANT_SESSION_PREFIX = 'draft-assistant-';

// Test-friendly alias
export const getConversations = fetchConversations;

// Chat persistence: Load full conversation history from database when user clicks a chat
export async function fetchConversation(
    id: string,
    options: { allowMissing?: boolean } = {},
): Promise<ConversationDetailResponse | null> {
    try {
        if (!(await checkBackendReadiness())) {
            throw new Error('Backend is not ready. Please ensure the offline-intelligence service is running.');
        }
        // allowMissing: a thread with no row yet (Aurora's, before its
        // first message is saved) comes back as an empty conversation rather
        // than a 404, so opening it is never a failed request.
        const query = options.allowMissing ? '?allow_missing=true' : '';
        const response = await fetch(`${getApiBaseSync()}/conversations/${encodeURIComponent(id)}${query}`);
        if (response.status === 404) return null;
        if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
        }
        return await response.json();
    } catch (error) {
        console.error('Failed to fetch conversation:', error);
        return null;
    }
}

// Chat persistence: Save auto-generated title to database after first message
export async function updateConversationTitle(id: string, title: string): Promise<{ id: string; title: string }> {
    try {
        if (!(await checkBackendReadiness())) {
            throw new Error('Backend is not ready. Please ensure the offline-intelligence service is running.');
        }
        const response = await fetch(`${getApiBaseSync()}/conversations/${id}/title`, {
            method: 'PUT',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({ title }),
        });
        if (!response.ok) {
            // Surface backend response body to help debug failed title updates
            const errorData = await response.text();
            console.error(`Failed to update conversation title [${id}]: HTTP ${response.status} - ${errorData}`);
            throw new Error(`HTTP error! status: ${response.status}`);
        }
        console.log(`Title saved successfully for conversation [${id}]: "${title}"`);
        return { id, title };
    } catch (error) {
        console.error(`Failed to update conversation title [${id}]:`, error);
        throw error;
    }
}

// Create new conversation
export async function createNewConversation(): Promise<{ id: string; title: string }> {
    try {
        if (!(await checkBackendReadiness())) {
            throw new Error('Backend is not ready. Please ensure the offline-intelligence service is running.');
        }
        const response = await fetch(`${getApiBaseSync()}/conversations`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({}),
        });
        if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
        }
        const data = await response.json();
        return data;
    } catch (error) {
        console.error('Failed to create conversation:', error);
        throw error;
    }
}

// Delete conversation
export async function deleteConversation(id: string): Promise<boolean> {
    try {
        if (!(await checkBackendReadiness())) {
            throw new Error('Backend is not ready. Please ensure the offline-intelligence service is running.');
        }
        const response = await fetch(`${getApiBaseSync()}/conversations/${id}`, {
            method: 'DELETE',
        });
        if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
        }
        return true;
    } catch (error) {
        console.error(`Failed to delete conversation [${id}]:`, error);
        throw error;
    }
}

// Chat persistence: Update pinned status of a conversation in the database
export async function updateConversationPinned(id: string, pinned: boolean): Promise<boolean> {
    try {
        if (!(await checkBackendReadiness())) {
            throw new Error('Backend is not ready. Please ensure the offline-intelligence service is running.');
        }
        const response = await fetch(`${getApiBaseSync()}/conversations/${id}/pinned`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({ pinned }),
        });
        if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
        }
        return true;
    } catch (error) {
        console.error('Failed to update conversation pinned status:', error);
        return false;
    }
}
