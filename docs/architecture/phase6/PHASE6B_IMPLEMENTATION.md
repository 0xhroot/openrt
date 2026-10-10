# Phase 6B — Implementation

**Status:** COMPLETE  
**Date:** 2026-09-24  
**Predecessor:** `PHASE6_ACCEPTANCE_M61_M66.md` (brief M6.1–M6.6 workflow slice — frozen)  
**Roadmap:** `docs/roadmap/ROADMAP.md` Phase 6 (M6.1 remainder through M6.4; M6.5–M6.8 trading optional; M6.9 gate)  
**Governing ADRs:** ADR-002 (audit envelope frozen v1), ADR-003 (Sovereign fail-closed / signed offline bundles), ADR-004 (enclave-only), ADR-005 (single telemetry pipeline), ADR-006 (one foundation store), ADR-007 (naming), ADR-008 (kill scopes), ADR-010 (model fabric)

---

## 1. Scope

Phase 6B completes the remaining roadmap Phase 6 work **on top of the frozen Phase 6A compiler/scheduler foundation**. It does **not** rebuild or redesign the DAG engine, scheduler, or compiler. Trading (M6.5–M6.8) remains roadmap-optional per §8 of the roadmap and is **scoped out of this release** without blocking the rest of Phase 6 (only the reserved `KillScope::Trading` classification ships).

| Milestone | Roadmap | Owner crate(s) | Status |
|---|---|---|---|
| M6.1 remainder — autonomous unattended runs | Long-running agents / autonomous workflows | `openrt-supervisor` (`exec.autonomous`) | GREEN |
| M6.2 — security policy engine + audit strengthen | Hardened sandbox + policy + audit | `openrt-policy` (`policy.engine`), `openrt-audit` (additive) | GREEN |
| M6.3 — secrets + netpol + air-gap + supply-chain | Encrypted secrets / isolation / offline | `openrt-secrets`, `openrt-netpol`, `openrt-modes` + doctor air-gap, `openrt-verify`/`openrt-scan` | GREEN |
| M6.4 — enclave + federation + observability | Distributed workers + model migration + obs | `openrt-enclave`, `openrt-observability` | GREEN |
| M6.5–M6.8 — trading marks 0–3 | Optional high-risk domain | `openrt-trading` (not allocated) | **Scoped out** (roadmap §8 preserves optionality) |
| M6.9 — final production/security audit + phase gate | Gate | This doc + `PHASE6B_ACCEPTANCE.md` | GREEN |

### Architectural rules enforced

- One authority each: single `PolicyEvaluator`/`Arc<Policy>`, single `ModeManager` (network), single `WorkflowEngine` (scheduling), single `PersistenceStore`, single telemetry pipeline (ADR-005).
- No second permission/policy/scheduler/secrets/netpol/sandbox engine.
- Fail-closed + Sovereign defaults preserved.
- Every 6B entry point has a production caller (Runtime method and/or CLI) and real integration tests.

---

## 2. New crates and storage namespaces (ADR-006/007)

| Crate | Namespace(s) | Role |
|---|---|---|
| `openrt-policy` | `policy.engine` | Versioned, append-only deny-by-default policy document; converts to the same `Policy` the gate already enforces. |
| `openrt-secrets` | `secrets.data`, `secrets.keys` | ChaCha20-Poly1305 sealed secrets at rest; key material outside ciphertext (0600 keyfile / keyring path). |
| `openrt-netpol` | `netpolicy.grants` | Default-deny egress grant records; enforcement always consults `ModeManager`. |
| `openrt-enclave` | `enclave.nodes`, `enclave.leases`, `enclave.registry` | Mutual node identity, step leases over the sole scheduler, quarantine, verified model migration. |
| `openrt-observability` | `telemetry.retention`, `telemetry.health`, `telemetry.export` | Retention/health/export over the existing telemetry event log (no parallel pipeline). |
| `openrt-supervisor` | `exec.autonomous` | Unattended run envelopes: stop conditions, approval mats, lease + takeover, operator stop. |

Workspace members and `openrt-cli` / `openrt-runtime` / `integration-tests` dependency edges are wired in the root `Cargo.toml`.

---

## 3. Production callers

### Runtime façade (`crates/openrt-runtime/src/runtime.rs`)

| Method | Domain |
|---|---|
| `audit_verify` | M6.2 chain verification |
| `audit_query(&AuditQuery)` | M6.2 search/aggregation |
| `audit_retention` | M6.2 chain head/tail/total |
| `policy_document` | M6.2 load current revision (fail-closed if absent) |
| `policy_document_ensure` | M6.2 bootstrap deny-by-default |
| `policy_append_rule` | M6.2 operator append + audit |
| `policy_versions` | M6.2 revision list |
| `secrets()` | M6.3 open sealed store over keyfile |
| `netpol()` | M6.3 default-deny grants over ModeManager |
| `enclave()` | M6.4 nodes/leases/migration |
| `observability()` | M6.4 health/retention/export |
| `supervisor(holder)` | M6.1 remainder autonomous runs |
| `audit(actor, action, subject, result, ext)` | shared ADR-002 append |

### Doctor (`crates/openrt-runtime/src/doctor.rs`)

- **audit** — production `AuditLog::verify()` (ADR-002).
- **air-gap** — Sovereign = Pass; Provisioning = Warn; unreadable mode = Fail.
- **policy-doc** — load current, else bootstrap `policy.engine` deny-by-default.

