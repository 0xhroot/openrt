# PHASE 9 IMPLEMENTATION — Multimodal Intelligence Fabric

Phase: **9** (ROADMAP v0.6, milestones M9.1–M9.10).
Acceptance evidence (gates, TEST matrix, audits, capability matrix):
[`PHASE9_ACCEPTANCE.md`](./PHASE9_ACCEPTANCE.md).

## 1. Scope and allocation

Phase 9 turns every modality (PDF/image/audio/video/HTML/office) into
grounded, verifiable answers through ONE pipeline, without creating parallel
authorities (OWNERSHIP, ADR-006/ADR-007):

| Subsystem (milestone) | Crate(s) | Namespace |
|---|---|---|
| Multimodal ingestion core (M9.1) | `openrt-ingest` (extended) | `corpus.documents` |
| OCR + scanned PDF + layout (M9.2) | `openrt-ingest` | `corpus.documents` |
| Images + VLM (M9.3) | `openrt-ingest`, `openrt-vlm` | `corpus.documents{img}` |
| Audio + video + ASR (M9.4) | `openrt-ingest` | `corpus.documents{audio,video}` |
| Embeddings + vector store (M9.5) | `openrt-embeddings`, `openrt-vectorindex` | `knowledge.vectors` |
| Hybrid retrieval + reranking (M9.6) | `openrt-retrieval` | — |
| Evidence graph + knowledge graph (M9.7) | `openrt-evidence` | `knowledge.evidence`, `knowledge.graph` |
| Answer verification + hallucination firewall (M9.8) | `openrt-verifier` | `verify.verdicts` |
| Prompt-injection defense (M9.9) | `openrt-core` (shared types), `openrt-evidence`, `openrt-retrieval`, `openrt-runtime` | `knowledge.evidence` (trust/flags) |
| Surfaces (M9.10) | `openrt-cli`, `openrt-runtime` | — |

Single authorities preserved: `openrt-knowledge` remains THE deterministic
retrieval + citation authority (TEST 1 stays green); `openrt-audit` is the
only audit chain; `openrt-modes`/`openrt-netpol` remain the Sovereign/egress
authorities; `openrt-verifier` (answer verdicts) stays distinct from
`openrt-verify` (digest provenance) and the P2 Verifier agent role.

## 2. The honesty contract (applies to every layer)

1. **Capability honesty** — every backend reports
   `AVAILABLE` / `UNAVAILABLE` (with reason) / `MODEL_MISSING` / `CORRUPT` /
   `AMBIGUOUS` / `UNSUPPORTED` / `BLOCKED`. An adapter's existence is never
   evidence of a working backend; doctor probes the real thing.
2. **No fake AI** — OCR is Tesseract (TSV boxes + confidence), page
   rasterization is pdftoppm, media probing/decode is ffprobe/ffmpeg,
   neural embeddings are a real local Ollama endpoint, VLM answers are real
   model output, verification backends are real models or absent. Test
   doubles (canned JSON, mock providers) live only in `#[cfg(test)]` and
   explicitly named fixtures.
3. **Evidence is UNTRUSTED by default** — every retrieved passage carries
   `TrustLabel::Untrusted` unless an operator explicitly marks it trusted,
   and trusting NEVER erases injection flags.
4. **Deterministic-first verification** — the lexical evidence gate decides
   before any model is consulted; model verdicts may only DEMOTE; cross-model
   `INDEPENDENT` requires ≥2 genuinely distinct models.
5. **Knowledge is the citation authority** — citations must re-resolve
   against persisted `corpus.chunks` at every consumer (retrieval, defense,
   verification, evidence graph); no second citation store exists.

## 3. The pipeline (end to end)

