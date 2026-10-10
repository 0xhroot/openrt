# PHASE 12 ACCEPTANCE — Industrial Intelligence + Trading Research + Advanced Self-Improvement

Phase: **12** (OWNERSHIP `## Phase 12 — INDUSTRIAL INTELLIGENCE + TRADING
RESEARCH + ADVANCED SELF-IMPROVEMENT`, M12.1–M12.20). Design/implementation
notes: [`PHASE12_IMPLEMENTATION.md`](./PHASE12_IMPLEMENTATION.md).

## Final status

**PHASE 12 COMPLETE.**

All twenty milestones are implemented, tested, and audited. The 47
mandated integration tests (p12_01–p12_45 + 45b + 45c) and every E2E
journey (TEST A–H) are green; audits L (trading denial end-to-end) and M
(learning rollback evidence) pass; the production-caller audit found no
second authority and no gate bypass. The only non-GREEN entries are the
honest sub-capability `UNAVAILABLE` rows mandated by design (no training
backend, no anomaly model, no prefetch backend, broker execution out of
scope) plus one pre-existing `WARN` — never silent gaps, never a `Fail`.

## Capability table (per milestone — the Phase 12 verdict)

| Milestone | Capability | Verdict | Evidence |
|---|---|---|---|
| **M12.1** | Industrial domain model: typed Plant/Unit/Equipment/… records over `industrial.assets`, upsert-in-place, filters | **GREEN** | `p12_01`, `p12_02`; E2E `test_a`; doctor `industrial.data` PASS |
| **M12.2** | Relationship graph: canonical predicate set, idempotent relations, unknown predicates fail closed | **GREEN** | `p12_03`, `p12_04`; `test_a` |
| **M12.3** | Relations mirrored into the ONE `knowledge.graph`; documents enter the ONE jailed `corpus.*` ingest | **GREEN** | `p12_03`; `test_a`, E2E `test_b` (2-page PDF, digests); doctor `industrial.intelligence` PASS |
| **M12.4** | Telemetry series + trend + anomaly: missing buckets/quality never fabricated, least-squares trend, statistical detector, honest `model_assisted` UNAVAILABLE | **GREEN** | `p12_05`–`p12_09`; doctor `industrial.timeseries`/`industrial.anomaly-detector` PASS |
| **M12.5** | RCA + FMEA: OBSERVED/INFERRED/HYPOTHESIS/UNKNOWN labels, bounded reports, equipment-type FMEA with evidence links | **GREEN** | `p12_10`, `p12_11`; doctor `industrial.rca` PASS |
| **M12.6** | Heuristic risk band (INSUFFICIENT_DATA without evidence) + correlation with non-causal caveat | **GREEN** | `p12_12`, `p12_13`; smoke `industrial risk` prints `band=… (method heuristic)` |
| **M12.7** | Scenarios (KNOWN/SIMULATED/ASSUMED) + evidence-linked investigation (copilot) | **GREEN** | `p12_14`, `p12_15`; `test_a` |
| **M12.8** | Trading research data: instruments (metadata, no feed), content-digested datasets (CSV import/build, malformed fails closed), declarative strategies | **GREEN** | `p12_16`–`p12_20`; E2E `test_c`; doctor `trading.dataset-store` PASS |
| **M12.9** | Deterministic backtest: next-bar-open fills, costs/slippage, simulated marking, clean leakage report | **GREEN** | `p12_21`, `p12_22`; doctor `trading.backtest-engine` PASS |
| **M12.10** | Leakage detection for declared lookahead + walk-forward folds with honest in-sample-only scope | **GREEN** | `p12_23`, `p12_24`; `test_c` |
| **M12.11** | Research experiments + the ONE refusal point: `trading_execute` always denies, audited, CLI exit 3, no `trading.broker.*` tool | **GREEN** | `p12_25`, `p12_26`; E2E `test_d` (**Audit L**); doctor `trading.research` PASS |
| **M12.12** | Experience capture + evaluation: sealed episodes, MEASURED vs INFERRED split (`measured+inferred/v1`) | **GREEN** | `p12_27`, `p12_28`; E2E `test_e`; doctor `learning.experience-store` PASS |
| **M12.13** | Candidates from REAL execution history (skill/instinct/genome) — proposals only, never active | **GREEN** | `p12_29`, `p12_30`; `test_e` (`rt_toolseq()` history flow) |
| **M12.14** | Benchmark gate verdict matrix: promote only on measured `error_rate` gain, equal never promotes, no baseline → hold, reject never activates | **GREEN** | `p12_31`–`p12_35` incl. sync `p12_32_promotion_verdict_matrix`; doctor `learning.skill-adaptation` PASS |
| **M12.15** | Operator approval + activation only on the promote path; permissions/policy untouchable by learning | **GREEN** | `p12_34`, `p12_35`; `test_e` (validated candidate → approve → activate) |
| **M12.16** | Model adaptation experiments: `train.runs` records, adapter lineage, honest no-backend start failure + `TrainJobFailed` event, zero fabricated metrics | **GREEN** | `p12_38`, `p12_39`; E2E `test_f`; doctor `learning.model-adaptation` honest **UNAVAILABLE** |
| **M12.17** | Benchmark + promotion records: measured decisions persisted in `improve.promotions`/`bench.results`, CLI `learn promotions` | **GREEN** | `p12_31`, `p12_33`–`p12_37`; smoke `learn promotions`; doctor `learning.benchmark-store` PASS |
| **M12.18** | Learned routing signals: subordinate overlay (min 5 samples), never introduces an ineligible model | **GREEN** | `p12_40`, `p12_41`; E2E `test_g`; doctor `routing.signals` PASS |
| **M12.19** | Rollback: operator deactivate + persisted rationale + `learning.rollback` audit; genome previous-version re-apply | **GREEN** | `p12_36`, `p12_37`; `test_e` (**Audit M**); doctor `learning.rollback-capability` PASS |
| **M12.20** | Advanced memory: `cache.meta` lifecycle wired (first time), prioritized recall with method + lineage, prefetch honest UNAVAILABLE, PDF/dataset intake into one store | **GREEN** | `p12_42`–`p12_44`; E2E `test_b`, `test_g`; doctor `model.cache`/`memory.prioritized-recall` PASS, `memory.prefetch` UNAVAILABLE |

