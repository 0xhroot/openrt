//! Phase 13 (Parts L–T): CyberSaarthi + OpenRT SIH hero demo over the
//! existing production authorities — one store, one gate, one audit chain.
//!
//! The synthetic dataset is embedded (Part P: every record marked
//! `synthetic`) so `openrt sih demo` is reproducible from a clean
//! checkout with no hidden manual state. Every stage runs through a
//! production entry point: workspace/room, `corpus` ingest, knowledge
//! graph + evidence graph, retrieval, workflow engine, verifier,
//! artifact store, audit chain.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{Error, Runtime};

pub const SIH_KB: &str = "kb.sih-demo";
pub const SIH_ACTOR: &str = "sih.demo";
pub const SIH_WORKSPACE: &str = "sih-demo";
pub const SIH_ROOM_TITLE: &str = "Investigation Room";
pub const SIH_WORKFLOW_ID: &str = "wf.sih-investigation";

pub const CASE_JSON: &str = include_str!("sih_data/case.json");
pub const ENTITIES_JSON: &str = include_str!("sih_data/entities.json");
pub const RELATIONSHIPS_JSON: &str = include_str!("sih_data/relationships.json");
pub const TRANSACTIONS_JSON: &str = include_str!("sih_data/transactions.json");
pub const INCIDENTS_JSON: &str = include_str!("sih_data/incidents.json");
pub const DOC_WITNESS: &str = include_str!("sih_data/docs/witness_statement.md");
pub const DOC_LEDGER: &str = include_str!("sih_data/docs/financial_ledger.md");
pub const DOC_MEMO: &str = include_str!("sih_data/docs/association_memo.md");
pub const DOC_HOSTILE: &str = include_str!("sih_data/docs/hostile_note.md");

pub const HOSTILE_DOC_NAME: &str = "hostile_note.md";
pub const DEMO_CLAIM: &str =
    "Aria Khan transferred funds from account HOLD-777 into account NOVA-001 on 2026-01-03.";

