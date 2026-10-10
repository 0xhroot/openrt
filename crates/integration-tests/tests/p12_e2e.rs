//! Phase 12 mandatory E2E — TEST A–H: full-journey proofs that each
//! Definition-of-Done chain holds through the production runtime over ONE
//! durable store, with the honest-backend policy asserted end to end.
//!
//! * TEST A industrial advisory journey (asset → relation → telemetry →
//!   trend/anomaly → RCA/FMEA → risk → correlate → scenario → investigate,
//!   one audit chain, advisory-only tool surface)
//! * TEST B document intake into the SAME store as the industrial graph
//!   (corpus + `industrial.assets` + `knowledge.graph` coexist)
//! * TEST C trading research journey (instrument → CSV dataset → strategy →
//!   deterministic simulated backtest → leakage verdicts → walk-forward →
//!   experiment record, no broker tool)
//! * TEST D trading refusal bundle (runtime refusal + palette refusal +
//!   registry has no broker tool + every attempt audited denied)
//! * TEST E learning loop (episode → real-history candidate → promote
//!   holds without baseline, promotes with baseline → operator rollback)
//! * TEST F model experiment honest-fail: probe reason, no fabricated
//!   metrics, `TrainJobFailed` on the ONE bus, doctor row never Fail
//! * TEST G signal/recall/prefetch/cache honesty (signals subordinate,
//!   recall lineage, prefetch UNAVAILABLE, cache survives restart)
//! * TEST H full Phase 12 loop across runtime + TUI + palette + doctor +
//!   one verifying audit chain

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openrt_audit::AuditQuery;
use openrt_config::{Config, RuntimeConfig};
use openrt_industrial::{
    Asset, AssetKind, CorrelateParams, EventKind as IndustrialEventKind, Point,
    Provenance as IndustrialProvenance, Quality, TimedEvent, TrendParams,
};
use openrt_ingest::jail::IngestJail;
use openrt_models::mock::MockProvider;
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_router::{RouteRequest, RouteVerdict};
use openrt_runtime::{DoctorStatus, Runtime, RuntimeBuilder};
use openrt_selfimprove::{
    EpisodeOutcome, ExperienceEpisode, Provenance as ImproveProvenance, Verification,
};
use openrt_telemetry::ChannelSink;
use openrt_trading::{
    Bar, CostSpec, DatasetConfig, Instrument, InstrumentKind, Provenance as TradingProvenance,
    ResearchExperiment, WalkForwardParams,
};
use openrt_train::{AdapterSpec, ExperimentSpec, TrainMethod};
use openrt_tui::{App, OpsTab};
use openrt_webui::{CommandPalette, DispatchRequest, PaletteOutcome};

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
    rt_over(Arc::new(MemoryStore::new()), config).await
}

/// Build a runtime over an explicitly shared store — the ONE authority that
/// every surface in these journeys reads and writes.
async fn rt_over(store: Arc<MemoryStore>, config: Config) -> Arc<Runtime> {
    RuntimeBuilder::new()
        .config(config)
        .store(store)
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap()
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
        context: "e2e test".into(),
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
        tokio::time::sleep(Duration::from_millis(5)).await;
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

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p12e2e-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn runtime_config(ws: &Path) -> Config {
    Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    }
}

