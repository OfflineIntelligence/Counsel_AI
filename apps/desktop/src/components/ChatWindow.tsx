import React, { useState, useRef, useEffect } from 'react';
import type { AnswerSource, Message, ChatAttachment } from '../api/chat';
import { ModelNotReadyError, streamChat, updateConversationTitle } from '../api/chat';
import { getAnswerSource } from '../api/cloud';
import AnswerSourceToggle from './chat/AnswerSourceToggle';
import { announceModelsChanged, canAsk, getReadiness, prewarmModel, type Readiness } from '../api/models';
import ModelCapsule from './ModelCapsule';
import PaperclipIcon from './PaperclipIcon';
import { SourcePills, SourcePane, type AttachSource, type SourceEntry } from './AttachSources';
import ModelChip from './chat/ModelChip';
import { downloadDraft } from '../api/drafts';
import { getLibraryItemText } from '../api/library';
import { FileGlyph, FolderGlyph, formatOf, typeWordsOf, FILE_ICON } from './FileBrandIcon';

/** Files shown per attachment stack before it collapses to "+N more". */
const STACK_VISIBLE = 4;
import { getApiBaseSync } from '../api/backendUrl';
import { open as openNativeFileDialog } from '@tauri-apps/plugin-dialog';
import { readFile, stat as statFile } from '@tauri-apps/plugin-fs';
import { DIALOG_EXTENSIONS, SUPPORTED_LIST_LABEL, partitionSupported } from '../supportedFormats';
import { exceedsUploadBudget, totalBytesOf, uploadTooLargeMessage } from '../uploadLimits';
import { processAttachments } from '../api/attachments';
import IndexStatusChip from './rag/IndexStatusChip';
import { FileTypeIcon } from './workspace/FileBrowser';
import { useNotificationHelpers } from '../contexts/NotificationContext';
import { useAssistant, useAssistantPage } from '../contexts/AssistantContext';
import SourcesPanel from './chat/SourcesPanel';
import { stripAll } from './chat/answerText';
import ThinkingStatus, { type ThinkingPhase } from './ThinkingStatus';
import TriangleMark from './TriangleMark';

// A file picked via the native OS dialog - carries a REAL absolute path
// (unlike a browser <input type="file">, whose File objects deliberately
// never expose one). This is what lets the backend record "location of the
// file" as the product design requires.
interface NativeAttachedFile {
    name: string;
    /** Absolute path when the file came from the native dialog or a desktop
     *  drag-drop. Empty for a pasted file, which has no path by definition. */
    path: string;
    size: number;
    // Processing state, driven by the attach-time upload. Files are extracted
    // the moment they are picked, so the tray can show progress and surface a
    // failure while the user is still typing - rather than the user learning
    // about it from the model's answer after they send.
    status: 'processing' | 'ready' | 'failed';
    // Set once the backend has stored and extracted the file. This is what the
    // send carries: an id, not the bytes the backend already has.
    documentId?: number;
    error?: string;
    /** Characters the backend actually extracted. Shown in the tray because it
     *  is the only direct evidence that the document was READ rather than
     *  merely uploaded — a 4 MB scan that yields 0 chars looks identical to a
     *  successful one if all the card reports is its byte size. */
    charCount?: number;
    /** True when the backend recognised the content hash and reused a previous
     *  extraction. Worth surfacing: it explains why a 200-page PDF became
     *  ready instantly. */
    reused?: boolean;
    /** Bytes held in memory for a pasted file. Files picked from disk are read
     *  on demand from `path` instead, so this stays undefined for them and a
     *  large batch is not duplicated in memory. */
    bytes?: Uint8Array;
    /** Which engine read the document ('vision_model' | 'windows_ocr' |
     *  'native'). Images read with basic OCR get a notice: OCR cannot read
     *  handwriting, an active vision model can. */
    extractionEngine?: string;
}
import { SaveTranscriptDialog } from './SaveTranscriptDialog';
import { SaveTranscriptWebDialog } from './SaveTranscriptWebDialog';
import MessageContent from './MessageContent';
import { ChatHome } from './ChatHome';
import { MessageActions } from './MessageActions';
import './home.css';
import TypingSquares from './TypingSquares';

/** Ceiling for the auto-growing composer, in pixels — roughly eight lines.
 *  Past this the field scrolls internally rather than pushing the transcript
 *  off the top of the window, which is what happens if it is allowed to grow
 *  without bound while someone pastes a contract into it. */
const COMPOSER_MAX_HEIGHT = 200;

export interface Chat {
    id: string;
    title: string;
    messages: Message[];
    createdAt: Date;
    pinned?: boolean;
    saved?: boolean;
}

interface ChatWindowProps {
    messages: Message[];
    chatTitle: string | null;
    chatId: string | null;
    sessionId: string | null;
    onSessionIdChange: (sessionId: string) => void;
    isPinned?: boolean;
    onMessagesUpdate: (messages: Message[]) => void;
    onTitleGenerated?: (title: string, sessionId: string) => void;
    onPinChat?: (chatId: string) => void;
    onDeleteChat?: (chatId: string) => Promise<void>;
    onQuestionAsked: () => void;
    selectedModel?: { id: string; name: string; source: 'local' } | null;
    onOpenModels?: (focusHfToken?: boolean) => void;
    onBackgroundStreamUpdate?: (sessionId: string, content: string) => void;
    onRequestSendMessage?: (sessionId: string) => 'sent' | 'queued';
    onQueueMessage?: (sessionId: string, content: string) => void;
    onStreamComplete?: (sessionId: string) => void;
    onStopStream?: () => void;
    queuedMessages?: Array<{ id: string; content: string }>;
    isSessionStreaming?: boolean;
    pendingQueueRun?: { sessionId: string; content: string } | null;
    onQueueRunConsumed?: () => void;
    onRequestDeleteChat?: (chatId: string) => void;
    models?: Array<{ id: string; name: string }>;
    /** Home-state routes. Optional so the component still renders standalone
     *  in tests — an empty state missing a link is degraded, not broken. */
    onSelectChat?: (chatId: string) => void;
    onOpenLibrary?: () => void;
    onOpenDraft?: () => void;
    onOpenLocalFiles?: () => void;
}

