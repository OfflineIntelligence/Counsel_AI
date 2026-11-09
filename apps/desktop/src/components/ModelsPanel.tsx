// Models Panel Component

import React, { useState, useEffect, useRef } from 'react';
import EmbeddingModelsSection from './rag/EmbeddingModelsSection';
import { Search, Download, Star, Trash2, RefreshCw, Pause, Play, Square, ChevronDown, KeyRound, Type, Image as ImageIcon, X, Check } from 'lucide-react';
import { useNotificationHelpers, useErrorNotification } from '../contexts/NotificationContext';
import './ModelsPanel.css';
import LabLogo from './LabLogo';
import './workspace/workspace.css';
import { FilterSelect, ItemMenu, type MenuEntry } from './workspace/FileBrowser';
import { useAuth } from '../contexts/AuthContext';
import { open } from '@tauri-apps/plugin-shell';
import { getApiBaseSync } from '../api/backendUrl';
import { refreshDownloads, useDownloads } from '../api/modelSync';
import { saveApiKey as persistApiKey, getApiKey } from '../api/apiKeys';
import {
  MODELS_CHANGED_EVENT,
  buildSourcePayload,
  announceModelsChanged,
  sanitizeModelId,
  getInterruptedDownloads,
  resumeInterruptedDownload,
  discardInterruptedDownload,
  type InterruptedDownload,
} from '../api/models';
import { getHfToken as getStoredHfToken, setHfToken as storeHfToken, clearHfToken as clearStoredHfToken, HF_TOKEN_EVENT, HF_TOKEN_KEY } from '../api/hfToken';

// Helper function to check if backend is ready
async function checkBackendReadiness(): Promise<boolean> {
  try {
    const response = await fetch(`${getApiBaseSync()}/healthz`);
    return response.ok;
  } catch (error) {
    console.warn('Backend readiness check failed:', error);
    return false;
  }
}

// The HuggingFace-token and gated-model dialogs used to be built here, as
// ~550 lines of `document.createElement` + `innerHTML` + inline styles with
// `background: white; color: black` and an explicit `data-theme="light"`.
// They now live in components/models/ModelDialogs.tsx and are rendered through
// the app's own Dialog primitive, which gives them focus handling, Escape, the
// exit animation and both themes.
//
// This module used to re-export these three so SettingsPanel could keep
// importing showHuggingFaceApiKeyModal from here. It imports them from the bus
// directly now: a module that exports both components and plain functions
// defeats Fast Refresh for the whole file, and this is a 1,600-line panel to
// reload.
import {
  showHuggingFaceApiKeyModal,
  showGatedModelModal,
  showOllamaModal,
} from './models/modelDialogBus';


// Types
interface Model {
  id: string;
  name: string;
  description?: string;
  author?: string;
  status: string;
  size_bytes: number;
  format: string;
  download_source?: string;
  installed_version?: string;
  last_updated?: string;
  tags: string[];
  compatibility_score?: number;
  parameters?: string; // e.g., "7B", "13B", "70B", "671B"
  context_length?: number; // e.g., 4096, 8192, 128000
  provider?: string;
  filename?: string; // Specific filename for HuggingFace models
  /** Multimodal projector filename — present = VISION model. Installing it
   *  downloads this file too; activation loads it via --mmproj. */
  mmproj_filename?: string;
  /** Size of the mmproj file in bytes (0 = unknown/not vision). */
  mmproj_size_bytes?: number;
}

interface DownloadProgress {
  download_id: string;
  model_id: string;
  model_name: string;
  status: string;
  bytes_downloaded: number;
  total_bytes?: number;
  percentage: number;
  speed_bps: number;
  elapsed_time: number;
  estimated_time_remaining?: number;
  error_message?: string;
}

interface HardwareInfo {
  total_ram_gb: number;
  available_ram_gb: number;
  cpu_cores: number;
  gpu_available: boolean;
  gpu_vram_gb?: number;
  storage_used_bytes: number;
  storage_available_bytes: number;
}

type SortOption = 'name' | 'size_asc' | 'size_desc' | 'compatibility' | 'newest' | 'oldest' | 'source' | 'trending' | 'popularity';





interface ActiveModelInfo {
  model_path: string;
  model_name: string;
}

interface SelectedModel {
  id: string;
  name: string;
  source: 'local';
}

// ── Plain-language model details ────────────────────────────────────────────
// The catalogue speaks in repository names and quantisation tags. A lawyer
// needs three answers: which model, what it is good for, and whether this
// computer can run it.

/** Memory a model of this size needs while running, relative to its file size. */
const RUNTIME_OVERHEAD = 1.25;

/* A `hardwareCapacityGb(hw)` helper stood here, returning the largest model
   file this computer could run. It had no callers left. Not folded into
   fitOnThisComputer below, which looks like it duplicates the same arithmetic
   but does not: this one divided the memory budget by RUNTIME_OVERHEAD, while
   fitOnThisComputer multiplies the model's size by it, so substituting one for
   the other would apply the overhead twice and move every verdict. */

/** Whether this computer can run a model of this size, and how well.
 *
 *  `tone` has two values, not three. It was `'good' | 'slow' | 'no'`, but no
 *  branch below ever returned `'no'` -- the two worst cases both report
 *  "May be slow" / `'slow'` and differ only in `rank`, which is what orders
 *  the list. So the `.models-fit-no` rule in ModelsPanel.css could never
 *  match, and it was declared twice with two different colours precisely
 *  because nobody could see which one was winning. */
function fitOnThisComputer(
  sizeBytes: number,
  hw: HardwareInfo | null,
  score?: number,
): { label: string; tone: 'good' | 'slow'; rank: number } | null {
  if (hw && sizeBytes > 0) {
    const need = (sizeBytes / 1024 ** 3) * RUNTIME_OVERHEAD;
    const vram = hw.gpu_available ? (hw.gpu_vram_gb ?? 0) : 0;
    if (need <= vram || need <= hw.total_ram_gb * 0.6) return { label: 'Runs well', tone: 'good', rank: 0 };
    if (need <= hw.total_ram_gb * 0.9) return { label: 'May be slow', tone: 'slow', rank: 1 };
    return { label: 'May be slow', tone: 'slow', rank: 2 };
  }
  if (score != null) {
    if (score >= 0.7) return { label: 'Runs well', tone: 'good', rank: 0 };
    if (score >= 0.4) return { label: 'May be slow', tone: 'slow', rank: 1 };
    return { label: 'May be slow', tone: 'slow', rank: 2 };
  }
  return null;
}

// Order matters: a distilled model names both its maker and its base
// ("DeepSeek-R1-Distill-Qwen"), and the maker must win.
const MAKERS: [RegExp, string][] = [
  [/deepseek/i, 'DeepSeek'],
  [/nemotron|nvidia/i, 'NVIDIA'],
  [/gemma|google|madlad/i, 'Google'],
  [/llama|meta/i, 'Meta'],
  [/qwen|qwq|alibaba/i, 'Alibaba'],
  [/mistral|mixtral|ministral|devstral|magistral/i, 'Mistral AI'],
  [/\bphi|microsoft/i, 'Microsoft'],
  [/granite|ibm/i, 'IBM'],
  [/gpt-oss|openai/i, 'OpenAI'],
  [/smol|huggingface/i, 'Hugging Face'],
  [/command-r|aya|cohere/i, 'Cohere'],
  [/grok|xai/i, 'xAI'],
  [/kimi|moonshot/i, 'Moonshot AI'],
  [/glm|chatglm|zhipu|z-ai|zai-org/i, 'Zhipu AI'],
  [/minimax/i, 'MiniMax'],
  [/step-?\d|stepfun/i, 'StepFun'],
  [/seed-?oss|doubao|bytedance/i, 'ByteDance'],
  [/claude|anthropic/i, 'Anthropic'],
  [/hunyuan|tencent/i, 'Tencent'],
  [/ernie|baidu/i, 'Baidu'],
];

