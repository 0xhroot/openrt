# PHASE 12 IMPLEMENTATION — Industrial Intelligence + Trading Research + Advanced Self-Improvement

Phase: **12** (OWNERSHIP `## Phase 12 — INDUSTRIAL INTELLIGENCE + TRADING
RESEARCH + ADVANCED SELF-IMPROVEMENT`, milestones M12.1–M12.20;
consolidates the original Industrial Intelligence (P18), Trading Research
(P19), and Advanced Self-Improvement (P20) packs plus the memory-fabric
backlog).
Acceptance evidence (gates, capability table, tests, audits):
[`PHASE12_ACCEPTANCE.md`](./PHASE12_ACCEPTANCE.md).

## 1. Scope and allocation

Phase 12 layers domain intelligence ON TOP of the frozen Phases 5–11
authorities: two newly allocated crates (`openrt-industrial`,
`openrt-trading`), the Phase 4 `openrt-train` allocation implemented, and
extensions to self-improvement/bench/router/memory/cache — **no second
runtime, router, workflow engine, tool registry, permission system,
audit chain, store, memory database, or security authority** (OWNERSHIP
Phase 12 reconciliation note, ADR-006):

| Subsystem (milestone) | Crate(s) | Store namespace |
|---|---|---|
| Industrial domain model + asset graph (M12.1–M12.3) | `openrt-industrial` (newly allocated) | `industrial.assets`, `industrial.analysis` |
| Sensor/timeseries + trends + anomaly (M12.4) | `openrt-industrial` | `industrial.series`, `industrial.analysis` |
| RCA / FMEA / risk / correlation / scenario / copilot (M12.5–M12.7) | `openrt-industrial` | `industrial.analysis` |
| Trading instruments/datasets/strategies (M12.8) | `openrt-trading` (newly allocated) | `trading.instruments`, `trading.datasets`, `trading.strategies` |
| Backtest + leakage/walk-forward + reports (M12.9–M12.11) | `openrt-trading` + runtime surfaces | `trading.backtests`, `trading.experiments` |
| Experience capture + episode evaluation (M12.12) | `openrt-selfimprove` (extended) | `improve.episodes` |
| Candidates + benchmark gate + approval/activation (M12.13–M12.15) | `openrt-workspace-assets` + `openrt-runtime` (Phase 10 pipelines) | `assets.*`, `improve.proposals`, `improve.promotions` |
| Model adaptation experiments (M12.16) | `openrt-train` (Phase 4 allocation implemented) | `train.runs` |
| Benchmark + promotion records (M12.17) | `openrt-bench` + `openrt-runtime` | `bench.results`, `improve.promotions` |
| Learned routing signals (M12.18) | `openrt-router` (extended, additive) | `routing.signals` |
| Rollback evidence (M12.19) | `openrt-workspace-assets` + `openrt-runtime` | `improve.promotions` |
| Advanced memory integration: cache lifecycle + prioritized recall + prefetch honesty (M12.20) | `openrt-model-cache` (wired first time) + `openrt-memory` | `cache.meta` (existing), memory fabric via `corpus.*` |

Single authorities preserved: one `ToolRegistry`/`GateExecutor` (the
`openrt-runtime` build registers all eight Phase 12 tools through it —
`industrial.{assets,series,analysis,copilot}`, `trading.{dataset,strategy,
backtest}`, `learn.experience`, each with `FabricMeta`); `openrt-audit`
the only chain (ADR-002); `PersistenceStore` the only database
(ADR-006); `openrt-shield` the only security evaluator;
`openrt-permissions`/`openrt-policy` the permission authorities;
`openrt-safety` the only kill switch (`KillScope::Trading` stays reserved
and unused); `knowledge.graph` the only graph (industrial relations are
mirrored through `openrt_evidence::upsert_entity`/`add_relation`);
`corpus.*` the only ingest (trading datasets and PDF intake ride it);
`openrt-timeseries` remains the execution-history ledger (never
overloaded); the Phase 3 model registry/router and Phase 10 improve/bench
lifecycle stay the adaptation authorities.

## 2. The honest contract

1. **Trading is research-only** — `Runtime::trading_execute(&Value)` is
   the one audit point and always returns `Error::Trading(ResearchOnly)`;
   no `trading.broker.*` tool exists (registry assertion); CLI
   `trading execute` exits 3; palette `trading.execute` dispatches to a
   `Denied` outcome. The order JSON is recorded in the denial audit entry,
   never sent anywhere.
2. **Industrial is advisory-only** — no physical-control command exists
   in any surface; every analysis row carries source/window/parameters/
   timestamp; telemetry is only what was appended (missing buckets stay
   missing, never interpolated; `quality=missing` points are excluded).
3. **Self-improvement cannot self-activate** — candidates are proposals;
   validation, AgentShield, and a measured benchmark gate run before any
   approve/activate; permissions/policy are untouchable by learning.
4. **Measured-only decisions** — `promotion_verdict()` compares the
   measured `error_rate` against the recorded baseline; without a
   baseline the verdict is `hold` (never a silent promote); equal
   measurements never promote; rejections never activate.
5. **Honest UNAVAILABLE** — model-assisted anomaly detection, training
   backends (`model-start` fails with the probe's reason and emits
   `TrainJobFailed`), memory prefetch, broker execution, and risk
   `remaining_life` all report `UNAVAILABLE` with mechanism reasons.
