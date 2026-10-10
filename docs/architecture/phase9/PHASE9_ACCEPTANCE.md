# PHASE 9 ACCEPTANCE — Multimodal Intelligence Fabric

## Final status

**PHASE 9 COMPLETE** — M9.1–M9.10 green on this host: real multimodal intake
(born-digital + scanned + image + audio + video) with provenance through
chunking→retrieval→citation→verification, real OCR/VLM/media/embeddings
backends, hybrid retrieval + rerank with `retrieval_source` honesty, evidence
graph + knowledge graph on the allocated namespaces, a production
hallucination firewall (deterministic-first, demote-only models, honest
cross-model independence), prompt-injection defense enforced at the runtime
answer path, and honest CLI/doctor/capability surfaces. No fake OCR/VLM/ASR/
embeddings/reranker/verifier anywhere in a production path; TEST 1 (the
deterministic RAG/citation authority) stays green. Design:
[`PHASE9_IMPLEMENTATION.md`](./PHASE9_IMPLEMENTATION.md).

## Gates (2026-09-25, this host)

| Gate | Command | Result |
|---|---|---|
| Format | `cargo fmt --all --check` | **RC=0** |
| Types/build | `cargo check --workspace --offline` | **RC=0, 0 warnings** |
| Full test suite | `cargo test --workspace --offline --no-fail-fast` | **RC=0 — 1093 passed / 0 failed / 0 skipped** (162 `test result: ok` lines, 102 test binaries; log `/tmp/opencode/p9r_tests.log`, name baseline `/tmp/opencode/p9r_tests.txt`) |
| Test-name diff | `comm` vs pre-audit baseline `/tmp/opencode/p9q_tests.txt` | +1 (`verified_claims_land_in_the_evidence_graph`), 0 removed |
| Doctor | `openrt doctor` (default config) | **RC=0 — 0 fail / 1 warn / 2 unavailable** (27 PASS, 1 WARN `media.asr MODEL_MISSING`, 2 UNAVAILABLE `rerank.model`+`verifier.model`; log `/tmp/opencode/p9r_doctor.log`) |
| CLI smoke (live) | see “CLI evidence” below | all scenarios correct incl. exit-code contract |

Phase 9 net-new vs the frozen Phase 8 gate (867 tests): **+226** (Phase 8
suite grew by shared-crate additions; Phase 9-specific suites: integration
`p9_*` **53** + phase-9 crate units **238**, see below).

## Milestone evidence

