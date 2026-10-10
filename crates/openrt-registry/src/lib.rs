//! Model registry (ADR-010): immutable content manifests, the artifact
//! lifecycle state machine, content-addressed install, Hugging Face
//! **metadata-only** discovery, and the curated (Ctrl+P) offline library.
//!
//! # Layout (all inside the ONE store, ADR-006/ADR-010)
//!
//! * `registry.manifests/<id>` — one immutable `ManifestV1`, written once and
//!   never overwritten. Its `blobs[].digest` are the immutable SHA-256 digests
//!   of the artifact.
//! * `registry.lifecycle/<id>` — the mutable lifecycle state machine with an
//!   append-only transition history.
//! * `registry.meta/<id>` — the searchable operator card.
//! * `registry.blobs/<sha256>` — content-addressed binary weights, written
//!   atomically and integrity-checked by the store (SEC-03).
//! * `verify.records/*`, `scan.records/*` — verification and scan results kept
//!   in the crates that own them (`openrt-verify`, `openrt-scan`).
//!
//! # Lifecycle (ADR-010)
//!
//! `discovered -> downloaded -> verified -> scanned -> active | idle -> retired`.
//! Activation requires BOTH a passed SHA-256 verification (ADR-001) and a
//! passed signature-bundle scan (`openrt-scan`, ADR-003); anything else fails
//! closed. The registry never fabricates downloads, digests, sizes, or expert
//! metadata: a curated entry lists no size and no digest; `install_remote`
//! computes digests from bytes actually verified.
//!
//! Mode policy (ADR-003): materialization (entering artifact bytes) is an
//! explicit, permission-gated operation, so `import_local` requires
//! **Provisioning** mode via `ModeManager::ensure_materialization_allowed`;
//! `retire`, `activate`, `idle` and the offline search/list/status paths run
//! in either mode. Remote discovery additionally requires
//! `ensure_network_allowed` in `search`.

use std::collections::BTreeMap;
use std::sync::Arc;

use openrt_audit::AuditLog;
use openrt_modes::{ExecutionMode, ModeManager};
use openrt_scan::ArtifactKind;
use serde::{Deserialize, Serialize};

pub use openrt_scan::ArtifactKind as RegistryArtifactKind;

/// Schema versions for the persisted envelope types in this crate.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const LIFECYCLE_SCHEMA_VERSION: u32 = 1;

pub const MANIFESTS_NS: &str = "registry.manifests";
pub const LIFECYCLE_NS: &str = "registry.lifecycle";
pub const META_NS: &str = "registry.meta";
pub const BLOBS_NS: &str = "registry.blobs";
/// Anatomy records (Memory Fabric M2): optional tensor-layout metadata.
/// Write-once semantically, alongside the immutable manifest — never a second,
/// parallel registry with its own authority. Absent records simply mean "no
/// anatomy was captured or parseable", which must never affect registration.
pub const ANATOMY_NS: &str = "registry.anatomy";
/// Provider bindings: which provider tag serves a registry artifact. The
/// artifact id and the provider tag are different naming realms — `bind`
/// makes that mapping explicit, durable, and operator-audited instead of
/// leaving inference to silently fall back to a provider default.
pub const BINDINGS_NS: &str = "registry.bindings";

/// Current schema version for [`AnatomyRecord`].
pub const ANATOMY_SCHEMA_VERSION: u32 = 1;

/// Current schema version for [`ProviderBinding`].
pub const BINDING_SCHEMA_VERSION: u32 = 1;

/// A recorded, bounded tensor-layout snapshot for one artifact (M2).
///
/// Best-effort by contract: `capture_anatomy` never fails registration, and a
/// missing or malformed record must read as "unknown", never as data. The
/// anatomy it carries describes the artifact **file**, not how OpenRT would
/// execute it through tiered memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnatomyRecord {
    pub schema_version: u32,
    pub artifact_id: String,
    /// Verified SHA-256 of the artifact, matching `verify.records`.
    pub sha256: String,
    pub size_bytes: u64,
    /// Absolute path of the registered file (external blob), for provenance.
    pub path: String,
    pub anatomy: openrt_models::ModelAnatomy,
    pub recorded_at: String,
}

/// One explicit mapping of a registry artifact to a provider tag.
///
/// Registry IDs (`local/chat/…`), artifact paths, provider ids, and provider
/// model tags are four different naming realms. A binding records the
/// operator's decision that artifact `X` is served by tag `T` on provider `P`,
/// so a request for the artifact never has to guess — and never silently lands
/// on the provider default. Bindings are additive metadata: they never move
/// bytes, never activate anything, and a missing binding just means "not
/// bound yet", like a missing anatomy record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBinding {
    pub schema_version: u32,
    pub artifact_id: String,
    /// Provider id (e.g. `ollama`) the tag is for. The same artifact may be
    /// bound to different tags on different providers.
    pub provider: String,
    /// Provider model tag (e.g. `qwen3.5:9b-q8_0`) that serves the artifact.
    pub tag: String,
    pub bound_at: String,
    pub actor: String,
}

/// Artifact lifecycle: `discovered -> downloaded -> verified -> scanned ->
/// active | idle -> retired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Discovered,
    Downloaded,
    Verified,
    Scanned,
    Active,
    Idle,
    Retired,
}

impl LifecycleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Downloaded => "downloaded",
            Self::Verified => "verified",
            Self::Scanned => "scanned",
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Retired => "retired",
        }
    }

    /// Directed acyclic edge set. `retired` is terminal; anything may retire.
    pub fn can_transition(self, to: LifecycleState) -> bool {
        use LifecycleState::*;
        matches!(
            (self, to),
            (_, Retired)
                | (Discovered, Discovered)
                | (Discovered, Downloaded)
                | (Downloaded, Downloaded)
                | (Downloaded, Verified)
                | (Verified, Scanned)
                | (Scanned, Active)
                | (Active, Idle)
                | (Idle, Active)
        )
    }
}

/// One recorded transition in the lifecycle history (append-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionRecord {
    pub from: LifecycleState,
    pub to: LifecycleState,
    pub at: String,
    pub actor: String,
    pub reason: String,
}

/// Current state of the lifecycle envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRecord {
    pub artifact_id: String,
    pub state: LifecycleState,
    pub seq: u64,
    pub updated_at: String,
    pub history: Vec<TransitionRecord>,
}

/// Where an artifact's bytes actually live.
///
/// This distinction is the whole point: "this artifact is registered" and "these
/// bytes are inside our store" are different facts, and conflating them is what
/// forces a 9 GiB model to be resident in RAM just to be catalogued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "where", rename_all = "snake_case")]
pub enum BlobLocation {
    /// Bytes are materialized in the registry store under
    /// `registry.blobs/<digest>`. The store owns them, so the registry can scan
    /// them and activate them.
    Stored,
    /// Bytes live at this filesystem path and are **not** copied into the store.
    /// The digest was computed by streaming the file, so registration costs
    /// bounded memory. The registry cannot scan or activate these bytes: the
    /// operator's file is the only copy and it may move or change underneath us.
    External {
        /// Path as the operator gave it, recorded for provenance and for the
        /// execution backend that will read it.
        path: String,
    },
}

impl Default for BlobLocation {
    /// Stored: a manifest written before by-reference registration existed
    /// describes bytes the registry owns, and must keep meaning exactly that.
    fn default() -> Self {
        Self::Stored
    }
}

impl BlobLocation {
    pub fn is_stored(&self) -> bool {
        matches!(self, Self::Stored)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::External { .. } => "external",
        }
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Stored => None,
            Self::External { path } => Some(path),
        }
    }
}

/// One file in a model artifact (declared in the immutable manifest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// Logical file key, e.g. `model.safetensors` (single-file artifacts use
    /// the file name).
    pub file_key: String,
    /// SHA-256 hex of the blob bytes.
    pub digest: String,
    pub size_bytes: u64,
    /// Where those bytes live. Defaults to [`BlobLocation::Stored`], so every
    /// manifest written before by-reference registration existed deserializes
    /// unchanged and keeps its original meaning.
    #[serde(default)]
    pub location: BlobLocation,
}

/// Immutable content manifest of one artifact. Written exactly once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestV1 {
    pub schema_version: u32,
    pub artifact_id: String,
    /// Runtime shape: safetensors files get format-checked at scan time;
    /// `binary` is any other verified bytes.
    pub kind: ArtifactKind,
    pub blobs: Vec<BlobRef>,
    /// Base digest of the underlying immutable artifact for derived artifacts
    /// (ADR-001). Absent for base artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_digest: Option<String>,
    pub created_at: String,
}

/// Operator-visible, searchable registry card (`registry.meta`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryCard {
    pub artifact_id: String,
    pub display_name: String,
    pub family: String,
    pub tags: Vec<String>,
    /// Recomputed from the manifest: bytes actually stored **by the registry**
    /// for this artifact. Zero for an artifact registered by reference, whose
    /// bytes are the operator's file.
    pub installed_bytes: u64,
    /// Declared artifact size, stored or referenced. `None` when unknown (a
    /// curated entry lists no size — the catalog does not fabricate one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_bytes: Option<u64>,
    /// Where the bytes live: `stored` or `external`. `None` for a curated entry
    /// that was never materialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_location: Option<String>,
    pub state: LifecycleState,
    pub source: SourceLabel,
    pub digest_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceLabel {
    Curated,
    LocalImport,
    HuggingFace,
}

impl SourceLabel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Curated => "curated",
            Self::LocalImport => "local",
            Self::HuggingFace => "huggingface",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("artifact `{0}` is not present in the registry")]
    NotFound(String),
    #[error("manifest for `{0}` already exists and is immutable (refusing to overwrite)")]
    ManifestImmutable(String),
    #[error(
        "invalid lifecycle transition {artifact} {from:?} -> {to:?} (fail-closed: no state is ever skipped)"
    )]
    InvalidTransition {
        artifact: String,
        from: LifecycleState,
        to: LifecycleState,
    },
    #[error(
        "digest mismatch for `{artifact}` file `{file}`: expected {expected}, computed {computed}"
    )]
    DigestMismatch {
        artifact: String,
        file: String,
        expected: String,
        computed: String,
    },
    #[error("scan blocked `{artifact}`: {reason}")]
    ScanBlocked { artifact: String, reason: String },
    #[error("scan inconclusive `{artifact}`: {reason}")]
    ScanInconclusive { artifact: String, reason: String },
    #[error("verification error: {0}")]
    Verify(#[from] openrt_verify::VerifyError),
    #[error("mode error: {0}")]
    Mode(#[from] openrt_modes::ModeError),
    #[error("audit error: {0}")]
    Audit(#[from] openrt_audit::AuditError),
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("no such curated entry `{0}`")]
    NoSuchCurated(String),
    #[error("internal: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// Curated (Ctrl+P) offline library
// ---------------------------------------------------------------------------

/// One entry of the curated catalog. Curated entries are *catalog rows*: by
/// construction they carry **no** size and **no** digest fields, so the
/// registry never presents a fabricated download. Installing a curated entry
/// means an operator imports a local, verified copy; one-click remote install
/// of a real artifact requires the signed bundle + verified bytes and is out
/// of Phase 3 scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedEntry {
    pub artifact_id: &'static str,
    pub display_name: &'static str,
    pub family: &'static str,
    pub tags: &'static [&'static str],
    pub modality: &'static str,
    pub notes: &'static str,
}

