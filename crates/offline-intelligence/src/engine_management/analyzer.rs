//! Hardware Analyzer
//!
//! Enhanced hardware detection and profiling for engine compatibility analysis.
//! Extends the existing platform detection with detailed capability assessment.

use crate::model_runtime::platform_detector::{HardwareCapabilities, Platform, HardwareArchitecture};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;
use tracing::debug;

/// Detailed hardware profile for engine compatibility analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareProfile {
    pub platform: Platform,
    pub architecture: HardwareArchitecture,
    pub cpu_cores: u32,
    pub total_memory_gb: f32,
    pub available_memory_gb: f32,
    pub gpu_info: Option<GPUInfo>,
    pub acceleration_support: HashMap<String, bool>,
    pub system_info: SystemInfo,
}

// Static cache for hardware profiles
static PROFILE_CACHE: OnceLock<HardwareProfile> = OnceLock::new();

impl HardwareProfile {
    /// Analyze hardware capabilities to create a detailed profile (cached)
    pub async fn analyze(capabilities: &HardwareCapabilities) -> Result<Self, Box<dyn std::error::Error>> {
        // Return cached result if available
        if let Some(cached) = PROFILE_CACHE.get() {
            return Ok(cached.clone());
        }
        
        let analyzer = HardwareAnalyzer::new(capabilities.clone());
        let profile = analyzer.get_hardware_profile();
        
        // Cache the result
        let _ = PROFILE_CACHE.set(profile.clone());
        
        Ok(profile)
    }
}

/// Information about available GPUs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GPUInfo {
    pub vendor: String,
    pub model: String,
    pub memory_gb: f32,
    pub compute_capability: Option<String>,
    pub driver_version: Option<String>,
}

/// General system information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub os_name: String,
    pub os_version: String,
    pub kernel_version: Option<String>,
}

/// Analyzes hardware capabilities for engine compatibility
pub struct HardwareAnalyzer {
    capabilities: HardwareCapabilities,
}

impl HardwareAnalyzer {
    pub fn new(capabilities: HardwareCapabilities) -> Self {
        Self { capabilities }
    }

    /// Get detailed hardware profile
    pub fn get_hardware_profile(&self) -> HardwareProfile {
        let system_info = self.detect_system_info();
        let gpu_info = self.detect_gpu_info();
        let memory_info = self.detect_memory_info();
        let acceleration_support = self.detect_acceleration_support(&gpu_info);

        HardwareProfile {
            platform: self.capabilities.platform.clone(),
            architecture: self.capabilities.architecture.clone(),
            cpu_cores: self.detect_cpu_cores(),
            total_memory_gb: memory_info.total_gb,
            available_memory_gb: memory_info.available_gb,
            gpu_info,
            acceleration_support,
            system_info,
        }
    }

    /// Detect CPU core count
    fn detect_cpu_cores(&self) -> u32 {
        let logical_cores = num_cpus::get() as u32;
        debug!("Detected {} logical CPU cores", logical_cores);
        logical_cores
    }

    /// Detect memory information
    fn detect_memory_info(&self) -> MemoryInfo {
        let mut system = sysinfo::System::new_all();
        system.refresh_memory();
        
        let total_bytes = system.total_memory();
        let available_bytes = system.available_memory();
        
        let total_gb = total_bytes as f32 / (1024.0 * 1024.0 * 1024.0);
        let available_gb = available_bytes as f32 / (1024.0 * 1024.0 * 1024.0);
        
        debug!("Memory: {:.1}GB total, {:.1}GB available", total_gb, available_gb);
        
        MemoryInfo {
            total_gb,
            available_gb,
        }
    }

    /// Detect GPU information
    fn detect_gpu_info(&self) -> Option<GPUInfo> {
        match &self.capabilities.platform {
            Platform::Windows => self.detect_windows_gpu(),
            Platform::Linux => self.detect_linux_gpu(),
            Platform::MacOS => self.detect_macos_gpu(),
        }
    }

