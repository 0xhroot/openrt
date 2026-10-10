//! Phase 9 (M9.7 / TEST 7) — evidence graph end-to-end:
//! * citations resolve against persisted evidence (knowledge stays THE
//!   citation authority; an unpersisted citation can never be linked);
//! * evidence graph edges are append-only — relinking is idempotent and
//!   tampering (in-place rewrite, deletion) is detected by
//!   `verify_integrity`, never silently accepted;
//! * the optional knowledge graph lives on the SAME store under its own
//!   namespace, evolving in place without duplicating records.

use std::sync::Arc;

use openrt_evidence::{
    add_relation, claims_for_document, link_claim, list_entities, list_relations,
    passages_for_claim, record_claim, resolve_citation, upsert_entity, verify_integrity,
    EVIDENCE_NS, GRAPH_NS,
};
use openrt_knowledge::{persist_chunk, record_key, safe_key, Citation, PersistedChunk};
use openrt_persistence::{MemoryStore, PersistenceStore};

const KB: &str = "kb-evidence";

fn citation(doc: &str, chunk: &str) -> Citation {
    Citation {
        kb_id: KB.into(),
        document_id: doc.into(),
        document_version: 1,
        chunk_id: chunk.into(),
        block_keys: vec![format!("b/{chunk}")],
        page: Some(3),
        text_sha256: openrt_core::sha256_hex(b"the launch window is dawn"),
    }
}

async fn persist_citation(store: &dyn PersistenceStore, c: &Citation) {
    let chunk = PersistedChunk {
        chunk_id: c.chunk_id.clone(),
        document_id: c.document_id.clone(),
        document_version: c.document_version,
        block_keys: c.block_keys.clone(),
        page: c.page,
        text_sha256: c.text_sha256.clone(),
        text: Some("the launch window is dawn".into()),
    };
    persist_chunk(store, KB, &chunk).await.unwrap();
}

fn store() -> Arc<dyn PersistenceStore> {
    Arc::new(MemoryStore::new())
}

#[tokio::test]
async fn citations_resolve_and_traverse_through_the_evidence_graph() {
    let s = store();
    let c = citation("doc1", "c1");
    persist_citation(&*s, &c).await;
    let claim = record_claim(&*s, KB, "the launch window is dawn")
        .await
        .unwrap();
    let edge = link_claim(&*s, KB, &claim.claim_id, &c).await.unwrap();
    assert_eq!(edge.seq, 0);

    let resolved = resolve_citation(&*s, KB, &c).await.unwrap();
    assert_eq!(resolved.len(), 2);
    let passages = passages_for_claim(&*s, KB, &claim.claim_id).await.unwrap();
    assert_eq!(passages, vec![c.clone()]);
    let docs = claims_for_document(&*s, KB, "doc1").await.unwrap();
    assert_eq!(docs, vec![claim.claim_id.clone()]);
}

#[tokio::test]
async fn unpersisted_citation_cannot_be_linked_fail_closed() {
    let s = store();
    let claim = record_claim(&*s, KB, "ungrounded claim").await.unwrap();
    let ghost = citation("doc-absent", "c-absent");
    let err = link_claim(&*s, KB, &claim.claim_id, &ghost).await;
    assert!(err.is_err(), "linking an unpersisted citation must fail");
    let err = resolve_citation(&*s, KB, &ghost).await;
    assert!(err.is_err(), "resolving an unpersisted citation must fail");
    assert!(verify_integrity(&*s, KB).await.unwrap().intact());
}

#[tokio::test]
async fn relinking_is_idempotent_edges_stay_append_only() {
    let s = store();
    let c = citation("doc1", "c1");
    persist_citation(&*s, &c).await;
    let claim = record_claim(&*s, KB, "idempotent claim").await.unwrap();
    let first = link_claim(&*s, KB, &claim.claim_id, &c).await.unwrap();
    for _ in 0..3 {
        let again = link_claim(&*s, KB, &claim.claim_id, &c).await.unwrap();
        assert_eq!(again, first, "relink must return the existing edge");
    }
    let report = verify_integrity(&*s, KB).await.unwrap();
    assert!(report.intact(), "problems: {:?}", report.problems);
    assert_eq!(
        report.edges_checked, 2,
        "3 relinks must not append new edges"
    );
}

