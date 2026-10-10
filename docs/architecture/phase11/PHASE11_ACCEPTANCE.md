# PHASE 11 ACCEPTANCE — Universal AI Workbench

Phase: **11** (OWNERSHIP `## Phase 11 — UNIVERSAL AI WORKBENCH`,
M11.1–M11.10). Design/implementation notes:
[`PHASE11_IMPLEMENTATION.md`](./PHASE11_IMPLEMENTATION.md).

## Final status

**PHASE 11 COMPLETE.**

All ten milestones are implemented, tested, and audited. Every mandated
integration category (t37–t48) and every E2E journey (TEST A–H) is green;
the production-caller and security forensic audits found no bypass, no
second authority, and no payload leakage (one gap found and fixed: GUI
typed text in the durable event log now carries a length marker).
The only non-GREEN entries are honest sub-capability `UNAVAILABLE`/
`BLOCKED` rows mandated by design (remote MCP/marketplace under Sovereign,
host tool absence) — never silent gaps.

## Capability table (per milestone — the Phase 11 verdict)

| Milestone | Capability | Verdict | Evidence |
|---|---|---|---|
| **M11.1** | Extension manifest/registry/install pipeline + security scan + operator activation + build-time bindings + update/rollback/remove | **GREEN** | `p11_extensions` 12/12; `p11_e2e::test_a`; doctor `extensions` PASS |
| **M11.2** | MCP ecosystem: persisted registry, 4-level trust, scan-first enable, spawn gate, gated invocation | **GREEN** | `p7b_mcp_runtime` 7/7; `p11_integration::t40`; `p11_e2e::test_b`; doctor `mcp.ecosystem` PASS |
| **M11.3** | Workbench + materials: rooms, typed material records with digests, references only (no copies), reconstruction across restart | **GREEN** | `p11_workbench` t13–t18; `t38`; `p11_e2e::test_c` |
| **M11.4** | Agent rooms + roles as configuration mapped onto existing `SubAgentKind` identities, audited | **GREEN** | `p11_workbench` t13/t14/t18; `t39`; `test_c` |
| **M11.5** | Cooperative pause / resume / append-only steer; paused runs survive restart and are never auto-resumed | **GREEN** | `p11_workbench` t19–t24; `t46`; `test_c` |
| **M11.6** | WebUI palette Phase 11 commands dispatch through the ONE gate over shared Runtime state | **GREEN** | `webui` 28/28; `t43`; `test_e`, `test_h` |
| **M11.7** | TUI rooms/extensions/computer panels read the shared Runtime (no UI-local state) | **GREEN** | `openrt-tui` 12/12; `t44`; `test_e` |
| **M11.8** | ComputerUse loop: observe → resolve chain → plan (+structured alternative) → gated execute → bounded verify → twin invalidation | **GREEN** | `p11_computeruse` 13/13; `p11_e2e::test_d`; doctor `computer.{vision,grounding,twin,verify}` PASS |
| **M11.9** | Computer-use security: AgentShield scans GUI typed text, deny/halt before execution, payload never persisted, kill scope `computer` | **GREEN** | `p11_computeruse` t32–t35; `t42`; `p11_e2e::test_f` (findings + audit + event log) |
| **M11.10** | CLI `extension/mcp/workbench/room/material/computer *`; doctor rows present, never `Fail`; docs | **GREEN** | CLI smoke 16/16; doctor 59 rows **0 FAIL**; `t37`; `test_g`; this file |

### Honest sub-capability rows (never fabricated)

| Sub-capability | Status | Reason |
|---|---|---|
| Remote/HTTP MCP transport | **UNAVAILABLE** | stdio only; netpol `SCOPE_MCP` reserved, registration of `http(s)://` refused (doctor `mcp.remote-transport`) |
| Remote marketplace catalog | **BLOCKED** (Sovereign) | this build ships no remote client; Sovereign + netpol refuse network fetches — never a fabricated listing (doctor `marketplace`) |
| Pointer injection on this host | **UNAVAILABLE** | no `ydotool`/`dotool` installed — actions fail closed with the honest mechanism reason (engine supports it when present) |
| Screenshot capture on this host | **PARTIAL** | hyprland without a capture tool (`image=no`) — the resolution chain degrades honestly through the remaining sources |
| Accessibility tree on this host | **UNAVAILABLE** | no a11y source wired — chain reports `accessibility=unavailable` with full trace |

## Gates (2026-09-27, this host)

