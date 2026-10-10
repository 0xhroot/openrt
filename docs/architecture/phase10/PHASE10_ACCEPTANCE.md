# PHASE 10 ACCEPTANCE — Persistent Agent Intelligence

Phase: **10** (ROADMAP v0.6, milestones M10.1–M10.10).
Design/implementation notes: [`PHASE10_IMPLEMENTATION.md`](./PHASE10_IMPLEMENTATION.md).

## Final status

**COMPLETE.**

All ten milestones are implemented and green. Every ROADMAP § 3 (DoD)
bullet is satisfied with named-test evidence; all mandated test categories
(§ 4) are covered by green tests (64 integration + crate units); every
security requirement (§ 5) is enforced with audit evidence; every "must NOT
be implemented" rule (§ 6) is respected. The only non-GREEN capability rows
are honest `UNAVAILABLE`/`BLOCKED` entries that the ROADMAP itself mandates
(remote marketplace under Sovereign) or host conditions (models/tools not
installed, empty store), never silent gaps.

## Gates (2026-09-26, this host)

| Gate | Command | Result | Evidence |
|---|---|---|---|
| Format | `cargo fmt --all --check` | RC=0 | this run |
| Check + warnings | `cargo check --offline --workspace --all-targets` | RC=0, **0 warnings** | this run |
| Tests | `cargo test --offline --workspace` | **1283 passed / 0 failed / 0 warnings**, 172 `test result: ok` | `/tmp/opencode/p10p_tests.log` |
| Doctor | `openrt doctor` | RC=0; **44 rows: 34 PASS / 1 WARN / 9 UNAVAILABLE**, 0 FAIL | `/tmp/opencode/p10_doctor.log` |
| CLI smoke | scripted end-to-end transcript | **27 × exit 0 + 3 × exit 1 (intended fail-closed contracts)** | `/tmp/opencode/cli-smoke/transcript.log` |
| Resource gate | `/proc/meminfo` used < 46% before full gates | satisfied | — |

Phase 9 baseline was **1093** tests; Phase 10 adds **190** (1283 total) with
**zero regressions** (every Phase 5–9 suite is inside the 1283).

WARN row: `media.asr MODEL_MISSING` (pre-existing Phase 9 host condition).
UNAVAILABLE rows: `genome`, `instincts`, `learning`, `rerank.model`,
`skill-execution`, `skill-loader`, `skill-registry`, `skills`,
`verifier.model` — honest empty-store/model-absent probes, exit 0.

## Milestone evidence

