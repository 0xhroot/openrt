//! Resource budgets and the gate that enforces them.

use std::sync::RwLock;

use crate::preflight::{PreflightVerdict, ResourceDenial, ResourcePreflight};
use crate::system::SystemInfo;

/// Budgets a single task may consume before the gate blocks further work.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceLimits {
    /// Legacy *absolute* ceiling: fraction of host memory that may be in use
    /// before admission is refused.
    ///
    /// This is the **fallback** path. It only applies when
    /// [`Self::min_headroom_ratio`] is `0.0` *and*
    /// [`Self::min_free_memory_bytes`] is `None` — i.e. when no headroom rule
    /// is configured. See [`Self::min_headroom_ratio`] for why an absolute
    /// "used < X%" rule is the wrong primary signal on a developer host.
    pub max_mem_used_ratio: f64,
    /// **Primary admission rule**: the fraction of total host memory that must
    /// remain *available* before work is admitted. `0.0` disables the headroom
    /// guard (falling back to [`Self::max_mem_used_ratio`]).
    ///
    /// Why headroom and not "used": the host runs other things — a local model
    /// server, a browser, a build. A model holding 5 GB on a 16 GB host is
    /// normal, not a reason to refuse a 20 KB `filesystem.read`. What matters
    /// is whether starting this operation would leave enough memory to
    /// complete it. That is headroom.
    pub min_headroom_ratio: f64,
    /// Absolute minimum available bytes. Overrides [`Self::min_headroom_ratio`]
    /// when set. `None` = use the ratio.
    pub min_free_memory_bytes: Option<u64>,
    /// Fraction of total memory *above* the floor that still counts as
    /// [`crate::preflight::ResourceStatus::Tight`]. Admitting is never
    /// blocked for being tight — the operator is only told. `0.0` disables the
    /// warning band.
    pub tight_headroom_ratio: f64,
    /// Soft ceiling hint (% of one logical core the task may want). Not
    /// enforced by a kernel cgroup in Phase 1 — informational.
    pub max_cpu_percent: u32,
    /// Minimum free disk bytes required before allowing a task (e.g. model
    /// downloads in later phases). `None` = allow any free space.
    pub min_free_disk_bytes: Option<u64>,
    /// Whether an operator may authorise a one-shot override of a denial.
    ///
    /// Enforced at the gate, not just in the UI: when this is `false`,
    /// [`ResourceGate::grant_override`] is a no-op and `Remediation::OperatorOverride`
    /// is never offered. The model can never reach this either way — the method
    /// is not part of any tool surface — but the config must actually bind.
    pub allow_operator_override: bool,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_mem_used_ratio: 0.5,
            // 15% of total must remain free to admit work. Measured against a
            // live 16 GB host running Ollama with a 7B model resident: ~7.9 GB
            // available, ~49% headroom — admitted, where the old absolute rule
            // refused it at 50.4% used.
            min_headroom_ratio: 0.15,
            min_free_memory_bytes: None,
            tight_headroom_ratio: 0.30,
            max_cpu_percent: 80,
            min_free_disk_bytes: None,
            allow_operator_override: true,
        }
    }
}

/// An operator-authorised one-shot admission.
///
/// Produced only by an explicit operator action (CLI/TUI), **never** by the
/// model. Consumed by the next gate check and then cleared, so an override
/// cannot silently become standing policy.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceOverride {
    /// Why the operator bypassed the refusal — recorded verbatim in the audit.
    pub reason: String,
    /// Who authorised it, for the audit record.
    pub operator: String,
}

/// Result of a resource check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceDecision {
    Allow,
    /// Explicitly denied because a budget would be exceeded.
    Denied {
        reason: String,
    },
}

