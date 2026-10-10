# Phase 6B — Acceptance

**Status:** PHASE 6B COMPLETE  
**Date:** 2026-09-24  
**Predecessor:** `PHASE6B_IMPLEMENTATION.md` (implementation record)  
**Baseline:** Phase 6A frozen at 654 tests (`/tmp/opencode/final_test.log`) — compiler/scheduler foundation unchanged.

---

## 1. Milestone status

| Milestone | Requirement | Status | Evidence |
|---|---|---|---|
| M6.1 remainder | Long-running agents / autonomous unattended workflows with stop conditions, approval mats, lease + takeover, operator stop | **GREEN** | `openrt-supervisor` (`exec.autonomous`) 10/0; Runtime `supervisor()` (`runtime.rs:386`); CLI `autonomous *` (`main.rs:3073`); `p6b_production` #8 |
| M6.2 | Security policy engine + audit strengthen | **GREEN** | `openrt-policy` (`policy.engine`) 5/0; Runtime `policy_document`/`_ensure`/`_append_rule`/`versions` (`runtime.rs:309–358`); Runtime `audit_verify`/`audit_query`/`audit_retention` (`runtime.rs:289–302`); doctor `audit`/`policy-doc` (`doctor.rs:328,373`); CLI `audit *`/`policy *`; gate `required_caps` (`gate.rs:267`); `p6b_production` #1–#2, #9 |
| M6.3 | Encrypted secrets + network isolation + air-gapped operation + supply-chain | **GREEN** | `openrt-secrets` 6/0; `openrt-netpol` 5/0; Runtime `secrets()`/`netpol()` (`runtime.rs:363,370`); doctor `air-gap` (`doctor.rs:345`); CLI `secret *`/`netpol *`; supply-chain via frozen `openrt-verify`/`openrt-scan` (Phase 3); `p6b_production` #3–#4, #9 |
| M6.4 | Enclave + observability (distributed workers / federation surface) | **GREEN** | `openrt-enclave` 8/0; `openrt-observability` 6/0; Runtime `enclave()`/`observability()` (`runtime.rs:376,380`); CLI `enclave *`/`obs *`; `p6b_production` #5–#7 |
| M6.5–M6.8 | Trading marks 0–3 | **Scoped out** | ROADMAP §8 preserves optionality; no `openrt-trading` crate. Only `KillScope::Trading` reservation ships (`kill.rs:17`, `tool_scope` `trading.*` at `kill.rs:38`; 6/0 openrt-safety) |
| M6.9 | Final production/security audit + phase gate | **GREEN** | This doc + `PHASE6B_IMPLEMENTATION.md`; gates below; forensic caller matrix + TODO scan clean |

---

## 2. Production callers (file:line)

### Runtime façade — `crates/openrt-runtime/src/runtime.rs`

| Method | Line | Milestone |
|---|---|---|
| `audit(...)` | 274 | shared ADR-002 append |
| `audit_verify()` | 289 | M6.2 |
| `audit_query(&AuditQuery)` | 294 | M6.2 |
| `audit_retention()` | 302 | M6.2 |
| `policy_document()` | 309 | M6.2 |
| `policy_document_ensure()` | 315 | M6.2 |
| `policy_append_rule(...)` | 326 | M6.2 |
| `policy_versions()` | 355 | M6.2 |
| `secrets()` | 363 | M6.3 |
| `netpol()` | 370 | M6.3 |
| `enclave()` | 376 | M6.4 |
| `observability()` | 380 | M6.4 |
| `supervisor(holder)` | 386 | M6.1 remainder |

### Doctor — `crates/openrt-runtime/src/doctor.rs`

| Check name | Line | Milestone |
|---|---|---|
| `audit` | 328 | M6.2 chain verify |
| `air-gap` | 345 | M6.3 Sovereign=Pass / Provisioning=Warn / unreadable=Fail |
| `policy-doc` | 373 | M6.2 bootstrap on miss |

### Gate — `crates/openrt-runtime/src/gate.rs`

- Lines 228–280: evaluates tool id **and every `spec().required_caps`** as separate policy requests; `Deny` short-circuits; `Ask` → `resolve_ask` + combined events; `extract_scope_public` re-exported.

### CLI — `crates/openrt-cli/src/main.rs`

| Command | Dispatch | Handler |
|---|---|---|
| `audit verify\|query\|retention` | 915 | `cmd_audit` 2681 |
| `policy show\|init\|append\|versions` | 916 | `cmd_policy` 2741 |
| `secret set\|get\|delete\|list\|rotate` | 917 | `cmd_secret` 2808 |
| `netpol grant\|revoke\|list\|check` | 918 | `cmd_netpol` 2877 |
| `enclave enroll\|nodes\|quarantine\|migrate` | 919 | `cmd_enclave` 2934 |
| `obs health\|retention\|export` | 920 | `cmd_obs` 3009 |
| `autonomous create\|list\|stop\|takeover` | 921 | `cmd_autonomous` 3073 |

