// Models Panel Component
// Full-page model management view with source logos, inline download progress,
// pause/stop/resume controls, sort/filter, and download notification bubble.

import React, { useState, useEffect } from 'react';
import { Search, Download, Trash2, HardDrive, Cpu, Database, ArrowLeft, RefreshCw, Pause, Play, Square, ChevronDown } from 'lucide-react';
import { useNotificationHelpers } from '../contexts/NotificationContext';
import './ModelsPanel.css';
import { useAuth } from '../contexts/AuthContext';
import { open } from '@tauri-apps/plugin-shell';
import { getApiBaseSync } from '../api/backendUrl';
import { saveApiKey as persistApiKey, getApiKey } from '../api/apiKeys';
import { getHfToken as getStoredHfToken, setHfToken as storeHfToken, clearHfToken as clearStoredHfToken } from '../api/hfToken';

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

// Helper function to show the HuggingFace API Key modal with two-step flow
export function showHuggingFaceApiKeyModal(
  onHfTokenChange?: (token: string) => void,
  setApiKey?: (provider: 'huggingface', key: string) => void,
  onComplete?: () => void
) {
  // Create initial modal HTML
  const createInitialModalHTML = () => `
    <div id="huggingface-modal-model" style="
        position: fixed; 
        top: 0; 
        left: 0; 
        width: 100%; 
        height: 100%; 
        background: rgba(0,0,0,0.5); 
        display: flex; 
        justify-content: center; 
        align-items: center; 
        z-index: 10000;
        font-family: Arial, 'Helvetica Neue', Helvetica, sans-serif;
    ">
        <div id="huggingface-modal-content" style="
            background: white; 
            padding: 20px; 
            border-radius: 12px; 
            width: 560px; 
            max-width: 90vw; 
            box-shadow: 0 10px 25px rgba(0,0,0,0.2);
            color: black;
            position: relative;
        " data-theme="light">
            <button id="close-hf-modal" style="
                position: absolute;
                top: 8px;
                right: 8px;
                width: 30px;
                height: 30px;
                border-radius: 50%;
                background: #E5E7EB;
                color: #374151;
                border: none;
                cursor: pointer;
                font-size: 16px;
                display: flex;
                align-items: center;
                justify-content: center;
                font-weight: bold;
            ">×</button>
            <h3 style="color: black; margin-top: 0; margin-bottom: 15px; text-align: center;">HuggingFace Token Needed</h3>
            <p style="color: black; text-align: center;">Access gated models by adding your HuggingFace token.</p>
            <div style="display: flex; gap: 10px; margin-top: 20px; justify-content: center;">
                <button id="get-hf-token-btn-model" class="capsule-btn-gray" style="
                    width: 40%; 
                    padding: 10px 16px; 
                    background: rgb(233,233,233); 
                    color: black; 
                    border: none; 
                    border-radius: 9999px; 
                    cursor: pointer;
                    font-weight: 500;
                    transition: background-color 0.2s, color 0.2s;
                ">Create API Key</button>
                <button id="enter-hf-token-btn-model" class="capsule-btn-gray" style="
                    width: 40%; 
                    padding: 10px 16px; 
                    background: rgb(233,233,233); 
                    color: black; 
                    border: none; 
                    border-radius: 9999px; 
                    cursor: pointer;
                    font-weight: 500;
                    transition: background-color 0.2s, color 0.2s;
                ">Enter Existing Key</button>
            </div>
        </div>
    </div>
  `;

  // Create API key input modal HTML
  const createTokenInputHTML = () => `
    <div id="huggingface-modal-model" style="
        position: fixed; 
        top: 0; 
        left: 0; 
        width: 100%; 
        height: 100%; 
        background: rgba(0,0,0,0.5); 
        display: flex; 
        justify-content: center; 
        align-items: center; 
        z-index: 10000;
        font-family: Arial, 'Helvetica Neue', Helvetica, sans-serif;
    ">
        <div id="huggingface-modal-content" style="
            background: white; 
            padding: 20px; 
            border-radius: 12px; 
            width: 560px; 
            max-width: 90vw; 
            box-shadow: 0 10px 25px rgba(0,0,0,0.2);
            color: black;
            position: relative;
        " data-theme="light">
            <button id="close-hf-modal" style="
                position: absolute;
                top: 8px;
                right: 8px;
                width: 30px;
                height: 30px;
                border-radius: 50%;
                background: #E5E7EB;
                color: #374151;
                border: none;
                cursor: pointer;
                font-size: 16px;
                display: flex;
                align-items: center;
                justify-content: center;
                font-weight: bold;
            ">×</button>
            <button id="back-btn-hf-model" style="
                position: absolute;
                top: 8px;
                left: 8px;
                width: 30px;
                height: 30px;
                border-radius: 50%;
                background: #E5E7EB;
                color: #374151;
                border: none;
                cursor: pointer;
                font-size: 16px;
                display: flex;
                align-items: center;
                justify-content: center;
                font-weight: bold;
            ">←</button>
            <h3 style="color: black; margin-top: 0; margin-bottom: 15px; text-align: center;">Enter HuggingFace Token</h3>
            <p style="color: black; text-align: center; font-size: 13px; margin-bottom: 15px;">Paste your HuggingFace token below. You can get one at <a href="https://huggingface.co/settings/tokens" target="_blank" style="color: #2563eb; text-decoration: none;">huggingface.co/settings/tokens</a></p>
            <div style="margin-top: 20px;">
                <input 
                    id="hf-token-input-model" 
                    type="password" 
                    placeholder="hf_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" 
                    style="
                        width: 100%;
                        padding: 12px 16px;
                        border-radius: 8px;
                        border: 1px solid #d1d5db;
                        font-size: 14px;
                        box-sizing: border-box;
                        margin-bottom: 12px;
                    "
                />
                <button id="save-hf-token-btn-model" class="capsule-btn-black" style="
                    width: 100%; 
                    padding: 10px 16px; 
                    background: #000000; 
                    color: white; 
                    border: none; 
                    border-radius: 9999px; 
                    cursor: pointer;
                    font-weight: 500;
                    transition: background-color 0.2s;
                ">Save Token</button>
            </div>
        </div>
    </div>
  `;

  // Function to setup hover effects
  const setupHoverEffects = (modalElement: HTMLElement) => {
    const capsuleButtons = modalElement.querySelectorAll('.capsule-btn-gray, .capsule-btn-black');
    
    const handleMouseEnter = (e: Event) => {
      const target = e.target as HTMLElement;
      if (target.classList.contains('capsule-btn-gray')) {
        target.style.backgroundColor = 'var(--accent-hover)';
        target.style.color = 'var(--text-on-accent)';
      }
    };
    
    const handleMouseLeave = (e: Event) => {
      const target = e.target as HTMLElement;
      if (target.classList.contains('capsule-btn-gray')) {
        target.style.backgroundColor = 'var(--bg-surface)';
        target.style.color = 'var(--text-primary)';
      }
    };
    
    capsuleButtons.forEach(btn => {
      btn.addEventListener('mouseenter', handleMouseEnter);
      btn.addEventListener('mouseleave', handleMouseLeave);
    });
  };

  // Function to show initial modal
  const showInitialModal = () => {
    // Remove any existing modal
    const existingModal = document.getElementById('huggingface-modal-model');
    if (existingModal && existingModal.parentNode) {
      existingModal.parentNode.removeChild(existingModal);
    }

    // Add modal to DOM
    const tempDiv = document.createElement('div');
    tempDiv.innerHTML = createInitialModalHTML();
    const modalElement = tempDiv.firstElementChild as HTMLElement;
    document.body.appendChild(modalElement);

    setupHoverEffects(modalElement);

    // Handle Close button (X button)
    const closeBtn = document.getElementById('close-hf-modal') as HTMLButtonElement;
    closeBtn?.addEventListener('click', () => {
      if (modalElement.parentNode) {
        modalElement.parentNode.removeChild(modalElement);
      }
      document.removeEventListener('keydown', handleEscape);
    });

    // Handle "Create API Key" button - Open HuggingFace in browser
    const getTokenBtn = document.getElementById('get-hf-token-btn-model') as HTMLButtonElement;
    getTokenBtn?.addEventListener('click', async () => {
      // Open HuggingFace token page in default browser
      await open('https://huggingface.co/settings/tokens');
      // Close modal
      if (modalElement.parentNode) {
        modalElement.parentNode.removeChild(modalElement);
      }
      document.removeEventListener('keydown', handleEscape);
    });

    // Handle "Enter Existing Key" button - Show input form
    const enterTokenBtn = document.getElementById('enter-hf-token-btn-model') as HTMLButtonElement;
    enterTokenBtn?.addEventListener('click', () => {
      showTokenInputModal();
    });

    // Close modal on clicking outside
    modalElement.addEventListener('click', (e) => {
      if (e.target === modalElement) {
        if (modalElement.parentNode) {
          modalElement.parentNode.removeChild(modalElement);
        }
        document.removeEventListener('keydown', handleEscape);
      }
    });

    // Handle Escape key
    const handleEscape = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        if (modalElement.parentNode) {
          modalElement.parentNode.removeChild(modalElement);
        }
        document.removeEventListener('keydown', handleEscape);
      }
    };
    
    document.addEventListener('keydown', handleEscape);
  };

  // Function to show token input modal
  const showTokenInputModal = () => {
    // Remove existing modal
    const existingModal = document.getElementById('huggingface-modal-model');
    if (existingModal && existingModal.parentNode) {
      existingModal.parentNode.removeChild(existingModal);
    }

    // Add modal to DOM
    const tempDiv = document.createElement('div');
    tempDiv.innerHTML = createTokenInputHTML();
    const modalElement = tempDiv.firstElementChild as HTMLElement;
    document.body.appendChild(modalElement);

    // Focus the input field
    setTimeout(() => {
      const inputField = document.getElementById('hf-token-input-model') as HTMLInputElement;
      inputField?.focus();
    }, 100);

    // Handle Close button (X button)
    const closeBtn = document.getElementById('close-hf-modal') as HTMLButtonElement;
    closeBtn?.addEventListener('click', () => {
      if (modalElement.parentNode) {
        modalElement.parentNode.removeChild(modalElement);
      }
      document.removeEventListener('keydown', handleEscape);
    });

    // Handle Back button - Return to initial modal
    const backBtn = document.getElementById('back-btn-hf-model') as HTMLButtonElement;
    backBtn?.addEventListener('click', () => {
      if (modalElement.parentNode) {
        modalElement.parentNode.removeChild(modalElement);
      }
      document.removeEventListener('keydown', handleEscape);
      showInitialModal();
    });

    // Handle Save button - Save the token
    const saveBtn = document.getElementById('save-hf-token-btn-model') as HTMLButtonElement;
    const inputField = document.getElementById('hf-token-input-model') as HTMLInputElement;

    const saveToken = () => {
      const token = inputField?.value.trim();
      if (token) {
        // Save to browser storage (single canonical key)
        storeHfToken(token);

        // Persist to encrypted backend DB (fire-and-forget)
        persistApiKey('huggingface', token).catch(console.error);

        // Update AuthContext
        if (setApiKey) {
          setApiKey('huggingface', token);
        }

        // Update parent component state
        if (onHfTokenChange) {
          onHfTokenChange(token);
        }

        // Close modal
        if (modalElement.parentNode) {
          modalElement.parentNode.removeChild(modalElement);
        }
        document.removeEventListener('keydown', handleEscape);

        // Call onComplete callback if provided
        if (onComplete) {
          onComplete();
        }
      } else {
        // Show error or shake animation
        if (inputField) {
          inputField.style.borderColor = 'var(--danger)';
          setTimeout(() => {
            inputField.style.borderColor = 'var(--border)';
          }, 1000);
        }
      }
    };

    saveBtn?.addEventListener('click', saveToken);
    
    // Handle Enter key in input field
    inputField?.addEventListener('keypress', (e: KeyboardEvent) => {
      if (e.key === 'Enter') {
        saveToken();
      }
    });

    // Close modal on clicking outside
    modalElement.addEventListener('click', (e) => {
      if (e.target === modalElement) {
        if (modalElement.parentNode) {
          modalElement.parentNode.removeChild(modalElement);
        }
        document.removeEventListener('keydown', handleEscape);
      }
    });

    // Handle Escape key
    const handleEscape = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        if (modalElement.parentNode) {
          modalElement.parentNode.removeChild(modalElement);
        }
        document.removeEventListener('keydown', handleEscape);
      }
    };
    
    document.addEventListener('keydown', handleEscape);
  };

  // Show the initial modal
  showInitialModal();
}

