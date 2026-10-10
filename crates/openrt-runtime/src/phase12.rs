//! Phase 12 runtime surface (M12.1–M12.20): advisory industrial
//! intelligence, research-only trading analytics, experience-driven
//! self-improvement with benchmark-gated promotion, model-cache wiring,
//! prioritized recall, subordinate routing signals and honest doctor rows.
//!
//! Every operation flows through the ONE `PersistenceStore`, the ONE audit
//! chain and the ONE tool registry. Nothing here creates a second runtime,
//! router, workflow engine, tool registry, permission authority, audit
//! log or store; Phase 5–11 behaviour is never modified.
//!
//! Honesty rules baked into this module:
//! - industrial output is advisory (no physical-control path exists);
//! - trading is research-only — `trading_execute` always refuses and no
//!   `trading.broker.*` tool exists (ADR-008 `KillScope::Trading` unused);
//! - detection/promotion/eval methods are reported as run, UNAVAILABLE
//!   backends say so instead of returning heuristic output;
//! - self-improvement never changes permissions and never self-activates:
//!   every promotion/rollback is operator-gated and benchmark-measured;
//! - doctor rows are Pass/Warn/Unavailable only — never Fail.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{json, Value};

use openrt_fabric::{FabricMeta, RiskLevel, SupportState, ToolCategory};
use openrt_persistence::PersistenceStore;
use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};

use crate::doctor::{DoctorCheck, DoctorStatus};
use crate::runtime::Runtime;
use crate::Error;

// ---------------------------------------------------------------------------
// Shared helpers (used by both Runtime methods and the registered tools —
// tools have no Runtime context, so the domain logic lives here as free
// functions over the ONE store).
// ---------------------------------------------------------------------------

fn parse_ts_ms(ts: &str) -> Option<i64> {
    ts.parse::<openrt_core::Timestamp>()
        .ok()
        .map(|t| t.timestamp_millis())
}

fn ind_err(tool: &str, e: openrt_industrial::IndustrialError) -> ToolError {
    match e {
        openrt_industrial::IndustrialError::NotFound(msg) => {
            ToolError::InvalidArguments(tool.to_string(), msg)
        }
        other => ToolError::Internal(tool.to_string(), other.to_string()),
    }
}

fn trade_err(tool: &str, e: openrt_trading::TradingError) -> ToolError {
    match e {
        openrt_trading::TradingError::NotFound(msg) => {
            ToolError::InvalidArguments(tool.to_string(), msg)
        }
        other => ToolError::Internal(tool.to_string(), other.to_string()),
    }
}

fn str_arg<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            ToolError::InvalidArguments(tool.to_string(), format!("`{key}` is required"))
        })
}

fn f64_arg(args: &Value, key: &str, default: f64) -> f64 {
    args.get(key).and_then(Value::as_f64).unwrap_or(default)
}

fn usize_arg(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(default)
}

/// Subject + graph neighborhood of one asset (subject may be absent when
/// the id is unknown — callers decide whether that is fatal).
struct GraphContext {
    subject: Option<openrt_industrial::Asset>,
    related: Vec<openrt_industrial::Asset>,
    edges: Vec<(String, String, String)>,
}

async fn graph_context(
    store: &dyn PersistenceStore,
    subject: &str,
) -> Result<GraphContext, openrt_industrial::IndustrialError> {
    let subject_asset = openrt_industrial::get_asset(store, subject).await?;
    let rels = openrt_industrial::relations_for(store, subject).await?;
    let mut edges = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for r in rels {
        edges.push((r.subject.clone(), r.predicate.clone(), r.object.clone()));
        let other = if r.subject == subject {
            r.object.clone()
        } else {
            r.subject.clone()
        };
        if !ids.contains(&other) {
            ids.push(other);
        }
    }
    let mut related = Vec::new();
    for id in ids {
        if let Some(a) = openrt_industrial::get_asset(store, &id).await? {
            related.push(a);
        }
    }
    Ok(GraphContext {
        subject: subject_asset,
        related,
        edges,
    })
}

fn id_set(all: &[openrt_industrial::Asset]) -> BTreeSet<String> {
    all.iter().map(|a| a.id.clone()).collect()
}

fn kind_of(
    all: &[openrt_industrial::Asset],
    kind: openrt_industrial::AssetKind,
) -> Vec<openrt_industrial::Asset> {
    all.iter().filter(|a| a.kind == kind).cloned().collect()
}

/// Persisted trend analyses whose asset/series touch the given id set.
async fn stored_trends(
    store: &dyn PersistenceStore,
    ids: &BTreeSet<String>,
) -> Result<Vec<openrt_industrial::TrendResult>, openrt_industrial::IndustrialError> {
    let mut out = Vec::new();
    for rec in openrt_industrial::list_analysis(store, Some("trend")).await? {
        if let Some(p) = rec.get("payload") {
            if let Ok(t) = serde_json::from_value::<openrt_industrial::TrendResult>(p.clone()) {
                if ids.contains(&t.asset_id) || ids.contains(&t.series_id) {
                    out.push(t);
                }
            }
        }
    }
    Ok(out)
}

/// Persisted detector runs whose asset/series touch the given id set.
async fn stored_detector_runs(
    store: &dyn PersistenceStore,
    ids: &BTreeSet<String>,
) -> Result<Vec<openrt_industrial::DetectorRun>, openrt_industrial::IndustrialError> {
    let mut out = Vec::new();
    for rec in openrt_industrial::list_analysis(store, Some("anomaly")).await? {
        if let Some(p) = rec.get("payload") {
            if let Ok(run) = serde_json::from_value::<openrt_industrial::DetectorRun>(p.clone()) {
                if ids.contains(&run.asset_id) || ids.contains(&run.series_id) {
                    out.push(run);
                }
            }
        }
    }
    Ok(out)
}

/// Time-stamped events derivable from stored assets (alarms/incidents/
/// work orders) — timestamps come from asset attrs only, never invented.
fn timed_events(all: &[openrt_industrial::Asset]) -> Vec<openrt_industrial::TimedEvent> {
    use openrt_industrial::{AssetKind, EventKind};
    let mut out = Vec::new();
    for a in all {
        let (kind, attr) = match a.kind {
            AssetKind::Alarm => (EventKind::Alarm, "first_seen"),
            AssetKind::Incident => (EventKind::Incident, "occurred_at"),
            AssetKind::WorkOrder => (EventKind::WorkOrder, "scheduled_at"),
            _ => continue,
        };
        let Some(at_ms) = a
            .attrs
            .get(attr)
            .and_then(Value::as_str)
            .and_then(parse_ts_ms)
        else {
            continue;
        };
        out.push(openrt_industrial::TimedEvent {
            kind,
            id: a.id.clone(),
            at_ms,
            detail: String::new(),
        });
    }
    out
}

/// Build the bounded RCA context from the graph + persisted analyses.
async fn rca_context(
    store: &dyn PersistenceStore,
    subject: &str,
) -> Result<openrt_industrial::RcaContext, openrt_industrial::IndustrialError> {
    let g = graph_context(store, subject).await?;
    let mut all = g.related.clone();
    if let Some(s) = &g.subject {
        all.push(s.clone());
    }
    let ids = id_set(&all);
    let trends = stored_trends(store, &ids).await?;
    let mut anomalies = Vec::new();
    for run in stored_detector_runs(store, &ids).await? {
        anomalies.extend(run.anomalies);
    }
    let fmea = all
        .iter()
        .find(|a| a.kind == openrt_industrial::AssetKind::Equipment)
        .and_then(|a| a.attrs.get("equipment_type"))
        .and_then(Value::as_str)
        .map(openrt_industrial::fmea_table)
        .unwrap_or_default();
    Ok(openrt_industrial::RcaContext {
        alarms: kind_of(&all, openrt_industrial::AssetKind::Alarm),
        incidents: kind_of(&all, openrt_industrial::AssetKind::Incident),
        work_orders: kind_of(&all, openrt_industrial::AssetKind::WorkOrder),
        trends,
        anomalies,
        edges: g.edges,
        fmea,
        bounds: openrt_industrial::RcaBounds::default(),
    })
}

/// Build the evidence bundle behind an investigation answer.
async fn related_bundle(
    store: &dyn PersistenceStore,
    subject: &str,
) -> Result<openrt_industrial::RelatedBundle, openrt_industrial::IndustrialError> {
    let g = graph_context(store, subject).await?;
    let mut all = g.related.clone();
    if let Some(s) = &g.subject {
        all.push(s.clone());
    }
    let ids = id_set(&all);
    let trends = stored_trends(store, &ids).await?;
    let detector_runs = stored_detector_runs(store, &ids).await?;
    let groups = openrt_industrial::correlate(
        &timed_events(&all),
        openrt_industrial::CorrelateParams::default(),
    )
    .groups;
    Ok(openrt_industrial::RelatedBundle {
        alarms: kind_of(&all, openrt_industrial::AssetKind::Alarm),
        incidents: kind_of(&all, openrt_industrial::AssetKind::Incident),
        work_orders: kind_of(&all, openrt_industrial::AssetKind::WorkOrder),
        inspections: kind_of(&all, openrt_industrial::AssetKind::Inspection),
        procedures: kind_of(&all, openrt_industrial::AssetKind::Procedure),
        trends,
        detector_runs,
        neighbors: g.edges,
        correlation_groups: groups,
    })
}

async fn require_asset(
    store: &dyn PersistenceStore,
    asset_id: &str,
) -> Result<openrt_industrial::Asset, openrt_industrial::IndustrialError> {
    openrt_industrial::get_asset(store, asset_id)
        .await?
        .ok_or_else(|| openrt_industrial::IndustrialError::NotFound(format!("asset `{asset_id}`")))
}

async fn require_series(
    store: &dyn PersistenceStore,
    series_id: &str,
) -> Result<openrt_industrial::Series, openrt_industrial::IndustrialError> {
    openrt_industrial::get_series(store, series_id)
        .await?
        .ok_or_else(|| {
            openrt_industrial::IndustrialError::NotFound(format!("series `{series_id}`"))
        })
}

/// Trend analysis over a stored series; result persisted under
/// `industrial.analysis` (kind `trend`) for later context assembly.
async fn trend_analysis(
    store: &dyn PersistenceStore,
    asset_id: &str,
    series_id: &str,
    params: openrt_industrial::TrendParams,
    threshold: Option<openrt_industrial::Threshold>,
) -> Result<openrt_industrial::TrendResult, openrt_industrial::IndustrialError> {
    let series = require_series(store, series_id).await?;
    let result = openrt_industrial::trend(asset_id, series_id, &series.points, params, threshold);
    openrt_industrial::put_analysis(store, "trend", series_id, &result).await?;
    Ok(result)
}