| M | Verdict | Evidence |
|---|---|---|
| M9.1 Ingestion core | GREEN | `p9_ingest` 5/5: `status_gate_rejects_ambiguous_and_corrupt_with_reasons`, `detect_statuses_are_honest`, `jail_rejects_path_escapes`, `pdf_page_numbers_survive_storage`, `html_extract_stores_lists_inspects_removes`; `openrt-ingest` 108 units (incl. `truncated_pdf_is_corrupt`, `truncated_zip_is_corrupt`, `symlink_escape_rejected`); CLI `ingest *` |
| M9.2 OCR + scanned PDF + layout | GREEN | `p9_ocr` 4/4 on real backends (tesseract 5.5.3, pdftoppm 26.08): `image_ocr_stores_text_with_provenance`, `image_only_pdf_routes_to_full_ocr`, `mixed_pdf_routes_pages_per_text_layer` (text-layer vs OCR per page, both producers recorded), `image_detect_status_tracks_capability`; doctor `document.{ocr,render,scan,layout}` PASS |
| M9.3 Images + VLM | GREEN | `p9_vlm` 6/6 (real moondream + honest dead-endpoint/missing-model/doctor tests); drawing route `drawing_route_regions_and_honest_vlm`; CLI `vision {capabilities,describe,drawing}` |
| M9.4 Audio + video + ASR | GREEN | `p9_media` 3/3: `audio_ingest_refuses_structured_without_asr` (structured `MODEL_MISSING`, no whisper on host — no transcript fiction), `video_frames_ocr_with_timeline_and_chunks` (real ffmpeg frame sampling + t₀/t₁ chunks), `wav_detection_tracks_media_capability`; doctor `media.{probe,video}` PASS, `media.asr` WARN |
| M9.5 Embeddings + vector store | GREEN | `p9_embeddings` 6/6 (`real_model_semantic_sanity` on all-minilm/384, `dead_endpoint_reports_unavailable`, `missing_model_is_structured`, `doctor_embeddings_rows_match_probe`); CLI `knowledge semantic-search` probe-first; doctor `embeddings.{backend,config}` + `retrieval.semantic` PASS |
| M9.6 Hybrid retrieval + rerank | GREEN | `p9_retrieval` 7/7: `lexical_only_hybrid_is_honest_without_dense`, `filter_bounds_hybrid_universe`, `deterministic_rerank_is_reported_applied`, `model_rerank_*_honest_unavailable` ×2, `real_dense_fusion_reports_provenance`, `real_model_rerank_reorders_without_losing_citations`; CLI `knowledge hybrid-search --rerank {none,deterministic,model}`; `openrt-retrieval` 31 units |
| M9.7 Evidence graph + KG | GREEN | `p9_evidence` 6/6: `citations_resolve_and_traverse_through_the_evidence_graph`, `unpersisted_citation_cannot_be_linked_fail_closed`, `relinking_is_idempotent_edges_stay_append_only`, `tampering_and_deletion_are_detected_never_silent`, `deleted_source_chunk_unresolves_its_citation`, `knowledge_graph_lives_on_the_same_store_without_duplicates`; production writers: `Runtime::verify_claim` → `record_claim`/`link_claim` (`verified_claims_land_in_the_evidence_graph`), CLI `knowledge graph` + `knowledge evidence` (integrity/claims/verdicts) |
| M9.8 Verifier + firewall | GREEN | `p9_verifier` 7/7: `unsupported_claim_is_blocked_at_the_production_answer_path` (Block before any model call), `supported_claim_passes…`, `model_verdict_can_only_demote_never_lift`, `cross_model_independence_is_reported_honestly` (same-model-twice → DEGRADED; disagreement → Flag), `limits_are_enforced…`, `real_cross_model_verification_reports_independence_honestly`, `verified_claims_land_in_the_evidence_graph`; `openrt-verifier` 30 units; CLI `verify {claim,answer}` exit contract 0/2/3/1 |
| M9.9 Injection defense | GREEN | `p9_injection` 5/5: marker containment invariant (payload only between `<<< UNTRUSTED EVIDENCE` markers, never in the policy header), trust records default UNTRUSTED + flags survive `mark_operator_trusted`, audit entry with ids/categories only (no corpus text, no query), clean corpus → no flags/no audit, `exclude_flagged` withholds but still reports; `openrt-core::injection` shared types; doctor `defense.injection` PASS |
| M9.10 Final gate | GREEN | this file + `PHASE9_IMPLEMENTATION.md`, fmt/check/test/doctor green, CLI surfaces live, forensic audit + TEST matrix + capability matrix below |

## Definition of Done (ROADMAP § PHASE 9.5)

- **One pipeline, per-modality provenance** — born-digital PDF, scanned PDF,
  HTML, DOCX/XLSX/PPTX, PNG/JPEG, WAV, MP4 ingest through
  `Runtime::corpus.ingest_path`; page/bbox/t₀/t₁/extractor/confidence
  survive chunking → retrieval → citation → verdict
  (`p9_e2e::full_chain_from_multimodal_ingest_to_verified_answer_context`,
  `pdf_page_numbers_survive_storage`).
- **Honest availability everywhere** — statuses
  `UNSUPPORTED`/`CORRUPT`/`AMBIGUOUS`/`MODEL_MISSING`/`UNAVAILABLE` with
  reasons; doctor 0 fail / 1 warn / 2 unavailable on this host; capability
  matrix below; no fake OCR/VLM/ASR/embeddings/reranker in any production
  path (grep audit: only `openrt-models::mock` + named canned fixtures in
  `#[cfg(test)]`).
- **Hybrid + rerank + resolvable citations; TEST 1 green** —
  `p9_retrieval` proves fusion/filters/rerank honesty; knowledge oracle
  regression tests (`oracle_*`, `retrieval_never_invents_an_absent_chunk`)
  all green; `knowledge search` CLI alias works over the same authority.
- **Firewall at a real answer path** — CLI `verify claim/answer` blocks
  unsupported (exit 3), flags weak (exit 2), passes supported (exit 0) with
  verdict persisted first; cross-model honesty proven live
  (`qwen2.5:0.5b` + `llama3.1:8b` → INDEPENDENT, disagreement → Flag).
- **Evidence UNTRUSTED by default** — labels + flags enforced by the context
  builder at `Runtime::evidence_context`, hostile-fixture tests green,
  audit chain entries carry ids/categories only.
