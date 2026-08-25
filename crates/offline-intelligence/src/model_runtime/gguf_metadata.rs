//! Minimal GGUF header reader.
//!
//! Reads ONLY the metadata key-value section at the start of a .gguf file —
//! never the tensor data — to extract the fields needed for dynamic GPU-layer
//! offload calculation:
//!
//!   general.architecture              (e.g. "llama", "qwen2", "gemma3")
//!   {arch}.block_count                number of transformer layers
//!   {arch}.embedding_length           hidden dimension (n_embd)
//!   {arch}.attention.head_count       attention heads
//!   {arch}.attention.head_count_kv    KV heads (GQA; absent = same as head_count)
//!
//! GGUF layout (spec: ggml-org/ggml/docs/gguf.md):
//!   u32 magic "GGUF" | u32 version (2|3) | u64 tensor_count | u64 kv_count
//!   then kv_count pairs of: string key | u32 value_type | value
//!
//! Large values (tokenizer vocab arrays, megabytes of strings) are skipped
//! with seeks, so reading a 40 GB model file's metadata costs a few KB of IO.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const GGUF_MAGIC: u32 = 0x4655_4747; // "GGUF" little-endian

/// The subset of GGUF metadata needed to plan GPU offload.
#[derive(Debug, Clone)]
pub struct GgufModelInfo {
    pub architecture: String,
    /// Number of transformer layers (repeating blocks).
    pub block_count: u64,
    /// Hidden dimension (n_embd).
    pub embedding_length: u64,
    /// Attention head count (0 when the model does not declare it).
    pub head_count: u64,
    /// KV head count for GQA (defaults to head_count when not declared).
    pub head_count_kv: u64,
    /// Total .gguf file size in bytes.
    pub file_size: u64,
    /// The model's TRAINED maximum context length ({arch}.context_length),
    /// i.e. the ceiling beyond which the model was never trained to attend
    /// coherently. 0 when the model doesn't declare it (rare; ctx_size is
    /// then left to RAM/VRAM-based auto-detection alone).
    pub trained_context_length: u64,
}

impl GgufModelInfo {
    /// Estimated bytes of model weights per transformer layer.
    /// file_size / block_count slightly over-counts (embeddings + output layer
    /// are smeared across blocks) which errs on the safe side for VRAM planning.
    pub fn bytes_per_layer(&self) -> u64 {
        self.file_size / self.block_count.max(1)
    }

    /// Estimated KV-cache bytes per layer at the given context size (f16 K+V).
    /// With GQA the KV dimension is embedding_length * head_count_kv / head_count.
    pub fn kv_bytes_per_layer(&self, ctx_size: u64) -> u64 {
        let kv_dim = if self.head_count > 0 && self.head_count_kv > 0 {
            self.embedding_length * self.head_count_kv / self.head_count
        } else {
            // Head counts not declared — assume full attention (upper bound).
            self.embedding_length
        };
        // K + V, 2 bytes each (f16)
        ctx_size * kv_dim * 2 * 2
    }
}

/// Total on-disk size of the model, accounting for sharded multi-file models.
///
/// Large models ship split as "name-00001-of-00003.gguf" (llama.cpp
/// gguf-split convention; metadata lives in shard 1, which is the file the
/// user loads). Counting only the first shard would underestimate
/// bytes-per-layer and OFFLOAD TOO MANY layers — the failure mode that
/// overflows VRAM on exactly the biggest models. So shard siblings are
/// summed; if some shards are missing on disk, size is extrapolated from the
/// shard count (conservative beats optimistic here).
fn total_model_size(path: &Path, first_file_size: u64) -> u64 {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return first_file_size,
    };

    let re = regex::Regex::new(r"^(.+)-(\d{5})-of-(\d{5})\.gguf$").unwrap();
    let caps = match re.captures(name) {
        Some(c) => c,
        None => return first_file_size, // single-file model
    };

    let prefix = &caps[1];
    let total_shards: u32 = caps[3].parse().unwrap_or(1);
    let dir = match path.parent() {
        Some(d) => d,
        None => return first_file_size,
    };

    let mut sum = 0u64;
    let mut found = 0u32;
    for i in 1..=total_shards {
        let shard = dir.join(format!("{}-{:05}-of-{:05}.gguf", prefix, i, total_shards));
        if let Ok(md) = std::fs::metadata(&shard) {
            sum += md.len();
            found += 1;
        }
    }

    if found == total_shards {
        sum
    } else {
        // Shards missing on disk — extrapolate from the declared count so the
        // estimate stays on the conservative (larger) side.
        first_file_size * total_shards as u64
    }
}

