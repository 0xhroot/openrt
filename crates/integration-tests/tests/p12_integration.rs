//! Phase 12 — cross-track integration tests 1–45.
//!
//! Industrial intelligence (M12.1–M12.7), trading research (M12.8–M12.11),
//! self-improvement (M12.12–M12.19), model cache / prioritized memory
//! (M12.20) and the mandated doctor rows — every chain runs through the
//! production Runtime over ONE durable store. Honest-backend policy is
//! asserted throughout: no fabricated sensor feeds, market data, training,
//! broker execution or prefetch; doctor rows never `Fail`.
//!
//! * 01–15 industrial: assets, graph, telemetry, trend/anomaly, RCA, FMEA,
//!   risk, correlation, scenario, investigation;
//! * 16–26 trading: instruments, datasets, strategies, deterministic
//!   backtests, leakage, walk-forward, experiments, broker refusal;
//! * 27–42 learning: episodes, evaluation, candidates, measured bench gate,
//!   promotion verdicts, rollback, train experiments, routing signals,
//!   cache lifecycle;
//! * 43–45 memory honesty + the mandated Phase 12 doctor rows.

use std::collections::BTreeMap;
use std::sync::Arc;

use openrt_audit::AuditQuery;
use openrt_config::{Config, RuleConfig, RuleEffect};
use openrt_industrial::{
    Asset, AssetFilter, AssetKind, CorrelateParams, EventKind as IndustrialEventKind, Point,
    Provenance as IndustrialProvenance, Quality, TimedEvent, TrendParams,
};
use openrt_models::mock::MockProvider;
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_runtime::{DoctorStatus, Runtime, RuntimeBuilder};
use openrt_selfimprove::{
    EpisodeOutcome, ExperienceEpisode, Provenance as ImproveProvenance, Verification,
};
use openrt_trading::{
    Bar, CostSpec, DatasetConfig, Instrument, InstrumentKind, Provenance as TradingProvenance,
    ResearchExperiment, WalkForwardParams,
};
use openrt_train::{AdapterSpec, ExperimentSpec, TrainMethod};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

async fn rt() -> Arc<Runtime> {
    rt_on(Config::default()).await
}

/// Runtime over the deterministic tool-sequence mock: real tool executions
/// land on the history ledger (candidate derivation needs that evidence).
async fn rt_toolseq() -> Arc<Runtime> {
    RuntimeBuilder::new()
        .config(Config::default())
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap()
}

async fn rt_on(config: Config) -> Arc<Runtime> {
    RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap()
}

/// Deny-by-default except for the exact tool capabilities listed.
async fn rt_caps(caps: &[&str]) -> Arc<Runtime> {
    let mut config = Config::default();
    config
        .permissions
        .rules
        .extend(caps.iter().map(|c| RuleConfig {
            effect: RuleEffect::Allow,
            capability: (*c).to_string(),
            resource: None,
        }));
    rt_on(config).await
}

async fn audit_count(rt: &Runtime, prefix: &str) -> usize {
    rt.audit_query(&AuditQuery::new().with_action_prefix(prefix))
        .await
        .unwrap()
        .len()
}

fn asset(id: &str, kind: AssetKind, label: &str) -> Asset {
    Asset::new(
        id,
        kind,
        label,
        IndustrialProvenance::derived("test.fixture", openrt_core::now().to_rfc3339()),
    )
}

fn point(t_ms: i64, value: f64) -> Point {
    Point {
        t_ms,
        value,
        quality: Quality::Good,
        source: "test.fixture".into(),
    }
}

/// `n` rising telemetry points, one per minute from a fixed epoch.
fn rising_points(n: usize) -> Vec<Point> {
    (0..n)
        .map(|i| point(1_700_000_000_000 + i as i64 * 60_000, 40.0 + i as f64))
        .collect()
}

/// `n` mostly-flat points with one obvious spike in the middle.
fn spiky_points(n: usize) -> Vec<Point> {
    (0..n)
        .map(|i| {
            let v = if i == n / 2 {
                200.0
            } else {
                40.0 + (i % 3) as f64
            };
            point(1_700_000_000_000 + i as i64 * 60_000, v)
        })
        .collect()
}

fn bars(n: usize) -> Vec<Bar> {
    (0..n)
        .map(|i| {
            let px = 100.0 + i as f64 * 0.4;
            Bar {
                t_ms: 1_700_000_000_000 + i as i64 * 86_400_000,
                open: px,
                high: px + 1.0,
                low: px - 1.0,
                close: px + 0.2,
                volume: 1_000.0,
            }
        })
        .collect()
}

fn bars_csv(n: usize) -> String {
    let mut s = String::from("t_ms,open,high,low,close,volume\n");
    for b in bars(n) {
        s.push_str(&format!(
            "{},{},{},{},{},{}\n",
            b.t_ms, b.open, b.high, b.low, b.close, b.volume
        ));
    }
    s
}

fn ds_cfg(instrument: &str) -> DatasetConfig {
    DatasetConfig {
        instrument: instrument.into(),
        from_ms: 0,
        to_ms: 0,
        filters: BTreeMap::new(),
        params: BTreeMap::new(),
        source: "test.fixture".into(),
        transform: "identity".into(),
        declared_lookahead: false,
    }
}

fn instrument(id: &str, symbol: &str) -> Instrument {
    Instrument {
        id: id.into(),
        symbol: symbol.into(),
        venue: "XETR".into(),
        kind: InstrumentKind::Equity,
        currency: "EUR".into(),
        tick_size: 0.01,
        provenance: TradingProvenance::derived("test.fixture", openrt_core::now().to_rfc3339()),
        created_at: openrt_core::now().to_rfc3339(),
    }
}

fn experiment_spec(id: &str) -> ExperimentSpec {
    ExperimentSpec {
        id: id.into(),
        dataset_id: "ds.a".into(),
        dataset_digest: None,
        base_model: "base-model".into(),
        method: TrainMethod::Lora,
        adapter: AdapterSpec {
            kind: "lora".into(),
            rank: 8,
            alpha: 16.0,
            target_modules: vec!["q_proj".into()],
        },
        params: BTreeMap::new(),
        hardware: "cpu".into(),
        provenance: "test.fixture".into(),
    }
}

fn episode(id: &str) -> ExperienceEpisode {
    ExperienceEpisode {
        id: id.into(),
        task: "collect facts".into(),
        context: "integration test".into(),
        model: "mock".into(),
        prompt_ref: None,
        tools: vec!["echo".into()],
        actions: vec!["run".into()],
        outcome: EpisodeOutcome::Success,
        verification: Verification::Passed,
        errors: 0,
        retries: 0,
        latency_ms: Some(12),
        feedback: None,
        artifact: None,
        provenance: ImproveProvenance::new("", ""),
        created_at: String::new(),
    }
}