/// One detector run over a stored series; always reports the method that
/// actually executed (or an explicit UNAVAILABLE reason).
async fn anomaly_analysis(
    store: &dyn PersistenceStore,
    asset_id: &str,
    series_id: &str,
    method: &str,
    params: &Value,
) -> Result<openrt_industrial::DetectorRun, openrt_industrial::IndustrialError> {
    let series = require_series(store, series_id).await?;
    let z = f64_arg(params, "z_threshold", 3.0);
    let run = match method {
        "statistical" => {
            let mut p = openrt_industrial::StatisticalParams::default();
            if let Some(v) = params.get("window").and_then(Value::as_u64) {
                p.window = v as usize;
            }
            if let Some(v) = params.get("min_samples").and_then(Value::as_u64) {
                p.min_samples = v as usize;
            }
            if params.get("z_threshold").and_then(Value::as_f64).is_some() {
                p.z_threshold = z;
            }
            if let Some(v) = params.get("max_results").and_then(Value::as_u64) {
                p.max_results = v as usize;
            }
            openrt_industrial::statistical(asset_id, series_id, &series.points, p)
        }
        "rule" => {
            let p = openrt_industrial::RuleParams {
                low: f64_arg(params, "low", 0.0),
                high: f64_arg(params, "high", f64::MAX),
                max_results: usize_arg(params, "max_results", 50),
            };
            openrt_industrial::rule(asset_id, series_id, &series.points, p)
        }
        "time_series" => {
            let mut p = openrt_industrial::TimeSeriesParams::default();
            if let Some(v) = params.get("window").and_then(Value::as_u64) {
                p.window = v as usize;
            }
            if let Some(v) = params.get("min_samples").and_then(Value::as_u64) {
                p.min_samples = v as usize;
            }
            if params
                .get("deviation_sigma")
                .and_then(Value::as_f64)
                .is_some()
            {
                p.deviation_sigma = f64_arg(params, "deviation_sigma", 3.0);
            }
            if let Some(v) = params.get("max_results").and_then(Value::as_u64) {
                p.max_results = v as usize;
            }
            openrt_industrial::time_series(asset_id, series_id, &series.points, p)
        }
        "model_assisted" => {
            openrt_industrial::model_assisted(asset_id, series_id, &series.points, None, z)
        }
        other => {
            return Err(openrt_industrial::IndustrialError::Invalid(format!(
                "unknown detector method `{other}` (statistical|rule|time_series|model_assisted)"
            )));
        }
    };
    openrt_industrial::put_analysis(store, "anomaly", series_id, &run).await?;
    Ok(run)
}

/// Heuristic risk from stored observations — always labelled `heuristic`,
/// INSUFFICIENT_DATA below `RISK_MIN_OBSERVATIONS`.
async fn risk_analysis(
    store: &dyn PersistenceStore,
    asset_id: &str,
    series_id: Option<&str>,
) -> Result<openrt_industrial::RiskResult, openrt_industrial::IndustrialError> {
    let asset = require_asset(store, asset_id).await?;
    let sid = match series_id {
        Some(s) => s.to_string(),
        None => {
            let derived = openrt_industrial::fmea::series_id_of(&asset);
            if openrt_industrial::get_series(store, &derived)
                .await?
                .is_some()
            {
                derived
            } else {
                let candidates: Vec<String> = openrt_industrial::list_series(store)
                    .await?
                    .into_iter()
                    .filter(|s| s.asset_id == asset_id)
                    .map(|s| s.id)
                    .collect();
                match candidates.len() {
                    1 => candidates.into_iter().next().expect("one candidate"),
                    0 => derived,
                    _ => {
                        return Err(openrt_industrial::IndustrialError::Invalid(format!(
                            "asset `{asset_id}` has {} candidate series — pass an explicit                              series_id",
                            candidates.len()
                        )));
                    }
                }
            }
        }
    };
    let observations = match openrt_industrial::get_series(store, &sid).await? {
        Some(s) => s.points.len(),
        None => 0,
    };
    let mut anomalies = Vec::new();
    for run in stored_detector_runs(store, &BTreeSet::from([sid.clone()])).await? {
        anomalies.extend(run.anomalies);
    }
    let g = graph_context(store, asset_id).await?;
    let alarms = g
        .related
        .iter()
        .filter(|a| a.kind == openrt_industrial::AssetKind::Alarm)
        .count();
    let signals = openrt_industrial::RiskSignals {
        anomalies_recent: anomalies.len(),
        anomalies_total: anomalies.len(),
        alarms,
        maintenance_age_days: None,
        observations,
    };
    let result = openrt_industrial::risk(&asset, &sid, &anomalies, signals, observations);
    openrt_industrial::put_analysis(store, "risk", asset_id, &result).await?;
    Ok(result)
}

/// What-if scenario over the asset's observed graph state.
async fn scenario_analysis(
    store: &dyn PersistenceStore,
    asset_id: &str,
    event: &str,
    assumptions: Vec<String>,
) -> Result<openrt_industrial::ScenarioResult, openrt_industrial::IndustrialError> {
    let g = graph_context(store, asset_id).await?;
    let asset = g.subject.ok_or_else(|| {
        openrt_industrial::IndustrialError::NotFound(format!("asset `{asset_id}`"))
    })?;
    let input = openrt_industrial::ScenarioInput {
        asset,
        event: event.to_string(),
        assumptions,
        edges: g.edges,
    };
    let out = openrt_industrial::scenario(&input);
    openrt_industrial::put_analysis(store, "scenario", asset_id, &out).await?;
    Ok(out)
}

/// Evidence-linked investigation answer for one asset.
async fn investigate_asset(
    store: &dyn PersistenceStore,
    asset_id: &str,
) -> Result<openrt_industrial::Investigation, openrt_industrial::IndustrialError> {
    let asset = require_asset(store, asset_id).await?;
    let bundle = related_bundle(store, asset_id).await?;
    Ok(openrt_industrial::investigate(&asset, &bundle))
}

// ---------------------------------------------------------------------------
// Trading free functions (research-only).
// ---------------------------------------------------------------------------

async fn backtest_run(
    store: &dyn PersistenceStore,
    strategy_id: &str,
    dataset_id: &str,
    initial_cash: Option<f64>,
) -> Result<openrt_trading::BacktestResult, openrt_trading::TradingError> {
    let strategy = openrt_trading::get_strategy(store, strategy_id)
        .await?
        .ok_or_else(|| {
            openrt_trading::TradingError::NotFound(format!("strategy `{strategy_id}`"))
        })?;
    let dataset = openrt_trading::get_dataset(store, dataset_id)
        .await?
        .ok_or_else(|| openrt_trading::TradingError::NotFound(format!("dataset `{dataset_id}`")))?;
    let bars = openrt_trading::get_bars(store, dataset_id)
        .await?
        .ok_or_else(|| {
            openrt_trading::TradingError::NotFound(format!("bars for dataset `{dataset_id}`"))
        })?;
    let cfg = openrt_trading::BacktestConfig {
        initial_cash: initial_cash
            .unwrap_or_else(|| openrt_trading::BacktestConfig::default().initial_cash),
        execution_next_open: true,
    };
    let result = openrt_trading::run(&strategy, &dataset, &bars, cfg)?;
    openrt_trading::put_backtest(store, &result.id, &result).await?;
    Ok(result)
}

async fn walk_forward_run(
    store: &dyn PersistenceStore,
    strategy_id: &str,
    dataset_id: &str,
    params: openrt_trading::WalkForwardParams,
) -> Result<openrt_trading::WalkForwardResult, openrt_trading::TradingError> {
    let strategy = openrt_trading::get_strategy(store, strategy_id)
        .await?
        .ok_or_else(|| {
            openrt_trading::TradingError::NotFound(format!("strategy `{strategy_id}`"))
        })?;
    let dataset = openrt_trading::get_dataset(store, dataset_id)
        .await?
        .ok_or_else(|| openrt_trading::TradingError::NotFound(format!("dataset `{dataset_id}`")))?;
    let bars = openrt_trading::get_bars(store, dataset_id)
        .await?
        .ok_or_else(|| {
            openrt_trading::TradingError::NotFound(format!("bars for dataset `{dataset_id}`"))
        })?;
    let result = openrt_trading::walk_forward(&strategy, &dataset, &bars, params)?;
    let json = serde_json::to_string(&result)
        .map_err(|e| openrt_trading::TradingError::Serde(format!("serialize walk-forward: {e}")))?;
    let wf_id = openrt_core::sha256_hex(json.as_bytes());
    openrt_trading::put_backtest(store, &wf_id, &result).await?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Self-improvement helpers: measured bench gate + promotion bookkeeping.
// ---------------------------------------------------------------------------

/// MEASURED metrics of one bench run — the only numbers a promotion may
/// cite as facts (inferred heuristics never reach `candidate_metrics`).
pub fn bench_metrics(result: &openrt_bench::BenchResult) -> BTreeMap<String, f64> {
    let a = &result.aggregate;
    let mut m = BTreeMap::new();
    m.insert("error_rate".to_string(), a.error_rate as f64);
    m.insert("ok_count".to_string(), a.ok_count as f64);
    m.insert("sample_count".to_string(), a.count as f64);
    m.insert(
        "total_output_tokens".to_string(),
        a.total_output_tokens as f64,
    );
    if let Some(ms) = a.median_latency_ms {
        m.insert("median_latency_ms".to_string(), ms as f64);
    }
    m
}

/// Deterministic verdict from measured error rates. No baseline → Hold
/// (never promote on a single benchmark); better → Promote; worse →
/// Reject; equal → Hold (noise never promotes).
pub fn promotion_verdict(
    current: &BTreeMap<String, f64>,
    baseline: Option<&BTreeMap<String, f64>>,
) -> openrt_selfimprove::PromotionDecision {
    use openrt_selfimprove::PromotionDecision as D;
    let Some(baseline) = baseline else {
        return D::Hold;
    };
    let ce = current.get("error_rate").copied().unwrap_or(1.0);
    let be = baseline.get("error_rate").copied().unwrap_or(1.0);
    if ce < be {
        D::Promote
    } else if ce > be {
        D::Reject
    } else {
        D::Hold
    }
}

fn promotion_record(
    id: &str,
    kind: openrt_selfimprove::PromotionKind,
    artifact: &str,
    decision: openrt_selfimprove::PromotionDecision,
    metrics: BTreeMap<String, f64>,
    baseline: Option<&BTreeMap<String, f64>>,
    rationale: &str,
    operator: Option<&str>,
) -> openrt_selfimprove::PromotionRecord {
    let mut deltas = BTreeMap::new();
    if let Some(b) = baseline {
        for (k, v) in &metrics {
            if let Some(bv) = b.get(k) {
                deltas.insert(k.clone(), v - bv);
            }
        }
    }
    openrt_selfimprove::PromotionRecord {
        id: id.to_string(),
        kind,
        artifact: artifact.to_string(),
        decision,
        benchmark: openrt_selfimprove::BenchmarkEvidence {
            baseline_ref: None,
            candidate_metrics: metrics,
            deltas,
            source: "bench.results".to_string(),
        },
        rationale: rationale.to_string(),
        operator: operator.map(str::to_string),
        provenance: openrt_selfimprove::Provenance::new(
            "runtime.phase12.promotion",
            "openrt-runtime",
        ),
        created_at: openrt_core::now().to_rfc3339(),
    }
}

fn grant_for(operator: &str) -> Result<openrt_workspace_assets::OperatorGrant, Error> {
    openrt_workspace_assets::OperatorGrant::new(operator).map_err(|_| {
        Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
            operator: operator.to_string(),
        })
    })
}