| Gate | Command | Result | Evidence |
|---|---|---|---|
| Format | `cargo fmt --all --check` | RC=0 | `/tmp/opencode/p11z_fmt.log` |
| Check + warnings | `cargo check --offline --workspace --all-targets` | RC=0, **0 warnings** | `/tmp/opencode/p11z_check.log` |
| Tests | `cargo test --offline --workspace --no-fail-fast` | **1380 passed / 0 failed / 0 ignored**, 178 `test result: ok` | `/tmp/opencode/p11z_tests.log` |
| Doctor | `openrt doctor` | RC=0; **59 rows: 48 PASS / 1 WARN / 10 UNAVAILABLE / 0 FAIL** | `/tmp/opencode/p11z_doctor.log` |
| CLI smoke | Phase 11 scripted transcript | **16/16 expected exit codes** (incl. 2 intended fail-closed exits: resolve chain rc=1, deny-by-default rc=1) | `/tmp/opencode/p11smoke/transcript.log` |
| Resource gate | free < 46% before full gates | satisfied (42.4%) | — |

Phase 10 baseline was **1283** tests; Phase 11 brings the workspace to
**1380** (+97: the 58 dedicated `p11_*` integration tests plus Phase 11
crate units) with **zero regressions**.

WARN row: `media.asr MODEL_MISSING` (pre-existing Phase 9 host condition).
UNAVAILABLE rows: `genome`, `instincts`, `learning`, `rerank.model`,
`skill-execution`, `skill-loader`, `skill-registry`, `skills`,
`verifier.model` (pre-existing honest empty-store/model probes) and
`mcp.remote-transport` (Phase 11, honest by design).

## Milestone evidence

