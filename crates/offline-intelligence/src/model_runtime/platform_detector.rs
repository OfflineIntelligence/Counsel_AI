//! Platform and Hardware Detection
//!
//! Detects the appropriate runtime binary based on the platform (Windows, Linux, macOS)
//! and hardware capabilities (Intel, Apple Silicon, NVIDIA CUDA).

use std::sync::OnceLock;
use tracing::info;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Platform {
    Windows,
    Linux,
    MacOS,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum HardwareArchitecture {
    X86_64,
    Aarch64, // Apple Silicon, ARM
    Other(String),
}

/// NVIDIA driver / GPU capability details, queried via nvidia-smi.
/// Used to gate CUDA engine selection on the *verified* floors:
///   CUDA 12.x apps on Windows require driver >= 527.41 and compute capability >= 5.0
///   CUDA 13.x apps require driver r580+ and compute capability >= 7.5 (Turing+)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NvidiaDriverInfo {
    /// Driver version as (major, minor), e.g. (537, 13) for "537.13"
    pub driver_version: (u32, u32),
    /// Highest compute capability among installed GPUs as (major, minor),
    /// e.g. (8, 6) for an RTX 3080. None when the driver is too old to
    /// support the compute_cap query (which itself implies a pre-CUDA-12 driver).
    pub max_compute_cap: Option<(u32, u32)>,
}

/// Minimum Windows driver for CUDA 12.x applications (minor version compatibility floor)
pub const CUDA12_MIN_WINDOWS_DRIVER: (u32, u32) = (527, 41);
/// Minimum Linux driver for CUDA 12.x applications
pub const CUDA12_MIN_LINUX_DRIVER: (u32, u32) = (525, 60);
/// CUDA 12 toolkit supports compute capability 5.0 (Maxwell) and newer
pub const CUDA12_MIN_COMPUTE_CAP: (u32, u32) = (5, 0);
/// CUDA 13.x applications require an r580+ driver
pub const CUDA13_MIN_DRIVER: (u32, u32) = (580, 0);
/// CUDA 13 dropped Maxwell/Pascal/Volta — Turing (7.5) is the floor
pub const CUDA13_MIN_COMPUTE_CAP: (u32, u32) = (7, 5);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HardwareCapabilities {
    pub platform: Platform,
    pub architecture: HardwareArchitecture,
    pub has_cuda: bool,
    pub has_metal: bool, // For Apple GPUs
    pub has_vulkan: bool,
    /// NVIDIA driver/compute-capability details (None when no NVIDIA GPU
    /// or nvidia-smi query failed). Additive field: serde(default) keeps
    /// backward compatibility with any previously serialized data.
    #[serde(default)]
    pub nvidia_driver: Option<NvidiaDriverInfo>,
    /// True when a Qualcomm Adreno GPU is present (Windows-on-ARM devices).
    /// Enables the llama.cpp OpenCL-Adreno engine as an available option.
    #[serde(default)]
    pub has_adreno_gpu: bool,
}

// Static cache for hardware capabilities to avoid repeated detection
static HARDWARE_CACHE: OnceLock<HardwareCapabilities> = OnceLock::new();

impl Default for HardwareCapabilities {
    fn default() -> Self {
        Self::detect()
    }
}

impl HardwareCapabilities {
    /// Detect hardware capabilities automatically (cached)
    pub fn detect() -> Self {
        // Return cached result if available
        if let Some(cached) = HARDWARE_CACHE.get() {
            return cached.clone();
        }

        // Perform detection
        let platform = Self::detect_platform();
        let architecture = Self::detect_architecture();
        let has_cuda = Self::detect_cuda_support();
        let has_metal = Self::detect_metal_support();
        let has_vulkan = Self::detect_vulkan_support();
        // Only query driver details when an NVIDIA GPU responded to nvidia-smi
        let nvidia_driver = if has_cuda {
            Self::detect_nvidia_driver_info()
        } else {
            None
        };
        let has_adreno_gpu = Self::detect_adreno_gpu(&architecture);

        info!(
            "Detected platform: {:?}, architecture: {:?}, CUDA: {}, Metal: {}, Vulkan: {}, NVIDIA driver: {:?}, Adreno: {}",
            platform, architecture, has_cuda, has_metal, has_vulkan, nvidia_driver, has_adreno_gpu
        );

        let capabilities = Self {
            platform,
            architecture,
            has_cuda,
            has_metal,
            has_vulkan,
            nvidia_driver,
            has_adreno_gpu,
        };

        // Cache the result (ignore if already set by another thread)
        let _ = HARDWARE_CACHE.set(capabilities.clone());

        capabilities
    }

