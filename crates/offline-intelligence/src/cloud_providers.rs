//! Cloud model providers.
//!
//! Every provider here speaks the OpenAI chat-completions protocol (natively
//! or through its official compatibility endpoint), so a cloud answer takes
//! exactly the same path as a local one: retrieval, attachments, history and
//! persistence are unchanged; only the URL and the bearer key differ.
//!
//! Keys live in the encrypted api_keys store under the provider id.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct CloudProvider {
    pub id: &'static str,
    pub name: &'static str,
    /// Base URL; `/chat/completions` and `/models` are appended.
    pub base_url: &'static str,
}

pub const PROVIDERS: &[CloudProvider] = &[
    CloudProvider { id: "openai", name: "OpenAI", base_url: "https://api.openai.com/v1" },
    CloudProvider { id: "anthropic", name: "Anthropic (Claude)", base_url: "https://api.anthropic.com/v1" },
    CloudProvider { id: "gemini", name: "Google Gemini", base_url: "https://generativelanguage.googleapis.com/v1beta/openai" },
    CloudProvider { id: "deepseek", name: "DeepSeek", base_url: "https://api.deepseek.com/v1" },
    CloudProvider { id: "moonshot", name: "Moonshot (Kimi)", base_url: "https://api.moonshot.ai/v1" },
    CloudProvider { id: "perplexity", name: "Perplexity", base_url: "https://api.perplexity.ai" },
    CloudProvider { id: "mistral", name: "Mistral", base_url: "https://api.mistral.ai/v1" },
    CloudProvider { id: "xai", name: "xAI (Grok)", base_url: "https://api.x.ai/v1" },
    CloudProvider { id: "groq", name: "Groq", base_url: "https://api.groq.com/openai/v1" },
    CloudProvider { id: "nvidia", name: "NVIDIA NIM", base_url: "https://integrate.api.nvidia.com/v1" },
    CloudProvider { id: "together", name: "Together AI", base_url: "https://api.together.xyz/v1" },
    CloudProvider { id: "openrouter", name: "OpenRouter", base_url: "https://openrouter.ai/api/v1" },
];

pub fn find(id: &str) -> Option<&'static CloudProvider> {
    let id = id.to_lowercase();
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Headers beyond `Authorization: Bearer`, for providers that want them.
pub fn extra_headers(provider: &CloudProvider, key: &str) -> Vec<(&'static str, String)> {
    match provider.id {
        // The compatibility endpoint takes the bearer key; the native model
        // list wants these.
        "anthropic" => vec![("x-api-key", key.to_string()), ("anthropic-version", "2023-06-01".to_string())],
        "openrouter" => vec![("X-Title", "Counsel".to_string())],
        _ => vec![],
    }
}
