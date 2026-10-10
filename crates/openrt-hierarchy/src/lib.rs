//! VRAM → RAM → NVMe tier placement and content-addressed MoE expert store.
//!
//! Implements tiered placement planning (ADR-009) and sparse-expert shard
//! metadata persistence. Dense models have no [`ExpertLayout`] — expert
//! metadata is never fabricated; it exists only when a real sparse-expert
//! model manifest declares per-shard digests.

use std::collections::BTreeMap;

use openrt_core::encode_safe_key;
use openrt_persistence::{PersistenceError, PersistenceStore};
use openrt_verify::sha256_hex;
use serde::{Deserialize, Serialize};

// ── Tier ────────────────────────────────────────────────────────────────

/// Memory tier for model placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Vram,
    Ram,
    Nvme,
}

/// Canonical tier iteration order (fastest → slowest).
pub const TIERS: [Tier; 3] = [Tier::Vram, Tier::Ram, Tier::Nvme];

impl Tier {
    /// String representation matching the serde rename.
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Vram => "vram",
            Tier::Ram => "ram",
            Tier::Nvme => "nvme",
        }
    }
}

impl std::str::FromStr for Tier {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "vram" => Ok(Tier::Vram),
            "ram" => Ok(Tier::Ram),
            "nvme" => Ok(Tier::Nvme),
            _ => Err(format!("invalid tier: {s}")),
        }
    }
}

// ── Placement types ─────────────────────────────────────────────────────

/// Capacity statistics for one tier after placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierStats {
    pub used: u64,
    pub capacity: u64,
}

/// A single slab assigned to a tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slab {
    pub id: String,
    pub bytes: u64,
    pub tier: Tier,
}

/// Whether all slabs fit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PlacementOutcome {
    Fits,
    Insufficient { missing_bytes: u64 },
}

/// Complete placement plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementPlan {
    pub assignments: Vec<Slab>,
    pub tier_stats: BTreeMap<Tier, TierStats>,
    pub outcome: PlacementOutcome,
}

/// Atomically place slabs into tiers (Vram→Ram→Nvme greedy order).
pub fn place_atomic(slabs: Vec<(String, u64)>, capacities: &BTreeMap<Tier, u64>) -> PlacementPlan {
    let mut remaining: BTreeMap<Tier, u64> = TIERS
        .iter()
        .map(|&t| (t, capacities.get(&t).copied().unwrap_or(0)))
        .collect();
    let mut used: BTreeMap<Tier, u64> = BTreeMap::new();
    let mut assignments = Vec::with_capacity(slabs.len());
    let mut missing: u64 = 0;

    for (id, bytes) in slabs {
        let mut placed = false;
        for &tier in &TIERS {
            if let Some(rem) = remaining.get_mut(&tier) {
                if *rem >= bytes {
                    *rem -= bytes;
                    *used.entry(tier).or_insert(0) += bytes;
                    assignments.push(Slab { id, bytes, tier });
                    placed = true;
                    break;
                }
            }
        }
        if !placed {
            missing += bytes;
            // Unassigned slab: no tier recorded.
        }
    }

    let tier_stats: BTreeMap<Tier, TierStats> = TIERS
        .iter()
        .map(|&t| {
            let cap = capacities.get(&t).copied().unwrap_or(0);
            let u = used.get(&t).copied().unwrap_or(0);
            (
                t,
                TierStats {
                    used: u,
                    capacity: cap,
                },
            )
        })
        .collect();

    let outcome = if missing == 0 {
        PlacementOutcome::Fits
    } else {
        PlacementOutcome::Insufficient {
            missing_bytes: missing,
        }
    };

    PlacementPlan {
        assignments,
        tier_stats,
        outcome,
    }
}

// ── Expert layout (ADR-009 / ADR-010) ──────────────────────────────────

/// One shard declaration within a sparse MoE model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpertShardDecl {
    pub digest: String,
    pub expert_index: u64,
    pub bytes: u64,
}

