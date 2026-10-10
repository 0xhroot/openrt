//! Phase 11 (Track D): **material records** — the workbench's typed
//! catalog of inputs/outputs (documents, images, datasets, evidence,
//! artifacts, workflow definitions, knowledge, sessions, tasks).
//!
//! OWNERSHIP: namespace `materials.records` (key `mat.<hex24>`), id =
//! `sha256(kind|title|source|reference|created_at)` — deterministic.
//!
//! Invariants:
//! * a material is a **record + reference**, never a copy — content stays in
//!   its one authority (foundation store records or files on disk);
//! * `digest` seals what the reference resolved to at registration time;
//!   `verify_material` re-derives it (fail-closed drift detection);
//! * links (`attached_workbenches`) are additive and idempotent; removing a
//!   link never destroys the record;
//! * digesting is **bounded**: files larger than [`DIGEST_FILE_BOUND`] are
//!   refused (fail-closed — never read unbounded attacker-controlled input).

use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::WorkspaceError;

/// Namespace owned by this module (Phase 11 OWNERSHIP).
pub const MATERIALS_NS: &str = "materials.records";

/// Upper bound for file bytes read while digesting (fail-closed).
pub const DIGEST_FILE_BOUND: u64 = 8 * 1024 * 1024;

/// Material categories (stable, lowercase on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MaterialKind {
    Document,
    Image,
    Dataset,
    Evidence,
    Artifact,
    Workflow,
    Knowledge,
    Session,
    Task,
}

impl std::fmt::Display for MaterialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Document => "document",
            Self::Image => "image",
            Self::Dataset => "dataset",
            Self::Evidence => "evidence",
            Self::Artifact => "artifact",
            Self::Workflow => "workflow",
            Self::Knowledge => "knowledge",
            Self::Session => "session",
            Self::Task => "task",
        };
        f.write_str(s)
    }
}

/// Where a material's content lives — one of the two authorities, never a
/// copy inside the material record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum MaterialReference {
    /// A record already in the foundation store (namespace + key).
    Store { namespace: String, key: String },
    /// A file on disk (path is resolved against the process cwd / absolute).
    Path { path: String },
}

impl std::fmt::Display for MaterialReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store { namespace, key } => write!(f, "store:{namespace}/{key}"),
            Self::Path { path } => write!(f, "path:{path}"),
        }
    }
}

/// A durable material record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Material {
    pub id: String,
    pub kind: MaterialKind,
    pub title: String,
    /// Free-form provenance ("cli", "ingest", "workflow:wf1", …).
    pub source: String,
    pub reference: MaterialReference,
    /// sha256 of the referenced content (store record JSON / file bytes) at
    /// registration — re-derived by [`verify_material`].
    pub digest: String,
    pub created_by: String,
    pub created_at: String,
    /// Workbench address codes this material is attached to (additive).
    pub attached_workbenches: Vec<String>,
}

fn material_key(id: &str) -> String {
    format!("mat.{id}")
}

fn serde_err(e: impl std::fmt::Display) -> WorkspaceError {
    WorkspaceError::Serde(e.to_string())
}

/// Deterministic bytes for digesting a store record: object keys sorted at
/// every depth, compact encoding — the same logical record always digests
/// the same way regardless of insertion order.
fn canonical_bytes(value: &serde_json::Value) -> Vec<u8> {
    fn sorted(v: serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let mut kvs: Vec<(String, serde_json::Value)> =
                    m.into_iter().map(|(k, v)| (k, sorted(v))).collect();
                kvs.sort_by(|a, b| a.0.cmp(&b.0));
                serde_json::Value::Object(kvs.into_iter().collect())
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.into_iter().map(sorted).collect())
            }
            other => other,
        }
    }
    sorted(value.clone()).to_string().into_bytes()
}

/// Resolve the referenced content to bytes for digesting.
/// Fail-closed: missing records/files and oversized files are errors.
async fn content_bytes(
    store: &dyn PersistenceStore,
    reference: &MaterialReference,
) -> Result<Vec<u8>, WorkspaceError> {
    match reference {
        MaterialReference::Store { namespace, key } => {
            let Some(value) = store.get_record(namespace, key).await? else {
                return Err(WorkspaceError::Invalid(format!(
                    "referenced record `{namespace}/{key}` not found"
                )));
            };
            // Canonical bytes: keys sorted at every depth — the same input
            // always digests the same way.
            Ok(canonical_bytes(&value))
        }
        MaterialReference::Path { path } => {
            let meta = std::fs::metadata(path)
                .map_err(|e| WorkspaceError::Invalid(format!("cannot stat `{path}`: {e}")))?;
            if !meta.is_file() {
                return Err(WorkspaceError::Invalid(format!(
                    "`{path}` is not a regular file"
                )));
            }
            if meta.len() > DIGEST_FILE_BOUND {
                return Err(WorkspaceError::Invalid(format!(
                    "`{path}` exceeds the {DIGEST_FILE_BOUND}-byte digest bound \
                     (fail-closed: unbounded reads are never performed)"
                )));
            }
            std::fs::read(path)
                .map_err(|e| WorkspaceError::Invalid(format!("cannot read `{path}`: {e}")))
        }
    }
}

