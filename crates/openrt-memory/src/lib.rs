//! `openrt-memory` — Phase-5 **M5.3 memory layer** (working / episodic /
//! semantic(/workflow)).
//!
//! # Honest contract (M5.3; roadmap M5.3 row + DoD line ~749)
//!
//! > *Memory layers answer recall queries **with citations to their origin
//! > runs/sources**.*
//!
//! * **Working memory** — zero-persistence scratch slots bound to the *live
//!   session* (like registers). Fail-closed: a slot is named and quoted
//!   byte-exactly as the caller set it; nothing is invented.
//! * **Episodic memory** — persisted record of a *run* (execution/session
//!   slice), replayed deterministically, digest-verified byte-identical on
//!   every reopen (same canonical/sha256 discipline the workspace + history
//!   crates already gate). Recall gives citations to the exact origin run.
//! * **Semantic(/workflow) memory** — facts/knowledge accumulated and
//!   retrievable. **Semantic retrieval rides Phase 4 retrieval**:
//!   deterministic (`openrt-knowledge::retrieval`) **+ model-provider
//!   oracle**; every recalled item re-validates its `Citation` fail-closed
//!   against the persisted store before it may be presented. It NEVER invents
//!   an answer or a citation; when no evidence passes, it answers explicitly
//!   "no verified evidence" with an empty citation report.
//!
//! # Integrity / fail-closed (Phase-4 discipline carried forward, NOT weakened)
//!
//! * **One store** (ADR-006): this layer persists through the same
//!   `PersistenceStore` as everything else — it does NOT open a second
//!   vector/citation storecarsoku.
//! * Deterministic canonical digests (`sha256` over sorted-canonical bytes;
//!   `digest` field EXCLUDED from its own input, so digests never
//!   self-reference — the exact M5.1/M5.2 rule).
//! * `recall` re-verifies EVERY returned memory record against its persisted
//!   canonical bytes; a tampered / missing / re-keyed record fails closed
//!   (never silently repaired, never returned as verified).
//! * **Citations**: every recalled item must carry at least one `Citation`
//!   whose `validate` re-resolves against the persisted store; an item whose
//!   citations re-validate fail-closed can be presented — anything else
//!   cannot. No fabricated citations, ever.
//! * Workspace/session isolation: memory is keyed under the workspace
//!   `id` (path-safe) + a `session` id. One workspace's memory is NEVER
//!   mixed into another's recall surface.
//!
//! # Explicitly NOT claimed (roadmap-line 728 + DoD honesty; K6 audit)
//!
//! * Semantic/neural embeddings, real vector similarity search, FAISS/HNSW,
//!   OCR/VLM ingest, SFT/LoRA — all **deferred**; the roadmap records them as
//!   *what must not ship yet* (Phase-4 audit classifications). This crate does
//!   NOT claim them. Semantic memory here = deterministic Phase-4 retrieval +
//!   model-provider oracle + verified citations, nothing more/less.
//!
//! # Crate surface
//!
//! * [`working::WorkingMemory`] — `set` / `get` / `has` / `end_session` for
//!   the live session (zero-persistence; never written to the durable store).
//! * [`episodic::EpisodicMemory`] — persist + replay a *run*
//!   (execution/session), byte-identical reopen digest, recall with citation
//!   to its origin run record.
//! * [`semantic::SemanticMemory`] — accept a recalled fact-pack, persist it
//!   deterministically, and answer recall queries **with citations to origin
//!   sources/runs**, re-validated fail-closed through Phase-4 retrieval.
//! * [`recall`] — the one fail-closed recall oracle over all three layers.

pub mod canonical;
pub mod digest;
pub mod episodic;
pub mod prioritize;
pub mod recall;
pub mod semantic;
pub mod store;
pub mod working;

pub use episodic::{EpisodicError, EpisodicMemory, EpisodicRecord, RUNS_NS};
pub use prioritize::{
    prioritize, prioritize_result, PrioritizedHit, PrioritizedResult, PriorityParams,
};
pub use recall::{recall, MemoryRecall, MemoryRecallKind};
pub use semantic::{Citation, RecallSet, SemanticError, SemanticItem, SemanticMemory};
pub use store::{
    load_verified_episodes, load_verified_semantic, recall_episode, recall_fact, remember_episode,
    remember_fact, remember_origin, MemoryStoreError, EPISODIC_NS, MEMORY_KB, ORIGINS_NS,
    SEMANTIC_NS,
};
pub use working::{WorkingError, WorkingMemory, WorkingSlot};