- **OWNERSHIP + single authorities + phases 5–8 green** — allocations
  amended (M9.9 shared types in `openrt-core`, M9.10 implementation
  changelog); p5/p6/p6b/p7/p7b/p8 suites all green in the final run.

## TEST 1–13 matrix

Roadmap §6 definitions (operator-list variants folded in where they differ;
every row runs real production paths — no test-only plumbing):

| TEST | Definition | Proof | Result |
|---|---|---|---|
| 1 | Deterministic RAG + exact-citation regression stays green | `openrt-knowledge` units: `retrieval_grounds_only_real_persisted_chunks`, `retrieval_never_invents_an_absent_chunk`, `oracle_grounds_an_answer_only_on_real_persisted_chunks`, `oracle_fails_closed_never_answers_from_absent_evidence`, `oracle_never_cites_an_absent_chunk_as_grounded` (23 green); CLI `knowledge search` alias smoke | PASS |
| 2 | Scanned PDF → OCR → chunks → retrieval → answer with page/bbox provenance (real Tesseract + pdftoppm) | `p9_ocr::{image_only_pdf_routes_to_full_ocr, mixed_pdf_routes_pages_per_text_layer, image_ocr_stores_text_with_provenance}` + `p9_e2e` (OCR chunk retrieved by its own text, `citation.page == 2`, sha verified) | PASS |
| 3 | Born-digital PDF keeps text path; page provenance; malformed/hostile rejected `CORRUPT` (operator variant: image → VLM → structured evidence) | `p9_ingest::{pdf_page_numbers_survive_storage, status_gate_rejects_ambiguous_and_corrupt_with_reasons, detect_statuses_are_honest}` + `openrt-ingest` `truncated_pdf_is_corrupt`/`truncated_zip_is_corrupt` + `p9_vlm::moondream_answers_real_question` | PASS |
| 4 | Audio → probe → decode → ASR (or structured `MODEL_MISSING`) → timestamped provenance | `p9_media::{audio_ingest_refuses_structured_without_asr, wav_detection_tracks_media_capability}` (hostile case: no whisper → structured refusal, never a fake transcript) | PASS |
| 5 | Video → frame sampling + audio extraction → timeline → multimodal chunks with t₀/t₁ | `p9_media::video_frames_ocr_with_timeline_and_chunks` (real ffmpeg; chunks carry timeline + media meta) | PASS |
| 6 | Hybrid retrieval + reranking (fusion, filters, `retrieval_source`, honest availability) | `p9_retrieval` 7/7 + `openrt-retrieval` 31 units; CLI `knowledge hybrid-search` reports per-hit `bm25=`/`dense=`/`rerank=` | PASS |
| 7 | Citations resolve against persisted evidence; graph edges append-only; tampering detected; KG on same store | `p9_evidence` 6/6 + `openrt-evidence` 20 units; live: `knowledge evidence` → `INTACT (1 claim(s), 2 edge(s))`, `--cite doc-a:c999` → exit 1 | PASS |
| 8 | Hallucination firewall at the production answer path; limits; supported passes | `p9_verifier` 7/7 + `openrt-verifier` 30 units; live exit codes 0/2/3/1 verified | PASS |
| 9 | Injection fixtures flagged; trust labels flow; injected instructions never operator directives | `p9_injection` 5/5 (marker-containment invariant) + live `verify answer` on hostile corpus (`flags=instruction_override,exfiltration_bait`, audit seq recorded) | PASS |
| 10 | Capability honesty — structured unavailability, no fake GREEN (operator variant: conflicting evidence → rejected/inconclusive) | honesty: `p9_embeddings`/`p9_vlm` dead-endpoint & missing-model tests, `capabilities::no_fabricated_capabilities`, doctor 0 fail with 1 WARN + 2 UNAVAILABLE, `multimodal capabilities` live; conflict: `p9_conflict` 3/3 + `model_verdict_can_only_demote_never_lift` + `cross_model_independence_is_reported_honestly` | PASS |
| 11 | Ingestion bounds — resource/time/output limits on hostile inputs; sandbox admission (operator variant: Sovereign/backend restriction) | bounds: `p9_ingest::jail_rejects_path_escapes` + status gate + `openrt-ingest` limits/corrupt units (jail + `CorpusLimits` + Phase 8 `ResourceGate` for tool-driven parsing); Sovereign: `p3_fabric::sovereign_refuses_materialization_before_touching_bytes`, `p6b_production::netpol_sovereign_denies_then_grant_allows_in_provisioning`, `openrt-modes::{default_mode_is_sovereign, sovereign_blocks_network_operations}`, doctor `air-gap PASS (Sovereign mode: network blocked)` | PASS |
| 12 | Production-caller forensic audit — test-only callers don’t count (operator variant: persistence + restart + provenance) | audit table below (every required function has a CLI/runtime/doctor caller); persistence: `persistence_roundtrip::file_store_survives_restart`, `durability` 2/2, provenance: `p9_ingest`/`p9_e2e` | PASS |
| 13 | E2E reasoning → retrieval → evidence → verification with multimodal + drawing fixtures and cross-model honesty | `p9_e2e::full_chain_from_multimodal_ingest_to_verified_answer_context` (ingest→chunk→persist→hybrid→verify→defense), `p9_vlm::drawing_route_regions_and_honest_vlm`, `p9_verifier::real_cross_model_verification_reports_independence_honestly`, live cross-model CLI smoke | PASS |