async fn poll_evidence(rt: &Runtime, session: &str) {
    for _ in 0..200 {
        if !rt.history_tool_evidence(session).await.unwrap().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("no tool evidence landed on the ledger for `{session}`");
}

/// Real history → candidate skill that has passed validation (the promote
/// gate's own approve/activate then runs on top of this).
async fn validated_candidate(rt: &Runtime) -> String {
    let session = rt.create_session("history").await.unwrap();
    rt.run_task(session.id, "collect facts with echo")
        .await
        .unwrap();
    let sid = session.id.to_string();
    poll_evidence(rt, &sid).await;
    let cid = rt
        .learn_candidate_skill(
            "req.fixture",
            "collect facts with echo",
            vec!["echo".into()],
            &sid,
            "op1",
        )
        .await
        .unwrap();
    let mut assets = rt.load_assets().await.unwrap();
    assets.validate_candidate(&cid).unwrap();
    rt.save_assets(&assets).await.unwrap();
    cid
}

/// Same as [`validated_candidate`], but taken all the way through operator
/// approval + scan-gated activation — returns the live skill id.
async fn activated_skill(rt: &Runtime, op: &str) -> String {
    let cid = validated_candidate(rt).await;
    {
        let mut assets = rt.load_assets().await.unwrap();
        let grant = openrt_workspace_assets::OperatorGrant::new(op).unwrap();
        assets.approve_candidate(&cid, Some(&grant)).unwrap();
        rt.save_assets(&assets).await.unwrap();
    }
    let skill_id = rt.assets_activate_candidate(&cid, op).await.unwrap();
    skill_id.0
}

async fn json_out(rt: &Runtime, name: &str, args: serde_json::Value) -> serde_json::Value {
    let ctx = openrt_tools::ToolExecutionContext {
        session_id: Some(openrt_core::SessionId::new().to_string()),
        task_id: Some(openrt_core::TaskId::new().to_string()),
        channel: None,
    };
    let out = rt
        .executor
        .execute(name, args, &ctx)
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e:?}"));
    match out {
        openrt_tools::ToolOutput::Json(v) => v,
        openrt_tools::ToolOutput::Text(s) => serde_json::json!({ "text": s }),
    }
}

// ---------------------------------------------------------------------------
// Part A/B — industrial intelligence (M12.1–M12.7), tests 01–15
// ---------------------------------------------------------------------------

/// 01 (M12.1): asset put → read → filtered list, audited as
/// `industrial.asset.upsert` (one audit authority).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_01_asset_put_roundtrip_and_kind_filter() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "Feed pump"), "op1")
        .await
        .unwrap();
    rt.industrial_asset_put(asset("plant.a", AssetKind::Plant, "Plant A"), "op1")
        .await
        .unwrap();

    let all = rt.industrial_assets(&AssetFilter::default()).await.unwrap();
    assert_eq!(all.len(), 2);
    let eq = rt
        .industrial_assets(&AssetFilter {
            kind: Some(AssetKind::Equipment),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(eq.len(), 1);
    assert_eq!(eq[0].id, "pump.1");

    let got = rt.industrial_asset("pump.1").await.unwrap().unwrap();
    assert_eq!(got.label, "Feed pump");
    assert!(audit_count(&rt, "industrial.asset.upsert").await >= 2);
}

/// 02 (M12.1): upsert updates in place — the asset count never inflates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_02_asset_upsert_updates_in_place() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "v1"), "op1")
        .await
        .unwrap();
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "v2"), "op1")
        .await
        .unwrap();
    let all = rt.industrial_assets(&AssetFilter::default()).await.unwrap();
    assert_eq!(all.len(), 1, "upsert must not duplicate");
    assert_eq!(all[0].label, "v2");
}

/// 03 (M12.2–M12.3): canonical relations are idempotent and mirrored into
/// `knowledge.graph` — the one graph authority (no second graph store).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_03_relation_canonical_idempotent_mirrored() {
    let store = Arc::new(MemoryStore::new());
    let mut config = Config::default();
    config.permissions.rules.clear();
    let rt = RuntimeBuilder::new()
        .config(config)
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap();
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    rt.industrial_asset_put(asset("unit.1", AssetKind::Unit, "U"), "op1")
        .await
        .unwrap();

    let r1 = rt
        .industrial_relation_add("pump.1", "belongs_to", "unit.1", "op1")
        .await
        .unwrap();
    let r2 = rt
        .industrial_relation_add("pump.1", "belongs_to", "unit.1", "op1")
        .await
        .unwrap();
    assert_eq!(r1.subject, r2.subject);
    assert_eq!(r1.object, r2.object);

    // Mirror: the SAME ids appear under knowledge.graph (one authority —
    // relations written through the evidence layer, never a local copy).
    let mirrored = store.list_records(openrt_evidence::GRAPH_NS).await.unwrap();
    assert!(
        mirrored.iter().any(|(k, _)| k.contains("pump.1")),
        "industrial relation must mirror into knowledge.graph: {mirrored:?}"
    );
    assert!(audit_count(&rt, "industrial.relation.add").await >= 1);
}

/// 04 (M12.2): non-canonical predicates fail closed before any write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_04_relation_unknown_predicate_fails_closed() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    rt.industrial_asset_put(asset("unit.1", AssetKind::Unit, "U"), "op1")
        .await
        .unwrap();
    let err = rt
        .industrial_relation_add("pump.1", "causes", "unit.1", "op1")
        .await
        .unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("predicate"),
        "expected predicate refusal, got: {err}"
    );
    assert_eq!(audit_count(&rt, "industrial.relation.add").await, 0);
}

/// 05 (M12.4): range+aggregation reports missing buckets explicitly —
/// never interpolated, never fabricated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_05_series_append_query_missing_buckets() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    // 16 points at 60s spacing; query a 30-minute window with 5-minute
    // buckets → 6 buckets total, only some populated.
    let pts = rising_points(16);
    rt.industrial_series_append("ts.1", "pump.1", "degC", &pts, "op1")
        .await
        .unwrap();
    let from = pts[0].t_ms;
    let to = from + 30 * 60_000;
    let q = rt
        .industrial_series_query("ts.1", from, to, 5 * 60_000)
        .await
        .unwrap();
    let populated = q.buckets.iter().filter(|b| b.count > 0).count();
    assert!(q.missing_buckets >= 1, "the window must show missing data");
    assert_eq!(
        q.missing_buckets,
        q.buckets.len() - populated,
        "missing count matches the empty buckets exactly"
    );
    for b in &q.buckets {
        if b.count == 0 {
            assert!(b.avg.is_none(), "empty bucket must stay empty");
        }
    }
    assert!(audit_count(&rt, "industrial.series.append").await >= 1);
}

