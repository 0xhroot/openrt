# PHASE 13 ACCEPTANCE — Security Hardening + SIH Hero Demo (final phase)

Phase: **13** (OWNERSHIP `## Phase 13 — SECURITY HARDENING + SIH HERO DEMO`,
M13.1–M13.18 — the final phase; no Phase 14). Design/implementation notes:
[`PHASE13_IMPLEMENTATION.md`](./PHASE13_IMPLEMENTATION.md); threat model:
[`SECURITY_THREAT_MODEL.md`](./SECURITY_THREAT_MODEL.md); demo:
[`SIH_HERO_DEMO.md`](./SIH_HERO_DEMO.md); architecture:
[`FINAL_ARCHITECTURE.md`](./FINAL_ARCHITECTURE.md).

## Final status

**PHASE 13 COMPLETE.**

Every condition of the FINAL STATUS RULE holds on this host (Linux x86_64,
offline, 2026-09-28):

- **Security hardening complete** — the red-team suite (`p13_security`
  21/21), the adversarial injection corpus (`p13_injection` 5/5) and the
  mandatory final E2E attacks (secret leakage / disabled audit / network
  bypass / sandbox bypass / demo-only fake backend / test-only-as-production)
  are green; **no critical bypass remains** — the one found during the
  forensic audit (HTTP model providers were never mode-gated: Part E
  `model providers` gap, `SEC-P13-01`) is closed with an enforced
  `ModelProvider::base_url` egress guard (loopback still allowed per
  ADR-003; external endpoints fail closed in Sovereign with an audited,
  actionable reason).
- **SIH hero workflow works** — `openrt sih demo` runs the complete
  CyberSaarthi investigation end-to-end through the REAL pipeline (RC=0,
  5 artifacts, verdict `Supported`/`Pass`, chain verified) and the WebUI
  serves the same dashboard from the one Runtime.
- **Demo is reproducible** — two independent clean runs agree on every
  stage count, verdict, coverage, injection finding and artifact set
  (`p13_e2e::test_h_capstone_reproducible_demo`).