## Forensic production-caller audit (TEST 12)

Test-only callers do not count; every row shows the shipped caller, its
source line (CLI `crates/openrt-cli/src/main.rs`, runtime
`crates/openrt-runtime/src/{verify,defense,doctor}.rs`), the real backend,
and the integration test that exercises it.

| Function | Production caller | Source | Real backend | Integration test |
|---|---|---|---|---|
| `detect` (status gate) | CLI `ingest inspect` | main.rs `IngestCmd::Inspect` | real file bytes/signatures | `p9_ingest::detect_statuses_are_honest`, `status_gate_rejects_ambiguous_and_corrupt_with_reasons` |
| `corpus.ingest_path` | CLI `ingest extract`; `Runtime::corpus` | main.rs `IngestCmd::Extract` | pdf/html/office/ocr/media parsers | `p9_ingest::html_extract_stores_lists_inspects_removes`, `p9_ocr`, `p9_media` |
| `ocr_capability`/OCR extract | CLI `ingest ocr`; doctor `document.ocr` | main.rs `IngestCmd::Ocr`, doctor.rs | Tesseract 5.5.3 | `p9_ocr::image_ocr_stores_text_with_provenance` |
| `render_capability`/pdftoppm | scanned-PDF route; doctor `document.render` | doctor.rs `document_scan_check` | pdftoppm 26.08 | `p9_ocr::image_only_pdf_routes_to_full_ocr` |
| layout analysis | CLI `ingest analyze`; doctor `document.layout` | main.rs `IngestCmd::Analyze`, doctor.rs | real Tesseract boxes | `p9_ocr::mixed_pdf_routes_pages_per_text_layer` (+ ingest units) |
| image metadata intake | CLI `ingest extract` (png/jpeg/tiff) | main.rs | header parser | `p9_ingest`, `p9_ocr` |
| VLM describe | CLI `vision describe` | main.rs `VisionCmd::Describe` | moondream via local Ollama | `p9_vlm::moondream_answers_real_question` |
| drawing analysis | CLI `vision drawing` | main.rs `VisionCmd::Drawing` | Tesseract + moondream | `p9_vlm::drawing_route_regions_and_honest_vlm` |
| ASR status/refusal | intake path + doctor `media.asr` | doctor.rs `media_*_check` | ffprobe real; whisper absent → `MODEL_MISSING` | `p9_media::audio_ingest_refuses_structured_without_asr` |
| video probe/frame sampling | intake path; doctor `media.video` | doctor.rs | ffmpeg 9.0.1 | `p9_media::video_frames_ocr_with_timeline_and_chunks` |
| `probe_embedding`/`OllamaEmbedder` | CLI `knowledge semantic-search` + `hybrid-search` dense half; doctor `embeddings.*` | main.rs, doctor.rs `embeddings_check` | Ollama `/api/embed` (all-minilm, 384) | `p9_embeddings::real_model_semantic_sanity`, `doctor_embeddings_rows_match_probe` |
| vector index wiring | `knowledge semantic-search` | main.rs `KnowledgeCmd::SemanticSearch` | `knowledge.vectors` (derived) | `p9_embeddings::reference_provider_dense_path_is_honest` |
| `hybrid_search` | CLI `knowledge hybrid-search`; `Runtime::evidence_context` | main.rs, defense.rs | BM25 + dense fusion (RRF) | `p9_retrieval::real_dense_fusion_reports_provenance`, `p9_injection` (defense path) |
| rerank (deterministic/model) | CLI `knowledge hybrid-search --rerank` | main.rs `RerankArg` | lexical reranker; Ollama model rerank | `p9_retrieval::{deterministic_rerank_is_reported_applied, real_model_rerank_reorders_without_losing_citations}` |
| `record_claim`/`link_claim` | `Runtime::verify_claim`/`verify_claims` ← CLI `verify {claim,answer}` | verify.rs `record_claim_graph` | `corpus.chunks` re-validation | `p9_verifier::verified_claims_land_in_the_evidence_graph` |
| `resolve_citation`/`verify_integrity`/`list_claims`/`list_edges`/`list_trust` | CLI `knowledge evidence` | main.rs `KnowledgeCmd::Evidence` | append-only `knowledge.evidence` + chunk authority | `p9_evidence::citations_resolve_and_traverse_through_the_evidence_graph` (integrity walk uses `list_edges`) |
| KG `upsert_entity`/`add_relation`/`list_entities`/`list_relations` | CLI `knowledge graph {list,upsert-entity,add-relation}` | main.rs `KnowledgeCmd::Graph` | evolving `knowledge.graph` (same store) | `p9_evidence::knowledge_graph_lives_on_the_same_store_without_duplicates` + live smoke |
| `scan_evidence`/`detect_injection` | `Runtime::evidence_context` ← CLI `verify answer` | defense.rs | deterministic detector (no backend) | `p9_injection` 5/5 |
| `trust_label`/`get_trust` | `Runtime::evidence_context`; CLI `knowledge evidence` | defense.rs, main.rs | `knowledge.evidence` trust records | `p9_injection::trust_records_persist_flags_and_default_to_untrusted` |
| `build_evidence_context` (context builder) | `Runtime::evidence_context` ← CLI `verify answer` (`--show-context`) | defense.rs | UNTRUSTED wrapping | `p9_injection::hostile_evidence_is_flagged_labeled_and_wrapped` |
| injection audit entry | `Runtime::evidence_context` | defense.rs | ADR-002 chained audit | `p9_injection::injection_categories_are_audited_without_corpus_text` |
| `verify_claim` (firewall) | CLI `verify claim`/`verify answer` (exit codes) | main.rs `VerifyCmd`, verify.rs | deterministic gate; Ollama backends via `--model` | `p9_verifier` 7/7; live smoke (0/2/3/1) |
| `list_verdicts` | CLI `knowledge evidence` (`verdicts: N recorded`) | main.rs `KnowledgeCmd::Evidence` | `verify.verdicts` | `p9_verifier` (records asserted), live smoke |
| cross-model independence | CLI `verify claim --model A --model B` | main.rs `VerifyCmd::Claim` | Ollama `/api/tags` + generate (qwen, llama) | `p9_verifier::real_cross_model_verification_reports_independence_honestly`; live smoke |
| Phase 9 doctor rows | CLI `doctor`; CLI `multimodal capabilities` | doctor.rs (19 rows) | same probes as library fns | runtime doctor tests + `p9_embeddings::doctor_embeddings_rows_match_probe` + `p9_vlm::doctor_includes_vision_row` |