fn default_cache_caps() -> BTreeMap<openrt_hierarchy::Tier, u64> {
    BTreeMap::from([
        (openrt_hierarchy::Tier::Vram, 2 << 30),
        (openrt_hierarchy::Tier::Ram, 8 << 30),
        (openrt_hierarchy::Tier::Nvme, 32 << 30),
    ])
}

// ---------------------------------------------------------------------------
// Runtime: industrial intelligence (M12.1–M12.7)
// ---------------------------------------------------------------------------

impl Runtime {
    /// Upsert one industrial asset and mirror its canonical relations into
    /// `knowledge.graph` (via `upsert_asset`), then audit the mutation.
    pub async fn industrial_asset_put(
        &self,
        asset: openrt_industrial::Asset,
        actor: &str,
    ) -> Result<openrt_industrial::Asset, Error> {
        let id = asset.id.clone();
        let saved = openrt_industrial::upsert_asset(&*self.store, asset).await?;
        self.audit(
            actor,
            "industrial.asset.upsert",
            &id,
            "upserted",
            json!({"kind": saved.kind.as_str()}),
        )
        .await?;
        Ok(saved)
    }

    /// Add one canonical relation between two assets (predicate-restricted,
    /// idempotent), mirrored into `knowledge.graph`.
    pub async fn industrial_relation_add(
        &self,
        subject: &str,
        predicate: &str,
        object: &str,
        actor: &str,
    ) -> Result<openrt_industrial::Relation, Error> {
        let provenance = openrt_industrial::Provenance::derived(
            format!("operator:{actor}"),
            openrt_core::now().to_rfc3339(),
        );
        let rel =
            openrt_industrial::add_relation(&*self.store, subject, predicate, object, provenance)
                .await?;
        self.audit(
            actor,
            "industrial.relation.add",
            subject,
            "related",
            json!({"predicate": predicate, "object": object}),
        )
        .await?;
        Ok(rel)
    }

    /// List assets matching the given filter (read-only).
    pub async fn industrial_assets(
        &self,
        filter: &openrt_industrial::AssetFilter,
    ) -> Result<Vec<openrt_industrial::Asset>, Error> {
        Ok(openrt_industrial::list_assets(&*self.store, filter).await?)
    }

    /// Fetch one asset by id (read-only).
    pub async fn industrial_asset(
        &self,
        id: &str,
    ) -> Result<Option<openrt_industrial::Asset>, Error> {
        Ok(openrt_industrial::get_asset(&*self.store, id).await?)
    }

    /// Append telemetry points to a series (create-on-first-write).
    pub async fn industrial_series_append(
        &self,
        series_id: &str,
        asset_id: &str,
        unit: &str,
        points: &[openrt_industrial::Point],
        actor: &str,
    ) -> Result<openrt_industrial::Series, Error> {
        let series =
            openrt_industrial::append_points(&*self.store, series_id, asset_id, unit, points)
                .await?;
        self.audit(
            actor,
            "industrial.series.append",
            series_id,
            "appended",
            json!({"asset_id": asset_id, "points": series.points.len()}),
        )
        .await?;
        Ok(series)
    }

    /// Aggregate a series over a window; missing buckets are reported as
    /// missing, never interpolated.
    pub async fn industrial_series_query(
        &self,
        id: &str,
        from_ms: i64,
        to_ms: i64,
        bucket_ms: i64,
    ) -> Result<openrt_industrial::QueryResult, Error> {
        Ok(openrt_industrial::query_series(&*self.store, id, from_ms, to_ms, bucket_ms).await?)
    }

    /// Least-squares trend over a stored series; persisted + audited.
    pub async fn industrial_trend_analysis(
        &self,
        asset_id: &str,
        series_id: &str,
        params: openrt_industrial::TrendParams,
        threshold: Option<openrt_industrial::Threshold>,
        actor: &str,
    ) -> Result<openrt_industrial::TrendResult, Error> {
        let result = trend_analysis(&*self.store, asset_id, series_id, params, threshold).await?;
        self.audit(
            actor,
            "industrial.analysis",
            asset_id,
            "trend_computed",
            json!({"series_id": series_id, "direction": format!("{:?}", result.direction)}),
        )
        .await?;
        Ok(result)
    }

    /// One detector run (statistical|rule|time_series|model_assisted);
    /// the run records which method executed — model-assisted without a
    /// model backend reports UNAVAILABLE instead of heuristic output.
    pub async fn industrial_anomaly_analysis(
        &self,
        asset_id: &str,
        series_id: &str,
        method: &str,
        params: &Value,
        actor: &str,
    ) -> Result<openrt_industrial::DetectorRun, Error> {
        let run = anomaly_analysis(&*self.store, asset_id, series_id, method, params).await?;
        let result = if run.unavailable.is_some() {
            "unavailable"
        } else if run.anomalies.is_empty() {
            "no_anomalies"
        } else {
            "anomalies_detected"
        };
        self.audit(
            actor,
            "industrial.anomaly",
            asset_id,
            result,
            json!({
                "series_id": series_id,
                "method": run.method.as_str(),
                "anomalies": run.anomalies.len(),
                "unavailable": run.unavailable,
            }),
        )
        .await?;
        Ok(run)
    }

    /// Bounded, evidence-labelled RCA over the observed graph context.
    pub async fn industrial_rca(
        &self,
        subject: &str,
        actor: &str,
    ) -> Result<openrt_industrial::RcaReport, Error> {
        let ctx = rca_context(&*self.store, subject).await?;
        let report = openrt_industrial::root_cause(subject, &ctx);
        openrt_industrial::put_analysis(&*self.store, "rca", subject, &report).await?;
        self.audit(
            actor,
            "industrial.rca",
            subject,
            "report_generated",
            json!({"findings": report.findings.len()}),
        )
        .await?;
        Ok(report)
    }

    /// FMEA table for an asset's equipment type (library knowledge, no
    /// fabricated failure rates).
    pub async fn industrial_fmea(
        &self,
        asset_id: &str,
    ) -> Result<Vec<openrt_industrial::FailureMode>, Error> {
        let asset = require_asset(&*self.store, asset_id).await?;
        let equipment_type = asset
            .attrs
            .get("equipment_type")
            .and_then(Value::as_str)
            .unwrap_or("generic");
        Ok(openrt_industrial::fmea_table(equipment_type))
    }

    /// Deterministic heuristic risk band — `method` is always `heuristic`.
    pub async fn industrial_risk(
        &self,
        asset_id: &str,
        series_id: Option<&str>,
        actor: &str,
    ) -> Result<openrt_industrial::RiskResult, Error> {
        let result = risk_analysis(&*self.store, asset_id, series_id).await?;
        self.audit(
            actor,
            "industrial.analysis",
            asset_id,
            "risk_computed",
            json!({
                "series_id": result.series_id,
                "band": result.band.map(|b| format!("{b:?}")),
                "insufficient": result.insufficient,
            }),
        )
        .await?;
        Ok(result)
    }

    /// Temporal correlation over explicit events (grouping ≠ causation —
    /// the caveat travels with the result).
    pub async fn industrial_correlate(
        &self,
        events: &[openrt_industrial::TimedEvent],
        params: openrt_industrial::CorrelateParams,
        actor: &str,
    ) -> Result<openrt_industrial::CorrelationResult, Error> {
        let result = openrt_industrial::correlate(events, params);
        openrt_industrial::put_analysis(&*self.store, "correlation", "fleet", &result).await?;
        self.audit(
            actor,
            "industrial.correlation",
            "fleet",
            "groups_computed",
            json!({"events": events.len(), "groups": result.groups.len()}),
        )
        .await?;
        Ok(result)
    }

    /// What-if scenario: known (graph) / simulated (rules) / assumed
    /// (caller) statements, explicitly marked advisory-only.
    pub async fn industrial_scenario(
        &self,
        asset_id: &str,
        event: &str,
        assumptions: Vec<String>,
        actor: &str,
    ) -> Result<openrt_industrial::ScenarioResult, Error> {
        let out = scenario_analysis(&*self.store, asset_id, event, assumptions).await?;
        self.audit(
            actor,
            "industrial.scenario",
            asset_id,
            "simulated",
            json!({"event": event, "advisory_only": out.advisory_only}),
        )
        .await?;
        Ok(out)
    }