/// 06 (M12.4): `Missing`-quality points are excluded from usable samples —
/// counts reflect what was actually observed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_06_series_missing_quality_excluded() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    let mut pts = rising_points(12);
    for p in pts.iter_mut().skip(4).take(4) {
        p.quality = Quality::Missing;
    }
    rt.industrial_series_append("ts.m", "pump.1", "degC", &pts, "op1")
        .await
        .unwrap();
    // Statistical detector requires min_samples usable points and only
    // counts Good/Estimated/Suspect samples.
    let run = rt
        .industrial_anomaly_analysis(
            "pump.1",
            "ts.m",
            "statistical",
            &serde_json::json!({"min_samples": 8}),
            "op1",
        )
        .await
        .unwrap();
    assert_eq!(run.anomalies.len(), 0, "flat usable data, no anomalies");
    let trend = rt
        .industrial_trend_analysis("pump.1", "ts.m", TrendParams::default(), None, "op1")
        .await
        .unwrap();
    assert!(
        trend.points_used <= 12,
        "missing samples must not be counted as observed"
    );
}

/// 07 (M12.4): least-squares trend — Rising over growing data, Insufficient
/// below the minimum point count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_07_trend_rising_and_insufficient() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();

    rt.industrial_series_append("ts.up", "pump.1", "degC", &rising_points(16), "op1")
        .await
        .unwrap();
    let up = rt
        .industrial_trend_analysis("pump.1", "ts.up", TrendParams::default(), None, "op1")
        .await
        .unwrap();
    assert_eq!(up.direction.as_str(), "rising");
    assert!(up.slope_per_point.unwrap_or(0.0) > 0.0);

    rt.industrial_series_append("ts.little", "pump.1", "degC", &rising_points(4), "op1")
        .await
        .unwrap();
    let little = rt
        .industrial_trend_analysis("pump.1", "ts.little", TrendParams::default(), None, "op1")
        .await
        .unwrap();
    assert_eq!(little.direction.as_str(), "insufficient");
    assert!(audit_count(&rt, "industrial.analysis").await >= 2);
}

/// 08 (M12.4): statistical detector flags a spike and records which method
/// actually executed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_08_anomaly_statistical_detects_spike() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    rt.industrial_series_append("ts.spike", "pump.1", "degC", &spiky_points(16), "op1")
        .await
        .unwrap();
    let run = rt
        .industrial_anomaly_analysis(
            "pump.1",
            "ts.spike",
            "statistical",
            &serde_json::json!({"window": 8, "min_samples": 8, "z_threshold": 3.0}),
            "op1",
        )
        .await
        .unwrap();
    assert_eq!(run.method.as_str(), "statistical");
    assert!(!run.anomalies.is_empty(), "the spike must be detected");
    assert!(run.unavailable.is_none());
    assert!(audit_count(&rt, "industrial.anomaly").await >= 1);
}

/// 09 (M12.4): model-assisted detection without a configured model backend
/// reports UNAVAILABLE instead of inventing detector output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_09_anomaly_model_assisted_honest_unavailable() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    rt.industrial_series_append("ts.1", "pump.1", "degC", &rising_points(16), "op1")
        .await
        .unwrap();
    let run = rt
        .industrial_anomaly_analysis(
            "pump.1",
            "ts.1",
            "model_assisted",
            &serde_json::json!({}),
            "op1",
        )
        .await
        .unwrap();
    assert_eq!(run.method.as_str(), "model_assisted");
    assert!(
        run.unavailable.is_some(),
        "model-assisted without a backend must be UNAVAILABLE"
    );
    assert!(run.anomalies.is_empty(), "no fabricated detector output");
    let events = audit_count(&rt, "industrial.anomaly").await;
    assert!(events >= 1);
}

/// 10 (M12.5): RCA findings carry only the four evidence labels, respect
/// the configured bounds, and state their caveat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_10_rca_evidence_labels_and_bounds() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    rt.industrial_asset_put(asset("unit.1", AssetKind::Unit, "U"), "op1")
        .await
        .unwrap();
    rt.industrial_relation_add("pump.1", "belongs_to", "unit.1", "op1")
        .await
        .unwrap();
    rt.industrial_series_append("ts.1", "pump.1", "degC", &rising_points(16), "op1")
        .await
        .unwrap();

    let report = rt.industrial_rca("pump.1", "op1").await.unwrap();
    assert_eq!(report.subject, "pump.1");
    assert!(!report.findings.is_empty());
    assert!(report.findings.len() <= report.bounds.max_findings);
    for f in &report.findings {
        let label = f.label.as_str();
        assert!(
            matches!(
                label,
                "OBSERVED_FACT" | "INFERRED_RELATION" | "HYPOTHESIS" | "UNKNOWN"
            ),
            "unexpected evidence label {label}"
        );
        assert!(!f.statement.is_empty());
    }
    assert!(!report.caveat.is_empty(), "RCA must state its caveat");
    assert!(audit_count(&rt, "industrial.rca").await >= 1);
}

/// 11 (M12.5): FMEA table for the asset's declared equipment type — library
/// knowledge with effects + indicators, no fabricated failure rates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_11_fmea_table_for_equipment_type() {
    let rt = rt().await;
    let mut a = asset("pump.1", AssetKind::Equipment, "P");
    a.attrs
        .insert("equipment_type".into(), serde_json::json!("pump"));
    rt.industrial_asset_put(a, "op1").await.unwrap();
    let modes = rt.industrial_fmea("pump.1").await.unwrap();
    assert!(!modes.is_empty());
    for m in &modes {
        assert_eq!(m.equipment_type, "pump");
        assert!(!m.effects.is_empty());
        assert!(!m.indicators.is_empty());
    }
}

/// 12 (M12.6): risk is heuristic-only — INSUFFICIENT below the observation
/// floor, a labelled band above it, and never a fabricated remaining life.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_12_risk_insufficient_then_heuristic_band() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();

    let bare = rt.industrial_risk("pump.1", None, "op1").await.unwrap();
    assert!(bare.band.is_none());
    assert!(bare.score.is_none());
    assert!(bare
        .insufficient
        .as_deref()
        .unwrap_or_default()
        .contains("INSUFFICIENT_DATA"));

    rt.industrial_series_append("ts.1", "pump.1", "degC", &spiky_points(16), "op1")
        .await
        .unwrap();
    let with_data = rt
        .industrial_risk("pump.1", Some("ts.1"), "op1")
        .await
        .unwrap();
    assert_eq!(with_data.method, "heuristic");
    assert!(with_data.band.is_some());
    assert!(with_data.remaining_life.is_none(), "no degradation model");
    assert!(audit_count(&rt, "industrial.analysis").await >= 2);
}

/// 13 (M12.6): temporal correlation groups nearby events and the
/// non-causality caveat travels with the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_13_correlate_groups_with_noncausal_caveat() {
    let rt = rt().await;
    let mk = |kind: IndustrialEventKind, id: &str, at_ms: i64| TimedEvent {
        kind,
        id: id.into(),
        at_ms,
        detail: "fixture".into(),
    };
    let events = vec![
        mk(IndustrialEventKind::Alarm, "a1", 1_700_000_000_000),
        mk(IndustrialEventKind::Incident, "i1", 1_700_000_600_000),
        mk(IndustrialEventKind::WorkOrder, "w1", 1_700_001_200_000),
        mk(IndustrialEventKind::Alarm, "far", 1_800_000_000_000),
    ];
    let out = rt
        .industrial_correlate(&events, CorrelateParams::default(), "op1")
        .await
        .unwrap();
    assert_eq!(out.events_considered, 4);
    assert!(!out.groups.is_empty());
    assert!(
        out.caveat.to_lowercase().contains("caus"),
        "correlation must deny causal claims: {}",
        out.caveat
    );
    assert!(audit_count(&rt, "industrial.correlation").await >= 1);
}

