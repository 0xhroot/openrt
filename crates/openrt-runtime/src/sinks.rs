//! Runtime-side event sinks (the persistence adapter lives here to keep the
//! telemetry→persistence dependency acyclic).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use openrt_persistence::PersistenceStore;
use openrt_telemetry::EventSink;

/// Aggregated persistence health of the event-log sink. Write failures are
/// surfaced here (exposed on the [`Runtime`](crate::Runtime) and reported by
/// `openrt doctor`) instead of being silently dropped (SEC-06 / PC-Q8).
#[derive(Debug, Default)]
pub struct EventLogHealth {
    total: AtomicU64,
    failures: AtomicU64,
    last_error: Mutex<Option<String>>,
}

/// Point-in-time view of [`EventLogHealth`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventLogSnapshot {
    pub total: u64,
    pub failures: u64,
    pub last_error: Option<String>,
}

impl EventLogHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current counters and most recent failure message, if any.
    pub fn snapshot(&self) -> EventLogSnapshot {
        EventLogSnapshot {
            total: self.total.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            last_error: self.last_error.lock().map(|m| m.clone()).unwrap_or(None),
        }
    }

    fn note_success(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_failure(&self, error: String) {
        self.total.fetch_add(1, Ordering::Relaxed);
        self.failures.fetch_add(1, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(error);
    }
}

/// Writes every bus event into the foundation store's append-only event log.
/// The *stored* copy is a redacted clone (SEC-05): the in-memory bus event is
/// untouched, but secret-like payloads never land in the durable event log.
pub struct PersistenceSink {
    store: Arc<dyn PersistenceStore>,
    health: Arc<EventLogHealth>,
    redaction: openrt_core::RedactionPolicy,
}

impl std::fmt::Debug for PersistenceSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistenceSink")
            .field("health", &self.health.snapshot())
            .field(
                "redaction_active",
                &(self.redaction != openrt_core::RedactionPolicy::new()),
            )
            .finish()
    }
}

impl PersistenceSink {
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self::with_redaction(store, openrt_core::RedactionPolicy::default())
    }

    /// Persist through a custom redaction policy (SEC-05; still a clone so the
    /// live event on the bus is never rewritten).
    pub fn with_redaction(
        store: Arc<dyn PersistenceStore>,
        redaction: openrt_core::RedactionPolicy,
    ) -> Self {
        Self {
            store,
            health: Arc::new(EventLogHealth::new()),
            redaction,
        }
    }

    /// Access the shared health accumulator (exposed on the runtime for
    /// `openrt doctor` and embedded tools).
    pub fn health(&self) -> &Arc<EventLogHealth> {
        &self.health
    }
}

#[async_trait::async_trait]
impl EventSink for PersistenceSink {
    async fn write(&self, event: &openrt_core::DomainEvent) {
        let mut stored = event.clone();
        self.redaction.redact_value(&mut stored.data);
        match self.store.append_event(&stored).await {
            Ok(()) => self.health.note_success(),
            Err(e) => {
                // The event log is best-effort telemetry in Phase 1, but the
                // failure is surfaced (health counters + doctor) rather than
                // silently dropped (SEC-06).
                self.health.note_failure(e.to_string());
                tracing::error!(target: "openrt::sinks", "failed to persist event {:?}: {e}", event.kind);
            }
        }
    }
}

/// Writes every bus event into the Phase-5 execution-history ledger
/// (`history.ledger` via `openrt-timeseries`). This is the production bridge
/// that makes runtime runs replayable / forkable / time-machable (M5.2).
///
/// The `note` payload is a *redacted clone* of `event.data` (SEC-05: secrets
/// never enter the ledger). The live bus event is untouched. Failures are
/// counted on [`EventLogHealth`] (surfaced, never silent).
pub struct HistorySink {
    store: Arc<dyn PersistenceStore>,
    health: Arc<EventLogHealth>,
    redaction: openrt_core::RedactionPolicy,
}

impl std::fmt::Debug for HistorySink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistorySink")
            .field("health", &self.health.snapshot())
            .finish()
    }
}

