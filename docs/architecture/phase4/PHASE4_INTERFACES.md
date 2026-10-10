# Phase 4 — Interface Contract

This file is the single source of truth for new public types/namespaces/CLI surfaces. Everything
below is implemented; anything not listed here must not be invented ad-hoc. All namespaces use
`validate_ns_key`-safe keys (`[a-z0-9._-]`; IDs containing other bytes are `encode_safe_key`-encoded).

## Store namespaces (one store, Phase 4 slices)

| Namespace | Owns | Written by |
|---|---|---|
| `corpus.documents` | `DocumentRecord` per version + `DocumentMeta` pointer | `openrt-ingest` |
| `corpus.chunks` | chunk records for indexed docs | `openrt-knowledge` |
| `kb.meta` | `KnowledgeBase` records | `openrt-knowledge` |
| `kb.membership` | membership + index-state records | `openrt-knowledge` |
| `knowledge.vectors` | record + blob halves of vector index | `openrt-vectorindex` |
| `datasets.meta` | `DatasetRecord` | `openrt-dataset` |
| `datasets.entries` | per-version entry lists (JSONL line digests) | `openrt-dataset` |
| `train.runs` | `TrainingRun` state machine | `openrt-train` |
| `train.checkpoints` | checkpoint metadata | `openrt-train` |
| `eval.results` | `EvalResult` | `openrt-train` |
| `registry.adapters` | derived-artifact provenance (via registry) | `openrt-registry` |

## openrt-ingest

* `pub enum FormatKind { Text, Markdown, Csv, Json, Pdf, Png, Jpeg, Other }`
* `pub struct DetectReport { kind, confidence, declared_ext, magic_ok, size_bytes }`
* `pub fn detect(bytes: &[u8], declared_ext: Option<&str>) -> DetectReport` — magic-first;
  `Other` never claims support. Never executes anything.
* `pub struct Block { key: String, kind: BlockKind, page: Option<u32>, text: String, table: Option<TableRegion>, source: BlockSource }`
* `pub enum BlockKind { Paragraph, Heading, Bullet, ListItem, Page, Table, ImageRef, SectionDivider, Other }`
* `pub struct NormalizedDocumentV1 { doc_id, version, content_sha256, size_bytes, format, blocks, pages: u32, created_at, extractor, xtra: Provenance }`
* `pub fn normalize_text(data) -> NormalizedDocumentV1` and `normalize_csv`, `normalize_json`,
  `normalize_pdf` (all pure, no IO) — genuine extraction only.
* `pub struct IngestJail { root: PathBuf }` with `fn resolve(&self, p: &Path) -> Result<PathBuf, IngestError>`
  (canonicalization + symlink rejection, SEC-01).
* `pub struct IngestLimits { max_bytes, max_pages, max_blocks, chunk_default }` validated non-trivial.
* `pub struct DocCorpus` over `Arc<dyn PersistenceStore>`: `ingest_path`, `ingest_bytes`,
  `list`, `inspect(id)`, `versions(id)`, `status(id)`, `remove(id, verify_first)`, `get`.
  Ingestion state machine `Queued→Extracting→Validating→Stored→Failed|Duplicate`.
* Capability probes (honest): `pub fn ocr_available() -> Option<OcrCapability>` (None here),
  `pub fn vlm_available() ...`, `pub fn audio_available() ...`; extraction provenance records
  `method: "ocr:v(N)|unavailable"` etc. — never fabricated text.
* Chunking: `pub struct ChunkConfig { strategy, max_chars, overlap_chars, min_chars, table_aware }`;
  `pub fn chunk_normalized(doc: &NormalizedDocumentV1, cfg: &ChunkConfig) -> Result<Vec<Chunk>, ChunkError>`.
  `pub struct Chunk { chunk_id, doc_id, document_version, block_keys, page, text, sha256, index }`
  — chunk ids stable and deterministic (derived from doc id + version + block keys + order).
  `cfg.validate()` rejects overlap >= max, min_chars > max_chars, max_chars == 0.

## openrt-dataset

* `pub enum DatasetSchema { InstructionResponse, Conversation, Preference }` with per-schema
  record validation (functions, no required keys invented).