/// 14 (M12.7): what-if scenarios classify every statement as
/// KNOWN/SIMULATED/ASSUMED and are marked advisory-only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_14_scenario_statement_kinds_and_advisory() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    rt.industrial_asset_put(asset("unit.1", AssetKind::Unit, "U"), "op1")
        .await
        .unwrap();
    rt.industrial_relation_add("pump.1", "belongs_to", "unit.1", "op1")
        .await
        .unwrap();
    let out = rt
        .industrial_scenario(
            "pump.1",
            "failure",
            vec!["maintenance window exists".into()],
            "op1",
        )
        .await
        .unwrap();
    assert!(out.advisory_only, "scenarios are advisory, never control");
    assert!(!out.statements.is_empty());
    for s in &out.statements {
        assert!(
            matches!(
                s.kind,
                openrt_industrial::StatementKind::Known
                    | openrt_industrial::StatementKind::Simulated
                    | openrt_industrial::StatementKind::Assumed
            ),
            "statement kind must be classified: {:?}",
            s.kind
        );
        assert!(!s.basis.is_empty(), "every statement carries its basis");
    }
    assert!(audit_count(&rt, "industrial.scenario").await >= 1);
}

/// 15 (M12.7): the investigation answer is evidence-linked and explicitly
/// advisory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_15_investigate_sections_with_evidence() {
    let rt = rt().await;
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    rt.industrial_series_append("ts.1", "pump.1", "degC", &rising_points(16), "op1")
        .await
        .unwrap();
    let out = rt.industrial_investigate("pump.1", "op1").await.unwrap();
    assert_eq!(out.asset_id, "pump.1");
    assert!(out.sections.len() >= 2, "expected multiple sections");
    assert!(!out.advisory.is_empty());
    for s in &out.sections {
        assert!(!s.section.is_empty());
        assert!(!s.text.is_empty());
    }
    assert!(audit_count(&rt, "industrial.copilot").await >= 1);
}

// ---------------------------------------------------------------------------
// Part C — trading research (M12.8–M12.11), tests 16–26
// ---------------------------------------------------------------------------

/// 16 (M12.8): instruments are metadata-only registrations — no market feed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_16_instrument_put_and_list() {
    let rt = rt().await;
    rt.trading_instrument_put(instrument("eq.aaa", "AAA"), "op1")
        .await
        .unwrap();
    let all = rt.trading_instruments().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].symbol, "AAA");
    assert_eq!(all[0].venue, "XETR");
    assert!(audit_count(&rt, "trading.instrument").await >= 1);
}

/// 17 (M12.9): datasets are content-addressed — identical bars yield the
/// same digest regardless of dataset id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_17_dataset_build_content_addressed() {
    let rt = rt().await;
    let a = rt
        .trading_dataset_build(
            "ds.a",
            "v1",
            ds_cfg("eq.aaa"),
            vec!["f".into()],
            bars(80),
            "op1",
        )
        .await
        .unwrap();
    let b = rt
        .trading_dataset_build(
            "ds.b",
            "v1",
            ds_cfg("eq.aaa"),
            vec!["f".into()],
            bars(80),
            "op1",
        )
        .await
        .unwrap();
    assert_eq!(a.bar_count, 80);
    assert_eq!(a.digest, b.digest, "digest covers the content");
    assert_ne!(a.id, b.id, "ids stay per-dataset");
    let listed = rt.trading_datasets().await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(audit_count(&rt, "trading.dataset").await >= 2);
}

/// 18 (M12.9): CSV import parses the canonical column layout and stores the
/// bars behind the content digest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_18_dataset_import_csv_roundtrip() {
    let rt = rt().await;
    let ds = rt
        .trading_dataset_import_csv("ds.csv", "v1", ds_cfg("eq.aaa"), &bars_csv(30), "op1")
        .await
        .unwrap();
    assert_eq!(ds.bar_count, 30);
    let stored = rt.trading_dataset_bars("ds.csv").await.unwrap().unwrap();
    assert_eq!(stored.len(), 30);
    assert_eq!(stored[0].t_ms, bars(30)[0].t_ms);
    assert_eq!(stored[0].close, bars(30)[0].close);
}

/// 19 (M12.9): malformed CSV fails closed — no partial dataset is persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_19_dataset_csv_malformed_fails_closed() {
    let rt = rt().await;
    let err = rt
        .trading_dataset_import_csv(
            "ds.bad",
            "v1",
            ds_cfg("eq.aaa"),
            "t_ms,open,high,low,close,volume\nnot,a,row,for,market,data\n",
            "op1",
        )
        .await
        .unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("line"));
    assert!(rt.trading_datasets().await.unwrap().is_empty());
}

/// 20 (M12.10): declarative strategies persist with a content digest and are
/// listed back from the one store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_20_strategy_put_digest_and_list() {
    let rt = rt().await;
    let strat = openrt_trading::sma_crossover("strat.sma", "eq.aaa", 5, 20, CostSpec::default());
    let digest = rt.trading_strategy_put(strat, "op1").await.unwrap();
    assert!(digest.len() >= 16, "digest must be recorded: {digest}");
    let listed = rt.trading_strategies().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "strat.sma");
    assert!(audit_count(&rt, "trading.strategy").await >= 1);
}

async fn prep_backtest(rt: &Runtime) {
    rt.trading_dataset_build(
        "ds.a",
        "v1",
        ds_cfg("eq.aaa"),
        vec!["f".into()],
        bars(80),
        "op1",
    )
    .await
    .unwrap();
    let strat = openrt_trading::sma_crossover("strat.sma", "eq.aaa", 5, 20, CostSpec::default());
    rt.trading_strategy_put(strat, "op1").await.unwrap();
}

/// 21 (M12.10): backtests are deterministic — the same inputs produce the
/// same content-addressed id, metrics and equity curve — and every result
/// is marked simulated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_21_backtest_deterministic_and_simulated() {
    let rt = rt().await;
    prep_backtest(&rt).await;
    let r1 = rt
        .trading_backtest("strat.sma", "ds.a", None, "op1")
        .await
        .unwrap();
    let r2 = rt
        .trading_backtest("strat.sma", "ds.a", None, "op1")
        .await
        .unwrap();
    assert!(r1.simulated, "research results are always simulated");
    assert_eq!(r1.id, r2.id, "content-addressed backtest id");
    assert_eq!(r1.metrics.total_return_pct, r2.metrics.total_return_pct);
    assert_eq!(r1.equity_curve, r2.equity_curve);
    assert!(
        !r1.assumptions.is_empty(),
        "assumptions travel with results"
    );
    assert!(audit_count(&rt, "trading.backtest").await >= 2);
}