## Security audit (ROADMAP § PHASE 9.7)

| # | Requirement | Evidence | Result |
|---|---|---|---|
| 1 | Retrieved documents UNTRUSTED: flagged at detection, labeled through retrieval, wrapped by the context builder | trust records default `Untrusted` (`p9_injection::trust_records_persist_flags_and_default_to_untrusted`); every block carries `trust=untrusted`; doctor `defense.injection` | PASS |
| 2 | Flagged text never executed as operator instruction | marker-containment invariant: payload exists only between evidence markers, never in the policy header (`assert_payload_inside_markers`); header policy is fixed text | PASS |
| 3 | Raw model output cannot invoke privileged tools | `openrt-verifier` has no dependency on `openrt-tools`/registry/executor (Cargo deps listed; grep = 0 hits); model output parsed as strict verdict JSON only; verdict JSON keys cannot smuggle commands | PASS |
| 4 | Provenance cannot be dropped | page/bbox/t₀/t₁/block-keys/sha asserted through chunk→citation→verdict (`p9_ingest::pdf_page_numbers_survive_storage`, `p9_e2e` sha + `citation.page` asserts); unresolved citations recorded as such | PASS |
| 5 | Citations cannot resolve to unrelated evidence | `Citation::validate` identity re-resolution; `unpersisted_citation_cannot_be_linked_fail_closed`, `deleted_source_chunk_unresolves_its_citation`, `tampering_and_deletion_are_detected_never_silent`; live `--cite doc-a:c999` → exit 1 | PASS |
| 6 | No corpus/secrets egress to unauthorized backends | only the explicitly configured model/embedding endpoint sees claim/evidence text (CLI `--model`/provider config); verdict records carry no keys/prompts-as-secrets (grep `secret\|api_key` in verifier = 0 hits); Sovereign default blocks ambient network (row 7) | PASS |
| 7 | Sovereign blocks prohibited external models/network | `openrt-modes::default_mode_is_sovereign` + `sovereign_blocks_network_operations`; `p3_fabric::sovereign_refuses_materialization…`; `p6b_production::netpol_sovereign_denies…`; doctor `air-gap PASS (network blocked)`; no model download path exists in Phase 9 code | PASS |
| 8 | Injection audit never carries corpus text | `p9_injection::injection_categories_are_audited_without_corpus_text`: ext JSON contains no fixture payload, not even the query; entries are ids/categories only | PASS |
| 9 | Flagged evidence can never silently become trusted | `mark_operator_trusted` preserves flags/categories (asserted); default label is UNTRUSTED; `untrusted ≠ flagged` (clean corpus still labeled); `exclude_flagged` reports detections even when withholding | PASS |

