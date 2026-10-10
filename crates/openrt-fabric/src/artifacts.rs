//! Generated-artifact records (`fabric.artifacts` namespace, ADR-006 one-store).
//!
//! Every file produced by a 7B tool (XLSX/PPTX/DOCX/PDF/PNG/download/…) gets a
//! record with: artifact id, type, source task, creation time, producer tool,
//! provenance, SHA-256 digest, location, and verification state.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use openrt_core::encode_safe_key;
use openrt_persistence::PersistenceStore;
use openrt_verify::sha256_hex;

/// Storage namespace (ADR-006).
pub const ARTIFACT_NS: &str = "fabric.artifacts";

/// Verification state of a recorded artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactVerification {
    /// `"verified"` (digest recomputed and matched) or `"unverified"`.
    pub state: String,
    /// How it was verified (`"sha256-recompute"`, `"ooxml-structure"`, …).
    pub method: String,
}

/// One generated artifact record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub artifact_id: String,
    /// `"excel"`, `"pptx"`, `"docx"`, `"pdf"`, `"png"`, `"download"`, …
    pub kind: String,
    /// Task that produced it, when known.
    #[serde(default)]
    pub source_task: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
    /// The tool id that produced the file.
    pub producer_tool: String,
    /// Free-form provenance (input paths, source artifact ids, db query …).
    pub provenance: Value,
    pub sha256: String,
    pub size_bytes: u64,
    /// Filesystem location as given by the caller (audited via tool args).
    pub location: String,
    pub verification: ArtifactVerification,
}

impl ArtifactRecord {
    /// Result payload fragment for tool outputs.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Artifact store over the one persistence store.
pub struct ArtifactStore {
    store: Arc<dyn PersistenceStore>,
}

impl ArtifactStore {
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self { store }
    }