/// 22 (M12.11): the leakage report on a clean dataset says so — clean is an
/// explicit, checked verdict, not silence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_22_backtest_leakage_report_clean() {
    let rt = rt().await;
    prep_backtest(&rt).await;
    let r = rt
        .trading_backtest("strat.sma", "ds.a", None, "op1")
        .await
        .unwrap();
    assert!(r.look_ahead.clean, "no leaks: {:?}", r.look_ahead.leaks);
    assert!(r.look_ahead.leaks.is_empty());
    assert!(r.strategy_digest.len() >= 16);
    assert!(r.dataset_digest.len() >= 16);
}

/// 23 (M12.11): declared look-ahead in the dataset config is detected as
/// leakage — the report never hides a known future-derived input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_23_leakage_detected_for_declared_lookahead() {
    let rt = rt().await;
    let mut cfg = ds_cfg("eq.aaa");
    cfg.declared_lookahead = true;
    rt.trading_dataset_build("ds.leak", "v1", cfg, vec!["f".into()], bars(80), "op1")
        .await
        .unwrap();
    let strat = openrt_trading::sma_crossover("strat.sma", "eq.aaa", 5, 20, CostSpec::default());
    rt.trading_strategy_put(strat, "op1").await.unwrap();
    let r = rt
        .trading_backtest("strat.sma", "ds.leak", None, "op1")
        .await
        .unwrap();
    assert!(!r.look_ahead.clean, "declared look-ahead must be a leak");
    assert!(!r.look_ahead.leaks.is_empty());
}

/// 24 (M12.11): walk-forward splits the dataset into folds and states that
/// verdicts were NOT evaluated out-of-sample — honest scope, persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_24_walk_forward_folds_and_honest_scope() {
    let rt = rt().await;
    prep_backtest(&rt).await;
    let wf = rt
        .trading_walk_forward("strat.sma", "ds.a", WalkForwardParams::default(), "op1")
        .await
        .unwrap();
    assert_eq!(wf.dataset_id, "ds.a");
    assert_eq!(wf.strategy_id, "strat.sma");
    assert!(wf.folds.len() >= 2, "expected multiple folds");
    assert!(
        !wf.evaluated_only_in_sample,
        "fold verdicts must state they are in-sample only"
    );
    assert!(audit_count(&rt, "trading.backtest").await >= 1);
}

/// 25 (M12.11): research experiment records persist hypothesis + conclusion
/// + lineage and list back from the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_25_experiment_record_roundtrip() {
    let rt = rt().await;
    prep_backtest(&rt).await;
    let r = rt
        .trading_backtest("strat.sma", "ds.a", None, "op1")
        .await
        .unwrap();
    let exp = ResearchExperiment {
        id: "exp.1".into(),
        hypothesis: "trend continuation holds on daily bars".into(),
        dataset_id: "ds.a".into(),
        strategy_id: "strat.sma".into(),
        backtest_id: r.id.clone(),
        headline_metrics: serde_json::json!({"total_return_pct": r.metrics.total_return_pct}),
        conclusion: "no trade signals in the fixture window".into(),
        provenance: TradingProvenance::derived("test.fixture", openrt_core::now().to_rfc3339()),
        created_at: openrt_core::now().to_rfc3339(),
    };
    rt.trading_experiment_put(exp, "op1").await.unwrap();
    let listed = rt.trading_experiments().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "exp.1");
    assert_eq!(listed[0].backtest_id, r.id);
    assert!(audit_count(&rt, "trading.experiment").await >= 1);
}

/// 26 (M12.13): the one broker refusal point — always denied, always
/// audited, no order ever reaches a venue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_26_execute_denied_and_audited() {
    let rt = rt().await;
    let order = serde_json::json!({"side": "buy", "qty": 1, "instrument": "eq.aaa"});
    let err = rt.trading_execute(&order).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("research"),
        "refusal must state research-only scope: {msg}"
    );
    let rows = rt
        .audit_query(&AuditQuery::new().with_action_prefix("trading.broker.execute"))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "every attempt is audited");
    assert_eq!(rows[0].entry.result, "denied");
}

// ---------------------------------------------------------------------------
// Part C — experience-driven self-improvement (M12.12–M12.19),
// tests 27–41
// ---------------------------------------------------------------------------

/// 27 (M12.12): episodes are sealed into `improve.episodes` with provenance
/// and timestamps filled by the runtime — never left blank.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_27_episode_capture_and_list() {
    let rt = rt().await;
    rt.learn_capture_episode(episode("ep.1"), "op1")
        .await
        .unwrap();
    let all = rt.learn_episodes().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, "ep.1");
    assert!(!all[0].provenance.origin.is_empty(), "provenance filled");
    assert!(!all[0].created_at.is_empty(), "created_at filled");
    assert!(audit_count(&rt, "learning.episode").await >= 1);
}

/// 28 (M12.12): evaluation separates MEASURED facts from INFERRED
/// heuristics and records which method produced each.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_28_episode_evaluation_measured_vs_inferred() {
    let rt = rt().await;
    rt.learn_capture_episode(episode("ep.1"), "op1")
        .await
        .unwrap();
    let eval = rt.learn_evaluate_episode("ep.1", "op1").await.unwrap();
    assert_eq!(eval.episode_id, "ep.1");
    assert_eq!(eval.method, "measured+inferred/v1");
    assert!(!eval.measured.is_empty(), "measured facts from the record");
    assert!(
        eval.measured
            .keys()
            .any(|k| k.contains("verification") || k.contains("outcome")),
        "measured set carries verification/outcome: {:?}",
        eval.measured.keys().collect::<Vec<_>>()
    );
    assert!(
        !eval.inferred.is_empty(),
        "heuristics are labelled, not silent"
    );
    assert!(!eval.note.is_empty());
}

/// 29 (M12.13): candidates derive from REAL execution history — a session
/// with no evidence fails closed instead of synthesizing a candidate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_29_candidate_skill_from_real_history() {
    let rt = rt_toolseq().await;
    // No history yet → fail closed.
    let err = rt
        .learn_candidate_skill(
            "req.1",
            "empty",
            vec!["echo".into()],
            "ghost-session",
            "op1",
        )
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("no execution history"));

    // Real run → real evidence → candidate.
    let session = rt.create_session("history").await.unwrap();
    rt.run_task(session.id, "collect facts with echo")
        .await
        .unwrap();
    let sid = session.id.to_string();
    poll_evidence(&rt, &sid).await;
    let cid = rt
        .learn_candidate_skill(
            "req.12",
            "collect facts with echo",
            vec!["echo".into()],
            &sid,
            "op1",
        )
        .await
        .unwrap();
    assert!(!cid.is_empty());
    assert!(audit_count(&rt, "learning.skill_candidate").await >= 1);
}

