//! Persistence for industrial records in the ONE store (ADR-006):
//! assets+relations in `industrial.assets`, analysis outputs in
//! `industrial.analysis`. Every relation is mirrored into `knowledge.graph`
//! through `openrt-evidence` so the graph stays the single graph authority.

use openrt_evidence::{add_relation as graph_add_relation, upsert_entity as graph_upsert_entity};
use openrt_knowledge::safe_key;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::model::{Asset, AssetFilter, Provenance, Relation, REL_PREDICATES};
use crate::{IndustrialError, ANALYSIS_NS, ASSETS_NS};

const GRAPH_KB: &str = "industrial";

fn asset_key(id: &str) -> String {
    format!("asset.{}", safe_key(id))
}

fn rel_key(subject: &str, predicate: &str, object: &str) -> String {
    let hash =
        openrt_core::sha256_hex(format!("{subject}\u{1f}{predicate}\u{1f}{object}").as_bytes());
    format!("rel.{hash}")
}

fn parse<T: for<'de> Deserialize<'de>>(
    key: &str,
    value: serde_json::Value,
) -> Result<T, IndustrialError> {
    serde_json::from_value(value)
        .map_err(|e| IndustrialError::Corrupt(key.to_string(), e.to_string()))
}

/// Create or evolve an asset. `created_at` is preserved on update; the
/// knowledge-graph entity mirror is kept in sync.
pub async fn upsert_asset(
    store: &dyn PersistenceStore,
    mut asset: Asset,
) -> Result<Asset, IndustrialError> {
    if asset.id.trim().is_empty() {
        return Err(IndustrialError::Invalid(
            "asset id must not be empty".into(),
        ));
    }
    let now = openrt_core::now().to_rfc3339();
    let key = asset_key(&asset.id);
    if let Some(v) = store.get_record(ASSETS_NS, &key).await? {
        let existing: Asset = parse(&key, v)?;
        asset.created_at = existing.created_at;
    }
    asset.updated_at = now.clone();
    let value = serde_json::to_value(&asset)
        .map_err(|e| IndustrialError::Serde(format!("serialize asset `{key}`: {e}")))?;
    store.put_record(ASSETS_NS, &key, &value).await?;

    graph_upsert_entity(
        store,
        GRAPH_KB,
        &asset.id,
        asset.kind.as_str(),
        &asset.label,
        Vec::new(),
    )
    .await?;
    if let Some(parent) = &asset.parent {
        graph_add_relation(store, GRAPH_KB, &asset.id, "belongs_to", parent).await?;
        store
            .put_record(
                ASSETS_NS,
                &rel_key(&asset.id, "belongs_to", parent),
                &serde_json::json!({
                    "subject": asset.id,
                    "predicate": "belongs_to",
                    "object": parent,
                    "provenance": Provenance::derived("asset.parent", now.clone()),
                    "created_at": now,
                }),
            )
            .await?;
    }
    Ok(asset)
}