| Milestone | Evidence |
|---|---|
| **M10.1** Step 0 reconciliation + allocations | New `openrt-shield` crate (39 units); boot reconciliation `recovery::tests::boot_recovery_interrupts_stale_tasks_and_records_them`, `boot_recovery_clean_boot_records_evidence_without_noise`, `boot_records_stale_sandbox_without_touching_it`; doctor rows `recovery`, `time-machine`, `agentshield`, `extension-scan` PASS |
| **M10.2** Checkpoint model + integrity fail-closed | New `openrt-checkpoint` (35 units); `p10_recovery_extra::checkpoint_creation_seals_digest_and_tamper_fails_integrity`; `p10_recovery_extra::corrupted_checkpoint_is_refused_by_every_consumer` |
| **M10.3** Rollback end-to-end | `p10_recovery_extra::rollback_restores_state_and_never_rewrites_history` (validate → policy → plan → operator → restore → verify; chain before+after); plan/policy/rollback units in `openrt-checkpoint` |
| **M10.4** Crash recovery + live handles | `p10_recovery::crashed_workflow_boot_records_resume_and_operator_resumes_once` (TEST A substrate), `boot_records_stale_sandbox_without_touching_it`; `recovery::tests::task_recover_re_runs_interrupted_task_to_completion`, `task_recover_refuses_live_terminal_and_stale_active`, `task_recover_refuses_workflow_backed_task`; `p10_recovery_extra::partial_task_reconciliation_interrupts_without_phantom_runs`, `live_task_handle_spawns_observes_and_cancels`, `recovery::tests::live_handle_spawn_cancel_execute_lifecycle` |
| **M10.5** Time machine + TUI/WebUI surfaces | `timemachine::tests::{timeline_fails_closed_on_tampered_ledger, fork_base_beyond_tail_fails_closed, fork_from_historical_point_keeps_parent_intact, state_as_of_summarizes_sessions_at_the_point, branch_compare_reports_shared_prefix_and_divergence, return_to_current_reanchors_with_fork_catalog}`; `p10_recovery_extra::{time_machine_timeline_and_state_as_of_view_real_history, fork_after_historical_point_branches_without_touching_parent}`; TUI `time_machine_ops_actions_surface_store_data` (10/10); WebUI palette `history.fork/compare/state/return` + `m58_palette_lists_commands_sorted` (26/26); chains 7/8; CLI smoke `history fork/fork-commit/fork-verify/compare/return/verify` all exit 0 |
| **M10.6** Full skills system | `p10_skills` 12/12 (register, validate, install, activation gate, execution, permission denial, sandbox enforcement, version resolution, export/import roundtrip, tampered package, security scan, history-derived generation); `skill::tests::{loader_passes_for_healthy_skill, neg_loader_refuses_bad_tool_model_compat, neg_unknown_version_is_never_substituted, neg_workflow_skill_without_compiled_graph_fails_closed, neg_execution_requires_operator_activation, marketplace_local_catalog_and_remote_unavailable, version_gate_semantics, skill_compile_links_graph_and_execution_runs_it, skill_test_run_reports_named_verdicts}`; CLI smoke skill lifecycle exit 0 |
| **M10.7** ECC | `p10_ecc` 14/14 (deny rule overrides policy allow, allow rule never grants, exact scope, hook lifecycle across run/task/session/skill/workflow, tool-call hook fails closed, bounded instinct influence); `p10_ecc_extra::{instinct_creation_and_approval_lifecycle, instinct_application_is_durable_across_restart}` (production wrappers `assets_validate_instinct`/`assets_activate_instinct`); `openrt-selfimprove` 23/23 (`m10_learning_records_are_evidence_backed`, `m57_learning_derivation_is_deterministic_and_local`, full pipeline + all negatives); `phase5::tests::instinct_from_real_history_registers_unactivated`, `hook_permission_denied_fails_closed` |
| **M10.8** Agent genome | `phase5::tests::{m10_session_apply_genome_records_id_version_digest, m10_session_apply_refuses_in_flight_and_archived, m10_genome_runtime_validation_fails_closed, m10_genome_evolution_is_approval_gated, genome_apply_activates_referenced_assets}`; `p10_ecc_extra::genome_version_isolation_pins_exact_references`; `p10_integration::chain_genome_to_fresh_session`; CLI smoke `assets genome genome.nope --session` exit 1 (fail-closed contract) |
| **M10.9** AgentShield | `p10_shield` 10/10 (dangerous command, network escalation, filesystem escape, credential access, prompt injection, malicious tool, excessive permissions, extension scan, sandbox bypass, halt path); `gate::tests::{shield_scan_failure_fails_closed, shield_sovereign_deny_overrides_policy_allow, shield_halt_trips_kill_switch_and_blocks, shield_high_severity_approval_routes_through_resolver, shield_high_severity_fails_closed_without_resolver, shield_clean_call_proceeds_without_findings}`; findings persisted to `shield.findings` (`p10_skills` refusal is auditable there) |
| **M10.10** Final gate + docs | This file + `PHASE10_IMPLEMENTATION.md`; gates table above; forensic + security audits below |

## Definition of Done (ROADMAP § PHASE 10.3)

