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

import React, { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { getApiBaseSync } from '../api/backendUrl';
import { fetchWithNetworkRetry } from '../api/fetchWithTimeout';
import { Trash2, Upload, FolderOpen, ChevronRight, CornerLeftUp, LayoutGrid, List, MoreVertical, Search, X, Check,  } from 'lucide-react';
/* Loaded on demand, not with this panel.
   DocumentViewer statically imports pdfjs-dist and mammoth to render PDF and
   Word previews, which together are the single largest thing in the app's
   bundle. It is rendered only when `previewTarget` is set -- i.e. only once
   someone actually opens a document -- so there is no reason for the Vault to
   carry either library just to list files. */
const DocumentViewer = lazy(() =>
  import('./DocumentViewer').then((m) => ({ default: m.DocumentViewer })),
);
import type { OpenSourceRequest } from '../utils/openSource';
import { FileBrandTile, FolderGlyph, FILE_ICON } from './FileBrandIcon';
import UploadMenu from './workspace/UploadMenu';
import { useNameDialog } from './workspace/NameDialog';
import { pickFolders } from '../utils/pickFolders';
import SelectAll from './workspace/SelectAll';
import './workspace/workspace.css';
import { FILE_INPUT_ACCEPT, SUPPORTED_LIST_LABEL, partitionSupported } from '../supportedFormats';
import { exceedsUploadBudget, totalBytesOf, uploadTooLargeMessage } from '../uploadLimits';
import { useNotificationHelpers } from '../contexts/NotificationContext';
import { useAssistant, useAssistantPage } from '../contexts/AssistantContext';
import AssistantMark from './assistant/AssistantMark';
import './VaultPanel.css';
import { useConfirm } from '../contexts/confirm';
import { EmptyPlace, TypeFilter, fileTypeGroup } from './workspace/FileBrowser';

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
  if (!bytes) return '-';
  const k = 1024;
  const units = ['B', 'KB', 'MB', 'GB'];
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(k)), units.length - 1);
  return `${parseFloat((bytes / k ** i).toFixed(1))} ${units[i]}`;
};

