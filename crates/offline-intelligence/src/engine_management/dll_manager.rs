//! Engine Backend Verification
//!
//! Verifies an installed engine can actually load its acceleration backend by
//! running the binary itself and inspecting its own self-reported diagnostics —
//! deliberately NOT by checking a hardcoded list of expected DLL filenames.
//!
//! ## Why not a static DLL manifest
//!
//! llama.cpp's packaging has changed shape between releases before (a single
//! `ggml.dll` became `ggml-base.dll` plus 14 per-microarchitecture
//! `ggml-cpu-*.dll` files, for example). A hardcoded filename list goes stale
//! the moment upstream repackages, and silently under- or over-verifies until
//! someone notices. This module instead asks the binary itself what it
//! actually loaded, which stays correct across repackaging.
//!
//! ## What was empirically verified (not assumed) before writing this
//!
//! Downloaded the real b8037 Windows CPU, Vulkan, and CUDA release ZIPs on a
//! real Windows machine (with a real NVIDIA GPU available for the CUDA case)
//! and ran `llama-server.exe --version` under various DLLs deliberately removed:
//!
//!   - `--version` exits 0 and prints normally even with ALL backend DLLs
//!     missing (CPU variants, `ggml-vulkan.dll`, or `ggml-cuda.dll` all
//!     individually removed) — the OS does NOT refuse to start the process,
//!     because these are loaded dynamically (`LoadLibrary`) at runtime, not
//!     linked as hard imports. A plain spawn-success check is NOT sufficient.
//!   - The one reliable signal: llama-server prints
//!       `load_backend: loaded <Name> backend from <path>`
//!     to stderr for every backend it actually finds and loads. This line is
//!     present when the corresponding DLL exists and disappears when it
//!     doesn't — confirmed for the CPU backend (`ggml-cpu-*.dll`), the Vulkan
//!     backend (`ggml-vulkan.dll`), and the CUDA backend (`ggml-cuda.dll`).
//!   - CUDA's separate `cudart` package (`cublas64_12.dll`, `cublasLt64_12.dll`,
//!     `cudart64_12.dll`) is genuinely NOT exercised by `--version` at all:
//!     `ggml-cuda.dll` loads and enumerates the real GPU successfully even
//!     with ZERO cudart DLLs present, because cuBLAS is only actually invoked
//!     during real matrix multiplication at inference time. There is no
//!     pre-flight CLI check that can verify cudart completeness — this is a
//!     genuine limitation, not something to paper over with a fake check. The
//!     only verifiable signal for that package is whether ITS OWN download and
//!     extraction operations reported success (handled in downloader.rs,
//!     which now treats that as fatal on failure).
//!
//! `llama.cpp`'s backend-name strings ("CPU", "Vulkan", "CUDA") match this
//! codebase's `AccelerationType::Display` output exactly for the three
//! variants verified above. Metal/OpenCL/DirectML are not verified the same
//! way (no Mac/AMD/Intel hardware available in this session) — see the
//! comment on `expected_backend_name` for what that means in practice.

use std::path::Path;
use tokio::process::Command;

use super::registry::{AccelerationType, EngineInfo};

/// Result of launching the engine binary and inspecting which backend it
/// self-reported loading.
#[derive(Debug, Clone)]
pub struct BackendLoadResult {
    /// The exact `load_backend: loaded <Name> backend` line(s) found, if any —
    /// kept for diagnostics/logging, not just a bool.
    pub matched_lines: Vec<String>,
    /// Full captured stdout+stderr, for error reporting when verification fails.
    pub raw_output: String,
}

impl BackendLoadResult {
    pub fn backend_loaded(&self) -> bool {
        !self.matched_lines.is_empty()
    }
}

pub struct DllManager;

impl DllManager {
    /// The substring llama.cpp prints when a given backend loads. Verified by
    /// direct observation for CPU/Vulkan/CUDA (see module doc). Metal/OpenCL/
    /// DirectML follow the same `AccelerationType::Display` string by
    /// extrapolation from llama.cpp's consistent backend-naming convention,
    /// not by direct observation — if one of those ever mismatches, this
    /// check will report a false "backend not loaded" failure rather than
    /// silently passing, which is the safe direction for an unverified case.
    fn expected_backend_name(acceleration: &AccelerationType) -> String {
        acceleration.to_string()
    }

    /// Launch the engine binary with `--version` (no model required, fast,
    /// no GPU work actually performed) and check its own stdout+stderr for a
    /// `load_backend: loaded <Name> backend` line matching the engine's
    /// declared acceleration type.
    ///
    /// Returns `Err` if the OS could not even start the process (binary
    /// missing/corrupted/truly hard-missing dependency). A successful launch
    /// with the expected backend line absent is reported via
    /// `BackendLoadResult::backend_loaded() == false`, not an `Err` — the
    /// caller decides how to treat that (currently: fatal, per the
    /// no-silent-degradation policy — see downloader.rs).
    pub async fn verify_backend_loads(
        install_path: &Path,
        engine: &EngineInfo,
    ) -> anyhow::Result<BackendLoadResult> {
        let binary_path = install_path.join(&engine.binary_name);

        if !binary_path.exists() {
            return Err(anyhow::anyhow!("Engine binary not found at {:?}", binary_path));
        }

        let mut cmd = Command::new(&binary_path);
        cmd.arg("--version");
        // A binary that never exits must not stall the boot scan forever;
        // kill_on_drop terminates it when the timeout below fires.
        cmd.kill_on_drop(true);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash

        let output = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Engine binary {:?} did not exit within 30s of `--version` — treating as \
                     unusable (it was terminated).",
                    binary_path
                )
            })?
            .map_err(|e| {
                anyhow::anyhow!(
                    "OS could not launch engine binary {:?}: {}. The binary may be corrupted.",
                    binary_path, e
                )
            })?;

        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let needle = format!("loaded {} backend", Self::expected_backend_name(&engine.acceleration));
        let matched_lines: Vec<String> = combined
            .lines()
            .filter(|line| line.contains(&needle))
            .map(|line| line.trim().to_string())
            .collect();

        Ok(BackendLoadResult {
            matched_lines,
            raw_output: combined,
        })
    }
}