    /// Evidence-linked investigation answer for one asset.
    pub async fn industrial_investigate(
        &self,
        asset_id: &str,
        actor: &str,
    ) -> Result<openrt_industrial::Investigation, Error> {
        let out = investigate_asset(&*self.store, asset_id).await?;
        self.audit(
            actor,
            "industrial.copilot",
            asset_id,
            "investigation_generated",
            json!({"sections": out.sections.len()}),
        )
        .await?;
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Runtime: research-only trading (M12.8–M12.11)
    // -----------------------------------------------------------------------

    /// Register one research instrument (metadata only — no market feed).
    pub async fn trading_instrument_put(
        &self,
        instrument: openrt_trading::Instrument,
        actor: &str,
    ) -> Result<(), Error> {
        let id = instrument.id.clone();
        openrt_trading::put_instrument(&*self.store, &instrument).await?;
        self.audit(
            actor,
            "trading.instrument",
            &id,
            "registered",
            json!({"symbol": instrument.symbol, "kind": format!("{:?}", instrument.kind)}),
        )
        .await?;
        Ok(())
    }

    /// Build + persist a content-addressed research dataset from bars.
    pub async fn trading_dataset_build(
        &self,
        id: &str,
        version: &str,
        config: openrt_trading::DatasetConfig,
        inputs: Vec<String>,
        bars: Vec<openrt_trading::Bar>,
        actor: &str,
    ) -> Result<openrt_trading::ResearchDataset, Error> {
        let (ds, bars) = openrt_trading::build_dataset(id, version, config, inputs, bars)?;
        openrt_trading::put_dataset(&*self.store, &ds, &bars).await?;
        self.audit(
            actor,
            "trading.dataset",
            id,
            "built",
            json!({"bars": ds.bar_count, "digest": ds.digest}),
        )
        .await?;
        Ok(ds)
    }

    /// Parse a CSV (`t_ms,open,high,low,close,volume`) and build a dataset.
    pub async fn trading_dataset_import_csv(
        &self,
        id: &str,
        version: &str,
        config: openrt_trading::DatasetConfig,
        csv: &str,
        actor: &str,
    ) -> Result<openrt_trading::ResearchDataset, Error> {
        let bars = openrt_trading::parse_bars_csv(csv)?;
        let (ds, bars) = openrt_trading::build_dataset(
            id,
            version,
            config,
            vec!["csv:import".to_string()],
            bars,
        )?;
        openrt_trading::put_dataset(&*self.store, &ds, &bars).await?;
        self.audit(
            actor,
            "trading.dataset",
            id,
            "imported",
            json!({"bars": ds.bar_count, "digest": ds.digest, "format": "csv"}),
        )
        .await?;
        Ok(ds)
    }

    pub async fn trading_instruments(&self) -> Result<Vec<openrt_trading::Instrument>, Error> {
        Ok(openrt_trading::list_instruments(&*self.store).await?)
    }

    pub async fn trading_datasets(&self) -> Result<Vec<openrt_trading::ResearchDataset>, Error> {
        Ok(openrt_trading::list_datasets(&*self.store).await?)
    }

    pub async fn trading_dataset_bars(
        &self,
        id: &str,
    ) -> Result<Option<Vec<openrt_trading::Bar>>, Error> {
        Ok(openrt_trading::get_bars(&*self.store, id).await?)
    }

    /// Persist a declarative research strategy (causal DSL by construction).
    pub async fn trading_strategy_put(
        &self,
        strategy: openrt_trading::Strategy,
        actor: &str,
    ) -> Result<String, Error> {
        let id = strategy.id.clone();
        let digest = strategy.digest();
        openrt_trading::put_strategy(&*self.store, &strategy).await?;
        self.audit(
            actor,
            "trading.strategy",
            &id,
            "registered",
            json!({"digest": digest}),
        )
        .await?;
        Ok(digest)
    }

    pub async fn trading_strategies(&self) -> Result<Vec<openrt_trading::Strategy>, Error> {
        Ok(openrt_trading::list_strategies(&*self.store).await?)
    }

    /// Deterministic next-bar-open backtest — simulated fills, costs and
    /// leakage report; persisted under `trading.backtests`.
    pub async fn trading_backtest(
        &self,
        strategy_id: &str,
        dataset_id: &str,
        initial_cash: Option<f64>,
        actor: &str,
    ) -> Result<openrt_trading::BacktestResult, Error> {
        let result = backtest_run(&*self.store, strategy_id, dataset_id, initial_cash).await?;
        self.audit(
            actor,
            "trading.backtest",
            &result.id,
            "simulated",
            json!({
                "strategy_id": strategy_id,
                "dataset_id": dataset_id,
                "simulated": result.simulated,
                "total_return_pct": result.metrics.total_return_pct,
            }),
        )
        .await?;
        Ok(result)
    }

    /// Walk-forward folds over one dataset (in-sample evaluation only for
    /// fold verdicts); persisted under `trading.backtests`.
    pub async fn trading_walk_forward(
        &self,
        strategy_id: &str,
        dataset_id: &str,
        params: openrt_trading::WalkForwardParams,
        actor: &str,
    ) -> Result<openrt_trading::WalkForwardResult, Error> {
        let result = walk_forward_run(&*self.store, strategy_id, dataset_id, params).await?;
        let wf_subject = format!("walk_forward:{strategy_id}@{dataset_id}");
        self.audit(
            actor,
            "trading.backtest",
            &wf_subject,
            "walk_forward_simulated",
            json!({
                "strategy_id": strategy_id,
                "dataset_id": dataset_id,
                "folds": result.folds.len(),
            }),
        )
        .await?;
        Ok(result)
    }

    /// Persist one research experiment record.
    pub async fn trading_experiment_put(
        &self,
        experiment: openrt_trading::ResearchExperiment,
        actor: &str,
    ) -> Result<(), Error> {
        let id = experiment.id.clone();
        openrt_trading::put_experiment(&*self.store, &experiment).await?;
        self.audit(
            actor,
            "trading.experiment",
            &id,
            "recorded",
            json!({"conclusion": experiment.conclusion}),
        )
        .await?;
        Ok(())
    }

    pub async fn trading_experiments(
        &self,
    ) -> Result<Vec<openrt_trading::ResearchExperiment>, Error> {
        Ok(openrt_trading::list_experiments(&*self.store).await?)
    }

    /// The one broker-order refusal point: always denied, always audited.
    /// No order ever reaches any venue — execution is out of scope.
    pub async fn trading_execute(&self, order: &Value) -> Result<Value, Error> {
        let refused = openrt_trading::execute_order_denied(order);
        let result = if refused.is_ok() { "allowed" } else { "denied" };
        self.audit(
            "operator",
            "trading.broker.execute",
            "broker",
            result,
            json!({"order": order}),
        )
        .await?;
        match refused {
            Ok(()) => Err(Error::Trading(openrt_trading::TradingError::ResearchOnly(
                "refusal point reached an impossible success state".to_string(),
            ))),
            Err(e) => Err(Error::Trading(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime: experience-driven self-improvement (M12.12–M12.19)
// ---------------------------------------------------------------------------

impl Runtime {
    /// Seal one experience episode into `improve.episodes`.
    pub async fn learn_capture_episode(
        &self,
        mut episode: openrt_selfimprove::ExperienceEpisode,
        actor: &str,
    ) -> Result<(), Error> {
        if episode.provenance.origin.trim().is_empty() {
            episode.provenance = openrt_selfimprove::Provenance::new(
                "runtime.learn_capture_episode",
                "openrt-runtime",
            );
        }
        if episode.created_at.trim().is_empty() {
            episode.created_at = openrt_core::now().to_rfc3339();
        }
        let id = episode.id.clone();
        let mut improve = self.load_improve().await?;
        improve.record_episode(episode)?;
        self.save_improve(&improve).await?;
        self.audit(actor, "learning.episode", &id, "recorded", json!({}))
            .await?;
        Ok(())
    }

    /// Evaluate a stored episode into MEASURED vs INFERRED metrics.
    pub async fn learn_evaluate_episode(
        &self,
        id: &str,
        actor: &str,
    ) -> Result<openrt_selfimprove::EpisodeEvaluation, Error> {
        let improve = self.load_improve().await?;
        let eval = improve.evaluate_episode(id)?;
        self.audit(
            actor,
            "learning.episode",
            id,
            "evaluated",
            json!({"measured_keys": eval.measured.keys().cloned().collect::<Vec<String>>()}),
        )
        .await?;
        Ok(eval)
    }

    pub async fn learn_episodes(
        &self,
    ) -> Result<Vec<openrt_selfimprove::ExperienceEpisode>, Error> {
        let improve = self.load_improve().await?;
        Ok(improve.list_episodes().into_iter().cloned().collect())
    }

    /// Derive a candidate skill from real execution history (candidate —
    /// never self-activating; approval and activation stay operator-gated).
    pub async fn learn_candidate_skill(
        &self,
        request_id: &str,
        description: &str,
        permissions: Vec<String>,
        session_id: &str,
        actor: &str,
    ) -> Result<String, Error> {
        let id = self
            .skill_submit_from_history(request_id, description, permissions, session_id)
            .await?;
        self.audit(
            actor,
            "learning.skill_candidate",
            &id,
            "submitted",
            json!({}),
        )
        .await?;
        Ok(id)
    }

    /// Derive a candidate instinct from real execution history.
    pub async fn learn_candidate_instinct(
        &self,
        session_id: &str,
        actor: &str,
    ) -> Result<String, Error> {
        let id = self.instinct_submit_from_history(session_id).await?;
        self.audit(
            actor,
            "learning.instinct_candidate",
            &id,
            "submitted",
            json!({}),
        )
        .await?;
        Ok(id)
    }

    /// Propose a genome evolution (proposal only — applying stays gated).
    pub async fn learn_candidate_genome(
        &self,
        id: &str,
        genome_id: &str,
        summary: &str,
        actor: &str,
    ) -> Result<(), Error> {
        self.genome_evolve_propose(id, genome_id, summary).await?;
        self.audit(
            actor,
            "learning.genome_candidate",
            id,
            "proposed",
            json!({}),
        )
        .await?;
        Ok(())
    }

    /// Run + record the deterministic bench probe for one artifact and
    /// return the raw MEASURED result (no audit — callers audit with the
    /// action they are performing).
    pub async fn bench_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<openrt_bench::BenchResult, Error> {
        let params = openrt_bench::BenchParams {
            repetitions: 1,
            timeout_secs: 5,
        };
        let tasks = vec![openrt_bench::BenchTask::new("promotion_probe", "ping")];
        let runner = openrt_bench::BenchRunner::new(self.provider.as_ref(), params, tasks);
        let result = runner.run(artifact_id).await;
        openrt_bench::record_result(&*self.store, &result).await?;
        Ok(result)
    }

    /// Run + record a bench over an arbitrary prompt set, audited as
    /// `learning.benchmark` (the store keeps the measured numbers).
    pub async fn learn_benchmark(
        &self,
        artifact_id: &str,
        prompts: &[String],
        actor: &str,
    ) -> Result<openrt_bench::BenchResult, Error> {
        let params = openrt_bench::BenchParams {
            repetitions: 1,
            timeout_secs: 5,
        };
        let tasks = if prompts.is_empty() {
            vec![openrt_bench::BenchTask::new("probe", "ping")]
        } else {
            prompts
                .iter()
                .map(|p| openrt_bench::BenchTask::new("custom", p))
                .collect()
        };
        let runner = openrt_bench::BenchRunner::new(self.provider.as_ref(), params, tasks);
        let result = runner.run(artifact_id).await;
        openrt_bench::record_result(&*self.store, &result).await?;
        self.audit(
            actor,
            "learning.benchmark",
            artifact_id,
            "bench_recorded",
            json!({
                "ok_count": result.aggregate.ok_count,
                "error_rate": result.aggregate.error_rate,
            }),
        )
        .await?;
        Ok(result)
    }

    /// Benchmark-gated skill promotion. `baseline_metrics` must come from a
    /// previously recorded bench run; without it the verdict is Hold.
    /// Promotion itself is operator-gated (approve + shield-checked
    /// activation) and every decision is persisted + audited.
    pub async fn learn_promote_skill(
        &self,
        id: &str,
        operator: &str,
        baseline_metrics: Option<BTreeMap<String, f64>>,
    ) -> Result<String, Error> {
        let current = self.bench_artifact(&format!("skill.{id}")).await?;
        let metrics = bench_metrics(&current);
        let decision = promotion_verdict(&metrics, baseline_metrics.as_ref());
        if decision == openrt_selfimprove::PromotionDecision::Promote {
            self.assets_approve_candidate(id, operator).await?;
            self.assets_activate_candidate(id, operator).await?;
        }
        let record = promotion_record(
            &format!("promotion.skill.{id}"),
            openrt_selfimprove::PromotionKind::Skill,
            id,
            decision,
            metrics,
            baseline_metrics.as_ref(),
            "measured bench gate over skill.<id>: error_rate vs operator-supplied baseline",
            Some(operator),
        );
        let mut improve = self.load_improve().await?;
        improve.record_promotion(record)?;
        self.save_improve(&improve).await?;
        self.audit(
            operator,
            "learning.promotion",
            id,
            decision.as_str(),
            json!({"kind": "skill"}),
        )
        .await?;
        Ok(decision.as_str().to_string())
    }

    /// Benchmark-gated instinct promotion (validate + activate only on
    /// measured improvement).
    pub async fn learn_promote_instinct(
        &self,
        id: &str,
        operator: &str,
        baseline_metrics: Option<BTreeMap<String, f64>>,
    ) -> Result<String, Error> {
        let current = self.bench_artifact(&format!("instinct.{id}")).await?;
        let metrics = bench_metrics(&current);
        let decision = promotion_verdict(&metrics, baseline_metrics.as_ref());
        if decision == openrt_selfimprove::PromotionDecision::Promote {
            self.assets_validate_instinct(id).await?;
            self.assets_activate_instinct(id, operator).await?;
        }
        let record = promotion_record(
            &format!("promotion.instinct.{id}"),
            openrt_selfimprove::PromotionKind::Instinct,
            id,
            decision,
            metrics,
            baseline_metrics.as_ref(),
            "measured bench gate over instinct.<id>: error_rate vs operator-supplied baseline",
            Some(operator),
        );
        let mut improve = self.load_improve().await?;
        improve.record_promotion(record)?;
        self.save_improve(&improve).await?;
        self.audit(
            operator,
            "learning.promotion",
            id,
            decision.as_str(),
            json!({"kind": "instinct"}),
        )
        .await?;
        Ok(decision.as_str().to_string())
    }

    /// Benchmark-gated genome promotion (apply only on measured improvement).
    pub async fn learn_promote_genome(
        &self,
        genome_id: &str,
        operator: &str,
        baseline_metrics: Option<BTreeMap<String, f64>>,
    ) -> Result<String, Error> {
        let current = self.bench_artifact(&format!("genome.{genome_id}")).await?;
        let metrics = bench_metrics(&current);
        let decision = promotion_verdict(&metrics, baseline_metrics.as_ref());
        if decision == openrt_selfimprove::PromotionDecision::Promote {
            let grant = grant_for(operator)?;
            self.assets_apply_genome(genome_id, Some(&grant)).await?;
        }
        let record = promotion_record(
            &format!("promotion.genome.{genome_id}"),
            openrt_selfimprove::PromotionKind::Genome,
            genome_id,
            decision,
            metrics,
            baseline_metrics.as_ref(),
            "measured bench gate over genome.<id>: error_rate vs operator-supplied baseline",
            Some(operator),
        );
        let mut improve = self.load_improve().await?;
        improve.record_promotion(record)?;
        self.save_improve(&improve).await?;
        self.audit(
            operator,
            "learning.promotion",
            genome_id,
            decision.as_str(),
            json!({"kind": "genome"}),
        )
        .await?;
        Ok(decision.as_str().to_string())
    }

    /// Roll a skill back to inactive; the rollback decision is persisted in
    /// `improve.promotions` and audited.
    pub async fn learn_rollback_skill(&self, id: &str, operator: &str) -> Result<String, Error> {
        let grant = grant_for(operator)?;
        {
            let mut assets = self.load_assets().await?;
            assets.deactivate_skill(
                &openrt_workspace_assets::SkillId(id.to_string()),
                Some(&grant),
            )?;
            self.save_assets(&assets).await?;
        }
        let record = promotion_record(
            &format!("rollback.skill.{id}"),
            openrt_selfimprove::PromotionKind::Skill,
            id,
            openrt_selfimprove::PromotionDecision::Rollback,
            BTreeMap::new(),
            None,
            "operator-requested rollback to inactive",
            Some(operator),
        );
        let mut improve = self.load_improve().await?;
        improve.record_promotion(record)?;
        self.save_improve(&improve).await?;
        self.audit(
            operator,
            "learning.rollback",
            id,
            "rollback",
            json!({"kind": "skill"}),
        )
        .await?;
        Ok("rollback".to_string())
    }

    /// Roll a genome back: deactivate skills the current genome introduced,
    /// then re-apply the previous version (measured history kept).
    pub async fn learn_genome_rollback(
        &self,
        current_id: &str,
        previous_id: &str,
        operator: &str,
    ) -> Result<String, Error> {
        let grant = grant_for(operator)?;
        let to_deactivate: Vec<String> = {
            let assets = self.load_assets().await?;
            let cur =
                assets.get_genome(&openrt_workspace_assets::GenomeId(current_id.to_string()))?;
            let prev =
                assets.get_genome(&openrt_workspace_assets::GenomeId(previous_id.to_string()))?;
            let cur_skills: Vec<openrt_workspace_assets::SkillId> =
                cur.skills.iter().map(|(k, _)| k.clone()).collect();
            let prev_skills: BTreeSet<String> =
                prev.skills.iter().map(|(k, _)| k.0.clone()).collect();
            let mut out = Vec::new();
            for sid in cur_skills {
                if prev_skills.contains(&sid.0) {
                    continue;
                }
                if assets.get_skill(&sid)?.activation
                    == openrt_workspace_assets::ActivationState::Active
                {
                    out.push(sid.0.clone());
                }
            }
            out
        };
        {
            let mut assets = self.load_assets().await?;
            for sid in &to_deactivate {
                assets.deactivate_skill(
                    &openrt_workspace_assets::SkillId(sid.clone()),
                    Some(&grant),
                )?;
            }
            self.save_assets(&assets).await?;
        }
        self.assets_apply_genome(previous_id, Some(&grant)).await?;
        let record = promotion_record(
            &format!("rollback.genome.{current_id}"),
            openrt_selfimprove::PromotionKind::Genome,
            current_id,
            openrt_selfimprove::PromotionDecision::Rollback,
            BTreeMap::new(),
            None,
            &format!("re-applied previous genome `{previous_id}`"),
            Some(operator),
        );
        let mut improve = self.load_improve().await?;
        improve.record_promotion(record)?;
        self.save_improve(&improve).await?;
        self.audit(
            operator,
            "learning.rollback",
            current_id,
            "rollback",
            json!({"kind": "genome", "previous": previous_id}),
        )
        .await?;
        Ok("rollback".to_string())
    }

    /// Record one planned training experiment (no training runs here —
    /// Phase 12 has no backend; the run starts honest-failed).
    pub async fn learn_model_experiment_create(
        &self,
        spec: openrt_train::ExperimentSpec,
    ) -> Result<String, Error> {
        let run = openrt_train::new_run(spec)?;
        let id = run.id.clone();
        openrt_train::put_run(&*self.store, &run).await?;
        self.audit(
            "operator",
            "learning.model_experiment",
            &id,
            "created",
            json!({
                "method": run.spec.method.as_str(),
                "base_model": run.spec.base_model,
                "status": run.status.as_str(),
            }),
        )
        .await?;
        Ok(id)
    }

    /// Attempt to start a training run. With no `TrainingBackend`
    /// configured the run fails with the probe's explicit reason and a
    /// `TrainJobFailed` event is emitted — never a fabricated success.
    pub async fn learn_model_experiment_start(&self, id: &str) -> Result<String, Error> {
        let run = openrt_train::start_run(&*self.store, id, None).await?;
        let data = json!({
            "run_id": run.id,
            "backend": run.backend,
            "status": run.status.as_str(),
            "reason": run.failure_reason,
        });
        if run.status == openrt_train::ExperimentStatus::Failed {
            self.bus
                .emit_correlated(
                    openrt_core::EventKind::TrainJobFailed,
                    None,
                    None,
                    None,
                    data.clone(),
                )
                .await;
        } else if run.status == openrt_train::ExperimentStatus::Completed {
            self.bus
                .emit_correlated(
                    openrt_core::EventKind::TrainJobCompleted,
                    None,
                    None,
                    None,
                    data.clone(),
                )
                .await;
        }
        let result = if run.status == openrt_train::ExperimentStatus::Failed {
            "failed"
        } else {
            "started"
        };
        self.audit("operator", "learning.model_experiment", id, result, data)
            .await?;
        Ok(run.status.as_str().to_string())
    }

    pub async fn learn_model_experiment_get(
        &self,
        id: &str,
    ) -> Result<openrt_train::ExperimentRun, Error> {
        Ok(openrt_train::get_run(&*self.store, id)
            .await?
            .ok_or_else(|| {
                Error::Train(openrt_train::TrainError::NotFound(format!("run `{id}`")))
            })?)
    }

    pub async fn learn_model_experiment_list(
        &self,
    ) -> Result<Vec<openrt_train::ExperimentRun>, Error> {
        Ok(openrt_train::list_runs(&*self.store).await?)
    }

    /// Record one routing observation (success/failure of an artifact on a
    /// task type) — signals are a SUBORDINATE overlay, never authority.
    pub async fn learn_routing_record(
        &self,
        task_type: &str,
        artifact_id: &str,
        success: bool,
        latency_ms: Option<u64>,
        actor: &str,
    ) -> Result<(), Error> {
        let signal = openrt_router::RoutingSignal::new(
            task_type,
            artifact_id,
            success,
            latency_ms,
            "runtime",
        );
        openrt_router::record_signal(&*self.store, &signal).await?;
        self.audit(
            actor,
            "learning.routing",
            artifact_id,
            if success { "success" } else { "failure" },
            json!({"task_type": task_type}),
        )
        .await?;
        Ok(())
    }

    /// Route with base selection authoritative; historical signals may
    /// redirect only to artifacts that pass the SAME routing checks.
    pub async fn learn_route(
        &self,
        task_type: &str,
        request: openrt_router::RouteRequest,
        min_samples: usize,
    ) -> Result<openrt_router::RouteVerdict, Error> {
        let signals = openrt_router::list_signals(&*self.store).await?;
        Ok(self
            .router
            .route_with_signals(
                &self.model_registry,
                &request,
                &signals,
                task_type,
                min_samples,
            )
            .await?)
    }

    pub async fn learn_routing_signals(&self) -> Result<Vec<openrt_router::RoutingSignal>, Error> {
        Ok(openrt_router::list_signals(&*self.store).await?)
    }

    pub async fn learn_promotions(
        &self,
    ) -> Result<Vec<openrt_selfimprove::PromotionRecord>, Error> {
        let improve = self.load_improve().await?;
        Ok(improve.list_promotions().into_iter().cloned().collect())
    }
}

// ---------------------------------------------------------------------------
// Runtime: model cache + prioritized recall (M12.19–M12.20)
// ---------------------------------------------------------------------------

impl Runtime {
    async fn phase12_cache(&self) -> Result<openrt_model_cache::ModelCache, Error> {
        Ok(
            match openrt_model_cache::ModelCache::load(&*self.store).await? {
                Some(cache) => cache,
                None => openrt_model_cache::ModelCache::new(default_cache_caps()),
            },
        )
    }

    /// Register a cache entry (first-time wiring of `cache.meta`) and
    /// persist the state so restarts see it.
    pub async fn cache_put(
        &self,
        digest: &str,
        tier: openrt_hierarchy::Tier,
        size_bytes: u64,
        pinned: bool,
    ) -> Result<(), Error> {
        let mut cache = self.phase12_cache().await?;
        cache.put(digest.to_string(), tier, size_bytes, pinned)?;
        cache.save(&*self.store).await?;
        Ok(())
    }

    pub async fn cache_state(&self) -> Result<openrt_model_cache::CacheState, Error> {
        Ok(self.phase12_cache().await?.snapshot())
    }

    pub async fn cache_pin(&self, digest: &str) -> Result<(), Error> {
        let mut cache = self.phase12_cache().await?;
        cache.pin(digest)?;
        cache.save(&*self.store).await?;
        Ok(())
    }

    pub async fn cache_unpin(&self, digest: &str) -> Result<(), Error> {
        let mut cache = self.phase12_cache().await?;
        cache.unpin(digest)?;
        cache.save(&*self.store).await?;
        Ok(())
    }

    pub async fn cache_evict(
        &self,
        tier: openrt_hierarchy::Tier,
    ) -> Result<openrt_model_cache::CacheEntry, Error> {
        let mut cache = self.phase12_cache().await?;
        let victim = cache.evict_lru(tier)?;
        cache.save(&*self.store).await?;
        Ok(victim)
    }

    /// Deterministic rank+recency prioritization over semantic recall
    /// (lineage: digest + occurred_at travel with every hit).
    pub async fn memory_prioritized_recall(
        &self,
        query: &str,
        top_k: usize,
        threshold: f32,
        recency_weight: f64,
        half_life_days: f64,
    ) -> Result<openrt_memory::PrioritizedResult, Error> {
        let set = openrt_memory::recall_fact(&*self.store, query, top_k, threshold).await?;
        let params = openrt_memory::PriorityParams {
            recency_weight,
            half_life_days,
            now_ms: openrt_core::now().timestamp_millis(),
        };
        Ok(openrt_memory::prioritize_result(&set.items, params))
    }

    /// Prefetch has no backend in Phase 12 — honest UNAVAILABLE, never a
    /// fabricated warm cache.
    pub async fn memory_prefetch(&self, _query: &str) -> Result<Value, Error> {
        Err(Error::Unavailable(
            "memory prefetch: no prefetch backend configured (Phase 12 reports UNAVAILABLE \
             — prefetch is never faked)"
                .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Doctor rows (Part N): Pass / Warn / Unavailable only — never Fail.
// ---------------------------------------------------------------------------

impl Runtime {
    pub async fn phase12_doctor_checks(&self) -> Vec<DoctorCheck> {
        let mut out: Vec<DoctorCheck> = Vec::new();

        let asset_keys = self.store.list_records(openrt_industrial::ASSETS_NS).await;
        let series_keys = self.store.list_records(openrt_industrial::SERIES_NS).await;
        let analysis_keys = self
            .store
            .list_records(openrt_industrial::ANALYSIS_NS)
            .await;
        let dataset_keys = self.store.list_records(openrt_trading::DATASETS_NS).await;
        let instr_keys = self
            .store
            .list_records(openrt_trading::INSTRUMENTS_NS)
            .await;
        let strat_keys = self.store.list_records(openrt_trading::STRATEGIES_NS).await;
        let bench_keys = self.store.list_records(openrt_bench::BENCH_NS).await;
        let signal_keys = self.store.list_records(openrt_router::SIGNALS_NS).await;

        type StoreRows = Result<Vec<(String, Value)>, openrt_persistence::PersistenceError>;
        let count = |keys: &StoreRows, prefix: &str| -> usize {
            keys.as_ref()
                .map(|v| v.iter().filter(|(k, _)| k.starts_with(prefix)).count())
                .unwrap_or(0)
        };
        let store_err =
            |keys: &StoreRows| -> Option<String> { keys.as_ref().err().map(|e| e.to_string()) };

        // ---- industrial intelligence (M12.1–M12.7) ----
        let assets = count(&asset_keys, "asset.");
        let rels = count(&asset_keys, "rel.");
        match store_err(&asset_keys) {
            None => out.push(DoctorCheck {
                name: "industrial.intelligence".to_string(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "advisory industrial intelligence online: {assets} assets, {rels} relations \
                     (graph mirror), {} analysis records",
                    count(&analysis_keys, "analysis.")
                ),
            }),
            Some(e) => out.push(DoctorCheck {
                name: "industrial.intelligence".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("industrial store unreadable: {e}"),
            }),
        }
        match store_err(&asset_keys) {
            None => out.push(DoctorCheck {
                name: "industrial.data".to_string(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "{assets} assets under industrial.assets; canonical relations mirrored \
                     into knowledge.graph"
                ),
            }),
            Some(e) => out.push(DoctorCheck {
                name: "industrial.data".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("industrial.assets unreadable: {e}"),
            }),
        }
        let series = count(&series_keys, "");
        match store_err(&series_keys) {
            None => out.push(DoctorCheck {
                name: "industrial.timeseries".to_string(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "{series} series under industrial.series (missing buckets reported, \
                     never interpolated; cap 100000 points/series)"
                ),
            }),
            Some(e) => out.push(DoctorCheck {
                name: "industrial.timeseries".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("industrial.series unreadable: {e}"),
            }),
        }
        out.push(DoctorCheck {
            name: "industrial.anomaly-detector".to_string(),
            status: DoctorStatus::Pass,
            detail: "statistical/rule/time_series detectors deterministic and measured; \
                     model_assisted reports UNAVAILABLE without a model backend"
                .to_string(),
        });
        out.push(DoctorCheck {
            name: "industrial.rca".to_string(),
            status: DoctorStatus::Pass,
            detail: "bounded RCA with OBSERVED_FACT/INFERRED_RELATION/HYPOTHESIS/UNKNOWN \
                     labels; advisory only — no control path"
                .to_string(),
        });

        // ---- trading research (M12.8–M12.11) ----
        let instruments = count(&instr_keys, "instrument.");
        match store_err(&dataset_keys) {
            None => out.push(DoctorCheck {
                name: "trading.research".to_string(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "{instruments} instruments, {} datasets, {} strategies (research only — \
                     no broker, no order path)",
                    count(&dataset_keys, "dataset."),
                    count(&strat_keys, "strategy.")
                ),
            }),
            Some(e) => out.push(DoctorCheck {
                name: "trading.research".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("trading store unreadable: {e}"),
            }),
        }
        out.push(DoctorCheck {
            name: "trading.backtest-engine".to_string(),
            status: DoctorStatus::Pass,
            detail: "deterministic next-bar-open fills with slippage/commission + leakage \
                     detection; execute_order_denied is the one refusal point"
                .to_string(),
        });
        match store_err(&dataset_keys) {
            None => out.push(DoctorCheck {
                name: "trading.dataset-store".to_string(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "{} datasets with content digests under trading.datasets",
                    count(&dataset_keys, "dataset.")
                ),
            }),
            Some(e) => out.push(DoctorCheck {
                name: "trading.dataset-store".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("trading.datasets unreadable: {e}"),
            }),
        }

        // ---- learning / self-improvement (M12.12–M12.19) ----
        match self.load_improve().await {
            Ok(improve) => {
                let episodes = improve.list_episodes().len();
                out.push(DoctorCheck {
                    name: "learning.subsystem".to_string(),
                    status: DoctorStatus::Pass,
                    detail: format!(
                        "experience + promotion records in improve.episodes/improve.promotions \
                         ({episodes} episodes); promotions are operator-gated"
                    ),
                });
                out.push(DoctorCheck {
                    name: "learning.experience-store".to_string(),
                    status: DoctorStatus::Pass,
                    detail: format!(
                        "{episodes} sealed episodes; evaluation splits measured from inferred \
                         (method measured+inferred/v1)"
                    ),
                });
            }
            Err(e) => {
                out.push(DoctorCheck {
                    name: "learning.subsystem".to_string(),
                    status: DoctorStatus::Warn,
                    detail: format!("improve store unavailable: {e}"),
                });
                out.push(DoctorCheck {
                    name: "learning.experience-store".to_string(),
                    status: DoctorStatus::Unavailable,
                    detail: format!("episodes unreadable: {e}"),
                });
            }
        }
        out.push(DoctorCheck {
            name: "learning.skill-adaptation".to_string(),
            status: DoctorStatus::Pass,
            detail: "candidate → approve → activate only after a measured bench gate \
                     (operator-gated; promotion records persist)"
                .to_string(),
        });
        out.push(DoctorCheck {
            name: "learning.genome-adaptation".to_string(),
            status: DoctorStatus::Pass,
            detail: "apply on measured improvement; rollback re-applies the previous \
                     genome version (operator-gated)"
                .to_string(),
        });
        let probe = openrt_train::probe_backend();
        let (model_status, model_detail) = if probe.trainable {
            (
                DoctorStatus::Warn,
                format!(
                    "environment probe trainable (gpu={}) but no TrainingBackend is \
                     configured in Phase 12: {}",
                    probe.gpu,
                    probe.reasons.join("; ")
                ),
            )
        } else {
            (
                DoctorStatus::Unavailable,
                format!(
                    "no training backend configured (environment probe: gpu={}, {})",
                    probe.gpu,
                    probe.reasons.join("; ")
                ),
            )
        };
        out.push(DoctorCheck {
            name: "learning.model-adaptation".to_string(),
            status: model_status,
            detail: model_detail,
        });
        match store_err(&bench_keys) {
            None => out.push(DoctorCheck {
                name: "learning.benchmark-store".to_string(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "{} measured bench runs under bench.results (promotion gate source)",
                    count(&bench_keys, "")
                ),
            }),
            Some(e) => out.push(DoctorCheck {
                name: "learning.benchmark-store".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("bench.results unreadable: {e}"),
            }),
        }
        out.push(DoctorCheck {
            name: "learning.rollback-capability".to_string(),
            status: DoctorStatus::Pass,
            detail: "skill deactivate, genome previous-version re-apply, and persisted \
                     rollback decisions in improve.promotions"
                .to_string(),
        });

        // ---- model cache / memory / routing (M12.19–M12.20) ----
        match self.phase12_cache().await {
            Ok(cache) => {
                let state = cache.snapshot();
                out.push(DoctorCheck {
                    name: "model.cache".to_string(),
                    status: DoctorStatus::Pass,
                    detail: format!(
                        "{} cache entries under cache.meta (first wiring in Phase 12)",
                        state.entries.len()
                    ),
                });
            }
            Err(e) => out.push(DoctorCheck {
                name: "model.cache".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("cache.meta unreadable: {e}"),
            }),
        }
        out.push(DoctorCheck {
            name: "memory.prioritized-recall".to_string(),
            status: DoctorStatus::Pass,
            detail: "rank+recency/v1 deterministic blend with id tie-break and digest \
                     + occurred_at lineage"
                .to_string(),
        });
        out.push(DoctorCheck {
            name: "memory.prefetch".to_string(),
            status: DoctorStatus::Unavailable,
            detail: "no prefetch backend configured — reported UNAVAILABLE, never faked"
                .to_string(),
        });
        match store_err(&signal_keys) {
            None => out.push(DoctorCheck {
                name: "routing.signals".to_string(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "{} routing signals (subordinate overlay; min 5 samples, base \
                     routing authoritative)",
                    count(&signal_keys, "signal.")
                ),
            }),
            Some(e) => out.push(DoctorCheck {
                name: "routing.signals".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("routing.signals unreadable: {e}"),
            }),
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Registered tools (Part D): eight namespaced tools riding the ONE gate.
// Tools carry no Runtime context — audits happen in the Runtime methods
// above; tool invocations are covered by the gate's permission events.
// ---------------------------------------------------------------------------

async fn tool_load_improve(
    store: &dyn PersistenceStore,
    operators: &[String],
    sovereign: bool,
) -> Result<openrt_selfimprove::ImproveStore, ToolError> {
    match openrt_selfimprove::ImproveStore::load(store).await {
        Ok(s) => Ok(s),
        Err(openrt_selfimprove::ImproveError::Store(msg))
            if msg.contains("missing improve.meta") =>
        {
            Ok(openrt_selfimprove::ImproveStore::new(operators, sovereign))
        }
        Err(e) => Err(ToolError::Internal(
            "learn.experience".to_string(),
            e.to_string(),
        )),
    }
}

struct IndustrialAssetsTool {
    spec: ToolSpec,
    store: Arc<dyn PersistenceStore>,
}

impl IndustrialAssetsTool {
    fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self {
            spec: ToolSpec::new(
                "industrial.assets",
                "List or read advisory industrial assets (plant/unit/equipment/sensor/alarm \
                 structure) from the ONE store. Read-only; no control path exists.",
            )
            .with_caps(&["industrial.assets.read"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["list", "get"]},
                    "id": {"type": "string"},
                    "kind": {"type": "string"},
                    "plant": {"type": "string"},
                    "unit": {"type": "string"},
                    "text": {"type": "string"}
                }
            })),
            store,
        }
    }
}

#[async_trait::async_trait]
impl Tool for IndustrialAssetsTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        const T: &str = "industrial.assets";
        let op = args.get("op").and_then(Value::as_str).unwrap_or("list");
        match op {
            "get" => {
                let id = str_arg(&args, T, T)?;
                let asset = openrt_industrial::get_asset(&*self.store, id)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                Ok(ToolOutput::Json(json!({"asset": asset})))
            }
            "list" => {
                let kind = args.get("kind").and_then(Value::as_str).and_then(|k| {
                    serde_json::from_value::<openrt_industrial::AssetKind>(json!(k)).ok()
                });
                let filter = openrt_industrial::AssetFilter {
                    kind,
                    plant: args
                        .get("plant")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    unit: args.get("unit").and_then(Value::as_str).map(str::to_string),
                    text: args.get("text").and_then(Value::as_str).map(str::to_string),
                };
                let assets = openrt_industrial::list_assets(&*self.store, &filter)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                Ok(ToolOutput::Json(json!({
                    "count": assets.len(),
                    "assets": assets,
                })))
            }
            other => Err(ToolError::InvalidArguments(
                T.to_string(),
                format!("unknown op `{other}` (list|get)"),
            )),
        }
    }
}

struct IndustrialSeriesTool {
    spec: ToolSpec,
    store: Arc<dyn PersistenceStore>,
}

impl IndustrialSeriesTool {
    fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self {
            spec: ToolSpec::new(
                "industrial.series",
                "Read or aggregate telemetry time series; missing buckets are reported as \
                 missing, never interpolated.",
            )
            .with_caps(&["industrial.series.read"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["list", "get", "query"]},
                    "id": {"type": "string"},
                    "from_ms": {"type": "integer"},
                    "to_ms": {"type": "integer"},
                    "bucket_ms": {"type": "integer"}
                }
            })),
            store,
        }
    }
}

#[async_trait::async_trait]
impl Tool for IndustrialSeriesTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        const T: &str = "industrial.series";
        let op = args.get("op").and_then(Value::as_str).unwrap_or("list");
        match op {
            "list" => {
                let series = openrt_industrial::list_series(&*self.store)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                Ok(ToolOutput::Json(json!({
                    "count": series.len(),
                    "series": series.iter().map(|s| json!({
                        "id": s.id,
                        "asset_id": s.asset_id,
                        "unit": s.unit,
                        "points": s.points.len(),
                        "updated_at": s.updated_at,
                    })).collect::<Vec<_>>(),
                })))
            }
            "get" => {
                let id = str_arg(&args, T, T)?;
                let series = openrt_industrial::get_series(&*self.store, id)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                Ok(ToolOutput::Json(json!({"series": series})))
            }
            "query" => {
                let id = str_arg(&args, T, T)?;
                let from_ms = args.get("from_ms").and_then(Value::as_i64).unwrap_or(0);
                let to_ms = args
                    .get("to_ms")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX);
                let bucket_ms = args
                    .get("bucket_ms")
                    .and_then(Value::as_i64)
                    .filter(|v| *v > 0)
                    .unwrap_or(60_000);
                let q =
                    openrt_industrial::query_series(&*self.store, id, from_ms, to_ms, bucket_ms)
                        .await
                        .map_err(|e| ind_err(T, e))?;
                Ok(ToolOutput::Json(
                    serde_json::to_value(q).unwrap_or(Value::Null),
                ))
            }
            other => Err(ToolError::InvalidArguments(
                T.to_string(),
                format!("unknown op `{other}` (list|get|query)"),
            )),
        }
    }
}

