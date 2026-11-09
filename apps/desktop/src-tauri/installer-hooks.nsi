; Offline Counsel AI — Tauri NSIS custom-install hook
; Included by Tauri's generated NSIS installer via installerHooks in tauri.conf.json.
; Runs AFTER Tauri has installed the main executable and Resources folder.
; Tauri v2 invokes !insertmacro NSIS_HOOK_POSTINSTALL (v1 used customInstall).
;
; DESIGN CONTRACT (no fallbacks, no silent failures):
;   - Hardware detection selects exactly ONE engine from the decision table
;     mirrored in crates/offline-intelligence/src/engine_management/registry.rs
;     (EngineRegistry::select_correct_engine). The two tables MUST stay in sync.
;   - The selected engine is VERIFIED BY LAUNCH before it is committed: the
;     binary is run with --version and must self-report `load_backend: loaded
;     <Accel> backend`. This is byte-for-byte the same predicate the app applies
;     on every boot (DllManager::verify_backend_loads), so an install that
;     succeeds here cannot fail there. Registry-based capability probes decide
;     what to TRY; only a real load decides what is KEPT.
;   - An acceleration that fails that launch check is recorded as proven-unusable
;     on this machine (verification.json, shared schema with the app) and the
;     decision table is re-run with it excluded. This is bounded and terminal:
;     CUDA -> Vulkan -> CPU, at most three attempts, and the CPU row is never
;     excludable. Demotion is always reported to the user, never silent.
;   - Every download is verified against a pinned SHA256 (from the GitHub
;     release API digests). Mismatch = the file is deleted and the user chooses
;     Retry or Cancel — never installed unverified.
;   - metadata.json is written LAST, only after the engine binary AND (for CUDA)
;     the cudart runtime DLLs are all verified on disk. A directory containing
;     metadata.json is therefore always a complete engine. On any failure the
;     whole engine directory is deleted so no partial install can be discovered
;     later by the app's registry scan.
;   - If the user cancels, the app starts with NO engine and shows an explicit
;     "engine not installed" screen with a user-triggered install button
;     (POST /engines/install). Nothing downloads in the background.
;
; Decision table (first match wins — same rows as select_correct_engine).
; A row whose acceleration is recorded as proven-unusable is skipped; rows 1/5
; (CPU) are terminal and never skipped:
;   1. ARM64 host                                       -> win-cpu-arm64
;   2. x64 + NVIDIA + driver >= 580.0 + compute >= 7.5  -> win-cuda-13.1 (+cudart)
;   3. x64 + NVIDIA + driver >= 527.41 + compute >= 5.0 -> win-cuda-12.4 (+cudart)
;   4. x64 + any GPU with a real Vulkan ICD + loader    -> win-vulkan
;   5. x64 otherwise                                    -> win-cpu-x64
;
; SUPPORTED PLATFORMS: Windows 10 1803 (build 17134) and newer, x64 or ARM64.
; Below that floor the installer refuses explicitly rather than producing a
; broken install: this app is Tauri v2 + Evergreen WebView2, neither of which
; supports Windows 7/8/8.1, and the installer itself relies on curl.exe
; (Win10 1803+), Expand-Archive (PowerShell 5, Win10+) and Win10-format
; certutil hash output. 32-bit x86 Windows is refused because the application
; binary is x86_64/ARM64 only.
;
; MSVC RUNTIME: llama.cpp's binaries import the VC++ 2015-2022 runtime, and the
; GPU backends need a NEWER piece of it than the rest (verified from the PE
; import directories of the b8037 assets):
;   llama-server.exe, ggml-cpu-*.dll, ggml-rpc.dll  -> VCRUNTIME140.dll, MSVCP140.dll
;   ggml-vulkan.dll, ggml-cuda.dll                  -> ALSO VCRUNTIME140_1.dll
; VCRUNTIME140_1.dll only ships with redist 14.20+ (VS2019). On a machine with
; an older 14.0-14.16 redist the engine starts, the CPU and RPC backends load,
; and LoadLibrary("ggml-vulkan.dll") fails SILENTLY — ggml simply never
; registers the backend and prints no error. That is indistinguishable from a
; broken GPU driver unless the runtime is checked directly, so it is checked
; directly, before any engine is selected.
;
; RELEASE CHECKLIST — when bumping ENGINE_VERSION, also update:
;   1. ENGINE_VERSION + every SHA256_* define below (digests from
;      https://api.github.com/repos/ggml-org/llama.cpp/releases/tags/<ver>)
;   2. ENGINE_VERSION + ENGINE_ASSET_SHA256 in
;      crates/offline-intelligence/src/engine_management/registry.rs
;   3. Confirm the asset names still exist in the new release.
;
; The driver/compute-capability floors must match the constants in
;   crates/offline-intelligence/src/model_runtime/platform_detector.rs
; (CUDA12: driver >= 527.41, cc >= 5.0. CUDA13: driver >= 580.0, cc >= 7.5).

!include "FileFunc.nsh"
!include "LogicLib.nsh"

!define ENGINE_VERSION "b8037"
!define LLAMA_BASE_URL "https://github.com/ggml-org/llama.cpp/releases/download/${ENGINE_VERSION}"

; Pinned SHA256 digests (lowercase hex) from the GitHub release API for ${ENGINE_VERSION}
!define SHA256_WIN_CPU_X64   "d7f460b1782e054b070f1a6345a652c6592faae4716da8584f6ac3dba8583caa"
!define SHA256_WIN_CPU_ARM64 "ffc80fb38b6061ef2195a792d65fa9c84eaccb5f289a43690d7ec0ed0bb114a3"
!define SHA256_WIN_CUDA124   "b31bfbc9c9f1e91a63471ceee9adaaeac7f626c8791f611502dd398bf852abe8"
!define SHA256_CUDART124     "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6"
!define SHA256_WIN_CUDA131   "4ba1fd0d12ea75fadb25ebe37e41fddf2551bd1a434fc184c846a2b3c963d83c"
!define SHA256_CUDART131     "f96935e7e385e3b2d0189239077c10fe8fd7e95690fea4afec455b1b6c7e3f18"
!define SHA256_WIN_VULKAN    "c190664ddb25232bba6547df0802d057df52bce7fc3407c26a03a1c91fac4e57"

; Display adapter class GUID — the single authoritative registry path for ALL GPUs
; Multilingual embedding model used for document retrieval (RAG). Pinned like
; the engine: exact file, size and SHA256. Must match
; crates/offline-intelligence/src/rag/model.rs (MODEL).
!define EMBED_MODEL_ID     "qwen3-embedding-0.6b-q8_0"
!define EMBED_MODEL_FILE   "Qwen3-Embedding-0.6B-Q8_0.gguf"
!define EMBED_MODEL_URL    "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF/resolve/main/Qwen3-Embedding-0.6B-Q8_0.gguf"
!define EMBED_MODEL_SHA256 "06507c7b42688469c4e7298b0a1e16deff06caf291cf0a5b278c308249c3e439"
!define EMBED_MODEL_SIZE   639150592

!define DISPLAY_CLASS_GUID "SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}"

; Minimum supported Windows build: 10 version 1803. See SUPPORTED PLATFORMS above.
!define MIN_WINDOWS_BUILD 17134

; Microsoft VC++ 2015-2022 redistributable.
;
; These are DELIBERATELY not SHA256-pinned, unlike every llama.cpp asset here.
; The llama.cpp assets are immutable artifacts of a fixed release tag, so a pin
; is exactly right. These are evergreen vendor URLs that Microsoft re-issues on
; every servicing update — a pin would make the installer fail for everyone the
; first time Microsoft ships a security fix, which is strictly worse than the
; problem it solves. The integrity control appropriate to a mutable vendor
; binary is its Authenticode signature: the file must carry a VALID signature
; naming Microsoft as the signer, or it is deleted and never executed
; (oci_VerifyMicrosoftSignature). That refuses tampering and MITM while
; surviving legitimate vendor updates.
!define VCREDIST_X64_URL   "https://aka.ms/vs/17/release/vc_redist.x64.exe"
!define VCREDIST_ARM64_URL "https://aka.ms/vs/17/release/vc_redist.arm64.exe"

; PDFium (Chrome's PDF engine), used for PDF text extraction and for
; rasterizing scanned pages before OCR.
;
; The x64 build is vendored in-tree and bundled by Tauri as a resource. No
; ARM64 build is vendored, so on an ARM64 host it is fetched here and the
; bundled x64 copy next to the executable is replaced with it. Unlike the VC++
; redistributable this IS an immutable artifact of a fixed release tag, so it
; is SHA256-pinned like the llama.cpp assets.
;
; Digest verified against the GitHub release API `digest` field for this tag,
; and against the downloaded file. The archive is a .tar.gz containing
; bin/pdfium.dll (an ARM64 PE importing only KERNEL32/USER32/GDI32/ADVAPI32 —
; the CRT is linked statically, so it needs no redistributable of its own).
;
; RELEASE CHECKLIST — when bumping PDFIUM_RELEASE, refresh PDFIUM_ARM64_SHA256
; from https://api.github.com/repos/bblanchon/pdfium-binaries/releases/tags/<tag>
!define PDFIUM_RELEASE       "chromium/8035"
!define PDFIUM_ARM64_ASSET   "pdfium-win-arm64.tgz"
!define PDFIUM_ARM64_SHA256  "278b915eb2253a54cb09e82baaa1741cdb322fe3bcaf1f092de48956e4271d93"
!define PDFIUM_ARM64_URL     "https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_RELEASE}/${PDFIUM_ARM64_ASSET}"

; ── Variables ─────────────────────────────────────────────────────────────────
Var EngineDownloadUrl
Var EngineFilename
Var EngineDestPath
Var EngineId
Var EngineName
Var EngineAcceleration
Var EngineSha256    ; pinned SHA256 of the selected engine zip
Var CudartFilename  ; cudart zip name ("" for non-CUDA engines)
Var CudartSha256    ; pinned SHA256 of the cudart zip
Var AppDataPath
Var IsArm64Host
Var EngineArch
Var GpuCandidate  ; tracks best GPU found so far: "NVIDIA" > "Vulkan" > ""
Var CurlPath      ; resolved path to curl.exe (Sysnative or System32)
Var PwshPath      ; resolved path to powershell.exe (Sysnative or System32)
Var NvidiaSmiPath ; resolved path to nvidia-smi.exe ("" when not found)
Var CudaCapable   ; "13" when CUDA 13 floors met, "12" when only CUDA 12 floors met, "0" otherwise
Var VulkanAvailable ; "1" when a real Vulkan ICD is registered + loader present
Var DlUrl         ; oci_DownloadFile input: source URL
Var DlDest        ; oci_DownloadFile input: destination file path
Var DlExit        ; oci_DownloadFile output: "0" on success
Var HashTarget    ; oci_VerifySha256 input: file to hash
Var ExpectedSha   ; oci_VerifySha256 input: expected lowercase SHA256
Var HashOk        ; oci_VerifySha256 output: "1" on match
Var CudartOk      ; oci_DownloadCudaRuntimeDlls output: "1" on verified success
Var BackendOk     ; oci_VerifyBackendLoads output: "1" when the engine self-reported its backend
Var ExcludeCuda   ; "1" once a CUDA engine has failed the launch check on this machine
Var ExcludeVulkan ; "1" once a Vulkan engine has failed the launch check on this machine
Var DemotedFrom   ; acceleration we demoted away from ("" when none) — reported to the user
Var InstallAttempt ; bounded attempt counter for the select/verify/demote loop
Var VcRuntimeOk   ; "1" when VCRUNTIME140 + VCRUNTIME140_1 + MSVCP140 are all present
Var EmbedAllowed  ; "1" once the platform check passed
Var Sys64Dir      ; real 64-bit/native System32 (Sysnative from this 32-bit process)

; ── NSIS_HOOK_POSTINSTALL (called by Tauri v2's NSIS template) ────────────────
!macro NSIS_HOOK_POSTINSTALL

  ; A0) Platform floor. Refuse explicitly instead of leaving a broken install:
  ;     every failure below is unfixable at runtime, so saying so here is the
  ;     only honest outcome. Tauri has already copied the app by this point, so
  ;     we do not abort the installer — we skip engine setup and tell the user.
  StrCpy $EmbedAllowed "0"
  Call oci_CheckPlatformSupported
  Pop $0
  ${If} $0 != "1"
    Goto oci_EngineDone
  ${EndIf}
  StrCpy $EmbedAllowed "1"

  ; A) Application data directories
  StrCpy $AppDataPath "$LOCALAPPDATA\Offline Counsel AI"
  CreateDirectory "$AppDataPath"
  CreateDirectory "$AppDataPath\engines"
  CreateDirectory "$AppDataPath\models"
  CreateDirectory "$AppDataPath\data"
  CreateDirectory "$AppDataPath\logs"
  CreateDirectory "$AppDataPath\registry"
  CreateDirectory "$AppDataPath\downloads"

  ; Temp working directory (detection scripts + downloads live here)
  CreateDirectory "$TEMP\OCA-Install"

  ; B) Resolve paths to curl.exe and powershell.exe
  ;    NSIS is a 32-bit process.  On 64-bit Windows, $SYSDIR points to SysWOW64.
  ;    Sysnative is the 32-bit alias for the real System32.
  ;    Try Sysnative first; fall back to $SYSDIR for safety.
  StrCpy $CurlPath "$WINDIR\Sysnative\curl.exe"
  IfFileExists $CurlPath +2
    StrCpy $CurlPath "$SYSDIR\curl.exe"
  StrCpy $PwshPath "$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe"
  IfFileExists $PwshPath +2
    StrCpy $PwshPath "$SYSDIR\WindowsPowerShell\v1.0\powershell.exe"

  ; B2) Write the PowerShell probes. Done here rather than inside hardware
  ;     detection because the backend-launch check needs them on EVERY path,
  ;     including the ARM64 path which returns before the detection probes run.
  Call oci_WriteDetectScripts

  ; B3) MSVC runtime. Must precede engine selection: without VCRUNTIME140_1.dll
  ;     the GPU backends fail to load SILENTLY (see MSVC RUNTIME above), which
  ;     would otherwise be misdiagnosed as a broken GPU and demote a perfectly
  ;     good GPU machine to CPU forever.
  Call oci_EnsureVcRuntime

  ; B4) ARM64 hosts need the ARM64 PDFium; the bundled resource is x64.
  ${If} $IsArm64Host == "1"
    Call oci_EnsureArm64Pdfium
  ${EndIf}

  ; C) Select -> download -> verify by launch -> (on backend failure) exclude
  ;    that acceleration and re-run the table. Bounded: CUDA -> Vulkan -> CPU.
  StrCpy $ExcludeCuda    "0"
  StrCpy $ExcludeVulkan  "0"
  StrCpy $DemotedFrom    ""
  StrCpy $InstallAttempt "0"

  oci_SelectAndInstall:
  IntOp $InstallAttempt $InstallAttempt + 1
  ${If} $InstallAttempt > 3
    DetailPrint "Exhausted the decision table without a working engine."
    Goto oci_EngineFailed
  ${EndIf}

  Call oci_DetectHardwareAndSetEngine

  ; D) Skip only when a COMPLETE engine is present: binary AND metadata.json.
  ;    metadata.json is written last on success, so its presence proves the
  ;    previous install finished all verification steps. A binary without
  ;    metadata is a broken partial install — remove it and reinstall.
  IfFileExists "$EngineDestPath\llama-server.exe" 0 oci_DownloadEngine
  IfFileExists "$EngineDestPath\metadata.json" oci_EngineExists 0
  DetailPrint "Removing incomplete previous engine install at $EngineDestPath"
  RMDir /r "$EngineDestPath"
  Goto oci_DownloadEngine

  oci_DownloadEngine:
    DetailPrint "Downloading llama.cpp engine ${ENGINE_VERSION}: $EngineName"
    DetailPrint "Source : $EngineDownloadUrl"
    DetailPrint "Target : $EngineDestPath"
    DetailPrint "Please wait, downloading. This may take a few minutes..."

    StrCpy $DlUrl  "$EngineDownloadUrl"
    StrCpy $DlDest "$TEMP\OCA-Install\$EngineFilename"
    Call oci_DownloadFile

    ${If} $DlExit != "0"
      DetailPrint "Engine download failed (exit code $DlExit)."
      MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
        "Engine download failed (exit code $DlExit).$\n$\nRetry to try again, or Cancel to skip. If you cancel, the application will show an 'engine not installed' screen with an install button." \
        IDRETRY oci_DownloadEngine
      Goto oci_EngineFailed
    ${EndIf}

    ; Integrity: pinned SHA256 — a corrupted or tampered download is never installed
    StrCpy $HashTarget "$TEMP\OCA-Install\$EngineFilename"
    StrCpy $ExpectedSha "$EngineSha256"
    Call oci_VerifySha256
    ${If} $HashOk != "1"
      Delete "$TEMP\OCA-Install\$EngineFilename"
      DetailPrint "Engine download failed SHA256 verification."
      MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
        "The downloaded engine failed its integrity check (SHA256 mismatch) and was deleted.$\n$\nRetry to download again, or Cancel to skip." \
        IDRETRY oci_DownloadEngine
      Goto oci_EngineFailed
    ${EndIf}

    DetailPrint "Download verified — extracting..."
    CreateDirectory "$EngineDestPath"

    ; llama.cpp releases are ZIP archives with a flat layout (no top-level
    ; directory).  Windows tar.exe cannot extract ZIP files, so we use
    ; PowerShell's Expand-Archive which ships with Windows 10+.
    ; Paths are passed via environment variables to avoid all quoting
    ; issues (spaces in app name, apostrophes in usernames, etc.).
    System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_ZIP", t "$TEMP\OCA-Install\$EngineFilename")i'
    System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_DST", t "$EngineDestPath")i'
    nsExec::ExecToLog '"$PwshPath" -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "Expand-Archive -LiteralPath $$env:OCA_ZIP -DestinationPath $$env:OCA_DST -Force"'
    Pop $0
    Delete "$TEMP\OCA-Install\$EngineFilename"

    ; Verify extraction actually produced the binary
    IfFileExists "$EngineDestPath\llama-server.exe" 0 oci_ExtractFailed

    ; CUDA engines additionally require the separate cudart runtime package.
    ; A CUDA engine without it would pass every startup check but fail at the
    ; first inference — so cudart failure is fatal and rolls back the engine.
    ${If} $EngineAcceleration == "CUDA"
      Call oci_DownloadCudaRuntimeDlls
      ${If} $CudartOk != "1"
        DetailPrint "CUDA runtime install failed — rolling back the engine directory."
        RMDir /r "$EngineDestPath"
        MessageBox MB_ICONEXCLAMATION|MB_OK \
          "The CUDA runtime libraries could not be installed, so the CUDA engine was removed (it would not work without them).$\n$\nThe application will show an 'engine not installed' screen with an install button."
        Goto oci_EngineFailed
      ${EndIf}
    ${EndIf}

    ; THE decisive check: launch the binary and require it to self-report
    ; loading the backend it was selected for. Everything up to here proves the
    ; bytes are correct; only this proves they WORK on this machine.
    Call oci_VerifyBackendLoads
    ${If} $BackendOk != "1"
      DetailPrint "$EngineAcceleration backend did not load — removing $EngineDestPath"
      RMDir /r "$EngineDestPath"

      ${If} $EngineAcceleration == "CUDA"
        StrCpy $ExcludeCuda "1"
        StrCpy $DemotedFrom "CUDA"
        Call oci_WriteVerificationLog
        DetailPrint "CUDA recorded as unusable on this machine — re-running the decision table"
        Goto oci_SelectAndInstall
      ${EndIf}

      ${If} $EngineAcceleration == "Vulkan"
        StrCpy $ExcludeVulkan "1"
        StrCpy $DemotedFrom "Vulkan"
        Call oci_WriteVerificationLog
        DetailPrint "Vulkan recorded as unusable on this machine — re-running the decision table"
        Goto oci_SelectAndInstall
      ${EndIf}

      ; CPU is the terminal row. If it cannot load its own backend, nothing can.
      DetailPrint "The CPU engine failed to load its backend — no usable engine exists."
      MessageBox MB_ICONEXCLAMATION|MB_OK \
        "The inference engine could not start on this computer: even the CPU engine failed to load its backend.$\n$\nThis usually means the Microsoft Visual C++ 2015-2022 Redistributable is missing or damaged.$\n$\nThe application will show an 'engine not installed' screen with an install button."
      Goto oci_EngineFailed
    ${EndIf}

    ; LAST step: metadata.json — its presence marks a fully verified engine
    Call oci_WriteEngineMetadata
    DetailPrint "Engine installed and verified: $EngineName"
    Goto oci_EngineReady

  oci_ExtractFailed:
    DetailPrint "Extraction failed — llama-server.exe not found in $EngineDestPath"
    RMDir /r "$EngineDestPath"
    MessageBox MB_ICONEXCLAMATION|MB_OK \
      "Engine extraction failed — llama-server.exe was not found. The partial install was removed.$\nThe application will show an 'engine not installed' screen with an install button."
    Goto oci_EngineFailed

  oci_EngineFailed:
    ; Explicit no-engine outcome: nothing partial remains on disk; the app's
    ; startup scan finds no metadata.json and reports state=not_installed.
    DetailPrint "No engine installed. The application will offer an explicit install button."
    Goto oci_EngineDone

  oci_EngineExists:
    DetailPrint "Verified engine already installed at $EngineDestPath — skipping download"

  oci_EngineReady:
    ; A demotion is a real change to what the user gets, so it is reported on
    ; BOTH success paths — including when the demoted-to engine was already
    ; installed from a previous run and no download happened this time.
    ${If} $DemotedFrom != ""
      DetailPrint "Demoted from $DemotedFrom to $EngineAcceleration after backend verification failed"
      MessageBox MB_ICONINFORMATION|MB_OK \
        "$DemotedFrom acceleration failed to initialize on this computer's graphics driver, so the $EngineAcceleration engine was installed instead.$\n$\nThe application is fully functional; responses will be slower than with GPU acceleration.$\n$\nUpdating your graphics driver, or installing the latest Microsoft Visual C++ 2015-2022 Redistributable, and then reinstalling may enable $DemotedFrom."
    ${EndIf}

  oci_EngineDone:
    ; The embedding model is independent of which engine was chosen (or
    ; whether one installed at all): the app offers both again if missing.
    ${If} $EmbedAllowed == "1"
      Call oci_InstallEmbeddingModel
    ${EndIf}
    RMDir /r "$TEMP\OCA-Install"
