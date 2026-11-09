import React, { useState, useEffect, useRef } from 'react';
import { useTheme } from '../contexts/ThemeContext';
import { useAuth } from '../contexts/AuthContext';
import { getApiBaseSync } from '../api/backendUrl';
import {
  ArrowLeft, ChevronRight, HelpCircle, SlidersHorizontal, User, HardDrive, Box, Activity, Info,
} from 'lucide-react';
import { showHuggingFaceApiKeyModal } from './models/modelDialogBus';
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
import HelpFeedbackPanel from './HelpFeedbackPanel';
import './workspace/workspace.css';
import './SettingsPanel.css';

/** The sections, as tabs along the top of one scrolling page. Each carries the one-line
 *  description shown under its heading. */
const SETTINGS_TABS = [
  { id: 'general', label: 'General', icon: SlidersHorizontal, blurb: '' },
  { id: 'account', label: 'Account', icon: User, blurb: 'Your sign-in on this device.' },
  { id: 'storage', label: 'Storage', icon: HardDrive, blurb: 'Where your documents live and how much disk the software uses.' },
  { id: 'models', label: 'Models', icon: Box, blurb: '' },
  { id: 'system', label: 'System', icon: Activity, blurb: '' },
  { id: 'help', label: 'Help & Support', icon: HelpCircle, blurb: '' },
  { id: 'about', label: 'About', icon: Info, blurb: '' },
] as const;

type SettingsTab = (typeof SETTINGS_TABS)[number]['id'];

/** Storage breakdown, in the order a user thinks about it: their own files
 *  first, then what runs the app, then what it can rebuild. */
const USAGE_PARTS = [
  { key: 'vault_bytes', label: 'Vault' },
  { key: 'drafts_bytes', label: 'Drafts' },
  { key: 'models_bytes', label: 'Models' },
  { key: 'engines_bytes', label: 'Engines' },
  { key: 'database_bytes', label: 'Database' },
  { key: 'downloads_bytes', label: 'Downloads' },
  { key: 'kv_cache_bytes', label: 'Cache' },
  { key: 'other_bytes', label: 'Other' },
] as const;

const THEME_CHOICES = [
  { id: 'system', label: 'Automatic' },
  { id: 'light', label: 'Light' },
  { id: 'dark', label: 'Dark' },
] as const;

function initialsOf(name?: string, email?: string): string {
  const source = (name || (email ?? '').split('@')[0] || '').trim();
  if (!source) return '?';
  // First letter of the first name and of the last name ("Ada M. Lovelace" → AL).
  const parts = source.split(/[\s@._-]+/).filter(Boolean);
  const last = parts.length > 1 ? parts[parts.length - 1] : '';
  return ((parts[0]?.[0] ?? '') + (last[0] ?? '')).toUpperCase() || '?';
}