/// Fixture: a minimal flate-compressed, two-page PDF (page numbers derived
/// from the page objects and carried through storage).
fn two_page_pdf() -> Vec<u8> {
    use std::io::Write;
    fn flate(text: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(text).unwrap();
        enc.finish().unwrap()
    }
    let s1 = flate(b"BT (Page one text) Tj ET");
    let s2 = flate(b"BT (Page two text) Tj ET");
    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");
    pdf.extend_from_slice(b"1 0 obj << /Type /Page /Contents 2 0 R >> endobj\n");
    pdf.extend_from_slice(
        format!(
            "2 0 obj << /Filter /FlateDecode /Length {} >>\nstream\n",
            s1.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&s1);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    pdf.extend_from_slice(b"3 0 obj << /Type /Page /Contents 4 0 R >> endobj\n");
    pdf.extend_from_slice(
        format!(
            "4 0 obj << /Filter /FlateDecode /Length {} >>\nstream\n",
            s2.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&s2);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    pdf.extend_from_slice(b"5 0 obj << /Type /Catalog >> endobj\n");
    pdf.extend_from_slice(
        b"xref\n0 6\n0000000000 65535 f \ntrailer << /Root 5 0 R >>\nstartxref 1\n%%EOF\n",
    );
    pdf
}

// ---------------------------------------------------------------------------
// TEST A — industrial advisory journey + audit chain + advisory tool surface
// ---------------------------------------------------------------------------

/// TEST A: the full M12.1–M12.7 journey runs through the production
/// Runtime — idempotent canonical relations mirrored into `knowledge.graph`,
/// missing buckets reported honestly, a detected spike and an honest
/// UNAVAILABLE model-assisted path, evidence-labelled RCA, library FMEA,
/// heuristic-only risk, non-causal correlation, advisory scenarios, an
/// evidence-linked investigation — all under one verifying `industrial.*`
/// audit chain, with an industrial tool surface that is read/run only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_industrial_advisory_journey() {
    let rt = rt().await;

    // Assets: pump with declared equipment type, its parent unit, and a bare
    // asset with no telemetry (for the insufficient-risk verdict).
    let mut pump = asset("pump.1", AssetKind::Equipment, "Feed pump");
    pump.attrs
        .insert("equipment_type".into(), serde_json::json!("pump"));
    rt.industrial_asset_put(pump, "op1").await.unwrap();
    rt.industrial_asset_put(asset("unit.1", AssetKind::Unit, "Unit 1"), "op1")
        .await
        .unwrap();
    rt.industrial_asset_put(asset("tank.9", AssetKind::Equipment, "Bare tank"), "op1")
        .await
        .unwrap();

    // Canonical relation: idempotent, mirrored into the ONE knowledge graph.
    let r1 = rt
        .industrial_relation_add("pump.1", "belongs_to", "unit.1", "op1")
        .await
        .unwrap();
    let r2 = rt
        .industrial_relation_add("pump.1", "belongs_to", "unit.1", "op1")
        .await
        .unwrap();
    assert_eq!(r1.subject, r2.subject, "relation add is idempotent");
    assert_eq!(r1.object, r2.object);
    let mirrored = rt
        .store
        .list_records(openrt_evidence::GRAPH_NS)
        .await
        .unwrap();
    assert!(
        mirrored.iter().any(|(k, _)| k.contains("pump.1")),
        "relation must mirror into knowledge.graph: {mirrored:?}"
    );

    // Telemetry: rising + spiky series; the query window reports its gaps.
    let up_pts = rising_points(16);
    rt.industrial_series_append("ts.up", "pump.1", "degC", &up_pts, "op1")
        .await
        .unwrap();
    rt.industrial_series_append("ts.spike", "pump.1", "degC", &spiky_points(16), "op1")
        .await
        .unwrap();
    let from = up_pts[0].t_ms;
    let q = rt
        .industrial_series_query("ts.up", from, from + 30 * 60_000, 5 * 60_000)
        .await
        .unwrap();
    assert!(
        q.missing_buckets >= 1,
        "the wider window must report missing data"
    );
    assert_eq!(
        q.missing_buckets,
        q.buckets.iter().filter(|b| b.count == 0).count(),
        "missing count matches empty buckets exactly"
    );

    // Trend + anomaly: rising trend detected; statistical detector flags the
    // spike; model-assisted without a backend reports UNAVAILABLE.
    let trend = rt
        .industrial_trend_analysis("pump.1", "ts.up", TrendParams::default(), None, "op1")
        .await
        .unwrap();
    assert_eq!(trend.direction.as_str(), "rising");
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
    let assisted = rt
        .industrial_anomaly_analysis(
            "pump.1",
            "ts.up",
            "model_assisted",
            &serde_json::json!({}),
            "op1",
        )
        .await
        .unwrap();
    assert!(
        assisted.unavailable.is_some(),
        "model-assisted without a backend must be UNAVAILABLE"
    );
    assert!(assisted.anomalies.is_empty(), "no fabricated output");

    // RCA + FMEA: evidence labels, stated bounds, library failure modes.
    let rca = rt.industrial_rca("pump.1", "op1").await.unwrap();
    assert_eq!(rca.subject, "pump.1");
    assert!(!rca.findings.is_empty());
    assert!(!rca.caveat.is_empty());
    for f in &rca.findings {
        assert!(
            matches!(
                f.label.as_str(),
                "OBSERVED_FACT" | "INFERRED_RELATION" | "HYPOTHESIS" | "UNKNOWN"
            ),
            "unexpected evidence label {}",
            f.label.as_str()
        );
    }
    let modes = rt.industrial_fmea("pump.1").await.unwrap();
    assert!(!modes.is_empty());
    for m in &modes {
        assert_eq!(m.equipment_type, "pump");
        assert!(!m.effects.is_empty());
        assert!(!m.indicators.is_empty());
    }

    // Risk: INSUFFICIENT below the observation floor, heuristic band above
    // it, never a fabricated remaining life.
    let bare = rt.industrial_risk("tank.9", None, "op1").await.unwrap();
    assert!(bare.band.is_none());
    assert!(bare
        .insufficient
        .as_deref()
        .unwrap_or_default()
        .contains("INSUFFICIENT_DATA"));
    let with_data = rt
        .industrial_risk("pump.1", Some("ts.spike"), "op1")
        .await
        .unwrap();
    assert_eq!(with_data.method, "heuristic");
    assert!(with_data.band.is_some());
    assert!(with_data.remaining_life.is_none(), "no degradation model");

    // Correlation + scenario + investigation: caveats and advisory scope.
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
    let corr = rt
        .industrial_correlate(&events, CorrelateParams::default(), "op1")
        .await
        .unwrap();
    assert_eq!(corr.events_considered, 4);
    assert!(!corr.groups.is_empty());
    assert!(
        corr.caveat.to_lowercase().contains("caus"),
        "correlation must deny causal claims: {}",
        corr.caveat
    );

    let scenario = rt
        .industrial_scenario(
            "pump.1",
            "failure",
            vec!["maintenance window exists".into()],
            "op1",
        )
        .await
        .unwrap();
    assert!(
        scenario.advisory_only,
        "scenarios are advisory, never control"
    );
    assert!(!scenario.statements.is_empty());
    for s in &scenario.statements {
        assert!(!s.basis.is_empty(), "every statement carries its basis");
    }

    let inv = rt.industrial_investigate("pump.1", "op1").await.unwrap();
    assert_eq!(inv.asset_id, "pump.1");
    assert!(inv.sections.len() >= 2);
    assert!(!inv.advisory.is_empty());
    for s in &inv.sections {
        assert!(!s.section.is_empty());
        assert!(!s.text.is_empty());
    }

    // The five mandated industrial doctor rows: present, never Fail.
    let checks = rt.phase12_doctor_checks().await;
    let industrial: Vec<_> = checks
        .iter()
        .filter(|c| c.name.starts_with("industrial."))
        .collect();
    assert_eq!(industrial.len(), 5, "expected 5 industrial doctor rows");
    for c in industrial {
        assert_ne!(c.status, DoctorStatus::Fail, "{}", c.name);
        assert!(!c.detail.is_empty(), "{} needs a detail", c.name);
    }

    // ONE verifying audit chain across every industrial action.
    for action in [
        "industrial.asset.upsert",
        "industrial.relation.add",
        "industrial.series.append",
        "industrial.analysis",
        "industrial.anomaly",
        "industrial.rca",
        "industrial.correlation",
        "industrial.scenario",
        "industrial.copilot",
    ] {
        assert!(
            audit_count(&rt, action).await >= 1,
            "audit action `{action}` never recorded"
        );
    }
    let verified = rt.audit_verify().await.unwrap();
    assert!(verified >= 10, "audit chain too small: {verified}");

    // Advisory-only tool surface: exactly the four read/run industrial
    // tools, each gated by an industrial read/run capability — no
    // actuation tool exists.
    let mut ind_names: Vec<String> = rt
        .registry
        .names()
        .into_iter()
        .filter(|n| n.starts_with("industrial."))
        .collect();
    ind_names.sort();
    assert_eq!(
        ind_names,
        vec![
            "industrial.analysis".to_string(),
            "industrial.assets".to_string(),
            "industrial.copilot".to_string(),
            "industrial.series".to_string(),
        ],
        "only the four advisory industrial tools may exist"
    );
    for name in &ind_names {
        let tool = rt.registry.get(name).unwrap_or_else(|| panic!("{name}"));
        let caps = &tool.spec().required_caps;
        assert!(!caps.is_empty(), "{name} declares no capability");
        for cap in caps {
            let s = cap.as_str();
            assert!(s.starts_with("industrial."), "{name}: {s}");
            assert!(
                s.ends_with(".read") || s.ends_with(".run"),
                "{name}: capability `{s}` must be read/run only"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// TEST B — document intake into the same store as the industrial graph
// ---------------------------------------------------------------------------

/// TEST B: a two-page PDF flows through the jailed production corpus
/// (stored → duplicate on re-ingest, page numbers + extractor provenance
/// intact) while the industrial asset + mirrored relation live in the SAME
/// store — one authority for documents, assets and knowledge graph.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_b_pdf_intake_into_one_store() {
    let root = tmp_dir("intake");
    let store = Arc::new(MemoryStore::new());
    let rt = rt_over(store.clone(), runtime_config(&root)).await;

    std::fs::write(root.join("two.pdf"), two_page_pdf()).unwrap();
    let jail = IngestJail::new(root.clone()).unwrap();
    let out = rt
        .corpus
        .ingest_path(&jail, Path::new("two.pdf"), "p12-e2e")
        .await
        .unwrap();
    assert_eq!(out.state.label(), "stored");
    assert!(out.doc.doc_id.starts_with("doc/"));
    assert_eq!(out.doc.pages, 2);
    assert!(
        out.doc.extractor.starts_with("openrt-pdf"),
        "honest extractor provenance: {}",
        out.doc.extractor
    );

    // Re-ingest of identical bytes: a duplicate, not a second document.
    let dup = rt
        .corpus
        .ingest_path(&jail, Path::new("two.pdf"), "p12-e2e")
        .await
        .unwrap();
    assert_eq!(dup.state.label(), "duplicate");
    assert_eq!(dup.doc.doc_id, out.doc.doc_id);

    // Page numbers survive extraction + storage.
    let shown = rt.corpus.inspect(&out.doc.doc_id).await.unwrap();
    assert_eq!(shown.pages, 2);
    let pages: Vec<Option<u32>> = shown.blocks.iter().map(|b| b.page).collect();
    assert_eq!(pages, vec![Some(1), Some(2)]);

    // Industrial writes into the SAME store, relation mirrored to the graph.
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "P"), "op1")
        .await
        .unwrap();
    rt.industrial_asset_put(asset("unit.1", AssetKind::Unit, "U"), "op1")
        .await
        .unwrap();
    rt.industrial_relation_add("pump.1", "belongs_to", "unit.1", "op1")
        .await
        .unwrap();

    let asset_rows = store
        .list_records(openrt_industrial::ASSETS_NS)
        .await
        .unwrap();
    assert!(
        asset_rows.iter().any(|(k, _)| k.contains("pump.1")),
        "industrial assets live in the shared store: {asset_rows:?}"
    );
    let graph_rows = store.list_records(openrt_evidence::GRAPH_NS).await.unwrap();
    assert!(
        graph_rows.iter().any(|(k, _)| k.contains("pump.1")),
        "knowledge graph lives in the shared store: {graph_rows:?}"
    );
    let docs = rt.corpus.list().await.unwrap();
    assert_eq!(docs.len(), 1, "exactly one document in the shared store");
    assert_eq!(docs[0].doc_id, out.doc.doc_id);

    // Industrial doctor rows stay honest after the intake writes.
    let checks = rt.phase12_doctor_checks().await;
    for c in checks.iter().filter(|c| c.name.starts_with("industrial.")) {
        assert_ne!(c.status, DoctorStatus::Fail, "{}", c.name);
        assert!(!c.detail.is_empty(), "{}", c.name);
    }
}

// ---------------------------------------------------------------------------
// TEST C — trading research journey (M12.8–M12.11)
// ---------------------------------------------------------------------------

/// TEST C: instrument → CSV dataset import → strategy → deterministic
/// simulated backtests (identical content-addressed ids) → clean leakage
/// verdict → declared look-ahead detected as a leak → walk-forward folds
/// that state in-sample-only scope → experiment record — with the registry
/// carrying research tools only (no `trading.broker.*`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_c_trading_research_journey() {
    let rt = rt().await;

    rt.trading_instrument_put(instrument("eq.aaa", "AAA"), "op1")
        .await
        .unwrap();
    let instruments = rt.trading_instruments().await.unwrap();
    assert_eq!(instruments.len(), 1);
    assert_eq!(instruments[0].symbol, "AAA");

    let ds = rt
        .trading_dataset_import_csv("ds.journey", "v1", ds_cfg("eq.aaa"), &bars_csv(80), "op1")
        .await
        .unwrap();
    assert_eq!(ds.bar_count, 80);

    let strat = openrt_trading::sma_crossover("strat.sma", "eq.aaa", 5, 20, CostSpec::default());
    let digest = rt.trading_strategy_put(strat, "op1").await.unwrap();
    assert!(digest.len() >= 16, "digest must be recorded: {digest}");

    // Deterministic + simulated + clean.
    let r1 = rt
        .trading_backtest("strat.sma", "ds.journey", None, "op1")
        .await
        .unwrap();
    let r2 = rt
        .trading_backtest("strat.sma", "ds.journey", None, "op1")
        .await
        .unwrap();
    assert!(r1.simulated, "research results are always simulated");
    assert_eq!(r1.id, r2.id, "content-addressed backtest id");
    assert_eq!(r1.equity_curve, r2.equity_curve);
    assert!(r1.look_ahead.clean, "no leaks: {:?}", r1.look_ahead.leaks);

    // Declared look-ahead is detected, never hidden.
    let mut leak_cfg = ds_cfg("eq.aaa");
    leak_cfg.declared_lookahead = true;
    rt.trading_dataset_build("ds.leak", "v1", leak_cfg, vec!["f".into()], bars(80), "op1")
        .await
        .unwrap();
    let leaky = rt
        .trading_backtest("strat.sma", "ds.leak", None, "op1")
        .await
        .unwrap();
    assert!(
        !leaky.look_ahead.clean,
        "declared look-ahead must be a leak"
    );
    assert!(!leaky.look_ahead.leaks.is_empty());

    // Walk-forward: multiple folds, honest in-sample-only scope.
    let wf = rt
        .trading_walk_forward(
            "strat.sma",
            "ds.journey",
            WalkForwardParams::default(),
            "op1",
        )
        .await
        .unwrap();
    assert_eq!(wf.dataset_id, "ds.journey");
    assert!(wf.folds.len() >= 2, "expected multiple folds");
    assert!(
        !wf.evaluated_only_in_sample,
        "fold verdicts must state they are in-sample only"
    );

    // Experiment record with lineage back to the backtest.
    let exp = ResearchExperiment {
        id: "exp.journey".into(),
        hypothesis: "trend continuation holds on daily bars".into(),
        dataset_id: "ds.journey".into(),
        strategy_id: "strat.sma".into(),
        backtest_id: r1.id.clone(),
        headline_metrics: serde_json::json!({"total_return_pct": r1.metrics.total_return_pct}),
        conclusion: "simulated only — no trade signals executed".into(),
        provenance: TradingProvenance::derived("test.fixture", openrt_core::now().to_rfc3339()),
        created_at: openrt_core::now().to_rfc3339(),
    };
    rt.trading_experiment_put(exp, "op1").await.unwrap();
    let experiments = rt.trading_experiments().await.unwrap();
    assert_eq!(experiments.len(), 1);
    assert_eq!(experiments[0].backtest_id, r1.id);

    // Research tools exist; no broker tool can ever be registered.
    let names = rt.registry.names();
    for n in ["trading.dataset", "trading.strategy", "trading.backtest"] {
        assert!(names.contains(&n.to_string()), "`{n}` missing");
    }
    assert!(
        !names.iter().any(|n| n.starts_with("trading.broker")),
        "no broker tool may exist: {names:?}"
    );

    for action in [
        "trading.instrument",
        "trading.dataset",
        "trading.strategy",
        "trading.backtest",
        "trading.experiment",
    ] {
        assert!(
            audit_count(&rt, action).await >= 1,
            "audit action `{action}` never recorded"
        );
    }
    let verified = rt.audit_verify().await.unwrap();
    assert!(verified >= 5, "audit chain too small: {verified}");
}

// ---------------------------------------------------------------------------
// TEST D — trading refusal bundle (M12.13)
// ---------------------------------------------------------------------------

/// TEST D: the ONE refusal point — the runtime method always fails with the
/// research-only reason, the palette surfaces the same denial, no
/// `trading.broker.*` tool exists in the registry, and every attempt lands
/// in the audit trail as `denied`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_d_trading_refusal_bundle() {
    let rt = rt().await;
    let order = serde_json::json!({"side": "buy", "qty": 1, "instrument": "eq.aaa"});

    let err = rt.trading_execute(&order).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("research"),
        "refusal must state research-only scope: {msg}"
    );

    // Registry: research tools present, broker tool impossible.
    let names = rt.registry.names();
    for n in ["trading.dataset", "trading.strategy", "trading.backtest"] {
        assert!(names.contains(&n.to_string()), "`{n}` missing");
    }
    assert!(
        !names.iter().any(|n| n.starts_with("trading.broker")),
        "no trading.broker.* tool may exist: {names:?}"
    );

    // The palette surfaces the SAME refusal — no alternate execution path.
    let improve = rt.load_improve().await.unwrap();
    let handle = tokio::runtime::Handle::current();
    let rt_palette = rt.clone();
    let order_str = order.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut palette = CommandPalette::with_runtime(improve, rt_palette, handle);
        palette.dispatch(
            &DispatchRequest::new("trading.execute").with_arg("order", order_str.as_str()),
        )
    })
    .await
    .unwrap()
    .expect("trading.execute dispatch");
    let PaletteOutcome::Denied { reason, .. } = outcome else {
        panic!("palette must deny trading execution: {outcome:?}");
    };
    assert!(
        reason.to_lowercase().contains("research"),
        "palette denial must state research-only scope: {reason}"
    );

    // Both attempts are audited as denied — no order ever reaches a venue.
    let rows = rt
        .audit_query(&AuditQuery::new().with_action_prefix("trading.broker.execute"))
        .await
        .unwrap();
    assert!(rows.len() >= 2, "runtime + palette attempts: {rows:?}");
    for r in &rows {
        assert_eq!(r.entry.result, "denied");
    }
}

