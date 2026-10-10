# PHASE 13 IMPLEMENTATION — Security Hardening + SIH Hero Demo (final phase)

Phase: **13** (OWNERSHIP `## Phase 13 — SECURITY HARDENING + SIH HERO DEMO`,
milestones M13.1–M13.18 — the final phase of the roadmap; there is no
Phase 14). Acceptance evidence (gates, capability matrix, tests, audits):
[`PHASE13_ACCEPTANCE.md`](./PHASE13_ACCEPTANCE.md); threat model:
[`SECURITY_THREAT_MODEL.md`](./SECURITY_THREAT_MODEL.md); demo runbook:
[`SIH_HERO_DEMO.md`](./SIH_HERO_DEMO.md); final stack:
[`FINAL_ARCHITECTURE.md`](./FINAL_ARCHITECTURE.md).

## 1. Scope and allocation

Phase 13 hardens the finished system and ships the SIH hero demo **on top of
the frozen Phases 5–12 authorities** — one `ToolRegistry`/gate, one
`openrt-shield`, one `openrt-audit` chain, one `PersistenceStore` (ADR-006),
one mode/netpol stack (ADR-003), one evidence/citation authority. It creates
**no new crate** (the demo/doctor/surfacing work lives in existing
allocations) and **one new runtime-private module** for the network fix:

| Subsystem (milestone) | Crate(s) | Store namespace |
|---|---|---|
| Threat model + red-team suites (M13.1, M13.11) | `openrt-shield`/`openrt-runtime` (consumed, unchanged authority) + `crates/integration-tests/tests/p13_security.rs` | `shield.findings` (existing) |
| Supply-chain posture (M13.2) | `openrt-runtime/src/phase13.rs` doctor rows + shield detectors | `extensions.packages` (existing) |
| Secret hardening (M13.3) | `openrt-core::RedactionPolicy` + `openrt-secrets` (existing, re-audited) | `secrets.*` (existing) |
| Sandbox hardening (M13.4) | `openrt-sandbox` (existing, re-tested) | `sandbox.instances` (existing) |
| Network hardening incl. **model-egress fix** (M13.5) | `openrt-fabric::net_guard` (`SCOPE_MODEL`) + **new** `crates/openrt-runtime/src/model_egress.rs` + `openrt-models` (`ModelProvider::base_url`, `ModelError::EgressRefused`) + doctor/CLI gates | `netpolicy.grants`/`modes.state` (existing); audit action `model.egress` |
| Model security (M13.6) | gate chain (existing) + the egress guard above | `permission.decision` (existing) |
| Prompt-injection corpus (M13.7) | `openrt-evidence::detect_injection` (existing) + corpus tests | `knowledge.evidence` (existing) |
| Computer-use security (M13.8) | `openrt-computer::computeruse` (existing) | — (events audited) |
| Persistence/recovery security (M13.9) | `openrt-checkpoint` + `openrt-audit` (existing) | `exec.checkpoints`, `audit` (existing) |
| Cross-session isolation (M13.10) | `openrt-persistence` scoping (existing) | one store (ADR-006) |
| CyberSaarthi pipeline + dataset (M13.12/M13.13) | `openrt-runtime/src/sih.rs` + `src/sih_data/` | `kb.sih-demo`, `workbench.*`, `corpus.*` (existing namespaces, demo records) |
| SIH CLI demo (M13.14) | `openrt-cli` `sih demo` | — (hermetic `--dir` store) |
| SIH WebUI (M13.15) | `openrt-webui` `GET /api/sih`, `GET /sih`, palette `sih.info` | — (view over the one Runtime) |
| Demo artifacts (M13.16) | artifact system via `sih.rs` | `fabric.artifacts`-style digests via existing artifact records |
| Doctor (Part W) | `openrt-runtime/src/phase13.rs` (8 rows) | — |
| Performance audit (Part V) | measured externally (log `/tmp/opencode/p13_perf.log`) | — |
| Documentation (Part Y/Z) | `docs/architecture/phase13/*`, OWNERSHIP, root `README.md` | — |

Phase 13 adds **no event kinds, no storage namespace, and no ADR**; it adds
one audit action (`model.egress`) as additive data on the existing chained log.

## 2. The honest contract

Phase 13 **does**:

