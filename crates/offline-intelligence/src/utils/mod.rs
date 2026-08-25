//! Utilities module - Common utility functions for text processing and topic extraction

pub mod file_processor;
pub mod pdf_text;
pub mod doc_context;
pub mod extraction_coordinator;
pub mod extraction_scheduler;
#[cfg(target_os = "windows")]
pub mod win_ocr;
pub mod image_ocr;
pub mod vision_extraction;
pub mod text_utils;
pub mod storage_governor;
pub mod thesaurus;
pub mod topic_extractor;

// Re-export commonly used utilities
pub use file_processor::{
    extract_content_from_bytes, extract_file_content, extraction_outcome,
    is_supported_attachment, supported_attachment_list, SUPPORTED_ATTACHMENT_EXTENSIONS,
};
pub use extraction_coordinator::{ExtractionCoordinator, ExtractionPermit};
pub use extraction_scheduler::{ExtractionScheduler, Lane, LanePermit};
pub use text_utils::TextUtils;
pub use topic_extractor::TopicExtractor;