!macroend

; ── NSIS_HOOK_POSTUNINSTALL ───────────────────────────────────────────────────
; Engines and downloads are app-owned and re-creatable: always removed.
; Models and user data (chats, accounts, files) are removed only with explicit
; consent — one clear question, no silent deletion of user data.
!macro NSIS_HOOK_POSTUNINSTALL
  StrCpy $AppDataPath "$LOCALAPPDATA\Offline Counsel AI"

  RMDir /r "$AppDataPath\engines"
  RMDir /r "$AppDataPath\downloads"
  RMDir /r "$AppDataPath\registry"
  RMDir /r "$AppDataPath\logs"

  IfFileExists "$AppDataPath\*.*" 0 oci_UninstDone
  MessageBox MB_YESNO|MB_ICONQUESTION \
    "Also delete downloaded AI models and your chat data (conversations, files, accounts)?$\n$\nChoose No to keep them for a future reinstall." \
    IDNO oci_UninstKeepData
  RMDir /r "$AppDataPath"
  Goto oci_UninstDone

  oci_UninstKeepData:
    DetailPrint "Keeping models and user data at $AppDataPath"

  oci_UninstDone:
!macroend

; ── Hardware detection ────────────────────────────────────────────────────────
; Implements the decision table at the top of this file. Capability-gated
; selection, evaluated once, producing exactly one engine. NVIDIA machines that
; fail the CUDA floors get Vulkan because that IS the correct engine for them
; (NVIDIA drivers ship a Vulkan ICD) — selection, not fallback.
Function oci_DetectHardwareAndSetEngine
  ; ── Default: CPU engine (x64) — decision-table row 5 ──
  StrCpy $EngineFilename    "llama-${ENGINE_VERSION}-bin-win-cpu-x64.zip"
  StrCpy $EngineDownloadUrl "${LLAMA_BASE_URL}/llama-${ENGINE_VERSION}-bin-win-cpu-x64.zip"
  StrCpy $EngineDestPath    "$AppDataPath\engines\llama-cpu-windows-x64-${ENGINE_VERSION}"
  StrCpy $EngineId          "llama-cpu-windows-x64-${ENGINE_VERSION}"
  StrCpy $EngineName        "llama.cpp CPU (Windows x64)"
  StrCpy $EngineAcceleration "CPU"
  StrCpy $EngineArch        "X86_64"
  StrCpy $EngineSha256      "${SHA256_WIN_CPU_X64}"
  StrCpy $CudartFilename    ""
  StrCpy $CudartSha256      ""

  ; ── Row 1: ARM64 host ────────────────────────────────────────────────────────
  ; This installer is compiled as x86_64. On ARM64 hosts it runs under WOW64
  ; emulation.  PROCESSOR_ARCHITEW6432 reveals the *native* architecture.
  ; No CUDA or Vulkan build exists for ARM64 Windows at ${ENGINE_VERSION}.
  ; $IsArm64Host was already established by oci_CheckPlatformSupported from the
  ; same two environment variables; it is not re-derived here so the two cannot
  ; drift apart.
  ${If} $IsArm64Host == "1"
    DetailPrint "ARM64 host detected — selecting native ARM64 CPU engine"
    StrCpy $EngineFilename    "llama-${ENGINE_VERSION}-bin-win-cpu-arm64.zip"
    StrCpy $EngineDownloadUrl "${LLAMA_BASE_URL}/llama-${ENGINE_VERSION}-bin-win-cpu-arm64.zip"
    StrCpy $EngineDestPath    "$AppDataPath\engines\llama-cpu-windows-arm64-${ENGINE_VERSION}"
    StrCpy $EngineId          "llama-cpu-windows-arm64-${ENGINE_VERSION}"
    StrCpy $EngineName        "llama.cpp CPU (Windows ARM64)"
    StrCpy $EngineAcceleration "CPU"
    StrCpy $EngineArch        "Aarch64"
    StrCpy $EngineSha256      "${SHA256_WIN_CPU_ARM64}"
    Goto oci_DetectionDone
  ${EndIf}

  ; ── Locate nvidia-smi.exe (ships with every NVIDIA driver) ──────────────────
  ; NSIS is a 32-bit process, so $WINDIR\System32 is redirected to SysWOW64 by
  ; WOW64.  We use Sysnative (the 32-bit alias for the real System32) to reach
  ; the 64-bit nvidia-smi.
  StrCpy $NvidiaSmiPath ""
  IfFileExists "$PROGRAMFILES64\NVIDIA Corporation\NVSMI\nvidia-smi.exe" 0 +2
    StrCpy $NvidiaSmiPath "$PROGRAMFILES64\NVIDIA Corporation\NVSMI\nvidia-smi.exe"
  ${If} $NvidiaSmiPath == ""
    IfFileExists "$WINDIR\Sysnative\nvidia-smi.exe" 0 +2
      StrCpy $NvidiaSmiPath "$WINDIR\Sysnative\nvidia-smi.exe"
  ${EndIf}

  ; ── Enumerate display adapters via class GUID ───────────────────────────────
  ; Scan ALL subkeys, tracking the best GPU found.  NVIDIA beats AMD/Intel
  ; beats nothing.  This handles multi-GPU systems (e.g. NVIDIA dGPU + Intel iGPU)
  ; regardless of which adapter is registered first.
  ; (The SYSTEM hive is not subject to WOW64 registry redirection.)
  StrCpy $GpuCandidate ""
  StrCpy $R0 0
  oci_EnumDisplayAdapters:
    ClearErrors
    EnumRegKey $R1 HKLM "${DISPLAY_CLASS_GUID}" $R0
    IfErrors oci_EnumDone
    ; EnumRegKey returns "" WITHOUT setting the error flag when the subkeys
    ; are exhausted — without this check the loop never terminates on
    ; machines where no NVIDIA match exits it early.
    StrCmp $R1 "" oci_EnumDone
    ReadRegStr $R2 HKLM "${DISPLAY_CLASS_GUID}\$R1" "DriverDesc"
    IfErrors oci_EnumNext

    ; Check for NVIDIA — highest priority, stop scanning
    Push $R2
    Push "NVIDIA"
    Call oci_StrContains
    Pop $R3
    ${If} $R3 != ""
      DetailPrint "NVIDIA GPU detected: $R2"
      StrCpy $GpuCandidate "NVIDIA"
      Goto oci_EnumDone
    ${EndIf}

    ; Check for AMD / ATI — record as Vulkan candidate, keep scanning for NVIDIA
    Push $R2
    Push "AMD"
    Call oci_StrContains
    Pop $R3
    ${If} $R3 != ""
    ${AndIf} $GpuCandidate != "Vulkan"
      StrCpy $GpuCandidate "Vulkan"
      DetailPrint "AMD GPU detected: $R2 — Vulkan candidate (checking for NVIDIA...)"
    ${EndIf}
    Push $R2
    Push "ATI"
    Call oci_StrContains
    Pop $R3
    ${If} $R3 != ""
    ${AndIf} $GpuCandidate != "Vulkan"
      StrCpy $GpuCandidate "Vulkan"
      DetailPrint "AMD/ATI GPU detected: $R2 — Vulkan candidate (checking for NVIDIA...)"
    ${EndIf}

    ; Check for Intel — record as Vulkan candidate, keep scanning for NVIDIA
    Push $R2
    Push "Intel"
    Call oci_StrContains
    Pop $R3
    ${If} $R3 != ""
    ${AndIf} $GpuCandidate != "Vulkan"
      StrCpy $GpuCandidate "Vulkan"
      DetailPrint "Intel GPU detected: $R2 — Vulkan candidate (checking for NVIDIA...)"
    ${EndIf}

  oci_EnumNext:
    IntOp $R0 $R0 + 1
    Goto oci_EnumDisplayAdapters
  oci_EnumDone:

  ; nvidia-smi present but registry scan missed it (edge case: adapter names
  ; without the NVIDIA substring) — trust the driver tool
  ${If} $GpuCandidate == ""
  ${AndIf} $NvidiaSmiPath != ""
    StrCpy $GpuCandidate "NVIDIA"
    DetailPrint "NVIDIA driver tools found — treating as NVIDIA GPU"
  ${EndIf}

  ; ── Rows 2+3: NVIDIA CUDA capability pre-check ───────────────────────────────
  ; CUDA 13.x requires driver >= 580.0 and compute capability >= 7.5 (Turing+).
  ; CUDA 12.x requires driver >= 527.41 and compute capability >= 5.0.
  ; Cards/drivers failing both floors route to Vulkan below — NVIDIA drivers
  ; ship a Vulkan ICD, so acceleration is still available.
  ${If} $GpuCandidate == "NVIDIA"
    StrCpy $CudaCapable "0"
    ; A CUDA engine already proved it cannot load its backend here — skip both
    ; CUDA rows outright rather than re-downloading 370+ MB to fail identically.
    ; Both rows are excluded together because they share the same failure
    ; surface (ggml-cuda.dll, nvcuda.dll, VCRUNTIME140_1.dll).
    ${If} $ExcludeCuda == "1"
      DetailPrint "CUDA previously failed backend verification on this machine — skipping rows 2 and 3"
      StrCpy $GpuCandidate "Vulkan"
      Goto oci_CudaRowsDone
    ${EndIf}
    ${If} $NvidiaSmiPath != ""
      Call oci_CheckCudaCapable
    ${EndIf}
    ${If} $CudaCapable == "13"
      DetailPrint "CUDA 13 capability confirmed (driver >= 580, compute cap >= 7.5) — selecting CUDA 13.1 engine"
      Goto oci_NvidiaFound13
    ${EndIf}
    ${If} $CudaCapable == "12"
      DetailPrint "CUDA 12 capability confirmed (driver >= 527.41, compute cap >= 5.0) — selecting CUDA 12.4 engine"
      Goto oci_NvidiaFound12
    ${EndIf}
    DetailPrint "NVIDIA GPU present but CUDA floors not met (old driver or pre-Maxwell GPU) — trying Vulkan"
    StrCpy $GpuCandidate "Vulkan"
  ${EndIf}
  oci_CudaRowsDone:

  ; ── Row 4: Any GPU candidate: Vulkan ICD pre-check ───────────────────────────
  ; A registered Vulkan ICD (legacy Khronos key or per-device VulkanDriverName)
  ; plus the loader DLL is required.  Loader-only systems (old Intel HD with no
  ; Vulkan driver) fall through to the CPU engine instead of a doomed install.
  ${If} $GpuCandidate == "Vulkan"
  ${AndIf} $ExcludeVulkan == "1"
    DetailPrint "Vulkan previously failed backend verification on this machine — skipping row 4"
  ${ElseIf} $GpuCandidate == "Vulkan"
    Call oci_CheckVulkanAvailable
    ${If} $VulkanAvailable == "1"
      DetailPrint "Vulkan ICD confirmed — selecting Vulkan engine"
      Goto oci_UseVulkanEngine
    ${EndIf}
    DetailPrint "GPU present but no Vulkan driver ICD registered — selecting CPU engine"
  ${EndIf}

  ; Row 5: CPU engine (set at top of function) applies
  Goto oci_DetectionDone

  oci_NvidiaFound13:
    StrCpy $EngineFilename    "llama-${ENGINE_VERSION}-bin-win-cuda-13.1-x64.zip"
    StrCpy $EngineDownloadUrl "${LLAMA_BASE_URL}/llama-${ENGINE_VERSION}-bin-win-cuda-13.1-x64.zip"
    StrCpy $EngineDestPath    "$AppDataPath\engines\llama-cuda13-windows-x64-${ENGINE_VERSION}"
    StrCpy $EngineId          "llama-cuda13-windows-x64-${ENGINE_VERSION}"
    StrCpy $EngineName        "llama.cpp CUDA 13 (Windows x64) (${ENGINE_VERSION})"
    StrCpy $EngineAcceleration "CUDA"
    StrCpy $EngineArch        "X86_64"
    StrCpy $EngineSha256      "${SHA256_WIN_CUDA131}"
    StrCpy $CudartFilename    "cudart-llama-bin-win-cuda-13.1-x64.zip"
    StrCpy $CudartSha256      "${SHA256_CUDART131}"
    Goto oci_DetectionDone

  oci_NvidiaFound12:
    StrCpy $EngineFilename    "llama-${ENGINE_VERSION}-bin-win-cuda-12.4-x64.zip"
    StrCpy $EngineDownloadUrl "${LLAMA_BASE_URL}/llama-${ENGINE_VERSION}-bin-win-cuda-12.4-x64.zip"
    StrCpy $EngineDestPath    "$AppDataPath\engines\llama-cuda-windows-x64-${ENGINE_VERSION}"
    StrCpy $EngineId          "llama-cuda-windows-x64-${ENGINE_VERSION}"
    StrCpy $EngineName        "llama.cpp CUDA (Windows x64) (${ENGINE_VERSION})"
    StrCpy $EngineAcceleration "CUDA"
    StrCpy $EngineArch        "X86_64"
    StrCpy $EngineSha256      "${SHA256_WIN_CUDA124}"
    StrCpy $CudartFilename    "cudart-llama-bin-win-cuda-12.4-x64.zip"
    StrCpy $CudartSha256      "${SHA256_CUDART124}"
    Goto oci_DetectionDone

  oci_UseVulkanEngine:
    StrCpy $EngineFilename    "llama-${ENGINE_VERSION}-bin-win-vulkan-x64.zip"
    StrCpy $EngineDownloadUrl "${LLAMA_BASE_URL}/llama-${ENGINE_VERSION}-bin-win-vulkan-x64.zip"
    StrCpy $EngineDestPath    "$AppDataPath\engines\llama-vulkan-windows-x64-${ENGINE_VERSION}"
    StrCpy $EngineId          "llama-vulkan-windows-x64-${ENGINE_VERSION}"
    StrCpy $EngineName        "llama.cpp Vulkan (Windows x64) (${ENGINE_VERSION})"
    StrCpy $EngineAcceleration "Vulkan"
    StrCpy $EngineArch        "X86_64"
    StrCpy $EngineSha256      "${SHA256_WIN_VULKAN}"

  oci_DetectionDone:
    DetailPrint "Engine selected: $EngineName ($EngineAcceleration)"
