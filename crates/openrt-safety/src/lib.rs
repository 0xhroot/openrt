//! ADR-008 safety surface: the common "HALT" signal bus semantics and the
//! per-scope kill-switch mechanisms.
//!
//! Phase 2 implements the computer-use kill switch. The rules from ADR-008:
//!
//! * HALT signals carry a **scope** (`computer` today; `trading` + `global`
//!   arrive in Phase 6) and a severity. They are emitted on the Phase 1 event
//!   bus as `EventKind::SafetyHalt` with the scope/reason in `data`.
//! * Each subsystem subscribes **only** to its own scope and runs its own
//!   mitigation; a HALT alone never triggers another subsystem's mitigation.
//!   [`tool_scope`] is the classifier that keeps a halt inside its scope.
//! * Kill switches are **operator-only** and tamper-resistant: no agent/tool
//!   can arm, disarm, or clear them (this crate exposes no such path to the
//!   tool layer; the runtime's `halt()` is the operator surface).
//! * Every trip and every subsystem reaction is auditable — the runtime emits
//!   `SafetyHalt` on the bus and the gate emits one for each blocked call.
//!
//! This crate is dependency-free so every subsystem can use the latch without
//! dragging in runtime concerns.

mod kill;

pub use kill::{halt_reason, tool_scope, KillScope, KillSwitch};
