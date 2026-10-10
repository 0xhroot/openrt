# ADR-004: Distributed Execution Inside an Operator-Owned Enclave

**Status:** Accepted
**Date:** 2026-09-17
**Applies to:** Phase 6 (distributed workers, model federation, model migration)

## Context

Phase 6 introduces distributed workers, model federation, and model migration.
"Local-first / sovereign" and "air-gapped operation" appear to conflict with any
cross-machine traffic. The conflict must be resolved by scoping *what kind* of
network use is acceptable.

Prior conflict: air-gap was defined as "no network interface" while distributed
workers implicitly require a channel between nodes.

## Decision

1. **Enclave scope.** Distributed execution runs **only inside an
   operator-owned enclave**: the operator's own nodes on a private network.
   There is no third-party participation, no cloud inference, and no
   cross-operator federation by default. Out-of-scope: third-party cloud
   inference and cross-operator federation.

2. **No internet/egress in Sovereign Mode.** All enclave traffic is
   internal-only. In Sovereign Execution Mode (ADR-003), egress to the internet
   is denied by network isolation and policy; a distributed job that attempts
   external contact fails closed. "Air-gapped" applies at the enclave boundary
   (no egress), not between enclave nodes.

3. **Node authentication.** Mutual TLS with operator-generated, per-node
   identity certificates. Enrollment is out-of-band and operator-initiated.
   A node identity revocation list is maintained and propagated.

4. **Rogue-node detection.** Defenses include, at a minimum:
   - node attestation at join (identity + runtime artifact digest),
   - heartbeat and liveness accounting,
   - Byzantine-failure criteria for shared state (majority agreement),
   - connection anomaly detection — a node that attempts to reach non-enclave
     addresses is quarantined and reported in the audit log (ADR-002).

5. **Verified payloads.** Every payload (models, configs, code) carries its
   SHA256 + provenance chain (ADR-001). A node that cannot verify a payload
   refuses it. Model migration preserves verification end to end.

## Consequences

- Threat model is scoped to the enclave; third-party inclusion is structurally
  excluded.
- Adds key management and node lifecycle (enrollment, revocation, quarantine).
- Resolves the air-gap/distribution tension: enclave LAN is "network" but never
  "egress".

**References:** ROADMAP.md Phase 6 (distributed workers, model federation,
model migration, DoD, tests, security); ADR-001, ADR-003;
OWNERSHIP.md.