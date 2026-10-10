# SECURITY THREAT MODEL — OpenRT (Phase 13, final)

Phase: **13** (M13.1). This document is the Phase 13 threat model: the
assets we protect, the boundaries we defend, the adversary classes, the
threat table with mitigations and test evidence, and the residual risks we
state honestly. It governs the red-team suite (`p13_security`),
the injection corpus (`p13_injection`) and the final E2E attacks recorded
in [`PHASE13_ACCEPTANCE.md`](./PHASE13_ACCEPTANCE.md).

## 1. Assets

| Asset | Why it matters | Owner authority |
|---|---|---|
| The audit chain | tamper evidence is the basis of every verdict | `openrt-audit` (ADR-002), `Runtime::audit_verify` |
| Operator secrets/keys | credential loss is unrecoverable | `openrt-secrets` (sealed, ChaCha20-Poly1305), `RedactionPolicy` at write |
| The persistence store | every record is authoritative state | one `PersistenceStore` (ADR-006) + digests (ADR-001) |
| Policy / permissions / mode | the rule system itself | `openrt-permissions`/`openrt-policy`/`openrt-modes` (ADR-003) |
| The host machine | arbitrary code execution is game over | `openrt-sandbox` + gate + jail |
| Model/VLM/embedding endpoints | who the agent talks to, from which mode | `net_guard::ensure_url_allowed` + `model_egress` guard (ADR-003/010) |
| Learned artifacts (skills/genome/datasets) | poisoned learning becomes future authority | operator-gated activation (`openrt-workspace-assets`, `openrt-selfimprove`) |
| The operator's attention (demo integrity) | a faked demo is a security claim failure | honest pipeline, reproducibility (`test_h`) |

## 2. Trust boundaries

Everything crossing these boundaries is **untrusted input**:

1. **User/agent prompts** — including model output (models are
   untrusted *advisors*, never authorities).
2. **Documents/PDFs/OCR/media text** — the ingest jail + injection detector.
3. **MCP server descriptions/tool payloads**, **extension manifests/metadata**,
   **skill/genome/dataset packages** — scan-first, deny-by-default.
4. **GUI/browser content** — hostile UI reaching computer-use grounding.
5. **Network responses** (doctor probes, provider replies, marketplace) —
   gated before dialling; responses never become commands without the gate.
6. **Config files** — operator-owned, but scanned (`openrt-shield`) and
   never allowed to weaken deny-by-default silently.

## 3. Adversary classes

- **Adversarial content author** — writes prompts/documents/pages intended
  to override system role, exfiltrate credentials, or summon tool calls.
- **Malicious package author** — extension/MCP/skill/genome/dataset that
  tries to activate or escalate on install.
- **Local co-tenant** — tries path traversal, cross-session reads, checkpoint
  or audit tampering.
- **Network attacker / misconfiguration** — remote endpoints, downgrade of
  Sovereign posture, hidden provider dial-outs.
- **The model itself** (Part F) — must never grant itself permission,
  activate artifacts, disable shield/audit, escape sandbox, enable network,
  or bypass Sovereign mode.
- **Demo/development dishonesty** — a fake backend or test-only stub
  presented as production capability.

## 4. Threat table