## Capability matrix (this host, final)

| Capability | Status | Backend / reason |
|---|---|---|
| text/markdown/csv/json intake | SUPPORTED | normalized text path (no backend) |
| PDF born-digital | SUPPORTED | `openrt-pdf-text` text layer (page provenance) |
| PDF scanned (raster pages) | SUPPORTED | pdftoppm 26.08 → Tesseract 5.5.3 |
| PDF mixed routing | SUPPORTED | per-page text-layer vs OCR, both producers recorded |
| HTML intake | SUPPORTED | normalized HTML path |
| Office DOCX/XLSX/PPTX | SUPPORTED | `openrt-office` OOXML readers |
| Image metadata (png/jpeg/tiff) | SUPPORTED | header parser (dims/color, never guessed) |
| Image OCR | SUPPORTED | Tesseract 5.5.3 (boxes + confidence) |
| Layout analysis | SUPPORTED | real Tesseract boxes (line/block/column + reading order) |
| Image → VLM | SUPPORTED | `moondream:latest` via local Ollama (images transport) |
| Engineering-drawing route | SUPPORTED | OCR regions always; VLM part real or structured failure |
| Audio probe/decode | SUPPORTED | ffprobe/ffmpeg 9.0.1 |
| Audio → transcript (ASR) | **UNAVAILABLE (WARN)** | no whisper engine on PATH → `MODEL_MISSING`, intake refuses structurally |
| Video frames + audio track | SUPPORTED | ffmpeg 9.0.1 (timeline t₀/t₁ chunks) |
| Neural embeddings | SUPPORTED | Ollama `all-minilm` (384 dims, probed) |
| Reference (hash) embedder | SUPPORTED — labeled reference/lexical | never presented as neural |
| Lexical retrieval (BM25) | SUPPORTED | no backend required |
| Dense retrieval | SUPPORTED | Ollama embed → `knowledge.vectors` (hash-reference path works without it) |
| Hybrid fusion + filters | SUPPORTED | RRF over real BM25/dense lists; `retrieval_source` per hit |
| Deterministic rerank | SUPPORTED | lexical-coverage reranker (no backend) |
| Model rerank | **UNAVAILABLE** | provider reachable but no `default_model` configured (honest reason; fused order kept) |
| Deterministic citation authority | SUPPORTED | `openrt-knowledge` (TEST 1 green) |
| Hallucination firewall (deterministic gate) | SUPPORTED | lexical floor 0.50 + citation re-resolution, always on |
| LLM claim verification | **UNAVAILABLE** without `--model`/`default_model` | honest deterministic-only degradation (this host default config: no `default_model`) |
| Cross-model verification | SUPPORTED | 4 distinct Ollama models (`qwen2.5:0.5b`, `llama3.1:8b`, `moondream`, `all-minilm`) → genuinely `INDEPENDENT` possible |
| Evidence graph integrity | SUPPORTED | append-only + `verify_integrity` (claims/edges hashed) |
| Knowledge graph | SUPPORTED | evolving on same store (CLI `knowledge graph`) |
| Prompt-injection defense | SUPPORTED | deterministic detector (5 categories), no backend needed |
| Phase 9 WebUI/TUI status | **NOT BUILT** | Phase 8 precedent — no feature exists only inside a UI; CLI/doctor are the status surfaces |

