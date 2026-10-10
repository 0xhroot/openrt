# Phase 4 — Preflight Inspection (Step Zero)

Status: complete. Date: 2026-09-18.

## 1. Repository state (verified, not assumed)

* Working tree `~/Projects/openrt`, branch `master`, **zero commits** — every
  file is untracked (`git status --short` shows `?? .gitignore ?? .opencode/ ?? Cargo.lock
  ?? Cargo.toml ?? P2_6_FINAL_REPORT.md ?? PHASE2_FINAL_REPORT.md ?? PHASE3_FINAL_REPORT.md
  ?? crates/ ?? docs/`).
* Constraints: never stage/commit/push/reset; never modify `.opencode/`; preserve all
  Phase 1–3 behavior; do not start Phase 5/6.
* Workspace: `resolver = "2"`, 34 members, edition 2021, `rust-version = "1.81"`,
  `[workspace.lints.rust] unsafe_code = "forbid"`. Shared deps include `serde`, `serde_json`,
  `thiserror 2`, `uuid`, `chrono`, `tokio`, `clap 4`, `reqwest` (rustls),
  `sha2`, `hex`, `ed25519-dalek 2.2` (default-features off). No `tempfile`.
* Verified gate baseline: `cargo test --workspace` → **367 passed**, 0 failed.

## 2. Phase 3 surface Phase 4 must build on (interfaces inspected in source)

| Surface | File | Notes reused by Phase 4 |
|---|---|---|
| Mode policy | `crates/openrt-modes/src/lib.rs` | `ModeManager::new(store)`, `current`, `status`,
  `ensure_network_allowed`, `ensure_materialization_allowed(what)`, `transition`, `seed_from_config`.
  Sovereign is fail-closed. |
| Registry | `crates/openrt-registry/src/lib.rs` | `ModelRegistry`, `ManifestV1` (has
  `base_digest: Option<String>`), `LifecycleState`, `RegistryCard`, `import_local`, `publish_manifest`
  (write-once), `activate`, `retire`, `curated_catalog`. **Residual: no derived-artifact registration
  path — Phase 4 must add it.** |
| Verify | `crates/openrt-verify/src/lib.rs` | `sha256_hex`, `Verifier::verify_base`,
  `verify_derived`, `ProvenanceManifest` (`canonical_recipe`, `validate_recipe_digest`),
  `ArtifactClass::{Base, Derived}`. Proof chain fully exists. |
| Scan | `crates/openrt-scan/src/lib.rs` | bundle signing/verification, `ArtifactKind`. |
| Router | `crates/openrt-router/src/lib.rs` | `Router::route/is_eligible`, `RouteRequest/Verdict`.
  Duplicate-free: only one router. |
| Persistence | `crates/openrt-persistence/src/store.rs` | `PersistenceStore`:
  `put_record/get_record/list_records/delete_record/put_blob/get_blob`; ns/key charset enforced by
  `validate_ns_key` (`[a-z0-9._-]`). `MemoryStore`, `LocalFileStore`. Reopen round-trip proven by
  Phase 3 tests. |
| Audit | `crates/openrt-audit/src/lib.rs` | `AuditLog::new(store)`, `append(actor, action, subject,
  result, ext)`, `verify` (chain), `count`. |
| Permissions | `crates/openrt-permissions/src/lib.rs` | `Policy`/`PolicyEvaluator`/
  `PermissionRequest`/`Decision`, deny-by-default, `path_scope` jail (`check_path`). |
| Events | `crates/openrt-core/src/event.rs` | `EventKind` enum — Phase 4 adds new kinds following
  the documented convention. |
| Models | `crates/openrt-models` | `ModelProvider` trait, `MockProvider` (deterministic
  offline), readiness reporting. |
| Runtime | `crates/openrt-runtime/src/runtime.rs` | `Runtime` owns store, audit, modes,
  router; `RuntimeBuilder`; `Error::Modes`. |
| Config | `crates/openrt-config/src/lib.rs` | `Config` with `#[serde(deny_unknown_fields)]`;
  new sections must use `#[serde(default)]` and be added to `Default` + `validate`. |
| CLI | `crates/openrt-cli/src/main.rs` | clap; `registry_for(&rt)` pattern;
  `actor_user()`; `--memory`/`--mock` globals; mode-gated model commands. |

## 3. Verified Phase 3 residual: derived-artifact registry gap

