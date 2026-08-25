/**
 * Local Storage — the document vault.
 *
 * A persistent library of documents that can be referenced from any chat with
 * `@filename`. Rebuilt from an indented tree of inline-styled rows into a
 * table, for one reason above the rest:
 *
 *   THE OLD SURFACE NEVER SAID WHETHER A DOCUMENT HAD BEEN READ.
 *
 * Storing a file and extracting its text are separate steps here — upload
 * records the bytes and schedules extraction in a background task
 * (files_api::spawn_background_extraction). That task can fail per file, and
 * legitimately does: a legacy binary .doc is refused outright, a scan with no
 * text layer depends on Windows OCR being available, and a task in flight does
 * not survive the app closing. Until now every one of those outcomes rendered
 * as an ordinary row with a byte count, and the user discovered the problem
 * only when the model answered without the document.
 *
 * The status column closes that gap. It joins GET /documents onto the file rows
 * by local_file_id — an endpoint that already existed and already returns
 * extraction_status — so no backend change was needed to surface it.
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { getApiBaseSync } from '../api/backendUrl';
import { fetchWithNetworkRetry } from '../api/fetchWithTimeout';
import {
  ArrowLeft, FolderPlus, Trash2, Upload, Folder, FolderOpen,
  ChevronRight, MoreVertical, Eye, Search, Library, X,
} from 'lucide-react';
import { DocumentViewer } from './DocumentViewer';
import { FILE_INPUT_ACCEPT, SUPPORTED_LIST_LABEL, partitionSupported } from '../supportedFormats';
import { exceedsUploadBudget, totalBytesOf, uploadTooLargeMessage } from '../uploadLimits';
import { useNotificationHelpers } from '../contexts/NotificationContext';
import './VaultPanel.css';

interface FileEntry {
  id: number;
  name: string;
  path: string;
  isDirectory: boolean;
  size: number;
  modified: string;
  access_count?: number;
  last_accessed?: string | null;
  children?: FileEntry[];
}

/** Extraction outcome for one stored file, as reported by GET /documents. */
type ExtractionState = 'indexed' | 'pending' | 'failed';

interface DocumentInfo {
  documentId: number;
  state: ExtractionState;
  charCount: number;
  error?: string | null;
}

const formatSize = (bytes: number): string => {
  if (!bytes) return '—';
  const k = 1024;
  const units = ['B', 'KB', 'MB', 'GB'];
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(k)), units.length - 1);
  return `${parseFloat((bytes / k ** i).toFixed(1))} ${units[i]}`;
};

const formatDate = (iso: string): string => {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '—';
  const now = new Date();
  const sameYear = d.getFullYear() === now.getFullYear();
  return d.toLocaleDateString(undefined, {
    day: 'numeric',
    month: 'short',
    ...(sameYear ? {} : { year: 'numeric' }),
  });
};

/** Short extension label for the row glyph. Same rationale as the chat tray:
 *  an emoji tells you less than three letters of the real extension, and
 *  renders differently on every machine. */
const extensionLabel = (name: string): string => {
  const ext = name.split('.').pop()?.toLowerCase() ?? '';
  if (!ext || ext === name.toLowerCase()) return '—';
  if (ext === 'jpeg') return 'JPG';
  return ext.slice(0, 4).toUpperCase();
};

const STATUS_COPY: Record<ExtractionState, string> = {
  indexed: 'Indexed',
  pending: 'Not indexed',
  failed: 'Unreadable',
};

/** Count non-directory entries in a tree.
 *  Module scope, not a closure inside the component: as a local function it got
 *  a fresh identity every render, which made the useMemo depending on it
 *  recompute every render and defeated the point of memoising it. */
const countFiles = (entries: FileEntry[]): number =>
  entries.reduce((n, e) => n + (e.isDirectory ? countFiles(e.children ?? []) : 1), 0);

/** Parse GET /documents into a local_file_id → extraction-state index.
 *  Pure, so it can be exercised without mounting the component. */