## Phase 5–8 regression (final run, all green)

| Phase | Suites | Tests |
|---|---|---|
| 5 (`p5_*`) | `p5_workbench` | 6 |
| 6 (`p6_*`) | `p6_milestones`, `p6_workflow_compiler` | 28 |
| 6B (`p6b_*`) | `p6b_production` | 10 |
| 7A (`p7_*`) | `p7_tool_fabric` | 8 |
| 7B (`p7b_*`) | `p7b_browser`, `p7b_cross_tool`, `p7b_database_sqlite`, `p7b_fabric_deny`, `p7b_mcp_runtime`, `p7b_office_roundtrip` | 21 |
| 8 (`p8_*`) | `p8_sandbox` | 7 |

Workspace total: **1093 / 0 failed / 0 skipped** (frozen Phase 8 baseline
867 → +226).

## CLI evidence (live host)

Store: scratch config `/tmp/opencode/cli-smoke/config.json`
(`OPENRT_CONFIG`), real `~/.openrt` for the doctor gate.

| Scenario | Result |
|---|---|
| `verify claim kb1 "the launch window is dawn" --cite doc-a:c1` | exit 0 — Pass, coverage 1.00, 1/1 resolved |
| `verify claim` unsupported claim (no evidence) | exit 3 — Block before any model call |
| `verify claim` weak claim (coverage 0.29 < 0.50) | exit 2 — Flag |
| `verify claim --cite doc-a:c999` (unpersisted) | exit 1 — fail-closed, never resolves |
| `verify claim --cite doc-b:c1` (hostile chunk) | exit 0 + `injection flagged` line (grounded ≠ trusted) |
| `verify claim … --model qwen2.5:0.5b --model llama3.1:8b` | exit 2 — 2 distinct models, `INDEPENDENT`, disagreement → `Insufficient` (never averaged) |
| `verify answer "…"` (grounded) | exit 0 — `audit: evidence.injection_flagged seq=N (ADR-002 chained)` when flags exist |
| `verify answer` weak answer | exit 2 |
| `verify answer --json` | structured `{answer, injection:{flagged,categories,audit_seq,included,excluded_flagged}, report}` |
| `knowledge evidence kb1` | `INTACT (1 claim(s), 2 edge(s))`, claims, `verdicts: N recorded`, trust rows with flags |
| `knowledge evidence --cite doc-a:c1 --cite doc-a:c999` | `[resolved] … (2 evidence edge(s))` + `[UNRESOLVED]`, exit 1 |
| `knowledge graph kb1 upsert-entity/add-relation` + `knowledge graph kb1` | exit 0 — entity/relation round-trip on `knowledge.graph` |
| `knowledge search kb1 "launch window"` (alias) | exit 0 — scored hits over the same authority |
| `multimodal capabilities` | exit 0 — per-modality intake table + 19 Phase 9 doctor rows (same probes) |
| `doctor` (default config) | exit 0 — 0 fail / 1 warn / 2 unavailable |

## Known limitations / not claimed

1. **`media.asr` is `WARN … MODEL_MISSING`** — no whisper engine on this
   host; audio→transcript is honestly UNAVAILABLE (intake refuses
   structurally). Installing a real engine flips the row via the same probe.
2. **`verifier.model` / `rerank.model` are `UNAVAILABLE`** — the reachable
   provider config has no `default_model`; model verdicts/rerank run only
   when the operator passes `--model` (deterministic gate + deterministic
   rerank remain active).
3. **No phase-specific WebUI/TUI status surfaces** — consistent with the
   Phase 8 acceptance precedent (no feature exists only inside a UI);
   CLI + doctor + `multimodal capabilities` are the status surfaces.
4. **Cross-model disagreement is `Flag`, not `Block`** — by design:
   independent inconclusive verdicts are reported as such rather than
   fabricated into consensus; the deterministic floor still decides.
5. **Knowledge-graph extraction is operator-driven** — entities/relations
   are created via CLI (no automatic NER); the graph evolves, never
   duplicates (ADR-006).