    /// Hash a file and persist its record. Verification state is
    /// `"verified"` with method `"sha256-recompute"` (digest computed from
    /// the bytes on disk at record time).
    pub async fn record_file(
        &self,
        path: &Path,
        kind: &str,
        producer_tool: &str,
        source_task: Option<String>,
        provenance: Value,
    ) -> Result<ArtifactRecord, String> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| format!("read `{}`: {e}", path.display()))?;
        let sha = sha256_hex(&bytes);
        let record = ArtifactRecord {
            artifact_id: format!("art-{}", uuid::Uuid::new_v4()),
            kind: kind.to_string(),
            source_task,
            created_at: Utc::now().to_rfc3339(),
            producer_tool: producer_tool.to_string(),
            provenance,
            sha256: sha,
            size_bytes: bytes.len() as u64,
            location: path.display().to_string(),
            verification: ArtifactVerification {
                state: "verified".into(),
                method: "sha256-recompute".into(),
            },
        };
        let key = encode_safe_key(&record.artifact_id);
        self.store
            .put_record(
                ARTIFACT_NS,
                &key,
                &serde_json::to_value(&record).map_err(|e| e.to_string())?,
            )
            .await
            .map_err(|e| format!("persist artifact: {e}"))?;
        Ok(record)
    }

    /// Recompute a recorded artifact's digest from disk; flips verification to
    /// `integrity_failed` in the returned copy when it no longer matches.
    pub async fn verify(&self, artifact_id: &str) -> Result<ArtifactRecord, String> {
        let key = encode_safe_key(artifact_id);
        let value = self
            .store
            .get_record(ARTIFACT_NS, &key)
            .await
            .map_err(|e| format!("load artifact: {e}"))?
            .ok_or_else(|| format!("artifact `{artifact_id}` not found"))?;
        let mut rec: ArtifactRecord =
            serde_json::from_value(value).map_err(|e| format!("artifact decode: {e}"))?;
        let bytes = tokio::fs::read(&rec.location)
            .await
            .map_err(|e| format!("read `{}`: {e}", rec.location))?;
        let sha = sha256_hex(&bytes);
        if sha == rec.sha256 && bytes.len() as u64 == rec.size_bytes {
            rec.verification = ArtifactVerification {
                state: "verified".into(),
                method: "sha256-recompute".into(),
            };
        } else {
            rec.verification = ArtifactVerification {
                state: "integrity_failed".into(),
                method: format!("expected {sha}, file digest differs"),
            };
        }
        Ok(rec)
    }

    /// All recorded artifacts (optionally filtered by `kind`).
    pub async fn list(&self, kind: Option<&str>) -> Result<Vec<ArtifactRecord>, String> {
        let rows = self
            .store
            .list_records(ARTIFACT_NS)
            .await
            .map_err(|e| format!("list artifacts: {e}"))?;
        let mut out = Vec::new();
        for (_, value) in rows {
            if let Ok(rec) = serde_json::from_value::<ArtifactRecord>(value) {
                if kind.map(|k| rec.kind == k).unwrap_or(true) {
                    out.push(rec);
                }
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }
}

/// `artifact.list` — inspect recorded artifacts and re-verify digests.
pub struct ArtifactListTool {
    pub spec: openrt_tools::ToolSpec,
    pub store: Arc<ArtifactStore>,
}

#[async_trait::async_trait]
impl openrt_tools::Tool for ArtifactListTool {
    fn spec(&self) -> &openrt_tools::ToolSpec {
        &self.spec
    }

    async fn run(
        &self,
        args: serde_json::Value,
    ) -> Result<openrt_tools::ToolOutput, openrt_tools::ToolError> {
        let kind = args.get("kind").and_then(Value::as_str);
        let verify = args.get("verify").and_then(Value::as_bool).unwrap_or(false);
        let mut rows = self
            .store
            .list(kind)
            .await
            .map_err(|e| openrt_tools::ToolError::Internal("artifact.list".into(), e))?;
        if verify {
            let mut verified = Vec::new();
            for r in rows {
                match self.store.verify(&r.artifact_id).await {
                    Ok(v) => verified.push(v),
                    Err(e) => {
                        return Ok(crate::error::fail(
                            "artifact.list",
                            crate::error::FabricError::verification_failed(
                                format!("artifact `{}`", r.artifact_id),
                                e,
                                "verify file digest against the recorded sha256",
                            ),
                        )?)
                    }
                }
            }
            rows = verified;
        }
        Ok(openrt_tools::ToolOutput::Json(json!({
            "ok": true,
            "count": rows.len(),
            "artifacts": rows.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        })))
    }
}

/// Factory for `artifact.list`.
pub fn artifact_list_tool(store: Arc<ArtifactStore>) -> Arc<dyn openrt_tools::Tool> {
    Arc::new(ArtifactListTool {
        spec: openrt_tools::ToolSpec::new(
            "artifact.list",
            "List recorded generated artifacts (id, type, task, producer, provenance, digest, location, verification) and optionally re-verify their digests.",
        )
        .with_caps(&["fabric.artifacts.read"])
        .with_schema(json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "description": "filter by artifact kind" },
                "verify": { "type": "boolean", "default": false, "description": "recompute digests and flag integrity failures" }
            }
        })),
        store,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    #[tokio::test]
    async fn record_verify_list_roundtrip() {
        let dir = std::env::temp_dir().join(format!("fabric-art-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let file = dir.join("hello.txt");
        tokio::fs::write(&file, b"hello artifact").await.unwrap();

        let store = Arc::new(MemoryStore::new()) as Arc<dyn PersistenceStore>;
        let artifacts = ArtifactStore::new(store);
        let rec = artifacts
            .record_file(
                &file,
                "download",
                "browser.download",
                None,
                json!({"url": "file:///x"}),
            )
            .await
            .unwrap();
        assert_eq!(rec.kind, "download");
        assert_eq!(rec.verification.state, "verified");
        assert_eq!(rec.sha256.len(), 64);

        // Tamper → integrity failure detected on verify.
        tokio::fs::write(&file, b"tampered").await.unwrap();
        let v = artifacts.verify(&rec.artifact_id).await.unwrap();
        assert_eq!(v.verification.state, "integrity_failed");

        // Restore → verified again; list sees it.
        tokio::fs::write(&file, b"hello artifact").await.unwrap();
        let list = artifacts.list(Some("download")).await.unwrap();
        assert_eq!(list.len(), 1);
        let v = artifacts.verify(&rec.artifact_id).await.unwrap();
        assert_eq!(v.verification.state, "verified");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn missing_file_fails_structured() {
        let store = Arc::new(MemoryStore::new()) as Arc<dyn PersistenceStore>;
        let artifacts = ArtifactStore::new(store);
        let err = artifacts
            .record_file(
                Path::new("/nonexistent/xyz"),
                "docx",
                "docx.create",
                None,
                json!({}),
            )
            .await
            .unwrap_err();
        assert!(err.contains("read"));
    }
}
