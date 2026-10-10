//! `openrt-industrial` — advisory industrial intelligence (Phase 12, M12.1–M12.7).
//!
//! Domain model, asset graph, telemetry analysis, trend/anomaly detection,
//! bounded RCA, FMEA, heuristic risk, temporal correlation, what-if
//! scenarios and evidence-linked investigation — all read/write through the
//! one `PersistenceStore`, relations mirrored into `knowledge.graph`.
//!
//! Advisory only: no physical-control tool, no safety bypass, no fabricated
//! telemetry. Detector methods are always reported as run.

use thiserror::Error;

pub mod anomaly;
pub mod copilot;
pub mod fmea;
pub mod model;
pub mod rca;
pub mod scenario;
pub mod series;
pub mod store;
pub mod trend;

pub const ASSETS_NS: &str = "industrial.assets";
pub const SERIES_NS: &str = "industrial.series";
pub const ANALYSIS_NS: &str = "industrial.analysis";

#[derive(Debug, Error)]
pub enum IndustrialError {
    #[error("store error: {0}")]
    Store(#[from] openrt_persistence::PersistenceError),
    #[error("graph error: {0}")]
    Graph(#[from] openrt_evidence::EvidenceError),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("corrupt record `{0}`: {1}")]
    Corrupt(String, String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}

pub use anomaly::{
    model_assisted, rule, statistical, time_series, AnomalyModel, AnomalyResult, DetectorMethod,
    DetectorRun, RuleParams, StatisticalParams, TimeSeriesParams,
};
pub use copilot::{investigate, AnswerSection, EvidenceRef, Investigation, RelatedBundle};
pub use fmea::{fmea_table, risk, FailureMode, RiskBand, RiskResult, RiskSignals};
pub use model::{Asset, AssetFilter, AssetKind, Provenance, Relation, REL_PREDICATES};
pub use rca::{root_cause, EvidenceLabel, Finding, RcaBounds, RcaContext, RcaReport};
pub use scenario::{
    correlate, scenario, CorrelateParams, CorrelationResult, EventKind, ScenarioInput,
    ScenarioResult, Statement, StatementKind, TimedEvent,
};
pub use series::{
    aggregate, append_points, get_series, list_series, query_series, AggregateBucket, Point,
    Quality, QueryResult, Series, MAX_POINTS_PER_SERIES,
};
pub use store::{
    add_relation, get_asset, list_analysis, list_assets, put_analysis, relations_for, upsert_asset,
};
pub use trend::{
    baseline_deviation, persistent_drift, trend, Threshold, TrendDirection, TrendParams,
    TrendResult,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn points(vals: &[f64]) -> Vec<Point> {
        vals.iter()
            .enumerate()
            .map(|(i, v)| Point {
                t_ms: i as i64 * 1000,
                value: *v,
                quality: Quality::Good,
                source: "test".into(),
            })
            .collect()
    }

    #[test]
    fn trend_detects_rising_and_falling() {
        let rising = trend(
            "a",
            "s",
            &points(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]),
            TrendParams::default(),
            None,
        );
        assert_eq!(rising.direction, TrendDirection::Rising);
        let falling = trend(
            "a",
            "s",
            &points(&[8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0]),
            TrendParams::default(),
            None,
        );
        assert_eq!(falling.direction, TrendDirection::Falling);
    }

    #[test]
    fn trend_insufficient_data_is_labelled() {
        let r = trend("a", "s", &points(&[1.0, 2.0]), TrendParams::default(), None);
        assert_eq!(r.direction, TrendDirection::Insufficient);
        assert!(r.slope_per_point.is_none());
    }

    #[test]
    fn threshold_breach_reported() {
        let r = trend(
            "a",
            "s",
            &points(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 10.0]),
            TrendParams::default(),
            Some(Threshold {
                low: Some(0.0),
                high: Some(8.0),
            }),
        );
        let th = r.threshold.expect("threshold present");
        assert!(th.breached && th.high_breach && !th.low_breach);
    }

    #[test]
    fn aggregate_reports_missing_buckets() {
        let mut pts = points(&[1.0, 3.0]);
        pts.push(Point {
            t_ms: 100_000,
            value: 5.0,
            quality: Quality::Good,
            source: "test".into(),
        });
        let q = aggregate("s", "bar", &pts, 0, 30_000, 10_000);
        assert_eq!(q.buckets.len(), 3);
        assert_eq!(q.buckets[0].avg, Some(2.0));
        assert!(q.buckets[1].count == 0 && q.buckets[1].avg.is_none());
        assert_eq!(q.missing_buckets, 2);
    }

    #[test]
    fn statistical_detector_flags_z_outlier() {
        let mut vals: Vec<f64> = (0..20).map(|i| 10.0 + (i % 2) as f64 * 0.1).collect();
        vals.push(40.0);
        let run = statistical("a", "s", &points(&vals), StatisticalParams::default());
        assert!(!run.anomalies.is_empty());
        assert_eq!(run.anomalies[0].method, DetectorMethod::Statistical);
        assert!(run.unavailable.is_none());
    }

    #[test]
    fn model_assisted_without_model_is_unavailable() {
        let run = model_assisted("a", "s", &points(&[1.0; 16]), None, 3.0);
        assert!(run.anomalies.is_empty());
        assert!(run
            .unavailable
            .as_deref()
            .unwrap_or("")
            .contains("UNAVAILABLE"));
    }

    #[test]
    fn rule_detector_enforces_envelope() {
        let run = rule(
            "a",
            "s",
            &points(&[1.0, 50.0, -20.0, 5.0]),
            RuleParams {
                low: 0.0,
                high: 20.0,
                max_results: 10,
            },
        );
        assert_eq!(run.anomalies.len(), 2);
    }

    #[test]
    fn rca_labels_findings_epistemically() {
        let alarm = {
            let mut a = Asset::new(
                "alm.1",
                AssetKind::Alarm,
                "high temp",
                Provenance::derived("test", "2026-01-01T00:00:00Z"),
            );
            a.attrs.insert("severity".into(), serde_json::json!("high"));
            a.attrs.insert(
                "first_seen".into(),
                serde_json::json!("2026-01-01T00:00:00Z"),
            );
            a
        };
        let ctx = RcaContext {
            alarms: vec![alarm],
            fmea: fmea_table("pump"),
            ..RcaContext::default()
        };
        let report = root_cause("pump.1", &ctx);
        assert!(!report.findings.is_empty());
        assert!(report
            .findings
            .iter()
            .any(|f| f.label == EvidenceLabel::ObservedFact));
        assert!(report
            .findings
            .iter()
            .any(|f| f.label == EvidenceLabel::Hypothesis));
        assert!(report.findings.len() <= RcaBounds::default().max_findings);
        assert_eq!(report.inputs.alarms, 1);
    }

    #[test]
    fn rca_without_observations_reports_unknown() {
        let report = root_cause("x", &RcaContext::default());
        assert!(report
            .findings
            .iter()
            .any(|f| f.label == EvidenceLabel::Unknown));
    }

    #[test]
    fn correlate_groups_temporal_neighbours() {
        let events = vec![
            TimedEvent {
                kind: EventKind::Alarm,
                id: "a1".into(),
                at_ms: 1000,
                detail: String::new(),
            },
            TimedEvent {
                kind: EventKind::Incident,
                id: "i1".into(),
                at_ms: 2000,
                detail: String::new(),
            },
            TimedEvent {
                kind: EventKind::Alarm,
                id: "a2".into(),
                at_ms: 9_999_999_999,
                detail: String::new(),
            },
        ];
        let r = correlate(&events, CorrelateParams::default());
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].members.len(), 2);
        assert!(r.caveat.contains("does not establish causation"));
    }

    #[test]
    fn scenario_splits_known_simulated_assumed() {
        let asset = Asset::new(
            "p.1",
            AssetKind::Equipment,
            "pump",
            Provenance::derived("test", "2026-01-01T00:00:00Z"),
        )
        .with_attr("equipment_type", "pump");
        let input = ScenarioInput {
            asset,
            event: "failure".into(),
            assumptions: vec!["spare part in stock".into()],
            edges: vec![("p.1".into(), "belongs_to".into(), "u.1".into())],
        };
        let out = scenario(&input);
        assert!(out.simulated && out.advisory_only);
        assert!(out
            .statements
            .iter()
            .any(|s| s.kind == StatementKind::Known));
        assert!(out
            .statements
            .iter()
            .any(|s| s.kind == StatementKind::Simulated));
        assert!(out
            .statements
            .iter()
            .any(|s| s.kind == StatementKind::Assumed));
    }

    #[test]
    fn risk_below_min_observations_is_insufficient() {
        let asset = Asset::new(
            "p.1",
            AssetKind::Equipment,
            "pump",
            Provenance::derived("test", "2026-01-01T00:00:00Z"),
        );
        let sig = RiskSignals {
            anomalies_recent: 2,
            anomalies_total: 2,
            alarms: 1,
            maintenance_age_days: Some(100),
            observations: 2,
        };
        let r = risk(&asset, "s", &[], sig, 2);
        assert!(r.score.is_none());
        assert!(r
            .insufficient
            .as_deref()
            .unwrap_or("")
            .contains("INSUFFICIENT_DATA"));
    }

    #[test]
    fn risk_with_observations_returns_heuristic_score() {
        let asset = Asset::new(
            "p.1",
            AssetKind::Equipment,
            "pump",
            Provenance::derived("test", "2026-01-01T00:00:00Z"),
        );
        let sig = RiskSignals {
            anomalies_recent: 5,
            anomalies_total: 8,
            alarms: 3,
            maintenance_age_days: Some(300),
            observations: 40,
        };
        let r = risk(&asset, "s", &[], sig, 40);
        let score = r.score.expect("score");
        assert!((0.0..=1.0).contains(&score));
        assert_eq!(r.method, "heuristic");
        assert!(r.remaining_life.is_none());
    }

    #[test]
    fn persistent_drift_requires_consistent_windows() {
        let vals: Vec<f64> = (0..64).map(|i| 1.0 + i as f64 * 0.5).collect();
        let d = persistent_drift(&points(&vals), TrendParams::default(), 4);
        assert!(d.persistent);
        assert_eq!(d.consistent_direction, Some(TrendDirection::Rising));
    }
}