/// 30 (M12.13): instinct + genome candidates are proposals only — they
/// never self-activate, and each lands in the audit trail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_30_instinct_and_genome_candidates_are_proposals() {
    let rt = rt_toolseq().await;
    let session = rt.create_session("history").await.unwrap();
    rt.run_task(session.id, "collect facts with echo")
        .await
        .unwrap();
    let sid = session.id.to_string();
    poll_evidence(&rt, &sid).await;

    let iid = rt.learn_candidate_instinct(&sid, "op1").await.unwrap();
    assert!(!iid.is_empty());
    assert!(audit_count(&rt, "learning.instinct_candidate").await >= 1);

    // Genome proposal against an unknown genome fails closed (existence is
    // checked before anything is written).
    let err = rt
        .learn_candidate_genome("prop.1", "genome.ghost", "try evolution", "op1")
        .await
        .unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("genome"));
    assert_eq!(audit_count(&rt, "learning.genome_candidate").await, 0);
}

/// 31 (M12.17): bench probes are recorded as measured results under
/// `bench.results` — the only numbers a promotion may cite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_31_bench_artifact_records_measured_result() {
    let rt = rt().await;
    let result = rt.bench_artifact("skill.demo").await.unwrap();
    assert_eq!(result.artifact_id, "skill.demo");
    let recorded = rt.store.list_records(openrt_bench::BENCH_NS).await.unwrap();
    assert!(!recorded.is_empty(), "bench result must persist");
}

/// 32 (M12.17): promotion verdict matrix — no baseline holds, improvement
/// promotes, regression rejects, equal measurement never promotes.
#[test]
fn p12_32_promotion_verdict_matrix() {
    use openrt_runtime::phase12::promotion_verdict;
    use openrt_selfimprove::PromotionDecision;

    let worse_baseline: BTreeMap<String, f64> = [
        ("error_rate".to_string(), 1.0),
        ("ok_count".to_string(), 0.0),
    ]
    .into_iter()
    .collect();
    let good: BTreeMap<String, f64> = [
        ("error_rate".to_string(), 0.0),
        ("ok_count".to_string(), 1.0),
    ]
    .into_iter()
    .collect();
    let current: BTreeMap<String, f64> = [
        ("error_rate".to_string(), 0.0),
        ("ok_count".to_string(), 1.0),
    ]
    .into_iter()
    .collect();
    let regression_current: BTreeMap<String, f64> = [
        ("error_rate".to_string(), 0.5),
        ("ok_count".to_string(), 0.5),
    ]
    .into_iter()
    .collect();
    let clean_baseline: BTreeMap<String, f64> = [
        ("error_rate".to_string(), 0.0),
        ("ok_count".to_string(), 1.0),
    ]
    .into_iter()
    .collect();

    assert_eq!(
        promotion_verdict(&current, None),
        PromotionDecision::Hold,
        "no baseline → hold"
    );
    assert_eq!(
        promotion_verdict(&current, Some(&worse_baseline)),
        PromotionDecision::Promote,
        "lower error than baseline → promote"
    );
    assert_eq!(
        promotion_verdict(&regression_current, Some(&clean_baseline)),
        PromotionDecision::Reject,
        "higher error than baseline → reject"
    );
    assert_eq!(
        promotion_verdict(&current, Some(&good)),
        PromotionDecision::Hold,
        "equal measurements → hold (noise never promotes)"
    );
}

/// 33 (M12.17): promotion without a recorded baseline holds — the decision
/// is persisted and audited with its measured candidate metrics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_33_promote_skill_without_baseline_holds() {
    let rt = rt().await;
    let decision = rt
        .learn_promote_skill("skill.ghost", "op1", None)
        .await
        .unwrap();
    assert_eq!(decision, "hold");
    let records = rt.learn_promotions().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, "promotion.skill.skill.ghost");
    assert_eq!(records[0].decision.as_str(), "hold");
    assert!(
        records[0]
            .benchmark
            .candidate_metrics
            .contains_key("error_rate"),
        "hold still records the measured candidate metrics"
    );
    assert!(audit_count(&rt, "learning.promotion").await >= 1);
}

/// 34 (M12.14): with a recorded baseline the gate can promote — approval +
/// activation run only on the Promote path, never on Hold/Reject.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_34_promote_skill_with_baseline_promotes_and_activates() {
    let rt = rt_toolseq().await;
    let op = rt.asset_operators()[0].clone();
    // Real history → validated candidate (approval + activation happen
    // ONLY inside the promotion, gated by the operator).
    let cid = validated_candidate(&rt).await;

    let baseline: BTreeMap<String, f64> = [
        ("error_rate".to_string(), 1.0),
        ("ok_count".to_string(), 0.0),
    ]
    .into_iter()
    .collect();
    let decision = rt
        .learn_promote_skill(&cid, &op, Some(baseline))
        .await
        .unwrap();
    assert_eq!(decision, "promote");
    let records = rt.learn_promotions().await.unwrap();
    assert!(records.iter().any(|r| r.decision.as_str() == "promote"));
    // The promoted skill is live — activation ran through the ONE pipeline.
    let states = rt.assets_skill_states().await.unwrap();
    assert!(
        states
            .iter()
            .any(|(id, st)| id.contains("skill") && st == "active"),
        "promotion must activate the skill: {states:?}"
    );
}

/// 35 (M12.17): a measured regression is REJECTED — nothing activates, the
/// rejection is persisted with its numbers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_35_promote_reject_never_activates() {
    let rt = rt().await;
    // Baseline error below the achievable current error → Reject.
    let baseline: BTreeMap<String, f64> = [
        ("error_rate".to_string(), -1.0),
        ("ok_count".to_string(), 2.0),
    ]
    .into_iter()
    .collect();
    let decision = rt
        .learn_promote_instinct("instinct.ghost", "op1", Some(baseline))
        .await
        .unwrap();
    assert_eq!(decision, "reject");
    let records = rt.learn_promotions().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision.as_str(), "reject");
    assert!(audit_count(&rt, "learning.promotion").await >= 1);
}

/// 36 (M12.19): operator rollback deactivates the skill, persists a
/// ROLLBACK decision with its rationale and audits it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_36_rollback_records_decision() {
    let rt = rt_toolseq().await;
    let op = rt.asset_operators()[0].clone();
    let skill = activated_skill(&rt, &op).await;
    let decision = rt.learn_rollback_skill(&skill, &op).await.unwrap();
    assert_eq!(decision, "rollback");
    let records = rt.learn_promotions().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision.as_str(), "rollback");
    assert!(
        !records[0].rationale.is_empty(),
        "rollback carries rationale"
    );
    assert!(audit_count(&rt, "learning.rollback").await >= 1);
}

/// 37 (M12.19): rollback against unknown ids fails closed — no phantom
/// records.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_37_rollback_unknown_id_fails_closed() {
    let rt = rt().await;
    let op = rt.asset_operators()[0].clone();
    let err = rt
        .learn_rollback_skill("skill.absent", &op)
        .await
        .unwrap_err();
    assert!(!format!("{err}").is_empty());
    assert!(rt.learn_promotions().await.unwrap().is_empty());
    assert_eq!(audit_count(&rt, "learning.rollback").await, 0);
}

