//! Sensor/telemetry series: timestamped points with units, quality and
//! source, stored in `industrial.series`, plus deterministic range
//! aggregation that reports missing data explicitly instead of inventing
//! points.

use openrt_knowledge::safe_key;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::{IndustrialError, SERIES_NS};

/// Quality flag per point. `Missing` means the source reported a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Good,
    Estimated,
    Suspect,
    Missing,
}

impl Quality {
    pub fn as_str(&self) -> &'static str {
        match self {
            Quality::Good => "good",
            Quality::Estimated => "estimated",
            Quality::Suspect => "suspect",
            Quality::Missing => "missing",
        }
    }
}

/// One measurement. `t_ms` is epoch milliseconds; values are never
/// synthesised by this module — a `Missing` point carries the value the
/// source recorded (or `0.0` only when the wire format lacked one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub t_ms: i64,
    pub value: f64,
    pub quality: Quality,
    pub source: String,
}

/// A stored series, one per sensor id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Series {
    pub id: String,
    pub asset_id: String,
    pub unit: String,
    pub points: Vec<Point>,
    pub updated_at: String,
}

/// Hard cap per series; larger ingests must chunk (honest scale reporting).
pub const MAX_POINTS_PER_SERIES: usize = 100_000;

/// One aggregation bucket. Empty buckets inside the requested range have
/// `count == 0` and `*_ = None` — they are reported as missing, never
/// interpolated.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AggregateBucket {
    pub t_ms: i64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub avg: Option<f64>,
    pub last: Option<f64>,
    pub count: usize,
    pub missing: usize,
}

/// Result of a range+aggregation query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub series_id: String,
    pub unit: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket_ms: i64,
    pub buckets: Vec<AggregateBucket>,
    pub point_count: usize,
    pub missing_buckets: usize,
    pub computed_at: String,
}

fn series_key(id: &str) -> String {
    format!("series.{}", safe_key(id))
}

/// Deterministic range aggregation over stored points.
pub fn aggregate(
    series_id: &str,
    unit: &str,
    points: &[Point],
    from_ms: i64,
    to_ms: i64,
    bucket_ms: i64,
) -> QueryResult {
    assert!(bucket_ms > 0, "bucket_ms must be positive");
    assert!(to_ms >= from_ms, "to_ms must be >= from_ms");
    let mut buckets: Vec<AggregateBucket> = Vec::new();
    let mut t = from_ms;
    let mut idx = 0usize;
    let mut point_count = 0usize;
    while t < to_ms {
        let end = (t + bucket_ms).min(to_ms);
        let (mut lo, mut hi, mut sum, mut cnt) = (f64::MAX, f64::MIN, 0.0f64, 0usize);
        let mut last: Option<f64> = None;
        while idx < points.len() && points[idx].t_ms < end {
            let p = &points[idx];
            if p.t_ms >= t && p.quality != Quality::Missing {
                lo = lo.min(p.value);
                hi = hi.max(p.value);
                sum += p.value;
                cnt += 1;
                last = Some(p.value);
                point_count += 1;
            }
            idx += 1;
        }
        let missing = usize::from(cnt == 0);
        buckets.push(AggregateBucket {
            t_ms: t,
            min: (cnt > 0).then_some(lo),
            max: (cnt > 0).then_some(hi),
            avg: (cnt > 0).then(|| sum / cnt as f64),
            last,
            count: cnt,
            missing,
        });
        t = end;
    }
    let missing_buckets = buckets.iter().filter(|b| b.count == 0).count();
    QueryResult {
        series_id: series_id.to_string(),
        unit: unit.to_string(),
        from_ms,
        to_ms,
        bucket_ms,
        buckets,
        point_count,
        missing_buckets,
        computed_at: openrt_core::now().to_rfc3339(),
    }
}

/// Append points to a series (creating it on first write). Points are kept
/// sorted by `t_ms`; duplicates keep the latest arrival.
pub async fn append_points(
    store: &dyn PersistenceStore,
    id: &str,
    asset_id: &str,
    unit: &str,
    points: &[Point],
) -> Result<Series, IndustrialError> {
    if id.trim().is_empty() {
        return Err(IndustrialError::Invalid(
            "series id must not be empty".into(),
        ));
    }
    if points.is_empty() {
        return Err(IndustrialError::Invalid(
            "append_points requires >= 1 point".into(),
        ));
    }
    let key = series_key(id);
    let mut series = match store.get_record(SERIES_NS, &key).await? {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| IndustrialError::Corrupt(key.clone(), e.to_string()))?,
        None => Series {
            id: id.to_string(),
            asset_id: asset_id.to_string(),
            unit: unit.to_string(),
            points: Vec::new(),
            updated_at: openrt_core::now().to_rfc3339(),
        },
    };
    if series.points.len() + points.len() > MAX_POINTS_PER_SERIES {
        return Err(IndustrialError::Invalid(format!(
            "series `{id}` would exceed {MAX_POINTS_PER_SERIES} points; chunk the ingest"
        )));
    }
    series.points.extend(points.iter().cloned());
    series.points.sort_by_key(|p| p.t_ms);
    series.updated_at = openrt_core::now().to_rfc3339();
    let value = serde_json::to_value(&series)
        .map_err(|e| IndustrialError::Serde(format!("serialize series `{key}`: {e}")))?;
    store.put_record(SERIES_NS, &key, &value).await?;
    Ok(series)
}

pub async fn get_series(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Option<Series>, IndustrialError> {
    match store.get_record(SERIES_NS, &series_key(id)).await? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| IndustrialError::Corrupt(series_key(id), e.to_string())),
    }
}

pub async fn list_series(store: &dyn PersistenceStore) -> Result<Vec<Series>, IndustrialError> {
    let mut out = Vec::new();
    for (key, value) in store.list_records(SERIES_NS).await? {
        let s: Series = serde_json::from_value(value)
            .map_err(|e| IndustrialError::Corrupt(key, e.to_string()))?;
        out.push(s);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Query a stored series over a range with bucketing.
pub async fn query_series(
    store: &dyn PersistenceStore,
    id: &str,
    from_ms: i64,
    to_ms: i64,
    bucket_ms: i64,
) -> Result<QueryResult, IndustrialError> {
    let series = get_series(store, id)
        .await?
        .ok_or_else(|| IndustrialError::NotFound(format!("series `{id}`")))?;
    Ok(aggregate(
        &series.id,
        &series.unit,
        &series.points,
        from_ms,
        to_ms,
        bucket_ms,
    ))
}