const formatDate = (iso: string): string => {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '-';
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
  if (!ext || ext === name.toLowerCase()) return '-';
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

/** Where the Vault's tiles-or-list choice is kept, as in the Workspace. */
const VAULT_VIEW_KEY = 'oca.vault.view';

interface VaultFocusProps {
  /** A document to open in this screen's viewer (a source opened from chat). */
  focusDocument?: OpenSourceRequest | null;
  onFocusHandled?: () => void;
}

const VaultBody: React.FC<VaultFocusProps> = ({ focusDocument, onFocusHandled }) => {
  const [files, setFiles] = useState<FileEntry[]>([]);
  const [docsByLocalFile, setDocsByLocalFile] = useState<Map<number, DocumentInfo>>(new Map());
  const [expandedDirs, setExpandedDirs] = useState<Set<number>>(new Set());
  /* Tiles or a list, remembered between visits — the same choice, the same
     storage idiom and the same control as the Workspace and the Library. */
  // List view only: tiles and the grid/list switch were removed.
  const view = 'list' as 'grid' | 'list';
  /* Tiles cannot nest, so the grid walks INTO a folder where the list expands
     it in place. Its own state: coming back out to the list must not collapse
     the folders that were open there. */
  const [gridPath, setGridPath] = useState<number[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [query, setQuery] = useState('');
  const [isLoading, setIsLoading] = useState(true);
  const [previewTarget, setPreviewTarget] = useState<{ documentId: number; name: string; highlight?: string; page?: number | null; entryId?: number } | null>(null);
  // A source opened from chat: show it in this screen's viewer, at the passage.
  useEffect(() => {
    if (!focusDocument) return;
    setPreviewTarget({ documentId: focusDocument.documentId, name: focusDocument.filename, highlight: focusDocument.highlight, page: focusDocument.page });
    onFocusHandled?.();
  }, [focusDocument, onFocusHandled]);
  const [isDragging, setIsDragging] = useState(false);

  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);
  const [uploadParentId, setUploadParentId] = useState<number | null>(null);

  const { showSuccess, showError, showWarning } = useNotificationHelpers();
  const confirm = useConfirm();

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
        showError("Couldn't load Vault", e instanceof Error ? e.message : String(e));
        return [] as FileEntry[];
      });

    const docsPromise = fetch(`${apiBase}/documents`)
      .then(r => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((payload: Record<string, unknown>[] | { documents?: Record<string, unknown>[] }) =>
        indexDocuments(Array.isArray(payload) ? payload : (payload.documents ?? [])),
      )
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

  /* ── Search ───────────────────────────────────────────────────────────────
     Filters the tree while KEEPING the ancestors of a match, so a hit three
     folders deep is still shown in context rather than as an orphan row. */
  /* File-type filter (PDF, Word, Excel…): empty means every type. */
  const [types, setTypes] = useState<string[]>([]);
  const typeCounts = useMemo(() => {
    const counts: Record<string, number> = {};
    const walk = (entries: FileEntry[]) =>
      entries.forEach(e => {
        if (e.isDirectory) walk(e.children ?? []);
        else {
          const g = fileTypeGroup(e.name);
          counts[g] = (counts[g] ?? 0) + 1;
        }
      });
    walk(files);
    return counts;
  }, [files]);

  const visibleFiles = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q && types.length === 0) return files;
    const fileMatches = (e: FileEntry) =>
      (!q || e.name.toLowerCase().includes(q)) && (types.length === 0 || types.includes(fileTypeGroup(e.name)));
    const prune = (entries: FileEntry[]): FileEntry[] =>
      entries.flatMap(e => {
        if (!e.isDirectory) return fileMatches(e) ? [e] : [];
        const children = prune(e.children ?? []);
        if (children.length > 0) return [{ ...e, children }];
        // An empty match on the folder's own name counts only while no type is picked.
        return q && types.length === 0 && e.name.toLowerCase().includes(q) ? [{ ...e, children: [] }] : [];
      });
    return prune(files);
  }, [files, query, types]);

  /** While searching, every surviving folder is expanded — a filtered tree
   *  with collapsed matches shows the user nothing. */
  const effectiveExpanded = useMemo(() => {
    if (!query.trim() && types.length === 0) return expandedDirs;
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
  }, [query, types, expandedDirs, visibleFiles]);


  /** The folder the grid is inside, resolved fresh so a refresh cannot strand it. */
  const gridFolder = useMemo(() => {
    let level = visibleFiles;
    let found: FileEntry | null = null;
    for (const id of gridPath) {
      const next = level.find(e => e.id === id && e.isDirectory);
      if (!next) return null;
      found = next;
      level = next.children ?? [];
    }
    return found;
  }, [visibleFiles, gridPath]);

  /* A folder the path pointed at can be deleted, renamed away or filtered out
     by a search. Rather than showing an empty grid, step back to the root —
     during render, so the stranded state is never painted for a frame first. */
  if (gridPath.length > 0 && gridFolder === null) setGridPath([]);

  const gridEntries = useMemo(() => {
    const level = gridFolder ? (gridFolder.children ?? []) : visibleFiles;
    return [...level].sort((a, b) =>
      a.isDirectory === b.isDirectory
        ? a.name.localeCompare(b.name, undefined, { sensitivity: 'base' })
        : a.isDirectory
          ? -1
          : 1,
    );
  }, [gridFolder, visibleFiles]);

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

      // A picked folder arrives as files carrying their path inside it
      // ("Contracts/2024/nda.pdf"). The upload endpoint stores bare names, so
      // the folders are created first and each file is sent to its own folder;
      // otherwise the whole tree would land flat in one place.
      const apiBase = getApiBaseSync();
      const dirOf = (f: File) => {
        const rel = (f as File & { webkitRelativePath?: string }).webkitRelativePath || '';
        const cut = rel.lastIndexOf('/');
        return cut > 0 ? rel.slice(0, cut) : '';
      };
      const groups = new Map<string, File[]>();
      for (const f of accepted) {
        const dir = dirOf(f);
        groups.set(dir, [...(groups.get(dir) ?? []), f]);
      }

      try {
        const folderIds = new Map<string, number | null>([['', parentId]]);
        const folderFor = async (dir: string): Promise<number | null> => {
          if (folderIds.has(dir)) return folderIds.get(dir)!;
          const cut = dir.lastIndexOf('/');
          const parent = await folderFor(cut === -1 ? '' : dir.slice(0, cut));
          const r = await fetchWithNetworkRetry(`${apiBase}/files/folder`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: dir.slice(cut + 1), parent_id: parent }),
          });
          if (!r.ok) throw new Error((await r.text()) || `Folder "${dir}" could not be created (${r.status}).`);
          const created = (await r.json()) as { id: number };
          folderIds.set(dir, created.id);
          return created.id;
        };

        for (const [dir, groupFiles] of [...groups.entries()].sort(([a], [b]) => a.localeCompare(b))) {
          const target = await folderFor(dir);
          const form = new FormData();
          groupFiles.forEach(f => form.append('files', f, f.name));
          const url = target != null ? `${apiBase}/files/upload?parent_id=${target}` : `${apiBase}/files/upload`;
          // Network-retrying fetch: survives WebView network suspension (sleep/
          // lid-close). Safe to repeat — uploads are content-hash-deduplicated.
          const response = await fetchWithNetworkRetry(url, { method: 'POST', body: form });
          if (!response.ok) {
            const text = await response.text();
            showError("Couldn't upload", text || `HTTP ${response.status}`);
            refresh();
            return;
          }
        }
        if (parentId != null) setExpandedDirs(prev => new Set(prev).add(parentId));
        const skipped =
          rejected.length > 0
            ? ` Skipped ${rejected.length} unsupported: ${rejected.map(f => f.name).join(', ')}.`
            : '';
        showSuccess(
          accepted.length === 1 ? 'Document added' : `${accepted.length} documents added`,
          `Indexing runs in the background. The status column updates when it finishes.${skipped}`,
        );
        refresh();
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        showError(
          'Upload failed',
          /Failed to fetch|NetworkError/.test(message)
            ? "Counsel's local service didn't respond. Try again in a moment."
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
    if (folder) {
      // Several folders at once through the app's dialog; the browser's own
      // folder input (one folder only) is the fallback outside the app shell.
      void (async () => {
        try {
          const files = await pickFolders();
          if (files === null) {
            setUploadParentId(parentId);
            folderInputRef.current?.click();
            return;
          }
          if (files.length > 0) await uploadBatch(files, parentId);
        } catch (e) {
          showError("Couldn't read folders", e instanceof Error ? e.message : String(e));
        }
      })();
      return;
    }
    setUploadParentId(parentId);
    fileInputRef.current?.click();
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
  const [nameDialog, askName] = useNameDialog();

  const createFolder = async (parentId: number | null) => {
    const name = await askName({ title: 'Create Folder', initial: 'New folder' });
    if (!name) return;
    try {
      const r = await fetch(`${getApiBaseSync()}/files/folder`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name, parent_id: parentId }),
      });
      if (!r.ok) throw new Error(await r.text());
      if (parentId != null) setExpandedDirs(prev => new Set(prev).add(parentId));
      refresh();
    } catch (e) {
      showError("Couldn't create folder", e instanceof Error ? e.message : String(e));
    }
  };

  const deleteEntries = async (ids: number[]) => {
    if (ids.length === 0) return;
    const plural = ids.length === 1 ? 'this item' : `these ${ids.length} items`;
    const ok = await confirm({
      title: ids.length === 1 ? 'Delete from Vault?' : `Delete ${ids.length} items from Vault?`,
      message: `Delete ${plural}? Folders are removed with all their contents. The files will no longer be available to chats or document search. This cannot be undone.`,
      confirmLabel: 'Delete',
    });
    if (!ok) return;

    const results = await Promise.allSettled(
      ids.map(id => fetch(`${getApiBaseSync()}/files/${id}`, { method: 'DELETE' })),
    );
    const failed = results.filter(r => r.status === 'rejected' || !r.value.ok).length;
    if (failed > 0) {
      showError(
        "Some items weren't deleted",
        `${failed} of ${ids.length} couldn't be removed. The list has been refreshed.`,
      );
    }
    setSelected(new Set());
    refresh();
  };

  const openPreview = async (entry: FileEntry) => {
    if (entry.isDirectory) return;
    // A second click on the open row folds its preview away.
    if (previewTarget?.entryId === entry.id) {
      setPreviewTarget(null);
      return;
    }
    // Fast path: the joined index already knows this file's document id.
    const known = docsByLocalFile.get(entry.id);
    if (known) {
      setPreviewTarget({ documentId: known.documentId, name: entry.name, entryId: entry.id });
      return;
    }
    // Miss — a file uploaded before the document store existed. The backend
    // backfills a document row on demand for exactly this case.
    try {
      const resp = await fetch(`${getApiBaseSync()}/documents/by-local-file/${entry.id}`);
      if (!resp.ok) throw new Error(await resp.text());
      const doc = await resp.json();
      setPreviewTarget({ documentId: doc.id, name: entry.name, entryId: entry.id });
      refresh();
    } catch (e) {
      showError(`Couldn't preview "${entry.name}"`, e instanceof Error ? e.message : String(e));
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
  const rowMenu = (entry: FileEntry): VaultMenuEntry[] =>
    entry.isDirectory
      ? [
          { label: isOpenDir(entry.id) ? 'Collapse' : 'Expand', onSelect: () => toggleDir(entry.id) },
          { label: 'New folder inside', onSelect: () => void createFolder(entry.id) },
          { label: 'Upload files here', onSelect: () => triggerUpload(entry.id, false) },
          'separator',
          { label: 'Delete', onSelect: () => void deleteEntries([entry.id]) },
        ]
      : [
          { label: 'Preview', onSelect: () => void openPreview(entry) },
          'separator',
          { label: 'Delete', onSelect: () => void deleteEntries([entry.id]) },
        ];

  function isOpenDir(id: number) {
    return effectiveExpanded.has(id);
  }

  /**
   * One tile, for the grid.
   *
   * It carries the same four facts the row does — selected, name, kind and
   * indexing state — because a document that is stored but not readable is
   * exactly as invisible to chat in tiles as it is in the list, and the view
   * you happen to prefer must not decide whether you are told.
   */
  const renderTile = (entry: FileEntry): React.ReactNode => {
    const isSelected = selected.has(entry.id);
    const doc = entry.isDirectory ? undefined : docsByLocalFile.get(entry.id);
    const state: ExtractionState = doc?.state ?? 'pending';
    const ext = extensionLabel(entry.name);

    return (
      <div
        key={entry.id}
        className={`ws-tile vault-tile${isSelected ? ' ws-tile-selected' : ''}`}
      >
        <div className="ws-tile-top">
          <button
            type="button"
            className={`ws-check${isSelected ? ' ws-check-on' : ''}`}
            aria-pressed={isSelected}
            aria-label={`Select ${entry.name}`}
            onClick={() => toggleSelected(entry.id)}
          >
            {isSelected && <Check size={12} strokeWidth={3} />}
          </button>
          <VaultMenu label={entry.name} entries={rowMenu(entry)} />
        </div>
        <button
          type="button"
          className="ws-tile-open"
          onClick={() => {
            if (entry.isDirectory) setGridPath(path => [...path, entry.id]);
            else void openPreview(entry);
          }}
          title={entry.isDirectory ? `Open ${entry.name}` : entry.name}
        >
          {entry.isDirectory ? (
            <span className="ws-tile-icon ws-icon-folder">
              <FolderGlyph size={FILE_ICON.grid} />
            </span>
          ) : (
            <span className="ws-tile-icon ws-tile-icon-file">
              <FileBrandTile format={ext === '-' ? 'file' : ext} size={FILE_ICON.grid} />
            </span>
          )}
          <span className="ws-tile-text">
            <span className="ws-tile-name">{entry.name}</span>
            <span className="ws-tile-meta">
              {entry.isDirectory ? 'Folder' : `${ext === '-' ? 'FILE' : ext.toUpperCase()} · ${formatSize(entry.size)}`}
            </span>
          </span>
        </button>
        {!entry.isDirectory && (
          <span
            className={`ws-chip vault-chip-${state} vault-tile-chip`}
            title={
              state === 'failed'
                ? (doc?.error ?? 'The text of this document could not be extracted.')
                : state === 'indexed'
                  ? `${doc?.charCount.toLocaleString()} characters extracted, usable in chat`
                  : 'Stored, but its text has not been extracted yet'
            }
          >
            <span className="ws-chip-dot" aria-hidden="true" />
            {STATUS_COPY[state]}
          </span>
        )}
      </div>
    );
  };

  const renderRow = (entry: FileEntry, depth = 0): React.ReactNode => {
    const isOpen = effectiveExpanded.has(entry.id);
    const isSelected = selected.has(entry.id);
    const doc = entry.isDirectory ? undefined : docsByLocalFile.get(entry.id);
    const state: ExtractionState = doc?.state ?? 'pending';
    const ext = extensionLabel(entry.name);

    return (
      <React.Fragment key={entry.id}>
        <div
          role="row"
          className={`ws-list-row vault-list-row${isSelected ? ' ws-list-selected' : ''}`}
          onDoubleClick={() => {
            if (entry.isDirectory) toggleDir(entry.id);
            else void openPreview(entry);
          }}
        >
          <span>
            <button
              type="button"
              className={`ws-check${isSelected ? ' ws-check-on' : ''}`}
              aria-pressed={isSelected}
              aria-label={`Select ${entry.name}`}
              onClick={() => toggleSelected(entry.id)}
            >
              {isSelected && <Check size={12} strokeWidth={3} />}
            </button>
          </span>

          <button
            type="button"
            className="ws-list-name"
            style={{ paddingLeft: `${depth * 20}px` }}
            onClick={() => {
              if (entry.isDirectory) toggleDir(entry.id);
              else void openPreview(entry);
            }}
            title={entry.name}
          >
            {entry.isDirectory ? (
              <>
                <ChevronRight
                  size={14}
                  strokeWidth={2}
                  className={`vault-chevron${isOpen ? ' is-open' : ''}`}
                  aria-hidden="true"
                />
                {isOpen ? (
                  <span className="ws-list-icon ws-icon-folder"><FolderGlyph size={FILE_ICON.list} /></span>
                ) : (
                  <span className="ws-list-icon ws-icon-folder"><FolderGlyph size={FILE_ICON.list} /></span>
                )}
              </>
            ) : (
              <>
                <span className="vault-chevron-space" aria-hidden="true" />
                <span className="ws-list-icon ws-list-icon-file">
                  <FileBrandTile format={ext === '-' ? 'file' : ext} size={FILE_ICON.list} />
                </span>
              </>
            )}
            <span className="ws-list-title">{entry.name}</span>
            {!entry.isDirectory && (entry.access_count ?? 0) > 0 && (
              <span className="vault-refs" title={`Referenced in ${entry.access_count} chat message(s)`}>
                {entry.access_count}&nbsp;ref{entry.access_count === 1 ? '' : 's'}
              </span>
            )}
          </button>

          <span>
            {entry.isDirectory ? (
              <span className="ws-list-cell vault-muted">-</span>
            ) : (
              <span
                className={`ws-chip vault-chip-${state}`}
                title={
                  state === 'failed'
                    ? (doc?.error ?? 'The text of this document could not be extracted.')
                    : state === 'indexed'
                      ? `${doc?.charCount.toLocaleString()} characters extracted, usable in chat`
                      : 'Stored, but its text has not been extracted yet'
                }
              >
                <span className="ws-chip-dot" aria-hidden="true" />
                {STATUS_COPY[state]}
              </span>
            )}
          </span>

          <span className="ws-list-cell">{entry.isDirectory ? 'Folder' : formatSize(entry.size)}</span>
          <span className="ws-list-cell">{formatDate(entry.modified)}</span>

          <span className="ws-list-actions">
            <VaultMenu label={entry.name} entries={rowMenu(entry)} />
          </span>
        </div>
        {/* The preview opens right under its row, as wide as the list; the
            rest of the list continues below it. */}
        {!entry.isDirectory && previewTarget?.entryId === entry.id && (
          <Suspense fallback={null}>
            <DocumentViewer
              inline
              documentId={previewTarget.documentId}
              filename={previewTarget.name}
              highlight={previewTarget.highlight}
              page={previewTarget.page}
              onClose={() => setPreviewTarget(null)}
            />
          </Suspense>
        )}
        {entry.isDirectory && isOpen && entry.children?.map(c => renderRow(c, depth + 1))}
      </React.Fragment>
    );
  };

  const selectionMode = selected.size > 0;

  /* ── AI Assistant ─────────────────────────────────────────────────────────
     The assistant reads what the user is looking at here: the open preview,
     otherwise the files ticked in the list. Folders contribute their name only.
     Text comes from the extraction the Vault already performs, so a file that
     reads "Not indexed" or "Unreadable" is named but not quoted. */
  const { ask } = useAssistant();
  const entriesById = useMemo(() => {
    const map = new Map<number, FileEntry>();
    const walk = (list: FileEntry[]) => list.forEach(e => { map.set(e.id, e); walk(e.children ?? []); });
    walk(files);
    return map;
  }, [files]);

  const assistantTargets = useMemo(() => {
    if (previewTarget) return [{ name: previewTarget.name, documentId: previewTarget.documentId as number | null }];
    return [...selected]
      .map(id => entriesById.get(id))
      .filter((e): e is FileEntry => !!e)
      .map(e => ({ name: e.isDirectory ? `${e.name}/` : e.name, documentId: e.isDirectory ? null : docsByLocalFile.get(e.id)?.documentId ?? null }));
  }, [previewTarget, selected, entriesById, docsByLocalFile]);

  const [assistantTexts, setAssistantTexts] = useState<{ key: string; text: string } | null>(null);
  const targetsKey = assistantTargets.map(t => `${t.name}:${t.documentId ?? ''}`).join('|');
  useEffect(() => {
    if (!targetsKey) return;
    let live = true;
    // At most three documents are quoted; a long selection is listed by name.
    const readable = assistantTargets.filter(t => t.documentId != null).slice(0, 3);
    const perDoc = Math.floor(30_000 / Math.max(1, readable.length));
    void Promise.all(
      readable.map(async t => {
        try {
          const r = await fetch(`${getApiBaseSync()}/documents/${t.documentId}`);
          if (!r.ok) return '';
          const meta = await r.json();
          const text = String(meta.extracted_text ?? '');
          return text ? `=== ${t.name} ===\n${text.slice(0, perDoc)}` : '';
        } catch {
          return '';
        }
      }),
    ).then(parts => live && setAssistantTexts({ key: targetsKey, text: parts.filter(Boolean).join('\n\n') }));
    return () => {
      live = false;
    };
    // assistantTargets is fully described by targetsKey.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [targetsKey]);

  const assistantBody = assistantTexts?.key === targetsKey ? assistantTexts.text : '';
  const textsReady = assistantTexts?.key === targetsKey;
  // A question asked before the files' text has loaded waits for it.
  const [queuedAsk, setQueuedAsk] = useState<string | null>(null);
  useEffect(() => {
    if (!queuedAsk || !textsReady) return;
    const q = queuedAsk;
    setQueuedAsk(null);
    // After the page registration above has reached the assistant.
    window.setTimeout(() => ask(q), 60);
  }, [queuedAsk, textsReady, ask]);
  const names = assistantTargets.map(t => t.name);
  useAssistantPage(
    assistantTargets.length
      ? {
          label: 'Vault',
          title: names.length === 1 ? names[0] : `${names.length} selected files`,
          format: names.length > 1 ? names.slice(0, 8).join(', ') + (names.length > 8 ? '…' : '') : undefined,
          text: assistantBody || undefined,
          suggestions:
            names.length > 1
              ? ['Compare these documents', 'What do these files have in common?', 'Build an index of these documents']
              : ['Summarise this document', 'List the key dates and parties', 'What risks should I flag?'],
        }
      : null,
  );

  // Everything the user can currently see: top-level items plus the contents
  // of expanded folders. "Select all" acts on exactly these.
  const visibleIds = useMemo(() => {
    // In tiles, "on screen" is exactly the folder being looked at — nothing
    // below it is drawn, so nothing below it may be selected by Select all.
    if (view === 'grid') return gridEntries.map(e => e.id);
    const ids: number[] = [];
    const walk = (entries: FileEntry[]) =>
      entries.forEach(e => {
        ids.push(e.id);
        if (e.isDirectory && effectiveExpanded.has(e.id)) walk(e.children ?? []);
      });
    walk(visibleFiles);
    return ids;
  }, [view, gridEntries, visibleFiles, effectiveExpanded]);
  const selectedVisible = visibleIds.filter(id => selected.has(id)).length;
  const selectAllVisible = () => setSelected(new Set(visibleIds));
  const clearSelection = () => setSelected(new Set());

  return (
    <div
      className="vault"
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
    >
      {nameDialog}
      <div className={`vault-dropzone${isDragging ? ' is-active' : ''}`} aria-hidden={!isDragging}>
        <div className="vault-dropzone__scrim" />
        <div className="vault-dropzone__label">
          <Upload size={28} strokeWidth={1.5} />
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

      <div className="ws-landing vault-landing">
        <header className="ws-landing-head vault-head">
          {/* No glyph: page titles across the app are the word alone. */}
          <h1 className="ws-landing-title vault-title-row">Vault</h1>
        </header>

        <div className="ws-searchbar">
          <Search size={15} strokeWidth={2.25} className="ws-search-icon" />
          <input
            className="ws-search-input"
            type="search"
            value={query}
            onChange={e => setQuery(e.target.value)}
            onKeyDown={e => {
              if (e.key === 'Escape') setQuery('')
            }}
            placeholder="Search vault"
            aria-label="Search vault"
          />
          {query && (
            <button type="button" className="ws-close" onClick={() => setQuery('')} aria-label="Clear search">
              <X size={14} strokeWidth={1.75} />
            </button>
          )}
        </div>

        <section className="ws-files-box" aria-label="Vault documents">
          <div className="ws-files-toolbar">
            <div className="ws-filters">
              {files.length > 0 && (
                <SelectAll
                  total={visibleIds.length}
                  selected={selectedVisible}
                  onSelectAll={selectAllVisible}
                  onClear={clearSelection}
                />
              )}
              {files.length > 0 && <TypeFilter value={types} onChange={setTypes} counts={typeCounts} />}
              {selectionMode ? (
                <>
                  <span className="vault-count">{selected.size} selected</span>
                  <button
                    type="button"
                    className="ws-filter"
                    onClick={() => setSelected(new Set())}
                  >
                    <X size={13} strokeWidth={2.25} /> Clear
                  </button>
                  <button
                    type="button"
                    className="ws-filter"
                    onClick={() => setQueuedAsk(selected.size === 1 ? 'Summarise this document.' : 'Summarise these documents and how they relate.')}
                    title="Open LARA with the selected files"
                  >
                    <AssistantMark size={12} /> Ask LARA
                  </button>
                  <button
                    type="button"
                    className="ws-filter"
                    onClick={() => void deleteEntries([...selected])}
                  >
                    <Trash2 size={13} strokeWidth={2} /> Delete selected
                  </button>
                </>
              ) : (
                !isLoading &&
                totalFiles > 0 && (
                  <span className="vault-count">
                    {indexedCount} of {totalFiles} document{totalFiles === 1 ? '' : 's'} indexed and usable in chat
                  </span>
                )
              )}
            </div>
            <div className="ws-toolbar-end">
              <div className="ws-toolbar-actions ws-library-actions">
                <UploadMenu
                  className="ws-upload-btn"
                  onAddFiles={() => triggerUpload(null, false)}
                  onAddFolder={() => triggerUpload(null, true)}
                  onCreateFolder={() => void createFolder(null)}
                />
              </div>

            </div>
          </div>

          {isLoading ? (
            <div className="vault-state">Loading your Vault…</div>
          ) : files.length === 0 ? (
            // Same empty state as the Workspace and Library.
            <EmptyPlace place="Vault" onImport={() => triggerUpload(null)} />
          ) : visibleFiles.length === 0 ? (
            <div className="ws-empty">
              <span className="ws-empty__title">No matches</span>
              <span className="ws-empty__text">
                {query.trim()
                  ? `Nothing in your Vault matches “${query}”${types.length ? ' of the chosen types' : ''}.`
                  : 'No files of the chosen types.'}
              </span>
            </div>
          ) : view === 'grid' ? (
            <div className="ws-grid-items ws-tiles vault-tiles ws-mode-in" key={`g${gridPath.join('-')}`}>
              {gridPath.length > 0 && (
                <button
                  type="button"
                  className="ws-tile ws-tile-up"
                  onClick={() => setGridPath(path => path.slice(0, -1))}
                  title="Up one level"
                >
                  <span className="ws-tile-icon">
                    <CornerLeftUp size={20} strokeWidth={1.75} />
                  </span>
                  <span className="ws-tile-text">
                    <span className="ws-tile-name">Up one level</span>
                  </span>
                </button>
              )}
              {gridEntries.map(entry => renderTile(entry))}
            </div>
          ) : (
            <div className="ws-file-list vault-list" role="table" aria-label="Vault documents">
              <div className="ws-list-head" role="row">
                <span role="columnheader">
                  <SelectAll
                    compact
                    total={visibleIds.length}
                    selected={selectedVisible}
                    onSelectAll={selectAllVisible}
                    onClear={clearSelection}
                  />
                </span>
                <span role="columnheader">Name</span>
                <span role="columnheader">Status</span>
                <span role="columnheader">Size</span>
                <span role="columnheader">Added</span>
                <span role="columnheader" />
              </div>
              <div className="ws-list-body">{visibleFiles.map(entry => renderRow(entry))}</div>
            </div>
          )}
        </section>
      </div>

      {previewTarget && previewTarget.entryId === undefined && (
        /* No fallback UI: the viewer opens over the page, and a spinner that
           appears for the one frame the chunk takes to arrive reads as a
           flicker rather than as progress. The page stays put until it is
           ready. */
        <Suspense fallback={null}>
          <DocumentViewer
            documentId={previewTarget.documentId}
            filename={previewTarget.name}
            highlight={previewTarget.highlight}
            page={previewTarget.page}
            onClose={() => setPreviewTarget(null)}
          />
        </Suspense>
      )}
    </div>
  );
};

type VaultMenuEntry = { label: string; onSelect: () => void } | 'separator';

/** The row's ⋮ menu, drawn with the Workspace dropdown styles. */
const VaultMenu: React.FC<{ label: string; entries: VaultMenuEntry[] }> = ({ label, entries }) => {
  const [open, setOpen] = useState(false);
  const [up, setUp] = useState(false);
  const anchor = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!open) return;
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

  return (
    <div className={`ws-item-menu${open ? ' is-open' : ''}`} ref={anchor}>
      <button
        type="button"
        className="ws-dots"
        aria-label={`Actions for ${label}`}
        aria-haspopup="menu"
        aria-expanded={open}
        title="More actions"
        onClick={e => {
          e.stopPropagation();
          if (!open && anchor.current) {
            const trigger = anchor.current.getBoundingClientRect();
            const scroller = anchor.current.closest('.ws-list-body');
            const bottom = scroller ? scroller.getBoundingClientRect().bottom : window.innerHeight;
            setUp(trigger.bottom + 240 > bottom);
          }
          setOpen(o => !o);
        }}
      >
        <MoreVertical size={16} strokeWidth={2} />
      </button>
      {open && (
        <div className={`ws-filter-menu ws-actions-menu${up ? ' is-up' : ''}`} role="menu">
          {entries.map((entry, i) =>
            entry === 'separator' ? (
              <div key={`sep${i}`} className="ws-menu-divider" role="separator" />
            ) : (
              <button
                key={entry.label}
                type="button"
                role="menuitem"
                className="ws-filter-option"
                onClick={() => {
                  setOpen(false);
                  entry.onSelect();
                }}
              >
                {entry.label}
              </button>
            ),
          )}
        </div>
      )}
    </div>
  );
};

const LocalFilesPanel: React.FC<{ isOpen: boolean; onClose: () => void } & VaultFocusProps> = ({ isOpen, focusDocument, onFocusHandled }) => {
  if (!isOpen) return null;
  return <VaultBody focusDocument={focusDocument} onFocusHandled={onFocusHandled} />;
};

export default LocalFilesPanel;
export { LocalFilesPanel };
