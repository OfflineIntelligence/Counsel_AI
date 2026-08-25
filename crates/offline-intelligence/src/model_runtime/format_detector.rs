//! Format Detector
//!
//! Automatically detects model format from file extension

use super::runtime_trait::ModelFormat;
use std::path::Path;
use tracing::info;

pub struct FormatDetector;

impl FormatDetector {
    /// Detect model format from file extension
    pub fn detect_from_path(path: &Path) -> Option<ModelFormat> {
        let extension = path.extension()?.to_str()?.to_lowercase();
        
        let format = if ModelFormat::GGUF.extensions().contains(&extension.as_str()) {
            Some(ModelFormat::GGUF)
        } else if ModelFormat::GGML.extensions().contains(&extension.as_str()) {
            // Need to disambiguate .bin files (could be GGML or other)
            if extension == "ggml" {
                Some(ModelFormat::GGML)
            } else if extension == "bin" {
                // Check filename for hints
                if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                    if filename.contains("ggml") {
                        Some(ModelFormat::GGML)
                    } else {
                        None // Ambiguous .bin file
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else if ModelFormat::ONNX.extensions().contains(&extension.as_str()) {
            Some(ModelFormat::ONNX)
        } else if ModelFormat::TensorRT.extensions().contains(&extension.as_str()) {
            Some(ModelFormat::TensorRT)
        } else if ModelFormat::Safetensors.extensions().contains(&extension.as_str()) {
            Some(ModelFormat::Safetensors)
        } else if ModelFormat::CoreML.extensions().contains(&extension.as_str()) {
            Some(ModelFormat::CoreML)
        } else {
            None
        };

        if let Some(fmt) = format {
            info!("Detected model format: {} for file: {}", fmt.name(), path.display());
        }

        format
    }

    /// List all supported extensions
    pub fn supported_extensions() -> Vec<String> {
        let mut exts = Vec::new();
        for format in &[
            ModelFormat::GGUF,
            ModelFormat::GGML,
            ModelFormat::ONNX,
            ModelFormat::TensorRT,
            ModelFormat::Safetensors,
            ModelFormat::CoreML,
        ] {
            for ext in format.extensions() {
                exts.push(ext.to_string());
            }
        }
        exts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_gguf_detection() {
        let path = PathBuf::from("model.gguf");
        assert_eq!(FormatDetector::detect_from_path(&path), Some(ModelFormat::GGUF));
    }

    #[test]
    fn test_onnx_detection() {
        let path = PathBuf::from("model.onnx");
        assert_eq!(FormatDetector::detect_from_path(&path), Some(ModelFormat::ONNX));
    }

    #[test]
    fn test_tensorrt_detection() {
        let path = PathBuf::from("model.trt");
        assert_eq!(FormatDetector::detect_from_path(&path), Some(ModelFormat::TensorRT));
    }

    #[test]
    fn test_safetensors_detection() {
        let path = PathBuf::from("model.safetensors");
        assert_eq!(FormatDetector::detect_from_path(&path), Some(ModelFormat::Safetensors));
    }
}
