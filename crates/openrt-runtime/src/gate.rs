//! The runtime-side gate: permission + resource checks before any tool runs.

use std::sync::Arc;

use openrt_core::{id::SessionTag, id::TaskTag, DomainEvent, EventKind, Id, SessionId, TaskId};
use openrt_permissions::{
    ApprovalRequest, Decision, PermissionContext, PermissionDecision, PermissionRequest,
    PermissionResolver, Policy, PolicyEvaluator,
};
use openrt_resources::{ResourceGate, ResourceLimits};
use openrt_telemetry::EventBus;
use openrt_tools::{ToolError, ToolExecutionContext, ToolExecutor, ToolOutput, ToolRegistry};

use uuid::Uuid;

/// Implements [`ToolExecutor`] with permission and resource enforcement.
///
/// Agents only ever see this gate; a denied call returns an error and the
/// underlying tool is never invoked. This is the single point that makes the
/// "a denied tool must not execute" guarantee testable.
pub struct GateExecutor {
    registry: ToolRegistry,
    policy: Arc<Policy>,
    resources: Arc<ResourceGate>,
    bus: EventBus,
    /// Optional approval resolver for `Ask` policy decisions (fail closed when
    /// absent — an `Ask` then behaves as a hard deny).
    resolver: Option<Arc<dyn PermissionResolver>>,
    /// ADR-008 kill switch: when engaged, tools in its scope are blocked with
    /// a `SafetyHalt` reaction before they can run.
    kill: Option<Arc<openrt_safety::KillSwitch>>,
    /// ADR-002 chained audit log: every computer-use permission decision and
    /// kill-switch reaction is recorded here (fail-closed on writes).
    audit: Option<Arc<openrt_audit::AuditLog>>,
    /// Phase 8: unified sandbox authority. Tools with a declared sandbox
    /// requirement pass profile/backend/resource admission here — between
    /// permission policy and execution — so no tool bypasses the sandbox.
    sandbox: Option<Arc<openrt_sandbox::SandboxManager>>,
    /// M10.7 ECC: deny-only asset-rule layer consulted BEFORE policy. An
    /// enabled `Deny` rule from the AssetStore hard-denies a tool call no
    /// matter what the policy allows — rules can only restrict, never grant.
    rules: Option<Arc<AssetRuleLayer>>,
    /// M10.7 lifecycle: shared hook layer — the gate fires `ToolCall` hooks
    /// for every decision (allowed or denied) at the enforcement edge.
    hooks: Option<Arc<crate::hooks::HookLayer>>,
    /// M10.9 AgentShield: deterministic security scan of every resolved call
    /// before policy adjudication — the most restrictive finding maps onto
    /// the gate's existing authorities (never a parallel policy engine).
    shield: Option<Arc<crate::shield::ShieldLayer>>,
    /// Operator tiered-approval memory (permission memory). Recorded ONLY
    /// after the resolver explicitly answers `AllowSession`/`AllowWorkspace`
    /// for an `Ask` decision, scoped to the exact `(session?, tool, scope)`
    /// that was approved. Consulted only on the `Ask` path (a policy `Deny`
    /// still denies), never widens beyond the approved scope, lives for this
    /// process only (no silent promotion to a durable/global grant).
    grants: std::sync::Mutex<Vec<GrantEntry>>,
}

/// One recorded tiered-approval grant (see [`GateExecutor::grants`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct GrantEntry {
    /// `false` = session tier (must match `sid`), `true` = workspace tier
    /// (any session of this process).
    workspace: bool,
    /// Session tier: the session the operator approved for.
    sid: Option<String>,
    tool: String,
    /// Exact scope that was approved (`None` = the request had no scope).
    scope: Option<String>,
}

/// One entry of the gate's deny-only rule view: an enabled `Deny` rule from
/// the durable [`openrt_workspace_assets::AssetStore`], flattened for exact
/// `(scope, subject)` matching (mirrors `AssetStore::evaluate_rule`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRuleEntry {
    pub scope: String,
    pub subject: String,
    pub id: String,
}

/// M10.7 ECC: the gate's deny-only asset-rule layer. Refreshed by the
/// runtime from the durable store on every asset load/save. Consulted before
/// policy evaluation: a match turns any outcome into a hard deny. There is
/// no allow side — a rule can never widen permissions (fail closed).
#[derive(Debug, Default)]
pub struct AssetRuleLayer {
    entries: std::sync::RwLock<Vec<GateRuleEntry>>,
}

impl AssetRuleLayer {
    /// Replace the layer's entries (called on every asset load/save —
    /// the store is the one authority for rule content).
    pub fn set(&self, entries: Vec<GateRuleEntry>) {
        *self.entries.write().expect("asset rule layer poisoned") = entries;
    }

    /// Load all enabled `Deny` rules out of a store snapshot.
    pub fn refresh_from(&self, assets: &openrt_workspace_assets::AssetStore) {
        let entries = assets
            .deny_rules()
            .into_iter()
            .map(|(scope, subject, id)| GateRuleEntry { scope, subject, id })
            .collect();
        self.set(entries);
    }

    /// Exact match (scope, subject) — same matcher semantics as
    /// `AssetStore::evaluate_rule`, deny side only.
    pub fn denies(&self, scope: &str, subject: &str) -> Option<String> {
        self.entries
            .read()
            .expect("asset rule layer poisoned")
            .iter()
            .find(|e| e.scope == scope && e.subject == subject)
            .map(|e| e.id.clone())
    }
}

impl std::fmt::Debug for GateExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateExecutor")
            .field("tools", &self.registry.names())
            .field("policy_rules", &self.policy.rules.len())
            .field("resolver_attached", &self.resolver.is_some())
            .field("audit_attached", &self.audit.is_some())
            .finish()
    }
}

impl GateExecutor {
    /// Convenience constructor without an approval resolver (every `Ask`
    /// decision is a hard deny — the fail-closed default).
    pub fn new(
        registry: ToolRegistry,
        policy: Arc<Policy>,
        resources: Arc<ResourceGate>,
        bus: EventBus,
    ) -> Self {
        Self::with_resolver(registry, policy, resources, bus, None)
    }

    /// Attach an optional approval resolver consulted on `Ask` decisions
    /// ([ask boundary](openrt_permissions::PermissionResolver)). `None` keeps
    /// the fail-closed default.
    pub fn with_resolver(
        registry: ToolRegistry,
        policy: Arc<Policy>,
        resources: Arc<ResourceGate>,
        bus: EventBus,
        resolver: Option<Arc<dyn PermissionResolver>>,
    ) -> Self {
        Self::with_kill(registry, policy, resources, bus, resolver, None)
    }

