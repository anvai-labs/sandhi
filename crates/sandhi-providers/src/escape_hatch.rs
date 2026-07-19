//! Host-language / custom-provider escape hatch (AnvaiOps ADR-0047 D10).
//!
//! [`FnProvider`] is a [`Provider`] backed by a user-supplied async function, so a consumer can
//! register a **custom / air-gapped / community** provider without adding a Rust adapter. This is
//! the foundation the Python/TS bindings expose as a host-language callback (victor's custom
//! providers keep working without a Rust contribution).

use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;

use crate::{ByteStream, Provider, ProviderError, ProviderRequest, ProviderResponse};

type CompleteFut = Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send>>;

/// A provider whose `complete` is a user-supplied async closure.
pub struct FnProvider {
    slug: String,
    complete: Box<dyn Fn(ProviderRequest) -> CompleteFut + Send + Sync>,
}

impl FnProvider {
    /// Build a custom provider from a `slug` and an async function `req → Result<response>`.
    pub fn new<F, Fut>(slug: impl Into<String>, f: F) -> Self
    where
        F: Fn(ProviderRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'static,
    {
        Self {
            slug: slug.into(),
            complete: Box::new(move |req| Box::pin(f(req))),
        }
    }
}

#[async_trait]
impl Provider for FnProvider {
    fn slug(&self) -> &str {
        &self.slug
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        (self.complete)(req).await
    }

    async fn stream(&self, _req: ProviderRequest) -> Result<ByteStream, ProviderError> {
        // Custom providers implement the non-streaming path; a streaming closure variant can be
        // added later. Callers should route custom providers through `complete`.
        Err(ProviderError::Upstream(501))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParsedUsage;
    use serde_json::json;

    #[tokio::test]
    async fn dispatches_to_the_user_closure() {
        let p = FnProvider::new("custom", |req| async move {
            // Echo the request model back; report fixed usage.
            Ok(ProviderResponse {
                status: 200,
                body: json!({ "model": req.model }),
                usage: ParsedUsage {
                    tokens_in: 3,
                    tokens_out: 4,
                    ..Default::default()
                },
            })
        });

        assert_eq!(p.slug(), "custom");
        let out = p
            .complete(ProviderRequest::new("my-model", json!({})))
            .await
            .unwrap();
        assert_eq!(out.status, 200);
        assert_eq!(out.usage.tokens_in, 3);
        assert_eq!(out.body["model"], "my-model");
    }
}
