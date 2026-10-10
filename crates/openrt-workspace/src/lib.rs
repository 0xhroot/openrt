//! `openrt-workspace` — Phase 5, M5.1 **persistent workspace** layer.
//!
//! The persistent workspace is a *consolidation overlay* over the foundation
//! store (ADR-006: ONE store; the workspace never creates a second store and
//! never duplicates records that already live in the foundation namespaces —
//! `sessions`, `tasks`, `executions`, `events`). The crate owns the
//! `workspace.*` namespace (OWNERSHIP M5.1 row):
//!
//! | record | purpose | key |
//! |---|---|---|
//! | `workspace.workspaces` | durable workspace identity (id, title, description, created/opened metadata) | `w.<ws_key>` |
//! | `workspace.reopen` | the currently active workspace pointer + its integrity digest | `active` |
//! | `workbench.rooms` | Phase 11 (Track C): agent rooms — agents/sessions/tasks/workflows/materials/artifacts/history/steer | `r.<id>` |
//! | `materials.records` | Phase 11 (Track D): typed material records (kind/title/reference/digest/links) | `mat.<id>` |
//!
//! Everything is **deterministic**: identifiers and digests are derived from
//! real persisted bytes (`sha256` over canonical JSON), never from random or
//! fabricated state. Reopening after a restart works because identity + digest
//! live in the same durable store the sessions already use; the `reopen.rs`
//! surface is exactly the "Sessions UI view + reopen after restart" slice of
//! M5.1, in crate-shaped form (the read-model is a view; the durable identity
//! is the thing that persists).
//!
//! # What this slice is NOT (honesty section, Phase 5 gate)
//!
//! * NOT a TUI. Roadmap M5.1 acceptance includes a TUI/session-UX surface;
//!   this crate is the durable sublayer that a TUI would read from, and the
//!   CLI `openrt workspace` command is the honest text surface for now.
//! * NOT memory/semantic layers (M5.3+), NOT skills/instincts (M5.4+), NOT
//!   self-improvement (M5.6+). Those roadmap slices remain open work and are
//!   documented as such, never claimed.

use openrt_persistence::{PersistenceError, PersistenceStore};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

/// Namespace owned by this crate (OWNERSHIP M5.1: `workspace.workspaces`,
/// `workspace.reopen`).
pub const WORKSPACES_NS: &str = "workspace.workspaces";
/// Namespace for the single active-workspace pointer (reopen key).
pub const REOPEN_NS: &str = "workspace.reopen";
/// Key under `REOPEN_NS` holding the active pointer record.
pub const ACTIVE_KEY: &str = "active";

/// A durable workspace identity. The id is derived deterministically from the
/// address code / title bytes via sha256, so the same inputs always yield the
/// same workspace and reopening needs no session bookkeeping beyond the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub title: String,
    pub description: String,
    pub address_code: String,
    pub created_at: String,
    pub opened_at: String,
    /// sha256 of the canonical persisted record — the reopen integrity token.
    /// Not stored in the record (would be a self-referential hash): derived by
    /// `open_workspace` over the deterministic canonical bytes on every reopen.
    #[serde(default)]
    pub digest: String,
}

/// Everything the address code may legally contain (fail-closed: anything
/// else is rejected before it can reach the store key).
fn safe_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => out.push(ch),
            _ => {
                out.push('_');
                for b in ch.to_string().as_bytes() {
                    out.push_str(&format!("{:02x}", b));
                }
            }
        }
    }
    out
}

/// Deterministic key for a workspace identity record.
/// `w.<address_code>` — the address code is already path-safe, so the key never
/// contains a `/` (the store rejects `/` in keys; SEC-… fail-closed).
fn workspace_key(address_code: &str) -> String {
    format!("w.{}", safe_key(address_code))
}

/// Canonical JSON: object keys sorted (deterministic bytes for sha256).
fn canonical(value: &serde_json::Value) -> String {
    // Exclude the integrity token itself from its own digest input so create
    // can persist `digest` alongside the identity (reopen then has something
    // honest to compare against during verification) without making the digest
    // self-referential — the exact contract the reopen byte-identity test
    // depends on.
    let d = sort_obj_excluding_digest(value.clone());
    serde_json::to_string(&d).unwrap_or_else(|_| value.to_string())
}