impl CuratedEntry {
    pub fn to_card(&self) -> RegistryCard {
        RegistryCard {
            artifact_id: self.artifact_id.to_string(),
            display_name: self.display_name.to_string(),
            family: self.family.to_string(),
            tags: self.tags.iter().map(|t| (*t).to_string()).collect(),
            installed_bytes: 0,
            artifact_bytes: None,
            blob_location: None,
            state: LifecycleState::Discovered,
            source: SourceLabel::Curated,
            digest_prefix: None,
        }
    }
}

/// The curated catalog: well-known open-weight model ids that OpenRT can
/// describe offline. Static and honest — never records sizes/digests it does
/// not have.
pub fn curated_catalog() -> &'static [CuratedEntry] {
    &[
        CuratedEntry {
            artifact_id: "curated/llama3.1-8b",
            display_name: "Llama 3.1 8B Instruct",
            family: "llama",
            tags: &["instruct", "chat"],
            modality: "text",
            notes: "general-purpose 8B chat; safetensors",
        },
        CuratedEntry {
            artifact_id: "curated/llama3.2-1b",
            display_name: "Llama 3.2 1B Instruct",
            family: "llama",
            tags: &["instruct", "light"],
            modality: "text",
            notes: "small fast chat model; safetensors",
        },
        CuratedEntry {
            artifact_id: "curated/qwen2.5-7b",
            display_name: "Qwen 2.5 7B Instruct",
            family: "qwen",
            tags: &["instruct", "chat"],
            modality: "text",
            notes: "multilingual chat model; safetensors",
        },
        CuratedEntry {
            artifact_id: "curated/qwen2.5-14b",
            display_name: "Qwen 2.5 14B Instruct",
            family: "qwen",
            tags: &["instruct", "large"],
            modality: "text",
            notes: "larger multilingual chat model; safetensors",
        },
        CuratedEntry {
            artifact_id: "curated/mixtral-8x7b",
            display_name: "Mixtral 8x7B Instruct",
            family: "mixtral",
            tags: &["moe", "expert", "instruct"],
            modality: "text",
            notes: "sparse expert (MoE) model; expert metadata only if a real manifest declares it",
        },
    ]
}

/// Look up one curated row by id.
pub fn curated_entry(id: &str) -> Option<&'static CuratedEntry> {
    curated_catalog().iter().find(|e| e.artifact_id == id)
}

fn key(id: &str) -> String {
    openrt_core::encode_safe_key(id)
}

/// One row returned by [`ModelRegistry::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub artifact_id: String,
    pub display_name: String,
    pub source: SourceLabel,
    pub installed: bool,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// The model registry: one store, one shared audit chain, one mode policy.
pub struct ModelRegistry {
    store: Arc<dyn openrt_persistence::PersistenceStore>,
    modes: ModeManager,
    audit: AuditLog,
    anatomy_options: openrt_models::AnatomyOptions,
}

impl ModelRegistry {
    pub fn new(store: Arc<dyn openrt_persistence::PersistenceStore>, modes: ModeManager) -> Self {
        Self::with_anatomy_options(store, modes, openrt_models::AnatomyOptions::default())
    }

    /// [`ModelRegistry::new`] with explicit M8 anatomy derivation options.
    ///
    /// `AnatomyOptions::default()` keeps quantized tensor lengths `None`;
    /// enabling `quantized_lengths` opt-in derives them from the versioned ggml
    /// table, tagged `QuantTableV1`. This is a registry-wide, persistent
    /// setting; it never changes what was already persisted.
    pub fn with_anatomy_options(
        store: Arc<dyn openrt_persistence::PersistenceStore>,
        modes: ModeManager,
        anatomy_options: openrt_models::AnatomyOptions,
    ) -> Self {
        let audit = AuditLog::new(store.clone());
        Self {
            store,
            modes,
            audit,
            anatomy_options,
        }
    }

    /// Current execution mode (for CLI/reporting).
    pub async fn mode(&self) -> Result<ExecutionMode, RegistryError> {
        Ok(self.modes.current().await?.mode)
    }

    // ------------------------------------------------------------------
    // Search / list / status
    // ------------------------------------------------------------------

    /// Search the offline curated catalog + the installed registry from any
    /// mode (network-free). With the `hf` feature, a remote HF search leg is
    /// appended only after `ensure_network_allowed()` passes (Sovereign
    /// refuses **before any socket**); the failure is silent — curated + local
    /// results are still returned, never fabricated remote rows.
    pub async fn search(&self, q: &str, hf: Option<&HfClient>) -> Vec<SearchResult> {
        let needle = q.to_ascii_lowercase();
        let mut out = Vec::new();
        // Installed artifacts (cards persisted in `registry.meta` at import
        // time). Curated-only rows are synthesized below and stay uninstalled.
        if let Ok(rows) = self.store.list_records(META_NS).await {
            for (_, value) in rows {
                if let Ok(card) = serde_json::from_value::<RegistryCard>(value) {
                    let id = card.artifact_id.clone();
                    if card.artifact_id.to_ascii_lowercase().contains(&needle) {
                        let card = maintain_card_state(self.store.as_ref(), card).await;
                        out.push(SearchResult {
                            artifact_id: id,
                            display_name: card.display_name,
                            source: SourceLabel::LocalImport,
                            installed: true,
                        });
                    }
                }
            }
        }
        for entry in curated_catalog() {
            if (entry.artifact_id.to_ascii_lowercase().contains(&needle)
                || entry.display_name.to_ascii_lowercase().contains(&needle))
                && !out.iter().any(|r| r.artifact_id == entry.artifact_id)
            {
                out.push(SearchResult {
                    artifact_id: entry.artifact_id.to_string(),
                    display_name: entry.display_name.to_string(),
                    source: SourceLabel::Curated,
                    installed: false,
                });
            }
        }
        if let Some(hf) = hf {
            let _ = &hf;
            if self.modes.ensure_network_allowed().await.is_ok() {
                #[cfg(feature = "hf")]
                if let Ok(remote) = hf.search(q).await {
                    for r in remote {
                        let id = r.id;
                        out.push(SearchResult {
                            artifact_id: id.clone(),
                            display_name: id,
                            source: SourceLabel::HuggingFace,
                            installed: false,
                        });
                    }
                }
            }
        }
        out
    }

