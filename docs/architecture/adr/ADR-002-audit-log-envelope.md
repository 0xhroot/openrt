# ADR-002: Audit Log Envelope — Chained from Phase 2

**Status:** Accepted
**Date:** 2026-09-17
**Applies to:** Phase 2 (envelope creation), Phase 3 (lifecycle events),
Phase 5 (replay/time machine), Phase 6 (audit system)

## Context

Phase 2 creates the append-only audit log for computer-use actions. Phase 6
requires a tamper-evident, centralized, searchable audit system. If the Phase 2
entry format lacks chaining/tamper-evidence fields, retrofitting would rewrite
historical entries, which would break Phase 5 replay/time-machine fidelity or
force an incompatible migration.

Prior conflict: P2 log defined as "append-only" with no format guarantee;
P6 "hardens/centralizes" it, leaving the migration interaction unspecified.

## Decision

1. **The audit-log entry envelope is defined once in Phase 2 and frozen.**
   Envelope version (format id) = 1. Canonical field layout:
   - `seq` — monotonic sequence number,
   - `ts` — timestamp,
   - `actor` / `action` / `subject` / `result` — semantic content,
   - `prev_hash` — SHA256 of the previous entry's canonical serialization,
   - `chain_mode` — `plain` (initial) or `chained` (set once chaining begins),
   - `sig` — optional signature over the entry (stamped at sealing time),
   - `ext` — reserved, additive-only extension field for future schemes.
2. **Tamper evidence is available from Phase 2**: any modification of a past
   entry breaks the `prev_hash` chain and is detectable by construction.
3. **Phase 6 strengthens verification, not format**: stronger signature
   algorithms, centralized retention, and key management may be added. They
   must not change the envelope version, the entry serialization, or the field
   layout. Only additive `ext` fields are permitted. No migration that rewrites
   historical entries is allowed.
4. The log tail (head-of-chain) may be sealed and signed by the runtime;
   sealing is idempotent and does not alter existing entries.
5. Phase 5 replay/time machine reads the same canonical serialization, so
   tamper verification and history replay agree on one representation.

## Consequences

- Forward compatibility is structural: P3 lifecycle events, P5 history, and P6
  audit all share one envelope with no later rewrite.
- Tamper-evidence is effectively available earlier than planned (Phase 2), at
  negligible cost (one hash field per entry).
- Discipline required: `ext` additions only; no semantic reinterpretation of
  existing fields. Enforced conceptually as "format frozen at id=1".

**References:** ROADMAP.md Phase 2 (security requirements, tests), Phase 3
(security baseline), Phase 6 (audit system); OWNERSHIP.md.