- [x] **Crash recovery** — checkpointed mid-flight workflow survives
  simulated restart; discovered, reconciled (never marked successful),
  resumed with no duplicate execution of uncertain non-idempotent steps,
  recovery recorded. Evidence: `p10_recovery` 2/2,
  `partial_task_reconciliation_interrupts_without_phantom_runs`, TEST A.
- [x] **Rollback** — validated checkpoint restores through an explicit
  authorized path; ledger never rewritten; chain verification before and
  after. Evidence: `rollback_restores_state_and_never_rewrites_history`.
- [x] **Time machine** — timeline/state-as-of/fork/branch-compare/
  return-to-current as Runtime APIs over the one ledger; TUI and WebUI
  expose them with no UI-local state. Evidence: `timemachine` 7 units,
  `time_machine_timeline_and_state_as_of_view_real_history`,
  TUI keys + `time_machine_ops_actions_surface_store_data`, WebUI
  `history.*` palette + chains 7/8.
- [x] **Skills** — execution history materially derives a content-backed
  candidate → provenance → validation → security scan → test → operator
  approval → activation → execution through the real gate/sandbox →
  recorded id+version+digest; imported packages arrive REGISTERED; tampered
  packages rejected. Evidence: `p10_skills` 12/12 + TEST C/D.
- [x] **Effective permissions = intersection** — skill never exceeds its
  caller; model output never mutates permissions/policy. Evidence:
  `skill_permissions_never_exceed_its_caller`, `active_skill_executes_through_gated_production_path`,
  `m10_genome_evolution_is_approval_gated`, `improve_approval_gated_apply_production_path`.
- [x] **Rules/hooks/instincts enforced at real runtime points**; instincts
  and learnings carry evidence; genome validated, applied to a fresh
  session with recorded id/version/digest, evolves only through the
  approval gate. Evidence: `p10_ecc` 14/14, `p10_ecc_extra` 3/3,
  `chain_genome_to_fresh_session`, `m10_learning_records_are_evidence_backed`.
- [x] **AgentShield denies/halts before execution**, findings persisted
  (`shield.findings`) and audited without secrets; Phase 9 injection
  defense consumed (not duplicated); shield cannot weaken policy; no
  parallel kill switch (halt trips the shared one). Evidence:
  `p10_shield` 10/10 + gate shield units + TEST F/G.
- [x] **Phases 5–9 stay GREEN; no fabricated backends.** Evidence: full
  workspace 1283/0/0 including every prior suite; doctor probes report
  real availability only.

## Tests required (ROADMAP § PHASE 10.4) — mapping, all green

**Recovery (10/10)**

| Required | Test |
|---|---|
| checkpoint creation/integrity | `p10_recovery_extra::checkpoint_creation_seals_digest_and_tamper_fails_integrity` |
| rollback | `p10_recovery_extra::rollback_restores_state_and_never_rewrites_history` |
| crash/restart | `p10_recovery::crashed_workflow_boot_records_resume_and_operator_resumes_once` |
| resume | same + `p10_integration::chain_recovery_to_resume` |
| partial-task reconciliation | `p10_recovery_extra::partial_task_reconciliation_interrupts_without_phantom_runs` |
| stale-sandbox recovery | `p10_recovery::boot_records_stale_sandbox_without_touching_it` |
| corrupted-checkpoint refusal | `p10_recovery_extra::corrupted_checkpoint_is_refused_by_every_consumer` |
| time-machine view | `p10_recovery_extra::time_machine_timeline_and_state_as_of_view_real_history` |
| fork after historical point | `p10_recovery_extra::fork_after_historical_point_branches_without_touching_parent` |
| live task handle | `p10_recovery_extra::live_task_handle_spawns_observes_and_cancels` |

**Skills (12/12)**