    fn detect_platform() -> Platform {
        if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "macos") {
            Platform::MacOS
        } else {
            // Default to current platform if unknown
            #[cfg(target_os = "windows")]
            return Platform::Windows;
            #[cfg(target_os = "linux")]
            return Platform::Linux;
            #[cfg(target_os = "macos")]
            return Platform::MacOS;
            #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
            return Platform::Linux; // Default fallback
        }
    }

    /// Detect the *host* CPU architecture — not just the architecture this binary
    /// was compiled for.
    ///
    /// On Windows this matters because we only ship an x86_64 build (no native
    /// ARM64 build of the app/installer exists). On a Surface/Snapdragon ARM64
    /// device, this x86_64 process runs under Windows' built-in WOW64 x64-emulation
    /// layer, so `cfg!(target_arch = ...)` and `std::env::consts::ARCH` would both
    /// report "x86_64" even though the real CPU is ARM64. Windows exposes the true
    /// native architecture to emulated processes via the `PROCESSOR_ARCHITEW6432`
    /// environment variable (set only when running under emulation); a genuine
    /// native x64 process never has it set, in which case we fall back to
    /// `PROCESSOR_ARCHITECTURE`. This lets us route ARM64 hosts to the native
    /// ARM64 llama-server engine instead of the slower emulated x64 one.
    #[cfg(target_os = "windows")]
    fn detect_architecture() -> HardwareArchitecture {
        if let Ok(native_arch) = std::env::var("PROCESSOR_ARCHITEW6432") {
            if native_arch.eq_ignore_ascii_case("ARM64") {
                return HardwareArchitecture::Aarch64;
            }
            if native_arch.eq_ignore_ascii_case("AMD64") {
                return HardwareArchitecture::X86_64;
            }
        }

        if let Ok(arch) = std::env::var("PROCESSOR_ARCHITECTURE") {
            if arch.eq_ignore_ascii_case("ARM64") {
                return HardwareArchitecture::Aarch64;
            }
            if arch.eq_ignore_ascii_case("AMD64") {
                return HardwareArchitecture::X86_64;
            }
        }

        // Fall back to the compile-time architecture if env vars are unavailable.
        if cfg!(target_arch = "x86_64") {
            HardwareArchitecture::X86_64
        } else if cfg!(target_arch = "aarch64") {
            HardwareArchitecture::Aarch64
        } else {
            HardwareArchitecture::Other(std::env::consts::ARCH.to_string())
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn detect_architecture() -> HardwareArchitecture {
        if cfg!(target_arch = "x86_64") {
            HardwareArchitecture::X86_64
        } else if cfg!(target_arch = "aarch64") {
            HardwareArchitecture::Aarch64
        } else {
            HardwareArchitecture::Other(std::env::consts::ARCH.to_string())
        }
    }

    fn detect_cuda_support() -> bool {
        // Check for NVIDIA GPU via nvidia-smi with a timeout to prevent hangs
        // on systems with broken driver installations
        use std::process::{Command, Stdio};

        // Create command with hidden window on Windows
        #[cfg(target_os = "windows")]
        let child = {
            use std::os::windows::process::CommandExt;
            Command::new("nvidia-smi")
                .arg("--query-gpu=name")
                .arg("--format=csv,noheader,nounits")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                .spawn()
        };

        #[cfg(not(target_os = "windows"))]
        let child = Command::new("nvidia-smi")
            .arg("--query-gpu=name")
            .arg("--format=csv,noheader,nounits")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        match child {
            Ok(mut process) => {
                // Wait up to 5 seconds for nvidia-smi to respond
                let start = std::time::Instant::now();
                loop {
                    match process.try_wait() {
                        Ok(Some(status)) => return status.success(),
                        Ok(None) => {
                            if start.elapsed() > std::time::Duration::from_secs(5) {
                                let _ = process.kill();
                                let _ = process.wait();
                                return false;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        Err(_) => return false,
                    }
                }
            }
            Err(_) => false,
        }
    }

    /// Query NVIDIA driver version and highest compute capability via nvidia-smi.
    ///
    /// Runs `nvidia-smi --query-gpu=driver_version,compute_cap` first; if that
    /// fails (the compute_cap field requires a reasonably recent driver), retries
    /// with `driver_version` alone. A driver too old to answer compute_cap is by
    /// definition older than the CUDA 12 floor (527.41), so the driver-version
    /// gate still produces the correct decision in that case.
    fn detect_nvidia_driver_info() -> Option<NvidiaDriverInfo> {
        // Combined query: one line per GPU, e.g. "537.13, 8.6"
        if let Some(output) = Self::run_nvidia_smi_query("driver_version,compute_cap") {
            let mut driver: Option<(u32, u32)> = None;
            let mut max_cc: Option<(u32, u32)> = None;
            for line in output.lines() {
                let mut parts = line.split(',');
                let drv_str = parts.next().unwrap_or("").trim();
                let cc_str = parts.next().unwrap_or("").trim();
                if driver.is_none() {
                    driver = parse_version_pair(drv_str);
                }
                if let Some(cc) = parse_version_pair(cc_str) {
                    max_cc = Some(match max_cc {
                        Some(prev) if prev >= cc => prev,
                        _ => cc,
                    });
                }
            }
            if let Some(driver_version) = driver {
                return Some(NvidiaDriverInfo {
                    driver_version,
                    max_compute_cap: max_cc,
                });
            }
        }

        // Fallback: driver_version only (older drivers reject the compute_cap field)
        if let Some(output) = Self::run_nvidia_smi_query("driver_version") {
            if let Some(driver_version) = output.lines().next().and_then(|l| parse_version_pair(l.trim())) {
                return Some(NvidiaDriverInfo {
                    driver_version,
                    max_compute_cap: None,
                });
            }
        }

        None
    }

    /// Run an nvidia-smi --query-gpu invocation with a 5-second timeout and
    /// hidden window, returning stdout on success. Mirrors detect_cuda_support's
    /// process handling so broken driver installs cannot hang startup.
    fn run_nvidia_smi_query(fields: &str) -> Option<String> {
        use std::process::{Command, Stdio};

        let query_arg = format!("--query-gpu={}", fields);

        #[cfg(target_os = "windows")]
        let child = {
            use std::os::windows::process::CommandExt;
            Command::new("nvidia-smi")
                .arg(&query_arg)
                .arg("--format=csv,noheader,nounits")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                .spawn()
        };

        #[cfg(not(target_os = "windows"))]
        let child = Command::new("nvidia-smi")
            .arg(&query_arg)
            .arg("--format=csv,noheader,nounits")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        match child {
            Ok(mut process) => {
                let start = std::time::Instant::now();
                loop {
                    match process.try_wait() {
                        Ok(Some(status)) => {
                            if status.success() {
                                if let Ok(output) = process.wait_with_output() {
                                    return Some(String::from_utf8_lossy(&output.stdout).to_string());
                                }
                            }
                            return None;
                        }
                        Ok(None) => {
                            if start.elapsed() > std::time::Duration::from_secs(5) {
                                let _ = process.kill();
                                let _ = process.wait();
                                return None;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        Err(_) => return None,
                    }
                }
            }
            Err(_) => None,
        }
    }

    /// Detect a Qualcomm Adreno GPU (Windows-on-ARM devices such as Snapdragon X).
    /// Reads the display-adapter class registry via reg.exe — same source the
    /// NSIS installer uses — looking for Adreno/Qualcomm in DriverDesc.
    /// Only meaningful on Windows ARM64; returns false everywhere else.
    fn detect_adreno_gpu(architecture: &HardwareArchitecture) -> bool {
        if !cfg!(target_os = "windows") || *architecture != HardwareArchitecture::Aarch64 {
            return false;
        }

        #[cfg(target_os = "windows")]
        {
            if let Some(output) = Self::run_reg_query(&[
                "query",
                r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}",
                "/s",
                "/v",
                "DriverDesc",
            ]) {
                let lower = output.to_lowercase();
                return lower.contains("adreno") || lower.contains("qualcomm");
            }
        }

        false
    }

    /// Run reg.exe with a hidden window and 5-second timeout, returning stdout on success.
    /// Used for registry checks that std has no API for (no extra crate needed).
    #[cfg(target_os = "windows")]
    fn run_reg_query(args: &[&str]) -> Option<String> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let child = Command::new("reg")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .spawn();

        match child {
            Ok(mut process) => {
                let start = std::time::Instant::now();
                loop {
                    match process.try_wait() {
                        Ok(Some(status)) => {
                            if status.success() {
                                if let Ok(output) = process.wait_with_output() {
                                    return Some(String::from_utf8_lossy(&output.stdout).to_string());
                                }
                            }
                            return None;
                        }
                        Ok(None) => {
                            if start.elapsed() > std::time::Duration::from_secs(5) {
                                let _ = process.kill();
                                let _ = process.wait();
                                return None;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        Err(_) => return None,
                    }
                }
            }
            Err(_) => None,
        }
    }

    fn detect_metal_support() -> bool {
        // Metal is available on Apple Silicon and newer Intel Macs
        cfg!(target_os = "macos")
    }

    fn detect_vulkan_support() -> bool {
        #[cfg(target_os = "windows")]
        {
            // Two-stage check:
            // 1. vulkan-1.dll (the Khronos loader) must exist. It is installed by
            //    AMD/Intel/NVIDIA GPU drivers — but ALSO by many unrelated apps, so
            //    its presence alone over-reports (e.g. old Intel HD machines with a
            //    loader but no driver).
            // 2. An actual Vulkan ICD (driver) must be registered. Drivers register
            //    ICDs either under the legacy HKLM\SOFTWARE\Khronos\Vulkan\Drivers
            //    key or (modern PnP method) as a VulkanDriverName value under their
            //    display-adapter device key. Either is proof of a real driver.
            let loader_present =
                std::path::Path::new("C:\\Windows\\System32\\vulkan-1.dll").exists();
            if !loader_present {
                return false;
            }
            Self::detect_windows_vulkan_icd()
        }
        #[cfg(target_os = "linux")]
        {
            std::path::Path::new("/usr/lib/x86_64-linux-gnu/libvulkan.so.1").exists()
                || std::path::Path::new("/usr/lib/libvulkan.so.1").exists()
                || std::path::Path::new("/usr/local/lib/libvulkan.so.1").exists()
        }
        #[cfg(target_os = "macos")]
        {
            false // macOS uses Metal, not Vulkan
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
        {
            false
        }
    }

    /// Check for a registered Vulkan ICD (installable client driver) on Windows.
    /// See detect_vulkan_support for why loader-DLL presence alone is not enough.
    #[cfg(target_os = "windows")]
    fn detect_windows_vulkan_icd() -> bool {
        // Legacy ICD registration: HKLM\SOFTWARE\Khronos\Vulkan\Drivers holds one
        // value per ICD manifest. reg.exe prints the values; REG_DWORD lines
        // appear only when at least one ICD is registered.
        if let Some(output) = Self::run_reg_query(&[
            "query",
            r"HKLM\SOFTWARE\Khronos\Vulkan\Drivers",
        ]) {
            if output.contains("REG_DWORD") {
                return true;
            }
        }

        // Modern PnP registration: display-adapter device keys carry a
        // VulkanDriverName (or VulkanDriverNameWow) value pointing at the ICD
        // manifest. Searched recursively across all adapters.
        for value_name in ["VulkanDriverName", "VulkanDriverNameWow"] {
            if let Some(output) = Self::run_reg_query(&[
                "query",
                r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}",
                "/s",
                "/v",
                value_name,
            ]) {
                if output.contains(value_name) {
                    return true;
                }
            }
        }

        false
    }

    /// Whether this machine can run the llama.cpp CUDA 12.4 engine.
    ///
    /// Verified requirements for CUDA 12.x applications:
    ///   - Windows driver >= 527.41 (Linux >= 525.60) — minor version compatibility floor
    ///   - Compute capability >= 5.0 (CUDA 12 dropped Kepler)
    /// When the driver answered nvidia-smi but the compute_cap field is
    /// unavailable, the driver-version gate alone decides (a driver new enough
    /// to pass the version floor always supports the compute_cap query).
    pub fn cuda12_usable(&self) -> bool {
        if !self.has_cuda {
            return false;
        }
        let min_driver = match self.platform {
            Platform::Windows => CUDA12_MIN_WINDOWS_DRIVER,
            _ => CUDA12_MIN_LINUX_DRIVER,
        };
        match &self.nvidia_driver {
            Some(info) => meets_cuda_floor(info, min_driver, CUDA12_MIN_COMPUTE_CAP),
            // nvidia-smi answered the presence probe but the detail query failed —
            // cannot confirm the floor, so do not select CUDA (Vulkan/CPU instead).
            None => false,
        }
    }

    /// Whether this machine can run the llama.cpp CUDA 13.1 engine.
    /// CUDA 13 requires an r580+ driver and dropped Maxwell/Pascal/Volta
    /// (compute capability floor 7.5 / Turing).
    pub fn cuda13_usable(&self) -> bool {
        if !self.has_cuda {
            return false;
        }
        match &self.nvidia_driver {
            Some(info) => meets_cuda_floor(info, CUDA13_MIN_DRIVER, CUDA13_MIN_COMPUTE_CAP),
            None => false,
        }
    }

    // Runtime binary resolution is handled by the engine registry
    // (EngineRegistry::get_default_engine_binary_path), which knows the exact
    // install path from the downloaded engine metadata.  There is no fallback
    // static path here because any hardcoded version string would go stale.
}

/// Parse a leading "<major>.<minor>" pair from a string such as "537.13" or
/// "8.6". Ignores surrounding whitespace. A bare integer ("580") parses with
/// minor 0. Returns None when no leading digits exist.
pub fn parse_version_pair(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let mut parts = s.splitn(2, '.');
    let major: u32 = parts
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    let minor: u32 = parts
        .next()
        .map(|m| {
            m.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    Some((major, minor))
}

/// Pure floor comparison so the gating logic is unit-testable without hardware.
/// Compute capability is only enforced when known (see cuda12_usable for why an
/// unknown value alongside a passing driver check is not reachable in practice).
pub fn meets_cuda_floor(
    info: &NvidiaDriverInfo,
    min_driver: (u32, u32),
    min_compute_cap: (u32, u32),
) -> bool {
    if info.driver_version < min_driver {
        return false;
    }
    match info.max_compute_cap {
        Some(cc) => cc >= min_compute_cap,
        None => true,
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Windows => write!(f, "Windows"),
            Platform::Linux => write!(f, "Linux"),
            Platform::MacOS => write!(f, "MacOS"),
        }
    }
}

impl std::fmt::Display for HardwareArchitecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HardwareArchitecture::X86_64 => write!(f, "x86_64"),
            HardwareArchitecture::Aarch64 => write!(f, "aarch64"),
            HardwareArchitecture::Other(s) => write!(f, "{}", s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version_pair() {
        assert_eq!(parse_version_pair("537.13"), Some((537, 13)));
        assert_eq!(parse_version_pair(" 580.88 "), Some((580, 88)));
        assert_eq!(parse_version_pair("8.6"), Some((8, 6)));
        assert_eq!(parse_version_pair("580"), Some((580, 0)));
        assert_eq!(parse_version_pair("537.13\r"), Some((537, 13)));
        assert_eq!(parse_version_pair(""), None);
        assert_eq!(parse_version_pair("N/A"), None);
        assert_eq!(parse_version_pair("[N/A]"), None);
    }

    fn info(driver: (u32, u32), cc: Option<(u32, u32)>) -> NvidiaDriverInfo {
        NvidiaDriverInfo { driver_version: driver, max_compute_cap: cc }
    }

    #[test]
    fn test_cuda12_floor_windows() {
        // Modern GPU + modern driver: usable
        assert!(meets_cuda_floor(&info((551, 61), Some((8, 6))), CUDA12_MIN_WINDOWS_DRIVER, CUDA12_MIN_COMPUTE_CAP));
        // Exactly at the floor: usable
        assert!(meets_cuda_floor(&info((527, 41), Some((5, 0))), CUDA12_MIN_WINDOWS_DRIVER, CUDA12_MIN_COMPUTE_CAP));
        // Driver one minor below the floor: not usable
        assert!(!meets_cuda_floor(&info((527, 40), Some((8, 6))), CUDA12_MIN_WINDOWS_DRIVER, CUDA12_MIN_COMPUTE_CAP));
        // Old driver (e.g. Kepler-era 475.x): not usable
        assert!(!meets_cuda_floor(&info((475, 14), Some((3, 5))), CUDA12_MIN_WINDOWS_DRIVER, CUDA12_MIN_COMPUTE_CAP));
        // Kepler card behind a hypothetical new driver: compute cap gate rejects
        assert!(!meets_cuda_floor(&info((551, 61), Some((3, 5))), CUDA12_MIN_WINDOWS_DRIVER, CUDA12_MIN_COMPUTE_CAP));
        // Compute cap unknown but driver passes: allowed (driver gate dominates)
        assert!(meets_cuda_floor(&info((551, 61), None), CUDA12_MIN_WINDOWS_DRIVER, CUDA12_MIN_COMPUTE_CAP));
    }

    #[test]
    fn test_cuda13_floor() {
        // Turing on r580: usable
        assert!(meets_cuda_floor(&info((580, 88), Some((7, 5))), CUDA13_MIN_DRIVER, CUDA13_MIN_COMPUTE_CAP));
        // Pascal (6.1) on r580: rejected by compute cap
        assert!(!meets_cuda_floor(&info((580, 88), Some((6, 1))), CUDA13_MIN_DRIVER, CUDA13_MIN_COMPUTE_CAP));
        // Turing on pre-580 driver: rejected by driver
        assert!(!meets_cuda_floor(&info((551, 61), Some((7, 5))), CUDA13_MIN_DRIVER, CUDA13_MIN_COMPUTE_CAP));
    }

    #[test]
    fn test_cuda_usable_requires_driver_info() {
        let caps = HardwareCapabilities {
            platform: Platform::Windows,
            architecture: HardwareArchitecture::X86_64,
            has_cuda: true,
            has_metal: false,
            has_vulkan: true,
            nvidia_driver: None, // presence probe passed, detail query failed
            has_adreno_gpu: false,
        };
        // Without confirmed driver details, CUDA must not be selected
        assert!(!caps.cuda12_usable());
        assert!(!caps.cuda13_usable());

        let caps_ok = HardwareCapabilities {
            nvidia_driver: Some(NvidiaDriverInfo {
                driver_version: (551, 61),
                max_compute_cap: Some((8, 6)),
            }),
            ..caps
        };
        assert!(caps_ok.cuda12_usable());
        assert!(!caps_ok.cuda13_usable()); // driver below 580
    }
}