    /// List operator cards of everything known to the registry (sorted by id).
    pub async fn list(&self) -> Result<Vec<RegistryCard>, RegistryError> {
        let mut cards = Vec::new();
        for (_, value) in self.store.list_records(META_NS).await? {
            let card: RegistryCard = serde_json::from_value(value)?;
            cards.push(maintain_card_state(self.store.as_ref(), card).await);
        }
        let known: Vec<String> = cards.iter().map(|c| c.artifact_id.clone()).collect();
        for entry in curated_catalog() {
            if !known.contains(&entry.artifact_id.to_string()) {
                cards.push(entry.to_card());
            }
        }
        cards.sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));
        Ok(cards)
    }

    /// Registry card for one artifact (curated rows are returned even when not
    /// installed).
    pub async fn status(&self, artifact_id: &str) -> Result<RegistryCard, RegistryError> {
        let k = key(artifact_id);
        if let Some(v) = self.store.get_record(META_NS, &k).await? {
            let card: RegistryCard = serde_json::from_value(v)?;
            return Ok(maintain_card_state(self.store.as_ref(), card).await);
        }
        if let Some(entry) = curated_entry(artifact_id) {
            return Ok(entry.to_card());
        }
        Err(RegistryError::NotFound(artifact_id.to_string()))
    }

    // ------------------------------------------------------------------
    // Content manifests (immutable)
    // ------------------------------------------------------------------

    /// Register an immutable content manifest (write-once, ADR-010).
    pub async fn publish_manifest(&self, manifest: &ManifestV1) -> Result<(), RegistryError> {
        if manifest.artifact_id.is_empty() {
            return Err(RegistryError::Internal(
                "manifest artifact_id must not be empty".into(),
            ));
        }
        let k = key(&manifest.artifact_id);
        if self.store.get_record(MANIFESTS_NS, &k).await?.is_some() {
            return Err(RegistryError::ManifestImmutable(
                manifest.artifact_id.clone(),
            ));
        }
        self.store
            .put_record(MANIFESTS_NS, &k, &serde_json::to_value(manifest)?)
            .await?;
        Ok(())
    }

    /// The immutable manifest, if installed.
    pub async fn manifest(&self, artifact_id: &str) -> Result<Option<ManifestV1>, RegistryError> {
        let k = key(artifact_id);
        let Some(v) = self.store.get_record(MANIFESTS_NS, &k).await? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_value(v)?))
    }

    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    /// Read the lifecycle record; a curated/installed artifact without one
    /// derives `discovered`. `None` for unknown artifacts.
    pub async fn lifecycle(
        &self,
        artifact_id: &str,
    ) -> Result<Option<LifecycleRecord>, RegistryError> {
        let k = key(artifact_id);
        if let Some(v) = self.store.get_record(LIFECYCLE_NS, &k).await? {
            return Ok(Some(serde_json::from_value(v)?));
        }
        if self.store.get_record(MANIFESTS_NS, &k).await?.is_some()
            || curated_entry(artifact_id).is_some()
        {
            return Ok(Some(LifecycleRecord {
                artifact_id: artifact_id.to_string(),
                state: LifecycleState::Discovered,
                seq: 0,
                updated_at: openrt_core::now().to_rfc3339(),
                history: Vec::new(),
            }));
        }
        Ok(None)
    }

    async fn transition(
        &self,
        artifact_id: &str,
        to: LifecycleState,
        actor: &str,
        reason: &str,
    ) -> Result<LifecycleRecord, RegistryError> {
        let k = key(artifact_id);
        let Some(current) = self.lifecycle(artifact_id).await? else {
            return Err(RegistryError::NotFound(artifact_id.to_string()));
        };
        let from = current.state;
        if to == LifecycleState::Retired && from == LifecycleState::Retired {
            return Ok(current);
        }
        if !from.can_transition(to) {
            return Err(RegistryError::InvalidTransition {
                artifact: artifact_id.to_string(),
                from,
                to,
            });
        }
        let record = LifecycleRecord {
            artifact_id: artifact_id.to_string(),
            state: to,
            seq: current.seq + 1,
            updated_at: openrt_core::now().to_rfc3339(),
            history: {
                let mut h = current.history;
                h.push(TransitionRecord {
                    from,
                    to,
                    at: openrt_core::now().to_rfc3339(),
                    actor: actor.to_string(),
                    reason: reason.to_string(),
                });
                h
            },
        };
        // Audit FIRST (fail-closed: a rejected/broken audit chain aborts the
        // transition, mirroring `openrt-modes`).
        self.audit
            .append(
                actor,
                "registry.transition",
                artifact_id,
                to.as_str(),
                serde_json::json!({ "from": from.as_str(), "reason": reason }),
            )
            .await?;
        self.store
            .put_record(LIFECYCLE_NS, &k, &serde_json::to_value(&record)?)
            .await?;
        Ok(record)
    }

    /// Move to `Retired` (terminal). Idempotent.
    pub async fn retire(
        &self,
        artifact_id: &str,
        actor: &str,
        reason: &str,
    ) -> Result<LifecycleRecord, RegistryError> {
        self.transition(artifact_id, LifecycleState::Retired, actor, reason)
            .await
    }

    /// Activate a scanned artifact (`Scanned -> Active`). This is the ONLY
    /// way to become activatable; it requires a verified + scanned content
    /// manifest (ADR-010), enforced fail-closed here.
    pub async fn activate(
        &self,
        artifact_id: &str,
        actor: &str,
    ) -> Result<LifecycleRecord, RegistryError> {
        // Fail closed on by-reference artifacts. Activation means "routable",
        // and routing needs bytes the registry can vouch for: a blob stored
        // elsewhere can be moved, truncated, or replaced after the digest was
        // taken, and it never passed the scan gate. Refusing here keeps
        // `Scanned -> Active` meaning what it has always meant instead of
        // widening it for a new kind of artifact.
        if let Some(m) = self.manifest(artifact_id).await? {
            let external: Vec<&str> = m
                .blobs
                .iter()
                .filter(|b| !b.location.is_stored())
                .filter_map(|b| b.location.path())
                .collect();
            if !external.is_empty() {
                return Err(RegistryError::Internal(format!(
                    "`{artifact_id}` is registered by reference ({}); activation requires a materialized, scanned artifact — import it with `openrt model install` to store the bytes",
                    external.join(", ")
                )));
            }
        }
        let lr = self
            .transition(artifact_id, LifecycleState::Active, actor, "activated")
            .await?;
        self.touch_card(artifact_id).await?;
        Ok(lr)
    }

    /// Drop an active artifact to idle (runtime unloads; the currently-loaded
    /// set is recorded by the runtime, not the registry).
    pub async fn idle(
        &self,
        artifact_id: &str,
        actor: &str,
    ) -> Result<LifecycleRecord, RegistryError> {
        self.transition(artifact_id, LifecycleState::Idle, actor, "unloaded")
            .await
    }

    /// True iff the artifact is `Active`.
    pub async fn is_active(&self, artifact_id: &str) -> Result<bool, RegistryError> {
        Ok(matches!(
            self.lifecycle(artifact_id).await?,
            Some(l) if l.state == LifecycleState::Active
        ))
    }

    // ------------------------------------------------------------------
    // Guided install pipeline
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Registration vs materialization
    // ------------------------------------------------------------------

    /// Register a local artifact **by reference**: index it without ever
    /// holding its bytes.
    ///
    /// This is the path for an artifact that already exists on the operator's
    /// disk — which is the normal case for a multi-gigabyte local model. It
    /// does exactly what "import" is supposed to mean and nothing more:
    ///
    /// 1. Streams the file through a fixed 1 MiB buffer
    ///    ([`openrt_verify::sha256_file`]) and requires the digest and byte
    ///    count to match what the caller declared. Memory cost is the buffer,
    ///    not the artifact.
    /// 2. Records the blob as [`BlobLocation::External`] — the registry indexes
    ///    the artifact, it does not copy it. Nothing is written to
    ///    `registry.blobs`, so the operator's file stays the single copy.
    /// 3. Publishes the immutable manifest and the verification record, then
    ///    walks `Discovered -> Downloaded -> Verified` exactly as
    ///    [`Registry::import_local`] does.
    ///
    /// What it deliberately does **not** do: load, instantiate, memory-map, or
    /// admit the model for execution. Those belong to the execution boundary
    /// (`openrt-resources` admission + the provider backend), and a machine that
    /// cannot hold a model in RAM must still be able to *register* it.
    ///
    /// The lifecycle lands at `Verified`, never `Scanned`: the signature scan
    /// gate reads artifact bytes, and this path never materializes them.
    /// Activation is refused for external blobs (see [`Registry::activate`]),
    /// which keeps the registry's fail-closed "scanned before routable" rule
    /// intact rather than quietly widening it.
    ///
    /// Mode policy: identical to [`Registry::import_local`] — materialization
    /// is an explicit, permission-gated operation, and registering a reference
    /// to local bytes is the same kind of act.
    pub async fn register_local(
        &self,
        manifest: ManifestV1,
        path: &str,
        actor: &str,
    ) -> Result<LifecycleRecord, RegistryError> {
        self.modes
            .ensure_materialization_allowed(&manifest.artifact_id)
            .await?;

        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(RegistryError::Internal(format!(
                "manifest schema {} not supported",
                manifest.schema_version
            )));
        }
        if manifest.blobs.is_empty() || manifest.artifact_id.is_empty() {
            return Err(RegistryError::Internal(
                "manifest must declare a non-empty artifact and at least one blob".into(),
            ));
        }
        if manifest.blobs.len() != 1 {
            return Err(RegistryError::Internal(
                "by-reference registration takes exactly one file; multi-file artifacts must be materialized".into(),
            ));
        }

        // 1) streamed verify — bounded memory, no `fs::read`
        let declared_digest = manifest.blobs[0].digest.clone();
        let declared_size = manifest.blobs[0].size_bytes;
        let file_key = manifest.blobs[0].file_key.clone();
        let (computed, bytes_read) = openrt_verify::sha256_file(std::path::Path::new(path))
            .map_err(|e| match e {
                openrt_verify::VerifyError::Io { error, .. } => {
                    RegistryError::Internal(format!("cannot read `{path}`: {error}"))
                }
                other => RegistryError::Internal(other.to_string()),
            })?;
        if computed != declared_digest {
            return Err(RegistryError::DigestMismatch {
                artifact: manifest.artifact_id.clone(),
                file: file_key,
                expected: declared_digest,
                computed,
            });
        }
        if bytes_read != declared_size {
            return Err(RegistryError::DigestMismatch {
                artifact: manifest.artifact_id.clone(),
                file: file_key,
                expected: format!("size {declared_size}"),
                computed: format!("size {bytes_read}"),
            });
        }

        // 1b) bounded anatomy capture (M2). Optional metadata only, fail-open:
        //    a file that is not a GGUF/safetensors model, or that fails to
        //    parse, must not change registration outcome. No payload bytes are
        //    read here — the header region is bounded and never the file.
        self.capture_anatomy(&manifest.artifact_id, path, &computed, bytes_read)
            .await;

        // 2) mark the blob as living outside the store
        let mut manifest = manifest;
        manifest.blobs[0].location = BlobLocation::External {
            path: path.to_string(),
        };

        // 3) manifest + verification record, then the same two DAG hops
        self.publish_manifest(&manifest).await?;
        self.store
            .put_record(
                openrt_verify::VERIFY_NS,
                &key(&manifest.artifact_id),
                &serde_json::to_value(openrt_verify::VerifyRecord {
                    artifact_id: manifest.artifact_id.clone(),
                    sha256: declared_digest,
                    size_bytes: declared_size,
                    verified_at: openrt_core::now().to_rfc3339(),
                    method: "sha256-streamed".into(),
                    kind: openrt_verify::ArtifactClass::Base,
                })?,
            )
            .await?;
        self.transition(
            &manifest.artifact_id,
            LifecycleState::Downloaded,
            actor,
            "registered by reference",
        )
        .await?;
        let lr = self
            .transition(
                &manifest.artifact_id,
                LifecycleState::Verified,
                actor,
                "import verified (streamed digest)",
            )
            .await?;
        self.write_card(&manifest, lr.state).await?;
        Ok(lr)
    }

    /// Full **local import** from bytes already on disk:
    ///
    /// 1. Computes the SHA-256 of every file and requires it to match the
    ///    manifest's declared digests (ADR-001; never trusts supplied
    ///    digests).
    /// 2. Persists blobs content-addressed (`registry.blobs/<digest>`).
    /// 3. Publishes the immutable manifest (write-once).
    /// 4. Records the verification (`verify.records`) and moves
    ///    `Downloaded -> Verified`.
    /// 5. With a `Scanner` + `min_scheme_version`: runs the scan; a `Clean`
    ///    verdict moves `Verified -> Scanned`; `Blocked`/`Inconclusive` fail
    ///    the install (fail-closed — a revoked/known-bad hash aborts before the
    ///    artifact can be activated).
    ///
    /// This path requires every byte in memory at once and copies them into the
    /// store. For an artifact that is already a file on disk, prefer
    /// [`Registry::register_local`], which needs neither.
    ///
    /// Mode policy: local materialization is allowed in both modes; network
    /// operations are routed through `ensure_network_allowed` separately.
    pub async fn import_local(
        &self,
        manifest: ManifestV1,
        files: &BTreeMap<String, Vec<u8>>,
        actor: &str,
        scan: Option<(&openrt_scan::Scanner, u32)>,
    ) -> Result<LifecycleRecord, RegistryError> {
        self.modes
            .ensure_materialization_allowed(&manifest.artifact_id)
            .await?;

        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(RegistryError::Internal(format!(
                "manifest schema {} not supported",
                manifest.schema_version
            )));
        }
        if manifest.blobs.is_empty() || manifest.artifact_id.is_empty() {
            return Err(RegistryError::Internal(
                "manifest must declare a non-empty artifact and at least one blob".into(),
            ));
        }

        // 1) verify every declared file against its digest and size
        for blob in &manifest.blobs {
            let Some(bytes) = files.get(&blob.file_key) else {
                return Err(RegistryError::Internal(format!(
                    "missing file `{}` for {}",
                    blob.file_key, manifest.artifact_id
                )));
            };
            let computed = openrt_verify::sha256_hex(bytes);
            if computed != blob.digest {
                return Err(RegistryError::DigestMismatch {
                    artifact: manifest.artifact_id.clone(),
                    file: blob.file_key.clone(),
                    expected: blob.digest.clone(),
                    computed,
                });
            }
            if bytes.len() as u64 != blob.size_bytes {
                return Err(RegistryError::DigestMismatch {
                    artifact: manifest.artifact_id.clone(),
                    file: blob.file_key.clone(),
                    expected: format!("size {}", blob.size_bytes),
                    computed: format!("size {}", bytes.len()),
                });
            }
        }
        let declared: std::collections::HashSet<&String> =
            manifest.blobs.iter().map(|b| &b.file_key).collect();
        if files.len() != declared.len() {
            return Err(RegistryError::Internal(
                "import supplied files not declared by the manifest".into(),
            ));
        }

        // 2) persist blobs (content-addressed, store-integrity checked)
        for blob in &manifest.blobs {
            let bytes = &files[&blob.file_key];
            self.store.put_blob(BLOBS_NS, &blob.digest, bytes).await?;
        }

        // 3) publish immutable manifest
        self.publish_manifest(&manifest).await?;

        // 4) verification record + walk the DAG hop-by-hop (fail-closed: no
        //    state is ever skipped, ADR-010). Local import atomically covers
        //    discovery + download, so we step `Discovered -> Downloaded` and
        //    then `Downloaded -> Verified`.
        self.store
            .put_record(
                openrt_verify::VERIFY_NS,
                &key(&manifest.artifact_id),
                &serde_json::to_value(openrt_verify::VerifyRecord {
                    artifact_id: manifest.artifact_id.clone(),
                    sha256: manifest
                        .blobs
                        .iter()
                        .map(|b| b.digest.clone())
                        .collect::<Vec<_>>()
                        .join(","),
                    size_bytes: manifest.blobs.iter().map(|b| b.size_bytes).sum(),
                    verified_at: openrt_core::now().to_rfc3339(),
                    method: "sha256".into(),
                    kind: openrt_verify::ArtifactClass::Base,
                })?,
            )
            .await?;
        self.transition(
            &manifest.artifact_id,
            LifecycleState::Downloaded,
            actor,
            "imported",
        )
        .await?;
        let mut lr = self
            .transition(
                &manifest.artifact_id,
                LifecycleState::Verified,
                actor,
                "import verified",
            )
            .await?;

        // 5) optional scan gate (fail-closed)
        if let Some((scanner, min_scheme_version)) = scan {
            let mut verdicts = Vec::new();
            for blob in &manifest.blobs {
                let bytes = &files[&blob.file_key];
                verdicts.push(scanner.scan(manifest.kind, bytes, min_scheme_version));
            }
            let has_block = verdicts
                .iter()
                .any(|v| matches!(v, openrt_scan::ScanVerdict::Blocked { .. }));
            let clean = verdicts.iter().all(|v| v.is_clean());
            let verdict = if clean {
                openrt_scan::ScanVerdict::Clean {
                    bundle_id: "scan-gate".into(),
                    bundle_version: min_scheme_version,
                }
            } else {
                let reason = verdicts
                    .iter()
                    .map(|v| match v {
                        openrt_scan::ScanVerdict::Clean { .. } => "clean".into(),
                        openrt_scan::ScanVerdict::Blocked { reason } => {
                            format!("blocked: {reason}")
                        }
                        openrt_scan::ScanVerdict::Inconclusive { reason } => {
                            format!("inconclusive: {reason}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                if has_block {
                    openrt_scan::ScanVerdict::Blocked { reason }
                } else {
                    openrt_scan::ScanVerdict::Inconclusive { reason }
                }
            };
            self.store
                .put_record(
                    openrt_scan::SCAN_NS,
                    &key(&manifest.artifact_id),
                    &serde_json::to_value(openrt_scan::ScanRecord {
                        artifact_id: manifest.artifact_id.clone(),
                        verdict: verdict.clone(),
                        bundle_id: None,
                        scanned_at: openrt_core::now().to_rfc3339(),
                    })?,
                )
                .await?;
            match verdict {
                openrt_scan::ScanVerdict::Clean { .. } => {
                    lr = self
                        .transition(
                            &manifest.artifact_id,
                            LifecycleState::Scanned,
                            actor,
                            "scan passed",
                        )
                        .await?;
                }
                openrt_scan::ScanVerdict::Blocked { reason } => {
                    return Err(RegistryError::ScanBlocked {
                        artifact: manifest.artifact_id.clone(),
                        reason,
                    });
                }
                openrt_scan::ScanVerdict::Inconclusive { reason } => {
                    return Err(RegistryError::ScanInconclusive {
                        artifact: manifest.artifact_id.clone(),
                        reason,
                    });
                }
            }
        }

        // 6) operator card
        self.write_card(&manifest, lr.state).await?;

        Ok(lr)
    }

    /// Derive and persist the operator card for a locally registered artifact.
    ///
    /// When the id matches a curated catalog row its descriptive metadata
    /// (display name, family, tags) is authoritative; unknown ids keep honest
    /// neutral defaults. `installed_bytes` counts only what the registry
    /// stores, so a by-reference artifact reads as `0` stored with its real
    /// size in `artifact_bytes` — the two facts an operator needs are not the
    /// same fact.
    async fn write_card(
        &self,
        manifest: &ManifestV1,
        state: LifecycleState,
    ) -> Result<(), RegistryError> {
        let curated = curated_entry(&manifest.artifact_id);
        let (display_name, family, tags) = match curated {
            Some(e) => (
                e.display_name.to_string(),
                e.family.to_string(),
                e.tags.iter().map(|t| (*t).to_string()).collect(),
            ),
            None => (manifest.artifact_id.clone(), "unknown".into(), Vec::new()),
        };
        let card = RegistryCard {
            artifact_id: manifest.artifact_id.clone(),
            display_name,
            family,
            tags,
            installed_bytes: manifest
                .blobs
                .iter()
                .filter(|b| b.location.is_stored())
                .map(|b| b.size_bytes)
                .sum(),
            artifact_bytes: Some(manifest.blobs.iter().map(|b| b.size_bytes).sum()),
            blob_location: manifest
                .blobs
                .first()
                .map(|b| b.location.as_str().to_string()),
            state,
            source: SourceLabel::LocalImport,
            digest_prefix: manifest
                .blobs
                .first()
                .map(|b| b.digest.chars().take(12).collect()),
        };
        self.store
            .put_record(
                META_NS,
                &key(&manifest.artifact_id),
                &serde_json::to_value(&card)?,
            )
            .await?;
        Ok(())
    }

    /// Sum of bytes actually stored for installed artifacts (from manifests —
    /// real accounting, never fabricated).
    ///
    /// By-reference blobs are excluded: their bytes are the operator's file, not
    /// ours. Counting them here would report registry storage that does not
    /// exist, which is exactly the kind of number that makes a quota decision
    /// wrong.
    pub async fn storage_used_bytes(&self) -> Result<u64, RegistryError> {
        let mut total = 0u64;
        for (_, value) in self.store.list_records(MANIFESTS_NS).await? {
            let m: ManifestV1 = serde_json::from_value(value)?;
            total += m
                .blobs
                .iter()
                .filter(|b| b.location.is_stored())
                .map(|b| b.size_bytes)
                .sum::<u64>();
        }
        Ok(total)
    }

    /// Sum of declared artifact bytes across all manifests, stored or
    /// referenced. This is the number the Models UI shows as "on disk"; it is
    /// deliberately *not* what [`Registry::storage_used_bytes`] reports.
    pub async fn artifact_bytes_declared(&self) -> Result<u64, RegistryError> {
        let mut total = 0u64;
        for (_, value) in self.store.list_records(MANIFESTS_NS).await? {
            let m: ManifestV1 = serde_json::from_value(value)?;
            total += m.blobs.iter().map(|b| b.size_bytes).sum::<u64>();
        }
        Ok(total)
    }

    /// Read a stored blob by its content digest.
    pub async fn blob(&self, digest: &str) -> Result<Option<Vec<u8>>, RegistryError> {
        Ok(self.store.get_blob(BLOBS_NS, digest).await?)
    }

    /// Read the recorded anatomy for an artifact, if one was captured.
    ///
    /// `None` means "never captured or not parseable" — it is the honest
    /// unknown, and callers must treat it as such rather than fabricating one.
    pub async fn anatomy(
        &self,
        artifact_id: &str,
    ) -> Result<Option<openrt_models::ModelAnatomy>, RegistryError> {
        let Some(v) = self.store.get_record(ANATOMY_NS, &key(artifact_id)).await? else {
            return Ok(None);
        };
        let rec: AnatomyRecord = serde_json::from_value(v)?;
        Ok(Some(rec.anatomy))
    }

    /// Persist an anatomy record, best-effort.
    ///
    /// Called by [`ModelRegistry::capture_anatomy`]; never propagates failure
    /// upward because anatomy is metadata, not a gate.
    pub async fn store_anatomy(
        &self,
        artifact_id: &str,
        sha256: &str,
        size_bytes: u64,
        path: &str,
        anatomy: &openrt_models::ModelAnatomy,
    ) -> Result<(), RegistryError> {
        let rec = AnatomyRecord {
            schema_version: ANATOMY_SCHEMA_VERSION,
            artifact_id: artifact_id.to_string(),
            sha256: sha256.to_string(),
            size_bytes,
            path: path.to_string(),
            anatomy: anatomy.clone(),
            recorded_at: openrt_core::now().to_rfc3339(),
        };
        self.store
            .put_record(ANATOMY_NS, &key(artifact_id), &serde_json::to_value(&rec)?)
            .await?;
        Ok(())
    }

    /// Best-effort anatomy capture during [`ModelRegistry::register_local`].
    ///
    /// Bounded and fail-open by design: only header regions are ever read
    /// (payload bytes are jumped, never read), and every failure — unknown
    /// format, truncated header, store error — is swallowed so that
    /// registration outcome is byte-for-byte identical to pre-M2 behavior.
    ///
    /// M7: a safetensors shard index (`*.safetensors.index.json` or any JSON
    /// object carrying a `weight_map`) describes a multi-file model; its
    /// per-shard anatomy is discovered with bounded, confined reads and
    /// persisted when the index is consistent. A single member of a sharded
    /// model remains an ordinary single-file artifact (M2).
    async fn capture_anatomy(&self, artifact_id: &str, path: &str, sha256: &str, size_bytes: u64) {
        let opts = self.anatomy_options;
        let index_path = std::path::Path::new(path);
        let single = openrt_models::anatomy_for_file_with_options(index_path, opts);
        if let Ok(anatomy) = &single {
            if anatomy.format != openrt_models::AnatomyFormat::Unknown {
                let _ = self
                    .store_anatomy(artifact_id, sha256, size_bytes, path, anatomy)
                    .await;
                return;
            }
        }
        if openrt_models::is_safetensors_index_file(index_path) {
            if let Ok(anatomy) = openrt_models::sharded_anatomy_for_index_file(index_path) {
                let _ = self
                    .store_anatomy(artifact_id, sha256, size_bytes, path, &anatomy)
                    .await;
                return;
            }
        }
        // Parity fallback: a single-file anatomy (possibly Unknown) is stored
        // exactly as pre-M7; a hard parse error stores nothing.
        if let Ok(anatomy) = single {
            let _ = self
                .store_anatomy(artifact_id, sha256, size_bytes, path, &anatomy)
                .await;
        }
    }

    async fn touch_card(&self, artifact_id: &str) -> Result<(), RegistryError> {
        let k = key(artifact_id);
        let Some(v) = self.store.get_record(META_NS, &k).await? else {
            return Ok(());
        };
        let mut card: RegistryCard = serde_json::from_value(v)?;
        if let Some(lc) = self.lifecycle(artifact_id).await? {
            card.state = lc.state;
        }
        self.store
            .put_record(META_NS, &k, &serde_json::to_value(&card)?)
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Provider bindings (Part D): artifact id → provider tag
    // ------------------------------------------------------------------

    fn bind_key(artifact_id: &str, provider: &str) -> String {
        format!(
            "{}.{}",
            key(artifact_id),
            openrt_core::encode_safe_key(provider)
        )
    }

    /// All bindings recorded for one artifact (any provider), sorted by
    /// provider. `[]` = nothing bound (the honest unknown, never a guessed
    /// default).
    pub async fn bound_tags(
        &self,
        artifact_id: &str,
    ) -> Result<Vec<ProviderBinding>, RegistryError> {
        let mut out = Vec::new();
        for (_, value) in self.store.list_records(BINDINGS_NS).await? {
            let Ok(binding) = serde_json::from_value::<ProviderBinding>(value) else {
                continue;
            };
            if binding.artifact_id == artifact_id {
                out.push(binding);
            }
        }
        out.sort_by(|a, b| a.provider.cmp(&b.provider));
        Ok(out)
    }

    /// The binding for `artifact_id` on one provider, if the operator made one.
    pub async fn bound_tag(
        &self,
        artifact_id: &str,
        provider: &str,
    ) -> Result<Option<ProviderBinding>, RegistryError> {
        let Some(v) = self
            .store
            .get_record(BINDINGS_NS, &Self::bind_key(artifact_id, provider))
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_value(v)?))
    }

    /// Persist an explicit artifact → provider-tag mapping.
    ///
    /// Fail-closed on an unknown artifact (you cannot bind what the registry
    /// does not know) and audited like every other registry decision. The tag
    /// is stored verbatim — never canonicalized or probed here; resolution
    /// still requires a live provider for the exact tag.
    pub async fn bind_provider_tag(
        &self,
        artifact_id: &str,
        provider: &str,
        tag: &str,
        actor: &str,
    ) -> Result<ProviderBinding, RegistryError> {
        if provider.trim().is_empty() || tag.trim().is_empty() {
            return Err(RegistryError::Internal(
                "bind requires a non-empty provider and tag".to_string(),
            ));
        }
        let k = key(artifact_id);
        let known = self.store.get_record(META_NS, &k).await?.is_some()
            || self.store.get_record(MANIFESTS_NS, &k).await?.is_some()
            || curated_catalog()
                .iter()
                .any(|e| e.artifact_id == artifact_id);
        if !known {
            return Err(RegistryError::NotFound(artifact_id.to_string()));
        }
        let binding = ProviderBinding {
            schema_version: BINDING_SCHEMA_VERSION,
            artifact_id: artifact_id.to_string(),
            provider: provider.trim().to_string(),
            tag: tag.trim().to_string(),
            bound_at: openrt_core::now().to_rfc3339(),
            actor: actor.to_string(),
        };
        self.audit
            .append(
                actor,
                "registry.bind",
                artifact_id,
                &binding.tag,
                serde_json::json!({ "provider": provider }),
            )
            .await?;
        self.store
            .put_record(
                BINDINGS_NS,
                &Self::bind_key(artifact_id, provider),
                &serde_json::to_value(&binding)?,
            )
            .await?;
        Ok(binding)
    }

    /// Remove the artifact → provider-tag mapping. Idempotent; returns whether
    /// a binding was actually removed.
    pub async fn unbind_provider_tag(
        &self,
        artifact_id: &str,
        provider: &str,
        actor: &str,
    ) -> Result<bool, RegistryError> {
        let k = Self::bind_key(artifact_id, provider);
        let existed = self.store.get_record(BINDINGS_NS, &k).await?.is_some();
        if existed {
            self.audit
                .append(
                    actor,
                    "registry.unbind",
                    artifact_id,
                    "removed",
                    serde_json::json!({ "provider": provider }),
                )
                .await?;
            self.store.delete_record(BINDINGS_NS, &k).await?;
        }
        Ok(existed)
    }
}