/** Epoch ms of a catalogue entry's last update; 0 when it declares none. */
function updatedAt(model: Model): number {
  const t = model.last_updated ? Date.parse(model.last_updated) : NaN;
  return Number.isNaN(t) ? 0 : t;
}

function makerName(model: Model): string | null {
  const text = `${model.provider ?? ''} ${model.author ?? ''} ${model.name} ${model.id}`;
  for (const [pattern, name] of MAKERS) if (pattern.test(text)) return name;
  const raw = model.provider || model.author;
  return raw ? raw.charAt(0).toUpperCase() + raw.slice(1) : null;
}

/** "gemma-4-12B-it-qat-q4_0" → "Gemma 4 · 12B". */
function friendlyModelName(model: Model): string {
  let base = model.name.split('/').pop() ?? model.name;
  base = base
    .replace(/\.gguf$/i, '')
    .replace(/[-_.](gguf|it|instruct|chat|qat|unsloth|imatrix)(?=[-_.]|$)/gi, '')
    .replace(/[-_.](i?q\d[\w]*|f16|f32|bf16|fp16|int[48])(?=[-_.]|$)/gi, '')
    .replace(/[-_.]{2,}/g, '-')
    .replace(/[-_.]+$/g, '');
  const params = base.match(/(?:^|[-_.\s])(\d+(?:\.\d+)?[bm])(?:[-_.\s]|$)/i)?.[1];
  const withoutParams = params ? base.replace(new RegExp(`[-_.]?${params.replace('.', '\\.')}`, 'i'), '') : base;
  const words = withoutParams
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map(w => (/^[a-z]/.test(w) ? w.charAt(0).toUpperCase() + w.slice(1) : w));
  const title = words.join(' ') || model.name;
  return params ? `${title} · ${params.toUpperCase()}` : title;
}

/* A `goodFor(model)` helper stood here: sixteen branches of product copy
   mapping a model name to a one-line "good for ..." description ("Careful
   legal reasoning", "Long contracts, many languages"). Nothing called it --
   the column it was written for is not in the table. Removed rather than left
   as a silent fixture; the copy is worth having, but it belongs next to the
   UI that shows it. */

/** Input modalities as small boxed icons, like the Input column on models.dev. */
function Modalities({ vision }: { vision: boolean }) {
  return (
    <span className="models-modality" aria-label={vision ? 'Text and images' : 'Text'}>
      <span className="models-modality__icon" title="Text">
        <Type size={13} strokeWidth={1.75} />
      </span>
      {vision && (
        <span className="models-modality__icon" title="Images and scanned documents">
          <ImageIcon size={13} strokeWidth={1.75} />
        </span>
      )}
    </span>
  );
}
/** Whether the access-token panel was open, kept while the app runs so both tabs and later visits agree. */
let tokenPanelRemembered = false;

/**
 * Company filter: the same control as the Sort menu beside it (same button,
 * same menu, same rows), with a checkbox on each company so several can be
 * shown at once. The menu stays open while you tick, carries a search box
 * because the catalogue has dozens of makers, and "All companies" clears the
 * selection. Counts come from the whole catalogue so the menu never shrinks.
 */
const CompanySelect: React.FC<{
  value: string[];
  companies: { name: string; count: number }[];
  onChange: (value: string[]) => void;
}> = ({ value, companies, onChange }) => {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const anchor = useRef<HTMLDivElement | null>(null);
  const field = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    if (!open) return;
    field.current?.focus();
    const onPointer = (e: MouseEvent) => {
      if (!anchor.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onPointer);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onPointer);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  const shown = companies.filter(c => c.name.toLowerCase().includes(query.trim().toLowerCase()));
  const toggle = (name: string) =>
    onChange(value.includes(name) ? value.filter(v => v !== name) : [...value, name]);
  const label = value.length === 0 ? 'All companies' : value.length === 1 ? value[0] : `${value.length} companies`;

  return (
    <div className="ws-filter-anchor" ref={anchor}>
      <button
        type="button"
        className="ws-filter"
        aria-label="Filter by company"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen(o => !o)}
      >
        {label}
        <ChevronDown size={13} strokeWidth={2.25} />
      </button>
      {open && (
        <div className="ws-filter-menu models-companies" role="listbox" aria-label="Company" aria-multiselectable="true">
          <div className="models-companies__search">
            <Search size={14} strokeWidth={2} />
            <input
              ref={field}
              type="search"
              value={query}
              onChange={e => setQuery(e.target.value)}
              placeholder="Search companies"
              aria-label="Search companies"
            />
          </div>
          <button
            type="button"
            role="option"
            aria-selected={value.length === 0}
            className={`ws-filter-option models-company${value.length === 0 ? ' is-on' : ''}`}
            onClick={() => onChange([])}
          >
            <span className={`models-check${value.length === 0 ? ' is-on' : ''}`} aria-hidden="true">
              {value.length === 0 && <Check size={11} strokeWidth={3} />}
            </span>
            <span className="models-company__name">All companies</span>
          </button>
          {shown.map(c => {
            const on = value.includes(c.name);
            return (
              <button
                key={c.name}
                type="button"
                role="option"
                aria-selected={on}
                className={`ws-filter-option models-company${on ? ' is-on' : ''}`}
                onClick={() => toggle(c.name)}
              >
                <span className={`models-check${on ? ' is-on' : ''}`} aria-hidden="true">
                  {on && <Check size={11} strokeWidth={3} />}
                </span>
                <LabLogo company={c.name} />
                <span className="models-company__name">{c.name}</span>
                <span className="models-companies__count">{c.count}</span>
              </button>
            );
          })}
          {shown.length === 0 && <p className="models-companies__empty">No company matches “{query}”.</p>}
        </div>
      )}
    </div>
  );
};

