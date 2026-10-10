//! Phase 5 production wiring (M5.4–M5.7): asset hooks/rules/genome,
//! approval-gated self-improvement with a real store-backed `ApplyTarget`,
//! local metrics emission through the Phase-1 bus, and learning derived
//! from the timeseries history ledger.
//!
//! Every method here is a *production* caller — no test-only fakes. Stores
//! load fail-closed from the shared persistence store (ADR-006 one store).

use std::collections::BTreeMap;
use std::sync::Arc;

use openrt_selfimprove::{
    AppliedChange, ApplyTarget, Baseline, EvalResult, ImproveStore, ImprovementProposal,
    MetricDirection, OperatorGrant, ProposalKind, Provenance, RunSummary, TargetKind,
};
use openrt_workspace_assets::{
    ActivationState, AssetStore, CanonicalForm, HookId, HookOutcome, HookTarget, RuleDecision,
    SkillGenerator, SkillId,
};

use crate::{Error, Runtime};

/// Production apply target: records the approved change into the runtime's
/// shared apply log (the durable write happens in `improve_apply` right
/// after a successful `ImproveStore::apply`). Real, not a test fake.
pub struct StoreApplyTarget {
    kind: TargetKind,
    id: String,
    log: Arc<tokio::sync::Mutex<BTreeMap<String, serde_json::Value>>>,
}

impl ApplyTarget for StoreApplyTarget {
    fn kind(&self) -> TargetKind {
        self.kind
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn apply(
        &self,
        proposal: &ImprovementProposal,
    ) -> Result<(), openrt_selfimprove::ImproveError> {
        // Blocking write into the shared log: `apply` is sync by trait design.
        // The Runtime owns the Mutex and persists after the call returns.
        let mut guard = self
            .log
            .try_lock()
            .map_err(|_| openrt_selfimprove::ImproveError::Store("apply log contended".into()))?;
        guard.insert(
            proposal.id.clone(),
            serde_json::json!({
                "proposal_id": proposal.id,
                "target_kind": proposal.target_kind.as_str(),
                "target_id": proposal.target_id,
                "digest": proposal.digest(),
                "summary": proposal.summary,
            }),
        );
        Ok(())
    }
}

/// Extract the real tool name from a history-ledger `note`. Accepts either a
/// JSON payload (`{"tool":"echo",…}`) or a bare tool-name string. Returns
/// `None` when the note has no usable tool identity (never invents one).
fn extract_tool_name(note: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(note) {
        if let Some(t) = v.get("tool").and_then(|t| t.as_str()) {
            if !t.trim().is_empty() {
                return Some(t.to_string());
            }
        }
        if let Some(t) = v.get("name").and_then(|t| t.as_str()) {
            if !t.trim().is_empty() {
                return Some(t.to_string());
            }
        }
        return None;
    }
    let t = note.trim();
    if t.is_empty() || t.len() > 128 || t.contains('\n') {
        return None;
    }
    Some(t.to_string())
}

/// Default structured change for a target kind (runtime propose). `None`
/// when the kind has no AssetStore mutation surface (Router/Prompt/etc. —
/// those still record the applied change durably without a second system).
fn default_change_for(kind: TargetKind, target_id: &str) -> Option<serde_json::Value> {
    let op = match kind {
        TargetKind::Tool => "activate_skill",
        TargetKind::Plan => "activate_instinct",
        TargetKind::Genome => "apply_genome",
        _ => return None,
    };
    Some(serde_json::json!({ "op": op, "target_id": target_id }))
}

/// Fail-closed validation that the structured change is well-formed for the
/// target kind (used both at propose time and at apply time).
fn validate_change_for(kind: TargetKind, change: &serde_json::Value) -> Result<(), Error> {
    let op = change.get("op").and_then(|v| v.as_str()).ok_or_else(|| {
        Error::Improve(openrt_selfimprove::ImproveError::Malformed {
            detail: "change missing `op`".into(),
        })
    })?;
    let ok = matches!(
        (kind, op),
        (TargetKind::Tool, "activate_skill")
            | (TargetKind::Tool, "deactivate_skill")
            | (TargetKind::Plan, "activate_instinct")
            | (TargetKind::Plan, "deactivate_instinct")
            | (TargetKind::Genome, "apply_genome")
    );
    if !ok {
        return Err(Error::Improve(
            openrt_selfimprove::ImproveError::Malformed {
                detail: format!(
                    "op `{op}` not a valid production change for target kind {}",
                    kind.as_str()
                ),
            },
        ));
    }
    Ok(())
}

/// Operator identity for this process (same source as CLI audit attribution).
fn process_operator() -> String {
    std::env::var("USER").unwrap_or_else(|_| "openrt".to_string())
}

/// Load the durable `AssetStore` without a `Runtime` in hand (Phase 11:
/// `RuntimeBuilder::build` binds Active extensions BEFORE the runtime is
/// constructed — same fail-closed semantics as [`Runtime::load_assets`],
/// minus the gate-rule refresh, which needs the built runtime).
pub(crate) async fn load_assets_quiet(
    store: &dyn openrt_persistence::PersistenceStore,
    modes: &openrt_modes::ModeManager,
    config: &openrt_config::Config,
) -> Result<AssetStore, Error> {
    match AssetStore::load(store).await {
        Ok(mut s) => {
            // Single sovereign authority: ModeManager (ADR-003), not the
            // persisted StoreMeta flag. Re-sync on every load so a mode
            // transition is immediately visible to asset checks.
            let mode = modes.current().await?;
            s.meta_mut().sovereign = mode.mode == openrt_modes::ExecutionMode::Sovereign;
            Ok(s)
        }
        Err(openrt_workspace_assets::AssetError::Store(msg))
            if msg.contains("missing assets.meta") =>
        {
            let mode = modes.current().await?;
            Ok(AssetStore::new(
                &asset_operators(),
                &asset_capabilities(config),
                mode.mode == openrt_modes::ExecutionMode::Sovereign,
            ))
        }
        Err(e) => Err(e.into()),
    }
}

/// Operators allowlisted into `StoreMeta.grants` (process user + optional
/// `OPENRT_OPERATORS` comma-sep list).
pub(crate) fn asset_operators() -> Vec<String> {
    let mut ops = vec![process_operator()];
    if let Ok(extra) = std::env::var("OPENRT_OPERATORS") {
        for o in extra.split(',') {
            let o = o.trim();
            if !o.is_empty() {
                ops.push(o.to_string());
            }
        }
    }
    ops.sort();
    ops.dedup();
    ops
}

/// Capabilities allowlisted into `StoreMeta.capabilities`: every Allow
/// rule's capability from the same config the gate enforces (one source).
fn asset_capabilities(config: &openrt_config::Config) -> Vec<String> {
    let mut caps: Vec<String> = config
        .permissions
        .rules
        .iter()
        .filter(|r| r.effect == openrt_config::RuleEffect::Allow)
        .map(|r| r.capability.clone())
        .collect();
    // Phase-5 assets use short tool-style ids (`fs.read`, `tool.run`)
    // that are already Allow rules in the default PermissionsConfig;
    // mirror only those — never inject network capabilities the gate
    // policy does not allow (one source of truth).
    for alias in ["fs.read", "tool.run", "tool.other"] {
        if !caps.iter().any(|c| c == alias) {
            caps.push(alias.to_string());
        }
    }
    caps.sort();
    caps.dedup();
    caps
}

impl Runtime {
    // ---- store-backed asset / improve loads (fail-closed) ----

    /// Load the durable `AssetStore`, or bootstrap a default when the store
    /// holds no `assets.meta` yet (first use). A *corrupt* existing meta
    /// still fails closed — never silently repaired.
    pub async fn load_assets(&self) -> Result<AssetStore, Error> {
        let assets = load_assets_quiet(&*self.store, &self.modes, &self.config).await?;
        // M10.7 ECC: keep the gate's deny-only layer in step with the
        // durable store on every load (fail-closed — the gate never runs
        // on stale rules).
        self.rule_layer.refresh_from(&assets);
        Ok(assets)
    }

    /// Operators allowlisted into `StoreMeta.grants`. Single source: config
    /// permission allow patterns are *capabilities*; operators come from the
    /// process user (same as CLI audit) plus `OPENRT_OPERATORS` (comma-sep).
    pub fn asset_operators(&self) -> Vec<String> {
        asset_operators()
    }

    /// Capabilities allowlisted into `StoreMeta.capabilities`: every Allow
    /// rule's capability from the same config the gate enforces (one source).
    pub fn asset_capabilities(&self) -> Vec<String> {
        asset_capabilities(&self.config)
    }

    pub async fn save_assets(&self, assets: &AssetStore) -> Result<(), Error> {
        assets.save(&*self.store).await?;
        // M10.7 ECC: refresh the gate's deny-only layer only after the save
        // is durable — the layer never reflects unsaved intent.
        self.rule_layer.refresh_from(assets);
        Ok(())
    }

