//! Bounded resource wait/retry (Part 3).
//!
//! When a task is resource-blocked the agent must be able to *wait* for the
//! host to settle and then resume — without ever entering a polling loop that
//! cannot end. This module is that bounded wait, and nothing else:
//!
//! * the attempt count is **hard-capped** by [`ResourceRetryPolicy`];
//! * the delay is **bounded** per attempt and the total wait is bounded;
//! * the wait is **cancellable** — a cancel token ends it immediately;
//! * exhaustion is a **structured outcome**, never a hang and never a silent
//!   "still waiting".
//!
//! The default policy is deliberately short (2 attempts, ≤1.2 s total): the
//! overwhelmingly common cause is a model server holding memory, which frees
//! nothing in 400 ms. Waiting longer belongs to the operator's explicit
//! "Wait" action, which re-enters this path with a longer budget.

use std::sync::Arc;
use std::time::Duration;

use openrt_core::CancellationToken;
use openrt_resources::{Remediation, ResourceDenial, ResourceGate};

/// Bounded budget for a resource wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceRetryPolicy {
    /// Maximum number of *re-checks*. `0` means "check once, never retry".
    pub max_attempts: u32,
    /// Delay before the first re-check.
    pub initial_delay_ms: u64,
    /// Ceiling for any single delay (exponential backoff is clamped here).
    pub max_delay_ms: u64,
}

impl Default for ResourceRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            initial_delay_ms: 500,
            max_delay_ms: 4_000,
        }
    }
}

impl ResourceRetryPolicy {
    /// A single re-check, no waiting.
    pub fn once() -> Self {
        Self {
            max_attempts: 0,
            ..Self::default()
        }
    }

    /// Delay before attempt `n` (0-based), clamped to `max_delay_ms`.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        if attempt == 0 || self.initial_delay_ms == 0 {
            return Duration::from_millis(self.initial_delay_ms.min(self.max_delay_ms));
        }
        // 2^n, saturating, then clamped.
        let shift = attempt.min(20);
        let scaled = self
            .initial_delay_ms
            .saturating_mul(1u64 << shift)
            .min(self.max_delay_ms);
        Duration::from_millis(scaled)
    }

    /// Longest total wall time this policy can spend waiting.
    pub fn max_total_wait(&self) -> Duration {
        (0..self.max_attempts).map(|a| self.delay_for(a)).sum()
    }

    /// Attempts actually permitted (`max_attempts` retries + the first check).
    pub fn total_checks(&self) -> u32 {
        self.max_attempts + 1
    }
}

/// What the bounded wait concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceWaitOutcome {
    /// Admitted within the budget.
    Admitted {
        /// Number of re-checks performed (0 = admitted on the first check).
        attempts: u32,
        /// Total time spent waiting.
        waited: Duration,
    },
    /// The budget ran out. Carries the last structured denial so the caller can
    /// present "WAITING FOR RESOURCES" with the operator's real options.
    Exhausted {
        attempts: u32,
        waited: Duration,
        denial: ResourceDenial,
    },
    /// The operator cancelled while waiting.
    Cancelled { attempts: u32, waited: Duration },
}

impl ResourceWaitOutcome {
    pub fn admitted(&self) -> bool {
        matches!(self, ResourceWaitOutcome::Admitted { .. })
    }

    pub fn denial(&self) -> Option<&ResourceDenial> {
        match self {
            ResourceWaitOutcome::Exhausted { denial, .. } => Some(denial),
            _ => None,
        }
    }

    /// One-line summary for the transcript / CLI.
    pub fn summary(&self) -> String {
        match self {
            ResourceWaitOutcome::Admitted { attempts, waited } => format!(
                "admitted after {attempts} re-check(s) in {}ms",
                waited.as_millis()
            ),
            ResourceWaitOutcome::Exhausted {
                attempts, denial, ..
            } => format!(
                "resource wait exhausted after {attempts} re-check(s): {}",
                denial.to_line()
            ),
            ResourceWaitOutcome::Cancelled { attempts, .. } => {
                format!("resource wait cancelled after {attempts} re-check(s)")
            }
        }
    }