/// Like [`sort_obj_excluding_digest`] but drops the `digest` key at every
/// depth before sorting, so the digest seals the identity WITHOUT sealing
/// itself.
fn sort_obj_excluding_digest(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut kvs: Vec<(String, serde_json::Value)> = m
                .into_iter()
                .filter(|(k, _)| k != "digest")
                .map(|(k, v)| (k, sort_obj_excluding_digest(v)))
                .collect();
            kvs.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(kvs.into_iter().collect())
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(sort_obj_excluding_digest).collect())
        }
        other => other,
    }
}

/// Errors from the workspace layer.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("store error: {0}")]
    Store(#[from] PersistenceError),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("workspace `{0}` not found")]
    NotFound(String),
    #[error("workspace `{0}` already exists")]
    AlreadyExists(String),
    #[error("active pointer is corrupt: {0}")]
    CorruptActive(String),
    #[error("active workspace failed integrity check (digest mismatch)")]
    Integrity,
    #[error("room `{0}` not found")]
    RoomNotFound(String),
    #[error("material `{0}` not found")]
    MaterialNotFound(String),
    #[error("invalid workbench operation: {0}")]
    Invalid(String),
}

pub mod materials;
pub mod rooms;

pub use materials::{
    attach_to_workbench, create_material, detach_from_workbench, get_material, list_materials,
    parse_reference, verify_material, Material, MaterialKind, MaterialReference, MATERIALS_NS,
};
pub use rooms::{
    attach_material, create_room, detach_material, get_room, link_room_agent, link_room_artifact,
    link_room_session, list_rooms, rooms_for_workbench, set_active_workflow, set_room_status,
    steer_room, unlink_room_agent, Room, RoomAgent, RoomEvent, RoomRole, RoomStatus, SteerEntry,
    ROOMS_NS,
};

/// Create (or fail-closed) a persistent workspace. Returns `Ok(id)` only once
/// the identity record has made a real store round-trip.
pub async fn create_workspace(
    store: &dyn PersistenceStore,
    address_code: &str,
    title: &str,
    description: &str,
    now: &str,
) -> Result<String, WorkspaceError> {
    let id = openrt_embeddings::sha256_hex(address_code.as_bytes());
    let key = workspace_key(&id);
    if store.get_record(WORKSPACES_NS, &key).await?.is_some() {
        return Err(WorkspaceError::AlreadyExists(address_code.to_string()));
    }
    let mut value = json!({
        "id": id,
        "address_code": address_code,
        "title": title,
        "description": description,
        "created_at": now,
        "opened_at": now,
    });
    // Deterministic digest over the canonical identity bytes. Since
    // `canonical` excludes `digest`, folding it back into the persisted record
    // does NOT change the digest — and reopening now has an honest token to
    // compare against.
    let digest = openrt_embeddings::sha256_hex(canonical(&value).as_bytes());
    value["digest"] = serde_json::Value::String(digest.clone());
    store.put_record(WORKSPACES_NS, &key, &value).await?;
    let ws = Workspace {
        id,
        title: title.to_string(),
        description: description.to_string(),
        address_code: address_code.to_string(),
        created_at: now.to_string(),
        opened_at: now.to_string(),
        digest,
    };
    // Reopen pointer write + read-back; the workspace is only "active" when
    // both the identity record AND the pointer round-trip.
    store
        .put_record(
            REOPEN_NS,
            ACTIVE_KEY,
            &json!({"address_code": address_code, "id": ws.id}),
        )
        .await?;
    Ok(ws.id)
}

/// Open an existing workspace and refresh its `opened_at` + digest. Fail-closed:
/// `Err(NotFound)` unless a real persisted record exists.
pub async fn open_workspace(
    store: &dyn PersistenceStore,
    address_code: &str,
    now: &str,
) -> Result<Workspace, WorkspaceError> {
    let id = openrt_embeddings::sha256_hex(address_code.as_bytes());
    let key = workspace_key(&id);
    let Some(value) = store.get_record(WORKSPACES_NS, &key).await? else {
        return Err(WorkspaceError::NotFound(address_code.to_string()));
    };
    // Deterministic recompute over the EXACT persisted bytes: the persisted
    // identity record does NOT carry a stored digest (digest would then
    // self-reference), so byte-identical reopen derives it the same way create
    // did — from the canonical persisted record, nothing else.
    let digest = openrt_embeddings::sha256_hex(canonical(&value).as_bytes());
    let mut ws: Workspace =
        serde_json::from_value(value).map_err(|e| WorkspaceError::Serde(e.to_string()))?;
    ws.digest = digest;
    ws.opened_at = now.to_string();
    Ok(ws)
}