// Helper function to show the Gated Model Access modal
export function showGatedModelModal(
  repoId: string,
  onRetry?: () => void
) {
  // Create modal HTML
  const modalHtml = `
    <div id="gated-model-modal" style="
        position: fixed; 
        top: 0; 
        left: 0; 
        width: 100%; 
        height: 100%; 
        background: rgba(0,0,0,0.5); 
        display: flex; 
        justify-content: center; 
        align-items: center; 
        z-index: 10000;
        font-family: Arial, 'Helvetica Neue', Helvetica, sans-serif;
    ">
        <div id="gated-modal-content" style="
            background: white; 
            padding: 24px; 
            border-radius: 12px; 
            width: 520px; 
            max-width: 90vw; 
            box-shadow: 0 10px 25px rgba(0,0,0,0.2);
            color: black;
            position: relative;
        " data-theme="light">
            <button id="close-gated-modal" style="
                position: absolute;
                top: 8px;
                right: 8px;
                width: 30px;
                height: 30px;
                border-radius: 50%;
                background: #E5E7EB;
                color: #374151;
                border: none;
                cursor: pointer;
                font-size: 16px;
                display: flex;
                align-items: center;
                justify-content: center;
                font-weight: bold;
            ">×</button>
            <h3 style="color: black; margin-top: 0; margin-bottom: 15px; text-align: center; font-size: 18px;">Model Access Required</h3>
            <p style="color: #374151; text-align: center; font-size: 14px; line-height: 1.5; margin-bottom: 20px;">
                This model requires explicit access approval from HuggingFace.
            </p>
            <div style="background: #FEF3C7; border-left: 4px solid #F59E0B; padding: 12px; margin-bottom: 20px; border-radius: 4px;">
                <p style="color: #92400E; font-size: 13px; margin: 0;">
                    <strong>Repository:</strong> ${repoId}<br/>
                    You need to visit the model page and click "Request Access" to download this model.
                </p>
            </div>
            <div style="display: flex; gap: 10px; justify-content: center;">
                <button id="visit-hf-page-btn" class="capsule-btn-gray" style="
                    flex: 1;
                    padding: 12px 20px; 
                    background: #2563EB; 
                    color: white; 
                    border: none; 
                    border-radius: 9999px; 
                    cursor: pointer;
                    font-weight: 500;
                    font-size: 14px;
                    transition: background-color 0.2s, color 0.2s;
                ">Visit Model Page</button>
                <button id="retry-download-btn" class="capsule-btn-gray" style="
                    flex: 1;
                    padding: 12px 20px; 
                    background: rgb(233,233,233); 
                    color: black; 
                    border: none; 
                    border-radius: 9999px; 
                    cursor: pointer;
                    font-weight: 500;
                    font-size: 14px;
                    transition: background-color 0.2s, color 0.2s;
                ">Retry Download</button>
            </div>
            <p style="color: #6B7280; text-align: center; font-size: 12px; margin-top: 16px; margin-bottom: 0;">
                After requesting access on HuggingFace, click "Retry Download" to try again.
            </p>
        </div>
    </div>
  `;

  // Add modal to DOM
  const tempDiv = document.createElement('div');
  tempDiv.innerHTML = modalHtml;
  const modalElement = tempDiv.firstElementChild as HTMLElement;
  document.body.appendChild(modalElement);

  // Setup hover effects
  const capsuleButtons = modalElement.querySelectorAll('.capsule-btn-gray');
  
  const handleMouseEnter = (e: Event) => {
    const target = e.target as HTMLElement;
    if (target.id === 'visit-hf-page-btn') {
      target.style.backgroundColor = 'var(--accent-hover)';
    } else if (target.id === 'retry-download-btn') {
      target.style.backgroundColor = 'var(--accent-hover)';
      target.style.color = 'var(--text-on-accent)';
    }
  };
  
  const handleMouseLeave = (e: Event) => {
    const target = e.target as HTMLElement;
    if (target.id === 'visit-hf-page-btn') {
      target.style.backgroundColor = 'var(--accent)';
    } else if (target.id === 'retry-download-btn') {
      target.style.backgroundColor = 'var(--bg-surface)';
      target.style.color = 'var(--text-primary)';
    }
  };
  
  capsuleButtons.forEach(btn => {
    btn.addEventListener('mouseenter', handleMouseEnter);
    btn.addEventListener('mouseleave', handleMouseLeave);
  });

  // Handle "Visit Model Page" button
  const visitBtn = document.getElementById('visit-hf-page-btn') as HTMLButtonElement;
  visitBtn?.addEventListener('click', async () => {
    await open(`https://huggingface.co/${repoId}`);
  });

  // Handle "Retry Download" button
  const retryBtn = document.getElementById('retry-download-btn') as HTMLButtonElement;
  retryBtn?.addEventListener('click', () => {
    if (modalElement.parentNode) {
      modalElement.parentNode.removeChild(modalElement);
    }
    document.removeEventListener('keydown', handleEscape);
    if (onRetry) {
      onRetry();
    }
  });

  // Handle Close button
  const closeBtn = document.getElementById('close-gated-modal') as HTMLButtonElement;
  closeBtn?.addEventListener('click', () => {
    if (modalElement.parentNode) {
      modalElement.parentNode.removeChild(modalElement);
    }
    document.removeEventListener('keydown', handleEscape);
  });

  // Close modal on clicking outside
  modalElement.addEventListener('click', (e) => {
    if (e.target === modalElement) {
      if (modalElement.parentNode) {
        modalElement.parentNode.removeChild(modalElement);
      }
      document.removeEventListener('keydown', handleEscape);
    }
  });

  // Handle Escape key
  const handleEscape = (e: KeyboardEvent) => {
    if (e.key === 'Escape') {
      if (modalElement.parentNode) {
        modalElement.parentNode.removeChild(modalElement);
      }
      document.removeEventListener('keydown', handleEscape);
    }
  };
  
  document.addEventListener('keydown', handleEscape);
}

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

type SortOption = 'name' | 'size_asc' | 'size_desc' | 'compatibility' | 'source' | 'trending' | 'popularity';





interface ActiveModelInfo {
  model_path: string;
  model_name: string;
}

interface SelectedModel {
  id: string;
  name: string;
  source: 'local';
}