struct IndustrialAnalysisTool {
    spec: ToolSpec,
    store: Arc<dyn PersistenceStore>,
}

impl IndustrialAnalysisTool {
    fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self {
            spec: ToolSpec::new(
                "industrial.analysis",
                "Run advisory industrial analyses: trend, anomaly detection (statistical|rule|\
                 time_series|model_assisted), bounded RCA, FMEA table, heuristic risk, \
                 temporal correlation and what-if scenario. Results persist under \
                 industrial.analysis; advisory only.",
            )
            .with_caps(&["industrial.analysis.run"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "analysis": {"type": "string", "enum": ["trend", "anomaly", "rca", "fmea", "risk", "correlate", "scenario"]},
                    "asset_id": {"type": "string"},
                    "series_id": {"type": "string"},
                    "subject": {"type": "string"},
                    "method": {"type": "string"},
                    "event": {"type": "string"},
                    "assumptions": {"type": "array", "items": {"type": "string"}},
                    "events": {"type": "array"},
                    "params": {"type": "object"},
                    "low": {"type": "number"},
                    "high": {"type": "number"}
                },
                "required": ["analysis"]
            })),
            store,
        }
    }
}

#[async_trait::async_trait]
impl Tool for IndustrialAnalysisTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        const T: &str = "industrial.analysis";
        let analysis = str_arg(&args, T, T)?;
        let params = args.get("params").cloned().unwrap_or_else(|| json!({}));
        match analysis {
            "trend" => {
                let asset_id = str_arg(&args, T, T)?;
                let series_id = args
                    .get("series_id")
                    .and_then(Value::as_str)
                    .unwrap_or(asset_id);
                let mut tp = openrt_industrial::TrendParams::default();
                if let Some(v) = args
                    .get("params")
                    .and_then(|p| p.get("min_points"))
                    .and_then(Value::as_u64)
                {
                    tp.min_points = v as usize;
                }
                if let Some(v) = args
                    .get("params")
                    .and_then(|p| p.get("window"))
                    .and_then(Value::as_u64)
                {
                    tp.window = v as usize;
                }
                let threshold = match (
                    args.get("low").and_then(Value::as_f64),
                    args.get("high").and_then(Value::as_f64),
                ) {
                    (None, None) => None,
                    (low, high) => Some(openrt_industrial::Threshold { low, high }),
                };
                let r = trend_analysis(&*self.store, asset_id, series_id, tp, threshold)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                Ok(ToolOutput::Json(
                    serde_json::to_value(r).unwrap_or(Value::Null),
                ))
            }
            "anomaly" => {
                let asset_id = str_arg(&args, T, T)?;
                let series_id = args
                    .get("series_id")
                    .and_then(Value::as_str)
                    .unwrap_or(asset_id);
                let method = args
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("statistical");
                let run = anomaly_analysis(&*self.store, asset_id, series_id, method, &params)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                Ok(ToolOutput::Json(
                    serde_json::to_value(run).unwrap_or(Value::Null),
                ))
            }
            "rca" => {
                let subject = args
                    .get("subject")
                    .and_then(Value::as_str)
                    .or_else(|| args.get("asset_id").and_then(Value::as_str))
                    .ok_or_else(|| {
                        ToolError::InvalidArguments(T.to_string(), "`subject` is required".into())
                    })?;
                let ctx = rca_context(&*self.store, subject)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                let report = openrt_industrial::root_cause(subject, &ctx);
                Ok(ToolOutput::Json(
                    serde_json::to_value(report).unwrap_or(Value::Null),
                ))
            }
            "fmea" => {
                let asset_id = str_arg(&args, T, T)?;
                let asset = require_asset(&*self.store, asset_id)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                let equipment_type = asset
                    .attrs
                    .get("equipment_type")
                    .and_then(Value::as_str)
                    .unwrap_or("generic");
                let table = openrt_industrial::fmea_table(equipment_type);
                Ok(ToolOutput::Json(json!({
                    "equipment_type": equipment_type,
                    "modes": table,
                })))
            }
            "risk" => {
                let asset_id = str_arg(&args, T, T)?;
                let series_id = args.get("series_id").and_then(Value::as_str);
                let r = risk_analysis(&*self.store, asset_id, series_id)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                Ok(ToolOutput::Json(
                    serde_json::to_value(r).unwrap_or(Value::Null),
                ))
            }
            "correlate" => {
                let raw = args.get("events").cloned().unwrap_or_else(|| json!([]));
                let events: Vec<openrt_industrial::TimedEvent> = serde_json::from_value(raw)
                    .map_err(|e| {
                        ToolError::InvalidArguments(T.to_string(), format!("`events`: {e}"))
                    })?;
                let mut p = openrt_industrial::CorrelateParams::default();
                if let Some(v) = args.get("gap_ms").and_then(Value::as_i64) {
                    p.gap_ms = v;
                }
                if let Some(v) = args.get("min_members").and_then(Value::as_u64) {
                    p.min_members = v as usize;
                }
                let r = openrt_industrial::correlate(&events, p);
                Ok(ToolOutput::Json(
                    serde_json::to_value(r).unwrap_or(Value::Null),
                ))
            }
            "scenario" => {
                let asset_id = str_arg(&args, T, T)?;
                let event = args
                    .get("event")
                    .and_then(Value::as_str)
                    .unwrap_or("failure")
                    .to_string();
                let assumptions = args
                    .get("assumptions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let out = scenario_analysis(&*self.store, asset_id, &event, assumptions)
                    .await
                    .map_err(|e| ind_err(T, e))?;
                Ok(ToolOutput::Json(
                    serde_json::to_value(out).unwrap_or(Value::Null),
                ))
            }
            other => Err(ToolError::InvalidArguments(
                T.to_string(),
                format!("unknown analysis `{other}`"),
            )),
        }
    }
}

