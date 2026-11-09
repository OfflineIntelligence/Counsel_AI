import { Suspense, lazy, useState, useEffect, useMemo, useRef, useCallback } from 'react'
import { ChatWindow } from './components/ChatWindow'
import { WhatsNewDialog } from './components/WhatsNewDialog'
import { Sidebar } from './components/Sidebar'
import { ModelSelectionProvider } from './contexts/ModelSelectionContext'
import { useDownloads } from './api/modelSync';
import {
  MODELS_CHANGED_EVENT,
  discardInterruptedDownload,
  formatBytes,
  getInterruptedDownloads,
  reportWindowFocus,
  chooseModel,
  resumeInterruptedDownload,
} from './api/models'
import { useNotifications } from './contexts/NotificationContext'
import TitleBar from './components/TitleBar'
import { listDrafts, listFolders } from './api/drafts'
import { listLibrary } from './api/library'
import { OPEN_SOURCE_EVENT, nameKey, type OpenSourceRequest } from './utils/openSource'
import { SearchModal } from './components/SearchModal'
import NotificationsContainer from './components/Notifications'


/* ── The six pages that are not the chat, loaded when first visited ──────────
   `activeView` is a switch over seven views, and six of them are reachable
   only by a deliberate click in the rail. They were all imported eagerly, so
   starting the app parsed Models (1.6k lines), the Workspace and its five
   document editors, the Vault, the Library, Settings and Help before drawing
   the chat — the one view that actually appears.

   ChatWindow stays a static import: it is the default view, so deferring it
   would only add a round trip to the thing everyone sees first. */
const ModelsPanel = lazy(() => import('./components/ModelsPanel'))
const SettingsPanel = lazy(() => import('./components/SettingsPanel'))
const LocalFilesPanel = lazy(() => import('./components/LocalFilesPanel'))
const DraftWorkspace = lazy(() => import('./components/workspace/DraftWorkspace'))
const LibraryPanel = lazy(() => import('./components/workspace/LibraryPanel'))
import LoginModal from './components/LoginModal'
import LoginPage from './components/LoginPage'
import FeedbackPage from './components/FeedbackPage'
import { useAuth } from './contexts/AuthContext'
import { useAssistant, type AssistantPage } from './contexts/AssistantContext'
import type { Chat } from './components/ChatWindow'
import type { Message } from './api/chat'
import { fetchConversations, fetchConversation, deleteConversation, updateConversationPinned } from './api/chat'
import { SYSTEM_PROMPT } from './systemPrompt'
import { getApiBaseSync } from './api/backendUrl'
import { getAllApiKeys, migrateKeysFromLocalStorage } from './api/apiKeys'
import { syncAnswerSource } from './api/cloud'
import { getHfToken, setHfToken as storeHfToken } from './api/hfToken'
import { useConfirm } from './contexts/confirm'
import ModelDialogHost from './components/models/ModelDialogs'
import ViewBoundary from './components/ViewBoundary'
import './App.css'
import './styles/buttons.css'

interface SimpleModel {
    id: string;
    name: string;
}

function App() {
  return <MainApp />;
}