- red-team the shipped system (21 adversarial scenarios + a 12-category
  injection corpus) against the real stack;
- close every network egress gap found — including the one the forensic
  audit found in HTTP model providers (`SEC-P13-01`);
- ship one hermetic, reproducible, synthetic CyberSaarthi investigation
  demo through the REAL pipeline, exposed on CLI + WebUI;
- measure performance honestly (no invented benchmarks);
- produce the final documentation set and capability matrix.

Phase 13 **does not**:

- add a second security authority, fake backend, or demo-only code path;
- modify any Phase 5–12 authority behavior (they stay frozen and green);
- claim Windows support, CI coverage, or hardware attestation;
- run a live model for the demo by default (the offline mock provider is
  documented as such; a real local model is optional and never faked);
- mark any doctor row `Fail`, or hide an UNAVAILABLE behind a fake PASS.

## 3. Key design decisions

**D1 — egress guard wraps the ONE existing authority.** The fix for
`SEC-P13-01` does not invent a second network policy. `net_guard` gained a
`SCOPE_MODEL` constant; `openrt-runtime::model_egress` wraps the assembled
`ModelProvider` (and the VLM) so `generate()`/`ground()` first call
`ensure_url_allowed(url, SCOPE_MODEL, modes, netpol)` — the same function
browser/MCP/database already use. Deterministic providers (no `base_url()`)
are never gated; loopback stays reachable under Sovereign (ADR-003);
external endpoints fail closed with the standard
`POLICY_DENIED … operating mode refused network access … fix: switch to
Provisioning mode (audited)` message. Denials append an audited
`model.egress` row (destination + reason) before returning
`ModelError::EgressRefused`.

**D2 — guard at the assembly point, not at each call site.** The wrapper is
applied once in `RuntimeBuilder` where the provider/`verify_options`
backends/agent/provider-bound vision are constructed, so every downstream
consumer (agent runs, cross-model verify probe, rerank, vision grounding)
inherits the gate with zero call-site churn.

**D3 — doctor probes never bypass the gate.** Doctor's reachability probes
(`vision`, `embeddings`, `ollama`, `backend`, `cross_model`) call
`ensure_url_allowed` *before* spawning any client. A refused endpoint is
reported as `WARN`/`UNAVAILABLE` with the refusal text as the reason —
never probed, never `Fail`. This also fixed a latent honesty bug where
`computer.vision` could show `AVAILABLE` for an endpoint the CLI would
refuse.

**D4 — CLI follows the same authority.** `knowledge semantic-search` /
`hybrid-search` (embedding probe) and `vision *` gate their configured
endpoint through `ensure_url_allowed(…, SCOPE_MODEL, …)` before any
connection, so a Sovereign operator gets a 0.1 s actionable refusal instead
of a socket timeout.

**D5 — injection corpus is honest about its limits.** The detector scans
string surfaces with a bounded budget and classifies into typed
`InjectionCategory`s; it does **not** decode base64 — the encoded item is
asserted to stay *unclean-free* (no false claim) while remaining confined
by its `UNTRUSTED` label. High findings route to `RequireApproval` (gate
fails closed); Medium to `Warn`.

**D6 — the demo is hermetic and synthetic.** `openrt sih demo` builds its
own store/workspace under `--dir`, embeds the synthetic case (marked as
such), uses the offline mock model backend by default, and exits non-zero
if any stage fails. Nothing in the demo bypasses the gate/shield/verify
steps — the demo *exercises* them.

**D7 — doctor rows extend the honest vocabulary only.** The 8 Phase 13 rows
(`security.posture`, `security.redaction`, `secrets.store`,
`network.egress`, `kill-switch`, `persistence.integrity`,
`supply-chain.lockfile`, `sih.demo`) are computed facts (mode posture,
redaction coverage, secret-store presence, egress-policy config, kill-switch
state, store chain verification, lockfile digest, demo dir state) — each
`PASS`/`WARN`/`UNAVAILABLE` with a reason, never `Fail`.

## 4. Surfaces