    /// Detect Windows GPU information using three independent methods for reliability.
    /// No fallback to capability flags — each method queries the hardware directly.
    fn detect_windows_gpu(&self) -> Option<GPUInfo> {
        #[cfg(target_os = "windows")]
        {
            // Method 1: Get-CimInstance — modern WMI, preferred on Windows 8+
            if let Some(gpu) = Self::run_powershell_gpu_query(
                "Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name"
            ) {
                return Some(gpu);
            }

            // Method 2: Get-WmiObject — legacy WMI, still present on all Windows 10/11.
            // Succeeds when CIM namespace is broken but WMI itself is intact.
            if let Some(gpu) = Self::run_powershell_gpu_query(
                "Get-WmiObject Win32_VideoController | Select-Object -ExpandProperty Name"
            ) {
                return Some(gpu);
            }

            // Method 3: Registry scan of the Windows display adapter class key.
            // The DriverDesc value exists for every installed GPU driver regardless of WMI.
            // Works even if the winmgmt service is stopped or corrupted.
            if let Some(gpu) = Self::run_powershell_gpu_query(
                "Get-ChildItem 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}' | \
                 ForEach-Object { (Get-ItemProperty $_.PSPath -Name DriverDesc -ErrorAction SilentlyContinue).DriverDesc } | \
                 Where-Object { $_ }"
            ) {
                return Some(gpu);
            }
        }

        None
    }