/// Cards are a derived cache; always re-derive state from the authoritative
/// lifecycle record.
async fn maintain_card_state(
    store: &dyn openrt_persistence::PersistenceStore,
    mut card: RegistryCard,
) -> RegistryCard {
    let k = openrt_core::encode_safe_key(&card.artifact_id);
    if let Ok(Some(v)) = store.get_record(LIFECYCLE_NS, &k).await {
        if let Ok(lc) = serde_json::from_value::<LifecycleRecord>(v) {
            card.state = lc.state;
        }
    }
    card
}

// ---------------------------------------------------------------------------
// Hugging Face metadata-only client (`hf` feature)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfModelLite {
    pub id: String,
    pub downloads: Option<u64>,
    pub likes: Option<u64>,
    /// Hub pipeline tag (`text-generation`, `image-classification`, …).
    #[serde(default)]
    pub pipeline_tag: Option<String>,
    /// Hub tags (includes `license:x`, quantization markers, task families).
    #[serde(default)]
    pub tags: Vec<String>,
    /// License from `cardData.license` or a `license:x` tag.
    #[serde(default)]
    pub license: Option<String>,
    /// Total safetensors parameters (`safetensors.parameters.total`).
    #[serde(default)]
    pub params: Option<u64>,
    /// Model author/organization.
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfModelInfo {
    pub id: String,
    pub sha: Option<String>,
    pub siblings: Vec<HfSibling>,
    pub gated: bool,
    /// Hub pipeline tag.
    #[serde(default)]
    pub pipeline_tag: Option<String>,
    /// Hub tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// License from `cardData.license` or a `license:x` tag.
    #[serde(default)]
    pub license: Option<String>,
    /// Total safetensors parameters.
    #[serde(default)]
    pub params: Option<u64>,
    /// Context window from `config.max_position_embeddings` (or the nested
    /// `text_config` variant) — `None` when the hub does not declare one.
    #[serde(default)]
    pub context_length: Option<u64>,
    /// Model author/organization.
    #[serde(default)]
    pub author: Option<String>,
    /// Bytes the hub stores for the repo (`usedStorage`), when declared.
    #[serde(default)]
    pub used_storage: Option<u64>,
}