FunctionEnd

; ── Write the PowerShell capability probes ────────────────────────────────────
; PowerShell does the parsing/registry work (trivial there, error-prone in
; NSIS) and prints a single RESULT-* token that NSIS matches with the existing
; oci_StrContains helper.  64-bit PowerShell (reached via Sysnative) sees the
; native SOFTWARE hive and real System32, so no WOW64 redirection issues.
; NOTE: every PowerShell "$" is written as "$$" ($ is the NSIS escape char).
Function oci_WriteDetectScripts
  ; cuda-check.ps1 — floors must match platform_detector.rs:
  ;   CUDA 13: driver >= 580.0, compute capability >= 7.5 (unknown cc allowed
  ;            when the driver floor passes — same rule as cuda13_usable()).
  ;   CUDA 12: driver >= 527.41, compute capability >= 5.0 (unknown cc allowed
  ;            only if the driver floor passes; drivers that old reject the
  ;            compute_cap query anyway, so the driver gate decides).
  ; Emits exactly one token: RESULT-CUDA13, RESULT-CUDA12, or RESULT-NONE.
  ; Multi-GPU: highest compute capability wins.
  FileOpen $0 "$TEMP\OCA-Install\cuda-check.ps1" w
  FileWrite $0 "$$ErrorActionPreference = 'SilentlyContinue'$\r$\n"
  FileWrite $0 "$$smi = $$env:OCA_SMI$\r$\n"
  ; nvidia-smi can hang outright on broken driver installs, so it runs under a
  ; 5-second kill timeout — the same bound platform_detector.rs uses. Launch
  ; failure, timeout, nonzero exit, or empty output all mean "capability not
  ; confirmed" (RESULT-NONE -> Vulkan row), never a pass and never a wait.
  FileWrite $0 "function Get-SmiOutput([string]$$query) {$\r$\n"
  FileWrite $0 "  $$psi = New-Object System.Diagnostics.ProcessStartInfo$\r$\n"
  FileWrite $0 "  $$psi.FileName = $$smi$\r$\n"
  FileWrite $0 "  $$psi.Arguments = '--query-gpu=' + $$query + ' --format=csv,noheader,nounits'$\r$\n"
  FileWrite $0 "  $$psi.RedirectStandardOutput = $$true$\r$\n"
  FileWrite $0 "  $$psi.RedirectStandardError = $$true$\r$\n"
  FileWrite $0 "  $$psi.UseShellExecute = $$false$\r$\n"
  FileWrite $0 "  $$psi.CreateNoWindow = $$true$\r$\n"
  FileWrite $0 "  $$p = $$null$\r$\n"
  FileWrite $0 "  try { $$p = [System.Diagnostics.Process]::Start($$psi) } catch { return $$null }$\r$\n"
  FileWrite $0 "  if (-not $$p) { return $$null }$\r$\n"
  FileWrite $0 "  $$outTask = $$p.StandardOutput.ReadToEndAsync()$\r$\n"
  FileWrite $0 "  $$null = $$p.StandardError.ReadToEndAsync()$\r$\n"
  FileWrite $0 "  if (-not $$p.WaitForExit(5000)) { try { $$p.Kill() } catch {}; return $$null }$\r$\n"
  FileWrite $0 "  if ($$p.ExitCode -ne 0) { return $$null }$\r$\n"
  FileWrite $0 "  $$txt = $$outTask.Result$\r$\n"
  FileWrite $0 "  if (-not $$txt) { return $$null }$\r$\n"
  FileWrite $0 "  return ($$txt -split $\"`r?`n$\")$\r$\n"
  FileWrite $0 "}$\r$\n"
  FileWrite $0 "$$drvMaj = 0; $$drvMin = 0; $$ccMaj = -1; $$ccMin = 0$\r$\n"
  FileWrite $0 "$$q = Get-SmiOutput 'driver_version,compute_cap'$\r$\n"
  FileWrite $0 "if (-not $$q) { $$q = Get-SmiOutput 'driver_version' }$\r$\n"
  FileWrite $0 "foreach ($$l in @($$q)) {$\r$\n"
  FileWrite $0 "  if (-not $$l) { continue }$\r$\n"
  FileWrite $0 "  $$p = ($\"$$l$\").Split(',')$\r$\n"
  FileWrite $0 "  if ($$drvMaj -eq 0 -and $$p[0].Trim() -match '^(\d+)\.?(\d*)') {$\r$\n"
  FileWrite $0 "    $$drvMaj = [int]$$Matches[1]$\r$\n"
  FileWrite $0 "    if ($$Matches[2] -ne '') { $$drvMin = [int]$$Matches[2] }$\r$\n"
  FileWrite $0 "  }$\r$\n"
  FileWrite $0 "  if ($$p.Count -gt 1 -and $$p[1].Trim() -match '^(\d+)\.?(\d*)') {$\r$\n"
  FileWrite $0 "    $$m = [int]$$Matches[1]; $$n = 0$\r$\n"
  FileWrite $0 "    if ($$Matches[2] -ne '') { $$n = [int]$$Matches[2] }$\r$\n"
  FileWrite $0 "    if (($$m -gt $$ccMaj) -or (($$m -eq $$ccMaj) -and ($$n -gt $$ccMin))) { $$ccMaj = $$m; $$ccMin = $$n }$\r$\n"
  FileWrite $0 "  }$\r$\n"
  FileWrite $0 "}$\r$\n"
  FileWrite $0 "$$driver12Ok = ($$drvMaj -gt 527) -or (($$drvMaj -eq 527) -and ($$drvMin -ge 41))$\r$\n"
  FileWrite $0 "$$cc12Ok = ($$ccMaj -lt 0) -or ($$ccMaj -ge 5)$\r$\n"
  FileWrite $0 "$$driver13Ok = ($$drvMaj -ge 580)$\r$\n"
  FileWrite $0 "$$cc13Ok = ($$ccMaj -lt 0) -or ($$ccMaj -gt 7) -or (($$ccMaj -eq 7) -and ($$ccMin -ge 5))$\r$\n"
  FileWrite $0 "if ($$driver13Ok -and $$cc13Ok) { Write-Output 'RESULT-CUDA13' }$\r$\n"
  FileWrite $0 "elseif ($$driver12Ok -and $$cc12Ok) { Write-Output 'RESULT-CUDA12' }$\r$\n"
  FileWrite $0 "else { Write-Output ('RESULT-NONE driver=' + $$drvMaj + '.' + $$drvMin + ' cc=' + $$ccMaj + '.' + $$ccMin) }$\r$\n"
  FileClose $0

  ; vulkan-check.ps1 — a real Vulkan driver registers an ICD either under the
  ; legacy HKLM\SOFTWARE\Khronos\Vulkan\Drivers key or as a VulkanDriverName
  ; value on its display-adapter device key (modern PnP registration).
  ; The loader DLL (vulkan-1.dll) must also exist.
  FileOpen $0 "$TEMP\OCA-Install\vulkan-check.ps1" w
  FileWrite $0 "$$ErrorActionPreference = 'SilentlyContinue'$\r$\n"
  FileWrite $0 "$$found = $$false$\r$\n"
  FileWrite $0 "$$k = Get-Item 'HKLM:\SOFTWARE\Khronos\Vulkan\Drivers' -ErrorAction SilentlyContinue$\r$\n"
  FileWrite $0 "if ($$k -and $$k.ValueCount -gt 0) { $$found = $$true }$\r$\n"
  FileWrite $0 "if (-not $$found) {$\r$\n"
  FileWrite $0 "  $$cls = Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}' -ErrorAction SilentlyContinue$\r$\n"
  FileWrite $0 "  foreach ($$c in @($$cls)) {$\r$\n"
  FileWrite $0 "    if (-not $$c) { continue }$\r$\n"
  FileWrite $0 "    $$v = Get-ItemProperty -Path $$c.PSPath -Name VulkanDriverName -ErrorAction SilentlyContinue$\r$\n"
  FileWrite $0 "    if ($$v) { $$found = $$true; break }$\r$\n"
  FileWrite $0 "  }$\r$\n"
  FileWrite $0 "}$\r$\n"
  FileWrite $0 "$$loader = Test-Path (Join-Path $$env:windir 'System32\vulkan-1.dll')$\r$\n"
  FileWrite $0 "if ($$found -and $$loader) { Write-Output 'RESULT-VULKAN' } else { Write-Output 'RESULT-NONE' }$\r$\n"
  FileClose $0

  ; sig-check.ps1 — Authenticode verification for the VC++ redistributable.
  ; Written as a file rather than an inline -Command because the condition needs
  ; nested double quotes, and NSIS has no backslash escaping (its escape is $\").
  ; Requires BOTH a Valid status AND a Microsoft subject; anything else, including
  ; a probe that throws, is a failure.
  FileOpen $0 "$TEMP\OCA-Install\sig-check.ps1" w
  FileWrite $0 "$$ErrorActionPreference = 'SilentlyContinue'$\r$\n"
  FileWrite $0 "$$f = $$env:OCA_SIGFILE$\r$\n"
  FileWrite $0 "if (-not (Test-Path $$f)) { Write-Output 'RESULT-SIG-FAIL missing'; exit }$\r$\n"
  FileWrite $0 "$$s = $$null$\r$\n"
  FileWrite $0 "try { $$s = Get-AuthenticodeSignature -LiteralPath $$f } catch { Write-Output 'RESULT-SIG-FAIL probe-error'; exit }$\r$\n"
  FileWrite $0 "if (-not $$s) { Write-Output 'RESULT-SIG-FAIL no-result'; exit }$\r$\n"
  FileWrite $0 "$$subj = ''$\r$\n"
  FileWrite $0 "if ($$s.SignerCertificate) { $$subj = $$s.SignerCertificate.Subject }$\r$\n"
  FileWrite $0 "if (($$s.Status -eq 'Valid') -and ($$subj -like '*Microsoft*')) { Write-Output 'RESULT-SIG-OK' }$\r$\n"
  FileWrite $0 "else { Write-Output ('RESULT-SIG-FAIL status=' + $$s.Status + ' subject=' + $$subj) }$\r$\n"
  FileClose $0

  ; backend-check.ps1 — THE decisive engine check. Runs the freshly extracted
  ; llama-server.exe with --version and looks for llama.cpp's own
  ;   load_backend: loaded <Name> backend from <path>
  ; line. This is the identical predicate used by
  ; crates/offline-intelligence/src/engine_management/dll_manager.rs
  ; (same needle, same 30s bound), which is what makes install-time success a
  ; real guarantee of boot-time success rather than a hope.
  ;
  ; Why --version and not a spawn check: verified empirically (see dll_manager
  ; module docs) that --version exits 0 even with every backend DLL deleted,
  ; because backends are LoadLibrary'd at runtime rather than hard-imported. A
  ; process that merely starts proves nothing; the self-reported line does.
  ;
  ; WorkingDirectory is the engine directory so the backend DLLs resolve the
  ; same way they will in production.
  FileOpen $0 "$TEMP\OCA-Install\backend-check.ps1" w
  FileWrite $0 "$$ErrorActionPreference = 'SilentlyContinue'$\r$\n"
  FileWrite $0 "$$exe = $$env:OCA_EXE$\r$\n"
  FileWrite $0 "$$accel = $$env:OCA_ACCEL$\r$\n"
  FileWrite $0 "if (-not (Test-Path $$exe)) { Write-Output 'RESULT-BACKEND-FAIL missing-binary'; exit }$\r$\n"
  FileWrite $0 "$$psi = New-Object System.Diagnostics.ProcessStartInfo$\r$\n"
  FileWrite $0 "$$psi.FileName = $$exe$\r$\n"
  FileWrite $0 "$$psi.Arguments = '--version'$\r$\n"
  FileWrite $0 "$$psi.WorkingDirectory = Split-Path $$exe$\r$\n"
  FileWrite $0 "$$psi.RedirectStandardOutput = $$true$\r$\n"
  FileWrite $0 "$$psi.RedirectStandardError = $$true$\r$\n"
  FileWrite $0 "$$psi.UseShellExecute = $$false$\r$\n"
  FileWrite $0 "$$psi.CreateNoWindow = $$true$\r$\n"
  FileWrite $0 "$$p = $$null$\r$\n"
  FileWrite $0 "try { $$p = [System.Diagnostics.Process]::Start($$psi) } catch { Write-Output 'RESULT-BACKEND-FAIL launch-refused'; exit }$\r$\n"
  FileWrite $0 "if (-not $$p) { Write-Output 'RESULT-BACKEND-FAIL launch-refused'; exit }$\r$\n"
  FileWrite $0 "$$o = $$p.StandardOutput.ReadToEndAsync()$\r$\n"
  FileWrite $0 "$$e = $$p.StandardError.ReadToEndAsync()$\r$\n"
  FileWrite $0 "if (-not $$p.WaitForExit(30000)) { try { $$p.Kill() } catch {}; Write-Output 'RESULT-BACKEND-FAIL timeout'; exit }$\r$\n"
  FileWrite $0 "$$txt = $$o.Result + $$e.Result$\r$\n"
  FileWrite $0 "if ($$txt -match ('loaded ' + [regex]::Escape($$accel) + ' backend')) { Write-Output 'RESULT-BACKEND-OK' }$\r$\n"
  FileWrite $0 "else { Write-Output ('RESULT-BACKEND-FAIL no-load-line :: ' + (($$txt -split $\"`r?`n$\") -match 'load_backend' -join ' | ')) }$\r$\n"
  FileClose $0
FunctionEnd

; ── Engine backend launch verification (Phase 1) ──────────────────────────────
; Inputs : $EngineDestPath, $EngineAcceleration
; Output : $BackendOk ("1" when the engine self-reported loading its backend)
; A failure here is never fatal by itself — the caller demotes and re-runs the
; decision table. Any non-OK result (launch refused, timeout, no load line) is
; treated as failure; there is no "assume it worked" branch.
Function oci_VerifyBackendLoads
  StrCpy $BackendOk "0"
  DetailPrint "Verifying the $EngineAcceleration backend actually loads..."
  System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_EXE", t "$EngineDestPath\llama-server.exe")i'
  System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_ACCEL", t "$EngineAcceleration")i'
  nsExec::ExecToStack '"$PwshPath" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$TEMP\OCA-Install\backend-check.ps1"'
  Pop $0 ; exit code
  Pop $1 ; output
  DetailPrint "Backend verification: $1"
  Push $1
  Push "RESULT-BACKEND-OK"
  Call oci_StrContains
  Pop $2
  ${If} $2 != ""
    StrCpy $BackendOk "1"
    DetailPrint "$EngineAcceleration backend confirmed loaded"
  ${EndIf}
FunctionEnd

; ── CUDA capability probe ─────────────────────────────────────────────────────
; Runs cuda-check.ps1 with OCA_SMI pointing at nvidia-smi.exe.
; Sets $CudaCapable to "13", "12", or "0" (matches the decision-table rows).
Function oci_CheckCudaCapable
  StrCpy $CudaCapable "0"
  System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_SMI", t "$NvidiaSmiPath")i'
  nsExec::ExecToStack '"$PwshPath" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$TEMP\OCA-Install\cuda-check.ps1"'
  Pop $0 ; exit code
  Pop $1 ; output
  DetailPrint "CUDA capability probe: $1"
  Push $1
  Push "RESULT-CUDA13"
  Call oci_StrContains
  Pop $2
  ${If} $2 != ""
    StrCpy $CudaCapable "13"
    Return
  ${EndIf}
  Push $1
  Push "RESULT-CUDA12"
  Call oci_StrContains
  Pop $2
  ${If} $2 != ""
    StrCpy $CudaCapable "12"
  ${EndIf}
FunctionEnd

; ── Vulkan ICD probe ──────────────────────────────────────────────────────────
; Runs vulkan-check.ps1.  Sets $VulkanAvailable to "1" when a real Vulkan
; driver ICD is registered and the loader DLL exists.
Function oci_CheckVulkanAvailable
  StrCpy $VulkanAvailable "0"
  nsExec::ExecToStack '"$PwshPath" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$TEMP\OCA-Install\vulkan-check.ps1"'
  Pop $0 ; exit code
  Pop $1 ; output
  DetailPrint "Vulkan ICD probe: $1"
  Push $1
  Push "RESULT-VULKAN"
  Call oci_StrContains
  Pop $2
  ${If} $2 != ""
    StrCpy $VulkanAvailable "1"
  ${EndIf}
FunctionEnd

; ── Platform support gate ─────────────────────────────────────────────────────
; Pushes "1" when this machine can run the application, "0" otherwise (after
; telling the user exactly why). See SUPPORTED PLATFORMS at the top of the file.
;
; Two independent refusals:
;   a) Windows older than 10 build 17134. Tauri v2 and the Evergreen WebView2
;      runtime do not support Windows 7/8/8.1 at all, and this installer needs
;      curl.exe, PowerShell 5's Expand-Archive, and Win10-format certutil output.
;   b) 32-bit x86 Windows. The application binary is x86_64 (or ARM64); there is
;      no 32-bit build, so nothing we could install here would run.
; ARM64 hosts are SUPPORTED on every Windows 10 ARM build: the ARM64 application
; and the ARM64 llama.cpp engine both run natively, so the x64-emulation
; capability that Windows 10 ARM lacks is never needed.
Function oci_CheckPlatformSupported
  ; (b) 32-bit x86 Windows. This installer is a 32-bit process, so on 64-bit
  ; Windows PROCESSOR_ARCHITECTURE reads "x86" and PROCESSOR_ARCHITEW6432 holds
  ; the real architecture. On genuinely 32-bit Windows, ARCHITEW6432 is unset.
  ClearErrors
  ReadEnvStr $R0 "PROCESSOR_ARCHITEW6432"
  ReadEnvStr $R1 "PROCESSOR_ARCHITECTURE"

  ; Establish the host architecture here, before anything needs it. The engine
  ; decision table re-derives this independently later; both read the same two
  ; variables, so they cannot disagree.
  StrCpy $IsArm64Host "0"
  ${If} $R0 == "ARM64"
  ${OrIf} $R1 == "ARM64"
    StrCpy $IsArm64Host "1"
  ${EndIf}

  ${If} $R0 == ""
  ${AndIf} $R1 == "x86"
    DetailPrint "Unsupported: 32-bit x86 Windows (no 32-bit build of this application exists)"
    MessageBox MB_ICONSTOP|MB_OK \
      "Offline Counsel AI requires 64-bit Windows.$\n$\nThis computer is running 32-bit Windows, which cannot run the application or its inference engine.$\n$\nNo inference engine was installed."
    Push "0"
    Return
  ${EndIf}

  ; (a) Build number floor. CurrentBuildNumber is a REG_SZ under the native
  ; SOFTWARE hive, so the 64-bit registry view is required from this 32-bit process.
  SetRegView 64
  ClearErrors
  ReadRegStr $R2 HKLM "SOFTWARE\Microsoft\Windows NT\CurrentVersion" "CurrentBuildNumber"
  SetRegView 32
  ${If} ${Errors}
  ${OrIf} $R2 == ""
    ; Unreadable build number: do NOT refuse on a failed probe. Every supported
    ; OS reports it; an unreadable value means the probe failed, not that the OS
    ; is old. The engine's own launch verification is the real gate anyway.
    DetailPrint "Could not read Windows build number — continuing (engine verification still applies)"
    Push "1"
    Return
  ${EndIf}

  IntOp $R3 $R2 + 0
  DetailPrint "Windows build: $R3 (minimum supported: ${MIN_WINDOWS_BUILD})"
  ${If} $R3 < ${MIN_WINDOWS_BUILD}
    MessageBox MB_ICONSTOP|MB_OK \
      "Offline Counsel AI requires Windows 10 version 1803 (build ${MIN_WINDOWS_BUILD}) or newer.$\n$\nThis computer reports build $R3.$\n$\nNo inference engine was installed."
    Push "0"
    Return
  ${EndIf}

  Push "1"