Confirmed by reading `openrt-registry` import path: `import_local` records base artifacts with
`base_digest: None` and `ArtifactClass::Base`; no API accepts a `ProvenanceManifest`. Meanwhile
`openrt-verify` already implements `verify_derived` + chain tests. Phase 4 resolves this by adding
`ModelRegistry::register_derived(...)`:

* requires **Provisioning** mode (materialization gate) and operator identity;
* confirms the base artifact digest already exists in the registry and is installed/active (base
  manifest stays immutable — no writes to base manifest IDs);
* verifies the artifact bytes against `ProvenanceManifest` (artifact digest, provenance chain,
  recipe digest) **before** any registration record is written;
* publishes a derived manifest with `base_digest` set, `ArtifactClass::Derived`, and its own
  lifecycle; on any verification failure nothing is registered.

## 4. Capability posture of the environment (honest)

* No internet requirement for tests; no GPU; no `tesseract`; no llama.cpp/vLLM servers running;
  mock provider is offline-deterministic.
* Therefore Phase 4 implements the **pipelines, interfaces, and deterministic backends** and
  classifies real OCR / audio transcription / real embeddings / real training backends as
  **capability-discovered, currently unavailable** — a Phase 4 command invoking an unavailable
  backend fails with an explicit, honest "unsupported: no <backend> available" error, never a fake
  output.

## 5. Scope committed for this milestone (correctness over feature count)

Fully implemented + tested (deterministic, offline):

1. `openrt-ingest` — format detection (magic bytes + extension), genuine parsers for
   text / markdown / CSV / JSON and a minimal real PDF text extractor (flate2); PNG/JPEG
   header validation (no heavy image dep); safe path jail (canonical + symlink); limits;
   content-hash dedup; versioned `NormalizedDocumentV1` (blocks + provenance); ingestion
   status machine; OCR / VLM / audio **interfaces** with honest `unavailable` capability probes.
2. Chunking — deterministic strategies (fixed / paragraph / heading / page / table-aware /
   overlap), stable IDs, chunk provenance, config validation; never labels characters as tokens.
3. `openrt-dataset` — JSONL import/validate/preview/export; typed schemas (instruction/
   response, conversation, preference); provenance + versioning; split train/val/test;
   safe diagnostics (no full-content leak); no script execution on import.
4. `openrt-embeddings` — `EmbeddingProvider` trait, `Embedding`, NaN/dim validation,
   deterministic hash embedder (offline), interface for real backends.
5. `openrt-vectorindex` — persisted vector index (`knowledge.vectors`), bounded exact
   baseline (no ANN claims), insert/batch/delete/query/persist/reload/integrity/rebuild.
6. `openrt-knowledge` — KB lifecycle (`kb.meta`), persisted index states, membership
   add/remove/reindex, retrieval (top-k/threshold/filters/provenance), RAG oracle with
   generation-backend interface, context budgeting, evidence dedup, citation integrity
   validation, prompt-injection defense, no-evidence honest behavior.
7. `openrt-train` — typed versioned `TrainConfig`, training-backend capability discovery,
   resource preflight, persistent job state machine `train.runs`, cancels, checkpoints,
   `eval.results`, and derived-artifact registration via the registry (resolves §3).
8. CLI groups `docs` / `kb` / `dataset` / `train` (help, args, exit codes, JSON output,
   preflight without side effects, audit, cancellation).
9. New `EventKind` variants for ingestion/OCR/chunk/embed/index/retrieval/RAG/citation/
   dataset/train/eval/artifact events with safe payloads.
10. Integration suites `p4_rag.rs` (workflows A–D) and `p4_train.rs` (E–F, fixture-based).
11. Docs + ADR-011/012, OWNERSHIP/ROADMAP updates, gate + `PHASE4_FINAL_REPORT.md` (A–L).

Deferred (classified honestly in docs): real OCR/tesseract, real VLM, audio transcription,
DOCX/XLSX binary parsing, ANN index, real training backends in this environment, GPU/network
features, mid-session embedding hot-swap.

## 6. Risk notes

* `Config` has `deny_unknown_fields`; new fields must default so old configs keep loading.
* ns/key charset excludes `/` — safe-key-encode any ID containing forbidden chars.
* Audit log ordering is seq-based and append-only; all Phase 4 mutating ops route through it.
* No `tempfile`: tests use `std::env::temp_dir().join("openrt-…")` cleanup pattern.