```
file bytes
  │  detect (SUPPORTED/UNSUPPORTED/CORRUPT/AMBIGUOUS, jail-bounded)
  ▼
extract ──── born-digital: text layer / HTML / OOXML / HTML
  │          scanned: pdftoppm → Tesseract (boxes, confidence)
  │          image: header metadata (+ OCR / VLM)
  │          audio/video: ffprobe/ffmpeg (+ ASR or MODEL_MISSING)
  ▼
NormalizedDocumentV1 (blocks: page, bbox, t₀/t₁, source.method/confidence)
  │  chunk_normalized (Fixed/Paragraph/Heading/Page/Table/Overlap)
  ▼
corpus.chunks  (page/bbox/timeline provenance + text sha256 per chunk)
  │
  ├─ hybrid_search: BM25 (lexical) ⊕ dense (Ollama embeddings) → RRF,
  │    metadata filters → rerank (deterministic | model | off)
  │    every hit re-validated against persisted chunks (citation authority)
  │
  ├─ Runtime::evidence_context  (M9.9 answer path)
  │    hybrid_search → load_chunk → scan_evidence (5 injection categories)
  │    → trust labels → hardened context builder (UNTRUSTED wrappers)
  │    → chained audit "evidence.injection_flagged" (ids/categories only)
  │
  └─ Runtime::verify_claim  (M9.8 answer path)
       deterministic gate (coverage floor 0.50, citation re-resolution)
       → bounded model backends (demote-only) → cross-model independence
       → verdict persisted FIRST (verify.verdicts)
       → claim + resolved-citation links recorded (knowledge.evidence)
```

## 4. Milestone design notes

### M9.1 — Multimodal ingestion core
- Detection statuses `SUPPORTED`/`UNSUPPORTED`/`CORRUPT`/`AMBIGUOUS` with a
  reason for every non-supported outcome (`openrt-ingest::detect`);
  truncated/malformed headers fail closed as `CORRUPT`, never as guesses.
- `NormalizedDocumentV1` carries per-block provenance: `page`, `source.bbox`,
  `source.confidence`, `source.method` (`PdfText` vs `Ocr` vs …), media
  timeline fields for A/V. Chunking (`chunk_normalized`) propagates page /
  t₀/t₁ / block keys / content sha — provenance survives to citations.
- `IngestJail` (SEC-01) bounds paths; extraction limits live in
  `CorpusLimits`. Production callers: CLI `ingest {inspect,extract,list,
  show,remove,ocr,analyze}` + `Runtime::corpus`.

### M9.2 — OCR + scanned PDF + layout
- `ocr_capability()`/`render_capability()` probe the real binaries
  (version strings included in doctor details).
- Routing: born-digital pages keep the text layer (no invented
  bbox/confidence); image-only pages rasterize via pdftoppm and OCR via
  Tesseract TSV (word boxes + confidence); mixed PDFs record BOTH producers
  in `extractor` (e.g. `openrt-pdf-text … tesseract|5.5.3`).
- Layout analysis (line/block/column, reading order) is computed from real
  OCR boxes — `document.layout` doctor row describes real geometry.

### M9.3 — Images + VLM
- Image intake parses header metadata (dimensions, color model) — never
  guesses; `corpus.documents{img}`.
- `openrt-vlm` rides the existing `openrt-models` provider infrastructure
  with an images transport (local Ollama `/api/generate` with images);
  probes report `AVAILABLE`/`UNAVAILABLE(reason)`/`MODEL_MISSING(model,
  installed list)`.
- Engineering-drawing route (`openrt_vlm::drawing::analyze`): OCR regions
  with bbox/confidence always; VLM part answered or structurally failed —
  never silent. CLI: `vision {capabilities,describe,drawing}`.

### M9.4 — Audio + video + ASR
- ffprobe/ffmpeg-backed format probing, decode, frame sampling; detection
  status tracks the actual capability on the host.
- ASR sits behind a provider trait: with no real engine present the intake
  refuses with structured `MODEL_MISSING` (no transcript-shaped fiction;
  this host has no whisper → `media.asr` = `WARN … MODEL_MISSING`).
- Video chunks carry t₀/t₁ timeline provenance from frame sampling.

### M9.5 — Embeddings + vector store
- Embedding **provider abstraction** (`EmbeddingProvider`) with a real local
  backend (`OllamaEmbedder` → `/api/embed`) and the pre-existing hash
  embedder explicitly labeled reference/lexical — never presented as neural.
- `probe_embedding` returns an honest status (`AVAILABLE` with model/dims/
  latency, `UNAVAILABLE`, `MODEL_MISSING`); doctor and CLI both consume the
  same probe (one probe, one truth).
- `knowledge.vectors` (derived, rebuildable) wired into the production
  retrieval path (`knowledge semantic-search`, hybrid dense half).

### M9.6 — Hybrid retrieval + reranking
- `openrt-retrieval`: deterministic Okapi BM25 (k1=1.2, b=0.75, always-
  positive IDF) + dense candidates fused by Reciprocal Rank Fusion;
  metadata filters bound BOTH universes; every hit re-validates against the
  persisted store before it is presented.
