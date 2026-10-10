//! Cooperative cancellation primitives.
//!
//! `CancellationToken` is a cheap, sync, `Send + Sync` signal that runs
//! anywhere (no tokio/runtime requirement here — this crate stays async-free).
//! The runtime combines a token with its own `Future` (`CancellationFut`) via
//! `tokio::select!` so a long-running provider call or tool can be aborted
//! promptly. `child()` wires multi-stage cancellation (e.g. shutdown cancels
//! every in-flight task).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

enum State {
    Armed { wakers: Vec<Waker> },
    Cancelled,
}

struct Inner {
    cancelled: AtomicBool,
    state: Mutex<State>,
    /// Weak handles to child tokens owned by this token's parent.
    children: Mutex<Vec<Weak<Inner>>>,
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// A cooperative cancellation signal.
#[derive(Clone)]
pub struct CancellationToken(Arc<Inner>);

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            cancelled: AtomicBool::new(false),
            state: Mutex::new(State::Armed { wakers: Vec::new() }),
            children: Mutex::new(Vec::new()),
        }))
    }

    /// True once `cancel()` has been called.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Cancel this token and every live child token.
    pub fn cancel(&self) {
        cancel_inner(&self.0);
    }

    /// Create a new token that is cancelled whenever this token is.
    pub fn child(&self) -> CancellationToken {
        let inner = Arc::new(Inner {
            cancelled: AtomicBool::new(false),
            state: Mutex::new(State::Armed { wakers: Vec::new() }),
            children: Mutex::new(Vec::new()),
        });
        self.0.children.lock().unwrap().push(Arc::downgrade(&inner));
        CancellationToken(inner)
    }

    /// A future that resolves when this token is cancelled. Idempotent-safe:
    /// awaiting after cancellation completes immediately.
    pub fn future(&self) -> CancellationFut {
        CancellationFut(Arc::clone(&self.0))
    }
}

/// Shared cancellation logic used by leaf and root tokens alike.
fn cancel_inner(inner: &Arc<Inner>) {
    if inner.cancelled.swap(true, Ordering::AcqRel) {
        return;
    }
    let wakers = {
        let mut st = inner.state.lock().unwrap();
        match std::mem::replace(&mut *st, State::Cancelled) {
            State::Armed { wakers } => wakers,
            State::Cancelled => Vec::new(),
        }
    };
    for w in wakers {
        w.wake();
    }
    let children = inner.children.lock().unwrap().clone();
    for child in children {
        if let Some(c) = child.upgrade() {
            cancel_inner(&c);
        }
    }
}

/// Owns an `Arc` to the token so the future is `'static` and can be raced with
/// other futures inside a task.
pub struct CancellationFut(Arc<Inner>);

