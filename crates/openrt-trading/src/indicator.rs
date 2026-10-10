//! Deterministic technical indicators over OHLCV bars. Every series is
//! `Option<f64>` where history is insufficient — no warm-up fabrication.

use serde::{Deserialize, Serialize};

use crate::model::Bar;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Indicator {
    Close,
    Sma { period: usize },
    Ema { period: usize },
    Momentum { period: usize },
    Volatility { period: usize },
    Volume { period: usize },
}

impl Indicator {
    pub fn label(&self) -> String {
        match self {
            Indicator::Close => "close".into(),
            Indicator::Sma { period } => format!("sma{period}"),
            Indicator::Ema { period } => format!("ema{period}"),
            Indicator::Momentum { period } => format!("mom{period}"),
            Indicator::Volatility { period } => format!("vol{period}"),
            Indicator::Volume { period } => format!("volume{period}"),
        }
    }
}

pub fn closes(bars: &[Bar]) -> Vec<f64> {
    bars.iter().map(|b| b.close).collect()
}

pub fn sma(bars: &[Bar], period: usize) -> Vec<Option<f64>> {
    assert!(period > 0, "period must be > 0");
    let c = closes(bars);
    let mut out = Vec::with_capacity(c.len());
    for i in 0..c.len() {
        if i + 1 >= period {
            let win = &c[i + 1 - period..=i];
            out.push(Some(win.iter().sum::<f64>() / period as f64));
        } else {
            out.push(None);
        }
    }
    out
}

pub fn ema(bars: &[Bar], period: usize) -> Vec<Option<f64>> {
    assert!(period > 0, "period must be > 0");
    let c = closes(bars);
    let mut out: Vec<Option<f64>> = vec![None; c.len()];
    if c.len() < period {
        return out;
    }
    let seed: f64 = c[..period].iter().sum::<f64>() / period as f64;
    out[period - 1] = Some(seed);
    let k = 2.0 / (period as f64 + 1.0);
    let mut prev = seed;
    for i in period..c.len() {
        prev = c[i] * k + prev * (1.0 - k);
        out[i] = Some(prev);
    }
    out
}

/// Close-to-close momentum: `close[i] / close[i-period] - 1`.
pub fn momentum(bars: &[Bar], period: usize) -> Vec<Option<f64>> {
    assert!(period > 0, "period must be > 0");
    let c = closes(bars);
    let mut out = Vec::with_capacity(c.len());
    for i in 0..c.len() {
        if i >= period && c[i - period] != 0.0 {
            out.push(Some(c[i] / c[i - period] - 1.0));
        } else {
            out.push(None);
        }
    }
    out
}

pub fn returns(bars: &[Bar]) -> Vec<Option<f64>> {
    let c = closes(bars);
    let mut out = Vec::with_capacity(c.len());
    for i in 0..c.len() {
        if i == 0 || c[i - 1] == 0.0 {
            out.push(None);
        } else {
            out.push(Some(c[i] / c[i - 1] - 1.0));
        }
    }
    out
}

/// Rolling standard deviation of close-to-close returns (population σ).
pub fn volatility(bars: &[Bar], period: usize) -> Vec<Option<f64>> {
    assert!(period > 0, "period must be > 0");
    let r = returns(bars);
    let mut out = Vec::with_capacity(r.len());
    for i in 0..r.len() {
        if i + 1 >= period {
            let win: Vec<f64> = r[i + 1 - period..=i].iter().copied().flatten().collect();
            if win.len() == period {
                let m = win.iter().sum::<f64>() / win.len() as f64;
                let var = win.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / win.len() as f64;
                out.push(Some(var.sqrt()));
            } else {
                out.push(None);
            }
        } else {
            out.push(None);
        }
    }
    out
}

pub fn rolling_volume(bars: &[Bar], period: usize) -> Vec<Option<f64>> {
    assert!(period > 0, "period must be > 0");
    let mut out = Vec::with_capacity(bars.len());
    for i in 0..bars.len() {
        if i + 1 >= period {
            let win = &bars[i + 1 - period..=i];
            let sum: f64 = win.iter().map(|b| b.volume).sum();
            out.push(Some(sum / period as f64));
        } else {
            out.push(None);
        }
    }
    out
}

/// Evaluate an indicator reference to its value series.
pub fn eval(ind: &Indicator, bars: &[Bar]) -> Vec<Option<f64>> {
    match *ind {
        Indicator::Close => bars.iter().map(|b| Some(b.close)).collect(),
        Indicator::Sma { period } => sma(bars, period),
        Indicator::Ema { period } => ema(bars, period),
        Indicator::Momentum { period } => momentum(bars, period),
        Indicator::Volatility { period } => volatility(bars, period),
        Indicator::Volume { period } => rolling_volume(bars, period),
    }
}

/// Max drawdown of an equity curve, as a fraction (0.2 = 20%).
pub fn max_drawdown(equity: &[f64]) -> f64 {
    let mut peak = f64::MIN;
    let mut max_dd = 0.0f64;
    for v in equity {
        peak = peak.max(*v);
        if peak > 0.0 {
            let dd = (peak - v) / peak;
            max_dd = max_dd.max(dd);
        }
    }
    max_dd
}