| Required | Test |
|---|---|
| register | `p10_skills::skill_registration_refuses_overbroad_and_duplicates` |
| validate | `p10_skills::skill_validation_seals_state_and_tampered_records_fail_closed` |
| install | `p10_skills::skill_install_is_durable_across_restart` |
| activation gate | `p10_skills::activation_gate_requires_validation_then_operator` |
| execution | `p10_skills::active_skill_executes_through_gated_production_path` |
| permission denial | `p10_skills::skill_permissions_never_exceed_its_caller` |
| sandbox enforcement | `p10_skills::sandbox_enforcement_refuses_unknown_profiles` |
| version resolution | `p10_skills::skill_version_resolution_is_exact_and_never_substitutes` |
| export/import roundtrip | `p10_skills::export_import_roundtrip_forces_registered` |
| tampered package | `p10_skills::tampered_package_is_rejected_on_import` |
| security scan | `p10_skills::activation_security_scan_refuses_flagged_content` |
| history-derived generation | `p10_skills::history_derived_generation_produces_content_backed_candidate` |

**ECC (7/7)**

| Required | Test |
|---|---|
| rule enforcement | `p10_ecc::m10_gate_deny_rule_overrides_policy_allow` + `m10_gate_allow_rule_never_grants` + `m10_gate_rule_scope_mismatch_does_not_deny` |
| hook enforcement | `p10_ecc::m10_run_started_hook_fires`, `m10_task_started_hook_fires`, `m10_session_end_archives_and_fires_hook`, `m10_skill_activated_hook_fires`, `m10_workflow_started/completed_hook_fires`, `m10_gate_tool_call_hook_fails_closed` |
| instinct creation/approval/application | `p10_ecc_extra::instinct_creation_and_approval_lifecycle`, `instinct_application_is_durable_across_restart` |
| learning derivation | `openrt-selfimprove` `m57_learning_derivation_is_deterministic_and_local`, `m10_learning_records_are_evidence_backed` (+ Phase 5 `p5_learning_and_local_telemetry_emit`, still green) |
| genome validation | `phase5::tests::m10_genome_runtime_validation_fails_closed` |
| fresh-session genome application | `phase5::tests::m10_session_apply_genome_records_id_version_digest` + `p10_integration::chain_genome_to_fresh_session` |
| genome version isolation | `p10_ecc_extra::genome_version_isolation_pins_exact_references` |

**AgentShield (10/10)** — `p10_shield` maps 1:1:
`dangerous_command_denied_and_audited`, `network_escalation_denied_under_sovereign`,
`filesystem_escape_denied_before_execution`, `credential_access_denied_before_execution`,
`prompt_injection_denied_before_execution`, `malicious_tool_flagged_by_scanner_and_refused_by_gate`,
`excessive_permissions_flagged_at_scan`, `extension_scan_flags_hostile_package_and_passes_clean_one`,
`sandbox_bypass_attempt_denied`, `halt_path_trips_the_shared_kill_switch`.

**Integration (8/8)** — `p10_integration` maps 1:1:
`chain_history_to_skill_generation`, `chain_skill_to_workflow_execution`,
`chain_workflow_to_agentshield`, `chain_agentshield_to_sandbox_containment`,
`chain_genome_to_fresh_session`, `chain_recovery_to_resume`,
`chain_tui_runtime_shared_state`, `chain_webui_runtime_shared_state`.

**Mandatory E2E** — see TEST A–H matrix.

## TEST A–H matrix (all green)

| TEST | Suite test | Proves | Result |
|---|---|---|---|
| A crash recovery | `p10_e2e::test_a_crash_recovery` | mid-flight checkpoint survives restart; reconciled not completed; single operator resume; recorded | PASS |
| B time machine | `p10_e2e::test_b_time_machine` | timeline/as-of/fork/compare/return over the shared ledger; mode stays non-network | PASS |
| C skill lifecycle | `p10_e2e::test_c_skill_lifecycle` | history → candidate → validate → scan → approve → activate → execute; production-threshold shield action; real ledger tool evidence | PASS |
| D skill import security | `p10_e2e::test_d_skill_import_security` | activate-before-validate refused (fail-closed); validate then activate; imported package never ACTIVE | PASS |
| E genome | `p10_e2e::test_e_genome` | validation fail-closed, fresh-session application records id/version/digest, approval-gated evolution | PASS |
| F AgentShield | `p10_e2e::test_f_agentshield` | denied operation never executes; findings persisted + audited; no parallel kill switch | PASS |
| G prompt injection | `p10_e2e::test_g_prompt_injection` | Phase 9 detector consumed; injected skill id refused and left unregistered | PASS |
| H full Phase 10 loop | `p10_e2e::test_h_full_phase10_loop` | boot → recover → resume → skills → ECC → genome → shield → history integrity over a shared store with stable non-shrinking `history_verify()` | PASS |