impl Future for CancellationFut {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &*self.0;
        if inner.cancelled.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        let mut st = inner.state.lock().unwrap();
        match &mut *st {
            State::Cancelled => Poll::Ready(()),
            State::Armed { wakers } => {
                if wakers.iter().any(|w| w.will_wake(cx.waker())) {
                    return Poll::Pending;
                }
                wakers.push(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CancellationToken>();
    assert_send_sync::<CancellationFut>();
};

// ── Part C: resettable cooperative pause ─────────────────────────────────────

enum PauseState {
    /// Paused flag + waiters awaiting a *change* (either direction).
    Armed { paused: bool, wakers: Vec<Waker> },
    // The state lives in the mutex only to close the register/wake race the
    // same way `CancellationToken` does: a waiter that registered under the
    // lock is guaranteed to be woken by the transition that ran after it.
}

/// A level-based, resettable pause signal for cooperative task pausing.
///
/// Unlike [`CancellationToken`] (one-shot), `pause()`/`resume()` may be
/// toggled any number of times. Agents poll `is_paused()` at turn boundaries
/// and can *await* the transitions, exactly as they await cancellation. Like
/// `CancellationToken`, this crate stays async-free: waiters are woken through
/// the standard waker list (no tokio dependency here).
#[derive(Clone)]
pub struct PauseToken(Arc<PauseInner>);

struct PauseInner {
    paused: std::sync::atomic::AtomicBool,
    state: std::sync::Mutex<PauseState>,
}

impl Default for PauseToken {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PauseToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PauseToken")
            .field("paused", &self.is_paused())
            .finish()
    }
}

impl PauseToken {
    pub fn new() -> Self {
        Self(Arc::new(PauseInner {
            paused: std::sync::atomic::AtomicBool::new(false),
            state: std::sync::Mutex::new(PauseState::Armed {
                paused: false,
                wakers: Vec::new(),
            }),
        }))
    }

    /// True while a `pause()` is in force (until the matching `resume()`).
    pub fn is_paused(&self) -> bool {
        self.0.paused.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Engage the pause. Re-entrant; wakes any waiters (which then observe the
    /// flag and stop at the next turn boundary).
    pub fn pause(&self) {
        PauseInner::set(&self.0, true);
    }

    /// Release the pause (no-op when not paused). Wakes any waiters so a run
    /// blocked inside its pause resumes.
    pub fn resume(&self) {
        PauseInner::set(&self.0, false);
    }

    /// A future that completes once the token becomes *paused*. Awaiting while
    /// already paused completes immediately.
    pub fn wait_paused(&self) -> PauseWait {
        PauseWait(Arc::clone(&self.0), true)
    }

    /// A future that completes once the token becomes *unpaused*. Awaiting
    /// while unpaused completes immediately.
    pub fn wait_resumed(&self) -> PauseWait {
        PauseWait(Arc::clone(&self.0), false)
    }
}

impl PauseInner {
    fn set(inner: &Arc<PauseInner>, paused: bool) {
        inner
            .paused
            .store(paused, std::sync::atomic::Ordering::Release);
        let wakers = {
            let mut st = inner.state.lock().unwrap_or_else(|p| p.into_inner());
            match &mut *st {
                PauseState::Armed { paused: p, wakers } => {
                    *p = paused;
                    std::mem::take(wakers)
                }
            }
        };
        for w in wakers {
            w.wake();
        }
    }
}

/// Resolves when the token either becomes paused (`true`) or unpaused
/// (`false`), per the requested target.
pub struct PauseWait(Arc<PauseInner>, bool);

impl Future for PauseWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &*self.0;
        let want = self.1;
        let mut st = inner.state.lock().unwrap_or_else(|p| p.into_inner());
        match &mut *st {
            PauseState::Armed { paused, wakers } => {
                if *paused == want {
                    return Poll::Ready(());
                }
                if wakers.iter().any(|w| w.will_wake(cx.waker())) {
                    return Poll::Pending;
                }
                wakers.push(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PauseToken>();
    assert_send_sync::<PauseWait>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_starts_armed() {
        let t = CancellationToken::new();
        assert!(!t.is_cancelled());
    }

    #[test]
    fn token_cancel_flips_flag() {
        let t = CancellationToken::new();
        t.cancel();
        assert!(t.is_cancelled());
    }

    #[test]
    fn child_cancels_with_parent() {
        let parent = CancellationToken::new();
        let child = parent.child();
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(child.is_cancelled());
    }

    #[test]
    fn cancelling_child_does_not_cancel_parent() {
        let parent = CancellationToken::new();
        let child = parent.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());
    }

    #[tokio::test]
    async fn future_resolves_after_cancel() {
        let t = CancellationToken::new();
        let fut = t.future();
        let handle = tokio::spawn(async move {
            fut.await;
        });
        tokio::task::yield_now().await;
        t.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn pause_is_resettable_level() {
        let p = PauseToken::new();
        assert!(!p.is_paused());
        p.pause();
        assert!(p.is_paused());
        p.resume();
        assert!(!p.is_paused());
        // Re-entrant + idempotent.
        p.resume();
        assert!(!p.is_paused());
        p.pause();
        p.pause();
        assert!(p.is_paused());
        p.resume();
        assert!(!p.is_paused());
    }

    #[tokio::test]
    async fn wait_paused_resolves_on_pause() {
        let p = std::sync::Arc::new(PauseToken::new());
        let p2 = p.clone();
        let handle = tokio::spawn(async move { p2.wait_paused().await });
        tokio::task::yield_now().await;
        p.resume(); // a spurious unpause must not satisfy the waiter
        tokio::task::yield_now().await;
        p.pause();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn wait_resumed_resolves_on_resume() {
        let p = std::sync::Arc::new(PauseToken::new());
        p.pause();
        let p2 = p.clone();
        let handle = tokio::spawn(async move { p2.wait_resumed().await });
        tokio::task::yield_now().await;
        p.pause(); // re-pause must not satisfy the waiter
        tokio::task::yield_now().await;
        p.resume();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn wait_immediately_satisfied_when_state_matches() {
        let p = PauseToken::new();
        assert!(!p.is_paused());
        // Already unpaused → wait_resumed completes immediately.
        p.clone().wait_resumed().await;
        p.pause();
        p.wait_paused().await;
    }
}
