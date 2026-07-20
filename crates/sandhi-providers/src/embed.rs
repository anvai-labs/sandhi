//! Embedding transport — the vector-modality sibling of the chat [`crate::Provider`] trait.
//!
//! Chat and embeddings are different provider APIs (`/chat/completions` vs `/embeddings`), so
//! embeddings get their own [`EmbeddingProvider`] contract rather than overloading `Provider`.
//! The OpenAI-compatible and Cohere adapters implement it in addition to `Provider`, reusing the
//! same client/base-url/key. Returns vectors plus the provider's **real** token usage measured at
//! the source — neutral units only, no dollars (AnvaiOps ADR-0047 D3).
//!
//! In-process consumers (e.g. ProximaDB's embedding drainer, ADR-067) link this directly and call
//! [`EmbeddingProvider::embed`] with no HTTP hop of their own.

use crate::ProviderError;
use async_trait::async_trait;

/// An embedding request: the model and the input texts to embed.
#[derive(Debug, Clone)]
pub struct EmbedRequest {
    pub model: String,
    pub input: Vec<String>,
    /// Cohere requires an `input_type` (e.g. `search_document` / `search_query`); OpenAI ignores
    /// it. Defaults to `search_document` for Cohere when unset.
    pub input_type: Option<String>,
}

impl EmbedRequest {
    pub fn new(model: impl Into<String>, input: Vec<String>) -> Self {
        Self {
            model: model.into(),
            input,
            input_type: None,
        }
    }

    #[must_use]
    pub fn with_input_type(mut self, input_type: impl Into<String>) -> Self {
        self.input_type = Some(input_type.into());
        self
    }
}

/// Real token usage from an embedding response, measured at the source. Neutral units only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmbedUsage {
    /// Input (prompt) tokens the provider billed for the batch.
    pub input_tokens: u64,
    /// Total tokens (for embeddings, typically equal to `input_tokens`).
    pub total_tokens: u64,
}

/// A completed embedding response: one vector per input text, plus measured usage when the
/// provider reports it.
#[derive(Debug, Clone)]
pub struct EmbedResponse {
    pub status: u16,
    pub embeddings: Vec<Vec<f32>>,
    pub usage: Option<EmbedUsage>,
}

/// The embedding adapter contract — the vector-modality sibling of [`crate::Provider`].
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Neutral provider slug (e.g. `openai`, `cohere`).
    fn slug(&self) -> &str;

    /// Embed a batch of texts, returning one vector per input plus measured token usage.
    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError>;
}

/// Parse an OpenAI-shaped `data: [{embedding: [...]}, ...]` array into vectors.
pub(crate) fn parse_openai_embeddings(body: &serde_json::Value) -> Vec<Vec<f32>> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("embedding").and_then(|e| e.as_array()))
                .map(|nums| {
                    nums.iter()
                        .filter_map(|n| n.as_f64().map(|f| f as f32))
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default()
}