Operator identity: `actor_user()` (`USER` env, else `openrt-cli`). 6A workflow CLI (`cmd_workflow` 1615) untouched.

### Cross-cutting

| Item | Location |
|---|---|
| `KillScope::Trading` + `trading.*` tool scope | `crates/openrt-safety/src/kill.rs:17,38` |
| Value-pattern redaction (`is_sensitive_value` + string walk) | `crates/openrt-core/src/redaction.rs:97,166,186` |
| Default caps `system.info.all` / `filesystem.read.all` | `crates/openrt-config/src/lib.rs:148,158` |
| Key-meta record key `k.{id}` (path-safe per ADR-006) | `crates/openrt-secrets/src/lib.rs:244,412,424` |

---

## 3. Test counts

| Suite / crate | Passed | Failed |
|---|---:|---:|
| **Workspace total** (`cargo test --workspace --offline`) | **708** | **0** |
| `p6b_production` (integration) | 10 | 0 |
| `p6_milestones` (6A regression) | 17 | 0 |
| `p6_workflow_compiler` (6A regression) | 11 | 0 |
| `openrt-policy` | 5 | 0 |
| `openrt-secrets` | 6 | 0 |
| `openrt-netpol` | 5 | 0 |
| `openrt-enclave` | 8 | 0 |
| `openrt-observability` | 6 | 0 |
| `openrt-supervisor` | 10 | 0 |
| `openrt-safety` | 6 | 0 |
| `openrt-core` | 32 | 0 |
| `openrt-config` | 8 | 0 |
| 6A baseline | 654 | 0 |

`p6b_production` tests (all through production `Runtime` methods, no test-only shims):

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

---

## 4. Gate commands and RCs

| Gate | Command | RC | Log |
|---|---|---:|---|
| Format | `cargo fmt --all -- --check` | **0** | `/tmp/opencode/p6b_fmt2.log` |
| Check | `cargo check --workspace --offline` | **0** | `/tmp/opencode/p6b_check9.log` |
| Tests | `cargo test --workspace --offline` | **0** | `/tmp/opencode/p6b_final_test2.log` (708/0, 121 suites) |
| Forensic TODO/FIXME/`unimplemented!` scan | `rg` over 6B crates | **clean** | 6B crates empty; `unimplemented!()` only in pre-existing test-only mock stubs (`openrt-modes/src/lib.rs` L461–554, `openrt-audit/src/lib.rs` L678–767) |
| 6A regression | `p6_workflow_compiler` + `p6_milestones` | **11/0 + 17/0** | preserved in workspace run |

---

## 5. Forensic audit notes / remaining gaps

| Gap | Severity | Notes |
|---|---|---|
| Trading subsystem (M6.5–M6.8) | N/A (optional) | Scoped out per ROADMAP §8; only `KillScope::Trading` reserved; no `openrt-trading` crate |
| Gate reads config-built policy, not `policy.engine` document | Documented | `runtime.rs:320–325`: in-process gate policy is built at Runtime construction from config; document appends take effect for newly built runtimes and `policy_document` readers; gate continues to enforce the live `permission_policy` Arc (never a second authority) |
| `policy_from_store` / `load_revision` no production caller | Low | `openrt-policy/src/lib.rs:279,182` — exercised by unit tests; production path is Runtime `policy_*` methods which use `PolicyDocumentStore` directly |
| TPM/keyring hardware binding | Low | `openrt-secrets` supports keyfile 0600 today; keyring/TPM is an operator-supplied key path, not hardware-enforced in this tree |
| selinux/landlock hardening of `openrt-sandbox` | Low | bwrap remains the production sandbox; no extension in this pass |
| Physical deletion of audit records | By design | ADR-002 verification requires the full chain; retention reports head/tail; truncation would break verification |

No second DAG engine, scheduler, verifier, persistence system, policy evaluator, secrets engine, netpol engine, or sandbox was introduced. Phase 6A foundation is frozen and green.

---

## 6. Verdict

**PHASE 6B COMPLETE**

All assigned non-optional Phase 6 milestones (M6.1 remainder, M6.2, M6.3, M6.4, M6.9) are GREEN with production callers, real integration tests, and full workspace gates at **708 tests / 0 failures** (fmt RC=0, check RC=0, test RC=0). Trading (M6.5–M6.8) is explicitly scoped out per roadmap optionality. Phase 6A regression preserved (11/0 + 17/0).
