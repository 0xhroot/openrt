//! OpenRT telemetry: structured logging and domain event emission.
//!
//! This crate owns the *emission* side of the Phase 1 event bus plus logging.
//! The event *types* (`DomainEvent`, `EventKind`) live in `openrt-core` so any
//! crate can describe events without depending on emission machinery.
//!
//! Guarantees:
//! * **Ordering** — events are emitted through a single channel to a dispatcher
//!   that writes each sink sequentially, giving a total order per sink.
//! * **Non-blocking callers** — `emit` hands the event to the channel and
//!   returns; a slow sink never stalls the runtime call path (bounded queue).
//! * **Sovereignty** — all sinks are local (logging, store adapter). No egress
//!   sink exists in Phase 1 (ADR-005).

pub mod bus;
pub mod logging;
pub mod metrics;
pub mod sink;

pub use bus::{emit, EventBus, EventBusBuilder};
pub use metrics::{MetricRecord, MetricsError, MetricsRegistry, METRICS_SCHEMA_VERSION};
pub use sink::{ChannelSink, EventSink, LoggingSink};

/// Re-export for the crate's public surface convenience.
pub use openrt_core::{DomainEvent, EventKind};
