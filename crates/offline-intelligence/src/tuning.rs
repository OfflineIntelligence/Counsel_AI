//! Performance tuning flags read from the environment (.env).
//!
//! Every value is optional: unset, empty, "auto" or "default" means the
//! built-in default (usually the engine's own). They are read when used, after
//! Config::from_env has loaded .env into the process environment.

use std::env;

/// A trimmed value, or None for unset / empty / "auto" / "default".
pub fn var(name: &str) -> Option<String> {
    let v = env::var(name).ok()?;
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("auto") || v.eq_ignore_ascii_case("default") {
        None
    } else {
        Some(v.to_string())
    }
}

pub fn parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    var(name).and_then(|v| v.parse().ok())
}

pub fn parse_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    parse(name).unwrap_or(default)
}

/// true/false, yes/no, on/off, 1/0.
pub fn flag(name: &str) -> Option<bool> {
    match var(name)?.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub fn flag_or(name: &str, default: bool) -> bool {
    flag(name).unwrap_or(default)
}

fn push_value(args: &mut Vec<String>, env_name: &str, cli: &str) {
    if let Some(v) = var(env_name) {
        args.push(cli.to_string());
        args.push(v);
    }
}

/// Push `on` when the flag is true and `off` when false; nothing when unset.
fn push_switch(args: &mut Vec<String>, env_name: &str, on: &str, off: &str) {
    match flag(env_name) {
        Some(true) if !on.is_empty() => args.push(on.to_string()),
        Some(false) if !off.is_empty() => args.push(off.to_string()),
        _ => {}
    }
}

/// llama-server flags shared by the chat and embedding servers.
fn common_engine_args(prefix: &str) -> Vec<String> {
    let mut a = Vec::new();
    let k = |s: &str| format!("{}{}", prefix, s);
    // llama.cpp's own memory fitting probes every GPU at startup (measured
    // at 20 s on a laptop with two GPUs) and then gives up when the layer
    // count is set explicitly, which the app always does
    // (model_runtime::gpu_offload). Off unless asked for.
    a.push("--fit".into());
    a.push(if flag_or(&k("FIT"), false) { "on".into() } else { "off".into() });
    // Skip the empty warmup decode at startup: a faster wake from sleep.
    if !flag_or(&k("WARMUP"), false) {
        a.push("--no-warmup".into());
    }
    // The built-in browser UI is never used by the app.
    if !flag_or(&k("WEBUI"), false) {
        a.push("--no-webui".into());
    }
    push_switch(&mut a, &k("MLOCK"), "--mlock", "");
    push_switch(&mut a, &k("MMAP"), "", "--no-mmap");
    push_switch(&mut a, &k("DIRECT_IO"), "--direct-io", "--no-direct-io");
    push_value(&mut a, &k("PRIO"), "--prio");
    push_value(&mut a, &k("PRIO_BATCH"), "--prio-batch");
    push_value(&mut a, &k("POLL"), "--poll");
    push_value(&mut a, &k("POLL_BATCH"), "--poll-batch");
    push_value(&mut a, &k("CPU_STRICT"), "--cpu-strict");
    push_value(&mut a, &k("NUMA"), "--numa");
    push_value(&mut a, &k("SPLIT_MODE"), "--split-mode");
    push_value(&mut a, &k("MAIN_GPU"), "--main-gpu");
    push_switch(&mut a, &k("KV_OFFLOAD"), "", "--no-kv-offload");
    push_switch(&mut a, &k("OP_OFFLOAD"), "", "--no-op-offload");
    push_switch(&mut a, &k("REPACK"), "", "--no-repack");
    push_value(&mut a, &k("THREADS_HTTP"), "--threads-http");
    a
}

/// Extra llama-server arguments for the chat model, from LLAMA_* flags.
pub fn chat_engine_args() -> Vec<String> {
    let mut a = common_engine_args("LLAMA_");
    push_value(&mut a, "LLAMA_FLASH_ATTN", "--flash-attn");
    push_value(&mut a, "LLAMA_UBATCH_SIZE", "--ubatch-size");
    push_switch(&mut a, "LLAMA_CONT_BATCHING", "--cont-batching", "--no-cont-batching");
    push_switch(&mut a, "LLAMA_CONTEXT_SHIFT", "--context-shift", "--no-context-shift");
    push_switch(&mut a, "LLAMA_SWA_FULL", "--swa-full", "");
    // Large Mixture-of-Experts models: keep expert weights on the CPU so the
    // rest of the model fits in VRAM.
    push_switch(&mut a, "LLAMA_CPU_MOE", "--cpu-moe", "");
    push_value(&mut a, "LLAMA_N_CPU_MOE", "--n-cpu-moe");
    // Draft-free speculative decoding (n-gram lookup): faster when answers
    // repeat document text, e.g. quoting.
    push_value(&mut a, "LLAMA_SPEC_TYPE", "--spec-type");
    push_value(&mut a, "LLAMA_DRAFT_MAX", "--draft-max");
    a
}

/// Extra llama-server arguments for the embedding model, from EMBED_* flags.
pub fn embed_engine_args() -> Vec<String> {
    let mut a = common_engine_args("EMBED_");
    push_value(&mut a, "EMBED_THREADS", "--threads");
    push_value(&mut a, "EMBED_FLASH_ATTN", "--flash-attn");
    a
}

/// Chat engine slots (LLAMA_PARALLEL, default 1). One slot halves the KV
/// cache and enables warm start; two let chat and Aurora answer at once.
pub fn chat_parallel_slots() -> u32 {
    parse::<u32>("LLAMA_PARALLEL").filter(|n| *n >= 1).unwrap_or(crate::model_runtime::DEFAULT_PARALLEL_SLOTS)
}

/// Whether the frontend's prewarm requests may load the model (MODEL_PREWARM).
pub fn prewarm_enabled() -> bool {
    flag_or("MODEL_PREWARM", true)
}
