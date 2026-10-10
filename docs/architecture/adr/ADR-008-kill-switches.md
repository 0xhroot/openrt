# ADR-008: Kill Switches — Common Safety Signal, Separate Mechanisms

**Status:** Accepted
**Date:** 2026-09-17
**Applies to:** Phase 2 (computer-use kill switch + safety signal),
Phase 6 (trading kill switch)

## Context

Phase 2 needs an emergency halt for the computer-use loop (mouse/keyboard/
terminal/browser). Phase 6 needs a kill switch for autonomous trading (halt,
cancel/flatten orders). These are fundamentally different mechanisms with
different mitigations and SLAs. Conflating them (e.g., the trading switch also
stopping computer actions, or vice versa) is dangerous and un-testable.

Prior conflict (flagged): the roadmap had two "kill switches" with no stated
relationship or common halting pathway.

## Decision

1. **Common runtime safety/halt signal.** Phase 2 introduces `openrt-safety`: a
   broadcast "HALT" signal bus built over the Phase 1 event bus. Signals carry
   a **scope** (`computer`, `trading`, or `global`) and a severity. This is the
   single operator-visible halting pathway.

2. **Kill switches are mechanism adapters, one per subsystem, scoped.**
   - *Computer-use kill switch* (Phase 2, scope=computer): on HALT(computer),
     stops mouse, keyboard, terminal, and browser actions immediately
     regardless of agent state. It remains inside its scope.
   - *Trading kill switch* (Phase 6, scope=trading): on HALT(trading), stops
     order placement and runs the configured cancel/flatten policy within its
     SLA. It remains inside its scope.

3. **Scoping discipline.** A HALT signal alone never triggers a subsystem's
   mitigation; each subsystem subscribes only to its scope and runs its own
   mitigation. A global operator-abort signal may fan out to all scopes, but
   each subsystem still executes its own mitigation. **No subsystem's kill
   switch may silently perform another's actions.**

4. **Operator-only; tamper-resistant.** Kill switches are operator-only. No
   agent, skill, instinct, hook, or self-optimization process (Phase 5) may
   arm, disarm, or modify them (self-optimization override is explicitly
   forbidden in the roadmap).

5. **Auditable.** Every signal emission and every subsystem reaction is
   recorded in the audit log (ADR-002).

## Consequences

- Two independent mechanisms; one coherent operator control surface.
- Clear, disjoint test story: each switch is tested against its own audited
  signal and its own mitigation, and cross-triggering is a rejected case.
- Modest new surface in Phase 2 (`openrt-safety` bus), built on the existing
  event bus.
- Slight policy-broadcast layering for `global` signals; acceptable.

**References:** ROADMAP.md Phase 2 (kill switch, security requirements, tests),
Phase 6 (trading kill switch, safety rails, tests); ADR-002;
OWNERSHIP.md.