const SettingsPanel: React.FC<{
  isOpen: boolean;
  onClose: () => void;
  onOpenModels?: (focusApiKey?: boolean, focusHfToken?: boolean) => void;
  onHuggingFaceTokenChange?: (token: string) => void;
  onOpenStorage?: () => void;
  onOpenMetrics?: () => void;
  onOpenHelp?: () => void;
  onRequestLogin?: () => void;
  /** Which tab to open on (the sidebar's Help entry opens 'help'). */
  initialTab?: 'general' | 'account' | 'storage' | 'models' | 'system' | 'help' | 'about';
}> = ({
  isOpen,
  onClose,
  onOpenModels,
  onHuggingFaceTokenChange,
  onOpenStorage,
  onRequestLogin,
  initialTab = 'general',
}) => {
  const [tab, setTab] = useState<SettingsTab>(initialTab);
  // Where the user has been, so Escape walks back: previous section, then
  // the settings home (general), then out to a new chat via onClose.
  const tabHistory = useRef<SettingsTab[]>([]);
  // Every section is on one scrolling page; a tab scrolls to its section.
  const contentRef = useRef<HTMLDivElement>(null);
  const scrollToTab = (id: SettingsTab, smooth = true) => {
    const box = contentRef.current;
    const el = box?.querySelector<HTMLElement>(`[data-settings-tab="${id}"]`);
    if (box && el) box.scrollTo({ top: el.offsetTop - box.offsetTop - 8, behavior: smooth ? 'smooth' : 'auto' });
    setTab(id);
  };
  const goTab = (id: SettingsTab) => {
    if (id === tab) return;
    tabHistory.current.push(tab);
    scrollToTab(id);
  };
  // The highlighted tab follows the scroll: the last section whose top has
  // passed the top of the content area.
  const onContentScroll = () => {
    const box = contentRef.current;
    if (!box) return;
    let current: SettingsTab = SETTINGS_TABS[0].id;
    for (const t of SETTINGS_TABS) {
      const el = box.querySelector<HTMLElement>(`[data-settings-tab="${t.id}"]`);
      if (el && el.offsetTop - box.offsetTop - box.scrollTop <= 80) current = t.id;
    }
    // At the very bottom, the last section counts as current even if short.
    if (box.scrollTop + box.clientHeight >= box.scrollHeight - 2) current = SETTINGS_TABS[SETTINGS_TABS.length - 1].id;
    if (current !== tab) setTab(current);
  };
  // Open on the requested section (the sidebar's Help entry opens 'help').
  useEffect(() => {
    if (isOpen) requestAnimationFrame(() => scrollToTab(initialTab, false));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isOpen, initialTab]);
  const { theme, preference, setPreference } = useTheme();
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
  // the backend enforces the limit, so it must be the one source of truth.
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

  // Disk space on this drive, shown beside the app's own budget. Read once
  // when Storage is opened, since a disk does not fill while a settings page
  // is on screen.
  const [disk, setDisk] = useState<{ used: number; free: number; total: number } | null>(null);
  useEffect(() => {
    if (!isOpen) return;
    let live = true;
    fetch(`${getApiBaseSync()}/hardware/info`)
      .then((r) => (r.ok ? r.json() : null))
      .then((hw: { storage_used_bytes: number; storage_available_bytes: number } | null) => {
        if (!live || !hw) return;
        setDisk({
          used: hw.storage_used_bytes,
          free: hw.storage_available_bytes,
          total: hw.storage_used_bytes + hw.storage_available_bytes,
        });
      })
      .catch(() => {
        /* Not fatal: the row is simply not shown. */
      });
    return () => {
      live = false;
    };
  }, [isOpen]);

  // Escape steps back through the sections: previous tab, then the settings
  // home, then out to a new chat. Backspace jumps straight out — unless the
  // user is typing in a field, where it must keep deleting text.
  useEffect(() => {
    if (!isOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        const prev = tabHistory.current.pop();
        if (prev !== undefined) scrollToTab(prev);
        else if (tab !== 'general') scrollToTab('general');
        else onClose();
        return;
      }
      if (e.key === 'Backspace') {
        const t = e.target as HTMLElement | null;
        const typing =
          !!t &&
          (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
        if (!typing) onClose();
      }
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [isOpen, onClose, tab]);

  if (!isOpen) return null;


  // Share of the app's budget each component takes, for the stacked bar. With
  // no limit (Unlimited) the bar is scaled to what is in use instead.
  const usageTotal = storage?.usage.total_bytes ?? 0;
  const barScale = storage && storage.limit_mb > 0
    ? Math.max(storage.limit_mb * 1024 * 1024, usageTotal)
    : usageTotal;
  const usageParts = storage
    ? USAGE_PARTS.map((p, i) => ({ ...p, bytes: storage.usage[p.key], shade: i }))
        .filter((p) => p.bytes > 0)
    : [];

  // One section of the page: its heading and blurb, then its content.
  const block = (id: SettingsTab, content: React.ReactNode) => {
    const t = SETTINGS_TABS.find((x) => x.id === id)!;
    return (
      <div className="settings-block" data-settings-tab={id}>
        {id !== 'about' && (
          <header className="settings-head">
            <h2 className="settings-head__title">{t.label}</h2>
            {t.blurb && <p className="settings-head__blurb">{t.blurb}</p>}
          </header>
        )}
        {content}
      </div>
    );
  };

  return (
    <div className="app-view settings-page">
      <div className="settings-shell">
        {/* Top bar: back, title, and the sections as tabs. Selecting one
            scrolls the page below to it. */}
        <header className="settings-top">
          <div className="settings-top__row">
            <button type="button" className="settings-back" onClick={onClose} title="Back to chat">
              <ArrowLeft size={15} strokeWidth={1.75} aria-hidden="true" />
              <span>Back to chat</span>
            </button>
            <h1 className="ws-landing-title settings-title">Settings</h1>
          </div>
          <nav className="settings-tabs" aria-label="Settings sections">
            {SETTINGS_TABS.map(({ id, label }) => (
              <button
                key={id}
                type="button"
                className={`settings-tab${tab === id ? ' is-on' : ''}`}
                aria-current={tab === id ? 'true' : undefined}
                onClick={() => goTab(id)}
              >
                {label}
              </button>
            ))}
          </nav>
        </header>

        <div className="settings-content" ref={contentRef} onScroll={onContentScroll}>
          <div className="settings-inner ui-anim ui-anim--rise">

            {block('general', (
              <section className="settings-section">
                <div className="settings-themes" role="radiogroup" aria-label="Appearance">
                  {THEME_CHOICES.map(({ id, label }) => (
                    <button
                      key={id}
                      type="button"
                      role="radio"
                      aria-checked={preference === id}
                      className="settings-theme"
                      onClick={() => setPreference(id)}
                    >
                      <span className={`settings-theme__preview settings-theme__preview--${id}`} aria-hidden="true">
                        {/* A tiny screen in that theme: a search bar, an agent
                            bar and one button. Automatic shows both halves. */}
                        {(id === 'system' ? ['light', 'dark'] : [id]).map((half) => (
                          <span key={half} className={`tp tp--${half}`}>
                            <span className="tp-search" />
                            <span className="tp-agent" />
                            <span className="tp-btn" />
                          </span>
                        ))}
                      </span>
                      <span className="settings-theme__label">{label}</span>
                    </button>
                  ))}
                </div>
              </section>
            ))}

            {block('account', (
              <section className="settings-section">
                <div className="settings-profile">
                  <span className={`settings-avatar${isLoggedIn ? '' : ' is-empty'}`} aria-hidden="true">
                    {isLoggedIn ? initialsOf(user?.name, user?.email) : <User size={22} strokeWidth={1.6} />}
                  </span>
                  <div className="settings-profile__text">
                    <span className="settings-profile__name">
                      {isLoggedIn ? user?.name || 'Signed in' : 'Not signed in'}
                    </span>
                    {isLoggedIn && user?.email && <span className="settings-profile__sub">{user.email}</span>}
                  </div>
                  {!isLoggedIn && (
                    <button className="settings-btn settings-btn--primary" onClick={() => onRequestLogin?.()}>
                      Sign in
                    </button>
                  )}
                </div>

                <p className="settings-row__desc settings-privacy">
                  Your account is used only to identify you. Your account keeps the intelligence and information
                  only for yourself, everything stays on your device.
                </p>

                {isLoggedIn && (
                  <div className="settings-row settings-row--action">
                    <button className="settings-link settings-link--danger" onClick={() => logout()}>
                      Sign out
                    </button>
                  </div>
                )}
              </section>
            ))}

            {block('storage', (
              <>
                <section className="settings-section">
                  <h3 className="settings-section__title">Counsel AI</h3>
                  <div className="settings-usage">
                    <div className="settings-usage__head">
                      {storage ? (
                        <>
                          <span className="settings-usage__figure">{storage.usage_human}</span>
                          <span className="settings-usage__of">
                            {storage.limit_mb > 0 ? `of ${storage.limit_human}` : 'used, no limit'}
                          </span>
                          {storage.over_budget && <span className="settings-badge">Over limit</span>}
                        </>
                      ) : (
                        <span className="settings-usage__of">{storageError ? '—' : 'Calculating…'}</span>
                      )}
                    </div>
                    <div className="settings-stack" role="img" aria-label="Storage used by category">
                      {barScale > 0 &&
                        usageParts.map((p) => (
                          <span
                            key={p.key}
                            className={`settings-stack__seg settings-shade-${p.shade}`}
                            style={{ width: `${(p.bytes / barScale) * 100}%` }}
                            title={`${p.label}: ${formatBytes(p.bytes)}`}
                          />
                        ))}
                    </div>
                    {usageParts.length > 0 && (
                      <ul className="settings-legend">
                        {usageParts.map((p) => (
                          <li key={p.key}>
                            <span className={`settings-legend__dot settings-shade-${p.shade}`} />
                            <span className="settings-legend__label">{p.label}</span>
                            <span className="settings-legend__value">{formatBytes(p.bytes)}</span>
                          </li>
                        ))}
                      </ul>
                    )}
                    {storageError && <span className="settings-note settings-note--danger">{storageError}</span>}
                  </div>

                  <div className="settings-row">
                    <div className="settings-row__text">
                      <span className="settings-row__title">Disk limit</span>
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

                </section>

                {disk && (
                  <section className="settings-section">
                    <h3 className="settings-section__title">This drive</h3>
                    <div className="settings-row">
                      <div className="settings-row__text settings-row__text--grow">
                        <span className="settings-row__title">
                          {formatBytes(disk.free)} available
                        </span>
                        <div
                          className="settings-bar"
                          role="progressbar"
                          aria-label="Disk space in use"
                          aria-valuenow={disk.total > 0 ? Math.round((disk.used / disk.total) * 100) : 0}
                          aria-valuemin={0}
                          aria-valuemax={100}
                        >
                          <span
                            className="settings-bar__fill"
                            style={{ width: `${disk.total > 0 ? (disk.used / disk.total) * 100 : 0}%` }}
                          />
                        </div>
                        <span className="settings-row__desc">
                          {formatBytes(disk.used)} of {formatBytes(disk.total)} used
                        </span>
                      </div>
                    </div>
                  </section>
                )}
              </>
            ))}

            {block('models', (
              <>
                <section className="settings-section">
                  <button type="button" className="settings-row settings-row--nav" onClick={() => onOpenModels?.()}>
                    <div className="settings-row__text">
                      <span className="settings-row__title">Manage models</span>
                    </div>
                    <div className="settings-row__control">
                      <ChevronRight size={16} strokeWidth={1.75} className="settings-row__chevron" />
                    </div>
                  </button>
                </section>

                <section className="settings-section">
                  <h3 className="settings-section__title">Access tokens</h3>
                  <div className="settings-row">
                    <div className="settings-row__text">
                      <span className="settings-row__title">Hugging Face</span>
                      <span className="settings-row__desc">
                        <span className={`settings-status${apiKeyStatus.huggingface ? ' is-on' : ''}`} />
                        {apiKeyStatus.huggingface
                          ? 'Connected'
                          : 'Not set'}
                      </span>
                    </div>
                    <div className="settings-row__control">
                      <button
                        className="settings-btn"
                        onClick={() =>
                          showHuggingFaceApiKeyModal(onHuggingFaceTokenChange, setApiKey, refreshApiKeyStatus)
                        }
                      >
                        {apiKeyStatus.huggingface ? 'Change…' : 'Add token…'}
                      </button>
                    </div>
                  </div>
                </section>
              </>
            ))}

            {/* The real MetricsPanel, embedded: one implementation of the
                polling and the charts. */}
            {block('system', (
              <section className="settings-section settings-section--flush">
                <MetricsPanel isOpen embedded />
              </section>
            ))}

            {block('help', <HelpFeedbackPanel isOpen embedded onClose={onClose} isLoggedIn={isLoggedIn} />)}

            {block('about', (
              <>
                <div className="settings-about">
                  <h2 className="settings-about__name">Offline Counsel AI</h2>
                  <span className="settings-about__version">Version 0.1.0</span>
                </div>
              </>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
};

export default SettingsPanel;
