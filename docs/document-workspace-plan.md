# Document Workspace (Draft) — End-to-End Implementation Plan

Status: PLAN ONLY — nothing implemented yet.
Date: 2026-08-08 · Revision 2 (integration audit applied)

Decisions taken: all formats editable (incl. PPTX) · drafts are their own store
with versions · AI assistance is a later phase · **documents must not lose
anything** · prefer native engines already shipped (pdfium) for lower compute.

Revision 2 adds §8 (integration gaps found by reading the code — several would
have broken the build mid-phase), §9 (behavioural contracts), and §10
(test strategy). Everything in §8 was **verified against the source**, not
assumed.

---

## 0. The central design decision, stated plainly

Two different kinds of fidelity. Conflating them is how editors lose data:

| | Meaning | Guaranteed? |
|---|---|---|
| **File fidelity** | What is written back to disk | **YES — 100%, by construction** |
| **Display fidelity** | How closely our editor *looks* like Office | No. Best-effort, improves per phase |

Every web editor that imports a file into an editor model and **regenerates** it
destroys what the model doesn't understand: theme fonts, numbering, headers,
footnotes, charts, tracked changes, content controls, custom XML, macros. True
of `docx`, `exceljs`, `pptxgenjs`, and mammoth round-trips alike. Regeneration is
simply the wrong architecture for "must not lose anything".

**Therefore: preserve-and-patch.**

1. Original bytes remain the source of truth, permanently.
2. The editor emits a **typed patch list** against stable addresses
   (`body/p[12]/r[0]`, cell `B7`, `slide3/sp[2]`), never a file.
3. Save = open the original package, mutate **only** the addressed XML nodes,
   repack. Unreferenced parts are copied byte-for-byte.
4. **Invariant, enforced by test:** open → save with zero edits → every ZIP entry
   byte-identical.

Display fidelity is then free to be imperfect without ever risking the document.

---

## 1. Technology selection (researched 2026-08-08, verified against npm & vendor docs)

### 1.1 The pdfium angle