// ---------------------------------------------------------------------------
// TEST E — learning loop (M12.12–M12.17, M12.19)
// ---------------------------------------------------------------------------

/// TEST E: episode capture + evaluation, a candidate derived from REAL
/// execution history, promotion HOLDING without a baseline, PROMOTING with
/// a recorded baseline (approval + activation only on that path), then an
/// operator ROLLBACK — three persisted decisions with measured metrics and
/// rationale, one verifying `learning.*` audit chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_e_learning_promotion_loop() {
    let rt = rt_toolseq().await;
    let op = rt.asset_operators()[0].clone();

    rt.learn_capture_episode(episode("ep.1"), "op1")
        .await
        .unwrap();
    let eval = rt.learn_evaluate_episode("ep.1", "op1").await.unwrap();
    assert_eq!(eval.method, "measured+inferred/v1");

    let cid = validated_candidate(&rt).await;

    // No baseline → HOLD (never silently promotes).
    let hold = rt
        .learn_promote_skill("skill.ghost", "op1", None)
        .await
        .unwrap();
    assert_eq!(hold, "hold");

    // Recorded baseline → PROMOTE; approval + activation run inside the gate.
    let baseline: BTreeMap<String, f64> = [
        ("error_rate".to_string(), 1.0),
        ("ok_count".to_string(), 0.0),
    ]
    .into_iter()
    .collect();
    let promote = rt
        .learn_promote_skill(&cid, &op, Some(baseline))
        .await
        .unwrap();
    assert_eq!(promote, "promote");
    let states = rt.assets_skill_states().await.unwrap();
    let sid = states
        .iter()
        .find(|(_, st)| st == "active")
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| panic!("promotion must activate the skill: {states:?}"));

    // Operator rollback deactivates and records its rationale.
    let rollback = rt.learn_rollback_skill(&sid, &op).await.unwrap();
    assert_eq!(rollback, "rollback");
    let states = rt.assets_skill_states().await.unwrap();
    assert!(
        !states.iter().any(|(id, st)| id == &sid && st == "active"),
        "rollback must deactivate the skill: {states:?}"
    );

    // Three persisted decisions, each with its measured evidence.
    let records = rt.learn_promotions().await.unwrap();
    assert_eq!(records.len(), 3, "hold + promote + rollback: {records:?}");
    let hold_rec = records
        .iter()
        .find(|r| r.decision.as_str() == "hold")
        .expect("hold record");
    let promote_rec = records
        .iter()
        .find(|r| r.decision.as_str() == "promote")
        .expect("promote record");
    let rollback_rec = records
        .iter()
        .find(|r| r.decision.as_str() == "rollback")
        .expect("rollback record");
    assert!(
        hold_rec
            .benchmark
            .candidate_metrics
            .contains_key("error_rate"),
        "hold still records measured candidate metrics"
    );
    assert!(
        promote_rec
            .benchmark
            .candidate_metrics
            .contains_key("error_rate"),
        "promote cites its measured metrics"
    );
    assert!(
        !rollback_rec.rationale.is_empty(),
        "rollback carries rationale"
    );

    for action in [
        "learning.episode",
        "learning.skill_candidate",
        "learning.promotion",
        "learning.rollback",
    ] {
        assert!(
            audit_count(&rt, action).await >= 1,
            "audit action `{action}` never recorded"
        );
    }
    let verified = rt.audit_verify().await.unwrap();
    assert!(verified >= 5, "audit chain too small: {verified}");
}

