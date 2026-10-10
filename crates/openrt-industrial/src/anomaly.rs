//! Anomaly detectors: statistical (z-score), rule (thresholds),
//! time-series (moving-average deviation) and model-assisted (only when a
//! configured model exists — otherwise reported UNAVAILABLE, never faked).
//! Every result records which method ran, with what parameters.

use serde::{Deserialize, Serialize};

use crate::series::{Point, Quality};
use crate::trend::{mean, std_dev};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorMethod {
    Statistical,
    Rule,
    TimeSeries,
    ModelAssisted,
}

impl DetectorMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            DetectorMethod::Statistical => "statistical",
            DetectorMethod::Rule => "rule",
            DetectorMethod::TimeSeries => "time_series",
            DetectorMethod::ModelAssisted => "model_assisted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpectedBand {
    pub low: f64,
    pub high: f64,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnomalyResult {
    pub detector_id: String,
    pub method: DetectorMethod,
    pub asset_id: String,
    pub series_id: String,
    pub at_ms: i64,
    pub value: f64,
    pub expected: ExpectedBand,
    pub severity: Severity,
    pub evidence: Vec<String>,
    pub computed_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectorRun {
    pub asset_id: String,
    pub series_id: String,
    pub method: DetectorMethod,
    pub params: serde_json::Value,
    pub anomalies: Vec<AnomalyResult>,
    /// `unavailable` is set when a configured method could not run honestly
    /// (e.g. model-assisted without a model backend).
    pub unavailable: Option<String>,
    pub computed_at: String,
}

fn usable(points: &[Point]) -> Vec<(i64, f64)> {
    points
        .iter()
        .filter(|p| p.quality != Quality::Missing)
        .map(|p| (p.t_ms, p.value))
        .collect()
}

fn severity_from_z(z: f64) -> Severity {
    let a = z.abs();
    if a >= 5.0 {
        Severity::High
    } else if a >= 3.5 {
        Severity::Medium
    } else {
        Severity::Low
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StatisticalParams {
    pub window: usize,
    pub min_samples: usize,
    pub z_threshold: f64,
    pub max_results: usize,
}

impl Default for StatisticalParams {
    fn default() -> Self {
        Self {
            window: 20,
            min_samples: 8,
            z_threshold: 3.0,
            max_results: 50,
        }
    }
}

/// Rolling z-score over the last `window` usable points of each step.
pub fn statistical(
    asset_id: &str,
    series_id: &str,
    points: &[Point],
    params: StatisticalParams,
) -> DetectorRun {
    let samples = usable(points);
    let computed_at = openrt_core::now().to_rfc3339();
    let mut anomalies = Vec::new();
    if samples.len() >= params.min_samples {
        let mut i = params.window.min(samples.len());
        while i < samples.len() {
            let hist: Vec<f64> = samples[i - params.window..i].iter().map(|s| s.1).collect();
            let (t, v) = samples[i];
            if let (Some(m), Some(sd)) = (mean(&hist), std_dev(&hist)) {
                if sd > 0.0 {
                    let z = (v - m) / sd;
                    if z.abs() > params.z_threshold {
                        anomalies.push(AnomalyResult {
                            detector_id: format!("stat.{series_id}"),
                            method: DetectorMethod::Statistical,
                            asset_id: asset_id.to_string(),
                            series_id: series_id.to_string(),
                            at_ms: t,
                            value: v,
                            expected: ExpectedBand {
                                low: m - params.z_threshold * sd,
                                high: m + params.z_threshold * sd,
                                basis: format!(
                                    "mean {m:.4} ± {z:.1}σ over last {} points (σ={sd:.4})",
                                    params.window
                                ),
                            },
                            severity: severity_from_z(z),
                            evidence: vec![format!("z={z:.2} at t_ms={t}")],
                            computed_at: computed_at.clone(),
                        });
                        if anomalies.len() >= params.max_results {
                            break;
                        }
                    }
                }
            }
            i += 1;
        }
    }
    DetectorRun {
        asset_id: asset_id.to_string(),
        series_id: series_id.to_string(),
        method: DetectorMethod::Statistical,
        params: serde_json::json!({"window": params.window, "min_samples": params.min_samples, "z_threshold": params.z_threshold}),
        anomalies,
        unavailable: None,
        computed_at,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RuleParams {
    pub low: f64,
    pub high: f64,
    pub max_results: usize,
}

/// Hard threshold rule: value outside [low, high] is anomalous.
pub fn rule(asset_id: &str, series_id: &str, points: &[Point], params: RuleParams) -> DetectorRun {
    let samples = usable(points);
    let computed_at = openrt_core::now().to_rfc3339();
    let mut anomalies = Vec::new();
    for (t, v) in &samples {
        let (low_bad, high_bad) = (*v < params.low, *v > params.high);
        if low_bad || high_bad {
            let margin = if high_bad {
                *v - params.high
            } else {
                params.low - *v
            };
            anomalies.push(AnomalyResult {
                detector_id: format!("rule.{series_id}"),
                method: DetectorMethod::Rule,
                asset_id: asset_id.to_string(),
                series_id: series_id.to_string(),
                at_ms: *t,
                value: *v,
                expected: ExpectedBand {
                    low: params.low,
                    high: params.high,
                    basis: "configured operating envelope".into(),
                },
                severity: if margin > (params.high - params.low).abs() * 0.25 {
                    Severity::High
                } else {
                    Severity::Medium
                },
                evidence: vec![format!(
                    "{} at t_ms={t}",
                    if low_bad {
                        "below low limit"
                    } else {
                        "above high limit"
                    }
                )],
                computed_at: computed_at.clone(),
            });
            if anomalies.len() >= params.max_results {
                break;
            }
        }
    }
    DetectorRun {
        asset_id: asset_id.to_string(),
        series_id: series_id.to_string(),
        method: DetectorMethod::Rule,
        params: serde_json::json!({"low": params.low, "high": params.high}),
        anomalies,
        unavailable: None,
        computed_at,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TimeSeriesParams {
    pub window: usize,
    pub min_samples: usize,
    pub deviation_sigma: f64,
    pub max_results: usize,
}

impl Default for TimeSeriesParams {
    fn default() -> Self {
        Self {
            window: 12,
            min_samples: 8,
            deviation_sigma: 3.0,
            max_results: 50,
        }
    }
}

/// Deviation of a point from the moving average of the preceding window,
/// scaled by that window's standard deviation.
pub fn time_series(
    asset_id: &str,
    series_id: &str,
    points: &[Point],
    params: TimeSeriesParams,
) -> DetectorRun {
    let samples = usable(points);
    let computed_at = openrt_core::now().to_rfc3339();
    let mut anomalies = Vec::new();
    if samples.len() >= params.min_samples {
        let mut i = params.window.min(samples.len());
        while i < samples.len() {
            let hist: Vec<f64> = samples[i - params.window..i].iter().map(|s| s.1).collect();
            let (t, v) = samples[i];
            if let (Some(m), Some(sd)) = (mean(&hist), std_dev(&hist)) {
                let band = params.deviation_sigma * sd.max(f64::EPSILON);
                if (v - m).abs() > band {
                    let z = (v - m) / sd.max(f64::EPSILON);
                    anomalies.push(AnomalyResult {
                        detector_id: format!("ts.{series_id}"),
                        method: DetectorMethod::TimeSeries,
                        asset_id: asset_id.to_string(),
                        series_id: series_id.to_string(),
                        at_ms: t,
                        value: v,
                        expected: ExpectedBand {
                            low: m - band,
                            high: m + band,
                            basis: format!(
                                "SMA{}/σ band over preceding {} points",
                                params.window, params.window
                            ),
                        },
                        severity: severity_from_z(z),
                        evidence: vec![format!("sma={m:.4}, dev={:.4} at t_ms={t}", v - m)],
                        computed_at: computed_at.clone(),
                    });
                    if anomalies.len() >= params.max_results {
                        break;
                    }
                }
            }
            i += 1;
        }
    }
    DetectorRun {
        asset_id: asset_id.to_string(),
        series_id: series_id.to_string(),
        method: DetectorMethod::TimeSeries,
        params: serde_json::json!({"window": params.window, "min_samples": params.min_samples, "deviation_sigma": params.deviation_sigma}),
        anomalies,
        unavailable: None,
        computed_at,
    }
}

/// Optional model backend for model-assisted detection. No model backend is
/// configured in this build; callers get an honest UNAVAILABLE run instead
/// of heuristic output mislabelled as ML.
pub trait AnomalyModel: Send + Sync {
    fn score(&self, series_id: &str, samples: &[(i64, f64)]) -> Option<f64>;
    fn model_id(&self) -> &str;
}

pub fn model_assisted(
    asset_id: &str,
    series_id: &str,
    points: &[Point],
    model: Option<&dyn AnomalyModel>,
    z_threshold: f64,
) -> DetectorRun {
    let computed_at = openrt_core::now().to_rfc3339();
    match model {
        None => DetectorRun {
            asset_id: asset_id.to_string(),
            series_id: series_id.to_string(),
            method: DetectorMethod::ModelAssisted,
            params: serde_json::json!({"z_threshold": z_threshold}),
            anomalies: Vec::new(),
            unavailable: Some(
                "model-assisted detection UNAVAILABLE: no anomaly model backend is configured"
                    .into(),
            ),
            computed_at,
        },
        Some(m) => {
            let samples = usable(points);
            let mut anomalies = Vec::new();
            if let Some(score) = m.score(series_id, &samples) {
                if score.abs() > z_threshold {
                    let (t, v) = *samples.last().expect("usable samples checked by caller");
                    anomalies.push(AnomalyResult {
                        detector_id: format!("model.{}.{}", m.model_id(), series_id),
                        method: DetectorMethod::ModelAssisted,
                        asset_id: asset_id.to_string(),
                        series_id: series_id.to_string(),
                        at_ms: t,
                        value: v,
                        expected: ExpectedBand {
                            low: -z_threshold,
                            high: z_threshold,
                            basis: format!("model `{}` score band", m.model_id()),
                        },
                        severity: severity_from_z(score),
                        evidence: vec![format!("model score {score:.4}")],
                        computed_at: computed_at.clone(),
                    });
                }
            }
            DetectorRun {
                asset_id: asset_id.to_string(),
                series_id: series_id.to_string(),
                method: DetectorMethod::ModelAssisted,
                params: serde_json::json!({"model": m.model_id(), "z_threshold": z_threshold}),
                anomalies,
                unavailable: None,
                computed_at,
            }
        }
    }
}
