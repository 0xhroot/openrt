//! Phase 3 — model fabric + sovereign infrastructure, end-to-end through the
//! real runtime (`rt.modes`, `rt.model_registry`, `rt.router`).
//!
//! Covers the ADR-010 lifecycle (discovered → downloaded → verified → scanned
//! → active → retired) walked one hop at a time, ADR-003 mode enforcement
//! (fail-closed Sovereign default; materialization refused *before* bytes are
//! touched), fail-closed content trust (digest mismatch, known-bad scan,
//! ed25519 bundle trust), deterministic local routing, and persistence of the
//! fabric across a real `LocalFileStore` reopen.
//!
//! Deterministic and network-free: in-memory / local-file persistence, ed25519
//! keys derived from fixed seeds, mock provider.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use openrt_config::{Config, OperatingMode};
use openrt_models::mock::MockProvider;
use openrt_modes::ExecutionMode;
use openrt_persistence::{LocalFileStore, MemoryStore, PersistenceStore};
use openrt_registry::{
    curated_entry, BlobRef, LifecycleState, ManifestV1, ModelRegistry, RegistryError,
};
use openrt_router::{RouteRequest, RouteVerdict};
use openrt_runtime::{Runtime, RuntimeBuilder};
use openrt_scan::{sign_bundle, verify_signature, ArtifactKind, BundleContent, Scanner};
use openrt_verify::sha256_hex;

const ID_A: &str = "curated/llama3.1-8b";
const ID_B: &str = "curated/llama3.2-1b";
const FILE: &str = "model.safetensors";

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p3_fabric-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Deterministic signing key (fixed seed) — never a real secret.
fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn scanner(known_bad: Vec<String>) -> Scanner {
    let bundle = sign_bundle(
        &key(),
        "p3-test-key",
        BundleContent {
            generated_at: "2026-01-01T00:00:00Z".into(),
            scheme_version: 1,
            known_bad,
            safetensors_policy: None,
        },
    );
    let mut s = Scanner::new();
    s.add_bundle(bundle);
    s
}