// ---------------------------------------------------------------------------
// TEST F — model experiment honest-fail (M12.16)
// ---------------------------------------------------------------------------

/// TEST F: starting a training run with no training backend fails with the
/// probe's explicit reason, records NO fabricated metrics, emits
/// `TrainJobFailed` on the ONE bus, and keeps the doctor row honest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_f_model_experiment_honest_fail() {
    let sink = Arc::new(ChannelSink::new());
    let rt = RuntimeBuilder::new()
        .config(Config::default())
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(sink.clone())
        .bus_capacity(32)
        .build()
        .await
        .unwrap();

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

    // The honest failure reaches the ONE bus as TrainJobFailed.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if sink
            .snapshot()
            .iter()
            .any(|e| e.kind == openrt_core::EventKind::TrainJobFailed)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "TrainJobFailed never reached the event sink"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Doctor row: present, non-empty detail, never Fail.
    let report = rt.doctor().await;
    let row = report
        .checks
        .iter()
        .find(|c| c.name == "learning.model-adaptation")
        .expect("learning.model-adaptation doctor row");
    assert_ne!(row.status, DoctorStatus::Fail, "{}", row.detail);
    assert!(!row.detail.is_empty());
    assert!(audit_count(&rt, "learning.model_experiment").await >= 1);
}