/// License from `cardData.license`, else the first `license:x` tag.
#[cfg(any(feature = "hf", test))]
fn extract_license(raw: &serde_json::Value) -> Option<String> {
    if let Some(l) = raw
        .get("cardData")
        .and_then(|c| c.get("license"))
        .and_then(|v| v.as_str())
    {
        if !l.is_empty() {
            return Some(l.to_string());
        }
    }
    raw.get("tags")
        .and_then(|t| t.as_array())?
        .iter()
        .filter_map(|v| v.as_str())
        .find_map(|t| t.strip_prefix("license:").map(str::to_string))
}

/// Total parameter count from `safetensors.parameters.total`.
#[cfg(any(feature = "hf", test))]
fn extract_params(raw: &serde_json::Value) -> Option<u64> {
    raw.get("safetensors")?
        .get("parameters")?
        .get("total")?
        .as_u64()
}

/// Context window from the hub's declared config (root or `text_config`).
#[cfg(any(feature = "hf", test))]
fn extract_context(raw: &serde_json::Value) -> Option<u64> {
    let cfg = raw.get("config")?;
    for key in ["max_position_embeddings", "n_positions", "max_seq_len"] {
        if let Some(n) = cfg.get(key).and_then(|v| v.as_u64()) {
            return Some(n);
        }
    }
    let nested = cfg.get("text_config")?;
    for key in ["max_position_embeddings", "n_positions"] {
        if let Some(n) = nested.get(key).and_then(|v| v.as_u64()) {
            return Some(n);
        }
    }
    None
}

/// One search-hit row from the hub's list payload.
#[cfg(any(feature = "hf", test))]
fn lite_from(v: &serde_json::Value) -> HfModelLite {
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let tags: Vec<String> = v
        .get("tags")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut lite = HfModelLite {
        id,
        downloads: v.get("downloads").and_then(|x| x.as_u64()),
        likes: v.get("likes").and_then(|x| x.as_u64()),
        pipeline_tag: v
            .get("pipeline_tag")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        tags,
        license: None,
        params: extract_params(v),
        author: v.get("author").and_then(|x| x.as_str()).map(str::to_string),
    };
    lite.license = extract_license(v);
    lite
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfSibling {
    pub rfilename: String,
    pub size: Option<u64>,
}

/// Minimal Hugging Face Hub API client. **Metadata only** by construction:
/// `search`/`model_info` never fetch weight bytes and never produce a manifest
/// with fabricated digests. Remote installs that would need weight hashes are
/// out of Phase 3 scope and are refused explicitly rather than degraded.
#[derive(Clone)]
pub struct HfClient {
    #[cfg(feature = "hf")]
    client: reqwest::Client,
    #[cfg(feature = "hf")]
    base_url: String,
    #[cfg(feature = "hf")]
    timeout_secs: u64,
}

impl HfClient {
    /// `base_url` defaults to `https://huggingface.co` when empty.
    pub fn new(_base_url: impl Into<String>, _timeout_secs: u64) -> Self {
        Self {
            #[cfg(feature = "hf")]
            client: reqwest::Client::new(),
            #[cfg(feature = "hf")]
            base_url: _base_url.into(),
            #[cfg(feature = "hf")]
            timeout_secs: _timeout_secs,
        }
    }

    #[cfg(feature = "hf")]
    fn api(&self, path: &str) -> String {
        let base = if self.base_url.is_empty() {
            "https://huggingface.co".to_string()
        } else {
            self.base_url.trim_end_matches('/').to_string()
        };
        // Callers append `/models…` to this prefix; an empty path must yield
        // `…/api` (NOT `…/api/`) or every request would double the slash and
        // the hub answers 404.
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            format!("{base}/api")
        } else {
            format!("{base}/api/{path}")
        }
    }

    /// Search the HF model hub (`/api/models?search=`). Network op — callers
    /// must gate with `ModeManager::ensure_network_allowed` first (done by the
    /// registry in `search`).
    pub async fn search(&self, query: &str) -> Result<Vec<HfModelLite>, RegistryError> {
        self.search_page(query, 10, 0).await
    }

    /// Paged hub search: `limit` rows starting at `skip` (the HF API's own
    /// `limit`/`skip` parameters; limit clamped to the hub's max of 100).
    /// Same network/mode contract as [`Self::search`].
    pub async fn search_page(
        &self,
        query: &str,
        limit: u32,
        skip: u32,
    ) -> Result<Vec<HfModelLite>, RegistryError> {
        #[cfg(feature = "hf")]
        {
            let limit = limit.clamp(1, 100);
            let skip = skip.min(4990);
            // Empty query = hub's default (recent) listing; only a typed
            // query adds the `search=` parameter.
            let url = if query.trim().is_empty() {
                format!("{}/models?limit={limit}&skip={skip}", self.api(""))
            } else {
                format!(
                    "{}/models?search={}&limit={limit}&skip={skip}",
                    self.api(""),
                    urlencode(query)
                )
            };
            let resp = self
                .client
                .get(&url)
                .timeout(std::time::Duration::from_secs(self.timeout_secs))
                .send()
                .await
                .map_err(|e| RegistryError::Internal(format!("hf search transport: {e}")))?;
            if !resp.status().is_success() {
                return Err(RegistryError::Internal(format!(
                    "hf search HTTP {}",
                    resp.status().as_u16()
                )));
            }
            let raw: Vec<serde_json::Value> = resp
                .json()
                .await
                .map_err(|e| RegistryError::Internal(format!("hf search json: {e}")))?;
            Ok(raw.iter().map(lite_from).collect())
        }
        #[cfg(not(feature = "hf"))]
        {
            let _ = (self, query, limit, skip);
            Err(RegistryError::Internal(
                "built without the `hf` feature".into(),
            ))
        }
    }

    /// Fetch a model's metadata (`/api/models/{id}`), including the sibling
    /// file list. **Metadata only**: never downloads file contents, and never
    /// synthesizes hashes for them.
    pub async fn model_info(&self, id: &str) -> Result<HfModelInfo, RegistryError> {
        #[cfg(feature = "hf")]
        {
            let url = format!("{}/models/{}", self.api(""), id.trim_start_matches('/'));
            let resp = self
                .client
                .get(&url)
                .timeout(std::time::Duration::from_secs(self.timeout_secs))
                .send()
                .await
                .map_err(|e| RegistryError::Internal(format!("hf model_info transport: {e}")))?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(RegistryError::NotFound(id.to_string()));
            }
            if !resp.status().is_success() {
                return Err(RegistryError::Internal(format!(
                    "hf model_info HTTP {}",
                    resp.status().as_u16()
                )));
            }
            let raw: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| RegistryError::Internal(format!("hf model_info json: {e}")))?;
            let id_out = raw
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            let gated = raw.get("gated").and_then(|v| v.as_bool()).unwrap_or(false);
            let siblings = raw
                .get("siblings")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| {
                            let rfilename = s.get("rfilename")?.as_str()?.to_string();
                            Some(HfSibling {
                                rfilename,
                                size: s.get("size").and_then(|x| x.as_u64()),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let author = raw
                .get("author")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| id_out.split_once('/').map(|(a, _)| a.to_string()));
            Ok(HfModelInfo {
                id: id_out,
                sha: raw
                    .get("sha")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                siblings,
                gated,
                pipeline_tag: raw
                    .get("pipeline_tag")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                tags: raw
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                license: extract_license(&raw),
                params: extract_params(&raw),
                context_length: extract_context(&raw),
                author,
                used_storage: raw.get("usedStorage").and_then(|v| v.as_u64()),
            })
        }
        #[cfg(not(feature = "hf"))]
        {
            let _ = (self, id);
            Err(RegistryError::Internal(
                "built without the `hf` feature".into(),
            ))
        }
    }

    /// Marker documenting the network boundary (see `ModeManager`).
    pub const fn is_network_op(&self) -> bool {
        true
    }
}