FunctionEnd

; ── MSVC runtime (Phase 2) ────────────────────────────────────────────────────
; llama.cpp's engines import the VC++ 2015-2022 runtime. The GPU backends need
; VCRUNTIME140_1.dll, which the CPU/RPC backends and llama-server.exe itself do
; NOT — verified from the b8037 PE import directories (see MSVC RUNTIME at the
; top of this file). A machine with an older 14.0-14.16 redist therefore runs
; the engine fine but silently loses GPU acceleration, so all three DLLs are
; required, not just the two the main binary imports.
;
; The DLL files are the ground truth, not the registry: a version key can be
; present after a partially removed or repaired install. We check what actually
; has to load.
Function oci_EnsureVcRuntime
  ; Resolve the native System32. Sysnative is the 32-bit alias for the real
  ; System32; $SYSDIR from this 32-bit process is SysWOW64, whose copies of
  ; these DLLs are the 32-bit ones and irrelevant to a 64-bit engine.
  StrCpy $Sys64Dir "$WINDIR\Sysnative"
  IfFileExists "$Sys64Dir\kernel32.dll" +2
    StrCpy $Sys64Dir "$SYSDIR"

  Call oci_CheckVcRuntimeFiles
  ${If} $VcRuntimeOk == "1"
    DetailPrint "Microsoft Visual C++ runtime present (VCRUNTIME140, VCRUNTIME140_1, MSVCP140)"
    Return
  ${EndIf}

  DetailPrint "Microsoft Visual C++ 2015-2022 Redistributable missing or too old — installing"

  ${If} $IsArm64Host == "1"
    StrCpy $DlUrl "${VCREDIST_ARM64_URL}"
  ${Else}
    StrCpy $DlUrl "${VCREDIST_X64_URL}"
  ${EndIf}
  StrCpy $DlDest "$TEMP\OCA-Install\vc_redist.exe"

 oci_VcAttempt:
  Call oci_DownloadFile
  ${If} $DlExit != "0"
    DetailPrint "VC++ redistributable download failed (exit $DlExit)."
    MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
      "Could not download the Microsoft Visual C++ Redistributable (exit code $DlExit).$\n$\nWithout it, GPU acceleration will be unavailable and the engine may not start at all.$\n$\nRetry, or Cancel to continue without it." \
      IDRETRY oci_VcAttempt
    Return
  ${EndIf}

  ; Integrity for a mutable vendor binary: Authenticode, not a SHA256 pin.
  ; See the VCREDIST_* defines for why. An unsigned or non-Microsoft file is
  ; deleted and never executed.
  Call oci_VerifyMicrosoftSignature
  ${If} $HashOk != "1"
    Delete "$DlDest"
    DetailPrint "VC++ redistributable failed Authenticode verification — deleted, not executed."
    MessageBox MB_ICONEXCLAMATION|MB_OK \
      "The downloaded Microsoft Visual C++ Redistributable was not validly signed by Microsoft and has been deleted.$\n$\nIt was not run. Please check your network connection for interception, then reinstall."
    Return
  ${EndIf}

  DetailPrint "Signature verified — installing Visual C++ runtime (this may take a minute)..."
  ExecWait '"$DlDest" /install /quiet /norestart' $0
  Delete "$DlDest"

  ; 0 = installed, 3010 = installed, reboot pending, 1638 = a newer version is
  ; already present. All three mean the runtime is in place.
  ${If} $0 == "0"
  ${OrIf} $0 == "3010"
  ${OrIf} $0 == "1638"
    DetailPrint "Visual C++ runtime installer completed (exit $0)"
  ${Else}
    DetailPrint "Visual C++ runtime installer returned $0"
  ${EndIf}

  ; The installer's exit code is a claim; the DLLs on disk are the fact.
  Call oci_CheckVcRuntimeFiles
  ${If} $VcRuntimeOk == "1"
    DetailPrint "Visual C++ runtime verified present after install"
  ${Else}
    DetailPrint "Visual C++ runtime still incomplete after install (exit $0)"
    MessageBox MB_ICONEXCLAMATION|MB_OK \
      "The Microsoft Visual C++ Redistributable did not install completely (exit code $0).$\n$\nSetup will continue and verify the engine directly. If GPU acceleration is unavailable, install the 'Microsoft Visual C++ 2015-2022 Redistributable (x64)' manually and reinstall this application."
  ${EndIf}
