//! Engine ↔ model compatibility.
//!
//! A GGUF file only loads if the installed llama.cpp build knows the names
//! written in its header: the model architecture (`general.architecture`), the
//! tokenizer type (`tokenizer.ggml.model`) and the pre-tokenizer
//! (`tokenizer.ggml.pre`). A model with a name the engine has never heard of
//! makes llama-server exit with code 1 the moment it is switched to — so the
//! catalogue hides such models instead.
//!
//! Both sides are read from real data, never from a hand-kept list that drifts
//! with every engine upgrade:
//!   - The engine's supported names are the string literals compiled into its
//!     llama library (llama.dll / libllama.so / libllama.dylib). llama.cpp
//!     matches these names by string comparison, so every supported name is
//!     present there as a NUL-terminated literal.
//!   - The model's names come from its GGUF header: a local file read for
//!     installed models, an HTTP range request for catalogue entries.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{debug, info, warn};

/// The header names that decide whether an engine can load a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GgufIdentity {
    pub architecture: String,
    #[serde(default)]
    pub tokenizer_model: Option<String>,
    /// `None` when the file has no `tokenizer.ggml.pre` (older conversions);
    /// llama.cpp then uses its default pre-tokenizer, which every build has.
    #[serde(default)]
    pub pre_tokenizer: Option<String>,
}

/// Names the installed engine build was compiled with.
pub struct EngineCapabilities {
    library: Vec<u8>,
    /// Answers already worked out. The catalogue holds hundreds of models but
    /// only a few dozen distinct names, and the model list is served on every
    /// page view, so each name is searched for once per engine, not per call.
    known: Mutex<HashMap<String, bool>>,
}

impl EngineCapabilities {
    /// Capabilities of the engine whose server binary is `binary_path`,
    /// cached per path. `None` if the llama library cannot be read, in which
    /// case callers must not filter anything — unknown is not incompatible.
    pub fn for_engine_binary(binary_path: &Path) -> Option<Arc<Self>> {
        static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<EngineCapabilities>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(caps) = cache.lock().ok()?.get(binary_path) {
            return Some(caps.clone());
        }

        let dir = binary_path.parent()?;
        let library = ["llama.dll", "libllama.so", "libllama.dylib"]
            .iter()
            .find_map(|name| std::fs::read(dir.join(name)).ok());
        let Some(library) = library else {
            warn!(
                "No llama library next to {} - model compatibility cannot be checked",
                binary_path.display()
            );
            return None;
        };
        info!("Loaded engine capabilities from {}", dir.display());
        let caps = Arc::new(Self { library, known: Mutex::new(HashMap::new()) });
        cache.lock().ok()?.insert(binary_path.to_path_buf(), caps.clone());
        Some(caps)
    }

    /// Whether `name` is compiled into the engine. Matched as a NUL-terminated
    /// suffix rather than a whole string: the linker merges a literal into the
    /// tail of a longer one ("qwen2" is stored inside "rwkv6qwen2").
    fn knows(&self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        if let Some(&hit) = self.known.lock().ok().as_ref().and_then(|m| m.get(name)) {
            return hit;
        }
        let mut needle = name.as_bytes().to_vec();
        needle.push(0);
        let hit = memchr::memmem::find(&self.library, &needle).is_some();
        if let Ok(mut m) = self.known.lock() {
            m.insert(name.to_string(), hit);
        }
        hit
    }

    pub fn supports(&self, identity: &GgufIdentity) -> bool {
        // "default" is how llama.cpp spells "no special pre-tokenizer"; the
        // comparison against it is inlined, so it is not in the library.
        let pre_ok = match identity.pre_tokenizer.as_deref() {
            None | Some("") | Some("default") => true,
            Some(p) => self.knows(p),
        };
        self.knows(&identity.architecture)
            && identity.tokenizer_model.as_deref().map_or(true, |t| self.knows(t))
            && pre_ok
    }
}

/// Outcome of parsing the start of a GGUF file.
enum Parse {
    Done(GgufIdentity),
    /// The buffer ended before every name was found; read more. Carries
    /// what was found so far, if the architecture was among it.
    NeedMore(Option<GgufIdentity>),
    /// Not a GGUF file, or a malformed header.
    Invalid,
}

const KEY_ARCH: &str = "general.architecture";
const KEY_TOKENIZER: &str = "tokenizer.ggml.model";
const KEY_PRE: &str = "tokenizer.ggml.pre";