/// A gate decision paired with the structured explanation behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    pub decision: ResourceDecision,
    pub denial: Option<ResourceDenial>,
    pub preflight: ResourcePreflight,
    /// True when this `Allow` came from an operator override rather than from
    /// the limits. Surfaced so the audit row can say so.
    pub overridden: bool,
    /// Why the operator authorised the bypass, when `overridden` is true.
    ///
    /// The override record exists to be auditable, so its reason and operator
    /// must survive to the caller that writes the audit row. They used to be
    /// dropped at the point of consumption, which made every `resource.override`
    /// row unattributable: the chain could say that an override happened but
    /// never who authorised it or why.
    pub override_reason: Option<String>,
    /// Who authorised the bypass, when `overridden` is true.
    pub override_operator: Option<String>,
}

impl Admission {
    pub fn allowed(&self) -> bool {
        matches!(self.decision, ResourceDecision::Allow)
    }

    /// The preflight verdict (admit / constrained / deny).
    pub fn verdict(&self) -> PreflightVerdict {
        match &self.denial {
            Some(d) => PreflightVerdict::Denied(d.clone()),
            None => PreflightVerdict::Admitted,
        }
    }
}

/// Evaluates resource usage against [`ResourceLimits`] from a host snapshot.
/// Thread-safe; the snapshot is refreshed via [`ResourceGate::refresh`].
pub struct ResourceGate {
    limits: ResourceLimits,
    snapshot: RwLock<SystemInfo>,
    /// One-shot operator bypass. `None` = no override outstanding.
    override_slot: RwLock<Option<ResourceOverride>>,
}

