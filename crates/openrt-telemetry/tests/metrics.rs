use std::sync::Arc;

use openrt_core::{DomainEvent, EventKind};
use openrt_telemetry::{
    ChannelSink, EventBusBuilder, MetricRecord, MetricsError, MetricsRegistry,
    METRICS_SCHEMA_VERSION,
};

#[test]
fn m57_metric_record_get_snapshot_deterministic() {
    let mut reg = MetricsRegistry::new();
    assert_eq!(reg.schema_version(), METRICS_SCHEMA_VERSION);
    reg.record("run.latency_ms", 42.0, "ms").unwrap();
    reg.record("run.success_rate", 0.97, "ratio").unwrap();
    reg.record("run.calls", 12.0, "count").unwrap();
    let snap: Vec<MetricRecord> = reg.snapshot();
    let names: Vec<&str> = snap.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["run.calls", "run.latency_ms", "run.success_rate"],
        "snapshot must be sorted by name"
    );
    assert_eq!(reg.get("run.latency_ms").unwrap().value, 42.0);
    assert_eq!(reg.get("run.latency_ms").unwrap().schema_version, 1);
    assert!(reg.get("run.missing").is_none());
}

#[test]
fn m57_metric_last_value_wins_per_name() {
    let mut reg = MetricsRegistry::new();
    reg.record("gauge.a", 1.0, "count").unwrap();
    reg.record("gauge.a", 2.0, "count").unwrap();
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.get("gauge.a").unwrap().value, 2.0);
}

#[test]
fn m57_neg_metric_empty_name_or_unit_rejected() {
    let mut reg = MetricsRegistry::new();
    assert_eq!(
        reg.record("", 1.0, "count").unwrap_err(),
        MetricsError::EmptyName
    );
    assert_eq!(
        reg.record("  ", 1.0, "count").unwrap_err(),
        MetricsError::EmptyName
    );
    assert_eq!(
        reg.record("name", 1.0, "").unwrap_err(),
        MetricsError::EmptyUnit
    );
    assert!(reg.is_empty());
}

#[tokio::test]
async fn m57_metrics_emit_through_local_bus_only() {
    let sink = Arc::new(ChannelSink::new());
    let (bus, _handle) = EventBusBuilder::new().sink(sink.clone()).start();
    let mut reg = MetricsRegistry::new();
    reg.record("run.latency_ms", 42.0, "ms").unwrap();
    reg.record("run.calls", 12.0, "count").unwrap();
    let sent = reg.emit_all(&bus).await;
    assert_eq!(sent, 2);
    drop(bus);
    tokio::task::yield_now().await;
    let events = sink.snapshot();
    assert_eq!(events.len(), 2);
    for ev in &events {
        assert_eq!(ev.kind, EventKind::MetricRecorded);
        assert_eq!(ev.data["schema_version"], 1u64);
        assert!(ev.data["name"].is_string());
        assert!(ev.data["unit"].is_string());
    }
    assert_eq!(events[0].data["name"], "run.calls");
    assert_eq!(events[1].data["name"], "run.latency_ms");
}

#[tokio::test]
async fn m57_phase5_observability_event_kinds_round_trip() {
    use serde_json::json;
    let sink = Arc::new(ChannelSink::new());
    let (bus, _handle) = EventBusBuilder::new().sink(sink.clone()).start();
    assert!(
        bus.emit(
            DomainEvent::new(EventKind::ImprovementProposed)
                .with_data(json!({"proposal_id": "prop.a", "proposal_kind": "self_optimize", "target_kind": "router"}))
        )
        .await
    );
    assert!(
        bus.emit(
            DomainEvent::new(EventKind::ImprovementEvaluated)
                .with_data(json!({"proposal_id": "prop.a", "result": "passed"}))
        )
        .await
    );
    assert!(
        bus.emit(
            DomainEvent::new(EventKind::ImprovementApproved)
                .with_data(json!({"proposal_id": "prop.a", "operator": "alice"}))
        )
        .await
    );
    assert!(
        bus.emit(DomainEvent::new(EventKind::ImprovementApplied).with_data(
            json!({"proposal_id": "prop.a", "target_id": "router.main", "digest": "abc"})
        ))
        .await
    );
    assert!(
        bus.emit(DomainEvent::new(EventKind::DebugReported).with_data(
            json!({"debug_id": "debug.1", "run_id": "run.1", "failure_signature": "sig.x"})
        ))
        .await
    );
    assert!(
        bus.emit(
            DomainEvent::new(EventKind::LearningDerived)
                .with_data(json!({"learning_id": "learn.x", "learning_kind": "instinct_candidate", "run_id": "run.1"}))
        )
        .await
    );
    drop(bus);
    tokio::task::yield_now().await;
    let events = sink.snapshot();
    assert_eq!(events.len(), 6);
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&EventKind::ImprovementApproved));
    assert!(kinds.contains(&EventKind::LearningDerived));
    for ev in &events {
        let json = serde_json::to_value(ev).unwrap();
        let back: DomainEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back.kind, ev.kind, "phase 5 events must round-trip");
    }
}
