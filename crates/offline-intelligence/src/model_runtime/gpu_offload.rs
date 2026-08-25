//! Dynamic GPU layer offload planning.
//!
//! Replaces the static VRAM-bucket → fixed-layer table with the calculation
//! Ollama and LM Studio perform: read the model's real layer count and size
//! from its GGUF header, estimate the KV-cache cost per layer at the active
//! context size, and offload exactly as many layers as fit in the measured
//! VRAM. Small models get fully offloaded (all layers + output layer);
//! oversized models get exactly what fits instead of a guess.
//!
//! The decision and every input to it are logged at info level so a slow
//! machine can always be diagnosed from the log alone.

use std::path::Path;
use tracing::info;

use super::gguf_metadata::{read_model_info, GgufModelInfo};

/// How the VRAM figure was measured — determines the safety reserve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VramKind {
    /// Free (currently unused) VRAM — display/compositor use already excluded.
    Free,
    /// Total adapter VRAM — the OS/display share must be reserved from it.
    Total,
}

/// Result of the offload calculation, with the inputs kept for logging/UI.
#[derive(Debug, Clone)]
pub struct OffloadPlan {
    /// Value for llama-server --n-gpu-layers.
    pub gpu_layers: u32,
    pub block_count: u64,
    pub weight_mb_per_layer: u64,
    pub kv_mb_per_layer: u64,
    pub usable_vram_mb: u64,
    /// True when every layer (plus the output layer) fits on the GPU.
    pub fully_offloaded: bool,
}

/// Compute the number of layers to offload for `model_info` given measured
/// VRAM. Pure function — unit-testable without hardware.
pub fn plan_offload(
    model_info: &GgufModelInfo,
    ctx_size: u32,
    vram_mb: u64,
    vram_kind: VramKind,
) -> OffloadPlan {
    const MB: u64 = 1024 * 1024;

    // Safety reserve:
    //  - Free-VRAM sources: small buffer for compute graph + fragmentation.
    //  - Total-VRAM sources (registry/WMI): the display server is using an
    //    unknown share, so reserve max(768 MB, 10%) before planning.
    let reserve_mb = match vram_kind {
        VramKind::Free => 256,
        VramKind::Total => (vram_mb / 10).max(768),
    };
    let usable_vram_mb = vram_mb.saturating_sub(reserve_mb);

    let weight_per_layer = model_info.bytes_per_layer().max(1);
    let kv_per_layer = model_info.kv_bytes_per_layer(ctx_size as u64);
    let cost_per_layer = weight_per_layer + kv_per_layer;

    let fits = (usable_vram_mb * MB) / cost_per_layer;

    let (gpu_layers, fully_offloaded) = if fits >= model_info.block_count {
        // Everything fits: offload all blocks plus the output layer
        // (llama.cpp convention: n_layers + 1 covers the output layer).
        ((model_info.block_count + 1) as u32, true)
    } else {
        (fits as u32, false)
    };

    OffloadPlan {
        gpu_layers,
        block_count: model_info.block_count,
        weight_mb_per_layer: weight_per_layer / MB,
        kv_mb_per_layer: kv_per_layer / MB,
        usable_vram_mb,
        fully_offloaded,
    }
}

/// Full pipeline for a GGUF model on this machine: parse the header, measure
/// VRAM, plan the offload. Errors (non-GGUF file, unreadable header, no
/// measurable VRAM) propagate to the caller, which decides on the fallback —
/// they are never silently converted into a guess here.
pub fn compute_gpu_layers_for_model(model_path: &Path, ctx_size: u32) -> anyhow::Result<OffloadPlan> {
    let model_info = read_model_info(model_path)?;

    let (vram_mb, source) = crate::config::detect_available_vram_mb()
        .ok_or_else(|| anyhow::anyhow!(
            "GPU VRAM could not be measured (no NVML, nvidia-smi, or registry value)"
        ))?;

    let vram_kind = if source.ends_with("free") { VramKind::Free } else { VramKind::Total };
    let plan = plan_offload(&model_info, ctx_size, vram_mb, vram_kind);

    info!(
        "GPU offload plan for {:?} [{}]: {}/{} layers on GPU ({}) — \
         {} MB VRAM via {} ({} MB usable), {} MB weights + {} MB KV per layer at ctx {}",
        model_path.file_name().unwrap_or_default(),
        model_info.architecture,
        plan.gpu_layers,
        plan.block_count + 1,
        if plan.fully_offloaded { "fully offloaded" } else { "partial — remainder on CPU" },
        vram_mb,
        source,
        plan.usable_vram_mb,
        plan.weight_mb_per_layer,
        plan.kv_mb_per_layer,
        ctx_size,
    );

    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(block_count: u64, file_size_mb: u64, n_embd: u64, heads: u64, kv_heads: u64) -> GgufModelInfo {
        GgufModelInfo {
            architecture: "llama".into(),
            block_count,
            embedding_length: n_embd,
            head_count: heads,
            head_count_kv: kv_heads,
            file_size: file_size_mb * 1024 * 1024,
            trained_context_length: 0,
        }
    }

    #[test]
    fn small_model_fully_offloads_on_small_gpu() {
        // ~0.7 GB 1B-class model, 4 GB card measured as total VRAM
        let m = model(22, 700, 2048, 32, 4);
        let plan = plan_offload(&m, 8192, 4096, VramKind::Total);
        assert!(plan.fully_offloaded);
        assert_eq!(plan.gpu_layers, 23); // 22 blocks + output layer
    }

    #[test]
    fn big_model_partially_offloads() {
        // ~4.4 GB 7B-class Q4 model on the same 4 GB card: partial offload,
        // strictly more than the old bucket table's fixed 12/16.
        let m = model(32, 4400, 4096, 32, 8);
        let plan = plan_offload(&m, 8192, 4096, VramKind::Total);
        assert!(!plan.fully_offloaded);
        assert!(plan.gpu_layers > 0);
        assert!((plan.gpu_layers as u64) < m.block_count);
    }

    #[test]
    fn huge_model_gets_zero_layers_not_a_crash() {
        // 40 GB model (500 MB/layer) on an adapter whose usable VRAM after the
        // display reserve cannot hold even one layer.
        let m = model(80, 40_000, 8192, 64, 8);
        let plan = plan_offload(&m, 4096, 512, VramKind::Total);
        assert_eq!(plan.gpu_layers, 0);
        assert!(!plan.fully_offloaded);
    }

    #[test]
    fn bigger_context_reduces_offload() {
        let m = model(32, 4400, 4096, 32, 8);
        let small_ctx = plan_offload(&m, 2048, 8192, VramKind::Free);
        let big_ctx = plan_offload(&m, 32768, 8192, VramKind::Free);
        assert!(big_ctx.gpu_layers <= small_ctx.gpu_layers);
    }

    #[test]
    fn free_vram_reserves_less_than_total() {
        let m = model(32, 4400, 4096, 32, 8);
        let from_free = plan_offload(&m, 8192, 8192, VramKind::Free);
        let from_total = plan_offload(&m, 8192, 8192, VramKind::Total);
        assert!(from_free.usable_vram_mb > from_total.usable_vram_mb);
    }
}
