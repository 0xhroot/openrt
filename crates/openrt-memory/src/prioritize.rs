//! Prioritized retrieval (M12.20): deterministic re-ranking of recall hits
//! blending retrieval rank with recency of the recorded fact. Inputs are
//! measured (rank order + `occurred_at` timestamps); the blend weights and
//! method are recorded on every result. Predictive prefetch is NOT
//! implemented — callers get honest UNAVAILABLE from the runtime surface,
//! never a fabricated prefetch.

use serde::{Deserialize, Serialize};

use crate::semantic::SemanticItem;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriorityParams {
    /// Weight of the recency component in [0, 1]; rank weight is `1 - w`.
    pub recency_weight: f64,
    /// Half-life for recency decay, in days.
    pub half_life_days: f64,
    /// Reference instant for recency (epoch ms); results are deterministic
    /// for a fixed `now_ms`.
    pub now_ms: i64,
}

impl Default for PriorityParams {
    fn default() -> Self {
        Self {
            recency_weight: 0.35,
            half_life_days: 30.0,
            now_ms: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrioritizedHit {
    pub id: String,
    pub key: String,
    pub base_rank: usize,
    pub recency: f64,
    pub score: f64,
    pub occurred_at: String,
    pub digest: String,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrioritizedResult {
    pub hits: Vec<PrioritizedHit>,
    pub params: PriorityParams,
    pub method: String,
    pub computed_at: String,
}

fn parse_ms(ts: &str) -> Option<i64> {
    ts.parse::<openrt_core::Timestamp>()
        .ok()
        .map(|t| t.timestamp_millis())
}

/// Exponential recency in (0, 1]: `0.5 ^ (age / half_life)`. Unknown or
/// future timestamps score 0.0 — never invented freshness.
fn recency_at(occurred_at: &str, now_ms: i64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 0.0;
    }
    let Some(t) = parse_ms(occurred_at) else {
        return 0.0;
    };
    let age_ms = now_ms.saturating_sub(t);
    if age_ms < 0 {
        return 0.0;
    }
    let half_ms = half_life_days * 86_400_000.0;
    0.5f64.powf(age_ms as f64 / half_ms)
}

/// Blend retrieval rank with recency: `score = (1-w)/(rank+1) + w*recency`.
/// Deterministic; ties break by id ascending.
pub fn prioritize(items: &[SemanticItem], params: PriorityParams) -> Vec<PrioritizedHit> {
    let w = params.recency_weight.clamp(0.0, 1.0);
    let mut hits: Vec<PrioritizedHit> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let recency = recency_at(&item.occurred_at, params.now_ms, params.half_life_days);
            let score = (1.0 - w) / (i as f64 + 1.0) + w * recency;
            PrioritizedHit {
                id: item.id.clone(),
                key: item.key.clone(),
                base_rank: i,
                recency,
                score,
                occurred_at: item.occurred_at.clone(),
                digest: item.digest.clone(),
                method: "rank+recency/v1".into(),
            }
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });
    hits
}

/// Full prioritized query result with the params recorded (lineage).
pub fn prioritize_result(items: &[SemanticItem], params: PriorityParams) -> PrioritizedResult {
    let hits = prioritize(items, params);
    PrioritizedResult {
        hits,
        params,
        method: "rank+recency/v1".into(),
        computed_at: openrt_core::now().to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::{Citation, SemanticItem};

    fn item(id: &str, occurred_at: &str) -> SemanticItem {
        SemanticItem {
            id: id.to_string(),
            origin_run: "run.1".into(),
            origin_source: "src.1".into(),
            scope: "default".into(),
            key: format!("key.{id}"),
            value: "v".into(),
            occurred_at: occurred_at.to_string(),
            citation: Citation {
                origin_run: "run.1".into(),
                origin_source: "src.1".into(),
                origin_digest: "d".into(),
            },
            digest: format!("digest-{id}"),
        }
    }

    #[test]
    fn recency_decays_with_half_life_and_guards_bad_input() {
        let now = 1_700_000_000_000_i64;
        let fresh = recency_at("2023-11-14T22:13:20Z", now, 30.0);
        let older = recency_at("2023-10-01T00:00:00Z", now, 30.0);
        assert!(fresh > 0.0 && fresh <= 1.0);
        assert!(older < fresh);
        assert_eq!(recency_at("not-a-time", now, 30.0), 0.0);
        assert_eq!(recency_at("2023-11-14T22:13:20Z", now, 0.0), 0.0);
    }

    #[test]
    fn prioritize_blends_rank_and_recency_deterministically() {
        let items = vec![
            item("old", "2020-01-01T00:00:00Z"),
            item("mid", "2023-11-01T00:00:00Z"),
            item("new", "2023-11-14T00:00:00Z"),
        ];
        let params = PriorityParams {
            recency_weight: 1.0,
            half_life_days: 30.0,
            now_ms: 1_700_000_000_000,
        };
        let ranked = prioritize(&items, params);
        assert_eq!(ranked.len(), 3);
        assert_eq!(
            ranked[0].id, "new",
            "recency-only ranks newest first: {ranked:?}"
        );
        assert!(ranked[0].score >= ranked[1].score && ranked[1].score >= ranked[2].score);
        assert!(ranked.iter().all(|h| h.method == "rank+recency/v1"));

        let rank_only = PriorityParams {
            recency_weight: 0.0,
            ..params
        };
        let r2 = prioritize(&items, rank_only);
        assert_eq!(r2[0].id, "old", "rank-only preserves retrieval order");
        assert_eq!(r2[0].base_rank, 0);

        let again = prioritize(&items, params);
        assert_eq!(ranked, again, "deterministic for fixed now_ms");
    }

    #[test]
    fn result_records_params_for_lineage() {
        let items = vec![item("a", "2023-11-14T00:00:00Z")];
        let params = PriorityParams::default();
        let res = prioritize_result(&items, params);
        assert_eq!(res.params, params);
        assert_eq!(res.method, "rank+recency/v1");
        assert_eq!(res.hits.len(), 1);
        assert_eq!(res.hits[0].digest, "digest-a");
    }
}