    /// Remediations to offer. Cancellation offers a narrower set — there is
    /// nothing left to wait for.
    pub fn remediations(&self) -> Vec<Remediation> {
        match self {
            ResourceWaitOutcome::Exhausted { denial, .. } => denial.remediations.clone(),
            _ => vec![
                Remediation::Retry,
                Remediation::Wait,
                Remediation::SwitchModel,
                Remediation::Cancel,
            ],
        }
    }
}

/// Refresh the gate, then check admission — the single place a decision is
/// made, so every caller gets identical semantics.
async fn refresh_and_admit(gate: &Arc<ResourceGate>) -> openrt_resources::Admission {
    gate.refresh().await;
    gate.admit()
}

/// Sleep, returning `true` when the caller should stop waiting.
///
/// `openrt_core::CancellationToken` is a cooperative flag rather than a future,
/// so the wait is sliced and the flag is polled between slices. Slices are
/// short enough that a cancel is acted on within [`CANCEL_POLL_MS`] and long
/// enough that a long wait does not spin the CPU.
const CANCEL_POLL_MS: u64 = 50;

async fn sleep_cancellable(total: Duration, cancel: &CancellationToken) -> bool {
    if total.is_zero() {
        return cancel.is_cancelled();
    }
    let slice = Duration::from_millis(CANCEL_POLL_MS);
    let mut left = total;
    while !left.is_zero() {
        if cancel.is_cancelled() {
            return true;
        }
        let step = if left > slice { slice } else { left };
        tokio::time::sleep(step).await;
        left -= step;
    }
    cancel.is_cancelled()
}

/// Bounded, cancellable wait against an injected admission source.
///
/// This is the whole wait with the check factored out, so the retry semantics
/// can be regression-tested against a synthetic host instead of live `/proc`.
pub async fn await_admission_with<F, Fut>(
    mut check: F,
    policy: ResourceRetryPolicy,
    cancel: &CancellationToken,
) -> ResourceWaitOutcome
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = openrt_resources::Admission>,
{
    let started = std::time::Instant::now();
    let mut attempts: u32 = 0;
    loop {
        let admission = check().await;
        if admission.allowed() {
            return ResourceWaitOutcome::Admitted {
                attempts,
                waited: started.elapsed(),
            };
        }
        let Some(denial) = admission.denial.clone() else {
            // Allowed-or-denied with no explanation cannot happen through the
            // gate; treat it as admitted rather than looping on a mystery.
            return ResourceWaitOutcome::Admitted {
                attempts,
                waited: started.elapsed(),
            };
        };
        if attempts >= policy.max_attempts {
            return ResourceWaitOutcome::Exhausted {
                attempts,
                waited: started.elapsed(),
                denial,
            };
        }
        let delay = policy.delay_for(attempts);
        if sleep_cancellable(delay, cancel).await {
            return ResourceWaitOutcome::Cancelled {
                attempts,
                waited: started.elapsed(),
            };
        }
        attempts += 1;
    }
}

