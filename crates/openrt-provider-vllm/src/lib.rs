//! vLLM backend behind the Phase 1 `ModelProvider` trait.
//!
//! The provider is feature-gated (`vllm`) so the default build stays empty and
//! never drags in an HTTP/TLS stack unless explicitly opted in.

#[cfg(feature = "vllm")]
mod provider;

#[cfg(feature = "vllm")]
pub use provider::VllmProvider;
