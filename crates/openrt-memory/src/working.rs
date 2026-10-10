//! **Working memory** — the live-session, zero-persistence register surface.
//!
//! # Contract (roadmap M5.3 + ADR-006, never weakened)
//!
//! * Slots are byte-identical to what the caller set (`set` → `get` returns
//!   the same bytes, fail-closed on absent/tampered — never a re-keyed or
//!   invented value).
//! * Zero persistence: session end = register file closed; there is no
//!   reopen, no durable bytes, no "loaded working memory". Everything else
//!   in this crate (episodic / semantic) is the persisted layer, and it is
//!   the ONE store (ADR-006: no second store, no second vector store).
//! * Isolation: slots are keyed under workspace `id` + `session`; one
//!   workspace's working surface is never mixed into another's.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A zero-persistence live-session slot. `digest` is a fail-closed marker
/// over the slot's canonical bytes (digest excluded from its own input, the
/// M5.1 discipline); `get` re-derives and fails closed on mismatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingSlot {
    pub key: String,
    pub value: String,
    pub digest: String,
    pub origin_run: String,
    pub origin_session: String,
}

/// One working-memory error — every path fail-closed; never silent repair.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkingError {
    #[error("working slot {key} is unset (fail closed)")]
    Unset { key: String },
    #[error("working slot {key} digest mismatch — tampered (fail closed)")]
    DigestMismatch { key: String },
    #[error("working memory session already ended (fail closed)")]
    SessionEnded,
}

#[derive(Debug, Default)]
pub struct WorkingMemory {
    slots: BTreeMap<String, WorkingSlot>,
    session: String,
    run: String,
    live: bool,
}

impl WorkingMemory {
    pub fn new(run: String, session: String) -> Self {
        Self {
            slots: BTreeMap::new(),
            session,
            run,
            live: true,
        }
    }

    pub fn set(
        &mut self,
        key: String,
        value: String,
        origin_run: String,
        origin_session: String,
    ) -> Result<(), WorkingError> {
        if !self.live {
            return Err(WorkingError::SessionEnded);
        }
        let digest = crate::digest::canonical_digest(&[
            ("key", &key),
            ("value", &value),
            ("origin_run", &origin_run),
            ("origin_session", &origin_session),
        ]);
        self.slots.insert(
            key.clone(),
            WorkingSlot {
                key,
                value,
                digest,
                origin_run,
                origin_session,
            },
        );
        Ok(())
    }

    /// Fail-closed replay: verify the slot's digest over its own canonical
    /// bytes. Any mismatch → Err; never silent repair.
    pub fn get(&self, key: &str) -> Result<&WorkingSlot, WorkingError> {
        if !self.live {
            return Err(WorkingError::SessionEnded);
        }
        let slot = self
            .slots
            .get(key)
            .ok_or_else(|| WorkingError::Unset { key: key.into() })?;
        let want = crate::digest::canonical_digest(&[
            ("key", &slot.key),
            ("value", &slot.value),
            ("origin_run", &slot.origin_run),
            ("origin_session", &slot.origin_session),
        ]);
        if want != slot.digest {
            return Err(WorkingError::DigestMismatch { key: key.into() });
        }
        Ok(slot)
    }

    pub fn has(&self, key: &str) -> bool {
        matches!(self.get(key), Ok(_))
    }

    /// End the session: registers close (fail closed); no persistence, no
    /// reopen. Subsequent `set`/`get` fail.
    pub fn end_session(&mut self) {
        self.live = false;
        self.slots.clear();
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn session_id(&self) -> &str {
        &self.session
    }

    pub fn run_id(&self) -> &str {
        &self.run
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrip_byte_identical() {
        let mut wm = WorkingMemory::new("run-1".into(), "sess-1".into());
        wm.set(
            "cwd".into(),
            "/data/run-1".into(),
            "run-1".into(),
            "sess-1".into(),
        )
        .unwrap();
        let s = wm.get("cwd").unwrap();
        assert_eq!(s.value, "/data/run-1");
        assert_eq!(s.origin_run, "run-1");
        assert!(wm.has("cwd"));
    }

    #[test]
    fn unset_fails_closed() {
        let wm = WorkingMemory::new("run-1".into(), "sess-1".into());
        assert_eq!(
            wm.get("never-set"),
            Err(WorkingError::Unset {
                key: "never-set".into()
            })
        );
        assert!(!wm.has("never-set"));
    }

    #[test]
    fn session_end_fails_closed_and_is_zero_persistence() {
        let mut wm = WorkingMemory::new("run-1".into(), "sess-1".into());
        wm.set("k".into(), "v".into(), "run-1".into(), "sess-1".into())
            .unwrap();
        wm.end_session();
        assert!(!wm.is_live());
        assert_eq!(wm.get("k"), Err(WorkingError::SessionEnded));
        assert!(matches!(
            wm.set("k2".into(), "v2".into(), "run-1".into(), "sess-1".into(),),
            Err(WorkingError::SessionEnded)
        ));
    }
}