pub async fn get_asset(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Option<Asset>, IndustrialError> {
    match store.get_record(ASSETS_NS, &asset_key(id)).await? {
        None => Ok(None),
        Some(v) => Ok(Some(parse(&asset_key(id), v)?)),
    }
}

pub async fn list_assets(
    store: &dyn PersistenceStore,
    filter: &AssetFilter,
) -> Result<Vec<Asset>, IndustrialError> {
    let mut out = Vec::new();
    for (key, value) in store.list_records(ASSETS_NS).await? {
        if !key.starts_with("asset.") {
            continue;
        }
        let asset: Asset = parse(&key, value)?;
        if filter.matches(&asset) {
            out.push(asset);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Add a relation (predicate must be one of the canonical set) and mirror it
/// into the knowledge graph. Idempotent.
pub async fn add_relation(
    store: &dyn PersistenceStore,
    subject: &str,
    predicate: &str,
    object: &str,
    provenance: Provenance,
) -> Result<Relation, IndustrialError> {
    if !REL_PREDICATES.contains(&predicate) {
        return Err(IndustrialError::Invalid(format!(
            "predicate `{predicate}` not in canonical set {REL_PREDICATES:?}"
        )));
    }
    if subject.trim().is_empty() || object.trim().is_empty() {
        return Err(IndustrialError::Invalid(
            "relation endpoints must not be empty".into(),
        ));
    }
    let now = openrt_core::now().to_rfc3339();
    let key = rel_key(subject, predicate, object);
    let rel = match store.get_record(ASSETS_NS, &key).await? {
        Some(v) => parse::<Relation>(&key, v)?,
        None => Relation {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            provenance,
            created_at: now,
        },
    };
    let value = serde_json::to_value(&rel)
        .map_err(|e| IndustrialError::Serde(format!("serialize relation `{key}`: {e}")))?;
    store.put_record(ASSETS_NS, &key, &value).await?;
    graph_add_relation(store, GRAPH_KB, subject, predicate, object).await?;
    Ok(rel)
}

/// Relations touching `id` as either endpoint.
pub async fn relations_for(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Vec<Relation>, IndustrialError> {
    let mut out = Vec::new();
    for (key, value) in store.list_records(ASSETS_NS).await? {
        if !key.starts_with("rel.") {
            continue;
        }
        let rel: Relation = parse(&key, value)?;
        if rel.subject == id || rel.object == id {
            out.push(rel);
        }
    }
    out.sort_by(|a, b| {
        (a.subject.as_str(), a.predicate.as_str(), a.object.as_str()).cmp(&(
            b.subject.as_str(),
            b.predicate.as_str(),
            b.object.as_str(),
        ))
    });
    Ok(out)
}

/// Persist an analysis output (trend/anomaly/rca/fmea/risk/correlation/
/// scenario/investigation). Content-addressed by its canonical JSON so
/// identical computations do not duplicate records.
pub async fn put_analysis<T: Serialize>(
    store: &dyn PersistenceStore,
    kind: &str,
    subject: &str,
    value: &T,
) -> Result<String, IndustrialError> {
    let json = serde_json::to_string(value)
        .map_err(|e| IndustrialError::Serde(format!("serialize analysis: {e}")))?;
    let id = openrt_core::sha256_hex(json.as_bytes());
    let key = format!("analysis.{}.{}", safe_key(kind), safe_key(subject));
    let record = serde_json::json!({
        "kind": kind,
        "subject": subject,
        "id": id,
        "computed_at": openrt_core::now().to_rfc3339(),
        "payload": value,
    });
    store.put_record(ANALYSIS_NS, &key, &record).await?;
    Ok(id)
}

/// List persisted analysis outputs of one kind (or all kinds when `None`).
pub async fn list_analysis(
    store: &dyn PersistenceStore,
    kind: Option<&str>,
) -> Result<Vec<serde_json::Value>, IndustrialError> {
    let prefix = match kind {
        Some(k) => format!("analysis.{}.", safe_key(k)),
        None => "analysis.".to_string(),
    };
    let mut out = Vec::new();
    for (key, value) in store.list_records(ANALYSIS_NS).await? {
        if key.starts_with(&prefix) {
            out.push(value);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AssetKind;
    use crate::series::{append_points, query_series, Point, Quality};
    use openrt_persistence::MemoryStore;

    #[tokio::test]
    async fn asset_relation_roundtrip_mirrors_graph() {
        let store = MemoryStore::new();
        let asset = Asset::new(
            "plant.north",
            AssetKind::Plant,
            "North plant",
            Provenance::derived("unit", "2026-01-01T00:00:00Z"),
        );
        upsert_asset(&store, asset.clone()).await.unwrap();
        let mut unit = Asset::new(
            "unit.u1",
            AssetKind::Unit,
            "Unit 1",
            Provenance::derived("unit", "2026-01-01T00:00:00Z"),
        );
        unit.plant = Some("plant.north".into());
        unit.parent = Some("plant.north".into());
        upsert_asset(&store, unit).await.unwrap();

        let back = get_asset(&store, "unit.u1").await.unwrap().unwrap();
        assert_eq!(back.created_at, "2026-01-01T00:00:00Z");
        let all = list_assets(
            &store,
            &AssetFilter {
                kind: Some(AssetKind::Unit),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(all.len(), 1);

        let rel = add_relation(
            &store,
            "unit.u1",
            "belongs_to",
            "plant.north",
            Provenance::derived("unit", "2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(rel.predicate, "belongs_to");
        let rels = relations_for(&store, "unit.u1").await.unwrap();
        assert!(!rels.is_empty());
        let graph = openrt_evidence::list_relations(&store, "industrial")
            .await
            .unwrap();
        assert!(graph
            .iter()
            .any(|r| r.subject == "unit.u1" && r.predicate == "belongs_to"));

        assert!(add_relation(
            &store,
            "a",
            "not_a_predicate",
            "b",
            Provenance::derived("unit", "2026-01-01T00:00:00Z"),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn series_append_query_and_analysis_persist() {
        let store = MemoryStore::new();
        let pts: Vec<Point> = (0..10)
            .map(|i| Point {
                t_ms: i * 1000,
                value: i as f64,
                quality: Quality::Good,
                source: "unit".into(),
            })
            .collect();
        append_points(&store, "s1", "sensor.1", "bar", &pts)
            .await
            .unwrap();
        let q = query_series(&store, "s1", 0, 10_000, 5_000).await.unwrap();
        assert_eq!(q.buckets.len(), 2);
        assert_eq!(q.point_count, 10);

        let trend = crate::trend("sensor.1", "s1", &pts, crate::TrendParams::default(), None);
        let id = put_analysis(&store, "trend", "sensor.1", &trend)
            .await
            .unwrap();
        assert_eq!(id.len(), 64);
        let listed = list_analysis(&store, Some("trend")).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(list_analysis(&store, Some("rca")).await.unwrap().is_empty());
    }
}
