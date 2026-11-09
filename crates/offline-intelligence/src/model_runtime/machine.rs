//! One picture of the machine, and one plan for running a model on it.
//!
//! The same design runs on a single-board computer with no GPU, a laptop, a
//! workstation with several GPUs, or a server whose model is split across
//! other machines. What changes from one to the next is only how much of
//! each kind of memory there is:
//!
//! - GPU memory: dedicated memory on one or more GPUs, or the share of
//!   unified memory a GPU may use (Apple Silicon), or none (CPU only);
//! - system RAM: what the CPU-side layers, their KV and the prefix cache use;
//! - storage: where the KV store keeps saved states.
//!
//! [`profile`] measures all three. [`plan`] then sizes the active window and
//! every budget from them and from the model's own shape: how many layers go
//! to the GPU, how large the context window is, how much RAM the engine's
//! prefix cache may take, how much storage the KV store may use, and whether
//! the embedding model has room on the GPU. Planning is a pure function so it
//! can be tested for every kind of machine without having one.

use std::path::Path;
use std::sync::RwLock;

use lazy_static::lazy_static;
use tracing::{info, warn};

/// How the machine's GPU memory relates to its RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accelerator {
    /// No usable GPU: every layer runs on the CPU from system RAM.
    None,
    /// One or more GPUs with their own memory.
    Dedicated,
    /// A GPU that shares system RAM (Apple Silicon): one pool for both.
    Unified,
}

/// What this machine has, measured now.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineProfile {
    pub os: &'static str,
    pub arch: &'static str,
    pub cpu_logical: usize,
    pub cpu_physical: usize,
    pub ram_total_mb: u64,
    pub ram_available_mb: u64,
    pub accelerator: Accelerator,
    /// GPU memory usable now, summed over every GPU (or the unified share).
    pub gpu_mb: u64,
    /// Whether `gpu_mb` is free memory (true) or total memory (false).
    pub gpu_mb_is_free: bool,
    pub gpu_source: Option<&'static str>,
    /// GPU memory on other machines that run part of the model through
    /// llama.cpp's RPC backend (declared, since it cannot be measured here).
    pub remote_gpu_mb: u64,
    pub storage_free_mb: Option<u64>,
    pub storage_total_mb: Option<u64>,
    pub storage_kind: Option<&'static str>,
}

/// The parts of a model's shape that decide its memory use.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelShape {
    pub layers: u64,
    pub weight_bytes: u64,
    /// KV bytes for one token in one layer, at the stored precision.
    pub kv_bytes_per_token_layer: f64,
    pub trained_ctx: u32,
    /// Sliding-window size and how many layers use it (0 = none).
    pub swa_window: u32,
    pub swa_layers: u64,
    /// GPU compute (scratch) buffer llama.cpp allocates besides weights and KV:
    /// dominated by the logits of one micro-batch over the vocabulary.
    pub compute_bytes: u64,
}

impl ModelShape {
    pub fn from_gguf(info: &super::gguf_metadata::GgufModelInfo, kv_cache_type: &str) -> Self {
        let b = super::gpu_offload::kv_bytes_per_element(kv_cache_type);
        ModelShape {
            layers: info.block_count.max(1),
            weight_bytes: info.file_size,
            kv_bytes_per_token_layer: info.kv_values_per_token_layer() as f64 * b,
            trained_ctx: info.trained_context_length as u32,
            swa_window: if crate::tuning::flag_or("LLAMA_SWA_FULL", false) { 0 } else { info.sliding_window as u32 },
            swa_layers: info.swa_layers,
            compute_bytes: compute_buffer_bytes(info.vocab_size, info.embedding_length, ubatch()),
        }
    }

    /// Average KV cells per layer at a window of `ctx` tokens: sliding-window
    /// layers hold the window plus one micro-batch, the rest the whole window.
    pub fn kv_cells_per_layer(&self, ctx: u32) -> f64 {
        let layers = self.layers.max(1);
        if self.swa_window == 0 || self.swa_layers == 0 {
            return ctx as f64;
        }
        let swa = self.swa_layers.min(layers);
        let swa_cells = ctx.min(self.swa_window.saturating_add(ubatch() as u32)) as u64;
        ((layers - swa) * ctx as u64 + swa * swa_cells) as f64 / layers as f64
    }

