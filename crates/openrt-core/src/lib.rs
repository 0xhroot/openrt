//! OpenRT domain primitives and shared types.
//!
//! This crate is the dependency-inversion anchor of the workspace: it holds the
//! cross-cutting types (identifiers, session/task/status models, domain events,
//! error enums, cancellation) that every other crate exchanges. It must stay
//! free of async infrastructure and of any sibling-crate dependency so the
//! whole graph stays acyclic.

pub mod cancel;
pub mod digest;
pub mod error;
pub mod event;
pub mod execution;
pub mod id;
pub mod injection;
pub mod key;
pub mod message;
pub mod redaction;
pub mod session;
pub mod task;
pub mod timestamp;

pub use cancel::{CancellationFut, CancellationToken, PauseToken, PauseWait};
pub use digest::{canonical_pair_bytes, canonical_pair_digest, sha256_hex};
pub use error::CoreError;
pub use event::{DomainEvent, EventKind};
pub use execution::ExecutionRecord;
pub use id::*;
pub use injection::{InjectionCategory, InjectionReport, SecurityFlag, TrustLabel};
pub use key::{decode_safe_key, encode_safe_key};
pub use message::{Message, MessageRole, PendingToolCall};
pub use redaction::{default_sensitive_names, RedactionPolicy, REDACTED};
pub use session::{Session, SessionGenome, SessionStatus};
pub use task::{
    SegmentOutcome, SegmentRecord, SegmentTotals, Task, TaskError, TaskStatus, TurnKind, TurnRecord,
};
pub use timestamp::{now, Timestamp};

/// Version of this crate's public contract. Consumers should pin behavior to
/// this value so schema evolution stays explicit (ADR-006: versioned surface).
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