/// Read the offload-relevant metadata from a .gguf file.
pub fn read_model_info(path: &Path) -> Result<GgufModelInfo> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Cannot open GGUF file {:?}", path))?;
    let file_size = total_model_size(path, file.metadata()?.len());
    let mut r = BufReader::new(file);

    let magic = read_u32(&mut r)?;
    if magic != GGUF_MAGIC {
        bail!("Not a GGUF file (magic 0x{magic:08x}) — {:?}", path);
    }
    let version = read_u32(&mut r)?;
    if !(2..=3).contains(&version) {
        bail!("Unsupported GGUF version {version} in {:?}", path);
    }
    let _tensor_count = read_u64(&mut r)?;
    let kv_count = read_u64(&mut r)?;

    // Suffix → value for every integer key we might care about. Collected for
    // all architectures seen, then resolved against general.architecture.
    const WANTED: &[&str] = &[
        ".block_count",
        ".embedding_length",
        ".attention.head_count",
        ".attention.head_count_kv",
        ".context_length",
    ];

    let mut architecture = String::new();
    let mut ints: HashMap<String, u64> = HashMap::new();

    for _ in 0..kv_count {
        let key = read_string(&mut r, 1024)?; // metadata keys are short
        let value_type = read_u32(&mut r)?;

        if key == "general.architecture" && value_type == 8 {
            architecture = read_string(&mut r, 256)?;
            continue;
        }

        if WANTED.iter().any(|s| key.ends_with(s)) {
            if let Some(v) = read_integer_value(&mut r, value_type)? {
                ints.insert(key, v);
                continue;
            }
            // Non-integer type under a wanted key (unexpected) — value already
            // consumed by read_integer_value's skip path.
            continue;
        }

        skip_value(&mut r, value_type)?;
    }

    if architecture.is_empty() {
        bail!("GGUF metadata has no general.architecture — {:?}", path);
    }

    let get = |suffix: &str| -> Option<u64> {
        ints.get(&format!("{architecture}{suffix}"))
            .or_else(|| {
                // Fall back to any architecture prefix carrying the suffix
                ints.iter().find(|(k, _)| k.ends_with(suffix)).map(|(_, v)| v)
            })
            .copied()
    };

    let block_count = get(".block_count")
        .ok_or_else(|| anyhow!("GGUF metadata has no {architecture}.block_count — {:?}", path))?;
    if block_count == 0 {
        bail!("GGUF declares block_count = 0 — {:?}", path);
    }
    let embedding_length = get(".embedding_length").unwrap_or(0);
    let head_count = get(".attention.head_count").unwrap_or(0);
    let head_count_kv = get(".attention.head_count_kv").unwrap_or(head_count);
    let trained_context_length = get(".context_length").unwrap_or(0);

    Ok(GgufModelInfo {
        architecture,
        block_count,
        embedding_length,
        head_count,
        head_count_kv,
        file_size,
        trained_context_length,
    })
}

// ── low-level readers ─────────────────────────────────────────────────────────

fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_string<R: Read>(r: &mut R, max_len: u64) -> Result<String> {
    let len = read_u64(r)?;
    if len > max_len {
        bail!("GGUF string length {len} exceeds sane limit {max_len}");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Fixed byte width of a scalar GGUF value type; None for string/array.
fn scalar_width(value_type: u32) -> Option<u64> {
    match value_type {
        0 | 1 | 7 => Some(1),      // u8, i8, bool
        2 | 3 => Some(2),          // u16, i16
        4 | 5 | 6 => Some(4),      // u32, i32, f32
        10 | 11 | 12 => Some(8),   // u64, i64, f64
        _ => None,                 // 8 string, 9 array
    }
}

/// Read a value expected to be an unsigned-integer-compatible scalar.
/// Returns None (after consuming the value) for non-integer types.
fn read_integer_value<R: Read + Seek>(r: &mut R, value_type: u32) -> Result<Option<u64>> {
    let v = match value_type {
        0 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b)?;
            Some(b[0] as u64)
        }
        2 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            Some(u16::from_le_bytes(b) as u64)
        }
        4 => Some(read_u32(r)? as u64),
        10 => Some(read_u64(r)?),
        // Signed types: accept non-negative values
        1 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b)?;
            let v = b[0] as i8;
            (v >= 0).then_some(v as u64)
        }
        3 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            let v = i16::from_le_bytes(b);
            (v >= 0).then_some(v as u64)
        }
        5 => {
            let v = read_u32(r)? as i32;
            (v >= 0).then_some(v as u64)
        }
        11 => {
            let v = read_u64(r)? as i64;
            (v >= 0).then_some(v as u64)
        }
        _ => {
            skip_value(r, value_type)?;
            None
        }
    };
    Ok(v)
}

