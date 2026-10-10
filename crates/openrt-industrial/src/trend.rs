//! Deterministic trend primitives: least-squares slope, moving averages,
//! rate of change, baseline deviation and persistent-drift detection.
//! Every result records its parameters, window and computation time.

use serde::{Deserialize, Serialize};

use crate::series::{Point, Quality};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TrendDirection {
    Rising,
    Falling,
    Flat,
    Insufficient,
}

impl TrendDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrendDirection::Rising => "rising",
            TrendDirection::Falling => "falling",
            TrendDirection::Flat => "flat",
            TrendDirection::Insufficient => "insufficient",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrendParams {
    /// Minimum non-missing points required for a direction verdict.
    pub min_points: usize,
    /// |slope| at or below this counts as flat (value units per point).
    pub flat_slope_eps: f64,
    pub window: usize,
}

impl Default for TrendParams {
    fn default() -> Self {
        Self {
            min_points: 8,
            flat_slope_eps: 1e-9,
            window: 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Threshold {
    pub low: Option<f64>,
    pub high: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThresholdBreach {
    pub breached: bool,
    pub high_breach: bool,
    pub low_breach: bool,
    pub latest: Option<f64>,
    pub threshold: Threshold,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendResult {
    pub asset_id: String,
    pub series_id: String,
    pub direction: TrendDirection,
    pub slope_per_point: Option<f64>,
    pub points_used: usize,
    pub params: TrendParams,
    pub start_value: Option<f64>,
    pub end_value: Option<f64>,
    pub change_pct: Option<f64>,
    pub moving_avg: Option<f64>,
    pub rate_of_change: Option<f64>,
    pub threshold: Option<ThresholdBreach>,
    pub computed_at: String,
}

fn usable(points: &[Point]) -> Vec<(i64, f64)> {
    points
        .iter()
        .filter(|p| p.quality != Quality::Missing)
        .map(|p| (p.t_ms, p.value))
        .collect()
}

fn slope(samples: &[(i64, f64)]) -> Option<f64> {
    let n = samples.len() as f64;
    if samples.len() < 2 {
        return None;
    }
    let mean_x = samples.iter().map(|s| s.0 as f64).sum::<f64>() / n;
    let mean_y = samples.iter().map(|s| s.1).sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (x, y) in samples {
        let dx = *x as f64 - mean_x;
        num += dx * (y - mean_y);
        den += dx * dx;
    }
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

pub(crate) fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

pub fn std_dev(values: &[f64]) -> Option<f64> {
    let m = mean(values)?;
    if values.len() < 2 {
        return Some(0.0);
    }
    let var = values.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / values.len() as f64;
    Some(var.sqrt())
}

/// Trend over the last `params.window` usable points of a series.
pub fn trend(
    asset_id: &str,
    series_id: &str,
    points: &[Point],
    params: TrendParams,
    threshold: Option<Threshold>,
) -> TrendResult {
    let all = usable(points);
    let window_start = all.len().saturating_sub(params.window);
    let samples: Vec<(i64, f64)> = all[window_start..].to_vec();
    let computed_at = openrt_core::now().to_rfc3339();
    if samples.len() < params.min_points {
        return TrendResult {
            asset_id: asset_id.to_string(),
            series_id: series_id.to_string(),
            direction: TrendDirection::Insufficient,
            slope_per_point: None,
            points_used: samples.len(),
            params,
            start_value: None,
            end_value: None,
            change_pct: None,
            moving_avg: mean(&samples.iter().map(|s| s.1).collect::<Vec<_>>()),
            rate_of_change: None,
            threshold: None,
            computed_at,
        };
    }
    let raw_slope = slope(&samples);
    let direction = match raw_slope {
        None => TrendDirection::Flat,
        Some(s) if s > params.flat_slope_eps => TrendDirection::Rising,
        Some(s) if s < -params.flat_slope_eps => TrendDirection::Falling,
        Some(_) => TrendDirection::Flat,
    };
    let start = samples.first().map(|s| s.1);
    let end = samples.last().map(|s| s.1);
    let change_pct = match (start, end) {
        (Some(a), Some(b)) if a != 0.0 => Some((b - a) / a * 100.0),
        _ => None,
    };
    let values: Vec<f64> = samples.iter().map(|s| s.1).collect();
    let moving_avg = mean(&values);
    let rate_of_change = match (raw_slope, moving_avg) {
        (Some(s), Some(m)) if m != 0.0 => Some(s / m * 100.0),
        _ => None,
    };
    let threshold = threshold.map(|th| ThresholdBreach {
        breached: end.map_or(false, |v| {
            th.high.map_or(false, |h| v > h) || th.low.map_or(false, |l| v < l)
        }),
        high_breach: end.map_or(false, |v| th.high.map_or(false, |h| v > h)),
        low_breach: end.map_or(false, |v| th.low.map_or(false, |l| v < l)),
        latest: end,
        threshold: th,
    });
    TrendResult {
        asset_id: asset_id.to_string(),
        series_id: series_id.to_string(),
        direction,
        slope_per_point: raw_slope,
        points_used: samples.len(),
        params,
        start_value: start,
        end_value: end,
        change_pct,
        moving_avg,
        rate_of_change,
        threshold,
        computed_at,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BaselineDeviation {
    pub latest: f64,
    pub baseline_mean: f64,
    pub baseline_std: f64,
    pub z: Option<f64>,
    pub sigma_threshold: f64,
    pub outside_band: bool,
    pub baseline_points: usize,
}

/// Compare the latest usable value against a baseline window's mean/σ.
pub fn baseline_deviation(
    points: &[Point],
    baseline_points: usize,
    sigma_threshold: f64,
) -> Option<BaselineDeviation> {
    let all = usable(points);
    if all.len() < baseline_points + 1 {
        return None;
    }
    let (split, rest) = all.split_at(all.len() - 1);
    let base: Vec<f64> = split[split.len() - baseline_points..]
        .iter()
        .map(|s| s.1)
        .collect();
    let latest = rest[0].1;
    let m = mean(&base)?;
    let sd = std_dev(&base)?;
    let z = if sd > 0.0 {
        Some((latest - m) / sd)
    } else {
        None
    };
    Some(BaselineDeviation {
        latest,
        baseline_mean: m,
        baseline_std: sd,
        z,
        sigma_threshold,
        outside_band: z.map_or(false, |z| z.abs() > sigma_threshold),
        baseline_points: base.len(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DriftReport {
    pub persistent: bool,
    pub windows: usize,
    pub consistent_direction: Option<TrendDirection>,
}

/// Drift is persistent only when `windows` consecutive windows agree on a
/// non-flat direction.
pub fn persistent_drift(points: &[Point], params: TrendParams, windows: usize) -> DriftReport {
    let all = usable(points);
    if windows == 0 || all.len() < params.min_points * windows {
        return DriftReport {
            persistent: false,
            windows,
            consistent_direction: None,
        };
    }
    let chunk = all.len() / windows;
    let mut dirs = Vec::new();
    for i in 0..windows {
        let seg: Vec<Point> = all[i * chunk..(i + 1) * chunk]
            .iter()
            .map(|(t, v)| Point {
                t_ms: *t,
                value: *v,
                quality: Quality::Good,
                source: "derived".into(),
            })
            .collect();
        let r = trend(
            "",
            "",
            &seg,
            TrendParams {
                window: chunk,
                ..params
            },
            None,
        );
        dirs.push(r.direction);
    }
    let non_flat: Vec<TrendDirection> = dirs
        .iter()
        .copied()
        .filter(|d| !matches!(d, TrendDirection::Flat | TrendDirection::Insufficient))
        .collect();
    let consistent = if non_flat.len() == windows && non_flat.windows(2).all(|w| w[0] == w[1]) {
        non_flat.first().copied()
    } else {
        None
    };
    DriftReport {
        persistent: consistent.is_some(),
        windows,
        consistent_direction: consistent,
    }
}
