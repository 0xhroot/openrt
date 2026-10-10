//! The Agent TUI's two wiring points into the ONE Runtime.
//!
//! * [`UiSink`] — an extra [`EventSink`] on the runtime bus. Every
//!   `DomainEvent` is forwarded over an unbounded channel so the terminal
//!   event loop can render live state. The bus still owns durability (the
//!   persistence sink is untouched); this sink is a view only.
//! * [`TuiResolver`] — the `Ask` approval resolver (P2.2). When policy says
//!   `Ask`, the gate hands us the real [`ApprovalRequest`] and waits for the
//!   operator's explicit Allow/Deny answer. No answer ⇒ the oneshot is dropped
//!   on exit ⇒ `resolve` errors ⇒ the gate denies (fail closed).

use std::sync::Mutex;

use openrt_core::DomainEvent;
use openrt_permissions::{
    ApprovalRequest, PermissionDecision, PermissionError, PermissionResolver,
};
use openrt_telemetry::EventSink;
use tokio::sync::{oneshot, Notify};

/// Forwards bus events to the terminal event loop. Unbounded on purpose: the
/// loop drains every tick, and dropping events (bounded channel overflow)
/// would render an incomplete stream — a UI that lies by omission.
pub struct UiSink {
    tx: tokio::sync::mpsc::UnboundedSender<DomainEvent>,
}

impl UiSink {
    /// Create the sink plus its receiving end.
    pub fn channel() -> (
        std::sync::Arc<Self>,
        tokio::sync::mpsc::UnboundedReceiver<DomainEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (std::sync::Arc::new(Self { tx }), rx)
    }
}

#[async_trait::async_trait]
impl EventSink for UiSink {
    async fn write(&self, ev: &DomainEvent) {
        let _ = self.tx.send(ev.clone());
    }
}

/// One approval waiting for an operator decision.
pub struct PendingApproval {
    pub request: ApprovalRequest,
    reply: oneshot::Sender<PermissionDecision>,
}

impl PendingApproval {
    /// Answer the gate: `Allow` releases the call, `Deny` (or dropping this
    /// value without answering) blocks it.
    pub fn answer(self, decision: PermissionDecision) {
        let _ = self.reply.send(decision);
    }

    /// Build a request directly, so a test can render the card without going
    /// through the gate. Production always constructs this inside
    /// [`TuiResolver::resolve`].
    #[cfg(test)]
    pub fn for_test(request: ApprovalRequest, reply: oneshot::Sender<PermissionDecision>) -> Self {
        Self { request, reply }
    }
}

/// `Ask` resolver backed by the terminal: requests are queued and answered by
/// the operator through the approval card.
#[derive(Default)]
pub struct TuiResolver {
    inbox: Mutex<Vec<PendingApproval>>,
    notify: Notify,
}

impl TuiResolver {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Move every queued request out for the UI to display.
    ///
    /// Poisoning is recovered rather than propagated. `drain` runs inside
    /// `AgentApp::pump`, which is the event-loop path, so a propagated poison here
    /// turns one unrelated panic into a permanently dead interface: every later
    /// `pump` panics too and the operator cannot even quit cleanly. The guarded
    /// value is a `Vec<PendingApproval>` and dropping half its elements only
    /// denies the calls waiting on them — the fail-closed outcome this queue
    /// exists to produce — so a poisoned vector cannot have left anything
    /// inconsistent that is worth refusing to read.
    pub fn drain(&self) -> Vec<PendingApproval> {
        let mut guard = match self.inbox.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        std::mem::take(&mut *guard)
    }

    /// Number of requests still waiting inside the resolver (before drain).
    pub fn pending(&self) -> usize {
        match self.inbox.lock() {
            Ok(g) => g.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Enqueue a request without waiting on the answer (test wiring).
    #[cfg(test)]
    pub(crate) fn resolve_now(
        &self,
        request: ApprovalRequest,
        reply: oneshot::Sender<PermissionDecision>,
    ) {
        match self.inbox.lock() {
            Ok(mut g) => g.push(PendingApproval { request, reply }),
            Err(poisoned) => poisoned
                .into_inner()
                .push(PendingApproval { request, reply }),
        }
    }

    /// Raw access to the guarded queue, so a test can poison it the way a panic
    /// inside the critical section would.
    #[cfg(test)]
    pub(crate) fn inbox_for_test(&self) -> &Mutex<Vec<PendingApproval>> {
        &self.inbox
    }
}

#[async_trait::async_trait]
impl PermissionResolver for TuiResolver {
    async fn resolve(&self, req: &ApprovalRequest) -> Result<PermissionDecision, PermissionError> {
        let (tx, rx) = oneshot::channel();
        let pending = PendingApproval {
            request: req.clone(),
            reply: tx,
        };
        match self.inbox.lock() {
            Ok(mut g) => g.push(pending),
            // Recovered rather than propagated: refusing to enqueue here would
            // leave the gate waiting on a oneshot nobody will ever answer,
            // which blocks the call instead of denying it. Push anyway — the
            // fail-closed path is the operator denying, or quitting.
            Err(poisoned) => poisoned.into_inner().push(pending),
        }
        self.notify.notify_one();
        // Sender dropped (operator quit without answering) ⇒ error ⇒ the gate
        // treats it as a hard denial. Fail closed, always.
        rx.await.map_err(|_| {
            PermissionError::NoResolver("operator dismissed the approval request".into())
        })
    }
}