const indexDocuments = (documents: Record<string, unknown>[]): Map<number, DocumentInfo> => {
  const map = new Map<number, DocumentInfo>();
  for (const raw of documents) {
    const localFileId = raw.local_file_id as number | null;
    if (localFileId == null) continue; // paperclip-only document, not in the library
    const status = String(raw.extraction_status ?? '');
    const chars = Number(raw.char_count ?? 0);
    map.set(localFileId, {
      documentId: Number(raw.id),
      // status 'ok' with zero characters is NOT success — it is the exact shape
      // a silently-empty extraction takes (a scan whose OCR recovered nothing),
      // and calling it indexed is what let those files look usable.
      state: status === 'ok' && chars > 0 ? 'indexed' : 'failed',
      charCount: chars,
      error: (raw.extraction_error as string | null) ?? null,
    });
  }
  return map;
};

const VaultBody: React.FC = () => {
  const [files, setFiles] = useState<FileEntry[]>([]);
  const [docsByLocalFile, setDocsByLocalFile] = useState<Map<number, DocumentInfo>>(new Map());
  const [expandedDirs, setExpandedDirs] = useState<Set<number>>(new Set());
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [query, setQuery] = useState('');
  const [isLoading, setIsLoading] = useState(true);
  const [showNewFolder, setShowNewFolder] = useState(false);
  const [newFolderName, setNewFolderName] = useState('');
  const [newFolderParentId, setNewFolderParentId] = useState<number | null>(null);
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; entry: FileEntry } | null>(null);
  const [previewTarget, setPreviewTarget] = useState<{ documentId: number; name: string } | null>(null);
  const [isDragging, setIsDragging] = useState(false);

  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);
  const [uploadParentId, setUploadParentId] = useState<number | null>(null);

  const { showSuccess, showError, showWarning } = useNotificationHelpers();

  /* ── Data ─────────────────────────────────────────────────────────────────
     Two requests, in parallel. The document index is a separate id space from
     local_files, so it is keyed by local_file_id for the join. A failure to
     load it is NOT fatal: the file list still renders, with status shown as
     unknown rather than a wrong value. */
  const fetchAll = useCallback(async () => {
    const apiBase = getApiBaseSync();

    // Both requests issued together. The document index lives in a different id
    // space from local_files, so it is keyed by local_file_id for the join.
    // Losing it is NOT fatal: the file list still renders and status reads
    // "Unknown" rather than a value that might be wrong.
    const filesPromise = fetch(`${apiBase}/files`)
      .then(r => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then(d => (Array.isArray(d) ? (d as FileEntry[]) : []))
      .catch(e => {
        console.error('[Vault] Could not load files:', e);
        showError('Could not load your Vault', e instanceof Error ? e.message : String(e));
        return [] as FileEntry[];
      });

    const docsPromise = fetch(`${apiBase}/documents`)
      .then(r => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((payload: { documents?: Record<string, unknown>[] }) => indexDocuments(payload.documents ?? []))
      .catch(e => {
        console.warn('[Vault] Document index unavailable; status will read Unknown:', e);
        return new Map<number, DocumentInfo>();
      });

    return Promise.all([filesPromise, docsPromise]);
  }, [showError]);

  /** Load, then commit. Split so the effect below performs no synchronous
   *  setState — and so both the mount path and every refresh share one place
   *  that decides what "loaded" means. */
  const [refreshToken, setRefreshToken] = useState(0);
  const refresh = useCallback(() => setRefreshToken(t => t + 1), []);

  useEffect(() => {
    // `cancelled` guards against a response arriving after unmount, or after a
    // newer refresh superseded this one — without it a slow first load could
    // overwrite the result of a later upload.
    let cancelled = false;
    (async () => {
      const [nextFiles, nextDocs] = await fetchAll();
      if (cancelled) return;
      setFiles(nextFiles);
      setDocsByLocalFile(nextDocs);
      setIsLoading(false);
    })();
    return () => {
      cancelled = true;
    };
  }, [fetchAll, refreshToken]);

  useEffect(() => {
    if (!contextMenu) return;
    const close = () => setContextMenu(null);
    document.addEventListener('click', close);
    document.addEventListener('scroll', close, true);
    return () => {
      document.removeEventListener('click', close);
      document.removeEventListener('scroll', close, true);
    };
  }, [contextMenu]);

  /* ── Search ───────────────────────────────────────────────────────────────
     Filters the tree while KEEPING the ancestors of a match, so a hit three
     folders deep is still shown in context rather than as an orphan row. */
  const visibleFiles = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return files;
    const prune = (entries: FileEntry[]): FileEntry[] =>
      entries.flatMap(e => {
        if (!e.isDirectory) return e.name.toLowerCase().includes(q) ? [e] : [];
        const children = prune(e.children ?? []);
        if (children.length > 0) return [{ ...e, children }];
        return e.name.toLowerCase().includes(q) ? [{ ...e, children: [] }] : [];
      });
    return prune(files);
  }, [files, query]);

  /** While searching, every surviving folder is expanded — a filtered tree
   *  with collapsed matches shows the user nothing. */
  const effectiveExpanded = useMemo(() => {
    if (!query.trim()) return expandedDirs;
    const all = new Set<number>();
    const walk = (entries: FileEntry[]) =>
      entries.forEach(e => {
        if (e.isDirectory) {
          all.add(e.id);
          walk(e.children ?? []);
        }
      });
    walk(visibleFiles);
    return all;
  }, [query, expandedDirs, visibleFiles]);

  const totalFiles = useMemo(() => countFiles(files), [files]);
  const indexedCount = useMemo(
    () => [...docsByLocalFile.values()].filter(d => d.state === 'indexed').length,
    [docsByLocalFile],
  );

  /* ── Upload ─────────────────────────────────────────────────────────────── */
  const uploadBatch = useCallback(
    async (selectedFiles: File[], parentId: number | null) => {
      const { accepted, rejected } = partitionSupported(selectedFiles);

      if (rejected.length > 0 && accepted.length === 0) {
        showWarning(
          rejected.length === 1 ? 'Unsupported file' : 'No supported files',
          `Accepted types are ${SUPPORTED_LIST_LABEL}.`,
        );
        return;
      }

      // Size gate, sharing the budget and the wording used by the chat
      // paperclip (uploadLimits.ts). Without it an oversized folder was
      // rejected by axum with a bare 413 and surfaced as raw response text.
      const totalBytes = totalBytesOf(accepted);
      if (exceedsUploadBudget(totalBytes)) {
        showWarning('Too much at once', uploadTooLargeMessage(totalBytes));
        return;
      }

      const form = new FormData();
      accepted.forEach(f => form.append('files', f));
      const url = parentId
        ? `${getApiBaseSync()}/files/upload?parent_id=${parentId}`
        : `${getApiBaseSync()}/files/upload`;

      try {
        // Network-retrying fetch: survives WebView network suspension (sleep/
        // lid-close). Safe to repeat — uploads are content-hash-deduplicated.
        const response = await fetchWithNetworkRetry(url, { method: 'POST', body: form });
        const text = await response.text();
        if (!response.ok) {
          showError('Upload failed', text || `HTTP ${response.status}`);
          return;
        }
        const skipped =
          rejected.length > 0
            ? ` Skipped ${rejected.length} unsupported: ${rejected.map(f => f.name).join(', ')}.`
            : '';
        showSuccess(
          accepted.length === 1 ? 'Document added' : `${accepted.length} documents added`,
          `Indexing runs in the background — the status column updates when it finishes.${skipped}`,
        );
        refresh();
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        showError(
          'Upload failed',
          /Failed to fetch|NetworkError/.test(message)
            ? 'Could not reach the backend. Make sure the app is running.'
            : message,
        );
      }
    },
    [refresh, showError, showSuccess, showWarning],
  );

  const handleInputChange = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const list = e.target.files;
    if (list && list.length > 0) await uploadBatch(Array.from(list), uploadParentId);
    // Reset so re-picking the same file fires another change event.
    e.target.value = '';
    setUploadParentId(null);
  };

  /** Open one of the two hidden inputs. `parentId` scopes the upload to a
   *  folder; both inputs share one change handler. */
  const triggerUpload = (parentId: number | null = null, folder = false) => {
    setUploadParentId(parentId);
    (folder ? folderInputRef : fileInputRef).current?.click();
  };

  /* Drag-and-drop into the vault. Uses HTML5 dataTransfer here rather than
     Tauri's path-based handler, unlike the chat composer: this endpoint takes
     multipart File objects and does not record on-disk provenance, so File is
     exactly what is needed and no path is lost. */
  const onDragOver = (e: React.DragEvent) => {
    if (!e.dataTransfer.types.includes('Files')) return;
    e.preventDefault();
    setIsDragging(true);
  };
  const onDragLeave = (e: React.DragEvent) => {
    // Fires for child elements too; only clear when the pointer truly leaves.
    if (e.currentTarget === e.target) setIsDragging(false);
  };
  const onDrop = async (e: React.DragEvent) => {
    if (!e.dataTransfer.types.includes('Files')) return;
    e.preventDefault();
    setIsDragging(false);
    const dropped = Array.from(e.dataTransfer.files);
    if (dropped.length > 0) await uploadBatch(dropped, null);
  };

  /* ── Mutations ──────────────────────────────────────────────────────────── */
  const createFolder = async () => {
    const name = newFolderName.trim();
    if (!name) return;
    try {
      const r = await fetch(`${getApiBaseSync()}/files/folder`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name, parent_id: newFolderParentId }),
      });
      if (!r.ok) throw new Error(await r.text());
      setNewFolderName('');
      setShowNewFolder(false);
      setNewFolderParentId(null);
      refresh();
    } catch (e) {
      showError('Could not create the folder', e instanceof Error ? e.message : String(e));
    }
  };

  const deleteEntries = async (ids: number[]) => {
    if (ids.length === 0) return;
    const plural = ids.length === 1 ? 'this item' : `these ${ids.length} items`;
    if (!window.confirm(`Delete ${plural}? Folders are removed with all their contents.`)) return;

    const results = await Promise.allSettled(
      ids.map(id => fetch(`${getApiBaseSync()}/files/${id}`, { method: 'DELETE' })),
    );
    const failed = results.filter(r => r.status === 'rejected' || !r.value.ok).length;
    if (failed > 0) {
      showError(
        'Some items were not deleted',
        `${failed} of ${ids.length} could not be removed. The list has been refreshed.`,
      );
    }
    setSelected(new Set());
    refresh();
  };

  const openPreview = async (entry: FileEntry) => {
    if (entry.isDirectory) return;
    // Fast path: the joined index already knows this file's document id.
    const known = docsByLocalFile.get(entry.id);
    if (known) {
      setPreviewTarget({ documentId: known.documentId, name: entry.name });
      return;
    }
    // Miss — a file uploaded before the document store existed. The backend
    // backfills a document row on demand for exactly this case.
    try {
      const resp = await fetch(`${getApiBaseSync()}/documents/by-local-file/${entry.id}`);
      if (!resp.ok) throw new Error(await resp.text());
      const doc = await resp.json();
      setPreviewTarget({ documentId: doc.id, name: entry.name });
      refresh();
    } catch (e) {
      showError(`Could not preview "${entry.name}"`, e instanceof Error ? e.message : String(e));
    }
  };

  const toggleDir = (id: number) =>
    setExpandedDirs(prev => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const toggleSelected = (id: number) =>
    setSelected(prev => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  /* ── Rows ───────────────────────────────────────────────────────────────── */
  const renderRow = (entry: FileEntry, depth = 0): React.ReactNode => {
    const isOpen = effectiveExpanded.has(entry.id);
    const isSelected = selected.has(entry.id);
    const doc = entry.isDirectory ? undefined : docsByLocalFile.get(entry.id);
    const state: ExtractionState | 'unknown' = entry.isDirectory
      ? 'unknown'
      : (doc?.state ?? 'pending');

    return (
      <React.Fragment key={entry.id}>
        <div
          className={`vault-row vault-row--file${isSelected ? ' is-selected' : ''}${entry.isDirectory ? ' is-folder' : ''}`}
          onClick={() => {
            if (entry.isDirectory) toggleDir(entry.id);
            else void openPreview(entry);
          }}
          onContextMenu={e => {
            e.preventDefault();
            e.stopPropagation();
            setContextMenu({ x: e.clientX, y: e.clientY, entry });
          }}
          tabIndex={0}
          onKeyDown={e => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              if (entry.isDirectory) toggleDir(entry.id);
              else void openPreview(entry);
            }
          }}
          role="button"
        >
          <span className="vault-name" style={{ paddingLeft: `${depth * 18}px` }}>
            <input
              type="checkbox"
              className="vault-checkbox"
              checked={isSelected}
              onChange={() => toggleSelected(entry.id)}
              onClick={e => e.stopPropagation()}
              aria-label={`Select ${entry.name}`}
            />
            {entry.isDirectory ? (
              <button
                type="button"
                className={`vault-disclosure${isOpen ? ' is-open' : ''}`}
                onClick={e => {
                  e.stopPropagation();
                  toggleDir(entry.id);
                }}
                aria-label={isOpen ? 'Collapse' : 'Expand'}
                aria-expanded={isOpen}
              >
                <ChevronRight size={13} />
              </button>
            ) : (
              <span className="vault-spacer" />
            )}
            <span className={`vault-glyph${entry.isDirectory ? ' vault-glyph--folder' : ''}`} aria-hidden="true">
              {entry.isDirectory ? (
                isOpen ? <FolderOpen size={15} /> : <Folder size={15} />
              ) : (
                extensionLabel(entry.name)
              )}
            </span>
            <span className="vault-label" title={entry.name}>{entry.name}</span>
            {!entry.isDirectory && (entry.access_count ?? 0) > 0 && (
              <span className="vault-refs" title={`Referenced in ${entry.access_count} chat message(s)`}>
                ·&nbsp;{entry.access_count}&nbsp;ref{entry.access_count === 1 ? '' : 's'}
              </span>
            )}
          </span>

          <span>
            {entry.isDirectory ? (
              <span className="vault-cell">—</span>
            ) : (
              <span
                className={`vault-status vault-status--${state}`}
                title={
                  state === 'failed'
                    ? (doc?.error ?? 'The text of this document could not be extracted.')
                    : state === 'indexed'
                      ? `${doc?.charCount.toLocaleString()} characters extracted — usable in chat`
                      : 'Stored, but its text has not been extracted yet'
                }
              >
                <span className="vault-status__dot" />
                {state === 'unknown' ? 'Unknown' : STATUS_COPY[state]}
              </span>
            )}
          </span>

          <span className="vault-cell vault-cell--num">
            {entry.isDirectory ? '—' : formatSize(entry.size)}
          </span>
          <span className="vault-cell">{formatDate(entry.modified)}</span>

          <span className="vault-actions">
            {!entry.isDirectory && (
              <button
                type="button"
                className="ui-iconbtn ui-iconbtn--sm"
                onClick={e => {
                  e.stopPropagation();
                  void openPreview(entry);
                }}
                title="Preview document"
              >
                <Eye size={13} />
              </button>
            )}
            <button
              type="button"
              className="ui-iconbtn ui-iconbtn--sm"
              onClick={e => {
                e.stopPropagation();
                setContextMenu({ x: e.clientX, y: e.clientY, entry });
              }}
              title="More actions"
            >
              <MoreVertical size={13} />
            </button>
          </span>
        </div>
        {entry.isDirectory && isOpen && entry.children?.map(c => renderRow(c, depth + 1))}
      </React.Fragment>
    );
  };

  const selectionMode = selected.size > 0;

  return (
    <div
      className="vault"
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
    >
      <div className={`vault-dropzone${isDragging ? ' is-active' : ''}`} aria-hidden={!isDragging}>
        <div className="vault-dropzone__scrim" />
        <div className="vault-dropzone__label">
          <Upload size={28} strokeWidth={1.5} style={{ color: 'var(--accent-fg)' }} />
          <span className="vault-dropzone__title">Drop to add to your Vault</span>
          <span className="vault-dropzone__hint">{SUPPORTED_LIST_LABEL}</span>
        </div>
      </div>

      <input
        type="file"
        ref={fileInputRef}
        multiple
        accept={FILE_INPUT_ACCEPT}
        onChange={handleInputChange}
        style={{ display: 'none' }}
      />
      {/* Folder picker. Deliberately NO accept filter: browsers ignore `accept`
          on a directory picker, so advertising one would mislead. Unsupported
          files are filtered — and reported — by uploadBatch instead. */}
      <input
        type="file"
        ref={folderInputRef}
        multiple
        {...({ webkitdirectory: '', directory: '' } as React.InputHTMLAttributes<HTMLInputElement>)}
        onChange={handleInputChange}
        style={{ display: 'none' }}
      />

      <div className="vault-toolbar">
        {selectionMode ? (
          <>
            <span className="vault-selection-count">{selected.size} selected</span>
            <button type="button" className="ui-btn ui-btn--sm ui-btn--ghost" onClick={() => setSelected(new Set())}>
              <X size={13} /> Clear
            </button>
            <span style={{ marginLeft: 'auto' }} />
            <button
              type="button"
              className="ui-btn ui-btn--sm ui-btn--danger"
              onClick={() => void deleteEntries([...selected])}
            >
              <Trash2 size={13} /> Delete selected
            </button>
          </>
        ) : (
          <>
            <label className="vault-search">
              <Search size={13} />
              <input
                className="ui-input"
                type="search"
                value={query}
                onChange={e => setQuery(e.target.value)}
                placeholder="Search your Vault…"
                aria-label="Search documents"
              />
            </label>
            <span style={{ marginLeft: 'auto' }} />
            <button
              type="button"
              className="ui-btn ui-btn--sm ui-btn--ghost"
              onClick={() => {
                setNewFolderParentId(null);
                setShowNewFolder(true);
              }}
            >
              <FolderPlus size={14} /> New folder
            </button>
            <button type="button" className="ui-btn ui-btn--sm ui-btn--secondary" onClick={() => triggerUpload(null, true)}>
              <Upload size={14} /> Folder
            </button>
            <button type="button" className="ui-btn ui-btn--sm ui-btn--primary" onClick={() => triggerUpload(null, false)}>
              <Upload size={14} /> Add documents
            </button>
          </>
        )}
      </div>

      {showNewFolder && (
        <div className="vault-newfolder">
          <input
            className="ui-input"
            type="text"
            value={newFolderName}
            onChange={e => setNewFolderName(e.target.value)}
            onKeyDown={e => {
              if (e.key === 'Enter') void createFolder();
              if (e.key === 'Escape') setShowNewFolder(false);
            }}
            placeholder="Folder name"
            autoFocus
          />
          {newFolderParentId != null && <span className="vault-newfolder__scope">inside the selected folder</span>}
          <span style={{ marginLeft: 'auto' }} />
          <button type="button" className="ui-btn ui-btn--sm ui-btn--ghost" onClick={() => setShowNewFolder(false)}>
            Cancel
          </button>
          <button
            type="button"
            className="ui-btn ui-btn--sm ui-btn--primary"
            onClick={() => void createFolder()}
            disabled={!newFolderName.trim()}
          >
            Create
          </button>
        </div>
      )}

      <div className="vault-body">
        {isLoading ? (
          <div className="vault-table">
            {Array.from({ length: 6 }).map((_, i) => (
              <div key={i} className="ui-skeleton vault-skeleton-row" />
            ))}
          </div>
        ) : totalFiles === 0 && files.length === 0 ? (
          <div className="vault-empty">
            <Library size={34} strokeWidth={1.25} className="vault-empty__icon" />
            <span className="vault-empty__title">Your Vault is empty</span>
            <span className="vault-empty__hint">
              Documents added here are read once and stay available to every conversation — reference
              them by typing <code>@filename</code> in any chat.
            </span>
            <span className="vault-empty__actions">
              <button type="button" className="ui-btn ui-btn--secondary" onClick={() => triggerUpload(null, true)}>
                <Upload size={14} /> Add a folder
              </button>
              <button type="button" className="ui-btn ui-btn--primary" onClick={() => triggerUpload(null, false)}>
                <Upload size={14} /> Add documents
              </button>
            </span>
          </div>
        ) : visibleFiles.length === 0 ? (
          <div className="vault-empty">
            <Search size={30} strokeWidth={1.25} className="vault-empty__icon" />
            <span className="vault-empty__title">No matches</span>
            <span className="vault-empty__hint">
              Nothing in your Vault matches “{query}”. Search covers file names.
            </span>
          </div>
        ) : (
          <div className="vault-table" role="table">
            <div className="vault-row vault-row--head" role="row">
              <span>Document</span>
              <span>Status</span>
              <span className="vault-cell--num">Size</span>
              <span>Added</span>
              <span />
            </div>
            {visibleFiles.map(entry => renderRow(entry))}
          </div>
        )}
      </div>

      {/* Footer summary. "N indexed of M" is the honest headline for this
          surface: it is the count that determines what the model can actually
          use, and it was not shown anywhere before. */}
      {!isLoading && totalFiles > 0 && (
        <div className="vault-header" style={{ borderBottom: 'none', borderTop: '1px solid var(--border-subtle)', padding: 'var(--sp-3) var(--sp-6)' }}>
          <span className="vault-subtitle">
            {indexedCount} of {totalFiles} document{totalFiles === 1 ? '' : 's'} indexed and usable in chat
            {indexedCount < totalFiles && ' — hover a status to see why'}
          </span>
        </div>
      )}

      {contextMenu && (
        <div className="vault-menu" style={{ top: contextMenu.y, left: contextMenu.x }} role="menu">
          {!contextMenu.entry.isDirectory && (
            <button type="button" className="vault-menu__item" onClick={() => void openPreview(contextMenu.entry)}>
              <Eye size={14} /> Preview
            </button>
          )}
          {contextMenu.entry.isDirectory && (
            <>
              <button
                type="button"
                className="vault-menu__item"
                onClick={() => {
                  setNewFolderParentId(contextMenu.entry.id);
                  setShowNewFolder(true);
                }}
              >
                <FolderPlus size={14} /> New folder inside
              </button>
              <button type="button" className="vault-menu__item" onClick={() => triggerUpload(contextMenu.entry.id, false)}>
                <Upload size={14} /> Add documents here
              </button>
              <button type="button" className="vault-menu__item" onClick={() => triggerUpload(contextMenu.entry.id, true)}>
                <Upload size={14} /> Add a folder here
              </button>
            </>
          )}
          <div className="vault-menu__sep" />
          <button
            type="button"
            className="vault-menu__item vault-menu__item--danger"
            onClick={() => void deleteEntries([contextMenu.entry.id])}
          >
            <Trash2 size={14} /> Delete
          </button>
        </div>
      )}

      {previewTarget && (
        <DocumentViewer
          documentId={previewTarget.documentId}
          filename={previewTarget.name}
          onClose={() => setPreviewTarget(null)}
        />
      )}
    </div>
  );
};

const LocalFilesPanel: React.FC<{ isOpen: boolean; onClose: () => void }> = ({ isOpen, onClose }) => {
  if (!isOpen) return null;

  return (
    <div className="vault">
      <div className="vault-header">
        <button type="button" className="ui-iconbtn" onClick={onClose} title="Back" aria-label="Back">
          <ArrowLeft size={17} />
        </button>
        <div className="vault-header__titles">
          <h1 className="vault-title">
            <Library size={19} strokeWidth={1.5} />
            Vault
          </h1>
          <p className="vault-subtitle">
            Read once, available to every conversation. Reference with <code>@filename</code>.
          </p>
        </div>
      </div>
      <VaultBody />
    </div>
  );
};

export default LocalFilesPanel;
export { LocalFilesPanel };