fn manifest(id: &str, bytes: &[u8]) -> ManifestV1 {
    ManifestV1 {
        schema_version: 1,
        artifact_id: id.into(),
        kind: ArtifactKind::Binary,
        blobs: vec![BlobRef {
            file_key: FILE.into(),
            digest: sha256_hex(bytes),
            size_bytes: bytes.len() as u64,
            location: openrt_registry::BlobLocation::Stored,
        }],
        base_digest: None,
        created_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn files(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([(FILE.to_string(), bytes.to_vec())])
}

async fn runtime(config: Config, store: Arc<dyn PersistenceStore>) -> Arc<Runtime> {
    RuntimeBuilder::new()
        .config(config)
        .store(store)
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .build()
        .await
        .unwrap()
}

fn provisioned() -> Config {
    Config {
        mode: OperatingMode::Provisioning,
        ..Default::default()
    }
}

async fn import_scanned(
    registry: &ModelRegistry,
    id: &str,
    bytes: &[u8],
    scan: Option<(&Scanner, u32)>,
) -> openrt_registry::LifecycleRecord {
    registry
        .import_local(manifest(id, bytes), &files(bytes), "test", scan)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Happy path: full lifecycle through the wired runtime.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provision_import_activate_route_retire_end_to_end() {
    let rt = runtime(provisioned(), Arc::new(MemoryStore::new())).await;
    let reg = &rt.model_registry;

    // Config seeded the initial (audited) mode; fail-closed default replaced.
    assert_eq!(
        rt.modes.current().await.unwrap().mode,
        ExecutionMode::Provisioning
    );

    let bytes = b"phase-3 weights (real, verified bytes)";
    let sc = scanner(vec![]);
    let lr = import_scanned(reg, ID_A, bytes, Some((&sc, 1))).await;
    assert_eq!(
        lr.state,
        LifecycleState::Scanned,
        "scan gate must reach Scanned"
    );
    assert!(reg.manifest(ID_A).await.unwrap().is_some());

    // Cannot activate from an un-scanned/un-verified state (walked one hop at
    // a time); here it is already Scanned, so activation is the next hop.
    reg.activate(ID_A, "test").await.unwrap();
    assert!(reg.is_active(ID_A).await.unwrap());

    let verdict = rt
        .router
        .route(
            reg,
            &RouteRequest {
                artifact_id: Some(ID_A.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    match verdict {
        RouteVerdict::Selected(d) => {
            assert_eq!(d.artifact_id, ID_A);
            assert_eq!(d.rank, 1);
            assert_eq!(d.mode, ExecutionMode::Provisioning);
        }
        RouteVerdict::Refused { summary, .. } => panic!("active artifact refused: {summary}"),
    }

    reg.retire(ID_A, "test", "cleanup").await.unwrap();
    assert!(!reg.is_active(ID_A).await.unwrap());
    let refused = rt
        .router
        .route(
            reg,
            &RouteRequest {
                artifact_id: Some(ID_A.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(refused, RouteVerdict::Refused { .. }));

    // Every lifecycle hop and the mode seed are on one intact audit chain.
    assert!(rt.audit.verify().await.unwrap() >= 5);
}

// ---------------------------------------------------------------------------
// Sovereign: materialization refused before any byte is touched.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sovereign_refuses_materialization_before_touching_bytes() {
    // No persisted mode: the runtime is fail-closed Sovereign.
    let rt = runtime(Config::default(), Arc::new(MemoryStore::new())).await;
    assert_eq!(
        rt.modes.current().await.unwrap().mode,
        ExecutionMode::Sovereign
    );
    assert!(rt.modes.ensure_network_allowed().await.is_err());

    let bytes = b"sovereign must never ingest these bytes";
    let err = rt
        .model_registry
        .import_local(manifest(ID_A, bytes), &files(bytes), "test", None)
        .await
        .unwrap_err();
    assert!(matches!(err, RegistryError::Mode(_)), "got {err:?}");

    // No blob was persisted and no manifest published: the refused import
    // left the curated artifact at its derived `Discovered` state only.
    assert!(rt.model_registry.storage_used_bytes().await.unwrap() == 0);
    assert!(rt.model_registry.manifest(ID_A).await.unwrap().is_none());
    assert_eq!(
        rt.model_registry
            .lifecycle(ID_A)
            .await
            .unwrap()
            .unwrap()
            .state,
        LifecycleState::Discovered
    );

    // An explicit operator transition unlocks it (audited), proving the gate
    // was the mode, not a broken pipeline.
    rt.modes
        .transition("operator", ExecutionMode::Provisioning, "bootstrap")
        .await
        .unwrap();
    let sc = scanner(vec![]);
    import_scanned(&rt.model_registry, ID_A, bytes, Some((&sc, 1))).await;
}

// ---------------------------------------------------------------------------
// Fail-closed content trust.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_fails_closed_on_tampered_bytes() {
    let rt = runtime(provisioned(), Arc::new(MemoryStore::new())).await;
    let original = b"honest bytes";
    let tampered = files(b"malicious bytes");
    let err = rt
        .model_registry
        .import_local(manifest(ID_A, original), &tampered, "test", None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, RegistryError::DigestMismatch { .. }),
        "got {err:?}"
    );
    // Nothing was published or persisted for the artifact.
    assert!(rt.model_registry.manifest(ID_A).await.unwrap().is_none());
    assert_eq!(rt.model_registry.storage_used_bytes().await.unwrap(), 0);
}

#[tokio::test]
async fn import_fails_closed_on_known_bad_hash() {
    let rt = runtime(provisioned(), Arc::new(MemoryStore::new())).await;
    let bytes = b"revoked artifact bytes";
    let sc = scanner(vec![sha256_hex(bytes)]);
    let err = rt
        .model_registry
        .import_local(manifest(ID_A, bytes), &files(bytes), "test", Some((&sc, 1)))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RegistryError::ScanBlocked { .. }),
        "got {err:?}"
    );
    // Bytes/manifest exist (import got as far as Verified) but the artifact is
    // not activatable — fail-closed, no scan pass, no silent activation.
    let lr = rt.model_registry.lifecycle(ID_A).await.unwrap().unwrap();
    assert_eq!(lr.state, LifecycleState::Verified);
    assert!(!rt.model_registry.is_active(ID_A).await.unwrap());
}

#[tokio::test]
async fn bundle_signature_trust_is_fail_closed() {
    let bundle = sign_bundle(
        &key(),
        "p3-test-key",
        BundleContent {
            generated_at: "2026-01-01T00:00:00Z".into(),
            scheme_version: 1,
            known_bad: vec![],
            safetensors_policy: None,
        },
    );
    // The trusted key verifies.
    verify_signature(&bundle, &key().verifying_key()).unwrap();
    // A different key does not.
    let other = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
    assert!(verify_signature(&bundle, &other).is_err());
}

// ---------------------------------------------------------------------------
// Routing determinism + honest metadata.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn router_ranks_lightest_installed_first_deterministically() {
    let rt = runtime(provisioned(), Arc::new(MemoryStore::new())).await;
    let reg = &rt.model_registry;
    let sc = scanner(vec![]);
    // B is deliberately lighter than A.
    import_scanned(reg, ID_A, &vec![0u8; 4096], Some((&sc, 1))).await;
    import_scanned(reg, ID_B, &vec![0u8; 512], Some((&sc, 1))).await;
    reg.activate(ID_A, "test").await.unwrap();
    reg.activate(ID_B, "test").await.unwrap();

    let request = RouteRequest {
        query: Some("llama".into()),
        ..Default::default()
    };
    let a = rt.router.route(reg, &request).await.unwrap();
    let b = rt.router.route(reg, &request).await.unwrap();
    let pick = |v: &RouteVerdict| match v {
        RouteVerdict::Selected(d) => d.artifact_id.clone(),
        RouteVerdict::Refused { summary, .. } => panic!("refused: {summary}"),
    };
    assert_eq!(pick(&a), ID_B, "lightest installed artifact must win");
    assert_eq!(pick(&a), pick(&b), "routing must be deterministic");
}

#[tokio::test]
async fn unknown_artifact_gets_honest_neutral_metadata() {
    let rt = runtime(provisioned(), Arc::new(MemoryStore::new())).await;
    let reg = &rt.model_registry;
    let custom = "local/custom-tiny";
    let sc = scanner(vec![]);
    import_scanned(reg, custom, b"x", Some((&sc, 1))).await;
    reg.activate(custom, "test").await.unwrap();
    let card = reg.status(custom).await.unwrap();
    assert_eq!(card.display_name, custom, "no fabricated display name");
    assert_eq!(card.family, "unknown");
    assert!(card.tags.is_empty());
    // A curated id reuses the catalog's descriptive metadata.
    let curated = curated_entry(ID_A).unwrap();
    import_scanned(reg, ID_A, b"y", Some((&sc, 1))).await;
    let card_a = reg.status(ID_A).await.unwrap();
    assert_eq!(card_a.display_name, curated.display_name);
}

// ---------------------------------------------------------------------------
// Lifecycle DAG + manifest immutability.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manifest_immutable_and_no_lifecycle_hop_is_skipped() {
    let rt = runtime(provisioned(), Arc::new(MemoryStore::new())).await;
    let reg = &rt.model_registry;
    let bytes = b"manifest bytes";
    let m = manifest(ID_A, bytes);
    reg.publish_manifest(&m).await.unwrap();
    let again = reg.publish_manifest(&m).await.unwrap_err();
    assert!(matches!(again, RegistryError::ManifestImmutable(_)));

    // Discovered -> Active is a skipped hop and must be rejected fail-closed.
    let err = reg.activate(ID_A, "test").await.unwrap_err();
    assert!(
        matches!(err, RegistryError::InvalidTransition { .. }),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Persistence: the fabric survives a real store reopen.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fabric_state_survives_local_file_store_reopen() {
    let dir = tmp_dir("reopen");
    let bytes = b"persisted weights";

    let store1: Arc<dyn PersistenceStore> = Arc::new(LocalFileStore::open(&dir).await.unwrap());
    let rt1 = runtime(provisioned(), store1).await;
    let sc = scanner(vec![]);
    import_scanned(&rt1.model_registry, ID_A, bytes, Some((&sc, 1))).await;
    rt1.model_registry.activate(ID_A, "test").await.unwrap();
    // Explicitly lock the host down again before closing; the operator choice
    // must outlive the restart.
    rt1.modes
        .transition("operator", ExecutionMode::Sovereign, "lock down")
        .await
        .unwrap();
    drop(rt1);

    // Reopen the same directory: no config seed overrides the persisted mode.
    let store2: Arc<dyn PersistenceStore> = Arc::new(LocalFileStore::open(&dir).await.unwrap());
    let rt2 = runtime(provisioned(), store2).await;
    let modes = rt2.modes.status().await.unwrap();
    assert_eq!(
        modes.current,
        ExecutionMode::Sovereign,
        "operator choice wins"
    );
    assert!(modes.transitions.iter().any(|t| t.to == "sovereign"));

    assert!(rt2.model_registry.is_active(ID_A).await.unwrap());
    let verdict = rt2
        .router
        .route(
            &rt2.model_registry,
            &RouteRequest {
                artifact_id: Some(ID_A.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(verdict, RouteVerdict::Selected(_)));
    // Blob bytes round-tripped intact (store-integrity checked on read).
    let digest = rt2
        .model_registry
        .manifest(ID_A)
        .await
        .unwrap()
        .unwrap()
        .blobs[0]
        .digest
        .clone();
    let blob = rt2.model_registry.blob(&digest).await.unwrap().unwrap();
    assert_eq!(blob, bytes);
}