impl HistorySink {
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self::with_redaction(store, openrt_core::RedactionPolicy::default())
    }

    pub fn with_redaction(
        store: Arc<dyn PersistenceStore>,
        redaction: openrt_core::RedactionPolicy,
    ) -> Self {
        Self {
            store,
            health: Arc::new(EventLogHealth::new()),
            redaction,
        }
    }

    pub fn health(&self) -> &Arc<EventLogHealth> {
        &self.health
    }

    /// Map a domain event onto ledger fields. `session_id`/`node_id` are the
    /// correlation ids; `note` is the redacted data payload (truncated).
    fn ledger_fields(&self, event: &openrt_core::DomainEvent) -> (String, String, String, String) {
        let session_id = event
            .session_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".into());
        let node_id = event
            .task_id
            .map(|t| t.to_string())
            .unwrap_or_else(|| event.event_id.to_string());
        let event_kind = format!("{:?}", event.kind);
        let mut data = event.data.clone();
        self.redaction.redact_value(&mut data);
        let mut note = match &data {
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        // Keep ledger rows bounded; the full redacted event still lives in the
        // event log (`PersistenceSink`), this note is a replay aid only.
        if note.len() > 512 {
            note.truncate(512);
            note.push('…');
        }
        // Status: map terminal task outcomes; everything else is a step.
        let status = match event.kind {
            openrt_core::EventKind::TaskCompleted => "ok",
            openrt_core::EventKind::TaskFailed => "failed",
            openrt_core::EventKind::ErrorOccurred => "failed",
            openrt_core::EventKind::SafetyHalt => "halt",
            _ => "step",
        };
        (
            session_id,
            node_id,
            status.to_string(),
            format!("{event_kind}\n{note}"),
        )
    }
}

#[async_trait::async_trait]
impl EventSink for HistorySink {
    async fn write(&self, event: &openrt_core::DomainEvent) {
        let (session_id, node_id, status, note) = self.ledger_fields(event);
        let event_kind = format!("{:?}", event.kind);
        // `note` embeds a newline; ledger `event_kind` stays the bare kind so
        // replay can group by kind cleanly. Put the redacted payload in `note`.
        let note_only = note
            .split_once('\n')
            .map(|(_, n)| n.to_string())
            .unwrap_or(note);
        match openrt_timeseries::append_event(
            &*self.store,
            &session_id,
            &node_id,
            &status,
            &event_kind,
            &note_only,
        )
        .await
        {
            Ok(_) => self.health.note_success(),
            Err(e) => {
                self.health.note_failure(e.to_string());
                tracing::error!(target: "openrt::sinks", "failed to append history for {:?}: {e}", event.kind);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::{DomainEvent, EventKind};

    /// SEC-05: what lands in the durable store is a *redacted clone* — the
    /// live event travelling on the bus is untouched, but secret-like payloads
    /// never reach the event log.
    #[tokio::test]
    async fn persistence_sink_redacts_stored_copy_only() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let sink = PersistenceSink::new(store.clone());

        let live =
            DomainEvent::new(EventKind::ToolInvocationStarted).with_data(serde_json::json!({
                "tool": "echo",
                "args": { "password": "hunter2", "path": "/ok" }
            }));

        sink.write(&live).await;

        // The live event was left intact.
        assert_eq!(live.data["args"]["password"], "hunter2");

        // The stored event has secrets redacted, safe fields preserved.
        let stored = store
            .list_events(openrt_persistence::ListOptions::default())
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].data["args"]["password"], "[REDACTED]");
        assert_eq!(stored[0].data["args"]["path"], "/ok");

        // Health counted the successful write.
        let snap = sink.health().snapshot();
        assert_eq!(snap.total, 1);
        assert_eq!(snap.failures, 0);
    }

    /// M5.2 + SEC-05: the history ledger receives redacted notes only.
    #[tokio::test]
    async fn history_sink_appends_redacted_ledger_rows() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let sink = HistorySink::new(store.clone());
        let sid = openrt_core::SessionId::new();
        let live = DomainEvent::new(EventKind::TaskCompleted)
            .with_session(sid)
            .with_data(serde_json::json!({ "result": "ok", "password": "hunter2" }));
        sink.write(&live).await;
        assert_eq!(live.data["password"], "hunter2"); // live untouched

        let n = openrt_timeseries::verify_chain(&*store).await.unwrap();
        assert_eq!(n, 1);
        let rows = openrt_timeseries::materialize_as_of(&*store, 1)
            .await
            .unwrap();
        assert_eq!(rows[0].event_kind, "TaskCompleted");
        assert_eq!(rows[0].session_id, sid.to_string());
        assert_eq!(rows[0].status, "ok");
        assert!(!rows[0].note.contains("hunter2"), "note must be redacted");
        assert!(rows[0].note.contains("[REDACTED]"));
        let snap = sink.health().snapshot();
        assert_eq!(snap.failures, 0);
    }
}