const ModelsPanel: React.FC<{
  isOpen: boolean;
  onClose: () => void;
  selectedModel?: SelectedModel | null;
  onSelectModel?: (model: SelectedModel | null) => void;
  focusHfTokenInput?: boolean;
}> = ({ isOpen, onClose, selectedModel, onSelectModel, focusHfTokenInput }) => {
  const [models, setModels] = useState<Model[]>([]);
  const [downloads, setDownloads] = useState<DownloadProgress[]>([]);
  const [searchQuery, setSearchQuery] = useState('');
  const [isLoading, setIsLoading] = useState(false);
  const [fetchError, setFetchError] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<'installed' | 'available' | 'downloads'>('available');
  const [hardwareInfo, setHardwareInfo] = useState<HardwareInfo | null>(null);
  const [activeModelInfo, setActiveModelInfo] = useState<ActiveModelInfo | null>(null);
  const [sortBy, setSortBy] = useState<SortOption>('name');
  const [showSortDropdown, setShowSortDropdown] = useState(false);
  const [showHfTokenInput, setShowHfTokenInput] = useState(false);
  const { showSuccess, showError, showDownload } = useNotificationHelpers();
  const { user, setApiKey } = useAuth();
  
  const [hfToken, setHfToken] = useState<string>(() => {
    // Get initial value from browser storage first
    const storedToken = getStoredHfToken();
    if (storedToken) return storedToken;
    // Get from auth context if available
    return user?.apiKeys?.huggingface || '';
  });
  
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
        fetchModels();
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

  useEffect(() => {
    if (isOpen && downloads.some(d => d.status === 'Downloading' || d.status === 'Starting' || d.status === 'Queued')) {
      const interval = setInterval(fetchDownloads, 1000); // Poll every 1 second for active downloads
      return () => clearInterval(interval);
    }
    // If no active downloads, return empty cleanup function
    return () => {};
  }, [isOpen, downloads]);

  // Detect completed downloads and refresh model list
  useEffect(() => {
    const completedDownloads = downloads.filter(d => d.status === 'Completed');

    if (completedDownloads.length > 0) {
      // Refresh the model list to pick up the new "Installed" status from backend
      fetchModels();

      // Clean up completed downloads after a short delay to show success state
      const timer = setTimeout(() => {
        setDownloads(prev => prev.filter(d => d.status !== 'Completed'));
      }, 2000); // 2 second delay to let user see 100% completion

      return () => clearTimeout(timer);
    }
  }, [downloads]);

  const fetchModels = async () => {
    try {
      setIsLoading(true);
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
        setModels(data);
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
      
      const response = await fetch(`${getApiBaseSync()}/models/downloads`);
      if (response.ok) {
        setDownloads(await response.json());
      } else {
        console.warn('Downloads endpoint returned non-200 status:', response.status);
      }
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
        showError('Backend Not Ready', `Please ensure the offline-intelligence service is started on ${getApiBaseSync()}.`);
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

  const buildSourcePayload = (model: Model) => {
    const source = model.download_source || 'huggingface';
    if (source === 'ollama') {
      const modelName = model.id.startsWith('ollama:') ? model.id.slice(7) : model.id;
      return { type: 'Ollama', model_name: modelName };
    } else {
      // Use filename from model info if available, otherwise construct a guess
      const filename = model.filename || (() => {
        const parts = model.id.split('/');
        return parts.length > 1
          ? `${parts[parts.length - 1].toLowerCase().replace(/-gguf$/i, '')}.Q4_K_M.gguf`
          : `${model.id}.gguf`;
      })();
      return { type: 'HuggingFace', repo_id: model.id, filename };
    }
  };

  const handleInstallModel = async (model: Model) => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError('Backend Not Ready', `Please ensure the offline-intelligence service is started on ${getApiBaseSync()}.`);
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
        showDownload('Download Started', `Downloading ${model.name}`);
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
            // Create a custom modal for HuggingFace token
          const modalHtml = `
            <div id="huggingface-modal-model" style="
                position: fixed; 
                top: 0; 
                left: 0; 
                width: 100%; 
                height: 100%; 
                background: rgba(0,0,0,0.5); 
                display: flex; 
                justify-content: center; 
                align-items: center; 
                z-index: 10000;
                font-family: Arial, 'Helvetica Neue', Helvetica, sans-serif;
            ">
                <div style="
                    background: white; 
                    padding: 20px; 
                    border-radius: 12px; 
                    width: 560px; 
                    max-width: 90vw; 
                    box-shadow: 0 10px 25px rgba(0,0,0,0.2);
                    color: black;
                    position: relative;
                " data-theme="light">
                    <button id="close-hf-modal" style="
                        position: absolute;
                        top: 8px;
                        right: 8px;
                        width: 30px;
                        height: 30px;
                        border-radius: 50%;
                        background: #E5E7EB;
                        color: #374151;
                        border: none;
                        cursor: pointer;
                        font-size: 16px;
                        display: flex;
                        align-items: center;
                        justify-content: center;
                        font-weight: bold;
                    ">×</button>
                    <h3 style="color: black; margin-top: 0; margin-bottom: 15px; text-align: center;">HuggingFace Token Needed</h3>
                    <p style="color: black; text-align: center;">Access gated models by adding your HuggingFace token.</p>
                    <div style="display: flex; gap: 10px; margin-top: 20px; justify-content: center;">
                        <button id="get-hf-token-btn-model" class="capsule-btn-gray" style="
                            width: 40%; 
                            padding: 10px 16px; 
                            background: rgb(233,233,233); 
                            color: black; 
                            border: none; 
                            border-radius: 9999px; 
                            cursor: pointer;
                            font-weight: 500;
                            transition: background-color 0.2s, color 0.2s;
                        ">Create API Key</button>
                        <button id="enter-hf-token-btn-model" class="capsule-btn-gray" style="
                            width: 40%; 
                            padding: 10px 16px; 
                            background: rgb(233,233,233); 
                            color: black; 
                            border: none; 
                            border-radius: 9999px; 
                            cursor: pointer;
                            font-weight: 500;
                            transition: background-color 0.2s, color 0.2s;
                        ">Enter Existing Key</button>
                    </div>
                </div>
            </div>
          `;
          
          // Add modal to DOM
          const tempDiv = document.createElement('div');
          tempDiv.innerHTML = modalHtml;
          const modalElement = tempDiv.firstElementChild as HTMLElement;
          document.body.appendChild(modalElement);
          
          // When removing modal, also clean up event listeners
          const removeModalHF = () => {
              cleanupModalHF();
              document.body.removeChild(modalElement);
          };
          
          // Clean up event listeners when modal is closed
          const cleanupModalHF = () => {
              document.removeEventListener('keydown', handleEscape);
          };
          
          // Close modal on pressing Escape key
          const handleEscape = (e: KeyboardEvent) => {
              if (e.key === 'Escape') {
                  removeModalHF();
              }
          };
          
          document.addEventListener('keydown', handleEscape);
          
          // Get buttons from the modal
          const getHfTokenBtn = document.getElementById('get-hf-token-btn-model') as HTMLButtonElement;
          const enterHfTokenBtn = document.getElementById('enter-hf-token-btn-model') as HTMLButtonElement;
          const closeHfModalBtn = document.getElementById('close-hf-modal') as HTMLButtonElement;
          
          // Handle "Get Token" button
          const handleGetHfToken = async () => {
              // Open HuggingFace token page in default browser
              await open('https://huggingface.co/settings/tokens');
              // Remove modal
              removeModalHF();
          };
          
          // Handle Close button (X button)
          closeHfModalBtn?.addEventListener('click', () => {
              document.body.removeChild(modalElement);
          });
          
          // Handle "Enter Token" button
          const handleEnterHfToken = () => {
              // Close any existing modals first
              const existingModals = document.querySelectorAll('[id$="-modal"]');
              existingModals.forEach(modal => {
                  // Check if it's an element before removing to avoid errors
                  if (modal.parentNode) {
                      modal.parentNode.removeChild(modal);
                  }
              });
              
              // Create a modal with input field instead of using prompt
              const tokenModalHtml = `
                <div id="hf-token-input-modal" style="
                  position: fixed; 
                  top: 0; 
                  left: 0; 
                  width: 100%; 
                  height: 100%; 
                  background: rgba(0,0,0,0.5); 
                  display: flex; 
                  justify-content: center; 
                  align-items: center; 
                  z-index: 10002;
                  font-family: Arial, 'Helvetica Neue', Helvetica, sans-serif;
                ">
                  <div style="
                    background: white; 
                    padding: 20px; 
                    border-radius: 12px; 
                    width: 400px; 
                    max-width: 90vw; 
                    box-shadow: 0 10px 25px rgba(0,0,0,0.2);
                    color: black;
                    text-align: center;
                    position: relative;
                  ">
                    <button id="close-hf-token-modal" style="
                      position: absolute;
                      top: 8px;
                      right: 8px;
                      width: 30px;
                      height: 30px;
                      border-radius: 50%;
                      background: #E5E7EB;
                      color: #374151;
                      border: none;
                      cursor: pointer;
                      font-size: 16px;
                      display: flex;
                      align-items: center;
                      justify-content: center;
                      font-weight: bold;
                    ">×</button>
                    <h3 style="color: black; margin-top: 0; margin-bottom: 15px; text-align: center; font-size: 16px; font-weight: 600;">Enter HuggingFace Token</h3>
                    <input 
                      type="password" 
                      id="hf-token-input" 
                      placeholder="hf_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
                      style="
                        width: 100%;
                        padding: 10px;
                        margin: 10px 0;
                        border: 1px solid #ccc;
                        border-radius: 8px;
                        font-size: 14px;
                        outline: none;
                      "
                    />
                    <div style="display: flex; gap: 8px; margin-top: 15px; justify-content: center;">
                      <button id="save-hf-token-btn" class="capsule-btn-gray" style="
                        flex: 1;
                        padding: 8px 16px;
                        background: #1e40af;
                        color: white;
                        border: none;
                        border-radius: 9999px;
                        cursor: pointer;
                        font-weight: 500;
                        font-size: 14px;
                        transition: background-color 0.2s, color 0.2s;
                      ">Save Token</button>
                      <button id="cancel-hf-token-btn" class="capsule-btn-gray" style="
                        flex: 1;
                        padding: 8px 16px;
                        background: #E5E7EB;
                        color: #1e40af;
                        border: none;
                        border-radius: 9999px;
                        cursor: pointer;
                        font-weight: 500;
                        font-size: 14px;
                        transition: background-color 0.2s, color 0.2s;
                      ">Cancel</button>
                    </div>
                  </div>
                </div>
              `;
              
              // Add modal to DOM
              const tempDiv = document.createElement('div');
              tempDiv.innerHTML = tokenModalHtml;
              const tokenModalElement = tempDiv.firstElementChild as HTMLElement;
              document.body.appendChild(tokenModalElement);
              
              const inputElement = document.getElementById('hf-token-input') as HTMLInputElement;
              const saveBtn = document.getElementById('save-hf-token-btn') as HTMLButtonElement;
              const cancelBtn = document.getElementById('cancel-hf-token-btn') as HTMLButtonElement;
              const closeBtn = document.getElementById('close-hf-token-modal') as HTMLButtonElement;
              
              // Add hover event listeners for capsule buttons
              const capsuleButtons = tokenModalElement.querySelectorAll('.capsule-btn-gray');
              
              const handleMouseEnter = (e: { target: any }) => {
                  (e.target as HTMLElement).style.backgroundColor = 'var(--accent-hover)';
                  (e.target as HTMLElement).style.color = 'var(--text-on-accent)';
              };
              
              const handleMouseLeave = (e: { target: any }) => {
                  const target = e.target as HTMLElement;
                  if (target.id === 'save-hf-token-btn') {
                      target.style.backgroundColor = 'var(--accent)';
                  } else if (target.id === 'cancel-hf-token-btn') {
                      target.style.backgroundColor = 'var(--bg-hover)';
                      target.style.color = 'var(--text-primary)';
                  }
              };
              
              capsuleButtons.forEach(btn => {
                  btn.addEventListener('mouseenter', handleMouseEnter);
                  btn.addEventListener('mouseleave', handleMouseLeave);
              });
              
              // Handle Save button
              saveBtn?.addEventListener('click', () => {
                  const newToken = inputElement?.value;
                  if (newToken && newToken.trim()) {
                      // Store the token (single canonical browser-storage key)
                      storeHfToken(newToken.trim());
                      alert('HuggingFace token saved successfully! Please restart the app for changes to take effect.');
                      // Remove modal
                      document.body.removeChild(tokenModalElement);
                      // Remove the original modal
                      removeModalHF();
                  }
              });
              
              // Handle Cancel button
              cancelBtn?.addEventListener('click', () => {
                  document.body.removeChild(tokenModalElement);
              });
              
              // Handle Close button (X button)
              closeBtn?.addEventListener('click', () => {
                  document.body.removeChild(tokenModalElement);
              });
              
              // Close modal on clicking outside
              tokenModalElement.addEventListener('click', (e) => {
                  if (e.target === tokenModalElement) {
                      document.body.removeChild(tokenModalElement);
                  }
              });
              
              // Close modal on pressing Escape key
              const handleEscape = (e: KeyboardEvent) => {
                  if (e.key === 'Escape') {
                      document.body.removeChild(tokenModalElement);
                  }
              };
              
              document.addEventListener('keydown', handleEscape);
              
              // Focus the input field
              setTimeout(() => {
                  inputElement?.focus();
              }, 100);
          };
          
          // Attach event listener
          getHfTokenBtn?.addEventListener('click', handleGetHfToken);
          
          // Attach event listener
          enterHfTokenBtn?.addEventListener('click', handleEnterHfToken);
          
          // Add hover event listeners for capsule buttons
          const capsuleButtons = modalElement.querySelectorAll('.capsule-btn-gray');
          
          const handleMouseEnter = (e: { target: any }) => {
              (e.target as HTMLElement).style.backgroundColor = 'var(--accent-hover)';
              (e.target as HTMLElement).style.color = 'var(--text-on-accent)';
          };
          
          const handleMouseLeave = (e: { target: any }) => {
              (e.target as HTMLElement).style.backgroundColor = 'var(--bg-surface)';
              (e.target as HTMLElement).style.color = 'var(--text-primary)';
          };
          
          capsuleButtons.forEach(btn => {
              btn.addEventListener('mouseenter', handleMouseEnter);
              btn.addEventListener('mouseleave', handleMouseLeave);
          });
          
          // Close modal on clicking outside
          modalElement.addEventListener('click', (e) => {
              if (e.target === modalElement) {
                  removeModalHF();
              }
          });
          }
        } else if (errorText.includes('401 Unauthorized') && model.download_source === 'ollama') {
          // Create a custom modal for Ollama
          const modalHtml = `
            <div id="ollama-modal-model" style="
                position: fixed; 
                top: 0; 
                left: 0; 
                width: 100%; 
                height: 100%; 
                background: rgba(0,0,0,0.5); 
                display: flex; 
                justify-content: center; 
                align-items: center; 
                z-index: 10000;
                font-family: Arial, 'Helvetica Neue', Helvetica, sans-serif;
            ">
                <div style="
                    background: white; 
                    padding: 20px; 
                    border-radius: 12px; 
                    width: 560px; 
                    max-width: 90vw; 
                    box-shadow: 0 10px 25px rgba(0,0,0,0.2);
                    color: black;
                    position: relative;
                " data-theme="light">
                    <button id="close-ollama-modal" style="
                        position: absolute;
                        top: 8px;
                        right: 8px;
                        width: 30px;
                        height: 30px;
                        border-radius: 50%;
                        background: #E5E7EB;
                        color: #374151;
                        border: none;
                        cursor: pointer;
                        font-size: 16px;
                        display: flex;
                        align-items: center;
                        justify-content: center;
                        font-weight: bold;
                    ">×</button>
                    <h3 style="color: black; margin-top: 0; margin-bottom: 15px; text-align: center;">Ollama Server Required</h3>
                    <p style="color: black; text-align: center;">Connect to your Ollama server to access models.</p>
                    <div style="display: flex; gap: 10px; margin-top: 20px; justify-content: center;">
                        <button id="setup-ollama-btn-model" class="capsule-btn-gray" style="
                            width: 40%; 
                            padding: 10px 16px; 
                            background: rgb(233,233,233); 
                            color: black; 
                            border: none; 
                            border-radius: 9999px; 
                            cursor: pointer;
                            font-weight: 500;
                            transition: background-color 0.2s, color 0.2s;
                        ">Setup Guide</button>
                        <button id="connect-ollama-btn-model" class="capsule-btn-gray" style="
                            width: 40%; 
                            padding: 10px 16px; 
                            background: rgb(233,233,233); 
                            color: black; 
                            border: none; 
                            border-radius: 9999px; 
                            cursor: pointer;
                            font-weight: 500;
                            transition: background-color 0.2s, color 0.2s;
                        ">Connect</button>
                    </div>
                </div>
            </div>
          `;
          
          // Add modal to DOM
          const tempDiv = document.createElement('div');
          tempDiv.innerHTML = modalHtml;
          const modalElement = tempDiv.firstElementChild as HTMLElement;
          document.body.appendChild(modalElement);
          
          // When removing modal, also clean up event listeners
          const removeModalOllama = () => {
              cleanupModalOllama();
              document.body.removeChild(modalElement);
          };
          
          // Clean up event listeners when modal is closed
          const cleanupModalOllama = () => {
              document.removeEventListener('keydown', handleEscape);
          };
          
          // Close modal on pressing Escape key
          const handleEscape = (e: KeyboardEvent) => {
              if (e.key === 'Escape') {
                  removeModalOllama();
              }
          };
          
          document.addEventListener('keydown', handleEscape);
          
          // Get buttons from the modal
          const setupOllamaBtn = document.getElementById('setup-ollama-btn-model') as HTMLButtonElement;
          const connectOllamaBtn = document.getElementById('connect-ollama-btn-model') as HTMLButtonElement;
          const closeOllamaModalBtn = document.getElementById('close-ollama-modal') as HTMLButtonElement;
          
          // Handle "Setup Guide" button
          const handleSetupOllama = async () => {
              // Open Ollama setup page in default browser
              await open('https://ollama.com/download');
              // Remove modal
              removeModalOllama();
          };
          
          // Handle Close button (X button)
          closeOllamaModalBtn?.addEventListener('click', () => {
              document.body.removeChild(modalElement);
          });
          
          // Handle "Connect" button
          const handleConnectOllama = () => {
              // Prompt for the Ollama server URL
              const ollamaUrl = prompt('Please enter your Ollama server URL (e.g., http://localhost:11434):', 'http://localhost:11434');
              if (ollamaUrl && ollamaUrl.trim()) {
                  // Store the Ollama URL in localStorage
                  localStorage.setItem('offline-intelligence-ollama-url', ollamaUrl.trim());
                  alert('Ollama server URL saved successfully! Please restart the app for changes to take effect.');
                  // Remove modal
                  removeModalOllama();
              }
          };
          
          // Attach event listener
          setupOllamaBtn?.addEventListener('click', handleSetupOllama);
          
          // Attach event listener
          connectOllamaBtn?.addEventListener('click', handleConnectOllama);
          
          // Add hover event listeners for capsule buttons
          const capsuleButtons = modalElement.querySelectorAll('.capsule-btn-gray');
          
          const handleMouseEnter = (e: { target: any }) => {
              (e.target as HTMLElement).style.backgroundColor = 'var(--accent-hover)';
              (e.target as HTMLElement).style.color = 'var(--text-on-accent)';
          };
          
          const handleMouseLeave = (e: { target: any }) => {
              (e.target as HTMLElement).style.backgroundColor = 'var(--bg-surface)';
              (e.target as HTMLElement).style.color = 'var(--text-primary)';
          };
          
          capsuleButtons.forEach(btn => {
              btn.addEventListener('mouseenter', handleMouseEnter);
              btn.addEventListener('mouseleave', handleMouseLeave);
          });
          
          // Close modal on clicking outside
          modalElement.addEventListener('click', (e) => {
              if (e.target === modalElement) {
                  removeModalOllama();
              }
          });
        } else {
          showError('Installation Failed', `Failed to start download for ${model.name}`);
        }
      }
    } catch (error) {
      console.error('Failed to install model:', error);
      showError('Installation Error', `Failed to install ${model.name}`);
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
        showError('Backend Not Ready', `Please ensure the offline-intelligence service is started on ${getApiBaseSync()}.`);
        return;
      }
      
      const response = await fetch(`${getApiBaseSync()}/models/remove?model_id=${modelToRemove.id}`, { method: 'DELETE' });
      if (response.ok) {
        showSuccess('Model Removed', 'Model successfully removed');
        fetchModels();
      } else {
        showError('Removal Failed', 'Failed to remove model');
      }
    } catch (error) {
      console.error('Failed to remove model:', error);
      showError('Removal Error', 'Failed to remove model');
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
        showError('Backend Not Ready', `Please ensure the offline-intelligence service is started on ${getApiBaseSync()}.`);
        return;
      }
      
      const response = await fetch(`${getApiBaseSync()}/models/switch`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model_id: modelId }),
      });

      if (response.ok) {
        showSuccess('Model Switched', `Successfully switched to ${modelName}`);
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
            'Inference Engine Problem',
            errorData.detail || 'The inference engine is not available on this machine.'
          );
          // Ask the engine setup gate to re-check /healthz and take over.
          window.dispatchEvent(new CustomEvent('oca-recheck-engine'));
        } else {
          showError('Switch Failed', errorData.detail || errorData.error || `Failed to switch to ${modelName}`);
        }
      }
    } catch (error) {
      console.error('Failed to switch model:', error);
      showError('Switch Error', `Failed to switch to ${modelName}`);
    }
  };

  const handlePauseDownload = async (downloadId: string) => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError('Backend Not Ready', `Please ensure the offline-intelligence service is started on ${getApiBaseSync()}.`);
        return;
      }
      
      await fetch(`${getApiBaseSync()}/models/downloads/pause?download_id=${downloadId}`, { method: 'POST' });
      fetchDownloads();
    } catch (e) { console.error('Pause failed:', e); }
  };

  const handleResumeDownload = async (downloadId: string) => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError('Backend Not Ready', `Please ensure the offline-intelligence service is started on ${getApiBaseSync()}.`);
        return;
      }
      
      await fetch(`${getApiBaseSync()}/models/downloads/resume?download_id=${downloadId}`, { method: 'POST' });
      fetchDownloads();
    } catch (e) { console.error('Resume failed:', e); }
  };

  const handleCancelDownload = async (downloadId: string) => {
    try {
      // Check backend readiness before making request
      if (!(await checkBackendReadiness())) {
        showError('Backend Not Ready', `Please ensure the offline-intelligence service is started on ${getApiBaseSync()}.`);
        return;
      }
      
      await fetch(`${getApiBaseSync()}/models/downloads/cancel?download_id=${downloadId}`, { method: 'POST' });
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


  const isAvailable = (model: Model) => model.status === 'Available' || (typeof model.status === 'object');

  // Find download progress for a model
  const getDownloadForModel = (modelId: string) =>
    downloads.find(d => d.model_id === modelId && (d.status === 'Downloading' || d.status === 'Starting' || d.status === 'Paused' || d.status === 'Queued'));

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
      // First sort by company priority
      const priorityA = getCompanyPriority(a);
      const priorityB = getCompanyPriority(b);
      if (priorityA !== priorityB) {
        return priorityA - priorityB;
      }
      
      // Then apply selected sort
      switch (sortBy) {
        case 'name': return a.name.localeCompare(b.name);
        case 'size_asc': return a.size_bytes - b.size_bytes;
        case 'size_desc': return b.size_bytes - a.size_bytes;
        case 'compatibility':
          return (b.compatibility_score ?? 0) - (a.compatibility_score ?? 0);
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
    const searchLower = searchQuery.toLowerCase();
    const matchesSearch = searchQuery === '' ||
      model.name.toLowerCase().includes(searchLower) ||
      model.id.toLowerCase().includes(searchLower) ||
      model.description?.toLowerCase().includes(searchLower) ||
      model.provider?.toLowerCase().includes(searchLower) ||
      model.author?.toLowerCase().includes(searchLower) ||
      model.tags.some(tag => tag.toLowerCase().includes(searchLower));
    if (activeTab === 'installed') return matchesSearch && isInstalled(model);
    if (activeTab === 'available') return matchesSearch && isAvailable(model);
    return matchesSearch;
  }));

  if (!isOpen) return null;

  const activeDownloadCount = downloads.filter(d => d.status === 'Downloading' || d.status === 'Starting').length;

  const sortOptions: { value: SortOption; label: string }[] = [
    { value: 'popularity', label: 'Trending' },
    { value: 'name', label: 'Name (A-Z)' },
    { value: 'size_desc', label: 'Largest Models' },
    { value: 'size_asc', label: 'Smallest Models' },
    { value: 'compatibility', label: 'Best Match' },
    { value: 'source', label: 'Source' },
  ];

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100vh', width: '100%', backgroundColor: 'var(--bg-canvas)' }}>
      {/* Header */}
      <div className="chat-header">
        <div className="chat-header-bar centered">
          <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
            <button className="header-icon-button" onClick={onClose} title="Back to chat">
              <ArrowLeft size={18} />
            </button>
            <h1 className="chat-title" style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
              <svg width="20" height="20" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M20 7l-8-4-8 4m16 0l-8 4m8-4v10l-8 4m0-10L4 7m8 4v10M4 7v10l8 4" />
              </svg>
              Model Management
            </h1>
          </div>
        </div>
      </div>

      {/* Hardware Info Bar */}
      {hardwareInfo && (
        <div style={{
          display: 'flex', justifyContent: 'center', gap: '24px', padding: '10px 16px',
          backgroundColor: 'var(--bg-secondary)',
          fontSize: '13px', color: 'var(--text-secondary)',
        }}>
          <span style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
            <Database size={14} />
            RAM: {hardwareInfo.available_ram_gb.toFixed(1)} GB free / {hardwareInfo.total_ram_gb.toFixed(1)} GB
          </span>
          <span style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
            <Cpu size={14} />
            CPU: {hardwareInfo.cpu_cores} cores
          </span>
          <span style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
            <Cpu size={14} />
            GPU: {hardwareInfo.gpu_available
              ? `${hardwareInfo.gpu_vram_gb?.toFixed(1) ?? '?'} GB VRAM`
              : 'Not detected'}
          </span>
          <span style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
            <HardDrive size={14} />
            Storage: {formatBytes(hardwareInfo.storage_used_bytes)} used / {formatBytes(hardwareInfo.storage_available_bytes)} free
          </span>
        </div>
      )}

      {/* Search + Tabs */}
      <div style={{ padding: '16px 16px 0', maxWidth: '960px', margin: '0 auto', width: '100%' }}>
        {/* Search Bar */}
        <div style={{ position: 'relative', marginBottom: '16px' }}>
          <Search size={16} style={{ position: 'absolute', left: '14px', top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />
          <input
            type="text"
            placeholder="Search by model name, company (Google, Meta, OpenAI...), or tags..."
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            style={{
              width: '100%', padding: '10px 14px 10px 40px',
              border: '1px solid var(--border-primary)', borderRadius: '12px',
              fontSize: '14px', outline: 'none', backgroundColor: 'var(--bg-input)',
              color: 'var(--text-primary)',
            }}
          />
        </div>

        {/* Tabs + Sort/Refresh */}
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', borderBottom: '1px solid var(--border-primary)' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '0' }}>
            {(['available', 'downloads', 'installed'] as const).map(tab => (
              <button
                key={tab}
                onClick={() => setActiveTab(tab)}
                style={{
                  padding: '10px 20px', fontSize: '14px', fontWeight: 500,
                  border: 'none', background: 'none', cursor: 'pointer',
                  color: activeTab === tab ? 'var(--accent)' : 'var(--text-secondary)',
                  borderBottom: activeTab === tab ? '2px solid var(--accent)' : '2px solid transparent',
                  position: 'relative',
                }}
              >
                {tab.charAt(0).toUpperCase() + tab.slice(1)}
                {tab === 'downloads' && activeDownloadCount > 0 && (
                  <span style={{
                    position: 'absolute', top: '4px', right: '2px',
                    backgroundColor: 'var(--danger)', color: 'var(--bg-surface)', fontSize: '11px',
                    borderRadius: '50%', width: '18px', height: '18px',
                    display: 'flex', alignItems: 'center', justifyContent: 'center',
                  }}>
                    {activeDownloadCount}
                  </span>
                )}
              </button>
            ))}
          </div>
          <div style={{ display: 'flex', alignItems: 'center', gap: '8px', paddingBottom: '2px' }}>
            {/* Sort dropdown */}
            <div style={{ position: 'relative' }}>
              <button className="header-button" onClick={() => setShowSortDropdown(!showSortDropdown)}>
                Sort <ChevronDown size={14} />
              </button>
              {showSortDropdown && (
                <div className="dropdown-menu" style={{ right: 0, top: '100%', minWidth: '160px' }}>
                  {sortOptions.map(opt => (
                    <button
                      key={opt.value}
                      className="dropdown-item"
                      style={{ fontSize: '13px', fontWeight: sortBy === opt.value ? 600 : 400 }}
                      onClick={() => { setSortBy(opt.value); setShowSortDropdown(false); }}
                    >
                      {opt.label}
                    </button>
                  ))}
                </div>
              )}
            </div>
            <button className="header-button" onClick={refreshCatalog}>
              <RefreshCw size={16} />
              Refresh
            </button>
          </div>
        </div>
      </div>

      {/* API Keys Section */}
      <div style={{ padding: '16px', maxWidth: '960px', margin: '0 auto', width: '100%' }}>
        <div style={{
          padding: '14px 16px', marginBottom: '0',
          backgroundColor: 'var(--bg-secondary)', borderRadius: '12px',
          border: '1px solid var(--border-primary)',
        }}>
          <div style={{ fontSize: '14px', fontWeight: 600, color: 'var(--text-primary)', marginBottom: '12px' }}>
            API Keys
          </div>
          
          {/* HuggingFace Token */}
          <div style={{ marginBottom: showHfTokenInput ? '12px' : '8px' }}>
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: showHfTokenInput ? '10px' : '0' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                <span style={{
                  width: '8px', height: '8px', borderRadius: '50%',
                  backgroundColor: hfToken ? 'var(--success-fg)' : 'var(--danger)',
                  boxShadow: hfToken ? '0 0 8px #166534' : '0 0 8px #ef4444',
                  marginRight: '8px'
                }} />
                <span style={{ fontSize: '13px', fontWeight: 500, color: 'var(--text-primary)' }}>HuggingFace Token</span>
              </div>
              <button
                onClick={() => setShowHfTokenInput(!showHfTokenInput)}
                style={{
                  padding: '4px 16px', borderRadius: '6px', fontSize: '12px',
                  border: '1px solid var(--border-primary)', backgroundColor: 'var(--bg-surface)',
                  color: 'var(--text-primary)', cursor: 'pointer', fontWeight: 500,
                }}
              >
                {showHfTokenInput ? 'Hide' : hfToken ? 'Change' : 'Add'}
              </button>
            </div>
            {showHfTokenInput && (
              <div>
                <input
                  id="hf-token-input"
                  type="password"
                  placeholder="hf_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
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
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      const val = (e.target as HTMLInputElement).value.trim();
                      if (val) {
                        storeHfToken(val);
                        setApiKey('huggingface', val);
                        persistApiKey('huggingface', val).catch(console.error);
                      } else {
                        clearStoredHfToken();
                        setApiKey('huggingface', '');
                      }
                    }
                  }}
                  style={{
                    width: '100%', padding: '8px 12px', borderRadius: '8px',
                    border: '1px solid var(--border-primary)', fontSize: '13px',
                    backgroundColor: 'var(--bg-input)', color: 'var(--text-primary)',
                    outline: 'none',
                  }}
                />
                <p style={{ fontSize: '11px', color: 'var(--text-muted)', marginTop: '6px' }}>
                  Required for gated models (Gemma, Llama-2, etc.). Get a token at{' '}
                  <a href="#" onClick={async (e) => {
                    e.preventDefault();
                    await open('https://huggingface.co/settings/tokens');
                  }} style={{ color: 'var(--accent)', textDecoration: 'none', cursor: 'pointer' }}>huggingface.co/settings/tokens</a>
                </p>
                <p style={{ fontSize: '11px', color: 'var(--text-muted)', marginTop: '4px' }}>
                  <strong>Note:</strong> After adding your token, you may need to restart the application for it to take effect.
                </p>
              </div>
            )}
          </div>
          
        </div>
      </div>

      {/* Content Area - Scrollable */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '16px', backgroundColor: 'var(--bg-canvas)' }}>
        <div style={{ maxWidth: '960px', margin: '0 auto' }}>
          {fetchError && !isLoading && (
            <div style={{
              padding: '14px 16px', marginBottom: '16px',
              backgroundColor: 'var(--danger-quiet)', borderRadius: '12px',
              border: '1px solid #fca5a5', color: 'var(--danger-fg)', fontSize: '13px',
            }}>
              {fetchError}
            </div>
          )}

          {isLoading ? (
            <div style={{ display: 'flex', justifyContent: 'center', padding: '60px 0' }}>
              <div style={{
                width: '32px', height: '32px', border: '3px solid var(--border-primary)',
                borderTop: '3px solid var(--accent)', borderRadius: '50%',
                animation: 'spin 1s linear infinite',
              }} />
            </div>
          ) : activeTab === 'downloads' ? (
            /* Downloads Tab */
            <div>
              {downloads.length === 0 ? (
                <div style={{ textAlign: 'center', padding: '60px 0', color: 'var(--text-muted)' }}>
                  <Download size={40} style={{ margin: '0 auto 12px', opacity: 0.5 }} />
                  <p>No active downloads</p>
                  <p style={{ marginTop: '8px', fontSize: '13px' }}>
                    Go to the <button onClick={() => setActiveTab('available')} style={{ color: 'var(--accent)', textDecoration: 'underline', background: 'none', border: 'none', cursor: 'pointer', fontSize: '13px' }}>Available</button> tab to browse and download models.
                  </p>
                </div>
              ) : (
                <div style={{ display: 'flex', flexDirection: 'column', gap: '12px' }}>
                  {downloads.map(download => (
                    <DownloadCard
                      key={download.download_id}
                      download={download}
                      onPause={handlePauseDownload}
                      onResume={handleResumeDownload}
                      onCancel={handleCancelDownload}
                      formatBytes={formatBytes}
                    />
                  ))}
                </div>
              )}
            </div>
          ) : (
            /* Installed / Available Tab */
            <div>
              {filteredModels.length === 0 ? (
                <div style={{ textAlign: 'center', padding: '60px 0', color: 'var(--text-muted)' }}>
                  <Download size={40} style={{ margin: '0 auto 12px', opacity: 0.5 }} />
                  <p>{searchQuery ? 'No models match your search' : `No ${activeTab} models`}</p>
                  {activeTab === 'installed' && !searchQuery && (
                    <p style={{ marginTop: '8px', fontSize: '13px' }}>
                      Go to the <button onClick={() => setActiveTab('available')} style={{ color: 'var(--accent)', textDecoration: 'underline', background: 'none', border: 'none', cursor: 'pointer', fontSize: '13px' }}>Available</button> tab to browse and install models.
                    </p>
                  )}
                </div>
              ) : (
                <div className="models-grid">
                  {filteredModels.map(model => {
                    const dl = getDownloadForModel(model.id);
                    return (
                      <ModelCard
                        key={model.id}
                        model={model}
                        download={dl}
                        isInstalled={isInstalled(model)}
                        isAvailable={isAvailable(model)}
                        onInstall={handleInstallModel}
                        onRemove={handleRemoveModel}
                        onPauseDownload={handlePauseDownload}
                        onResumeDownload={handleResumeDownload}
                        onCancelDownload={handleCancelDownload}
                        formatBytes={formatBytes}
                        selectedModel={selectedModel}
                        activeModelInfo={activeModelInfo}
                        onSwitchModel={handleSwitchModel}
                      />
                    );
                  })}
                </div>
              )}
            </div>
          )}
        </div>
      </div>

      {/* Download notification bubble (shown on other pages via App-level, but also here for context) */}
      {activeTab !== 'downloads' && activeDownloadCount > 0 && (
        <div className="download-bubble">
          <div className="download-bubble-header">
            <span className="download-bubble-title">
              {activeDownloadCount} download{activeDownloadCount > 1 ? 's' : ''} in progress
            </span>
            <button className="download-bubble-close" onClick={() => setActiveTab('downloads')}>
              View
            </button>
          </div>
          {/* Third and last inline progress bar in this file, now also on the
              shared primitive. All three previously hand-rolled a trough and a
              fill with their own heights, radii and transition durations. */}
          {downloads.filter(d => d.status === 'Downloading').slice(0, 2).map(dl => (
            <div key={dl.download_id} style={{ marginBottom: 'var(--sp-2)' }}>
              <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-secondary)', marginBottom: 'var(--sp-1)' }}>
                {dl.model_name}
              </div>
              <div
                className="ui-progress ui-progress--thin is-active"
                style={{ '--progress': Math.min(dl.percentage ?? 0, 100) } as React.CSSProperties}
                role="progressbar"
                aria-valuenow={Math.round(dl.percentage ?? 0)}
                aria-valuemin={0}
                aria-valuemax={100}
              >
                <div className="ui-progress__fill" />
              </div>
              <div
                style={{
                  fontSize: 'var(--fs-2xs)',
                  color: 'var(--text-muted)',
                  marginTop: '3px',
                  fontVariantNumeric: 'tabular-nums',
                }}
              >
                {Math.min(dl.percentage ?? 0, 100).toFixed(0)}% · {formatBytes(dl.bytes_downloaded ?? 0)}
                {dl.total_bytes
                  ? ` / ${formatBytes(Math.max(dl.total_bytes, dl.bytes_downloaded ?? 0))}`
                  : ''}
              </div>
            </div>
          ))}
        </div>
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
            borderRadius: 'var(--r-xl)',
            padding: 'var(--sp-6)',
            maxWidth: '500px',
            width: '90%',
            boxShadow: 'var(--shadow-lg)',
          }}>
            <h3 style={{ margin: '0 0 var(--sp-4) 0', color: 'var(--text-heading)', fontSize: 'var(--fs-xl)' }}>Remove this model?</h3>
            <p style={{ margin: '0 0 16px 0', color: 'var(--text-secondary)', lineHeight: 1.5 }}>
              Are you sure you want to permanently remove this model? This action cannot be undone.
            </p>
            
            {/* Model Details */}
            <div style={{
              backgroundColor: 'var(--bg-inset)',
              border: '1px solid var(--border-subtle)',
              borderRadius: 'var(--r-md)',
              padding: '16px',
              marginBottom: '16px',
            }}>
              <h4 style={{ margin: '0 0 12px 0', color: 'var(--text-primary)', fontSize: '16px' }}>{modelToRemove.name}</h4>
              
              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, 1fr)', gap: '12px', marginBottom: '12px' }}>
                <div>
                  <div style={{ fontSize: '12px', color: 'var(--text-muted)', marginBottom: '4px' }}>Format</div>
                  <div style={{ fontSize: '14px', color: 'var(--text-primary)', fontWeight: 500 }}>{modelToRemove.format.toUpperCase()}</div>
                </div>
                <div>
                  <div style={{ fontSize: '12px', color: 'var(--text-muted)', marginBottom: '4px' }}>Size</div>
                  <div style={{ fontSize: '14px', color: 'var(--text-primary)', fontWeight: 500 }}>{formatBytes(modelToRemove.size_bytes)}</div>
                </div>
                <div>
                  <div style={{ fontSize: '12px', color: 'var(--text-muted)', marginBottom: '4px' }}>Parameters</div>
                  <div style={{ fontSize: '14px', color: 'var(--text-primary)', fontWeight: 500 }}>
                    {modelToRemove.parameters || 'N/A'}
                  </div>
                </div>
                <div>
                  <div style={{ fontSize: '12px', color: 'var(--text-muted)', marginBottom: '4px' }}>Source</div>
                  <div style={{ fontSize: '14px', color: 'var(--text-primary)', fontWeight: 500 }}>
                    {modelToRemove.download_source ? modelToRemove.download_source.charAt(0).toUpperCase() + modelToRemove.download_source.slice(1) : 'Local'}
                  </div>
                </div>
              </div>
              
              <div style={{ display: 'flex', gap: '6px', flexWrap: 'wrap' }}>
                {(modelToRemove.tags || []).slice(0, 5).map((tag, index) => (
                  <span key={index} style={{
                    fontSize: '11px',
                    backgroundColor: 'rgba(99, 102, 241, 0.1)',
                    color: 'var(--accent)',
                    padding: '4px 8px',
                    borderRadius: '6px',
                  }}>
                    #{tag}
                  </span>
                ))}
              </div>
            </div>
            
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '8px' }}>
              <button
                onClick={cancelRemoveModel}
                style={{
                  padding: '8px 16px',
                  borderRadius: '8px',
                  border: '1px solid var(--border-primary)',
                  backgroundColor: 'transparent',
                  color: 'var(--text-primary)',
                  cursor: 'pointer',
                  fontWeight: 500,
                }}
              >
                Cancel
              </button>
              <button
                onClick={confirmRemoveModel}
                style={{
                  padding: '8px 16px',
                  borderRadius: '8px',
                  border: 'none',
                  backgroundColor: 'var(--danger)',
                  color: 'var(--text-on-accent)',
                  cursor: 'pointer',
                  fontWeight: 600,
                }}
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