function MainApp() {
  const { user, isLoggedIn } = useAuth();
  const [chats, setChats] = useState<Chat[]>([]);
  const [activeChatId, setActiveChatId] = useState<string | null>(null);
  const [currentSessionId, setCurrentSessionId] = useState<string | null>(null);
  const [currentMessages, setCurrentMessages] = useState<Message[]>([
    { role: 'system', content: SYSTEM_PROMPT }
  ]);
  const [currentChatTitle, setCurrentChatTitle] = useState<string | null>(null);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(() => {
    // Restore active session from localStorage on app load
    return localStorage.getItem('offline-intelligence-active-session');
  });
  const [isSearchOpen, setIsSearchOpen] = useState(false);
  const [activeView, setActiveView] = useState<'chat' | 'models' | 'settings' | 'help' | 'localfiles' | 'draft' | 'library' | 'login' | 'feedback'>('chat');

  // ── "Go to this source" (a source clicked under an answer) ────────────────
  // Resolves where the cited document lives and switches to that screen:
  // Vault files open in the Vault's viewer at the passage, Workspace drafts
  // open in the editor, Library items are selected in the Library. A file
  // attached to a chat only has no other home, so that is said instead.
  const [vaultFocus, setVaultFocus] = useState<OpenSourceRequest | null>(null);
  const [libraryFocus, setLibraryFocus] = useState<number | null>(null);
  const clearVaultFocus = useCallback(() => setVaultFocus(null), []);
  const clearLibraryFocus = useCallback(() => setLibraryFocus(null), []);
  useEffect(() => {
    const onOpen = async (e: Event) => {
      const req = (e as CustomEvent<OpenSourceRequest>).detail;
      try {
        const resp = await fetch(`${getApiBaseSync()}/documents/${req.documentId}`);
        if (!resp.ok) throw new Error(await resp.text());
        const meta: { source_kind?: string; local_file_id?: number | null; original_filename?: string } = await resp.json();
        const name = meta.original_filename ?? req.filename;
        if (meta.source_kind === 'local_storage' || meta.local_file_id != null) {
          setVaultFocus({ ...req, filename: name });
          setActiveView('localfiles');
          return;
        }
        if (meta.source_kind === 'draft') {
          const drafts = await listDrafts();
          const hit = drafts.find(d => d.source_document_id === req.documentId)
            ?? drafts.find(d => nameKey(`${d.title}.${d.format}`) === nameKey(name));
          if (hit) {
            setDraftToOpen(hit.id);
            setActiveView('draft');
            return;
          }
        }
        if (meta.source_kind === 'knowledge') {
          const items = await listLibrary();
          const hit = items.find(i => nameKey(i.title) === nameKey(name));
          if (hit) {
            setLibraryFocus(hit.id);
            setActiveView('library');
            return;
          }
        }
        addNotification({
          type: 'info',
          title: 'Attached in chat only',
          message: `${name} was attached to a conversation and isn't stored in your Vault, Workspace or Library.`,
          duration: 6000,
        });
      } catch (err) {
        addNotification({
          type: 'error',
          title: "Couldn't open source",
          message: err instanceof Error ? err.message : String(err),
          duration: 6000,
        });
      }
    };
    window.addEventListener(OPEN_SOURCE_EVENT, onOpen);
    return () => window.removeEventListener(OPEN_SOURCE_EVENT, onOpen);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  // A document picked on the Library page, handed to the Workspace to open.
  const [draftToOpen, setDraftToOpen] = useState<number | null>(null);
  // Top-level Workspace folders, listed as Spaces in the sidebar.
  const [spaces, setSpaces] = useState<{ id: number; name: string }[]>([]);
  const [folderToOpen, setFolderToOpen] = useState<number | null>(null);
  const [recentDrafts, setRecentDrafts] = useState<{ id: number; title: string }[]>([]);
  const assistant = useAssistant();
  const { setDefaultPage } = assistant;

  // What the AI Assistant knows about each page. Views with something more
  // specific (an open draft) register over this while mounted.
  useEffect(() => {
    const pages: Record<typeof activeView, AssistantPage> = {
      chat: {
        label: 'Chat',
        title: currentChatTitle ?? undefined,
        // The visible discussion, so "summarise this conversation" works.
        text: currentMessages
          .filter(m => m.role !== 'system')
          .slice(-12)
          .map(m => `${m.role === 'user' ? 'User' : 'Assistant'}: ${m.content}`)
          .join('\n\n') || undefined,
        suggestions: ['Summarise this conversation', 'List the open questions', 'Turn this into a client email'],
      },
      draft: { label: 'Workspace', suggestions: ['Which document should I start from for an NDA?', 'Draft a short engagement letter'] },
      library: { label: 'Library', suggestions: ['What makes a good precedent?', 'Draft a governing-law clause'] },
      localfiles: { label: 'Vault', suggestions: ['How should I organise matter files?', 'What should an index of documents include?'] },
      models: { label: 'Models', suggestions: ['Which model size suits my computer?', 'What does quantisation change?'] },
      settings: { label: 'Settings' },
      help: { label: 'Help' },
      login: { label: 'Sign in' },
      feedback: { label: 'Feedback' },
    };
    setDefaultPage(pages[activeView]);
  }, [activeView, currentChatTitle, currentMessages, setDefaultPage]);

  // Report window focus so the backend unloads the model sooner while the app
  // sits in the background.
  useEffect(() => {
    const report = () => {
      const visible = document.visibilityState === 'visible';
      const active = document.hasFocus() && visible;
      document.documentElement.dataset.appActive = active ? 'true' : 'false';
      // index.css pauses CSS animations only while the window is HIDDEN
      // (minimised, or behind a fullscreen app). Pausing on mere blur froze
      // one-shot entrances at their first frame — opacity 0 — so a window
      // that was visible but unfocused (a second monitor, a side-by-side app)
      // showed blank pages until it was clicked.
      document.documentElement.dataset.appHidden = visible ? 'false' : 'true';
      reportWindowFocus(active);
    };
    window.addEventListener('focus', report);
    window.addEventListener('blur', report);
    document.addEventListener('visibilitychange', report);
    report();
    return () => {
      window.removeEventListener('focus', report);
      window.removeEventListener('blur', report);
      document.removeEventListener('visibilitychange', report);
    };
  }, []);
  useEffect(() => {
    if (activeView !== 'draft') return;
    let cancelled = false;
    Promise.all([listFolders(), listDrafts()])
      .then(([folders, drafts]) => {
        if (cancelled) return;
        setSpaces(folders.filter(f => f.parent_id === null).map(f => ({ id: f.id, name: f.name })));
        setRecentDrafts(
          [...drafts]
            .sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at))
            .map(d => ({ id: d.id, title: d.title })),
        );
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [activeView]);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  // On Computer / On Cloud lives in the backend; restore it there on start.
  useEffect(() => { void syncAnswerSource(); }, []);
  const [questionCount, setQuestionCount] = useState(0);
  const [showLoginModal, setShowLoginModal] = useState(false);
  // A computer that already has an account asks for the user name and password
  // at launch, until someone has signed in.
  const [loginGate, setLoginGate] = useState(false);
  useEffect(() => {
    let live = true;
    fetch(`${getApiBaseSync()}/auth/status`)
      .then(r => (r.ok ? r.json() : null))
      .then(s => { if (live && s?.has_users) setLoginGate(true); })
      .catch(() => {});
    return () => { live = false; };
  }, []);
  const [selectedModel, setSelectedModel] = useState<{
    id: string;
    name: string;
    source: 'local';
  } | null>(() => {
    try {
      const saved = localStorage.getItem('offline-intelligence-selected-model');
      return saved ? JSON.parse(saved) : null;
    } catch { return null; }
  });

  const [hfToken, setHfToken] = useState<string>(() => {
    // Get from browser storage first (single canonical key)
    const storedToken = getHfToken();
    if (storedToken) return storedToken;
    // Then try from user context
    return user?.apiKeys?.huggingface || '';
  });

  const [shouldFocusHfToken, setShouldFocusHfToken] = useState(false);
  const [availableModels, setAvailableModels] = useState<SimpleModel[]>([]);

  // Streaming background content ref — accumulates chunks even after component unmount
  const streamingContentRef = useRef<Record<string, string>>({});
  // Track which sessions are actively streaming
  const [sessionStreaming, setSessionStreaming] = useState<Set<string>>(new Set());
  // Per-session message queue
  const [sessionQueues, setSessionQueues] = useState<Record<string, Array<{ id: string; content: string }>>>({});
  const nextQueueIdRef = useRef(0);
  // Pending queue execution trigger
  const [pendingQueueRun, setPendingQueueRun] = useState<{ sessionId: string; content: string } | null>(null);
  // Deleting a chat asks through the app's own confirmation (ConfirmProvider),
  // the same dialog every other destructive action in the app uses.
  const confirm = useConfirm();
  const [chatWindowKey, setChatWindowKey] = useState(0);

  // Sync API keys with auth context
  useEffect(() => {
    if (user?.apiKeys?.huggingface) {
      setHfToken(user.apiKeys.huggingface);
    }
  }, [user]);

  // Sync hfToken to auth context when it changes
  useEffect(() => {
    if (user && hfToken !== user.apiKeys?.huggingface) {
      // Update auth context with new hfToken
      // We need to call setApiKey but it's not available here directly
      // So we rely on components to update the context when values change
    }
  }, [user, hfToken]);

  // Persist active session to localStorage
  useEffect(() => {
    if (activeSessionId) {
      localStorage.setItem('offline-intelligence-active-session', activeSessionId);
    }
  }, [activeSessionId]);

  useEffect(() => {
    localStorage.setItem('offline-intelligence-selected-model', JSON.stringify(selectedModel));
  }, [selectedModel]);

  useEffect(() => {
    storeHfToken(hfToken);
  }, [hfToken]);

  useEffect(() => {
    if (currentSessionId && currentSessionId !== lastWrittenSessionRef.current) {
      lastWrittenSessionRef.current = currentSessionId;
      setActiveSessionId(currentSessionId);
    }
  }, [currentSessionId]);

  useEffect(() => {
    if (currentSessionId) {
      const firstUserMessage = currentMessages.find(m => m.role === 'user');
      const chatTitle = firstUserMessage 
        ? firstUserMessage.content.slice(0, 50) + (firstUserMessage.content.length > 50 ? '...' : '')
        : (currentChatTitle || 'New conversation');
      
      setChats(prev => {
        if (!prev.find(c => c.id === currentSessionId)) {
          return [...prev, {
            id: currentSessionId,
            title: chatTitle,
            messages: currentMessages,
            createdAt: new Date(),
            pinned: false,
          }];
        }
        return prev;
      });
    }
  }, [currentSessionId, currentMessages]);

  // On first mount: migrate any localStorage keys to the backend, then load backend
  // keys into component state so keys saved in previous sessions are available immediately.
  useEffect(() => {
    const syncKeysFromBackend = async () => {
      try {
        // One-time migration: move localStorage keys → encrypted backend DB
        await migrateKeysFromLocalStorage();
        // Pull all stored keys from backend (decrypted) into component state
        const keys = await getAllApiKeys();
        keys.forEach(k => {
          if (k.key_type === 'huggingface' && k.value) {
            setHfToken(prev => prev || k.value!);
          }
        });
      } catch {
        // Non-fatal: backend may not be ready yet; user can re-enter keys manually
      }
    };
    syncKeysFromBackend();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Downloads interrupted by closing the app: offer to continue each one from
  // where it stopped. Asked once per launch; the Models page's Downloads tab
  // lists them too, for anyone who dismisses this.
  const { addNotification } = useNotifications()
  useEffect(() => {
    let cancelled = false
    getInterruptedDownloads()
      .then(list => {
        if (cancelled) return
        for (const d of list) {
          const progress = d.size_bytes
            ? `${formatBytes(d.bytes_on_disk)} of ${formatBytes(d.size_bytes)}`
            : formatBytes(d.bytes_on_disk)
          addNotification({
            type: 'download',
            title: 'Resume download?',
            message: `${d.model_name} paused at ${progress}.`,
            duration: 0,
            actions: [
              {
                label: 'Resume',
                primary: true,
                onClick: () => {
                  void resumeInterruptedDownload(d).then(result => {
                    if (!result.ok) {
                      addNotification({ type: 'error', title: "Couldn't resume download", message: result.message, duration: 0 })
                    }
                  })
                },
              },
              { label: 'Discard', onClick: () => void discardInterruptedDownload(d.model_id) },
            ],
          })
        }
      })
      .catch(() => {
        /* Nothing to offer if the backend cannot say. */
      })
    return () => {
      cancelled = true
    }
  }, []) // eslint-disable-line react-hooks/exhaustive-deps

  // Global download progress tracking for notification bubble
  // One shared watcher for every download (api/modelSync.ts).
  const globalDownloads = useDownloads();
  const [showDownloadBubble, setShowDownloadBubble] = useState(true);


  const activeGlobalDownloads = globalDownloads.filter(d => d.status === 'Downloading' || d.status === 'Starting');

  // Where the agent lives on this view: the search bar in chat, the glass
  // panel from the right on every other page.
  // One placement everywhere: the compact card at the bottom-right.
  const auroraPlacement = 'card' as const;

  // Opening Aurora collapses the sidebar, from any trigger (rail button,
  // 'Ask Aurora', a page), so the panel has the room it takes.
  useEffect(() => {
    if (assistant.open && activeView !== 'chat') setSidebarOpen(false);
  }, [assistant.open, activeView]);
  // The draft editor keeps the sidebar shut. Clicking the rail twice in a
  // moment leaves the document for the Workspace list instead.
  const draftEditing = useRef(false);
  const railClicks = useRef<number[]>([]);
  useEffect(() => {
    const onEditor = (e: Event) => {
      draftEditing.current = (e as CustomEvent).detail === true;
      if (draftEditing.current) setSidebarOpen(false);
    };
    window.addEventListener('oca-draft-editor', onEditor);
    return () => window.removeEventListener('oca-draft-editor', onEditor);
  }, []);
  // Settings and Help & Support also keep the sidebar shut; two quick clicks
  // on the rail return to the chat window.
  const railLocked = activeView === 'settings' || activeView === 'help';
  useEffect(() => {
    if (railLocked) setSidebarOpen(false);
  }, [railLocked]);
  // Typing in the composer hides the sidebar; after 120 s without typing it
  // comes back, but only if typing is what hid it.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let hidByTyping = false;
    const onTyping = () => {
      setSidebarOpen(open => {
        if (open) hidByTyping = true;
        return false;
      });
      clearTimeout(timer);
      timer = setTimeout(() => {
        if (hidByTyping) setSidebarOpen(true);
        hidByTyping = false;
      }, 120_000);
    };
    window.addEventListener('oca-composer-typing', onTyping);
    return () => {
      window.removeEventListener('oca-composer-typing', onTyping);
      clearTimeout(timer);
    };
  }, []);
  const toggleRail = () => {
    if (draftEditing.current || railLocked) {
      const now = Date.now();
      railClicks.current = [...railClicks.current.filter(t => now - t < 1500), now];
      if (railClicks.current.length >= 2) {
        railClicks.current = [];
        if (railLocked) {
          setActiveView('chat');
          setSidebarOpen(false);
        } else {
          window.dispatchEvent(new Event('oca-draft-back'));
        }
      }
      return;
    }
    setSidebarOpen(open => !open);
  };
  // Whether the download notification bubble is on screen.
  const downloadsVisible = activeView !== 'models' && activeGlobalDownloads.length > 0 && showDownloadBubble;

  // The bubble's measured height, published as --dl-bubble-h for the panel's
  // docked top. Measured live (ResizeObserver) rather than estimated, so the
  // card always clears one, two or three download rows exactly.
  const bubbleRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    const el = bubbleRef.current;
    if (!el) {
      document.documentElement.style.removeProperty('--dl-bubble-h');
      return;
    }
    const publish = () => {
      document.documentElement.style.setProperty('--dl-bubble-h', `${el.offsetHeight}px`);
    };
    publish();
    const ro = new ResizeObserver(publish);
    ro.observe(el);
    return () => {
      ro.disconnect();
      document.documentElement.style.removeProperty('--dl-bubble-h');
    };
  }, [downloadsVisible, activeGlobalDownloads.length]);

  /* The Activity panel was removed from the product (2026-08-08). Its
     settings state (activityEnabled / autoDeletePeriod) and its bulk
     delete-by-age handler went with it — nothing rendered them, and keeping
     write-only localStorage keys around implies a feature that no longer
     exists. Per-conversation delete is unaffected (sidebar + chat header). */

  // Load conversations on mount (pure async - no retries)
  useEffect(() => {
    const loadConversations = async () => {
      try {
        const conversations = await fetchConversations();
        const loadedChats: Chat[] = conversations.map(conv => ({
          id: conv.id,
          title: conv.title,
          messages: [],
          createdAt: new Date(conv.created_at),
          pinned: conv.pinned,
        }));
        setChats(loadedChats);
        
        // Restore active session if we have one saved
        const savedSessionId = localStorage.getItem('offline-intelligence-active-session');
        if (savedSessionId) {
          const existingChat = loadedChats.find(c => c.id === savedSessionId);
          if (existingChat) {
            // Load the saved session's messages
            const conversation = await fetchConversation(savedSessionId);
            if (conversation && conversation.messages.length > 0) {
              const messages = conversation.messages[0]?.role === 'system'
                ? conversation.messages
                : [{ role: 'system' as const, content: SYSTEM_PROMPT }, ...conversation.messages];
              setCurrentMessages(messages);
              setActiveChatId(savedSessionId);
              setCurrentSessionId(savedSessionId);
              setActiveSessionId(savedSessionId);
              setCurrentChatTitle(existingChat.title);
              // Update the chat with loaded messages
              setChats(prev => prev.map(c => c.id === savedSessionId ? { ...c, messages } : c));
            }
          } else {
            // Saved session no longer exists, clear it
            localStorage.removeItem('offline-intelligence-active-session');
          }
        }
      } catch (error) {
        console.error('Failed to load conversations:', error);
        // User can refresh or conversations will load when backend is ready
      }
    };
    loadConversations();
  }, []);

  useEffect(() => {
    const fetchModels = async () => {
      try {
        const [modelsResponse, activeResponse] = await Promise.all([
          fetch(`${getApiBaseSync()}/models`),
          fetch(`${getApiBaseSync()}/models/active`).catch(() => null)
        ]);
        if (!modelsResponse.ok) return;
        const models = await modelsResponse.json();
        const installedModels = models.filter((m: any) => m.status === 'Installed');
        const modelList: SimpleModel[] = installedModels.map((m: any) => ({
          id: m.id,
          name: m.name
        }));

        let activeModelName: string | null = null;
        if (activeResponse?.ok) {
          const activeData = await activeResponse.json().catch(() => null);
          if (activeData?.status === 'loaded' && activeData?.model_name) {
            activeModelName = activeData.model_name;
            if (!modelList.some(m => m.name === activeModelName)) {
              modelList.unshift({ id: `active:${activeModelName}`, name: `${activeModelName} (running)` });
            }
          }
        }

        setAvailableModels(modelList);

        // Functional update: this runs again on every models-changed event,
        // so it must read the CURRENT selection, not the one captured when
        // the effect was first created.
        setSelectedModel(prev => {
          if (prev && modelList.some(m => m.id === prev.id)) return prev;
          if (installedModels.length > 0) {
            return { id: installedModels[0].id, name: installedModels[0].name, source: 'local' };
          }
          if (activeModelName) {
            return { id: `active:${activeModelName}`, name: activeModelName, source: 'local' };
          }
          return null;
        });
      } catch (error) {
        console.error('Failed to fetch models:', error);
      }
    };
    fetchModels();
    // A download finishing, a model being loaded from the chat's setup
    // prompt, or a switch on the Models page all announce this.
    window.addEventListener(MODELS_CHANGED_EVENT, fetchModels);
    return () => window.removeEventListener(MODELS_CHANGED_EVENT, fetchModels);
  }, []);

  // Sync messages to the chats array so sidebar shows up-to-date message counts.
  // Only sync when the messages actually belong to the active chat (guards against
  // the brief window where activeChatId updated but currentMessages still holds the old chat's data).
  useEffect(() => {
    if (!activeChatId) return;
    const hasMatchingMessage = currentMessages.some(m => m.role !== 'system');
    if (!hasMatchingMessage && currentMessages.length <= 1) return;
    setChats(prev => {
      const chat = prev.find(c => c.id === activeChatId);
      if (!chat) return prev;
      if (chat.messages === currentMessages) return prev;
      return prev.map(c =>
        c.id === activeChatId ? { ...c, messages: currentMessages } : c
      );
    });
  }, [activeChatId, currentMessages]);

  // Show login prompt every 5 questions for unauthenticated users.
  useEffect(() => {
    if (questionCount > 0 && questionCount % 5 === 0 && !isLoggedIn) {
      setShowLoginModal(true);
    }
  }, [questionCount, isLoggedIn]);

  // Every 5 questions, a "Give us Feedback" capsule offers the feedback page.
  const { addNotification: notify } = useNotifications();
  useEffect(() => {
    if (questionCount > 0 && questionCount % 5 === 0) {
      notify({
        type: 'feedback',
        title: 'Give us Feedback',
        message: '',
        duration: 12000,
        tag: 'feedback-prompt',
        actions: [{
          label: 'Give feedback',
          primary: true,
          onClick: () => { setActiveView('feedback'); setSidebarOpen(false); },
        }],
      });
    }
  }, [questionCount, notify]);

  const handleTitleGenerated = (title: string, sessionIdArg: string) => {
    // Update refs immediately so handleMessagesUpdate (via ref) sees the chatId during streaming
    activeChatIdRef.current = sessionIdArg;
    currentSessionIdRef.current = sessionIdArg;
    activeSessionIdRef.current = sessionIdArg;
    setCurrentChatTitle(title);
    setActiveChatId(sessionIdArg);
    setCurrentSessionId(sessionIdArg);
    setActiveSessionId(sessionIdArg);
    setChats(prev => {
      const existing = prev.find(c => c.id === sessionIdArg);
      if (existing) {
        return prev.map(c => c.id === sessionIdArg ? { ...c, title } : c);
      }
      return [{ id: sessionIdArg, title, messages: [], createdAt: new Date(), pinned: false }, ...prev];
    });
  };

  const handleNewChat = () => {
    // Update refs immediately so background stream callbacks see the change before React renders
    activeChatIdRef.current = null;
    currentSessionIdRef.current = null;
    activeSessionIdRef.current = null;
    setActiveChatId(null);
    setCurrentSessionId(null);
    setActiveSessionId(null);
    setCurrentChatTitle(null);
    setCurrentMessages([{ role: 'system', content: SYSTEM_PROMPT }]);
    setChatWindowKey(prev => prev + 1);
    localStorage.removeItem('offline-intelligence-active-session');
  };



  const handleSelectChat = async (chatId: string) => {
    const chat = chats.find(c => c.id === chatId);
    if (chat) {
      // Update refs immediately so background stream callbacks see the change before React renders
      activeChatIdRef.current = chatId;
      currentSessionIdRef.current = chatId;
      activeSessionIdRef.current = chatId;
      setActiveChatId(chatId);
      setCurrentSessionId(chatId);
      setActiveSessionId(chatId);
      setCurrentChatTitle(chat.title);
      setChatWindowKey(prev => prev + 1);
      // Set messages immediately from in-memory cache to prevent flash of wrong content
      const cachedMessages = chat.messages.length > 1
        ? chat.messages
        : [{ role: 'system' as const, content: SYSTEM_PROMPT }];
      console.log('[DIAG] handleSelectChat cached roles:', cachedMessages.map(m => m.role), 'userCount:', cachedMessages.filter(m => m.role === 'user').length);
      setCurrentMessages(cachedMessages);
      try {
        const conversation = await fetchConversation(chatId);
        if (activeChatIdRef.current !== chatId) return;
        if (conversation) {
          const dbMessages = conversation.messages[0]?.role === 'system'
            ? conversation.messages
            : [{ role: 'system' as const, content: SYSTEM_PROMPT }, ...conversation.messages];
          console.log('[DIAG] handleSelectChat DB roles:', dbMessages.map(m => m.role), 'userCount:', dbMessages.filter(m => m.role === 'user').length);
          const bgContent = streamingContentRef.current[chatId];
          console.log('[DIAG] handleSelectChat bgContent:', bgContent ? `${bgContent.length} chars` : 'none');
          let finalMessages: Message[];
          if (bgContent) {
            const filtered = dbMessages.filter(m => m.role !== 'system');
            const last = filtered[filtered.length - 1];
            if (last && last.role === 'assistant') {
              filtered[filtered.length - 1] = { ...last, content: bgContent };
            } else {
              filtered.push({ role: 'assistant', content: bgContent });
            }
            finalMessages = [{ role: 'system', content: SYSTEM_PROMPT }, ...filtered];
          } else {
            finalMessages = dbMessages;
          }
          setCurrentMessages(finalMessages);
          setChats(prev => prev.map(c => c.id === chatId ? { ...c, messages: finalMessages } : c));
        }
      } catch (error) {
        console.error('Failed to fetch conversation:', error);
        if (activeChatIdRef.current !== chatId) return;
        setCurrentMessages(cachedMessages);
      }
    }
  };

  // Refs to track current session IDs (for use in background stream callback without stale closures)
  const activeChatIdRef = useRef<string | null>(null);
  const currentSessionIdRef = useRef<string | null>(null);
  const activeSessionIdRef = useRef<string | null>(null);
  const lastWrittenSessionRef = useRef<string | null>(null);
  // Keep refs in sync with state on every render
  activeChatIdRef.current = activeChatId;
  currentSessionIdRef.current = currentSessionId;
  activeSessionIdRef.current = activeSessionId;

  const handleMessagesUpdate = useCallback((messages: Message[]) => {
    const chatId = activeChatIdRef.current;
    if (!chatId) return;
    const userCount = messages.filter(m => m.role === 'user').length;
    if (userCount > 1) {
      console.warn('[DIAG] handleMessagesUpdate received', userCount, 'user messages — roles:', messages.map(m => m.role));
    }
    setCurrentMessages(messages);
    setChats(prev => prev.map(chat =>
      chat.id === chatId ? { ...chat, messages } : chat
    ));
  }, []);

  // Background streaming: accumulates content even when user switches sessions
  const handleBackgroundStreamUpdate = useCallback((sessionId: string, content: string) => {
    streamingContentRef.current[sessionId] = content;
    if (sessionId === activeChatIdRef.current || sessionId === currentSessionIdRef.current || sessionId === activeSessionIdRef.current) {
      setCurrentMessages(prev => {
        const filtered = prev.filter(m => m.role !== 'system');
        const last = filtered[filtered.length - 1];
        if (last && last.role === 'assistant') {
          filtered[filtered.length - 1] = { ...last, content };
        } else {
          filtered.push({ role: 'assistant', content });
        }
        return [{ role: 'system', content: SYSTEM_PROMPT }, ...filtered];
      });
    }
  }, []);

  // Queue gate: called by ChatWindow before sending
  const handleRequestSendMessage = useCallback((sessionId: string): 'sent' | 'queued' => {
    if (sessionStreaming.has(sessionId)) {
      return 'queued';
    }
    setSessionStreaming(prev => {
      const next = new Set(prev);
      next.add(sessionId);
      return next;
    });
    return 'sent';
  }, [sessionStreaming]);

  // Stream completion: processes next item in queue
  const handleStreamComplete = useCallback((sessionId: string) => {
    setSessionStreaming(prev => {
      const next = new Set(prev);
      next.delete(sessionId);
      return next;
    });
    setSessionQueues(prev => {
      const queue = prev[sessionId] || [];
      if (queue.length > 0) {
        const [nextItem, ...rest] = queue;
        setPendingQueueRun({ sessionId, content: nextItem.content });
        return { ...prev, [sessionId]: rest };
      }
      return prev;
    });
  }, []);

  // Called from ChatWindow when queued send should be enqueued (session is busy)
  const handleQueueMessage = useCallback((sessionId: string, content: string) => {
    const id = String(nextQueueIdRef.current++);
    setSessionQueues(prev => ({
      ...prev,
      [sessionId]: [...(prev[sessionId] || []), { id, content }],
    }));
  }, []);

  const handlePinChat = async (chatId: string) => {
    const chat = chats.find(c => c.id === chatId);
    if (!chat) return;
    const newPinnedState = !(chat.pinned ?? false);
    setChats(prev => prev.map(c => c.id === chatId ? { ...c, pinned: newPinnedState } : c));
    const success = await updateConversationPinned(chatId, newPinnedState);
    if (!success) {
      setChats(prev => prev.map(c => c.id === chatId ? { ...c, pinned: !newPinnedState } : c));
    }
  };

  const handleSaveChat = (chatId: string) => {
    setChats(prev => prev.map(c =>
      c.id === chatId ? { ...c, saved: !(c.saved ?? false) } : c
    ));
  };

  /**
   * Ask, then delete.
   *
   * The asking used to be a modal of its own down in the render tree — its own
   * scrim, its own 40px warning glyph, its own square buttons, its own Escape
   * handler — none of which matched the dialog every other destructive action
   * in the app already used. One shape now, from ConfirmProvider.
   */
  const confirmDeleteChat = async (chatId: string) => {
    const ok = await confirm({
      title: 'Delete chat?',
      message: 'This conversation and its messages will be removed. This cannot be undone.',
      confirmLabel: 'Delete',
    });
    if (!ok) return;
    try {
      await handleDeleteChat(chatId);
    } catch (error) {
      console.error('Error deleting chat:', error);
    }
  };

  const handleDeleteChat = async (chatId: string) => {
    const success = await deleteConversation(chatId);
    if (!success) throw new Error('Failed to delete conversation from database');
    setChats(prev => prev.filter(chat => chat.id !== chatId));
    if (activeChatId === chatId) handleNewChat();
  };

  /**
   * Go somewhere from the sidebar, and put the sidebar away.
   *
   * Every rail destination routes through this. Opening the sidebar is a
   * means, not an end: you open it to get somewhere, and once you are there it
   * is 260px of navigation sitting on top of the thing you went to look at.
   * Closing it is what the click was FOR.
   *
   * Deliberately not applied to picking a chat out of the history list. That
   * list only exists while the sidebar is open, and people move through it
   * several at a time; closing after each pick would fight them.
   */
  const goTo = (view: Parameters<typeof setActiveView>[0], before?: () => void) => {
    before?.();
    setActiveView(view);
    setSidebarOpen(false);
  };

  /**
   * Aurora is a panel, not a page: it opens over whatever is already showing,
   * from the right. So the rail closes on the left as the panel arrives on the
   * right — one movement across the window, rather than two surfaces competing
   * for the middle of it.
   */
  const openAurora = () => {
    setSidebarOpen(false);
    assistant.toggle();
  };

  const openModelsWithFocus = (_focusApiKey = false, focusHfToken = false) => {
    setActiveView('models');
    if (focusHfToken) {
      setShouldFocusHfToken(true);
      // Reset the focus flag after a brief moment to ensure it works correctly
      setTimeout(() => setShouldFocusHfToken(false), 300);
    }
  };

  const renderMainContent = () => {
    switch (activeView) {
      case 'models':
        return <ModelsPanel
          isOpen={activeView === 'models'}
          onClose={() => setActiveView('chat')}
          selectedModel={selectedModel}
          onSelectModel={(model) => { setSelectedModel(model); if (model) setActiveView('chat'); }}
          focusHfTokenInput={shouldFocusHfToken}
        />;

      case 'settings':
        return <SettingsPanel
          isOpen={true}
          onClose={() => goTo('chat', handleNewChat)}
          onOpenModels={(focusApiKey = false, focusHfToken = false) => {
            openModelsWithFocus(focusApiKey, focusHfToken);
          }}
          onHuggingFaceTokenChange={setHfToken}
          onOpenStorage={() => setActiveView('localfiles')}
          onOpenHelp={() => goTo('help')}
          onRequestLogin={() => goTo('login')}
        />;
      case 'feedback':
        return <FeedbackPage onDone={() => setActiveView('chat')} />;
      case 'login':
        return <LoginPage onDone={() => setActiveView('chat')} />;
      case 'help':
        // Help & Support is a tab of Settings.
        return <SettingsPanel
          isOpen={true}
          initialTab="help"
          onClose={() => goTo('chat', handleNewChat)}
          onOpenModels={(focusApiKey = false, focusHfToken = false) => {
            openModelsWithFocus(focusApiKey, focusHfToken);
          }}
          onHuggingFaceTokenChange={setHfToken}
          onOpenStorage={() => setActiveView('localfiles')}
          onRequestLogin={() => goTo('login')}
        />;
      case 'localfiles':
        return (
          <LocalFilesPanel
            isOpen={true}
            onClose={() => setActiveView('chat')}
            focusDocument={vaultFocus}
            onFocusHandled={clearVaultFocus}
          />
        );
      case 'draft':
        return (
          <DraftWorkspace
            openDraftId={draftToOpen}
            onDraftOpened={() => setDraftToOpen(null)}
            openFolderId={folderToOpen}
            onFolderOpened={() => setFolderToOpen(null)}
          />
        );
      case 'library':
        return (
          <LibraryPanel
            focusItemId={libraryFocus}
            onFocusHandled={clearLibraryFocus}
            onOpenDraft={(id) => {
              setDraftToOpen(id);
              setActiveView('draft');
            }}
          />
        );
      default:
        return (
          <ChatWindow
            key={chatWindowKey}
            messages={currentMessages}
            chatTitle={currentChatTitle}
            chatId={activeChatId}
            sessionId={currentSessionId}
            onSessionIdChange={setCurrentSessionId}
            isPinned={chats.find(c => c.id === activeChatId)?.pinned}
            onMessagesUpdate={handleMessagesUpdate}
            onTitleGenerated={handleTitleGenerated}
            onPinChat={handlePinChat}
            onDeleteChat={handleDeleteChat}
            onQuestionAsked={() => setQuestionCount(prev => prev + 1)}
            selectedModel={selectedModel}
            models={availableModels}
            onOpenModels={(focusHfToken = false) => {
              openModelsWithFocus(false, focusHfToken);
            }}
            onBackgroundStreamUpdate={handleBackgroundStreamUpdate}
            onRequestSendMessage={handleRequestSendMessage}
            onQueueMessage={handleQueueMessage}
            onStreamComplete={handleStreamComplete}
            onStopStream={() => {}}
            queuedMessages={sessionQueues[currentSessionId ?? ''] || []}
            isSessionStreaming={sessionStreaming.has(currentSessionId ?? '')}
            pendingQueueRun={pendingQueueRun}
            onQueueRunConsumed={() => setPendingQueueRun(null)}
            onRequestDeleteChat={(id) => void confirmDeleteChat(id)}
            onSelectChat={(id) => { handleSelectChat(id); setActiveView('chat'); }}
            onOpenLibrary={() => setActiveView('library')}
            onOpenDraft={() => setActiveView('draft')}
            onOpenLocalFiles={() => setActiveView('localfiles')}
          />
        );
    }
  };

  // One model selection for the whole app: the chat composer's capsule and
  // the drafts composer's capsule are the same choice, because there is one
  // engine with one model loaded.
  const modelSelection = useMemo(
    () => ({
      models: availableModels,
      selected: selectedModel,
      // Choosing in the model menu also tells the backend, lazily: the
      // previous model is unloaded and the new one loads on the next
      // question. A failure reverts the menu to the model still in use.
      select: (model: { id: string; name: string; source: 'local' } | null) => {
        const previous = selectedModel;
        setSelectedModel(model);
        if (!model || model.id.startsWith('active:')) return;
        void chooseModel(model.id).then(result => {
          if (!result.ok) {
            setSelectedModel(previous);
            addNotification({ type: 'error', title: "Couldn't switch model", message: result.message ?? '', duration: 0 });
          }
        });
      },
      openModels: () => openModelsWithFocus(false, false),
    }),
    // openModelsWithFocus is redeclared every render and only sets view state.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [availableModels, selectedModel],
  );

  return (
    <ModelSelectionProvider value={modelSelection}>
    <div className="app-root" data-sidebar={sidebarOpen ? 'open' : 'closed'} data-assistant={assistant.open ? 'open' : 'closed'} data-view={activeView} data-downloads={downloadsVisible ? 'active' : 'idle'} data-aurora={auroraPlacement}>
      <TitleBar />
      {/* Suppressed on a machine with no models installed: that is a first
          run, and someone who has never used Counsel does not need to be told
          what changed since a version they never had. */}
      <WhatsNewDialog suppressed={availableModels.length === 0} />
      {/* Aurora lives in the notification island (NotificationsContainer). */}
      <Sidebar
        chats={chats}
        activeChatId={activeChatId}
        isOpen={sidebarOpen}
        onToggle={toggleRail}
        onNewChat={() => goTo('chat', handleNewChat)}
        onSelectChat={(id) => { handleSelectChat(id); setActiveView('chat'); }}
        onOpenModels={() => goTo('models', () => setShouldFocusHfToken(false))}
        onPinChat={handlePinChat}
        onSaveChat={handleSaveChat}
        onDeleteChat={(id) => void confirmDeleteChat(id)}
        onOpenSettings={() => goTo('settings')}
        onOpenHelp={() => setActiveView('help')}
        onOpenAccount={() => goTo('login')}
        onOpenLocalFiles={() => goTo('localfiles')}
        onOpenDraft={() => goTo('draft')}
        onOpenLibrary={() => goTo('library')}
        spaces={spaces}
        onOpenSpace={(id) => goTo('draft', () => setFolderToOpen(id))}
        recentDrafts={recentDrafts}
        onOpenRecentDraft={(id) => goTo('draft', () => setDraftToOpen(id))}
        activeView={activeView}
        assistantOpen={assistant.open}
        onToggleAssistant={openAurora}
      />
      {/* One boundary around the switch rather than one per page: the pages are
          mutually exclusive, and the fallback is null because every chunk is a
          local file — a spinner for the frame it takes to arrive would read as
          a flicker, not as progress. The rail and the window band stay drawn
          throughout, so the frame is never blank. */}
      <ViewBoundary resetKey={activeView}>
        <Suspense fallback={null}>{renderMainContent()}</Suspense>
      </ViewBoundary>
      <SearchModal
        isOpen={isSearchOpen}
        chats={chats}
        onClose={() => setIsSearchOpen(false)}
        onSelectChat={(chatId) => { handleSelectChat(chatId); setActiveView('chat'); }}
      />
      <LoginModal
        isOpen={showLoginModal}
        onClose={() => setShowLoginModal(false)}
      />
      {/* Mounted here rather than inside ModelsPanel because its dialogs are
          also opened from Settings, which is a sibling of that panel — only
          one of the two is ever mounted at a time. See the header note in
          ModelDialogs.tsx. */}
      <ModelDialogHost />
      {loginGate && !isLoggedIn && (
        <div className="login-gate"><LoginPage gated onDone={() => setLoginGate(false)} /></div>
      )}
      <NotificationsContainer
        onOpenModels={() => openModelsWithFocus(false, false)}
        downloadNote={downloadsVisible ? {
          title: `${activeGlobalDownloads.length} download${activeGlobalDownloads.length > 1 ? 's' : ''} in progress`,
          message: activeGlobalDownloads.slice(0, 3).map(dl =>
            `${dl.model_name} ${dl.percentage.toFixed(1)}%${dl.speed_bps > 0 ? ` · ${(dl.speed_bps / (1024 * 1024)).toFixed(1)} MB/s` : ''}`
          ).join('  |  '),
          onView: () => setActiveView('models'),
          onDismiss: () => setShowDownloadBubble(false),
        } : null}
      />
    </div>
    </ModelSelectionProvider>
  );
}

export default App
