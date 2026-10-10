//! Execution-scoped shared blackboard (P2.3, namespace `exec.blackboard`).
//!
//! The blackboard is the coordination memory of a multi-agent run: role sub
//! agents (planner, researcher, coder, verifier, critic) read and write shared
//! facts, artifacts and messages here instead of threading state through every
//! tool call. It is deliberately "dumb storage":
//!
//! * a validated string-key → JSON-value map;
//! * no reactions, watchers or ownership — the orchestrator decides who may
//!   write what (ownership lives in `openrt-orch`);
//! * optional write-through persistence through [`PersistenceStore`]
//!   (ADR-006 single store, namespace keys validated to `a-z0-9._-`);
//! * a serde [`BlackboardSnapshot`] for checkpoints (P2.6) and deterministic
//!   ingest into the ADR-002 audit chain.
//!
//! When constructed with [`Blackboard::with_store`] every `put`/`remove` is
//! persisted immediately, and `list_records` hydrates the map at open, so two
//! blackboards over the same store/namespace observe the same durable state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use openrt_persistence::{PersistenceError, PersistenceStore};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default persistence namespace (OWNERSHIP: `exec.blackboard`).
pub const BLACKBOARD_NS: &str = "blackboard";

/// Only `[a-z0-9._-]` keys are accepted so a key can never escape the
/// namespace directory on the durable backend.
fn validate_key(key: &str) -> Result<(), PersistenceError> {
    let ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(PersistenceError::Corrupt(
            "blackboard".into(),
            format!("invalid key `{key}` (must be a-z0-9._-)"),
        ));
    }
    Ok(())
}

/// A cloneable, serde-able point-in-time copy of a blackboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BlackboardSnapshot {
    /// Persistence namespace this snapshot was taken from (`blackboard`).
    pub namespace: String,
    /// Sorted `(key, value)` pairs — order is deterministic for checkpoints.
    pub entries: Vec<(String, Value)>,
}

/// Shared, validated, optionally durable key → JSON-value workspace.
#[derive(Default)]
pub struct Blackboard {
    inner: Mutex<HashMap<String, Value>>,
    store: Option<Arc<dyn PersistenceStore>>,
    namespace: String,
}

impl Blackboard {
    /// A purely in-memory blackboard (tests, ephemeral demo runs).
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            store: None,
            namespace: BLACKBOARD_NS.to_string(),
        }
    }

    /// Open a blackboard over `store`, hydrating any records already stored
    /// under `namespace` (default [`BLACKBOARD_NS`]). Every later write is
    /// persisted through the store.
    pub async fn with_store(
        store: Arc<dyn PersistenceStore>,
        namespace: &str,
    ) -> Result<Self, PersistenceError> {
        let out = Self {
            inner: Mutex::new(HashMap::new()),
            store: Some(store.clone()),
            namespace: namespace.to_string(),
        };
        for (key, value) in store.list_records(namespace).await? {
            validate_key(&key)?;
            out.inner.lock().unwrap().insert(key, value);
        }
        Ok(out)
    }

    /// Insert or replace a value. Persisted when a store is attached.
    pub async fn put(&self, key: &str, value: Value) -> Result<(), PersistenceError> {
        validate_key(key)?;
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_string(), value.clone());
        if let Some(store) = &self.store {
            store.put_record(&self.namespace, key, &value).await?;
        }
        Ok(())
    }

    /// Read a value (None when absent).
    pub fn get(&self, key: &str) -> Result<Option<Value>, PersistenceError> {
        validate_key(key)?;
        Ok(self.inner.lock().unwrap().get(key).cloned())
    }

    /// Remove a key (no-op when absent). Persisted when a store is attached.
    pub async fn remove(&self, key: &str) -> Result<(), PersistenceError> {
        validate_key(key)?;
        self.inner.lock().unwrap().remove(key);
        if let Some(store) = &self.store {
            store.delete_record(&self.namespace, key).await?;
        }
        Ok(())
    }

    /// All keys, sorted for determinism.
    pub fn keys(&self) -> Vec<String> {
        let mut v: Vec<_> = self.inner.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Deterministic point-in-time copy (sorted entries).
    pub fn snapshot(&self) -> BlackboardSnapshot {
        let mut entries: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        BlackboardSnapshot {
            namespace: self.namespace.clone(),
            entries,
        }
    }

    /// Overwrite the whole blackboard from a snapshot (used on recover). With a
    /// store attached, any keys removed by the restore are deleted there too.
    pub async fn restore(&self, snapshot: &BlackboardSnapshot) -> Result<(), PersistenceError> {
        let mut want: HashMap<String, Value> = HashMap::new();
        for (key, value) in &snapshot.entries {
            validate_key(key)?;
            want.insert(key.clone(), value.clone());
        }
        // Compute the delta under the lock, then persist without holding it.
        let missing: Vec<String> = {
            let mut inner = self.inner.lock().unwrap();
            let missing = inner
                .keys()
                .filter(|k| !want.contains_key(*k))
                .cloned()
                .collect();
            *inner = want;
            missing
        };
        if let Some(store) = &self.store {
            for key in &missing {
                store.delete_record(&self.namespace, key).await?;
            }
            for (key, value) in &snapshot.entries {
                store.put_record(&self.namespace, key, value).await?;
            }
        }
        Ok(())
    }

    /// Serialize the snapshot to JSON (checkpoint-friendly).
    pub fn to_json(&self) -> Result<String, PersistenceError> {
        Ok(serde_json::to_string_pretty(&self.snapshot())?)
    }
}