### Gate (`crates/openrt-runtime/src/gate.rs`)

Evaluates tool id **and every `spec().required_caps`** capability. `Deny` short-circuits; `Ask` goes through `resolve_ask` and emits combined `PermissionGranted`/`Denied`/`Approved` events. `extract_scope_public` re-exported from `openrt-permissions`.

### CLI (`crates/openrt-cli/src/main.rs`)

New top-level commands and subcommands:

- `audit verify|query|retention`
- `policy show|init|append|versions`
- `secret set|get|delete|list|rotate`
- `netpol grant|revoke|list|check`
- `enroll` / `nodes` / `quarantine` / `migrate` under `enclave`
- `obs health|retention|export`
- `autonomous create|list|stop|takeover`

Handlers: `cmd_audit`, `cmd_policy`, `cmd_secret`, `cmd_netpol`, `cmd_enclave`, `cmd_obs`, `cmd_autonomous`. Operator identity via `actor_user()` (`USER` env, else `openrt-cli`).

---

## 4. Cross-cutting hardening

### Kill scopes (ADR-008)

`KillScope::Trading` reserved in `crates/openrt-safety/src/kill.rs`. `tool_scope` classifies `trading.*` ids into trading scope; computer tools remain computer-scoped. A trading HALT never stops computer tools and vice versa. No trading tools ship until the optional trading subsystem does.

### Value-pattern redaction (SEC-05)

`RedactionPolicy::is_sensitive_value` + string-value walk in `crates/openrt-core/src/redaction.rs`: PEM private-key blocks, `Bearer`/`Basic` headers, and common provider token shapes (`sk-`, `ghp_`, `xox*-`, `eyJ`, `akia`, …) are redacted even under innocuous field names. Ordinary prose and paths are not over-redacted (unit-tested).

### Secrets at rest

Sealed records under `secrets.data` never contain plaintext (ChaCha20-Poly1305 ciphertext + key id + algorithm label). Integration test asserts raw store bytes omit the secret.

### Network / air-gap / export

- Sovereign denies all non-enclave egress regardless of grants (sole authority = `ModeManager`).
- Provisioning allows only destinations covered by a scope grant (default-deny otherwise).
- Observability export queue refuses in Sovereign (`ExportBlocked`); queues only in Provisioning (send still requires a netpol grant at the actual drain site).

### Enclave (ADR-004)

Mutual fingerprint auth; spoof attempt → automatic quarantine; operator quarantine/clear; non-enclave contact always refused; migration validates 64-hex SHA256 and rejects quarantined/revoked endpoints; content-address checked against the single registry when a manifest exists.

### Supervisor

Create/lease/takeover/operator-stop/check_stops with budgets, wall clock, error limits, completion stop, approval mats (`always_deny` fail-closed for unattended), generation bump on takeover (no double execution), resume identity preserves workflow/session/task/checkpoint.

---

## 5. Integration tests

New suite: `crates/integration-tests/tests/p6b_production.rs` (10 tests, all GREEN):

1. `policy_document_bootstraps_denies_by_default_and_versions`
2. `audit_verify_query_retention_through_runtime`
3. `secrets_are_encrypted_at_rest`
4. `netpol_sovereign_denies_then_grant_allows_in_provisioning`
5. `enclave_enroll_quarantine_authenticate_fail_closed`
6. `enclave_migrate_requires_active_nodes_and_valid_sha`
7. `observability_health_retention_export_sovereign_blocked`
8. `supervisor_create_lease_takeover_stop_list`
9. `doctor_includes_audit_airgap_policy_doc_checks`
10. `runtime_exposes_all_6b_production_entry_points`

All callers go through production `Runtime` methods (never a test-only shim). Phase 6A suites (`p6_workflow_compiler`, `p6_milestones`) and the rest of the workspace remain green (see acceptance doc for gate commands and counts).

---

## 6. Explicit non-goals / scoped out

- **Trading subsystem (M6.5–M6.8):** not implemented; roadmap §8 preserves optionality. Only `KillScope::Trading` reservation ships. No `openrt-trading` crate allocated.
- **selinux/landlock hardening of `openrt-sandbox`:** bwrap remains the production sandbox; terminal confinement was not extended beyond existing jail checks in this pass.
- **TPM/keyring hardware binding:** `openrt-secrets` supports keyfile 0600 today; keyring/TPM is an operator-supplied key path (documented), not hardware-enforced in this tree.
- **Physical deletion of audit records:** retention reports verifiable chain head/tail; truncation would break ADR-002 verification (archival cursor lives at observability layer).
- **No second DAG engine / scheduler / verifier / persistence system / policy evaluator** was introduced.

---

## 7. Gate summary

| Gate | Command | Result |
|---|---|---|
| Format | `cargo fmt --all -- --check` | RC=0 |
| Check | `cargo check --workspace --offline` | RC=0 |
| Tests | `cargo test --workspace --offline` | see `PHASE6B_ACCEPTANCE.md` for counts |
| Forensic TODO scan | `rg TODO\|FIXME\|unimplemented!\|todo!` on 6B crates | clean |
| Production callers | Runtime + CLI + doctor for every 6B entry | see acceptance table |
| 6A regression | `p6_workflow_compiler` + `p6_milestones` | 11/0 + 17/0 preserved |

**Phase 6B COMPLETE** when every assigned non-optional requirement is verified GREEN in `PHASE6B_ACCEPTANCE.md`.