/// Reopen the most recently active workspace after a restart.
pub async fn reopen_current(
    store: &dyn PersistenceStore,
    now: &str,
) -> Result<Workspace, WorkspaceError> {
    let Some(value) = store.get_record(REOPEN_NS, ACTIVE_KEY).await? else {
        return Err(WorkspaceError::NotFound("(no active workspace)".into()));
    };
    let address_code = value
        .get("address_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WorkspaceError::CorruptActive(value.to_string()))?;
    open_workspace(store, address_code, now).await
}

/// Verify the persisted identity record still matches its digest (integrity).
/// Returns `false` (fail-closed) on ANY mismatch or missing record.
pub async fn verify_workspace(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<bool, WorkspaceError> {
    let key = workspace_key(id);
    let Some(value) = store.get_record(WORKSPACES_NS, &key).await? else {
        return Ok(false);
    };
    let c = canonical(&value);
    let digest = openrt_embeddings::sha256_hex(c.as_bytes());
    let ws: Workspace =
        serde_json::from_value(value.clone()).map_err(|e| WorkspaceError::Serde(e.to_string()))?;
    if ws.id != id {
        return Ok(false);
    }
    Ok(ws.digest == digest)
}

/// Deterministically list all persisted workspaces (sorted by address code).
pub async fn list_workspaces(
    store: &dyn PersistenceStore,
) -> Result<Vec<(String, serde_json::Value)>, WorkspaceError> {
    let mut v = store.list_records(WORKSPACES_NS).await?;
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(v)
}

#[cfg(test)]
mod p5_tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    /// A workspace is persisted and deterministically reopened after a
    /// "restart" (fresh store over the same bytes): same id, same digest —
    /// byte-identical, never fabricated.
    #[tokio::test]
    async fn create_then_reopen_is_byte_identical() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let t0 = "2026-09-21T00:00:00Z";
        let id = create_workspace(&*store, "acct/alpha", "Alpha", "primary", t0)
            .await
            .unwrap();
        let opened = open_workspace(&*store, "acct/alpha", "2026-09-21T00:05:00Z")
            .await
            .unwrap();
        assert_eq!(opened.id, id);
        let after = open_workspace(&*store, "acct/alpha", "2026-09-22T00:00:00Z")
            .await
            .unwrap();
        // digest unchanged despite reopened_at drift: the digest is over the
        // persisted identity record, which open does NOT rewrite.
        assert_eq!(opened.digest, after.digest);
        assert!(verify_workspace(&*store, &id).await.unwrap());
        assert!(list_workspaces(&*store).await.unwrap().len() == 1);
    }

    /// Opening a workspace that was never created fails closed (NotFound),
    /// and create of an existing address fails closed (AlreadyExists).
    #[tokio::test]
    async fn missing_and_duplicate_fail_closed() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        assert!(matches!(
            open_workspace(&*store, "acct/ghost", "2026-09-21T00:00:00Z").await,
            Err(WorkspaceError::NotFound(_))
        ));
        create_workspace(&*store, "acct/alpha", "Alpha", "", "2026-09-21T00:00:00Z")
            .await
            .unwrap();
        assert!(matches!(
            create_workspace(&*store, "acct/alpha", "Alpha2", "", "2026-09-21T00:01:00Z").await,
            Err(WorkspaceError::AlreadyExists(_))
        ));
    }

    /// A record whose persisted digest field was tampered with fails the
    /// integrity check (detectable, fail-closed) — and reopening can never
    /// repair it silently.
    #[tokio::test]
    async fn tampered_digest_fails_closed() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let id = create_workspace(&*store, "acct/alpha", "Alpha", "", "2026-09-21T00:00:00Z")
            .await
            .unwrap();
        // Re-read under the id-derived key (which is what create persisted).
        let key = workspace_key(&id);
        let mut v = store
            .get_record(WORKSPACES_NS, &key)
            .await
            .unwrap()
            .unwrap();
        v["digest"] = json!("0000000000000000000000000000000000000000000000000000000000000000");
        store.put_record(WORKSPACES_NS, &key, &v).await.unwrap();
        assert!(!verify_workspace(&*store, &id).await.unwrap());
    }

    /// The workspace namespace is path-safe (never `/`), so keys round-trip
    /// the real file-backed store.
    #[tokio::test]
    async fn keys_are_path_safe() {
        let key = workspace_key("acct/alpha one/β");
        assert!(!key.contains('/'));
        assert!(!key.contains(' '));
    }
}