/// Full expert layout declared by a real sparse-expert model manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpertLayout {
    pub expert_count: u64,
    pub shards: Vec<ExpertShardDecl>,
}

/// Strict validation: `Some` only when `expert_count >= 1`, shards length
/// equals `expert_count`, indices are exactly `0..count` each present once,
/// and every digest is a lowercase 64-char hex string.
///
/// A dense model has no `ExpertLayout` (`None`); expert metadata is **never**
/// fabricated.
pub fn layout_from_json(v: &serde_json::Value) -> Option<ExpertLayout> {
    let expert_count = v.get("expert_count")?.as_u64()?;
    if expert_count < 1 {
        return None;
    }
    let shards_raw = v.get("shards")?.as_array()?;
    if shards_raw.len() as u64 != expert_count {
        return None;
    }

    let mut seen = BTreeMap::new();
    let mut shards = Vec::with_capacity(shards_raw.len());

    for entry in shards_raw {
        let digest = entry.get("digest")?.as_str()?;
        let expert_index = entry.get("expert_index")?.as_u64()?;
        let bytes = entry.get("bytes")?.as_u64()?;

        if seen.contains_key(&expert_index) {
            return None; // duplicate index
        }
        if !is_valid_hex_digest(digest) {
            return None;
        }
        seen.insert(expert_index, true);
        shards.push(ExpertShardDecl {
            digest: digest.to_string(),
            expert_index,
            bytes,
        });
    }

    // Indices must be exactly 0..count.
    if seen.len() as u64 != expert_count {
        return None;
    }
    for i in 0..expert_count {
        if !seen.contains_key(&i) {
            return None;
        }
    }

    Some(ExpertLayout {
        expert_count,
        shards,
    })
}

fn is_valid_hex_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && b.is_ascii_lowercase())
}

// ── Expert persistence (free-function store surface) ────────────────────

/// Namespace for expert shard records.
pub const EXPERTS_NS: &str = "hierarchy.experts";

/// Write one record per shard in `layout`. The record key is the
/// content-addressed digest (encoded for path safety). The stored JSON carries
/// `expert_index`, `bytes`, `expert_count`, and `layout_digest` (SHA-256 of
/// the canonical layout JSON).
pub async fn record_expert_shard(
    store: &dyn PersistenceStore,
    layout: &ExpertLayout,
) -> Result<(), PersistenceError> {
    let layout_canonical = serde_json::to_string(layout)?;
    let layout_digest = sha256_hex(layout_canonical.as_bytes());

    for shard in &layout.shards {
        let key = encode_safe_key(&shard.digest);
        let value = serde_json::json!({
            "expert_index": shard.expert_index,
            "bytes": shard.bytes,
            "expert_count": layout.expert_count,
            "layout_digest": layout_digest,
        });
        store.put_record(EXPERTS_NS, &key, &value).await?;
    }
    Ok(())
}

/// Load an expert shard declaration by its content-addressed digest.
///
/// The key is the encoded digest; the stored record is returned as-is (the
/// content-addressed key is the integrity property).
pub async fn load_expert_shard(
    store: &dyn PersistenceStore,
    digest: &str,
) -> Result<Option<ExpertShardDecl>, PersistenceError> {
    let key = encode_safe_key(digest);
    match store.get_record(EXPERTS_NS, &key).await? {
        Some(v) => {
            let expert_index = v.get("expert_index").and_then(|x| x.as_u64()).unwrap_or(0);
            let bytes = v.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0);
            Ok(Some(ExpertShardDecl {
                digest: digest.to_string(),
                expert_index,
                bytes,
            }))
        }
        None => Ok(None),
    }
}

// ── Placement persistence ──────────────────────────────────────────────

/// Namespace for placement records.
pub const PLACEMENTS_NS: &str = "hierarchy.placements";