We already ship `pdfium.dll` (Chrome's PDF engine), bound via `pdfium-render 0.8`.
Native C++ rendering costs the user far less than JS/WASM decoding, and the
binary is already paid for in installer size. **PDF rendering moves server-side
to pdfium**, streaming page bitmaps to the UI. The same principle applies
throughout: OOXML work happens in **Rust**, not the browser.

### 1.2 Final stack

| Concern | Choice | License | Why |
|---|---|---|---|
| DOCX/XLSX/PPTX read + **lossless write** | `zip` 1.1 + `quick-xml` 0.37 (**already deps**) | MIT/Apache | Surgical OOXML patching. Zero new backend crates. |
| Spreadsheet value read | `calamine` 0.26 (**already dep**) | MIT | View model only; never used to write. |
| PDF render/text/annotations/save | `pdfium-render` 0.8 + `pdfium.dll` (**already deps**) | MIT/BSD | Native engine, already shipped, lowest compute. |
| Word-like editor | **TipTap v3.29** | MIT | Product choice. ProseMirror-based; designed for streaming AI edits later. |
| Spreadsheet grid | **`@revolist/revogrid` 4.25** | MIT | Virtualised, framework-agnostic, actively maintained (Aug 2026). |
| Formula evaluation | **`@formulajs/formulajs` 4.6** + our dependency graph | MIT | 500+ Excel-compatible functions, maintained. |
| Slide rendering | **Custom DrawingML → SVG** (ours) | — | No library round-trips PPTX losslessly. |
| Plain text | TipTap plain-text mode | MIT | Avoids a fifth editor dependency. |

### 1.3 Rejected — with reasons, so these are not revisited

| Rejected | Reason |
|---|---|
| **Univer** | XLSX import/export **requires the Univer server** + commercial `@univerjs-pro/exchange-client`. Verified in their docs. Fatal offline. |
| **HyperFormula** | **GPL-3.0-only** on npm — incompatible with a closed-source commercial product. |
| **Handsontable** | Commercial licence, not free for commercial use. |
| **SheetJS (`xlsx`)** | npm frozen at 0.18.5 (2022), known prototype-pollution CVEs. |
| **`exceljs` for writing** | Regenerates the workbook — drops charts, pivots, macros, unknown parts. |
| **`pptxgenjs`** | Generation-only; cannot round-trip. |
| **`docx` (npm) for export** | Builds documents from scratch. Stays only for the existing transcript export. |
| **LibreOffice headless** | Only true 100%-fidelity renderer, but ~400 MB + separate process. Future "render exactly like Office" option, not v1. |

---

## 2. Scope per format — honest

| Format | Editing in v1 | File fidelity |
|---|---|---|
| **DOCX** | Text, character formatting, paragraph styles, lists, tables (cell text, add/remove rows), insert/delete paragraphs | **Lossless** |
| **XLSX** | Cell values, formulas, number formats, cell styling, insert/delete rows & columns, multi-sheet | **Lossless** |
| **PPTX** | Text in existing shapes/placeholders, shape position/size, delete shapes, reorder/duplicate/delete slides | **Lossless** |
| **PDF** | **Annotations** (highlight, note, free text, ink, strikeout), form filling, page ops (rotate/reorder/delete/extract) | **Lossless** |
| **TXT** | Full text editing | Lossless incl. encoding/BOM/line endings (§9.6) |

**PDF text reflow is NOT offered.** PDF has no paragraph model — text is
positioned glyph runs. Reflow is unsolved outside commercial engines; an offline
approximation would corrupt layout. Annotate / fill / reorganise is what legal
PDF review needs. Converting PDF → DOCX to enable text editing is inherently
lossy and must stay a deliberate, user-initiated action.

**PPTX display is the weakest surface in v1.** Text boxes, placeholders, images,
basic shapes and theme colours render; SmartArt, 3-D, transitions and animations
render approximately or as a labelled placeholder. **Files still lose nothing** —
an unrendered effect is an XML part we never patch.

---

## 3. Backend design

### 3.1 New module

```
crates/offline-intelligence/src/document_workspace/
├── mod.rs                 DraftManager: create/open/patch/version orchestration
├── ooxml/
│   ├── package.rs         ZIP open/repack preserving unchanged entries byte-exact
│   ├── addressing.rs      Stable addresses (w:p index, cell ref, shape id)
│   ├── docx_model.rs      document.xml + styles.xml → DocxViewModel
│   ├── docx_patch.rs      DocxPatch → surgical mutation
│   ├── xlsx_model.rs      sheetN.xml + sharedStrings + styles → SheetViewModel
│   ├── xlsx_patch.rs      XlsxPatch → surgical mutation
│   ├── pptx_model.rs      slideN.xml + theme → SlideViewModel
│   └── pptx_patch.rs      PptxPatch → surgical mutation
├── pdf_workspace.rs       pdfium: page render, text layer, annotation CRUD, save
├── text_workspace.rs      Encoding/BOM/line-ending-preserving plain text
└── view_model.rs          Serde types shared with the frontend
```

**No new crates.**

### 3.2 Database — additive migration `014_drafts.sql`

```sql
CREATE TABLE drafts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  title TEXT NOT NULL,
  format TEXT NOT NULL,               -- docx|xlsx|pptx|pdf|txt
  origin_kind TEXT NOT NULL,          -- vault|upload|blank
  source_document_id INTEGER,         -- documents.id when opened from the Vault
  source_local_file_id INTEGER,
  current_version INTEGER NOT NULL DEFAULT 1,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE draft_versions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  draft_id INTEGER NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
  version_no INTEGER NOT NULL,
  storage_path TEXT NOT NULL,         -- AppData/drafts/{draft_id}/v{n}.{ext}
  byte_size INTEGER NOT NULL,
  patch_json TEXT,
  label TEXT,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE(draft_id, version_no)
);
CREATE INDEX idx_draft_versions_draft ON draft_versions(draft_id);
```

Version 1 is always an exact copy of the source, so the original is recoverable
forever and **the Vault file is never mutated**.

> **Registration is required, not optional:** the file alone does nothing.
> `memory_db/migration.rs` holds an explicit `(14, include_str!("migrations/014_drafts.sql"))`
> entry in its ordered list. Additionally, in-memory test databases take a
> *different* path (`schema::SCHEMA_SQL` + per-store `initialize_schema`) and
> never run migrations — so `DraftsStore::initialize_schema()` must create the
> same tables idempotently, exactly as `DocumentsStore` does. Missing this is
> what previously forced migration 010 to special-case itself.

### 3.3 API — new `api/drafts_api.rs`

| Method | Route | Purpose |
|---|---|---|
| POST | `/drafts` | Create from Vault doc / upload / blank |
| GET | `/drafts` | List |
| GET | `/drafts/:id` | Metadata |
| PATCH | `/drafts/:id` | Rename |
| DELETE | `/drafts/:id` | Delete draft + versions + files |
| GET | `/drafts/:id/content` | Format-specific view model |
| POST | `/drafts/:id/patch` | Apply typed patches → new version |
| GET | `/drafts/:id/raw` | Current bytes (download / Save as) |
| GET | `/drafts/:id/versions` | History |
| POST | `/drafts/:id/versions/:n/restore` | Restore as a **new** version (never destructive) |
| GET | `/drafts/:id/page/:n` | pdfium-rendered page PNG (`?scale=`) — PDF & PPTX |
| POST | `/drafts/:id/publish` | Optional: copy current version back to the Vault |

Localhost-only like everything else; registered in `thread_server.rs`.
**No new ports, no new `.env` keys.**

### 3.4 View-model contracts (Rust ⇄ React)

```ts
// docx
{ blocks: [{ addr: "body/p[12]", kind: "paragraph", style: "Heading1",
             runs: [{ addr: "body/p[12]/r[0]", text, bold, italic, underline, color, size }] }],
  styles: { Heading1: { fontSize, bold, color } } }

// xlsx
{ sheets: [{ name, index, dims: "A1:H240",
             cells: { "B7": { v, f: "=SUM(B1:B6)", fmt: "#,##0.00", style: 4 } },
             merges: ["A1:C1"], colWidths, rowHeights }] }

// pptx
{ slides: [{ addr: "slide3", size: { w, h },
             shapes: [{ addr: "slide3/sp[2]", kind, bbox, runs, imageRef? }] }] }

// pdf
{ pages: [{ index, width, height,
            annotations: [{ id, kind, rects, color, contents, author, created }] }] }
```

**The address is the contract** — it maps a browser edit to one XML node in Rust.

---

## 4. Frontend design

```
components/workspace/
├── DraftWorkspace.tsx      Shell: draft rail, toolbar, editor host, context panel
├── DraftList.tsx           Drafts + New + open-from-Vault picker
├── editors/
│   ├── DocxEditor.tsx      TipTap; nodes carry OOXML addresses
│   ├── XlsxEditor.tsx      revogrid + formula bar + sheet tabs
│   ├── PptxEditor.tsx      Slide rail + SVG canvas + text editing
│   ├── PdfEditor.tsx       pdfium page images + annotation overlay
│   └── TextEditor.tsx      Plain text
├── VersionHistory.tsx      List, preview, restore
├── usePatchQueue.ts        Debounce + batch + retry + version guard
└── workspace.css           Design-token styling, consistent with Settings
```

### 4.1 Editing loop

1. Render from the view model; every node holds its `addr`.
2. An edit produces a typed patch: `{ op: "SetRunText", addr: "body/p[12]/r[0]", text }`.
3. `usePatchQueue` debounces (~800 ms idle) and batches.
4. `POST /drafts/:id/patch` (with `base_version`) → Rust applies surgically.
5. Response returns refreshed view model + new version; UI shows "Saved".

A patch names one node, so edits in different parts can never clobber each other,
and unsupported constructs are never addressed — so they cannot be damaged.

### 4.2 Layout (deliberately not a copy of the reference images)

Three zones, consistent with our language (black rail, grouped cards, Arial):
**left** draft rail (format glyph, unsaved dot) · **centre** paper surface with a
slim per-format toolbar · **right** collapsible context panel (version history
now; the AI surface later, designed in now so it drops in without re-layout).
Controls reuse `settings-btn` / `ui-*` primitives so the workspace doesn't become
a sixth visual style.

---

## 5. Phasing

| Phase | Deliverable | Proves |
|---|---|---|
| **0. Spike** | Round-trip invariant on real fixtures; confirm `pdfium-render 0.8` annotation-creation API; CSP + ACL changes landed | The approach is sound before UI exists |
| **1. Backend core** | migration 014 (+ registration + `initialize_schema`), `DraftManager`, drafts API, storage-governor integration | Drafts exist, version, and are accounted |
| **2. DOCX** | `docx_model`/`docx_patch` + TipTap editor + workspace shell + nav wiring | Hardest editor, end to end |
| **3. XLSX** | `xlsx_model`/`xlsx_patch` + revogrid + formulas | Spreadsheets |
| **4. PDF** | pdfium page rendering + annotation CRUD + overlay | Review workflow |
| **5. PPTX** | `pptx_model`/`pptx_patch` + SVG renderer | Decks |
| **6. Polish** | Version UI, export/Save-as, publish-to-Vault, shortcuts | Product completeness |
| **7. Later** | AI suggestions, redlines, streaming edits | (Deferred by decision) |

Phase 0 is not optional. If the byte-identical invariant cannot be met on a real
fixture, that must surface in days — not after five editors are built.

---

## 6. Collision & compatibility audit

| Area | Impact |
|---|---|
| **Rust crates** | **None added.** `zip`, `quick-xml`, `calamine`, `pdfium-render` present. |
| **npm** | Added: `@tiptap/react`, `@tiptap/pm`, `@tiptap/starter-kit`, `@tiptap/extension-table`, `@revolist/revogrid`, `@formulajs/formulajs`. All MIT, pure JS, no native modules or postinstall binaries, React-18 compatible. No overlap with `pdfjs-dist`, `mammoth`, `docx`, `jspdf`, `dompurify`. |
| **`.env` / ports** | No new keys, no new ports. |
| **Database** | Additive migration 014 only; no existing table altered. |
| **Existing pipelines** | Extraction, Vault, chat, attachments, vision untouched. Drafts *read* the documents store; never write to it. |
| **Vault safety** | Opening from the Vault **copies**. Original modified only by explicit "publish". |
| **Installer size** | +~600 KB gzipped JS. No new native binaries. |
| **pdf.js** | Stays for the lightweight Vault preview; the workspace uses pdfium. Consolidation later is optional. |

---

## 7. Risks

1. Display fidelity ≠ file fidelity (§0).
2. PDF text reflow out of scope (§2).
3. PPTX rendering weakest in v1 (§2).
4. **Formula evaluation is ours.** formulajs supplies functions; the dependency
   graph and recalc order are our code. Excel semantics (iterative calc, array
   spilling, volatile functions) are deep — v1 targets the common set and
   displays the file's cached value for anything it cannot evaluate, rather than
   showing a wrong number.
5. **`pdfium-render` annotation ergonomics unconfirmed.** The crate exposes
   `FPDFAnnot_*` bindings; I could not verify high-level creation for every
   annotation type. Phase 0 resolves it; fallback is the raw bindings.

---

## 8. INTEGRATION GAPS — found by reading the code (Revision 2)

Each item below was verified against source and **would have broken the build**
if left undiscovered.

### 8.1 CSP blocks backend-served images — **would break Phase 4**
`apps/desktop/index.html` currently has:
```
img-src 'self' asset: https://asset.localhost data:;
```
There is **no `http://127.0.0.1:*` and no `blob:`**. Every pdfium-rendered page
(`<img src="http://127.0.0.1:8888/drafts/1/page/2">`) would be silently blocked,
as would `URL.createObjectURL(blob)`. `connect-src` already allows 127.0.0.1, so
`fetch` works — but painting the result does not.
**Required change:** `img-src 'self' asset: https://asset.localhost data: blob: http://127.0.0.1:* ;`
(Do it in Phase 0 so it is never mistaken for a rendering bug.)

### 8.2 Tauri ACL lacks write/save permissions — **would break "Save as"**
`src-tauri/capabilities/default.json` grants `fs:default`, `fs:allow-stat`,
`fs:allow-read-file`, `dialog:default`, `dialog:allow-open`. Exporting a draft to
a user-chosen path needs **`dialog:allow-save`** and **`fs:allow-write-file`**.
This exact class of omission already cost us a debugging cycle on the attachment
flow (ACL denial surfaced only as a caught console error).

### 8.3 Storage governor needs a new component, and it is a chain
`DiskUsage` in `utils/storage_governor.rs` has fixed named fields
(`models_bytes`, `engines_bytes`, `database_bytes`, `vault_bytes`,
`downloads_bytes`, `kv_cache_bytes`, `other_bytes`). Drafts would otherwise land
in `other_bytes` — measured, but invisible and uncategorised.
**Required chain:** add `DRAFTS_DIR` + `drafts_bytes` → `measure()` → the
`/settings/storage` and `/storage/metadata` responses → `api/storageSettings.ts`
types → the Settings UI breakdown.
**Eviction policy:** drafts join the **protected** set (engine, database, active
model, Vault) — never auto-deleted. The single evictable tier is **superseded
draft versions** (oldest first, never the current version), and only after
models. The over-budget message in `run_eviction` must name drafts among the
protected remainder.

### 8.4 pdfium is globally serialised — page rendering must respect it
`utils/extraction_scheduler::pdfium_permit()` is a process-wide mutex; the PDF
extraction lane already forces PDF work to be serial (deliberately —
`pdfium-render`'s own `thread_safe` feature was disabled in favour of our
non-poisoning gate). Workspace page rendering **must take the same permit**, or
two `Pdfium` bindings will exist simultaneously.
**Consequence to design for:** while a 50-page scanned PDF is being OCR'd, page
rendering will block. Mitigations: (a) render lazily, visible page first;
(b) **cache rendered pages** on disk under `AppData/drafts/{id}/.cache/` keyed by
`(version, page, scale)` so scrolling never re-renders; (c) return HTTP 503 with
a structured "engine busy, retry" body rather than hanging the request.

### 8.5 Format policy list is a build-enforced contract
`SUPPORTED_ATTACHMENT_EXTENSIONS` (Rust) is `pdf doc docx xls xlsx ppt pptx txt
png jpg jpeg` and a Rust test **parses `apps/desktop/src/supportedFormats.ts`
and fails the build on divergence**. Consequences:
- **`md` is not supported.** Revision 1 of this plan mentioned Markdown editing —
  that would either bypass the gate or require adding `md` to **both** lists
  deliberately. **Decision: drop Markdown from v1.**
- Legacy `.doc` / `.xls` / `.ppt` are accepted by the picker but **refused loudly**
  by the extractors ("save as .docx"). Draft creation must refuse them the same
  way — never open a legacy binary in an editor that cannot round-trip it.

### 8.6 Draft navigation is currently a dead button
The sidebar "Draft" button is deliberately inert, and `activeView` is typed
`'chat' | 'models' | 'settings' | 'help' | 'localfiles'`. Wiring required:
add `'draft'` to the union, add `onOpenDraft` to `Sidebar`, wire it in `App.tsx`,
add the `case 'draft'` render branch. Small, but it is the difference between a
built feature and a reachable one.

### 8.7 Request size limits
`DefaultBodyLimit::max(50 MB)` is global, and `uploadLimits.ts` derives the
client budget from it. Patch payloads are tiny, but **draft creation from upload
and `publish` carry whole files**. Reuse the existing `exceedsUploadBudget`
helper and the same honest error, rather than inventing a second limit.
`GET /drafts/:id/raw` is a *response* and unaffected by the request limit.

### 8.8 Network resilience for saves
We fixed WebView network suspension (sleep/lid-close) by adding
`fetchWithNetworkRetry`. **Draft saves must use it** — a suspended network
mid-edit must not lose the user's work. Patches are idempotent when guarded by
`base_version` (§9.1), so retrying is safe.

### 8.9 Deleting the source of a draft
Deleting a Vault file whose `source_document_id`/`source_local_file_id` a draft
references must **not** cascade into the draft. The draft owns its own byte copy;
the reference is provenance only. Null the reference and keep the draft (mirrors
how `documents.local_file_id` is handled today).

### 8.10 Encrypted / password-protected packages
An encrypted OOXML file is a valid ZIP whose parts are unreadable. It must fail
**loudly and by name** at draft-creation time ("this file is password-protected;
remove protection and try again"), never open as an empty document.

---

## 9. Behavioural contracts

### 9.1 Optimistic concurrency
Every `POST /drafts/:id/patch` carries `base_version`. If it does not match
`drafts.current_version`, the server returns **409** with the current view model
so the client can rebase. Prevents two workspace tabs (or a retry after a lost
response) from silently double-applying edits.

### 9.2 Version explosion
Debounced autosave would otherwise create hundreds of versions.
**Rule:** patches within a rolling window (default 5 min) *amend* the current
version in place; a new version is cut on explicit save, on format/sheet/slide
switch, on close, or when the window expires. `POST /drafts/:id/versions` with a
label always cuts one. Version 1 (the pristine original) is never amended.

### 9.3 Crash safety
The patch queue persists unsent patches to `localStorage` keyed by draft id, and
replays them on next open after confirming `base_version`. An app killed mid-edit
loses at most the debounce window, and never silently.

### 9.4 Undo / redo
Editor-level undo is local (ProseMirror/grid history) and does **not** hit the
server. Versions are the durable, cross-session undo. These are presented as
different things in the UI: ⌘Z is "undo my typing", version history is "go back
to a saved state".

### 9.5 Drafts and the chat pipeline
When a version is saved, the draft's current bytes are registered in the existing
`documents` store (content-hash dedup applies) so a draft is `@`-referenceable in
chat, searchable via FTS, and usable as document context. **This is what stops
the workspace being an island** — and it reuses the extraction pipeline exactly
as it stands. (Extraction runs on the saved bytes through the normal lanes; no
new extraction code.)

### 9.6 Plain text is not trivially lossless
Round-tripping `.txt` must preserve **encoding** (the extractor uses `chardetng`;
a Windows-1252 file must not silently become UTF-8), **BOM presence**, and **line
endings** (CRLF must stay CRLF). Store all three at open and re-apply at save.

### 9.7 Errors and logging
Structured JSON errors (`{ error, detail, action }`) like `/models/switch`, never
bare status codes. Logging per the house levels: `error!` for a failed save or a
violated invariant, `warn!` for a skipped/unsupported construct, `info!` for
draft/version lifecycle, `debug!` for patch counts and render timings.

### 9.8 Large-file guardrails
Bound what is opened in an editor (initial proposal: 40 MB package / 200 sheets /
500 slides / 2,000 PDF pages) and fail with a named, actionable message rather
than exhausting memory. The existing `/documents/:id/raw` viewer remains available
for files beyond the editing bound.

---

## 10. Test strategy

| Level | Tests |
|---|---|
| **Invariant** | Open → save with no edits → **byte-identical** ZIP entries, for docx/xlsx/pptx fixtures containing images, charts, headers/footers, footnotes, numbering, custom XML, tracked changes. |
| **Surgical** | One text edit changes **exactly one** part; `styles.xml`, theme, media, rels byte-identical. |
| **Fixtures** | **Generated programmatically** in-repo (never a user's real document — the same rule that kept real client text out of the document-memory tests). |
| **Store** | Version chain, restore-as-new-version, cascade delete, FK enforcement with `PRAGMA foreign_keys=ON` (in-memory test pools must set it — a real gap found before). |
| **API** | Drive real HTTP through `build_compatible_router()` + `tower::ServiceExt::oneshot`, as `stream_api`'s attach tests already do. Includes the 409 stale-version path. |
| **Governor** | Drafts measured under `drafts_bytes`; drafts never evicted; superseded versions evicted before models are protected… and never before models are exhausted. |
| **Router** | `router_builds_without_route_conflicts` must cover the new `/drafts/*` paths (12 new routes — the only test that would catch an axum conflict panic). |
| **Frontend** | `tsc -b` + `vite build` clean. (The existing vitest suite is known-broken and pre-dates this work; not a gate.) |
| **Text** | Encoding/BOM/line-ending round-trip (§9.6). |

---

## 11. Phase 0 checklist (do these first, in this order)

1. `img-src` CSP fix (§8.1) — one line, unblocks all later image work.
2. Tauri ACL: `dialog:allow-save`, `fs:allow-write-file` (§8.2).
3. Round-trip invariant spike on generated docx/xlsx/pptx fixtures (§10).
4. `pdfium-render` annotation-creation spike (§7.5) — confirm or fall back to raw bindings.
5. Confirm `pdfium_permit()` sharing + page-cache design under a simulated concurrent OCR (§8.4).

Only after all five pass does Phase 1 begin.