#[derive(Debug, Clone, Deserialize)]
pub struct SyntheticEntity {
    pub synthetic: bool,
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub identifiers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyntheticRelation {
    pub synthetic: bool,
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticTransaction {
    pub synthetic: bool,
    pub id: String,
    pub at: String,
    pub from: String,
    pub to: String,
    pub amount_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticIncident {
    pub synthetic: bool,
    pub id: String,
    pub at: String,
    pub title: String,
    pub location: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyntheticCase {
    pub synthetic: bool,
    pub case_id: String,
    pub title: String,
    pub jurisdiction: String,
    pub opened_at: String,
    pub summary: String,
    pub hypothesis: String,
}

#[derive(Debug, Clone)]
pub struct SyntheticDoc {
    pub name: &'static str,
    pub text: &'static str,
    pub hostile: bool,
}

#[derive(Debug, Clone)]
pub struct SihDataset {
    pub case: SyntheticCase,
    pub entities: Vec<SyntheticEntity>,
    pub relationships: Vec<SyntheticRelation>,
    pub transactions: Vec<SyntheticTransaction>,
    pub incidents: Vec<SyntheticIncident>,
    pub docs: Vec<SyntheticDoc>,
}

fn parse_dataset() -> Result<SihDataset, Error> {
    let case: SyntheticCase = serde_json::from_str(CASE_JSON)
        .map_err(|e| Error::Unavailable(format!("sih case.json: {e}")))?;
    let wrap_e: Value = serde_json::from_str(ENTITIES_JSON)
        .map_err(|e| Error::Unavailable(format!("sih entities.json: {e}")))?;
    let entities: Vec<SyntheticEntity> = serde_json::from_value(
        wrap_e
            .get("entities")
            .cloned()
            .unwrap_or(Value::Array(Vec::new())),
    )
    .map_err(|e| Error::Unavailable(format!("sih entities.json: {e}")))?;
    let wrap_r: Value = serde_json::from_str(RELATIONSHIPS_JSON)
        .map_err(|e| Error::Unavailable(format!("sih relationships.json: {e}")))?;
    let relationships: Vec<SyntheticRelation> = serde_json::from_value(
        wrap_r
            .get("relationships")
            .cloned()
            .unwrap_or(Value::Array(Vec::new())),
    )
    .map_err(|e| Error::Unavailable(format!("sih relationships.json: {e}")))?;
    let wrap_t: Value = serde_json::from_str(TRANSACTIONS_JSON)
        .map_err(|e| Error::Unavailable(format!("sih transactions.json: {e}")))?;
    let transactions: Vec<SyntheticTransaction> = serde_json::from_value(
        wrap_t
            .get("transactions")
            .cloned()
            .unwrap_or(Value::Array(Vec::new())),
    )
    .map_err(|e| Error::Unavailable(format!("sih transactions.json: {e}")))?;
    let wrap_i: Value = serde_json::from_str(INCIDENTS_JSON)
        .map_err(|e| Error::Unavailable(format!("sih incidents.json: {e}")))?;
    let incidents: Vec<SyntheticIncident> = serde_json::from_value(
        wrap_i
            .get("incidents")
            .cloned()
            .unwrap_or(Value::Array(Vec::new())),
    )
    .map_err(|e| Error::Unavailable(format!("sih incidents.json: {e}")))?;

    if !case.synthetic
        || !entities.iter().all(|e| e.synthetic)
        || !relationships.iter().all(|r| r.synthetic)
        || !transactions.iter().all(|t| t.synthetic)
        || !incidents.iter().all(|i| i.synthetic)
    {
        return Err(Error::Unavailable(
            "sih dataset: every record must carry synthetic=true".into(),
        ));
    }

    let docs = vec![
        SyntheticDoc {
            name: "witness_statement.md",
            text: DOC_WITNESS,
            hostile: false,
        },
        SyntheticDoc {
            name: "financial_ledger.md",
            text: DOC_LEDGER,
            hostile: false,
        },
        SyntheticDoc {
            name: "association_memo.md",
            text: DOC_MEMO,
            hostile: false,
        },
        SyntheticDoc {
            name: HOSTILE_DOC_NAME,
            text: DOC_HOSTILE,
            hostile: true,
        },
    ];

    Ok(SihDataset {
        case,
        entities,
        relationships,
        transactions,
        incidents,
        docs,
    })
}

pub fn dataset() -> Result<SihDataset, Error> {
    parse_dataset()
}

pub fn health() -> Result<(usize, usize, usize, usize), Error> {
    let d = parse_dataset()?;
    Ok((
        d.docs.len(),
        d.entities.len(),
        d.relationships.len(),
        d.transactions.len() + d.incidents.len(),
    ))
}

#[derive(Debug, Clone, Serialize)]
pub struct SihArtifact {
    pub id: String,
    pub kind: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SihReport {
    pub case_id: String,
    pub workspace: String,
    pub room: String,
    pub documents: usize,
    pub chunks: usize,
    pub entities: usize,
    pub relations: usize,
    pub hits: usize,
    pub workflow_steps: usize,
    pub workflow_status: String,
    pub verdict: String,
    pub decision: String,
    pub grounding_coverage: f64,
    pub verdict_id: String,
    pub injection_flags: usize,
    pub injection_categories: Vec<String>,
    pub artifacts: Vec<SihArtifact>,
    pub audit_entries: u64,
    pub audit_verified: u64,
    pub timings_ms: Vec<(String, u128)>,
    pub total_ms: u128,
}

#[derive(Debug, Clone)]
pub struct SihOptions {
    pub out_dir: PathBuf,
    pub kb_id: String,
    pub query: String,
}

impl Default for SihOptions {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::from("sih-demo"),
            kb_id: SIH_KB.to_string(),
            query: DEMO_CLAIM.to_string(),
        }
    }
}

pub async fn run_demo(rt: &Runtime, opts: &SihOptions) -> Result<SihReport, Error> {
    let total = Instant::now();
    let mut timings: Vec<(String, u128)> = Vec::new();
    let mark = |label: &str, t: &Instant, out: &mut Vec<(String, u128)>| {
        out.push((label.to_string(), t.elapsed().as_millis()));
    };

    let t = Instant::now();
    let report = rt.doctor().await;
    let fails = report
        .checks
        .iter()
        .filter(|c| matches!(c.status, crate::doctor::DoctorStatus::Fail))
        .count();
    if fails > 0 {
        return Err(Error::Unavailable(format!(
            "prerequisites: doctor reported {fails} FAIL row(s)"
        )));
    }
    mark("prerequisites", &t, &mut timings);

    let data = parse_dataset()?;
    let now = openrt_core::now().to_rfc3339();

    let t = Instant::now();
    let workspace = match openrt_workspace::create_workspace(
        &*rt.store,
        SIH_WORKSPACE,
        &data.case.title,
        &data.case.summary,
        &now,
    )
    .await
    {
        Ok(_id) => SIH_WORKSPACE.to_string(),
        Err(openrt_workspace::WorkspaceError::AlreadyExists(_)) => {
            openrt_workspace::open_workspace(&*rt.store, SIH_WORKSPACE, &now)
                .await
                .map(|ws| ws.address_code)?
        }
        Err(e) => return Err(Error::Workbench(e)),
    };
    rt.audit(
        SIH_ACTOR,
        "sih.case.create",
        &data.case.case_id,
        "ok",
        json!({"synthetic": true, "title": data.case.title}),
    )
    .await?;
    mark("case", &t, &mut timings);

    let t = Instant::now();
    let room = match rt
        .list_rooms()
        .await?
        .into_iter()
        .find(|r| r.title == SIH_ROOM_TITLE)
    {
        Some(r) => r,
        None => {
            rt.create_room(&workspace, SIH_ROOM_TITLE, SIH_ACTOR)
                .await?
        }
    };
    mark("room", &t, &mut timings);

    let t = Instant::now();
    let mut doc_ids: Vec<(String, bool)> = Vec::new();
    for doc in &data.docs {
        let out = rt
            .corpus
            .ingest_bytes(doc.text.as_bytes(), Some(doc.name), SIH_ACTOR)
            .await
            .map_err(|e| Error::Unavailable(format!("ingest {}: {e}", doc.name)))?;
        match out.state {
            openrt_ingest::corpus::IngestState::Stored
            | openrt_ingest::corpus::IngestState::Duplicate => {}
            other => {
                return Err(Error::Unavailable(format!(
                    "ingest {}: state {}",
                    doc.name,
                    other.label()
                )))
            }
        }
        doc_ids.push((out.doc.doc_id.clone(), doc.hostile));
        rt.audit(
            SIH_ACTOR,
            "sih.evidence.ingest",
            &out.doc.doc_id,
            "ok",
            json!({"file": doc.name, "synthetic": true, "state": out.state.label()}),
        )
        .await?;
    }
    mark("ingest", &t, &mut timings);

    let t = Instant::now();
    let mut chunks_total = 0usize;
    let mut hostile_citations: Vec<(openrt_knowledge::Citation, String)> = Vec::new();
    for (doc_id, hostile) in &doc_ids {
        let doc = rt
            .corpus
            .inspect(doc_id)
            .await
            .map_err(|e| Error::Unavailable(format!("inspect {doc_id}: {e}")))?;
        let chunks = openrt_ingest::chunk::chunk_normalized(
            &doc,
            &openrt_ingest::chunk::ChunkConfig {
                strategy: openrt_ingest::chunk::ChunkStrategy::Paragraph,
                ..openrt_ingest::chunk::ChunkConfig::default()
            },
        )
        .map_err(|e| Error::Unavailable(format!("chunk {doc_id}: {e}")))?;
        for c in &chunks {
            openrt_knowledge::persist_chunk(
                &*rt.store,
                &opts.kb_id,
                &openrt_knowledge::PersistedChunk {
                    chunk_id: c.chunk_id.clone(),
                    document_id: c.doc_id.clone(),
                    document_version: c.document_version,
                    block_keys: c.block_keys.clone(),
                    page: c.page,
                    text_sha256: c.sha256.clone(),
                    text: Some(c.text.clone()),
                },
            )
            .await?;
            if *hostile {
                hostile_citations.push((
                    openrt_knowledge::Citation {
                        kb_id: opts.kb_id.clone(),
                        document_id: c.doc_id.clone(),
                        document_version: c.document_version,
                        chunk_id: c.chunk_id.clone(),
                        block_keys: c.block_keys.clone(),
                        page: c.page,
                        text_sha256: c.sha256.clone(),
                    },
                    c.text.clone(),
                ));
            }
        }
        chunks_total += chunks.len();
    }
    mark("chunk", &t, &mut timings);

    let t = Instant::now();
    for e in &data.entities {
        openrt_evidence::upsert_entity(
            &*rt.store,
            &opts.kb_id,
            &e.id,
            &e.kind,
            &e.label,
            e.aliases.clone(),
        )
        .await?;
    }
    for r in &data.relationships {
        openrt_evidence::add_relation(&*rt.store, &opts.kb_id, &r.subject, &r.predicate, &r.object)
            .await?;
    }
    rt.audit(
        SIH_ACTOR,
        "sih.graph.build",
        &opts.kb_id,
        "ok",
        json!({"entities": data.entities.len(), "relations": data.relationships.len()}),
    )
    .await?;
    mark("graph", &t, &mut timings);

    let t = Instant::now();
    for (doc_id, _) in &doc_ids {
        let meta = rt
            .corpus
            .status(doc_id)
            .await
            .map_err(|e| Error::Unavailable(format!("status {doc_id}: {e}")))?
            .ok_or_else(|| Error::Unavailable(format!("meta {doc_id} missing")))?;
        let key =
            openrt_ingest::document_key(&openrt_ingest::ns_key_doc_id(doc_id), meta.latest_version);
        rt.create_material(
            openrt_workspace::MaterialKind::Document,
            &format!("Evidence {doc_id} (synthetic demo)"),
            "sih-demo",
            openrt_workspace::MaterialReference::Store {
                namespace: openrt_ingest::corpus::DOCUMENTS_NS.to_string(),
                key,
            },
            SIH_ACTOR,
        )
        .await?;
    }
    rt.audit(
        SIH_ACTOR,
        "sih.material.attach",
        &room.id,
        "ok",
        json!({"materials": doc_ids.len()}),
    )
    .await?;
    mark("materials", &t, &mut timings);

    let t = Instant::now();
    let search = openrt_knowledge::hybrid_search(
        Arc::clone(&rt.store),
        &opts.kb_id,
        &openrt_knowledge::RetrieveRequest {
            query_text: opts.query.clone(),
            top_k: 4,
            threshold: 0.0,
        },
        &openrt_knowledge::HybridOptions::default(),
    )
    .await?;
    let mut citations = Vec::new();
    let mut evidence_texts = Vec::new();
    for hit in &search.hits {
        let pc = openrt_knowledge::load_chunk(
            &*rt.store,
            &opts.kb_id,
            &hit.citation.document_id,
            &hit.citation.chunk_id,
        )
        .await?
        .ok_or_else(|| Error::Unavailable("retrieved chunk missing".into()))?;
        citations.push(hit.citation.clone());
        evidence_texts.push(pc.text.unwrap_or_default());
    }
    rt.audit(
        SIH_ACTOR,
        "sih.retrieve",
        &opts.kb_id,
        "ok",
        json!({"hits": search.hits.len()}),
    )
    .await?;
    mark("retrieve", &t, &mut timings);

    let t = Instant::now();
    let session = rt.create_session(&data.case.case_id).await?;
    let wf = openrt_orch::Workflow::new(SIH_WORKFLOW_ID)
        .named("cybersaarthi-investigation")
        .step(openrt_orch::WorkflowStep::new(
            "retrieve",
            openrt_orch::SubAgentKind::Researcher,
            format!(
                "Case {}: retrieve and summarize the synthetic evidence relevant to: {}",
                data.case.case_id, opts.query
            ),
        ))
        .step(
            openrt_orch::WorkflowStep::new(
                "analyze",
                openrt_orch::SubAgentKind::Planner,
                "Correlate the suspects, accounts, and locations from the retrieved evidence.",
            )
            .dep("retrieve"),
        )
        .step(
            openrt_orch::WorkflowStep::new(
                "verify",
                openrt_orch::SubAgentKind::Verifier,
                "Check every statement against the retrieved citations before reporting.",
            )
            .dep("analyze"),
        );
    let run = rt.run_workflow(session.id.clone(), wf).await?;
    let sid = session.id.to_string();
    let _ = rt.link_room_session(&room.id, &sid, SIH_ACTOR).await;
    let _ = rt
        .set_room_workflow(&room.id, Some(SIH_WORKFLOW_ID), SIH_ACTOR)
        .await;
    rt.audit(
        SIH_ACTOR,
        "sih.workflow.run",
        SIH_WORKFLOW_ID,
        &format!("{:?}", run.status),
        json!({"steps": run.steps.len(), "session": sid}),
    )
    .await?;
    mark("workflow", &t, &mut timings);

    let t = Instant::now();
    let vreport = rt
        .verify_claim(
            openrt_verifier::VerifyInput {
                kb_id: opts.kb_id.clone(),
                claim: DEMO_CLAIM.to_string(),
                citations,
                evidence_texts,
            },
            &rt.verify_options(),
        )
        .await?;
    rt.audit(
        SIH_ACTOR,
        "sih.verify.claim",
        DEMO_CLAIM,
        &format!("{:?}", vreport.decision),
        json!({"verdict": format!("{:?}", vreport.verdict), "verdict_id": vreport.verdict_id}),
    )
    .await?;
    mark("verify", &t, &mut timings);

    let t = Instant::now();
    let mut injection_flags = 0usize;
    let mut injection_categories: Vec<String> = Vec::new();
    for (citation, text) in &hostile_citations {
        let rec = openrt_evidence::scan_evidence(&*rt.store, &opts.kb_id, citation, text).await?;
        injection_flags += rec.flags.len();
        for c in &rec.categories {
            let name = c.as_str().to_string();
            if !injection_categories.contains(&name) {
                injection_categories.push(name);
            }
        }
    }
    if !hostile_citations.is_empty() {
        rt.audit(
            SIH_ACTOR,
            "sih.injection.detected",
            HOSTILE_DOC_NAME,
            if injection_flags > 0 {
                "blocked"
            } else {
                "clean"
            },
            json!({"flags": injection_flags, "categories": injection_categories, "trust": "untrusted"}),
        )
        .await?;
    }
    mark("injection", &t, &mut timings);

    let t = Instant::now();
    let claim = openrt_evidence::record_claim(&*rt.store, &opts.kb_id, DEMO_CLAIM).await?;
    for hit in search.hits.iter().take(2) {
        openrt_evidence::link_claim(&*rt.store, &opts.kb_id, &claim.claim_id, &hit.citation)
            .await?;
    }
    rt.audit(
        SIH_ACTOR,
        "sih.evidence.claim",
        &claim.claim_id,
        "ok",
        json!({"citations": search.hits.len().min(2)}),
    )
    .await?;
    mark("claims", &t, &mut timings);

    let t = Instant::now();
    let art_dir = opts.out_dir.join("artifacts");
    std::fs::create_dir_all(&art_dir)
        .map_err(|e| Error::Unavailable(format!("artifacts dir: {e}")))?;

    let mut entity_lines: Vec<String> = data
        .entities
        .iter()
        .map(|e| format!("- [{}] {} ({})", e.kind, e.label, e.id))
        .collect();
    entity_lines.sort();
    let mut relation_lines: Vec<String> = data
        .relationships
        .iter()
        .map(|r| format!("- {} -[{}]-> {}", r.subject, r.predicate, r.object))
        .collect();
    relation_lines.sort();

    let report_md = format!(
        "# Investigation Report — {}\n\n\
         SYNTHETIC DEMO OUTPUT — generated by the CyberSaarthi + OpenRT hero demo.\n\n\
         Case: {} ({})\nJurisdiction: {}\nOpened: {}\n\n\
         ## Hypothesis under verification\n\n{}\n\n\
         ## Entities (synthetic)\n\n{}\n\n\
         ## Relationships (synthetic)\n\n{}\n\n\
         ## Agent workflow\n\n- workflow: {} ({}, {} steps)\n- session: {}\n\n\
         ## Evidence verification\n\n- claim: {}\n- decision: {}\n- verdict: {}\n- lexical coverage: {:.3}\n- verdict id: {}\n\n\
         ## Security\n\n- injected content flags: {}\n- injection categories: {}\n- hostile document labeled untrusted, no authority granted\n\n\
         ## Provenance\n\n- documents ingested: {}\n- knowledge chunks: {}\n- retrieval hits: {}\n- audit entries at report time: {}\n",
        data.case.case_id,
        data.case.title,
        data.case.case_id,
        data.case.jurisdiction,
        data.case.opened_at,
        DEMO_CLAIM,
        entity_lines.join("\n"),
        relation_lines.join("\n"),
        SIH_WORKFLOW_ID,
        &format!("{:?}", run.status),
        run.steps.len(),
        sid,
        DEMO_CLAIM,
        &format!("{:?}", vreport.decision),
        format!("{:?}", vreport.verdict),
        vreport.deterministic.lexical_coverage,
        vreport.verdict_id,
        injection_flags,
        injection_categories.join(", "),
        doc_ids.len(),
        chunks_total,
        search.hits.len(),
        rt.audit_query(&openrt_audit::AuditQuery::new().with_limit(10_000))
            .await?
            .len(),
    );
    let report_path = art_dir.join("investigation_report.md");
    std::fs::write(&report_path, report_md)
        .map_err(|e| Error::Unavailable(format!("write report: {e}")))?;

    let evidence_summary = json!({
        "synthetic": true,
        "case_id": data.case.case_id,
        "documents": doc_ids.iter().map(|(id, hostile)| json!({"doc_id": id, "hostile": hostile})).collect::<Vec<_>>(),
        "chunks": chunks_total,
        "hits": search.hits.iter().map(|h| json!({
            "document_id": h.citation.document_id,
            "chunk_id": h.citation.chunk_id,
            "score": h.fused_score,
        })).collect::<Vec<_>>(),
        "transactions": data.transactions,
        "incidents": data.incidents,
    });
    let evidence_path = art_dir.join("evidence_summary.json");
    std::fs::write(
        &evidence_path,
        serde_json::to_string_pretty(&evidence_summary).unwrap_or_default(),
    )
    .map_err(|e| Error::Unavailable(format!("write evidence summary: {e}")))?;

    let graph_json = json!({
        "synthetic": true,
        "case_id": data.case.case_id,
        "nodes": data.entities.iter().map(|e| json!({"id": e.id, "kind": e.kind, "label": e.label, "identifiers": e.identifiers})).collect::<Vec<_>>(),
        "edges": data.relationships.iter().map(|r| json!({"subject": r.subject, "predicate": r.predicate, "object": r.object})).collect::<Vec<_>>(),
    });
    let graph_path = art_dir.join("relationship_graph.json");
    std::fs::write(
        &graph_path,
        serde_json::to_string_pretty(&graph_json).unwrap_or_default(),
    )
    .map_err(|e| Error::Unavailable(format!("write graph: {e}")))?;

    let verification_json = json!({
        "synthetic": true,
        "claim": DEMO_CLAIM,
        "decision": format!("{:?}", vreport.decision),
        "verdict": format!("{:?}", vreport.verdict),
        "basis": format!("{:?}", vreport.basis),
        "lexical_coverage": vreport.deterministic.lexical_coverage,
        "citations_total": vreport.deterministic.citations_total,
        "citations_resolved": vreport.deterministic.citations_resolved,
        "verdict_id": vreport.verdict_id,
    });
    let verification_path = art_dir.join("verification_report.json");
    std::fs::write(
        &verification_path,
        serde_json::to_string_pretty(&verification_json).unwrap_or_default(),
    )
    .map_err(|e| Error::Unavailable(format!("write verification: {e}")))?;

    let mut recorded: Vec<SihArtifact> = Vec::new();
    let files: [(&str, &str, &PathBuf); 4] = [
        ("report", "investigation_report.md", &report_path),
        ("evidence", "evidence_summary.json", &evidence_path),
        ("graph", "relationship_graph.json", &graph_path),
        (
            "verification",
            "verification_report.json",
            &verification_path,
        ),
    ];
    for (kind, name, path) in files {
        let rec = rt
            .fabric
            .artifacts
            .record_file(
                path,
                kind,
                SIH_ACTOR,
                None,
                json!({"case_id": data.case.case_id, "synthetic": true}),
            )
            .await
            .map_err(|e| Error::Unavailable(format!("record {name}: {e}")))?;
        rt.audit(
            SIH_ACTOR,
            "sih.artifact.record",
            &rec.artifact_id,
            "ok",
            json!({"file": name, "sha256": rec.sha256}),
        )
        .await?;
        recorded.push(SihArtifact {
            id: rec.artifact_id,
            kind: kind.to_string(),
            path: path.display().to_string(),
            sha256: rec.sha256,
        });
    }

    let audit_rows = rt
        .audit_query(
            &openrt_audit::AuditQuery::new()
                .with_action_prefix("sih.")
                .with_limit(10_000),
        )
        .await?;
    let audit_verified = rt.audit_verify().await?;
    let audit_summary = json!({
        "synthetic": true,
        "case_id": data.case.case_id,
        "sih_actions": audit_rows.iter().map(|r| json!({"seq": r.seq, "action": r.entry.action, "result": r.entry.result})).collect::<Vec<_>>(),
        "chain_verified_entries": audit_verified,
        "shield_findings": openrt_shield::store::list_findings(&*rt.store, None)
            .await
            .map(|f| f.len())
            .unwrap_or(0),
    });
    let audit_path = art_dir.join("audit_summary.json");
    std::fs::write(
        &audit_path,
        serde_json::to_string_pretty(&audit_summary).unwrap_or_default(),
    )
    .map_err(|e| Error::Unavailable(format!("write audit summary: {e}")))?;
    let rec = rt
        .fabric
        .artifacts
        .record_file(
            &audit_path,
            "audit",
            SIH_ACTOR,
            None,
            json!({"case_id": data.case.case_id, "synthetic": true}),
        )
        .await
        .map_err(|e| Error::Unavailable(format!("record audit summary: {e}")))?;
    rt.audit(
        SIH_ACTOR,
        "sih.artifact.record",
        &rec.artifact_id,
        "ok",
        json!({"file": "audit_summary.json", "sha256": rec.sha256}),
    )
    .await?;
    recorded.push(SihArtifact {
        id: rec.artifact_id,
        kind: "audit".into(),
        path: audit_path.display().to_string(),
        sha256: rec.sha256,
    });
    mark("artifacts", &t, &mut timings);

    let audit_entries = rt
        .audit_query(
            &openrt_audit::AuditQuery::new()
                .with_action_prefix("sih.")
                .with_limit(10_000),
        )
        .await?
        .len() as u64;
    let audit_verified = rt.audit_verify().await?;
    rt.audit(
        SIH_ACTOR,
        "sih.demo.complete",
        &data.case.case_id,
        "ok",
        json!({"artifacts": recorded.len(), "audit_entries": audit_entries}),
    )
    .await?;
    let audit_entries = audit_entries + 1;

    timings.push(("total".to_string(), total.elapsed().as_millis()));

    Ok(SihReport {
        case_id: data.case.case_id,
        workspace,
        room: room.id,
        documents: doc_ids.len(),
        chunks: chunks_total,
        entities: data.entities.len(),
        relations: data.relationships.len(),
        hits: search.hits.len(),
        workflow_steps: run.steps.len(),
        workflow_status: format!("{:?}", run.status).to_string(),
        verdict: format!("{:?}", vreport.verdict).to_string(),
        decision: format!("{:?}", vreport.decision).to_string(),
        grounding_coverage: f64::from(vreport.deterministic.lexical_coverage),
        verdict_id: vreport.verdict_id.clone(),
        injection_flags,
        injection_categories,
        artifacts: recorded,
        audit_entries,
        audit_verified,
        timings_ms: timings,
        total_ms: total.elapsed().as_millis(),
    })
}

pub async fn dashboard(rt: &Runtime) -> Value {
    let kb = SIH_KB;
    let rooms = rt.list_rooms().await.unwrap_or_default();
    let docs = rt.corpus.list().await.map(|d| d.len()).unwrap_or(0);
    let claims = openrt_evidence::list_claims(&*rt.store, kb)
        .await
        .map(|c| c.len())
        .unwrap_or(0);
    let entities = openrt_evidence::list_entities(&*rt.store, kb)
        .await
        .unwrap_or_default();
    let relations = openrt_evidence::list_relations(&*rt.store, kb)
        .await
        .unwrap_or_default();
    let sessions = rt.list_sessions().await.unwrap_or_default();
    let runs = rt
        .store
        .list_records(openrt_orch::RUNS_NS)
        .await
        .map(|r| r.len())
        .unwrap_or(0);
    let verdicts = openrt_verifier::list_verdicts(&*rt.store, kb)
        .await
        .unwrap_or_default();
    let artifacts = rt.fabric.artifacts.list(None).await.unwrap_or_default();
    let findings = openrt_shield::store::list_findings(&*rt.store, None)
        .await
        .unwrap_or_default();
    let trust = openrt_evidence::list_trust(&*rt.store, kb)
        .await
        .unwrap_or_default();
    let halted = rt.halted();
    let mode = rt.netpol().mode().await.map(|m| format!("{m:?}")).ok();
    let grants = rt.netpol().list().await.map(|g| g.len()).unwrap_or(0);
    let secrets = match rt.secrets().await {
        Ok(store) => store.list().await.map(|l| l.len()).unwrap_or(0),
        Err(_) => 0,
    };
    let audit = rt
        .audit_query(&openrt_audit::AuditQuery::new().with_limit(25))
        .await
        .unwrap_or_default();
    let chain = rt.audit_verify().await.unwrap_or(0);

    json!({
        "case": {
            "case_id": "case.sih-demo",
            "title": "Operation Synthetic Ledger (SYNTHETIC DEMO CASE)",
            "synthetic": true,
            "rooms": rooms.iter().map(|r| json!({
                "id": r.id, "title": r.title, "status": format!("{:?}", r.status),
                "materials": r.materials.len(), "artifacts": r.artifacts.len(),
                "active_workflow": r.active_workflow,
            })).collect::<Vec<_>>(),
        },
        "evidence": {"documents": docs, "claims": claims, "kb": kb},
        "entities": {
            "count": entities.len(),
            "relations": relations.len(),
            "graph": relations.iter().take(50).map(|r| json!({
                "subject": r.subject, "predicate": r.predicate, "object": r.object,
            })).collect::<Vec<_>>(),
        },
        "agent": {
            "sessions": sessions.len(),
            "workflow_runs": runs,
            "recent_sessions": sessions.iter().rev().take(5).map(|s| json!({
                "id": s.id.to_string(), "title": s.title, "status": format!("{:?}", s.status),
            })).collect::<Vec<_>>(),
        },
        "verification": {
            "verdicts": verdicts.len(),
            "last": verdicts.last().map(|v| json!({
                "decision": format!("{:?}", v.decision),
                "verdict": format!("{:?}", v.verdict),
                "claim_sha256": v.claim_sha256,
            })),
        },
        "artifacts": artifacts.iter().map(|a| json!({
            "id": a.artifact_id, "kind": a.kind, "sha256": a.sha256, "verification": a.verification.state,
        })).collect::<Vec<_>>(),
        "security": {
            "shield_findings": findings.len(),
            "injection_trust_records": trust.iter().filter(|t| !t.categories.is_empty()).count(),
            "halted": halted,
            "network_mode": mode,
            "egress_grants": grants,
            "secrets": secrets,
        },
        "audit": {
            "chain_verified": chain,
            "recent": audit.iter().map(|r| json!({
                "seq": r.seq, "action": r.entry.action, "result": r.entry.result, "subject": r.entry.subject,
            })).collect::<Vec<_>>(),
        },
    })
}