    /// KV bytes for one token across every layer: Equation (1) of the design.
    pub fn kv_bytes_per_token(&self) -> f64 {
        self.kv_bytes_per_token_layer * self.layers as f64
    }
}

/// Limits the plan works within.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanLimits {
    /// Largest window to aim for (the model's trained length, a ceiling, or a
    /// size the user asked for).
    pub want_ctx: u32,
    /// Keep exactly `want_ctx` (the user fixed it) instead of shrinking it.
    pub ctx_fixed: bool,
    /// Smallest window worth running.
    pub min_ctx: u32,
    /// Upper bound on the engine's RAM prefix cache, in MB.
    pub cache_ram_cap_mb: u64,
    /// Upper bound on the KV store, in MB.
    pub kv_store_cap_mb: u64,
    /// GPU memory the embedding server needs if it runs on the GPU, and the
    /// EMBED_GPU policy that decides whether it is reserved.
    pub embed_gpu_mb: u64,
    pub embed_mode: EmbedMode,
}

/// EMBED_GPU: where the embedding server runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EmbedMode {
    /// GPU when that costs the chat model nothing, else CPU.
    Auto,
    /// Always GPU: its memory is reserved before chat layers are placed.
    Gpu,
    /// Always CPU: nothing reserved.
    Cpu,
}

impl EmbedMode {
    pub fn from_env() -> Self {
        match crate::tuning::var("EMBED_GPU").map(|v| v.to_ascii_lowercase()).as_deref() {
            Some("gpu") => EmbedMode::Gpu,
            Some("cpu") => EmbedMode::Cpu,
            _ => EmbedMode::Auto,
        }
    }
}

/// LLAMA_UBATCH_SIZE, llama-server's default 512 when unset.
pub fn ubatch() -> u64 {
    crate::tuning::parse_or("LLAMA_UBATCH_SIZE", 512u64).max(1)
}

/// Compute buffer estimate: logits of one micro-batch in f32 plus the
/// activations of one layer, and a fixed allowance for the graph. Measured
/// 505 MiB for gemma-2-2b (vocab 256k, ubatch 512); this gives 607 MiB.
pub fn compute_buffer_bytes(vocab: u64, n_embd: u64, ubatch: u64) -> u64 {
    let vocab = if vocab > 0 { vocab } else { 32_000 };
    ubatch * 4 * (vocab + 4 * n_embd) + 64 * 1024 * 1024
}

/// GPU memory the active embedding model needs: its weights, its KV cache at
/// EMBED_CTX_SIZE (f16) and its compute buffer. Falls back to
/// EMBED_GPU_NEED_MB when the model file cannot be read.
pub fn embedder_gpu_need_mb() -> u64 {
    let path = crate::rag::model::model_path();
    let Ok(info) = super::gguf_metadata::read_model_info(&path) else {
        return EMBED_GPU_NEED_MB;
    };
    let ctx = crate::tuning::parse_or("EMBED_CTX_SIZE", 4096u64);
    let ub = crate::tuning::parse_or("EMBED_UBATCH_SIZE", 2048u64);
    let kv = info.kv_bytes_per_layer(ctx) * info.block_count;
    // Pooled output: no logits over the vocabulary, only activations.
    let compute = ub * 4 * info.embedding_length * 8 + 64 * 1024 * 1024;
    (info.file_size + kv + compute) / (1024 * 1024)
}

/// How the model will run on this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourcePlan {
    /// Layers on the GPU, in llama.cpp's convention (layers + 1 = all of them
    /// including the output layer).
    pub gpu_layers: u32,
    pub ctx: u32,
    /// The model and its window fit in this machine's memory.
    pub fits: bool,
    pub cache_ram_mb: u64,
    pub kv_store_mb: u64,
    /// Enough GPU memory is left over for the embedding model.
    pub embed_on_gpu: bool,
    pub spare_gpu_mb: u64,
    /// GPU memory one chat layer takes (weights and its KV at the window).
    pub gpu_mb_per_layer: u64,
    /// The model's layer count.
    pub layers: u64,
    pub spare_ram_mb: u64,
    pub summary: String,
}