async fn digest_of(
    store: &dyn PersistenceStore,
    reference: &MaterialReference,
) -> Result<String, WorkspaceError> {
    let bytes = content_bytes(store, reference).await?;
    Ok(openrt_embeddings::sha256_hex(&bytes))
}

/// Register a material. Fail-closed: the referenced content must exist and
/// be digestible right now (a material never points at nothing).
pub async fn create_material(
    store: &dyn PersistenceStore,
    kind: MaterialKind,
    title: &str,
    source: &str,
    reference: MaterialReference,
    created_by: &str,
    now: &str,
) -> Result<Material, WorkspaceError> {
    if title.trim().is_empty() {
        return Err(WorkspaceError::Invalid(
            "material title must not be empty".into(),
        ));
    }
    let digest = digest_of(store, &reference).await?;
    let id = openrt_embeddings::sha256_hex(
        format!("{kind}|{title}|{source}|{reference}|{now}").as_bytes(),
    );
    let material = Material {
        id: id.clone(),
        kind,
        title: title.to_string(),
        source: source.to_string(),
        reference,
        digest,
        created_by: created_by.to_string(),
        created_at: now.to_string(),
        attached_workbenches: Vec::new(),
    };
    let value = serde_json::to_value(&material).map_err(serde_err)?;
    store
        .put_record(MATERIALS_NS, &material_key(&id), &value)
        .await?;
    Ok(material)
}