// Model Card Component with inline download progress
const ModelCard: React.FC<{
  model: Model;
  download?: DownloadProgress;
  isInstalled: boolean;
  isAvailable: boolean;
  onInstall: (m: Model) => void;
  onRemove: (id: string) => void;
  onPauseDownload: (id: string) => void;
  onResumeDownload: (id: string) => void;
  onCancelDownload: (id: string) => void;
  formatBytes: (b: number) => string;
  selectedModel?: SelectedModel | null;
  activeModelInfo?: ActiveModelInfo | null;
  onSwitchModel?: (modelId: string, modelName: string) => void;
}> = ({ model, download, isInstalled, isAvailable, onInstall, onRemove, onPauseDownload, onResumeDownload, onCancelDownload, formatBytes, selectedModel, activeModelInfo, onSwitchModel }) => {
  const { setApiKey } = useAuth();
  const modelIdClean = model.id;
  const isSelected = selectedModel?.id === modelIdClean;

  const isActiveModel = () => {
    return activeModelInfo && activeModelInfo.model_path && 
      (activeModelInfo.model_path.includes(model.id) || 
       (model.filename && activeModelInfo.model_path.includes(model.filename)) ||
       activeModelInfo.model_name.includes(model.name));
  };
  
  // Define helper function locally to ensure availability
  const getSourceLabel = (source?: string): string => {
    switch (source) {
      case 'huggingface': return 'HuggingFace';
      case 'ollama': return 'Ollama';

      default: return 'Local';
    }
  };
  
  /* Compatibility verdict REMOVED from the card (user decision 2026-08-08):
     the "Runs well here / Too large for this machine" line on every card read
     as odd/judgmental. The backend still computes compatibility_score (it
     drives Best Match sorting); it is simply no longer rendered here. */

  // Parameter count: from the field when the backend supplies one, otherwise
  // from the name, since many HuggingFace entries encode it only there.
  const paramLabel =
    model.parameters ??
    model.name.match(/(\d+(?:\.\d+)?\s*[BMK])\b/i)?.[1]?.replace(/\s+/g, '').toUpperCase() ??
    null;

  const dlState = download?.status ?? '';
  const dlTone =
    dlState === 'Paused'
      ? 'warning'
      : dlState === 'Failed'
        ? 'danger'
        : dlState === 'Completed'
          ? 'success'
          : '';

  return (
    <div className={`model-card${isSelected || isActiveModel() ? ' is-active' : ''}`}>
      <div className="model-card__head">
        <h3 className="model-card__name">{model.name}</h3>
        <span className="ui-badge">{getSourceLabel(model.download_source)}</span>
      </div>

      {model.description && <p className="model-card__desc">{model.description}</p>}

      <div className="model-card__meta">
        {model.mmproj_filename && (
          <span
            className="ui-badge ui-badge--success"
            title="Vision-capable: reads text (including handwriting) from images and scanned documents. The multimodal projector downloads with the model and loads automatically."
          >
            Vision
          </span>
        )}
        <span className="ui-badge">{model.format.toUpperCase()}</span>
        {paramLabel && <span className="ui-badge ui-badge--accent">{paramLabel}</span>}
        {(model.provider || model.author) && <span className="ui-badge">{model.provider || model.author}</span>}
        {model.size_bytes > 0 && (
          <span
            className="ui-badge"
            title={
              model.mmproj_filename && (model.mmproj_size_bytes ?? 0) > 0
                ? `Model ${formatBytes(model.size_bytes)} + vision projector ${formatBytes(model.mmproj_size_bytes!)}`
                : undefined
            }
          >
            {formatBytes(model.size_bytes + (model.mmproj_filename ? (model.mmproj_size_bytes ?? 0) : 0))}
          </span>
        )}
        {model.context_length != null && model.context_length > 0 && (
          <span className="ui-badge" title="Context window this model was trained for">
            {model.context_length >= 1024
              ? `${Math.round(model.context_length / 1024)}K ctx`
              : `${model.context_length} ctx`}
          </span>
        )}
      </div>

      {model.tags.length > 0 && (
        <div className="model-card__tags">
          {model.tags.slice(0, 5).map(tag => (
            <span key={tag}>#{tag}</span>
          ))}
          {model.tags.length > 5 && <span>+{model.tags.length - 5}</span>}
        </div>
      )}

      {/* Download in flight.
          Built on the .ui-progress primitive, which means a PAUSED transfer is
          visibly still: the travelling sheen runs only while .is-active. That
          stillness is the clearest confirmation that pausing actually worked —
          the previous bar looked identical paused or downloading, and only the
          amber fill colour distinguished them. */}
      {download && (
        <div className="model-dl">
          <div className="model-dl__top">
            <span className="model-dl__pct">
              {/* Clamped: the backend total is an estimate until the transfer's
                  own Content-Length lands, and a raw value once rendered as
                  "112%" in production. The bars below already clamped; the
                  number did not. */}
              {Math.min(download.percentage ?? 0, 100).toFixed(0)}
              <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-muted)' }}>%</span>
            </span>
            <span className="model-dl__state">{dlState || 'Downloading'}</span>
          </div>

          <div
            className={[
              'ui-progress',
              dlTone && `ui-progress--${dlTone}`,
              dlState === 'Downloading' && 'is-active',
              // Queued has no meaningful percentage yet, so an indeterminate
              // bar is honest where a 0% determinate bar reads as "stalled".
              dlState === 'Queued' && 'ui-progress--indeterminate',
            ]
              .filter(Boolean)
              .join(' ')}
            style={{ '--progress': Math.min(download.percentage ?? 0, 100) } as React.CSSProperties}
            role="progressbar"
            aria-valuenow={Math.round(download.percentage ?? 0)}
            aria-valuemin={0}
            aria-valuemax={100}
          >
            <div className="ui-progress__fill" />
          </div>

          <div className="model-dl__facts">
            <span className="model-dl__fact">
              <span className="model-dl__fact-key">Received</span>
              <span className="model-dl__fact-val">
                {formatBytes(download.bytes_downloaded ?? 0)}
                {/* Never show "2 GB / 1.8 GB": if the running total is behind
                    the bytes already received, the received figure IS the
                    better lower bound for the total. */}
                {download.total_bytes
                  ? ` / ${formatBytes(Math.max(download.total_bytes, download.bytes_downloaded ?? 0))}`
                  : ''}
              </span>
            </span>
            <span className="model-dl__fact">
              <span className="model-dl__fact-key">Speed</span>
              <span className="model-dl__fact-val">
                {download.speed_bps > 0 ? `${formatBytes(download.speed_bps)}/s` : '—'}
              </span>
            </span>
            <span className="model-dl__fact">
              <span className="model-dl__fact-key">Remaining</span>
              <span className="model-dl__fact-val">
                {download.speed_bps > 0 &&
                download.estimated_time_remaining != null &&
                download.estimated_time_remaining > 0
                  ? formatEta(download.estimated_time_remaining)
                  : '—'}
              </span>
            </span>
          </div>

          {download.error_message && (
            <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--danger-fg)' }}>
              {download.error_message}
            </span>
          )}

          <div className="model-dl__actions">
            {dlState === 'Downloading' && (
              <button
                type="button"
                className="ui-btn ui-btn--sm ui-btn--secondary"
                onClick={() => onPauseDownload(download.download_id)}
              >
                <Pause size={12} /> Pause
              </button>
            )}
            {dlState === 'Paused' && (
              <button
                type="button"
                className="ui-btn ui-btn--sm ui-btn--primary"
                onClick={() => onResumeDownload(download.download_id)}
              >
                <Play size={12} /> Resume
              </button>
            )}
            <button
              type="button"
              className="ui-btn ui-btn--sm ui-btn--danger"
              onClick={() => onCancelDownload(download.download_id)}
            >
              <Square size={12} /> Stop
            </button>
          </div>
        </div>
      )}

      <div className="model-card__spacer" />

      <div className="model-card__actions">
        {!download && isInstalled && (
          <>
            <button
              type="button"
              className="ui-iconbtn ui-iconbtn--danger"
              onClick={() => onRemove(model.id)}
              title="Remove this model from disk"
              aria-label={`Remove ${model.name}`}
            >
              <Trash2 size={14} />
            </button>
            <button
              type="button"
              className={`ui-btn ui-btn--sm ui-btn--pill ${
                isSelected || isActiveModel() ? 'ui-btn--secondary' : 'ui-btn--primary'
              }`}
              onClick={() => onSwitchModel?.(model.id, model.name)}
              disabled={Boolean(isActiveModel())}
            >
              {isSelected || isActiveModel() ? 'Active' : 'Use model'}
            </button>
          </>
        )}
        {!download && isAvailable && (
          <button
            type="button"
            className="ui-btn ui-btn--sm ui-btn--pill ui-btn--solid"
            onClick={() => {
              // A gated HuggingFace repo needs a token. Ask once, then continue
              // into the same install call whichever way it resolves.
              if (model.download_source === 'huggingface' && !getStoredHfToken()) {
                showHuggingFaceApiKeyModal(() => onInstall(model), setApiKey, () => onInstall(model));
              } else {
                onInstall(model);
              }
            }}
          >
            <Download size={13} /> Install
          </button>
        )}
        {download && (
          <span className="model-dl__state">
            {dlState === 'Queued' ? 'Queued' : `Installing ${(download.percentage ?? 0).toFixed(0)}%`}
          </span>
        )}
      </div>
    </div>
  );
};