// ---------------------------------------------------------------------------
// TEST G — signal / recall / prefetch / cache honesty (M12.18, M12.20)
// ---------------------------------------------------------------------------

/// TEST G: routing signals are a subordinate overlay (ten successes for a
/// ghost artifact still refuse), prioritized recall carries lineage, prefetch
/// fails UNAVAILABLE with the doctor row agreeing, and a cache entry is seen
/// by a fresh runtime over the same store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_g_signal_recall_prefetch_cache_honesty() {
    let store = Arc::new(MemoryStore::new());
    let rt = rt_over(store.clone(), Config::default()).await;

    // Signals alone confer no eligibility.
    for _ in 0..10 {
        rt.learn_routing_record("summarize", "ghost/model", true, Some(5), "op1")
            .await
            .unwrap();
    }
    let request = RouteRequest {
        artifact_id: Some("ghost/model".into()),
        ..Default::default()
    };
    let verdict = rt.learn_route("summarize", request, 5).await.unwrap();
    assert!(
        matches!(verdict, RouteVerdict::Refused { .. }),
        "signals must not make an ineligible artifact routable: {verdict:?}"
    );
    let signals = rt.learn_routing_signals().await.unwrap();
    assert_eq!(signals.len(), 10);
    for s in &signals {
        assert_eq!(s.source, "runtime");
        assert!(!s.at.is_empty());
    }

    // Prioritized recall over the ONE memory authority: method + lineage.
    openrt_memory::remember_origin(&*rt.store, "run.g", "canonical-g")
        .await
        .unwrap();
    openrt_memory::remember_fact(
        &*rt.store,
        "fact.turbine",
        "run.g",
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
    assert!(!out.hits.is_empty(), "the seeded fact must be recallable");
    for h in &out.hits {
        assert!(!h.digest.is_empty(), "lineage digest travels with hits");
        assert!(!h.occurred_at.is_empty(), "lineage timestamp travels");
        assert_eq!(h.method, "rank+recency/v1");
    }

    // Prefetch has no backend: UNAVAILABLE, and the doctor says so.
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

    // Cache entry persists; a fresh runtime over the SAME store sees it.
    rt.cache_put("digest.g", openrt_hierarchy::Tier::Vram, 4_096, false)
        .await
        .unwrap();
    let rt2 = rt_over(store, Config::default()).await;
    let state = rt2.cache_state().await.unwrap();
    assert_eq!(state.entries.len(), 1);
    assert_eq!(state.entries[0].digest, "digest.g");
}

