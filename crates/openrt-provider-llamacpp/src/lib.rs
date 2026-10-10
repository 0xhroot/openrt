//! llama.cpp `server` backend behind the Phase 1 `ModelProvider` trait.
//!
//! The provider is feature-gated (`llamacpp`) so the default build stays empty
//! and never drags in an HTTP/TLS stack unless explicitly opted in.

#[cfg(feature = "llamacpp")]
mod provider;

#[cfg(feature = "llamacpp")]
pub use provider::LlamacppProvider;