struct IndustrialCopilotTool {
    spec: ToolSpec,
    store: Arc<dyn PersistenceStore>,
}

impl IndustrialCopilotTool {
    fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self {
            spec: ToolSpec::new(
                "industrial.copilot",
                "Evidence-linked investigation answer for one industrial asset, assembled \
                 strictly from stored graph context and persisted analyses (no fabricated \
                 telemetry; advisory only).",
            )
            .with_caps(&["industrial.copilot.run"])
            .with_schema(json!({
                "type": "object",
                "properties": {"asset_id": {"type": "string"}},
                "required": ["asset_id"]
            })),
            store,
        }
    }
}

#[async_trait::async_trait]
impl Tool for IndustrialCopilotTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        const T: &str = "industrial.copilot";
        let asset_id = str_arg(&args, T, T)?;
        let out = investigate_asset(&*self.store, asset_id)
            .await
            .map_err(|e| ind_err(T, e))?;
        Ok(ToolOutput::Json(
            serde_json::to_value(out).unwrap_or(Value::Null),
        ))
    }
}

struct TradingDatasetTool {
    spec: ToolSpec,
    store: Arc<dyn PersistenceStore>,
}

impl TradingDatasetTool {
    fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self {
            spec: ToolSpec::new(
                "trading.dataset",
                "List, read or verify research datasets (content-addressed bars + config). \
                 Research only — no market feed, no broker, no execution.",
            )
            .with_caps(&["trading.datasets.read"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["list", "get", "bars", "verify"]},
                    "id": {"type": "string"}
                }
            })),
            store,
        }
    }
}

