//! Encryption of secrets at rest (API keys, the JWT signing key).
//!
//! AES-256-GCM with a random 96-bit nonce per value; the tag authenticates
//! every ciphertext, so a tampered value fails to decrypt instead of decoding
//! to garbage. Stored form: `v2:` + base64(nonce || ciphertext || tag).
//!
//! The 256-bit master key is random, generated once per install, and kept in
//! `<AppData>/secrets/master.key`. On Windows that file holds the key wrapped
//! by DPAPI (`CryptProtectData`, current-user scope), so copying the file to
//! another account or machine does not reveal it. Elsewhere the raw key is
//! written with owner-only permissions.
//!
//! Values written by the old XOR scheme (no `v2:` prefix) still decrypt
//! through [`decrypt_legacy`] so they can be re-encrypted once at startup.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use rand::RngCore;
use std::path::PathBuf;
use std::sync::OnceLock;

const PREFIX: &str = "v2:";
const NONCE_LEN: usize = 12;

static MASTER_KEY: OnceLock<[u8; 32]> = OnceLock::new();

#[cfg(not(test))]
fn key_path() -> PathBuf {
    crate::config::get_app_data_dir().join("secrets").join("master.key")
}

#[cfg(test)]
fn key_path() -> PathBuf {
    std::env::temp_dir().join("oca-secret-box-test").join("master.key")
}

fn master_key() -> Result<&'static [u8; 32]> {
    if let Some(k) = MASTER_KEY.get() {
        return Ok(k);
    }
    let key = load_or_create_key()?;
    Ok(MASTER_KEY.get_or_init(|| key))
}

fn load_or_create_key() -> Result<[u8; 32]> {
    let path = key_path();
    if let Ok(stored) = std::fs::read(&path) {
        let raw = unprotect(&stored).context("master key could not be unwrapped")?;
        return raw
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("master key has the wrong length"));
    }
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, protect(&key)?)?;
    restrict_permissions(&tmp);
    std::fs::rename(&tmp, &path)?;
    Ok(key)
}

pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(PREFIX)
}

pub fn encrypt(plaintext: &str) -> Result<String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key()?));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| anyhow!("encryption failed"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(format!("{}{}", PREFIX, BASE64.encode(out)))
}

pub fn decrypt(stored: &str) -> Result<String> {
    let body = stored
        .strip_prefix(PREFIX)
        .ok_or_else(|| anyhow!("value is not in the encrypted format"))?;
    let bytes = BASE64.decode(body)?;
    if bytes.len() <= NONCE_LEN {
        return Err(anyhow!("encrypted value is truncated"));
    }
    let (nonce, ct) = bytes.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key()?));
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| anyhow!("decryption failed: wrong key or tampered value"))?;
    Ok(String::from_utf8(pt)?)
}

/// The pre-v2 scheme (XOR with the device name). Read-only: used to upgrade
/// old rows, never to write.
pub fn decrypt_legacy(stored: &str) -> Result<String> {
    let mut key: Vec<u8> = whoami::devicename().into_bytes();
    while key.len() < 32 {
        key.push((key.len() as u8).wrapping_mul(17));
    }
    key.truncate(32);
    let bytes = BASE64.decode(stored)?;
    let plain: Vec<u8> = bytes.iter().enumerate().map(|(i, b)| b ^ key[i % 32]).collect();
    Ok(String::from_utf8(plain)?)
}

#[cfg(windows)]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, true)
}

#[cfg(windows)]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, false)
}

#[cfg(windows)]
fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    const ENTROPY: &[u8] = b"OfflineCounselAI.secret_box.v2";
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if protect {
            CryptProtectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)?;
        } else {
            CryptUnprotectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)?;
        }
        let out = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as _));
        Ok(out)
    }
}

#[cfg(not(windows))]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(not(windows))]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper_detection() {
        let enc = encrypt("hf_1234567890abcdef").unwrap();
        assert!(is_encrypted(&enc));
        assert_eq!(decrypt(&enc).unwrap(), "hf_1234567890abcdef");
        // Same plaintext, different nonce.
        assert_ne!(enc, encrypt("hf_1234567890abcdef").unwrap());
        let mut bytes = BASE64.decode(&enc[3..]).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(decrypt(&format!("v2:{}", BASE64.encode(bytes))).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip() {
        let wrapped = protect(b"0123456789abcdef0123456789abcdef").unwrap();
        assert_ne!(&wrapped[..], b"0123456789abcdef0123456789abcdef");
        assert_eq!(unprotect(&wrapped).unwrap(), b"0123456789abcdef0123456789abcdef");
    }
}