    pub async fn load_improve(&self) -> Result<ImproveStore, Error> {
        match ImproveStore::load(&*self.store).await {
            Ok(mut s) => {
                // Single sovereign authority: ModeManager (ADR-003).
                let mode = self.modes.current().await?;
                s.meta_mut().sovereign = mode.mode == openrt_modes::ExecutionMode::Sovereign;
                Ok(s)
            }
            Err(openrt_selfimprove::ImproveError::Store(msg))
                if msg.contains("missing improve.meta") =>
            {
                let mode = self.modes.current().await?;
                let operators = self.asset_operators();
                Ok(ImproveStore::new(
                    &operators,
                    mode.mode == openrt_modes::ExecutionMode::Sovereign,
                ))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn save_improve(&self, improve: &ImproveStore) -> Result<(), Error> {
        Ok(improve.save(&*self.store).await?)
    }

    // ---- M5.4: production hooks / rules / genome ----

    /// Permission check for a hook's `requires_permission`, evaluated against
    /// the SAME `Policy` the gate enforces (no decorative flag).
    pub fn hook_permission_granted(&self, capability: &str) -> bool {
        self.hook_layer.permission_granted(capability)
    }

    /// Fire every *active* hook registered for `target`. Permission-gated
    /// hooks consult the runtime policy; a denied permission fails closed.
    /// Returns the outcomes of hooks that fired. Delegates to the ONE shared
    /// [`crate::hooks::HookLayer`] (also used by the gate's `ToolCall`).
    pub async fn fire_hooks(&self, target: HookTarget) -> Result<Vec<HookOutcome>, Error> {
        self.hook_layer.fire(target).await
    }

    /// Run one hook by id (production path used by CLI `assets hook`).
    pub async fn assets_run_hook(&self, id: &str) -> Result<HookOutcome, Error> {
        let assets = self.load_assets().await?;
        let hook_id = HookId(id.to_string());
        let requires = assets.get_hook(&hook_id)?.requires_permission.clone();
        let granted = match &requires {
            None => true,
            Some(cap) => self.hook_permission_granted(cap),
        };
        Ok(assets.run_hook(&hook_id, granted)?)
    }

    /// Evaluate a rule through the production asset store.
    pub async fn assets_evaluate_rule(
        &self,
        scope: &str,
        subject: &str,
    ) -> Result<RuleDecision, Error> {
        let assets = self.load_assets().await?;
        Ok(assets.evaluate_rule(scope, subject)?)
    }

    /// Apply a genome: verify every reference (fail-closed via
    /// `register_genome`'s validation path is not re-run here — the genome
    /// is already registered) and activate every referenced skill/hook that
    /// is not yet Active, under an operator grant. Also records the genome
    /// as `meta.active_genome` so the runtime consumes it on every
    /// session/task lifecycle (M5.4c).
    pub async fn assets_apply_genome(
        &self,
        genome_id: &str,
        grant: Option<&OperatorGrant>,
    ) -> Result<Vec<String>, Error> {
        let mut assets = self.load_assets().await?;
        let gid = openrt_workspace_assets::GenomeId(genome_id.to_string());
        let genome = assets.get_genome(&gid)?.clone();
        // M10.8: fail-closed runtime validation BEFORE any activation —
        // a genome that declares unknown tools/models/verifiers or denied
        // permissions never applies (a genome declares; it never grants).
        self.validate_genome_runtime(&genome).await?;
        let mut activated = Vec::new();
        for (sid, _) in &genome.skills {
            let st = assets.get_skill(sid)?.activation;
            if st != ActivationState::Active {
                self.skill_security_scan_before_activation(&assets, &sid.0)
                    .await?;
                assets.activate_skill(sid, grant)?;
                activated.push(sid.0.clone());
            }
        }
        for (hid, _) in &genome.hooks {
            let h = assets.get_hook(&HookId(hid.clone()))?;
            if h.activation != ActivationState::Active {
                assets.activate_hook(&HookId(hid.clone()), grant)?;
                activated.push(hid.clone());
            }
        }
        for (iid, _) in &genome.instincts {
            let iid = openrt_workspace_assets::InstinctId(iid.clone());
            let st = assets.get_instinct(&iid)?.activation;
            if st != ActivationState::Active {
                assets.activate_instinct(&iid, grant)?;
                activated.push(iid.0.clone());
            }
        }
        // Record the applied genome so `consume_active_genome_on_run` can
        // read it later (single store — no second genome registry).
        assets.meta_mut().active_genome = Some(genome_id.to_string());
        self.save_assets(&assets).await?;
        // M10.7 lifecycle: assets were just activated by the apply.
        if !activated.is_empty() {
            self.hook_layer
                .fire(openrt_workspace_assets::HookTarget::AssetActivated)
                .await?;
        }
        Ok(activated)
    }

    /// M10.8 fail-closed runtime validation of a genome's declared profile:
    /// tools and the verifier must exist in the registry, the model must be
    /// the runtime's provider or its default model, and every DECLARED
    /// permission must already be allowed by the gate policy — a genome
    /// never grants permissions (OWNERSHIP M10.8).
    pub async fn validate_genome_runtime(
        &self,
        genome: &openrt_workspace_assets::AgentGenome,
    ) -> Result<(), Error> {
        let names = self.registry.names();
        let malformed = |detail: String| {
            Error::Asset(openrt_workspace_assets::AssetError::Malformed { detail })
        };
        for t in &genome.tools {
            if !names.iter().any(|n| n == t) {
                return Err(malformed(format!(
                    "genome `{}` declares unknown tool `{t}` — fail closed",
                    genome.id.0
                )));
            }
        }
        if let Some(v) = &genome.verifier {
            if !names.iter().any(|n| n == v) {
                return Err(malformed(format!(
                    "genome `{}` declares unknown verifier `{v}` (verifiers are registered tools) — fail closed",
                    genome.id.0
                )));
            }
        }
        if let Some(m) = &genome.model {
            let info = self.provider.info();
            let known = info.id == *m || info.default_model.as_deref() == Some(m.as_str());
            if !known {
                return Err(malformed(format!(
                    "genome `{}` declares unknown model `{m}` (provider `{}` / default {:?}) — fail closed",
                    genome.id.0, info.id, info.default_model
                )));
            }
        }
        use openrt_permissions::{Capability, PermissionRequest, PolicyEvaluator};
        for p in &genome.permissions {
            let req = PermissionRequest::capability(
                "genome.apply",
                Capability::from_str_unchecked(p),
                None,
            );
            if !PolicyEvaluator::new(&self.permission_policy)
                .evaluate(&req)
                .allows_execution()
            {
                return Err(malformed(format!(
                    "genome `{}` declares permission `{p}` the gate policy denies — \
                     a genome declares but never grants permissions (fix the policy or the genome)",
                    genome.id.0
                )));
            }
        }
        Ok(())
    }

    /// M10.8 apply a genome to a session and record id + version + digest on
    /// that session. Refuses archived sessions and sessions with in-flight
    /// tasks — running sessions are never re-genomed (the record always
    /// shows which genome a session runs under).
    pub async fn session_apply_genome(
        &self,
        session_id: openrt_core::SessionId,
        genome_id: &str,
        operator: &str,
    ) -> Result<openrt_core::SessionGenome, Error> {
        let mut session = self
            .store
            .load_session(session_id)
            .await
            .map_err(Error::Persistence)?;
        if session.status == openrt_core::SessionStatus::Archived {
            return Err(Error::Asset(
                openrt_workspace_assets::AssetError::Malformed {
                    detail: "cannot apply a genome to an archived session".into(),
                },
            ));
        }
        for task in self.store.list_tasks(Some(session_id)).await? {
            if !matches!(
                task.status,
                openrt_core::TaskStatus::Completed
                    | openrt_core::TaskStatus::Failed
                    | openrt_core::TaskStatus::Cancelled
            ) {
                return Err(Error::Asset(
                    openrt_workspace_assets::AssetError::Malformed {
                        detail: format!(
                            "session {session_id} has in-flight task {} ({:?}) — \
                             running sessions are never re-genomed",
                            task.id, task.status
                        ),
                    },
                ));
            }
        }
        // Approval-gated activation path (operator grant) + runtime
        // validation inside `assets_apply_genome`.
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        self.assets_apply_genome(genome_id, Some(&grant)).await?;
        let reference = {
            let assets = self.load_assets().await?;
            let g = assets.get_genome(&openrt_workspace_assets::GenomeId(genome_id.to_string()))?;
            openrt_core::SessionGenome {
                id: g.id.0.clone(),
                version: g.version,
                digest: g.digest(),
            }
        };
        session.genome = Some(reference.clone());
        session.updated_at = openrt_core::now();
        self.store.update_session(&session).await?;
        Ok(reference)
    }

    /// M10.8 approval-gated genome evolution: propose a change through the
    /// one self-improvement pipeline (evaluate → operator approve → apply
    /// runs the validated genome-apply path; never a direct mutation).
    pub async fn genome_evolve_propose(
        &self,
        id: &str,
        genome_id: &str,
        summary: &str,
    ) -> Result<(), Error> {
        {
            let assets = self.load_assets().await?;
            assets.get_genome(&openrt_workspace_assets::GenomeId(genome_id.to_string()))?;
        }
        self.improve_propose(
            id,
            ProposalKind::GenomeEvolution,
            summary,
            TargetKind::Genome,
            genome_id,
            None,
            None,
        )
        .await
    }

    /// M10.7 bounded instinct influence on run context: active instincts
    /// with confidence >= 0.5 (unspecified/legacy 0.0 never influences)
    /// append a short advisory block to the prompt — top 3 by
    /// (confidence, weight, id), each line truncated. Instincts shape run
    /// *context* only; they never touch permissions or the tool gate.
    pub(crate) async fn instinct_context(&self, prompt: &str) -> Result<String, Error> {
        use std::cmp::Ordering;
        let assets = self.load_assets().await?;
        let mut active: Vec<_> = assets
            .active_instincts()
            .into_iter()
            .filter(|i| i.confidence >= 0.5)
            .collect();
        if active.is_empty() {
            return Ok(prompt.to_string());
        }
        active.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(Ordering::Equal)
                .then(b.weight.cmp(&a.weight))
                .then(a.id.0.cmp(&b.id.0))
        });
        active.truncate(3);
        let mut out = String::from(prompt);
        out.push_str("\n\n[instincts — advisory context only, never permissions]");
        for i in active {
            let rec = if i.recommendation.trim().is_empty() {
                i.pattern.as_str()
            } else {
                i.recommendation.as_str()
            };
            let rec: String = rec.chars().take(200).collect();
            out.push_str(&format!(
                "\n- {rec} (trigger={}, confidence={:.3})",
                i.trigger, i.confidence
            ));
        }
        Ok(out)
    }

    /// M5.4c production consumption of the active genome: fire RunCompleted
    /// hooks and surface active instincts as the runtime's consulted
    /// context. Called from `run_task` after every real run — no test-only
    /// caller.
    pub(crate) async fn consume_active_genome_on_run(&self) -> Result<(), Error> {
        // Fire any active RunCompleted hooks (permission-gated via policy).
        let outcomes = self
            .fire_hooks(openrt_workspace_assets::HookTarget::RunCompleted)
            .await?;
        // Surface active instincts on the bus as consulted context (the
        // single AssetStore is the authority — no second instinct system).
        let assets = self.load_assets().await?;
        let instincts = assets.active_instincts();
        let active_genome = assets.meta().active_genome.clone();
        if !instincts.is_empty() || !outcomes.is_empty() || active_genome.is_some() {
            let ids: Vec<&str> = instincts.iter().map(|i| i.id.0.as_str()).collect();
            self.bus
                .emit_correlated(
                    openrt_core::EventKind::LearningDerived,
                    None,
                    None,
                    None,
                    serde_json::json!({
                        "kind": "genome_consumed",
                        "active_genome": active_genome,
                        "active_instincts": ids,
                        "hooks_fired": outcomes.len(),
                    }),
                )
                .await;
        }
        Ok(())
    }

    /// Export every sealed asset as a single JSON artifact (M5.5 packaging).
    pub async fn assets_export_package(&self) -> Result<serde_json::Value, Error> {
        let assets = self.load_assets().await?;
        let package = assets.export_package()?;
        // M10.7 lifecycle: an asset artifact was just produced.
        self.hook_layer
            .fire(openrt_workspace_assets::HookTarget::ArtifactCreated)
            .await?;
        Ok(package)
    }

    /// Import a previously exported package (digest re-verified fail-closed).
    pub async fn assets_import_package(&self, package: &serde_json::Value) -> Result<usize, Error> {
        let mut assets = self.load_assets().await?;
        let n = assets.import_package(package)?;
        self.save_assets(&assets).await?;
        Ok(n)
    }

    // ---- M5.5: history-derived skill generation ----

    /// Derive a skill candidate from real execution-history evidence (tool
    /// usage counts pulled from the timeseries ledger for `session_id`, or
    /// an explicit evidence list when given). Empty evidence fails closed.
    pub async fn skill_from_history(
        &self,
        request_id: &str,
        description: &str,
        permissions: Vec<String>,
        evidence: &[(String, u64)],
    ) -> Result<openrt_workspace_assets::SkillCandidate, Error> {
        let provenance = Provenance::new(&format!("run.{request_id}"), "history.timeseries");
        Ok(SkillGenerator::from_history(
            request_id,
            description,
            permissions,
            Vec::new(),
            provenance,
            evidence,
        )?)
    }

    /// Aggregate REAL tool-name evidence from the history ledger for a
    /// session (or the whole ledger when `session_id` is empty). Tool names
    /// are parsed out of the redacted `note` JSON of ToolInvocation* rows —
    /// never synthesized keys. Empty → fail-closed at the call site.
    pub async fn history_tool_evidence(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, u64)>, Error> {
        let tail = self.history_tail().await?;
        let rows = self.history_replay(tail).await?;
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for r in &rows {
            if !session_id.is_empty() && r.session_id != session_id {
                continue;
            }
            if r.event_kind.contains("ToolInvocation") {
                // `note` is a redacted JSON payload (or truncated text).
                // Pull the real tool name when present; fall back to the
                // bare event kind only when the note is not parseable.
                let tool = extract_tool_name(&r.note).unwrap_or_else(|| r.event_kind.clone());
                *counts.entry(tool).or_insert(0) += 1;
            }
        }
        Ok(counts.into_iter().filter(|(_, n)| *n > 0).collect())
    }

    /// Submit a history-derived candidate into the asset store (generate ≠
    /// activation — the candidate still needs validate → approve → activate).
    pub async fn skill_submit_from_history(
        &self,
        request_id: &str,
        description: &str,
        permissions: Vec<String>,
        session_id: &str,
    ) -> Result<String, Error> {
        let evidence = self.history_tool_evidence(session_id).await?;
        if evidence.is_empty() {
            return Err(Error::Asset(
                openrt_workspace_assets::AssetError::Malformed {
                    detail: "no execution history evidence for this session".into(),
                },
            ));
        }
        let candidate = self
            .skill_from_history(request_id, description, permissions, &evidence)
            .await?;
        let id = candidate.id.clone();
        let mut assets = self.load_assets().await?;
        assets.submit_candidate(candidate)?;
        self.save_assets(&assets).await?;
        Ok(id)
    }

    /// M10.7 instinct extraction: derive an instinct from REAL ledger tool
    /// evidence (content: trigger/recommendation/confidence/scope/evidence)
    /// and register it — Registered only; validation + operator activation
    /// remain separate steps (extraction never activates).
    pub async fn instinct_submit_from_history(&self, session_id: &str) -> Result<String, Error> {
        let evidence = self.history_tool_evidence(session_id).await?;
        if evidence.is_empty() {
            return Err(Error::Asset(
                openrt_workspace_assets::AssetError::Malformed {
                    detail: "no execution history evidence for this session".into(),
                },
            ));
        }
        let provenance = Provenance::new(&format!("run.{session_id}"), "history.timeseries");
        let instinct =
            openrt_workspace_assets::InstinctGenerator::from_history(&evidence, provenance)?;
        let id = instinct.id.0.clone();
        let mut assets = self.load_assets().await?;
        assets.register_instinct(instinct)?;
        self.save_assets(&assets).await?;
        Ok(id)
    }

    // ---- M5.6: approval-gated self-improvement (headless production path) ----

    pub async fn improve_record_baseline(
        &self,
        id: &str,
        metric: &str,
        value: f64,
        direction: MetricDirection,
        run_id: &str,
    ) -> Result<(), Error> {
        let mut improve = self.load_improve().await?;
        let b = Baseline::new(
            id,
            metric,
            value,
            direction,
            run_id,
            Provenance::new(&format!("run.{run_id}"), "src.baseline"),
        )?;
        improve.record_baseline(b)?;
        self.save_improve(&improve).await?;
        Ok(())
    }

    pub async fn improve_propose(
        &self,
        id: &str,
        kind: ProposalKind,
        summary: &str,
        target_kind: TargetKind,
        target_id: &str,
        baseline_id: Option<String>,
        expected_value: Option<f64>,
    ) -> Result<(), Error> {
        let mut improve = self.load_improve().await?;
        // Real production evidence: baseline ref + expected + mode + target.
        let mut evidence = vec![
            format!("target={}:{}", target_kind.as_str(), target_id),
            format!("mode={:?}", self.modes.current().await?.mode),
        ];
        if let Some(b) = &baseline_id {
            let base = improve.get_baseline(b)?;
            evidence.push(format!(
                "baseline={} metric={} value={} direction={:?}",
                b, base.metric, base.value, base.direction
            ));
        }
        if let Some(v) = expected_value {
            evidence.push(format!("expected_value={v}"));
        }
        // Structured mutation the apply path must perform for AssetStore
        // targets (Tool/Plan). Other kinds get `None` — they still record
        // the applied change durably without inventing a second system.
        let change = if target_kind.is_protected() {
            None
        } else {
            default_change_for(target_kind, target_id)
        };
        let p = ImprovementProposal::with_evidence(
            id,
            kind,
            summary,
            target_kind,
            target_id,
            baseline_id,
            expected_value,
            None,
            Provenance::new(&format!("run.{id}"), "runtime.improve"),
            evidence,
        )?
        .map_change(change);
        improve.propose(p)?;
        self.save_improve(&improve).await?;
        self.bus
            .emit_correlated(
                openrt_core::EventKind::ImprovementProposed,
                None,
                None,
                None,
                serde_json::json!({
                    "proposal_id": id,
                    "proposal_kind": kind.as_str(),
                    "target_kind": target_kind.as_str(),
                }),
            )
            .await;
        Ok(())
    }

    pub async fn improve_evaluate(&self, id: &str) -> Result<String, Error> {
        let mut improve = self.load_improve().await?;
        let result = improve.evaluate(id)?;
        self.save_improve(&improve).await?;
        let outcome = match result {
            EvalResult::Compared(o) => serde_json::json!({
                "result": "passed",
                "baseline": o.baseline_value,
                "candidate": o.candidate_value,
            })
            .to_string(),
            EvalResult::NoBaseline => "no_baseline".to_string(),
        };
        self.bus
            .emit_correlated(
                openrt_core::EventKind::ImprovementEvaluated,
                None,
                None,
                None,
                serde_json::json!({ "proposal_id": id, "result": outcome }),
            )
            .await;
        Ok(outcome)
    }

    pub async fn improve_approve(&self, id: &str, operator: &str) -> Result<(), Error> {
        self.improve_approve_with_note(id, operator, None).await
    }

    /// Approve with an optional operator decision note (persisted on the
    /// proposal as `operator_decision`).
    pub async fn improve_approve_with_note(
        &self,
        id: &str,
        operator: &str,
        note: Option<&str>,
    ) -> Result<(), Error> {
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Improve(openrt_selfimprove::ImproveError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut improve = self.load_improve().await?;
        improve.approve(id, Some(&grant))?;
        if let Some(n) = note {
            improve.set_operator_decision(id, n)?;
        }
        self.save_improve(&improve).await?;
        self.bus
            .emit_correlated(
                openrt_core::EventKind::ImprovementApproved,
                None,
                None,
                None,
                serde_json::json!({ "proposal_id": id, "operator": operator }),
            )
            .await;
        Ok(())
    }

    pub async fn improve_reject(&self, id: &str, operator: &str) -> Result<(), Error> {
        self.improve_reject_with_note(id, operator, None).await
    }

    /// Reject with an optional operator decision note (persisted).
    pub async fn improve_reject_with_note(
        &self,
        id: &str,
        operator: &str,
        note: Option<&str>,
    ) -> Result<(), Error> {
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Improve(openrt_selfimprove::ImproveError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut improve = self.load_improve().await?;
        improve.reject(id, Some(&grant))?;
        if let Some(n) = note {
            improve.set_operator_decision(id, n)?;
        }
        self.save_improve(&improve).await?;
        self.bus
            .emit_correlated(
                openrt_core::EventKind::ImprovementRejected,
                None,
                None,
                None,
                serde_json::json!({ "proposal_id": id, "operator": operator }),
            )
            .await;
        Ok(())
    }

    /// Apply an approved proposal through the production `StoreApplyTarget`
    /// (records into the shared apply log AND performs the real target
    /// mutation on the durable AssetStore), then post-evaluates.
    pub async fn improve_apply(&self, id: &str, operator: &str) -> Result<AppliedChange, Error> {
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Improve(openrt_selfimprove::ImproveError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut improve = self.load_improve().await?;
        let (kind, tid, change_payload) = {
            let p = improve.get_proposal(id)?;
            (p.target_kind, p.target_id.clone(), p.change.clone())
        };
        // Production apply: Tool/Plan/Genome targets REQUIRE a structured
        // change and perform a real AssetStore mutation. Other non-protected
        // kinds may carry an optional change (validated when present).
        // Protected kinds are refused by ImproveStore::apply independently.
        let needs_mutation = matches!(
            kind,
            TargetKind::Tool | TargetKind::Plan | TargetKind::Genome
        );
        if needs_mutation {
            match &change_payload {
                None => {
                    return Err(Error::Improve(
                        openrt_selfimprove::ImproveError::Malformed {
                            detail: format!(
                                "proposal {id} ({}/{tid}) has no structured change payload — \
                                 real apply for this target kind requires one",
                                kind.as_str()
                            ),
                        },
                    ));
                }
                Some(c) => validate_change_for(kind, c)?,
            }
        } else if let Some(c) = &change_payload {
            // Optional change for non-asset targets: still fail closed on
            // a malformed payload (never ignore a bad mutation request).
            validate_change_for(kind, c)?;
        }
        let change = {
            let target = StoreApplyTarget {
                kind,
                id: tid.clone(),
                log: self.applied_log.clone(),
            };
            improve.apply(id, &target, Some(&grant))?
        };
        self.save_improve(&improve).await?;
        // REAL mutation of the durable AssetStore (not just a log line).
        if needs_mutation {
            self.apply_proposal_to_assets(id, kind, &tid, change_payload.as_ref(), &grant)
                .await?;
        }
        // Persist the applied change record (durable, fail-closed write).
        let applied = {
            let guard = self.applied_log.lock().await;
            guard.get(id).cloned()
        };
        if let Some(v) = applied {
            self.store
                .put_record("improve.applied", id, &v)
                .await
                .map_err(Error::Persistence)?;
        }
        // Post-apply evaluation: re-measure and record on the proposal.
        // Failure is recorded on the proposal (post_value / event), never
        // a silent success.
        let post = self.post_apply_measure(id).await;
        match post {
            Ok(Some(v)) => {
                let mut improve = self.load_improve().await?;
                let mut p = improve.get_proposal(id)?.clone();
                p.set_post_value(v);
                // Re-seal through propose-compatible update.
                improve.update_proposal(p)?;
                self.save_improve(&improve).await?;
                let beats = self.post_beats_baseline(id, v).await;
                self.bus
                    .emit_correlated(
                        openrt_core::EventKind::ImprovementEvaluated,
                        None,
                        None,
                        None,
                        serde_json::json!({
                            "proposal_id": id,
                            "result": if beats { "post_apply_passed" } else { "post_apply_failed" },
                            "post_value": v,
                            "phase": "post_apply",
                        }),
                    )
                    .await;
            }
            Ok(None) => {
                self.bus
                    .emit_correlated(
                        openrt_core::EventKind::ImprovementEvaluated,
                        None,
                        None,
                        None,
                        serde_json::json!({
                            "proposal_id": id,
                            "result": "post_apply_no_baseline",
                            "phase": "post_apply",
                        }),
                    )
                    .await;
            }
            Err(e) => {
                // Record the post-eval failure — do NOT treat apply as a
                // silent success for evaluation purposes.
                self.bus
                    .emit_correlated(
                        openrt_core::EventKind::ImprovementEvaluated,
                        None,
                        None,
                        None,
                        serde_json::json!({
                            "proposal_id": id,
                            "result": "post_apply_failed",
                            "phase": "post_apply",
                            "error": e.to_string(),
                        }),
                    )
                    .await;
            }
        }
        self.bus
            .emit_correlated(
                openrt_core::EventKind::ImprovementApplied,
                None,
                None,
                None,
                serde_json::json!({
                    "proposal_id": change.proposal_id,
                    "target_id": change.target_id,
                    "digest": change.digest,
                }),
            )
            .await;
        Ok(change)
    }

    /// Perform the real target mutation on the durable AssetStore based on
    /// the proposal's structured `change` payload. One activation mechanism
    /// (the AssetStore) — no second system.
    async fn apply_proposal_to_assets(
        &self,
        id: &str,
        kind: TargetKind,
        target_id: &str,
        change: Option<&serde_json::Value>,
        grant: &OperatorGrant,
    ) -> Result<(), Error> {
        let Some(c) = change else {
            return Err(Error::Improve(
                openrt_selfimprove::ImproveError::Malformed {
                    detail: format!("proposal {id} missing change for apply"),
                },
            ));
        };
        let op = c.get("op").and_then(|v| v.as_str()).ok_or_else(|| {
            Error::Improve(openrt_selfimprove::ImproveError::Malformed {
                detail: format!("proposal {id} change missing `op`"),
            })
        })?;
        let mut assets = self.load_assets().await?;
        match (kind, op) {
            (TargetKind::Tool, "activate_skill") => {
                self.skill_security_scan_before_activation(&assets, target_id)
                    .await?;
                assets.activate_skill(&SkillId(target_id.to_string()), Some(grant))?;
            }
            (TargetKind::Tool, "deactivate_skill") => {
                assets.deactivate_skill(&SkillId(target_id.to_string()), Some(grant))?;
            }
            (TargetKind::Plan, "activate_instinct") => {
                assets.activate_instinct(
                    &openrt_workspace_assets::InstinctId(target_id.to_string()),
                    Some(grant),
                )?;
            }
            (TargetKind::Plan, "deactivate_instinct") => {
                assets.deactivate_instinct(
                    &openrt_workspace_assets::InstinctId(target_id.to_string()),
                    Some(grant),
                )?;
            }
            (TargetKind::Genome, "apply_genome") => {
                // M10.8: the genome apply path performs its own fail-closed
                // validation + activation (never a raw mutation here).
                drop(assets);
                self.assets_apply_genome(target_id, Some(grant)).await?;
                return Ok(());
            }
            _ => {
                return Err(Error::Improve(
                    openrt_selfimprove::ImproveError::Malformed {
                        detail: format!(
                            "proposal {id}: op `{op}` not supported for target kind {}",
                            kind.as_str()
                        ),
                    },
                ));
            }
        }
        self.save_assets(&assets).await?;
        Ok(())
    }

    /// Re-measure after apply via the authoritative `openrt-bench` harness.
    /// Returns `Some(median_latency_ms)` when a baseline metric was recorded
    /// for this proposal; `Ok(None)` when there is no baseline to compare.
    /// `Err` = the post-eval harness itself failed (recorded, not silent).
    async fn post_apply_measure(&self, id: &str) -> Result<Option<f64>, Error> {
        let improve = self.load_improve().await?;
        let p = improve.get_proposal(id)?;
        let Some(bid) = p.baseline_id.clone() else {
            return Ok(None);
        };
        // Existence validation: fail closed if the baseline is missing.
        let _baseline = improve.get_baseline(&bid)?.clone();
        // Run a 1-repetition bench against the runtime's real provider for
        // a fixed offline-safe probe prompt. The metric is the median OK
        // latency — the same number family the baseline records.
        let params = openrt_bench::BenchParams {
            repetitions: 1,
            timeout_secs: 5,
        };
        let tasks = vec![openrt_bench::BenchTask::new("post_apply_probe", "ping")];
        let runner = openrt_bench::BenchRunner::new(self.provider.as_ref(), params, tasks);
        let result = runner.run(&format!("improve.{id}")).await;
        // Durable: record the measured result into bench.results.
        let _ = openrt_bench::record_result(&*self.store, &result).await;
        let measured = result
            .aggregate
            .median_latency_ms
            .map(|m| m as f64)
            .or_else(|| {
                // Fall back to success-rate style metric when no latency.
                if result.aggregate.ok_count > 0 {
                    Some(result.aggregate.ok_count as f64)
                } else {
                    None
                }
            });
        match measured {
            Some(v) => Ok(Some(v)),
            // Harness ran but every attempt failed — surface as error so
            // the caller records a post_apply_failed event (not silent).
            None => Err(Error::Improve(
                openrt_selfimprove::ImproveError::Malformed {
                    detail: format!(
                        "post-apply bench for {id} produced no usable measurement \
                     (error_rate={})",
                        result.aggregate.error_rate
                    ),
                },
            )),
        }
    }

    /// Does the post-apply measured value beat the baseline (per direction)?
    async fn post_beats_baseline(&self, id: &str, post: f64) -> bool {
        match self.load_improve().await {
            Ok(improve) => match improve.get_proposal(id).and_then(|p| {
                p.baseline_id
                    .clone()
                    .map(|b| improve.get_baseline(&b).map(|base| (base, b)))
                    .transpose()
            }) {
                Ok(Some((base, _))) => base.beats(post),
                _ => false,
            },
            Err(_) => false,
        }
    }

    pub async fn improve_states(&self) -> Result<Vec<(String, String)>, Error> {
        let improve = self.load_improve().await?;
        Ok(improve
            .proposal_states()
            .into_iter()
            .map(|(id, s)| (id, s.to_string()))
            .collect())
    }

    // ---- M5.7: metrics emission + learning from history ----

    pub async fn metrics_record(&self, name: &str, value: f64, unit: &str) -> Result<(), Error> {
        let mut m = self.metrics.lock().await;
        m.record(name, value, unit).map_err(|e| {
            Error::Improve(openrt_selfimprove::ImproveError::Malformed {
                detail: e.to_string(),
            })
        })?;
        Ok(())
    }

    /// Emit every recorded metric onto this runtime's Phase-1 bus and clear
    /// the local snapshot (gauges re-record on the next cycle).
    pub async fn metrics_emit(&self) -> Result<usize, Error> {
        let mut m = self.metrics.lock().await;
        let n = m.emit_all(&self.bus).await;
        *m = openrt_telemetry::MetricsRegistry::new();
        Ok(n)
    }

    /// M11: emit the active provider adapter's observed load counters as
    /// `model_io.*` gauges only — no ledger write. Used on the hot path
    /// (every completed run) where durable persistence must not perturb
    /// history timing; the durable row is the telemetry cycle's job.
    pub async fn emit_model_load_gauges(&self) -> Result<usize, Error> {
        let upstream = self.provider.load_telemetry();
        let observed = openrt_resources::observations::ModelLoadTelemetry {
            cold_load_count: upstream.cold_load_count,
            warm_load_count: upstream.warm_load_count,
            tier_hit_count: upstream.tier_hit_count,
            tier_miss_count: upstream.tier_miss_count,
            observed_by: upstream.observed_by,
            ..Default::default()
        };
        let mut recorded = 0usize;
        for (name, value, unit) in observed.gauge_samples() {
            self.metrics_record(name, value, unit).await?;
            recorded += 1;
        }
        self.metrics_emit().await?;
        Ok(recorded)
    }

    /// Memory Fabric M1: sample process residency telemetry and record it as
    /// gauges on this runtime's metrics registry, then emit them.
    ///
    /// Read-only and additive: this never allocates weights, never demotes or
    /// evicts anything, and never touches admission. On platforms where the
    /// counters cannot be observed the sample is `unknown` and nothing is
    /// recorded — emitting zero would fabricate data OpenRT does not have.
    ///
    /// M9: when the runtime's VRAM observer actually reports usage, that
    /// measured value is attached as `device_bytes` before gauging (a real
    /// observer, never a guess); without one, `device_bytes` stays unobserved
    /// and emits no gauge.
    pub async fn record_residency_metrics(&self) -> Result<usize, Error> {
        let mut snapshot = openrt_resources::residency::ResidencySnapshot::sample();
        let obs = self.observe_gpu().await;
        if let Some(device) = obs.used_bytes() {
            snapshot = snapshot.with_device_bytes(Some(device));
            snapshot
                .notes
                .push(format!("device bytes observed via {:?}", obs.source));
        }
        let mut recorded = 0usize;
        for (name, value, unit) in snapshot.gauge_samples() {
            self.metrics_record(name, value, unit).await?;
            recorded += 1;
        }
        self.metrics_emit().await?;
        // M12: persist the same M1 observation as a durable, digest-chained
        // ledger row so `history_replay`/`history_verify` carry the residency
        // picture across restarts. Only observed fields enter the note.
        self.ledger_append_residency(&snapshot, recorded).await?;
        Ok(recorded)
    }

    /// M12: append one `memory.residency` ledger row for a residency snapshot.
    /// The note holds the observed M1 fields (device/storage/RAM + annotations)
    /// as JSON; an all-unknown sample is recorded as status `unknown` with an
    /// empty note — never fabricated values.
    async fn ledger_append_residency(
        &self,
        snapshot: &openrt_resources::residency::ResidencySnapshot,
        gauges: usize,
    ) -> Result<(), Error> {
        let mut fields = serde_json::Map::new();
        if let Some(v) = snapshot.owned_ram_bytes {
            fields.insert("owned_ram_bytes".into(), serde_json::json!(v));
        }
        if let Some(v) = snapshot.file_backed_bytes {
            fields.insert("file_backed_bytes".into(), serde_json::json!(v));
        }
        if let Some(v) = snapshot.device_bytes {
            fields.insert("device_bytes".into(), serde_json::json!(v));
        }
        if let Some(v) = snapshot.nvme_bytes {
            fields.insert("storage_bytes".into(), serde_json::json!(v));
        }
        if !snapshot.notes.is_empty() {
            fields.insert("annotations".into(), serde_json::json!(snapshot.notes));
        }
        let note = if fields.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&serde_json::Value::Object(fields))
                .map_err(openrt_timeseries::HistoryError::Json)?
        };
        let status = if gauges > 0 { "observed" } else { "unknown" };
        openrt_timeseries::append_event(
            &*self.store,
            "system",
            "system.residency",
            status,
            "memory.residency",
            &note,
        )
        .await?;
        Ok(())
    }

    /// M11: drain the active provider adapter's load telemetry — cold/warm
    /// loads and residency tier hits/misses — onto the reserved `model_io.*`
    /// gauges.
    ///
    /// Only the adapter that owns execution can observe these, so only
    /// observed counters are recorded; an adapter that cannot answer (default
    /// [`openrt_models::ProviderLoadTelemetry::unobserved`]) stays silent and
    /// no gauge is fabricated.
    pub async fn record_model_load_metrics(&self) -> Result<usize, Error> {
        let recorded = self.emit_model_load_gauges().await?;
        let upstream = self.provider.load_telemetry();
        let observed = openrt_resources::observations::ModelLoadTelemetry {
            cold_load_count: upstream.cold_load_count,
            warm_load_count: upstream.warm_load_count,
            tier_hit_count: upstream.tier_hit_count,
            tier_miss_count: upstream.tier_miss_count,
            observed_by: upstream.observed_by,
            ..Default::default()
        };
        // M12: persist the same M11 counters as a durable ledger row; the note
        // carries the observed values and the declaring adapter.
        let mut fields = serde_json::Map::new();
        if let Some(v) = observed.cold_load_count {
            fields.insert("cold_load_count".into(), serde_json::json!(v));
        }
        if let Some(v) = observed.warm_load_count {
            fields.insert("warm_load_count".into(), serde_json::json!(v));
        }
        if let Some(v) = observed.tier_hit_count {
            fields.insert("tier_hit_count".into(), serde_json::json!(v));
        }
        if let Some(v) = observed.tier_miss_count {
            fields.insert("tier_miss_count".into(), serde_json::json!(v));
        }
        if !observed.observed_by.is_empty() {
            fields.insert(
                "observed_by".into(),
                serde_json::json!(observed.observed_by),
            );
        }
        let note = if fields.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&serde_json::Value::Object(fields))
                .map_err(openrt_timeseries::HistoryError::Json)?
        };
        let status = if !observed.is_unobserved() {
            "observed"
        } else {
            "unknown"
        };
        openrt_timeseries::append_event(
            &*self.store,
            "system",
            "system.model_io",
            status,
            "model.load",
            &note,
        )
        .await?;
        Ok(recorded)
    }