* `pub struct DatasetRecord { dataset_id, schema, version, sha256, record_count, splits: {train,val,test}, imported_at }`
* `pub fn validate_jsonl(bytes) -> ValidationReport { total, ok, skipped, sample_errors(truncated), schema }`
  — never echoes full record content into logs/errors.
* `pub struct DatasetStore` over store: `import`, `list`, `inspect(id)`, `preview(id, n)`,
  `export(id, PathBuf)` (safe-path checked), `delete(id)`. Import rejects executable
  extensions + magic; no script execution, ever.

## openrt-embeddings

* `pub struct Embedding { id, dims, values: Vec<f32>, sha256 }`
* `pub trait EmbeddingProvider: Send + Sync { fn dims(&self) -> usize; fn embed(&self, texts: &[&str]) -> Result<Vec<Embedding>, EmbedError>; fn describe(&self) -> &'static str; }`
* Validation `pub fn validate_embedding(e) -> Result<(), EmbedError>` — finite, no NaN, exact dims.
* `pub struct DeterministicHashEmbedder` — offline, deterministic, collision-free-enough for
  CI/RAG-of-record, honest `describe()`; used only where `Config` matches a local embedder.

## openrt-vectorindex

* `pub struct VectorRecord { id, doc_id, chunk_id, page, text_sha256, vector: Embedding, added_at }`
* `pub struct VectorIndex` over store (`knowledge.vectors`): `insert`, `insert_batch`, `delete`,
  `query(vec, top_k, threshold) -> Vec<(Score, VectorRecord)>` (bounded exact baseline),
  `persist`, `reload`, `integrity() -> IntegrityReport`, `rebuild_from(entries)`, `len`, `is_ready`.
  Persist is atomic via `put_blob` under index key; reload verifies hash before that key's lookups.
* Scores are **cosine similarity**, always real, computed by `query`; never fabricated.

## openrt-knowledge

* `pub enum IndexState { Empty, Pending, Indexing, Ready, Stale, Failed, Rebuilding, Cancelled }`
* `pub struct KnowledgeBase { kb_id, name, created_at, state, index_config: ChunkConfig, embedder_dims: usize, doc_count }`
* `pub struct KnowledgeBaseStore` over store: `create`, `list`, `inspect`, `delete` (recovers members),
  `add(kb_id, doc_id)`, `remove(kb_id, doc_id)`, `reindex(kb_id, CancellationToken-ish)`,
  `search(kb_id, query_text, top_k, threshold) -> Vec<KbHit { rank, score, chunk_id, doc_id, page, citation: Citation }`.
* `pub struct Citation { document_id, document_version, chunk_id, block_keys, page }` — integrity
  `validate(kb_store)` re-resolves every hit against persisted chunks (no fabricated citations).
* RAG: `pub struct RagOracle { store, gen: Box<dyn GenerationBackend> }`;
  `pub trait GenerationBackend: Send + Sync { fn generate(&self, prompt: &RagPrompt) -> Result<String, RagError>; fn describe(&self) -> &'static str; }`.
  `ask(question)` → retrieval under budget → evidence list (dedup by chunk_id, stable ids) →
  `RagPrompt` (includes instruction that the evidence is data, never policy/tools) → response +
  `RagCitationReport { all_valid, invalid: Vec<...>, verified: Vec<...>, unverified: Vec<...>, omitted: usize }`.
  No evidence ⇒ response generated honestly ("no supporting evidence") and report marks citations empty.
* Injection defense is architectural: generation sees evidence as *data*; evidence never changes
  permissions, modes, registry, or dispatches tools (system prompt + permission gate at CLI layer).

## openrt-train

* `pub struct TrainConfig` (schema `train/config-v1`, `#[serde(deny_unknown_fields)]`):
  `model_base_digest: Option<String>`, `method: TrainMethod`, `dataset_digest: Option<String>`,
  `epochs`, `learning_rate`, `batch_size`, `adamw`, `lora_r`, `dpo_beta`, `output_name`,
  `eval_steps`, `max_checkpoints`, `timeout_secs`. `validate(&registry) `checks fields per method.
* `pub enum TrainMethod { Sft, Lora, Qlora, Dpo, Merge, Quantize }` — only genuinely implemented
  ones; absence of a real backend ⇒ explicit `TrainError::NoBackend`.