/// Fetch one material (fail-closed NotFound).
pub async fn get_material(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Material, WorkspaceError> {
    let Some(value) = store.get_record(MATERIALS_NS, &material_key(id)).await? else {
        return Err(WorkspaceError::MaterialNotFound(id.to_string()));
    };
    serde_json::from_value(value).map_err(serde_err)
}

/// Deterministically list all materials (sorted by id).
pub async fn list_materials(store: &dyn PersistenceStore) -> Result<Vec<Material>, WorkspaceError> {
    let mut raw = store.list_records(MATERIALS_NS).await?;
    raw.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::with_capacity(raw.len());
    for (_key, value) in raw {
        out.push(serde_json::from_value(value).map_err(serde_err)?);
    }
    Ok(out)
}

/// Attach a material to a workbench (validates the workbench exists through
/// the M5.1 authority; idempotent).
pub async fn attach_to_workbench(
    store: &dyn PersistenceStore,
    material_id: &str,
    workbench_code: &str,
    now: &str,
) -> Result<Material, WorkspaceError> {
    let ws = crate::open_workspace(store, workbench_code, now).await?;
    let mut material = get_material(store, material_id).await?;
    debug_assert_eq!(ws.address_code, workbench_code);
    if !material
        .attached_workbenches
        .iter()
        .any(|c| c == workbench_code)
    {
        material
            .attached_workbenches
            .push(workbench_code.to_string());
        let value = serde_json::to_value(&material).map_err(serde_err)?;
        store
            .put_record(MATERIALS_NS, &material_key(&material.id), &value)
            .await?;
    }
    Ok(material)
}

/// Detach from a workbench (idempotent; never destroys the record).
pub async fn detach_from_workbench(
    store: &dyn PersistenceStore,
    material_id: &str,
    workbench_code: &str,
) -> Result<Material, WorkspaceError> {
    let mut material = get_material(store, material_id).await?;
    if material
        .attached_workbenches
        .iter()
        .any(|c| c == workbench_code)
    {
        material
            .attached_workbenches
            .retain(|c| c != workbench_code);
        let value = serde_json::to_value(&material).map_err(serde_err)?;
        store
            .put_record(MATERIALS_NS, &material_key(&material.id), &value)
            .await?;
    }
    Ok(material)
}

/// Re-derive the digest from the current referenced content and compare.
/// `Ok(false)` on ANY mismatch or unresolvable content (fail-closed — a
/// drifted or vanished reference is never reported as intact).
pub async fn verify_material(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<bool, WorkspaceError> {
    let material = get_material(store, id).await?;
    let current = match digest_of(store, &material.reference).await {
        Ok(d) => d,
        Err(_) => return Ok(false),
    };
    Ok(current == material.digest)
}

/// Convenience for the CLI/runtime: parse `--ref-ns/--ref-key` or
/// `--ref-path` style inputs into a reference (exactly one form required).
pub fn parse_reference(
    ns: Option<&str>,
    key: Option<&str>,
    path: Option<&str>,
) -> Result<MaterialReference, WorkspaceError> {
    match (ns, key, path) {
        (Some(namespace), Some(key), None) => Ok(MaterialReference::Store {
            namespace: namespace.to_string(),
            key: key.to_string(),
        }),
        (None, None, Some(path)) => Ok(MaterialReference::Path {
            path: path.to_string(),
        }),
        _ => Err(WorkspaceError::Invalid(
            "exactly one reference form required: (--ref-ns + --ref-key) or --ref-path".into(),
        )),
    }
}

#[cfg(test)]
mod p11_material_tests {
    use super::*;
    use openrt_persistence::MemoryStore;
    use std::sync::Arc;

    async fn store() -> Arc<MemoryStore> {
        Arc::new(MemoryStore::new())
    }

    /// Store-backed materials round-trip and their digest detects tampering
    /// of the referenced content (fail-closed verification).
    #[tokio::test]
    async fn store_reference_roundtrip_and_drift_detection() {
        let store = store().await;
        store
            .put_record("docs", "a1", &serde_json::json!({"body": "hello"}))
            .await
            .unwrap();
        let material = create_material(
            &*store,
            MaterialKind::Document,
            "hello doc",
            "cli",
            MaterialReference::Store {
                namespace: "docs".into(),
                key: "a1".into(),
            },
            "carol",
            "t0",
        )
        .await
        .unwrap();
        assert!(verify_material(&*store, &material.id).await.unwrap());
        let again = get_material(&*store, &material.id).await.unwrap();
        assert_eq!(again, material);
        // Tamper with the referenced content → verification fails closed.
        store
            .put_record("docs", "a1", &serde_json::json!({"body": "EVIL"}))
            .await
            .unwrap();
        assert!(!verify_material(&*store, &material.id).await.unwrap());
        // Missing reference at creation time is refused.
        let err = create_material(
            &*store,
            MaterialKind::Document,
            "gone",
            "cli",
            MaterialReference::Store {
                namespace: "docs".into(),
                key: "nope".into(),
            },
            "carol",
            "t1",
        )
        .await;
        assert!(matches!(err, Err(WorkspaceError::Invalid(_))));
    }

    /// File-backed materials digest bound is enforced fail-closed, and
    /// workbench attach/detach is idempotent + validated.
    #[tokio::test]
    async fn file_reference_bound_and_workbench_links() {
        let store = store().await;
        let dir = std::env::temp_dir().join(format!("openrt-material-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, b"material bytes").unwrap();
        let path = file.to_string_lossy().to_string();

        let material = create_material(
            &*store,
            MaterialKind::Evidence,
            "note",
            "cli",
            MaterialReference::Path { path: path.clone() },
            "dave",
            "t0",
        )
        .await
        .unwrap();
        assert!(verify_material(&*store, &material.id).await.unwrap());
        std::fs::write(&file, b"changed bytes").unwrap();
        assert!(!verify_material(&*store, &material.id).await.unwrap());

        // Reference parsing: exactly one form.
        assert!(parse_reference(Some("a"), Some("b"), None).is_ok());
        assert!(parse_reference(None, None, Some("p")).is_ok());
        assert!(parse_reference(Some("a"), None, Some("p")).is_err());
        assert!(parse_reference(None, None, None).is_err());

        // Attach requires an existing workbench; links are idempotent.
        crate::create_workspace(&*store, "acct/lab", "Lab", "", "t0")
            .await
            .unwrap();
        let err = attach_to_workbench(&*store, &material.id, "acct/ghost", "t1").await;
        assert!(matches!(err, Err(WorkspaceError::NotFound(_))));
        let m = attach_to_workbench(&*store, &material.id, "acct/lab", "t1")
            .await
            .unwrap();
        let m2 = attach_to_workbench(&*store, &material.id, "acct/lab", "t2")
            .await
            .unwrap();
        assert_eq!(m.attached_workbenches, m2.attached_workbenches);
        assert_eq!(m.attached_workbenches, vec!["acct/lab".to_string()]);
        let m3 = detach_from_workbench(&*store, &material.id, "acct/lab")
            .await
            .unwrap();
        assert!(m3.attached_workbenches.is_empty());
        assert!(get_material(&*store, &material.id).await.is_ok());
        let missing = get_material(&*store, "nope").await;
        assert!(matches!(missing, Err(WorkspaceError::MaterialNotFound(_))));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