const MB: f64 = 1024.0 * 1024.0;
/// RAM kept for the operating system and this application.
const RAM_RESERVE_MB: u64 = 512;
/// Share of available RAM the plan may use.
const RAM_SHARE: f64 = 0.85;
/// GPU memory the embedding model needs to run on the GPU (weights, KV of a
/// 4,096-token two-slot server, and working buffers).
pub const EMBED_GPU_NEED_MB: u64 = 1536;
/// Share of free storage the KV store may use.
const STORAGE_SHARE: f64 = 0.5;
/// Below this much total RAM no language model can run usefully.
pub const MIN_RAM_MB: u64 = 1024;

fn gpu_reserve_mb(p: &MachineProfile) -> u64 {
    match (p.accelerator, p.gpu_mb_is_free) {
        (Accelerator::None, _) => 0,
        (_, true) => 256,
        (_, false) => (p.gpu_mb / 10).max(768),
    }
}

/// Size everything from the machine and the model.
///
/// With w the weight bytes per layer and k(C) the KV bytes per layer at a
/// window of C tokens, a layer costs w + k(C) wherever it runs. GPU layers
/// are as many as fit in GPU memory G; the rest must fit in RAM R:
///
///   g(C) = min(L, ⌊G / (w + k(C))⌋),  (L − g(C)) · (w + k(C)) ≤ R.
///
/// The window starts at the limit and is halved until both hold (or, when
/// the user fixed it, is kept). On unified memory G and R are one pool.
pub fn plan(p: &MachineProfile, m: &ModelShape, lim: &PlanLimits) -> ResourcePlan {
    if p.accelerator != Accelerator::Dedicated {
        return plan_with(p, m, lim, 0, false);
    }
    match lim.embed_mode {
        EmbedMode::Cpu => plan_with(p, m, lim, 0, false),
        EmbedMode::Gpu => plan_with(p, m, lim, lim.embed_gpu_mb, true),
        EmbedMode::Auto => {
            // Keep the embedder on the GPU only if the chat model still gets
            // the same layers and window with its memory set aside.
            let alone = plan_with(p, m, lim, 0, false);
            let shared = plan_with(p, m, lim, lim.embed_gpu_mb, true);
            if shared.gpu_layers == alone.gpu_layers && shared.ctx == alone.ctx && shared.fits == alone.fits {
                shared
            } else {
                alone
            }
        }
    }
}

fn plan_with(p: &MachineProfile, m: &ModelShape, lim: &PlanLimits, embed_mb: u64, embed_on_gpu: bool) -> ResourcePlan {
    let layers = m.layers.max(1);
    let w = m.weight_bytes as f64 / layers as f64;
    let gpu = if p.accelerator == Accelerator::None {
        0.0
    } else {
        // Reserve, compute buffer and the embedder come off the top before
        // any layer is placed.
        p.gpu_mb
            .saturating_sub(gpu_reserve_mb(p))
            .saturating_sub(m.compute_bytes / (1024 * 1024))
            .saturating_sub(embed_mb)
            .saturating_add(p.remote_gpu_mb) as f64
            * MB
    };
    let ram = ((p.ram_available_mb as f64 * RAM_SHARE) as u64).saturating_sub(RAM_RESERVE_MB) as f64 * MB;

    let fit = |ctx: u32| -> (u64, f64, bool) {
        let per_layer = w + m.kv_bytes_per_token_layer * m.kv_cells_per_layer(ctx);
        let g = ((gpu / per_layer).floor() as u64).min(layers);
        let gpu_used = g as f64 * per_layer;
        let cpu_need = (layers - g) as f64 * per_layer;
        let ram_left = match p.accelerator {
            // One pool: what the GPU takes is gone from RAM too.
            Accelerator::Unified => (ram - gpu_used).max(0.0),
            _ => ram,
        };
        (g, gpu_used, cpu_need <= ram_left)
    };

    let top = lim.want_ctx.max(lim.min_ctx);
    let mut ctx = top;
    let (mut g, mut gpu_used, mut ok) = fit(ctx);
    if !lim.ctx_fixed {
        while !ok && ctx > lim.min_ctx {
            ctx = (ctx / 2).max(lim.min_ctx);
            (g, gpu_used, ok) = fit(ctx);
        }
    }

    let cpu_used = (layers - g) as f64 * (w + m.kv_bytes_per_token_layer * m.kv_cells_per_layer(ctx));
    let spare_gpu = ((gpu - gpu_used).max(0.0) / MB) as u64;
    let spare_ram = match p.accelerator {
        Accelerator::Unified => ((ram - gpu_used - cpu_used).max(0.0) / MB) as u64,
        _ => ((ram - cpu_used).max(0.0) / MB) as u64,
    };
    let cache_ram_mb = lim.cache_ram_cap_mb.min(spare_ram / 4);
    let kv_store_mb = match p.storage_free_mb {
        Some(free) => lim.kv_store_cap_mb.min((free as f64 * STORAGE_SHARE) as u64),
        None => lim.kv_store_cap_mb,
    };
    let gpu_layers = if g >= layers { (layers + 1) as u32 } else { g as u32 };

    let summary = format!(
        "{}; {} of {} layers on the GPU, window {} tokens{}, KV {:.0} KB/token ({} sliding-window layers), compute buffer {} MB, prefix cache {} MB, KV store {} MB, embedder on {}{}",
        describe(p),
        g,
        layers,
        ctx,
        if ok { "" } else { " (does not fit: the engine will page from disk and run slowly)" },
        m.kv_bytes_per_token() / 1024.0,
        m.swa_layers.min(layers),
        m.compute_bytes / (1024 * 1024),
        cache_ram_mb,
        kv_store_mb,
        if embed_on_gpu { "GPU" } else { "CPU" },
        if embed_on_gpu { format!(" ({} MB reserved)", embed_mb) } else { String::new() },
    );
    let gpu_mb_per_layer = ((w + m.kv_bytes_per_token_layer * m.kv_cells_per_layer(ctx)) / MB).ceil() as u64;
    ResourcePlan { gpu_layers, ctx, fits: ok, cache_ram_mb, kv_store_mb, embed_on_gpu, spare_gpu_mb: spare_gpu, gpu_mb_per_layer, layers, spare_ram_mb: spare_ram, summary }
}