/// Skip over a value of any GGUF type without materializing it.
fn skip_value<R: Read + Seek>(r: &mut R, value_type: u32) -> Result<()> {
    if let Some(w) = scalar_width(value_type) {
        r.seek(SeekFrom::Current(w as i64))?;
        return Ok(());
    }
    match value_type {
        8 => {
            // string: u64 length + bytes
            let len = read_u64(r)?;
            r.seek(SeekFrom::Current(len as i64))?;
            Ok(())
        }
        9 => {
            // array: u32 elem_type + u64 count + elements
            let elem_type = read_u32(r)?;
            let count = read_u64(r)?;
            if let Some(w) = scalar_width(elem_type) {
                r.seek(SeekFrom::Current((w * count) as i64))?;
            } else if elem_type == 8 {
                // array of strings (e.g. tokenizer vocab) — skip each
                for _ in 0..count {
                    let len = read_u64(r)?;
                    r.seek(SeekFrom::Current(len as i64))?;
                }
            } else {
                bail!("GGUF nested arrays are not supported (elem_type {elem_type})");
            }
            Ok(())
        }
        t => bail!("Unknown GGUF value type {t}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a synthetic GGUF header exercising strings, scalars and arrays.
    fn write_test_gguf(kvs: &[(&str, TestVal)]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let w = f.as_file_mut();
        w.write_all(&GGUF_MAGIC.to_le_bytes()).unwrap();
        w.write_all(&3u32.to_le_bytes()).unwrap(); // version
        w.write_all(&0u64.to_le_bytes()).unwrap(); // tensor_count
        w.write_all(&(kvs.len() as u64).to_le_bytes()).unwrap();

        for (key, val) in kvs {
            w.write_all(&(key.len() as u64).to_le_bytes()).unwrap();
            w.write_all(key.as_bytes()).unwrap();
            match val {
                TestVal::Str(s) => {
                    w.write_all(&8u32.to_le_bytes()).unwrap();
                    w.write_all(&(s.len() as u64).to_le_bytes()).unwrap();
                    w.write_all(s.as_bytes()).unwrap();
                }
                TestVal::U32(v) => {
                    w.write_all(&4u32.to_le_bytes()).unwrap();
                    w.write_all(&v.to_le_bytes()).unwrap();
                }
                TestVal::U64(v) => {
                    w.write_all(&10u32.to_le_bytes()).unwrap();
                    w.write_all(&v.to_le_bytes()).unwrap();
                }
                TestVal::F32(v) => {
                    w.write_all(&6u32.to_le_bytes()).unwrap();
                    w.write_all(&v.to_le_bytes()).unwrap();
                }
                TestVal::StrArray(items) => {
                    w.write_all(&9u32.to_le_bytes()).unwrap();
                    w.write_all(&8u32.to_le_bytes()).unwrap(); // elem type: string
                    w.write_all(&(items.len() as u64).to_le_bytes()).unwrap();
                    for s in items {
                        w.write_all(&(s.len() as u64).to_le_bytes()).unwrap();
                        w.write_all(s.as_bytes()).unwrap();
                    }
                }
            }
        }
        // Padding standing in for tensor data so file_size > header size
        w.write_all(&[0u8; 4096]).unwrap();
        w.flush().unwrap();
        f
    }

    enum TestVal {
        Str(&'static str),
        U32(u32),
        U64(u64),
        F32(f32),
        StrArray(Vec<&'static str>),
    }

    #[test]
    fn parses_llama_style_metadata() {
        let f = write_test_gguf(&[
            ("general.architecture", TestVal::Str("llama")),
            ("general.name", TestVal::Str("test-model")),
            // A big skippable array before the interesting keys
            ("tokenizer.ggml.tokens", TestVal::StrArray(vec!["a", "bb", "ccc"])),
            ("llama.block_count", TestVal::U32(32)),
            ("llama.embedding_length", TestVal::U32(4096)),
            ("llama.attention.head_count", TestVal::U32(32)),
            ("llama.attention.head_count_kv", TestVal::U32(8)),
            ("llama.rope.freq_base", TestVal::F32(10000.0)),
            ("llama.context_length", TestVal::U32(8192)),
        ]);

        let info = read_model_info(f.path()).unwrap();
        assert_eq!(info.architecture, "llama");
        assert_eq!(info.block_count, 32);
        assert_eq!(info.embedding_length, 4096);
        assert_eq!(info.head_count, 32);
        assert_eq!(info.head_count_kv, 8);
        assert_eq!(info.trained_context_length, 8192);
        assert!(info.file_size > 4096);

        // GQA: kv_dim = 4096 * 8/32 = 1024; per layer at ctx 8192 = 8192*1024*4
        assert_eq!(info.kv_bytes_per_layer(8192), 8192 * 1024 * 4);
    }

    #[test]
    fn missing_context_length_defaults_to_zero_unclamped() {
        let f = write_test_gguf(&[
            ("general.architecture", TestVal::Str("tinymodel")),
            ("tinymodel.block_count", TestVal::U32(4)),
        ]);
        let info = read_model_info(f.path()).unwrap();
        assert_eq!(
            info.trained_context_length, 0,
            "absent context_length must be 0 (caller's signal to skip clamping), never a guessed value"
        );
    }

    #[test]
    fn head_count_kv_defaults_to_head_count() {
        let f = write_test_gguf(&[
            ("general.architecture", TestVal::Str("qwen2")),
            ("qwen2.block_count", TestVal::U64(24)),
            ("qwen2.embedding_length", TestVal::U32(2048)),
            ("qwen2.attention.head_count", TestVal::U32(16)),
        ]);
        let info = read_model_info(f.path()).unwrap();
        assert_eq!(info.block_count, 24);
        assert_eq!(info.head_count_kv, 16);
    }

    #[test]
    fn sharded_model_size_sums_all_shards() {
        let dir = tempfile::tempdir().unwrap();

        // Shard 1 carries the real GGUF metadata (llama.cpp gguf-split layout)
        let src = write_test_gguf(&[
            ("general.architecture", TestVal::Str("llama")),
            ("llama.block_count", TestVal::U32(32)),
            ("llama.embedding_length", TestVal::U32(4096)),
            ("llama.attention.head_count", TestVal::U32(32)),
        ]);
        let shard1 = dir.path().join("model-00001-of-00002.gguf");
        std::fs::copy(src.path(), &shard1).unwrap();
        let shard1_size = std::fs::metadata(&shard1).unwrap().len();

        // Shard 2 is pure tensor data (no header needed for sizing)
        let shard2 = dir.path().join("model-00002-of-00002.gguf");
        std::fs::write(&shard2, vec![0u8; 10_000]).unwrap();

        let info = read_model_info(&shard1).unwrap();
        assert_eq!(info.file_size, shard1_size + 10_000);
        assert_eq!(info.block_count, 32);
    }

    #[test]
    fn missing_shards_extrapolate_conservatively() {
        let dir = tempfile::tempdir().unwrap();
        let src = write_test_gguf(&[
            ("general.architecture", TestVal::Str("llama")),
            ("llama.block_count", TestVal::U32(32)),
        ]);
        // Declares 4 shards but only shard 1 exists on disk
        let shard1 = dir.path().join("model-00001-of-00004.gguf");
        std::fs::copy(src.path(), &shard1).unwrap();
        let shard1_size = std::fs::metadata(&shard1).unwrap().len();

        let info = read_model_info(&shard1).unwrap();
        assert_eq!(info.file_size, shard1_size * 4);
    }

    #[test]
    fn rejects_non_gguf() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.as_file_mut().write_all(b"definitely not a gguf file").unwrap();
        assert!(read_model_info(f.path()).is_err());
    }

    #[test]
    fn missing_block_count_is_error() {
        let f = write_test_gguf(&[("general.architecture", TestVal::Str("llama"))]);
        assert!(read_model_info(f.path()).is_err());
    }
}