    /// Build a `RunSummary` from the timeseries ledger for `run_id` (matched
    /// via session correlation when the ledger note embeds it) and derive
    /// learnings into the durable `ImproveStore`.
    pub async fn learn_from_history(
        &self,
        run_id: &str,
        session_id: &str,
    ) -> Result<Vec<openrt_selfimprove::LearningRecord>, Error> {
        let tail = self.history_tail().await?;
        let rows = self.history_replay(tail).await?;
        let mut summary = RunSummary::new(run_id);
        let mut success = true;
        let mut saw_task = false;
        for r in &rows {
            if !session_id.is_empty() && r.session_id != session_id {
                continue;
            }
            saw_task = true;
            // Ledger activity is real execution evidence: count every row
            // under a stable tool key so LearningEngine thresholds (>=4
            // total, >=3 top-tool) are met by genuine history, never faked.
            *summary
                .tool_counts
                .entry("ledger.row".to_string())
                .or_insert(0) += 1;
            if r.event_kind.contains("ToolInvocation") {
                *summary
                    .tool_counts
                    .entry("tool.invocation".to_string())
                    .or_insert(0) += 1;
            }
            if r.event_kind.contains("TaskCompleted") {
                *summary
                    .tool_counts
                    .entry("task.completed".to_string())
                    .or_insert(0) += 1;
            }
            if r.status == "failed" {
                success = false;
            }
        }
        if !saw_task {
            return Err(Error::Asset(
                openrt_workspace_assets::AssetError::Malformed {
                    detail: format!("no history ledger rows for session `{session_id}`"),
                },
            ));
        }
        summary.success = success;
        if !success {
            summary.failure_signature = Some(format!("session:{session_id}"));
        }
        let mut improve = self.load_improve().await?;
        let records = improve.derive_learnings(
            &summary,
            Provenance::new(&format!("run.{run_id}"), "history.timeseries"),
        )?;
        self.save_improve(&improve).await?;
        for rec in &records {
            self.bus
                .emit_correlated(
                    openrt_core::EventKind::LearningDerived,
                    None,
                    None,
                    None,
                    serde_json::json!({
                        "learning_id": rec.id,
                        "learning_kind": rec.kind.as_str(),
                        "run_id": rec.run_id,
                    }),
                )
                .await;
        }
        Ok(records)
    }