/** Human ETA. Extracted to module scope because BOTH download surfaces
 *  rendered it, each with its own inline `Math.round(s/60)m Math.round(s%60)s`
 *  — which printed "0m 7s" for seven seconds and "94m 3s" for an hour and a
 *  half, neither of which reads naturally. */
const formatEta = (seconds: number): string => {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  const h = Math.floor(s / 3600);
  return `${h}h ${Math.round((s % 3600) / 60)}m`;
};

/** Which `.ui-progress` tone a download status maps to. Shared by both cards so
 *  the same status can never be two colours in two places. */
const downloadTone = (status: string): string =>
  status === 'Paused'
    ? 'warning'
    : status === 'Failed'
      ? 'danger'
      : status === 'Completed'
        ? 'success'
        : '';

/** Badge variant for a download status.
 *  Replaces two parallel 6-branch ternaries that hardcoded twelve hex values
 *  (#dcfce7/#166534, #dbeafe/#1e40af, …) — all of them light-mode tints with
 *  dark text, so on the dark theme this badge was a pale pastel block. */
const statusBadgeClass = (status: string): string => {
  switch (status) {
    case 'Completed':
      return 'ui-badge ui-badge--success';
    case 'Failed':
      return 'ui-badge ui-badge--danger';
    case 'Paused':
      return 'ui-badge ui-badge--warning';
    case 'Downloading':
    case 'Starting':
    case 'Queued':
      return 'ui-badge ui-badge--accent';
    default:
      return 'ui-badge';
  }
};