### Honest sub-capability rows (never fabricated)

| Sub-capability | Status | Reason |
|---|---|---|
| Model-assisted anomaly detection | **UNAVAILABLE** | no anomaly model backend configured — method always reported (`p12_09`, doctor `industrial.anomaly-detector` detail) |
| Training backend (SFT/LoRA/QLoRA) | **UNAVAILABLE** | environment probe: `peft` not importable — `model-start` fails with the probe's reason, emits `TrainJobFailed`, records no metrics (doctor `learning.model-adaptation`) |
| Memory prefetch | **UNAVAILABLE** | no prefetch backend — reported honestly, never faked (doctor `memory.prefetch`) |
| Broker execution / order path | **DENIED by design** | research-only scope: no `trading.broker.*` tool exists; `trading_execute` always returns research-only, audited as `denied` (doctor `trading.research`) |
| Market data feeds | **absent by design** | instruments are metadata only; datasets come from operator-supplied CSV/builds — no fabricated quotes |
| Risk `remaining_life` | **UNAVAILABLE** | no degradation model configured — risk band prints the note (smoke `industrial risk`) |

## Gates (2026-09-27, this host)

| Gate | Command | Result | Evidence |
|---|---|---|---|
| Format | `cargo fmt --all --check` | RC=0 | `/tmp/opencode/p12z_fmt.log` |
| Check + warnings | `cargo check --offline --workspace --all-targets` | RC=0, **0 warnings** | `/tmp/opencode/p12z_check.log` |
| Tests | `cargo test --offline --workspace --no-fail-fast` | **1477 passed / 0 failed / 0 ignored**, 184 `test result: ok` | `/tmp/opencode/p12_full_tests2.log` |
| Doctor | `openrt doctor` | RC=0; **78 rows: 65 PASS / 1 WARN / 12 UNAVAILABLE / 0 FAIL** | `/tmp/opencode/p12z_doctor.log` |
| CLI smoke | Phase 12 scripted transcript | **41/41 expected exit codes** (incl. `trading execute` rc=3 research-only refusal, rollback-unknown rc=1 fail-closed) | `/tmp/opencode/p12smoke/transcript.log` |
| Resource gate | free memory before full gates | satisfied (8.8 GiB / 58% free) | — |

Phase 11 baseline was **1380** tests; Phase 12 brings the workspace to
**1477** (+97: the 55 dedicated `p12_*` integration tests — 47
integration + 8 E2E — plus Phase 12 crate units: `openrt-industrial` 16,
`openrt-trading` 12, `openrt-train` 3, plus router/TUI/palette additions)
with **zero regressions** (Phases 1–11 suites all green).