    /// Run a PowerShell command and parse the first matching GPU from its output.
    #[cfg(target_os = "windows")]
    fn run_powershell_gpu_query(command: &str) -> Option<GPUInfo> {
        use std::process::{Command, Stdio};
        use std::os::windows::process::CommandExt;

        let child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", command])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x08000000) // CREATE_NO_WINDOW — no console flash
            .spawn();

        if let Ok(mut process) = child {
            let start = std::time::Instant::now();
            loop {
                match process.try_wait() {
                    Ok(Some(status)) => {
                        if status.success() {
                            if let Ok(output) = process.wait_with_output() {
                                let stdout = String::from_utf8_lossy(&output.stdout);
                                for line in stdout.lines() {
                                    if let Some(gpu) = Self::parse_gpu_name(line.trim()) {
                                        return Some(gpu);
                                    }
                                }
                            }
                        }
                        break;
                    }
                    Ok(None) => {
                        if start.elapsed() > std::time::Duration::from_secs(10) {
                            let _ = process.kill();
                            let _ = process.wait();
                            debug!("GPU detection query timed out: {}", &command[..command.len().min(60)]);
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
        }

        None
    }

    /// Parse a raw GPU name string into a GPUInfo with vendor and acceleration type.
    ///
    /// NVIDIA: matched by "NVIDIA", "GeForce", "Quadro", "Tesla", "RTX" (OEM boards
    ///   sometimes omit the "NVIDIA" prefix but always carry the product family name).
    /// AMD:    matched by "AMD", "Radeon", "ATI".
    /// Intel:  matched by "Intel" + a GPU keyword — the GPU keyword requirement prevents
    ///   false matches against CPU strings like "Intel(R) Core(TM) i9" which also contain
    ///   "Intel" but never contain Arc/Iris/UHD/HD Graphics/Xe.
    #[cfg(target_os = "windows")]
    fn parse_gpu_name(trimmed: &str) -> Option<GPUInfo> {
        if trimmed.is_empty() {
            return None;
        }

        if trimmed.contains("NVIDIA")
            || trimmed.contains("GeForce")
            || trimmed.contains("Quadro")
            || trimmed.contains("Tesla")
            || trimmed.contains("RTX")
        {
            Some(GPUInfo {
                vendor: "NVIDIA".to_string(),
                model: trimmed.to_string(),
                memory_gb: 0.0,
                compute_capability: Some("CUDA".to_string()),
                driver_version: None,
            })
        } else if trimmed.contains("AMD")
            || trimmed.contains("Radeon")
            || trimmed.contains("ATI")
        {
            Some(GPUInfo {
                vendor: "AMD".to_string(),
                model: trimmed.to_string(),
                memory_gb: 0.0,
                compute_capability: Some("Vulkan".to_string()),
                driver_version: None,
            })
        } else if trimmed.contains("Intel")
            && (trimmed.contains("Arc")
                || trimmed.contains("Iris")
                || trimmed.contains("UHD")
                || trimmed.contains("HD Graphics")
                || trimmed.contains("Xe"))
        {
            Some(GPUInfo {
                vendor: "Intel".to_string(),
                model: trimmed.to_string(),
                memory_gb: 0.0,
                compute_capability: Some("Vulkan".to_string()),
                driver_version: None,
            })
        } else {
            None
        }
    }

    /// Detect Linux GPU information
    fn detect_linux_gpu(&self) -> Option<GPUInfo> {
        use std::process::{Command, Stdio};

        // Try lspci with timeout to prevent hangs
        let child = Command::new("lspci")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        if let Ok(mut process) = child {
            let start = std::time::Instant::now();
            loop {
                match process.try_wait() {
                    Ok(Some(_)) => {
                        if let Ok(output) = process.wait_with_output() {
                            let stdout = String::from_utf8_lossy(&output.stdout);
                            for line in stdout.lines() {
                                if line.contains("VGA compatible controller") || line.contains("3D controller") {
                                    if line.contains("NVIDIA") {
                                        return Some(GPUInfo {
                                            vendor: "NVIDIA".to_string(),
                                            model: line.split(": ").nth(1).unwrap_or("Unknown").to_string(),
                                            memory_gb: 0.0,
                                            compute_capability: None,
                                            driver_version: None,
                                        });
                                    } else if line.contains("AMD") || line.contains("ATI") {
                                        return Some(GPUInfo {
                                            vendor: "AMD".to_string(),
                                            model: line.split(": ").nth(1).unwrap_or("Unknown").to_string(),
                                            memory_gb: 0.0,
                                            compute_capability: None,
                                            driver_version: None,
                                        });
                                    }
                                }
                            }
                        }
                        break;
                    }
                    Ok(None) => {
                        if start.elapsed() > std::time::Duration::from_secs(5) {
                            let _ = process.kill();
                            let _ = process.wait();
                            debug!("lspci GPU detection timed out");
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        }

        None
    }

    /// Detect macOS GPU information
    fn detect_macos_gpu(&self) -> Option<GPUInfo> {
        #[cfg(target_os = "macos")]
        {
            // On macOS, Metal support is the key indicator - skip slow system_profiler call
            // and use the already-detected has_metal flag from HardwareCapabilities
            if self.capabilities.has_metal {
                return Some(GPUInfo {
                    vendor: "Apple".to_string(),
                    model: "Integrated GPU".to_string(),
                    memory_gb: 0.0,
                    compute_capability: Some("Metal".to_string()),
                    driver_version: None,
                });
            }
        }
        
        None
    }

    /// Detect acceleration support capabilities
    fn detect_acceleration_support(&self, _gpu_info: &Option<GPUInfo>) -> HashMap<String, bool> {
        let mut support = HashMap::new();
        
        // CPU support is always available
        support.insert("cpu".to_string(), true);
        
        // Platform-specific acceleration
        match &self.capabilities.platform {
            Platform::Windows => {
                support.insert("cuda".to_string(), self.capabilities.has_cuda);
                support.insert("vulkan".to_string(), self.capabilities.has_vulkan);
                support.insert("directml".to_string(), true); // DirectML is available on Windows
            }
            Platform::MacOS => {
                support.insert("metal".to_string(), self.capabilities.has_metal);
            }
            Platform::Linux => {
                support.insert("cuda".to_string(), self.capabilities.has_cuda);
                support.insert("vulkan".to_string(), self.capabilities.has_vulkan);
                // Check for ROCm support on Linux
                support.insert("rocm".to_string(), self.detect_rocm_support());
            }
        }
        
        // Check for OpenCL support
        support.insert("opencl".to_string(), self.detect_opencl_support());
        
        debug!("Acceleration support: {:?}", support);
        support
    }

    /// Detect ROCm support (AMD GPU acceleration on Linux)
    fn detect_rocm_support(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            use std::path::Path;
            // Check for ROCm installation
            Path::new("/opt/rocm").exists() || Path::new("/usr/lib/rocm").exists()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Detect OpenCL support
    fn detect_opencl_support(&self) -> bool {
        // Simplified check - in practice you'd want to enumerate OpenCL platforms
        match &self.capabilities.platform {
            Platform::Windows => {
                // Check for OpenCL ICD loader
                std::path::Path::new("C:\\Windows\\System32\\OpenCL.dll").exists()
            }
            Platform::Linux => {
                std::path::Path::new("/usr/lib/libOpenCL.so").exists() ||
                std::path::Path::new("/usr/lib/x86_64-linux-gnu/libOpenCL.so").exists()
            }
            Platform::MacOS => {
                // OpenCL is deprecated on macOS but still available
                true
            }
        }
    }

    /// Detect system information
    fn detect_system_info(&self) -> SystemInfo {
        SystemInfo {
            os_name: std::env::consts::OS.to_string(),
            os_version: "Unknown".to_string(),
            kernel_version: None,
        }
    }

    /// Get compatibility recommendations for engines
    pub fn get_engine_recommendations(&self) -> Vec<EngineRecommendation> {
        let profile = self.get_hardware_profile();
        let mut recommendations = Vec::new();
        
        // CPU recommendation (always available)
        recommendations.push(EngineRecommendation {
            engine_type: "CPU".to_string(),
            priority: 1,
            reason: "Universal compatibility".to_string(),
            estimated_performance: self.estimate_cpu_performance(&profile),
        });
        
        // GPU recommendations based on detected hardware
        if profile.acceleration_support.get("cuda").copied().unwrap_or(false) {
            recommendations.push(EngineRecommendation {
                engine_type: "CUDA".to_string(),
                priority: 2,
                reason: "NVIDIA GPU acceleration available".to_string(),
                estimated_performance: self.estimate_cuda_performance(&profile),
            });
        }
        
        if profile.acceleration_support.get("metal").copied().unwrap_or(false) {
            recommendations.push(EngineRecommendation {
                engine_type: "Metal".to_string(),
                priority: 2,
                reason: "Apple Silicon GPU acceleration".to_string(),
                estimated_performance: self.estimate_metal_performance(&profile),
            });
        }
        
        if profile.acceleration_support.get("vulkan").copied().unwrap_or(false) {
            recommendations.push(EngineRecommendation {
                engine_type: "Vulkan".to_string(),
                priority: 3,
                reason: "Cross-platform GPU acceleration".to_string(),
                estimated_performance: self.estimate_vulkan_performance(&profile),
            });
        }
        
        // Sort by priority
        recommendations.sort_by(|a, b| a.priority.cmp(&b.priority));
        recommendations
    }

    /// Estimate CPU performance
    fn estimate_cpu_performance(&self, profile: &HardwareProfile) -> PerformanceEstimate {
        let core_multiplier = profile.cpu_cores as f32;
        let memory_multiplier = (profile.available_memory_gb / 8.0).min(4.0);
        let base_score = 50.0;
        
        PerformanceEstimate {
            inference_speed: base_score * core_multiplier * memory_multiplier * 0.1,
            memory_efficiency: (profile.available_memory_gb / profile.total_memory_gb) * 100.0,
            power_efficiency: 70.0, // CPUs are generally less power efficient than GPUs
        }
    }

    /// Estimate CUDA performance
    fn estimate_cuda_performance(&self, profile: &HardwareProfile) -> PerformanceEstimate {
        if let Some(gpu) = &profile.gpu_info {
            let memory_score = gpu.memory_gb * 10.0; // Rough estimation
            let base_score = 80.0;
            
            PerformanceEstimate {
                inference_speed: base_score + memory_score,
                memory_efficiency: 90.0, // GPUs excel at memory efficiency
                power_efficiency: 85.0, // Modern GPUs are quite power efficient
            }
        } else {
            PerformanceEstimate {
                inference_speed: 0.0,
                memory_efficiency: 0.0,
                power_efficiency: 0.0,
            }
        }
    }

    /// Estimate Metal performance
    fn estimate_metal_performance(&self, _profile: &HardwareProfile) -> PerformanceEstimate {
        // Apple Silicon typically has excellent unified memory performance
        PerformanceEstimate {
            inference_speed: 75.0,
            memory_efficiency: 95.0, // Unified memory architecture
            power_efficiency: 90.0, // Apple Silicon is very power efficient
        }
    }

    /// Estimate Vulkan performance
    fn estimate_vulkan_performance(&self, _profile: &HardwareProfile) -> PerformanceEstimate {
        // Vulkan performance varies significantly by GPU
        PerformanceEstimate {
            inference_speed: 70.0,
            memory_efficiency: 85.0,
            power_efficiency: 80.0,
        }
    }
}

/// Memory information structure
#[derive(Debug)]
struct MemoryInfo {
    total_gb: f32,
    available_gb: f32,
}

/// Engine recommendation with priority
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct EngineRecommendation {
    pub engine_type: String,
    pub priority: u32,
    pub reason: String,
    pub estimated_performance: PerformanceEstimate,
}

/// Performance estimates for different engine types
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PerformanceEstimate {
    pub inference_speed: f32,      // Relative speed score (higher is better)
    pub memory_efficiency: f32,    // Percentage (0-100)
    pub power_efficiency: f32,     // Percentage (0-100)
}