## Forensic production-caller audit (M10.10)

Every Phase 10 public API was traced to real non-test callers:

| API | Production caller(s) | Status |
|---|---|---|
| `checkpoint_workflow` | **none (dead duplicate of `run_workflow`)** | **removed**; `run_workflow` → `run_workflow_checkpointed` is the one path |
| `resume_workflow` | CLI `task resume` (→ `run_recovery`, `RecoveryScope::Resume`), TUI, WebUI palette | wired |
| `recovery_records` | CLI `recovery list/inspect`, TUI, WebUI palette, doctor | wired |
| `boot_recover` | `Runtime::build` | wired |
| `task_recover` | CLI `recovery resume` | wired |
| `history_fork`, `history_fork_commit`, `history_fork_verify`, `history_branch_compare`, `history_return_to_current` | CLI (`history fork/fork-commit/fork-verify/compare/return`), TUI keys `f/c/v`, WebUI `history.fork/return/compare` | wired (chains 7/8 + smoke) |
| `history_timeline`, `history_state_as_of`, `history_forks` | WebUI `history.timeline/state`, TUI timeline/as-of | wired |
| `skill_submit_from_history` | CLI (`assets generate`, `instinct generate` substrate) | wired |
| `skill_inspect`, `skill_load`, `skill_compile`, `skill_test_run` | CLI (`skill inspect/load/compile/test`, `skill execute`) | wired |
| `assets_validate_candidate`, `assets_approve_candidate`, `assets_activate_candidate` | CLI (`assets candidate-validate/approve/activate`), `p10_integration` chain 1 | wired |
| `assets_validate_instinct`, `assets_activate_instinct` | CLI (`instinct validate/activate`), `p10_ecc_extra` lifecycle | wired (fires `AssetActivated`) |
| `instinct_submit_from_history` | CLI (`instinct generate --session`) | wired |
| `session_apply_genome` | CLI (`assets genome --session`), TUI | wired |
| `assets_apply_genome` | CLI (`assets genome`), improve apply | wired |
| `improve_approve` / `improve_approve_with_note` / `improve_evaluate` | CLI (`improve approve [--note]`, `improve evaluate`) | wired |
| `improve_evaluate` negative path | store-level + CLI smoke (`improve evaluate` before baseline → exit 1) | wired |
| `scan_extension` (shield) | activation/extension-scan path + doctor `extension-scan` row | wired |
| `assets_install_skill_test` | test-only wrapper — **same authority** as production `import_package` (store import carries the `tests` field, `store.rs:1559`) | documented, not a parallel path |
| `assets_install_skill` (package import) | CLI (`assets import`) + integration tests | wired |
| `validate_instinct` (store) | wrapped by `assets_validate_instinct` (CLI) | wired |
| remote marketplace fetch | never invoked as a network path — reports honest UNAVAILABLE (`skill.rs:508`) | BLOCKED by design |

## Security requirements (ROADMAP § PHASE 10.5)

- [x] **Imported skill content can never arrive active** — `import_package`
  forces `ActivationState::Registered` with an explicit comment
  (`store.rs` skills loop); activation requires the operator path and passes
  `security_scan_for_activation` first (`activation_gate_requires_validation_then_operator`,
  `activation_security_scan_refuses_flagged_content`,
  `export_import_roundtrip_forces_registered`).