/// A one-line description of the machine, by resources rather than by name.
pub fn describe(p: &MachineProfile) -> String {
    let gpu = match p.accelerator {
        Accelerator::None => "CPU only".to_string(),
        Accelerator::Unified => format!("unified memory, {:.1} GB usable by the GPU", p.gpu_mb as f64 / 1024.0),
        Accelerator::Dedicated => format!(
            "GPU memory {:.1} GB {}",
            p.gpu_mb as f64 / 1024.0,
            if p.gpu_mb_is_free { "free" } else { "total" }
        ),
    };
    let remote = if p.remote_gpu_mb > 0 { format!(" + {:.1} GB on other machines", p.remote_gpu_mb as f64 / 1024.0) } else { String::new() };
    let storage = match (p.storage_free_mb, p.storage_kind) {
        (Some(f), Some(k)) => format!(", storage {:.0} GB free ({})", f as f64 / 1024.0, k),
        (Some(f), None) => format!(", storage {:.0} GB free", f as f64 / 1024.0),
        _ => String::new(),
    };
    format!(
        "{} {}, {} cores ({} threads), RAM {:.1} of {:.1} GB available, {}{}{}",
        p.os,
        p.arch,
        p.cpu_physical,
        p.cpu_logical,
        p.ram_available_mb as f64 / 1024.0,
        p.ram_total_mb as f64 / 1024.0,
        gpu,
        remote,
        storage
    )
}

/// Measure this machine. `storage_dir` is where the KV store keeps its files.
pub fn profile(storage_dir: &Path) -> MachineProfile {
    use sysinfo::{DiskKind, Disks, System};
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu();
    let cpu_logical = sys.cpus().len().max(1);
    let cpu_physical = sys.physical_core_count().unwrap_or(cpu_logical).max(1);
    let ram_total_mb = sys.total_memory() / 1024 / 1024;
    let ram_available_mb = sys.available_memory() / 1024 / 1024;

    let (accelerator, gpu_mb, gpu_mb_is_free, gpu_source) = match crate::config::detect_available_vram_mb() {
        Some((mb, src)) if src == "unified-free" => (Accelerator::Unified, mb, true, Some(src)),
        Some((mb, src)) if mb > 0 => (Accelerator::Dedicated, mb, src.ends_with("free"), Some(src)),
        _ => (Accelerator::None, 0, true, None),
    };
    let remote_gpu_mb = crate::tuning::parse_or("RPC_GPU_MEMORY_MB", 0u64);

    let disks = Disks::new_with_refreshed_list();
    let target = resolved_dir(storage_dir);
    let disk = disks
        .list()
        .iter()
        .filter(|d| target.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len());
    let (storage_free_mb, storage_total_mb, storage_kind) = match disk {
        Some(d) => (
            Some(d.available_space() / 1024 / 1024),
            Some(d.total_space() / 1024 / 1024),
            Some(match d.kind() {
                DiskKind::SSD => "SSD",
                DiskKind::HDD => "HDD",
                _ => "unknown type",
            }),
        ),
        None => (None, None, None),
    };

    let p = MachineProfile {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cpu_logical,
        cpu_physical,
        ram_total_mb,
        ram_available_mb,
        accelerator,
        gpu_mb,
        gpu_mb_is_free,
        gpu_source,
        remote_gpu_mb,
        storage_free_mb,
        storage_total_mb,
        storage_kind,
    };
    if p.ram_total_mb < MIN_RAM_MB {
        warn!("Machine has {} MB of RAM, below the {} MB a language model needs", p.ram_total_mb, MIN_RAM_MB);
    }
    p
}