WARN row: `media.asr MODEL_MISSING` (pre-existing Phase 9 host condition).
UNAVAILABLE rows: the 10 pre-existing honest probes (`genome`,
`instincts`, `learning`, `rerank.model`, `skill-execution`,
`skill-loader`, `skill-registry`, `skills`, `verifier.model`,
`mcp.remote-transport`) + 2 Phase 12 rows (`learning.model-adaptation`,
`memory.prefetch`).

## Definition of Done (OWNERSHIP Phase 12 rows)

- [x] **Industrial** — domain model + graph over `industrial.assets`/
  `industrial.analysis`, canonical relations mirrored into
  `knowledge.graph`, series with missing-data honesty, trend/detectors
  (method reported), RCA/FMEA/risk/correlation/scenario/investigate —
  advisory only, no control command, telemetry never fabricated.
  Evidence: `p12_01`–`p12_15`, `test_a`, smoke.
- [x] **Trading** — instruments/datasets/strategies over
  `trading.*`, deterministic backtest + leakage/walk-forward, experiments;
  `trading.broker.*` tools do not exist; `trading_execute` always refuses
  (the one audit point, exit 3); `KillScope::Trading` reserved and unused.
  Evidence: `p12_16`–`p12_26`, `test_c`, `test_d`.
- [x] **Experience + evaluation** — sealed `improve.episodes` records,
  MEASURED vs INFERRED evaluation over the one learning authority.
  Evidence: `p12_27`, `p12_28`, `test_e`.
- [x] **Adaptation gate** — candidates from real history → validate →
  benchmark → PROMOTE/HOLD/REJECT → operator approval → activation;
  skills/instincts/genomes never self-activate; genome rollback applies
  the previous version. Evidence: `p12_29`–`p12_35`, `test_e`.
- [x] **Model experiments** — `train.runs` records with adapter lineage,
  honest backend probe, `TrainJobFailed` on the one bus, no fabricated
  training. Evidence: `p12_38`, `p12_39`, `test_f`.
- [x] **Benchmark + promotion records** — measured decisions persisted in
  `bench.results`/`improve.promotions` with rationale; CLI
  `learn promotions` reads them. Evidence: `p12_31`, `p12_33`–`p12_37`,
  smoke.
- [x] **Routing signals** — subordinate overlay only; ineligible models
  never introduced. Evidence: `p12_40`, `p12_41`, `test_g`.
- [x] **Memory integration** — `cache.meta` wired, prioritized recall
  with recorded method/lineage, prefetch honest UNAVAILABLE, one
  ingest/store. Evidence: `p12_42`–`p12_44`, `test_b`, `test_g`.
- [x] **Surfaces + doctor** — CLI `industrial/trading/learn *`, TUI
  `Intel` tab, WebUI palette commands over the one Runtime; 19 Phase 12
  doctor rows present, nothing `Fail`. Evidence: `p12_45`, `p12_45b`,
  `test_h`, smoke 41/41, doctor log.
- [x] **Phases 5–11 stay GREEN; no fabricated backends; no second
  authority.** Evidence: full workspace 1477/0/0; audits below.

## Tests required — mapping, all green

**Integration (47) — `p12_integration` 47/47**

| Required area | Tests |
|---|---|
| industrial domain + graph | `p12_01_asset_put_roundtrip_and_kind_filter`, `p12_02_asset_upsert_updates_in_place`, `p12_03_relation_canonical_idempotent_mirrored`, `p12_04_relation_unknown_predicate_fails_closed` |
| series / trend / anomaly | `p12_05_series_append_query_missing_buckets`, `p12_06_series_missing_quality_excluded`, `p12_07_trend_rising_and_insufficient`, `p12_08_anomaly_statistical_detects_spike`, `p12_09_anomaly_model_assisted_honest_unavailable` |
| RCA / FMEA / risk / correlation / scenario / investigate | `p12_10_rca_evidence_labels_and_bounds`, `p12_11_fmea_table_for_equipment_type`, `p12_12_risk_insufficient_then_heuristic_band`, `p12_13_correlate_groups_with_noncausal_caveat`, `p12_14_scenario_statement_kinds_and_advisory`, `p12_15_investigate_sections_with_evidence` |
| trading instruments/datasets/strategies | `p12_16`–`p12_20` (malformed CSV fails closed) |
| backtest / leakage / walk-forward / experiments | `p12_21_deterministic_and_simulated`, `p12_22_leakage_report_clean`, `p12_23_leakage_detected_for_declared_lookahead`, `p12_24_walk_forward_folds_and_honest_scope`, `p12_25_experiment_record_roundtrip` |
| the one refusal point | `p12_26_execute_denied_and_audited` |
| episodes + candidates | `p12_27`, `p12_28`, `p12_29_candidate_skill_from_real_history`, `p12_30_instinct_and_genome_candidates_are_proposals` |
| promotion gate + rollback | `p12_31`–`p12_37` (incl. sync `p12_32_promotion_verdict_matrix`) |
| model experiments | `p12_38`, `p12_39` (honest fail) |
| routing signals | `p12_40`, `p12_41` (never introduce ineligible model) |
| cache / recall / prefetch | `p12_42_cache_lifecycle_persists`, `p12_43_prioritized_recall_method_and_lineage`, `p12_44_prefetch_unavailable_and_doctor_says_so` |
| doctor + tools + audit coverage | `p12_45_phase12_doctor_rows_complete_never_fail`, `p12_45b_phase12_tools_registered_and_gated`, `p12_45c_phase12_audit_action_coverage` |