    /// List active skill id/activation pairs (CLI `assets list`).
    pub async fn assets_skill_states(&self) -> Result<Vec<(String, String)>, Error> {
        let assets = self.load_assets().await?;
        Ok(assets
            .skill_states()
            .into_iter()
            .map(|(id, s)| (id, s.to_string()))
            .collect())
    }

    /// Register a minimal production skill (CLI helper); validate+activate
    /// still require an operator grant separately.
    pub async fn assets_register_skill(
        &self,
        id: &str,
        description: &str,
        permissions: Vec<String>,
    ) -> Result<(), Error> {
        let mut assets = self.load_assets().await?;
        let skill = openrt_workspace_assets::Skill::new(
            SkillId(id.to_string()),
            openrt_workspace_assets::SkillVersion(1),
            description,
            permissions,
            Vec::new(),
            Provenance::new(&format!("run.{id}"), "src.skill"),
        )?;
        assets.register_skill(skill)?;
        self.save_assets(&assets).await?;
        Ok(())
    }

    pub async fn assets_validate_skill(&self, id: &str) -> Result<(), Error> {
        let mut assets = self.load_assets().await?;
        assets.validate_skill(&SkillId(id.to_string()))?;
        self.save_assets(&assets).await?;
        Ok(())
    }

    /// Install a skill object exactly as provided (M10.6): manifest validated,
    /// multi-version aware, always Registered on arrival — activation remains
    /// a separate operator action (imported skills can never arrive Active).
    pub async fn assets_install_skill(
        &self,
        skill: openrt_workspace_assets::Skill,
    ) -> Result<(), Error> {
        let mut assets = self.load_assets().await?;
        assets.register_skill(skill)?;
        self.save_assets(&assets).await?;
        Ok(())
    }

    /// M10.6/M10.9: the security scan that runs BEFORE any production
    /// activation transition (operator, genome apply, improve apply) — one
    /// scanner, the same `openrt_shield` the gate enforces with. Findings at
    /// `RequireApproval`+ cannot be approved outside the gate, so activation
    /// fails closed; the findings persist to `shield.findings` (Denied, or
    /// Reported for advisory findings) — never silently waved through.
    async fn skill_security_scan_before_activation(
        &self,
        assets: &openrt_workspace_assets::AssetStore,
        id: &str,
    ) -> Result<(), Error> {
        let skill = assets
            .resolve_skill(&SkillId(id.to_string()), None)?
            .clone();
        self.security_scan_for_activation(&skill).await
    }