FunctionEnd

; Sets $VcRuntimeOk to "1" only when ALL THREE runtime DLLs the engines import
; are present in the native System32.
Function oci_CheckVcRuntimeFiles
  StrCpy $VcRuntimeOk "0"
  IfFileExists "$Sys64Dir\VCRUNTIME140.dll" 0 oci_VcMissing
  IfFileExists "$Sys64Dir\VCRUNTIME140_1.dll" 0 oci_VcMissing
  IfFileExists "$Sys64Dir\MSVCP140.dll" 0 oci_VcMissing
  StrCpy $VcRuntimeOk "1"
  Return
 oci_VcMissing:
FunctionEnd

; ── Authenticode verification ─────────────────────────────────────────────────
; Input  : $DlDest (file to verify)
; Output : $HashOk ("1" when validly signed by Microsoft)
; Requires BOTH a Valid signature status AND a Microsoft subject — a validly
; signed file from someone else must not pass. A probe failure is a failure,
; never a pass.
Function oci_VerifyMicrosoftSignature
  StrCpy $HashOk "0"
  System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_SIGFILE", t "$DlDest")i'
  nsExec::ExecToStack '"$PwshPath" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$TEMP\OCA-Install\sig-check.ps1"'
  Pop $0 ; exit code
  Pop $1 ; output
  DetailPrint "Signature check: $1"
  Push $1
  Push "RESULT-SIG-OK"
  Call oci_StrContains
  Pop $2
  ${If} $2 != ""
    StrCpy $HashOk "1"
  ${EndIf}
