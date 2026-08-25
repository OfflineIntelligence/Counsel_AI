import React, { useState, useEffect } from 'react';
import { useTheme } from '../contexts/ThemeContext';
import { useAuth } from '../contexts/AuthContext';
import { getApiBaseSync } from '../api/backendUrl';
import {
  ArrowLeft, Send, CheckCircle, MessageSquare, HardDrive, Key,
  Settings as SettingsIcon, User, Palette, LifeBuoy, Info,
} from 'lucide-react';
import { showHuggingFaceApiKeyModal } from './ModelsPanel';
import { getAllApiKeys } from '../api/apiKeys';
import { getHfToken } from '../api/hfToken';
import {
  fetchStorageSettings,
  updateStorageLimit,
  formatBytes,
  LIMIT_CHOICES_MB,
  type StorageSettings,
} from '../api/storageSettings';
import MetricsPanel from './MetricsPanel';
import './SettingsPanel.css';

const SettingsPanel: React.FC<{
  isOpen: boolean;
  onClose: () => void;
  onOpenModels?: (focusApiKey?: boolean, focusHfToken?: boolean) => void;
  onHuggingFaceTokenChange?: (token: string) => void;
  onOpenStorage?: () => void;
  onOpenMetrics?: () => void;
  onOpenHelp?: () => void;
  onRequestLogin?: () => void;
}> = ({
  isOpen,
  onClose,
  onOpenModels,
  onHuggingFaceTokenChange,
  onOpenStorage,
  onOpenMetrics,
  onOpenHelp,
  onRequestLogin,
}) => {
  const { theme, toggleTheme } = useTheme();
  const { user, isLoggedIn, logout, setApiKey } = useAuth();

  // Live API key presence indicators
  const [apiKeyStatus, setApiKeyStatus] = useState<{ huggingface: boolean }>({
    huggingface: !!getHfToken(),
  });

  const refreshApiKeyStatus = () => {
    setApiKeyStatus({
      huggingface: !!getHfToken(),
    });
    setTimeout(() => {
      getAllApiKeys().then(keys => {
        setApiKeyStatus({
          huggingface: keys.some(k => k.key_type === 'huggingface' && !!k.value),
        });
      }).catch(() => {});
    }, 400);
  };

  useEffect(() => {
    refreshApiKeyStatus();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Storage budget. Loaded from the backend rather than kept in localStorage:
  // the backend enforces the limit, so it must be the one source of truth —
  // a cached copy here could disagree with what is actually being applied.
  const [storage, setStorage] = useState<StorageSettings | null>(null);
  const [storageError, setStorageError] = useState('');
  const [savingLimit, setSavingLimit] = useState(false);
  const [evictionNote, setEvictionNote] = useState('');

  useEffect(() => {
    if (!isOpen) return;
    fetchStorageSettings()
      .then((s) => {
        setStorage(s);
        setStorageError('');
      })
      .catch((e: unknown) =>
        setStorageError(e instanceof Error ? e.message : 'Could not load storage settings'),
      );
  }, [isOpen]);

  const handleLimitChange = async (limitMb: number) => {
    setSavingLimit(true);
    setStorageError('');
    setEvictionNote('');
    try {
      const result = await updateStorageLimit(limitMb);
      setStorage(result);
      // Report what the change actually did on disk. Eviction runs inside the
      // PUT, so staying silent here would let files disappear unannounced.
      if (result.eviction.ran && result.eviction.freed_bytes > 0) {
        setEvictionNote(
          `Freed ${formatBytes(result.eviction.freed_bytes)} by removing ` +
            `${result.eviction.evicted.map((i) => i.id).join(', ')}.`,
        );
      } else if (result.eviction.shortfall_bytes > 0) {
        setEvictionNote(result.eviction.reason);
      }
    } catch (e: unknown) {
      setStorageError(e instanceof Error ? e.message : 'Could not save the storage limit');
    } finally {
      setSavingLimit(false);
    }
  };

  // Feedback form state
  const [feedbackName, setFeedbackName] = useState(user?.name || '');
  const [feedbackEmail, setFeedbackEmail] = useState(user?.email || '');
  const [feedbackMessage, setFeedbackMessage] = useState('');
  const [isSubmittingFeedback, setIsSubmittingFeedback] = useState(false);
  const [feedbackSubmitted, setFeedbackSubmitted] = useState(false);
  const [feedbackError, setFeedbackError] = useState('');

  const handleFeedbackSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    
    if (!feedbackMessage.trim()) {
      setFeedbackError('Please enter your feedback');
      return;
    }

    if (!feedbackEmail.trim()) {
      setFeedbackError('Please enter your email');
      return;
    }

    setIsSubmittingFeedback(true);
    setFeedbackError('');

    try {
      const response = await fetch(`${getApiBaseSync()}/feedback`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          name: feedbackName,
          email: feedbackEmail,
          message: feedbackMessage,
          subject: `FEEDBACK from ${feedbackName || 'Anonymous'}`,
          to_email: 'product@offlineintelligence.io',
        }),
      });

      if (response.ok) {
        setFeedbackSubmitted(true);
        setTimeout(() => {
          setFeedbackSubmitted(false);
          setFeedbackMessage('');
        }, 3000);
      } else {
        throw new Error('Failed to submit feedback');
      }
    } catch (err) {
      setFeedbackError('Failed to send feedback. Please try again.');
    } finally {
      setIsSubmittingFeedback(false);
    }
  };


  if (!isOpen) return null;

  return (
    <div className="settings-page">
      {/* Header — the same page chrome as every other view, so the app has one
          header pattern rather than five. */}
      <div className="chat-header">
        <div className="chat-header-bar centered">
          <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
            <button className="header-icon-btn" onClick={onClose} title="Back to chat">
              <ArrowLeft size={18} />
            </button>
            <h1 className="chat-title" style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
              <SettingsIcon size={20} strokeWidth={1.75} />
              Settings
            </h1>
          </div>
        </div>
      </div>

      <div className="settings-page__body">
        <div className="settings-col">
          {/* ── 1. SYSTEM METRICS ──────────────────────────────────────────
              First on the page, deliberately: it is the only LIVE thing here,
              and it answers the question a user actually opens Settings with
              when something feels slow. Rendered by the real MetricsPanel in
              embedded mode, so there is exactly one metrics implementation —
              the polling, rolling history and sparklines are the same code
              that used to power the standalone page. */}
          <div className="settings-block">
            <MetricsPanel isOpen embedded />
          </div>

          {/* ── 2. ACCOUNT ─────────────────────────────────────────────────── */}
          <div className="settings-block">
            <div className="settings-block__label">
              <User size={12} strokeWidth={2} />
              Account
            </div>
            <div className="settings-group">
              <div className="settings-row">
                <div className="settings-row__text">
                  <span className="settings-row__title">
                    {isLoggedIn ? (user?.name || 'Signed in') : 'Not signed in'}
                  </span>
                  <span className="settings-row__desc">
                    {isLoggedIn
                      ? (user?.email || 'Signed in on this device')
                      : 'Sign in to keep your details across sessions'}
                  </span>
                </div>
                <div className="settings-row__control">
                  {isLoggedIn ? (
                    <button className="settings-btn settings-btn--danger" onClick={() => logout()}>
                      Log out
                    </button>
                  ) : (
                    <button className="settings-btn settings-btn--primary" onClick={() => onRequestLogin?.()}>
                      Log in
                    </button>
                  )}
                </div>
              </div>
            </div>
          </div>

          {/* ── 3. API KEYS ────────────────────────────────────────────────── */}
          <div className="settings-block">
            <div className="settings-block__label">
              <Key size={12} strokeWidth={2} />
              API keys
            </div>
            <div className="settings-group">
              <div className="settings-row">
                <div className="settings-row__text">
                  <span className="settings-row__title">
                    HuggingFace token
                    <span
                      className={
                        apiKeyStatus.huggingface
                          ? 'settings-pill settings-pill--ok'
                          : 'settings-pill settings-pill--off'
                      }
                    >
                      <span className="settings-pill__dot" />
                      {apiKeyStatus.huggingface ? 'Configured' : 'Not set'}
                    </span>
                  </span>
                  <span className="settings-row__desc">
                    {apiKeyStatus.huggingface
                      ? 'Active. Gated and private models can be downloaded.'
                      : 'Needed only for gated or private models on HuggingFace.'}
                  </span>
                </div>
                <div className="settings-row__control">
                  <button
                    className="settings-btn"
                    onClick={() =>
                      showHuggingFaceApiKeyModal(onHuggingFaceTokenChange, setApiKey, refreshApiKeyStatus)
                    }
                  >
                    {apiKeyStatus.huggingface ? 'Change' : 'Add token'}
                  </button>
                </div>
              </div>
            </div>
          </div>

          {/* ── 4. STORAGE ─────────────────────────────────────────────────── */}
          <div className="settings-block">
            <div className="settings-block__label">
              <HardDrive size={12} strokeWidth={2} />
              Storage
            </div>
            <div className="settings-group">
              <div className="settings-row">
                <div className="settings-row__text">
                  <span className="settings-row__title">Vault</span>
                  <span className="settings-row__desc">
                    Your document library — read once, available to every conversation.
                  </span>
                </div>
                <div className="settings-row__control">
                  <button className="settings-btn" onClick={() => onOpenStorage?.()}>
                    Open Vault
                  </button>
                </div>
              </div>

              <div className="settings-row">
                <div className="settings-row__text">
                  <span className="settings-row__title">Disk limit</span>
                  <span className="settings-row__desc">
                    Maximum space for models, engines, the database, your Vault files and cached data.
                  </span>
                  {storageError && (
                    <span className="settings-note settings-note--danger">{storageError}</span>
                  )}
                  {storage && !storageError && (
                    <span className="settings-note">
                      Using <strong>{storage.usage_human}</strong> of {storage.limit_human}
                      {storage.source === 'environment' && ' (default)'}
                      {storage.over_budget && (
                        <span className="settings-note--danger"> — over the limit</span>
                      )}
                    </span>
                  )}
                  {evictionNote && <span className="settings-note">{evictionNote}</span>}
                </div>
                <div className="settings-row__control">
                  <select
                    className="settings-select"
                    disabled={!storage || savingLimit}
                    value={storage ? String(storage.limit_mb) : ''}
                    onChange={(e) => handleLimitChange(Number(e.target.value))}
                    aria-label="Disk limit"
                  >
                    {/* A limit set outside these choices (e.g. via .env) still needs
                        to be selectable, or the control would silently misreport it. */}
                    {storage && !LIMIT_CHOICES_MB.some((c) => c.value === storage.limit_mb) && (
                      <option value={String(storage.limit_mb)}>{storage.limit_human} (current)</option>
                    )}
                    {LIMIT_CHOICES_MB.map((choice) => (
                      <option key={choice.value} value={String(choice.value)}>
                        {choice.label}
                      </option>
                    ))}
                  </select>
                </div>
              </div>
            </div>
          </div>

          {/* ── 5. APPEARANCE ──────────────────────────────────────────────── */}
          <div className="settings-block">
            <div className="settings-block__label">
              <Palette size={12} strokeWidth={2} />
              Appearance
            </div>
            <div className="settings-group">
              <div className="settings-row">
                <div className="settings-row__text">
                  <span className="settings-row__title">Dark mode</span>
                  <span className="settings-row__desc">
                    Currently {theme === 'dark' ? 'dark' : 'light'}. The sidebar stays black in both.
                  </span>
                </div>
                <div className="settings-row__control">
                  <button
                    type="button"
                    role="switch"
                    aria-checked={theme === 'dark'}
                    aria-label="Dark mode"
                    className="settings-switch"
                    onClick={toggleTheme}
                  >
                    <span className="settings-switch__thumb" />
                  </button>
                </div>
              </div>
            </div>
          </div>

          {/* ── 6. HELP ────────────────────────────────────────────────────── */}
          <div className="settings-block">
            <div className="settings-block__label">
              <LifeBuoy size={12} strokeWidth={2} />
              Help
            </div>
            <div className="settings-group">
              <div className="settings-row">
                <div className="settings-row__text">
                  <span className="settings-row__title">Help &amp; guides</span>
                  <span className="settings-row__desc">
                    How attachments, the Vault and model selection work.
                  </span>
                </div>
                <div className="settings-row__control">
                  <button className="settings-btn" onClick={() => onOpenHelp?.()}>
                    Open help
                  </button>
                </div>
              </div>
              <div className="settings-row">
                <div className="settings-row__text">
                  <span className="settings-row__title">Models</span>
                  <span className="settings-row__desc">
                    Browse, install and activate local models, including vision models.
                  </span>
                </div>
                <div className="settings-row__control">
                  <button className="settings-btn" onClick={() => onOpenModels?.()}>
                    Open models
                  </button>
                </div>
              </div>
            </div>
          </div>

          {/* ── 7. FEEDBACK ────────────────────────────────────────────────── */}
          <div className="settings-block">
            <div className="settings-block__label">
              <MessageSquare size={12} strokeWidth={2} />
              Feedback
            </div>
            <div className="settings-group">
              {feedbackSubmitted ? (
                <div className="settings-sent">
                  <span className="settings-sent__icon">
                    <CheckCircle size={22} strokeWidth={1.75} />
                  </span>
                  <span className="settings-sent__title">Thank you</span>
                  <span className="settings-sent__hint">Your feedback has been sent.</span>
                </div>
              ) : (
                <form className="settings-row settings-row--stack" onSubmit={handleFeedbackSubmit}>
                  <div className="settings-row__text">
                    <span className="settings-row__title">Send us feedback</span>
                    <span className="settings-row__desc">
                      Goes to product@offlineintelligence.io — tell us what broke or what is missing.
                    </span>
                  </div>

                  <div className="settings-field">
                    <label className="settings-field__label" htmlFor="fb-name">Name</label>
                    <input
                      id="fb-name"
                      className="settings-input"
                      type="text"
                      value={feedbackName}
                      onChange={(e) => setFeedbackName(e.target.value)}
                      placeholder="Your name"
                    />
                  </div>

                  <div className="settings-field">
                    <label className="settings-field__label" htmlFor="fb-email">Email</label>
                    <input
                      id="fb-email"
                      className="settings-input"
                      type="email"
                      value={feedbackEmail}
                      onChange={(e) => setFeedbackEmail(e.target.value)}
                      placeholder="you@example.com"
                      required
                    />
                  </div>

                  <div className="settings-field">
                    <label className="settings-field__label" htmlFor="fb-message">Feedback</label>
                    <textarea
                      id="fb-message"
                      className={
                        feedbackError ? 'settings-textarea settings-textarea--invalid' : 'settings-textarea'
                      }
                      value={feedbackMessage}
                      onChange={(e) => setFeedbackMessage(e.target.value)}
                      placeholder="Tell us what you think…"
                      rows={4}
                      required
                    />
                    {feedbackError && (
                      <span className="settings-note settings-note--danger">{feedbackError}</span>
                    )}
                  </div>

                  <div className="settings-row__control" style={{ minWidth: 0 }}>
                    <button
                      className="settings-btn settings-btn--primary"
                      type="submit"
                      disabled={isSubmittingFeedback}
                    >
                      <Send size={15} strokeWidth={1.75} />
                      {isSubmittingFeedback ? 'Sending…' : 'Send feedback'}
                    </button>
                  </div>
                </form>
              )}
            </div>
          </div>

          {/* ── 8. ABOUT ───────────────────────────────────────────────────── */}
          <div className="settings-block">
            <div className="settings-block__label">
              <Info size={12} strokeWidth={2} />
              About
            </div>
            <div className="settings-group">
              <div className="settings-about">
                <span className="settings-about__name">Offline Counsel AI</span>
                <span>Version 0.1.0</span>
                <span>Offline-first legal AI. Models and documents never leave this device.</span>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};

export default SettingsPanel;
