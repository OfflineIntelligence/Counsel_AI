// Server/src/api/mod.rs
//! API module - External interfaces for the memory system

pub mod memory_api;
pub mod admin_api;
pub mod conversation_api;
pub mod settings_api;
pub mod stream_api;
pub mod model_api;
pub mod files_api;
pub mod feedback_api;
pub mod login_notification_api;
pub mod api_keys_api;
pub mod auth_api;
pub mod documents_api;
pub mod drafts_api;

// Re-export API handlers
pub use memory_api::{memory_optimize, memory_stats, memory_cleanup};
pub use conversation_api::{get_conversations, get_conversation, update_conversation_title, delete_conversation, update_conversation_pinned};
pub use stream_api::generate_stream;
pub use model_api::{list_models, get_active_model, search_models, refresh_models, install_model, get_download_progress as get_model_download_progress, remove_model, get_hardware_info as get_model_hardware_info};