/// Record a [`PlacementPlan`] under the encoded artifact id.
pub async fn record_placement(
    store: &dyn PersistenceStore,
    artifact_id: &str,
    plan: &PlacementPlan,
) -> Result<(), PersistenceError> {
    let key = encode_safe_key(artifact_id);
    let value = serde_json::to_value(plan)?;
    store.put_record(PLACEMENTS_NS, &key, &value).await
}

/// Load a previously-recorded [`PlacementPlan`].
pub async fn load_placement(
    store: &dyn PersistenceStore,
    artifact_id: &str,
) -> Result<Option<PlacementPlan>, PersistenceError> {
    let key = encode_safe_key(artifact_id);
    match store.get_record(PLACEMENTS_NS, &key).await? {
        Some(v) => {
            let plan: PlacementPlan = serde_json::from_value(v)?;
            Ok(Some(plan))
        }
        None => Ok(None),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn cap(v: u64, r: u64, n: u64) -> BTreeMap<Tier, u64> {
        let mut m = BTreeMap::new();
        m.insert(Tier::Vram, v);
        m.insert(Tier::Ram, r);
        m.insert(Tier::Nvme, n);
        m
    }

    #[test]
    fn place_atomic_all_in_vram() {
        let plan = place_atomic(
            vec![("a".into(), 30), ("b".into(), 40)],
            &cap(100, 100, 100),
        );
        assert_eq!(plan.outcome, PlacementOutcome::Fits);
        assert_eq!(plan.assignments.len(), 2);
        assert!(plan.assignments.iter().all(|s| s.tier == Tier::Vram));
        assert_eq!(plan.tier_stats[&Tier::Vram].used, 70);
    }

    #[test]
    fn place_atomic_ram_fallback() {
        let plan = place_atomic(vec![("a".into(), 40), ("b".into(), 30)], &cap(50, 100, 100));
        assert_eq!(plan.outcome, PlacementOutcome::Fits);
        assert_eq!(plan.assignments[0].tier, Tier::Vram);
        assert_eq!(plan.assignments[1].tier, Tier::Ram);
    }

    #[test]
    fn place_atomic_insufficient() {
        let plan = place_atomic(vec![("a".into(), 40), ("b".into(), 5)], &cap(10, 10, 10));
        assert_eq!(
            plan.outcome,
            PlacementOutcome::Insufficient { missing_bytes: 40 }
        );
        assert_eq!(plan.assignments.len(), 1);
        assert_eq!(plan.assignments[0].id, "b");
    }

    #[test]
    fn tier_order_uses_nvme_last() {
        let plan = place_atomic(vec![("a".into(), 20)], &cap(5, 5, 50));
        assert_eq!(plan.outcome, PlacementOutcome::Fits);
        assert_eq!(plan.assignments[0].tier, Tier::Nvme);
    }

    #[test]
    fn layout_from_json_accepts_valid() {
        let d1 = "a".repeat(64);
        let d2 = "b".repeat(64);
        let v = serde_json::json!({
            "expert_count": 2,
            "shards": [
                {"digest": d1, "expert_index": 0, "bytes": 10},
                {"digest": d2, "expert_index": 1, "bytes": 20}
            ]
        });
        let layout = layout_from_json(&v).expect("valid layout");
        assert_eq!(layout.expert_count, 2);
        assert_eq!(layout.shards.len(), 2);
    }

    #[test]
    fn layout_from_json_rejects_missing_index() {
        let d1 = "a".repeat(64);
        let d2 = "b".repeat(64);
        // Missing index 1
        let v = serde_json::json!({
            "expert_count": 2,
            "shards": [
                {"digest": d1, "expert_index": 0, "bytes": 10},
                {"digest": d2, "expert_index": 2, "bytes": 20}
            ]
        });
        assert_eq!(layout_from_json(&v), None);
    }

    #[test]
    fn layout_from_json_rejects_duplicate_index() {
        let d1 = "a".repeat(64);
        let d2 = "b".repeat(64);
        let v = serde_json::json!({
            "expert_count": 2,
            "shards": [
                {"digest": d1, "expert_index": 0, "bytes": 10},
                {"digest": d2, "expert_index": 0, "bytes": 20}
            ]
        });
        assert_eq!(layout_from_json(&v), None);
    }

    #[test]
    fn layout_from_json_rejects_bad_digest_length() {
        let d1 = "a".repeat(64);
        let d2 = "b".repeat(32); // too short
        let v = serde_json::json!({
            "expert_count": 2,
            "shards": [
                {"digest": d1, "expert_index": 0, "bytes": 10},
                {"digest": d2, "expert_index": 1, "bytes": 20}
            ]
        });
        assert_eq!(layout_from_json(&v), None);
    }

    #[test]
    fn layout_from_json_rejects_count_mismatch() {
        let d1 = "a".repeat(64);
        let v = serde_json::json!({
            "expert_count": 3,
            "shards": [
                {"digest": d1, "expert_index": 0, "bytes": 10}
            ]
        });
        assert_eq!(layout_from_json(&v), None);
    }

    #[test]
    fn layout_from_json_rejects_zero_expert_count() {
        let v = serde_json::json!({"expert_count": 0, "shards": []});
        assert_eq!(layout_from_json(&v), None);
    }

    #[test]
    fn layout_from_json_rejects_uppercase_hex() {
        let d1: String = "A".repeat(64);
        let d2 = "b".repeat(64);
        let v = serde_json::json!({
            "expert_count": 2,
            "shards": [
                {"digest": d1, "expert_index": 0, "bytes": 10},
                {"digest": d2, "expert_index": 1, "bytes": 20}
            ]
        });
        assert_eq!(layout_from_json(&v), None);
    }

    #[tokio::test]
    async fn record_load_expert_shard_roundtrip() {
        let store = MemoryStore::new();
        let d1 = "a".repeat(64);
        let d2 = "b".repeat(64);
        let layout = ExpertLayout {
            expert_count: 2,
            shards: vec![
                ExpertShardDecl {
                    digest: d1.clone(),
                    expert_index: 0,
                    bytes: 10,
                },
                ExpertShardDecl {
                    digest: d2.clone(),
                    expert_index: 1,
                    bytes: 20,
                },
            ],
        };

        record_expert_shard(&store, &layout).await.unwrap();

        let s0 = load_expert_shard(&store, &d1).await.unwrap().unwrap();
        assert_eq!(s0.expert_index, 0);
        assert_eq!(s0.bytes, 10);
        assert_eq!(s0.digest, d1);

        let s1 = load_expert_shard(&store, &d2).await.unwrap().unwrap();
        assert_eq!(s1.expert_index, 1);

        // Non-existent digest.
        let d3 = "c".repeat(64);
        assert!(load_expert_shard(&store, &d3).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn record_load_placement_roundtrip() {
        let store = MemoryStore::new();
        let plan = place_atomic(vec![("x".into(), 10), ("y".into(), 20)], &cap(50, 50, 50));
        record_placement(&store, "artifact-1", &plan).await.unwrap();
        let loaded = load_placement(&store, "artifact-1").await.unwrap().unwrap();
        assert_eq!(loaded, plan);
    }

    #[test]
    fn tier_from_str_roundtrip() {
        assert_eq!("vram".parse::<Tier>().unwrap(), Tier::Vram);
        assert_eq!("ram".parse::<Tier>().unwrap(), Tier::Ram);
        assert_eq!("nvme".parse::<Tier>().unwrap(), Tier::Nvme);
        assert!("bogus".parse::<Tier>().is_err());
    }

    #[test]
    fn tier_as_str() {
        assert_eq!(Tier::Vram.as_str(), "vram");
        assert_eq!(Tier::Ram.as_str(), "ram");
        assert_eq!(Tier::Nvme.as_str(), "nvme");
    }
}