// ---------------------------------------------------------------------------
// TEST H — full Phase 12 loop: runtime + TUI + palette + doctor + audit
// ---------------------------------------------------------------------------

/// TEST H: everything at once over ONE store — industrial, trading,
/// learning, routing and cache writes; the TUI Phase 12 intel rows and the
/// palette's `phase12.info` both reflect the same runtime counts; the full
/// doctor carries all 19 Phase 12 rows without a single Fail; and one
/// verifying audit chain spans the industrial, trading and learning
/// families.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_h_full_phase12_loop() {
    let store = Arc::new(MemoryStore::new());
    let rt = rt_over(store, Config::default()).await;

    // Industrial: one asset + telemetry.
    rt.industrial_asset_put(asset("pump.1", AssetKind::Equipment, "Feed pump"), "op1")
        .await
        .unwrap();
    rt.industrial_series_append("ts.1", "pump.1", "degC", &rising_points(16), "op1")
        .await
        .unwrap();

    // Trading: instrument + CSV dataset + strategy + simulated backtest.
    rt.trading_instrument_put(instrument("eq.aaa", "AAA"), "op1")
        .await
        .unwrap();
    let ds = rt
        .trading_dataset_import_csv("ds.h", "v1", ds_cfg("eq.aaa"), &bars_csv(60), "op1")
        .await
        .unwrap();
    assert_eq!(ds.bar_count, 60);
    let strat = openrt_trading::sma_crossover("strat.sma", "eq.aaa", 5, 20, CostSpec::default());
    rt.trading_strategy_put(strat, "op1").await.unwrap();
    let bt = rt
        .trading_backtest("strat.sma", "ds.h", None, "op1")
        .await
        .unwrap();
    assert!(bt.simulated && bt.look_ahead.clean);

    // Learning: one episode, one promotion decision, five signals, one
    // cache entry.
    rt.learn_capture_episode(episode("ep.1"), "op1")
        .await
        .unwrap();
    rt.learn_evaluate_episode("ep.1", "op1").await.unwrap();
    let decision = rt
        .learn_promote_skill("skill.ghost", "op1", None)
        .await
        .unwrap();
    assert_eq!(decision, "hold");
    for i in 0..5 {
        rt.learn_routing_record("summarize", "curated/llama", i % 2 == 0, Some(100), "op1")
            .await
            .unwrap();
    }
    rt.cache_put("digest.h", openrt_hierarchy::Tier::Vram, 2_048, false)
        .await
        .unwrap();

    // One verifying audit chain across all three families.
    for prefix in ["industrial.", "trading.", "learning."] {
        assert!(
            audit_count(&rt, prefix).await >= 1,
            "audit family `{prefix}` never recorded"
        );
    }
    let verified = rt.audit_verify().await.unwrap();
    assert!(verified >= 10, "audit chain too small: {verified}");

    // No broker tool in the registry.
    assert!(
        !rt.registry
            .names()
            .iter()
            .any(|n| n.starts_with("trading.broker")),
        "no trading.broker.* tool may exist"
    );

    // TUI: the Phase 12 intel rows are real runtime reads.
    let mut app = App::new(rt.clone()).await.unwrap();
    app.refresh_ops().await.unwrap();
    let intel = |rows: &[openrt_tui::app::IntelRow], label: &str| -> String {
        rows.iter()
            .find(|r| r.label == label)
            .map(|r| r.value.clone())
            .unwrap_or_else(|| panic!("missing intel row `{label}`"))
    };
    assert_eq!(intel(&app.ops_intel, "industrial assets"), "1");
    assert_eq!(intel(&app.ops_intel, "trading instruments"), "1");
    assert_eq!(intel(&app.ops_intel, "trading datasets"), "1");
    assert_eq!(intel(&app.ops_intel, "trading strategies"), "1");
    assert_eq!(
        intel(&app.ops_intel, "trading execute"),
        "DENIED (research only)"
    );
    assert_eq!(intel(&app.ops_intel, "experience episodes"), "1");
    assert_eq!(intel(&app.ops_intel, "promotion decisions"), "1");
    assert_eq!(intel(&app.ops_intel, "routing signals"), "5");
    assert_eq!(intel(&app.ops_intel, "model cache entries"), "1");
    assert!(intel(&app.ops_intel, "phase12 doctor").contains("pass"));

    // The Phase 12 tab is part of the Ops ring.
    app.set_ops_tab(OpsTab::Intel);
    assert_eq!(app.ops_tab, OpsTab::Intel);
    assert_eq!(app.ops_tab.index(), 7);
    assert_eq!(OpsTab::from_index(7), OpsTab::Intel);
    assert_eq!(app.ops_tab.title(), "Ops · Phase 12");

    // Palette: phase12.info reports the SAME counts, trading.execute the
    // SAME denial.
    let p12_row_count = rt.phase12_doctor_checks().await.len();
    assert!(p12_row_count >= 19, "mandated doctor rows: {p12_row_count}");
    let improve = rt.load_improve().await.unwrap();
    let handle = tokio::runtime::Handle::current();
    let rt_palette = rt.clone();
    let (info_detail, exec_outcome) = tokio::task::spawn_blocking(move || {
        let mut palette = CommandPalette::with_runtime(improve, rt_palette, handle);
        let info = palette
            .dispatch(&DispatchRequest::new("phase12.info"))
            .expect("phase12.info");
        let exec = palette
            .dispatch(
                &DispatchRequest::new("trading.execute")
                    .with_arg("order", r#"{"side":"buy","qty":1,"instrument":"eq.aaa"}"#),
            )
            .expect("trading.execute");
        (info, exec)
    })
    .await
    .unwrap();
    let PaletteOutcome::Completed { detail, .. } = info_detail else {
        panic!("phase12.info must complete: {info_detail:?}");
    };
    let v: serde_json::Value = serde_json::from_str(&detail).expect("phase12.info JSON");
    assert_eq!(v["industrial_assets"], 1);
    assert_eq!(v["trading_instruments"], 1);
    assert_eq!(v["trading_datasets"], 1);
    assert_eq!(v["trading_strategies"], 1);
    assert_eq!(v["trading_execute"], "DENIED (research only)");
    assert_eq!(v["experience_episodes"], 1);
    assert_eq!(v["promotion_decisions"], 1);
    assert_eq!(v["routing_signals"], 5);
    assert_eq!(v["model_cache_entries"], 1);
    let doctor = &v["doctor"];
    let palette_rows = doctor["pass"].as_u64().unwrap_or(0)
        + doctor["warn"].as_u64().unwrap_or(0)
        + doctor["other"].as_u64().unwrap_or(0);
    assert_eq!(
        palette_rows, p12_row_count as u64,
        "palette doctor counts must cover every Phase 12 row"
    );
    let PaletteOutcome::Denied { reason, .. } = exec_outcome else {
        panic!("palette must deny trading execution: {exec_outcome:?}");
    };
    assert!(
        reason.to_lowercase().contains("research"),
        "palette denial must state research-only scope: {reason}"
    );

    // Full doctor: all 19 mandated rows present, none Fail, details set.
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
    let report = rt.doctor().await;
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
}