* `pub trait TrainingBackend: Send + Sync { fn discover() -> Option<Self>...; fn describe() ...; }`
  — capability discovery via scan (returns None in this environment → honest unsupported).
* `pub struct ResourceEstimate { cpu, ram_gb, vram_gb: Option, disk_gb, concurrency, level: EstimateLevel {Estimated|Measured|Unknown} }`
  `pub fn estimate(config) -> ResourceEstimate` (deterministic formulas, honest `Unknown` where no data).
* Job states: `Created | Validating | Preflight | Queued | Running | Checkpointing | Evaluating |
  Completed | Failed | Cancelled | Interrupted | ResumePending` — transitions persisted per op in
  `train.runs` with timestamped journal; progress values only ever written by a real backend.
* `pub struct TrainingRun { run_id, config_sha256, config, state, backend, estimated, measured: Option, started_at, updated_at, cancel_requested }`
* `pub struct TrainingService` over store + audit: `create`, `validate`, `preflight`
  (no side effects), `start` (requires backend or fails `NoBackend`), `list`, `status`,
  `cancel(run_id)` (permission-checked + audited), `resume`→`ResumePending`, `checkpoint_meta`,
  `evaluate` (writes `eval.results` only from real eval), `artifact(run_id)` →
  `ArtifactClaim { digest, bytes, manifest, provenance }` for registration.
* `crate::registration`: `fn register_derived_artifact(registry, claim, actor) -> Result<()>` —
  calls the registry API, verifies, publishes, audits.

## openrt-registry addition

* `pub async fn register_derived(&self, claim: &DerivedClaim, actor: &str) -> Result<DerivedReceipt, RegistryError>`
  `DerivedClaim { name, kind: ArtifactKind, bytes: Vec<u8>, provenance: ProvenanceManifest, scan_bundle: Option<ScanVerdict> }`
  requires Provisioning mode; verifies base digest present+installed; verifies bytes + provenance
  before write; publishes derived `ManifestV1` (base_digest set, class Derived); audit entry;
  on failure nothing is registered.

## EventKind additions (openrt-core)

`DocumentQueued`, `DocumentExtracted`, `DocumentStored`, `DocumentDuplicate`,
`DocumentFailed`, `ChunkingStarted`, `ChunkingCompleted`, `EmbeddingRequested`,
`IndexUpdated`, `IndexRebuildStarted`, `IndexRebuildCompleted`, `IndexIntegrityFailed`,
`RetrievalStarted`, `RetrievalCompleted`, `RagAnswerGenerated`, `RagCitationInvalid`,
`DatasetImported`, `DatasetValidated`, `DatasetExported`, `TrainJobStarted`,
`TrainJobCompleted`, `TrainJobFailed`, `TrainJobCancelled`, `TrainCheckpointWritten`,
`TrainResourcePreflight`, `EvalCompleted`, `ArtifactVerified`, `ArtifactRegistered`.

## CLI (clap groups, exit codes 0 ok / 1 usage|error / 2 permission|mode|policy)

* `openrt docs ingest <path> [--format] [--no-dedup] [--json]`
* `openrt docs list`, `docs inspect <doc-id>`, `docs status <doc-id>`, `docs versions <doc-id>`,
  `docs reindex <doc-id>`, `docs remove <doc-id>`
* `openrt kb create <name>`, `kb list`, `kb inspect <kb>`, `kb add <kb> <doc>`,
  `kb remove <kb> <doc>`, `kb reindex <kb>`, `kb search <kb> <query> [--top-k N] [--threshold F]`,
  `kb ask <kb> <question> [--json]`
* `openrt dataset import <path> [--schema <s>]`, `dataset list`, `dataset inspect <id>`,
  `dataset preview <id> [--n]`, `dataset validate <path>`, `dataset export <id> <dest>`
* `openrt train validate <method> [--config <path>]`, `train preflight <method> [--config]`,
  `train start <method> [--config]`, `train list`, `train status <run>`,
  `train cancel <run>`, `train resume <run>`, `train evaluate <run>`,
  `train artifact <run>`, `train register <run> [--name]`

All mutating ops audit (`actor_user()`); `--json` prints one stable JSON envelope; preflight and
validate never mutate state. Kill-switch/mode gates run before any content read.