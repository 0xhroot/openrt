# ADR-003: Offline Security Scanning and Operating Modes

**Status:** Accepted
**Date:** 2026-09-17
**Applies to:** Phase 3 (scanning, modes), Phase 6 (air-gapped operation)

## Context

Phase 3 security scanning of model artifacts depends on up-to-date signature
feeds. Sovereign Execution Mode / air-gapped operation excludes the internet.
A naive design either silently degrades scanning offline (a security hole) or
requires network (violating sovereignty).

Prior conflict: P3 scanning was specified without an offline update path and
Sovereign Mode's behavior toward unverifiable artifacts was unspecified.

## Decision

1. **Signed, versioned offline signature bundles.** All scan signatures (known-bad
   hashes, format checks, model-specific checks) ship as **bundles**: bundle id,
   content digest, signature key pinned to the OpenRT release key, and an
   explicit offline refresh channel (installed during provisioning). Scan
   results always record the bundle version used.

2. **Fail-closed rule.** In Sovereign Execution Mode, an artifact that cannot be
   verified — unknown bundle version, unsigned bundle, stale bundle relative to
   the artifact, or inconclusive scan — is **refused activation**. An operator
   override, if granted, is explicit, per-artifact, and audited (ADR-002).
   There is no silent pass.

3. **Two operating modes, explicitly distinct.**
   - **Provisioning Mode** — conditionally networked for a single purpose:
     explicit, permission-gated materialization of content (model downloads,
     Hugging Face discovery, security-bundle refresh, dataset import).
     Network use is never implicit. Content that enters must still verify
     before it can run in Sovereign Mode.
   - **Sovereign Execution Mode** — network interfaces blocked (loopback only);
     network-dependent features disabled; artifact activation requires local
     verification against the installed signed bundles. No downloads, no
     discovery, no bundle refresh.
   - The Provisioning → Sovereign transition is an audited state change.

4. **Phase 6 air-gap reuses the same machinery.** Bundled signature-key pinned
   bundles plus offline provenance verification (ADR-001); no network canaries
   are used to validate anything.

## Consequences

- Operators must provision/refresh bundles before going Sovereign; stale bundles
  produce *policy decisions* (deny with reason), never silent passes.
- Mode is a state machine with an audited transition, not a config flag.
- Minor operational friction: receiving updated threat intel requires a
  provisioning step.
- Unknown-novelty degradation in air-gap is explicit ("reduced assurance" is
  surfaced, never hidden).

**References:** ROADMAP.md Phase 3 (security scanning, Provisioning/Sovereign
modes, tests, security requirements), Phase 6 (air-gapped operation);
OWNERSHIP.md.