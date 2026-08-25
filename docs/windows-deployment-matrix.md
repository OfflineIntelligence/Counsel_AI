# Windows Deployment Matrix

How Offline Counsel AI selects and installs the llama.cpp engine (`b8037`) across
every supported Windows hardware variant. The same decision table exists in two
places that must stay in sync:

- **Install time:** `apps/desktop/src-tauri/installer-hooks.nsi` (NSIS post-install hook)
- **Runtime:** `engine_management/registry.rs` (`EngineRegistry::select_correct_engine`)
  fed by `model_runtime/platform_detector.rs` (`HardwareCapabilities`)

## Design contract (no fallbacks, no silent failures)

- Exactly **one** engine is selected per machine by the decision table below.
  Capability-gated selection (e.g. old NVIDIA driver → Vulkan) is correct
  selection, not a fallback. There are **no substitution chains**: if the
  selected engine cannot be installed, the failure is shown to the user with an
  explicit retry/install action — a different engine is never installed instead.
- Every download is verified against a **pinned SHA256** (digests from the
  GitHub release API; `ENGINE_ASSET_SHA256` in `registry.rs`, `SHA256_*`
  defines in the NSIS hook). Mismatch = deleted + explicit retry.
- `metadata.json` is written **last**, only after every verification step
  (binary present, backend self-report, and — for CUDA — cudart DLLs). Any
  failure deletes the whole engine directory. A directory containing
  `metadata.json` is therefore always a complete, verified engine.
- If no engine exists at app start, the frontend blocks behind an explicit
  "engine not installed" screen (`EngineSetupGate.tsx`) whose single action is
  a user-triggered `POST /engines/install`. Nothing downloads in the background.
- Engines that fail startup verification are kept visible with
  `status: Corrupted` + `failure_reason` (surfaced via `/healthz` and
  `GET /engines`), never silently discarded or auto-replaced.

## Supported OS floor

64-bit Windows 10/11, x64 and ARM64. (Tauri v2 + WebView2 + llama.cpp release
binaries hard-require this; 32-bit Windows and Windows 7/8 cannot run the stack.)
Downloads use `curl.exe` (Windows 10 1803+) with an automatic PowerShell
`Invoke-WebRequest` transport retry for older Windows 10 builds (same URL, same
artifact — hash-verified either way). Hash checks use `certutil.exe`.

## Decision table (first match wins — identical rows in NSIS and Rust)

| Row | Hardware variant | Pre-checks | Engine installed |
|---|---|---|---|
| 1 | Windows on ARM (Snapdragon/Surface, via `PROCESSOR_ARCHITEW6432`) | — | **CPU ARM64** |
| 2 | NVIDIA, driver ≥ 580.0, compute cap ≥ 7.5 (Turing+) | nvidia-smi probe emits `RESULT-CUDA13` | **CUDA 13.1** + cudart 13.1 package |
| 3 | NVIDIA, driver ≥ 527.41, compute cap ≥ 5.0 (Maxwell+) | probe emits `RESULT-CUDA12` | **CUDA 12.4** + cudart 12.4 package |
| 4 | Any GPU with a **real** Vulkan ICD + loader (AMD GCN/RDNA, Intel Arc/Iris/UHD Skylake+, NVIDIA failing CUDA floors) | Vulkan ICD probe passes | **Vulkan** |
| 5 | Everything else (no GPU, Intel HD without ICD, loader-but-no-ICD) | — | **CPU x64** |

Notes:
- NVIDIA with broken/unqueryable nvidia-smi (permissions, corrupt install) is
  treated as not CUDA-capable → row 4/5. Verified live on an Optimus laptop
  where nvidia-smi returns exit 4.
- The x64 CPU build self-dispatches per microarchitecture at runtime
  (`ggml-cpu-*.dll`, SSE4.2 → AVX512).
- The Adreno **OpenCL** engine (`win-opencl-adreno-arm64`) exists in the b8037
  release and is listed in the registry catalog when a Qualcomm GPU is
  detected, but is never auto-selected — installable only by explicit id via
  `POST /engines/install`.

## Verified capability floors (do not change without re-verifying)