    /// Attach an approval resolver and an optional ADR-008 kill switch.
    pub fn with_kill(
        registry: ToolRegistry,
        policy: Arc<Policy>,
        resources: Arc<ResourceGate>,
        bus: EventBus,
        resolver: Option<Arc<dyn PermissionResolver>>,
        kill: Option<Arc<openrt_safety::KillSwitch>>,
    ) -> Self {
        Self {
            registry,
            policy,
            resources,
            bus,
            sandbox: None,
            resolver,
            kill,
            audit: None,
            rules: None,
            hooks: None,
            shield: None,
            grants: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Attach the ADR-002 chained audit log (P2.7). `None` keeps the gate as a
    /// pure permission boundary (only used by tests; the runtime always passes
    /// the shared log).
    /// Attach the Phase 8 sandbox authority for gate-time admission of
    /// tools with a declared sandbox requirement.
    pub fn with_sandbox(mut self, sandbox: Option<Arc<openrt_sandbox::SandboxManager>>) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_audit(mut self, audit: Option<Arc<openrt_audit::AuditLog>>) -> Self {
        self.audit = audit;
        self
    }

    /// Attach the M10.7 ECC deny-only asset-rule layer (consulted before
    /// policy; can only deny). `None` disables the layer (tests only — the
    /// runtime always passes one).
    pub fn with_asset_rules(mut self, rules: Option<Arc<AssetRuleLayer>>) -> Self {
        self.rules = rules;
        self
    }

    /// Attach the shared M10.7 hook layer so the gate fires `ToolCall`
    /// hooks for every decision. `None` skips hook firing.
    pub fn with_hooks(mut self, hooks: Option<Arc<crate::hooks::HookLayer>>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Attach the M10.9 AgentShield layer. `None` (the default) skips
    /// scanning; the runtime always attaches one.
    pub fn with_shield(mut self, shield: Option<Arc<crate::shield::ShieldLayer>>) -> Self {
        self.shield = shield;
        self
    }

    /// Tools whose actions are computer-use and must therefore be recorded in
    /// the audit log (filesystem/terminal/screen/input/browser/python). Plain
    /// read-only builtins (`echo`, `system.info`, …) are excluded.
    fn is_computer_tool(tool: &str) -> bool {
        [
            "filesystem.",
            "terminal.",
            "screen.",
            "input.",
            "browser.",
            "python.",
            "computer.",
            // M7B tool fabric: side-effecting 7B tools record permission
            // decisions too (`tools.*`/`artifact.*` stay read-only/audited
            // elsewhere — they are capability-gated, not computer-scoped).
            "database.",
            "excel.",
            "ppt.",
            "docx.",
            "mcp.",
            "application.",
        ]
        .iter()
        .any(|p| tool.starts_with(p))
    }

    /// Record a permission decision / kill-switch reaction in the chained
    /// audit log. Non-computer tools are skipped. Write failures propagate
    /// (fail-closed) so the caller decides how to surface them.
    async fn record(
        &self,
        tool: &str,
        result: &str,
        ext: serde_json::Value,
    ) -> Result<u64, String> {
        let Some(audit) = &self.audit else {
            return Ok(0);
        };
        if !Self::is_computer_tool(tool) {
            return Ok(0);
        }
        audit
            .append("system", "permission.decision", tool, result, ext)
            .await
            .map_err(|e| e.to_string())
    }

    pub fn resource_gate(&self) -> &ResourceGate {
        &self.resources
    }

    fn correlate(&self, ctx: &ToolExecutionContext) -> (Option<SessionId>, Option<TaskId>) {
        let sid = ctx
            .session_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
            .map(Id::<SessionTag>::from_uuid);
        let tid = ctx
            .task_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
            .map(Id::<TaskTag>::from_uuid);
        (sid, tid)
    }

    /// Handle a `Decision::Ask`: consult the optional resolver and either
    /// approve (emit `PermissionApproved`), or deny deterministically. `Ask`
    /// is never implicitly allowed and never bypasses the policy's own deny
    /// outcome (the evaluator only reaches `Ask` when no deny matched).
    /// Record a shield enforcement outcome on the audit trail (computer
    /// tools only — `record` enforces that) and fold any recording failure
    /// into the reason so it is never silent.
    async fn shield_audit(
        &self,
        tool: &str,
        action: &str,
        pctx: &PermissionContext,
        reason: String,
    ) -> String {
        let rec_ext = serde_json::json!({
            "session_id": pctx.session_id,
            "task_id": pctx.task_id,
            "reason": reason,
            "via": "agentshield",
        });
        match self.record(tool, action, rec_ext).await {
            Ok(_) => reason,
            Err(e) => format!("{reason} (audit recording also failed: {e})"),
        }
    }

    async fn resolve_ask(
        &self,
        tool: &str,
        args: &serde_json::Value,
        pctx: &PermissionContext,
        _req: PermissionRequest,
        _sid: Option<SessionId>,
        _tid: Option<TaskId>,
    ) -> Result<(), String> {
        let Some(resolver) = &self.resolver else {
            let reason =
                "ask decision requires approval; no resolver configured (fail closed)".to_string();
            return Err(reason);
        };

        let approval = ApprovalRequest {
            request_id: Uuid::new_v4().to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            scope: args
                .get("path")
                .or_else(|| args.get("root"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            context: pctx.clone(),
        };

        // Tiered permission memory: an identical request the operator already
        // approved for this session (or workspace) is released here. Only
        // reached when the evaluator returned `Ask` (a `Deny` never gets
        // this far), and only for the EXACT approved (tool, scope). The raw
        // `pctx.session_id` string keys the grant (the typed correlate()
        // id would silently drop non-UUID session ids).
        {
            let grants = self.grants.lock().expect("gate grants poisoned");
            let sid = pctx.session_id.clone();
            let hit = grants.iter().any(|g| {
                g.tool == tool
                    && g.scope == approval.scope
                    && if g.workspace {
                        true
                    } else {
                        g.sid.is_some() && g.sid == sid
                    }
            });
            if hit {
                return Ok(());
            }
        }

        match resolver.resolve(&approval).await {
            Ok(PermissionDecision::Allow) => Ok(()),
            Ok(PermissionDecision::Deny) => Err("denied by approval resolver".to_string()),
            Ok(tier @ (PermissionDecision::AllowSession | PermissionDecision::AllowWorkspace)) => {
                let entry = GrantEntry {
                    workspace: tier == PermissionDecision::AllowWorkspace,
                    sid: pctx.session_id.clone(),
                    tool: tool.to_string(),
                    scope: approval.scope.clone(),
                };
                let mut grants = self.grants.lock().expect("gate grants poisoned");
                if !grants.contains(&entry) {
                    grants.push(entry);
                }
                Ok(())
            }
            Err(e) => Err(format!("approval resolver error: {e}")),
        }
    }

    async fn emit(
        &self,
        kind: EventKind,
        session: Option<SessionId>,
        task: Option<TaskId>,
        data: impl Into<serde_json::Value>,
    ) {
        let mut ev = DomainEvent::new(kind);
        if let Some(s) = session {
            ev = ev.with_session(s);
        }
        if let Some(t) = task {
            ev = ev.with_task(t);
        }
        self.bus.emit(ev.with_data(data)).await;
    }
}

#[async_trait::async_trait]
impl ToolExecutor for GateExecutor {
    async fn execute(
        &self,
        tool: &str,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        let (sid, tid) = self.correlate(ctx);

        // 1. Resolve — unknown tools are denied outright.
        let registry_tool = self
            .registry
            .get(tool)
            .ok_or_else(|| ToolError::NotFound(tool.to_string()))?;

        // 1.5 M10.7 lifecycle: every resolved tool call fires `ToolCall`
        // hooks at the enforcement edge (before adjudication — one
        // observation point for allowed and denied calls alike). A hook
        // firing failure blocks the call closed: hooks carry the same
        // permission checks as tools.
        if let Some(hooks) = &self.hooks {
            hooks
                .fire(openrt_workspace_assets::HookTarget::ToolCall)
                .await
                .map_err(|e| {
                    ToolError::Internal(
                        tool.to_string(),
                        format!("ToolCall hook failed (fail closed): {e}"),
                    )
                })?;
        }

        // 1.6 M10.7 ECC: deny-only asset rules consulted BEFORE policy — an
        // enabled `Deny` rule matching the tool id or any declared
        // capability hard-denies the call no matter what the policy allows
        // (rules restrict; they never grant). Gate rules use scope "gate".
        if let Some(layer) = &self.rules {
            let mut subjects = vec![tool.to_string()];
            subjects.extend(
                registry_tool
                    .spec()
                    .required_caps
                    .iter()
                    .map(|c| c.as_str().to_string()),
            );
            let mut denied: Option<String> = None;
            for subject in &subjects {
                if let Some(rule_id) = layer.denies("gate", subject) {
                    denied = Some(rule_id);
                    break;
                }
            }
            if let Some(rule_id) = denied {
                let reason = format!("denied by asset rule `{rule_id}` (deny-only ECC layer)");
                self.emit(
                    EventKind::PermissionDenied,
                    sid,
                    tid,
                    serde_json::json!({ "tool": tool, "reason": reason, "rule": rule_id }),
                )
                .await;
                return Err(ToolError::PermissionDenied {
                    tool: tool.to_string(),
                    reason,
                });
            }
        }

        // 2. Permission gate: evaluate the tool id AND every declared
        // `required_caps` on the tool spec (M6.2 — capability annotations are
        // enforcement, not documentation). First Deny wins; an Ask on any
        // check routes through the same resolver path.
        let pctx = PermissionContext {
            session_id: ctx.session_id.clone(),
            task_id: ctx.task_id.clone(),
            channel: ctx.channel.clone(),
        };
        let req = PermissionRequest::tool(tool, &args, &pctx);
        // Phase 11 forensic rule: GUI typed payloads never enter the durable
        // event log — for `computer.*`/`input.*`/`screen.*` tools the event
        // carries a length marker for `text`/`content` (the same redaction
        // the report and plan paths apply), while the live request used for
        // enforcement keeps the real value.
        let mut event_args = args.clone();
        if let Some(obj) = event_args.as_object_mut() {
            let gui_tool = tool.starts_with("computer.")
                || tool.starts_with("input.")
                || tool.starts_with("screen.");
            if gui_tool {
                for key in ["text", "content"] {
                    let len = obj.get(key).and_then(|v| v.as_str()).map(str::len);
                    if let Some(n) = len {
                        obj.insert(
                            key.to_string(),
                            serde_json::Value::String(format!("<redacted: {n} chars>")),
                        );
                    }
                }
            }
        }
        self.emit(
            EventKind::PermissionRequested,
            sid,
            tid,
            serde_json::json!({ "tool": tool, "args": event_args }),
        )
        .await;

        // 1.7 M10.9 AgentShield: deterministic security scan of the call at
        // the enforcement edge. The most restrictive action maps onto the
        // gate's EXISTING authorities — Allow/Warn proceed with findings
        // persisted, RequireApproval routes through the approval path, Deny
        // is a permission denial, Halt trips the shared ADR-008 kill switch.
        // No parallel policy engine; a scan or recording failure blocks the
        // call closed.
        if let Some(shield) = &self.shield {
            let caps: Vec<String> = registry_tool
                .spec()
                .required_caps
                .iter()
                .map(|c| c.as_str().to_string())
                .collect();
            let sandboxed = self
                .sandbox
                .as_ref()
                .is_some_and(|s| s.requirement(tool).is_some());
            let report = shield
                .scan(
                    tool,
                    &args,
                    caps,
                    sandboxed,
                    pctx.session_id.clone(),
                    pctx.task_id.clone(),
                )
                .await
                .map_err(|e| {
                    ToolError::Internal(
                        tool.to_string(),
                        format!("shield scan failed (fail closed): {e}"),
                    )
                })?;
            let shield_record = |status: openrt_shield::FindingStatus| {
                let report = report.clone();
                async move { shield.record(&report, status).await }
            };
            if !report.findings.is_empty() {
                let blocked = |reason: String| {
                    ToolError::Internal(
                        tool.to_string(),
                        format!("shield finding recording failed (fail closed): {reason}"),
                    )
                };
                match report.action {
                    openrt_shield::ShieldAction::Allow => {
                        shield_record(openrt_shield::FindingStatus::Open)
                            .await
                            .map_err(|e| blocked(e.to_string()))?;
                    }
                    openrt_shield::ShieldAction::Warn => {
                        shield_record(openrt_shield::FindingStatus::Warned)
                            .await
                            .map_err(|e| blocked(e.to_string()))?;
                    }
                    openrt_shield::ShieldAction::RequireApproval => {
                        shield_record(openrt_shield::FindingStatus::ApprovalRequired)
                            .await
                            .map_err(|e| blocked(e.to_string()))?;
                        let shield_req = PermissionRequest::tool(tool, &args, &pctx);
                        let r = self
                            .resolve_ask(tool, &args, &pctx, shield_req, sid, tid)
                            .await;
                        if let Err(reason) = r {
                            shield_record(openrt_shield::FindingStatus::Denied)
                                .await
                                .map_err(|e| blocked(e.to_string()))?;
                            let reason = format!("denied by AgentShield approval: {reason}");
                            self.emit(
                                EventKind::PermissionDenied,
                                sid,
                                tid,
                                serde_json::json!({ "tool": tool, "reason": reason, "via": "agentshield" }),
                            )
                            .await;
                            let reason = self
                                .shield_audit(tool, "shield denied", &pctx, reason)
                                .await;
                            return Err(ToolError::PermissionDenied {
                                tool: tool.to_string(),
                                reason,
                            });
                        }
                        self.emit(
                            EventKind::PermissionApproved,
                            sid,
                            tid,
                            serde_json::json!({ "tool": tool, "via": "agentshield" }),
                        )
                        .await;
                    }
                    openrt_shield::ShieldAction::Deny => {
                        let reason = format!(
                            "denied by AgentShield ({} finding(s), severity {})",
                            report.findings.len(),
                            report
                                .highest_severity
                                .map(|s| s.as_str())
                                .unwrap_or("unknown"),
                        );
                        shield_record(openrt_shield::FindingStatus::Denied)
                            .await
                            .map_err(|e| blocked(e.to_string()))?;
                        self.emit(
                            EventKind::PermissionDenied,
                            sid,
                            tid,
                            serde_json::json!({ "tool": tool, "reason": reason, "via": "agentshield" }),
                        )
                        .await;
                        let reason = self
                            .shield_audit(tool, "shield denied", &pctx, reason)
                            .await;
                        return Err(ToolError::PermissionDenied {
                            tool: tool.to_string(),
                            reason,
                        });
                    }
                    openrt_shield::ShieldAction::Halt => {
                        let reason = format!("AgentShield halted `{tool}` (critical finding)");
                        shield_record(openrt_shield::FindingStatus::Halted)
                            .await
                            .map_err(|e| blocked(e.to_string()))?;
                        if let Some(kill) = &self.kill {
                            kill.trip(&reason);
                        }
                        self.emit(
                            EventKind::SafetyHalt,
                            sid,
                            tid,
                            serde_json::json!({ "tool": tool, "reason": reason, "via": "agentshield" }),
                        )
                        .await;
                        let reason = self
                            .shield_audit(tool, "shield halted", &pctx, reason)
                            .await;
                        return Err(ToolError::KillSwitchEngaged {
                            tool: tool.to_string(),
                            reason,
                        });
                    }
                }
            }
        }

        let evaluator = PolicyEvaluator::new(&self.policy);
        let mut grant: Result<(), String> = match evaluator.evaluate(&req) {
            Decision::Allow => Ok(()),
            Decision::Deny => {
                let reason = "denied by policy (deny-by-default, no allow rule)".to_string();
                Err(reason)
            }
            Decision::Ask => {
                let r = self.resolve_ask(tool, &args, &pctx, req, sid, tid).await;
                if r.is_ok() {
                    self.emit(
                        EventKind::PermissionApproved,
                        sid,
                        tid,
                        serde_json::json!({ "tool": tool }),
                    )
                    .await;
                }
                r
            }
        };
        // Thread required_caps: each declared capability is a separate
        // permission request. Deny short-circuits fail-closed; Ask requires
        // approval for that capability as well.
        if grant.is_ok() {
            let spec_caps = registry_tool.spec().required_caps.clone();
            for cap in spec_caps {
                let cap_req = PermissionRequest {
                    tool_id: tool.to_string(),
                    capability: Some(cap.clone()),
                    scope: openrt_permissions::extract_scope_public(&args),
                    context: pctx.clone(),
                };
                match evaluator.evaluate(&cap_req) {
                    Decision::Allow => {}
                    Decision::Deny => {
                        grant = Err(format!(
                            "denied by policy for required capability `{cap}` (tool `{tool}`)"
                        ));
                        break;
                    }
                    Decision::Ask => {
                        let r = self
                            .resolve_ask(tool, &args, &pctx, cap_req, sid, tid)
                            .await;
                        if r.is_ok() {
                            self.emit(
                                EventKind::PermissionApproved,
                                sid,
                                tid,
                                serde_json::json!({ "tool": tool, "capability": cap.as_str() }),
                            )
                            .await;
                        }
                        grant = r;
                        if grant.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        // Emit grant/deny events once for the combined outcome.
        match &grant {
            Ok(()) => {
                self.emit(
                    EventKind::PermissionGranted,
                    sid,
                    tid,
                    serde_json::json!({ "tool": tool }),
                )
                .await;
            }
            Err(reason) => {
                self.emit(
                    EventKind::PermissionDenied,
                    sid,
                    tid,
                    serde_json::json!({ "tool": tool, "reason": reason }),
                )
                .await;
            }
        }
        // Track whether any Ask was approved so we can emit PermissionApproved
        // once (the resolver path no longer emits individually).
        // (Granted event already covers the allow outcome for tool-id checks.)
        match grant {
            Err(reason) => {
                // ADR-002: record the denial. If recording also fails, surface
                // that on top of the denial so the failure is never silent.
                let rec_ext = serde_json::json!({
                    "session_id": pctx.session_id,
                    "task_id": pctx.task_id,
                    "reason": reason,
                });
                let reason = match self.record(tool, "denied", rec_ext).await {
                    Ok(_) => reason,
                    Err(e) => format!("{reason} (audit recording also failed: {e})"),
                };
                return Err(ToolError::PermissionDenied {
                    tool: tool.to_string(),
                    reason,
                });
            }
            Ok(()) => {
                // ADR-002: an allowed computer-use action must be recorded
                // BEFORE it can run — a recording failure fails the call closed
                // (the action never executes without an audit trail).
                self.record(
                    tool,
                    "allowed",
                    serde_json::json!({
                        "session_id": pctx.session_id,
                        "task_id": pctx.task_id,
                    }),
                )
                .await
                .map_err(|e| {
                    ToolError::Internal(
                        tool.to_string(),
                        format!("audit recording failed (fail closed): {e}"),
                    )
                })?;
            }
        }

        // 3. Resource gate. The admission is structured, not a bare sentence:
        //    the numbers and the operator's options travel with the refusal so
        //    the TUI can present "RESOURCE LIMIT" instead of implying the tool
        //    itself is broken.
        let admission = self.resources.admit();
        let allowed = admission.allowed();
        let reason = match &admission.denial {
            Some(d) => d.to_line(),
            None => String::new(),
        };
        self.emit(
            EventKind::ResourceCheck,
            sid,
            tid,
            serde_json::json!({
                "tool": tool,
                "allowed": allowed,
                "reason": reason,
                "overridden": admission.overridden,
                "preflight": admission.preflight,
                "denial": admission.denial,
            }),
        )
        .await;
        if !allowed {
            return Err(ToolError::ResourceDenied {
                tool: tool.to_string(),
                reason,
            });
        }
        if admission.overridden {
            // Operator bypass in force. This MUST land in the ADR-002 chained
            // audit log, not only as telemetry: an override is the one place a
            // declared resource limit is deliberately not enforced, so leaving
            // it out of the tamper-evident chain would make it unauditable.
            // A write failure is surfaced (fail-closed) rather than swallowed —
            // an override nobody can audit is not an override.
            let ext = serde_json::json!({
                "tool": tool,
                "denial": admission.denial,
                "note": "admission authorised by a one-shot operator override",
            });
            if let Some(audit) = &self.audit {
                if let Err(e) = audit
                    .append(
                        "operator",
                        "resource.override",
                        tool,
                        "allowed",
                        ext.clone(),
                    )
                    .await
                {
                    return Err(ToolError::Internal(
                        tool.to_string(),
                        format!("resource override could not be audited: {e}"),
                    ));
                }
            }
            self.emit(
                EventKind::ResourceCheck,
                sid,
                tid,
                serde_json::json!({
                    "tool": tool,
                    "override": true,
                    "audited": self.audit.is_some(),
                    "note": "admission authorised by a one-shot operator override",
                }),
            )
            .await;
        }

        // 3b. Sandbox admission (Phase 8): a tool with a declared sandbox
        // requirement must pass profile/backend/resource admission before it
        // runs. The sandbox sits between policy and execution — a denied
        // admission never reaches the tool, and the decision is recorded in
        // the chained audit log by the manager itself (`sandbox.admit`).
        if let Some(sbx) = &self.sandbox {
            if sbx.requirement(tool).is_some() {
                if let Err(f) = sbx.precheck_tool(tool, "agent").await {
                    self.emit(
                        EventKind::PermissionDenied,
                        sid,
                        tid,
                        serde_json::json!({
                            "tool": tool,
                            "reason": f.render(),
                            "sandbox": true,
                            "code": f.code.as_str(),
                        }),
                    )
                    .await;
                    let _ = self
                        .record(
                            tool,
                            "sandbox_denied",
                            serde_json::json!({ "code": f.code.as_str(), "reason": f.render() }),
                        )
                        .await;
                    return Err(ToolError::PermissionDenied {
                        tool: tool.to_string(),
                        reason: f.render(),
                    });
                }
            }
        }

        // 3. Safety gate (ADR-008): a scoped HALT blocks the tool before it
        // runs. Non-scope tools are untouched, and the reaction is audited.
        if let Some(kill) = &self.kill {
            if let Some(scope) = openrt_safety::tool_scope(tool) {
                if let Some(reason) = kill.engaged() {
                    self.emit(
                        EventKind::SafetyHalt,
                        sid,
                        tid,
                        serde_json::json!({
                            "scope": scope.as_str(),
                            "reason": reason,
                            "tool": tool,
                            "reaction": "denied",
                        }),
                    )
                    .await;
                    // ADR-002/ADR-008: the reaction is the security-critical
                    // record. If it cannot be written, surface the audit
                    // failure (the block itself is already in force).
                    return Err(
                        match self
                            .record(
                                tool,
                                "kill_blocked",
                                serde_json::json!({ "reason": reason }),
                            )
                            .await
                        {
                            Ok(_) => ToolError::KillSwitchEngaged {
                                tool: tool.to_string(),
                                reason,
                            },
                            Err(e) => ToolError::Internal(
                                tool.to_string(),
                                format!(
                                    "kill switch engaged; halt reaction could not be audited: {e}"
                                ),
                            ),
                        },
                    );
                }
            }
        }

        // 4. Execute.
        self.emit(
            EventKind::ToolInvocationStarted,
            sid,
            tid,
            serde_json::json!({ "tool": tool }),
        )
        .await;
        let result = registry_tool.run(args).await;
        // Audit honesty: `ok` must mirror the TOOL's verdict, not the Rust
        // Result variant. Operational failures (NOT_FOUND, UNAVAILABLE, ...)
        // are delivered as `Ok(Json({ok:false, error:{...}}))` by design
        // (`openrt_fabric::fail`), so reading `result.is_ok()` alone would
        // record them as successes in the event log and the TUI would tick
        // them green. Missing `ok` in a structured payload = success (many
        // tools return data-only JSON).
        let (ok, error) = match &result {
            Ok(ToolOutput::Json(v)) => {
                let ok = v
                    .get("ok")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                let error = match v.get("error") {
                    None => None,
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(other) => Some(other.to_string()),
                };
                (ok, error)
            }
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        self.emit(
            EventKind::ToolInvocationCompleted,
            sid,
            tid,
            serde_json::json!({
                "tool": tool,
                "ok": ok,
                "error": error,
            }),
        )
        .await;
        result
    }
}

/// Build a [`Policy`] from the configuration's rule list.
pub fn policy_from_config(permissions: &openrt_config::PermissionsConfig) -> Policy {
    use openrt_permissions::PolicyRule;
    let mut policy = Policy::deny_by_default();
    for rule in &permissions.rules {
        let action = match rule.effect {
            openrt_config::RuleEffect::Allow => openrt_permissions::RuleAction::Allow,
            openrt_config::RuleEffect::Deny => openrt_permissions::RuleAction::Deny,
            openrt_config::RuleEffect::Ask => openrt_permissions::RuleAction::Ask,
        };
        policy.rules.push(PolicyRule {
            pattern: rule.capability.clone(),
            action,
            scope: rule.resource.clone(),
        });
    }
    policy
}

/// Build a [`ResourceGate`] from the configuration.
///
/// Memory admission is **headroom-based**: the configured
/// `min_memory_headroom_percent` (default 15%) of total memory must remain
/// available. The historical absolute `used < max_memory_bytes%` rule is kept
/// as the fallback for operators who disable headroom entirely.
pub fn resource_gate_from_config(resources: &openrt_config::ResourcesConfig) -> ResourceGate {
    let mut limits = ResourceLimits::default();
    if let Some(floor) = resources.max_disk_bytes {
        limits.min_free_disk_bytes = Some(floor);
    }
    limits.min_free_memory_bytes = resources.min_free_memory_bytes;
    if let Some(pct) = resources.min_memory_headroom_percent {
        limits.min_headroom_ratio = (pct.min(100) as f64 / 100.0).clamp(0.0, 1.0);
    }
    if let Some(pct) = resources.tight_memory_headroom_percent {
        limits.tight_headroom_ratio = (pct.min(100) as f64 / 100.0).clamp(0.0, 1.0);
    }
    // `max_memory_bytes` historically means "percent of memory that must stay
    // free". When set it becomes the explicit headroom floor.
    if let Some(free_pct) = resources.max_memory_bytes {
        if free_pct > 0 {
            let headroom = (free_pct.min(100) as f64 / 100.0).clamp(0.0, 1.0);
            if resources.min_memory_headroom_percent.is_none() {
                limits.min_headroom_ratio = headroom;
            }
            limits.max_mem_used_ratio = 1.0 - headroom;
        }
    }
    // Explicit `false` disables the override affordance entirely: the gate
    // refuses to grant one and preflight stops offering it. Absent = default.
    if let Some(allow) = resources.allow_operator_override {
        limits.allow_operator_override = allow;
    }
    ResourceGate::new(limits, openrt_resources::system::SystemInfo::collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::{MemoryStore, PersistenceError, PersistenceStore};
    use openrt_tools::Tool;
    use openrt_tools::ToolSpec;
    use std::time::Duration;

    fn test_gate() -> GateExecutor {
        let registry = ToolRegistry::builtins();
        let policy = Arc::new(policy_from_config(
            &openrt_config::PermissionsConfig::default(),
        ));
        let resources = Arc::new(resource_gate_from_config(
            &openrt_config::ResourcesConfig::default(),
        ));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();
        GateExecutor::new(registry, policy, resources, bus)
    }

    fn ctx() -> ToolExecutionContext {
        ToolExecutionContext {
            session_id: Some("s".into()),
            task_id: Some("t".into()),
            channel: None,
        }
    }

    #[tokio::test]
    async fn allows_echo() {
        let gate = test_gate();
        let out = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out.render(), "hi");
    }

    #[tokio::test]
    async fn denies_unknown_tool_without_execution() {
        let gate = test_gate();
        let err = gate
            .execute("rm-rf", serde_json::json!({}), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    /// A tool body that counts invocations, so we can assert it never ran.
    struct CountingTool {
        spec: ToolSpec,
        calls: Arc<std::sync::atomic::AtomicU64>,
    }

    #[async_trait::async_trait]
    impl Tool for CountingTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn run(&self, _args: serde_json::Value) -> Result<ToolOutput, ToolError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutput::text("ran"))
        }
    }

    #[tokio::test]
    async fn denied_tool_never_executes() {
        // Register a tool the default policy does NOT allow (it exists in the
        // registry, so only the policy can stop it).
        let mut registry = ToolRegistry::new();
        registry.register_custom(openrt_tools::builtin::echo::Echo::new());
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        registry
            .register(Arc::new(CountingTool {
                spec: ToolSpec::new("shell.exec", "a tool with no allow rule"),
                calls: counter.clone(),
            }))
            .unwrap();

        let policy = Arc::new(policy_from_config(
            &openrt_config::PermissionsConfig::default(),
        ));
        let resources = Arc::new(resource_gate_from_config(
            &openrt_config::ResourcesConfig::default(),
        ));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();
        let gate = GateExecutor::new(registry, policy, resources, bus);

        let err = gate
            .execute("shell.exec", serde_json::json!({ "cmd": "rm" }), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// P2.2 ASK boundary: an `Ask` policy decision without a resolver is a
    /// hard deny — fail closed by default.
    #[tokio::test]
    async fn ask_without_resolver_fails_closed() {
        let mut p = openrt_permissions::Policy::deny_by_default();
        p.rules.push(openrt_permissions::PolicyRule {
            pattern: "echo".into(),
            action: openrt_permissions::RuleAction::Ask,
            scope: None,
        });
        let gate = gate_with(ToolRegistry::builtins(), policy(p), None, capturing_bus());
        let err = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PermissionDenied { ref reason, .. } if reason.contains("no resolver configured")),
            "expected ask-fail-closed denial, got {err:?}"
        );
    }

    /// An approved `Ask` lets the tool run and emits `PermissionApproved`.
    #[tokio::test]
    async fn ask_allowed_by_resolver_executes() {
        let mut p = openrt_permissions::Policy::deny_by_default();
        p.rules.push(openrt_permissions::PolicyRule {
            pattern: "echo".into(),
            action: openrt_permissions::RuleAction::Ask,
            scope: None,
        });
        let resolver: Arc<dyn openrt_permissions::PermissionResolver> =
            Arc::new(openrt_permissions::MockResolver::allow_only(&["echo"]));
        let sink = Arc::new(openrt_telemetry::ChannelSink::new());
        let bus = capturing_bus_sink(sink.clone());
        let gate = gate_with(ToolRegistry::builtins(), policy(p), Some(resolver), bus);

        let out = gate
            .execute("echo", serde_json::json!({ "text": "approved" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out.render(), "approved");
        drain_bus().await;
        let kinds: Vec<_> = sink.snapshot().into_iter().map(|e| e.kind).collect();
        assert!(
            kinds.contains(&EventKind::PermissionApproved),
            "expected PermissionApproved, got {kinds:?}"
        );
    }

    /// A resolver `Deny` is a hard denial and the tool never executes.
    #[tokio::test]
    async fn ask_denied_by_resolver_never_executes() {
        let mut p = openrt_permissions::Policy::deny_by_default();
        p.rules.push(openrt_permissions::PolicyRule {
            pattern: "echo".into(),
            action: openrt_permissions::RuleAction::Ask,
            scope: None,
        });
        let resolver: Arc<dyn openrt_permissions::PermissionResolver> =
            Arc::new(openrt_permissions::MockResolver::allow_only(&[
                "filesystem.read",
            ]));
        let gate = gate_with(
            ToolRegistry::builtins(),
            policy(p),
            Some(resolver),
            capturing_bus(),
        );
        let err = gate
            .execute("echo", serde_json::json!({ "text": "nope" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PermissionDenied { ref reason, .. } if reason.contains("denied by approval resolver")),
            "expected resolver denial, got {err:?}"
        );
    }

    /// Tiered permission memory (production permission system): an explicit
    /// `AllowSession` records an exact `(session, tool, scope)` grant, an
    /// identical follow-up request is released WITHOUT consulting the
    /// resolver again, a different session is NOT covered, and a policy
    /// `Deny` can never be reached by a grant (grants live only on the Ask
    /// path — see `resolver_never_overrides_policy_deny`).
    #[tokio::test]
    async fn session_tier_grant_releases_identical_request_without_reasking() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct CountingSessionResolver(AtomicU64);
        #[async_trait::async_trait]
        impl openrt_permissions::PermissionResolver for CountingSessionResolver {
            async fn resolve(
                &self,
                _req: &openrt_permissions::ApprovalRequest,
            ) -> Result<openrt_permissions::PermissionDecision, openrt_permissions::PermissionError>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(openrt_permissions::PermissionDecision::AllowSession)
            }
        }

        let mut p = openrt_permissions::Policy::deny_by_default();
        p.rules.push(openrt_permissions::PolicyRule {
            pattern: "echo".into(),
            action: openrt_permissions::RuleAction::Ask,
            scope: None,
        });
        let resolver = Arc::new(CountingSessionResolver(AtomicU64::new(0)));
        let sink = Arc::new(openrt_telemetry::ChannelSink::new());
        let bus = capturing_bus_sink(sink.clone());
        let gate = gate_with(
            ToolRegistry::builtins(),
            policy(p),
            Some(resolver.clone()),
            bus,
        );

        let out = gate
            .execute("echo", serde_json::json!({ "text": "one" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out.render(), "one");
        assert_eq!(resolver.0.load(Ordering::SeqCst), 1, "asked exactly once");

        // Identical (session, tool, scope): released by the recorded grant —
        // the resolver must NOT be consulted a second time.
        let out = gate
            .execute("echo", serde_json::json!({ "text": "two" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out.render(), "two");
        assert_eq!(
            resolver.0.load(Ordering::SeqCst),
            1,
            "session grant released the identical request without re-asking"
        );

        // A different session is NOT covered by the first session's grant.
        let other = ToolExecutionContext {
            session_id: Some("other-session".into()),
            task_id: Some("t".into()),
            channel: None,
        };
        let out = gate
            .execute("echo", serde_json::json!({ "text": "three" }), &other)
            .await
            .unwrap();
        assert_eq!(out.render(), "three");
        assert_eq!(
            resolver.0.load(Ordering::SeqCst),
            2,
            "other session must be asked again"
        );
        drain_bus().await;
    }

    /// Workspace tier: the recorded grant releases the identical request from
    /// ANY session of this process (still exact tool+scope, still Ask-path
    /// only).
    #[tokio::test]
    async fn workspace_tier_grant_releases_across_sessions() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct CountingWorkspaceResolver(AtomicU64);
        #[async_trait::async_trait]
        impl openrt_permissions::PermissionResolver for CountingWorkspaceResolver {
            async fn resolve(
                &self,
                _req: &openrt_permissions::ApprovalRequest,
            ) -> Result<openrt_permissions::PermissionDecision, openrt_permissions::PermissionError>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(openrt_permissions::PermissionDecision::AllowWorkspace)
            }
        }

        let mut p = openrt_permissions::Policy::deny_by_default();
        p.rules.push(openrt_permissions::PolicyRule {
            pattern: "echo".into(),
            action: openrt_permissions::RuleAction::Ask,
            scope: None,
        });
        let resolver = Arc::new(CountingWorkspaceResolver(AtomicU64::new(0)));
        let sink = Arc::new(openrt_telemetry::ChannelSink::new());
        let bus = capturing_bus_sink(sink.clone());
        let gate = gate_with(
            ToolRegistry::builtins(),
            policy(p),
            Some(resolver.clone()),
            bus,
        );

        gate.execute("echo", serde_json::json!({ "text": "a" }), &ctx())
            .await
            .unwrap();
        assert_eq!(resolver.0.load(Ordering::SeqCst), 1);

        let other = ToolExecutionContext {
            session_id: Some("another".into()),
            task_id: None,
            channel: None,
        };
        gate.execute("echo", serde_json::json!({ "text": "b" }), &other)
            .await
            .unwrap();
        assert_eq!(
            resolver.0.load(Ordering::SeqCst),
            1,
            "workspace grant covers the identical request in any session"
        );
        drain_bus().await;
    }

    /// Audit honesty: operational failures are delivered as
    /// `Ok(Json({ok:false, error:{...}}))` by design (`openrt_fabric::fail`),
    /// so the completed event must record `ok:false` + the structured message.
    /// Reading only `result.is_ok()` would log these as successes (found while
    /// accepting `docx.export_pdf` on a missing source: event said ok:true).
    #[tokio::test]
    async fn structured_ok_false_is_logged_as_failure() {
        struct StructuredFail {
            spec: ToolSpec,
        }
        #[async_trait::async_trait]
        impl Tool for StructuredFail {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn run(&self, _args: serde_json::Value) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::Json(serde_json::json!({
                    "ok": false,
                    "error": { "code": "NOT_FOUND", "why": "missing source" },
                })))
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register_custom(openrt_tools::builtin::echo::Echo::new());
        registry
            .register(Arc::new(StructuredFail {
                spec: ToolSpec::new("office.export", "reports a structured failure"),
            }))
            .unwrap();

        let mut p = openrt_permissions::Policy::deny_by_default();
        p.rules.push(openrt_permissions::PolicyRule {
            pattern: "office.export".into(),
            action: openrt_permissions::RuleAction::Allow,
            scope: None,
        });
        let sink = Arc::new(openrt_telemetry::ChannelSink::new());
        let gate = gate_with(registry, policy(p), None, capturing_bus_sink(sink.clone()));

        let out = gate
            .execute("office.export", serde_json::json!({}), &ctx())
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json output, got {other:?}"),
        };
        assert_eq!(v["ok"], serde_json::json!(false));

        drain_bus().await;
        let ev = sink
            .snapshot()
            .into_iter()
            .find(|e| e.kind == EventKind::ToolInvocationCompleted)
            .expect("completion event");
        assert_eq!(
            ev.data.get("ok").and_then(|x| x.as_bool()),
            Some(false),
            "structured failure must be logged as ok:false, got {}",
            ev.data
        );
        let err = ev
            .data
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        assert!(err.contains("NOT_FOUND"), "error text: {err}");
    }

    /// Ask is one-way: a resolver approving an unrelated call still does not
    /// turn a policy `Deny` into an allow.
    #[tokio::test]
    async fn resolver_never_overrides_policy_deny() {
        let resolver: Arc<dyn openrt_permissions::PermissionResolver> =
            Arc::new(openrt_permissions::MockResolver::allow_all());
        // Default policy has NO rule for this custom tool → Deny, not Ask;
        // even an allow-all resolver must not change that.
        let mut registry = ToolRegistry::new();
        registry.register_custom(openrt_tools::builtin::echo::Echo::new());
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        registry
            .register(Arc::new(CountingTool {
                spec: ToolSpec::new("shell.exec", "a tool with no allow rule"),
                calls: counter.clone(),
            }))
            .unwrap();

        let gate = gate_with(
            registry,
            Arc::new(policy_from_config(
                &openrt_config::PermissionsConfig::default(),
            )),
            Some(resolver),
            capturing_bus(),
        );
        let err = gate
            .execute("shell.exec", serde_json::json!({ "cmd": "ls" }), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PermissionDenied { .. }));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // ---- helpers ----
    fn policy(p: openrt_permissions::Policy) -> Arc<openrt_permissions::Policy> {
        Arc::new(p)
    }

    fn capturing_bus() -> EventBus {
        capturing_bus_sink(Arc::new(openrt_telemetry::ChannelSink::new()))
    }

    fn capturing_bus_sink(sink: Arc<openrt_telemetry::ChannelSink>) -> EventBus {
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().sink(sink).start();
        bus
    }

    /// Give the async dispatcher a beat to drain the channel (same pattern as
    /// the `openrt-telemetry` bus test).
    async fn drain_bus() {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }

    fn gate_with(
        registry: ToolRegistry,
        policy: Arc<openrt_permissions::Policy>,
        resolver: Option<Arc<dyn openrt_permissions::PermissionResolver>>,
        bus: EventBus,
    ) -> GateExecutor {
        let resources = Arc::new(resource_gate_from_config(
            &openrt_config::ResourcesConfig::default(),
        ));
        GateExecutor::with_resolver(registry, policy, resources, bus, resolver)
    }

    /// Phase 8 (M8.9): a tool with a declared sandbox requirement that cannot
    /// be met is denied at gate-time admission — between permission policy
    /// and execution — and the tool body never runs. The permission policy
    /// here ALLOWS the tool, so only the sandbox stops it.
    #[tokio::test]
    async fn sandbox_requirement_denies_before_execution() {
        let mut registry = ToolRegistry::new();
        registry.register_custom(openrt_tools::builtin::echo::Echo::new());
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        registry
            .register(Arc::new(CountingTool {
                spec: ToolSpec::new("shell.exec", "sandbox-gated tool"),
                calls: counter.clone(),
            }))
            .unwrap();

        let mut p = openrt_permissions::Policy::deny_by_default();
        p.rules.push(openrt_permissions::PolicyRule {
            pattern: "shell.exec".into(),
            action: openrt_permissions::RuleAction::Allow,
            scope: None,
        });

        // Sandbox authority whose Strict profile demands a syscall filter —
        // unenforceable on this host, so admission must fail honestly
        // (SYSCALL_DENIED), never silently downgrade.
        let mut strict =
            openrt_sandbox::SandboxPolicy::for_profile(openrt_sandbox::SandboxProfile::Strict);
        strict.require_syscall_filter = true;
        let sbx = openrt_sandbox::SandboxManager::new().with_policy(strict);
        sbx.require("shell.exec", openrt_sandbox::SandboxProfile::Strict);

        let gate =
            gate_with(registry, policy(p), None, capturing_bus()).with_sandbox(Some(Arc::new(sbx)));

        let err = gate
            .execute("shell.exec", serde_json::json!({ "cmd": "rm" }), &ctx())
            .await
            .unwrap_err();
        match &err {
            ToolError::PermissionDenied { ref reason, .. } => {
                assert!(reason.contains("SYSCALL_DENIED"), "{reason}");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // ---- M10.7 ECC: deny-only asset-rule layer ----

    #[test]
    fn asset_rule_layer_matches_exact_scope_and_subject() {
        let layer = AssetRuleLayer::default();
        layer.set(vec![GateRuleEntry {
            scope: "gate".into(),
            subject: "echo".into(),
            id: "rule.echo".into(),
        }]);
        assert_eq!(layer.denies("gate", "echo").as_deref(), Some("rule.echo"));
        assert_eq!(layer.denies("gate", "system.info"), None);
        assert_eq!(layer.denies("other", "echo"), None);
    }

    #[test]
    fn asset_rule_layer_refresh_takes_only_enabled_denies() {
        use openrt_workspace_assets::{AssetStore, Provenance, Rule, RuleEffect, RuleId};
        let prov = || Provenance::new("run.m10", "src.gate");
        let mut store = AssetStore::new(&["op".to_string()], &["cap".to_string()], false);
        store
            .register_rule(
                Rule::new(
                    RuleId("rule.deny.on".into()),
                    1,
                    "gate",
                    "echo",
                    RuleEffect::Deny,
                    10,
                    true,
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        store
            .register_rule(
                Rule::new(
                    RuleId("rule.deny.off".into()),
                    1,
                    "gate",
                    "system.info",
                    RuleEffect::Deny,
                    10,
                    false,
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        store
            .register_rule(
                Rule::new(
                    RuleId("rule.allow.on".into()),
                    1,
                    "gate",
                    "filesystem.read",
                    RuleEffect::Allow,
                    10,
                    true,
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        let layer = AssetRuleLayer::default();
        layer.refresh_from(&store);
        assert_eq!(
            layer.denies("gate", "echo").as_deref(),
            Some("rule.deny.on"),
            "enabled deny must load"
        );
        assert_eq!(
            layer.denies("gate", "system.info"),
            None,
            "disabled rules never load"
        );
        assert_eq!(
            layer.denies("gate", "filesystem.read"),
            None,
            "allow rules never load (deny-only)"
        );
    }

    #[tokio::test]
    async fn deny_rule_overrides_policy_allow() {
        // The default policy ALLOWS echo — only the deny-only layer stops it.
        let layer = Arc::new(AssetRuleLayer::default());
        layer.set(vec![GateRuleEntry {
            scope: "gate".into(),
            subject: "echo".into(),
            id: "rule.gate.echo".into(),
        }]);
        let gate = test_gate().with_asset_rules(Some(layer));
        let err = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap_err();
        match err {
            ToolError::PermissionDenied { tool, reason } => {
                assert_eq!(tool, "echo");
                assert!(reason.contains("rule.gate.echo"), "{reason}");
                assert!(reason.contains("deny-only"), "{reason}");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
        // Same gate without the layer still allows echo (rule is the only delta).
        let out = test_gate()
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out.render(), "hi");
    }

    #[tokio::test]
    async fn deny_rule_scope_mismatch_does_not_deny() {
        let layer = Arc::new(AssetRuleLayer::default());
        layer.set(vec![GateRuleEntry {
            scope: "assets".into(),
            subject: "echo".into(),
            id: "rule.elsewhere".into(),
        }]);
        let gate = test_gate().with_asset_rules(Some(layer));
        let out = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out.render(), "hi");
    }

    /// Deny-only: an Allow rule at the gate scope must NOT widen the policy.
    #[tokio::test]
    async fn allow_rule_at_gate_scope_never_grants() {
        use openrt_workspace_assets::{AssetStore, Provenance, Rule, RuleEffect, RuleId};
        let mut registry = ToolRegistry::new();
        registry.register_custom(openrt_tools::builtin::echo::Echo::new());
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        registry
            .register(Arc::new(CountingTool {
                spec: ToolSpec::new("custom.wide", "policy-denied tool"),
                calls: counter.clone(),
            }))
            .unwrap();

        let mut p = openrt_permissions::Policy::deny_by_default();
        p.rules.push(openrt_permissions::PolicyRule {
            pattern: "echo".into(),
            action: openrt_permissions::RuleAction::Allow,
            scope: None,
        });

        // A store holding an ALLOW rule at the gate scope: `refresh_from`
        // filters it out (the layer only ever contains denies), so the gate
        // falls through to the policy — which denies.
        let mut store = AssetStore::new(&["op".to_string()], &["cap".to_string()], false);
        store
            .register_rule(
                Rule::new(
                    RuleId("rule.widen".into()),
                    1,
                    "gate",
                    "custom.wide",
                    RuleEffect::Allow,
                    10,
                    true,
                    Provenance::new("run.m10", "src.gate"),
                )
                .unwrap(),
            )
            .unwrap();
        let layer = Arc::new(AssetRuleLayer::default());
        layer.refresh_from(&store);
        assert_eq!(
            layer.denies("gate", "custom.wide"),
            None,
            "allow never loads"
        );

        let gate =
            gate_with(registry, policy(p), None, capturing_bus()).with_asset_rules(Some(layer));

        let err = gate
            .execute("custom.wide", serde_json::json!({}), &ctx())
            .await
            .unwrap_err();
        match err {
            ToolError::PermissionDenied { tool, reason } => {
                assert_eq!(tool, "custom.wide");
                assert!(
                    reason.contains("denied by policy"),
                    "policy still decides — rules never grant: {reason}"
                );
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // ── M10.9 AgentShield at the gate ────────────────────────────────────────

    /// Shield fixture: policy allows `echo` + `probe.exec`, the shield layer
    /// attached over a fresh MemoryStore; the probe tool counts executions so
    /// "blocked before run" is directly observable.
    fn shield_gate(
        resolver: Option<Arc<dyn openrt_permissions::PermissionResolver>>,
        kill: Option<Arc<openrt_safety::KillSwitch>>,
    ) -> (
        GateExecutor,
        Arc<std::sync::atomic::AtomicU64>,
        Arc<openrt_persistence::MemoryStore>,
    ) {
        let store = Arc::new(openrt_persistence::MemoryStore::new());
        let mut registry = ToolRegistry::new();
        registry.register_custom(openrt_tools::builtin::echo::Echo::new());
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        registry
            .register(Arc::new(CountingTool {
                spec: ToolSpec::new("probe.exec", "shield probe tool"),
                calls: counter.clone(),
            }))
            .unwrap();
        let mut p = openrt_permissions::Policy::deny_by_default();
        for allow in ["echo", "probe.exec"] {
            p.rules.push(openrt_permissions::PolicyRule {
                pattern: allow.into(),
                action: openrt_permissions::RuleAction::Allow,
                scope: None,
            });
        }
        let resources = Arc::new(resource_gate_from_config(
            &openrt_config::ResourcesConfig::default(),
        ));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();
        let gate = GateExecutor::with_kill(registry, policy(p), resources, bus, resolver, kill)
            .with_shield(Some(Arc::new(crate::shield::ShieldLayer::new(
                store.clone(),
            ))));
        (gate, counter, store)
    }

    async fn shield_statuses(
        store: &openrt_persistence::MemoryStore,
    ) -> Vec<openrt_shield::FindingStatus> {
        let mut out = openrt_shield::store::list_findings(store, None)
            .await
            .expect("shield findings are readable")
            .into_iter()
            .map(|f| f.status)
            .collect::<Vec<_>>();
        out.sort_by_key(|s| s.as_str());
        out
    }

    /// A critical destructive command maps Halt onto the ADR-008 kill switch:
    /// the switch latches, the tool never runs, and the finding lands as
    /// `halted`.
    #[tokio::test]
    async fn shield_halt_trips_kill_switch_and_blocks() {
        let kill = Arc::new(openrt_safety::KillSwitch::new());
        let (gate, counter, store) = shield_gate(None, Some(kill.clone()));
        let err = gate
            .execute(
                "probe.exec",
                serde_json::json!({ "command": "rm -rf /" }),
                &ctx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::KillSwitchEngaged { tool, reason } => {
                assert_eq!(tool, "probe.exec");
                assert!(reason.contains("AgentShield"), "{reason}");
            }
            other => panic!("expected KillSwitchEngaged, got {other:?}"),
        }
        assert!(kill.engaged().is_some(), "Halt must latch the kill switch");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the tool body never runs"
        );
        assert_eq!(
            shield_statuses(&store).await,
            vec![openrt_shield::FindingStatus::Halted]
        );
    }

    /// Sovereign mode (the fail-closed default when no mode record exists)
    /// denies non-loopback egress even though the policy ALLOWS the tool —
    /// the shield restricts, it never grants.
    #[tokio::test]
    async fn shield_sovereign_deny_overrides_policy_allow() {
        let (gate, counter, store) = shield_gate(None, None);
        let err = gate
            .execute(
                "probe.exec",
                serde_json::json!({ "command": "curl https://evil.example" }),
                &ctx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::PermissionDenied { tool, reason } => {
                assert_eq!(tool, "probe.exec");
                assert!(
                    reason.contains("AgentShield"),
                    "shield reasons itself: {reason}"
                );
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
        let statuses = shield_statuses(&store).await;
        assert!(
            !statuses.is_empty()
                && statuses
                    .iter()
                    .all(|s| *s == openrt_shield::FindingStatus::Denied),
            "every finding recorded the denial: {statuses:?}"
        );
    }

    /// A clean call through a shield-attached gate proceeds and records
    /// nothing (no finding spam for clean traffic).
    #[tokio::test]
    async fn shield_clean_call_proceeds_without_findings() {
        let (gate, _counter, store) = shield_gate(None, None);
        let out = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .expect("clean call proceeds");
        assert_eq!(out.render(), "hi");
        assert!(shield_statuses(&store).await.is_empty());
    }

    /// High-severity findings (credential access) route through the approval
    /// path — with no resolver attached that fails closed as a denial, the
    /// tool never runs, and the final refusal is recorded.
    #[tokio::test]
    async fn shield_high_severity_fails_closed_without_resolver() {
        let (gate, counter, store) = shield_gate(None, None);
        let err = gate
            .execute(
                "probe.exec",
                serde_json::json!({ "path": "/home/user/.ssh/id_rsa" }),
                &ctx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::PermissionDenied { tool, reason } => {
                assert_eq!(tool, "probe.exec");
                assert!(
                    reason.contains("denied by AgentShield approval"),
                    "{reason}"
                );
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
        let statuses = shield_statuses(&store).await;
        assert!(
            !statuses.is_empty()
                && statuses
                    .iter()
                    .all(|s| *s == openrt_shield::FindingStatus::Denied),
            "every finding's refusal overwrites the pre-approval stamp: {statuses:?}"
        );
    }

    /// The same finding approved by the operator runs the tool, the resolver
    /// is consulted exactly once, and the finding keeps its
    /// `approval_required` stamp.
    #[tokio::test]
    async fn shield_high_severity_approval_routes_through_resolver() {
        let resolver = Arc::new(openrt_permissions::MockResolver::allow_only(&[
            "probe.exec",
        ]));
        let (gate, counter, store) = shield_gate(Some(resolver.clone()), None);
        let out = gate
            .execute(
                "probe.exec",
                serde_json::json!({ "path": "/home/user/.ssh/id_rsa" }),
                &ctx(),
            )
            .await
            .expect("operator-approved probe runs");
        assert_eq!(out.render(), "ran");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(resolver.consult_count(), 1);
        let statuses = shield_statuses(&store).await;
        assert!(
            !statuses.is_empty()
                && statuses
                    .iter()
                    .all(|s| *s == openrt_shield::FindingStatus::ApprovalRequired),
            "approved findings keep the approval-required stamp: {statuses:?}"
        );
    }

    /// A mode record the layer cannot decode fails the scan — and therefore
    /// the call — closed, never silently unchecked.
    #[tokio::test]
    async fn shield_scan_failure_fails_closed() {
        let (gate, _counter, store) = shield_gate(None, None);
        store
            .put_record("modes.state", "current", &serde_json::json!("corrupt"))
            .await
            .expect("seed corrupt mode record");
        let err = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap_err();
        match err {
            ToolError::Internal(tool, reason) => {
                assert_eq!(tool, "echo");
                assert!(reason.contains("shield scan failed"), "{reason}");
            }
            other => panic!("expected Internal fail-closed error, got {other:?}"),
        }
    }

    // ---- resource admission config binding ----

    fn blocked_snapshot() -> openrt_resources::SystemInfo {
        let mut s = openrt_resources::SystemInfo::default();
        s.mem.total_kb = 15_551_000;
        s.mem.available_kb = 1_000_000; // ~6% available, under any sane floor
        s
    }

    /// The exact production regression: a host with ~49% memory available used
    /// to be refused because usage crossed an absolute 50% line.
    #[test]
    fn host_memory_used_at_fifty_percent_is_admitted_when_headroom_is_ample() {
        let mut s = openrt_resources::SystemInfo::default();
        s.mem.total_kb = 15_551_000;
        s.mem.available_kb = 7_700_000; // ~49.5% available / ~50.5% used
        let limits = resource_gate_from_config(&openrt_config::ResourcesConfig::default())
            .limits()
            .clone();
        let pf = openrt_resources::ResourcePreflight::evaluate(&s, &limits);
        assert!(pf.verdict().is_admitted(), "{pf:?}");
        assert!(pf.denial.is_none());
    }

    /// Genuine pressure is still refused, with the numbers the operator needs.
    #[test]
    fn genuine_memory_pressure_is_still_refused_with_numbers() {
        let limits = resource_gate_from_config(&openrt_config::ResourcesConfig::default())
            .limits()
            .clone();
        let pf = openrt_resources::ResourcePreflight::evaluate(&blocked_snapshot(), &limits);
        let d = pf.denial.expect("must deny");
        assert_eq!(d.kind, openrt_resources::ResourceKind::Memory);
        assert!(d.used_pct.unwrap() > 90.0);
        assert!(d.headroom.is_some());
    }

    #[test]
    fn operator_override_config_actually_binds_at_the_gate() {
        // Use a blocked snapshot so the assertion does not depend on this host.
        let gate_for = |allow: bool| {
            let mut cfg = openrt_config::ResourcesConfig::default();
            cfg.allow_operator_override = Some(allow);
            let limits = resource_gate_from_config(&cfg).limits().clone();
            openrt_resources::ResourceGate::new(limits, blocked_snapshot())
        };

        let gate = gate_for(false);
        assert!(
            !gate.grant_override("please", "operator"),
            "a deployment that forbids overrides must refuse the grant"
        );
        assert!(!gate.override_pending());
        assert!(!gate.admit().allowed(), "and the denial still stands");

        let gate = gate_for(true);
        assert!(gate.grant_override("confirmed headroom", "operator"));
        let a = gate.admit();
        assert!(a.allowed());
        assert!(a.overridden);
    }

    #[test]
    fn a_denial_never_offers_an_override_the_gate_would_refuse() {
        let cfg = openrt_config::ResourcesConfig {
            allow_operator_override: Some(false),
            ..Default::default()
        };
        let limits = resource_gate_from_config(&cfg).limits().clone();
        let d = openrt_resources::ResourcePreflight::evaluate(&blocked_snapshot(), &limits)
            .denial
            .expect("deny");
        assert!(
            !d.offers(openrt_resources::Remediation::OperatorOverride),
            "UI must not render an affordance the gate rejects"
        );
        assert!(d.offers(openrt_resources::Remediation::Retry));
        assert!(d.offers(openrt_resources::Remediation::Wait));
        assert!(d.offers(openrt_resources::Remediation::SwitchModel));
        assert!(d.offers(openrt_resources::Remediation::Cancel));
    }

    /// The wire form must carry structure, not a bare string, or the TUI cannot
    /// render RESOURCE LIMIT without re-parsing prose.
    #[tokio::test]
    async fn resource_check_event_carries_structured_preflight_fields() {
        /// Collects emitted events so the test can assert on the wire form.
        #[derive(Default)]
        struct Collector(std::sync::Arc<std::sync::Mutex<Vec<openrt_core::DomainEvent>>>);
        #[async_trait::async_trait]
        impl openrt_telemetry::EventSink for Collector {
            async fn write(&self, ev: &openrt_core::DomainEvent) {
                if let Ok(mut v) = self.0.lock() {
                    v.push(ev.clone());
                }
            }
        }

        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new()
            .sink(Arc::new(Collector(captured.clone())))
            .start();

        let mut limits = resource_gate_from_config(&openrt_config::ResourcesConfig::default())
            .limits()
            .clone();
        limits.min_headroom_ratio = 1.0; // force a denial on any host
        let resources = Arc::new(openrt_resources::ResourceGate::new(
            limits,
            blocked_snapshot(),
        ));
        let gate = GateExecutor::new(
            ToolRegistry::builtins(),
            Arc::new(policy_from_config(
                &openrt_config::PermissionsConfig::default(),
            )),
            resources,
            bus,
        );

        let err = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::ResourceDenied { .. }),
            "resource refusal is its own error, got {err:?}"
        );

        // Give the bus a moment to deliver, then inspect.
        for _ in 0..50 {
            if captured.lock().map(|v| !v.is_empty()).unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let events = captured.lock().map(|v| v.clone()).unwrap_or_default();
        let ev = events
            .iter()
            .find(|e| e.kind == EventKind::ResourceCheck)
            .expect("a ResourceCheck event must be emitted");
        let payload = ev.data.clone();
        assert_eq!(payload["allowed"], false, "{payload}");
        assert!(payload.get("preflight").is_some(), "{payload}");
        assert!(payload.get("denial").is_some(), "{payload}");
        let reason = payload["reason"].as_str().unwrap_or_default();
        assert!(reason.to_lowercase().contains("memory"), "{reason}");
        assert!(
            payload["denial"]["kind"].as_str().is_some(),
            "denial names the dimension: {payload}"
        );
    }

    /// The one place a declared resource limit is deliberately not enforced
    /// must land in the ADR-002 chained audit log. Telemetry alone is not an
    /// audit trail.
    #[tokio::test]
    async fn an_operator_override_is_written_to_the_chained_audit_log() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let audit = Arc::new(openrt_audit::AuditLog::new(store.clone()));

        let mut limits = resource_gate_from_config(&openrt_config::ResourcesConfig::default())
            .limits()
            .clone();
        limits.min_headroom_ratio = 1.0; // force a denial regardless of host
        let resources = Arc::new(openrt_resources::ResourceGate::new(
            limits,
            blocked_snapshot(),
        ));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();
        let gate = GateExecutor::new(
            ToolRegistry::builtins(),
            Arc::new(policy_from_config(
                &openrt_config::PermissionsConfig::default(),
            )),
            resources.clone(),
            bus,
        )
        .with_audit(Some(audit.clone()));

        // Denied first: no audit row, no execution.
        let err = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ResourceDenied { .. }), "{err:?}");
        assert_eq!(
            audit.count().await.unwrap(),
            0,
            "a plain denial is not an override"
        );

        // Operator authorises exactly one admission.
        assert!(resources.grant_override("freed the model", "tui"));
        let out = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .expect("override must admit this one call");
        assert_eq!(out.render(), "hi");

        // The override is in the chain...
        assert_eq!(audit.count().await.unwrap(), 1);
        let rows = audit
            .query(&openrt_audit::AuditQuery::new().with_action_prefix("resource.override"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "exactly one override row: {rows:?}");
        assert_eq!(rows[0].entry.subject, "echo");

        // ...and it is one-shot: the next call is refused again.
        let err = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ResourceDenied { .. }), "{err:?}");
        assert_eq!(audit.count().await.unwrap(), 1, "no second override row");
    }

    /// An override that cannot be written to the chain must not silently pass.
    #[tokio::test]
    async fn an_unauditable_override_fails_closed() {
        let store: Arc<dyn PersistenceStore> = Arc::new(FailingStore::new());
        let audit = Arc::new(openrt_audit::AuditLog::new(store));

        let mut limits = resource_gate_from_config(&openrt_config::ResourcesConfig::default())
            .limits()
            .clone();
        limits.min_headroom_ratio = 1.0;
        let resources = Arc::new(openrt_resources::ResourceGate::new(
            limits,
            blocked_snapshot(),
        ));
        assert!(resources.grant_override("freed the model", "tui"));

        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();
        let gate = GateExecutor::new(
            ToolRegistry::builtins(),
            Arc::new(policy_from_config(
                &openrt_config::PermissionsConfig::default(),
            )),
            resources,
            bus,
        )
        .with_audit(Some(audit));

        let err = gate
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
            .await
            .unwrap_err();
        match err {
            ToolError::Internal(tool, reason) => {
                assert_eq!(tool, "echo");
                assert!(
                    reason.contains("could not be audited"),
                    "the refusal must name the audit failure: {reason}"
                );
            }
            other => panic!("an unauditable override must fail closed, got {other:?}"),
        }
    }

    /// A `PersistenceStore` that behaves normally except `put_record` always
    /// fails — exactly the case where an override could not be persisted to the
    /// chained audit log.
    ///
    /// Implemented by delegation to `MemoryStore` rather than a hand-written
    /// stub so it cannot drift from the real trait surface.
    struct FailingStore(Arc<MemoryStore>);

    impl FailingStore {
        fn new() -> Self {
            Self(Arc::new(MemoryStore::new()))
        }
    }

    #[async_trait::async_trait]
    impl PersistenceStore for FailingStore {
        async fn create_session(&self, s: &openrt_core::Session) -> Result<(), PersistenceError> {
            self.0.create_session(s).await
        }
        async fn update_session(&self, s: &openrt_core::Session) -> Result<(), PersistenceError> {
            self.0.update_session(s).await
        }
        async fn load_session(
            &self,
            id: openrt_core::SessionId,
        ) -> Result<openrt_core::Session, PersistenceError> {
            self.0.load_session(id).await
        }
        async fn list_sessions(&self) -> Result<Vec<openrt_core::Session>, PersistenceError> {
            self.0.list_sessions().await
        }
        async fn save_task(&self, t: &openrt_core::Task) -> Result<(), PersistenceError> {
            self.0.save_task(t).await
        }
        async fn load_task(
            &self,
            id: openrt_core::TaskId,
        ) -> Result<openrt_core::Task, PersistenceError> {
            self.0.load_task(id).await
        }
        async fn list_tasks(
            &self,
            session_id: Option<openrt_core::SessionId>,
        ) -> Result<Vec<openrt_core::Task>, PersistenceError> {
            self.0.list_tasks(session_id).await
        }
        async fn save_execution(
            &self,
            r: &openrt_core::ExecutionRecord,
        ) -> Result<(), PersistenceError> {
            self.0.save_execution(r).await
        }
        async fn load_execution(
            &self,
            id: openrt_core::ExecutionId,
        ) -> Result<openrt_core::ExecutionRecord, PersistenceError> {
            self.0.load_execution(id).await
        }
        async fn append_event(&self, e: &openrt_core::DomainEvent) -> Result<(), PersistenceError> {
            self.0.append_event(e).await
        }
        async fn list_events(
            &self,
            opts: openrt_persistence::ListOptions,
        ) -> Result<Vec<openrt_core::DomainEvent>, PersistenceError> {
            self.0.list_events(opts).await
        }
        async fn put_record(
            &self,
            _namespace: &str,
            _key: &str,
            _value: &serde_json::Value,
        ) -> Result<(), PersistenceError> {
            Err(PersistenceError::Io {
                path: std::path::PathBuf::from("audit"),
                source: std::io::Error::other("audit store is unwritable"),
            })
        }
        async fn get_record(
            &self,
            namespace: &str,
            key: &str,
        ) -> Result<Option<serde_json::Value>, PersistenceError> {
            self.0.get_record(namespace, key).await
        }
        async fn list_records(
            &self,
            namespace: &str,
        ) -> Result<Vec<(String, serde_json::Value)>, PersistenceError> {
            self.0.list_records(namespace).await
        }
        async fn delete_record(&self, ns: &str, k: &str) -> Result<(), PersistenceError> {
            self.0.delete_record(ns, k).await
        }
        async fn clear(&self) -> Result<(), PersistenceError> {
            self.0.clear().await
        }
    }
}
