//! Event sinks: where every emitted event goes.

use std::sync::Mutex;

use async_trait::async_trait;
use openrt_core::DomainEvent;

/// A consumer of every event. Implementations must be `Send + Sync`; they are
/// awaited sequentially by the bus dispatcher, which provides ordering.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn write(&self, ev: &DomainEvent);
}

/// Writes events into the structured log (tracing). This is the default sink —
/// event envelopes appear as log lines with correlation fields, so logs alone
/// can reconstruct runtime behavior. Payloads are redacted before rendering
/// (SEC-05): log lines never contain secret-like values, and the live event on
/// the bus is untouched.
#[derive(Debug, Clone, Default)]
pub struct LoggingSink {
    redaction: openrt_core::RedactionPolicy,
}

impl LoggingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Redact through a custom policy; logs then read with the configured
    /// placeholder (SEC-05).
    pub fn with_redaction(redaction: openrt_core::RedactionPolicy) -> Self {
        Self { redaction }
    }
}

#[async_trait]
impl EventSink for LoggingSink {
    async fn write(&self, ev: &DomainEvent) {
        let mut redacted = ev.data.clone();
        self.redaction.redact_value(&mut redacted);
        tracing::event!(
            tracing::Level::INFO,
            event_id = %ev.event_id,
            kind = %format!("{:?}", ev.kind),
            session_id = ?ev.session_id.map(|s| s.to_string()),
            task_id = ?ev.task_id.map(|t| t.to_string()),
            agent_id = ?ev.agent_id.map(|a| a.to_string()),
            data = ?redacted,
            "domain_event"
        );
    }
}

/// Collects events in memory; used by tests and by tools that need a snapshot
/// (e.g. `openrt task run --events`).
#[derive(Debug, Default)]
pub struct ChannelSink {
    events: Mutex<Vec<DomainEvent>>,
}

impl ChannelSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<DomainEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl EventSink for ChannelSink {
    async fn write(&self, ev: &DomainEvent) {
        self.events.lock().unwrap().push(ev.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::EventKind;

    #[tokio::test]
    async fn channel_sink_records() {
        let sink = ChannelSink::new();
        sink.write(&DomainEvent::new(EventKind::RuntimeStarted))
            .await;
        assert_eq!(sink.len(), 1);
        assert!(!sink.is_empty());
    }

    #[tokio::test]
    async fn logging_sink_is_constructible() {
        // initialization handled by logging::init; sink itself is inert
        let _ = LoggingSink::new();
    }
}