#[tokio::test]
async fn tampering_and_deletion_are_detected_never_silent() {
    let s = store();
    let c = citation("doc1", "c1");
    persist_citation(&*s, &c).await;
    let claim = record_claim(&*s, KB, "guarded claim").await.unwrap();
    link_claim(&*s, KB, &claim.claim_id, &c).await.unwrap();
    assert!(verify_integrity(&*s, KB).await.unwrap().intact());

    // in-place rewrite of an edge record (bypassing the append-only API)
    let edge_key = format!("{}.edge.{:012}", safe_key(KB), 0u64);
    let original = s
        .get_record(EVIDENCE_NS, &edge_key)
        .await
        .unwrap()
        .expect("edge exists");
    let mut value = original.clone();
    value["created_at"] = serde_json::Value::String("1999-01-01T00:00:00+00:00".into());
    s.put_record(EVIDENCE_NS, &edge_key, &value).await.unwrap();
    let report = verify_integrity(&*s, KB).await.unwrap();
    assert!(!report.intact(), "rewritten edge must be flagged");

    // restore the exact original, then delete the edge outright -> gap +
    // orphan detected; nothing is silently accepted.
    s.put_record(EVIDENCE_NS, &edge_key, &original)
        .await
        .unwrap();
    assert!(verify_integrity(&*s, KB).await.unwrap().intact());
    s.delete_record(EVIDENCE_NS, &edge_key).await.unwrap();
    let report = verify_integrity(&*s, KB).await.unwrap();
    assert!(!report.intact(), "deleted edge must be flagged");
}

#[tokio::test]
async fn deleted_source_chunk_unresolves_its_citation() {
    let s = store();
    let c = citation("doc1", "c1");
    persist_citation(&*s, &c).await;
    let claim = record_claim(&*s, KB, "claim over c1").await.unwrap();
    link_claim(&*s, KB, &claim.claim_id, &c).await.unwrap();
    assert!(verify_integrity(&*s, KB).await.unwrap().intact());

    // delete the persisted chunk -> citation must stop resolving
    let chunk_key = format!("{}.{}", safe_key(KB), record_key("doc1", "c1"));
    s.delete_record(openrt_knowledge::KB_CHUNKS_NS, &chunk_key)
        .await
        .unwrap();
    assert!(
        resolve_citation(&*s, KB, &c).await.is_err(),
        "citation over a deleted chunk must not resolve"
    );
    let report = verify_integrity(&*s, KB).await.unwrap();
    assert!(!report.intact());
}

#[tokio::test]
async fn knowledge_graph_lives_on_the_same_store_without_duplicates() {
    let s = store();
    // evidence + graph coexist under their own namespaces
    let c = citation("doc1", "c1");
    persist_citation(&*s, &c).await;
    let claim = record_claim(&*s, KB, "kg-adjacent claim").await.unwrap();
    link_claim(&*s, KB, &claim.claim_id, &c).await.unwrap();

    let e1 = upsert_entity(
        &*s,
        KB,
        "saturn-v",
        "rocket",
        "Saturn V",
        vec!["sa-5".into()],
    )
    .await
    .unwrap();
    let e2 = upsert_entity(
        &*s,
        KB,
        "saturn-v",
        "rocket",
        "Saturn V",
        vec!["sa-5".into()],
    )
    .await
    .unwrap();
    assert_eq!(
        e1.created_at, e2.created_at,
        "upsert must evolve, not recreate"
    );
    let entities = list_entities(&*s, KB).await.unwrap();
    assert_eq!(entities.len(), 1, "no duplicate entities");

    let r1 = add_relation(&*s, KB, "apollo-11", "launched-with", "saturn-v")
        .await
        .unwrap();
    let r2 = add_relation(&*s, KB, "apollo-11", "launched-with", "saturn-v")
        .await
        .unwrap();
    assert_eq!(r1, r2, "same triple must be idempotent");
    assert_eq!(list_relations(&*s, KB).await.unwrap().len(), 1);

    // namespaces are separated: evidence is unaffected by graph writes
    let evidence_records = s.list_records(EVIDENCE_NS).await.unwrap();
    let graph_records = s.list_records(GRAPH_NS).await.unwrap();
    assert_eq!(evidence_records.len(), 3, "2 edges + 1 claim");
    assert_eq!(graph_records.len(), 2, "1 entity + 1 relation");
    assert!(verify_integrity(&*s, KB).await.unwrap().intact());
}
