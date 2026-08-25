import React, { useState, useRef, useEffect } from 'react';
import type { Message, ChatAttachment } from '../api/chat';
import { streamChat, updateConversationTitle } from '../api/chat';
import { getApiBaseSync } from '../api/backendUrl';
import { open as openNativeFileDialog } from '@tauri-apps/plugin-dialog';
import { readFile, stat as statFile } from '@tauri-apps/plugin-fs';
import { DIALOG_EXTENSIONS, SUPPORTED_LIST_LABEL, partitionSupported } from '../supportedFormats';
import { exceedsUploadBudget, totalBytesOf, uploadTooLargeMessage } from '../uploadLimits';
import { processAttachments } from '../api/attachments';
import { useNotificationHelpers } from '../contexts/NotificationContext';

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

export interface Chat {
    id: string;
    title: string;
    messages: Message[];
    createdAt: Date;
    pinned?: boolean;
    saved?: boolean;
}

interface LocalFileEntry {
    id: number;
    name: string;
    path: string;
    is_directory: boolean;
    isDirectory?: boolean;
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
    onSelectedModelChange?: (model: { id: string; name: string; source: 'local' } | null) => void;
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
    onSelectedModelChange,
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
}) => {
    const firstPromptSent = useRef(false);
    const mountedRef = useRef(true);
    const abortControllerRef = useRef<AbortController | null>(null);
    const [input, setInput] = useState('');
    const [isLoading, setIsLoading] = useState(false);
    const messagesEndRef = useRef<HTMLDivElement>(null);
    const [isDropdownOpen, setIsDropdownOpen] = useState(false);
    const [isModelDropdownOpen, setIsModelDropdownOpen] = useState(false);
    const dropdownRef = useRef<HTMLDivElement>(null);
    const modelDropdownRef = useRef<HTMLDivElement>(null);
    const [attachedFiles, setAttachedFiles] = useState<NativeAttachedFile[]>([]);
    const [localFileAttachments, setLocalFileAttachments] = useState<ChatAttachment[]>([]);
    const [removingFiles, setRemovingFiles] = useState<Set<string>>(new Set());
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
    const inputRef = useRef<HTMLInputElement>(null);
    const fileAutocompleteRef = useRef<HTMLDivElement>(null);
    // @ picker stage 2's own search box - needs its own ref so keyboard nav
    // and refocus-after-navigation work even though it's a separate <input>
    // from the main chat input (autoFocus only fires on its initial mount).
    const browseInputRef = useRef<HTMLInputElement>(null);
    // Show curated files picker panel (separate from @autocomplete)
    const [showCuratedPicker, setShowCuratedPicker] = useState(false);
    const [curatedPickerFiles, setCuratedPickerFiles] = useState<Array<{ id: number; name: string; path: string; is_directory: boolean }>>([]);
    const [curatedPickerQuery, setCuratedPickerQuery] = useState('');
    const curatedPickerRef = useRef<HTMLDivElement>(null);

    // @ picker, stage 1: every document already attached to THIS conversation
    // (paperclip AND Local Storage alike) - sourced from GET
    // /documents/session/:id, which already existed but was never wired to
    // the frontend. local_file_id is null for paperclip-only documents.
    interface SessionDocumentEntry { id: number; name: string; local_file_id: number | null; }
    const [sessionDocuments, setSessionDocuments] = useState<SessionDocumentEntry[]>([]);

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

    useEffect(() => {
        const handleClickOutside = (event: MouseEvent) => {
            if (dropdownRef.current && !dropdownRef.current.contains(event.target as Node)) {
                setIsDropdownOpen(false);
            }
            if (modelDropdownRef.current && !modelDropdownRef.current.contains(event.target as Node)) {
                setIsModelDropdownOpen(false);
            }
        };
        if (isDropdownOpen || isModelDropdownOpen) {
            document.addEventListener('mousedown', handleClickOutside);
            return () => document.removeEventListener('mousedown', handleClickOutside);
        }
    }, [isDropdownOpen, isModelDropdownOpen]);

    const scrollToBottom = () => {
        if (messagesEndRef.current && typeof messagesEndRef.current.scrollIntoView === 'function') {
            messagesEndRef.current.scrollIntoView({ behavior: 'smooth' });
        }
    };

    useEffect(() => { scrollToBottom(); }, [messages]);

    // Fetch all_files for the folder-icon curated picker.
    // Uses /files/all — user-managed Local Storage files for context inclusion
    const fetchCuratedFiles = React.useCallback(async () => {
        try {
            const allFilesResponse = await fetch(`${getApiBaseSync()}/files/all`);

            if (allFilesResponse.ok) {
                const allFilesData = await allFilesResponse.json();
                const allFiles = Array.isArray(allFilesData) ? allFilesData : [];
                // Filter out directories - only show files
                const filesOnly = allFiles.filter((f: LocalFileEntry) => !f.isDirectory && !f.is_directory).map((f: any) => {
                    f.source = 'local_files';
                    return f;
                });
                setCuratedPickerFiles(filesOnly);
            }
        } catch (error) {
            console.error('Failed to fetch files:', error);
        }
    }, []);

    useEffect(() => {
        if (!backendReady) return;
        fetchCuratedFiles();
    }, [backendReady, fetchCuratedFiles]);

    // Fetch every document already attached to THIS conversation (paperclip
    // and Local Storage alike) for the @ picker's stage 1. Refetched on
    // session change and after every send, so a freshly-attached paperclip
    // file appears in @ on the next lookup within the same session.
    const fetchSessionDocuments = React.useCallback(async () => {
        if (!sessionId) {
            setSessionDocuments([]);
            return;
        }
        try {
            const resp = await fetch(`${getApiBaseSync()}/documents/session/${sessionId}`);
            if (resp.ok) {
                const docs = await resp.json();
                const list: SessionDocumentEntry[] = Array.isArray(docs)
                    ? docs.map((d: any) => ({
                        id: d.id,
                        name: d.original_filename,
                        local_file_id: d.local_file_id ?? null,
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

    // Handle click outside for curated files picker panel
    useEffect(() => {
        const handleClickOutside = (event: MouseEvent) => {
            if (curatedPickerRef.current && !curatedPickerRef.current.contains(event.target as Node)) {
                setShowCuratedPicker(false);
            }
        };
        if (showCuratedPicker) {
            document.addEventListener('mousedown', handleClickOutside);
            return () => document.removeEventListener('mousedown', handleClickOutside);
        }
    }, [showCuratedPicker]);

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
    const handleInputChange = (e: React.ChangeEvent<HTMLInputElement>) => {
        const value = e.target.value;
        setInput(value);

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
    const handleInputKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
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
                'Send these, then attach the rest in a follow-up — documents stay available for the whole conversation once attached.',
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
                'Send these, then attach the rest in a follow-up — documents stay available for the whole conversation once attached.',
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

    // Insert curated file as attachment from folder-icon picker. Same
    // local_file_id-based resolution as insertBrowsedFileAsAttachment.
    const insertCuratedFileAsAttachment = async (file: { id: number; name: string; is_directory: boolean }) => {
        if (file.is_directory) return;

        if (attachedFiles.length + localFileAttachments.length >= MAX_ATTACHMENTS_PER_MESSAGE) {
            showWarning(
                `Up to ${MAX_ATTACHMENTS_PER_MESSAGE} files per message`,
                'Send these, then attach the rest in a follow-up — documents stay available for the whole conversation once attached.',
            );
        } else {
            const newAttachment: ChatAttachment = {
                name: file.name,
                local_file_id: file.id,
            };
            setLocalFileAttachments(prev => [...prev, newAttachment]);
            console.log('[DEBUG] Added curated file attachment:', file.name);
        }

        setShowCuratedPicker(false);
        fetchCuratedFiles();
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

    // Filtered curated picker list
    const filteredCuratedPicker = curatedPickerFiles.filter(f =>
        f.name.toLowerCase().includes(curatedPickerQuery.toLowerCase())
    );

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

        onMessagesUpdate(newMessages);
        setInput('');
        setAttachedFiles([]);
        setLocalFileAttachments([]);
        setIsLoading(true);
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

            for await (const chunk of streamChat(
                newMessages,
                currentSessionId!,
                selectedModel?.id,
                allAttachments.length > 0 ? allAttachments : undefined,
                abortController.signal
            )) {
                fullContent += chunk;
                onBackgroundStreamUpdate?.(currentSessionId!, fullContent);
                if (mountedRef.current) {
                    onMessagesUpdate([...newMessages, { role: 'assistant' as const, content: fullContent }]);
                }
            }
        } catch (error: unknown) {
            if (error instanceof DOMException && error.name === 'AbortError') return;
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
            // name on the next message, in this same session.
            fetchSessionDocuments();
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
    // insertBrowsedFileAsAttachment / insertCuratedFileAsAttachment) - all of
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
                `${rejected.map(f => f.name).join(', ')} — accepted types are ${SUPPORTED_LIST_LABEL}.`,
            );
        }
        if (accepted.length === 0) return;

        const alreadyStaged = attachedFiles.length + localFileAttachments.length;
        if (alreadyStaged + accepted.length > MAX_ATTACHMENTS_PER_MESSAGE) {
            showWarning(
                `Up to ${MAX_ATTACHMENTS_PER_MESSAGE} files per message`,
                `Send these, then attach the rest in a follow-up — documents stay available for the whole conversation once attached.`,
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

        const picked: NativeAttachedFile[] = accepted.map(f => ({ ...f, status: 'processing' as const }));

        // Show the files immediately as 'processing', then extract them in the
        // background. The user keeps typing throughout; extraction no longer
        // waits for the send button, the send button waits for it.
        setAttachedFiles(prev => [...prev, ...picked]);
        void processPickedFiles(picked);
    };

    const handleFileUpload = async () => {
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
            showError('Could not open the file picker', error instanceof Error ? error.message : String(error));
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

    const getFileIcon = (name: string) => {
        const ext = name.split('.').pop()?.toLowerCase() || '';
        if (['pdf'].includes(ext)) return '📄';
        if (['doc', 'docx', 'rtf', 'odt', 'txt'].includes(ext)) return '📝';
        if (['xls', 'xlsx', 'csv', 'ods'].includes(ext)) return '📊';
        if (['ppt', 'pptx', 'odp'].includes(ext)) return '📽️';
        if (['py'].includes(ext)) return '🐍';
        if (['js', 'ts', 'jsx', 'tsx'].includes(ext)) return '⚡';
        if (['rs'].includes(ext)) return '🦀';
        if (['go'].includes(ext)) return '🔷';
        if (['html', 'css', 'scss'].includes(ext)) return '🌐';
        if (['json', 'xml', 'yaml', 'yml'].includes(ext)) return '📋';
        if (['md'].includes(ext)) return '📑';
        if (['sh', 'bat', 'ps1'].includes(ext)) return '⚙️';
        return '📎';
    };

    const hasOfflineCapability = models.length > 0;
    const showModelPromptBanner = !selectedModel && !hasOfflineCapability;

    return (
        <div className="chat-window">
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
                        {SUPPORTED_LIST_LABEL} — each file is read as soon as it lands and stays
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
            <SaveTranscriptWebDialog
                open={showWebSaveDialog}
                defaultFileName={pendingDefaultName}
                content={pendingTranscript}
                onClose={() => setShowWebSaveDialog(false)}
                onSaved={() => setShowWebSaveDialog(false)}
            />

            {/* Header */}
            <header className="chat-header">
                <div className="chat-header-bar centered">
                    {/* Left: Chat Title or empty - Kept empty for plain appearance */}
                    <div className="chat-header-left">
                    </div>

                    {/* Right: Model indicator + Save Transcript + Actions */}
                    <div className="header-actions">
                        {/* Selected Model Indicator */}
                        {selectedModel && (
                            <div style={{ position: 'relative' }} ref={modelDropdownRef}>
                                {/* Fixed-width chip: the name ellipses rather than wrapping, so
                                    the header's layout never shifts with the model id's length.
                                    The full name is in `title` so truncation loses nothing. */}
                                <button
                                    type="button"
                                    className="model-capsule"
                                    onClick={() => setIsModelDropdownOpen(!isModelDropdownOpen)}
                                    title={`${selectedModel.name} — click to change model`}
                                    aria-haspopup="listbox"
                                    aria-expanded={isModelDropdownOpen}
                                >
                                    <span className="model-capsule__dot" aria-hidden="true" />
                                    <span className="model-capsule__name">{selectedModel.name}</span>
                                </button>

                                {isModelDropdownOpen && (
                                    <div className="dropdown-menu" style={{ top: '100%', marginTop: '4px', minWidth: '220px' }}>
                                        {models.length > 0 && (
                                            <>
                                                <div style={{ padding: '8px 12px', fontSize: '11px', color: 'var(--text-primary)', fontWeight: 'bold', textTransform: 'uppercase', letterSpacing: '0.5px', borderBottom: '1px solid var(--bg-tertiary)' }}>
                                                    ● Local Models
                                                </div>
                                                {models.slice(0, 9).map((model) => (
                                                    <button
                                                        key={model.id}
                                                        className="dropdown-item"
                                                        style={{
                                                            fontSize: '12px',
                                                            padding: '3px 10px',
                                                            borderRadius: '999px',
                                                            backgroundColor: selectedModel.id === model.id ? 'var(--bg-tertiary)' : 'var(--bg-secondary)',
                                                            color: selectedModel.id === model.id ? 'var(--text-primary)' : 'var(--text-secondary)',
                                                            border: 'none',
                                                            cursor: 'pointer',
                                                            textAlign: 'left',
                                                            width: '100%',
                                                            marginBottom: '4px'
                                                        }}
                                                        onClick={() => {
                                                            onSelectedModelChange?.({ id: model.id, name: model.name, source: 'local' });
                                                            setIsModelDropdownOpen(false);
                                                        }}
                                                    >
                                                        <span>{model.name}</span>
                                                    </button>
                                                ))}
                                                {models.length > 9 && (
                                                    <button
                                                        onClick={() => { setIsModelDropdownOpen(false); onOpenModels?.(); }}
                                                        style={{ display: 'block', width: '100%', padding: '6px 12px', fontSize: '11px', color: 'var(--text-secondary)', background: 'none', border: 'none', cursor: 'pointer', textAlign: 'left', borderTop: '1px solid var(--bg-tertiary)', marginTop: '2px' }}
                                                    >
                                                        View all models →
                                                    </button>
                                                )}
                                            </>
                                        )}
                                        {models.length === 0 && (
                                            <div style={{ padding: '12px', fontSize: '12px', color: 'var(--text-secondary)', textAlign: 'center' }}>
                                                <p style={{ margin: '0 0 8px 0' }}>No models installed.</p>
                                                <button
                                                    onClick={() => { setIsModelDropdownOpen(false); onOpenModels?.(); }}
                                                    style={{ fontSize: '12px', padding: '6px 16px', borderRadius: '9999px', backgroundColor: 'var(--accent)', color: 'var(--text-on-accent)', border: 'none', cursor: 'pointer', fontWeight: 500 }}
                                                >
                                                    Browse & Download
                                                </button>
                                            </div>
                                        )}
                                    </div>
                                )}
                            </div>
                        )}

                        {/* Show capsule when no model selected */}
                        {!selectedModel && (
                            <div style={{ position: 'relative' }} ref={modelDropdownRef}>
                                {/* Same fixed-width chip in the no-model state, so the header
                                    does not reflow the moment a model is chosen. The dot is
                                    dimmed rather than green: nothing is loaded yet. */}
                                <button
                                    type="button"
                                    className="model-capsule"
                                    onClick={() => setIsModelDropdownOpen(!isModelDropdownOpen)}
                                    title="Click to select a model"
                                    aria-haspopup="listbox"
                                    aria-expanded={isModelDropdownOpen}
                                >
                                    <span className="model-capsule__dot model-capsule__dot--idle" aria-hidden="true" />
                                    <span className="model-capsule__name">Browse Models</span>
                                </button>
                                {isModelDropdownOpen && (
                                    <div className="dropdown-menu" style={{ top: '100%', marginTop: '4px', minWidth: '220px' }}>
                                        {models.length > 0 ? (
                                            <>
                                                <div style={{ padding: '8px 12px', fontSize: '11px', color: 'var(--text-primary)', fontWeight: 'bold', textTransform: 'uppercase', letterSpacing: '0.5px', borderBottom: '1px solid var(--bg-tertiary)' }}>
                                                    ● Local Models
                                                </div>
                                                {models.slice(0, 9).map((model) => (
                                                    <button
                                                        key={model.id}
                                                        className="dropdown-item"
                                                        style={{ fontSize: '12px', padding: '6px 12px', border: 'none', cursor: 'pointer', textAlign: 'left', width: '100%', marginBottom: '2px' }}
                                                        onClick={() => {
                                                            onSelectedModelChange?.({ id: model.id, name: model.name, source: 'local' });
                                                            setIsModelDropdownOpen(false);
                                                        }}
                                                    >
                                                        <span>{model.name}</span>
                                                    </button>
                                                ))}
                                                {models.length > 9 && (
                                                    <button
                                                        onClick={() => { setIsModelDropdownOpen(false); onOpenModels?.(); }}
                                                        style={{ display: 'block', width: '100%', padding: '6px 12px', fontSize: '11px', color: 'var(--text-secondary)', background: 'none', border: 'none', cursor: 'pointer', textAlign: 'left', borderTop: '1px solid var(--bg-tertiary)', marginTop: '2px' }}
                                                    >
                                                        View all models →
                                                    </button>
                                                )}
                                            </>
                                        ) : (
                                            <div style={{ padding: '12px', fontSize: '12px', color: 'var(--text-secondary)', textAlign: 'center' }}>
                                                <p style={{ margin: '0 0 8px 0' }}>No models installed.</p>
                                                <button
                                                    onClick={() => { setIsModelDropdownOpen(false); onOpenModels?.(); }}
                                                    style={{ fontSize: '12px', padding: '6px 16px', borderRadius: '9999px', backgroundColor: 'var(--accent)', color: 'var(--text-on-accent)', border: 'none', cursor: 'pointer', fontWeight: 500 }}
                                                >
                                                    Browse & Download
                                                </button>
                                            </div>
                                        )}
                                    </div>
                                )}
                            </div>
                        )}

                        {/* Download/Save Transcript Button */}
                        {hasMessages && (
                            <button
                                type="button"
                                className="header-capsule-btn"
                                onClick={handleSaveTranscript}
                                title="Save transcript"
                                style={{ display: 'inline-flex', alignItems: 'center', gap: '6px' }}
                            >
                                <svg width="16" height="16" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4" />
                                </svg>
                                <span>Save</span>
                            </button>
                        )}

                        {chatId && (
                            <div style={{ position: 'relative' }} ref={dropdownRef}>
                                <button
                                    type="button"
                                    className="header-capsule-btn"
                                    aria-label="More options"
                                    onClick={() => setIsDropdownOpen(!isDropdownOpen)}
                                >
                                    <svg width="16" height="16" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                        <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 5.5a1.5 1.5 0 110-3 1.5 1.5 0 010 3zm0 8a1.5 1.5 0 110-3 1.5 1.5 0 010 3zm0 8a1.5 1.5 0 110-3 1.5 1.5 0 010 3z" />
                                    </svg>
                                </button>
                                {isDropdownOpen && (
                                    <div className="dropdown-menu">
                                        <button className="dropdown-item" onClick={() => { onPinChat?.(chatId); setIsDropdownOpen(false); }}>
                                            <svg className="icon" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 5a2 2 0 012-2h10a2 2 0 012 2v16l-7-3.5L5 21V5z" />
                                            </svg>
                                            <span>{isPinned ? 'Unpin chat' : 'Pin chat'}</span>
                                        </button>
                                        <button className="dropdown-item delete" onClick={() => { setIsDropdownOpen(false); onRequestDeleteChat?.(chatId); }}>
                                            <svg className="icon" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
                                            </svg>
                                            <span>Delete</span>
                                        </button>
                                    </div>
                                )}
                            </div>
                        )}
                    </div>
                </div>
            </header>

            {/* Messages Area */}
            <main className="chat-messages">
                <div className="chat-messages-container">
                    {/* Welcome Screen */}
                    {!hasMessages && (
                        <div className="chat-welcome">
                            <h1 className="chat-welcome-title">Offline Counsel</h1>
                        </div>
                    )}

                    {messages.filter(m => m.role !== 'system').map((msg, idx) => (
                        <div key={idx} className={`message-wrapper ${msg.role}`}>
                            {msg.role === 'user' ? (
                                <div className="message-bubble user">
                                    <MessageContent content={msg.content} role="user" />
                                </div>
                            ) : (
                                <div className="message-content-plain">
                                    <MessageContent content={msg.content} role="assistant" />
                                </div>
                            )}
                        </div>
                    ))}
                    {isLoading && (
                        <div className="message-wrapper assistant">
                            <div className="loading-bubble">
                                <div className="loading-dot" />
                                <div className="loading-dot" />
                                <div className="loading-dot" />
                            </div>
                        </div>
                    )}
                    
                    <div ref={messagesEndRef} />
                </div>
            </main>

            {/* Input Bar - no footer compartment */}
            <div className="chat-input-bar" style={{ position: 'relative' }}>
                {/* Model Prompt Banner - shown based on user setup status */}
                {showModelPromptBanner && (
                    <div style={{ padding: '12px 16px', backgroundColor: 'var(--bg-tertiary)', borderRadius: '8px', marginBottom: '8px', textAlign: 'center', fontSize: '13px', color: 'var(--text-secondary)' }}>
                        <p style={{ margin: '0 0 8px 0' }}>No local models installed. Download a model to get started.</p>
                        <button
                            onClick={() => onOpenModels?.()}
                            style={{ fontSize: '12px', padding: '6px 16px', borderRadius: '9999px', backgroundColor: 'var(--accent)', color: 'var(--text-on-accent)', border: 'none', cursor: 'pointer', fontWeight: 500 }}
                        >
                            Browse & Download Models
                        </button>
                    </div>
                )}
                {/* Attachment Tray - above input box.
                    Each staged document is a two-line card: type glyph, name, and
                    a metadata line that CHANGES WITH STATE. That last part is the
                    point — extraction happens asynchronously after picking, and
                    whether it succeeded is the only thing the user really needs
                    from this tray. The previous single-line chip could only fit
                    the word "unreadable", so the backend's actionable reason
                    ("save it as .docx and attach again") went to the console. */}
                {(attachedFiles.length > 0 || localFileAttachments.length > 0) && (
                    <div className="attachment-tray-above-input">
                        {attachedFiles.map((file, index) => (
                            <div
                                key={`${file.name}-${index}`}
                                className={`attachment-chip ${
                                    file.status === 'processing'
                                        ? 'is-extracting'
                                        : file.status === 'failed'
                                          ? 'is-failed'
                                          : 'is-ready'
                                }`}
                            >
                                <span className="attachment-glyph" aria-hidden="true">
                                    {extensionLabel(file.name)}
                                </span>
                                <span className="attachment-body">
                                    <span className="attachment-name" title={file.name}>{file.name}</span>
                                    {file.status === 'failed' ? (
                                        <span className="attachment-error" title={file.error}>
                                            {file.error ?? 'Could not be read'}
                                        </span>
                                    ) : (
                                        <span className="attachment-meta">
                                            {file.status === 'processing' ? (
                                                <>Reading document…</>
                                            ) : (
                                                <>
                                                    {formatFileSize(file.size)}
                                                    {/* Character count is the only direct evidence the
                                                        document was READ and not merely uploaded. */}
                                                    {file.charCount != null && file.charCount > 0 && (
                                                        <>
                                                            <span className="attachment-meta__sep">·</span>
                                                            {formatCharCount(file.charCount)} extracted
                                                        </>
                                                    )}
                                                    {file.reused && (
                                                        <>
                                                            <span className="attachment-meta__sep">·</span>
                                                            already indexed
                                                        </>
                                                    )}
                                                    {file.extractionEngine === 'vision_model' && (
                                                        <>
                                                            <span className="attachment-meta__sep">·</span>
                                                            read by vision model
                                                        </>
                                                    )}
                                                </>
                                            )}
                                        </span>
                                    )}
                                    {/* Product decision (2026-08-08): when an image was read
                                        with basic OCR because no vision model is active, say
                                        so — basic OCR cannot read handwriting, and the user
                                        deserves to know before relying on the answer. */}
                                    {file.status === 'ready' &&
                                        file.extractionEngine === 'windows_ocr' &&
                                        /\.(png|jpe?g|bmp|tiff?|gif)$/i.test(file.name) && (
                                            <span
                                                className="attachment-ocr-note"
                                                title="This image was read with basic OCR, which cannot reliably read handwriting. Install and activate a vision model from the Models page to extract handwritten content."
                                            >
                                                No vision model used — activate a vision model for handwritten content
                                            </span>
                                        )}
                                </span>
                                <button
                                    type="button"
                                    className="attachment-remove"
                                    onClick={() => handleRemoveFile(file.name)}
                                    aria-label={`Remove ${file.name}`}
                                >
                                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.5}><path d="M18 6L6 18M6 6l12 12" /></svg>
                                </button>
                                {file.status === 'processing' && (
                                    <span className="attachment-chip__progress" aria-hidden="true" />
                                )}
                            </div>
                        ))}
                        {/* Local Storage references. These are already extracted —
                            they resolve by id and are never re-read — so they go
                            straight to the ready state. */}
                        {localFileAttachments.map((file, index) => (
                            <div key={`local-${file.name}-${index}`} className="attachment-chip is-ready">
                                <span className="attachment-glyph" aria-hidden="true">
                                    {extensionLabel(file.name)}
                                </span>
                                <span className="attachment-body">
                                    <span className="attachment-name" title={file.name}>{file.name}</span>
                                    <span className="attachment-meta">From Vault</span>
                                </span>
                                <button
                                    type="button"
                                    className="attachment-remove"
                                    onClick={() => removeLocalFileAttachment(file.name)}
                                    aria-label={`Remove ${file.name}`}
                                >
                                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.5}><path d="M18 6L6 18M6 6l12 12" /></svg>
                                </button>
                            </div>
                        ))}
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
                <form onSubmit={handleSubmit} className="chat-input-form">
                    <div className="chat-input-pill">
                        <button
                            type="button"
                            className="input-icon-btn"
                            onClick={handleFileUpload}
                            title="Attach file"
                        >
                            <svg width="18" height="18" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15.172 7l-6.586 6.586a2 2 0 102.828 2.828l6.414-6.586a4 4 0 00-5.656-5.656l-6.415 6.585a6 6 0 108.486 8.486L20.5 13" />
                            </svg>
                        </button>
                        {/* Curated files picker button — always visible, opens picker panel */}
                        <button
                            type="button"
                            className={`input-icon-btn ${showCuratedPicker ? 'attach-btn-active' : ''}`}
                            onClick={() => {
                                setCuratedPickerQuery('');
                                fetchCuratedFiles();
                                setShowCuratedPicker(!showCuratedPicker);
                            }}
                            title="Insert from Vault"
                        >
                            <svg width="18" height="18" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                            </svg>
                        </button>
                        <div style={{ position: 'relative', flex: 1 }}>
                            <input
                                ref={inputRef}
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
                                        style={{ borderTop: '1px solid var(--border-primary)', fontWeight: 500, cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '8px', padding: '8px 12px' }}
                                    >
                                        <span className="file-autocomplete-icon">📁</span>
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
                                                    <span className="file-autocomplete-icon">{file.isDirectory ? '📁' : getFileIcon(file.name)}</span>
                                                    <span className="file-autocomplete-name">{file.name}</span>
                                                </div>
                                            ))
                                        )}
                                    </div>
                                </div>
                            )}
                            {/* Curated files picker panel - dropdown above input */}
                            {showCuratedPicker && (
                                <div ref={curatedPickerRef} className="file-autocomplete-dropdown" style={{ position: 'absolute', bottom: '100%', left: 0, right: 0, marginBottom: '8px', maxHeight: '300px' }}>
                                    <div className="file-autocomplete-header" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                                        <span>Vault Files</span>
                                        <span style={{ fontSize: '11px', fontWeight: 400, opacity: 0.7 }}>Click to insert @filename</span>
                                    </div>
                                    <input
                                        type="text"
                                        placeholder="Search files..."
                                        value={curatedPickerQuery}
                                        onChange={(e) => setCuratedPickerQuery(e.target.value)}
                                        style={{ width: 'calc(100% - 24px)', margin: '8px 12px', padding: '6px 8px', borderRadius: '4px', border: '1px solid var(--border-primary)', backgroundColor: 'var(--bg-input)', color: 'var(--text-primary)', fontSize: '13px', outline: 'none' }}
                                        autoFocus
                                    />
                                    <div style={{ maxHeight: '200px', overflowY: 'auto' }}>
                                        {filteredCuratedPicker.length === 0 ? (
                                            <div style={{ padding: '16px', textAlign: 'center', color: 'var(--text-muted)', fontSize: '13px' }}>
                                                {curatedPickerFiles.length === 0 ? 'No files in Vault' : 'No matching files'}
                                            </div>
                                        ) : (
                                            filteredCuratedPicker.map((file, idx) => (
                                                <div
                                                    key={file.id}
                                                    className="file-autocomplete-item"
                                                    onClick={() => insertCuratedFileAsAttachment(file)}
                                                    style={{ padding: '8px 12px', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '8px' }}
                                                >
                                                    <span className="file-autocomplete-icon">{getFileIcon(file.name)}</span>
                                                    <span className="file-autocomplete-name">{file.name}</span>
                                                </div>
                                            ))
                                        )}
                                    </div>
                                    {curatedPickerFiles.length > 0 && (
                                        <div style={{ padding: '8px 12px', borderTop: '1px solid var(--border-primary)', fontSize: '11px', color: 'var(--text-muted)' }}>
                                            {curatedPickerFiles.length} file(s) available in Vault
                                        </div>
                                    )}
                                </div>
                            )}
                        </div>
                        {isLoading || isSessionStreaming ? (
                            <button type="button" className="input-send-btn stop" onClick={() => { onStopStream?.(); abortControllerRef.current?.abort(); }} title="Stop">
                                <svg width="16" height="16" fill="currentColor" viewBox="0 0 24 24">
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
                            >
                                <svg width="16" height="16" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 10l7-7m0 0l7 7m-7-7v18" />
                                </svg>
                            </button>
                        )}
                    </div>
                    <p className="chat-disclaimer">AI can make mistakes. Please verify important information.</p>
                </form>
            </div>


        </div>
    );
}