// Download Card for the Downloads tab
const DownloadCard: React.FC<{
  download: DownloadProgress;
  onPause: (id: string) => void;
  onResume: (id: string) => void;
  onCancel: (id: string) => void;
  formatBytes: (b: number) => string;
}> = ({ download, onPause, onResume, onCancel, formatBytes }) => {
  const inFlight =
    download.status === 'Downloading' ||
    download.status === 'Starting' ||
    download.status === 'Paused' ||
    download.status === 'Queued';
  const tone = downloadTone(download.status);

  return (
    <div className="model-card">
      <div className="model-card__head">
        <h3 className="model-card__name">{download.model_name}</h3>
        <span className={statusBadgeClass(download.status)}>
          {download.status === 'Downloading' && <span className="ui-badge__dot ui-badge__dot--pulse" />}
          {download.status}
        </span>
      </div>

      {inFlight && (
        <div className="model-dl">
          <div className="model-dl__top">
            <span className="model-dl__pct">
              {/* Clamped: the backend total is an estimate until the transfer's
                  own Content-Length lands, and a raw value once rendered as
                  "112%" in production. The bars below already clamped; the
                  number did not. */}
              {Math.min(download.percentage ?? 0, 100).toFixed(0)}
              <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-muted)' }}>%</span>
            </span>
            <span className="model-dl__state">
              {download.elapsed_time > 0 ? `${formatEta(download.elapsed_time)} elapsed` : ''}
            </span>
          </div>

          <div
            className={[
              'ui-progress',
              'ui-progress--thick',
              tone && `ui-progress--${tone}`,
              download.status === 'Downloading' && 'is-active',
              download.status === 'Queued' && 'ui-progress--indeterminate',
            ]
              .filter(Boolean)
              .join(' ')}
            style={{ '--progress': Math.min(download.percentage ?? 0, 100) } as React.CSSProperties}
            role="progressbar"
            aria-valuenow={Math.round(download.percentage ?? 0)}
            aria-valuemin={0}
            aria-valuemax={100}
          >
            <div className="ui-progress__fill" />
          </div>

          <div className="model-dl__facts">
            <span className="model-dl__fact">
              <span className="model-dl__fact-key">Received</span>
              <span className="model-dl__fact-val">
                {formatBytes(download.bytes_downloaded ?? 0)}
                {/* Never show "2 GB / 1.8 GB": if the running total is behind
                    the bytes already received, the received figure IS the
                    better lower bound for the total. */}
                {download.total_bytes
                  ? ` / ${formatBytes(Math.max(download.total_bytes, download.bytes_downloaded ?? 0))}`
                  : ''}
              </span>
            </span>
            <span className="model-dl__fact">
              <span className="model-dl__fact-key">Speed</span>
              <span className="model-dl__fact-val">
                {download.speed_bps > 0 ? `${formatBytes(download.speed_bps)}/s` : '—'}
              </span>
            </span>
            <span className="model-dl__fact">
              <span className="model-dl__fact-key">Remaining</span>
              <span className="model-dl__fact-val">
                {download.speed_bps > 0 &&
                download.estimated_time_remaining != null &&
                download.estimated_time_remaining > 0
                  ? formatEta(download.estimated_time_remaining)
                  : '—'}
              </span>
            </span>
          </div>

          <div className="model-dl__actions">
            {download.status === 'Downloading' && (
              <button
                type="button"
                className="ui-btn ui-btn--sm ui-btn--secondary"
                onClick={() => onPause(download.download_id)}
              >
                <Pause size={13} /> Pause
              </button>
            )}
            {download.status === 'Paused' && (
              <button
                type="button"
                className="ui-btn ui-btn--sm ui-btn--primary"
                onClick={() => onResume(download.download_id)}
              >
                <Play size={13} /> Resume
              </button>
            )}
            <button
              type="button"
              className="ui-btn ui-btn--sm ui-btn--danger"
              onClick={() => onCancel(download.download_id)}
            >
              <Square size={13} /> Stop
            </button>
          </div>
        </div>
      )}

      {download.status === 'Failed' && download.error_message && (
        <p style={{ color: 'var(--danger-fg)', fontSize: 'var(--fs-sm)', lineHeight: 'var(--lh-normal)' }}>
          {download.error_message}
        </p>
      )}
    </div>
  );
};

export default ModelsPanel;