- Rerankers: deterministic lexical-coverage reranker (always available) and
  a model-backed reranker (real model or honest `UNAVAILABLE` reason).
  Outcome is reported exactly (`applied`/`not requested`/`UNAVAILABLE`),
  rerank never adds/drops citations, never fabricates scores.
- Each hit carries `retrieval_source` (`lexical`/`dense`/`hybrid`) plus
  per-list scores; absent dense backend degrades to honest lexical-only.
- TEST 1 authority untouched: `openrt-knowledge::retrieve`/`answer` remain
  the deterministic path; hybrid is additive and opt-in per command.

### M9.7 — Evidence graph + knowledge graph
- `knowledge.evidence` (append-only): content-addressed `ClaimRecord`s
  (`sha256(text)`) and chained `EvidenceEdge`s (`prev_hash`/`edge_hash`) —
  `ClaimGroundedIn` ↔ companion `PassageExtractedFrom`. Tampering,
  deletion, reordering are detected by `verify_integrity`.
- Production writers: `Runtime::verify_claim`/`verify_claims` record every
  adjudicated claim and link exactly its RESOLVED citations (unresolved
  citations can never become edges — `link_claim` re-validates); idempotent
  on re-verify.
- `knowledge.graph` (evolving, never duplicated): entities/relations via
  CLI `knowledge graph {list,upsert-entity,add-relation}`; `created_at`
  preserved across upserts.
- Citations resolve through `resolve_citation`/`Citation::validate` against
  `corpus.chunks` — the graph never re-implements citation equality.

### M9.8 — Answer verification + hallucination firewall
- Deterministic first: clip evidence (limit), re-resolve citations, compute
  lexical coverage over content tokens (floor `MIN_GROUNDING = 0.50`),
  tier `Grounded`/`Weak`/`Ungrounded`. Ungrounded never spends a model call.
- Model layer: strict-JSON verdicts (`SUPPORTED`/`UNSUPPORTED`/
  `INSUFFICIENT` + reason, verbatim); backends configured explicitly
  (`--model`/`default_model`) — no default model ⇒ honest deterministic-only
  degradation ("no model verifier configured"). Models may only DEMOTE.
- Cross-model: ≥2 distinct model ids → `INDEPENDENT` with backend labels;
  same model twice is deduplicated and reported `DEGRADED`; disagreement
  stays `Insufficient`/`Flag` — never averaged into consensus.
- Limits: max claims (over-limit = error, not truncation), max evidence
  chars (clipped + recorded), wall-clock timeout (recorded failure +
  degradation). Every decision is persisted under `verify.verdicts` BEFORE
  the report returns (claim, evidence refs + resolution, backends,
  independence, limits, reasons — never model secrets).

### M9.9 — Prompt-injection defense
- Shared types in `openrt-core::injection` (`TrustLabel`,
  `InjectionCategory`, `SecurityFlag`, `InjectionReport`) so
  knowledge/retrieval/runtime/evidence can share one trust model without a
  dependency cycle.
- Detector (`openrt-evidence::injection`): deterministic needles over 5
  categories (instruction-override, exfiltration-bait, tool-calls-as-text,
  role-confusion, prompt-extraction), bounded scan (`MAX_SCAN_CHARS`), capped
  flag count; results land on `knowledge.evidence` trust records
  (default `Untrusted`; `mark_operator_trusted` preserves flags).
- Context builder (`openrt_retrieval::context`): every block wrapped
  `<<< UNTRUSTED EVIDENCE … >>` … `<<< /UNTRUSTED EVIDENCE … >>>` with
  document/chunk/page/trust/flags in the header, a policy header before any
  evidence, `[SECURITY WARNING: …]` inside flagged blocks, reserved closing
  `</evidence_context>` tag, optional `exclude_flagged` withholding that
  still reports detections.
- Enforcement: `Runtime::evidence_context` is THE answer-path composer
  (retrieval → load → scan → label → build → chained audit with
  ids/categories only — never corpus text, never the query text).

### M9.10 — Surfaces, doctor, docs
- CLI: `ingest {inspect,extract,list,show,remove,ocr,analyze}`,
  `knowledge {persist-chunk,search|retrieve,semantic-search,hybrid-search,
  answer,evidence,graph}`, `verify {claim,answer}` (exit contract
  **0 pass / 2 flag / 3 block / 1 error**), `vision
  {capabilities,describe,drawing}`, `multimodal capabilities`.