/// `dir` as an absolute path comparable with disk mount points: resolved
/// through its nearest existing ancestor (the folder may not exist yet), with
/// Windows' verbatim prefix removed.
fn resolved_dir(dir: &Path) -> std::path::PathBuf {
    let mut probe = Some(dir);
    while let Some(d) = probe {
        if let Ok(real) = std::fs::canonicalize(d) {
            let rest = dir.strip_prefix(d).unwrap_or(Path::new(""));
            return strip_verbatim(&real).join(rest);
        }
        probe = d.parent();
    }
    dir.to_path_buf()
}

/// `\\?\C:\x` → `C:\x` and `\\?\UNC\srv\share` → `\\srv\share`; other paths unchanged.
fn strip_verbatim(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{}", rest));
    }
    std::path::PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

lazy_static! {
    static ref CURRENT: RwLock<Option<ResourcePlan>> = RwLock::new(None);
}

/// Record the plan the running model was started with.
pub fn set_current(plan: ResourcePlan) {
    info!("Resource plan: {}", plan.summary);
    if let Ok(mut g) = CURRENT.write() {
        *g = Some(plan);
    }
}

/// The plan the running model was started with, if one was made.
pub fn current() -> Option<ResourcePlan> {
    CURRENT.read().ok().and_then(|g| g.clone())
}

/// GPU memory a vision projector of this file size needs: its weights plus
/// the image encoder's working buffers (about a quarter more).
pub fn projector_gpu_need_mb(file_bytes: u64) -> u64 {
    file_bytes / (1024 * 1024) * 5 / 4
}

