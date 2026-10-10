# Phase 4 — Milestones

Ground rule: every implementation milestone ships with deterministic, offline unit tests and
compiles clean under `cargo clippy -D warnings`. No milestone is "done" until `cargo test -p <crate>`
passes. Track progress by ticking boxes; the final gate re-runs everything.

## M0 — Preflight (done)
- [x] Inspect repo, verify Phase 3 (367 tests), map interfaces (PHASE4_PREFLIGHT.md).
- [x] This milestone plan + interface contract (PHASE4_INTERFACES.md).

## M1 — `openrt-ingest`: intake + document model + chunking
- [x] Crate, types: `FormatKind`, `DetectionReport` (magic-first + extension cross-check),
  `NormalizedDocumentV1`, `Block`, `BlockKind`, page/block provenance.
- [ ] Genuine parsers: text, markdown, CSV, JSON.
- [ ] Genuine minimal PDF text extractor (xref/trailer, FlateDecode streams via flate2,
      text-showing operators). Fails honestly on encrypted/unsupported structures.
- [ ] PNG (IHDR) + JPEG (SOF) header validation + dimensions.
- [ ] Path jail: canonicalization, symlink rejection, limits (max bytes/pages), dedup by
      content hash, doc versioning.
- [ ] Ingestion status machine + `corpus.documents` store + audit events.
- [ ] OCR / VLM / audio capability probes (honest unavailable) + extraction provenance rules.
- [ ] Chunking engine: strategies fixed/paragraph/heading/page/table-aware/overlap;
      stable IDs; validation; never char-as-token.

## M2 — `openrt-dataset`
- [ ] JSONL import with provenance (id/version/hash/records/splits), schemas,
      validation report (safe diagnostics), preview (truncated), export safe-path,
      delete; no script execution.

## M3 — embeddings + vector index
- [ ] `openrt-embeddings`: provider trait, `Embedding` validation, deterministic embedder.
- [ ] `openrt-vectorindex`: persisted exact index, CRUD + query + persist/reload +
      integrity + rebuild, honest bounded baseline.

## M4 — knowledge bases + RAG (`openrt-knowledge`)
- [ ] KB lifecycle + index states + membership + reindex pipeline.
- [ ] Retrieval: top-k/threshold/filters/provenance, structural validation.
- [ ] RAG oracle: context budget, evidence dedup/stable IDs, generation-backend interface,
      citation integrity validation, no-evidence behavior, injection defense.

## M5 — `openrt-train` adaptation studio + registry derived artifacts
- [ ] `TrainConfig` (versioned, deny-unknown), backend capability discovery.
- [ ] Resource preflight (estimated/measured/unknown) via resource manager + reserve semantics.
- [ ] Job state machine persisted `train.runs` (Created→Validating→Preflight→Queued→Running→
      Checkpointing→Evaluating→Completed | Failed/Cancelled/Interrupted/ResumePending);
      cancel (permission-checked + audited); checkpoint metadata; eval `eval.results`.
- [ ] `ModelRegistry::register_derived` — resolves Phase 3 gap; verify-before-register,
      base immutable, provenance + lifecycle, audit.

## M6 — CLI + events wiring
- [ ] `docs` group: ingest/list/inspect/status/reindex/remove.
- [ ] `kb` group: create/list/inspect/add/remove/search/ask/reindex.
- [ ] `dataset` group: import/list/inspect/preview/validate/export.
- [ ] `train` group: validate/preflight/start/list/status/cancel/resume/evaluate/artifact/register.
- [ ] New `EventKind` variants; audit on every mutation; JSON output; exit codes; preflight=no side effects.

## M7 — Integration workflows (p4_rag.rs, p4_train.rs)
- [ ] A: ingest→KB→chunks→embeddings→persist→reopen→retrieve with provenance.
- [ ] B: evidence-grounded question, citations all validate.
- [ ] C: prompt injection cannot grant permissions / invoke tools.
- [ ] D: Sovereign no remote fallback + explicit unavailable errors + audit verified.
- [ ] E: dataset→training: real backend or explicit unsupported; failed things never create runs.
- [ ] F: derived-artifact registration via fixture; base manifest stays immutable.

## M8 — Docs + security review + gate
- [ ] ARCHITECTURE.md, SECURITY_REVIEW.md, TEST_MATRIX.md, FINAL_GATE.md
- [ ] ADR-011 (multimodal knowledge + RAG), ADR-012 (adaptation studio), OWNERSHIP/ROADMAP updates.
- [ ] Full quality gate (fmt/check/clippy both feature sets/tests both/all-features + smoke).
- [ ] `PHASE4_FINAL_REPORT.md` with A–L and a single status decision.

Independent lanes that share no mutable state may run in parallel: M1 (ingest/chunk) ‖ M2 (dataset)
‖ M3 (embeddings/vector). M4 depends on M1+M3; M5 on M2+M4; M6/M7 on everything.