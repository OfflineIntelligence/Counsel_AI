//! Engine Registry
//!
//! Manages the collection of available and installed llama.cpp engines,
//! tracks compatibility with hardware capabilities, and maintains
//! metadata about each engine.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, info, warn};

use crate::model_runtime::platform_detector::{HardwareCapabilities, Platform, HardwareArchitecture};

/// Single source of truth for the bundled llama.cpp engine version.
///
/// RELEASE CHECKLIST — when bumping this version, ALL of the following must be
/// updated together or downloads will fail loudly (hash mismatch / 404):
///   1. This constant.
///   2. ENGINE_ASSET_SHA256 below — fetch the new digests from
///      `https://api.github.com/repos/ggml-org/llama.cpp/releases/tags/<ver>`
///      (each asset carries a `digest` field; do NOT trust third-party mirrors).
///   3. apps/desktop/src-tauri/installer-hooks.nsi — ENGINE_VERSION define and
///      the per-asset SHA256 defines.
///   4. Confirm every asset name referenced by the catalog still exists in the
///      new release (llama.cpp has renamed assets between releases before).
pub const ENGINE_VERSION: &str = "b8037";

/// Pinned SHA256 digests for every release asset this codebase can download,
/// taken from the GitHub release API for ENGINE_VERSION. Downloads whose hash
/// does not match are hard failures — the file is deleted and the install is
/// rolled back. An asset with no entry here is refused outright (fail closed),
/// never downloaded unverified.
pub const ENGINE_ASSET_SHA256: &[(&str, &str)] = &[
    ("llama-b8037-bin-win-cpu-x64.zip",             "d7f460b1782e054b070f1a6345a652c6592faae4716da8584f6ac3dba8583caa"),
    ("llama-b8037-bin-win-cpu-arm64.zip",           "ffc80fb38b6061ef2195a792d65fa9c84eaccb5f289a43690d7ec0ed0bb114a3"),
    ("llama-b8037-bin-win-cuda-12.4-x64.zip",       "b31bfbc9c9f1e91a63471ceee9adaaeac7f626c8791f611502dd398bf852abe8"),
    ("cudart-llama-bin-win-cuda-12.4-x64.zip",      "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6"),
    ("llama-b8037-bin-win-cuda-13.1-x64.zip",       "4ba1fd0d12ea75fadb25ebe37e41fddf2551bd1a434fc184c846a2b3c963d83c"),
    ("cudart-llama-bin-win-cuda-13.1-x64.zip",      "f96935e7e385e3b2d0189239077c10fe8fd7e95690fea4afec455b1b6c7e3f18"),
    ("llama-b8037-bin-win-vulkan-x64.zip",          "c190664ddb25232bba6547df0802d057df52bce7fc3407c26a03a1c91fac4e57"),
    ("llama-b8037-bin-win-opencl-adreno-arm64.zip", "15802641f1ca6df4162246c92ae724793129efa274b13c2e31cdb44ff38ba812"),
    ("llama-b8037-bin-macos-arm64.tar.gz",          "cee819ec5258e4ce72bb44be80ea8690746192402b8f0f263417b6608c5c2d6b"),
    ("llama-b8037-bin-macos-x64.tar.gz",            "23a313c03f260843f272924ce73d88ed997b4fc0b1a3f807d624d3bef3c6073f"),
    ("llama-b8037-bin-ubuntu-x64.tar.gz",           "1c7e8593fbbdaa20d9cc20552c2164b409eca881969280693d1a921de9cb2e7e"),
    ("llama-b8037-bin-ubuntu-vulkan-x64.tar.gz",    "c7a1b7716fe57a3e5bb5ac94e2adc090adb5d266d43290fb17458e217f7d4728"),
];

/// Look up the pinned SHA256 for a release asset filename.
pub fn pinned_sha256(asset_filename: &str) -> Option<&'static str> {
    ENGINE_ASSET_SHA256
        .iter()
        .find(|(name, _)| *name == asset_filename)
        .map(|(_, hash)| *hash)
}

/// Types of hardware acceleration supported by llama.cpp engines
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AccelerationType {
    CPU,
    CUDA,
    Metal,
    Vulkan,
    OpenCL,
    DirectML,
}

impl std::fmt::Display for AccelerationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccelerationType::CPU => write!(f, "CPU"),
            AccelerationType::CUDA => write!(f, "CUDA"),
            AccelerationType::Metal => write!(f, "Metal"),
            AccelerationType::Vulkan => write!(f, "Vulkan"),
            AccelerationType::OpenCL => write!(f, "OpenCL"),
            AccelerationType::DirectML => write!(f, "DirectML"),
        }
    }
}

/// Status of an engine installation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EngineStatus {
    NotInstalled,
    Available,
    Installed,
    Active,
    Corrupted,
}

/// Information about a specific llama.cpp engine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub platform: Platform,
    pub architecture: HardwareArchitecture,
    pub acceleration: AccelerationType,
    pub download_url: String,
    pub file_size: u64,
    pub checksum: String,
    pub compatibility_score: f32,
    pub status: EngineStatus,
    pub install_path: Option<PathBuf>,
    pub binary_name: String,
    pub required_dependencies: Vec<String>,
    /// Why this engine is Corrupted (set by the boot-time scan when an on-disk
    /// engine fails verification). None for healthy engines. Additive field:
    /// serde(default) keeps NSIS-written and older metadata.json parseable.
    #[serde(default)]
    pub failure_reason: Option<String>,
}

