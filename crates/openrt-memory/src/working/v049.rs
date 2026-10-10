//! Working memory — zero-persistence scratch **slots** bound to the live
//! session (registers discipline).
//!
//! A [`WorkingSlot`] is named + quoted **byte-exactly** as the caller set it.
//! There is no neural layer, no semantic "similarity", no invented fill-in.
//! `flush` writes the slot's final bytes as a *citation seed* to its origin
//! session run (once, deterministically), so a later episodic/semantic recall
//! can cite back to this exact session.
//!
//! # Fail-closed
//!
//! * `get` of an unset slot → [`WorkingError::Missing`] (never a blank guess).
//! * `get` after `flush` → still `Missing` (working slot does not survive
//!   flush — it was *scratch*).
//! * `set` after `flush` → [`WorkingError::Frozen`] (a flushed slot can no
//!   longer be silently rewritten; the citation seed is sealed).

use serde::{Deserialize, Serialize};

use crate::WORKING_NS;

/// Error for the working-memory layer (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkingError {
    #[error("no working slot named `{0}` is set in this session")]
    Missing(String),
    #[error("working slot `{0}` is already flushed (sealed); cannot rewrite it")]
    Frozen(String),
    #[error("working slot name `{0}` is empty or path-unsafe")]
    InvalidName(String),
    #[error("internal storage error: {0}")]
    Store(String),
}

/// A single named working slot, quoted byte-exactly as set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingSlot {
    pub name: String,
    pub bytes: String,
}

/// Working memory for one live session. Not backed by persistence — scratch.
#[derive(Debug, Default)]
pub struct WorkingMemory {
    slots: std::collections::BTreeMap<String, WorkingSlot>,
    flushed: Vec<String>,
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
        })
}

impl WorkingMemory {
    /// New, empty working memory (a fresh live session).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a slot, quoted byte-exactly as given. A flushed (sealed) slot
    /// cannot be rewritten.
    pub fn set(&mut self, name: &str, bytes: &str) -> Result<(), WorkingError> {
        if !safe_name(name) {
            return Err(WorkingError::InvalidName(name.to_string()));
        }
        if self.flushed.contains(&name.to_string()) {
            return Err(WorkingError::Frozen(name.to_string()));
        }
        self.slots.insert(
            name.to_string(),
            WorkingSlot {
                name: name.to_string(),
                bytes: bytes.to_string(),
            },
        );
        Ok(())
    }

    /// Get the byte-exact bytes of a set slot. Unset → `Missing`.
    pub fn get(&self, name: &str) -> Result<String, WorkingError> {
        match self.slots.get(name) {
            Some(s) => Ok(s.bytes.clone()),
            None => Err(WorkingError::Missing(name.to_string())),
        }
    }

    /// True iff a slot named `name` is *currently set* (before flush).
    pub fn has(&self, name: &str) -> bool {
        self.slots.contains_key(name)
    }

    /// Freeze this layer: every currently-set slot is sealed into the flush
    /// list. All subsequent `get` on any slot → `Missing` (scratch is gone),
    /// any `set` on a flushed name → `Frozen`. Returns the ordered
    /// (name, bytes) pairs that were sealed — these are the citation seed to
    /// the origin session.
    pub fn flush(&mut self) -> Vec<(String, String)> {
        let ordered: Vec<(String, String)> = {
            let mut v: Vec<_> = self
                .slots
                .iter()
                .map(|(n, s)| (n.clone(), s.bytes.clone()))
                .collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };
        for n in ordered.iter().map(|(n, _)| n.clone()) {
            self.flushed.push(n);
        }
        self.slots.clear();
        ordered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_is_byte_exact() {
        let mut wm = WorkingMemory::new();
        wm.set("note", "hello 2 3").unwrap();
        assert_eq!(wm.get("note").unwrap(), "hello 2 3");
        assert!(wm.has("note"));
    }

    #[test]
    fn unset_fails_closed() {
        let wm = WorkingMemory::new();
        assert!(matches!(
            wm.get("never-set"),
            Err(WorkingError::Missing(_))
        ));
    }

    #[test]
    fn flush_seals_scratch_and_forbids_rewrite() {
        let mut wm = WorkingMemory::new();
        wm.set("a", "x").unwrap();
        wm.set("b", "y").unwrap();
        let sealed = wm.flush();
        assert_eq!(sealed.len(), 2);
        // scratch is gone after flush
        assert!(matches!(wm.get("a"), Err(WorkingError::Missing(_))));
        // and the sealed name cannot be silently rewritten
        assert!(matches!(
            wm.set("a", "z"),
            Err(WorkingError::Frozen(_))
        ));
    }
}