/// Parse the GGUF header in `buf` (which may be just the start of the file)
/// far enough to find the architecture and tokenizer names.
fn parse_identity(buf: &[u8]) -> Parse {
    struct R<'a> {
        b: &'a [u8],
        p: usize,
    }
    impl R<'_> {
        fn take(&mut self, n: usize) -> Option<&[u8]> {
            let s = self.b.get(self.p..self.p.checked_add(n)?)?;
            self.p += n;
            Some(s)
        }
        fn u32(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn u64(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }
        fn string(&mut self) -> Option<&[u8]> {
            let n = self.u64()? as usize;
            self.take(n)
        }
    }
    // Byte width of fixed-size GGUF value types; None for string/array.
    fn fixed_size(t: u32) -> Option<usize> {
        match t {
            0 | 1 | 7 => Some(1),
            2 | 3 => Some(2),
            4 | 5 | 6 => Some(4),
            10 | 11 | 12 => Some(8),
            _ => None,
        }
    }
    // Skip one value; None = ran out of buffer, Some(false) = bad type.
    fn skip(r: &mut R, t: u32) -> Option<bool> {
        match t {
            8 => r.string().map(|_| true),
            9 => {
                let et = r.u32()?;
                let n = r.u64()? as usize;
                if let Some(w) = fixed_size(et) {
                    r.take(n.checked_mul(w)?).map(|_| true)
                } else if et == 8 {
                    for _ in 0..n {
                        r.string()?;
                    }
                    Some(true)
                } else {
                    Some(false)
                }
            }
            t => match fixed_size(t) {
                Some(w) => r.take(w).map(|_| true),
                None => Some(false),
            },
        }
    }

    let mut r = R { b: buf, p: 0 };
    let Some(magic) = r.take(4) else { return Parse::NeedMore(None) };
    if magic != b"GGUF" {
        return Parse::Invalid;
    }
    let Some(version) = r.u32() else { return Parse::NeedMore(None) };
    if version < 2 {
        return Parse::Invalid; // v1 used 32-bit lengths; nothing current ships it
    }
    let (Some(_tensors), Some(kv_count)) = (r.u64(), r.u64()) else {
        return Parse::NeedMore(None);
    };

    let mut arch: Option<String> = None;
    let mut tokenizer: Option<String> = None;
    let mut pre: Option<String> = None;
    let found = |arch: &Option<String>, tokenizer: &Option<String>, pre: &Option<String>| {
        arch.clone().map(|architecture| GgufIdentity {
            architecture,
            tokenizer_model: tokenizer.clone(),
            pre_tokenizer: pre.clone(),
        })
    };
    for _ in 0..kv_count {
        let Some(key) = r.string().map(|k| String::from_utf8_lossy(k).into_owned()) else {
            return Parse::NeedMore(found(&arch, &tokenizer, &pre));
        };
        let Some(t) = r.u32() else {
            return Parse::NeedMore(found(&arch, &tokenizer, &pre));
        };
        if t == 8 && (key == KEY_ARCH || key == KEY_TOKENIZER || key == KEY_PRE) {
            let Some(v) = r.string() else {
                return Parse::NeedMore(found(&arch, &tokenizer, &pre));
            };
            let v = Some(String::from_utf8_lossy(v).into_owned());
            match key.as_str() {
                KEY_ARCH => arch = v,
                KEY_TOKENIZER => tokenizer = v,
                _ => pre = v,
            }
        } else {
            match skip(&mut r, t) {
                None => return Parse::NeedMore(found(&arch, &tokenizer, &pre)),
                Some(false) => return Parse::Invalid,
                Some(true) => {}
            }
        }
        if arch.is_some() && tokenizer.is_some() && pre.is_some() {
            break;
        }
    }
    match found(&arch, &tokenizer, &pre) {
        Some(id) => Parse::Done(id),
        None => Parse::Invalid,
    }
}

/// Header prefix sizes tried in turn. The names usually sit near the top of
/// the header, before the large token arrays, so the first read nearly always
/// suffices. If even the last read ends first, what was found is used: an
/// unknown architecture or tokenizer already decides the answer.
const READ_SIZES: [usize; 2] = [256 * 1024, 4 * 1024 * 1024];

fn is_last_read(size: usize) -> bool {
    size == READ_SIZES[READ_SIZES.len() - 1]
}