| Surface | Addition |
|---|---|
| CLI | `openrt sih demo [--dir DIR] [--json] [--query Q]` — hermetic end-to-end run; nonzero exit on any stage failure |
| CLI (security) | `knowledge semantic-search` / `hybrid-search`, `vision *` now pre-gate their model/embedding/vision endpoint (POLICY_DENIED + mode hint in Sovereign) |
| WebUI | `GET /api/sih` (JSON dashboard), `GET /sih` (page), palette command `sih.info` |
| Doctor | 8 new rows (Part W) + egress-gated probe honesty (Part E) |
| TUI | no new panel (Phase 13 is CLI/WebUI/doctor-facing; no feature exists only inside a UI — the demo is a CLI command and a WebUI route, both over the one Runtime) |
| Docs | `docs/architecture/phase13/` five documents, OWNERSHIP `## Phase 13` + change log, root `README.md` |

## 5. Test architecture

| Layer | Suite | Count | What it proves |
|---|---|---|---|
| Red-team | `p13_security.rs` (`p13_01`–`p13_21`) | 21 | injection denial+audit, malicious extension/MCP/tool poisoning, secret non-leakage, sandbox containment, traversal denial, network escalation denial, permission fail-closed, model cannot self-authorize, computer-use hostile input, checkpoint/audit tamper, cross-session isolation, malicious artifact/dataset/skill/genome, no fabricated model experiment, Sovereign bypass impossible, **model egress fail-closed** |
| Injection corpus | `p13_injection.rs` (`p13_g01`–`p13_g05`) | 5 | 12-category corpus (system override, role confusion, tool-call injection, credential exfiltration, hidden/encoded instructions, malicious PDF via real text-layer extractor, OCR, VLM, browser page, MCP description, extension metadata) → detect/classify/label/constrain/never-grant + benign non-false-positive |
| E2E | `p13_e2e.rs` (`test_a`–`test_h`) | 8 | detection/denial/audit chain, plugin scan-reject, MCP trust gate, computer-use denial+recovery, tampered checkpoint rejection, **SIH hero flow**, full mega-chain, **capstone reproducibility** |
| Egress unit | `openrt-runtime::model_egress` | 5 | sovereign refusal with 0 provider calls + audit row; loopback reachable; deterministic provider ungated; provisioning+grant opens; vision gated by same authority |
| WebUI | `openrt-webui/tests/webui.rs` | 29 | palette incl. `sih.info`, `/api/sih`, token auth unchanged |

Full-workspace regression (Phases 1–12 untouched): **1517 passed /
0 failed / 0 ignored**, 187 binaries — see
[`PHASE13_ACCEPTANCE.md`](./PHASE13_ACCEPTANCE.md) gates table.

## 6. The Part E finding and fix (`SEC-P13-01`)

The forensic audit's network review found that `ensure_url_allowed` was
wired for browser/MCP/database/marketplace/telemetry but **HTTP model
providers (ollama/OpenAI-compatible/llamacpp/vllm) and the vision endpoint
were never mode-gated** — a Sovereign operator's configured external
endpoint would have been dialled directly. Remediation:

1. `openrt-models::ModelProvider::base_url() -> Option<String>` (default
   `None`) — implemented by every HTTP provider (`ollama`, `llamacpp`,
   `vllm`).
2. `ModelError::EgressRefused(String)` — permanent-class errors unchanged.
3. `openrt_fabric::net_guard::SCOPE_MODEL`.
4. `openrt-runtime::model_egress::{EgressGuardedProvider, EgressGuardedVision}`
   — gate → audit (`model.egress`, destination, reason) → refuse; wrappers
   applied at runtime assembly (providers, verify backends, vision).
5. Doctor probe gates (`vision`, `embeddings`, `ollama`, `backend`,
   `cross_model`) + `computer.vision` row gate + CLI knowledge/vision gates.
6. Tests: `p13_21` (integration: external refused, 0 inner calls, audit
   row, loopback second-runtime allowed) + 5 unit tests + live CLI/doctor
   e2e transcripts (`/tmp/opencode/p13_doctor_ext.log`,
   `/tmp/opencode/p13_final_doctor.log`).

## 7. Part X — deployment / platform honesty

Verified on Linux (this host): build, fmt, check, all gates, doctor, demo.
Windows: **untested, no claim** — code compiles against std only in places,
but the sandbox (bubblewrap), computer control (X11/Wayland), and process
supervision paths are Linux-specific; the capability matrix in
[`PHASE13_ACCEPTANCE.md`](./PHASE13_ACCEPTANCE.md) states this explicitly.