- [x] **Instincts/genomes/hooks/rules never grant permissions or bypass
  policy/sandbox/approval; model output never mutates persistent security
  state** — instincts are bounded run-context influence only
  (`m10_instinct_influence_on_run_context_is_bounded`,
  `m10_no_active_instincts_prompt_unchanged`); allow rules can never grant
  (`m10_gate_allow_rule_never_grants`); evolution/improvement require the
  operator approval gate (`m10_genome_evolution_is_approval_gated`,
  `improve_approval_gated_apply_production_path`); hooks are permission
  checked (`hook_permission_denied_fails_closed`).
- [x] **Rollback/restore never erases history; corrupted state never
  recovered as trusted** — `rollback.rs` performs zero ledger writes (grep:
  0 append/row-write sites); `rollback_restores_state_and_never_rewrites_history`;
  `corrupted_checkpoint_is_refused_by_every_consumer`;
  `timeline_fails_closed_on_tampered_ledger`.
- [x] **AgentShield consumes existing authorities** — evaluation happens
  once inside the existing gate; findings only; halt trips the shared kill
  switch (`halt_path_trips_the_shared_kill_switch`); scan failure fails
  closed (`shield_scan_failure_fails_closed`); no second policy engine.
- [x] **Skills execute only through the tool/sandbox/policy path; hooks
  same permission checks** — `active_skill_executes_through_gated_production_path`,
  `skill_permissions_never_exceed_its_caller`, `hook_permission_denied_fails_closed`.

## Security audit (grep-based, this tree)

| Check | Method | Result |
|---|---|---|
| `unsafe` in production code | `grep -rE "unsafe fn|unsafe impl|unsafe {" crates/*/src` | 2 sites, both `libc::statvfs` FFI in `openrt-resources/src/system.rs` (crate-level `unsafe_code = "allow"` override, workspace otherwise `unsafe_code = "forbid"` at `Cargo.toml:171`); zero in Phase 10 crates |
| Dynamic code execution in skill/loader path | grep `eval(`/`exec(`/`dlopen`/script loading in `workspace-assets`, `shield`, `skill.rs` | 0 — the only `.evaluate(` is policy evaluation of a `PermissionRequest` |
| Policy bypass functionality | grep `bypass` in production src | only comments, the Phase 9 injection category string, and the `sandbox_bypass` detector name — no bypass code |
| Import cannot pre-activate | `store.rs` import loop | `skill.activation = ActivationState::Registered` with fail-closed comment |
| Rollback cannot rewrite history | grep writes in `openrt-checkpoint/src/rollback.rs` | 0 |
| Findings/logs contain no secrets | redaction tests (`seal_record_redacts_secret_keys_in_change_payload`, `sinks::tests::*_redacted*`, `memory_value_redacted_at_write_for_sensitive_keys`) | green; shield findings carry ids/categories only |
| Permission writes confined | grep assignment sites | tests + operator-gated genome builder only |
| Remote marketplace | `skill.rs:508` + `marketplace_local_catalog_and_remote_unavailable` | honest `UNAVAILABLE`, asserted |

## What must NOT be implemented (§ PHASE 10.6) — compliance

| Rule | Status |
|---|---|
| No Phase 11 (no plugin registry beyond shared scanning; no autonomous marketplace network backend) | compliant — remote sources report UNAVAILABLE/BLOCKED; only the shared `scan_extension` interface exists |
| No rebuild of Phases 5–9 authorities; no parallel stores | compliant — one store, one ledger, one audit chain, one gate; all Phase 10 features wrap existing authorities |
| No self-modifying security | compliant — permissions/shield rules/sandbox policy/Sovereign mode/detectors change only via operator approval paths; all gated tests green |

## Capability matrix (this host, final)

