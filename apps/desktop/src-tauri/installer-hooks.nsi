; Offline Counsel AI — Tauri NSIS custom-install hook
; Included by Tauri's generated NSIS installer via installerHooks in tauri.conf.json.
; Runs AFTER Tauri has installed the main executable and Resources folder.
; Tauri v2 invokes !insertmacro NSIS_HOOK_POSTINSTALL (v1 used customInstall).
;
; DESIGN CONTRACT (no fallbacks, no silent failures):
;   - Hardware detection selects exactly ONE engine from the decision table
;     mirrored in crates/offline-intelligence/src/engine_management/registry.rs
;     (EngineRegistry::select_correct_engine). The two tables MUST stay in sync.
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
; Decision table (first match wins — same rows as select_correct_engine):
;   1. ARM64 host                                       -> win-cpu-arm64
;   2. x64 + NVIDIA + driver >= 580.0 + compute >= 7.5  -> win-cuda-13.1 (+cudart)
;   3. x64 + NVIDIA + driver >= 527.41 + compute >= 5.0 -> win-cuda-12.4 (+cudart)
;   4. x64 + any GPU with a real Vulkan ICD + loader    -> win-vulkan
;   5. x64 otherwise                                    -> win-cpu-x64
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
!define DISPLAY_CLASS_GUID "SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}"

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

; ── NSIS_HOOK_POSTINSTALL (called by Tauri v2's NSIS template) ────────────────
!macro NSIS_HOOK_POSTINSTALL

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

  ; C) Hardware detection -> sets engine variables (decision table, one result)
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
  oci_EngineDone:
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
  StrCpy $IsArm64Host "0"
  ClearErrors
  ReadEnvStr $0 "PROCESSOR_ARCHITEW6432"
  ${If} $0 == "ARM64"
    StrCpy $IsArm64Host "1"
  ${EndIf}

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

  ; Write the PowerShell capability probes used below
  Call oci_WriteDetectScripts

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

  ; ── Row 4: Any GPU candidate: Vulkan ICD pre-check ───────────────────────────
  ; A registered Vulkan ICD (legacy Khronos key or per-device VulkanDriverName)
  ; plus the loader DLL is required.  Loader-only systems (old Intel HD with no
  ; Vulkan driver) fall through to the CPU engine instead of a doomed install.
  ${If} $GpuCandidate == "Vulkan"
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