/// Bounded, cancellable wait for admission against the live host.
///
/// Never an infinite loop: `policy.max_attempts` bounds the re-checks,
/// `policy.max_delay_ms` bounds each delay, and the cancel token ends it
/// immediately. Every exit path returns a structured outcome.
pub async fn await_admission(
    gate: &Arc<ResourceGate>,
    policy: ResourceRetryPolicy,
    cancel: &CancellationToken,
) -> ResourceWaitOutcome {
    await_admission_with(|| refresh_and_admit(gate), policy, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_resources::{
        Admission, PreflightVerdict, ResourceDecision, ResourceKind, ResourceLimits,
        ResourcePreflight, ResourceStatus, SystemInfo,
    };
    use std::future::Future;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The wait loop takes any async check; tests hand it a boxed future so the
    /// synthetic host does not depend on the live `/proc` refresh.
    type Source =
        Box<dyn FnMut() -> std::pin::Pin<Box<dyn Future<Output = Admission> + Send>> + Send>;

    fn snap(total_kb: u64, avail_kb: u64) -> SystemInfo {
        let mut s = SystemInfo::default();
        s.mem.total_kb = total_kb;
        s.mem.available_kb = avail_kb;
        s
    }

    /// A gate whose snapshot is synthetic and whose refresh is never called by
    /// these tests (they drive `await_admission_with` directly).
    fn gate_with(total_kb: u64, avail_kb: u64) -> Arc<ResourceGate> {
        Arc::new(ResourceGate::new(
            ResourceLimits::default(),
            snap(total_kb, avail_kb),
        ))
    }

    /// Admission that never changes: either always admitted or always denied.
    fn static_source(gate: &Arc<ResourceGate>) -> Source {
        let g = gate.clone();
        Box::new(move || {
            let g = g.clone();
            Box::pin(async move { g.admit() })
        })
    }

    /// Admission that denies for the first `denied_checks` calls, then admits.
    fn recovering_source(denied_checks: u32, seen: Arc<AtomicU32>) -> Source {
        Box::new(move || {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            let denied_checks = denied_checks;
            Box::pin(async move {
                if n < denied_checks {
                    let s = snap(15_551_000, 1_000_000);
                    let pf = ResourcePreflight::evaluate(&s, &ResourceLimits::default());
                    let denial = pf.denial.clone();
                    Admission {
                        decision: ResourceDecision::Denied {
                            reason: denial.as_ref().map(|d| d.to_line()).unwrap_or_default(),
                        },
                        denial,
                        preflight: pf,
                        overridden: false,
                        override_reason: None,
                        override_operator: None,
                    }
                } else {
                    Admission {
                        decision: ResourceDecision::Allow,
                        denial: None,
                        preflight: ResourcePreflight::evaluate(
                            &snap(15_551_000, 9_000_000),
                            &ResourceLimits::default(),
                        ),
                        overridden: false,
                        override_reason: None,
                        override_operator: None,
                    }
                }
            })
        })
    }

    fn fast(max_attempts: u32) -> ResourceRetryPolicy {
        ResourceRetryPolicy {
            max_attempts,
            initial_delay_ms: 1,
            max_delay_ms: 2,
        }
    }

    #[tokio::test]
    async fn preflight_success_admits_immediately_with_zero_rechecks() {
        let gate = gate_with(15_551_000, 9_000_000); // ~58% available
        let out = await_admission_with(
            static_source(&gate),
            ResourceRetryPolicy::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(out.admitted());
        match out {
            ResourceWaitOutcome::Admitted { attempts, .. } => assert_eq!(attempts, 0),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn preflight_failure_returns_structured_denial_after_bounded_waits() {
        let gate = gate_with(15_551_000, 1_000_000); // ~6% available
        let out =
            await_admission_with(static_source(&gate), fast(2), &CancellationToken::new()).await;
        assert!(!out.admitted());
        let d = out.denial().expect("denial");
        assert_eq!(d.kind, ResourceKind::Memory);
        assert!(d.current.is_some() && d.limit.is_some());
        assert!(d.used_pct.unwrap() > 90.0);
        match out {
            ResourceWaitOutcome::Exhausted { attempts, .. } => assert_eq!(attempts, 2),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_recovers_when_the_host_frees_memory() {
        let seen = Arc::new(AtomicU32::new(0));
        // Denied twice, then admitted: exactly the "wait for resources" case.
        let out = await_admission_with(
            recovering_source(2, seen.clone()),
            fast(4),
            &CancellationToken::new(),
        )
        .await;
        assert!(out.admitted(), "{out:?}");
        match out {
            ResourceWaitOutcome::Admitted { attempts, .. } => assert_eq!(attempts, 2),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            seen.load(Ordering::SeqCst),
            3,
            "one check per attempt + the success"
        );
    }

    #[tokio::test]
    async fn retry_budget_is_hard_capped_never_infinite() {
        let gate = gate_with(15_551_000, 1_000_000);
        let policy = fast(5);
        let start = std::time::Instant::now();
        let out =
            await_admission_with(static_source(&gate), policy, &CancellationToken::new()).await;
        assert!(!out.admitted());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must not poll forever"
        );
        assert_eq!(policy.total_checks(), 6);
        // 1 + 2 + 2 + 2 + 2, capped by `max_delay_ms` from the second delay on.
        assert_eq!(policy.max_total_wait(), Duration::from_millis(9));
    }

    #[tokio::test]
    async fn zero_retry_budget_checks_exactly_once() {
        let gate = gate_with(15_551_000, 1_000_000);
        let out = await_admission_with(
            static_source(&gate),
            ResourceRetryPolicy::once(),
            &CancellationToken::new(),
        )
        .await;
        match out {
            ResourceWaitOutcome::Exhausted { attempts, .. } => assert_eq!(attempts, 0),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_ends_the_wait_immediately() {
        let policy = ResourceRetryPolicy {
            max_attempts: 100_000,
            initial_delay_ms: 10_000,
            max_delay_ms: 10_000,
        };
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        // A source that always denies: without cancellation this would spin for
        // a thousand years. It must return promptly.
        let handle = tokio::spawn(async move {
            let gate = Arc::new(ResourceGate::new(
                ResourceLimits::default(),
                snap(15_551_000, 1_000_000),
            ));
            await_admission_with(static_source(&gate), policy, &c).await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        let out = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("cancel must not hang")
            .unwrap();
        assert!(
            matches!(out, ResourceWaitOutcome::Cancelled { .. }),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn operator_override_admits_a_resource_blocked_task() {
        let gate = gate_with(15_551_000, 1_000_000);
        assert!(!gate.admit().allowed());
        gate.grant_override("operator confirmed the memory is fine", "tui");
        let out = await_admission_with(
            static_source(&gate),
            ResourceRetryPolicy::once(),
            &CancellationToken::new(),
        )
        .await;
        assert!(out.admitted(), "{out:?}");
    }

    #[test]
    fn backoff_is_clamped_to_the_configured_ceiling() {
        let p = ResourceRetryPolicy {
            max_attempts: 10,
            initial_delay_ms: 100,
            max_delay_ms: 1_000,
        };
        assert_eq!(p.delay_for(0), Duration::from_millis(100));
        assert_eq!(p.delay_for(1), Duration::from_millis(200));
        assert_eq!(p.delay_for(2), Duration::from_millis(400));
        assert_eq!(p.delay_for(3), Duration::from_millis(800));
        assert_eq!(p.delay_for(4), Duration::from_millis(1_000));
        assert_eq!(
            p.delay_for(50),
            Duration::from_millis(1_000),
            "never exceeds"
        );
    }

    #[test]
    fn total_wait_budget_is_bounded() {
        let p = ResourceRetryPolicy {
            max_attempts: 4,
            initial_delay_ms: 100,
            max_delay_ms: 500,
        };
        assert_eq!(p.max_total_wait(), Duration::from_millis(1_200));
    }

    #[tokio::test]
    async fn exhausted_outcome_offers_the_operator_real_options() {
        let gate = gate_with(15_551_000, 1_000_000);
        let out =
            await_admission_with(static_source(&gate), fast(0), &CancellationToken::new()).await;
        let r = out.remediations();
        assert!(r.contains(&Remediation::Retry));
        assert!(r.contains(&Remediation::Wait));
        assert!(r.contains(&Remediation::SwitchModel));
        assert!(r.contains(&Remediation::OpenDetails));
        assert!(r.contains(&Remediation::OperatorOverride));
        assert!(r.contains(&Remediation::Cancel));
        assert!(out.summary().contains("resource wait exhausted"));
    }

    #[test]
    fn a_denied_preflight_reads_as_resource_not_broken_tool() {
        // The exact UX regression: the message must not imply the tool failed.
        let s = snap(15_551_000, 1_000_000);
        let pf = ResourcePreflight::evaluate(&s, &ResourceLimits::default());
        let d = pf.denial.as_ref().unwrap();
        let line = d.to_line();
        assert!(line.contains("Host memory"), "{line}");
        assert!(!line.to_lowercase().contains("tool"), "{line}");
        assert_eq!(pf.memory.status, ResourceStatus::Blocked);
        assert!(!pf.verdict().is_admitted());
        assert!(pf.verdict().denial().is_some());
    }

    #[test]
    fn tight_headroom_is_constrained_not_denied() {
        let l = ResourceLimits {
            tight_headroom_ratio: 0.30,
            ..ResourceLimits::default()
        };
        // 3 GB of 16 GB = 18.75% available: above the 15% floor.
        let pf = ResourcePreflight::evaluate(&snap(16_000_000, 3_000_000), &l);
        assert!(pf.verdict().is_admitted());
        assert!(
            matches!(pf.verdict(), PreflightVerdict::Constrained(v) if v.contains(&ResourceKind::Memory))
        );
    }
}