#[async_trait::async_trait]
impl Tool for TradingDatasetTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        const T: &str = "trading.dataset";
        let op = args.get("op").and_then(Value::as_str).unwrap_or("list");
        match op {
            "list" => {
                let ds = openrt_trading::list_datasets(&*self.store)
                    .await
                    .map_err(|e| trade_err(T, e))?;
                Ok(ToolOutput::Json(json!({"count": ds.len(), "datasets": ds})))
            }
            "get" => {
                let id = str_arg(&args, T, T)?;
                let ds = openrt_trading::get_dataset(&*self.store, id)
                    .await
                    .map_err(|e| trade_err(T, e))?;
                Ok(ToolOutput::Json(json!({"dataset": ds})))
            }
            "bars" => {
                let id = str_arg(&args, T, T)?;
                let bars = openrt_trading::get_bars(&*self.store, id)
                    .await
                    .map_err(|e| trade_err(T, e))?;
                Ok(ToolOutput::Json(json!({"bars": bars})))
            }
            "verify" => {
                let id = str_arg(&args, T, T)?;
                let ok = openrt_trading::verify_dataset(&*self.store, id)
                    .await
                    .map_err(|e| trade_err(T, e))?;
                Ok(ToolOutput::Json(json!({"dataset_id": id, "digest_ok": ok})))
            }
            other => Err(ToolError::InvalidArguments(
                T.to_string(),
                format!("unknown op `{other}` (list|get|bars|verify)"),
            )),
        }
    }
}