- **Final documentation complete** — 5 Phase 13 documents + OWNERSHIP
  Phase 13 section + root README (this file's Part Y checklist).
- **Phase 5–12 regressions green** — the full workspace run is
  **1517 passed / 0 failed / 0 ignored** (187 test binaries, RC=0), which
  includes every Phase 5–12 suite unmodified.
- **Final doctor has 0 FAIL** — 86 rows: **73 PASS / 1 WARN /
  12 UNAVAILABLE / 0 FAIL** (the WARN is the pre-existing Phase 9
  `media.asr MODEL_MISSING`; UNAVAILABLE rows are the honest pre-existing
  probes plus Phase 12/13 additions — never silent, never `Fail`).
- **Capability matrix complete** — M13.1–M13.18 below, all verdicts
  explicit; genuinely absent things are marked UNAVAILABLE, not hidden.

## FINAL CAPABILITY MATRIX (M13.1–M13.18)

| Milestone | Capability | Verdict | Production path | Security | Test |
|---|---|---|---|---|---|
| **M13.1** | Threat model: assets, trust boundaries, threat table, residual risks | **COMPLETE** | `docs/architecture/phase13/SECURITY_THREAT_MODEL.md` (governs the Phase 13 checks; no new runtime authority) | Written against the one shield/gate/audit/mode stack; includes finding `SEC-P13-01` + fix | Reviewed against forensic audit (below) |
| **M13.2** | Supply-chain hardening: lockfile/dependency posture surfaced, malicious package detection | **COMPLETE** | doctor `supply-chain.lockfile` (`phase13.rs`), `openrt-shield` dependency/content detectors over `extensions.packages` | Cargo.lock digest tracked as a doctor PASS row; malicious artifact/package rejected before activation | `p13_15_malicious_artifact_detected`, `p13_16/17/18`; doctor row PASS |
| **M13.3** | Secret hardening: value-pattern redaction at write, no secret on durable/egress surfaces | **COMPLETE** | `openrt_core::RedactionPolicy` applied in `Runtime::memory_remember`/`seal_record` paths; `openrt-secrets` sealed store | Redaction before persistence; secrets never reach audit/telemetry/artifacts | `p13_05_secrets_never_reach_durable_surfaces`; doctor `security.redaction`, `secrets.store` PASS |
| **M13.4** | Sandbox hardening: escape contained, tool→profile step enforced | **COMPLETE** | `openrt-sandbox` authority + `GateExecutor` sandbox-requirement step (Phase 8) | One sandbox policy path; deny-by-default; fail closed | `p13_06_sandbox_escape_contained`; doctor `sandbox` row |
| **M13.5** | Network hardening: every network client routed through mode+netpol, Sovereign fail-closed | **COMPLETE** | `openrt_fabric::net_guard::ensure_url_allowed` at browser/MCP/database/marketplace/telemetry + **`openrt-runtime::model_egress`** at provider/VLM/doctor/CLI (Part E fix) | External denied, **no fallback, no hidden provider**, clear reason + fix hint; loopback allowed (ADR-003) | `p13_08`, `p13_20`, `p13_21_model_egress_sovereign_fail_closed`, `model_egress` unit 5/5, CLI e2e (0.16s `POLICY_DENIED`, no socket), doctor `network.egress` |
| **M13.6** | Model security: model output can never grant itself authority | **COMPLETE** | Model → planner → `ToolRegistry` → gate (`Capability`/`PolicyEvaluator`/`KillSwitch`/mode) → execution; egress guard added in Part E | Model cannot self-authorize, activate skill/plugin, disable shield/audit, escape sandbox, enable network, or bypass Sovereign | `p13_10_model_cannot_self_authorize`, `p13_21`, `model_egress` tests |
| **M13.7** | Prompt-injection hardening: 12-category corpus detect → classify → label → constrain → never grant authority | **COMPLETE** | `openrt-evidence::detect_injection` + `TrustLabel`/`SecurityFlag` consumed by shield + gate + retrieval context | High → RequireApproval fails closed; Medium → Warn; bounded scan; honest non-detection for undecodable payloads (base64 stays UNTRUSTED-confined) | `p13_injection` 5/5 (corpus, hostile PDF via real extractor, OCR, MCP-description + extension-metadata, label/authority), `p13_01` |
| **M13.8** | Computer-use security: hostile UI fails closed through grounding→policy→denial→recovery | **COMPLETE** | `openrt-computer::computeruse` resolution chain + gate on `computer.use` | Vision grounds only (never executes); typed-text/URL scanning; policy denial audited | `p13_11`, E2E `test_d` |
| **M13.9** | Persistence/recovery security: tampered checkpoint rejected, known-good preserved, audit chain tamper-evident | **COMPLETE** | `openrt-checkpoint` + `openrt-audit` chain verify (`Runtime::audit_verify`, ADR-002/001) | Digest-chain verification fail-closed; rollback operator-gated | `p13_12_checkpoint_tamper_rejected`, `p13_13_audit_tamper_detected`, E2E `test_e` |
| **M13.10** | Cross-session isolation | **COMPLETE** | One `PersistenceStore` with namespaced records + per-session/gate scoping (ADR-006) | No cross-session read/write leakage | `p13_14_cross_session_isolation` |
| **M13.11** | Security red-team: full adversarial suite (injection, malicious extension/MCP/tool, secrets, sandbox, traversal, escalation, tamper, isolation, dataset/skill/genome poisoning, Sovereign bypass, model egress) | **COMPLETE** | Every scenario runs against the real Runtime/gate/shield/audit stack | 21 scenarios, all deny/contain + audit evidence | `p13_security` **21/21** |
| **M13.12** | CyberSaarthi integration: complete synthetic fraud-network investigation through the real authorities | **COMPLETE** | `openrt-runtime::sih` (`run_demo`/`dashboard`) over ingest→graph→retrieve→workflow→verify→shield→artifacts→audit | Sovereign posture asserted; hostile note flagged; verdict evidence-linked | E2E `test_f_sih_hero_flow`, `test_h_capstone_reproducible_demo`, CLI gate run RC=0 |
| **M13.13** | SIH demo dataset: embedded, synthetic, marked | **COMPLETE** | `openrt-runtime/src/sih_data/` (case + 9 entities + 10 relations + 4 documents incl. `hostile_note.md`) | Everything marked synthetic; no real personal data | report counts (4 docs / 24 chunks / 9 entities / 10 relations) asserted in `test_f`/`test_h` |
| **M13.14** | SIH CLI demo: one command, hermetic, non-zero on any stage failure | **COMPLETE** | `openrt sih demo [--dir --json --query]` (own store + workspace under `--dir`, documented mock provider offline) | Never a fake success; exits non-zero on stage failure; runs Sovereign | Gate run: RC=0, transcript `/tmp/opencode/p13_final_sih.log` |
| **M13.15** | SIH WebUI: dashboard over the same Runtime (no UI-local authority) | **COMPLETE** | `GET /api/sih`, `GET /sih`, palette `sih.info` (`openrt-webui`) | Server reads the one Runtime; no client-asserted state | `openrt-webui/tests/webui.rs` **29/29** |
| **M13.16** | Demo artifacts: investigation report, evidence summary, relationship graph, verification report, audit summary | **COMPLETE** | Existing artifact system (`sih.rs` writes 5 files under `<dir>/artifacts/` with recorded sha256) | Artifacts carry digests + provenance; content scanned | report `artifacts` (5 × sha256) asserted in `test_f`/`test_h` |
| **M13.17** | Reproducibility: independent clean runs agree; no hidden state | **COMPLETE** | `test_h` two clean runs compare every stage + artifact set; dashboard `chain_verified` | Injection flags and chain verification asserted in both runs | `test_h_capstone_reproducible_demo` |
| **M13.18** | Final architecture + documentation | **COMPLETE** | `FINAL_ARCHITECTURE.md` (Part Z stack + domain map), this acceptance doc, `SECURITY_THREAT_MODEL.md`, `SIH_HERO_DEMO.md`, `PHASE13_IMPLEMENTATION.md`, OWNERSHIP `## Phase 13`, root `README.md` | Docs state honest limitations (Linux verified; Windows untested — no claim) | Part Y checklist complete |

## FINAL TEST MATRIX

Counts are the dedicated integration suites from the full workspace run
(`/tmp/opencode/p13_final_tests.log`: **1517 passed / 0 failed / 0 ignored**,
187 `test result: ok` lines, RC=0 — which additionally includes every crate's
unit/doc tests, Phases 1–4 included).

| Area | Tests | Result | Security | Evidence |
|------|-------|--------|----------|----------|
| Phase 5 regression | 6 integration (`p5_workbench`) + Phase 5 units | **green, 0 failed** | Hooks consult the ONE `Policy`; approval-gated improve; CSPRNG tokens; seal redaction | `p13_final_tests.log` |
| Phase 6 regression | 38 (`p6_milestones` 17, `p6_workflow_compiler` 11, `p6b_production` 10) + units | **green, 0 failed** | Deny-by-default policy engine, sealed secrets, default-deny netpol, gated enclave/observability, kill-scope reserved | `p13_final_tests.log` |
| Phase 7 regression | 32 (`p7_tool_fabric` 8, `p7b_*` 24) + units | **green, 0 failed** | Tool Fabric/registry deny-by-default caps; MCP untrusted deny; browser netpol | `p13_final_tests.log` |
| Phase 8 regression | 7 (`p8_sandbox`) + units | **green, 0 failed** | Unified sandbox authority, network modes, gate tool→profile step | `p13_final_tests.log` |
| Phase 9 regression | 53 (`p9_*` 11 suites) + units | **green, 0 failed** | Evidence/citation authority, injection defense, honest UNAVAILABLE backends, TEST 1 deterministic RAG | `p13_final_tests.log` |
| Phase 10 regression | 64 (`p10_*` 8 suites) + units | **green, 0 failed** | AgentShield one policy path, recovery records, operator-gated skills/genome | `p13_final_tests.log` |
| Phase 11 regression | 58 (`p11_*` 6 suites) + units | **green, 0 failed** | Extension scan-before-activate, MCP trust gate, computer-use policy denial | `p13_final_tests.log` |
| Phase 12 regression | 55 (`p12_integration` 47, `p12_e2e` 8) + units | **green, 0 failed** | Trading execution refusal (rc=3), advisory industrial outputs, rollback evidence | `p13_final_tests.log` |
| Phase 13 security (red-team + corpus) | **26** (`p13_security` 21, `p13_injection` 5) + `model_egress` unit 5 | **green, 0 failed** | Every scenario denies/contains with audit evidence; 12-category corpus; Part E egress fail-closed | `p13_final_tests.log` |
| SIH E2E (hero demo) | **8** (`p13_e2e` test_a–test_h, incl. `test_f` SIH flow + `test_h` capstone) | **green, 0 failed** | Sovereign dashboard, hostile-content flags, chain verified, reproducible | `p13_final_tests.log`; CLI run `/tmp/opencode/p13_final_sih.log` |
| WebUI cross-surface | 29 (`webui.rs`, incl. `sih.info` palette + `/api/sih`) | **green, 0 failed** | Server-issued token auth unchanged; palette over the one Runtime | `p13_final_tests.log` |

## MANDATORY FINAL E2E TESTS

| Attack | Requirement | Evidence | Result |
|---|---|---|---|
| Secret leakage | no secret on durable/egress surfaces | `p13_05_secrets_never_reach_durable_surfaces` (audit/telemetry/artifact scans); `RedactionPolicy` at write; forensic grep (no hardcoded keys) | **PASS** |
| Disabled audit | audit cannot be disabled; tamper detected | `p13_13_audit_tamper_detected` (chain verify fails closed); no code path disables `openrt-audit`; SIH demo asserts `chain_verified > 0` | **PASS** |
| Network bypass | external denied → no fallback → no hidden provider → clear reason | `p13_08` (SOVEREIGN escalation denied), `p13_20` (bypass impossible), `p13_21` + `model_egress` 5/5 (provider/VLM never called; `POLICY_DENIED` + ADR-003 reason + mode hint), CLI e2e (doctor/knowledge/vision gated in 0.12–0.16 s, no socket) | **PASS** |
| Sandbox bypass | escape contained, one policy path | `p13_06_sandbox_escape_contained`; Phase 8 gate step unmodified | **PASS** |
| Demo-only fake backend | no fake-success path in the demo | `test_f`/`test_h` run the REAL pipeline (ingest→graph→retrieve→workflow→verify→shield→artifacts→audit over the one Runtime); the mock provider is the documented offline model backend, not a fake demo backend | **PASS** |
| Test-only implementation presented as production | every capability has a production caller | Forensic + production-caller audits (below): constructors test-only; all Phase 13 capabilities reachable from CLI/doctor/WebUI over the one Runtime | **PASS** |

## PRODUCTION CALLER AUDIT (per Phase 13 capability)

| Capability | Implementation | Production caller | Security authority | Persistence | Integration test | E2E evidence |
|---|---|---|---|---|---|---|
| Threat model / red-team checks | shield detectors + gate | Runtime gate step on every tool call | `openrt-shield` + `GateExecutor` (one policy path) | `shield.findings` | `p13_security` 21 | `test_a`, `test_g` |
| Injection corpus defense | `openrt-evidence::detect_injection` | retrieval context builder + shield + gate (`defense.injection`) | evidence trust labels; RequireApproval fails closed | `knowledge.evidence` flags | `p13_injection` 5 | `test_f` (7 flags) |
| Secret redaction | `openrt_core::RedactionPolicy` | `Runtime::memory_remember`, asset/improve seal, audit records | redaction-at-write | one store | `p13_05` | `test_g` |
| Network/model egress gate | `net_guard::ensure_url_allowed` + `EgressGuardedProvider`/`EgressGuardedVision` | runtime assembly (agents, `verify_options` backends, vision), doctor probes, CLI `knowledge`/`vision` | `ModeManager` (ADR-003) + `NetworkPolicy` default-deny | audit row `model.egress` | `p13_21`, `model_egress` unit | doctor ext-config gate log `/tmp/opencode/p13_doctor_ext.log` |
| Supply-chain posture | `phase13.rs` doctor rows | `openrt doctor` | shield scan + lockfile digest | `extensions.packages` | doctor rows in `test_h` | `openrt doctor` RC=0 |
| CyberSaarthi pipeline | `openrt-runtime::sih` | CLI `openrt sih demo`, WebUI `GET /api/sih` + palette `sih.info` | gate/shield/verify/audit unchanged underneath | one store (`kb.sih-demo`, workbench records, artifacts) | `test_f`, `test_h` | CLI transcript `/tmp/opencode/p13_final_sih.log` (RC=0) |

## FORENSIC AUDIT (mission search list)

| Search item | Finding | Evidence | Status |
|---|---|---|---|
| Duplicate authority | none — Phase 13 adds no second registry/gate/policy/audit/store/shield; the egress guard WRAPS the existing mode+netpol check rather than inventing a new one | OWNERSHIP Phase 13 table; `model_egress.rs` delegates to `ensure_url_allowed` | **CLEAR** |
| Bypass path | **one found and closed**: HTTP model providers/VLM were never mode-gated (`SEC-P13-01`) | trait `base_url()` + `EgressGuardedProvider`/`EgressGuardedVision`; doctor/CLI gates | **FIXED + tested** |
| Raw model → privileged action | impossible: model output is text; planner→capability→permission→gate; egress path gated | `p13_10`, `p13_21` | **CLEAR** |
| Raw VLM → action | VLM grounds only; `ground()` endpoint gated by the same authority; never executes | `p13_11`, `model_egress` vision test, `test_d` | **CLEAR** |
| Plugin → host bypass | scan before activation; deny-by-default caps; no activation on findings | `p13_02`, `test_b` | **CLEAR** |
| MCP → host bypass | trust/security gate before connect; untrusted denied; payload merged into the one ToolRegistry with `mcp.*` ids | `p13_03`, `test_c` | **CLEAR** |
| GUI → security bypass | `computer.use` runs through the one gate; typed-text/URL scanning; vision executes nothing | `p13_11`, `test_d` | **CLEAR** |
| Secret leakage | value-pattern redaction at write; no hardcoded secrets/build.rs; constructor hits test-only | `p13_05`; grep audits | **CLEAR** |

Additional audit results: `KillSwitch`/`ToolRegistry::new`/`AuditLog::new`
constructors appear only in tests/build wiring (never on a bypass path);
no build scripts; Cargo.lock has 400 pinned packages (7 duplicate trees
— recorded, no offline upgrades performed); `MockProvider` is documented
as the offline demo backend (honest, never presented as a live model);
enclave decorative `let _ = ensure_network_allowed()` still always refuses.

## Gates (2026-09-28, this host)

| Gate | Command | Result | Evidence |
|---|---|---|---|
| Format | `cargo fmt --all -- --check` | **RC=0** | `/tmp/opencode/p13_final_fmt.log` |
| Check + warnings | `cargo check --offline --workspace --all-targets` | **RC=0, 0 warnings** | `/tmp/opencode/p13_final_check.log` |
| Tests | `cargo test --workspace --offline --no-fail-fast` | **1517 passed / 0 failed / 0 ignored**, 187 binaries, **RC=0** | `/tmp/opencode/p13_final_tests.log` |
| Phase 13 suites (targeted re-run) | `p13_security` + `p13_injection` + `p13_e2e` | **34/34, RC=0** (21 + 5 + 8) | `/tmp/opencode/p13_final_suites.log` |
| SIH demo E2E | `openrt sih demo --dir … --json` | **RC=0** (verdict Supported / decision Pass / 5 artifacts / chain verified) | `/tmp/opencode/p13_final_sih.log` |
| Doctor | `openrt doctor` | **RC=0; 86 rows: 73 PASS / 1 WARN / 12 UNAVAILABLE / 0 FAIL** | `/tmp/opencode/p13_final_doctor.log` |
| Doctor (hostile external config) | `OPENRT_CONFIG=… openrt doctor` | **0 FAIL** — provider/vision/embeddings/verifier/computer-vision rows honest `WARN`/`UNAVAILABLE` with the ADR-003 refusal (never probed) | `/tmp/opencode/p13_doctor_ext.log` |
| Resource gate | full gates after `df`/memory check | satisfied (memory ~58% free during runs) | — |

Established lint/security gates in this repository are fmt/check/tests/doctor
(run above; `clippy.toml`/`deny.toml` exist but were never established gates —
no CI exists to run them on; noted honestly under limitations).

Phase 12 baseline was **1477** tests; Phase 13 brings the workspace to
**1517** (+40: `p13_security` 21, `p13_injection` 5, `p13_e2e` 8,
`model_egress` unit 5, WebUI SIH route 1) with **zero regressions**.

WARN row: `media.asr MODEL_MISSING` (pre-existing Phase 9 host condition).
UNAVAILABLE rows: the 12 pre-existing honest probes + Phase 12/13 additions
(e.g. `learning.model-adaptation`, `memory.prefetch`, `verifier.cross`,
`computer.vision` when its endpoint is gated) — each carries its reason.

## Part U — UX polish (justified changes only)

No cosmetic rewrites; the Phase 13 UX work is limited to:

- **Clear status** — doctor rows and the SIH dashboard use the honest
  vocabulary (`PASS`/`WARN`/`UNAVAILABLE`/`BLOCKED` + reason, never `Fail`).
- **Clean errors** — egress refusals are actionable:
  `POLICY_DENIED: egress to '…' — why: … (ADR-003) — fix: switch to
  Provisioning mode (audited) …` instead of a raw socket error.
- **Obvious evidence provenance** — demo report/verification carry
  citations, grounding coverage (1.000), and per-artifact sha256.
- **Understandable security state** — `openrt mode status`, `network.egress`
  doctor row, and audited `model.egress` denials make the posture visible.
- **Fast demo path** — one command, ~0.8 s (Part V below).
- **Consistent terminology** — `sih demo` / `sih.info` / `/api/sih` all say
  the same words the TUI/CLI use elsewhere.

## Part V — performance / resource audit (measured only)

Method: Python `resource.getrusage(RUSAGE_CHILDREN)` around the child
process (no `/usr/bin/time` on this host). Log: `/tmp/opencode/p13_perf.log`.

| Command | Wall | Peak RSS |
|---|---|---|
| `openrt --version` | 0.034 s | 14.3 MiB |
| `openrt doctor` | 0.526 s | 68.0 MiB |
| `openrt sih demo --json` (fresh store) | 0.736 s | 68.9 MiB |
| `openrt sih demo --json` (rerun) | 0.837 s | 69.0 MiB |

Internal demo timings (fresh run): prerequisites 141 ms, ingest 9 ms,
chunk 11 ms, graph 13 ms, materials 9 ms, retrieve 3 ms, workflow 34 ms,
verify 20 ms, injection 14 ms, artifacts 19 ms, **total 292 ms**
(CLI wall 0.736 s includes process startup + store open).
Peak VRAM: **N/A** (no GPU model backend exercised; honest — nothing measured
on device). Artifact generation: 19 ms inside the demo. No invented benchmarks.

## Definition of Done (OWNERSHIP Phase 13 rows)

- [x] One authority per subsystem — no second shield/gate/audit/mode/store.
- [x] Every Phase 13 capability has a production caller (CLI/doctor/WebUI),
      persistence where applicable, an integration test and E2E evidence.
- [x] Sovereign fail-closed holds across all eight network scopes
      (browser/database/mcp/**model**/…), loopback allowed per ADR-003.
- [x] Doctor rows never `Fail`; statuses only from the honest vocabulary.
- [x] Red-team suite + injection corpus green; `SEC-P13-01` closed.
- [x] SIH hero demo green, deterministic-artifact set, reproducible.
- [x] Phase 5–12 frozen and green; workspace **1517/0**, fmt/check RC=0.
- [x] 5 documents + OWNERSHIP `## Phase 13` + root `README.md`.

## Honest limitations (Part X — deployment check)

**Linux (this host): VERIFIED** — all gates above ran natively
(x86_64, offline, local SQLite/file store, loopback Ollama present for
optional local probes).

**Windows: UNTESTED — no claim.** The codebase is Linux-targeted
(bubblewrap sandbox, `/proc`, X11/Wayland computer control). Compilation
elsewhere is not a capability claim; nothing in these documents asserts
Windows functionality.

Other honest UNAVAILABLE/limits:

- `media.asr` WARN (no whisper on host), `verifier.model`/`rerank.model`
  UNAVAILABLE without a configured default model — unchanged from Phase 9.
- Injection detector does not decode base64 payloads (item stays
  UNTRUSTED-confined; non-detection is asserted, not hidden).
- VLM/vision availability is host-dependent; egress-gated endpoints are
  reported `UNAVAILABLE`/`WARN` with the refusal reason, never probed.
- Demo uses the documented offline mock model backend; a live model is
  optional and never faked.
- No CI exists (no `.github/`); all gates were run manually on this host
  and recorded above.