6. **Doctor rows never `Fail`** — 19 Phase 12 rows use only
   `PASS/WARN/UNAVAILABLE` (see §3.5).
7. **Routing signals stay subordinate** — `routing.signals` only
   re-orders candidates that already passed policy/mode/eligibility
   checks (min 5 samples); it never introduces an ineligible model.

## 3. Key design decisions

### 3.1 The measured promotion gate (M12.14, M12.17)

- `learn_promote_skill/instinct/genome(id, operator, baseline)` runs the
  bench probe, records `bench.results`, and derives the verdict from
  `error_rate` only — the one comparable metric across artifacts.
- The Promote path performs `approve_candidate` + `activate_candidate`
  internally, so callers submit **validated, not pre-approved**
  candidates; approve additionally requires an authorized operator
  (default operator = `$USER` via `asset_operators`).
- Every decision persists as `promotion.skill.{id}` /
  `rollback.skill.{id}` records in `improve.promotions` with
  `candidate_metrics` and rationale (hold and promote therefore use
  different artifact ids on purpose).

### 3.2 Candidates from real execution history (M12.13)

- `derive` tools require actual tool evidence: a session runs
  `run_task` and `history_tool_evidence` polls for it. The deterministic
  echo mock provider produces no tool evidence, so history-based flows
  use `MockProvider::tool_sequence` (`rt_toolseq()` in tests).
- Instinct/genome candidates are proposals only; applying an evolved
  genome stays operator-gated with previous-version rollback (Phase 10).

### 3.3 One graph, one ingest (M12.1–M12.3, M12.8, M12.20)

- Relations are canonical (predicate-restricted set, idempotent) and
  mirrored as triples into `knowledge.graph` — the Phase 9 authority.
- Datasets and documents enter through the jailed `corpus.*` ingest
  (`IngestJail`), content-addressed; a two-page PDF and a 40-bar CSV
  both land in the same store with digests (E2E TEST B).

### 3.4 Routing signals, cache wiring, prioritized recall (M12.18, M12.20)

- `openrt-router::signals` is an additive overlay; `openrt-model-cache`
  is wired into the runtime for the first time (`cache.meta`);
  `openrt_memory::prioritize` blends rank + recency (half-life) with id
  tie-break and digest/`occurred_at` lineage; prefetch without a backend
  is honest `UNAVAILABLE` (doctor row agrees).

### 3.5 Doctor rows (M12.16-adjacent honesty)

Nineteen rows: `industrial.{intelligence,data,timeseries,anomaly-detector,
rca}`, `trading.{research,backtest-engine,dataset-store}`,
`learning.{subsystem,experience-store,skill-adaptation,genome-adaptation,
model-adaptation,benchmark-store,rollback-capability}`, `model.cache`,
`memory.{prioritized-recall,prefetch}`, `routing.signals`. On this host:
17 `PASS`, `learning.model-adaptation` + `memory.prefetch` `UNAVAILABLE`.

## 4. Surfaces

| Surface | Phase 12 surface area |
|---|---|
| CLI | `industrial *` (asset-put/relate/assets/asset/series-append/series-query/trend/anomaly/rca/fmea/risk/scenario/investigate), `trading *` (instrument/dataset/strategy/backtest/walk-forward/experiment + `execute` refusal exit 3), `learn *` (episodes, benchmark, promote-*, rollback-*, candidate-*, model-*, routing-*, promotions) |
| TUI | Ops tab `Intel` (index 7, key `8`): industrial assets, trading instruments/datasets/strategies, trading execute `DENIED (research only)`, experience episodes, promotion decisions, routing signals, model cache entries, phase12 doctor counts — all from `Runtime` reads |
| WebUI | Palette `industrial.assets`, `learn.episodes`, `learn.promotions`, `phase12.info`, `trading.datasets`, `trading.execute` over `CommandPalette::with_runtime`; dispatch runs outside async context via `spawn_blocking`; non-privileged commands need no token |
| Doctor | The 19 rows above + one verifying audit chain (`audit verify`) |

No feature exists only inside a UI: every palette/TUI/CLI action is a
Runtime API over the one store; the palette `phase12.info` JSON and the
TUI Intel rows derive from the same runtime reads the CLI prints.

## 5. Test architecture

| Suite | Count | Covers |
|---|---|---|
| `p12_integration` | 47 | p12_01–p12_45 (+45b, +45c): industrial 01–15, trading 16–26, learning 27–37, model 38–39, routing 40–41, memory/cache 42–44, doctor/tools/audit 45,45b,45c (`p12_32_promotion_verdict_matrix` is the sync matrix test) |
| `p12_e2e` | 8 | TEST A–H: industrial journey, PDF intake, trading research, refusal bundle, learning loop, honest model fail, signal/recall/cache honesty, full loop |
| crate units | industrial 16, trading 12, train 3, router 11, memory 19, model-cache 13, selfimprove 3, tui 15, webui (`tests/webui.rs`) 28, runtime 78 | phase internals |

Dedicated Phase 12 integration total: **55 tests** (47 + 8). Workspace
total: **1477 passed / 0 failed / 0 ignored** (Phase 11 baseline 1380;
the delta is the 55 `p12_*` tests plus Phase 12 crate units — new-crate
units, router signals, TUI Intel panel, palette command list — with zero
regressions). See [`PHASE12_ACCEPTANCE.md`](./PHASE12_ACCEPTANCE.md).