struct TradingStrategyTool {
    spec: ToolSpec,
    store: Arc<dyn PersistenceStore>,
}

impl TradingStrategyTool {
    fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self {
            spec: ToolSpec::new(
                "trading.strategy",
                "List or read declarative research strategies (causal condition DSL). \
                 Research only — strategies never reach a broker.",
            )
            .with_caps(&["trading.strategies.read"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["list", "get"]},
                    "id": {"type": "string"}
                }
            })),
            store,
        }
    }
}

#[async_trait::async_trait]
impl Tool for TradingStrategyTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        const T: &str = "trading.strategy";
        let op = args.get("op").and_then(Value::as_str).unwrap_or("list");
        match op {
            "list" => {
                let strategies = openrt_trading::list_strategies(&*self.store)
                    .await
                    .map_err(|e| trade_err(T, e))?;
                Ok(ToolOutput::Json(json!({
                    "count": strategies.len(),
                    "strategies": strategies,
                })))
            }
            "get" => {
                let id = str_arg(&args, T, T)?;
                let strategy = openrt_trading::get_strategy(&*self.store, id)
                    .await
                    .map_err(|e| trade_err(T, e))?;
                Ok(ToolOutput::Json(json!({"strategy": strategy})))
            }
            other => Err(ToolError::InvalidArguments(
                T.to_string(),
                format!("unknown op `{other}` (list|get)"),
            )),
        }
    }
}

struct TradingBacktestTool {
    spec: ToolSpec,
    store: Arc<dyn PersistenceStore>,
}

impl TradingBacktestTool {
    fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self {
            spec: ToolSpec::new(
                "trading.backtest",
                "Run a deterministic simulated backtest (or walk-forward folds) for a stored \
                 strategy over a stored dataset; results persist under trading.backtests. \
                 Research only — simulated fills, no execution.",
            )
            .with_caps(&["trading.backtest.run"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "strategy_id": {"type": "string"},
                    "dataset_id": {"type": "string"},
                    "initial_cash": {"type": "number"},
                    "walk_forward": {"type": "boolean"},
                    "train_frac": {"type": "number"},
                    "validation_frac": {"type": "number"}
                },
                "required": ["strategy_id", "dataset_id"]
            })),
            store,
        }
    }
}

#[async_trait::async_trait]
impl Tool for TradingBacktestTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        const T: &str = "trading.backtest";
        let strategy_id = str_arg(&args, T, T)?;
        let dataset_id = str_arg(&args, T, T)?;
        let initial_cash = args.get("initial_cash").and_then(Value::as_f64);
        if args
            .get("walk_forward")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let mut p = openrt_trading::WalkForwardParams::default();
            if let Some(v) = args.get("train_frac").and_then(Value::as_f64) {
                p.train_frac = v;
            }
            if let Some(v) = args.get("validation_frac").and_then(Value::as_f64) {
                p.validation_frac = v;
            }
            if let Some(v) = initial_cash {
                p.initial_cash = v;
            }
            let r = walk_forward_run(&*self.store, strategy_id, dataset_id, p)
                .await
                .map_err(|e| trade_err(T, e))?;
            return Ok(ToolOutput::Json(
                serde_json::to_value(r).unwrap_or(Value::Null),
            ));
        }
        let r = backtest_run(&*self.store, strategy_id, dataset_id, initial_cash)
            .await
            .map_err(|e| trade_err(T, e))?;
        Ok(ToolOutput::Json(
            serde_json::to_value(r).unwrap_or(Value::Null),
        ))
    }
}

struct LearnExperienceTool {
    spec: ToolSpec,
    store: Arc<dyn PersistenceStore>,
    operators: Vec<String>,
    sovereign: bool,
}

impl LearnExperienceTool {
    fn new(store: Arc<dyn PersistenceStore>, operators: Vec<String>, sovereign: bool) -> Self {
        Self {
            spec: ToolSpec::new(
                "learn.experience",
                "Capture, evaluate or list experience episodes (measured vs inferred metrics). \
                 Writes land in improve.episodes; self-improvement never changes permissions \
                 and never self-activates.",
            )
            .with_caps(&["learn.episodes.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["capture", "evaluate", "list"]},
                    "episode": {"type": "object"},
                    "id": {"type": "string"}
                },
                "required": ["op"]
            })),
            store,
            operators,
            sovereign,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LearnExperienceTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        const T: &str = "learn.experience";
        let op = str_arg(&args, T, T)?;
        match op {
            "capture" => {
                let raw = args.get("episode").cloned().unwrap_or(Value::Null);
                let mut episode: openrt_selfimprove::ExperienceEpisode =
                    serde_json::from_value(raw).map_err(|e| {
                        ToolError::InvalidArguments(
                            T.to_string(),
                            format!("`episode` invalid: {e}"),
                        )
                    })?;
                if episode.provenance.origin.trim().is_empty() {
                    episode.provenance = openrt_selfimprove::Provenance::new(
                        "tool.learn.experience",
                        "openrt-runtime",
                    );
                }
                if episode.created_at.trim().is_empty() {
                    episode.created_at = openrt_core::now().to_rfc3339();
                }
                let id = episode.id.clone();
                let mut improve =
                    tool_load_improve(&*self.store, &self.operators, self.sovereign).await?;
                improve
                    .record_episode(episode)
                    .map_err(|e| ToolError::Internal(T.to_string(), e.to_string()))?;
                improve
                    .save(&*self.store)
                    .await
                    .map_err(|e| ToolError::Internal(T.to_string(), e.to_string()))?;
                Ok(ToolOutput::Json(json!({"recorded": id})))
            }
            "evaluate" => {
                let id = str_arg(&args, T, T)?;
                let improve =
                    tool_load_improve(&*self.store, &self.operators, self.sovereign).await?;
                let eval = improve
                    .evaluate_episode(id)
                    .map_err(|e| ToolError::Internal(T.to_string(), e.to_string()))?;
                Ok(ToolOutput::Json(
                    serde_json::to_value(eval).unwrap_or(Value::Null),
                ))
            }
            "list" => {
                let improve =
                    tool_load_improve(&*self.store, &self.operators, self.sovereign).await?;
                let episodes = improve.list_episodes();
                Ok(ToolOutput::Json(json!({
                    "count": episodes.len(),
                    "episodes": episodes,
                })))
            }
            other => Err(ToolError::InvalidArguments(
                T.to_string(),
                format!("unknown op `{other}` (capture|evaluate|list)"),
            )),
        }
    }
}

/// The eight Phase 12 tools registered into the ONE registry.
pub(crate) fn phase12_tools(
    store: Arc<dyn PersistenceStore>,
    sovereign: bool,
    operators: Vec<String>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(IndustrialAssetsTool::new(store.clone())),
        Arc::new(IndustrialSeriesTool::new(store.clone())),
        Arc::new(IndustrialAnalysisTool::new(store.clone())),
        Arc::new(IndustrialCopilotTool::new(store.clone())),
        Arc::new(TradingDatasetTool::new(store.clone())),
        Arc::new(TradingStrategyTool::new(store.clone())),
        Arc::new(TradingBacktestTool::new(store.clone())),
        Arc::new(LearnExperienceTool::new(store, operators, sovereign)),
    ]
}

fn phase12_meta(tool: &str, risk: RiskLevel, effects: &[&str], rollback: &str) -> FabricMeta {
    FabricMeta::new(tool, ToolCategory::Utility, risk)
        .with_platform("host", SupportState::Green)
        .observe()
        .side_effects(effects)
        .rollback(rollback)
}

/// Fabric metadata for the eight Phase 12 tools (wiring test: every meta
/// must match a registered tool and vice versa).
pub(crate) fn phase12_metas() -> Vec<FabricMeta> {
    let deterministic =
        json!({"available": true, "backend": "deterministic", "source": "local store"});
    let metas = vec![
        phase12_meta(
            "industrial.assets",
            RiskLevel::Low,
            &["persists industrial asset/graph records"],
            "delete record (operator)",
        ),
        phase12_meta("industrial.series", RiskLevel::Low, &["read-only"], "none"),
        phase12_meta(
            "industrial.analysis",
            RiskLevel::Low,
            &["persists analysis records under industrial.analysis"],
            "delete analysis record",
        ),
        phase12_meta(
            "industrial.copilot",
            RiskLevel::Low,
            &["read-only (assembly over stored context)"],
            "none",
        ),
        phase12_meta(
            "trading.dataset",
            RiskLevel::Low,
            &["read-only over trading.datasets"],
            "none",
        ),
        phase12_meta(
            "trading.strategy",
            RiskLevel::Low,
            &["read-only over trading.strategies"],
            "none",
        ),
        phase12_meta(
            "trading.backtest",
            RiskLevel::Medium,
            &["persists simulated backtest records under trading.backtests"],
            "delete backtest record",
        ),
        phase12_meta(
            "learn.experience",
            RiskLevel::Medium,
            &["persists experience episodes in improve.episodes"],
            "episodes are sealed records (no delete API)",
        ),
    ];
    metas
        .into_iter()
        .map(|mut m| {
            m.runtime_status = Some(deterministic.clone());
            m
        })
        .collect()
}