**E2E (TEST A–H) — `p12_e2e` 8/8**

| Required journey | Test |
|---|---|
| A industrial advisory journey (M12.1–M12.7) | `test_a_industrial_advisory_journey` |
| B PDF intake into the one store (memory fabric) | `test_b_pdf_intake_into_one_store` |
| C trading research journey (M12.8–M12.11) | `test_c_trading_research_journey` |
| D trading refusal bundle (Audit L) | `test_d_trading_refusal_bundle` |
| E learning promotion loop + rollback (M12.12–M12.19, Audit M) | `test_e_learning_promotion_loop` |
| F model experiment honest fail (M12.16) | `test_f_model_experiment_honest_fail` |
| G signal / recall / prefetch / cache honesty (M12.18, M12.20) | `test_g_signal_recall_prefetch_cache_honesty` |
| H full Phase 12 loop over one store | `test_h_full_phase12_loop` |

## Audits

### Audit L — trading denial end-to-end — PASS

- `test_d`: the runtime method fails research-only; the palette surfaces
  the SAME denial (`PaletteOutcome::Denied`); the registry contains the
  research tools and **no `trading.broker.*` tool**; both attempts land as
  `trading.broker.execute` audit rows with `result = denied` (≥2).
- CLI smoke: `trading execute --order …` prints
  `DENIED (research only): …` and exits **3**; the order JSON appears
  only inside the denial audit entry.
- No order ever reaches a venue — there is no venue path in the code.

### Audit M — learning rollback evidence — PASS

- `test_e`: promote activates the skill; operator rollback deactivates it
  and persists a rollback record carrying rationale; all three decisions
  (hold/promote/rollback) persist in `improve.promotions` with measured
  `candidate_metrics`; audit actions `learning.{episode,
  skill_candidate,promotion,rollback}` each ≥1; `audit_verify()` ≥5.
- CLI smoke: `learn promotions` prints the persisted decisions;
  `learn rollback-skill` on an unknown id fails closed (rc=1).

### Production-caller / second-authority audit — PASS

- All eight Phase 12 tools are registered only by the runtime build
  (`phase12_tools` → `ToolRegistry`), gated by the ONE `GateExecutor`,
  with matching `FabricMeta` (`p12_45b`).
- No second `ToolRegistry::new`/`GateExecutor::new`/store/router in
  production code; industrial relations go through `openrt-evidence`,
  documents through `openrt-ingest`, decisions through the Phase 10
  improve/bench lifecycle (`p12_45c` verifies the full audit-action
  coverage in one chain).
- `KillScope::Trading` remains reserved (ADR-008) and unused; learning
  code has no permission/policy writes.

### Security honesty — PASS

- Default allow rules grant no Phase 12 tool cap; `p12_45b` proves
  gating. `trading.execute` and `phase12.info` in the palette need no
  auth only because they are read-only/refusing (the execute command can
  only produce a denial).
- Self-improvement cannot change permissions or self-activate (M12.15
  evidence); candidates stay proposals until the measured gate.

## Honest limitations (documented, never silent)

1. This host has no training backend (`peft` not importable) and no
   anomaly model — both report `UNAVAILABLE` with probe reasons; tests
   assert the honest failure, not a fake success.
2. Memory prefetch has no backend — doctor row and API agree.
3. Trading is research-only by charter: no broker, no market feeds;
   datasets are operator-supplied. Instruments carry metadata only.
4. `routing.signals` needs ≥5 samples and only re-orders eligible
   candidates — base routing stays authoritative.
5. `media.asr` remains `WARN MODEL_MISSING` (pre-existing Phase 9).
