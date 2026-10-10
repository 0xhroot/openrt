//! Declarative, causal strategies: conditions only read indicator history
//! up to the bar being evaluated — the DSL has no future-data reference, so
//! look-ahead is impossible by construction (leakage detection in
//! `backtest` audits the dataset side too).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::indicator::{eval, Indicator};
use crate::model::Bar;
use crate::Provenance;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Condition {
    Crossover { fast: Indicator, slow: Indicator },
    CrossUnder { fast: Indicator, slow: Indicator },
    Above { indicator: Indicator, value: f64 },
    Below { indicator: Indicator, value: f64 },
    Rising { indicator: Indicator, window: usize },
    Falling { indicator: Indicator, window: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskSpec {
    pub max_position_frac: f64,
    pub long_only: bool,
}

impl Default for RiskSpec {
    fn default() -> Self {
        Self {
            max_position_frac: 1.0,
            long_only: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostSpec {
    pub commission_bps: f64,
    pub slippage_bps: f64,
}

impl Default for CostSpec {
    fn default() -> Self {
        Self {
            commission_bps: 1.0,
            slippage_bps: 2.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Strategy {
    pub id: String,
    pub name: String,
    pub universe: Vec<String>,
    pub entry: Condition,
    pub exit: Condition,
    pub risk: RiskSpec,
    pub costs: CostSpec,
    #[serde(default)]
    pub params: BTreeMap<String, f64>,
    /// Set by the author when the strategy consumes a future-derived column.
    #[serde(default)]
    pub declared_lookahead: bool,
    pub provenance: Provenance,
    pub created_at: String,
}

impl Strategy {
    pub fn digest(&self) -> String {
        let json = serde_json::to_string(self).expect("strategy serialises");
        openrt_core::sha256_hex(json.as_bytes())
    }
}

fn prev_some(series: &[Option<f64>], i: usize) -> Option<f64> {
    if i == 0 {
        None
    } else {
        series[i - 1]
    }
}

/// Evaluate a condition at bar `i` using only data `<= i`.
pub fn eval_condition(
    cond: &Condition,
    _bars: &[Bar],
    series: &[(Indicator, Vec<Option<f64>>)],
    i: usize,
) -> bool {
    let lookup = |ind: &Indicator| -> &[Option<f64>] {
        series
            .iter()
            .find(|(k, _)| k == ind)
            .map(|(_, v)| v.as_slice())
            .expect("indicator precomputed")
    };
    match *cond {
        Condition::Crossover { fast, slow } => {
            let (f, s) = (lookup(&fast), lookup(&slow));
            match (prev_some(f, i), prev_some(s, i), f[i], s[i]) {
                (Some(fp), Some(sp), Some(fv), Some(sv)) => fp <= sp && fv > sv,
                _ => false,
            }
        }
        Condition::CrossUnder { fast, slow } => {
            let (f, s) = (lookup(&fast), lookup(&slow));
            match (prev_some(f, i), prev_some(s, i), f[i], s[i]) {
                (Some(fp), Some(sp), Some(fv), Some(sv)) => fp >= sp && fv < sv,
                _ => false,
            }
        }
        Condition::Above { indicator, value } => lookup(&indicator)[i].map_or(false, |v| v > value),
        Condition::Below { indicator, value } => lookup(&indicator)[i].map_or(false, |v| v < value),
        Condition::Rising { indicator, window } => {
            let s = lookup(&indicator);
            if window == 0 || i < window {
                return false;
            }
            match (s[i], s[i - window]) {
                (Some(cur), Some(prev)) => cur > prev,
                _ => false,
            }
        }
        Condition::Falling { indicator, window } => {
            let s = lookup(&indicator);
            if window == 0 || i < window {
                return false;
            }
            match (s[i], s[i - window]) {
                (Some(cur), Some(prev)) => cur < prev,
                _ => false,
            }
        }
    }
}

/// Indicators referenced by a strategy (deduplicated, deterministic order).
pub fn referenced_indicators(strategy: &Strategy) -> Vec<Indicator> {
    fn collect(cond: &Condition, out: &mut Vec<Indicator>) {
        match *cond {
            Condition::Crossover { fast, slow } | Condition::CrossUnder { fast, slow } => {
                out.push(fast);
                out.push(slow);
            }
            Condition::Above { indicator, .. }
            | Condition::Below { indicator, .. }
            | Condition::Rising { indicator, .. }
            | Condition::Falling { indicator, .. } => out.push(indicator),
        }
    }
    let mut out = Vec::new();
    collect(&strategy.entry, &mut out);
    collect(&strategy.exit, &mut out);
    out.sort_by_key(|i| i.label());
    out.dedup();
    out
}

/// Precompute all indicator series a strategy needs.
pub fn precompute(strategy: &Strategy, bars: &[Bar]) -> Vec<(Indicator, Vec<Option<f64>>)> {
    referenced_indicators(strategy)
        .into_iter()
        .map(|ind| {
            let series = eval(&ind, bars);
            (ind, series)
        })
        .collect()
}