FunctionEnd

; ── ARM64 PDFium ──────────────────────────────────────────────────────────────
; Replaces the bundled x64 pdfium.dll next to the executable with the native
; ARM64 build. Without this the app runs (it is a native ARM64 build) but PDF
; text extraction and scanned-PDF OCR fail at load time, because an x64 DLL
; cannot be loaded into an ARM64 process.
;
; Non-fatal: a failure here costs PDF support, not the application. Every other
; feature — chat, inference, all non-PDF document types — is unaffected, so
; aborting the install would destroy far more than it protects. The failure is
; reported plainly rather than swallowed.
;
; Extraction uses tar.exe (bsdtar, in Windows since 10 1803) because this asset
; is a .tar.gz, which Expand-Archive cannot read.
Function oci_EnsureArm64Pdfium
  DetailPrint "ARM64 host — fetching the native ARM64 PDFium (PDF support)"

 oci_PdfiumAttempt:
  StrCpy $DlUrl  "${PDFIUM_ARM64_URL}"
  StrCpy $DlDest "$TEMP\OCA-Install\${PDFIUM_ARM64_ASSET}"
  Call oci_DownloadFile
  ${If} $DlExit != "0"
    DetailPrint "ARM64 PDFium download failed (exit $DlExit)."
    MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
      "Could not download the ARM64 PDF engine (exit code $DlExit).$\n$\nEverything except PDF reading will work normally.$\n$\nRetry, or Cancel to continue without PDF support." \
      IDRETRY oci_PdfiumAttempt
    Return
  ${EndIf}

  StrCpy $HashTarget "$DlDest"
  StrCpy $ExpectedSha "${PDFIUM_ARM64_SHA256}"
  Call oci_VerifySha256
  ${If} $HashOk != "1"
    Delete "$DlDest"
    DetailPrint "ARM64 PDFium failed SHA256 verification — deleted."
    MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
      "The ARM64 PDF engine failed its integrity check (SHA256 mismatch) and was deleted.$\n$\nRetry, or Cancel to continue without PDF support." \
      IDRETRY oci_PdfiumAttempt
    Return
  ${EndIf}

  ; tar.exe lives in the real System32; from this 32-bit process that is
  ; Sysnative. Extract into a scratch dir, then place the DLL beside the exe.
  CreateDirectory "$TEMP\OCA-Install\pdfium"
  StrCpy $R5 "$WINDIR\Sysnative\tar.exe"
  IfFileExists "$R5" +2
    StrCpy $R5 "$SYSDIR\tar.exe"

  IfFileExists "$R5" 0 oci_PdfiumNoTar
  nsExec::ExecToLog '"$R5" -xzf "$DlDest" -C "$TEMP\OCA-Install\pdfium"'
  Pop $0
  Delete "$DlDest"

  IfFileExists "$TEMP\OCA-Install\pdfium\bin\pdfium.dll" 0 oci_PdfiumNoDll
  ; Overwrite the bundled x64 copy. CopyFiles' destination is a DIRECTORY, not
  ; a file path — the source file keeps its name — so $INSTDIR is passed here,
  ; not "$INSTDIR\pdfium.dll".
  SetOverwrite on
  Delete "$INSTDIR\pdfium.dll"
  CopyFiles /SILENT "$TEMP\OCA-Install\pdfium\bin\pdfium.dll" "$INSTDIR"
  RMDir /r "$TEMP\OCA-Install\pdfium"

  IfFileExists "$INSTDIR\pdfium.dll" 0 oci_PdfiumCopyFailed
  DetailPrint "ARM64 PDFium installed to $INSTDIR\pdfium.dll"
  Return

 oci_PdfiumNoTar:
  Delete "$DlDest"
  DetailPrint "tar.exe not found — cannot extract the ARM64 PDF engine."
  Goto oci_PdfiumWarn

 oci_PdfiumNoDll:
  RMDir /r "$TEMP\OCA-Install\pdfium"
  DetailPrint "bin/pdfium.dll not present in the extracted archive (tar exit $0)."
  Goto oci_PdfiumWarn

 oci_PdfiumCopyFailed:
  DetailPrint "Could not copy pdfium.dll into $INSTDIR."
  Goto oci_PdfiumWarn

 oci_PdfiumWarn:
  MessageBox MB_ICONEXCLAMATION|MB_OK \
    "The ARM64 PDF engine could not be installed.$\n$\nThe application will work normally except that PDF files cannot be read. Reinstalling may resolve this."