/// Identity of a GGUF file on disk.
pub fn read_local_identity(path: &Path) -> Option<GgufIdentity> {
    use std::io::Read;
    for size in READ_SIZES {
        let mut buf = Vec::with_capacity(size);
        std::fs::File::open(path).ok()?.take(size as u64).read_to_end(&mut buf).ok()?;
        match parse_identity(&buf) {
            Parse::Done(id) => return Some(id),
            Parse::NeedMore(_) if buf.len() == size && !is_last_read(size) => continue,
            Parse::NeedMore(partial) => return partial,
            Parse::Invalid => return None,
        }
    }
    None
}

/// Identity of a GGUF file in a Hugging Face repository, read with HTTP range
/// requests so only the header is transferred. `None` on any network failure.
pub async fn fetch_remote_identity(
    client: &reqwest::Client,
    repo_id: &str,
    filename: &str,
) -> Option<GgufIdentity> {
    let url = format!("https://huggingface.co/{}/resolve/main/{}", repo_id, filename);
    for size in READ_SIZES {
        let resp = client
            .get(&url)
            .header("User-Agent", "Offline_Intelligence-Desktop/1.0")
            .header("Range", format!("bytes=0-{}", size - 1))
            .send()
            .await
            .and_then(|r| r.error_for_status());
        let bytes = match resp {
            Ok(r) => match r.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    debug!("Could not read GGUF header of {}/{}: {}", repo_id, filename, e);
                    return None;
                }
            },
            Err(e) => {
                debug!("Could not fetch GGUF header of {}/{}: {}", repo_id, filename, e);
                return None;
            }
        };
        match parse_identity(&bytes) {
            Parse::Done(id) => return Some(id),
            Parse::NeedMore(_) if bytes.len() >= size && !is_last_read(size) => continue,
            Parse::NeedMore(partial) => return partial,
            Parse::Invalid => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gguf(kvs: &[(&str, &str)], extra_array: bool) -> Vec<u8> {
        let mut b = b"GGUF".to_vec();
        b.extend(3u32.to_le_bytes());
        b.extend(0u64.to_le_bytes());
        b.extend(((kvs.len() + extra_array as usize) as u64).to_le_bytes());
        let s = |b: &mut Vec<u8>, v: &str| {
            b.extend((v.len() as u64).to_le_bytes());
            b.extend(v.as_bytes());
        };
        if extra_array {
            s(&mut b, "general.tags");
            b.extend(9u32.to_le_bytes());
            b.extend(8u32.to_le_bytes());
            b.extend(2u64.to_le_bytes());
            s(&mut b, "a");
            s(&mut b, "bc");
        }
        for (k, v) in kvs {
            s(&mut b, k);
            b.extend(8u32.to_le_bytes());
            s(&mut b, v);
        }
        b
    }

    #[test]
    fn parses_identity() {
        let b = gguf(&[(KEY_ARCH, "llama"), (KEY_TOKENIZER, "gpt2"), (KEY_PRE, "minicpm5")], true);
        match parse_identity(&b) {
            Parse::Done(id) => {
                assert_eq!(id.architecture, "llama");
                assert_eq!(id.tokenizer_model.as_deref(), Some("gpt2"));
                assert_eq!(id.pre_tokenizer.as_deref(), Some("minicpm5"));
            }
            _ => panic!("expected identity"),
        }
        assert!(matches!(parse_identity(&b[..b.len() - 3]), Parse::NeedMore(Some(_))));
        assert!(matches!(parse_identity(b"NOPE...."), Parse::Invalid));
    }

    #[test]
    fn missing_pre_tokenizer_is_default() {
        match parse_identity(&gguf(&[(KEY_ARCH, "llama")], false)) {
            Parse::Done(id) => assert_eq!(id.pre_tokenizer, None),
            _ => panic!("expected identity"),
        }
    }

    #[test]
    fn engine_names_match_merged_literals() {
        let caps = EngineCapabilities {
            library: b"\0llama\0rwkv6qwen2\0llama-bpe\0gpt2\0".to_vec(),
            known: Mutex::new(HashMap::new()),
        };
        let id = |a: &str, p: Option<&str>| GgufIdentity {
            architecture: a.into(),
            tokenizer_model: Some("gpt2".into()),
            pre_tokenizer: p.map(Into::into),
        };
        assert!(caps.supports(&id("qwen2", None)));
        assert!(caps.supports(&id("llama", Some("llama-bpe"))));
        assert!(caps.supports(&id("llama", Some("default"))));
        assert!(!caps.supports(&id("llama", Some("minicpm5"))));
        assert!(!caps.supports(&id("gemma4", None)));
        assert!(!caps.supports(&GgufIdentity { tokenizer_model: Some("gemma4".into()), ..id("llama", None) }));
    }
}