/// LLAMA_MMPROJ_OFFLOAD (auto/true, false): where the vision projector runs
/// while it is loaded (only while images are being read, see
/// lifecycle::with_projector). auto: on the GPU, with as many chat layers
/// moved to the CPU as it needs beyond the plan's spare memory; false: on
/// the CPU, chat layers unchanged.
///
/// Returns the chat layers to use with the projector on the GPU, or None to
/// run it on the CPU. Measured (gemma-3-4b Q4_K_M + 851 MB BF16 projector,
/// RTX 3050 Ti 4 GB): projector on the GPU with all 35 layers fails to load;
/// with 23 layers an image is read in 2.7-3.5 s; on the CPU in 48 s.
pub fn projector_gpu_layers(file_bytes: u64, plan: Option<&ResourcePlan>) -> Option<u32> {
    if crate::tuning::flag("LLAMA_MMPROJ_OFFLOAD") == Some(false) {
        return None;
    }
    let Some(p) = plan else { return Some(u32::MAX) };
    let need = projector_gpu_need_mb(file_bytes);
    if p.spare_gpu_mb >= need {
        return Some(p.gpu_layers);
    }
    // Layers to move: the shortfall in whole layers, plus one for margin
    // (the output layer leaves the GPU with them).
    let per_layer = p.gpu_mb_per_layer.max(1);
    let moved = (need - p.spare_gpu_mb).div_ceil(per_layer) + 1;
    let on_gpu = (p.gpu_layers.min(p.layers as u32) as u64).saturating_sub(moved);
    (on_gpu > 0).then_some(on_gpu as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024;

    fn machine(accel: Accelerator, gpu_gb: u64, ram_gb: u64, storage_gb: Option<u64>) -> MachineProfile {
        MachineProfile {
            os: "test",
            arch: "test",
            cpu_logical: 8,
            cpu_physical: 4,
            ram_total_mb: ram_gb * GB,
            ram_available_mb: ram_gb * GB,
            accelerator: accel,
            gpu_mb: gpu_gb * GB,
            gpu_mb_is_free: true,
            gpu_source: None,
            remote_gpu_mb: 0,
            storage_free_mb: storage_gb.map(|g| g * GB),
            storage_total_mb: storage_gb.map(|g| g * GB),
            storage_kind: Some("SSD"),
        }
    }

    /// Llama-3.2-3B-like: 28 layers, 2.0 GB of weights, 8 KV heads of 128, q8_0.
    fn small() -> ModelShape {
        ModelShape { layers: 28, weight_bytes: 2_000 * 1024 * 1024, kv_bytes_per_token_layer: 2.0 * 1024.0 * 1.0625, trained_ctx: 131_072, swa_window: 0, swa_layers: 0, compute_bytes: 300 * 1024 * 1024 }
    }

    /// Llama-3.1-70B-like: 80 layers, 40 GB of weights, 8 KV heads of 128, q8_0.
    fn large() -> ModelShape {
        ModelShape { layers: 80, weight_bytes: 40 * 1024 * 1024 * 1024, kv_bytes_per_token_layer: 2.0 * 1024.0 * 1.0625, trained_ctx: 131_072, swa_window: 0, swa_layers: 0, compute_bytes: 300 * 1024 * 1024 }
    }

    fn limits(want: u32) -> PlanLimits {
        PlanLimits {
            want_ctx: want,
            ctx_fixed: false,
            min_ctx: 512,
            cache_ram_cap_mb: 1024,
            kv_store_cap_mb: 20 * GB,
            embed_gpu_mb: 1200,
            embed_mode: EmbedMode::Auto,
        }
    }

    #[test]
    fn sliding_window_layers_only_pay_for_the_window() {
        let full = small();
        let swa = ModelShape { swa_window: 4096, swa_layers: 14, ..small() };
        // Measured on gemma-2: window 4096 + ubatch 512 = 4608 cells.
        assert_eq!(swa.kv_cells_per_layer(4096), 4096.0);
        assert_eq!(swa.kv_cells_per_layer(32_768), (14.0 * 32_768.0 + 14.0 * 4608.0) / 28.0);
        let laptop = machine(Accelerator::Dedicated, 4, 64, Some(500));
        let lim = PlanLimits { ctx_fixed: true, ..limits(32_768) };
        assert!(plan(&laptop, &swa, &lim).gpu_layers >= plan(&laptop, &full, &lim).gpu_layers);
    }

    #[test]
    fn the_compute_buffer_is_taken_from_gpu_memory() {
        let laptop = machine(Accelerator::Dedicated, 3, 32, Some(500));
        let lean = ModelShape { compute_bytes: 0, ..small() };
        let heavy = ModelShape { compute_bytes: 1024 * 1024 * 1024, ..small() };
        assert!(plan(&laptop, &heavy, &limits(8192)).gpu_layers < plan(&laptop, &lean, &limits(8192)).gpu_layers);
        // gemma-2-2b: vocab 256,000, n_embd 2,304, ubatch 512 is at least the measured 505 MiB.
        assert!(compute_buffer_bytes(256_000, 2304, 512) / (1024 * 1024) >= 505);
    }

    #[test]
    fn the_embedder_is_reserved_on_the_gpu_or_moved_to_the_cpu() {
        let big = machine(Accelerator::Dedicated, 24, 64, Some(500));
        let p = plan(&big, &small(), &limits(8192));
        assert!(p.embed_on_gpu && p.gpu_layers == 29, "{}", p.summary);
        // Tight: auto keeps every chat layer and sends the embedder to the CPU.
        let tight = machine(Accelerator::Dedicated, 3, 64, Some(500));
        let alone = plan(&tight, &small(), &PlanLimits { embed_mode: EmbedMode::Cpu, ..limits(8192) });
        let auto = plan(&tight, &small(), &limits(8192));
        assert_eq!(auto.gpu_layers, alone.gpu_layers);
        assert!(!auto.embed_on_gpu);
        // EMBED_GPU=gpu: the embedder's memory is taken first, even at a cost.
        let forced = plan(&tight, &small(), &PlanLimits { embed_mode: EmbedMode::Gpu, ..limits(8192) });
        assert!(forced.embed_on_gpu && forced.gpu_layers <= alone.gpu_layers);
    }

    #[test]
    fn kv_per_token_follows_the_formula() {
        // 2 x 28 x 8 x 128 x 1.0625 bytes = 60,928 bytes.
        assert_eq!(small().kv_bytes_per_token().round() as u64, 60_928);
    }

    #[test]
    fn a_cpu_only_single_board_computer_runs_everything_from_ram() {
        let pi = machine(Accelerator::None, 0, 8, Some(64));
        let p = plan(&pi, &small(), &limits(32_768));
        assert_eq!(p.gpu_layers, 0);
        assert!(p.fits, "{}", p.summary);
        assert!(!p.embed_on_gpu);
        assert!(p.ctx >= 4096, "{}", p.summary);
        assert!(p.kv_store_mb <= 32 * GB, "storage share bounds the store on a small card");
    }

    #[test]
    fn a_laptop_gpu_takes_what_fits_and_the_window_shrinks_to_fit_ram() {
        let laptop = machine(Accelerator::Dedicated, 4, 16, Some(500));
        let p = plan(&laptop, &small(), &limits(32_768));
        assert!(p.fits, "{}", p.summary);
        assert!(p.gpu_layers > 0 && p.gpu_layers <= 29, "{}", p.summary);
    }

    #[test]
    fn a_large_gpu_holds_the_whole_model_and_has_room_for_the_embedder() {
        let workstation = machine(Accelerator::Dedicated, 48, 128, Some(2_000));
        let p = plan(&workstation, &small(), &limits(32_768));
        assert_eq!(p.gpu_layers, 29, "all layers plus the output layer");
        assert_eq!(p.ctx, 32_768);
        assert!(p.embed_on_gpu);
    }

    #[test]
    fn a_large_model_is_split_between_gpus_and_ram_and_marked_when_it_cannot_fit() {
        let server = machine(Accelerator::Dedicated, 2 * 16, 128, Some(4_000));
        let p = plan(&server, &large(), &limits(32_768));
        assert!(p.fits && p.gpu_layers < 81 && p.gpu_layers > 0, "{}", p.summary);

        let tiny = machine(Accelerator::None, 0, 8, Some(64));
        let p = plan(&tiny, &large(), &limits(32_768));
        assert!(!p.fits, "a 40 GB model cannot fit 8 GB: {}", p.summary);
        assert_eq!(p.ctx, 512, "the window falls to its floor before giving up");
    }

    #[test]
    fn unified_memory_is_one_pool_for_gpu_and_ram() {
        let mac = machine(Accelerator::Unified, 24, 32, Some(1_000));
        let p = plan(&mac, &small(), &limits(32_768));
        assert_eq!(p.gpu_layers, 29);
        assert!(!p.embed_on_gpu, "only a dedicated GPU counts as spare GPU memory for the embedder");
        assert!(p.spare_ram_mb < 32 * GB, "GPU use is subtracted from the shared pool");
    }

    #[test]
    fn remote_gpus_of_a_cluster_add_to_gpu_memory() {
        let mut head = machine(Accelerator::Dedicated, 24, 128, Some(4_000));
        let alone = plan(&head, &large(), &limits(8_192)).gpu_layers;
        head.remote_gpu_mb = 48 * GB;
        let together = plan(&head, &large(), &limits(8_192)).gpu_layers;
        assert!(together > alone, "{} vs {}", together, alone);
    }

    #[test]
    fn a_fixed_window_is_kept_even_if_it_does_not_fit() {
        let laptop = machine(Accelerator::None, 0, 4, Some(100));
        let lim = PlanLimits { ctx_fixed: true, ..limits(131_072) };
        let p = plan(&laptop, &small(), &lim);
        assert_eq!(p.ctx, 131_072);
        assert!(!p.fits);
    }

    #[test]
    fn the_kv_store_and_prefix_cache_shrink_with_free_storage_and_ram() {
        let p = plan(&machine(Accelerator::None, 0, 8, Some(10)), &small(), &limits(4_096));
        assert_eq!(p.kv_store_mb, 5 * GB, "half of 10 GB free");
        let p = plan(&machine(Accelerator::None, 0, 8, None), &small(), &limits(4_096));
        assert_eq!(p.kv_store_mb, 20 * GB, "unknown storage keeps the configured cap");
        let p = plan(&machine(Accelerator::None, 0, 2, Some(100)), &small(), &limits(4_096));
        assert!(p.cache_ram_mb < 1024, "prefix cache shrinks on a 2 GB machine: {}", p.cache_ram_mb);
    }

    #[test]
    fn total_gpu_memory_keeps_a_larger_reserve_than_free_memory() {
        let mut m = machine(Accelerator::Dedicated, 8, 32, None);
        let free = plan(&m, &large(), &limits(4_096)).gpu_layers;
        m.gpu_mb_is_free = false;
        let total = plan(&m, &large(), &limits(4_096)).gpu_layers;
        assert!(total <= free);
    }

    /// Measures the machine it runs on; run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn this_machine() {
        let p = profile(&std::env::temp_dir());
        println!("{}", describe(&p));
        assert!(p.ram_total_mb > 0 && p.cpu_logical > 0);
        let plan = plan(&p, &small(), &limits(32_768));
        println!("{}", plan.summary);
    }

    #[test]
    fn windows_verbatim_paths_compare_with_mount_points() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\Users\a")), std::path::PathBuf::from(r"C:\Users\a"));
        assert_eq!(strip_verbatim(Path::new(r"\\?\UNC\srv\share\x")), std::path::PathBuf::from(r"\\srv\share\x"));
        assert_eq!(strip_verbatim(Path::new("/mnt/nvme/kv")), std::path::PathBuf::from("/mnt/nvme/kv"));
        // A folder that does not exist yet resolves through its parent.
        let missing = std::env::temp_dir().join("oca_no_such_dir_yet").join("kv");
        assert!(resolved_dir(&missing).ends_with(Path::new("oca_no_such_dir_yet").join("kv")));
    }

    #[test]
    fn a_projector_takes_spare_memory_then_chat_layers() {
        let mut p = plan(&machine(Accelerator::Dedicated, 8_192, 16, Some(64)), &small(), &limits(8_192));
        let projector = 851 * 1024 * 1024;
        let need = projector_gpu_need_mb(projector);
        p.spare_gpu_mb = need;
        assert_eq!(projector_gpu_layers(projector, Some(&p)), Some(p.gpu_layers));
        // 710 MB short at 86 MB a layer: 9 layers move, plus one for margin.
        p.spare_gpu_mb = need - 710;
        p.gpu_mb_per_layer = 86;
        assert_eq!(projector_gpu_layers(projector, Some(&p)), Some(p.layers as u32 - 10));
        // Not even one layer left: the projector runs on the CPU.
        p.gpu_mb_per_layer = 1;
        p.spare_gpu_mb = 0;
        assert_eq!(projector_gpu_layers(projector, Some(&p)), None);
    }

    #[test]
    fn the_description_names_resources_not_products() {
        let d = describe(&machine(Accelerator::None, 0, 8, Some(64)));
        assert!(d.contains("CPU only") && d.contains("RAM") && d.contains("storage"), "{}", d);
    }

    /// Plan for a real model on this machine: PLAN_MODEL=<path.gguf>
    /// cargo test plan_for_a_real_model -- --ignored --nocapture
    #[test]
    #[ignore]
    fn plan_for_a_real_model() {
        let path = std::path::PathBuf::from(std::env::var("PLAN_MODEL").expect("PLAN_MODEL"));
        let info = super::super::gguf_metadata::read_model_info(&path).unwrap();
        println!("{:?}", info);
        let shape = ModelShape::from_gguf(&info, "q8_0");
        println!("{:?}", shape);
        let lim = PlanLimits {
            want_ctx: (info.trained_context_length as u32).min(32_768),
            ctx_fixed: false,
            min_ctx: 1024,
            cache_ram_cap_mb: 1024,
            kv_store_cap_mb: 20_480,
            embed_gpu_mb: embedder_gpu_need_mb(),
            embed_mode: EmbedMode::Auto,
        };
        let p = profile(&std::env::temp_dir());
        let planned = plan(&p, &shape, &lim);
        println!("{}; {} MB GPU spare", planned.summary, planned.spare_gpu_mb);
        if let Ok(mm) = std::env::var("PLAN_MMPROJ") {
            let bytes = std::fs::metadata(mm).unwrap().len();
            println!("projector needs {} MB; chat layers with it on the GPU: {:?}", projector_gpu_need_mb(bytes), projector_gpu_layers(bytes, Some(&planned)));
        }
    }
}