impl std::fmt::Debug for ResourceGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceGate")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ResourceGate {
    pub fn new(limits: ResourceLimits, snapshot: SystemInfo) -> Self {
        Self {
            limits,
            snapshot: RwLock::new(snapshot),
            override_slot: RwLock::new(None),
        }
    }

    pub fn current(&self) -> SystemInfo {
        self.snapshot.read().map(|s| s.clone()).unwrap_or_default()
    }

    /// The limits this gate enforces.
    pub fn limits(&self) -> &ResourceLimits {
        &self.limits
    }

    /// Pull a fresh host snapshot.
    pub async fn refresh(&self) {
        let si = tokio::task::spawn_blocking(SystemInfo::collect)
            .await
            .unwrap_or_else(|_| SystemInfo::collect());
        if let Ok(mut s) = self.snapshot.write() {
            *s = si;
        }
    }

    /// Compute the current resource posture without deciding anything.
    /// Cheap: pure arithmetic over the cached snapshot.
    pub fn preflight(&self) -> ResourcePreflight {
        ResourcePreflight::evaluate(&self.current(), &self.limits)
    }

    /// Operator-authorised one-shot bypass of the NEXT denial.
    ///
    /// The override is consumed by the next [`ResourceGate::admit`] call and
    /// is not persisted anywhere else. Callers are responsible for writing the
    /// audit row — the gate deliberately does not own the audit authority.
    ///
    /// Returns `false` (and changes nothing) when
    /// [`ResourceLimits::allow_operator_override`] is `false`: a deployment
    /// that forbids overrides must refuse them even if a caller asks.
    pub fn grant_override(&self, reason: impl Into<String>, operator: impl Into<String>) -> bool {
        if !self.limits.allow_operator_override {
            return false;
        }
        if let Ok(mut slot) = self.override_slot.write() {
            *slot = Some(ResourceOverride {
                reason: reason.into(),
                operator: operator.into(),
            });
            true
        } else {
            false
        }
    }

    /// True when an override is outstanding (without consuming it).
    pub fn override_pending(&self) -> bool {
        self.override_slot
            .read()
            .map(|s| s.is_some())
            .unwrap_or(false)
    }

    /// Take the outstanding override, if any.
    fn take_override(&self) -> Option<ResourceOverride> {
        self.override_slot.write().ok().and_then(|mut s| s.take())
    }

    /// Read the outstanding override without consuming it.
    fn peek_override(&self) -> Option<ResourceOverride> {
        self.override_slot.read().ok().and_then(|s| s.clone())
    }

    /// Check the current snapshot against the limits, with structure.
    ///
    /// Consumes an outstanding operator override: an override authorises
    /// exactly one admission.
    pub fn admit(&self) -> Admission {
        self.evaluate(true)
    }

    /// Check the current snapshot against the limits **without** consuming an
    /// outstanding override.
    ///
    /// This is a *probe*: a scheduler checkpoint re-check, a workflow-compile
    /// resource context, or a sandbox admission. None of those is the operator's
    /// actual work, so none of them may spend the one-shot grant. Routing the
    /// probe through [`ResourceGate::admit`] made the grant's fate depend on
    /// whichever unrelated call happened to land first — the grant would be
    /// silently eaten by a status check and the real work would still be
    /// refused, which fails closed but makes the feature non-deterministic.
    pub fn check_peek(&self) -> ResourceDecision {
        self.evaluate(false).decision
    }

    /// Like [`ResourceGate::admit`] but does **not** consume an outstanding
    /// override. Use this to ask "what would happen?" — to report a standing
    /// denial, or to check whether a grant is still armed — without spending it.
    pub fn admit_peek(&self) -> Admission {
        self.evaluate(false)
    }

    /// Check the current snapshot against the limits.
    ///
    /// Kept as the simple boolean surface for existing callers; the runtime
    /// gate uses [`ResourceGate::admit`] so it can record the numbers.
    ///
    /// Consumes an outstanding override, because a caller reaching for the
    /// boolean surface is making the admission decision. Probes must use
    /// [`ResourceGate::check_peek`] instead.
    pub fn check(&self) -> ResourceDecision {
        self.admit().decision
    }

    /// The one decision function, so `admit` and `check_peek` cannot drift.
    fn evaluate(&self, consume: bool) -> Admission {
        let preflight = self.preflight();
        match preflight.denial.clone() {
            None => Admission {
                decision: ResourceDecision::Allow,
                denial: None,
                preflight,
                overridden: false,
                override_reason: None,
                override_operator: None,
            },
            Some(denial) => {
                // A peek must not spend the grant; it reports what the decision
                // *would* be.
                let ov = if consume {
                    self.take_override()
                } else {
                    self.peek_override()
                };
                match ov {
                    Some(ov) => Admission {
                        decision: ResourceDecision::Allow,
                        denial: Some(denial),
                        preflight,
                        overridden: true,
                        override_reason: Some(ov.reason),
                        override_operator: Some(ov.operator),
                    },
                    None => Admission {
                        decision: ResourceDecision::Denied {
                            reason: denial.to_line(),
                        },
                        denial: Some(denial),
                        preflight,
                        overridden: false,
                        override_reason: None,
                        override_operator: None,
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::DiskUsage;

    fn snap_avail_mem(available_kb: u64, total_kb: u64) -> SystemInfo {
        SystemInfo {
            os: "linux".into(),
            kernel: "1.0".into(),
            arch: "x86_64".into(),
            cpu: Default::default(),
            uptime_secs: 0,
            mem: crate::system::MemInfo {
                total_kb,
                available_kb,
            },
            disk: DiskUsage {
                total_bytes: 1 << 40,
                avail_bytes: 1 << 40,
            },
        }
    }

    #[test]
    fn allows_within_limits() {
        let gate = ResourceGate::new(ResourceLimits::default(), snap_avail_mem(90_000, 100_000));
        assert_eq!(gate.check(), ResourceDecision::Allow);
    }

    #[test]
    fn denies_when_memory_depleted() {
        let gate = ResourceGate::new(ResourceLimits::default(), snap_avail_mem(10_000, 100_000));
        assert!(matches!(gate.check(), ResourceDecision::Denied { .. }));
    }

    #[test]
    fn disables_memory_guard_at_zero() {
        // Disabling the headroom guard falls the gate back to the legacy
        // absolute ratio, which must then be disabled explicitly too.
        let l = ResourceLimits {
            max_mem_used_ratio: 0.0,
            min_headroom_ratio: 0.0,
            min_free_memory_bytes: None,
            tight_headroom_ratio: 0.0,
            ..ResourceLimits::default()
        };
        let gate = ResourceGate::new(l, snap_avail_mem(1, 100_000));
        assert_eq!(gate.check(), ResourceDecision::Allow);
    }

    #[test]
    fn headroom_rule_admits_what_the_absolute_rule_refused() {
        // The live-battery shape: ~50.4% used on a 15 GB host. The absolute
        // `used < 50%` ceiling refused this; the headroom rule admits it
        // because ~49.6% of the host is still available.
        let gate = ResourceGate::new(
            ResourceLimits::default(),
            snap_avail_mem(7_700_000, 15_551_000),
        );
        let a = gate.admit();
        assert!(a.allowed(), "{:?}", a.denial);
        assert!(a.preflight.memory.used_pct.unwrap() > 50.0);
    }

    #[test]
    fn genuine_memory_pressure_still_denies_with_numbers() {
        let gate = ResourceGate::new(
            ResourceLimits::default(),
            snap_avail_mem(1_000_000, 15_551_000), // ~6% available
        );
        let a = gate.admit();
        assert!(!a.allowed());
        let d = a.denial.expect("structured denial");
        assert_eq!(d.kind, crate::preflight::ResourceKind::Memory);
        assert!(d.used_pct.is_some());
        assert!(d.headroom.is_some());
    }

    #[test]
    fn operator_override_authorises_exactly_one_admission() {
        let gate = ResourceGate::new(
            ResourceLimits::default(),
            snap_avail_mem(1_000_000, 15_551_000),
        );
        assert!(!gate.admit().allowed());
        gate.grant_override("operator saw the memory freed up", "tui");
        assert!(gate.override_pending());
        let first = gate.admit();
        assert!(first.allowed(), "override must admit");
        assert!(first.overridden, "and say it was an override");
        // Consumed: the very next check is refused again.
        let second = gate.admit();
        assert!(
            !second.allowed(),
            "an override is one-shot, never standing policy"
        );
        assert!(!second.overridden);
    }

    #[test]
    fn override_is_not_consumed_when_nothing_is_blocked() {
        let gate = ResourceGate::new(
            ResourceLimits::default(),
            snap_avail_mem(9_000_000, 15_551_000),
        );
        gate.grant_override("belt and braces", "cli");
        let a = gate.admit();
        assert!(a.allowed());
        assert!(!a.overridden, "a healthy host is not an override");
        assert!(
            gate.override_pending(),
            "override must survive an unused check"
        );
    }

    #[test]
    fn disk_floor_enforced() {
        let l = ResourceLimits {
            min_free_disk_bytes: Some((1 << 40) + 1),
            ..ResourceLimits::default()
        };
        let gate = ResourceGate::new(l, snap_avail_mem(50_000, 100_000));
        assert!(matches!(gate.check(), ResourceDecision::Denied { .. }));
    }
    /// A refusal stays a refusal when nobody authorises it.
    #[test]
    fn a_refusal_is_a_refusal_and_not_an_allow() {
        let gate = ResourceGate::new(ResourceLimits::default(), snap_avail_mem(10_000, 100_000));
        let admission = gate.admit();
        assert!(!admission.allowed(), "memory pressure must refuse");
        assert!(
            admission.denial.is_some(),
            "a refusal must carry its reason"
        );
        assert!(
            !admission.overridden,
            "nothing authorised this, so it must not claim to be overridden"
        );
    }

    /// The one-shot grant must be attributable, or the audit row cannot say who
    /// authorised it. The reason and operator used to be dropped here.
    #[test]
    fn a_consumed_override_carries_its_reason_and_operator() {
        let mut limits = ResourceLimits::default();
        limits.allow_operator_override = true;
        let gate = ResourceGate::new(limits, snap_avail_mem(10_000, 100_000));

        assert!(gate.grant_override("operator needs one more attempt", "alice"));
        let admission = gate.admit();
        assert!(admission.allowed());
        assert!(admission.overridden);
        assert_eq!(
            admission.override_reason.as_deref(),
            Some("operator needs one more attempt")
        );
        assert_eq!(admission.override_operator.as_deref(), Some("alice"));
    }

    /// Policy that forbids overrides must refuse even when asked directly.
    #[test]
    fn policy_refuses_an_override_without_arming_anything() {
        let mut limits = ResourceLimits::default();
        limits.allow_operator_override = false;
        let gate = ResourceGate::new(limits, snap_avail_mem(10_000, 100_000));

        assert!(
            !gate.grant_override("should not work", "mallory"),
            "a deployment that forbids overrides must refuse them"
        );
        assert!(!gate.override_pending(), "nothing may be armed");
        let admission = gate.admit();
        assert!(
            !admission.allowed(),
            "the refusal must stand after the attempt"
        );
    }

    /// A one-shot grant authorises exactly one admission, never a standing policy.
    #[test]
    fn an_override_authorises_exactly_one_admission() {
        let mut limits = ResourceLimits::default();
        limits.allow_operator_override = true;
        let gate = ResourceGate::new(limits, snap_avail_mem(10_000, 100_000));

        assert!(gate.grant_override("one shot", "alice"));
        assert!(gate.admit().allowed(), "first admission uses the grant");
        assert!(
            !gate.admit().allowed(),
            "the grant is spent; the second admission must be refused"
        );
    }

    /// The bug this closes: a status probe spent the operator's one-shot grant, so
    /// the work it was meant for was refused with the operator having no idea why.
    #[test]
    fn a_status_probe_does_not_spend_the_override() {
        let mut limits = ResourceLimits::default();
        limits.allow_operator_override = true;
        let gate = ResourceGate::new(limits, snap_avail_mem(10_000, 100_000));
        assert!(gate.grant_override("one shot", "alice"));

        // Any number of probes.
        for _ in 0..5 {
            let peeked = gate.admit_peek();
            assert!(
                peeked.overridden,
                "a probe should see that a grant is armed"
            );
            assert!(gate.check_peek() == ResourceDecision::Allow);
        }
        assert!(
            gate.override_pending(),
            "probing must leave the grant intact"
        );

        // The real admission still works, and is still one-shot.
        assert!(gate.admit().allowed(), "the grant survived the probes");
        assert!(!gate.admit().allowed(), "and is still only one admission");
    }

    /// A probe must never be able to turn a refusal into an allow either.
    #[test]
    fn a_status_probe_cannot_authorise_anything_by_itself() {
        let mut limits = ResourceLimits::default();
        limits.allow_operator_override = true;
        let gate = ResourceGate::new(limits, snap_avail_mem(10_000, 100_000));

        // No grant armed at all.
        assert!(!gate.override_pending());
        assert_eq!(
            gate.check_peek(),
            ResourceDecision::Denied {
                reason: gate
                    .admit_peek()
                    .denial
                    .as_ref()
                    .map(|d| d.to_line())
                    .unwrap_or_default()
            }
        );
        assert!(
            !gate.admit().allowed(),
            "and the real admission still refuses"
        );
    }

    /// Repeated `check_peek` calls are stable — a probe cannot drift into granting.
    #[test]
    fn repeated_probes_are_stable_and_never_grant() {
        let mut limits = ResourceLimits::default();
        limits.allow_operator_override = true;
        let gate = ResourceGate::new(limits, snap_avail_mem(10_000, 100_000));
        let first = gate.check_peek();
        for _ in 0..10 {
            assert_eq!(gate.check_peek(), first, "a probe must be idempotent");
        }
        assert!(matches!(first, ResourceDecision::Denied { .. }));
    }
}