| Capability | Status | Evidence / reason |
|---|---|---|
| Checkpoint creation/integrity, crash recovery, resume, reconciliation | GREEN | `openrt-checkpoint` 35 + `p10_recovery*` 9 |
| Rollback (authorized, non-destructive) | GREEN | `rollback_restores_state_and_never_rewrites_history` |
| Live task handles | GREEN | `live_task_handle_spawns_observes_and_cancels` |
| Time machine — runtime APIs | GREEN | `timemachine` 7 units + e2e TEST B |
| Time machine — TUI | GREEN | keys `f/c/a/v` + `time_machine_ops_actions_surface_store_data` |
| Time machine — WebUI | GREEN | `history.tail/timeline/fork/compare/state/return` + palette test |
| Time machine — CLI | GREEN | `history fork/fork-commit/fork-verify/compare/return` (smoke exit 0) |
| Skills: full lifecycle incl. scan/test/compile/versioning | GREEN | `p10_skills` 12/12 + TEST C/D |
| Skills: local offline marketplace catalog | GREEN | `marketplace_local_catalog_and_remote_unavailable` |
| Skills: remote marketplace network backend | **BLOCKED** | Sovereign mode — honest UNAVAILABLE by design (Phase 11) |
| ECC: rules/hooks at the gate | GREEN | `p10_ecc` 14/14 |
| ECC: instinct approval + application | GREEN | `p10_ecc_extra` 3/3 + CLI instinct validate/activate |
| ECC: evidence-backed learning | GREEN | `openrt-selfimprove` 23/23 |
| Genome: validate/apply/version isolation/evolution gate | GREEN | phase5 genome units + TEST E |
| AgentShield: all 7 detector families + halt + persistence | GREEN | `p10_shield` 10/10 + gate units + TEST F |
| doctor probes (44 rows) | GREEN (34 PASS, 1 WARN, 9 honest UNAVAILABLE) | `/tmp/opencode/p10_doctor.log`, exit 0 |

## Phase 5–9 regression (final run)

Full workspace suite — every prior gate file included:
`p5_workbench`, `p6_*`, `p7b_*`, `p8_sandbox`, `p9_*` (11 p9 suites),
`p2_*`, `p3_fabric`, durability/event-ordering/session-concurrency/persistence
roundtrips — **all green inside the 1283/0/0 run**. No test was deleted or
weakened; Phase 9 baseline 1093 → 1283 (+190 new Phase 10 tests).

## CLI evidence (live host)

Scripted transcript against a real store
(`/tmp/opencode/cli-smoke/transcript.log`), fresh store + `OPENRT_CONFIG`:

- 27 × exit 0 covering: session create → `task run --mock` (real gate
  evidence rows) → `instinct generate` → `instinct validate` →
  `instinct activate --operator` → `instinct list` → candidate
  generate → validate → approve → activate → `skill inspect` →
  `skill execute` → history fork/fork-commit/fork-verify/compare/verify/
  return → improve baseline/proposal/evaluate/approve → genome/list/
  timeline → `doctor` (exit 0).
- 3 × exit 1, all intended fail-closed contracts:
  `assets candidate-approve` before validate (`is not validated`),
  `assets candidate-activate` before validate (`is not validated`),
  `assets genome genome.nope --session` (`genome not found`).

## Known limitations / not claimed

1. **Remote marketplace** is intentionally absent (Sovereign / Phase 11);
   the catalog surface is local-only and says so honestly.
2. **doctor UNAVAILABLE rows** reflect this host's empty store and absent
   models (`rerank.model`, `verifier.model`) — they are probes, not stubs.
3. **`media.asr MODEL_MISSING` WARN** is a pre-existing Phase 9 host
   condition, unchanged by Phase 10.
4. **`assets_install_skill_test`** is a test-authoring convenience with the
   same authority as production package import — not a bypass.
5. Instincts influence run context within a hard bound; they are never a
   permission mechanism.
6. The two `unsafe` blocks live in Phase 2/5 resource FFI
   (`openrt-resources`), not in Phase 10 code.