FunctionEnd

; ── Verification log ──────────────────────────────────────────────────────────
; Records which accelerations proved unusable on THIS machine so neither the
; installer nor the app retries them. Schema is shared with
; crates/offline-intelligence/src/engine_management/verification_log.rs — keep
; the two in sync.
;
; Keyed on (acceleration, engine_version): a llama.cpp version bump clears the
; record, so a demotion is never permanent by accident — the next release gets
; a fresh chance on the same hardware.
;
; Rewritten in full from the exclusion flags on every demotion, so the file is
; always internally consistent regardless of where the install stopped.
Function oci_WriteVerificationLog
  FileOpen $0 "$AppDataPath\engines\verification.json" w
  FileWrite $0 '{$\n'
  FileWrite $0 '  "schema_version": 1,$\n'
  FileWrite $0 '  "failures": [$\n'

  StrCpy $R4 ""  ; tracks whether a comma is needed before the next entry

  ${If} $ExcludeCuda == "1"
    FileWrite $0 '    {$\n'
    FileWrite $0 '      "acceleration": "CUDA",$\n'
    FileWrite $0 '      "engine_version": "${ENGINE_VERSION}",$\n'
    FileWrite $0 '      "reason": "Engine never self-reported loading its CUDA backend when launched with --version during installation",$\n'
    FileWrite $0 '      "source": "installer"$\n'
    FileWrite $0 '    }'
    StrCpy $R4 ","
  ${EndIf}

  ${If} $ExcludeVulkan == "1"
    ${If} $R4 == ","
      FileWrite $0 ',$\n'
    ${EndIf}
    FileWrite $0 '    {$\n'
    FileWrite $0 '      "acceleration": "Vulkan",$\n'
    FileWrite $0 '      "engine_version": "${ENGINE_VERSION}",$\n'
    FileWrite $0 '      "reason": "Engine never self-reported loading its Vulkan backend when launched with --version during installation",$\n'
    FileWrite $0 '      "source": "installer"$\n'
    FileWrite $0 '    }'
  ${EndIf}

  FileWrite $0 '$\n  ]$\n'
  FileWrite $0 '}$\n'
  FileClose $0
  DetailPrint "Recorded backend verification failure in $AppDataPath\engines\verification.json"
FunctionEnd

; ── Download helper ───────────────────────────────────────────────────────────
; Inputs : $DlUrl (source), $DlDest (destination file)
; Output : $DlExit ("0" on success)
; curl.exe (ships with Windows 10 1803+) is the primary downloader — it follows
; GitHub's 302 redirects and shows progress.  On machines without curl (older
; Windows 10 builds) or when curl fails, falls back to PowerShell
; Invoke-WebRequest with TLS 1.2 forced (required for GitHub).
; NOTE: the curl->PowerShell sequence is a transport retry for the SAME url and
; destination — the artifact is identical and is hash-verified afterwards, so
; this is not an integrity-relevant fallback.
Function oci_DownloadFile
  StrCpy $DlExit "1"

  IfFileExists "$CurlPath" 0 oci_DlPowershell
  ExecWait '"$CurlPath" -L -f --connect-timeout 30 --max-time 1800 -# -o "$DlDest" "$DlUrl"' $DlExit
  ${If} $DlExit == "0"
    IfFileExists "$DlDest" oci_DlDone 0
  ${EndIf}
  DetailPrint "curl download failed (exit $DlExit) — retrying with PowerShell"

 oci_DlPowershell:
  System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_URL", t "$DlUrl")i'
  System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_OUT", t "$DlDest")i'
  nsExec::ExecToLog '"$PwshPath" -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -Uri $$env:OCA_URL -OutFile $$env:OCA_OUT -UseBasicParsing -TimeoutSec 1800"'
  Pop $DlExit

 oci_DlDone:
  ; Final arbiter: the file must exist on disk
  IfFileExists "$DlDest" +2
    StrCpy $DlExit "1"
FunctionEnd

; ── SHA256 verification ───────────────────────────────────────────────────────
; Inputs : $HashTarget (file), $ExpectedSha (lowercase hex, no spaces)
; Output : $HashOk ("1" on match)
; Uses certutil.exe (part of Windows since XP; Windows 10 prints lowercase hex
; without byte separators). The comparison itself is case-insensitive
; (oci_StrContains uses NSIS StrCmp semantics). A certutil failure counts as a
; verification failure — never as a pass.
Function oci_VerifySha256
  StrCpy $HashOk "0"
  nsExec::ExecToStack '"$SYSDIR\certutil.exe" -hashfile "$HashTarget" SHA256'
  Pop $0 ; exit code
  Pop $1 ; output (line 2 is the hex digest)
  ${If} $0 != "0"
    DetailPrint "certutil failed (exit $0) — cannot verify download integrity"
    Return
  ${EndIf}
  Push $1
  Push "$ExpectedSha"
  Call oci_StrContains
  Pop $2
  ${If} $2 != ""
    StrCpy $HashOk "1"
    DetailPrint "SHA256 verified: $ExpectedSha"
  ${Else}
    DetailPrint "SHA256 MISMATCH — expected $ExpectedSha"
    DetailPrint "certutil output: $1"
  ${EndIf}
FunctionEnd