| Gate | Floor | Source |
|---|---|---|
| CUDA 12.x apps, Windows driver | ≥ 527.41 | NVIDIA CUDA minor-version-compatibility docs |
| CUDA 12.x, compute capability | ≥ 5.0 (Maxwell) | CUDA 12 dropped Kepler |
| CUDA 13.x apps, driver | ≥ 580.0 (r580) | CUDA 13.0/13.1 release notes |
| CUDA 13.x, compute capability | ≥ 7.5 (Turing) | CUDA 13 dropped Maxwell/Pascal/Volta |

Constants live in `platform_detector.rs` (`CUDA12_MIN_WINDOWS_DRIVER`, etc.) and are
duplicated in the generated `cuda-check.ps1` inside `installer-hooks.nsi`.

## Vulkan detection (both installer and runtime)

`vulkan-1.dll` presence alone over-reports (the loader is installed by many apps).
A real driver must have registered an ICD via either:

1. Legacy: values under `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`, **or**
2. Modern PnP: a `VulkanDriverName` (or `VulkanDriverNameWow`) value under the
   display-adapter class key `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}\<NNNN>`

Modern drivers (Intel, NVIDIA) use only method 2 — checking method 1 alone
false-negatives current machines. The 32-bit NSIS process reads the SOFTWARE hive
through 64-bit PowerShell (via Sysnative) to avoid WOW64 registry redirection;
the SYSTEM hive is not redirected.

## GPU offload at runtime (`--n-gpu-layers`)

`config.rs::auto_detect_gpu_layers` order: NVML (NVIDIA, `nvidia` feature builds) →
nvidia-smi VRAM query → display-adapter registry `HardwareInformation.qwMemorySize`
(64-bit, exact for discrete AMD/Intel/Arc) → WMI `Win32_VideoController.AdapterRAM`
(32-bit DWORD; values ≥ 4000 MB are clamped-up because it saturates above 4 GB).
If a GPU engine is the default but VRAM could not be determined (gpu_layers = 0),
the condition is named loudly: `error!` at startup and a `warnings` entry in
`/healthz` — inference still runs, but never silently degraded.

There is ONE config file: the repo-root `.env`, which is both the development
config and the file the installer ships (`tauri.conf.json` maps it to
`resources/.env`). It keeps `GPU_LAYERS/CTX_SIZE/BATCH_SIZE/THREADS=auto` so the
RAM/VRAM-aware detection always runs on user machines. That is what makes a
single file safe to ship: machine-specific overrides (`MODEL_PATH`, `LLAMA_BIN`,
`MMPROJ_PATH`, credentials) are commented out and must stay that way — an
uncommented value there would override the safety detection on every install.

## Startup verification (every boot, every engine)

Every on-disk engine — however it was installed — is re-verified at each startup
(`engine_management/registry.rs::load_engine_metadata`):

1. `metadata.json` parses (else the directory is ignored as an orphan);
2. binary present (flat or one-level-deep layout);
3. **CUDA only:** `cudart64_*` and `cublas64_*` DLLs present on disk —
   `llama-server --version` genuinely cannot exercise cuBLAS, so file presence
   is the only pre-flight signal for the runtime package;
4. `llama-server --version` self-reports `load_backend: loaded <Name> backend`
   matching the engine's declared acceleration (`dll_manager.rs`).

Failures produce a visible `Corrupted` entry with the exact reason; the engine
is excluded from selection and the UI offers an explicit reinstall.

## Upgrade + uninstall lifecycle

- When `ENGINE_VERSION` is bumped, the newer engine installs into a new
  version-suffixed directory. After it passes boot verification, older-version
  engines of the same (platform, architecture, acceleration) are deleted with an
  `info!` log (`cleanup_superseded_engines`). Selection among installed engines
  is fully deterministic (table acceleration → version → score → id).
- The NSIS uninstaller (`NSIS_HOOK_POSTUNINSTALL`) always removes `engines/`,
  `downloads/`, `registry/`, `logs/` (app-owned, re-creatable) and asks the user
  explicitly before deleting models and chat data.

## Release checklist (ENGINE_VERSION bump)

1. `ENGINE_VERSION` + `ENGINE_ASSET_SHA256` in `engine_management/registry.rs`
   (digests from `https://api.github.com/repos/ggml-org/llama.cpp/releases/tags/<ver>`).
2. `ENGINE_VERSION` + `SHA256_*` defines in `installer-hooks.nsi`.
3. Confirm every referenced asset name still exists in the new release
   (llama.cpp has renamed assets between releases before).
4. Anything missed fails **closed**: unknown assets are refused, hash
   mismatches abort with rollback.
