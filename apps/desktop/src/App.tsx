import { useState, useEffect, useRef, useCallback } from 'react'
import { ChatWindow } from './components/ChatWindow'
import { Sidebar } from './components/Sidebar'
import { SearchModal } from './components/SearchModal'
import NotificationsContainer from './components/Notifications'
import ModelsPanel from './components/ModelsPanel'

import SettingsPanel from './components/SettingsPanel'
import HelpFeedbackPanel from './components/HelpFeedbackPanel'
import LocalFilesPanel from './components/LocalFilesPanel'
import DraftWorkspace from './components/workspace/DraftWorkspace'
import LoginModal from './components/LoginModal'
import FeedbackPopup from './components/FeedbackPopup'
import { useAuth } from './contexts/AuthContext'
import type { Chat } from './components/ChatWindow'
import type { Message } from './api/chat'
import { fetchConversations, fetchConversation, deleteConversation, updateConversationPinned } from './api/chat'
import { SYSTEM_PROMPT } from './systemPrompt'
import { getApiBaseSync } from './api/backendUrl'
import { getAllApiKeys, migrateKeysFromLocalStorage } from './api/apiKeys'
import { getHfToken, setHfToken as storeHfToken } from './api/hfToken'
import './App.css'

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
  const [activeView, setActiveView] = useState<'chat' | 'models' | 'settings' | 'help' | 'localfiles' | 'draft'>('chat');
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [questionCount, setQuestionCount] = useState(0);
  const [showLoginModal, setShowLoginModal] = useState(false);
  const [showFeedbackPopup, setShowFeedbackPopup] = useState(false);
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
  // Delete confirmation
  const [deleteConfirmTargetId, setDeleteConfirmTargetId] = useState<string | null>(null);
  const [isDeletingConfirm, setIsDeletingConfirm] = useState(false);
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

  // Global download progress tracking for notification bubble
  const [globalDownloads, setGlobalDownloads] = useState<{ download_id: string; model_name: string; status: string; percentage: number; bytes_downloaded: number; total_bytes?: number; speed_bps: number }[]>([]);
  const [showDownloadBubble, setShowDownloadBubble] = useState(true);

  useEffect(() => {
    const pollDownloads = async () => {
      try {
        const res = await fetch(`${getApiBaseSync()}/models/downloads`);
        if (res.ok) {
          const data = await res.json();
          setGlobalDownloads(data);
        }
      } catch { /* ignore */ }
    };
    const interval = setInterval(pollDownloads, 3000);
    pollDownloads();
    return () => clearInterval(interval);
  }, []);


  const activeGlobalDownloads = globalDownloads.filter(d => d.status === 'Downloading' || d.status === 'Starting');

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

        if (selectedModel && !modelList.some(m => m.id === selectedModel.id)) {
          setSelectedModel(null);
        }

        if (!selectedModel) {
          if (installedModels.length > 0) {
            setSelectedModel({ id: installedModels[0].id, name: installedModels[0].name, source: 'local' });
          } else if (activeModelName) {
            setSelectedModel({ id: `active:${activeModelName}`, name: activeModelName, source: 'local' });
          }
        }
      } catch (error) {
        console.error('Failed to fetch models:', error);
      }
    };
    fetchModels();
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

  // Show feedback popup every 5 queries
  useEffect(() => {
    if (questionCount > 0 && questionCount % 5 === 0) {
      setShowFeedbackPopup(true);
    }
  }, [questionCount]);

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

  const handleDeleteChat = async (chatId: string) => {
    const success = await deleteConversation(chatId);
    if (!success) throw new Error('Failed to delete conversation from database');
    setChats(prev => prev.filter(chat => chat.id !== chatId));
    if (activeChatId === chatId) handleNewChat();
  };

  // Close delete confirmation on Escape
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setDeleteConfirmTargetId(null);
    };
    if (deleteConfirmTargetId) {
      document.addEventListener('keydown', handleKeyDown);
      return () => document.removeEventListener('keydown', handleKeyDown);
    }
  }, [deleteConfirmTargetId]);

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
          onClose={() => setActiveView('chat')}
          onOpenModels={(focusApiKey = false, focusHfToken = false) => {
            openModelsWithFocus(focusApiKey, focusHfToken);
          }}
          onHuggingFaceTokenChange={setHfToken}
          onOpenStorage={() => setActiveView('localfiles')}
          onOpenHelp={() => setActiveView('help')}
          onRequestLogin={() => setShowLoginModal(true)}
        />;
      case 'help':
        return (
          <HelpFeedbackPanel
            isOpen={true}
            onClose={() => setActiveView('chat')}
            isLoggedIn={isLoggedIn}
          />
        );
      case 'localfiles':
        return <LocalFilesPanel isOpen={true} onClose={() => setActiveView('chat')} />;
      case 'draft':
        return <DraftWorkspace onClose={() => setActiveView('chat')} />;
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
            onSelectedModelChange={setSelectedModel}
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
            onRequestDeleteChat={(id) => setDeleteConfirmTargetId(id)}
          />
        );
    }
  };

  return (
    <div className="app-root" data-sidebar={sidebarOpen ? 'open' : 'closed'}>
      <Sidebar
        chats={chats}
        activeChatId={activeChatId}
        isOpen={sidebarOpen}
        onToggle={() => setSidebarOpen(!sidebarOpen)}
        onNewChat={() => { handleNewChat(); setActiveView('chat'); }}
        onSelectChat={(id) => { handleSelectChat(id); setActiveView('chat'); }}
        onOpenModels={() => {
          setActiveView('models');
          setShouldFocusHfToken(false);
        }}
        onPinChat={handlePinChat}
        onSaveChat={handleSaveChat}
        onDeleteChat={(id) => setDeleteConfirmTargetId(id)}
        onOpenSettings={() => setActiveView('settings')}
        onOpenHelp={() => setActiveView('help')}
        onOpenHome={() => { handleNewChat(); setActiveView('chat'); }}
        onOpenLocalFiles={() => setActiveView('localfiles')}
        onOpenDraft={() => setActiveView('draft')}
        activeView={activeView}
        userName={user?.name || 'User'}
      />
      {renderMainContent()}
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
      <FeedbackPopup
        isOpen={showFeedbackPopup}
        onClose={() => setShowFeedbackPopup(false)}
      />
      {deleteConfirmTargetId && (
        <div className="modal-overlay" onClick={() => setDeleteConfirmTargetId(null)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-header">
              <svg className="modal-icon" fill="none" stroke="currentColor" viewBox="0 0 24 24" width="40" height="40">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 9v2m0 4v2m0 4v2m0-14a9 9 0 110 18 9 9 0 010-18zm0 0a9 9 0 110 18 9 9 0 010-18z" />
              </svg>
            </div>
            <h2 className="modal-title">Delete Chat</h2>
            <p className="modal-message">Are you sure you want to delete this conversation?</p>
            <div className="modal-buttons">
              <button className="modal-button cancel" onClick={() => setDeleteConfirmTargetId(null)}>
                Cancel
              </button>
              <button
                className="modal-button delete"
                disabled={isDeletingConfirm}
                onClick={async () => {
                  setIsDeletingConfirm(true);
                  try {
                    await handleDeleteChat(deleteConfirmTargetId);
                    setDeleteConfirmTargetId(null);
                  } catch (error) {
                    console.error('Error deleting chat:', error);
                  } finally {
                    setIsDeletingConfirm(false);
                  }
                }}
              >
                {isDeletingConfirm ? 'Deleting...' : 'Delete'}
              </button>
            </div>
          </div>
        </div>
      )}
      {/* Global download notification bubble - shows on all pages except models */}
      {activeView !== 'models' && activeGlobalDownloads.length > 0 && showDownloadBubble && (
        <div className="download-bubble">
          <div className="download-bubble-header">
            <span className="download-bubble-title">
              {activeGlobalDownloads.length} download{activeGlobalDownloads.length > 1 ? 's' : ''} in progress
            </span>
            <div style={{ display: 'flex', gap: '4px' }}>
              <button className="download-bubble-close" onClick={() => setActiveView('models')} style={{ fontSize: '12px' }}>
                View
              </button>
              <button className="download-bubble-close" onClick={() => setShowDownloadBubble(false)}>
                <svg width="14" height="14" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
                </svg>
              </button>
            </div>
          </div>
          {activeGlobalDownloads.slice(0, 3).map(dl => (
            <div key={dl.download_id} style={{ marginBottom: '6px' }}>
              <div style={{ fontSize: '12px', color: 'var(--text-secondary)', marginBottom: '3px' }}>{dl.model_name}</div>
              <div style={{ width: '100%', height: '4px', backgroundColor: 'var(--bg-tertiary)', borderRadius: '2px', overflow: 'hidden' }}>
                <div style={{ width: `${Math.min(dl.percentage, 100)}%`, height: '100%', backgroundColor: 'var(--accent)', borderRadius: '2px', transition: 'width 0.3s' }} />
              </div>
              <div style={{ fontSize: '11px', color: 'var(--text-muted)', marginTop: '2px' }}>
                {dl.percentage.toFixed(1)}%{dl.speed_bps > 0 ? ` - ${(dl.speed_bps / (1024 * 1024)).toFixed(1)} MB/s` : ''}
              </div>
            </div>
          ))}
        </div>
      )}
      <NotificationsContainer />
    </div>
  );
}

export default App