const ModelsPanel: React.FC<{
  isOpen: boolean;
  onClose: () => void;
  selectedModel?: SelectedModel | null;
  onSelectModel?: (model: SelectedModel | null) => void;
  focusHfTokenInput?: boolean;
}> = ({ isOpen, selectedModel, onSelectModel, focusHfTokenInput }) => {
  const [models, setModels] = useState<Model[]>([]);
  const [downloads, setDownloads] = useState<DownloadProgress[]>([]);
  const [searchQuery, setSearchQuery] = useState('');

  // Starred models, stored by the backend so they survive a catalogue refresh
  // and a restart. Changed optimistically; a failed save puts the list back.
  const [starred, setStarred] = useState<Set<string>>(new Set());
  useEffect(() => {
    let live = true;
    fetch(`${getApiBaseSync()}/models/marks`)
      .then(r => (r.ok ? r.json() : null))
      .then(marks => {
        if (live && marks?.starred) setStarred(new Set<string>(marks.starred));
      })
      .catch(() => {
        /* Backend not ready: nothing starred yet is the right empty state. */
      });
    return () => {
      live = false;
    };
  }, []);

  const toggleStar = async (modelId: string, name: string) => {
    const next = !starred.has(modelId);
    const previous = starred;
    setStarred(s => {
      const copy = new Set(s);
      if (next) copy.add(modelId);
      else copy.delete(modelId);
      return copy;
    });
    try {
      const r = await fetch(`${getApiBaseSync()}/models/marks/star`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model_id: modelId, starred: next }),
      });
      if (!r.ok) throw new Error(await r.text());
      const marks = await r.json();
      setStarred(new Set<string>(marks.starred ?? []));
    } catch (e) {
      setStarred(previous);
      showError(`Couldn't ${next ? 'star' : 'unstar'} ${name}`, e instanceof Error ? e.message : String(e));
    }
  };
  const [isLoading, setIsLoading] = useState(false);
  const [fetchError, setFetchError] = useState<string | null>(null);
  useErrorNotification(fetchError, "Couldn't load models", () => setFetchError(null));
  const [activeTab, setActiveTab] = useState<'installed' | 'available' | 'starred' | 'downloads'>('available');
  const [hardwareInfo, setHardwareInfo] = useState<HardwareInfo | null>(null);
  const [activeModelInfo, setActiveModelInfo] = useState<ActiveModelInfo | null>(null);
  const [sortBy, setSortBy] = useState<SortOption>('name');
  /** The companies ticked in the filter; none ticked = every company. */
  const [company, setCompany] = useState<string[]>([]);
  const [showHfTokenInput, setShowHfTokenInput] = useState(() => tokenPanelRemembered);
  useEffect(() => {
    tokenPanelRemembered = showHfTokenInput;
  }, [showHfTokenInput]);
  const { showSuccess, showError, showDownload } = useNotificationHelpers();
  const { user, setApiKey } = useAuth();
  
  // One token, everywhere: any save or clear (this panel, the install prompt,
  // Settings, another window) updates the field and the status dot at once.
  useEffect(() => {
    const sync = () => setHfToken(getStoredHfToken());
    const onStorage = (e: StorageEvent) => {
      if (e.key === null || e.key === HF_TOKEN_KEY) sync();
    };
    window.addEventListener(HF_TOKEN_EVENT, sync);
    window.addEventListener('storage', onStorage);
    return () => {
      window.removeEventListener(HF_TOKEN_EVENT, sync);
      window.removeEventListener('storage', onStorage);
    };
  }, []);

  const [hfToken, setHfToken] = useState<string>(() => {
    // Get initial value from browser storage first
    const storedToken = getStoredHfToken();
    if (storedToken) return storedToken;
    // Get from auth context if available
    return user?.apiKeys?.huggingface || '';
  });
  
  // The access-token bar stays open until a complete token is entered; only
  // then can it be closed (and it reopens on its own if the token is cleared).
  // "Entered" means a complete Hugging Face token (hf_ followed by its 30+
  // letters and digits), not a partly typed one.
  const hasToken = /^hf_[A-Za-z0-9]{30,}$/.test(hfToken.trim());
  const tokenOpen = showHfTokenInput || !hasToken;
  const toggleTokenPanel = () => {
    if (!hasToken) return;
    setShowHfTokenInput(v => !v);
  };
  // Without a token the bar is held open; keep that state once typing begins
  // (the token saves as it is typed), so it closes only when the user asks.
  useEffect(() => {
    if (!hasToken) setShowHfTokenInput(true);
  }, [hasToken]);

  // State for model removal confirmation
  const [showRemoveConfirmation, setShowRemoveConfirmation] = useState(false);
  const [modelToRemove, setModelToRemove] = useState<Model | null>(null);

  // Sync hfToken with auth context when user changes
  useEffect(() => {
    if (user?.apiKeys?.huggingface && user.apiKeys.huggingface !== hfToken) {
      setHfToken(user.apiKeys.huggingface);
    }
  }, [user, hfToken]);

  // Load HF token from backend if missing
  useEffect(() => {
    if (!hfToken) {
      getApiKey('huggingface').then(key => {
        if (key) {
          setHfToken(key);
          setApiKey?.('huggingface', key);
        }
      }).catch(console.error);
    }
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Fetch data when panel opens and cleanup when closes
  useEffect(() => {
    if (isOpen) {
      // Immediate fetch
      fetchModels();
      fetchDownloads();
      fetchHardwareInfo();
      fetchActiveModel();
      
      // Refresh again after a short delay to ensure backend is fully ready
      // This helps catch any models that might have been missed in initial load
      const refreshTimer = setTimeout(() => {
        fetchModels(true);
        fetchDownloads();
      }, 500);
      
      return () => clearTimeout(refreshTimer);
    } else {
      // Reset state when closed to prevent stale data
      setModels([]);
      setDownloads([]);
      setHardwareInfo(null);
      setFetchError(null);
    }
  }, [isOpen]);

  // Effect to handle focusing API key input when requested
  // Effect to handle focusing HuggingFace token input when requested
  useEffect(() => {
    if (focusHfTokenInput) {
      // If the HuggingFace token input should be focused, make sure it's visible first
      if (!showHfTokenInput) {
        setShowHfTokenInput(true);
      }
      
      // Wait for the input to be rendered, then focus it
      const timer = setTimeout(() => {
        const hfTokenInput = document.getElementById('hf-token-input') as HTMLInputElement;
        if (hfTokenInput) {
          hfTokenInput.focus();
        }
      }, 100); // Small delay to ensure DOM is updated
      
      return () => clearTimeout(timer);
    }
  }, [focusHfTokenInput, showHfTokenInput]);

  // The shared watcher polls (fast while anything downloads); this page just
  // mirrors it while open.
  const sharedDownloads = useDownloads();
  useEffect(() => {
    if (isOpen) setDownloads(sharedDownloads as unknown as DownloadProgress[]);
  }, [isOpen, sharedDownloads]);

  const refreshedDownloads = useRef<Set<string>>(new Set());
  // Detect completed downloads and refresh model list
  useEffect(() => {
    const completedDownloads = downloads.filter(d => d.status === 'Completed');

    // Refresh once per newly completed download, not on every poll while it lingers.
    const fresh = completedDownloads.filter(d => !refreshedDownloads.current.has(d.download_id));
    if (fresh.length > 0) {
      fresh.forEach(d => refreshedDownloads.current.add(d.download_id));
      // Refresh the model list to pick up the new "Installed" status from backend
      fetchModels(true);
    }

    if (completedDownloads.length > 0) {

      // Clean up completed downloads after a short delay to show success state
      const timer = setTimeout(() => {
        setDownloads(prev => prev.filter(d => d.status !== 'Completed'));
      }, 2000); // 2 second delay to let user see 100% completion

      return () => clearTimeout(timer);
    }
  }, [downloads]);

  // `silent` refreshes in the background: the list stays on screen instead of
  // being swapped for the loading state, so nothing flickers mid-download.
  const fetchModels = async (silent = false) => {
    try {
      if (!silent) setIsLoading(true);
      setFetchError(null);
      
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        setFetchError(`Backend is not ready. Please ensure the offline-intelligence service is started on ${getApiBaseSync()}.`);
        setModels([]);
        return;
      }
      
      const response = await fetch(`${getApiBaseSync()}/models`);
      if (response.ok) {
        const data = await response.json();
        // Keep the same array when nothing changed, so the list does not re-render.
        setModels(prev => (JSON.stringify(prev) === JSON.stringify(data) ? prev : data));
        // Do nothing if backend returns empty list, keep the models as received
      } else {
        setFetchError(`Backend returned HTTP ${response.status}. Make sure the backend is running.`);
      }
    } catch (error) {
      console.error('Failed to fetch models:', error);
      // Provide a more helpful error message with troubleshooting steps
      setFetchError(`Cannot connect to backend. The backend may not be running. Please ensure the offline-intelligence service is started on ${getApiBaseSync()}.`);
      // Set empty models when backend is unreachable
      setModels([]);
    } finally {
      setIsLoading(false);
    }
  };

  const fetchDownloads = async () => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        console.warn('Backend not ready for download fetch');
        return;
      }
      
      setDownloads((await refreshDownloads()) as unknown as DownloadProgress[]);
    } catch (error) {
      console.error('Failed to fetch downloads:', error);
      // Don't show an error for downloads as it's not critical to main functionality
    }
  };

  const fetchHardwareInfo = async () => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        console.warn('Backend not ready for hardware info fetch');
        return;
      }
      
      const response = await fetch(`${getApiBaseSync()}/hardware/info`);
      if (response.ok) {
        setHardwareInfo(await response.json());
      } else {
        console.warn('Hardware info endpoint returned non-200 status:', response.status);
      }
    } catch (error) {
      console.error('Failed to fetch hardware info:', error);
      // Don't show an error for hardware info as it's not critical
    }
  };

  const fetchActiveModel = async () => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        console.warn('Backend not ready for active model fetch');
        return;
      }
      
      const response = await fetch(`${getApiBaseSync()}/models/active`);
      if (response.ok) {
        const activeModel = await response.json();
        setActiveModelInfo(activeModel);
      } else {
        console.warn('Active model endpoint returned non-200 status:', response.status);
      }
    } catch (error) {
      console.error('Failed to fetch active model:', error);
      // Don't show an error for active model as it's not critical
    }
  };

  const refreshCatalog = async () => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError("Counsel isn't ready yet", 'Its local service is still starting. Try again in a moment.');
        return;
      }
      
      const response = await fetch(`${getApiBaseSync()}/models/refresh`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ source: 'all' }),
      });

      if (!response.ok) {
        console.error('Failed to refresh model catalog:', response.status);
      }
    } catch (error) {
      console.error('Failed to refresh model catalog:', error);
    } finally {
      await fetchModels();
      await fetchHardwareInfo();
    }
  };

  const handleInstallModel = async (model: Model) => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError("Counsel isn't ready yet", 'Its local service is still starting. Try again in a moment.');
        return;
      }
      
      // Get HF token from browser storage if available
      const hfToken = getStoredHfToken();
      
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
          hf_token: hfToken || undefined
        })
      });
      if (response.ok) {
        showDownload('Download started', `${model.name} is downloading.`);
        // Immediately fetch downloads to get the new entry
        await fetchDownloads();
        // Switch to downloads tab to show progress
        setActiveTab('downloads');
        // Fetch again after a short delay to ensure we have fresh data
        setTimeout(fetchDownloads, 500);
      } else {
        const errorText = await response.text();
        if (errorText.includes('401 Unauthorized') && model.download_source === 'huggingface') {
          // Check if this is a gated model that requires access request
          const repoIdMatch = errorText.match(/REPO_ID:([^\s]+)/);
          const repoId = repoIdMatch ? repoIdMatch[1] : null;
          
          if (repoId) {
            // Show gated model modal with option to visit HuggingFace page
            showGatedModelModal(repoId, () => {
              // Retry the download when user clicks "Retry Download"
              handleInstallModel(model);
            });
          } else {
            // No REPO_ID in the error, so the model is not gated -- there is
            // simply no token. Same dialog Settings uses, and the retry is
            // wired to its completion so the download resumes once the token
            // is in. Three hundred lines of hand-built DOM used to live here.
            showHuggingFaceApiKeyModal(undefined, setApiKey, () => handleInstallModel(model));
          }
        } else if (errorText.includes('401 Unauthorized') && model.download_source === 'ollama') {
          showOllamaModal();
        } else {
          showError("Couldn't start download", `${model.name} didn't start downloading. Try again.`);
        }
      }
    } catch (error) {
      console.error('Failed to install model:', error);
      showError("Couldn't start download", `${model.name} didn't start downloading. Try again.`);
    }
  };

  const handleRemoveModel = async (modelId: string) => {
    // Find the model to be removed to show details in confirmation
    const model = models.find(m => m.id === modelId);
    if (model) {
      setModelToRemove(model);
      setShowRemoveConfirmation(true);
    }
  };
  
  const confirmRemoveModel = async () => {
    if (!modelToRemove) return;
    
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError("Counsel isn't ready yet", 'Its local service is still starting. Try again in a moment.');
        return;
      }
      
      const response = await fetch(`${getApiBaseSync()}/models/remove?model_id=${modelToRemove.id}`, { method: 'DELETE' });
      if (response.ok) {
        showSuccess('Model removed', 'It no longer takes up space on this computer.');
        fetchModels(true);
      } else {
        showError("Couldn't remove model", 'Try again. If it keeps happening, restart Counsel.');
      }
    } catch (error) {
      console.error('Failed to remove model:', error);
      showError("Couldn't remove model", 'Try again. If it keeps happening, restart Counsel.');
    } finally {
      // Close the confirmation modal
      setShowRemoveConfirmation(false);
      setModelToRemove(null);
    }
  };
  
  const cancelRemoveModel = () => {
    setShowRemoveConfirmation(false);
    setModelToRemove(null);
  };

  const handleSwitchModel = async (modelId: string, modelName: string) => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError("Counsel isn't ready yet", 'Its local service is still starting. Try again in a moment.');
        return;
      }
      
      const response = await fetch(`${getApiBaseSync()}/models/switch`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model_id: modelId }),
      });

      if (response.ok) {
        showSuccess('Model switched', `${modelName} is ready.`);
        announceModelsChanged();
        // Optionally update the selected model in the UI
        onSelectModel?.({ id: modelId, name: modelName, source: 'local' });
      } else {
        // The backend returns structured errors: { error, detail, action }.
        // Engine-cause failures are surfaced with their exact cause and routed
        // to the engine setup screen, which offers the explicit install action.
        const errorData = await response.json().catch(() => ({} as any));
        const engineErrors = ['engine_not_installed', 'engine_corrupted', 'engine_manager_unavailable'];
        if (engineErrors.includes(errorData.error)) {
          showError(
            'Engine unavailable',
            errorData.detail || 'The inference engine is not available on this machine.'
          );
          // Ask the engine setup gate to re-check /healthz and take over.
          window.dispatchEvent(new CustomEvent('oca-recheck-engine'));
        } else {
          showError("Couldn't switch model", errorData.detail || errorData.error || `${modelName} couldn't be loaded.`);
        }
      }
    } catch (error) {
      console.error('Failed to switch model:', error);
      showError("Couldn't switch model", `${modelName} couldn't be loaded.`);
    }
  };

  const handlePauseDownload = async (downloadId: string) => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError("Counsel isn't ready yet", 'Its local service is still starting. Try again in a moment.');
        return;
      }
      
      await fetch(`${getApiBaseSync()}/models/downloads/pause?download_id=${downloadId}`, { method: 'POST' });
      announceModelsChanged();
      fetchDownloads();
    } catch (e) { console.error('Pause failed:', e); }
  };

  const handleResumeDownload = async (downloadId: string) => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError("Counsel isn't ready yet", 'Its local service is still starting. Try again in a moment.');
        return;
      }
      
      await fetch(`${getApiBaseSync()}/models/downloads/resume?download_id=${downloadId}`, { method: 'POST' });
      announceModelsChanged();
      fetchDownloads();
    } catch (e) { console.error('Resume failed:', e); }
  };

  const handleCancelDownload = async (downloadId: string) => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError("Counsel isn't ready yet", 'Its local service is still starting. Try again in a moment.');
        return;
      }
      
      await fetch(`${getApiBaseSync()}/models/downloads/cancel?download_id=${downloadId}`, { method: 'POST' });
      announceModelsChanged();
      fetchDownloads();
    } catch (e) { console.error('Cancel failed:', e); }
  };

  const formatBytes = (bytes: number): string => {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
  };

  const isInstalled = (model: Model) => model.status === 'Installed';



  // Find download progress for a model
  // Downloads are tracked under the backend's sanitized id ("author_repo")
  // while the catalog lists the raw one ("author/repo"); comparing only the
  // raw id meant no catalog row ever found its download, so rows never showed
  // progress. The name is the fallback for ids the backend hashes.
  // Each download belongs to exactly ONE model: the one whose id it names, or
  // failing that the first with its name. The catalogue can list two builds
  // under one name, and matching by name alone showed one download twice.
  const idMatches = (d: DownloadProgress, modelId: string) =>
    d.model_id === modelId || d.model_id === sanitizeModelId(modelId);
  const ownerOf = (d: DownloadProgress) =>
    (models.find(m => idMatches(d, m.id)) ?? models.find(m => m.name === d.model_name))?.id;
  const getDownloadForModel = (modelId: string, modelName?: string) =>
    downloads.find(
      d =>
        (d.status === 'Downloading' || d.status === 'Starting' || d.status === 'Paused' || d.status === 'Queued') &&
        (idMatches(d, modelId) || (!!modelName && d.model_name === modelName)) &&
        (ownerOf(d) ?? modelId) === modelId,
    );

  // Installs left unfinished when the app last closed.
  const [interrupted, setInterrupted] = useState<InterruptedDownload[]>([]);
  const fetchInterrupted = async () => {
    try {
      setInterrupted(await getInterruptedDownloads());
    } catch {
      setInterrupted([]);
    }
  };
  useEffect(() => {
    if (!isOpen) return;
    void fetchInterrupted();
    const onChanged = () => {
      void fetchInterrupted();
      void fetchDownloads();
    };
    window.addEventListener(MODELS_CHANGED_EVENT, onChanged);
    return () => window.removeEventListener(MODELS_CHANGED_EVENT, onChanged);
  }, [isOpen]);

  const handleResumeInterrupted = async (d: InterruptedDownload) => {
    const result = await resumeInterruptedDownload(d);
    if (result.ok) {
      showDownload('Download resumed', `${d.model_name} is downloading again.`);
      await fetchDownloads();
      await fetchInterrupted();
    } else {
      showError("Couldn't resume download", result.message);
    }
  };

  const handleDiscardInterrupted = async (d: InterruptedDownload) => {
    await discardInterruptedDownload(d.model_id);
    await fetchInterrupted();
  };

  // Priority companies order (first priority)
  const priorityCompanies = [
    'google', 'google deepmind', 'deepmind', 'deepseek', 'anthropic', 'moonshot ai', 'meta',
    'openai', 'zlm', 'microsoft', 'x ai', 'xai', 'mistral', 'inflection ai', 'amazon'
  ];

  // Get company priority score (lower = higher priority)
  const getCompanyPriority = (model: Model): number => {
    const provider = (model.provider || model.author || '').toLowerCase();
    const name = model.name.toLowerCase();
    
    for (let i = 0; i < priorityCompanies.length; i++) {
      if (provider.includes(priorityCompanies[i]) || name.includes(priorityCompanies[i])) {
        return i;
      }
    }
    return priorityCompanies.length; // Others come after priority companies
  };

  const sortModels = (list: Model[]) => {
    return [...list].sort((a, b) => {
      // Company priority orders the default view. An explicit sort (size,
      // newest, oldest, name) must not be overruled by it, or choosing
      // "Largest first" would still group by company and look broken.
      const explicit = sortBy === 'size_asc' || sortBy === 'size_desc' || sortBy === 'newest' || sortBy === 'oldest' || sortBy === 'name';
      if (!explicit) {
        const priorityA = getCompanyPriority(a);
        const priorityB = getCompanyPriority(b);
        if (priorityA !== priorityB) {
          return priorityA - priorityB;
        }
      }
      
      // Then apply selected sort
      switch (sortBy) {
        case 'name': return a.name.localeCompare(b.name);
        case 'size_asc': return a.size_bytes - b.size_bytes;
        case 'size_desc': return b.size_bytes - a.size_bytes;
        // Catalogue entries without a date sort last either way, rather than
        // being treated as 1970 and flooding the top of "Oldest".
        case 'newest': return updatedAt(b) - updatedAt(a);
        case 'oldest': {
          const ua = updatedAt(a);
          const ub = updatedAt(b);
          if (!ua || !ub) return (ua ? 0 : 1) - (ub ? 0 : 1);
          return ua - ub;
        }
        case 'compatibility':
          return (fitOnThisComputer(a.size_bytes, hardwareInfo, a.compatibility_score)?.rank ?? 3) -
            (fitOnThisComputer(b.size_bytes, hardwareInfo, b.compatibility_score)?.rank ?? 3) ||
            b.size_bytes - a.size_bytes;
        case 'source':
          return (a.download_source || '').localeCompare(b.download_source || '');
        case 'trending':
          // Sort by popularity/downloads if available, fallback to name
          return (b.tags.length || 0) - (a.tags.length || 0) || a.name.localeCompare(b.name);
        case 'popularity':
          // Sort by provider priority first, then by name
          return a.name.localeCompare(b.name);
        default: return 0;
      }
    });
  };

  const filteredModels = sortModels(models.filter(model => {
    if (company.length > 0 && !company.includes(makerName(model) ?? "")) return false;
    const searchLower = searchQuery.toLowerCase();
    const matchesSearch = searchQuery === '' ||
      model.name.toLowerCase().includes(searchLower) ||
      model.id.toLowerCase().includes(searchLower) ||
      model.description?.toLowerCase().includes(searchLower) ||
      model.provider?.toLowerCase().includes(searchLower) ||
      model.author?.toLowerCase().includes(searchLower) ||
      model.tags.some(tag => tag.toLowerCase().includes(searchLower));
    return matchesSearch;
  }));
  const filteredModelsAll = filteredModels;

  if (!isOpen) return null;

  const activeDownloadCount = downloads.filter(d => d.status === 'Downloading' || d.status === 'Starting').length;
  // Only downloads still in flight (or stopped part-way and needing a
  // decision). A finished one leaves this tab at once: the completion effect
  // above refreshes the catalogue, so it reappears under "On this computer".
  // Filtered here rather than in state because each poll replaces the list
  // with the backend's, which keeps completed entries for a while.
  const shownDownloads = downloads.filter(d => d.status !== 'Cancelled' && d.status !== 'Completed');
  const downloadsTabCount =
    downloads.filter(d => ['Queued', 'Starting', 'Downloading', 'Paused'].includes(d.status)).length + interrupted.length;
  const formatEta = (secs?: number) => {
    if (!secs || !isFinite(secs)) return '';
    if (secs < 60) return `${Math.round(secs)}s left`;
    if (secs < 3600) return `${Math.round(secs / 60)} min left`;
    return `${(secs / 3600).toFixed(1)} h left`;
  };

  const sortOptions: [SortOption, string][] = [
    ['compatibility', 'Best for this computer'],
    ['name', 'Name'],
    ['size_desc', 'Largest first'],
    ['size_asc', 'Smallest first'],
    ['newest', 'Newest'],
    ['oldest', 'Oldest'],
  ];

  // Companies come from the whole catalogue, not the filtered list, so the
  // menu does not shrink as you filter by it.
  const companies: { name: string; count: number }[] = (() => {
    const counts = new Map<string, number>();
    for (const m of models) {
      const maker = makerName(m);
      if (maker) counts.set(maker, (counts.get(maker) ?? 0) + 1);
    }
    return [...counts.entries()]
      .map(([name, count]) => ({ name, count }))
      .sort((a, b) => b.count - a.count || a.name.localeCompare(b.name));
  })();

  const isActive = (model: Model) =>
    selectedModel?.id === model.id ||
    Boolean(
      activeModelInfo?.model_path &&
        (activeModelInfo.model_path.includes(model.id) ||
          (model.filename && activeModelInfo.model_path.includes(model.filename)) ||
          activeModelInfo.model_name.includes(model.name)),
    );

  const onThisComputer = (model: Model) => isInstalled(model) || Boolean(getDownloadForModel(model.id, model.name));
  const listedAll = activeTab === 'installed'
    ? filteredModelsAll.filter(onThisComputer)
    : activeTab === 'starred'
      ? filteredModelsAll.filter(m => starred.has(m.id))
      : filteredModelsAll;
  // One row per model, even if the catalogue repeats an id.
  const unique = listedAll.filter((m, i, all) => all.findIndex(x => x.id === m.id) === i);
  // On this computer, builds that read the same ("Gemma 3 · 4B" from both a
  // -it and a -qat repo) are one model to the user: keep the row that matters
  // most — in use, then installed, then downloading — and drop the rest.
  const rank = (m: Model) => (isActive(m) ? 3 : isInstalled(m) ? 2 : getDownloadForModel(m.id, m.name) ? 1 : 0);
  const listed = activeTab === 'installed'
    ? unique.filter(m => !unique.some(o => o !== m && friendlyModelName(o) === friendlyModelName(m) &&
        (rank(o) > rank(m) || (rank(o) === rank(m) && unique.indexOf(o) < unique.indexOf(m)))))
    : unique;

  const installWithTokenCheck = (model: Model) => {
    if (model.download_source === 'huggingface' && !getStoredHfToken()) {
      showHuggingFaceApiKeyModal(undefined, setApiKey, () => handleInstallModel(model));
    } else {
      void handleInstallModel(model);
    }
  };

  /**
   * The ⋮ menu for one model: what can be done with it in its current state,
   * then marking it, then removal last and in the danger colour.
   */
  const rowMenu = (model: Model): MenuEntry[] => {
    const name = friendlyModelName(model);
    const dl = getDownloadForModel(model.id, model.name);
    const primary: MenuEntry[] = dl
      ? [
          dl.status === 'Paused'
            ? { label: 'Resume download', onSelect: () => handleResumeDownload(dl.download_id) }
            : { label: 'Pause download', onSelect: () => handlePauseDownload(dl.download_id) },
        ]
      : isInstalled(model)
        ? isActive(model)
          ? []
          : [{ label: 'Use this model', onSelect: () => handleSwitchModel(model.id, model.name) }]
        : [{ label: 'Download', onSelect: () => installWithTokenCheck(model) }];

    const marks: MenuEntry[] = [
      starred.has(model.id)
        ? { label: 'Remove from starred', onSelect: () => void toggleStar(model.id, name) }
        : { label: 'Add to starred', onSelect: () => void toggleStar(model.id, name) },
      {
        label: 'Copy model ID',
        onSelect: () => {
          void navigator.clipboard?.writeText(model.id);
          showSuccess('Model ID copied', model.id);
        },
      },
    ];

    const removal: MenuEntry[] = dl
      ? [{ label: 'Stop download', danger: true, onSelect: () => handleCancelDownload(dl.download_id) }]
      : isInstalled(model)
        ? [{ label: 'Remove from this computer', danger: true, onSelect: () => handleRemoveModel(model.id) }]
        : [];

    return [primary, marks, removal]
      .filter(group => group.length > 0)
      .flatMap((group, i) => (i === 0 ? group : (['separator', ...group] as MenuEntry[])));
  };

  return (
    <div className="app-view models-page">
      <div className="ws-landing models-landing">
        <header className="ws-landing-head models-head">
          <h1 className="ws-landing-title">Models</h1>
        </header>

        <div className="ws-views" role="tablist" aria-label="Model views">
          {([
            ['available', 'All models'],
            ['installed', 'On this computer'],
            ['starred', 'Starred'],
            ['downloads', 'Downloads'],
          ] as const).map(([tab, label]) => (
            <button
              key={tab}
              type="button"
              role="tab"
              aria-selected={activeTab === tab}
              className={`ws-view-tab${activeTab === tab ? ' is-on' : ''}`}
              onClick={() => setActiveTab(tab)}
            >
              {label}
              {tab === 'starred' && starred.size > 0 && (
                <span className="models-tab-count">{starred.size}</span>
              )}
              {tab === 'installed' && activeDownloadCount > 0 && (
                <span className="models-tab-count">{activeDownloadCount}</span>
              )}
              {tab === 'downloads' && downloadsTabCount > 0 && (
                <span className="models-tab-count">{downloadsTabCount}</span>
              )}
            </button>
          ))}
        </div>

        <div className="ws-searchbar">
          <Search size={15} strokeWidth={2.25} className="ws-search-icon" />
          <input
            className="ws-search-input"
            type="search"
            placeholder="Search models"
            aria-label="Search models"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') setSearchQuery('');
            }}
          />
          {searchQuery && (
            <button type="button" className="ws-close" onClick={() => setSearchQuery('')} aria-label="Clear search">
              <X size={14} strokeWidth={1.75} />
            </button>
          )}
        </div>

        <section className="ws-files-box models-box" aria-label="Models">
          <div className="ws-files-toolbar">
            <div className="ws-filters">
              <div className="ws-toolbar-actions">
                <button
                  type="button"
                  className="ws-pill ws-create-folder vault-pill models-token-btn"
                  onClick={toggleTokenPanel}
                  aria-expanded={tokenOpen}
                  title={hasToken ? undefined : 'Enter a complete access token to close this'}
                >
                  Access token
                </button>
                <FilterSelect label="Sort models" value={sortBy} options={sortOptions} onChange={setSortBy} />
                <CompanySelect value={company} companies={companies} onChange={setCompany} />
              </div>
            </div>
            <div className="ws-toolbar-actions">
              <button
                type="button"
                className="ws-pill ws-create-folder models-icon-btn"
                onClick={refreshCatalog}
                title="Refresh the model list"
                aria-label="Refresh the model list"
              >
                <RefreshCw size={16} strokeWidth={1.75} />
              </button>
            </div>
          </div>

          {/* Always mounted; the CSS grid row eases between 0 and its content
              height, so opening and closing are one continuous slide. */}
          <div
            className={`models-token${tokenOpen ? ' is-open' : ''}`}
            aria-hidden={!tokenOpen}
            {...(tokenOpen ? {} : ({ inert: '' } as object))}
          >
            <div className="models-token__inner">
              <div className="models-token__text">
                <span className={`models-key-dot${hfToken ? ' is-set' : ''}`} aria-hidden="true" />
                <span>HuggingFace access token</span>
                <span className="models-token__hint">
                  Get one at{' '}
                  <a
                    href="#"
                    onClick={async (e) => {
                      e.preventDefault();
                      await open('https://huggingface.co/settings/tokens');
                    }}
                  >
                    huggingface.co/settings/tokens
                  </a>
                </span>
              </div>
              <div className="ws-searchbar models-token__field">
                <KeyRound size={15} strokeWidth={2} className="ws-search-icon" />
                <input
                  id="hf-token-input"
                  className="ws-search-input"
                  type="password"
                  placeholder="Paste your access token"
                  aria-label="HuggingFace access token"
                  value={hfToken || ''}
                  onChange={(e) => {
                    const val = e.target.value;
                    setHfToken(val);
                    if (val.trim()) {
                      storeHfToken(val.trim());
                      setApiKey('huggingface', val.trim());
                      persistApiKey('huggingface', val.trim()).catch(console.error);
                    } else {
                      clearStoredHfToken();
                      setApiKey('huggingface', '');
                    }
                  }}
                />
              </div>
            </div>
          </div>

          <div className="models-below">

          {/* The embedding model, with the other models that are on this
              computer.

              It used to sit in a box of its own above the tabs, which put the
              one model nobody chooses — it ships with the app and is installed
              before the page is ever opened — in the most prominent position
              on a page about choosing models. It is not an announcement; it is
              an inventory item, and this is the inventory. */}
          {activeTab === 'installed' && <EmbeddingModelsSection />}

          {activeTab === 'downloads' ? (
            shownDownloads.length === 0 && interrupted.length === 0 ? (
              <div className="models-state">
                <Download size={30} strokeWidth={1.5} className="models-state__icon" />
                <span className="models-state__title">No downloads right now</span>
              </div>
            ) : (
              <ul key="downloads" className="models-downloads ws-mode-in" aria-label="Downloads">
                {shownDownloads.map(dl => {
                  const pct = Math.min(dl.percentage ?? 0, 100);
                  const running = dl.status === 'Downloading' || dl.status === 'Starting' || dl.status === 'Queued';
                  const statusText =
                    dl.status === 'Completed'
                      ? 'Downloaded'
                      : dl.status === 'Failed'
                        ? `Failed${dl.error_message ? `: ${dl.error_message.split('\n')[0]}` : ''}`
                        : dl.status === 'Paused'
                          ? 'Paused'
                          : dl.status === 'Queued' || dl.status === 'Starting'
                            ? 'Starting…'
                            : [
                                dl.speed_bps > 0 ? `${formatBytes(dl.speed_bps)}/s` : '',
                                formatEta(dl.estimated_time_remaining),
                              ].filter(Boolean).join(' · ');
                  return (
                    <li key={dl.download_id} className={`models-dl models-dl--${dl.status.toLowerCase()}`}>
                      <div className="models-dl__head">
                        <span className="models-dl__name">{dl.model_name}</span>
                        <span className="models-dl__pct">{pct.toFixed(0)}%</span>
                      </div>
                      <div
                        className={`ui-progress ui-progress--thin${running ? ' is-active' : ''}`}
                        style={{ '--progress': pct } as React.CSSProperties}
                        role="progressbar"
                        aria-label={`${dl.model_name} download`}
                        aria-valuenow={Math.round(pct)}
                        aria-valuemin={0}
                        aria-valuemax={100}
                      >
                        <div className="ui-progress__fill" />
                      </div>
                      <div className="models-dl__foot">
                        <span className="models-dl__meta">
                          {formatBytes(dl.bytes_downloaded ?? 0)}
                          {dl.total_bytes ? ` of ${formatBytes(Math.max(dl.total_bytes, dl.bytes_downloaded ?? 0))}` : ''}
                          {statusText ? ` · ${statusText}` : ''}
                        </span>
                        <span className="models-dl__actions">
                          {dl.status === 'Downloading' && (
                            <button type="button" className="models-dl__btn" onClick={() => handlePauseDownload(dl.download_id)}>
                              <Pause size={13} strokeWidth={0} fill="currentColor" /> Pause
                            </button>
                          )}
                          {dl.status === 'Paused' && (
                            <button type="button" className="models-dl__btn models-dl__btn--primary" onClick={() => handleResumeDownload(dl.download_id)}>
                              <Play size={13} strokeWidth={0} fill="currentColor" /> Resume
                            </button>
                          )}
                          {(running || dl.status === 'Paused') && (
                            <button type="button" className="models-dl__btn" onClick={() => handleCancelDownload(dl.download_id)}>
                              <Square size={11} strokeWidth={0} fill="currentColor" /> Cancel
                            </button>
                          )}
                        </span>
                      </div>
                    </li>
                  );
                })}
                {interrupted.map(d => {
                  const pct = d.size_bytes > 0 ? Math.min((d.bytes_on_disk / d.size_bytes) * 100, 100) : 0;
                  return (
                    <li key={`interrupted-${d.model_id}`} className="models-dl models-dl--paused">
                      <div className="models-dl__head">
                        <span className="models-dl__name">{d.model_name}</span>
                        <span className="models-dl__pct">{pct.toFixed(0)}%</span>
                      </div>
                      <div
                        className="ui-progress ui-progress--thin"
                        style={{ '--progress': pct } as React.CSSProperties}
                        role="progressbar"
                        aria-label={`${d.model_name} download`}
                        aria-valuenow={Math.round(pct)}
                        aria-valuemin={0}
                        aria-valuemax={100}
                      >
                        <div className="ui-progress__fill" />
                      </div>
                      <div className="models-dl__foot">
                        <span className="models-dl__meta">
                          {formatBytes(d.bytes_on_disk)}
                          {d.size_bytes ? ` of ${formatBytes(d.size_bytes)}` : ''} · Paused when the app closed
                        </span>
                        <span className="models-dl__actions">
                          <button type="button" className="models-dl__btn models-dl__btn--primary" onClick={() => void handleResumeInterrupted(d)}>
                            <Play size={13} strokeWidth={2} /> Resume
                          </button>
                          <button type="button" className="models-dl__btn" onClick={() => void handleDiscardInterrupted(d)}>
                            <Trash2 size={12} strokeWidth={2} /> Discard
                          </button>
                        </span>
                      </div>
                    </li>
                  );
                })}
              </ul>
            )
          ) : isLoading ? (
            <div className="models-state">Loading models…</div>
          ) : listed.length === 0 ? (
            <div className="models-state">
              <Download size={30} strokeWidth={1.5} className="models-state__icon" />
              <span className="models-state__title">
                {searchQuery
                  ? 'No models match your search'
                  : activeTab === 'installed'
                    ? 'No models on this computer yet'
                    : activeTab === 'starred'
                      ? 'No starred models yet. Use ⋮ › Add to starred on any model'
                      : 'No models available'}
              </span>
              {(activeTab === 'installed' || activeTab === 'starred') && !searchQuery && (
                <button
                  type="button"
                  className="ws-pill ws-create-folder vault-pill models-all-btn"
                  onClick={() => setActiveTab('available')}
                >
                  Browse all models
                </button>
              )}
            </div>
          ) : (
            <div key={activeTab} className="ws-file-list models-table ws-mode-in" role="table" aria-label="Models">
              <div className="ws-list-head" role="row">
                <span role="columnheader">Model</span>
                <span role="columnheader">Company</span>
                <span role="columnheader">Download size</span>
                <span role="columnheader">Modality</span>
                <span role="columnheader" title="How well it runs on this computer">Fit</span>
                <span role="columnheader" />
              </div>
              <div className="ws-list-body">
                {listed.map(model => {
                  const dl = getDownloadForModel(model.id, model.name);
                  const size = model.size_bytes + (model.mmproj_filename ? (model.mmproj_size_bytes ?? 0) : 0);
                  const fit = fitOnThisComputer(size, hardwareInfo, model.compatibility_score);
                  const maker = makerName(model);
                  const pct = Math.min(dl?.percentage ?? 0, 100).toFixed(0);
                  const status = dl
                    ? dl.status === 'Paused'
                      ? `Paused ${pct}%`
                      : dl.status === 'Queued'
                        ? 'Queued'
                        : `Downloading ${pct}%`
                    : isInstalled(model)
                      ? isActive(model) ? 'In use' : 'Installed'
                      : 'Available';
                  const statusTone = dl ? 'busy' : isActive(model) ? 'active' : isInstalled(model) ? 'installed' : 'available';

                  return (
                    <div
                      key={model.id}
                      role="row"
                      className={`ws-list-row models-row${dl ? ' models-row--downloading' : ''}`}
                      style={dl ? ({ '--progress': Math.min(dl.percentage ?? 0, 100) } as React.CSSProperties) : undefined}
                    >
                      <span className="ws-list-name" title={model.id}>
                        <span className="ws-list-title">{friendlyModelName(model)}</span>
                        {starred.has(model.id) && (
                          <Star size={13} strokeWidth={2} className="models-star" aria-label="Starred" />
                        )}
                        {statusTone !== 'available' && (
                          <span className={`models-name-status models-status-${statusTone}`}>{status}</span>
                        )}
                      </span>
                      <span className="ws-list-cell models-lab">
                        {maker ? (
                          <>
                            <LabLogo company={maker} />
                            <span className="models-lab__name">{maker}</span>
                          </>
                        ) : '-'}
                      </span>
                      <span className="ws-list-cell">{size > 0 ? formatBytes(size) : '-'}</span>
                      <span className="ws-list-cell">
                        <Modalities vision={Boolean(model.mmproj_filename)} />
                      </span>
                      <span className="ws-list-cell">
                        {fit ? (
                          <span className={`ws-chip models-chip-plain models-fit-${fit.tone}`}>
                            <span className="ws-chip-dot" aria-hidden="true" />
                            {fit.label}
                          </span>
                        ) : '-'}
                      </span>
                      <span className="ws-list-actions">
                        {dl?.status === 'Downloading' && (
                          <button
                            type="button"
                            className="models-row-dlbtn"
                            onClick={() => handlePauseDownload(dl.download_id)}
                            aria-label={`Pause downloading ${friendlyModelName(model)}`}
                            title="Pause"
                          >
                            <Pause size={14} strokeWidth={0} fill="currentColor" />
                          </button>
                        )}
                        {dl?.status === 'Paused' && (
                          <button
                            type="button"
                            className="models-row-dlbtn"
                            onClick={() => handleResumeDownload(dl.download_id)}
                            aria-label={`Resume downloading ${friendlyModelName(model)}`}
                            title="Resume"
                          >
                            <Play size={14} strokeWidth={0} fill="currentColor" />
                          </button>
                        )}
                        <ItemMenu label={friendlyModelName(model)} vertical entries={rowMenu(model)} />
                      </span>
                    </div>
                  );
                })}
              </div>
            </div>
          )}
          </div>
        </section>
      </div>

      {/* Download notification bubble (shown on other pages via App-level, but also here for context) */}
      {activeTab !== 'installed' && activeTab !== 'downloads' && activeDownloadCount > 0 && (
        (() => {
          const running = downloads.filter(d => d.status === 'Downloading');
          const first = running[0] ?? downloads[0];
          const pct = Math.min(first?.percentage ?? 0, 100);
          return (
            <div className="download-bubble" role="status">
              <span className="download-bubble-name">
                {activeDownloadCount > 1 ? `${activeDownloadCount} downloads` : first?.model_name}
              </span>
              <span className="download-bubble-pct">{pct.toFixed(0)}%</span>
              <button className="download-bubble-close" onClick={() => setActiveTab('downloads')}>
                View
              </button>
              <span
                className="download-bubble-line"
                style={{ '--progress': pct } as React.CSSProperties}
                role="progressbar"
                aria-valuenow={Math.round(pct)}
                aria-valuemin={0}
                aria-valuemax={100}
              />
            </div>
          );
        })()
      )}

      {/* Model Removal Confirmation Modal */}
      {showRemoveConfirmation && modelToRemove && (
        <div style={{
          position: 'fixed',
          inset: 0,
          backgroundColor: 'var(--bg-overlay)',
          display: 'flex',
          justifyContent: 'center',
          alignItems: 'center',
          zIndex: 'var(--z-modal)' as unknown as number,
        }}>
          <div style={{
            backgroundColor: 'var(--bg-surface)',
            border: '1px solid var(--border)',
            borderRadius: '6px',
            padding: 'var(--sp-6)',
            fontFamily: 'var(--font-sans)',
            maxWidth: '500px',
            width: '90%',
            boxShadow: 'var(--shadow-lg)',
          }}>
            <h3 style={{ margin: '0 0 var(--sp-4) 0', color: 'var(--text-heading)', fontSize: 'var(--fs-xl)' }}>Remove this model?</h3>
            <p style={{ margin: '0 0 16px 0', color: 'var(--text-secondary)', lineHeight: 1.5 }}>
              The model will be deleted from this computer. You can install it again later.
            </p>
            
            <div className="models-remove-detail">
              <span className="models-remove-detail__name">{friendlyModelName(modelToRemove)}</span>
              <span className="models-remove-detail__size">
                Frees about {formatBytes(modelToRemove.size_bytes)} on this computer
              </span>
            </div>
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '8px' }}>
              <button
                onClick={cancelRemoveModel}
                className="models-modal-btn"
              >
                Cancel
              </button>
              <button
                onClick={confirmRemoveModel}
                className="models-modal-btn models-modal-btn--primary"
              >
                Remove Model
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};

// ── Two dead card components used to stand here (~455 lines) ────────────────
//
//   ModelCard     ~280 lines -- a card per model, with its own install button,
//                 badge, progress bar and a call into the HuggingFace token
//                 dialog.
//   DownloadCard  ~130 lines -- a card per download, with pause/resume/cancel.
//
// Neither was imported or rendered anywhere. The live page renders models and
// downloads as table rows (`.models-table`), which is what the CSS in
// ModelsPanel.css is written for.
//
// Three module-scope helpers went with them, because the cards were their only
// callers: `downloadTone`, `statusBadgeClass`, and a module-scope `formatEta`.
// That last one is worth a note, because its own comment said it had been
// "extracted to module scope because BOTH download surfaces rendered it" --
// and yet the live surface never called it. It called a second, differently
// formatted `formatEta` declared locally inside the component ("5 min left"
// against the module version's "5m 3s"). The extraction had been defeated
// without anyone noticing, because the only callers it ever gained were the
// two components nothing rendered. The local one is the one users see, so it
// is the one that stayed.

export default ModelsPanel;