| Milestone | Evidence |
|---|---|
| **M11.1** Extensions | `p11_extensions` 12/12: `import_lands_registered_unscanned_and_never_active`, `scan_records_clean_and_flagged_verdicts_persistently`, `activation_requires_an_authorized_operator_and_a_clean_scan`, `activation_gates_skills_capabilities_and_sovereignty`, `single_active_version_demotes_the_previous_one`, `active_extension_tools_bind_only_at_build`, `bound_tool_executes_the_skill_through_the_gate_with_input`, `only_active_extensions_bind_at_build`, `rollback_preserves_the_version_history`, `removal_refuses_while_active_or_depended_on`, `tool_name_conflicts_fail_validation_or_the_build`, `install_refuses_uncovered_skill_refs_and_validate_marks_validated`; E2E `test_a_extension_lifecycle_journey` |
| **M11.2** MCP | `p7b_mcp_runtime` 7/7 (fixture server, deny-by-default caps, trust never bypasses gate); `t40_mcp_trust_never_bypasses_the_permission_gate`; E2E `test_b_mcp_ecosystem_journey` (register → scan → trust → enable → spawn on rebuild → gated invocation → disable → never spawns) |
| **M11.3** Workbench/materials | `p11_workbench` t13–t18 (`t15_material_digest_seal_and_drift_detection`, `t16_material_workbench_attach_gates`, `t18_full_workbench_scenario_cross_build`); `t38_workbench_reconstruction_across_restart`; E2E `test_c` |
| **M11.4** Rooms/roles | `t13_room_lifecycle_gated_and_persistent`, `t14_room_material_links_fail_closed_and_idempotent`, `t39_room_roles_map_to_existing_identities_and_audit`, E2E `test_c` |
| **M11.5** Pause/resume/steer | `p11_workbench` t19–t24 (`t19_pause_records_partial_history_without_failure`, `t20_resume_completes_paused_run_end_to_end`, `t21_pause_resume_gates_fail_closed`, `t22_steer_hydrates_into_blackboard_and_survives_resume`, `t23_steer_scoped_to_owning_room`, `t24_full_flow_is_audited_with_verifying_chain`); `t46_paused_run_survives_restart_and_is_never_auto_resumed` |
| **M11.6** WebUI palette | `webui` 28/28 incl. `m58_palette_lists_commands_sorted`; `t43_palette_phase11_commands_dispatch_through_one_gate`; E2E `test_e`/`test_h` (palette `computer.use` invalidates the shared engine's twin) |
| **M11.7** TUI | `openrt-tui` 12/12; `t44_tui_computer_panel_reads_the_shared_runtime_snapshot`; `p11_cross_surface::h1_palette_room_visible_in_tui_and_steer_flows_both_ways`; E2E `test_e` (TUI room list == runtime room) |
| **M11.8** ComputerUse | `p11_computeruse` 13/13: t25 observe/clipboard-on-demand, t26 chain fail-closed, t27 vision step, t28 coordinate never guessed, t29 twin invalidation, t30 focus verify, t31 bounded ≤3, t31b honest Unavailable, t32 click/scroll/drag bounds, t33 deny-by-default, t34 gated+audited, t35 shield no-leak, t36 VLM grounds never executes; `t47_advisory_structured_alternative_for_every_action`; E2E `test_d` |
| **M11.9** Security | `t35`, `t42_kill_scope_computer_blocks_actions_not_discovery`; E2E `test_f_security_fail_closed_bundle` (extension never Active, hostile MCP never connects, typed `rm -rf /` halted, findings + audit + **event log** free of payload); gate event-length-marker fix |
| **M11.10** Surfaces/doctor/docs | `t37_doctor_phase11_rows_complete_and_never_fail`; `t48_cross_surface_mega_scenario_one_store_one_gate`; `test_g_doctor_capability_honesty`; CLI smoke 16/16; this file + `PHASE11_IMPLEMENTATION.md` |
| **Cross-cutting** | `p11_integration` 12/12 (t37–t48), `p11_e2e` 8/8 (TEST A–H), `t45_audit_chain_spans_every_phase11_namespace` |

## Definition of Done (OWNERSHIP Phase 11 rows)

- [x] **Extensions** — manifest/registry/install/validate/scan/activate/
  deactivate/update/rollback/remove over `extensions.packages`; imported
  extensions land `Registered`/unscanned, never Active; skills payload
  installs into `assets.skills` (one package authority); bindings at build
  only. Evidence: `p11_extensions` 12/12 + `test_a`.
- [x] **MCP ecosystem** — persisted `mcp.servers` records, 4-level trust
  (escalation requires clean scan), scan-first enable, spawn gate on the
  next build, deny-by-default caps per invocation, doctor rows; remote
  honest `UNAVAILABLE`. Evidence: `p7b_mcp_runtime`, `t40`, `test_b`.
- [x] **Workbench + materials** — rooms/materials as overlay records +
  references + digests; reconstruction across restart/UI/agent; drift
  detection fail-closed; no second database. Evidence: `p11_workbench`
  t13–t18, `t38`, `test_c`.
- [x] **Rooms + roles** — roles map onto existing `SubAgentKind`
  identities with audited link/unlink; unknown role refused. Evidence:
  `t39`, `test_c`, `test_h`.
- [x] **Pause/resume/steer** — cooperative durable pause, resume through
  the checkpoint authority, append-only audited steer hydrated at run
  start; boot recovery never auto-resumes paused runs. Evidence:
  `p11_workbench` t19–t24, `t46`, `test_c`.
- [x] **Cross-surface single state** — TUI + palette + CLI over one
  Runtime/store; palette-executed `computer.use` invalidates the shared
  engine's twin (proof of one engine); no UI-local authoritative state.
  Evidence: `t43`, `t44`, `h1`, `test_e`, `test_h`.
- [x] **ComputerUse** — observe → resolve (a11y → window-tree → elements
  → vision → coordinate, full `chain:` trace) → plan with structured
  alternative → gated execution → bounded verify (≤3) → twin
  invalidation; deny-by-default; honest UNAVAILABLE mechanisms. Evidence:
  `p11_computeruse` 13/13, `t47`, `test_d`.
- [x] **Computer security (M11.9)** — AgentShield scans GUI typed text
  (Deny → `PermissionDenied`, Halt → shared kill switch), findings redacted
  (`target = tool`), audit + event log free of payload, VLM grounds only,
  kill scope `computer` blocks side effects not discovery. Evidence:
  `t35`, `t42`, `test_f`, `test_d`.
- [x] **Doctor honesty** — all mandated rows present, nothing `Fail`,
  capability matrix restricted to `SUPPORTED/PARTIAL/PERMISSION_REQUIRED/
  UNAVAILABLE/UNSUPPORTED`. Evidence: `t37`, `test_g`, doctor log.
- [x] **Phases 1–10 stay GREEN; no fabricated backends; no second
  authority.** Evidence: full workspace 1380/0/0; audits below.

## Tests required (Phase 11 mandated categories) — mapping, all green

**Integration (t37–t48) — `p11_integration` 12/12**

| Required | Test |
|---|---|
| doctor rows complete, never Fail | `t37_doctor_phase11_rows_complete_and_never_fail` |
| workbench reconstruction across restart | `t38_workbench_reconstruction_across_restart` |
| roles → existing identities + audit | `t39_room_roles_map_to_existing_identities_and_audit` |
| MCP trust never bypasses gate | `t40_mcp_trust_never_bypasses_the_permission_gate` |
| marketplace honesty under Sovereign | `t41_marketplace_doctor_is_honest_under_sovereign` |
| kill scope computer (blocks, discovery survives) | `t42_kill_scope_computer_blocks_actions_not_discovery` |
| palette dispatch through one gate | `t43_palette_phase11_commands_dispatch_through_one_gate` |
| TUI reads shared snapshot | `t44_tui_computer_panel_reads_the_shared_runtime_snapshot` |
| audit chain spans all namespaces | `t45_audit_chain_spans_every_phase11_namespace` |
| paused run survives, never auto-resumed | `t46_paused_run_survives_restart_and_is_never_auto_resumed` |
| advisory structured alternatives (all actions) | `t47_advisory_structured_alternative_for_every_action` |
| cross-surface mega scenario | `t48_cross_surface_mega_scenario_one_store_one_gate` |

**E2E (TEST A–H) — `p11_e2e` 8/8**

| Required journey | Test |
|---|---|
| A extension lifecycle | `test_a_extension_lifecycle_journey` |
| B MCP ecosystem | `test_b_mcp_ecosystem_journey` |
| C workbench | `test_c_workbench_journey` |
| D computer-use | `test_d_computeruse_journey` |
| E cross-surface single state | `test_e_cross_surface_single_state` |
| F security fail-closed bundle | `test_f_security_fail_closed_bundle` |
| G doctor + capability honesty | `test_g_doctor_capability_honesty` |
| H full Phase 11 loop | `test_h_full_phase11_loop` |

## Audits

### Production-caller audit (Track W) — PASS

- **One engine**: `ComputerUse::new` is called in production only by
  `runtime.rs` (the shared `computer_engine` + registered
  `ComputerUseTool`) and `phase11.rs` (doctor's observe-only probe — never
  executes).
- **No bypass**: `computer.use` executes only via
  `rt.executor.execute` — CLI (`main.rs`), `Runtime::computer_use_dispatch`
  (palette), and agent tool calls. Engine `observe`/`resolve_target`/
  `plan`/`twin_status` readers are read-only. TUI renders
  `computer_env_snapshot()` only.
- **VLM never executes**: zero execute/`ControlOp` references in
  `openrt-vlm` (grounding trait only).
- **No second authority**: no production `ToolRegistry::new` /
  `GateExecutor::new` outside tests; no second Room/Material/Workbench
  store; no UI-local database in `openrt-tui`/`openrt-webui`.

### Security forensic audit (Track X) — PASS

- **Payload leakage**: `computeruse.rs` has zero logging statements;
  shield findings carry `target = tool` (no payload); audit entries carry
  tool + outcome only; and the durable event log previously received raw
  GUI `text`/`content` in `PermissionRequested` — **fixed this phase**:
  the gate now emits `<redacted: N chars>` for `computer.*`/`input.*`/
  `screen.*` typed fields (asserted in `test_f` over findings, audit, and
  all persisted events). Secret-like fields remain covered by the
  field-name `RedactionPolicy` (SEC-05) in every sink.
- **Gate bypass**: none — see Track W; deny-by-default proven by
  `t33`, `test_d`, CLI smoke (rc=1 with no allow rule).
- **One evaluator**: GUI typed text flows through the ONE AgentShield
  (`openrt_shield::scan` in the gate; `is_gui_tool` detector lives in
  `openrt-shield`); no parallel computer policy; halt trips the shared
  ADR-008 kill switch only.
- **Sovereign / netpol**: remote MCP honestly `UNAVAILABLE` (no remote
  client ships; `SCOPE_MCP` reserved); marketplace has no network fetch
  (`network.http` absent from default allows); `t41` asserts honesty.
- **Imported content**: extensions import `Registered`/unscanned and
  cannot activate without validate + clean scan + operator grant
  (`test_f`, `p11_extensions`).
- **Bounded retries**: `MAX_ATTEMPTS = 3` (`t31`).

## Honest limitations (documented, never silent)

1. This host lacks `ydotool`/`dotool` (pointer) and a screenshot tool —
   actions/grounding fail closed with mechanism reasons; the simulated
   backend covers the full loop in tests.
2. No a11y source is wired on this host — the resolution chain reports it
   honestly and falls through to the remaining sources.
3. Remote MCP and remote marketplace are `UNAVAILABLE`/`BLOCKED` by
   design (Sovereign + netpol), surfaced as doctor rows.
4. `media.asr` remains `WARN MODEL_MISSING` (pre-existing Phase 9).