| ID | Threat | Mitigation (authority) | Test evidence | Status |
|---|---|---|---|---|
| T-01 | Prompt injection overrides system role / summons tools | `detect_injection` → typed categories → shield finding + gate `RequireApproval` (fails closed); untrusted labels | `p13_01`, `p13_g01`–`g05` (12-category corpus), `test_f` (7 flags in demo) | **Mitigated** |
| T-02 | Encoded (base64) payload hides instructions | bounded scan does **not** decode; item confined by `UNTRUSTED` label — no false "clean" claim | `p13_g01` asserted honest non-detection + `p13_g05` label confinement | **Bounded (documented limit)** |
| T-03 | Malicious extension activates on install | scan (`openrt-shield::scan_extension`) before activation; rejected findings never activate | `p13_02`, `test_b` | **Mitigated** |
| T-04 | Malicious MCP server connects / poisons tools | trust+security gate before connect; payloads merged into the ONE ToolRegistry with `mcp.*` ids and deny-by-default caps | `p13_03`, `test_c` | **Mitigated** |
| T-05 | Tool poisoning / capability escalation | `ToolSpec.required_caps` evaluated on every call by the one gate; default policy deny | `p13_04`, `p13_09` | **Mitigated** |
| T-06 | Secret leakage into audit/telemetry/artifacts | value-pattern redaction at write (`RedactionPolicy`), sealed `openrt-secrets` store | `p13_05` | **Mitigated** |
| T-07 | Sandbox escape / unconfined fallback | unified `openrt-sandbox` authority, fail-safe (no unconfined fallback by default), gate tool→profile step | `p13_06`, `p8_sandbox` | **Mitigated** |
| T-08 | Filesystem traversal out of jail | SEC-01 path jail on every fs tool | `p13_07` | **Mitigated** |
| T-09 | Network escalation in Sovereign mode | `ensure_url_allowed` over all scopes incl. **model**; mode refusal + default-deny netpol; loopback allowed per ADR-003 | `p13_08`, `p13_20`, `p13_21`, `model_egress` unit, CLI/doctor transcripts | **Mitigated** |
| T-09b | **HTTP provider dials out despite Sovereign (found in audit)** | `ModelProvider::base_url` + `EgressGuardedProvider`/`EgressGuardedVision` at assembly; doctor/CLI gates; audit row `model.egress` | `p13_21` (0 inner calls + audit), 5 unit tests, `/tmp/opencode/p13_doctor_ext.log` (0 FAIL, honest WARN/UNAVAILABLE) | **FIXED (`SEC-P13-01`)** |
| T-10 | Model self-authorizes (grant permission, activate skill/plugin, disable shield/audit, escape sandbox, enable network, bypass Sovereign) | model output is text only; planner→capability→permission→gate chain; shield/mode/audit not reachable from model output; egress guard on provider | `p13_10`, `p13_21`, Part F review | **Mitigated** |
| T-11 | Hostile UI manipulates computer-use | resolution chain ends in policy check on `computer.use`; vision grounds only; typed-text/URL scanning; denial + bounded recovery audited | `p13_11`, `test_d` | **Mitigated** |
| T-12 | Checkpoint tampering / blind replay | digest-verified checkpoints; uncertain nodes never blindly replayed; rejection preserves known-good | `p13_12`, `test_e` | **Mitigated** |
| T-13 | Audit chain tamper | ADR-002 backward-walk verify fails closed; audit cannot be disabled by any surface | `p13_13` | **Mitigated** |
| T-14 | Cross-session data leak | one store, namespaced records, per-session/gate scoping | `p13_14` | **Mitigated** |
| T-15 | Malicious artifact/dataset/skill/genome poisoning learning | scan + validate + benchmark + operator approval before activation; datasets through the ONE jailed ingest | `p13_15`–`p13_18` | **Mitigated** |
| T-16 | Fabricated model/training results | honest UNAVAILABLE when no backend; no fabricated metrics | `p13_19` | **Mitigated** |
| T-17 | Hidden bypass path (Sovereign) | deny-by-default everywhere; `p13_20` asserts every documented escape hatch refuses | `p13_20` | **Mitigated** |
| T-18 | Demo-only fake backend / test-only presented as production | demo runs the real pipeline; `MockProvider` documented offline; production-caller audit | `test_f`, `test_h`, acceptance audit tables | **Mitigated** |
| T-19 | Kill switch / halt state tampering | first-tripper-wins kill switch; halt never auto-clears (ADR-008) | doctor `kill-switch` row, `p10_*` regressions | **Mitigated** |
| T-20 | Supply-chain dependency drift | Cargo.lock pinned (400 pkgs), surfaced as doctor `supply-chain.lockfile`; no build.rs; offline builds | doctor row PASS; forensic audit | **Mitigated (recorded:** 7 duplicate dep trees, no offline upgrades**) ** |

## 5. Enforcement invariants (non-negotiables)

1. **One authority per concern** — registry, gate, policy, sandbox, mode,
   audit, shield, evidence, store. Phase 13 wraps, never replaces.
2. **Sovereign fail-closed** — external network (all scopes) refused with a
   clear reason and an actionable fix hint; loopback/file allowed (ADR-003).
3. **High-risk never auto-approves** — RequireApproval fails closed.
4. **Doctor never `Fail`s** — statuses: PASS / WARN / UNAVAILABLE / BLOCKED,
   each with a reason; a gated endpoint is UNAVAILABLE, not probed-then-PASS.
5. **Honest UNAVAILABLE beats fake GREEN** — no backend, no model, no CI,
   no Windows claim.

## 6. Residual risks (stated, not hidden)

- **No hardware attestation / no TEE on this host** — enclave nodes exist
  (ADR-004) but attestation is platform-dependent and reported honestly.
- **No CI** — all gates were executed manually on this host and logged;
  `.github/` deliberately absent (offline, no established remote runner).
- **Detector coverage is heuristic** — unseen injection phrasings may score
  Medium/none; mitigated by untrusted labelling + deny-by-default caps
  (content never carries authority).
- **Loopback providers are reachable by design** (ADR-003) — a hostile
  local process is outside this threat model's boundary.
- **Windows untested** — see Part X; no capability claim.
- **7 duplicate dependency trees** in Cargo.lock — recorded for future
  offline-consistent dedup; not silently ignored, not "fixed" by risky
  offline upgrades.