impl EngineInfo {
    /// Calculate compatibility score for given hardware capabilities
    pub fn calculate_compatibility(&self, hardware: &HardwareCapabilities) -> f32 {
        let mut score: f32 = 0.0;
        
        // Platform match (highest priority)
        if self.platform == hardware.platform {
            score += 50.0;
        } else {
            return 0.0; // Incompatible platform
        }
        
        // Architecture match
        if self.architecture == hardware.architecture {
            score += 20.0;
        }
        
        // Acceleration support
        match (&self.acceleration, hardware) {
            (AccelerationType::CPU, _) => score += 15.0,
            // CUDA engines only score when the driver/compute-capability floors
            // are confirmed — an NVIDIA GPU behind a pre-CUDA-12 driver must not
            // beat a working Vulkan or CPU engine.
            (AccelerationType::CUDA, hw) if hw.cuda12_usable() => score += 25.0,
            (AccelerationType::Metal, hw) if hw.has_metal => score += 25.0,
            (AccelerationType::Vulkan, hw) if hw.has_vulkan => score += 20.0,
            // Adreno OpenCL (Windows-on-ARM Snapdragon): scores above CPU only
            // when the Qualcomm GPU is actually present, so an installed and
            // startup-verified OpenCL engine is preferred over plain CPU.
            (AccelerationType::OpenCL, hw) if hw.has_adreno_gpu => score += 18.0,
            _ => {
                // Unsupported acceleration type
                if self.acceleration != AccelerationType::CPU {
                    score -= 10.0;
                }
            }
        }
        
        // Version recency bonus
        if self.is_recent_version() {
            score += 5.0;
        }
        
        score.clamp(0.0, 100.0)
    }
    
    /// Check if this is a recent version (within last 6 months)
    fn is_recent_version(&self) -> bool {
        // Simplified version check - in practice this would parse version dates
        self.version.contains("b") || self.version.contains("latest")
    }
}

/// Manages the registry of available and installed engines
pub struct EngineRegistry {
    pub installed_engines: HashMap<String, EngineInfo>,
    pub available_engines: Vec<EngineInfo>,
    pub default_engine: Option<String>,
    pub storage_path: PathBuf,

}

impl EngineRegistry {
    pub fn new() -> Result<Self> {
        let storage_path = Self::get_engine_storage_path()?;
        std::fs::create_dir_all(&storage_path)?;
        
        Ok(Self {
            installed_engines: HashMap::new(),
            available_engines: Vec::new(),
            default_engine: None,
            storage_path,
        })
    }
    
    /// Get platform-appropriate storage path for engines
    fn get_engine_storage_path() -> Result<PathBuf> {
        Ok(crate::config::get_app_data_dir().join("engines"))
    }
    
