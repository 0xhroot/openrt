//! Model provider abstraction, the mock provider, and the Ollama provider.
//!
//! The [`ModelProvider`] trait is the contract the runtime and agents use;
//! concrete providers implement it behind feature flags. `mock` always builds
//! (deterministic, zero-network). `ollama` is feature-gated so the default
//! build never hard-depends on the HTTP stack.

pub mod anatomy;
pub mod artifact;
pub mod error;
pub mod memory;
#[cfg(feature = "ollama")]
pub mod ollama;
#[cfg(feature = "openai-http")]
pub mod openai;
pub mod provider;
pub mod toolcall;
pub mod types;

pub use anatomy::{
    anatomy_for_file, anatomy_for_file_with_options, anatomy_from_reader,
    anatomy_from_reader_with_options, is_safetensors_index_file, sharded_anatomy_for_index_file,
    AnatomyFormat, AnatomyOptions, ModelAnatomy, ShardAnatomy, TensorAnatomy, TensorLengthBasis,
    ANATOMY_MAX_INDEX_BYTES, ANATOMY_MAX_SAFETENSORS_HEADER_BYTES, ANATOMY_MAX_SHARDS,
    ANATOMY_MAX_TENSORS, ANATOMY_PAGE_BYTES, ANATOMY_SCHEMA_VERSION, GGML_QUANT_TABLE_VERSION,
};
pub use artifact::{assess_execution_fit, ArtifactMetadata, ModelResourceProfile};
pub use error::ModelError;
pub use memory::{
    AnatomyAccess, BackendCapabilities, CapabilityState, HintPriority, HintReceipt, HintTier,
    MemoryHint, MemoryRange,
};
pub use provider::{ContextCapability, ModelCapabilities, ModelProvider, ProviderLoadTelemetry};
pub use types::*;

/// Factory for the mock provider used by tests and `--mock` runs.
pub mod mock;

pub use mock::MockProvider;
