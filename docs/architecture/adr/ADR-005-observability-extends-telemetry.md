# ADR-005: Observability Extends Local Telemetry

**Status:** Accepted
**Date:** 2026-09-17
**Applies to:** Phase 5 (telemetry), Phase 6 (production observability)

## Context

Phase 5 delivers local-only telemetry of runtime behavior. Phase 6 needs
production observability (metrics, health, dashboards, optional export). If each
phase builds its own metrics/event plumbing, OpenRT ends up with two parallel,
drifting pipelines and duplicated counters.

Prior conflict: P5 "Telemetry (local-only)" and P6 "Production observability"
were specified independently with no stated relationship.

## Decision

1. **One pipeline.** There is exactly one metrics/events pipeline: the Phase 5
   telemetry subsystem, which emits over the Phase 1 event bus and structured
   logging. The metric/event schema is part of the versioned public surface
   from Phase 5 onward.
2. **Phase 6 extends, it does not rebuild.** Production observability adds
   retention policy, aggregations, health surfaces, and optional piped export
   adapters over the same pipeline and schema. It must not create a parallel
   architecture, duplicate counters, or introduce a second emission path.
3. **Sovereignty.** Telemetry is local-only by default. Any egress is an
   explicit, documented, opt-in release gate and is **blocked in Sovereign
   Execution Mode** (ADR-003). Telemetry must be able to degrade to
   memory/disk-only operation without behavior change.
4. Schema additions are additive and versioned; P6 may not fork or rename the
   P5 metric/event identifiers.

## Consequences

- Single source of truth; no metric drift; operators get one coherent
  telemetry model across P5 and P6.
- P5 carries a schema-contract responsibility; P6 delivers value on top of it.
- Sovereignty requirement literally enforced at the mode boundary.

**References:** ROADMAP.md Phase 5 (telemetry), Phase 6 (production
observability, DoD); OWNERSHIP.md.