; ── Case-insensitive substring search (NSIS helper) ─────────────────────────
; Stack: (input_string, search_term) -> returns "" if not found, or matched portion
Function oci_StrContains
  Exch $R5 ; search_term
  Exch
  Exch $R6 ; input_string
  Push $R7
  Push $R8
  Push $R9

  StrCpy $R7 0
  StrLen $R8 $R5
  StrCpy $R9 ""

  oci_StrLoop:
    StrCpy $R9 $R6 $R8 $R7
    ${If} $R9 == ""
      StrCpy $R6 ""
      Goto oci_StrExit
    ${EndIf}
    ${If} $R9 == $R5
      Goto oci_StrExit
    ${EndIf}
    IntOp $R7 $R7 + 1
    Goto oci_StrLoop

  oci_StrExit:
    Pop $R9
    Pop $R8
    Pop $R7
    Pop $R5
    Exch $R6
FunctionEnd

; ── CUDA runtime DLL download ─────────────────────────────────────────────────
; The main CUDA engine ZIP does NOT include the CUDA runtime DLLs
; (cudart64_*, cublas64_*, cublasLt64_*). They ship in the separate cudart
; package selected in $CudartFilename/$CudartSha256 and must be extracted flat
; alongside llama-server.exe.
;
; Sets $CudartOk to "1" ONLY when the package downloaded, hash-verified,
; extracted, AND both cudart64_* and cublas64_* DLLs are present on disk.
; The caller treats anything else as fatal and rolls back the engine —
; `llama-server --version` cannot exercise cuBLAS, so this file-presence check
; is the only pre-flight signal for the runtime package.
Function oci_DownloadCudaRuntimeDlls
  StrCpy $CudartOk "0"

 oci_CudartAttempt:
  DetailPrint "Downloading CUDA runtime DLLs: $CudartFilename"

  StrCpy $DlUrl  "${LLAMA_BASE_URL}/$CudartFilename"
  StrCpy $DlDest "$TEMP\OCA-Install\$CudartFilename"
  Call oci_DownloadFile

  ${If} $DlExit != "0"
    DetailPrint "CUDA runtime download failed (exit code $DlExit)."
    MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
      "The CUDA runtime download failed (exit code $DlExit).$\n$\nRetry to try again, or Cancel to abort the CUDA engine install." \
      IDRETRY oci_CudartAttempt
    Return
  ${EndIf}

  ; Integrity: pinned SHA256 for the cudart package
  StrCpy $HashTarget "$TEMP\OCA-Install\$CudartFilename"
  StrCpy $ExpectedSha "$CudartSha256"
  Call oci_VerifySha256
  ${If} $HashOk != "1"
    Delete "$TEMP\OCA-Install\$CudartFilename"
    DetailPrint "CUDA runtime package failed SHA256 verification."
    MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
      "The CUDA runtime package failed its integrity check (SHA256 mismatch) and was deleted.$\n$\nRetry to download again, or Cancel to abort the CUDA engine install." \
      IDRETRY oci_CudartAttempt
    Return
  ${EndIf}

  DetailPrint "CUDA runtime verified — extracting..."
  ; Extract flat into the same engine directory so DLLs sit alongside the exe
  System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_ZIP", t "$TEMP\OCA-Install\$CudartFilename")i'
  System::Call 'Kernel32::SetEnvironmentVariable(t "OCA_DST", t "$EngineDestPath")i'
  nsExec::ExecToLog '"$PwshPath" -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "Expand-Archive -LiteralPath $$env:OCA_ZIP -DestinationPath $$env:OCA_DST -Force"'
  Pop $0
  Delete "$TEMP\OCA-Install\$CudartFilename"

  ; Both DLL families must be present (versioned names differ between CUDA 12
  ; and 13, so wildcards are used: cudart64_12.dll vs cudart64_13.dll etc.)
  ClearErrors
  FindFirst $0 $1 "$EngineDestPath\cudart64_*.dll"
  ${If} $1 == ""
    FindClose $0
    DetailPrint "cudart64_*.dll not found after extraction — CUDA runtime incomplete."
    Return
  ${EndIf}
  FindClose $0

  ClearErrors
  FindFirst $0 $1 "$EngineDestPath\cublas64_*.dll"
  ${If} $1 == ""
    FindClose $0
    DetailPrint "cublas64_*.dll not found after extraction — CUDA runtime incomplete."
    Return
  ${EndIf}
  FindClose $0

  DetailPrint "CUDA runtime DLLs installed and verified alongside the engine binary"
  StrCpy $CudartOk "1"
FunctionEnd

; ── Write metadata.json ───────────────────────────────────────────────────────
; Must match EngineInfo struct in engine_management/registry.rs exactly.
; install_path is written as JSON null so the Rust scanner overwrites it with
; the real on-disk path — avoids Windows backslash escaping issues in JSON.
; Called ONLY after every verification step succeeded — its presence is the
; on-disk marker for "complete engine".
Function oci_WriteEngineMetadata
  DetailPrint "Writing engine metadata to $EngineDestPath\metadata.json"
  FileOpen $0 "$EngineDestPath\metadata.json" w
  FileWrite $0 '{$\n'
  FileWrite $0 '  "id": "$EngineId",$\n'
  FileWrite $0 '  "name": "$EngineName",$\n'
  FileWrite $0 '  "version": "${ENGINE_VERSION}",$\n'
  FileWrite $0 '  "platform": "Windows",$\n'
  FileWrite $0 '  "architecture": "$EngineArch",$\n'
  FileWrite $0 '  "acceleration": "$EngineAcceleration",$\n'
  FileWrite $0 '  "download_url": "$EngineDownloadUrl",$\n'
  FileWrite $0 '  "file_size": 0,$\n'
  FileWrite $0 '  "checksum": "$EngineSha256",$\n'
  FileWrite $0 '  "compatibility_score": 90.0,$\n'
  FileWrite $0 '  "status": "Installed",$\n'
  FileWrite $0 '  "install_path": null,$\n'
  FileWrite $0 '  "binary_name": "llama-server.exe",$\n'
  FileWrite $0 '  "required_dependencies": []$\n'
  FileWrite $0 '}$\n'
  FileClose $0
  DetailPrint "Engine metadata written"
FunctionEnd

; ----------------------------------------------------------------------------
; oci_InstallEmbeddingModel
;
; Downloads the pinned multilingual embedding model into
;   $AppDataPath\embeddings\<id>\<file>
; resuming a partial download, verifies size + SHA256, then writes
; verified.json (the app trusts the file without re-hashing 639 MB on every
; start). Skipped when a verified copy is already present. Failure never
; aborts setup: the Models page offers the same download.
; ----------------------------------------------------------------------------
Function oci_InstallEmbeddingModel
  StrCpy $R6 "$AppDataPath\embeddings\${EMBED_MODEL_ID}"
  StrCpy $R7 "$R6\${EMBED_MODEL_FILE}"
  CreateDirectory "$AppDataPath\embeddings"
  CreateDirectory "$R6"

  IfFileExists "$R6\verified.json" 0 oci_EmbDownload
  IfFileExists "$R7" 0 oci_EmbDownload
  Push "$R7"
  Call oci_FileSize
  Pop $R8
  ${If} $R8 == "${EMBED_MODEL_SIZE}"
    DetailPrint "Embedding model already installed and verified — skipping download"
    Return
  ${EndIf}

 oci_EmbDownload:
  Delete "$R6\verified.json"
  DetailPrint "Downloading document-search model (${EMBED_MODEL_FILE}, about 610 MB)"
  DetailPrint "Source : ${EMBED_MODEL_URL}"
  DetailPrint "Target : $R7"

  StrCpy $DlExit "1"
  IfFileExists "$CurlPath" 0 oci_EmbFallback
  ExecWait '"$CurlPath" -L -f -C - --retry 3 --connect-timeout 30 --max-time 7200 -# -o "$R7.part" "${EMBED_MODEL_URL}"' $DlExit
  ${If} $DlExit == "0"
    Goto oci_EmbVerify
  ${EndIf}
  DetailPrint "curl download failed (exit $DlExit) — retrying from scratch"
  Delete "$R7.part"

 oci_EmbFallback:
  StrCpy $DlUrl  "${EMBED_MODEL_URL}"
  StrCpy $DlDest "$R7.part"
  Call oci_DownloadFile
  ${If} $DlExit != "0"
    Delete "$R7.part"
    MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
      "The document-search model could not be downloaded (exit code $DlExit).$\n$\nRetry to try again, or Cancel to skip. The application will offer this download on the Models page; until then document search uses keywords only." \
      IDRETRY oci_EmbDownload
    Return
  ${EndIf}

 oci_EmbVerify:
  Push "$R7.part"
  Call oci_FileSize
  Pop $R8
  ${If} $R8 != "${EMBED_MODEL_SIZE}"
    DetailPrint "Embedding model size mismatch ($R8 bytes, expected ${EMBED_MODEL_SIZE})"
    Delete "$R7.part"
    MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
      "The document-search model download was incomplete and was removed.$\n$\nRetry to download again, or Cancel to skip (it can be installed later from the Models page)." \
      IDRETRY oci_EmbDownload
    Return
  ${EndIf}

  StrCpy $HashTarget "$R7.part"
  StrCpy $ExpectedSha "${EMBED_MODEL_SHA256}"
  Call oci_VerifySha256
  ${If} $HashOk != "1"
    Delete "$R7.part"
    MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL \
      "The document-search model failed its integrity check (SHA256 mismatch) and was deleted.$\n$\nRetry to download again, or Cancel to skip (it can be installed later from the Models page)." \
      IDRETRY oci_EmbDownload
    Return
  ${EndIf}

  Delete "$R7"
  Rename "$R7.part" "$R7"
  IfFileExists "$R7" 0 oci_EmbRenameFailed
  FileOpen $R9 "$R6\verified.json" w
  FileWrite $R9 '{"id":"${EMBED_MODEL_ID}","sha256":"${EMBED_MODEL_SHA256}","size_bytes":${EMBED_MODEL_SIZE}}'
  FileClose $R9
  DetailPrint "Embedding model installed and verified"
  Return

 oci_EmbRenameFailed:
  DetailPrint "Could not move the verified embedding model into place"
FunctionEnd

; Push <path>; Call oci_FileSize; Pop <size in bytes or "-1">
Function oci_FileSize
  Exch $0
  Push $1
  ClearErrors
  FileOpen $1 "$0" r
  ${If} ${Errors}
    StrCpy $0 "-1"
  ${Else}
    FileSeek $1 0 END $0
    FileClose $1
  ${EndIf}
  Pop $1
  Exch $0
FunctionEnd
