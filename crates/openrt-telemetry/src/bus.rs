//! Async publish/subscribe event bus with ordering guarantees.

use std::sync::Arc;

use openrt_core::{AgentId, DomainEvent, EventKind, SessionId, TaskId};
use tokio::sync::{mpsc, oneshot};

use crate::EventSink;

/// Internal channel message: either a domain event or a flush barrier.
/// FIFO order means a `Flush` ack fires only after every earlier event has
/// been written to every sink (M5.2 durability: no lost history rows on exit).
enum BusMsg {
    Event(DomainEvent),
    Flush(oneshot::Sender<()>),
}

/// A cloneable publisher handle to the bus. Emitting is cheap and non-blocking
/// to the caller (bounded queue; a full queue awaits space, which only happens
/// when the dispatcher is starved — effectively never in Phase 1 rates).
#[derive(Clone, Debug)]
pub struct EventBus {
    tx: mpsc::Sender<BusMsg>,
}

impl EventBus {
    /// Push an event onto the bus. Returns `false` if the bus has shut down.
    pub async fn emit(&self, ev: DomainEvent) -> bool {
        self.tx.send(BusMsg::Event(ev)).await.is_ok()
    }

    /// Non-async variant for call sites that cannot await (best-effort).
    pub fn try_emit(&self, ev: DomainEvent) -> bool {
        self.tx.try_send(BusMsg::Event(ev)).is_ok()
    }

    /// Wait until every event emitted before this call has been written to
    /// every registered sink. Used by shutdown/CLI exit so durable history
    /// and the event log are never cancelled mid-write.
    pub async fn flush(&self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(BusMsg::Flush(ack_tx)).await.is_ok() {
            let _ = ack_rx.await;
        }
    }

    /// Convenience: build a correlated `DomainEvent` and emit it.
    pub async fn emit_correlated(
        &self,
        kind: EventKind,
        session: Option<SessionId>,
        task: Option<TaskId>,
        agent: Option<AgentId>,
        data: impl Into<serde_json::Value>,
    ) -> bool {
        let mut ev = DomainEvent::new(kind);
        if let Some(id) = session {
            ev = ev.with_session(id);
        }
        if let Some(id) = task {
            ev = ev.with_task(id);
        }
        if let Some(id) = agent {
            ev = ev.with_agent(id);
        }
        self.emit(ev.with_data(data)).await
    }
}

/// Builder for an [`EventBus`]. Sinks are written in registration order for
/// every event (single dispatcher → per-sink total order).
#[derive(Default)]
pub struct EventBusBuilder {
    capacity: usize,
    sinks: Vec<Arc<dyn EventSink>>,
}

impl EventBusBuilder {
    pub fn new() -> Self {
        Self {
            capacity: 1024,
            sinks: Vec::new(),
        }
    }

    pub fn capacity(mut self, cap: usize) -> Self {
        self.capacity = cap.max(1);
        self
    }

    pub fn sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sinks.push(sink);
        self
    }

    /// Start the dispatcher task. Requires a running tokio runtime.
    ///
    /// Returns the publisher handle and the dispatcher's `JoinHandle`. The
    /// handle should be dropped/awaited when the runtime shuts down so the
    /// bus drains (the dispatcher exits when all senders are dropped).
    pub fn start(self) -> (EventBus, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<BusMsg>(self.capacity);
        let sinks = self.sinks;
        let handle = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    BusMsg::Event(ev) => {
                        for sink in &sinks {
                            sink.write(&ev).await;
                        }
                    }
                    BusMsg::Flush(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        });
        (EventBus { tx }, handle)
    }
}

/// Emit one event bus-style helper (thin wrapper over `EventBus::try_emit`
/// semantics kept for API stability with callers that hold the bus).
pub async fn emit(bus: &EventBus, ev: DomainEvent) -> bool {
    bus.emit(ev).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::ChannelSink;
    use std::sync::Arc;

    #[tokio::test]
    async fn events_are_delivered_in_order() {
        let sink = Arc::new(ChannelSink::default());
        let (bus, _handle) = EventBusBuilder::new().sink(sink.clone()).start();
        for i in 0..50 {
            bus.emit_correlated(
                EventKind::RuntimeStarted,
                None,
                None,
                None,
                serde_json::json!({"i": i}),
            )
            .await;
        }
        bus.flush().await;
        let events = sink.snapshot();
        assert_eq!(events.len(), 50);
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e.data["i"], i as u64);
        }
    }

    #[tokio::test]
    async fn bus_without_sinks_still_accepts() {
        let (bus, _handle) = EventBusBuilder::new().start();
        assert!(bus.emit(DomainEvent::new(EventKind::ErrorOccurred)).await);
        bus.flush().await;
    }

    #[tokio::test]
    async fn flush_waits_for_every_prior_event() {
        let sink = Arc::new(ChannelSink::default());
        let (bus, _handle) = EventBusBuilder::new().sink(sink.clone()).start();
        for i in 0..10 {
            bus.emit_correlated(
                EventKind::RuntimeStarted,
                None,
                None,
                None,
                serde_json::json!({"i": i}),
            )
            .await;
        }
        // Without flush, a yield is racy; with flush the count is exact.
        bus.flush().await;
        assert_eq!(sink.len(), 10);
    }
}