export const ChatWindow: React.FC<ChatWindowProps> = ({
    messages,
    chatTitle,
    chatId,
    sessionId,
    onSessionIdChange,
    isPinned = false,
    onMessagesUpdate,
    onTitleGenerated,
    onPinChat,
    onDeleteChat,
    onQuestionAsked,
    selectedModel,
    onOpenModels,
    onBackgroundStreamUpdate,
    onRequestSendMessage,
    onQueueMessage,
    onStreamComplete,
    onStopStream,
    queuedMessages = [],
    isSessionStreaming = false,
    pendingQueueRun,
    onQueueRunConsumed,
    onRequestDeleteChat,
    models = [],
    onSelectChat,
    onOpenLibrary,
    onOpenDraft,
    onOpenLocalFiles,
}) => {
    const firstPromptSent = useRef(false);
    const mountedRef = useRef(true);
    const abortControllerRef = useRef<AbortController | null>(null);
    const [input, setInput] = useState('');
    // The app-wide assistant, so an answer can be handed to it for follow-up.
    const assistant = useAssistant();
    const [isLoading, setIsLoading] = useState(false);
    // What the wait is spent on, named in the thinking indicator.
    const [thinkingPhase, setThinkingPhase] = useState<ThinkingPhase>('thinking');
    // The turn whose answer just finished: its triangle fills once, then goes.
    const [finishedTurn, setFinishedTurn] = useState<number | null>(null);
    const messagesEndRef = useRef<HTMLDivElement>(null);
    const chatScrollRef = useRef<HTMLElement | null>(null);
    // Stick-to-bottom: true while the user sits at the latest output. Any
    // manual scroll-up unsticks, so a streaming answer never yanks the view
    // back down; scrolling back to the bottom re-sticks. A ref, not state:
    // chunks arrive per token and must not re-render to decide this.
    const stickToBottomRef = useRef(true);
    const [showJumpToLatest, setShowJumpToLatest] = useState(false);
    const [attachedFiles, setAttachedFiles] = useState<NativeAttachedFile[]>([]);
    const [localFileAttachments, setLocalFileAttachments] = useState<ChatAttachment[]>([]);
    const [removingFiles, setRemovingFiles] = useState<Set<string>>(new Set());
    /**
     * The passages each assistant turn was built from, keyed by that turn's
     * index in the visible transcript.
     *
     * Per-turn rather than one "latest" value: scrolling back up a long chat
     * must show what THAT answer read, not what the newest one did. Reset with
     * the session, since the indices belong to this conversation only.
     */
    const [sourcesByTurn, setSourcesByTurn] = useState<Record<number, AnswerSource[]>>({});
    /**
     * The prompt blocks each assistant turn was given, for cutting a reprint
     * out of it exactly. Per turn for the same reason the sources are: an
     * answer higher up the transcript was given different blocks.
     */
    const [scaffoldByTurn, setScaffoldByTurn] = useState<Record<number, string[]>>({});
    /**
     * Sources now expand in place inside their own card (see SourcesPanel):
     * there is no document popup to hold state for.
     */

    /** True while a desktop drag is over the window, driven by Tauri's webview
     *  drag events. Purely presentational — it only shows the drop overlay. */
    const [isDragging, setIsDragging] = useState(false);
    const { showWarning, showError } = useNotificationHelpers();



    // Backend readiness state
    const [backendReady, setBackendReady] = useState(false);
    const [modelSwitching, setModelSwitching] = useState(false);
    
    // @ picker input-focus state
    const [showFileAutocomplete, setShowFileAutocomplete] = useState(false);
    const [fileAutocompleteQuery, setFileAutocompleteQuery] = useState('');
    const [fileAutocompleteIndex, setFileAutocompleteIndex] = useState(0);
    const inputRef = useRef<HTMLTextAreaElement>(null);
    const fileAutocompleteRef = useRef<HTMLDivElement>(null);
    // @ picker stage 2's own search box - needs its own ref so keyboard nav
    // and refocus-after-navigation work even though it's a separate <input>
    // from the main chat input (autoFocus only fires on its initial mount).
    // The attachments tray: closed until the types capsule is clicked.
    const [trayOpen, setTrayOpen] = useState(false);
    const trayRef = useRef<HTMLElement | null>(null);
    useEffect(() => {
        if (!trayOpen) return;
        const onDown = (e: MouseEvent) => {
            const t = e.target as Element;
            if (trayRef.current?.contains(t) || t.closest('.attachment-stack__head, .attach-capsule')) return;
            setTrayOpen(false);
        };
        const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setTrayOpen(false); };
        document.addEventListener('mousedown', onDown);
        document.addEventListener('keydown', onKey);
        return () => {
            document.removeEventListener('mousedown', onDown);
            document.removeEventListener('keydown', onKey);
        };
    }, [trayOpen]);
    const browseInputRef = useRef<HTMLInputElement>(null);
    // The folder button's pills ('menu'), or the source pane they opened.
    const [folderMenu, setFolderMenu] = useState<null | 'menu' | AttachSource>(null);
    // Browser fallback for the paperclip (the desktop app uses the native dialog).
    const browserFileInputRef = useRef<HTMLInputElement>(null);

    // @ picker, stage 1: every document already attached to THIS conversation
    // (paperclip AND Local Storage alike) - sourced from GET
    // /documents/session/:id, which already existed but was never wired to
    // the frontend. local_file_id is null for paperclip-only documents.
    interface SessionDocumentEntry {
        id: number;
        name: string;
        local_file_id: number | null;
        /** The turn this file was attached to, or null for pre-migration rows. */
        msg_index: number | null;
    }
    const [sessionDocuments, setSessionDocuments] = useState<SessionDocumentEntry[]>([]);
    /**
     * Which files were attached to which turn, for the chips on a sent message.
     *
     * The tray clears on send — it has to, the next message is a new one — and
     * until now nothing took its place, so a document you had just attached
     * left no trace in the transcript at all. The backend records the turn now
     * (session_documents.msg_index), so this survives a reload: reopening a
     * chat months later still shows what was sent with what.
     */
    const attachmentsByTurn = React.useMemo(() => {
        const map = new Map<number, SessionDocumentEntry[]>();
        for (const doc of sessionDocuments) {
            if (doc.msg_index == null) continue;
            const list = map.get(doc.msg_index);
            if (list) list.push(doc);
            else map.set(doc.msg_index, [doc]);
        }
        return map;
    }, [sessionDocuments]);

    /**
     * What was staged in the tray when each user message was sent, by that
     * message's absolute turn index.
     *
     * The tray clears on send and the store-backed turn (session_documents.
     * msg_index) only arrives after the stream completes — and only if the
     * link and the stamp both landed. Without this, a just-sent document
     * leaves no trace in the transcript in the meantime, and if either
     * backend step missed, never. Written at send time from the tray itself,
     * so it is always right for this session; the store map above remains
     * the source of truth after a reload, and wins on name conflicts.
     */
    const [sentAttachments, setSentAttachments] = useState<Record<number, { id: number | null; name: string }[]>>({});

    /** Chips for a user turn: store-backed first, optimistic snapshot filling gaps. */
    const chipsForTurn = (absolute: number): { id: number | null; name: string }[] => {
        const fromStore = attachmentsByTurn.get(absolute) ?? [];
        const seen = new Set(fromStore.map(d => d.name));
        const optimistic = (sentAttachments[absolute] ?? []).filter(l => !seen.has(l.name));
        return [
            ...fromStore.map(d => ({ id: d.id as number | null, name: d.name })),
            ...optimistic,
        ];
    };

    // ── AI Assistant ──────────────────────────────────────────────────────
    // The assistant reads this conversation AND its documents: everything
    // already attached to the session, plus files sitting in the tray that
    // have finished extracting but have not been sent yet. Text comes from the
    // extraction the backend already did; at most three documents are quoted,
    // the rest are listed by name so the assistant knows they exist.
    const assistantDocs = [
        ...sessionDocuments.map(d => ({ id: d.id as number | null, name: d.name })),
        ...attachedFiles
            .filter(f => f.documentId !== undefined && !sessionDocuments.some(d => d.id === f.documentId))
            .map(f => ({ id: f.documentId ?? null, name: `${f.name} (attached, not sent)` })),
        ...localFileAttachments.map(a => ({ id: null, name: `${a.name} (from Vault, not sent)` })),
    ];
    const assistantDocsKey = assistantDocs.map(d => `${d.id ?? ''}:${d.name}`).join('|');
    const [assistantDocText, setAssistantDocText] = useState<{ key: string; text: string } | null>(null);
    useEffect(() => {
        if (!assistantDocsKey) return;
        let live = true;
        const readable = assistantDocs.filter(d => d.id != null).slice(0, 3);
        const perDoc = Math.floor(30_000 / Math.max(1, readable.length));
        void Promise.all(
            readable.map(async d => {
                try {
                    const r = await fetch(`${getApiBaseSync()}/documents/${d.id}`);
                    if (!r.ok) return '';
                    const meta = await r.json();
                    const text = String(meta.extracted_text ?? '');
                    return text ? `=== ${d.name} ===\n${text.slice(0, perDoc)}` : '';
                } catch {
                    return '';
                }
            }),
        ).then(parts => live && setAssistantDocText({ key: assistantDocsKey, text: parts.filter(Boolean).join('\n\n') }));
        return () => {
            live = false;
        };
        // assistantDocs is fully described by assistantDocsKey.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [assistantDocsKey]);

    const conversationText = messages
        .filter(m => m.role !== 'system')
        .slice(-12)
        .map(m => `${m.role === 'user' ? 'User' : 'Assistant'}: ${m.content}`)
        .join('\n\n');
    const docText = assistantDocsKey && assistantDocText?.key === assistantDocsKey ? assistantDocText.text : '';
    const docNames = assistantDocs.map(d => d.name);
    useAssistantPage({
        label: 'Chat',
        title: chatTitle ?? (docNames.length ? `${docNames.length} attached document${docNames.length === 1 ? '' : 's'}` : undefined),
        format: docNames.length ? `Attached: ${docNames.slice(0, 6).join(', ')}${docNames.length > 6 ? '…' : ''}` : undefined,
        text:
            [
                conversationText && `--- CONVERSATION ---\n${conversationText}`,
                docText && `--- ATTACHED DOCUMENTS ---\n${docText}`,
            ]
                .filter(Boolean)
                .join('\n\n') || undefined,
        suggestions: docNames.length
            ? ['Summarise the attached documents', 'What do the attachments say about termination?', 'List the parties and key dates']
            : ['Summarise this conversation', 'List the open questions', 'Turn this into a client email'],
    });

    // @ picker, stage 2: "From Local Storage" - full folder/file browser,
    // separate state from the existing curated-picker panel's state so the
    // two surfaces never interfere with each other.
    interface LocalStorageTreeEntry { id: number; name: string; path: string; isDirectory: boolean; size: number; modified: string; children?: LocalStorageTreeEntry[]; }
    const [pickerStage, setPickerStage] = useState<'session' | 'browse'>('session');
    const [localStorageTree, setLocalStorageTree] = useState<LocalStorageTreeEntry[]>([]);
    const [folderStack, setFolderStack] = useState<LocalStorageTreeEntry[][]>([]);
    const [browseQuery, setBrowseQuery] = useState('');

    const hasMessages = messages.filter(m => m.role !== 'system').length > 0;
    const userMsgCount = messages.filter(m => m.role === 'user').length;
    if (userMsgCount > 1) {
      console.warn('[DIAG] ChatWindow render: ', userMsgCount, 'user messages — roles:', messages.map(m => m.role));
    }

    // Check backend readiness with retry logic
    useEffect(() => {
        let cancelled = false;
        let timeoutId: ReturnType<typeof setTimeout>;

        const checkBackendReadiness = async (attempt: number) => {
            if (cancelled) return;
            try {
                const response = await fetch(`${getApiBaseSync()}/healthz`);
                if (cancelled) return;
                if (response.ok) {
                    // Enhanced health check: Accept both "ready" and "degraded" states
                    try {
                        const healthData = await response.json();
                        // healthData = { status: "ready" | "initializing" | "degraded", runtime_ready: boolean, message?: string }

                        // For OLLAMA-style behavior: Accept "degraded" (no model) as backend ready
                        // User can download/activate models through UI
                        if (healthData.status === 'ready' || healthData.status === 'degraded') {
                            setBackendReady(true);
                            setModelSwitching(false);
                            console.log('Backend ready:', healthData.status, healthData.runtime_ready ? '(model loaded)' : '(no model loaded yet)');
                        } else if (healthData.status === 'switching') {
                            setBackendReady(true);
                            setModelSwitching(true);
                            console.log('Model switching in progress:', healthData.message);
                            const delay = 1000;
                            timeoutId = setTimeout(() => checkBackendReadiness(attempt + 1), delay);
                        } else if (healthData.status === 'initializing') {
                            console.log('Backend still initializing...');
                            setBackendReady(false);
                            // Retry after delay
                            const delay = Math.min(10000, 500 * Math.pow(2, attempt + 1));
                            timeoutId = setTimeout(() => checkBackendReadiness(attempt + 1), delay);
                        } else {
                            // Unknown status - retry
                            setBackendReady(false);
                            const delay = Math.min(10000, 500 * Math.pow(2, attempt + 1));
                            timeoutId = setTimeout(() => checkBackendReadiness(attempt + 1), delay);
                        }
                    } catch (parseError) {
                        // Fallback if JSON parsing fails (backward compatibility)
                        console.warn('Health check response format unexpected, assuming ready');
                        setBackendReady(true);
                    }
                } else {
                    throw new Error(`Backend health check failed with status: ${response.status}`);
                }
            } catch {
                if (cancelled) return;
                console.warn('Backend not ready yet, attempt:', attempt + 1);
                setBackendReady(false);

                // Exponential backoff: retry after progressively longer delays (max 10s)
                const delay = Math.min(10000, 500 * Math.pow(2, attempt + 1));
                timeoutId = setTimeout(() => checkBackendReadiness(attempt + 1), delay);
            }
        };

        checkBackendReadiness(0);
        return () => {
            cancelled = true;
            clearTimeout(timeoutId);
        };
    }, []);

    useEffect(() => {
        if (!chatTitle) firstPromptSent.current = false;
    }, [chatTitle]);

    useEffect(() => {
        mountedRef.current = true;
        return () => {
            mountedRef.current = false;
        };
    }, []);

    const NEAR_BOTTOM_PX = 80;

    const isNearBottom = () => {
        const el = chatScrollRef.current;
        if (!el) return true;
        return el.scrollHeight - el.scrollTop - el.clientHeight < NEAR_BOTTOM_PX;
    };

    const scrollToBottom = (smooth = false) => {
        const el = chatScrollRef.current;
        if (el) {
            el.scrollTo({ top: el.scrollHeight, behavior: smooth ? 'smooth' : 'auto' });
        } else if (messagesEndRef.current && typeof messagesEndRef.current.scrollIntoView === 'function') {
            messagesEndRef.current.scrollIntoView({ behavior: smooth ? 'smooth' : 'auto' });
        }
    };

    const handleChatScroll = () => {
        const stuck = isNearBottom();
        stickToBottomRef.current = stuck;
        setShowJumpToLatest(!stuck);
    };

    const jumpToLatest = () => {
        stickToBottomRef.current = true;
        setShowJumpToLatest(false);
        scrollToBottom(true);
    };

    // Follow new output only while stuck to the bottom. Instant, not smooth:
    // per-chunk smooth scrolls queue up and fight the user's own scrolling.
    useEffect(() => {
        if (stickToBottomRef.current) scrollToBottom();
    }, [messages]);

    /* Turn indices belong to one conversation. Carrying them into the next
       chat would hang the last answer's sources under an unrelated one.
       Cleared during render rather than in an effect, so the new chat never
       paints a frame carrying the old chat's sources. */
    const [sourcesSession, setSourcesSession] = useState(sessionId);
    if (sourcesSession !== sessionId) {
        setSourcesSession(sessionId);
        setSourcesByTurn({});
        setScaffoldByTurn({});
        setSentAttachments({});
    }

    // Fetch every document already attached to THIS conversation (paperclip
    // and Local Storage alike) for the @ picker's stage 1. Refetched on
    // session change and after every send, so a freshly-attached paperclip
    // file appears in @ on the next lookup within the same session.
    const fetchSessionDocuments = React.useCallback(async (sessionOverride?: string | null) => {
        // The id is a parameter, not just the prop: the send handler's
        // `finally` runs under the render whose prop may still hold the
        // pre-send (even null) session, while the request went out under the
        // effective one. Fetching under a stale id would return nothing — or
        // clear the list that the optimistic chips just filled.
        const effectiveId = sessionOverride !== undefined ? sessionOverride : sessionId;
        if (!effectiveId) {
            setSessionDocuments([]);
            return;
        }
        try {
            const resp = await fetch(`${getApiBaseSync()}/documents/session/${effectiveId}`);
            if (resp.ok) {
                const docs = await resp.json();
                const list: SessionDocumentEntry[] = Array.isArray(docs)
                    ? docs.map((d: any) => ({
                        id: d.id,
                        name: d.original_filename,
                        local_file_id: d.local_file_id ?? null,
                        msg_index: d.msg_index ?? null,
                    }))
                    : [];
                setSessionDocuments(list);
            }
        } catch (error) {
            console.error('Failed to fetch session documents:', error);
        }
    }, [sessionId]);

    useEffect(() => {
        if (!backendReady) return;
        fetchSessionDocuments();
    }, [backendReady, fetchSessionDocuments, sessionId]);

    // Fetch the full Local Storage folder/file tree for the @ picker's
    // "From Local Storage" stage - separate from the existing curated-picker
    // panel's own fetch, so the two surfaces never interfere.
    const fetchLocalStorageTree = React.useCallback(async () => {
        try {
            const resp = await fetch(`${getApiBaseSync()}/files`);
            if (resp.ok) {
                const tree = await resp.json();
                setLocalStorageTree(Array.isArray(tree) ? tree : []);
            }
        } catch (error) {
            console.error('Failed to fetch Local Storage tree:', error);
        }
    }, []);

    // Handle click outside for file autocomplete dropdown
    useEffect(() => {
        const handleClickOutside = (event: MouseEvent) => {
            if (fileAutocompleteRef.current && !fileAutocompleteRef.current.contains(event.target as Node) &&
                inputRef.current && !inputRef.current.contains(event.target as Node)) {
                setShowFileAutocomplete(false);
            }
        };
        if (showFileAutocomplete) {
            document.addEventListener('mousedown', handleClickOutside);
            return () => document.removeEventListener('mousedown', handleClickOutside);
        }
    }, [showFileAutocomplete]);

    // @ picker stage 1: files already attached to THIS conversation
    // (paperclip and Local Storage alike).
    const filteredSessionDocuments = sessionDocuments.filter(f =>
        f.name.toLowerCase().includes(fileAutocompleteQuery.toLowerCase())
    ).slice(0, 8);

    // @ picker stage 2 ("From Local Storage"): current folder's contents,
    // filtered by the stage's own search box.
    const currentBrowseList: LocalStorageTreeEntry[] =
        folderStack.length > 0 ? folderStack[folderStack.length - 1] : localStorageTree;
    const filteredBrowseList = currentBrowseList.filter(f =>
        f.name.toLowerCase().includes(browseQuery.toLowerCase())
    );

    // Handle input change with @ detection
    // Auto-grow the composer with its content, up to a ceiling.
    //
    // Driven from `input` rather than from the change handler, so text that
    // arrives by any other route resizes the box too — a staged prompt from
    // the home screen, a paste, a queued message being restored. Wiring this
    // to onChange alone is the usual bug: the box stays one line tall until
    // the user types a character into text that is already there.
    //
    // The height reset to 'auto' before measuring is required, not
    // superstition: scrollHeight never shrinks below the element's current
    // height, so without it the box can grow but never shrink again.
    useEffect(() => {
        const el = inputRef.current;
        if (!el) return;
        el.style.height = 'auto';
        el.style.height = `${Math.min(el.scrollHeight, COMPOSER_MAX_HEIGHT)}px`;
    }, [input]);

    // …and again whenever the box changes WIDTH. An empty field's scrollHeight
    // includes its wrapped placeholder, so a measurement taken while the box
    // was momentarily narrow (mid-layout, a window resize) left it hundreds
    // of pixels tall with nothing in it until the user typed.
    useEffect(() => {
        const el = inputRef.current;
        if (!el || typeof ResizeObserver === 'undefined') return;
        let lastWidth = el.clientWidth;
        const ro = new ResizeObserver(() => {
            if (el.clientWidth === lastWidth) return;
            lastWidth = el.clientWidth;
            el.style.height = 'auto';
            el.style.height = `${Math.min(el.scrollHeight, COMPOSER_MAX_HEIGHT)}px`;
        });
        ro.observe(el);
        return () => ro.disconnect();
    }, []);

    const handleInputChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
        const value = e.target.value;
        setInput(value);
        // Drives the typing squares and the sidebar's auto-hide (App.tsx).
        window.dispatchEvent(new Event('oca-composer-typing'));
        // Typing means a question is coming: start loading a sleeping model.
        if (value.trim()) prewarmModel();

        // Detect @filename pattern
        const cursorPos = e.target.selectionStart || value.length;
        const textBeforeCursor = value.slice(0, cursorPos);
        const atMatch = textBeforeCursor.match(/@(\S*)$/);

        if (atMatch) {
            // Fresh @ trigger (popup was closed) - always start at stage 1.
            if (!showFileAutocomplete) {
                setPickerStage('session');
                setFolderStack([]);
                setBrowseQuery('');
            }
            setFileAutocompleteQuery(atMatch[1]);
            setShowFileAutocomplete(true);
            setFileAutocompleteIndex(0);
        } else {
            setShowFileAutocomplete(false);
        }
    };

    // Handle keyboard navigation in the @ picker (both stages)
    const handleInputKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
        if (showFileAutocomplete) {
            const activeList: Array<{ name: string }> = pickerStage === 'browse' ? filteredBrowseList : filteredSessionDocuments;
            if (activeList.length > 0) {
                if (e.key === 'ArrowDown') {
                    e.preventDefault();
                    setFileAutocompleteIndex(prev => Math.min(prev + 1, activeList.length - 1));
                    return;
                } else if (e.key === 'ArrowUp') {
                    e.preventDefault();
                    setFileAutocompleteIndex(prev => Math.max(prev - 1, 0));
                    return;
                } else if (e.key === 'Enter' || e.key === 'Tab') {
                    e.preventDefault();
                    if (pickerStage === 'browse') {
                        insertBrowsedFileAsAttachment(filteredBrowseList[fileAutocompleteIndex]);
                    } else {
                        insertExistingDocumentAsAttachment(filteredSessionDocuments[fileAutocompleteIndex]);
                    }
                    return;
                }
            }
            if (e.key === 'Escape') {
                setShowFileAutocomplete(false);
                setPickerStage('session');
                setFolderStack([]);
                return;
            }
        }
        if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            handleSend();
        }
    };

    // Shared: remove the "@query" text from the input after a selection is
    // made in the @ picker (either stage).
    const clearAtQueryFromInput = () => {
        const cursorPos = inputRef.current?.selectionStart || input.length;
        const textBeforeCursor = input.slice(0, cursorPos);
        const textAfterCursor = input.slice(cursorPos);
        const atIndex = textBeforeCursor.lastIndexOf('@');
        if (atIndex >= 0) {
            // Find the end of the filename (next whitespace or end of string)
            let endIndex = cursorPos;
            for (let i = cursorPos; i < textAfterCursor.length; i++) {
                if (textAfterCursor[i] === ' ' || textAfterCursor[i] === '\n') {
                    endIndex = i;
                    break;
                }
            }
            const newText = textBeforeCursor.slice(0, atIndex) + textAfterCursor.slice(endIndex);
            setInput(newText.trim());
        }
    };

    // @ picker stage 1: attach a document already linked to THIS
    // conversation. Local-Storage-backed documents resolve via local_file_id
    // (same as before); paperclip-only documents (no local_file_id, no
    // permanent byte copy) resolve via the new document_id field instead -
    // both are idempotent re-links server-side, no bytes needed either way.
    const insertExistingDocumentAsAttachment = async (doc: SessionDocumentEntry) => {
        if (attachedFiles.length + localFileAttachments.length >= MAX_ATTACHMENTS_PER_MESSAGE) {
            showWarning(
                `Up to ${MAX_ATTACHMENTS_PER_MESSAGE} files per message`,
                'Send these, then attach the rest in a follow-up. Documents stay available for the whole conversation once attached.',
            );
        } else {
            const newAttachment: ChatAttachment = doc.local_file_id != null
                ? { name: doc.name, local_file_id: doc.local_file_id }
                : { name: doc.name, document_id: doc.id };
            setLocalFileAttachments(prev => [...prev, newAttachment]);
            console.log('[DEBUG] Re-referenced session document:', doc.name);
        }
        clearAtQueryFromInput();
        setShowFileAutocomplete(false);
        setTimeout(() => inputRef.current?.focus(), 50);
    };

    // @ picker stage 2 ("From Local Storage"): navigate folders (clicking a
    // directory descends into it), attach files by local_file_id - same
    // resolution the existing curated picker already uses.
    const insertBrowsedFileAsAttachment = async (file: LocalStorageTreeEntry) => {
        if (file.isDirectory) {
            setFolderStack(prev => [...prev, file.children || []]);
            setBrowseQuery('');
            setFileAutocompleteIndex(0);
            setTimeout(() => browseInputRef.current?.focus(), 50);
            return;
        }
        if (attachedFiles.length + localFileAttachments.length >= MAX_ATTACHMENTS_PER_MESSAGE) {
            showWarning(
                `Up to ${MAX_ATTACHMENTS_PER_MESSAGE} files per message`,
                'Send these, then attach the rest in a follow-up. Documents stay available for the whole conversation once attached.',
            );
        } else {
            const newAttachment: ChatAttachment = { name: file.name, local_file_id: file.id };
            setLocalFileAttachments(prev => [...prev, newAttachment]);
            console.log('[DEBUG] Added Local Storage file via @ browse:', file.name);
        }
        clearAtQueryFromInput();
        setShowFileAutocomplete(false);
        setPickerStage('session');
        setFolderStack([]);
        setTimeout(() => inputRef.current?.focus(), 50);
    };

    // Switch the @ popup from stage 1 (session files) to stage 2 (browse
    // Local Storage folders/files).
    const openBrowseStage = () => {
        setPickerStage('browse');
        setFolderStack([]);
        setBrowseQuery('');
        setFileAutocompleteIndex(0);
        fetchLocalStorageTree();
    };

    const goBackFolder = () => {
        setFolderStack(prev => prev.slice(0, -1));
        setBrowseQuery('');
        setFileAutocompleteIndex(0);
        setTimeout(() => browseInputRef.current?.focus(), 50);
    };

    // Keyboard navigation for stage 2's own search box - separate from
    // handleInputKeyDown because focus lives on this input, not the main
    // chat input, once stage 2 is open.
    const handleBrowseInputKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
        if (filteredBrowseList.length > 0) {
            if (e.key === 'ArrowDown') {
                e.preventDefault();
                setFileAutocompleteIndex(prev => Math.min(prev + 1, filteredBrowseList.length - 1));
                return;
            } else if (e.key === 'ArrowUp') {
                e.preventDefault();
                setFileAutocompleteIndex(prev => Math.max(prev - 1, 0));
                return;
            } else if (e.key === 'Enter') {
                e.preventDefault();
                insertBrowsedFileAsAttachment(filteredBrowseList[fileAutocompleteIndex]);
                return;
            }
        }
        if (e.key === 'Escape') {
            e.preventDefault();
            setShowFileAutocomplete(false);
            setPickerStage('session');
            setFolderStack([]);
            return;
        }
        if (e.key === 'Backspace' && browseQuery === '' && folderStack.length > 0) {
            e.preventDefault();
            goBackFolder();
        }
    };

    const backToSessionStage = () => {
        setPickerStage('session');
        setFolderStack([]);
        setBrowseQuery('');
        setFileAutocompleteIndex(0);
        setTimeout(() => inputRef.current?.focus(), 50);
    };

    // Remove local file attachment
    const removeLocalFileAttachment = (name: string) => {
        setLocalFileAttachments(prev => prev.filter(f => f.name !== name));
    };

    // Execute queued send when App.tsx triggers it
    useEffect(() => {
        if (pendingQueueRun && pendingQueueRun.sessionId === sessionId && !isLoading) {
            handleSend(pendingQueueRun.content);
            onQueueRunConsumed?.();
        }
    }, [pendingQueueRun]);

    /** Re-run the last question.
     *
     *  Retry DISCARDS the final assistant turn and re-sends the user message
     *  that produced it — it does not append a second answer. Two answers to
     *  one question in the transcript is the thing that makes a saved thread
     *  unreadable later, and the whole point of the button is that the first
     *  answer was not wanted. */
    const handleRetry = () => {
        if (isLoading) return;
        const visible = messages.filter(m => m.role !== 'system');
        const last = visible[visible.length - 1];
        if (!last || last.role !== 'assistant') return;
        const lastUser = [...visible].reverse().find(m => m.role === 'user');
        if (!lastUser) return;

        // Drop the answer being replaced, then re-send. The truncation is
        // committed before the send so a failed retry cannot leave the old
        // answer and a new one both in the thread.
        onMessagesUpdate(messages.slice(0, messages.length - 1));
        void handleSend(lastUser.content);
    };

    // Set when a question could not be answered because no model is ready;
    // the setup prompt shows in the thread until one is.
    const [modelPrompt, setModelPrompt] = useState<Readiness | null>(null);
    const handleModelReady = React.useCallback(() => {
        setModelPrompt(null);
        // App refreshes its model list and selection from the backend.
        announceModelsChanged();
        inputRef.current?.focus();
    }, []);

    const handleSend = async (overrideContent?: string) => {
        const sendContent = overrideContent ?? input.trim();
        if (!sendContent || isLoading) return;
        
        if (!backendReady) {
            console.log('Backend not ready');
            return;
        }

        if (modelSwitching) {
            console.log('Model switch in progress, blocking send');
            return;
        }

        // Do not send while an attachment is still being extracted.
        //
        // This is the one race attach-time processing introduces: a user can
        // pick a 50-page scan and hit send a second later. Sending then would
        // omit that document entirely and the model would answer "I don't see a
        // file" about something the user is looking at in the tray. Waiting is
        // the honest behaviour, and the tray already shows why.
        const stillProcessing = attachedFiles.filter(f => f.status === 'processing');
        if (stillProcessing.length > 0) {
            console.log(
                '[ChatWindow] Send held: still processing',
                stillProcessing.map(f => f.name),
            );
            return;
        }

        // Ask before sending, so "no model" is a prompt to download one rather
        // than a failed request. The check is one loopback call answered from
        // memory when a model is loaded. `for=chat` records the unanswerable
        // question in the backend log. The typed text stays in the box.
        // On Cloud needs no local model.
        if (getAnswerSource().mode === 'local') try {
            const readiness = await getReadiness('chat');
            if (!canAsk(readiness)) {
                setModelPrompt(readiness);
                return;
            }
        } catch {
            // Backend unreachable: the normal send path reports that.
        }
        setModelPrompt(null);

        let currentSessionId = sessionId;
        if (!currentSessionId) {
            currentSessionId = Date.now().toString();
            onSessionIdChange(currentSessionId);
        }

        // Queue gate: if session is already streaming, queue instead of sending
        if (!overrideContent && onRequestSendMessage?.(currentSessionId) === 'queued') {
            onQueueMessage?.(currentSessionId, sendContent);
            setInput('');
            return;
        }

        // Attachments were already read, uploaded and extracted when they were
        // PICKED (see processPickedFiles), so the send carries their document
        // ids rather than re-reading every file and base64-encoding it here.
        //
        // That old approach put the entire cost of attachment handling between
        // pressing send and the first token: a file read, a ~33% base64
        // inflation, and then the extraction itself - OCR included - all before
        // the model saw anything.
        //
        // Files still processing are not sent. handleSend refuses to run while
        // any attachment is in that state, so reaching here with one would be a
        // bug; filtering is belt-and-braces rather than a silent drop.
        const inlineAttachments: ChatAttachment[] = attachedFiles
            .filter(file => file.documentId !== undefined)
            .map(file => ({
                name: file.name,
                document_id: file.documentId,
                source_path: file.path,
            }));

        const unsent = attachedFiles.filter(f => f.documentId === undefined);
        if (unsent.length > 0) {
            console.warn(
                '[ChatWindow] Not sending attachments without a document id:',
                unsent.map(f => `${f.name} (${f.status})`),
            );
        }

        const allAttachments = [...inlineAttachments, ...localFileAttachments];

        // The user's message is stored and shown VERBATIM. Attachments flow
        // to the model separately, through the backend's document-memory
        // header (documents_store + document_memory), which enumerates every
        // attached file with its type and content. Appending a "[Attached
        // files: …]" line here would (a) duplicate what the backend already
        // says, (b) never match the backend's file-reference regex — which
        // looks for the singular "[Attached: name]" / "@name.ext" forms — and
        // (c) leave a stray marker in the persisted message body that the
        // renderer would then have to strip. Keep the record clean.
        const firstPrompt = !firstPromptSent.current ? sendContent : null;
        const userMsg: Message = { role: 'user', content: sendContent };
        const newMessages = [...messages, userMsg];
        console.log('[DIAG] handleSend: messages prop roles:', messages.map(m => m.role), '→ newMessages roles:', newMessages.map(m => m.role));

        // Snapshot the tray against this user turn BEFORE it clears: this is
        // the optimistic chip record (see sentAttachments). Only files that
        // actually carry a document id were sent; the rest get the warning
        // below and leave no chip, because the backend never saw them.
        const stagedForChips = [
            ...attachedFiles
                .filter(f => f.documentId !== undefined)
                .map(f => ({ id: f.documentId ?? null, name: f.name })),
            ...localFileAttachments.map(a => ({ id: a.document_id ?? null, name: a.name })),
        ];
        if (stagedForChips.length > 0) {
            const userTurn = newMessages.length - 1;
            setSentAttachments(prev => ({ ...prev, [userTurn]: stagedForChips }));
        }

        onMessagesUpdate(newMessages);
        setInput('');
        // The user's own send re-sticks to the bottom: the answer streams
        // beneath them unless they scroll up to read earlier output.
        stickToBottomRef.current = true;
        setShowJumpToLatest(false);
        setAttachedFiles([]);
        setLocalFileAttachments([]);
        setIsLoading(true);
        setThinkingPhase('thinking');
        setFinishedTurn(null);
        onQuestionAsked();

        if (firstPrompt && !chatTitle) {
            firstPromptSent.current = true;
            const title = firstPrompt.length > 50
                ? firstPrompt.slice(0, 50) + '...'
                : firstPrompt;
            updateConversationTitle(currentSessionId!, title)
                .catch(err => console.error('Failed to save title:', err));
            onTitleGenerated?.(title, currentSessionId!);
        }

        const abortController = new AbortController();
        abortControllerRef.current = abortController;

        try {
            onMessagesUpdate([...newMessages, { role: 'assistant' as const, content: '' }]);
            let fullContent = '';

            // The assistant turn about to be appended, by its index in the
            // FULL message list — the same basis the backend stamps
            // attachments with (processed_messages is req.messages verbatim),
            // so sources and chips agree about what a "turn" is.
            //
            // Its sources arrive before the first token, so the panel is there
            // as the answer writes itself rather than appearing after it.
            const assistantTurn = newMessages.length;

            for await (const chunk of streamChat(
                newMessages,
                currentSessionId!,
                selectedModel?.id,
                allAttachments.length > 0 ? allAttachments : undefined,
                abortController.signal,
                sources => {
                    setSourcesByTurn(prev => ({ ...prev, [assistantTurn]: sources }));
                    if (sources.length > 0 && mountedRef.current) setThinkingPhase('reading');
                },
                blocks => setScaffoldByTurn(prev => ({ ...prev, [assistantTurn]: blocks })),
            )) {
                fullContent += chunk;
                onBackgroundStreamUpdate?.(currentSessionId!, fullContent);
                if (mountedRef.current) {
                    onMessagesUpdate([...newMessages, { role: 'assistant' as const, content: fullContent }]);
                }
            }
            // Written in full (not stopped, not failed): the mark closes it.
            if (mountedRef.current && fullContent.trim()) {
                setFinishedTurn(assistantTurn);
                setTimeout(() => {
                    if (mountedRef.current) setFinishedTurn(t => (t === assistantTurn ? null : t));
                }, 2200);
            }
        } catch (error: unknown) {
            if (error instanceof DOMException && error.name === 'AbortError') return;
            if (error instanceof ModelNotReadyError) {
                // The model went away between the check and the send. The
                // backend stored nothing, so the thread goes back to how it
                // was and the question goes back in the box.
                if (mountedRef.current) {
                    onMessagesUpdate(messages);
                    if (!overrideContent) setInput(sendContent);
                    setModelPrompt(error.readiness);
                }
                return;
            }
            console.error('Chat error:', error);
            const errMessage = error instanceof Error ? error.message : 'Unknown error';
            let errorMsg = `An error occurred: ${errMessage}`;

            if (errMessage.includes('fetch')) {
                errorMsg = 'Could not connect to the LLM backend. Make sure the model is loaded and llama-server is running on port 9639.';
            }
            
            onBackgroundStreamUpdate?.(currentSessionId!, errorMsg);
            if (mountedRef.current) {
                onMessagesUpdate([...newMessages, { role: 'assistant' as const, content: errorMsg }]);
            }
        } finally {
            abortControllerRef.current = null;
            onStreamComplete?.(currentSessionId!);
            if (mountedRef.current) {
                setIsLoading(false);
            }
            // Refresh the @ picker's "attached in this conversation" list so
            // any file just sent (paperclip or Local Storage) is pickable by
            // name on the next message, in this same session. Under the
            // EFFECTIVE session id: on a first message this render's prop may
            // still be the pre-send one.
            fetchSessionDocuments(currentSessionId);
        }
    };

    const handleSubmit = (e: React.FormEvent) => {
        e.preventDefault();
        handleSend();
    };

    const [showSaveDialog, setShowSaveDialog] = useState(false);
    const [showWebSaveDialog, setShowWebSaveDialog] = useState(false);
    const [pendingTranscript, setPendingTranscript] = useState<string>('');
    const [pendingDefaultName, setPendingDefaultName] = useState<string>('chat.txt');

    // UNREACHABLE as of the header-actions removal: the Save button was this
    // function's only caller. Kept, with the two dialogs below, so the feature
    // can be given a new entry point rather than rebuilt. Delete both if
    // transcript export is not coming back.
    const handleSaveTranscript = async () => {
        if (messages.length === 0) return;
        try {
            let transcript = '';
            if (chatTitle) {
                transcript += `Chat: ${chatTitle}\n`;
                transcript += `Date: ${new Date().toLocaleString()}\n`;
                transcript += '='.repeat(60) + '\n\n';
            }
            messages.filter(m => m.role !== 'system').forEach(msg => {
                const sender = msg.role === 'user' ? 'User' : 'Offline Counsel AI';
                transcript += `${sender}:\n${msg.content}\n\n`;
            });
            setPendingTranscript(transcript);
            setPendingDefaultName(`${chatTitle || 'chat'}-${new Date().toISOString().slice(0, 10)}.txt`);
            setShowWebSaveDialog(true);
        } catch (error) {
            console.error('Error saving transcript:', error);
        }
    };

    // Opens the OS-native file picker (Tauri plugin-dialog) instead of a
    // browser <input type="file">. The native dialog returns a real absolute
    // path for each selection - the browser File API deliberately never
    // exposes one - which is what lets the backend record document location.
    // The real constraint isn't how many documents you attach - it's the total
    // request size. Files are uploaded as MULTIPART (see api/attachments.ts),
    // and the ceiling is the server's router-wide body limit; the shared budget
    // derived from it lives in uploadLimits.ts and is enforced identically here
    // and in Local Storage.
    //
    // MAX_ATTACHMENTS_PER_MESSAGE is the hard per-message count cap (matches
    // the backend's own defense-in-depth cap, MAX_ATTACHMENTS_PER_REQUEST in
    // stream_api.rs), shared by BOTH the paperclip flow here and the @ picker /
    // folder-icon picker flows (insertExistingDocumentAsAttachment /
    // insertBrowsedFileAsAttachment / the folder button's source pane) - all of
    // them together must not exceed it, since they all merge into the same
    // outgoing message's attachment list.
    const MAX_ATTACHMENTS_PER_MESSAGE = 16;

    // Read and upload picked files so the backend extracts them NOW.
    //
    // The whole batch goes in ONE request so the backend's per-format lanes see
    // it at once and can run different formats in parallel. Uploading one file
    // at a time would serialise exactly what those lanes exist to parallelise.
    //
    // Declared BEFORE stageFiles, which calls it. Both are const arrow
    // functions, so a later declaration would still work at runtime (stageFiles
    // only runs on interaction) but reads as a use-before-declaration and the
    // linter flags it as such.
    const processPickedFiles = async (picked: NativeAttachedFile[]) => {
        const markFailed = (names: string[], message: string) => {
            setAttachedFiles(prev =>
                prev.map(f =>
                    names.includes(f.name) && f.status === 'processing'
                        ? { ...f, status: 'failed' as const, error: message }
                        : f,
                ),
            );
        };

        const payload: { name: string; bytes: Uint8Array }[] = [];
        for (const file of picked) {
            // A pasted file already carries its bytes and has no path to read
            // from; a picked or dropped file is read from disk here so a large
            // batch is not held in memory twice.
            if (file.bytes) {
                payload.push({ name: file.name, bytes: file.bytes });
                continue;
            }
            try {
                payload.push({ name: file.name, bytes: await readFile(file.path) });
            } catch (error) {
                console.error(`Failed to read file ${file.name}:`, error);
                markFailed([file.name], 'Could not read this file from disk');
            }
        }
        if (payload.length === 0) return;

        try {
            const result = await processAttachments(payload);
            // Consume results per filename rather than looking each one up.
            //
            // Two picked files can share a name (same filename in different
            // folders). A plain `find` would match BOTH chips to the first
            // result, so the second file would silently adopt the first's
            // document id and never be attached. Shifting off a per-name queue
            // pairs them up one-to-one instead.
            const byName = new Map<string, typeof result.documents>();
            for (const d of result.documents) {
                const list = byName.get(d.filename) ?? [];
                list.push(d);
                byName.set(d.filename, list);
            }
            const submitted = new Set(payload.map(p => p.name));

            setAttachedFiles(prev =>
                prev.map(f => {
                    // Only files from THIS batch may be touched; chips attached
                    // earlier keep whatever state they already reached.
                    if (f.status !== 'processing' || !submitted.has(f.name)) return f;

                    const match = byName.get(f.name)?.shift();
                    if (!match) {
                        // No result came back for this file. Either it was
                        // rejected by type, or the server could not store it.
                        //
                        // Leaving it as 'processing' is NOT an option: the send
                        // button is disabled while anything is processing, so a
                        // stranded chip locks the composer permanently with no
                        // way out but removing it. Always resolve to a terminal
                        // state.
                        return {
                            ...f,
                            status: 'failed' as const,
                            error: result.rejected.includes(f.name)
                                ? 'Unsupported file type'
                                : 'The server did not return a result for this file',
                        };
                    }
                    return match.extractionStatus === 'ok'
                        ? {
                              ...f,
                              status: 'ready' as const,
                              documentId: match.documentId,
                              charCount: match.charCount,
                              reused: match.reused,
                              extractionEngine: match.extractionEngine,
                              // Bytes are no longer needed once the backend has
                              // the document; dropping the reference lets a
                              // pasted image be garbage collected instead of
                              // being pinned for the life of the composer.
                              bytes: undefined,
                          }
                        : {
                              ...f,
                              status: 'failed' as const,
                              documentId: match.documentId,
                              error: match.extractionError || 'Could not read this file',
                              bytes: undefined,
                          };
                }),
            );
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            console.error('Attachment processing failed:', message);
            markFailed(payload.map(p => p.name), message);
        }
    };

    /* ── One staging path, three entry points ─────────────────────────────────
       The paperclip dialog, a desktop drag-drop, and a clipboard paste all
       funnel through `stageFiles`. Previously only the dialog existed and its
       gating logic lived inline inside it; adding two more entry points without
       extracting this would have meant three copies of the count check, the
       size check and the failure messaging — and the size check has already
       been wrong once in this file's history.

       Rejections are reported through the toast system rather than `alert()`.
       `alert()` blocks the whole webview (it halts the SSE reader mid-stream if
       a reply is in flight), cannot be styled, and sat oddly next to the
       NotificationProvider this app already mounts. */
    const stageFiles = async (
        candidates: { name: string; path: string; size: number; bytes?: Uint8Array }[],
    ) => {
        if (candidates.length === 0) return;

        const { accepted, rejected } = partitionSupported(candidates);
        if (rejected.length > 0) {
            showWarning(
                rejected.length === 1 ? 'Unsupported file' : `${rejected.length} unsupported files`,
                `${rejected.map(f => f.name).join(', ')}. Accepted types are ${SUPPORTED_LIST_LABEL}.`,
            );
        }
        if (accepted.length === 0) return;

        const alreadyStaged = attachedFiles.length + localFileAttachments.length;
        if (alreadyStaged + accepted.length > MAX_ATTACHMENTS_PER_MESSAGE) {
            showWarning(
                `Up to ${MAX_ATTACHMENTS_PER_MESSAGE} files per message`,
                `Send these, then attach the rest in a follow-up. Documents stay available for the whole conversation once attached.`,
            );
            return;
        }

        // Already-attached files count toward the budget: they were sent in an
        // earlier request, but this component's tray is what the user reasons
        // about, and a second batch that pushes the tray past the limit is going
        // to fail on its own upload anyway.
        const totalBytes = totalBytesOf(attachedFiles) + totalBytesOf(accepted);
        if (exceedsUploadBudget(totalBytes)) {
            showWarning('Too much at once', uploadTooLargeMessage(totalBytes));
            return;
        }

        // Attaching a file means a question about it is coming.
        prewarmModel();
        const picked: NativeAttachedFile[] = accepted.map(f => ({ ...f, status: 'processing' as const }));

        // Show the files immediately as 'processing', then extract them in the
        // background. The user keeps typing throughout; extraction no longer
        // waits for the send button, the send button waits for it.
        setAttachedFiles(prev => [...prev, ...picked]);
        void processPickedFiles(picked);
    };

    const handleFileUpload = async () => {
        // Outside the desktop app there is no native dialog; the browser's own
        // file input opens the operating system's file manager instead.
        if (!('__TAURI_INTERNALS__' in window)) {
            browserFileInputRef.current?.click();
            return;
        }
        try {
            const selected = await openNativeFileDialog({
                multiple: true,
                filters: [{
                    name: 'Documents & Images',
                    // Shared with Local Storage and the backend upload gate -
                    // see supportedFormats.ts. Previously an inline literal
                    // here, which meant three copies of the same policy.
                    extensions: DIALOG_EXTENSIONS
                }]
            });
            if (!selected) return;
            const paths = Array.isArray(selected) ? selected : [selected];
            await stageFiles(await describePaths(paths));
        } catch (error) {
            console.error('File picker failed:', error);
            showError("Couldn't open file picker", error instanceof Error ? error.message : String(error));
        }
    };

    /** Resolve a list of absolute paths into staging candidates.
     *  Shared by the native dialog and drag-drop, which both deal in paths. */
    const describePaths = async (paths: string[]) => {
        const out: { name: string; path: string; size: number }[] = [];
        for (const path of paths) {
            const name = path.split(/[\\/]/).pop() || path;
            let size = 0;
            try {
                size = (await statFile(path)).size;
            } catch (error) {
                // A directory, or a path the app cannot stat. Size 0 keeps it
                // out of the byte budget; the format gate in stageFiles rejects
                // anything without a supported extension, which covers folders.
                console.warn(`Could not stat ${path}:`, error);
            }
            out.push({ name, path, size });
        }
        return out;
    };

    /* ── Desktop drag-and-drop ────────────────────────────────────────────────
       Uses Tauri's webview drag handler rather than HTML5 `onDrop`, for one
       decisive reason: HTML5 `dataTransfer` exposes File objects with NO
       absolute path (the browser deliberately withholds it), and this app
       records a document's on-disk location as provenance — that is the whole
       reason the paperclip uses the native dialog instead of an <input
       type="file">. Tauri's event carries real paths, so a dragged file gets
       the same provenance as a picked one.

       It also means HTML5 drag events never fire here: when the webview handles
       file drops natively it suppresses them. So there is no second code path
       to keep in sync, and no possibility of both firing for one drop. */
    useEffect(() => {
        let unlisten: (() => void) | undefined;
        let cancelled = false;

        (async () => {
            try {
                const { getCurrentWebview } = await import('@tauri-apps/api/webview');
                const stop = await getCurrentWebview().onDragDropEvent(event => {
                    if (event.payload.type === 'over') {
                        setIsDragging(true);
                    } else if (event.payload.type === 'leave') {
                        setIsDragging(false);
                    } else if (event.payload.type === 'drop') {
                        setIsDragging(false);
                        const paths = event.payload.paths ?? [];
                        if (paths.length > 0) {
                            void describePaths(paths).then(stageFiles);
                        }
                    }
                });
                if (cancelled) stop();
                else unlisten = stop;
            } catch (error) {
                // Non-Tauri context (a browser during development). Drag-drop is
                // simply unavailable there; every other entry point still works.
                console.warn('[ChatWindow] drag-and-drop unavailable:', error);
            }
        })();

        return () => {
            cancelled = true;
            unlisten?.();
        };
        // Re-registered when the staging inputs change so the handler always
        // closes over current tray state for its count and size checks.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [attachedFiles, localFileAttachments]);

    /* ── Paste to attach ──────────────────────────────────────────────────────
       A pasted screenshot is the fastest way to get a scanned page in front of
       the model, and it was not possible at all before this. Pasted files have
       no path by definition, so their bytes are carried on the staging record
       and `processPickedFiles` uses them directly instead of reading from disk. */
    /** Files chosen through the browser file input (the paperclip outside the
     *  desktop app): staged exactly like pasted files, from their bytes. */
    const handleBrowserFiles = async (list: FileList | null) => {
        const files = Array.from(list ?? []);
        if (files.length === 0) return;
        const candidates = await Promise.all(
            files.map(async file => ({
                name: file.name,
                path: '',
                size: file.size,
                bytes: new Uint8Array(await file.arrayBuffer()),
            })),
        );
        await stageFiles(candidates);
    };

    /** A Workspace draft or Library item, as an inline attachment: a draft as
     *  its current file's bytes (so edits since it was created are included),
     *  a Library item as its text. The backend stores and extracts both like
     *  any fresh file. */
    const fetchSourceAttachment = async (
        source: 'workspace' | 'library',
        entry: SourceEntry,
    ): Promise<ChatAttachment> => {
        if (source === 'library') {
            const item = await getLibraryItemText(entry.id);
            return { name: `${item.title}.txt`, content_text: item.text, mime_type: 'text/plain' };
        }
        const blob = await downloadDraft(entry.id);
        const bytes = new Uint8Array(await blob.arrayBuffer());
        let binary = '';
        for (let i = 0; i < bytes.length; i += 0x8000) {
            binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
        }
        return { name: entry.name, content_base64: btoa(binary), mime_type: blob.type || undefined };
    };

    /** Files checked in the source pane join the folder stack. Vault files go
     *  by id (the backend resolves them); Workspace and Library files are
     *  fetched one by one, and any that fail are named together. */
    const attachPicked = async (source: AttachSource, entries: SourceEntry[]) => {
        setFolderMenu(null);
        const room = MAX_ATTACHMENTS_PER_MESSAGE - attachedFiles.length - localFileAttachments.length;
        const take = entries
            .filter(e => !localFileAttachments.some(a => a.name === e.name))
            .slice(0, Math.max(room, 0));
        if (source === 'vault') {
            setLocalFileAttachments(prev => [...prev, ...take.map(e => ({ name: e.name, local_file_id: e.id }))]);
        } else {
            const failed: string[] = [];
            for (const entry of take) {
                try {
                    const attachment = await fetchSourceAttachment(source, entry);
                    setLocalFileAttachments(prev => [...prev, attachment]);
                } catch (error) {
                    console.error(`Failed to attach ${entry.name}:`, error);
                    failed.push(entry.name);
                }
            }
            if (failed.length > 0) {
                showError(
                    `Couldn't attach from ${source === 'workspace' ? 'Workspace' : 'Library'}`,
                    failed.join(', '),
                );
            }
        }
        setTimeout(() => inputRef.current?.focus(), 50);
    };

    const handlePaste = async (e: React.ClipboardEvent<HTMLInputElement | HTMLTextAreaElement>) => {
        const files = Array.from(e.clipboardData?.files ?? []);
        if (files.length === 0) return; // ordinary text paste
        e.preventDefault();

        const candidates = await Promise.all(
            files.map(async (file, i) => ({
                // A pasted bitmap arrives as "image.png" or with no useful name
                // at all, and several pasted in a row would collide in the
                // per-name result matching inside processPickedFiles. Stamp them.
                name: file.name || `pasted-${Date.now()}-${i}.png`,
                path: '',
                size: file.size,
                bytes: new Uint8Array(await file.arrayBuffer()),
            })),
        );
        await stageFiles(candidates);
    };

    const handleRemoveFile = (fileName: string) => {
        setRemovingFiles(prev => new Set(prev).add(fileName));
        setTimeout(() => {
            setAttachedFiles(prev => prev.filter(f => f.name !== fileName));
            setRemovingFiles(prev => {
                const next = new Set(prev);
                next.delete(fileName);
                return next;
            });
        }, 250); // matches chipSlideOut animation duration
    };

    const formatFileSize = (bytes: number) => {
        if (bytes < 1024) return `${bytes} B`;
        if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
        return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    };

    /** Extracted characters, abbreviated. A raw "184320 chars" is noise in a
     *  metadata line; "184k" conveys the same thing at a glance. */
    const formatCharCount = (chars: number) => {
        if (chars < 1000) return `${chars} chars`;
        if (chars < 1_000_000) return `${(chars / 1000).toFixed(chars < 10_000 ? 1 : 0)}k chars`;
        return `${(chars / 1_000_000).toFixed(1)}M chars`;
    };

    /** Short uppercase extension for the type glyph.
     *  Replaces the emoji map: emoji render differently on every platform,
     *  several of the old mappings collapsed unrelated formats onto one symbol
     *  (.txt and .docx both got 📝), and at 14px none of them were legible.
     *  The actual extension is unambiguous and aligns in a column. */
    const extensionLabel = (name: string) => {
        const ext = name.split('.').pop()?.toLowerCase() ?? '';
        if (!ext || ext === name.toLowerCase()) return 'FILE';
        // JPEG/JPG both read as JPG; anything longer than 4 chars is truncated
        // so the glyph box never has to grow.
        if (ext === 'jpeg') return 'JPG';
        return ext.slice(0, 4).toUpperCase();
    };

    // Every file in the pickers is drawn with the shared glyph, the same one
    // the Workspace, Vault and Library use.
    const getFileIcon = (name: string) => <FileGlyph format={formatOf(name)} size={FILE_ICON.list} />;


    // The capsule's status light, in three states. It is the only thing on the
    return (
        <div className="chat-window">
            <TypingSquares />
            {/* Drop overlay. `pointer-events: none` until .is-active, so it can
                never intercept a click on the chat beneath it. Rendered
                unconditionally rather than mounted on drag so the fade has
                something to transition from. */}
            <div className={`chat-dropzone${isDragging ? ' is-active' : ''}`} aria-hidden={!isDragging}>
                <div className="chat-dropzone__scrim" />
                <div className="chat-dropzone__label">
                    <svg className="chat-dropzone__icon" width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.5}>
                        <path strokeLinecap="round" strokeLinejoin="round" d="M12 16V4m0 0L8 8m4-4 4 4M4 16v2a2 2 0 002 2h12a2 2 0 002-2v-2" />
                    </svg>
                    <span className="chat-dropzone__title">Drop to attach</span>
                    <span className="chat-dropzone__hint">
                        {SUPPORTED_LIST_LABEL}. Each file is read as soon as it lands and stays
                        available for the whole conversation.
                    </span>
                </div>
            </div>
            <SaveTranscriptDialog
                open={showSaveDialog}
                defaultFileName={pendingDefaultName}
                content={pendingTranscript}
                onClose={() => setShowSaveDialog(false)}
                onSaved={() => setShowSaveDialog(false)}
            />
            {/* Aurora is the same compact bottom-right card here as on every
                other page (AssistantPanel, mounted in App.tsx). The large
                top-right search bar that used to stand in for it is gone. */}
            <SaveTranscriptWebDialog
                open={showWebSaveDialog}
                defaultFileName={pendingDefaultName}
                content={pendingTranscript}
                onClose={() => setShowWebSaveDialog(false)}
                onSaved={() => setShowWebSaveDialog(false)}
            />


            {/* Messages Area */}
            <main className="chat-messages" ref={chatScrollRef} onScroll={handleChatScroll}>
                <div className="chat-messages-container">
                    {/* Welcome Screen */}
                    {!hasMessages && (
                        <ChatHome
                            onUsePrompt={(prompt) => {
                                setInput(prompt);
                                // After the value lands, put the caret at the
                                // END of it. A staged prompt is a sentence the
                                // user is meant to finish, and focus() alone
                                // leaves the caret at position 0 — typing then
                                // prepends to it.
                                setTimeout(() => {
                                    const el = inputRef.current;
                                    if (!el) return;
                                    el.focus();
                                    el.setSelectionRange(prompt.length, prompt.length);
                                }, 0);
                            }}
                            onOpenModels={() => onOpenModels?.()}
                            onOpenLibrary={onOpenLibrary}
                            onOpenDraft={onOpenDraft}
                            onOpenLocalFiles={onOpenLocalFiles}
                            modelCount={models.length}
                        />
                    )}

                    {(() => {
                        // Computed once per render rather than per message.
                        // The action bar needs to know which turn is the last
                        // one, and `messages` still holds the system turns that
                        // are filtered out for display.
                        //
                        // Each turn also keeps its index in the FULL list, not
                        // just its position on screen: sources and attachment
                        // chips are both keyed by that absolute index, because
                        // it is what the backend records a turn as.
                        const visible = messages
                            .map((m, absolute) => ({ m, absolute }))
                            .filter(({ m }) => m.role !== 'system');
                        const lastIdx = visible.length - 1;
                        return visible.map(({ m: msg, absolute }, idx) => {
                        // Cut once, and used everywhere this answer is: Copy
                        // must not put a reprinted prompt on the clipboard
                        // either.
                        const answer =
                            msg.role === 'assistant'
                                ? stripAll(msg.content, scaffoldByTurn[absolute] ?? [])
                                : msg.content;
                        return (
                            <div
                                key={idx}
                                // Only the exchange just sent rises in; opening
                                // a saved chat does not replay every message.
                                className={`message-wrapper ${msg.role}${isLoading && idx >= lastIdx - 1 ? ' is-entering' : ''}`}
                            >
                                {msg.role === 'user' ? (
                                    <>
                                    <div className="message-bubble user">
                                        <MessageContent content={msg.content} role="user" />
                                        {/* What was sent with this message. The
                                            tray above the composer cleared on
                                            send; this is where that record now
                                            lives — the optimistic snapshot
                                            taken at send time, reconciled with
                                            what the store reports, so it shows
                                            immediately and survives a reload. */}
                                                                            </div>
                                    {/* What was sent with it: soft-rectangle
                                        file cards under the bubble, as in
                                        Harvey or ChatGPT. */}
                                    {chipsForTurn(absolute).length > 0 && (
                                            <div className="sent-attachments">
                                                {chipsForTurn(absolute).map(doc => (
                                                    <span
                                                        key={doc.id ?? doc.name}
                                                        className="file-capsule sent-chip"
                                                        title={doc.name}
                                                    >
                                                        <span className="sent-chip-glyph" aria-hidden="true">
                                                            <FileTypeIcon
                                                                format={extensionLabel(doc.name).toLowerCase()}
                                                                showLabel={false}
                                                            />
                                                        </span>
                                                        <span className="sent-chip-name">{doc.name.replace(/\.[^.]+$/, '')}</span>
                                                        <span className="sent-chip-ext">{extensionLabel(doc.name)}</span>
                                                    </span>
                                                ))}
                                            </div>
                                        )}
                                    </>
                                ) : (
                                    <div className="message-content-plain">
                                        {/* A small model often reprints the
                                            excerpts it was given before it
                                            answers. They are shown properly
                                            below instead, so the copy in the
                                            prose is cut — live, as it streams,
                                            and in chats already saved with one
                                            in them. */}
                                        <MessageContent
                                            content={answer}
                                            role="assistant"
                                            streaming={isLoading && idx === lastIdx}
                                        />
                                        {finishedTurn === absolute && (
                                            <span className="answer-done">
                                                <TriangleMark state="done" size={14} />
                                            </span>
                                        )}
                                        {/* One row under the answer: the sources stack
                                            (hanging in the strip the sidebar covers)
                                            and the actions, on the same line. */}
                                        <div className="answer-footer">
                                        {sourcesByTurn[absolute] && (
                                            <SourcesPanel
                                                sources={sourcesByTurn[absolute]}
                                            />
                                        )}
                                        {/* Never while the answer is still being
                                            written: Copy would take a half
                                            sentence and Retry would race the
                                            stream it is meant to replace. */}
                                        {answer && !(isLoading && idx === lastIdx) && (
                                            <MessageActions
                                                content={answer}
                                                isLast={idx === lastIdx}
                                                onRetry={idx === lastIdx ? handleRetry : undefined}
                                                onAsk={text =>
                                                    assistant.ask(
                                                        `Here is an answer from my Counsel thread. Help me work with it.

${text}`,
                                                    )
                                                }
                                            />
                                        )}
                                        </div>
                                    </div>
                                )}
                            </div>
                        );
                        });
                    })()}
                    {/* The "no model" prompt is a pill at the composer's
                        top-right (ModelChip), not a card in the thread. */}
                    {/* Until the first word: once the answer is writing, the
                        caret at its end is the sign of life instead. */}
                    {isLoading && !(
                        messages[messages.length - 1]?.role === 'assistant' &&
                        messages[messages.length - 1].content.trim()
                    ) && (
                        <div className="message-wrapper assistant is-entering">
                            <div className="loading-bubble">
                                <ThinkingStatus phase={thinkingPhase} />
                            </div>
                        </div>
                    )}
                    
                    <div ref={messagesEndRef} />
                    {/* Visible only once the user scrolls up: one click
                        re-sticks to the live output and glides to it. Sticky
                        to the scroller's bottom, so it floats over the
                        content without shifting layout. */}
                    {showJumpToLatest && (
                        <div className="chat-jump-wrap">
                            <button
                                type="button"
                                className="chat-jump-latest"
                                onClick={jumpToLatest}
                                aria-label="Jump to latest message"
                            >
                                <svg width="15" height="15" fill="none" stroke="currentColor" viewBox="0 0 24 24" aria-hidden="true">
                                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M19 14l-7 7m0 0l-7-7m7 7V3" />
                                </svg>
                                Latest
                            </button>
                        </div>
                    )}
                </div>
            </main>

            {/* Input Bar - no footer compartment */}
            <div className="chat-input-bar" style={{ position: 'relative' }}>
                {/* Attachment Tray - above input box.
                    Each staged document is a two-line card: type glyph, name, and
                    a metadata line that CHANGES WITH STATE. That last part is the
                    point — extraction happens asynchronously after picking, and
                    whether it succeeded is the only thing the user really needs
                    from this tray. The previous single-line chip could only fit
                    the word "unreadable", so the backend's actionable reason
                    ("save it as .docx and attach again") went to the console. */}
                {/* Attachments: two stacks on the composer's top-right corner,
                    on the same line as the folder and paperclip buttons at its
                    top-left — the folder stack (Vault / Library) first, then
                    the paperclip stack (files from this computer), mirroring
                    the order of those buttons, each as compact
                    soft-rectangle chips one under another. Detail (size,
                    characters read, index state, OCR notes) is in each chip's
                    tooltip, so every chip is the same quiet shape; reading
                    shows a progress line, failure a red edge. */}
                {(attachedFiles.length > 0 || localFileAttachments.length > 0 || modelPrompt) && (
                    <div className="attachment-stacks" aria-label="Attached files">
                        {modelPrompt && (
                            <ModelChip
                                readiness={modelPrompt}
                                onOpenModels={() => onOpenModels?.()}
                                onReady={handleModelReady}
                            />
                        )}
                        {/* Every attached file, wherever it came from, in ONE
                            capsule centred above the composer: an icon per file
                            type (PDF, Word, Excel…) and the count. It opens the
                            tray with the files themselves. Hidden while the
                            Vault / Workspace / Library pills are out, which use
                            the same row. */}
                        {(localFileAttachments.length > 0 || attachedFiles.length > 0) && folderMenu !== 'menu' && (() => {
                            const names = [...localFileAttachments.map(f => f.name), ...attachedFiles.map(f => f.name)];
                            // One entry per file type, with how many of it.
                            const types = new Map<string, number>();
                            for (const n of names) {
                                const t = extensionLabel(n);
                                types.set(t, (types.get(t) ?? 0) + 1);
                            }
                            const reading = attachedFiles.some(f => f.status === 'processing');
                            const failed = attachedFiles.some(f => f.status === 'failed');
                            return (
                                <button
                                    type="button"
                                    className={`file-capsule attach-capsule${reading ? ' is-reading' : ''}${failed ? ' is-error' : ''}`}
                                    onClick={() => setTrayOpen(o => !o)}
                                    aria-expanded={trayOpen}
                                    title={names.join(', ')}
                                    aria-label={`${names.length} attached file${names.length === 1 ? '' : 's'}: ${[...types.keys()].join(', ')}`}
                                >
                                    {[...types.keys()].map(t => (
                                        <span key={t} className="file-capsule__type">
                                            <FileGlyph format={t.toLowerCase()} size={FILE_ICON.list} />
                                            {t}
                                        </span>
                                    ))}
                                    {reading && <span className="file-capsule__state">Reading…</span>}
                                </button>
                            );
                        })()}
                        {/* The capsule, opened: the same spot, grown into a small
                            box listing the files (three in view, the rest scroll). */}
                {trayOpen && (localFileAttachments.length > 0 || attachedFiles.length > 0) && (
                    <aside className="attach-tray attach-tray--from-capsule" ref={trayRef} aria-label="Attached files">
                        <div className="attach-tray__head">
                            Attached Files
                        </div>
                        <div className="attach-tray__body">
                            {localFileAttachments.length > 0 && (
                                <section className="attach-tray__group">
                        {localFileAttachments.map((file, index) => (
                            <div key={`local-${file.name}-${index}`} className="stack-chip is-ready" title={`${file.name} · from your Vault`}>
                                <FileGlyph format={formatOf(file.name)} size={FILE_ICON.list} />
                                <span className="stack-chip__name">{file.name}</span>
                                <button
                                    type="button"
                                    className="stack-chip__remove"
                                    onClick={() => removeLocalFileAttachment(file.name)}
                                    aria-label={`Remove ${file.name}`}
                                >
                                    <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.5}><path d="M18 6L6 18M6 6l12 12" /></svg>
                                </button>
                            </div>
                        ))}
                                </section>
                            )}
                            {attachedFiles.length > 0 && (
                                <section className="attach-tray__group">
                        {attachedFiles.map((file, index) => (
                            <div
                                key={`${file.name}-${index}`}
                                className={`stack-chip ${
                                    file.status === 'processing'
                                        ? 'is-extracting'
                                        : file.status === 'failed'
                                          ? 'is-failed'
                                          : 'is-ready'
                                }`}
                                title={
                                    file.status === 'failed'
                                        ? `${file.name}: ${file.error ?? 'could not be read'}`
                                        : file.status === 'processing'
                                          ? `${file.name}: reading document…`
                                          : [
                                                file.name,
                                                formatFileSize(file.size),
                                                file.charCount != null && file.charCount > 0
                                                    ? `${formatCharCount(file.charCount)} characters read`
                                                    : null,
                                                file.reused ? 'already indexed from an earlier attachment' : null,
                                                file.extractionEngine === 'vision_model' ? 'read by the vision model' : null,
                                                file.extractionEngine === 'windows_ocr' && /\.(png|jpe?g|bmp|tiff?|gif)$/i.test(file.name)
                                                    ? 'read with basic OCR, which cannot reliably read handwriting; activate a vision model on the Models page'
                                                    : null,
                                            ]
                                                .filter(Boolean)
                                                .join(' · ')
                                }
                            >
                                <FileGlyph format={formatOf(file.name)} size={FILE_ICON.list} />
                                <span className="stack-chip__name">{file.name}</span>
                                {file.status === 'failed' && (
                                    <span className="stack-chip__flag" aria-label="Could not be read">!</span>
                                )}
                                {file.documentId !== undefined && file.status === 'ready' && (
                                    <IndexStatusChip documentId={file.documentId} dot />
                                )}
                                <button
                                    type="button"
                                    className="stack-chip__remove"
                                    onClick={() => handleRemoveFile(file.name)}
                                    aria-label={`Remove ${file.name}`}
                                >
                                    <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.5}><path d="M18 6L6 18M6 6l12 12" /></svg>
                                </button>
                                {file.status === 'processing' && (
                                    <span className="stack-chip__progress" aria-hidden="true" />
                                )}
                            </div>
                        ))}
                                </section>
                            )}
                        </div>
                    </aside>
                )}
                    </div>
                )}
                {queuedMessages.length > 0 && (
                    <div className="attachment-tray-above-input">
                        {queuedMessages.map((item) => (
                            <div key={item.id} className="attachment-chip queued">
                                <span className="attachment-icon">⏳</span>
                                <span className="attachment-name" title={item.content}>
                                    {item.content.length > 50 ? item.content.slice(0, 50) + '...' : item.content}
                                </span>
                            </div>
                        ))}
                        <span className="attachment-count">{queuedMessages.length} queued</span>
                    </div>
                )}
                <input
                    ref={browserFileInputRef}
                    type="file"
                    multiple
                    accept={DIALOG_EXTENSIONS.map(e => `.${e}`).join(',')}
                    style={{ display: 'none' }}
                    onChange={e => {
                        void handleBrowserFiles(e.target.files);
                        e.target.value = '';
                    }}
                />
                <form onSubmit={handleSubmit} className="chat-input-form">
                    <div className={`chat-input-pill${isLoading || isSessionStreaming ? ' is-busy' : ''}`}>
                        <button
                            type="button"
                            className={`input-icon-btn input-attach-btn${attachedFiles.length > 0 ? ' has-items' : ''}`}
                            onClick={handleFileUpload}
                            title="Attach file"
                            aria-label="Attach file"
                        >
                            <PaperclipIcon size={15} />
                        </button>
                        {/* Vault, Workspace and Library: pills on the tool row,
                            right beside the folder and paperclip. Positioned
                            against the box itself, so they sit above it. */}
                        {folderMenu === 'menu' && (
                            <SourcePills onClose={() => setFolderMenu(null)} onPick={setFolderMenu} />
                        )}
                        {/* On Computer | On Cloud, at the tool row's right. */}
                        <AnswerSourceToggle />
                        {/* Folder: Vault, Workspace and Library, as pills beside it. */}
                        <button
                            type="button"
                            className={`input-icon-btn input-folder-btn ${folderMenu ? 'attach-btn-active' : ''}${localFileAttachments.length > 0 ? ' has-items' : ''}`}
                            onClick={() => {
                                setFolderMenu(m => (m ? null : 'menu'));
                            }}
                            title="Attach from Vault, Workspace or Library"
                            aria-label="Attach from Vault, Workspace or Library"
                            aria-haspopup="menu"
                            aria-expanded={folderMenu !== null}
                        >
                            <svg width="18" height="18" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                            </svg>
                        </button>
                        <div className="chat-input-field">
                            <textarea
                                ref={inputRef}
                                rows={1}
                                value={input}
                                onChange={handleInputChange}
                                onKeyDown={handleInputKeyDown}
                                onPaste={handlePaste}
                                placeholder={modelSwitching ? 'Switching models… please wait' : 'Ask Counsel anything...'}
                                disabled={modelSwitching}
                                className="chat-input"
                            />
                            {/* @ picker — stage 1: files already attached to this conversation, plus a "From Local Storage" escape hatch */}
                            {showFileAutocomplete && pickerStage === 'session' && (
                                <div ref={fileAutocompleteRef} className="file-autocomplete-dropdown">
                                    <div className="file-autocomplete-header">Attached in this conversation</div>
                                    {filteredSessionDocuments.length === 0 ? (
                                        <div style={{ padding: '10px 12px', color: 'var(--text-muted)', fontSize: '13px' }}>
                                            No files attached in this conversation yet
                                        </div>
                                    ) : (
                                        filteredSessionDocuments.map((doc, idx) => (
                                            <div
                                                key={doc.id}
                                                className={`file-autocomplete-item ${idx === fileAutocompleteIndex ? 'selected' : ''}`}
                                                onClick={() => insertExistingDocumentAsAttachment(doc)}
                                                onMouseEnter={() => setFileAutocompleteIndex(idx)}
                                            >
                                                <span className="file-autocomplete-icon">{getFileIcon(doc.name)}</span>
                                                <span className="file-autocomplete-name">{doc.name}</span>
                                            </div>
                                        ))
                                    )}
                                    <div
                                        className="file-autocomplete-item"
                                        onClick={openBrowseStage}
                                        style={{ borderTop: '1px solid var(--border-primary)', fontWeight: 'var(--fw-medium)', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '8px', padding: '8px 12px' }}
                                    >
                                        <span className="file-autocomplete-icon"><FolderGlyph size={FILE_ICON.list} /></span>
                                        <span className="file-autocomplete-name">From Vault →</span>
                                    </div>
                                </div>
                            )}
                            {/* @ picker — stage 2: browse Local Storage folders/files */}
                            {showFileAutocomplete && pickerStage === 'browse' && (
                                <div ref={fileAutocompleteRef} className="file-autocomplete-dropdown" style={{ maxHeight: '320px' }}>
                                    <div className="file-autocomplete-header" style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
                                        <span onClick={backToSessionStage} style={{ cursor: 'pointer' }}>← Back</span>
                                        <span>Vault</span>
                                    </div>
                                    {folderStack.length > 0 && (
                                        <div
                                            className="file-autocomplete-item"
                                            onClick={goBackFolder}
                                            style={{ padding: '8px 12px', cursor: 'pointer', color: 'var(--text-muted)' }}
                                        >
                                            .. (up one level)
                                        </div>
                                    )}
                                    <input
                                        ref={browseInputRef}
                                        type="text"
                                        placeholder="Search this folder..."
                                        value={browseQuery}
                                        onChange={(e) => { setBrowseQuery(e.target.value); setFileAutocompleteIndex(0); }}
                                        onKeyDown={handleBrowseInputKeyDown}
                                        style={{ width: 'calc(100% - 24px)', margin: '8px 12px', padding: '6px 8px', borderRadius: '4px', border: '1px solid var(--border-primary)', backgroundColor: 'var(--bg-input)', color: 'var(--text-primary)', fontSize: '13px', outline: 'none' }}
                                        autoFocus
                                    />
                                    <div style={{ maxHeight: '200px', overflowY: 'auto' }}>
                                        {filteredBrowseList.length === 0 ? (
                                            <div style={{ padding: '16px', textAlign: 'center', color: 'var(--text-muted)', fontSize: '13px' }}>
                                                {currentBrowseList.length === 0 ? 'This folder is empty' : 'No matching files'}
                                            </div>
                                        ) : (
                                            filteredBrowseList.map((file, idx) => (
                                                <div
                                                    key={file.id}
                                                    className={`file-autocomplete-item ${idx === fileAutocompleteIndex ? 'selected' : ''}`}
                                                    onClick={() => insertBrowsedFileAsAttachment(file)}
                                                    onMouseEnter={() => setFileAutocompleteIndex(idx)}
                                                    style={{ padding: '8px 12px', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '8px' }}
                                                >
                                                    <span className="file-autocomplete-icon">{file.isDirectory ? <FolderGlyph size={FILE_ICON.list} /> : getFileIcon(file.name)}</span>
                                                    <span className="file-autocomplete-name">{file.name}</span>
                                                </div>
                                            ))
                                        )}
                                    </div>
                                </div>
                            )}
                            {folderMenu && folderMenu !== 'menu' && (
                                <SourcePane
                                    source={folderMenu}
                                    room={MAX_ATTACHMENTS_PER_MESSAGE - attachedFiles.length - localFileAttachments.length}
                                    attached={localFileAttachments.map(a => a.name)}
                                    onClose={() => setFolderMenu(null)}
                                    onAttach={entries => void attachPicked(folderMenu, entries)}
                                />
                            )}
                        </div>
                        {/* The model capsule and its menu.
                            One control for both states (a model loaded, or none yet): the
                            label changes, the menu does not. The menu opens UPWARD and is
                            centred on the capsule, because the capsule lives at the bottom
                            of the composer. The caret is the open/closed indicator — it
                            points up at rest (the menu will rise) and flips down while the
                            menu is showing. Only two model slots are offered here: the one
                            in use and one other installed model. This is a switcher, not a
                            catalogue; the catalogue is one row below, on the Models page. */}
                        {/* The same capsule the drafts composer carries:
                            one shared selection (ModelSelectionContext). */}
                        {/* Rough size of this conversation in tokens (about four
                            characters each), for the capsule's context gauge. */}
                        <ModelCapsule contextUsed={Math.round(messages.reduce((n, m) => n + (m.content?.length ?? 0), input.length) / 4)} />
                        {isLoading || isSessionStreaming ? (
                            <button type="button" className="input-send-btn stop" onClick={() => { onStopStream?.(); abortControllerRef.current?.abort(); }} title="Stop" aria-label="Stop">
                                <svg className="input-send-btn__arrow" width="13" height="13" fill="currentColor" viewBox="0 0 24 24" aria-hidden="true">
                                    <rect x="6" y="6" width="12" height="12" rx="2" />
                                </svg>
                            </button>
                        ) : (
                            <button
                                type="submit"
                                // Also held while an attachment is still being
                                // extracted. handleSend refuses in that state, so
                                // without this the button would look active and do
                                // nothing - the tooltip says which files we are
                                // waiting on rather than leaving the user guessing.
                                disabled={!input.trim() || attachedFiles.some(f => f.status === 'processing')}
                                title={
                                    attachedFiles.some(f => f.status === 'processing')
                                        ? `Still reading ${attachedFiles
                                              .filter(f => f.status === 'processing')
                                              .map(f => f.name)
                                              .join(', ')}`
                                        : undefined
                                }
                                className="input-send-btn"
                                aria-label="Send"
                            >
                                <span className="input-send-btn__label">Send</span>
                            </button>
                        )}
                    </div>
                    <p className="chat-disclaimer">AI can make mistakes. Please verify important information.</p>
                </form>
            </div>

        </div>
    );
}
