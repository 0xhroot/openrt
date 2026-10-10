//! Optional knowledge-graph layer (`knowledge.graph`): entities and
//! relations on the SAME store as the evidence graph (ADR-006), evolving in
//! place — upserts update the one record (preserving `created_at`), never
//! duplicate it.

use openrt_core::sha256_hex;
use openrt_knowledge::safe_key;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::{EvidenceError, GRAPH_NS};

/// A knowledge-graph entity for one KB. `entity_id` is caller-chosen and
/// scoped to the KB; upserting the same id evolves the record in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub kb_id: String,
    pub entity_id: String,
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Entity {
    pub(crate) fn key(kb_id: &str, entity_id: &str) -> String {
        format!("{}.entity.{}", safe_key(kb_id), safe_key(entity_id))
    }
}

/// A directed relation `subject --predicate--> object`, content-addressed by
/// `sha256` of the triple — adding the same relation twice is idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub kb_id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub created_at: String,
}

impl Relation {
    pub(crate) fn triple_hash(&self) -> String {
        sha256_hex(
            format!(
                "{}\u{1f}{}\u{1f}{}",
                self.subject, self.predicate, self.object
            )
            .as_bytes(),
        )
    }

    pub(crate) fn key(kb_id: &str, subject: &str, predicate: &str, object: &str) -> String {
        let hash = sha256_hex(format!("{subject}\u{1f}{predicate}\u{1f}{object}").as_bytes());
        format!("{}.rel.{hash}", safe_key(kb_id))
    }
}

fn parse<T: serde::de::DeserializeOwned>(
    key: &str,
    value: serde_json::Value,
) -> Result<T, EvidenceError> {
    serde_json::from_value(value)
        .map_err(|e| EvidenceError::Corrupt(key.to_string(), e.to_string()))
}

async fn list_records<T: serde::de::DeserializeOwned>(
    store: &dyn PersistenceStore,
    kb_id: &str,
    marker: &str,
) -> Result<Vec<T>, EvidenceError> {
    let prefix = format!("{}.{marker}.", safe_key(kb_id));
    let mut out: Vec<T> = Vec::new();
    for (key, value) in store.list_records(GRAPH_NS).await? {
        let Some(rest) = key.strip_prefix(prefix.as_str()) else {
            continue;
        };
        // `rel` keys are always sha256 hex; anything else with dots is a
        // foreign KB sharing our key prefix — skipped, never parsed.
        if marker == "rel" && !(rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit())) {
            continue;
        }
        if rest.is_empty() {
            continue;
        }
        let rec: T = parse(&key, value)?;
        out.push(rec);
    }
    Ok(out)
}

/// Create or evolve an entity. A new id is created with `created_at ==
/// updated_at`; an existing id keeps its original `created_at` and receives
/// the new kind/label/aliases with a fresh `updated_at`. Idempotent: the
/// store always holds exactly one record per (kb, entity_id).
pub async fn upsert_entity(
    store: &dyn PersistenceStore,
    kb_id: &str,
    entity_id: &str,
    kind: &str,
    label: &str,
    aliases: Vec<String>,
) -> Result<Entity, EvidenceError> {
    if entity_id.trim().is_empty() {
        return Err(EvidenceError::Invalid("entity_id must not be empty".into()));
    }
    let key = Entity::key(kb_id, entity_id);
    let now = openrt_core::now().to_rfc3339();
    let entity = match store.get_record(GRAPH_NS, &key).await? {
        Some(value) => {
            let existing: Entity = parse(&key, value)?;
            Entity {
                kb_id: kb_id.to_string(),
                entity_id: entity_id.to_string(),
                kind: kind.to_string(),
                label: label.to_string(),
                aliases,
                created_at: existing.created_at,
                updated_at: now,
            }
        }
        None => Entity {
            kb_id: kb_id.to_string(),
            entity_id: entity_id.to_string(),
            kind: kind.to_string(),
            label: label.to_string(),
            aliases,
            created_at: now.clone(),
            updated_at: now,
        },
    };
    let value = serde_json::to_value(&entity)
        .map_err(|e| EvidenceError::Serde(format!("serialize entity `{key}`: {e}")))?;
    store.put_record(GRAPH_NS, &key, &value).await?;
    let back = store
        .get_record(GRAPH_NS, &key)
        .await?
        .ok_or_else(|| EvidenceError::AppendViolation(key.clone(), "read-back missing".into()))?;
    let verified: Entity = parse(&key, back)?;
    if verified != entity {
        return Err(EvidenceError::AppendViolation(
            key,
            "read-back does not match written entity".into(),
        ));
    }
    Ok(entity)
}