- Doctor rows (statuses `PASS`/`WARN`/`UNAVAILABLE`/`BLOCKED`, never Fail):
  `document.{ocr,render,scan,layout}`, `vision.vlm`, `media.{probe,video,
  asr}`, `embeddings.{backend,config}`, `retrieval.{semantic,hybrid}`,
  `rerank.{deterministic,model}`, `knowledge.graph`, `evidence.graph`,
  `defense.injection`, `verifier.{model,cross}` — all from the same probes
  the library functions use.
- `multimodal capabilities` prints the per-modality intake table plus the
  Phase 9 doctor rows — the same probes, never a second capability claim.

## 5. Storage map (ADR-006)

| Namespace | Writer | Content |
|---|---|---|
| `corpus.documents` | `openrt-ingest` | normalized documents + blocks/provenance |
| `corpus.chunks` | `openrt-knowledge::persist_chunk` | chunks + text + sha256 (citation authority) |
| `knowledge.vectors` | `openrt-vectorindex` | derived dense index (rebuildable) |
| `knowledge.evidence` | `openrt-evidence` (+ `Runtime::evidence_context`, `Runtime::verify_claim`) | append-only claims/edges + trust records/flags |
| `knowledge.graph` | `openrt-evidence::kg` (CLI `knowledge graph`) | evolving entities/relations |
| `verify.verdicts` | `openrt-verifier` | verdict records (never model secrets) |
| ADR-002 audit chain | `openrt-audit` | `evidence.injection_flagged` entries (ids/categories only) |

## 6. Security boundaries

- **Bounded intake** — `IngestJail` (SEC-01 path jail) + `CorpusLimits`;
  malformed/truncated inputs fail closed (`CORRUPT`) with structured errors;
  hostile status gates reject `AMBIGUOUS` with reasons.
- **Sovereign fail-closed** — default mode blocks external network/model
  discovery (Phase 3/6 authorities unchanged); model verification runs only
  when the operator configures a backend (explicit `--model`/provider), and
  no corpus content reaches any endpoint except that configured model
  backend's generate call.
- **No tool execution from model output** — `openrt-verifier` parses model
  output as a strict verdict JSON only; it has no dependency on the tool
  registry/executor (grep: no `openrt-tools`/`ToolRegistry` references).
- **No secrets in records** — verdict records carry claim/evidence refs/
  backends/limits/outcomes; never prompts-as-secrets, keys, or model
  credentials.
- **Injection containment** — hostile text exists only between evidence
  markers; audit ext carries ids/categories only; flagged ≠ trusted.
- **Tamper evidence** — evidence-graph hashes, audit chain, verdict ids.

## 7. Honest degradations on this host (not bugs)

| State | Why | Where surfaced |
|---|---|---|
| `media.asr` = WARN `MODEL_MISSING` | no whisper engine installed | doctor, intake refusal |
| `verifier.model` = UNAVAILABLE | reachable provider but no `default_model` | doctor; `verify claim` runs deterministic-only (degradation printed) |
| `rerank.model` = UNAVAILABLE | same | doctor; CLI `--rerank model` without model says so, keeps fused order |
| dense/hybrid `dense: ready` | needs local Ollama embed (present here) | doctor `embeddings.backend`, CLI hybrid |
| WebUI/TUI phase-specific status | not built for Phase 9 (Phase 8 precedent: no feature exists only inside a UI) | documented limitation |

## 8. Tests and gates

- Integration suites: `p9_ingest` 5, `p9_ocr` 4, `p9_vlm` 6, `p9_media` 3,
  `p9_embeddings` 6, `p9_retrieval` 7, `p9_evidence` 6, `p9_verifier` 7,
  `p9_injection` 5, `p9_conflict` 3, `p9_e2e` 1 → **53**.
- Crate units: `openrt-ingest` 108, `openrt-retrieval` 31,
  `openrt-verifier` 30, `openrt-evidence` 20, `openrt-knowledge` 23,
  `openrt-embeddings` 13, `openrt-vlm` 8, `openrt-vectorindex` 5 → **238**.
- Backend-gated tests print `SKIP: …` and return instead of faking when a
  host lacks Tesseract/pdftoppm/ffmpeg/Ollama (0 skips on this host — all
  real backends present).
- Final gate numbers and the full TEST 1–13 matrix:
  [`PHASE9_ACCEPTANCE.md`](./PHASE9_ACCEPTANCE.md).