    /// Scan for already installed engines in the storage directory
    pub async fn scan_installed_engines(&mut self, hardware_capabilities: &HardwareCapabilities) -> Result<()> {
        self.installed_engines.clear();

        if self.storage_path.exists() {
            for entry in std::fs::read_dir(&self.storage_path)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let engine_dir = entry.path();
                    match self.load_engine_metadata(engine_dir.clone()).await {
                        Some(engine_info) => {
                            self.installed_engines.insert(engine_info.id.clone(), engine_info);
                        }
                        None => {
                            // Check if directory contains a binary (orphaned engine)
                            if self.has_binary(&engine_dir) {
                                warn!("Found orphaned engine at {} (missing or invalid metadata.json). Consider re-downloading this engine.", engine_dir.display());
                            }
                        }
                    }
                }
            }
        }

        // Deterministic upgrade lifecycle: once a newer engine of the same
        // (platform, architecture, acceleration) triple has passed verification
        // in the loop above, older-version engines of that triple are removed.
        // Logged removal of a superseded duplicate, never a silent one — and
        // never before its replacement is verified.
        self.cleanup_superseded_engines();

        // Always refresh available engines to ensure there are recommendations
        self.refresh_available_engines(hardware_capabilities).await?;

        debug!("Found {} installed engines", self.installed_engines.len());
        Ok(())
    }

    /// Remove engines superseded by a newer, VERIFIED engine of the same
    /// (platform, architecture, acceleration) triple.
    ///
    /// Rules:
    ///   - The survivor must have status Installed (i.e. it passed this boot's
    ///     backend verification). An unverified newer engine never causes the
    ///     deletion of a working older one.
    ///   - Only strictly-lower versions are removed (equal versions are kept —
    ///     deterministic default selection orders them by id).
    ///   - Corrupted older engines are also removed when superseded: their
    ///     failure state is obsolete once a newer verified engine exists.
    fn cleanup_superseded_engines(&mut self) {
        // Highest VERIFIED version per triple
        let mut best: HashMap<(String, String, String), u64> = HashMap::new();
        for e in self.installed_engines.values() {
            if e.status != EngineStatus::Installed {
                continue;
            }
            let key = (
                e.platform.to_string(),
                e.architecture.to_string(),
                e.acceleration.to_string(),
            );
            let v = Self::version_number(&e.version);
            let entry = best.entry(key).or_insert(0);
            if v > *entry {
                *entry = v;
            }
        }

        let superseded: Vec<String> = self
            .installed_engines
            .values()
            .filter(|e| {
                let key = (
                    e.platform.to_string(),
                    e.architecture.to_string(),
                    e.acceleration.to_string(),
                );
                match best.get(&key) {
                    Some(best_v) => Self::version_number(&e.version) < *best_v,
                    None => false,
                }
            })
            .map(|e| e.id.clone())
            .collect();

        for id in superseded {
            let engine = match self.installed_engines.get(&id) {
                Some(e) => e,
                None => continue,
            };

            // The on-disk directory is named by engine id (both the NSIS
            // installer and the app downloader use this layout). Fall back to
            // the recorded install_path if the id-named directory is absent.
            let id_dir = self.storage_path.join(&id);
            let dir = if id_dir.exists() {
                id_dir
            } else {
                // Safety: only ever delete paths strictly inside the engine
                // storage directory — never the storage root or anything outside it.
                match &engine.install_path {
                    Some(p) if p.starts_with(&self.storage_path) && *p != self.storage_path => {
                        p.clone()
                    }
                    _ => {
                        warn!(
                            "Superseded engine '{}' has no locatable directory under {:?} — removing registry entry only",
                            id, self.storage_path
                        );
                        self.installed_engines.remove(&id);
                        continue;
                    }
                }
            };

            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {
                    info!(
                        "Removed superseded engine '{}' (version {}) at {:?} — a newer verified \
                         engine of the same platform/architecture/acceleration is installed",
                        id, engine.version, dir
                    );
                    self.installed_engines.remove(&id);
                }
                Err(e) => {
                    warn!(
                        "Failed to remove superseded engine '{}' at {:?}: {}. It remains \
                         registered; deterministic selection still prefers the newer engine.",
                        id, dir, e
                    );
                }
            }
        }

        // A default id pointing at a removed engine must not survive the cleanup
        if let Some(ref d) = self.default_engine {
            if !self.installed_engines.contains_key(d) {
                self.default_engine = None;
            }
        }
    }

    /// Check if directory contains engine binary files.
    /// Checks flat layout (binary directly in engine_dir) â€” our standard layout.
    fn has_binary(&self, engine_dir: &PathBuf) -> bool {
        let binary_names = ["llama-server.exe", "llama-server", "llama-cli.exe", "llama-cli"];
        for binary_name in binary_names.iter() {
            if engine_dir.join(binary_name).exists() {
                return true;
            }
        }
        false
    }
    
    /// Load engine metadata from installation directory.
    ///
    /// Binary search order:
    ///   1. Flat layout: `engine_dir/<binary_name>` (standard â€” produced by `tar --strip-components=1`)
    ///   2. One-level-deep: first subdirectory that contains `<binary_name>`
    ///      (fallback for older Windows where `tar --strip-components` is unsupported)
    ///
    /// An engine found this way is NOT trusted purely on metadata+binary presence.
    /// This is the only path through which an NSIS- or postinstall-script-installed
    /// engine ever gets validated by this codebase â€” those installers do their own
    /// download/extraction with no DLL or backend verification at all (NSIS only
    /// checks `tar.exe`'s exit code; a truncated extraction that still exits 0 would
    /// otherwise sail straight through into "Installed" status with no further check
    /// ever happening, since `EngineDownloader::download_engine`'s verification only
    /// runs for engines the app itself downloads). So every engine found on disk â€”
    /// regardless of how it got there â€” is launched and checked for a self-reported
    /// `load_backend: loaded <Name> backend` line here, every startup, before being
    /// considered Installed. See dll_manager.rs's module doc for why that's the
    /// correct (non-hardcoded) verification method.
    async fn load_engine_metadata(&self, engine_dir: PathBuf) -> Option<EngineInfo> {
        let metadata_path = engine_dir.join("metadata.json");
        if !metadata_path.exists() {
            return None;
        }

        let mut engine_info = match std::fs::read_to_string(&metadata_path) {
            Ok(content) => match serde_json::from_str::<EngineInfo>(&content) {
                Ok(info) => info,
                Err(e) => {
                    warn!("Failed to parse engine metadata at {:?}: {}", metadata_path, e);
                    return None;
                }
            },
            Err(e) => {
                warn!("Failed to read engine metadata at {:?}: {}", metadata_path, e);
                return None;
            }
        };

        // 1. Standard flat layout
        let flat_binary = engine_dir.join(&engine_info.binary_name);
        let install_path = if flat_binary.exists() {
            Some(engine_dir.clone())
        } else {
            // 2. One-level-deep search (tar without --strip-components left a subdir)
            let mut found = None;
            if let Ok(entries) = std::fs::read_dir(&engine_dir) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        let sub_binary = entry.path().join(&engine_info.binary_name);
                        if sub_binary.exists() {
                            info!(
                                "Engine binary found in subdirectory: {:?} (moving install_path)",
                                entry.path()
                            );
                            found = Some(entry.path());
                            break;
                        }
                    }
                }
            }
            found
        };

        let install_path = match install_path {
            Some(p) => p,
            None => {
                let reason = format!(
                    "Engine binary '{}' not found in {:?} or any immediate subdirectory",
                    engine_info.binary_name, engine_dir
                );
                warn!("{} — marking engine '{}' as Corrupted", reason, engine_info.id);
                engine_info.status = EngineStatus::Corrupted;
                engine_info.install_path = None;
                engine_info.failure_reason = Some(reason);
                return Some(engine_info);
            }
        };

        // CUDA engines require the separately-shipped cudart runtime DLLs
        // (cudart64_*/cublas64_*). `--version` genuinely cannot exercise cuBLAS
        // (verified empirically — see dll_manager.rs), so backend verification
        // below would pass even with the runtime missing and inference would
        // then fail. A deterministic file-presence check is the only pre-flight
        // signal available for this package.
        if engine_info.acceleration == AccelerationType::CUDA {
            let (has_cudart, has_cublas) = std::fs::read_dir(&install_path)
                .map(|entries| {
                    let mut cudart = false;
                    let mut cublas = false;
                    for e in entries.flatten() {
                        let name = e.file_name().to_string_lossy().to_lowercase();
                        if name.starts_with("cudart64_") { cudart = true; }
                        if name.starts_with("cublas64_") { cublas = true; }
                    }
                    (cudart, cublas)
                })
                .unwrap_or((false, false));

            if !has_cudart || !has_cublas {
                let reason = format!(
                    "CUDA runtime DLLs missing from {:?} (cudart64_*: {}, cublas64_*: {}). \
                     Inference would fail at the first matrix multiplication.",
                    install_path, has_cudart, has_cublas
                );
                warn!("{} — marking engine '{}' as Corrupted", reason, engine_info.id);
                engine_info.status = EngineStatus::Corrupted;
                engine_info.install_path = Some(install_path);
                engine_info.failure_reason = Some(reason);
                return Some(engine_info);
            }
        }

        match super::dll_manager::DllManager::verify_backend_loads(&install_path, &engine_info).await {
            Ok(result) if result.backend_loaded() => {
                engine_info.status = EngineStatus::Installed;
                engine_info.install_path = Some(install_path);
                engine_info.failure_reason = None;
                Some(engine_info)
            }
            Ok(result) => {
                let reason = format!(
                    "Engine never self-reported loading its {} backend on launch \
                     (binary and metadata are present but the backend DLLs did not load)",
                    engine_info.acceleration
                );
                warn!(
                    "{} at {:?} — marking engine '{}' as Corrupted. Launch output:\n{}",
                    reason, install_path, engine_info.id, result.raw_output
                );
                engine_info.status = EngineStatus::Corrupted;
                engine_info.install_path = Some(install_path);
                engine_info.failure_reason = Some(reason);
                Some(engine_info)
            }
            Err(e) => {
                let reason = format!("Engine binary failed to launch during startup verification: {}", e);
                warn!(
                    "{} at {:?} — marking engine '{}' as Corrupted",
                    reason, install_path, engine_info.id
                );
                engine_info.status = EngineStatus::Corrupted;
                engine_info.install_path = Some(install_path);
                engine_info.failure_reason = Some(reason);
                Some(engine_info)
            }
        }
    }
    
    /// Get engines compatible with given hardware capabilities
    pub fn get_compatible_engines(&self, hardware: &HardwareCapabilities) -> Vec<&EngineInfo> {
        self.installed_engines
            .values()
            .filter(|engine| {
                let compatibility = engine.calculate_compatibility(hardware);
                compatibility > 30.0 && engine.status == EngineStatus::Installed
            })
            .collect()
    }
    
    /// Select the default among *installed, verified* engines — deterministically.
    ///
    /// Total order (no HashMap-iteration nondeterminism can survive it):
    ///   1. Engines whose (acceleration, architecture) match the decision-table
    ///      choice for this hardware
    ///   2. Newer llama.cpp version (numeric, "b8037" -> 8037)
    ///   3. Higher compatibility score
    ///   4. Lexicographic id
    ///
    /// Choosing an installed non-table-acceleration engine when no table-matching
    /// engine is installed is NOT a fallback: every candidate here already passed
    /// startup backend verification (it works), and the mismatch with the
    /// decision table is surfaced explicitly through GET /engines rather than
    /// silently "upgraded" behind the user's back.
    pub fn select_best_compatible_engine(&self, hardware: &HardwareCapabilities) -> Option<EngineInfo> {
        let table_choice = self.select_correct_engine(hardware).ok();
        let matches_table = |e: &EngineInfo| {
            table_choice.as_ref().map_or(false, |t| {
                e.acceleration == t.acceleration && e.architecture == t.architecture
            })
        };

        let mut candidates: Vec<&EngineInfo> = self.get_compatible_engines(hardware);
        candidates.sort_by(|a, b| {
            matches_table(b)
                .cmp(&matches_table(a))
                .then_with(|| {
                    Self::version_number(&b.version).cmp(&Self::version_number(&a.version))
                })
                .then_with(|| {
                    b.calculate_compatibility(hardware)
                        .partial_cmp(&a.calculate_compatibility(hardware))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.id.cmp(&b.id))
        });

        candidates.first().map(|e| (*e).clone())
    }
    
    /// Decision table: exactly ONE correct engine per machine. First match wins.
    /// This is the single source of truth for engine selection and is mirrored
    /// verbatim by the NSIS installer (installer-hooks.nsi,
    /// oci_DetectHardwareAndSetEngine). There are no substitution chains: a
    /// hardware profile maps to one engine id, and failure to resolve it is an
    /// explicit error, never a different engine.
    ///
    /// | Row | Condition (evaluated top-down)                        | Engine            |
    /// |-----|-------------------------------------------------------|-------------------|
    /// | 1   | Windows ARM64                                         | cpu-arm64         |
    /// | 2   | Windows x64, NVIDIA, driver>=580.0, cc>=7.5           | cuda13 (13.1)     |
    /// | 3   | Windows x64, NVIDIA, driver>=527.41, cc>=5.0          | cuda (12.4)       |
    /// | 4   | Windows x64, real Vulkan ICD registered + loader      | vulkan            |
    /// | 5   | Windows x64, everything else                          | cpu-x64           |
    /// | 6   | macOS ARM64 (Metal)                                   | metal-arm64       |
    /// | 7   | macOS x64                                             | cpu-macos-x64     |
    /// | 8   | Linux x64 with Vulkan (covers NVIDIA via ICD)         | vulkan-linux      |
    /// | 9   | Linux x64                                             | cpu-linux-x64     |
    pub fn select_correct_engine(&self, hardware: &HardwareCapabilities) -> Result<EngineInfo> {
        let latest_version = Self::get_engine_version();

        let engine_id = match (&hardware.platform, &hardware.architecture) {
            (Platform::Windows, HardwareArchitecture::Aarch64) => {
                format!("llama-cpu-windows-arm64-{}", latest_version)
            }
            (Platform::Windows, HardwareArchitecture::X86_64) => {
                if hardware.cuda13_usable() {
                    format!("llama-cuda13-windows-x64-{}", latest_version)
                } else if hardware.cuda12_usable() {
                    format!("llama-cuda-windows-x64-{}", latest_version)
                } else if hardware.has_vulkan {
                    format!("llama-vulkan-windows-x64-{}", latest_version)
                } else {
                    format!("llama-cpu-windows-x64-{}", latest_version)
                }
            }
            (Platform::MacOS, HardwareArchitecture::Aarch64) => {
                format!("llama-metal-macos-arm64-{}", latest_version)
            }
            (Platform::MacOS, HardwareArchitecture::X86_64) => {
                format!("llama-cpu-macos-x64-{}", latest_version)
            }
            (Platform::Linux, HardwareArchitecture::X86_64) => {
                if hardware.has_vulkan || hardware.has_cuda {
                    format!("llama-vulkan-linux-x64-{}", latest_version)
                } else {
                    format!("llama-cpu-linux-x64-{}", latest_version)
                }
            }
            (platform, arch) => {
                return Err(anyhow::anyhow!(
                    "No engine exists for platform {:?} / architecture {:?} at llama.cpp {}",
                    platform, arch, latest_version
                ));
            }
        };

        // Resolve the id against the capability-gated catalog for this hardware.
        // The catalog is built from the same gates the table uses, so a table hit
        // that is missing from the catalog indicates an internal inconsistency —
        // reported loudly, never substituted.
        self.get_all_compatible_engines(hardware)
            .into_iter()
            .find(|e| e.id == engine_id)
            .ok_or_else(|| anyhow::anyhow!(
                "Internal error: decision table selected '{}' but the engine catalog \
                 does not contain it for hardware {:?}/{:?}. This is a bug — the table \
                 and catalog gates must match.",
                engine_id, hardware.platform, hardware.architecture
            ))
    }

    fn get_engine_version() -> &'static str {
        ENGINE_VERSION
    }

    /// Parse the numeric part of a llama.cpp version tag ("b8037" -> 8037).
    /// Unparseable versions order lowest so a well-formed version always wins.
    fn version_number(version: &str) -> u64 {
        version
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    }
    
    
    /// Set the default engine for single-engine mode.
    /// Only a verified (Installed) engine may become the default — a Corrupted
    /// entry is present in the registry for visibility, never for use.
    pub fn set_default_engine(&mut self, engine_id: &str) -> Result<()> {
        match self.installed_engines.get(engine_id) {
            Some(e) if e.status == EngineStatus::Installed => {
                self.default_engine = Some(engine_id.to_string());
                info!("Set default engine: {}", engine_id);
                Ok(())
            }
            Some(e) => Err(anyhow::anyhow!(
                "Engine '{}' cannot be set as default: status is {:?}{}",
                engine_id,
                e.status,
                e.failure_reason
                    .as_deref()
                    .map(|r| format!(" ({})", r))
                    .unwrap_or_default()
            )),
            None => Err(anyhow::anyhow!("Engine not found: {}", engine_id)),
        }
    }

    /// Check if we have any verified installed engine
    pub fn has_installed_engine(&self) -> bool {
        self.installed_engines
            .values()
            .any(|e| e.status == EngineStatus::Installed)
    }

    /// Get the default engine (single-engine model).
    /// Only ever returns a verified (Installed) engine; when no default id is
    /// set, the choice among verified engines is deterministic (newest version,
    /// then lexicographic id) — never HashMap iteration order.
    pub fn get_default_engine(&self) -> Option<&EngineInfo> {
        if let Some(ref engine_id) = self.default_engine {
            if let Some(e) = self.installed_engines.get(engine_id) {
                if e.status == EngineStatus::Installed {
                    return Some(e);
                }
            }
        }

        self.installed_engines
            .values()
            .filter(|e| e.status == EngineStatus::Installed)
            .max_by(|a, b| {
                Self::version_number(&a.version)
                    .cmp(&Self::version_number(&b.version))
                    // On version tie, the lexicographically-smaller id wins
                    // (inverted because this is a max_by).
                    .then_with(|| b.id.cmp(&a.id))
            })
    }
    
    /// Add a newly installed engine to the registry
    pub async fn add_installed_engine(&mut self, mut engine: EngineInfo) -> Result<()> {
        engine.status = EngineStatus::Installed;
        self.installed_engines.insert(engine.id.clone(), engine);
        Ok(())
    }
    
    /// Refresh available engines from official sources
    pub async fn refresh_available_engines(&mut self, hardware_capabilities: &HardwareCapabilities) -> Result<()> {
        // Clear existing available engines and populate with ALL compatible options
        self.available_engines.clear();
        
        // Get all engines for the current platform (not just the recommended one)
        let all_engines = self.get_all_compatible_engines(hardware_capabilities);
        
        for engine in all_engines {
            if !self.available_engines.iter().any(|e| e.id == engine.id) {
                self.available_engines.push(engine);
            }
        }

        info!("Refreshed available engines: {} found", self.available_engines.len());
        Ok(())
    }
    
    /// Get ALL compatible engines for the platform (like LM Studio)
    fn get_all_compatible_engines(&self, hardware: &HardwareCapabilities) -> Vec<EngineInfo> {
        let mut engines = Vec::new();
        let latest_version = Self::get_engine_version();
        
        match (&hardware.platform, &hardware.architecture) {
            (Platform::Windows, HardwareArchitecture::X86_64) => {
                // Windows x64: Always add CPU, CUDA, and Vulkan options
                
                // 1. CPU Engine (works on all Windows x64)
                engines.push(EngineInfo {
                    id: format!("llama-cpu-windows-x64-{}", latest_version),
                    name: format!("llama.cpp CPU (Windows x64) ({})", latest_version),
                    version: latest_version.to_string(),
                    platform: Platform::Windows,
                    architecture: HardwareArchitecture::X86_64,
                    acceleration: AccelerationType::CPU,
                    download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-win-cpu-x64.zip", latest_version, latest_version),
                    file_size: 50 * 1024 * 1024,
                    checksum: "".to_string(),
                    compatibility_score: if !hardware.has_cuda { 95.0 } else { 80.0 },
                    status: EngineStatus::Available,
                    install_path: None,
                    binary_name: "llama-server.exe".to_string(),
                    required_dependencies: vec![],
                    failure_reason: None,
                });
                
                // 2. CUDA Engine — only when the NVIDIA driver/compute-capability
                //    floors for CUDA 12.x are confirmed (driver >= 527.41, cc >= 5.0).
                if hardware.cuda12_usable() {
                    engines.push(EngineInfo {
                        id: format!("llama-cuda-windows-x64-{}", latest_version),
                        name: format!("llama.cpp CUDA (Windows x64) ({})", latest_version),
                        version: latest_version.to_string(),
                        platform: Platform::Windows,
                        architecture: HardwareArchitecture::X86_64,
                        acceleration: AccelerationType::CUDA,
                        download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-win-cuda-12.4-x64.zip", latest_version, latest_version),
                        file_size: 373 * 1024 * 1024, // Based on actual release size
                        checksum: "".to_string(),
                        compatibility_score: 100.0,
                        status: EngineStatus::Available,
                        install_path: None,
                        binary_name: "llama-server.exe".to_string(),
                        required_dependencies: vec!["NVIDIA GPU with CUDA support".to_string()],
                        failure_reason: None,
                    });
                    
                    // CUDA 13 variant — stricter floors: r580+ driver and
                    // Turing+ (compute capability 7.5), since CUDA 13 dropped
                    // Maxwell/Pascal/Volta support.
                    if hardware.cuda13_usable() {
                    engines.push(EngineInfo {
                        id: format!("llama-cuda13-windows-x64-{}", latest_version),
                        name: format!("llama.cpp CUDA 13 (Windows x64) ({})", latest_version),
                        version: latest_version.to_string(),
                        platform: Platform::Windows,
                        architecture: HardwareArchitecture::X86_64,
                        acceleration: AccelerationType::CUDA,
                        download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-win-cuda-13.1-x64.zip", latest_version, latest_version),
                        file_size: 384 * 1024 * 1024, // Based on actual release size
                        checksum: "".to_string(),
                        compatibility_score: 95.0,
                        status: EngineStatus::Available,
                        install_path: None,
                        binary_name: "llama-server.exe".to_string(),
                        required_dependencies: vec!["CUDA 13.1+ Runtime".to_string()],
                        failure_reason: None,
                    });
                    } // end cuda13_usable
                }

                // 3. Vulkan Engine (alternative GPU acceleration)
                if hardware.has_vulkan || hardware.has_cuda {
                    engines.push(EngineInfo {
                        id: format!("llama-vulkan-windows-x64-{}", latest_version),
                        name: format!("llama.cpp Vulkan (Windows x64) ({})", latest_version),
                        version: latest_version.to_string(),
                        platform: Platform::Windows,
                        architecture: HardwareArchitecture::X86_64,
                        acceleration: AccelerationType::Vulkan,
                        download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-win-vulkan-x64.zip", latest_version, latest_version),
                        file_size: 80 * 1024 * 1024,
                        checksum: "".to_string(),
                        compatibility_score: 85.0,
                        status: EngineStatus::Available,
                        install_path: None,
                        binary_name: "llama-server.exe".to_string(),
                        required_dependencies: vec!["Vulkan-compatible GPU".to_string()],
                        failure_reason: None,
                    });
                }
            }
            
            (Platform::Windows, HardwareArchitecture::Aarch64) => {
                // Windows ARM64 (Surface/Snapdragon): CPU is the default â€” no
                // CUDA/Vulkan ARM64 Windows binary exists in the b8037 release.
                engines.push(EngineInfo {
                    id: format!("llama-cpu-windows-arm64-{}", latest_version),
                    name: format!("llama.cpp CPU (Windows ARM64) ({})", latest_version),
                    version: latest_version.to_string(),
                    platform: Platform::Windows,
                    architecture: HardwareArchitecture::Aarch64,
                    acceleration: AccelerationType::CPU,
                    download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-win-cpu-arm64.zip", latest_version, latest_version),
                    file_size: 50 * 1024 * 1024,
                    checksum: "".to_string(),
                    compatibility_score: 95.0,
                    status: EngineStatus::Available,
                    install_path: None,
                    binary_name: "llama-server.exe".to_string(),
                    required_dependencies: vec![],
                    failure_reason: None,
                });

                // Qualcomm Adreno GPU acceleration via OpenCL — the b8037
                // release ships llama-{ver}-bin-win-opencl-adreno-arm64.zip.
                // Listed as available (not auto-installed) when an Adreno GPU
                // is present; startup verification checks the self-reported
                // "loaded OpenCL backend" line like every other engine.
                if hardware.has_adreno_gpu {
                    engines.push(EngineInfo {
                        id: format!("llama-opencl-adreno-windows-arm64-{}", latest_version),
                        name: format!("llama.cpp OpenCL Adreno (Windows ARM64) ({})", latest_version),
                        version: latest_version.to_string(),
                        platform: Platform::Windows,
                        architecture: HardwareArchitecture::Aarch64,
                        acceleration: AccelerationType::OpenCL,
                        download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-win-opencl-adreno-arm64.zip", latest_version, latest_version),
                        file_size: 25 * 1024 * 1024,
                        checksum: "".to_string(),
                        compatibility_score: 90.0,
                        status: EngineStatus::Available,
                        install_path: None,
                        binary_name: "llama-server.exe".to_string(),
                        required_dependencies: vec!["Qualcomm Adreno GPU with OpenCL driver".to_string()],
                        failure_reason: None,
                    });
                }
            }

            (Platform::MacOS, HardwareArchitecture::Aarch64) => {
                // macOS Apple Silicon: Metal and CPU
                engines.push(EngineInfo {
                    id: format!("llama-metal-macos-arm64-{}", latest_version),
                    name: format!("llama.cpp Metal (macOS Apple Silicon) ({})", latest_version),
                    version: latest_version.to_string(),
                    platform: Platform::MacOS,
                    architecture: HardwareArchitecture::Aarch64,
                    acceleration: AccelerationType::Metal,
                    download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-macos-arm64.tar.gz", latest_version, latest_version),
                    file_size: 29 * 1024 * 1024,
                    checksum: "".to_string(),
                    compatibility_score: 100.0,
                    status: EngineStatus::Available,
                    install_path: None,
                    binary_name: "llama-server".to_string(),
                    required_dependencies: vec![],
                    failure_reason: None,
                });
                
                // CPU fallback
                engines.push(EngineInfo {
                    id: format!("llama-cpu-macos-arm64-{}", latest_version),
                    name: format!("llama.cpp CPU (macOS Apple Silicon) ({})", latest_version),
                    version: latest_version.to_string(),
                    platform: Platform::MacOS,
                    architecture: HardwareArchitecture::Aarch64,
                    acceleration: AccelerationType::CPU,
                    download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-macos-arm64.tar.gz", latest_version, latest_version),
                    file_size: 29 * 1024 * 1024,
                    checksum: "".to_string(),
                    compatibility_score: 90.0,
                    status: EngineStatus::Available,
                    install_path: None,
                    binary_name: "llama-server".to_string(),
                    required_dependencies: vec![],
                    failure_reason: None,
                });
            }
            
            (Platform::MacOS, HardwareArchitecture::X86_64) => {
                // macOS Intel: CPU only
                engines.push(EngineInfo {
                    id: format!("llama-cpu-macos-x64-{}", latest_version),
                    name: format!("llama.cpp CPU (macOS Intel) ({})", latest_version),
                    version: latest_version.to_string(),
                    platform: Platform::MacOS,
                    architecture: HardwareArchitecture::X86_64,
                    acceleration: AccelerationType::CPU,
                    download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-macos-x64.tar.gz", latest_version, latest_version),
                    file_size: 82 * 1024 * 1024,
                    checksum: "".to_string(),
                    compatibility_score: 95.0,
                    status: EngineStatus::Available,
                    install_path: None,
                    binary_name: "llama-server".to_string(),
                    required_dependencies: vec![],
                    failure_reason: None,
                });
            }
            
            (Platform::Linux, HardwareArchitecture::X86_64) => {
                // Linux x64: CPU, CUDA
                engines.push(EngineInfo {
                    id: format!("llama-cpu-linux-x64-{}", latest_version),
                    name: format!("llama.cpp CPU (Linux x64) ({})", latest_version),
                    version: latest_version.to_string(),
                    platform: Platform::Linux,
                    architecture: HardwareArchitecture::X86_64,
                    acceleration: AccelerationType::CPU,
                    download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-ubuntu-x64.tar.gz", latest_version, latest_version),
                    file_size: 45 * 1024 * 1024,
                    checksum: "".to_string(),
                    compatibility_score: if !hardware.has_cuda { 95.0 } else { 80.0 },
                    status: EngineStatus::Available,
                    install_path: None,
                    binary_name: "llama-server".to_string(),
                    required_dependencies: vec![],
                    failure_reason: None,
                });
                
                // No Linux CUDA build exists for b8037. Offer Vulkan for GPU-equipped systems
                // (works for both NVIDIA and AMD on Linux via their respective Vulkan ICDs).
                if hardware.has_cuda || hardware.has_vulkan {
                    engines.push(EngineInfo {
                        id: format!("llama-vulkan-linux-x64-{}", latest_version),
                        name: format!("llama.cpp Vulkan (Linux x64) ({})", latest_version),
                        version: latest_version.to_string(),
                        platform: Platform::Linux,
                        architecture: HardwareArchitecture::X86_64,
                        acceleration: AccelerationType::Vulkan,
                        download_url: format!("https://github.com/ggml-org/llama.cpp/releases/download/{}/llama-{}-bin-ubuntu-vulkan-x64.tar.gz", latest_version, latest_version),
                        file_size: 60 * 1024 * 1024,
                        checksum: "".to_string(),
                        compatibility_score: 100.0,
                        status: EngineStatus::Available,
                        install_path: None,
                        binary_name: "llama-server".to_string(),
                        required_dependencies: vec!["Vulkan-compatible GPU (NVIDIA/AMD)".to_string()],
                        failure_reason: None,
                    });
                }
            }
            
            _ => {
                // Unknown platform/architecture - add generic CPU engine
                info!("Unknown platform/architecture: {:?}/{:?}", hardware.platform, hardware.architecture);
            }
        }
        
        // Sort by compatibility score (highest first)
        engines.sort_by(|a, b| b.compatibility_score.partial_cmp(&a.compatibility_score).unwrap());
        
        engines
    }
    
    
    /// Get the path to the default engine binary
    pub fn get_default_engine_binary_path(&self) -> Option<PathBuf> {
        if let Some(engine) = self.get_default_engine() {
            engine.install_path.as_ref().map(|path| path.join(&engine.binary_name))
        } else {
            None
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_runtime::platform_detector::NvidiaDriverInfo;

    fn registry() -> EngineRegistry {
        // Construct directly — tests must not touch the real AppData engine dir.
        EngineRegistry {
            installed_engines: HashMap::new(),
            available_engines: Vec::new(),
            default_engine: None,
            storage_path: PathBuf::from("."),
        }
    }

    fn hw(
        platform: Platform,
        arch: HardwareArchitecture,
        has_cuda: bool,
        has_vulkan: bool,
        driver: Option<((u32, u32), Option<(u32, u32)>)>,
    ) -> HardwareCapabilities {
        HardwareCapabilities {
            platform,
            architecture: arch,
            has_cuda,
            has_metal: false,
            has_vulkan,
            nvidia_driver: driver.map(|(d, cc)| NvidiaDriverInfo {
                driver_version: d,
                max_compute_cap: cc,
            }),
            has_adreno_gpu: false,
        }
    }

    fn installed(id: &str, version: &str, accel: AccelerationType) -> EngineInfo {
        EngineInfo {
            id: id.to_string(),
            name: id.to_string(),
            version: version.to_string(),
            platform: Platform::Windows,
            architecture: HardwareArchitecture::X86_64,
            acceleration: accel,
            download_url: String::new(),
            file_size: 0,
            checksum: String::new(),
            compatibility_score: 0.0,
            status: EngineStatus::Installed,
            install_path: Some(PathBuf::from(".")),
            binary_name: "llama-server.exe".to_string(),
            required_dependencies: vec![],
            failure_reason: None,
        }
    }

    #[test]
    fn table_windows_x64_cuda13() {
        // Turing+ on r580+: CUDA 13.1 is the one correct engine
        let h = hw(
            Platform::Windows,
            HardwareArchitecture::X86_64,
            true,
            true,
            Some(((581, 42), Some((8, 6)))),
        );
        let e = registry().select_correct_engine(&h).unwrap();
        assert_eq!(e.id, format!("llama-cuda13-windows-x64-{}", ENGINE_VERSION));
        assert_eq!(e.acceleration, AccelerationType::CUDA);
    }

    #[test]
    fn table_windows_x64_cuda12() {
        // Passes CUDA 12 floors but not 13 (driver below 580)
        let h = hw(
            Platform::Windows,
            HardwareArchitecture::X86_64,
            true,
            true,
            Some(((551, 61), Some((8, 6)))),
        );
        let e = registry().select_correct_engine(&h).unwrap();
        assert_eq!(e.id, format!("llama-cuda-windows-x64-{}", ENGINE_VERSION));
    }

    #[test]
    fn table_windows_x64_nvidia_old_driver_is_vulkan() {
        // NVIDIA present but below the CUDA 12 floor: Vulkan is the CORRECT
        // engine for this machine (capability selection, not a fallback).
        let h = hw(
            Platform::Windows,
            HardwareArchitecture::X86_64,
            true,
            true,
            Some(((475, 14), Some((3, 5)))),
        );
        let e = registry().select_correct_engine(&h).unwrap();
        assert_eq!(e.id, format!("llama-vulkan-windows-x64-{}", ENGINE_VERSION));
    }

    #[test]
    fn table_windows_x64_amd_intel_vulkan() {
        let h = hw(Platform::Windows, HardwareArchitecture::X86_64, false, true, None);
        let e = registry().select_correct_engine(&h).unwrap();
        assert_eq!(e.id, format!("llama-vulkan-windows-x64-{}", ENGINE_VERSION));
    }

    #[test]
    fn table_windows_x64_cpu_only() {
        // No GPU, or Intel HD with a loader but no real Vulkan ICD
        let h = hw(Platform::Windows, HardwareArchitecture::X86_64, false, false, None);
        let e = registry().select_correct_engine(&h).unwrap();
        assert_eq!(e.id, format!("llama-cpu-windows-x64-{}", ENGINE_VERSION));
    }

    #[test]
    fn table_windows_arm64_is_cpu_regardless_of_gpu_flags() {
        let h = hw(Platform::Windows, HardwareArchitecture::Aarch64, false, true, None);
        let e = registry().select_correct_engine(&h).unwrap();
        assert_eq!(e.id, format!("llama-cpu-windows-arm64-{}", ENGINE_VERSION));
    }

    #[test]
    fn table_unknown_arch_is_explicit_error() {
        let h = hw(
            Platform::Windows,
            HardwareArchitecture::Other("riscv64".into()),
            false,
            false,
            None,
        );
        assert!(registry().select_correct_engine(&h).is_err());
    }

    #[test]
    fn version_number_parses() {
        assert_eq!(EngineRegistry::version_number("b8037"), 8037);
        assert_eq!(EngineRegistry::version_number("b9001"), 9001);
        assert_eq!(EngineRegistry::version_number("garbage"), 0);
    }

    #[test]
    fn default_selection_prefers_newer_version_deterministically() {
        // Two installed CPU engines from different releases: newer must win,
        // regardless of HashMap iteration order.
        let h = hw(Platform::Windows, HardwareArchitecture::X86_64, false, false, None);
        let mut reg = registry();
        reg.installed_engines.insert(
            "llama-cpu-windows-x64-b7000".into(),
            installed("llama-cpu-windows-x64-b7000", "b7000", AccelerationType::CPU),
        );
        reg.installed_engines.insert(
            "llama-cpu-windows-x64-b8037".into(),
            installed("llama-cpu-windows-x64-b8037", "b8037", AccelerationType::CPU),
        );
        let best = reg.select_best_compatible_engine(&h).unwrap();
        assert_eq!(best.id, "llama-cpu-windows-x64-b8037");
    }

    #[test]
    fn default_selection_prefers_table_acceleration() {
        // Vulkan machine with both a CPU and a Vulkan engine installed:
        // the decision-table acceleration (Vulkan) must win even though the
        // CPU engine has an equal-or-higher raw compatibility score.
        let h = hw(Platform::Windows, HardwareArchitecture::X86_64, false, true, None);
        let mut reg = registry();
        reg.installed_engines.insert(
            "llama-cpu-windows-x64-b8037".into(),
            installed("llama-cpu-windows-x64-b8037", "b8037", AccelerationType::CPU),
        );
        reg.installed_engines.insert(
            "llama-vulkan-windows-x64-b8037".into(),
            installed("llama-vulkan-windows-x64-b8037", "b8037", AccelerationType::Vulkan),
        );
        let best = reg.select_best_compatible_engine(&h).unwrap();
        assert_eq!(best.id, "llama-vulkan-windows-x64-b8037");
    }
}