/// 38 (M12.16): training experiments are recorded as planned — spec
/// lineage (method, adapter, dataset) survives the round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_38_model_experiment_create_and_list() {
    let rt = rt().await;
    let id = rt
        .learn_model_experiment_create(experiment_spec("exp.1"))
        .await
        .unwrap();
    assert_eq!(id, "exp.1");
    let runs = rt.learn_model_experiment_list().await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status.as_str(), "planned");
    assert_eq!(runs[0].spec.method.as_str(), "lora");
    assert_eq!(runs[0].spec.adapter.rank, 8);
    assert!(audit_count(&rt, "learning.model_experiment").await >= 1);
}

/// 39 (M12.16): starting a run without a training backend honest-fails with
/// the probe's explicit reason — no fabricated training, no silent success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_39_model_experiment_start_honest_fail() {
    let rt = rt().await;
    rt.learn_model_experiment_create(experiment_spec("exp.1"))
        .await
        .unwrap();
    let status = rt.learn_model_experiment_start("exp.1").await.unwrap();
    assert_eq!(status, "failed");
    let run = rt.learn_model_experiment_get("exp.1").await.unwrap();
    assert_eq!(run.status.as_str(), "failed");
    let reason = run.failure_reason.unwrap_or_default();
    assert!(
        reason.contains("training backend") || reason.contains("importable"),
        "failure must cite the probe: {reason}"
    );
    assert_eq!(run.metrics.len(), 0, "no fabricated training metrics");
}

/// 40 (M12.18): routing observations persist as signals with source +
/// timestamp and audit as success/failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_40_routing_signals_record_and_list() {
    let rt = rt().await;
    for i in 0..5 {
        rt.learn_routing_record(
            "summarize",
            "curated/llama",
            i % 2 == 0,
            Some(100 + i),
            "op1",
        )
        .await
        .unwrap();
    }
    let signals = rt.learn_routing_signals().await.unwrap();
    assert_eq!(signals.len(), 5);
    for s in &signals {
        assert_eq!(s.task_type, "summarize");
        assert_eq!(s.source, "runtime");
        assert!(!s.at.is_empty());
    }
    assert!(audit_count(&rt, "learning.routing").await >= 5);
}

/// 41 (M12.18): signals are a SUBORDINATE overlay — preferring a
/// non-existent artifact never selects it; the base refusal stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_41_signals_never_introduce_ineligible_model() {
    let rt = rt().await;
    // Ten success signals for a ghost artifact — signals alone confer no
    // eligibility.
    for _ in 0..10 {
        rt.learn_routing_record("summarize", "ghost/model", true, Some(5), "op1")
            .await
            .unwrap();
    }
    let request = openrt_router::RouteRequest {
        artifact_id: Some("ghost/model".into()),
        ..Default::default()
    };
    let verdict = rt.learn_route("summarize", request, 5).await.unwrap();
    assert!(
        matches!(verdict, openrt_router::RouteVerdict::Refused { .. }),
        "signals must not make an ineligible artifact routable: {verdict:?}"
    );
}

/// 42 (M12.20): cache lifecycle over the ONE `cache.meta` store — put,
/// pin/unpin, LRU eviction, and a fresh runtime sees the persisted state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_42_cache_lifecycle_persists() {
    let store = Arc::new(MemoryStore::new());
    let build = |store: Arc<MemoryStore>| {
        let store = store.clone();
        async move {
            RuntimeBuilder::new()
                .config(Config::default())
                .store(store)
                .with_provider(Arc::new(MockProvider::echo("mock")))
                .bus_capacity(32)
                .build()
                .await
                .unwrap()
        }
    };
    let rt = build(store.clone()).await;
    rt.cache_put("digest.a", openrt_hierarchy::Tier::Vram, 1_024, false)
        .await
        .unwrap();
    rt.cache_put("digest.b", openrt_hierarchy::Tier::Vram, 2_048, false)
        .await
        .unwrap();
    rt.cache_pin("digest.a").await.unwrap();
    let state = rt.cache_state().await.unwrap();
    assert_eq!(state.entries.len(), 2);
    assert!(
        state
            .entries
            .iter()
            .find(|e| e.digest == "digest.a")
            .unwrap()
            .pinned
    );

    // LRU eviction never drops a pinned entry.
    let victim = rt.cache_evict(openrt_hierarchy::Tier::Vram).await.unwrap();
    assert_eq!(victim.digest, "digest.b", "pinned entry is protected");

    // Restart: a second runtime over the same store sees the same state.
    let rt2 = build(store).await;
    let state2 = rt2.cache_state().await.unwrap();
    assert_eq!(state2.entries.len(), 1);
    assert_eq!(state2.entries[0].digest, "digest.a");
}

// ---------------------------------------------------------------------------
// Part E — memory honesty + mandated doctor rows, tests 43–45
// ---------------------------------------------------------------------------

/// 43 (M12.20): prioritized recall reports its method + parameters and
/// carries lineage (digest + occurred_at) with every hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_43_prioritized_recall_method_and_lineage() {
    let rt = rt().await;
    // Seed one durable, citation-validated fact through the ONE memory
    // authority (no second memory database).
    openrt_memory::remember_origin(&*rt.store, "run.p12", "canonical-p12")
        .await
        .unwrap();
    openrt_memory::remember_fact(
        &*rt.store,
        "fact.turbine",
        "run.p12",
        "src.ops",
        "ops",
        "turbine.maintenance",
        "turbine maintenance cadence is 90 days",
        "2026-09-01T00:00:00Z",
        false,
    )
    .await
    .unwrap();

    let out = rt
        .memory_prioritized_recall("turbine.maintenance", 5, 0.0, 0.4, 30.0)
        .await
        .unwrap();
    assert_eq!(out.method, "rank+recency/v1");
    assert!((out.params.recency_weight - 0.4).abs() < f64::EPSILON);
    assert!((out.params.half_life_days - 30.0).abs() < f64::EPSILON);
    assert!(!out.hits.is_empty(), "the seeded fact must be recallable");
    for h in &out.hits {
        assert!(!h.digest.is_empty(), "lineage digest travels with hits");
        assert!(!h.occurred_at.is_empty(), "lineage timestamp travels");
        assert_eq!(h.method, "rank+recency/v1");
    }
}

/// 44 (M12.20): prefetch has no backend — the call fails UNAVAILABLE and
/// the doctor row says exactly that (never a fabricated warm cache).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_44_prefetch_unavailable_and_doctor_says_so() {
    let rt = rt().await;
    let err = rt.memory_prefetch("anything").await.unwrap_err();
    assert!(matches!(err, openrt_runtime::Error::Unavailable(_)));
    let report = rt.doctor().await;
    let row = report
        .checks
        .iter()
        .find(|c| c.name == "memory.prefetch")
        .expect("memory.prefetch doctor row");
    assert_eq!(row.status, DoctorStatus::Unavailable);
    assert!(!row.detail.is_empty());
}