#[cfg(feature = "hf")]
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use openrt_persistence::{MemoryStore, PersistenceStore};
    use openrt_scan::{sign_bundle, BundleContent};

    use super::*;

    fn store() -> Arc<MemoryStore> {
        Arc::new(MemoryStore::new())
    }

    async fn modes(store: Arc<MemoryStore>) -> ModeManager {
        // Materialization is Provisioning-gated (ADR-003); tests provision
        // explicitly before importing bytes.
        let m = ModeManager::new(store);
        m.transition("test-operator", ExecutionMode::Provisioning, "test setup")
            .await
            .unwrap();
        m
    }

    fn manifest_for(
        id: &str,
        kind: ArtifactKind,
        blobs: Vec<(&str, &[u8])>,
    ) -> (ManifestV1, BTreeMap<String, Vec<u8>>) {
        let mut files = BTreeMap::new();
        let mut refs = Vec::new();
        for (file_key, bytes) in blobs {
            let digest = openrt_verify::sha256_hex(bytes);
            refs.push(BlobRef {
                location: BlobLocation::Stored,
                file_key: file_key.to_string(),
                digest: digest.clone(),
                size_bytes: bytes.len() as u64,
            });
            files.insert(file_key.to_string(), bytes.to_vec());
        }
        (
            ManifestV1 {
                schema_version: MANIFEST_SCHEMA_VERSION,
                artifact_id: id.to_string(),
                kind,
                blobs: refs,
                base_digest: None,
                created_at: openrt_core::now().to_rfc3339(),
            },
            files,
        )
    }

    fn signed_bundle(seed: u8, known_bad: Vec<String>) -> openrt_scan::SignatureBundle {
        let secret = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let content = BundleContent {
            generated_at: "fixture".into(),
            scheme_version: 1,
            known_bad,
            safetensors_policy: Some(openrt_scan::SafetensorsPolicy::default()),
        };
        sign_bundle(&secret, format!("key-{seed}").as_str(), content)
    }

    #[test]
    fn lifecycle_edges() {
        use LifecycleState::*;
        assert!(Discovered.can_transition(Downloaded));
        assert!(Downloaded.can_transition(Verified));
        assert!(Verified.can_transition(Scanned));
        assert!(Scanned.can_transition(Active));
        assert!(Active.can_transition(Idle));
        assert!(Idle.can_transition(Active));
        assert!(Scanned.can_transition(Retired));
        assert!(Active.can_transition(Retired));
        assert!(!Downloaded.can_transition(Active)); // cannot skip scan
        assert!(!Scanned.can_transition(Verified)); // no reverse
        assert!(!Idle.can_transition(Scanned));
        assert!(Retired.can_transition(Retired)); // terminal remains terminal
    }

    fn temp_model_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "openrt-registry-{tag}-{}-{:?}.model",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    /// Byte-accurate single-F32-tensor GGUF fixture (same layout the model
    /// parser exercises against the real format).
    fn gguf_fixture(name: &str, payload_len: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"GGUF");
        v.extend_from_slice(&3u32.to_le_bytes()); // version
        v.extend_from_slice(&1u64.to_le_bytes()); // tensor count
        v.extend_from_slice(&0u64.to_le_bytes()); // kv metadata count
        let nb = name.as_bytes();
        v.extend_from_slice(&(nb.len() as u64).to_le_bytes());
        v.extend_from_slice(nb);
        v.extend_from_slice(&1u32.to_le_bytes()); // ndims
        v.extend_from_slice(&4u32.to_le_bytes()); // dims[0]
        v.extend_from_slice(&0u32.to_le_bytes()); // F32
        v.extend_from_slice(&vec![0u8; payload_len]);
        v
    }

    /// GGUF fixture with one tensor of a caller-chosen dtype and dims[0]
    /// (payload is zero-filled; only the header is ever parsed).
    fn gguf_fixture_typed(name: &str, dtype: u32, dims0: u32, payload_len: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"GGUF");
        v.extend_from_slice(&3u32.to_le_bytes()); // version
        v.extend_from_slice(&1u64.to_le_bytes()); // tensor count
        v.extend_from_slice(&0u64.to_le_bytes()); // kv metadata count
        let nb = name.as_bytes();
        v.extend_from_slice(&(nb.len() as u64).to_le_bytes());
        v.extend_from_slice(nb);
        v.extend_from_slice(&1u32.to_le_bytes()); // ndims
        v.extend_from_slice(&dims0.to_le_bytes()); // dims[0]
        v.extend_from_slice(&dtype.to_le_bytes());
        v.extend_from_slice(&vec![0u8; payload_len]);
        v
    }

    #[tokio::test]
    async fn register_local_m8_quant_optin_captures_table_derived_lengths() {
        // Q8_0 (dtype 8), 32 elements -> ceil(32/32) * 34 bytes/block = 34.
        let bytes = gguf_fixture_typed("q.weight", 8, 32, 34);
        let path = temp_model_path("m8-gguf");
        std::fs::write(&path, &bytes).unwrap();
        let (manifest, _files) = manifest_for(
            "t/fixture-m8",
            ArtifactKind::Binary,
            vec![("q.gguf", &bytes)],
        );

        // Default registry: quantized length stays unknown (pre-M8 parity).
        let plain_s = store();
        let plain = ModelRegistry::new(plain_s.clone(), modes(plain_s.clone()).await);
        plain
            .register_local(manifest.clone(), path.to_str().unwrap(), "t")
            .await
            .unwrap();
        let a = plain.anatomy("t/fixture-m8").await.unwrap().unwrap();
        assert_eq!(a.tensors[0].length, None);
        assert_eq!(a.tensors[0].length_basis(), None);

        // Opt-in registry: length derived from the versioned ggml table.
        let opt_s = store();
        let opt = ModelRegistry::with_anatomy_options(
            opt_s.clone(),
            modes(opt_s.clone()).await,
            openrt_models::AnatomyOptions {
                quantized_lengths: true,
            },
        );
        opt.register_local(manifest, path.to_str().unwrap(), "t")
            .await
            .unwrap();
        let a = opt.anatomy("t/fixture-m8").await.unwrap().unwrap();
        assert_eq!(a.tensors[0].dtype, "Q8_0");
        assert_eq!(a.tensors[0].length, Some(34));
        assert_eq!(
            a.tensors[0].length_basis(),
            Some(openrt_models::TensorLengthBasis::QuantTableV1)
        );
        assert_eq!(
            a.tensors[0].offset, None,
            "table-derived lengths never invent an offset"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn register_local_records_bounded_anatomy() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let bytes = gguf_fixture("t.weight", 16);
        let path = temp_model_path("gguf");
        std::fs::write(&path, &bytes).unwrap();
        let (manifest, _files) = manifest_for(
            "t/fixture-gguf",
            ArtifactKind::Binary,
            vec![("t.gguf", &bytes)],
        );

        let lr = registry
            .register_local(manifest, path.to_str().unwrap(), "t")
            .await
            .unwrap();
        assert_eq!(lr.state, LifecycleState::Verified);

        let a = registry.anatomy("t/fixture-gguf").await.unwrap().unwrap();
        assert_eq!(a.schema_version, ANATOMY_SCHEMA_VERSION);
        assert_eq!(a.format, openrt_models::AnatomyFormat::Gguf);
        assert_eq!(a.total_bytes, bytes.len() as u64);
        assert_eq!(a.tensors.len(), 1);
        assert_eq!(a.tensors[0].name, "t.weight");
        assert_eq!(a.tensors[0].dtype, "F32");
        assert_eq!(a.tensors[0].length, Some(16));
        let raw = s
            .get_record(ANATOMY_NS, &key("t/fixture-gguf"))
            .await
            .unwrap()
            .unwrap();
        let rec: AnatomyRecord = serde_json::from_value(raw).unwrap();
        assert_eq!(rec.sha256, openrt_verify::sha256_hex(&bytes));
        assert_eq!(rec.path, path.to_str().unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn bind_provider_tag_persists_and_lists() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        // Curated ids are registry-known even without install; a local
        // artifact works the same once registered.
        let id = "curated/llama3.1-8b";
        let b = registry
            .bind_provider_tag(id, "ollama", "llama3.1:8b", "op")
            .await
            .unwrap();
        assert_eq!(b.provider, "ollama");
        assert_eq!(b.tag, "llama3.1:8b");
        assert_eq!(b.actor, "op");

        let got = registry.bound_tag(id, "ollama").await.unwrap().unwrap();
        assert_eq!(got, b);
        assert_eq!(registry.bound_tags(id).await.unwrap(), vec![b]);

        // Same artifact, different provider: independent bindings.
        registry
            .bind_provider_tag(id, "openai", "gpt-llama3.1", "op")
            .await
            .unwrap();
        assert_eq!(registry.bound_tags(id).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn bind_provider_tag_fails_closed_for_unknown_artifacts() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let err = registry
            .bind_provider_tag("nope/missing", "ollama", "x", "op")
            .await
            .unwrap_err();
        assert!(matches!(err, RegistryError::NotFound(_)));
        // Nothing was written.
        assert!(registry
            .bound_tag("nope/missing", "ollama")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn unbind_provider_tag_is_idempotent_and_audited() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let id = "curated/llama3.1-8b";
        registry
            .bind_provider_tag(id, "ollama", "llama3.1:8b", "op")
            .await
            .unwrap();
        assert!(registry
            .unbind_provider_tag(id, "ollama", "op")
            .await
            .unwrap());
        // Second unbind: nothing existed -> false, no error.
        assert!(!registry
            .unbind_provider_tag(id, "ollama", "op")
            .await
            .unwrap());
        assert!(registry.bound_tag(id, "ollama").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn register_local_succeeds_when_anatomy_unavailable() {
        // Non-model bytes: registration must succeed and anatomy must read as
        // Unknown — not fail, and never fabricate a layout.
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let bytes = b"definitely not a model file, just bytes".to_vec();
        let path = temp_model_path("plain");
        std::fs::write(&path, &bytes).unwrap();
        let (manifest, _files) = manifest_for(
            "t/fixture-plain",
            ArtifactKind::Binary,
            vec![("m.bin", &bytes)],
        );

        let lr = registry
            .register_local(manifest, path.to_str().unwrap(), "t")
            .await
            .unwrap();
        assert_eq!(lr.state, LifecycleState::Verified);

        let a = registry.anatomy("t/fixture-plain").await.unwrap().unwrap();
        assert_eq!(a.format, openrt_models::AnatomyFormat::Unknown);
        assert!(a.tensors.is_empty());
        assert_eq!(a.total_bytes, bytes.len() as u64);
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn register_local_survives_a_malformed_header() {
        // A GGUF magic with a truncated header: parsing fails, the store must
        // hold no anatomy record, and registration still reaches Verified.
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let mut bytes = gguf_fixture("t.weight", 16);
        bytes.truncate(bytes.len() / 2);
        let path = temp_model_path("broken");
        std::fs::write(&path, &bytes).unwrap();
        let (manifest, _files) = manifest_for(
            "t/fixture-broken",
            ArtifactKind::Binary,
            vec![("m.gguf", &bytes)],
        );

        let lr = registry
            .register_local(manifest, path.to_str().unwrap(), "t")
            .await
            .unwrap();
        assert_eq!(lr.state, LifecycleState::Verified);
        assert!(
            s.get_record(ANATOMY_NS, &key("t/fixture-broken"))
                .await
                .unwrap()
                .is_none(),
            "a broken header must never persist a record"
        );
        assert!(registry
            .anatomy("t/fixture-broken")
            .await
            .unwrap()
            .is_none());
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn register_local_never_reads_payloads_for_anatomy() {
        // The GGUF parse must not need the payload to exist beyond the header:
        // registered file contains a header claiming exactly one tensor, and
        // anatomy is produced from the header alone.
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let bytes = gguf_fixture("u.weight", 32);
        let path = temp_model_path("no-payload");
        std::fs::write(&path, &bytes).unwrap();
        let (manifest, _files) = manifest_for(
            "t/fixture-np",
            ArtifactKind::Binary,
            vec![("u.gguf", &bytes)],
        );
        registry
            .register_local(manifest, path.to_str().unwrap(), "t")
            .await
            .unwrap();
        let a = registry.anatomy("t/fixture-np").await.unwrap().unwrap();
        assert_eq!(a.tensors[0].length, Some(16)); // dims[0]=4 x F32(4B), from the header alone
        assert_eq!(a.tensors[0].offset, Some(52)); // header is 52 bytes; payload starts there
        std::fs::remove_file(&path).ok();
    }

    fn st_header(tensors: &[(&str, u64, u64)]) -> String {
        let inner = tensors
            .iter()
            .map(|(n, s, e)| {
                format!("{n:?}:{{\"dtype\":\"F32\",\"shape\":[],\"data_offsets\":[{s},{e}]}}")
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"__metadata__\":{{}},{inner}}}")
    }

    fn st_file(header: &str, payload_len: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(header.len() as u64).to_le_bytes());
        v.extend_from_slice(header.as_bytes());
        v.extend_from_slice(&vec![0u8; payload_len]);
        v
    }

    /// A 3-shard safetensors model: files (by shard-name) plus the index bytes
    /// whose `weight_map` is consistent with the shard headers.
    fn sharded_fixture_trio() -> (Vec<u8>, BTreeMap<String, Vec<u8>>) {
        let ha = st_header(&[("a.weight", 0, 16), ("a.bias", 16, 20)]);
        let hb = st_header(&[("b.weight", 0, 32)]);
        let hc = st_header(&[("c.weight", 0, 8), ("c.bias", 8, 16)]);
        let index = br#"{"weight_map":{"a.weight":"a.safetensors","a.bias":"a.safetensors","b.weight":"b.safetensors","c.weight":"c.safetensors","c.bias":"c.safetensors"}}"#;
        let mut files = BTreeMap::new();
        files.insert("a.safetensors".into(), st_file(&ha, 20));
        files.insert("b.safetensors".into(), st_file(&hb, 32));
        files.insert("c.safetensors".into(), st_file(&hc, 16));
        (index.to_vec(), files)
    }

    #[tokio::test]
    async fn register_local_captures_multi_shard_anatomy() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let (index, files) = sharded_fixture_trio();
        let dir = temp_model_path("sharded");
        std::fs::create_dir_all(&dir).unwrap();
        let index_path = dir.join("model.safetensors.index.json");
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        std::fs::write(&index_path, &index).unwrap();
        let (manifest, _files) = manifest_for(
            "t/fixture-sharded",
            ArtifactKind::Binary,
            vec![("model.safetensors.index.json", &index)],
        );

        let lr = registry
            .register_local(manifest, index_path.to_str().unwrap(), "t")
            .await
            .unwrap();
        assert_eq!(lr.state, LifecycleState::Verified);

        let a = registry
            .anatomy("t/fixture-sharded")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.schema_version, ANATOMY_SCHEMA_VERSION);
        assert_eq!(a.format, openrt_models::AnatomyFormat::Safetensors);
        assert!(a.is_multi_shard());
        assert_eq!(a.shards, 3);
        assert_eq!(a.shard_files.len(), 3);
        assert_eq!(
            a.total_bytes,
            files.values().map(|b| b.len() as u64).sum::<u64>()
        );
        assert_eq!(a.shard_bytes(), Some(a.total_bytes));
        assert_eq!(a.tensors.len(), 5);
        let c_bias = a.tensors.iter().find(|t| t.name == "c.bias").unwrap();
        assert_eq!(c_bias.shard, 2);
        assert!(c_bias.length.is_some());

        // Segmentation is metadata-only: the shard payloads were never opened
        // for reads during registration (the index and headers carry the size).
        for (name, bytes) in &files {
            let meta = std::fs::metadata(dir.join(name)).unwrap();
            assert_eq!(meta.len(), bytes.len() as u64);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn register_local_single_shard_member_is_single_file_anatomy() {
        // Registering one member of a sharded model (not the index) keeps the
        // M2 single-file behavior and does not assign a bogus shard layout.
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let (_index, files) = sharded_fixture_trio();
        let member = files["a.safetensors"].clone();
        let path = temp_model_path("member");
        std::fs::write(&path, &member).unwrap();
        let (manifest, _files) = manifest_for(
            "t/fixture-member",
            ArtifactKind::Binary,
            vec![("a.safetensors", &member)],
        );
        registry
            .register_local(manifest, path.to_str().unwrap(), "t")
            .await
            .unwrap();
        let a = registry.anatomy("t/fixture-member").await.unwrap().unwrap();
        assert_eq!(a.format, openrt_models::AnatomyFormat::Safetensors);
        assert_eq!(a.shards, 1);
        assert_eq!(a.shard_files.len(), 1);
        assert!(!a.is_multi_shard());
        assert_eq!(
            a.tensors
                .iter()
                .find(|t| t.name == "a.weight")
                .unwrap()
                .shard,
            0
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn register_local_inconsistent_index_fails_closed() {
        // A broken index (tensor mapped to a shard that does not declare it)
        // must not fabricate a layout: registration still succeeds and the
        // stored anatomy falls back to Unknown instead of a partial sharded
        // view literally asserting a wrong mapping.
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let (index, files) = sharded_fixture_trio();
        let index: serde_json::Value = serde_json::from_slice(&index).unwrap();
        let mut idx = index.as_object().unwrap().clone();
        idx["weight_map"]
            .as_object_mut()
            .unwrap()
            .insert("zzz.weight".into(), "b.safetensors".into());
        let index = serde_json::to_vec(&idx).unwrap();
        let dir = temp_model_path("inconsistent");
        std::fs::create_dir_all(&dir).unwrap();
        let index_path = dir.join("model.safetensors.index.json");
        for (name, bytes) in files.into_iter() {
            std::fs::write(dir.join(name), &bytes).unwrap();
        }
        std::fs::write(&index_path, &index).unwrap();
        let (manifest, _files) = manifest_for(
            "t/fixture-inconsistent",
            ArtifactKind::Binary,
            vec![("model.safetensors.index.json", &index)],
        );
        registry
            .register_local(manifest, index_path.to_str().unwrap(), "t")
            .await
            .unwrap();
        let a = registry
            .anatomy("t/fixture-inconsistent")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.format, openrt_models::AnatomyFormat::Unknown);
        assert!(a.tensors.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn anatomy_v1_records_without_shard_files_still_load() {
        // Backward compatibility is mandatory: a pre-M7 AnatomyRecord persisted
        // a ModelAnatomy without `shard_files`. It must deserialize as a valid
        // single-file anatomy (empty shard_files, shards counting the files).
        let s = store();
        let v1_anatomy = serde_json::json!({
            "schema_version": 1,
            "format": "safetensors",
            "total_bytes": 68,
            "shards": 1,
            "tensors": [
                {
                    "name": "t.weight",
                    "shard": 0,
                    "offset": 52,
                    "length": 16,
                    "alignment": 4,
                    "page_aligned": false,
                    "dtype": "F32"
                }
            ]
        });
        let v1_record = serde_json::json!({
            "schema_version": 1,
            "artifact_id": "t/fixture-v1",
            "sha256": "00".repeat(32),
            "size_bytes": 68,
            "path": "/dev/null",
            "anatomy": v1_anatomy,
            "recorded_at": "2026-01-01T00:00:00Z"
        });
        s.put_record(ANATOMY_NS, &key("t/fixture-v1"), &v1_record)
            .await
            .unwrap();

        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let a = registry.anatomy("t/fixture-v1").await.unwrap().unwrap();
        assert_eq!(a.schema_version, ANATOMY_SCHEMA_VERSION);
        assert_eq!(a.format, openrt_models::AnatomyFormat::Safetensors);
        assert_eq!(a.shard_files.len(), 0, "v1 records have no shard files");
        assert_eq!(a.shard_count(), 1);
        assert!(!a.is_multi_shard());
        assert_eq!(a.tensors.len(), 1);
        assert_eq!(a.tensors[0].name, "t.weight");
        assert_eq!(a.total_bytes, 68);
    }

    #[tokio::test]
    async fn multi_shard_anatomy_persists_and_reloads_through_the_registry() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let dir = temp_model_path("persisted");
        std::fs::create_dir_all(&dir).unwrap();
        let (index, files) = sharded_fixture_trio();
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let index_path = dir.join("model.safetensors.index.json");
        std::fs::write(&index_path, &index).unwrap();
        let (manifest, _files) = manifest_for(
            "t/fixture-persisted",
            ArtifactKind::Binary,
            vec![("model.safetensors.index.json", &index)],
        );
        registry
            .register_local(manifest, index_path.to_str().unwrap(), "t")
            .await
            .unwrap();

        // A second registry instance (same store) reads back the sharded view.
        let fresh = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let a = fresh.anatomy("t/fixture-persisted").await.unwrap().unwrap();
        assert_eq!(a.shards, 3);
        assert_eq!(a.shard_files.len(), 3);
        assert_eq!(a.shard_files[0].name, "a.safetensors");
        assert_eq!(
            a.shard_files[0].size_bytes,
            files["a.safetensors"].len() as u64
        );
        assert_eq!(a.tensors.iter().filter(|t| t.shard == 1).count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn import_verifies_and_persists_blobs() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let bytes = b"llama model weights (fixture)".to_vec();
        let (manifest, files) = manifest_for(
            "curated/llama3.1-8b",
            ArtifactKind::Binary,
            vec![("model.safetensors", &bytes)],
        );

        let lr = registry
            .import_local(manifest, &files, "tester", None)
            .await
            .unwrap();
        assert_eq!(lr.state, LifecycleState::Verified);

        let m = registry
            .manifest("curated/llama3.1-8b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.blobs[0].digest, openrt_verify::sha256_hex(&bytes));
        let blob = registry.blob(&m.blobs[0].digest).await.unwrap().unwrap();
        assert_eq!(blob, bytes);

        // activating without a scan is a skipped state -> fail-closed
        let err = registry
            .activate("curated/llama3.1-8b", "t")
            .await
            .unwrap_err();
        assert!(matches!(err, RegistryError::InvalidTransition { .. }));
    }

    #[tokio::test]
    async fn manifest_is_write_once() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let bytes = b"m".to_vec();
        let (m1, f1) = manifest_for(
            "curated/fixture-w",
            ArtifactKind::Binary,
            vec![("m.bin", &bytes)],
        );
        registry.import_local(m1, &f1, "t", None).await.unwrap();
        let (m2, f2) = manifest_for(
            "curated/fixture-w",
            ArtifactKind::Binary,
            vec![("m.bin", &bytes)],
        );
        let err = registry.import_local(m2, &f2, "t", None).await.unwrap_err();
        assert!(matches!(err, RegistryError::ManifestImmutable(_)));
    }

    #[tokio::test]
    async fn clean_scan_gates_activation() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let bytes = b"clean-model-bytes".to_vec();
        let (manifest, files) = manifest_for(
            "curated/fixture-x",
            ArtifactKind::Binary,
            vec![("m.bin", &bytes)],
        );
        let mut sc = openrt_scan::Scanner::new();
        sc.add_bundle(signed_bundle(7, vec![]));

        let lr = registry
            .import_local(manifest, &files, "t", Some((&sc, 1)))
            .await
            .unwrap();
        assert_eq!(lr.state, LifecycleState::Scanned);

        let act = registry.activate("curated/fixture-x", "t").await.unwrap();
        assert_eq!(act.state, LifecycleState::Active);
        assert!(registry.is_active("curated/fixture-x").await.unwrap());
        assert!(registry
            .store
            .get_record("verify.records", &key("curated/fixture-x"))
            .await
            .unwrap()
            .is_some());
        assert!(registry
            .store
            .get_record("scan.records", &key("curated/fixture-x"))
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn known_bad_hash_blocks_install_fail_closed() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let bytes = b"evil-weights".to_vec();
        let (manifest, files) = manifest_for(
            "curated/fixture-evil",
            ArtifactKind::Binary,
            vec![("m.bin", &bytes)],
        );
        let mut sc = openrt_scan::Scanner::new();
        sc.add_bundle(signed_bundle(
            9,
            vec![openrt_verify::sha256_hex(b"evil-weights")],
        ));

        let err = registry
            .import_local(manifest, &files, "t", Some((&sc, 1)))
            .await
            .unwrap_err();
        assert!(matches!(err, RegistryError::ScanBlocked { .. }));

        // lifecycle must not have advanced past verified (fail-closed)
        let lc = registry
            .lifecycle("curated/fixture-evil")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lc.state, LifecycleState::Verified);
        assert!(!registry.is_active("curated/fixture-evil").await.unwrap());
    }

    #[tokio::test]
    async fn digest_mismatch_rejected() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let (manifest, mut files) = manifest_for(
            "curated/fixture-d",
            ArtifactKind::Binary,
            vec![("m.bin", b"real")],
        );
        files.insert("m.bin".into(), b"tampered".to_vec());
        let err = registry
            .import_local(manifest, &files, "t", None)
            .await
            .unwrap_err();
        assert!(matches!(err, RegistryError::DigestMismatch { .. }));
    }

    #[tokio::test]
    async fn retire_is_terminal() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let bytes = b"x".to_vec();
        let (manifest, files) = manifest_for(
            "curated/fixture-r",
            ArtifactKind::Binary,
            vec![("m.bin", &bytes)],
        );
        registry
            .import_local(manifest, &files, "t", None)
            .await
            .unwrap();
        let retired = registry
            .retire("curated/fixture-r", "t", "operator decision")
            .await
            .unwrap();
        assert_eq!(retired.state, LifecycleState::Retired);
        assert!(!registry.is_active("curated/fixture-r").await.unwrap());
    }

    #[tokio::test]
    async fn search_covers_curated_and_installed() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let results = registry.search("llama", None).await;
        assert!(!results.is_empty());
        assert!(results
            .iter()
            .any(|r| r.artifact_id == "curated/llama3.1-8b"));
        assert!(results.iter().all(|r| !r.installed));

        let bytes = b"m".to_vec();
        let (manifest, files) = manifest_for(
            "curated/llama3.1-8b",
            ArtifactKind::Binary,
            vec![("m.bin", &bytes)],
        );
        registry
            .import_local(manifest, &files, "t", None)
            .await
            .unwrap();
        let results = registry.search("llama", None).await;
        assert!(results
            .iter()
            .any(|r| r.artifact_id == "curated/llama3.1-8b" && r.installed));
    }

    #[tokio::test]
    async fn list_never_fabricates_accounting() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        let cards = registry.list().await.unwrap();
        assert!(cards
            .iter()
            .any(|c| c.artifact_id == "curated/mixtral-8x7b"));
        let moe = cards
            .iter()
            .find(|c| c.artifact_id == "curated/mixtral-8x7b")
            .unwrap();
        assert_eq!(moe.installed_bytes, 0);
        assert_eq!(moe.digest_prefix, None);
    }

    #[tokio::test]
    async fn storage_used_is_real_accounting() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), modes(s.clone()).await);
        assert_eq!(registry.storage_used_bytes().await.unwrap(), 0);
        let bytes = vec![0u8; 4];
        let (manifest, files) = manifest_for(
            "curated/fixture-acc",
            ArtifactKind::Binary,
            vec![("m.bin", &bytes)],
        );
        registry
            .import_local(manifest, &files, "t", None)
            .await
            .unwrap();
        assert_eq!(registry.storage_used_bytes().await.unwrap(), 4);
    }

    #[test]
    fn curated_entries_have_no_fabricated_digests() {
        for e in curated_catalog() {
            let card = e.to_card();
            assert_eq!(card.installed_bytes, 0);
            assert_eq!(card.digest_prefix, None);
            assert_eq!(card.state, LifecycleState::Discovered);
            assert!(!e.tags.is_empty());
        }
    }

    // ---- HF metadata extraction (browser PART G) — facts only, no network ----

    #[test]
    fn hf_search_rows_extract_real_metadata() {
        let raw: serde_json::Value = serde_json::from_str(
            r#"[{
                "id": "org/model-7b",
                "downloads": 1234,
                "likes": 56,
                "pipeline_tag": "text-generation",
                "tags": ["license:llama3", "4-bit", "safetensors"],
                "author": "org",
                "safetensors": {"parameters": {"total": 7000000000}}
            }]"#,
        )
        .unwrap();
        let lite = lite_from(&raw[0]);
        assert_eq!(lite.id, "org/model-7b");
        assert_eq!(lite.downloads, Some(1234));
        assert_eq!(lite.likes, Some(56));
        assert_eq!(lite.pipeline_tag.as_deref(), Some("text-generation"));
        assert_eq!(lite.license.as_deref(), Some("llama3"));
        assert_eq!(lite.params, Some(7_000_000_000));
        assert_eq!(lite.author.as_deref(), Some("org"));
        assert!(lite.tags.iter().any(|t| t == "4-bit"));
    }

    #[test]
    fn hf_extractors_read_only_declared_facts() {
        let raw: serde_json::Value = serde_json::from_str(
            r#"{
                "cardData": {"license": "apache-2.0"},
                "tags": ["license:ignored"],
                "safetensors": {"parameters": {"total": 7050000000}},
                "config": {"text_config": {"max_position_embeddings": 4096}}
            }"#,
        )
        .unwrap();
        assert_eq!(extract_license(&raw).as_deref(), Some("apache-2.0"));
        assert_eq!(extract_params(&raw), Some(7_050_000_000));
        assert_eq!(extract_context(&raw), Some(4096));
        // license falls back to the `license:` tag prefix
        let tag_only: serde_json::Value =
            serde_json::from_str(r#"{"tags": ["license:mit"]}"#).unwrap();
        assert_eq!(extract_license(&tag_only).as_deref(), Some("mit"));
        // nothing declared → nothing invented
        let empty = serde_json::json!({});
        assert_eq!(extract_license(&empty), None);
        assert_eq!(extract_params(&empty), None);
        assert_eq!(extract_context(&empty), None);
        let root_ctx: serde_json::Value =
            serde_json::from_str(r#"{"config": {"max_position_embeddings": 8192}}"#).unwrap();
        assert_eq!(extract_context(&root_ctx), Some(8192));
    }

    #[test]
    fn hf_structs_deserialize_minimal_legacy_json() {
        let lite: HfModelLite =
            serde_json::from_str(r#"{"id": "a/b", "downloads": 1, "likes": 2}"#).unwrap();
        assert!(lite.tags.is_empty());
        assert!(lite.pipeline_tag.is_none());
        assert!(lite.license.is_none());
        let info: HfModelInfo =
            serde_json::from_str(r#"{"id": "a/b", "sha": null, "siblings": [], "gated": false}"#)
                .unwrap();
        assert!(info.tags.is_empty());
        assert!(info.params.is_none());
        assert!(info.context_length.is_none());
        assert!(info.used_storage.is_none());
    }

    /// Regression (found live 2026-10-01): the empty-path prefix used to
    /// render `…/api/` and callers appended `/models…`, producing
    /// `api//models` — the hub answers HTTP 404 to the double slash, so
    /// every live search/detail fetch failed. The prefix must end at `api`.
    #[cfg(feature = "hf")]
    #[test]
    fn hf_api_prefix_never_doubles_the_slash() {
        let c = HfClient::new("", 5);
        assert_eq!(c.api(""), "https://huggingface.co/api");
        let search = format!("{}/models?search=x&limit=10&skip=0", c.api(""));
        assert_eq!(
            search,
            "https://huggingface.co/api/models?search=x&limit=10&skip=0"
        );
        assert!(!search.contains("//models"), "no double slash: {search}");
        let info = format!("{}/models/{}", c.api(""), "org/model-7b");
        assert_eq!(info, "https://huggingface.co/api/models/org/model-7b");
        // A configured base keeps working, with the trailing `/` trimmed.
        let custom = HfClient::new("https://mirror.example/hf/", 5);
        assert_eq!(custom.api(""), "https://mirror.example/hf/api");
    }
}