/// Append a relation. Content-addressed: re-adding the identical triple is
/// a no-op returning the existing record; the same (subject, predicate,
/// object) never duplicates.
pub async fn add_relation(
    store: &dyn PersistenceStore,
    kb_id: &str,
    subject: &str,
    predicate: &str,
    object: &str,
) -> Result<Relation, EvidenceError> {
    if subject.trim().is_empty() || predicate.trim().is_empty() || object.trim().is_empty() {
        return Err(EvidenceError::Invalid(
            "relation subject/predicate/object must not be empty".into(),
        ));
    }
    let rel = Relation {
        kb_id: kb_id.to_string(),
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: object.to_string(),
        created_at: openrt_core::now().to_rfc3339(),
    };
    let key = Relation::key(kb_id, subject, predicate, object);
    if let Some(value) = store.get_record(GRAPH_NS, &key).await? {
        let existing: Relation = parse(&key, value)?;
        if existing.subject != rel.subject
            || existing.predicate != rel.predicate
            || existing.object != rel.object
            || existing.kb_id != rel.kb_id
            || existing.triple_hash() != rel.triple_hash()
        {
            return Err(EvidenceError::AppendViolation(
                key,
                "existing relation does not match its content-addressed key".into(),
            ));
        }
        return Ok(existing);
    }
    let value = serde_json::to_value(&rel)
        .map_err(|e| EvidenceError::Serde(format!("serialize relation `{key}`: {e}")))?;
    store.put_record(GRAPH_NS, &key, &value).await?;
    let back = store
        .get_record(GRAPH_NS, &key)
        .await?
        .ok_or_else(|| EvidenceError::AppendViolation(key.clone(), "read-back missing".into()))?;
    let verified: Relation = parse(&key, back)?;
    if verified != rel {
        return Err(EvidenceError::AppendViolation(
            key,
            "read-back does not match written relation".into(),
        ));
    }
    Ok(rel)
}

/// List a KB's entities (sorted by entity id). Malformed records fail
/// closed (`Corrupt`); foreign-KB records sharing the key prefix are
/// skipped.
pub async fn list_entities(
    store: &dyn PersistenceStore,
    kb_id: &str,
) -> Result<Vec<Entity>, EvidenceError> {
    let mut entities: Vec<Entity> = list_records(store, kb_id, "entity").await?;
    entities.retain(|e| e.kb_id == kb_id);
    entities.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
    Ok(entities)
}

/// List a KB's relations in deterministic order (subject, predicate,
/// object).
pub async fn list_relations(
    store: &dyn PersistenceStore,
    kb_id: &str,
) -> Result<Vec<Relation>, EvidenceError> {
    let mut relations: Vec<Relation> = list_records(store, kb_id, "rel").await?;
    relations.retain(|r| r.kb_id == kb_id);
    relations.sort_by(|a, b| {
        (&a.subject, &a.predicate, &a.object).cmp(&(&b.subject, &b.predicate, &b.object))
    });
    Ok(relations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::{MemoryStore, PersistenceStore};

    fn store() -> std::sync::Arc<dyn PersistenceStore> {
        std::sync::Arc::new(MemoryStore::new())
    }

    #[tokio::test]
    async fn entity_upsert_evolves_in_place_without_duplicates() {
        let s = store();
        let e1 = upsert_entity(
            &*s,
            "kb1",
            "saturn-v",
            "rocket",
            "Saturn V",
            vec!["sa-5".into()],
        )
        .await
        .unwrap();
        let e2 = upsert_entity(
            &*s,
            "kb1",
            "saturn-v",
            "rocket",
            "Saturn V (upgraded)",
            vec!["sa-5".into(), "nasa".into()],
        )
        .await
        .unwrap();
        assert_eq!(e1.created_at, e2.created_at);
        assert_eq!(e2.label, "Saturn V (upgraded)");
        let list = list_entities(&*s, "kb1").await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].aliases.len(), 2);
    }

    #[tokio::test]
    async fn relation_is_content_addressed_and_idempotent() {
        let s = store();
        let r1 = add_relation(&*s, "kb1", "apollo-11", "launched", "saturn-v")
            .await
            .unwrap();
        let r2 = add_relation(&*s, "kb1", "apollo-11", "launched", "saturn-v")
            .await
            .unwrap();
        assert_eq!(r1, r2);
        let r3 = add_relation(&*s, "kb1", "apollo-11", "launched", "f-1")
            .await
            .unwrap();
        assert_ne!(r1.triple_hash(), r3.triple_hash());
        let rels = list_relations(&*s, "kb1").await.unwrap();
        assert_eq!(rels.len(), 2);
    }

    #[tokio::test]
    async fn kg_and_evidence_use_separate_namespaces_and_kbs_are_isolated() {
        let s = store();
        upsert_entity(&*s, "kb1", "e1", "person", "Grace", vec![])
            .await
            .unwrap();
        upsert_entity(&*s, "kb2", "e1", "person", "Other", vec![])
            .await
            .unwrap();
        let kb1 = list_entities(&*s, "kb1").await.unwrap();
        let kb2 = list_entities(&*s, "kb2").await.unwrap();
        assert_eq!(kb1[0].label, "Grace");
        assert_eq!(kb2[0].label, "Other");
        // nothing leaked into the evidence namespace
        assert!(s.list_records(crate::EVIDENCE_NS).await.unwrap().is_empty());
        assert_eq!(
            s.list_records(GRAPH_NS).await.unwrap().len(),
            2,
            "exactly one record per (kb, entity)"
        );
    }

    #[tokio::test]
    async fn invalid_inputs_fail_closed() {
        let s = store();
        assert!(matches!(
            upsert_entity(&*s, "kb1", "  ", "k", "l", vec![]).await,
            Err(EvidenceError::Invalid(_))
        ));
        assert!(matches!(
            add_relation(&*s, "kb1", "", "p", "o").await,
            Err(EvidenceError::Invalid(_))
        ));
    }
}