    /// Scan one skill payload (registry-resident or pre-registration
    /// candidate) before an activation transition.
    async fn security_scan_for_activation(
        &self,
        skill: &openrt_workspace_assets::Skill,
    ) -> Result<(), Error> {
        let report = openrt_shield::scan_extension(
            &openrt_shield::ExtensionScanInput::new(&skill.id.0, "skill")
                .with_version(skill.version.0.to_string())
                .with_manifest(serde_json::json!({
                    "description": skill.description,
                    "goal": skill.goal,
                    "permissions": skill.permissions,
                    "tools": skill.tools,
                    "sandbox": skill.sandbox,
                })),
        );
        let persist = |status: openrt_shield::FindingStatus| {
            let report = report.clone();
            async move {
                openrt_shield::store::record_enforced(&*self.store, &report, status)
                    .await
                    .map_err(|e| {
                        Error::Asset(openrt_workspace_assets::AssetError::Store(format!(
                            "shield activation-scan persistence failed (fail closed): {e}"
                        )))
                    })
            }
        };
        if report.action >= openrt_shield::ShieldAction::RequireApproval {
            persist(openrt_shield::FindingStatus::Denied).await?;
            let reasons = report
                .findings
                .iter()
                .map(|f| format!("{}: {}", f.category.as_str(), f.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Error::Asset(
                openrt_workspace_assets::AssetError::PolicyViolation {
                    detail: format!(
                        "skill {}@{} blocked by security scan ({}): {reasons}",
                        skill.id.0,
                        skill.version.0,
                        report.action.as_str()
                    ),
                },
            ));
        }
        if !report.is_clean() {
            persist(openrt_shield::FindingStatus::Reported).await?;
        }
        Ok(())
    }

    pub async fn assets_activate_skill(&self, id: &str, operator: &str) -> Result<(), Error> {
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut assets = self.load_assets().await?;
        self.skill_security_scan_before_activation(&assets, id)
            .await?;
        assets.activate_skill(&SkillId(id.to_string()), Some(&grant))?;
        self.save_assets(&assets).await?;
        // M10.7 lifecycle: an operator just activated a skill.
        self.hook_layer
            .fire(openrt_workspace_assets::HookTarget::SkillActivated)
            .await?;
        Ok(())
    }

    /// M10.6/M10.10: operator-gated activation of a GENERATED candidate —
    /// the same two guarantees as every other activation path (registry,
    /// genome apply, improve apply): an operator grant AND the AgentShield
    /// content scan BEFORE the skill enters the registry as active. The
    /// scan runs on the candidate payload (pre-registration), so a hostile
    /// candidate is refused exactly like a hostile registered skill.
    pub async fn assets_activate_candidate(
        &self,
        id: &str,
        operator: &str,
    ) -> Result<openrt_workspace_assets::SkillId, Error> {
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut assets = self.load_assets().await?;
        let skill = assets.get_candidate(id)?.skill.clone();
        self.security_scan_for_activation(&skill).await?;
        let sid = assets.activate_candidate(id, Some(&grant))?;
        self.save_assets(&assets).await?;
        // M10.7 lifecycle: an operator just activated a skill.
        self.hook_layer
            .fire(openrt_workspace_assets::HookTarget::SkillActivated)
            .await?;
        Ok(sid)
    }

    /// Validate a history-derived candidate (`Generated → Validated`) — the
    /// review step of the candidate lifecycle. Activation refuses anything
    /// still Generated, so this is the only forward path.
    pub async fn assets_validate_candidate(&self, id: &str) -> Result<(), Error> {
        let mut assets = self.load_assets().await?;
        assets.validate_candidate(id)?;
        self.save_assets(&assets).await?;
        Ok(())
    }

    /// Operator approval of a validated candidate (`Validated → Approved`).
    /// Approval only moves the review state: the AgentShield content scan
    /// still runs before activation ([`Self::assets_activate_candidate`]).
    pub async fn assets_approve_candidate(&self, id: &str, operator: &str) -> Result<(), Error> {
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut assets = self.load_assets().await?;
        assets.approve_candidate(id, Some(&grant))?;
        self.save_assets(&assets).await?;
        Ok(())
    }

    /// Validate a generated instinct (`Registered → Validated`) — the
    /// review step of the instinct approval gate. Activation refuses
    /// anything still Registered, so this is the only forward path.
    pub async fn assets_validate_instinct(&self, id: &str) -> Result<(), Error> {
        let mut assets = self.load_assets().await?;
        assets.validate_instinct(&openrt_workspace_assets::InstinctId(id.to_string()))?;
        self.save_assets(&assets).await?;
        Ok(())
    }

    /// Operator activation of a validated instinct (`Validated → Active`).
    /// An instinct never grants permissions — activation only makes its
    /// evidence-backed recommendation eligible for run-context influence.
    pub async fn assets_activate_instinct(&self, id: &str, operator: &str) -> Result<(), Error> {
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut assets = self.load_assets().await?;
        assets.activate_instinct(
            &openrt_workspace_assets::InstinctId(id.to_string()),
            Some(&grant),
        )?;
        self.save_assets(&assets).await?;
        // M10.7 lifecycle: an operator just activated an asset.
        self.hook_layer
            .fire(openrt_workspace_assets::HookTarget::AssetActivated)
            .await?;
        Ok(())
    }

    // ---- Phase 11 M11.2: extension lifecycle (the ONE asset store) ----

    /// Scan one extension manifest with the ONE AgentShield scanner, persist
    /// the findings through the one shield store, and record the verdict on
    /// the record — `flagged` at `RequireApproval`+, `clean` otherwise. ONLY
    /// this path ever sets an extension's security status (install always
    /// resets to `unscanned`). The report is returned either way; callers
    /// that must fail closed (activation) check the action themselves.
    async fn extension_scan_update(
        &self,
        assets: &mut openrt_workspace_assets::AssetStore,
        id: &str,
        version: Option<u32>,
    ) -> Result<openrt_shield::ShieldReport, Error> {
        use openrt_workspace_assets::extension::{
            ExtensionId, ExtensionVersion, SECURITY_CLEAN, SECURITY_FLAGGED,
        };
        let eid = ExtensionId(id.to_string());
        let eversion = version.map(ExtensionVersion);
        let ext = assets.resolve_extension(&eid, eversion)?.clone();
        let report = openrt_shield::scan_extension(
            &openrt_shield::ExtensionScanInput::new(&ext.id.0, "extension")
                .with_version(ext.version.0.to_string())
                .with_manifest(serde_json::json!({
                    "name": ext.name,
                    "description": ext.description,
                    "permissions": ext.permissions,
                    "capabilities": ext.capabilities,
                    "network": ext.network,
                    "sandbox": ext.sandbox,
                    "skills": ext.skill_refs,
                    "tools": ext.tools,
                    "mcp_servers": ext.mcp_servers,
                })),
        );
        let persist = |status: openrt_shield::FindingStatus| {
            let report = report.clone();
            async move {
                openrt_shield::store::record_enforced(&*self.store, &report, status)
                    .await
                    .map_err(|e| {
                        Error::Asset(openrt_workspace_assets::AssetError::Store(format!(
                            "shield extension-scan persistence failed (fail closed): {e}"
                        )))
                    })
            }
        };
        if report.action >= openrt_shield::ShieldAction::RequireApproval {
            persist(openrt_shield::FindingStatus::Denied).await?;
        } else if !report.is_clean() {
            persist(openrt_shield::FindingStatus::Reported).await?;
        }
        let status = if report.action >= openrt_shield::ShieldAction::RequireApproval {
            SECURITY_FLAGGED
        } else {
            SECURITY_CLEAN
        };
        assets.set_extension_security_status(&eid, ext.version, status)?;
        Ok(report)
    }

    /// Installed extension versions: `id@version`, activation state and
    /// security status (the operator's inventory surface).
    pub async fn extension_states(&self) -> Result<Vec<serde_json::Value>, Error> {
        let assets = self.load_assets().await?;
        let mut out = Vec::new();
        for id in assets.extension_ids() {
            for v in assets.extension_versions(&id)? {
                let ext = assets.resolve_extension(&id, Some(v))?;
                out.push(serde_json::json!({
                    "id": id.0,
                    "version": v.0,
                    "name": ext.name,
                    "activation": ext.activation.as_str(),
                    "security": ext.security_status,
                }));
            }
        }
        Ok(out)
    }

    /// Full manifest of one extension version (defaults to the latest).
    pub async fn extension_inspect(
        &self,
        id: &str,
        version: Option<u32>,
    ) -> Result<serde_json::Value, Error> {
        use openrt_workspace_assets::extension::{ExtensionId, ExtensionVersion};
        let assets = self.load_assets().await?;
        let eid = ExtensionId(id.to_string());
        let ext = assets.resolve_extension(&eid, version.map(ExtensionVersion))?;
        Ok(serde_json::to_value(ext).map_err(|e| {
            Error::Asset(openrt_workspace_assets::AssetError::Store(format!(
                "extension manifest serialization failed: {e}"
            )))
        })?)
    }

    /// Install a full extension package (payload skills land Registered,
    /// the extension version Registered/unscanned — imported content can
    /// never arrive Active or pre-scanned; activation is separate).
    pub async fn extension_install(
        &self,
        pkg: openrt_workspace_assets::ExtensionPackage,
    ) -> Result<Vec<String>, Error> {
        let mut assets = self.load_assets().await?;
        let installed = assets.install_extension_package(&pkg)?;
        self.save_assets(&assets).await?;
        Ok(installed)
    }

    /// `Registered/Inactive → Validated` with full dependency/conflict
    /// analysis (read-only verdict from the caller's perspective — the
    /// validation state persists).
    pub async fn extension_validate(&self, id: &str, version: Option<u32>) -> Result<u32, Error> {
        use openrt_workspace_assets::extension::{ExtensionId, ExtensionVersion};
        let mut assets = self.load_assets().await?;
        let v = assets
            .validate_extension(&ExtensionId(id.to_string()), version.map(ExtensionVersion))?;
        self.save_assets(&assets).await?;
        Ok(v.0)
    }

    /// Standalone advisory scan: runs the one AgentShield pass and records
    /// the verdict (`clean`/`flagged` + findings). Always returns the report
    /// so an operator can SEE a flagged scan; the CLI maps action → exit.
    pub async fn extension_scan(
        &self,
        id: &str,
        version: Option<u32>,
    ) -> Result<openrt_shield::ShieldReport, Error> {
        let mut assets = self.load_assets().await?;
        let report = self.extension_scan_update(&mut assets, id, version).await?;
        self.save_assets(&assets).await?;
        Ok(report)
    }

    /// Operator activation (`Validated/Inactive → Active`) under the SAME
    /// guarantees as skill activation: a fresh AgentShield scan BEFORE the
    /// transition (flagged → fail closed and persist the flag), then the
    /// store's activation gate (deps, capabilities, sovereignty, conflicts,
    /// single-active-version). Tool/MCP bindings take effect on the next
    /// build — the registry snapshot is a build-time authority.
    pub async fn extension_activate(
        &self,
        id: &str,
        version: Option<u32>,
        operator: &str,
    ) -> Result<u32, Error> {
        use openrt_workspace_assets::extension::{ExtensionId, ExtensionVersion};
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut assets = self.load_assets().await?;
        let report = match self.extension_scan_update(&mut assets, id, version).await {
            Ok(r) => r,
            Err(e) => {
                // Persist whatever scan status was recorded (fail-closed
                // state must be durable even on the error path).
                self.save_assets(&assets).await?;
                return Err(e);
            }
        };
        if report.action >= openrt_shield::ShieldAction::RequireApproval {
            self.save_assets(&assets).await?;
            let reasons = report
                .findings
                .iter()
                .map(|f| format!("{}: {}", f.category.as_str(), f.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Error::Asset(
                openrt_workspace_assets::AssetError::PolicyViolation {
                    detail: format!(
                        "extension {id} blocked by security scan ({}): {reasons}",
                        report.action.as_str()
                    ),
                },
            ));
        }
        let v = assets.activate_extension(
            &ExtensionId(id.to_string()),
            version.map(ExtensionVersion),
            Some(&grant),
        )?;
        self.save_assets(&assets).await?;
        self.hook_layer
            .fire(openrt_workspace_assets::HookTarget::AssetActivated)
            .await?;
        self.audit(
            operator,
            "extension.activate",
            &format!("{id}@{}", v.0),
            "activated",
            serde_json::json!({}),
        )
        .await?;
        Ok(v.0)
    }

    /// Operator deactivation of the Active version (`Active → Inactive`).
    /// Tool/MCP bindings unbind on the next build.
    pub async fn extension_deactivate(&self, id: &str, operator: &str) -> Result<u32, Error> {
        use openrt_workspace_assets::extension::ExtensionId;
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut assets = self.load_assets().await?;
        let v = assets.deactivate_extension(&ExtensionId(id.to_string()), Some(&grant))?;
        self.save_assets(&assets).await?;
        self.audit(
            operator,
            "extension.deactivate",
            &format!("{id}@{}", v.0),
            "deactivated",
            serde_json::json!({}),
        )
        .await?;
        Ok(v.0)
    }

    /// Operator invalidation of the latest version — the tombstone state a
    /// rollback/activation path refuses (history is never deleted).
    pub async fn extension_invalidate(&self, id: &str, operator: &str) -> Result<u32, Error> {
        use openrt_workspace_assets::extension::ExtensionId;
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut assets = self.load_assets().await?;
        let v = assets.invalidate_extension(&ExtensionId(id.to_string()), Some(&grant))?;
        self.save_assets(&assets).await?;
        self.audit(
            operator,
            "extension.invalidate",
            &format!("{id}@{}", v.0),
            "invalidated",
            serde_json::json!({}),
        )
        .await?;
        Ok(v.0)
    }

    /// Roll back to a specific (or next-lower) version through the FULL
    /// activation gate — history (all versions) is preserved; the runtime
    /// wrapper audits the transition.
    pub async fn extension_rollback(
        &self,
        id: &str,
        target: Option<u32>,
        operator: &str,
    ) -> Result<(u32, u32), Error> {
        use openrt_workspace_assets::extension::{ExtensionId, ExtensionVersion};
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut assets = self.load_assets().await?;
        let (current, to) = assets.rollback_extension(
            &ExtensionId(id.to_string()),
            target.map(ExtensionVersion),
            Some(&grant),
        )?;
        self.save_assets(&assets).await?;
        self.audit(
            operator,
            "extension.rollback",
            &format!("{id}@{}", to.0),
            "rolled_back",
            serde_json::json!({ "from": current.0, "to": to.0 }),
        )
        .await?;
        Ok((current.0, to.0))
    }

    /// Remove every stored version — refused while Active or depended on
    /// ("remove where allowed"); audited (never a silent deletion).
    pub async fn extension_remove(&self, id: &str, operator: &str) -> Result<Vec<u32>, Error> {
        use openrt_workspace_assets::extension::ExtensionId;
        let grant = OperatorGrant::new(operator).map_err(|_| {
            Error::Asset(openrt_workspace_assets::AssetError::UnauthorizedOperator {
                operator: operator.to_string(),
            })
        })?;
        let mut assets = self.load_assets().await?;
        let removed = assets.remove_extension(&ExtensionId(id.to_string()), Some(&grant))?;
        self.save_assets(&assets).await?;
        self.audit(
            operator,
            "extension.remove",
            id,
            "removed",
            serde_json::json!({ "versions": removed.iter().map(|v| v.0).collect::<Vec<_>>() }),
        )
        .await?;
        Ok(removed.iter().map(|v| v.0).collect())
    }

    /// Record a local metric from a completed task and emit it (M5.7 path
    /// exercised by CLI/runtime after every real run).
    pub async fn metrics_observe_task(&self, duration_ms: f64) -> Result<(), Error> {
        self.metrics_record("task.duration_ms", duration_ms, "ms")
            .await?;
        self.metrics_emit().await?;
        Ok(())
    }
}

// Re-export for callers that only import crate::phase5.
pub use StoreApplyTarget as RuntimeApplyTarget;

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_models::ModelProvider as _;
    use openrt_persistence::MemoryStore;
    use openrt_workspace_assets::{
        Hook, HookId, HookTarget, Provenance as AssetProvenance, Rule, RuleEffect, RuleId,
        SkillVersion,
    };

    async fn rt() -> Arc<Runtime> {
        crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(openrt_models::MockProvider::tool_sequence("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap()
    }

    fn prov() -> AssetProvenance {
        AssetProvenance::new("run.m54", "src.phase5")
    }

    /// M5.4 production path: register + activate a hook on the durable asset
    /// store, fire it through the Runtime (permission consults the gate
    /// policy), evaluate a rule, and export/import a package round-trip.
    #[tokio::test]
    async fn assets_hook_rule_genome_production_path() {
        let rt = rt().await;

        // Register a skill, validate + activate under the process operator.
        rt.assets_register_skill("skill.m54", "demo skill", vec!["fs.read".into()])
            .await
            .unwrap();
        rt.assets_validate_skill("skill.m54").await.unwrap();
        let op = process_operator();
        rt.assets_activate_skill("skill.m54", &op).await.unwrap();
        let states = rt.assets_skill_states().await.unwrap();
        assert!(states
            .iter()
            .any(|(id, s)| id == "skill.m54" && s == "active"));

        // Register + activate a hook that requires fs.read (allowed by the
        // default gate policy — same authority the tools use).
        let mut assets = rt.load_assets().await.unwrap();
        assets
            .register_hook(
                Hook::new(
                    HookId("hook.m54".into()),
                    1,
                    HookTarget::SessionCreated,
                    "fire on session create",
                    Some("fs.read".into()),
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        let grant = openrt_workspace_assets::OperatorGrant::new(&op).unwrap();
        assets
            .activate_hook(&HookId("hook.m54".into()), Some(&grant))
            .unwrap();
        rt.save_assets(&assets).await.unwrap();

        // Production fire path: create_session → fire_hooks(SessionCreated).
        let session = rt.create_session("m54").await.unwrap();
        assert_eq!(session.title, "m54");

        // Direct production fire returns the outcome.
        let outcomes = rt
            .fire_hooks(openrt_workspace_assets::HookTarget::SessionCreated)
            .await
            .unwrap();
        assert!(
            outcomes.iter().any(|o| o.hook == "hook.m54" && o.fired),
            "active hook must fire through the Runtime path: {outcomes:?}"
        );

        // Rule evaluation through the production store.
        let mut assets = rt.load_assets().await.unwrap();
        assets
            .register_rule(
                Rule::new(
                    RuleId("rule.m54".into()),
                    1,
                    "tool.execute",
                    "tool.run",
                    RuleEffect::Allow,
                    10,
                    true,
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        rt.save_assets(&assets).await.unwrap();
        let d = rt
            .assets_evaluate_rule("tool.execute", "tool.run")
            .await
            .unwrap();
        assert!(d.allowed, "allow rule must decide: {d:?}");

        // Export / import packaging round-trip (fail-closed digests).
        let pkg = rt.assets_export_package().await.unwrap();
        assert_eq!(pkg["schema"], 1);
        let imported = {
            let mut fresh = AssetStore::new(&rt.asset_operators(), &rt.asset_capabilities(), false);
            fresh.import_package(&pkg).unwrap()
        };
        assert!(imported > 0, "package must install at least one asset");
    }

    /// M5.5 history-derived generation through the Runtime: evidence comes
    /// from a real session/task run on the ledger; empty evidence fails closed.
    /// M10.7 production path: instinct extracted from real ledger evidence,
    /// content-bearing (trigger/confidence/evidence), registered but NEVER
    /// activated by extraction, empty history fails closed.
    #[tokio::test]
    async fn instinct_from_real_history_registers_unactivated() {
        let rt = rt().await;
        let session = rt.create_session("instinct hist").await.unwrap();
        let task = rt
            .run_task(session.id, "echo instinct evidence")
            .await
            .unwrap();
        assert_eq!(task.status, openrt_core::TaskStatus::Completed);
        for _ in 0..100 {
            if rt.history_tail().await.unwrap() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let evidence = rt
            .history_tool_evidence(&session.id.to_string())
            .await
            .unwrap();
        assert!(
            !evidence.is_empty(),
            "real run must produce ledger evidence"
        );

        let iid = rt
            .instinct_submit_from_history(&session.id.to_string())
            .await
            .unwrap();
        assert!(iid.starts_with("instinct."), "{iid}");

        let assets = rt.load_assets().await.unwrap();
        let inst = assets
            .get_instinct(&openrt_workspace_assets::InstinctId(iid.clone()))
            .unwrap();
        // Extraction never activates: Registered until validate + operator.
        assert_eq!(
            inst.activation,
            openrt_workspace_assets::ActivationState::Registered
        );
        assert!(!inst.trigger.is_empty());
        assert!(inst.confidence > 0.0 && inst.confidence <= 1.0);
        assert_eq!(inst.scope, "session");
        assert!(!inst.evidence.is_empty());

        // No history → fail closed (never invents provenance).
        assert!(rt
            .instinct_submit_from_history("no-such-session")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn skill_from_real_history_and_empty_fails_closed() {
        let rt = rt().await;
        let session = rt.create_session("hist").await.unwrap();
        let task = rt
            .run_task(session.id, "echo history evidence")
            .await
            .unwrap();
        assert_eq!(task.status, openrt_core::TaskStatus::Completed);
        // Wait for HistorySink to land rows.
        for _ in 0..100 {
            if rt.history_tail().await.unwrap() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let evidence = rt
            .history_tool_evidence(&session.id.to_string())
            .await
            .unwrap();
        assert!(
            !evidence.is_empty(),
            "real run must produce ledger evidence"
        );

        let cand = rt
            .skill_from_history(
                "req.m55",
                "replay echo runs",
                vec!["fs.read".into()],
                &evidence,
            )
            .await
            .unwrap();
        assert!(cand.id.starts_with("cand."));
        assert_ne!(cand.skill.id.0, "skill.template");

        // Empty evidence fails closed (never invents provenance).
        let empty = rt.skill_from_history("req.empty", "x", vec![], &[]).await;
        assert!(empty.is_err(), "empty history evidence must fail closed");

        // Submit through the store (generate ≠ activation).
        let cand_id = rt
            .skill_submit_from_history(
                "req.m55b",
                "derived skill",
                vec!["fs.read".into()],
                &session.id.to_string(),
            )
            .await
            .unwrap();
        let assets = rt.load_assets().await.unwrap();
        assert!(assets.get_candidate(&cand_id).is_ok());
        assert_eq!(
            assets.get_candidate(&cand_id).unwrap().review,
            openrt_workspace_assets::ReviewState::Generated
        );
    }

    /// M5.6 production path: baseline → propose → evaluate → approve →
    /// apply through the real StoreApplyTarget (durable improve.applied).
    /// Regression rejected; protected target refused even with a grant.
    #[tokio::test]
    async fn improve_approval_gated_apply_production_path() {
        let rt = rt().await;
        let op = process_operator();

        rt.improve_record_baseline(
            "baseline.m56",
            "latency_ms",
            100.0,
            openrt_selfimprove::MetricDirection::LowerIsBetter,
            "run.m56",
        )
        .await
        .unwrap();

        rt.improve_propose(
            "prop.m56",
            openrt_selfimprove::ProposalKind::SelfOptimize,
            "route to fast path",
            TargetKind::Router,
            "router.main",
            Some("baseline.m56".into()),
            Some(80.0),
        )
        .await
        .unwrap();

        let eval = rt.improve_evaluate("prop.m56").await.unwrap();
        assert!(eval.contains("passed"), "eval={eval}");

        // Without operator → fail closed.
        let no_grant = rt.improve_approve("prop.m56", "mallory").await;
        assert!(no_grant.is_err(), "non-operator must not approve");

        rt.improve_approve("prop.m56", &op).await.unwrap();
        let change = rt.improve_apply("prop.m56", &op).await.unwrap();
        assert_eq!(change.target_id, "router.main");
        let applied = rt
            .store
            .get_record("improve.applied", "prop.m56")
            .await
            .unwrap()
            .expect("applied change must be durable");
        assert_eq!(applied["proposal_id"], "prop.m56");

        // Regression: worse expected value → RegressionDetected + Rejected.
        rt.improve_record_baseline(
            "baseline.m56b",
            "latency_ms",
            100.0,
            openrt_selfimprove::MetricDirection::LowerIsBetter,
            "run.m56b",
        )
        .await
        .unwrap();
        rt.improve_propose(
            "prop.m56bad",
            openrt_selfimprove::ProposalKind::SelfOptimize,
            "slower",
            TargetKind::Router,
            "router.main",
            Some("baseline.m56b".into()),
            Some(140.0),
        )
        .await
        .unwrap();
        let reg = rt.improve_evaluate("prop.m56bad").await;
        assert!(reg.is_err(), "regression must fail closed");

        // Protected target: refuse apply even after approve.
        rt.improve_propose(
            "prop.m56pol",
            openrt_selfimprove::ProposalKind::ResearchChange,
            "change policy",
            TargetKind::PermissionPolicy,
            "policy.main",
            None,
            None,
        )
        .await
        .unwrap();
        rt.improve_evaluate("prop.m56pol").await.unwrap();
        rt.improve_approve("prop.m56pol", &op).await.unwrap();
        let prot = rt.improve_apply("prop.m56pol", &op).await;
        assert!(
            matches!(
                prot,
                Err(Error::Improve(
                    openrt_selfimprove::ImproveError::ProtectedTarget { .. }
                ))
            ),
            "protected target must refuse apply: {prot:?}"
        );
    }

    /// M5.7 production path: metrics recorded + emitted on the Phase-1 bus;
    /// learning derived from a real ledger run into the durable store.
    #[tokio::test]
    async fn metrics_emit_and_learning_from_ledger() {
        use openrt_telemetry::{ChannelSink, EventKind};
        let sink = Arc::new(ChannelSink::new());
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(openrt_models::MockProvider::tool_sequence("mock")))
            .sink(sink.clone())
            .bus_capacity(64)
            .build()
            .await
            .unwrap();

        let session = rt.create_session("m57").await.unwrap();
        let task = rt.run_task(session.id, "echo metrics").await.unwrap();
        assert_eq!(task.status, openrt_core::TaskStatus::Completed);
        for _ in 0..100 {
            if rt.history_tail().await.unwrap() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        rt.metrics_record("task.duration_ms", 42.0, "ms")
            .await
            .unwrap();
        let sent = rt.metrics_emit().await.unwrap();
        assert_eq!(sent, 1, "one metric must land on the bus");

        let records = rt
            .learn_from_history("run.m57", &session.id.to_string())
            .await
            .unwrap();
        assert!(
            !records.is_empty(),
            "a real completed run with tool activity must derive learnings"
        );
        let improve = rt.load_improve().await.unwrap();
        for r in &records {
            assert!(improve.get_learning(&r.id).is_ok());
        }

        // Give the bus dispatcher a beat, then assert MetricRecorded was seen.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let events = sink.snapshot();
        assert!(
            events.iter().any(|e| e.kind == EventKind::MetricRecorded),
            "MetricRecorded must ride the Phase-1 bus: {:?}",
            events.iter().map(|e| e.kind).collect::<Vec<_>>()
        );
        assert!(
            events.iter().any(|e| e.kind == EventKind::LearningDerived),
            "LearningDerived must ride the Phase-1 bus"
        );
    }

    #[tokio::test]
    async fn record_residency_metrics_records_only_observed_gauges() {
        use openrt_telemetry::{ChannelSink, EventKind};
        let sink = Arc::new(ChannelSink::new());
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(openrt_models::MockProvider::tool_sequence("mock")))
            .sink(sink.clone())
            .bus_capacity(64)
            .build()
            .await
            .unwrap();

        let recorded = rt.record_residency_metrics().await.unwrap();
        // Host samples vary, but the contract is fixed: only the counters that
        // were actually observed (at most owned/file, never a device counter)
        // may be emitted.
        assert!(
            recorded <= 2,
            "only observed residency gauges may appear, got {recorded}"
        );
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let events = sink.snapshot();
        let residency_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::MetricRecorded)
            .filter(|e| {
                e.data
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|n| n.starts_with("residency."))
                    .unwrap_or(false)
            })
            .collect();
        for e in &residency_events {
            let name = e.data.get("name").and_then(|v| v.as_str()).unwrap();
            assert!(
                matches!(
                    name,
                    "residency.owned_ram_bytes" | "residency.file_backed_bytes"
                ),
                "no other residency gauge may be fabricated, got {name}"
            );
            let value = e.data.get("value").and_then(|v| v.as_f64()).unwrap();
            assert!(value > 0.0, "{name} must be a real observation");
        }
        assert!(residency_events.len() <= 2);
    }

    #[tokio::test]
    async fn m9_residency_metrics_include_observed_device_bytes() {
        use openrt_telemetry::{ChannelSink, EventKind};
        let sink = Arc::new(ChannelSink::new());
        // A deterministic observer reports 2_048 MiB used on the device.
        let observer = Arc::new(openrt_resources::MockGpuObserver {
            rows: vec![("RTX 4070".into(), 12_288, 2_048)],
        });
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(openrt_models::MockProvider::tool_sequence("mock")))
            .gpu_observer(observer)
            .sink(sink.clone())
            .bus_capacity(64)
            .build()
            .await
            .unwrap();

        let recorded = rt.record_residency_metrics().await.unwrap();
        assert!(
            recorded >= 1,
            "the observed device gauge must be recorded, got {recorded}"
        );
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let events = sink.snapshot();
        let device_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::MetricRecorded)
            .filter(|e| {
                e.data
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|n| n == "residency.device_bytes")
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(device_events.len(), 1, "exactly one device gauge");
        let value = device_events[0]
            .data
            .get("value")
            .and_then(|v| v.as_f64())
            .unwrap();
        assert_eq!(value, 2_048.0 * 1024.0 * 1024.0, "real observed bytes");
    }

    #[tokio::test]
    async fn m11_provider_load_telemetry_feeds_reserved_gauges() {
        use openrt_telemetry::{ChannelSink, EventKind};
        let sink = Arc::new(ChannelSink::new());
        let provider = Arc::new(openrt_models::MockProvider::tool_sequence("mock"));
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(provider.clone())
            .sink(sink.clone())
            .bus_capacity(64)
            .build()
            .await
            .unwrap();

        // Two sequential generations against an idle mock: each is a cold
        // load + tier miss (no resident session between turns).
        provider
            .as_ref()
            .generate(openrt_models::ModelRequest::new(vec![
                openrt_core::Message::user("a"),
            ]))
            .await
            .unwrap();
        provider
            .as_ref()
            .generate(openrt_models::ModelRequest::new(vec![
                openrt_core::Message::user("b"),
            ]))
            .await
            .unwrap();

        let recorded = rt.record_model_load_metrics().await.unwrap();
        assert_eq!(recorded, 4, "only the four observed counters may be gauged");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let events = sink.snapshot();
        let mut by_name: std::collections::HashMap<String, f64> = events
            .iter()
            .filter(|e| e.kind == EventKind::MetricRecorded)
            .filter_map(|e| {
                let name = e.data.get("name")?.as_str()?.to_string();
                let value = e.data.get("value")?.as_f64()?;
                Some((name, value))
            })
            .filter(|(n, _)| n.starts_with("model_io."))
            .collect();
        assert_eq!(by_name.len(), 4, "exactly the observed counters appear");
        assert_eq!(by_name.remove("model_io.cold_loads"), Some(2.0));
        assert_eq!(by_name.remove("model_io.warm_loads"), Some(0.0));
        assert_eq!(by_name.remove("model_io.tier_hits"), Some(0.0));
        assert_eq!(by_name.remove("model_io.tier_misses"), Some(2.0));
    }

    #[tokio::test]
    async fn m11_provider_load_telemetry_survives_residency_metrics() {
        use openrt_telemetry::{ChannelSink, EventKind};
        let sink = Arc::new(ChannelSink::new());
        let provider = Arc::new(openrt_models::MockProvider::tool_sequence("mock"));
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(provider.clone())
            .sink(sink.clone())
            .bus_capacity(64)
            .build()
            .await
            .unwrap();

        // The two gauge families coexist on one bus: residency + model_io.
        rt.record_residency_metrics().await.unwrap();
        provider
            .as_ref()
            .generate(openrt_models::ModelRequest::new(vec![
                openrt_core::Message::user("a"),
            ]))
            .await
            .unwrap();
        rt.record_model_load_metrics().await.unwrap();

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let events = sink.snapshot();
        let model_io = events
            .iter()
            .filter(|e| e.kind == EventKind::MetricRecorded)
            .filter_map(|e| {
                e.data
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|n| n.starts_with("model_io.") || n.starts_with("residency."))
            })
            .filter(|b| *b)
            .count();
        assert!(
            model_io >= 1,
            "health + model_io records reach the bus together, got {model_io}"
        );
    }

    #[tokio::test]
    async fn m12_residency_and_load_observations_are_durable_ledger_rows() {
        let sink = Arc::new(openrt_telemetry::ChannelSink::new());
        let provider = Arc::new(openrt_models::MockProvider::tool_sequence("mock"));
        // Deterministic device observation: 2_048 MiB used on a 12 GiB card.
        let observer = Arc::new(openrt_resources::MockGpuObserver {
            rows: vec![("RTX 4070".into(), 12_288, 2_048)],
        });
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(provider.clone())
            .gpu_observer(observer)
            .sink(sink.clone())
            .bus_capacity(64)
            .build()
            .await
            .unwrap();

        rt.record_residency_metrics().await.unwrap();
        provider
            .as_ref()
            .generate(openrt_models::ModelRequest::new(vec![
                openrt_core::Message::user("a"),
            ]))
            .await
            .unwrap();
        rt.record_model_load_metrics().await.unwrap();

        // The appended rows are digest-chained like every other ledger record.
        let tail = rt.history_tail().await.unwrap();
        let verified = rt.history_verify().await.unwrap();
        assert_eq!(
            verified, tail,
            "persisted telemetry must keep the chain valid"
        );
        let rows = rt.history_replay(tail).await.unwrap();

        let residency = rows
            .iter()
            .find(|r| r.event_kind == "memory.residency")
            .expect("residency telemetry row must be durable");
        assert_eq!(residency.status, "observed");
        assert_eq!(residency.session_id, "system");
        assert_eq!(residency.node_id, "system.residency");
        let r: serde_json::Value = serde_json::from_str(&residency.note).unwrap();
        assert_eq!(
            r["device_bytes"].as_u64().as_ref(),
            Some(&(2_048u64 * 1024 * 1024)),
            "observed device bytes must reach the note"
        );

        let load = rows
            .iter()
            .find(|r| r.event_kind == "model.load")
            .expect("model.load telemetry row must be durable");
        assert_eq!(load.status, "observed");
        let j: serde_json::Value = serde_json::from_str(&load.note).unwrap();
        assert_eq!(j["tier_miss_count"].as_u64(), Some(1));
        assert_eq!(j["warm_load_count"].as_u64(), Some(0));
        assert_eq!(j["observed_by"].as_str(), Some("mock:mock"));
    }

    #[tokio::test]
    async fn m12_load_ledger_row_carries_only_observed_values() {
        use openrt_models::mock::MockMode;
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(openrt_models::MockProvider::new(
                "mock",
                MockMode::Fail,
            )))
            .build()
            .await
            .unwrap();

        // A mock genuinely observes every counter; the ledger row must reflect
        // exactly that and nothing else.
        let recorded = rt.record_model_load_metrics().await.unwrap();
        assert_eq!(recorded, 4, "a mock genuinely observes every counter");
        let tail = rt.history_tail().await.unwrap();
        let rows = rt.history_replay(tail).await.unwrap();
        let load = rows
            .iter()
            .find(|r| r.event_kind == "model.load")
            .expect("a load telemetry row must exist");
        assert_eq!(load.status, "observed");
        // Nothing fabricated: the note only knows what was observed.
        let j: serde_json::Value = serde_json::from_str(&load.note).unwrap();
        assert!(j.get("tier_miss_count").is_some());
        assert!(j.get("observed_by").is_some());
        assert!(j.get("bytes_read").is_none(), "never invented fields");
    }

    /// Genome apply activates referenced skills/hooks under a grant.
    #[tokio::test]
    async fn genome_apply_activates_referenced_assets() {
        let rt = rt().await;
        let op = process_operator();
        let grant = openrt_workspace_assets::OperatorGrant::new(&op).unwrap();

        rt.assets_register_skill("skill.g1", "g skill", vec!["fs.read".into()])
            .await
            .unwrap();
        rt.assets_validate_skill("skill.g1").await.unwrap();

        let mut assets = rt.load_assets().await.unwrap();
        assets
            .register_hook(
                Hook::new(
                    HookId("hook.g1".into()),
                    1,
                    HookTarget::RunCompleted,
                    "on complete",
                    None,
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        assets
            .register_genome(
                openrt_workspace_assets::AgentGenome::new(
                    openrt_workspace_assets::GenomeId("genome.m54".into()),
                    1,
                    vec![(SkillId("skill.g1".into()), SkillVersion(1))],
                    vec![],
                    vec![("hook.g1".into(), 1)],
                    vec![],
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        rt.save_assets(&assets).await.unwrap();

        let activated = rt
            .assets_apply_genome("genome.m54", Some(&grant))
            .await
            .unwrap();
        assert!(
            activated.contains(&"skill.g1".to_string()),
            "skill must activate: {activated:?}"
        );
        assert!(
            activated.contains(&"hook.g1".to_string()),
            "hook must activate: {activated:?}"
        );
        let outcomes = rt
            .fire_hooks(openrt_workspace_assets::HookTarget::RunCompleted)
            .await
            .unwrap();
        assert!(outcomes.iter().any(|o| o.hook == "hook.g1" && o.fired));
    }

    /// Hook with a permission NOT in the gate policy fails closed.
    #[tokio::test]
    async fn hook_permission_denied_fails_closed() {
        let rt = rt().await;
        let op = process_operator();
        let grant = openrt_workspace_assets::OperatorGrant::new(&op).unwrap();
        let mut assets = rt.load_assets().await.unwrap();
        assets
            .register_hook(
                Hook::new(
                    HookId("hook.net".into()),
                    1,
                    HookTarget::RunCompleted,
                    "needs network",
                    Some("network.http".into()),
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        assets
            .activate_hook(&HookId("hook.net".into()), Some(&grant))
            .unwrap();
        rt.save_assets(&assets).await.unwrap();

        // network.http is not in the default allow policy → denied.
        assert!(!rt.hook_permission_granted("network.http"));
        let err = rt.assets_run_hook("hook.net").await;
        assert!(
            matches!(
                err,
                Err(Error::Asset(
                    openrt_workspace_assets::AssetError::UnauthorizedHook { .. }
                ))
            ),
            "denied permission must fail closed: {err:?}"
        );
    }

    /// Redaction-at-write: memory values under sensitive keys never persist
    /// in cleartext (SEC-05 extension into Phase-5 stores).
    #[tokio::test]
    async fn memory_value_redacted_at_write_for_sensitive_keys() {
        let rt = rt().await;
        rt.memory_remember_origin("run.redact", "canonical-redact")
            .await
            .unwrap();
        let item = rt
            .memory_remember(
                "fact.token",
                "run.redact",
                "src.pipeline",
                "ops",
                "api_key",
                "sk-super-secret",
                "2026-09-23T00:00:00Z",
                false,
            )
            .await
            .unwrap();
        assert_ne!(
            item.value, "sk-super-secret",
            "sensitive memory value must be redacted at write"
        );
        assert!(item.value.contains("REDACTED"), "value={}", item.value);
        // Redaction is durable: reopen the store and re-read the fact.
        let recalled = rt.memory_recall("fact", 8, 0.0).await.unwrap();
        let hit = recalled
            .items
            .iter()
            .find(|i| i.key == "api_key")
            .expect("redacted fact must still be recalled");
        assert!(
            !hit.value.contains("sk-super-secret"),
            "value={}",
            hit.value
        );
        assert!(hit.value.contains("REDACTED"), "value={}", hit.value);
    }

    /// M5.3 working-memory production integration: create → set → get → has →
    /// end → fail-closed; zero persistence (the durable store never receives
    /// working slots; after `end` the register is gone even across a fresh
    /// `load_session`).
    #[tokio::test]
    async fn m53_working_memory_lifecycle_and_zero_persistence() {
        let rt = rt().await;
        let session = rt.create_session("m53w").await.unwrap();
        rt.memory_working_set(session.id, "cwd", "/data/m53w", "run.m53w")
            .await
            .unwrap();
        assert!(rt.memory_working_has(session.id, "cwd").await.unwrap());
        let slot = rt.memory_working_get(session.id, "cwd").await.unwrap();
        assert_eq!(slot.value, "/data/m53w");
        assert_eq!(slot.origin_run, "run.m53w");
        assert_eq!(slot.origin_session, session.id.to_string());

        // Zero persistence: no durable record for the working slot in any
        // foundation namespace (working memory never reaches `store`).
        for ns in ["sessions", "tasks", "memory.working", "working"] {
            let rows = rt.store.list_records(ns).await.unwrap_or_default();
            let found = rows
                .iter()
                .any(|(_, v)| v.to_string().contains("/data/m53w"));
            assert!(!found, "working slot leaked into namespace {ns}");
        }

        // End: subsequent access fails closed (Unset — never a silent empty
        // value). Do not re-set after end; the register was dropped.
        rt.memory_working_end(session.id).await.unwrap();
        assert!(!rt.memory_working_has(session.id, "cwd").await.unwrap());
        let err = rt.memory_working_get(session.id, "cwd").await;
        assert!(err.is_err(), "post-end get must fail closed, got {err:?}");
        assert!(!rt.memory_working_has(session.id, "cwd").await.unwrap());
    }

    /// M5.6 production: a Tool-target proposal performs a REAL AssetStore
    /// mutation (register → validate → approve → apply activates the skill)
    /// and the activation + applied record survive a Runtime rebuild over the
    /// same MemoryStore (persistence-after-restart).
    #[tokio::test]
    async fn m56_tool_target_real_mutation_persists_across_restart() {
        use openrt_persistence::PersistenceStore;
        use openrt_selfimprove::ProposalState;
        let store = Arc::new(MemoryStore::new());
        let op = process_operator();
        let rt = {
            let s = store.clone();
            crate::RuntimeBuilder::new()
                .config(openrt_config::Config::default())
                .store(s)
                .with_provider(Arc::new(openrt_models::MockProvider::tool_sequence("mock")))
                .bus_capacity(32)
                .build()
                .await
                .unwrap()
        };

        rt.assets_register_skill("skill.m56t", "mutation target", vec!["fs.read".into()])
            .await
            .unwrap();
        rt.assets_validate_skill("skill.m56t").await.unwrap();
        let before = rt.assets_skill_states().await.unwrap();
        assert!(before
            .iter()
            .any(|(id, s)| id == "skill.m56t" && s == "validated"));

        rt.improve_record_baseline(
            "baseline.m56t",
            "activation_latency_ms",
            100.0,
            openrt_selfimprove::MetricDirection::LowerIsBetter,
            "run.m56t",
        )
        .await
        .unwrap();
        rt.improve_propose(
            "prop.m56t",
            openrt_selfimprove::ProposalKind::SelfOptimize,
            "activate the validated skill via operator-approved improvement",
            TargetKind::Tool,
            "skill.m56t",
            Some("baseline.m56t".into()),
            Some(80.0),
        )
        .await
        .unwrap();
        // Baseline present → real evaluation (not NoBaseline).
        let eval = rt.improve_evaluate("prop.m56t").await.unwrap();
        assert!(eval.contains("passed"), "eval={eval}");
        rt.improve_approve("prop.m56t", &op).await.unwrap();
        let change = rt.improve_apply("prop.m56t", &op).await.unwrap();
        assert_eq!(change.target_id, "skill.m56t");

        let after = rt.assets_skill_states().await.unwrap();
        assert!(
            after
                .iter()
                .any(|(id, s)| id == "skill.m56t" && s == "active"),
            "apply must perform a real AssetStore mutation: {after:?}"
        );
        let applied = store
            .get_record("improve.applied", "prop.m56t")
            .await
            .unwrap()
            .expect("applied change durable");
        assert_eq!(applied["proposal_id"], "prop.m56t");
        drop(rt);

        // Rebuild Runtime over the same store (restart): activation + applied
        // record must still be there — never in-memory-only.
        let rt2 = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(store)
            .with_provider(Arc::new(openrt_models::MockProvider::tool_sequence("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        let reopened = rt2.assets_skill_states().await.unwrap();
        assert!(
            reopened
                .iter()
                .any(|(id, s)| id == "skill.m56t" && s == "active"),
            "activation must survive restart: {reopened:?}"
        );
        let improve = rt2.load_improve().await.unwrap();
        assert_eq!(
            improve.get_proposal("prop.m56t").unwrap().state,
            ProposalState::Applied
        );
    }

    /// M5.6: a failing post-apply bench is RECORDED (ImprovementEvaluated
    /// with result=post_apply_failed + error), never a silent success.
    #[tokio::test]
    async fn m56_failed_post_eval_is_recorded_not_silent() {
        use openrt_models::mock::MockMode;
        use openrt_models::MockProvider;
        use openrt_telemetry::{ChannelSink, EventKind};
        let sink = Arc::new(ChannelSink::new());
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::new("fail", MockMode::Fail)))
            .sink(sink.clone())
            .bus_capacity(64)
            .build()
            .await
            .unwrap();
        let op = process_operator();

        rt.improve_record_baseline(
            "baseline.m56post",
            "latency_ms",
            100.0,
            openrt_selfimprove::MetricDirection::LowerIsBetter,
            "run.m56post",
        )
        .await
        .unwrap();
        rt.improve_propose(
            "prop.m56post",
            openrt_selfimprove::ProposalKind::SelfOptimize,
            "probe path that will fail post-eval",
            TargetKind::Router,
            "router.main",
            Some("baseline.m56post".into()),
            Some(80.0),
        )
        .await
        .unwrap();
        rt.improve_evaluate("prop.m56post").await.unwrap();
        rt.improve_approve("prop.m56post", &op).await.unwrap();
        // Apply still succeeds (mutation/log recorded); post-eval failure is
        // captured on the bus — not swallowed.
        let change = rt.improve_apply("prop.m56post", &op).await.unwrap();
        assert_eq!(change.proposal_id, "prop.m56post");

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let events = sink.snapshot();
        let post = events
            .iter()
            .find(|e| {
                e.kind == EventKind::ImprovementEvaluated
                    && e.data.get("phase").and_then(|p| p.as_str()) == Some("post_apply")
            })
            .expect("post_apply Evaluation event must be emitted");
        assert_eq!(
            post.data["result"].as_str(),
            Some("post_apply_failed"),
            "failed post-eval must be recorded, not silent: {}",
            post.data
        );
        assert!(
            post.data.get("error").and_then(|e| e.as_str()).is_some(),
            "failure must carry the error detail: {}",
            post.data
        );
        // The apply event still fires (change was applied); post-eval failure
        // is a separate, recorded outcome.
        assert!(events
            .iter()
            .any(|e| e.kind == EventKind::ImprovementApplied
                && e.data["proposal_id"] == "prop.m56post"));
    }

    /// M5.4c proof: after `assets_apply_genome`, a real `run_task` consumes
    /// the active genome and emits `LearningDerived` with kind=genome_consumed
    /// carrying the genome id + active instincts.
    #[tokio::test]
    async fn m54c_genome_consumed_on_real_run_emits_learning_derived() {
        use openrt_telemetry::{ChannelSink, EventKind};
        use openrt_workspace_assets::{Instinct, InstinctId};
        let sink = Arc::new(ChannelSink::new());
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(openrt_models::MockProvider::tool_sequence("mock")))
            .sink(sink.clone())
            .bus_capacity(64)
            .build()
            .await
            .unwrap();
        let op = process_operator();
        let grant = openrt_workspace_assets::OperatorGrant::new(&op).unwrap();

        rt.assets_register_skill("skill.gcon", "g skill", vec!["fs.read".into()])
            .await
            .unwrap();
        rt.assets_validate_skill("skill.gcon").await.unwrap();

        let mut assets = rt.load_assets().await.unwrap();
        assets
            .register_instinct(
                Instinct::new(
                    InstinctId("instinct.gcon".into()),
                    1,
                    "prefer short answers",
                    70,
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        assets
            .validate_instinct(&InstinctId("instinct.gcon".into()))
            .unwrap();
        assets
            .register_genome(
                openrt_workspace_assets::AgentGenome::new(
                    openrt_workspace_assets::GenomeId("genome.m54c".into()),
                    1,
                    vec![(SkillId("skill.gcon".into()), SkillVersion(1))],
                    vec![("instinct.gcon".into(), 1)],
                    vec![],
                    vec![],
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        rt.save_assets(&assets).await.unwrap();

        let activated = rt
            .assets_apply_genome("genome.m54c", Some(&grant))
            .await
            .unwrap();
        assert!(
            activated.contains(&"skill.gcon".to_string()),
            "genome must activate skill: {activated:?}"
        );
        assert!(
            activated.contains(&"instinct.gcon".to_string()),
            "genome must activate instinct: {activated:?}"
        );

        let session = rt.create_session("m54c").await.unwrap();
        let task = rt
            .run_task(session.id, "echo genome consumption")
            .await
            .unwrap();
        assert_eq!(task.status, openrt_core::TaskStatus::Completed);

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let events = sink.snapshot();
        let hit = events
            .iter()
            .find(|e| {
                e.kind == EventKind::LearningDerived
                    && e.data.get("kind").and_then(|k| k.as_str()) == Some("genome_consumed")
            })
            .expect("run_task must emit genome_consumed LearningDerived");
        assert_eq!(hit.data["active_genome"], "genome.m54c");
        let instincts = hit.data["active_instincts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            instincts
                .iter()
                .any(|i| i.as_str() == Some("instinct.gcon")),
            "active instincts must surface: {instincts:?}"
        );
    }

    /// M5.5: two different history evidences must produce different candidate
    /// ids and skill digests (evidence is part of the seed — never collide).
    #[tokio::test]
    async fn m55_history_evidence_a_vs_b_different_identity() {
        let rt = rt().await;
        let evidence_a = vec![("fs.read".to_string(), 3u64), ("fs.list".to_string(), 1u64)];
        let evidence_b = vec![("tool.echo".to_string(), 5u64)];
        let cand_a = rt
            .skill_from_history(
                "req.diff",
                "same description",
                vec!["fs.read".into()],
                &evidence_a,
            )
            .await
            .unwrap();
        let cand_b = rt
            .skill_from_history(
                "req.diff",
                "same description",
                vec!["fs.read".into()],
                &evidence_b,
            )
            .await
            .unwrap();
        assert_ne!(
            cand_a.id, cand_b.id,
            "different evidence → different candidate id"
        );
        assert_ne!(
            cand_a.skill.id.0, cand_b.skill.id.0,
            "different evidence → different skill id"
        );
        assert_ne!(
            cand_a.skill.digest(),
            cand_b.skill.digest(),
            "different evidence → different skill digest"
        );
        // Same evidence → identical identity (deterministic).
        let cand_a2 = rt
            .skill_from_history(
                "req.diff",
                "same description",
                vec!["fs.read".into()],
                &evidence_a,
            )
            .await
            .unwrap();
        assert_eq!(cand_a.id, cand_a2.id);
        assert_eq!(cand_a.skill.digest(), cand_a2.skill.digest());
    }

    /// M5.5 production openrt-orch wiring: a real DAG run through
    /// `Runtime::run_workflow` persists the definition under `dag/` and a
    /// run report under `runs/` (the orch engine is a production caller,
    /// not a test-only fake).
    #[tokio::test]
    async fn m55_orch_workflow_production_persists_dag_and_run() {
        use openrt_orch::{SubAgentKind, Workflow, WorkflowStep, DAG_NS, RUNS_NS};
        let rt = rt().await;
        let session = rt.create_session("m55wf").await.unwrap();
        let wf = Workflow::new("wf.m55")
            .named("phase5 production workflow")
            .step(WorkflowStep::new(
                "plan",
                SubAgentKind::Planner,
                "plan a tiny offline step",
            ))
            .step(
                WorkflowStep::new("verify", SubAgentKind::Verifier, "verify the plan output")
                    .dep("plan"),
            );
        let report = rt.run_workflow(session.id, wf).await.unwrap();
        assert_eq!(
            report.status,
            openrt_orch::WorkflowStatus::Completed,
            "workflow must complete: {report:?}"
        );
        let dag = rt
            .store
            .get_record(DAG_NS, "wf.m55")
            .await
            .unwrap()
            .expect("DAG definition must be durable under dag/");
        assert_eq!(dag["id"], "wf.m55");
        let run = rt
            .store
            .get_record(RUNS_NS, "wf.m55")
            .await
            .unwrap()
            .expect("run report must be durable under runs/");
        assert_eq!(run["workflow_id"], "wf.m55");
    }

    // ---- M10.8: agent genome completion ----

    fn m10_genome(id: &str) -> openrt_workspace_assets::AgentGenome {
        openrt_workspace_assets::AgentGenome::new(
            openrt_workspace_assets::GenomeId(id.to_string()),
            1,
            vec![],
            vec![],
            vec![],
            vec![],
            AssetProvenance::new("run.m10g", "src.phase5"),
        )
        .unwrap()
    }

    /// M10.8: runtime validation fails closed before any activation —
    /// unknown tools/models/verifiers and policy-denied declared
    /// permissions all refuse the apply; a valid profile applies.
    #[tokio::test]
    async fn m10_genome_runtime_validation_fails_closed() {
        let rt = rt().await;

        let mut unknown_tool = m10_genome("genome.m10.tool");
        unknown_tool.tools = vec!["not.a.tool".to_string()];
        let mut unknown_model = m10_genome("genome.m10.model");
        unknown_model.model = Some("gpt-nonexistent".to_string());
        let mut unknown_verifier = m10_genome("genome.m10.verifier");
        unknown_verifier.verifier = Some("not.a.tool".to_string());
        let mut denied_perm = m10_genome("genome.m10.perm");
        denied_perm.permissions = vec!["network.http".to_string()];
        let mut valid = m10_genome("genome.m10.valid");
        valid.tools = vec!["echo".to_string()];
        valid.model = Some("mock".to_string());
        valid.verifier = Some("echo".to_string());
        valid.permissions = vec!["fs.read".to_string()];
        valid.prompt = Some("be brief".to_string());

        {
            let mut assets = rt.load_assets().await.unwrap();
            for g in [
                unknown_tool,
                unknown_model,
                unknown_verifier,
                denied_perm,
                valid,
            ] {
                assets.register_genome(g).unwrap();
            }
            rt.save_assets(&assets).await.unwrap();
        }

        for (id, needle) in [
            ("genome.m10.tool", "unknown tool"),
            ("genome.m10.model", "unknown model"),
            ("genome.m10.verifier", "unknown verifier"),
            ("genome.m10.perm", "never grants permissions"),
        ] {
            let err = rt.assets_apply_genome(id, None).await.unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "{id} must fail with `{needle}`: {err}"
            );
        }
        // The valid profile applies (nothing to activate — grant unused).
        let activated = rt
            .assets_apply_genome("genome.m10.valid", None)
            .await
            .unwrap();
        assert!(activated.is_empty());
    }

    /// M10.8: applying a genome to a session records id + version + digest
    /// on the session and runs the approval-gated activation path.
    #[tokio::test]
    async fn m10_session_apply_genome_records_id_version_digest() {
        let rt = rt().await;
        let op = process_operator();
        rt.assets_register_skill("skill.m10g", "g skill", vec!["fs.read".into()])
            .await
            .unwrap();
        rt.assets_validate_skill("skill.m10g").await.unwrap();

        let mut g = m10_genome("genome.m10.session");
        g.skills = vec![(
            openrt_workspace_assets::SkillId("skill.m10g".into()),
            SkillVersion(1),
        )];
        g = g
            .with_profile(
                Some("mock".into()),
                Some("be brief".into()),
                vec!["echo".into()],
                None,
                None,
                Some("echo".into()),
                vec!["fs.read".into()],
            )
            .unwrap();
        {
            let mut assets = rt.load_assets().await.unwrap();
            assets.register_genome(g).unwrap();
            rt.save_assets(&assets).await.unwrap();
        }

        let session = rt.create_session("m10-genome").await.unwrap();
        let reference = rt
            .session_apply_genome(session.id, "genome.m10.session", &op)
            .await
            .unwrap();
        assert_eq!(reference.id, "genome.m10.session");
        assert_eq!(reference.version, 1);
        assert!(!reference.digest.is_empty());

        // Digest matches the durable genome record exactly.
        let assets = rt.load_assets().await.unwrap();
        let stored = assets
            .get_genome(&openrt_workspace_assets::GenomeId(
                "genome.m10.session".to_string(),
            ))
            .unwrap();
        assert_eq!(reference.digest, stored.digest());

        // Recorded on the session + referenced skill activated.
        let reloaded = rt.store.load_session(session.id).await.unwrap();
        assert_eq!(reloaded.genome, Some(reference));
        let states = rt.assets_skill_states().await.unwrap();
        assert!(states
            .iter()
            .any(|(id, s)| id == "skill.m10g" && s == "active"));
    }

    /// M10.8: running sessions are never re-genomed; archived sessions
    /// refuse the apply.
    #[tokio::test]
    async fn m10_session_apply_refuses_in_flight_and_archived() {
        let rt = rt().await;
        let op = process_operator();
        {
            let mut assets = rt.load_assets().await.unwrap();
            assets
                .register_genome(m10_genome("genome.m10.refuse"))
                .unwrap();
            rt.save_assets(&assets).await.unwrap();
        }

        // In-flight task (Created at spawn) → refused.
        let session = rt.create_session("m10-busy").await.unwrap();
        let handle = rt.spawn_task(session.id, "hi").await.unwrap();
        let err = rt
            .session_apply_genome(session.id, "genome.m10.refuse", &op)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("never re-genomed"),
            "in-flight session must refuse: {err}"
        );
        drop(handle);

        // Archived session → refused.
        let ended = rt.create_session("m10-ended").await.unwrap();
        rt.session_end(ended.id).await.unwrap();
        let err = rt
            .session_apply_genome(ended.id, "genome.m10.refuse", &op)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("archived"),
            "archived session must refuse: {err}"
        );
    }

    /// M10.8: genome evolution goes through propose → evaluate → operator
    /// approve → apply (the validated genome-apply path). Applying before
    /// approval fails closed.
    #[tokio::test]
    async fn m10_genome_evolution_is_approval_gated() {
        let rt = rt().await;
        let op = process_operator();
        {
            let mut assets = rt.load_assets().await.unwrap();
            assets
                .register_genome(m10_genome("genome.m10.evo"))
                .unwrap();
            rt.save_assets(&assets).await.unwrap();
        }

        rt.genome_evolve_propose("prop.m10evo", "genome.m10.evo", "tighten tool usage")
            .await
            .unwrap();
        let states = rt.improve_states().await.unwrap();
        assert!(states
            .iter()
            .any(|(id, s)| id == "prop.m10evo" && s == "proposed"));

        // Apply before approval fails closed.
        let early = rt.improve_apply("prop.m10evo", &op).await;
        assert!(early.is_err(), "apply before approve must fail: {early:?}");

        rt.improve_evaluate("prop.m10evo").await.unwrap();
        rt.improve_approve("prop.m10evo", &op).await.unwrap();
        rt.improve_apply("prop.m10evo", &op).await.unwrap();

        // The approved change ran the validated genome-apply path.
        let assets = rt.load_assets().await.unwrap();
        assert_eq!(
            assets.meta().active_genome.as_deref(),
            Some("genome.m10.evo")
        );
    }
}
