//! FMEA tables and heuristic predictive-maintenance risk. Tables are
//! deterministic reference knowledge; risk scores are explicitly labelled
//! heuristics and report INSUFFICIENT_DATA rather than inventing numbers.

use serde::{Deserialize, Serialize};

use crate::anomaly::AnomalyResult;
use crate::model::Asset;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureMode {
    pub equipment_type: String,
    pub mode: String,
    pub effects: Vec<String>,
    pub possible_causes: Vec<String>,
    pub indicators: Vec<String>,
}

fn pump_modes() -> Vec<FailureMode> {
    let t = "pump";
    vec![
        FailureMode {
            equipment_type: t.into(),
            mode: "bearing wear".into(),
            effects: vec![
                "rising vibration".into(),
                "temperature rise".into(),
                "eventual seizure".into(),
            ],
            possible_causes: vec![
                "contamination".into(),
                "misalignment".into(),
                "lubrication loss".into(),
            ],
            indicators: vec![
                "vibration above baseline".into(),
                "bearing temperature drift".into(),
                "noise".into(),
            ],
        },
        FailureMode {
            equipment_type: t.into(),
            mode: "impeller erosion".into(),
            effects: vec!["flow degradation".into(), "efficiency loss".into()],
            possible_causes: vec!["abrasive media".into(), "cavitation".into()],
            indicators: vec![
                "flow trending down at constant speed".into(),
                "discharge pressure drop".into(),
            ],
        },
        FailureMode {
            equipment_type: t.into(),
            mode: "seal leak".into(),
            effects: vec!["product loss".into(), "secondary containment risk".into()],
            possible_causes: vec!["dry running".into(), "chemical attack".into()],
            indicators: vec![
                "leak detection alarm".into(),
                "level deviation in seal pot".into(),
            ],
        },
        FailureMode {
            equipment_type: t.into(),
            mode: "cavitation".into(),
            effects: vec!["impeller pitting".into(), "noise surge".into()],
            possible_causes: vec!["low suction pressure".into(), "restricted suction".into()],
            indicators: vec!["suction pressure low".into(), "acoustic noise rise".into()],
        },
    ]
}

fn generic_modes(t: &str) -> Vec<FailureMode> {
    vec![FailureMode {
        equipment_type: t.into(),
        mode: "functional degradation".into(),
        effects: vec!["output outside expected band".into()],
        possible_causes: vec![
            "wear".into(),
            "upstream disturbance".into(),
            "maintenance gap".into(),
        ],
        indicators: vec![
            "repeated threshold breach".into(),
            "trend divergence from baseline".into(),
        ],
    }]
}

/// Reference FMEA table for an equipment type (falls back to generic).
pub fn fmea_table(equipment_type: &str) -> Vec<FailureMode> {
    match equipment_type {
        "pump" => pump_modes(),
        "compressor" => {
            let mut v = generic_modes("compressor");
            v[0].mode = "stage fouling".into();
            v[0].possible_causes = vec!["deposit build-up".into(), "filter bypass".into()];
            v
        }
        "valve" => {
            let mut v = generic_modes("valve");
            v[0].mode = "stem seizure".into();
            v[0].indicators = vec!["stroke time increase".into(), "position deviation".into()];
            v
        }
        "heat_exchanger" => {
            let mut v = generic_modes("heat_exchanger");
            v[0].mode = "fouling".into();
            v[0].indicators = vec!["approach temperature rise".into(), "ΔP increase".into()];
            v
        }
        "instrument" => {
            let mut v = generic_modes("instrument");
            v[0].mode = "calibration drift".into();
            v[0].indicators = vec!["repeated deviation vs reference".into()];
            v
        }
        _ => generic_modes("generic"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskBand {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskSignals {
    pub anomalies_recent: usize,
    pub anomalies_total: usize,
    pub alarms: usize,
    pub maintenance_age_days: Option<i64>,
    pub observations: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskResult {
    pub asset_id: String,
    pub series_id: String,
    pub score: Option<f64>,
    pub band: Option<RiskBand>,
    /// Always `heuristic` — no machine-learning model stands behind this.
    pub method: String,
    pub signals: RiskSignals,
    pub failing_modes: Vec<String>,
    pub remaining_life: Option<f64>,
    pub insufficient: Option<String>,
    pub computed_at: String,
}

/// Minimum observations before any score is emitted.
pub const RISK_MIN_OBSERVATIONS: usize = 3;

/// Deterministic heuristic risk in [0, 1]. Below `RISK_MIN_OBSERVATIONS`
/// observations the result is INSUFFICIENT_DATA (score `None`).
pub fn risk(
    asset: &Asset,
    series_id: &str,
    anomalies: &[AnomalyResult],
    signals: RiskSignals,
    observations: usize,
) -> RiskResult {
    let computed_at = openrt_core::now().to_rfc3339();
    let failing_modes: Vec<String> = {
        let mut modes: Vec<String> = Vec::new();
        for mode in fmea_table(asset.equipment_type()) {
            let hit = anomalies.iter().any(|a| {
                mode.indicators.iter().any(|ind| {
                    a.evidence
                        .join(" ")
                        .to_lowercase()
                        .contains(&ind.to_lowercase())
                })
            }) || (signals.alarms > 0
                && mode.indicators.iter().any(|i| i.contains("alarm")));
            if hit {
                modes.push(mode.mode);
            }
        }
        modes.sort();
        modes.dedup();
        modes
    };
    if observations < RISK_MIN_OBSERVATIONS {
        return RiskResult {
            asset_id: asset.id.clone(),
            series_id: series_id.to_string(),
            score: None,
            band: None,
            method: "heuristic".into(),
            signals,
            failing_modes,
            remaining_life: None,
            insufficient: Some(format!(
                "INSUFFICIENT_DATA: {observations} observations < {RISK_MIN_OBSERVATIONS} required"
            )),
            computed_at,
        };
    }
    let recent = (signals.anomalies_recent.min(10) as f64) * 0.05;
    let total = (signals.anomalies_total.min(20) as f64) * 0.02;
    let alarms = (signals.alarms.min(10) as f64) * 0.06;
    let maint = match signals.maintenance_age_days {
        None => 0.0,
        Some(age) => (age.min(365) as f64) / 365.0 * 0.15,
    };
    let mode_boost = (failing_modes.len().min(3) as f64) * 0.05;
    let score = (recent + total + alarms + maint + mode_boost).min(1.0);
    let band = if score >= 0.65 {
        RiskBand::High
    } else if score >= 0.35 {
        RiskBand::Medium
    } else {
        RiskBand::Low
    };
    RiskResult {
        asset_id: asset.id.clone(),
        series_id: series_id.to_string(),
        score: Some(score),
        band: Some(band),
        method: "heuristic".into(),
        signals,
        failing_modes,
        remaining_life: None,
        insufficient: Some("remaining_life UNAVAILABLE: no degradation model configured".into()),
        computed_at,
    }
}

pub fn series_id_of(asset: &Asset) -> String {
    asset
        .attr_str("series_id")
        .map(str::to_string)
        .unwrap_or_else(|| format!("sensor.{}", asset.id))
}
