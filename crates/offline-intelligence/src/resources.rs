// Server/src/resources.rs

pub const HAS_EMBEDDED_RESOURCES: bool = false;

use std::fs;
use std::path::Path;
use anyhow::Context;

pub struct ResourceManager;

impl ResourceManager {
    pub fn extract_all(target_dir: &Path) -> anyhow::Result<()> {
        println!("ℹ️ Using external resources (not embedded)");
        // Just create the directory structure
        fs::create_dir_all(target_dir)?;
        Ok(())
    }

    pub fn ensure_llama_binary(&self) -> anyhow::Result<String> {
        // Use LLAMA_BIN directly from environment variable
        let llama_bin = std::env::var("LLAMA_BIN")
            .context("LLAMA_BIN environment variable not set. Please set it in your .env file")?;
        
        // Verify the binary exists
        if std::path::Path::new(&llama_bin).exists() {
            Ok(llama_bin)
        } else {
            Err(anyhow::anyhow!(
                "Llama binary not found at: {}. Please check LLAMA_BIN in .env file.",
                llama_bin
            ))
        }
    }
}