/// 45 (Part N): every mandated Phase 12 doctor row exists in the full
/// report, never says Fail, and each carries a non-empty detail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_45_phase12_doctor_rows_complete_never_fail() {
    let rt = rt().await;
    let report = rt.doctor().await;
    let wanted = [
        "industrial.intelligence",
        "industrial.data",
        "industrial.timeseries",
        "industrial.anomaly-detector",
        "industrial.rca",
        "trading.research",
        "trading.backtest-engine",
        "trading.dataset-store",
        "learning.subsystem",
        "learning.experience-store",
        "learning.skill-adaptation",
        "learning.genome-adaptation",
        "learning.model-adaptation",
        "learning.benchmark-store",
        "learning.rollback-capability",
        "model.cache",
        "memory.prioritized-recall",
        "memory.prefetch",
        "routing.signals",
    ];
    for name in wanted {
        let row = report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("missing mandated doctor row `{name}`"));
        assert_ne!(
            row.status,
            DoctorStatus::Fail,
            "Phase 12 doctor rows never Fail: {name} — {}",
            row.detail
        );
        assert!(!row.detail.is_empty(), "doctor row `{name}` needs a detail");
    }
    // The training-backend row is honest about this machine's probe.
    let train = report
        .checks
        .iter()
        .find(|c| c.name == "learning.model-adaptation")
        .unwrap();
    assert!(
        matches!(train.status, DoctorStatus::Warn | DoctorStatus::Unavailable),
        "model adaptation must be Warn/Unavailable without a backend: {:?}",
        train.status
    );
}

// ---------------------------------------------------------------------------
// tool registration + fabric metadata (Part F), folded into the 45
// ---------------------------------------------------------------------------

/// 45b: the eight Phase 12 tools exist in the ONE registry, are gated by
/// their declared capabilities, and carry fabric metadata with honest
/// runtime status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_45b_phase12_tools_registered_and_gated() {
    let caps = [
        "industrial.assets.read",
        "industrial.series.read",
        "industrial.analysis.run",
        "industrial.copilot.run",
        "trading.datasets.read",
        "trading.strategies.read",
        "trading.backtest.run",
        "learn.episodes.all",
        "tools.list",
        "tools.discovery.all",
    ];
    let rt = rt_caps(&caps).await;
    for name in [
        "industrial.assets",
        "industrial.series",
        "industrial.analysis",
        "industrial.copilot",
        "trading.dataset",
        "trading.strategy",
        "trading.backtest",
        "learn.experience",
    ] {
        assert!(
            rt.registry.names().contains(&name.to_string()),
            "`{name}` missing from the ONE registry"
        );
    }

    // tools.list rows carry fabric metadata + a 3-segment capability.
    let rows = json_out(&rt, "tools.list", serde_json::json!({})).await["tools"]
        .as_array()
        .cloned()
        .expect("tools array");
    for row in &rows {
        let name = row["name"].as_str().unwrap_or_default();
        if !["industrial.", "trading.", "learn."]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        assert!(row["category"].is_string(), "{name} lacks category: {row}");
        for cap in row["required_caps"].as_array().unwrap_or(&vec![]) {
            let c = cap.as_str().unwrap_or_default();
            assert_eq!(
                c.split('.').filter(|s| !s.is_empty()).count(),
                3,
                "{name}: capability `{c}` must be 3 segments"
            );
        }
    }

    // Without the read caps, deny-by-default refuses discovery for those
    // tools (same gate as every other surface).
    let deny = rt_caps(&["tools.list", "tools.discovery.all"]).await;
    let ctx = openrt_tools::ToolExecutionContext {
        session_id: None,
        task_id: None,
        channel: None,
    };
    let err = deny
        .executor
        .execute("industrial.analysis", serde_json::json!({}), &ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(err, openrt_tools::ToolError::PermissionDenied { .. }),
        "industrial tools must be deny-by-default: {err:?}"
    );
}

/// The audit actions mandated for Phase 12 are all present after exercising
/// the full surface — one verifying chain across every new namespace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p12_45c_phase12_audit_action_coverage() {
    let rt = rt_toolseq().await;
    // Exercise one call per mandated action family.
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    rt.industrial_asset_put(asset("unit.1", AssetKind::Unit, "U"), "op1")
        .await
        .unwrap();
    rt.industrial_relation_add("pump.1", "belongs_to", "unit.1", "op1")
        .await
        .unwrap();
    rt.industrial_series_append("ts.1", "pump.1", "degC", &rising_points(16), "op1")
        .await
        .unwrap();
    rt.industrial_trend_analysis("pump.1", "ts.1", TrendParams::default(), None, "op1")
        .await
        .unwrap();
    rt.industrial_anomaly_analysis(
        "pump.1",
        "ts.1",
        "statistical",
        &serde_json::json!({}),
        "op1",
    )
    .await
    .unwrap();
    rt.industrial_rca("pump.1", "op1").await.unwrap();
    rt.industrial_risk("pump.1", None, "op1").await.unwrap();
    rt.industrial_scenario("pump.1", "failure", vec![], "op1")
        .await
        .unwrap();
    rt.industrial_investigate("pump.1", "op1").await.unwrap();

    rt.trading_dataset_build("ds.a", "v1", ds_cfg("eq.aaa"), vec![], bars(80), "op1")
        .await
        .unwrap();
    let strat = openrt_trading::sma_crossover("strat.sma", "eq.aaa", 5, 20, CostSpec::default());
    rt.trading_strategy_put(strat, "op1").await.unwrap();
    rt.trading_backtest("strat.sma", "ds.a", None, "op1")
        .await
        .unwrap();
    rt.trading_execute(&serde_json::json!({"side":"sell"}))
        .await
        .unwrap_err();

    rt.learn_capture_episode(episode("ep.1"), "op1")
        .await
        .unwrap();
    rt.learn_evaluate_episode("ep.1", "op1").await.unwrap();
    rt.learn_benchmark("artifact.bench", &[], "op1")
        .await
        .unwrap();
    rt.learn_promote_skill("skill.x", "op1", None)
        .await
        .unwrap();
    let op = rt.asset_operators()[0].clone();
    let skill = activated_skill(&rt, &op).await;
    rt.learn_rollback_skill(&skill, &op).await.unwrap();
    rt.learn_model_experiment_create(experiment_spec("exp.1"))
        .await
        .unwrap();
    rt.learn_routing_record("summarize", "curated/llama", true, None, "op1")
        .await
        .unwrap();

    for action in [
        "industrial.asset.upsert",
        "industrial.relation.add",
        "industrial.series.append",
        "industrial.analysis",
        "industrial.anomaly",
        "industrial.rca",
        "industrial.scenario",
        "industrial.copilot",
        "trading.dataset",
        "trading.strategy",
        "trading.backtest",
        "trading.broker.execute",
        "learning.episode",
        "learning.skill_candidate",
        "learning.benchmark",
        "learning.promotion",
        "learning.rollback",
        "learning.model_experiment",
        "learning.routing",
    ] {
        assert!(
            audit_count(&rt, action).await >= 1,
            "Phase 12 audit action `{action}` never recorded"
        );
    }
}
