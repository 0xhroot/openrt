//! Trading research data model: instruments, OHLCV bars, corporate and
//! economic events. Records only — no order/execution type exists in this
//! crate at all.

use serde::{Deserialize, Serialize};

/// Where a research record came from (mirrors the industrial pack).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub at: String,
}

impl Provenance {
    pub fn derived(source: impl Into<String>, at: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            detail: None,
            at: at.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstrumentKind {
    Equity,
    Index,
    Future,
    Fx,
    Crypto,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instrument {
    pub id: String,
    pub symbol: String,
    pub venue: String,
    pub kind: InstrumentKind,
    pub currency: String,
    pub tick_size: f64,
    pub provenance: Provenance,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub t_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

impl Bar {
    /// Bars must be internally consistent; out-of-order bars are the
    /// importer's problem (sort before building a dataset).
    pub fn validate(&self) -> Result<(), String> {
        if self.high < self.open.max(self.close) || self.low > self.open.min(self.close) {
            return Err(format!(
                "bar t_ms={} violates high/low envelope (o={} h={} l={} c={})",
                self.t_ms, self.open, self.high, self.low, self.close
            ));
        }
        if self.close <= 0.0 || self.open <= 0.0 {
            return Err(format!("bar t_ms={} has non-positive price", self.t_ms));
        }
        Ok(())
    }
}

/// Non-price research events (records only — never fabricated by engines).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchEvent {
    pub id: String,
    pub instrument: String,
    pub kind: String,
    pub t_ms: i64,
    pub headline: String,
    pub provenance: Provenance,
}