impl From<BlackboardSnapshot> for Blackboard {
    fn from(snapshot: BlackboardSnapshot) -> Self {
        let mut inner = HashMap::new();
        for (k, v) in snapshot.entries {
            inner.insert(k, v);
        }
        Blackboard {
            inner: Mutex::new(inner),
            store: None,
            namespace: snapshot.namespace,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(v: &str) -> Value {
        serde_json::json!({ "v": v })
    }

    #[tokio::test]
    async fn put_get_remove_keys() {
        let bb = Blackboard::new();
        bb.put("facts.app", key("a")).await.unwrap();
        bb.put("facts.bpp", key("b")).await.unwrap();
        assert_eq!(bb.get("facts.app").unwrap(), Some(key("a")));
        assert_eq!(
            bb.keys(),
            vec!["facts.app".to_string(), "facts.bpp".to_string()]
        );
        assert_eq!(bb.len(), 2);
        bb.remove("facts.app").await.unwrap();
        assert_eq!(bb.get("facts.app").unwrap(), None);
        assert!(matches!(
            bb.put("../escape", key("x")).await,
            Err(PersistenceError::Corrupt(_, _))
        ));
        assert!(matches!(
            bb.get("").unwrap_err(),
            PersistenceError::Corrupt(_, _)
        ));
    }

    #[tokio::test]
    async fn snapshot_restore_round_trip() {
        let bb = Blackboard::new();
        bb.put("a", key("1")).await.unwrap();
        bb.put("b", key("2")).await.unwrap();
        let json = bb.to_json().unwrap();
        let parsed: BlackboardSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, bb.snapshot());

        let other = Blackboard::from(parsed);
        assert_eq!(other.get("b").unwrap(), Some(key("2")));
        assert_eq!(other.len(), 2);
    }

    #[tokio::test]
    async fn durable_blackboard_write_through_and_reopen() {
        let dir = std::env::temp_dir().join(format!("openrt-bb-{}", uuid::Uuid::new_v4()));
        let store: Arc<dyn PersistenceStore> = Arc::new(
            openrt_persistence::LocalFileStore::open(&dir)
                .await
                .unwrap(),
        );

        {
            let bb = Blackboard::with_store(store.clone(), BLACKBOARD_NS)
                .await
                .unwrap();
            bb.put("fact.1", key("x")).await.unwrap();
            bb.put("fact.2", key("y")).await.unwrap();
        }
        {
            let bb = Blackboard::with_store(store.clone(), BLACKBOARD_NS)
                .await
                .unwrap();
            assert_eq!(bb.get("fact.1").unwrap(), Some(key("x")));
            assert_eq!(bb.len(), 2);
            bb.remove("fact.1").await.unwrap();
        }
        {
            let bb = Blackboard::with_store(store.clone(), BLACKBOARD_NS)
                .await
                .unwrap();
            assert_eq!(bb.len(), 1);
            assert_eq!(bb.get("fact.2").unwrap(), Some(key("y")));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
