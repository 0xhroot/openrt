//! Reproducible research datasets: inputs/source/transform/time range/
//! filters/params plus a content digest, stored in `trading.datasets`.
//! Rebuilding from the same inputs must yield the same digest.

use std::collections::BTreeMap;

use openrt_knowledge::safe_key;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::model::{Bar, Provenance};
use crate::{TradingError, DATASETS_NS};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatasetConfig {
    pub instrument: String,
    pub from_ms: i64,
    pub to_ms: i64,
    #[serde(default)]
    pub filters: BTreeMap<String, String>,
    #[serde(default)]
    pub params: BTreeMap<String, f64>,
    pub source: String,
    pub transform: String,
    /// Set by the importer when the source contains future-derived columns
    /// (e.g. next-day return). Drives leakage detection honestly.
    #[serde(default)]
    pub declared_lookahead: bool,
}

impl DatasetConfig {
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).expect("config serialises")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchDataset {
    pub id: String,
    pub version: String,
    pub config: DatasetConfig,
    pub inputs: Vec<String>,
    pub bar_count: usize,
    pub digest: String,
    pub provenance: Provenance,
    pub created_at: String,
}

fn dataset_key(id: &str) -> String {
    format!("dataset.{}", safe_key(id))
}

fn bars_key(id: &str) -> String {
    format!("bars.{}", safe_key(id))
}

fn canonical_digest(config: &DatasetConfig, bars: &[Bar]) -> String {
    let mut src = String::new();
    src.push_str(&config.canonical());
    src.push('\u{1f}');
    for b in bars {
        src.push_str(&format!(
            "{}|{}|{}|{}|{}|{};",
            b.t_ms, b.open, b.high, b.low, b.close, b.volume
        ));
    }
    openrt_core::sha256_hex(src.as_bytes())
}

fn parse<T: for<'de> Deserialize<'de>>(
    key: &str,
    value: serde_json::Value,
) -> Result<T, TradingError> {
    serde_json::from_value(value).map_err(|e| TradingError::Corrupt(key.to_string(), e.to_string()))
}

/// Build a dataset from bars (sorted ascending by `t_ms`, validated).
pub fn build_dataset(
    id: &str,
    version: &str,
    config: DatasetConfig,
    inputs: Vec<String>,
    bars: Vec<Bar>,
) -> Result<(ResearchDataset, Vec<Bar>), TradingError> {
    if id.trim().is_empty() {
        return Err(TradingError::Invalid("dataset id must not be empty".into()));
    }
    if bars.is_empty() {
        return Err(TradingError::Invalid("dataset requires >= 1 bar".into()));
    }
    let mut bars = bars;
    for b in &bars {
        b.validate().map_err(TradingError::Invalid)?;
    }
    bars.sort_by_key(|b| b.t_ms);
    for w in bars.windows(2) {
        if w[0].t_ms == w[1].t_ms {
            return Err(TradingError::Invalid(format!(
                "duplicate bar timestamp {} in dataset `{id}`",
                w[0].t_ms
            )));
        }
    }
    let digest = canonical_digest(&config, &bars);
    let now = openrt_core::now().to_rfc3339();
    let ds = ResearchDataset {
        id: id.to_string(),
        version: version.to_string(),
        config,
        inputs,
        bar_count: bars.len(),
        digest,
        provenance: Provenance::derived("trading.dataset.build", now.clone()),
        created_at: now,
    };
    Ok((ds, bars))
}

pub async fn put_dataset(
    store: &dyn PersistenceStore,
    ds: &ResearchDataset,
    bars: &[Bar],
) -> Result<(), TradingError> {
    let meta = serde_json::to_value(ds)
        .map_err(|e| TradingError::Serde(format!("serialize dataset: {e}")))?;
    store
        .put_record(DATASETS_NS, &dataset_key(&ds.id), &meta)
        .await?;
    let bars_v = serde_json::to_value(bars)
        .map_err(|e| TradingError::Serde(format!("serialize bars: {e}")))?;
    store
        .put_record(DATASETS_NS, &bars_key(&ds.id), &bars_v)
        .await?;
    Ok(())
}

pub async fn get_dataset(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Option<ResearchDataset>, TradingError> {
    match store.get_record(DATASETS_NS, &dataset_key(id)).await? {
        None => Ok(None),
        Some(v) => Ok(Some(parse(&dataset_key(id), v)?)),
    }
}

pub async fn get_bars(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Option<Vec<Bar>>, TradingError> {
    match store.get_record(DATASETS_NS, &bars_key(id)).await? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| TradingError::Corrupt(bars_key(id), e.to_string())),
    }
}

pub async fn list_datasets(
    store: &dyn PersistenceStore,
) -> Result<Vec<ResearchDataset>, TradingError> {
    let mut out: Vec<ResearchDataset> = Vec::new();
    for (key, value) in store.list_records(DATASETS_NS).await? {
        if !key.starts_with("dataset.") {
            continue;
        }
        out.push(parse(&key, value)?);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Reproducibility check: recompute the digest from stored bars + config.
pub async fn verify_dataset(store: &dyn PersistenceStore, id: &str) -> Result<bool, TradingError> {
    let ds = get_dataset(store, id)
        .await?
        .ok_or_else(|| TradingError::NotFound(format!("dataset `{id}`")))?;
    let bars = get_bars(store, id)
        .await?
        .ok_or_else(|| TradingError::NotFound(format!("bars for dataset `{id}`")))?;
    Ok(canonical_digest(&ds.config, &bars) == ds.digest)
}

/// Import bars from already-ingested CSV lines (`t_ms,o,h,l,c,v` header or
/// not). This only parses; ingestion of the file itself belongs to the one
/// ingest pipeline.
pub fn parse_bars_csv(text: &str) -> Result<Vec<Bar>, TradingError> {
    let mut bars = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty()
            || i == 0
                && line.contains(',')
                && !line
                    .chars()
                    .next()
                    .map_or(false, |c| c.is_ascii_digit() || c == '-')
        {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() < 6 {
            return Err(TradingError::Invalid(format!(
                "line {}: expected t_ms,o,h,l,c,v",
                i + 1
            )));
        }
        let parse = |s: &str, what: &str| -> Result<f64, TradingError> {
            s.trim()
                .parse::<f64>()
                .map_err(|_| TradingError::Invalid(format!("line {}: bad {what} `{s}`", i + 1)))
        };
        let t = parts[0]
            .trim()
            .parse::<i64>()
            .map_err(|_| TradingError::Invalid(format!("line {}: bad t_ms", i + 1)))?;
        bars.push(Bar {
            t_ms: t,
            open: parse(parts[1], "open")?,
            high: parse(parts[2], "high")?,
            low: parse(parts[3], "low")?,
            close: parse(parts[4], "close")?,
            volume: parse(parts[5], "volume")?,
        });
    }
    if bars.is_empty() {
        return Err(TradingError::Invalid("no bars parsed from CSV".into()));
    }
    Ok(bars)
}
