use std::collections::BTreeMap;

use openrt_persistence::PersistenceStore;
use openrt_workspace_assets::{
    ensure_path_safe_id, AssetEnvelope, CanonicalForm, OperatorGrant, Provenance,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::apply::{AppliedChange, ApplyTarget};
use crate::baseline::{map_asset, Baseline, EvalOutcome, EvalResult};
use crate::debug::DebugReport;
use crate::learn::{LearningEngine, LearningRecord, RunSummary};
use crate::proposal::{ImprovementProposal, ProposalState};
use crate::research::ResearchNote;
use crate::{
    EpisodeEvaluation, ExperienceEpisode, ImproveError, PromotionRecord, BASELINES_NS, DEBUG_NS,
    EPISODES_NS, LEARNINGS_NS, META_NS, PROMOTIONS_NS, PROPOSALS_NS, RESEARCH_NS,
};

fn store_err(e: impl std::fmt::Display) -> ImproveError {
    ImproveError::Store(e.to_string())
}

/// Explicit network-capability classification for Sovereign Mode (ADR-003).
/// Exact names only — never a string-prefix heuristic on arbitrary ids.
fn is_network_capability(cap: &str) -> bool {
    matches!(
        cap,
        "network.http" | "network.egress" | "network.ingress" | "network.dns" | "http" | "net"
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImproveMeta {
    pub operators: Vec<String>,
    pub sovereign: bool,
}

impl ImproveMeta {
    pub fn new(operators: &[String], sovereign: bool) -> Self {
        let mut ops: Vec<String> = operators.to_vec();
        ops.sort();
        ops.dedup();
        Self {
            operators: ops,
            sovereign,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImproveStore {
    pub(crate) baselines: BTreeMap<String, AssetEnvelope<Baseline>>,
    pub(crate) proposals: BTreeMap<String, AssetEnvelope<ImprovementProposal>>,
    pub(crate) debugs: BTreeMap<String, AssetEnvelope<DebugReport>>,
    pub(crate) research: BTreeMap<String, AssetEnvelope<ResearchNote>>,
    pub(crate) learnings: BTreeMap<String, AssetEnvelope<LearningRecord>>,
    pub(crate) episodes: BTreeMap<String, AssetEnvelope<ExperienceEpisode>>,
    pub(crate) promotions: BTreeMap<String, AssetEnvelope<PromotionRecord>>,
    pub(crate) meta: ImproveMeta,
}

impl ImproveStore {
    pub fn new(operators: &[String], sovereign: bool) -> Self {
        Self {
            baselines: BTreeMap::new(),
            proposals: BTreeMap::new(),
            debugs: BTreeMap::new(),
            research: BTreeMap::new(),
            learnings: BTreeMap::new(),
            episodes: BTreeMap::new(),
            promotions: BTreeMap::new(),
            meta: ImproveMeta::new(operators, sovereign),
        }
    }

    pub fn meta(&self) -> &ImproveMeta {
        &self.meta
    }

    /// Mutable view of `ImproveMeta` so the runtime can re-sync the sovereign
    /// flag from the single ModeManager authority on every load (ADR-003).
    pub fn meta_mut(&mut self) -> &mut ImproveMeta {
        &mut self.meta
    }

    fn require_operator(
        &self,
        grant: Option<&OperatorGrant>,
        id: &str,
    ) -> Result<(), ImproveError> {
        let grant =
            grant.ok_or_else(|| ImproveError::UnauthorizedApproval { id: id.to_string() })?;
        if !self.meta.operators.iter().any(|o| o == &grant.operator) {
            return Err(ImproveError::UnauthorizedOperator {
                operator: grant.operator.clone(),
            });
        }
        Ok(())
    }

    fn state_str(state: ProposalState) -> String {
        state.as_str().to_string()
    }

    // ---- baselines ----

    pub fn record_baseline(&mut self, baseline: Baseline) -> Result<(), ImproveError> {
        ensure_path_safe_id(&baseline.id).map_err(map_asset)?;
        baseline.provenance.validate().map_err(map_asset)?;
        if self.baselines.contains_key(&baseline.id) {
            return Err(ImproveError::Duplicate {
                kind: "baseline".into(),
                id: baseline.id.clone(),
            });
        }
        let id = baseline.id.clone();
        self.baselines.insert(id, AssetEnvelope::seal(baseline));
        Ok(())
    }

    pub fn get_baseline(&self, id: &str) -> Result<&Baseline, ImproveError> {
        let env = self
            .baselines
            .get(id)
            .ok_or_else(|| ImproveError::NotFound {
                kind: "baseline".into(),
                id: id.to_string(),
            })?;
        env.verify().map_err(map_asset)?;
        Ok(&env.payload)
    }

    // ---- proposals ----

    pub fn propose(&mut self, proposal: ImprovementProposal) -> Result<(), ImproveError> {
        proposal.provenance.validate().map_err(map_asset)?;
        ensure_path_safe_id(&proposal.id).map_err(map_asset)?;
        // Sovereign Mode is re-synced from ModeManager on every Runtime load
        // (ADR-003). Network capabilities are denied by exact classification
        // — never a string-prefix heuristic on target_id text.
        if self.meta.sovereign {
            if is_network_capability(&proposal.target_id) {
                return Err(ImproveError::SovereignViolation {
                    detail: format!(
                        "network capability `{}` on proposal {} denied in sovereign mode",
                        proposal.target_id, proposal.id
                    ),
                });
            }
            if let Some(c) = &proposal.change {
                if let Some(tid) = c.get("target_id").and_then(|v| v.as_str()) {
                    if is_network_capability(tid) {
                        return Err(ImproveError::SovereignViolation {
                            detail: format!(
                                "network capability `{tid}` in change denied in sovereign mode"
                            ),
                        });
                    }
                }
            }
        }
        if let Some(bid) = &proposal.baseline_id {
            self.get_baseline(bid)?;
        }
        if self.proposals.contains_key(&proposal.id) {
            return Err(ImproveError::Duplicate {
                kind: "proposal".into(),
                id: proposal.id.clone(),
            });
        }
        let id = proposal.id.clone();
        self.proposals.insert(id, AssetEnvelope::seal(proposal));
        Ok(())
    }

    pub fn get_proposal(&self, id: &str) -> Result<&ImprovementProposal, ImproveError> {
        let env = self
            .proposals
            .get(id)
            .ok_or_else(|| ImproveError::NotFound {
                kind: "proposal".into(),
                id: id.to_string(),
            })?;
        env.verify().map_err(map_asset)?;
        Ok(&env.payload)
    }

    /// Update a proposal's mutable fields (e.g. post-apply measurement) and
    /// re-seal. State must remain valid; digest is recomputed.
    pub fn update_proposal(&mut self, proposal: ImprovementProposal) -> Result<(), ImproveError> {
        proposal.provenance.validate().map_err(map_asset)?;
        ensure_path_safe_id(&proposal.id).map_err(map_asset)?;
        let env = self
            .proposals
            .get_mut(&proposal.id)
            .ok_or_else(|| ImproveError::NotFound {
                kind: "proposal".into(),
                id: proposal.id.clone(),
            })?;
        env.verify().map_err(map_asset)?;
        env.payload = proposal;
        env.digest = env.payload.digest();
        env.verify().map_err(map_asset)
    }

    /// Record the operator's decision note at approve/reject time (persisted).
    pub fn set_operator_decision(&mut self, id: &str, note: &str) -> Result<(), ImproveError> {
        let env = self
            .proposals
            .get_mut(id)
            .ok_or_else(|| ImproveError::NotFound {
                kind: "proposal".into(),
                id: id.to_string(),
            })?;
        env.verify().map_err(map_asset)?;
        env.payload.operator_decision = Some(note.to_string());
        env.digest = env.payload.digest();
        env.verify().map_err(map_asset)
    }

    /// Deterministic (id, state) listing for operator surfaces (palette/UI).
    pub fn proposal_states(&self) -> Vec<(String, &'static str)> {
        self.proposals
            .values()
            .map(|env| (env.payload.id.clone(), env.payload.state.as_str()))
            .collect()
    }

    fn set_state(
        &mut self,
        id: &str,
        next: ProposalState,
        expected: ProposalState,
    ) -> Result<(), ImproveError> {
        let env = self
            .proposals
            .get_mut(id)
            .ok_or_else(|| ImproveError::NotFound {
                kind: "proposal".into(),
                id: id.to_string(),
            })?;
        env.verify().map_err(map_asset)?;
        if env.payload.state != expected {
            return Err(ImproveError::InvalidState {
                id: id.to_string(),
                state: Self::state_str(env.payload.state),
                expected: expected.as_str().to_string(),
            });
        }
        env.payload.state = next;
        env.digest = env.payload.digest();
        env.verify().map_err(map_asset)
    }

    pub fn evaluate(&mut self, id: &str) -> Result<EvalResult, ImproveError> {
        let (state, baseline_id, expected, kind_str) = {
            let p = self.get_proposal(id)?;
            (
                p.state,
                p.baseline_id.clone(),
                p.expected_value,
                p.kind.as_str().to_string(),
            )
        };
        if state == ProposalState::Rejected || state == ProposalState::Applied {
            return Err(ImproveError::InvalidState {
                id: id.to_string(),
                state: Self::state_str(state),
                expected: ProposalState::Proposed.as_str().to_string(),
            });
        }
        if state != ProposalState::Proposed {
            return Err(ImproveError::InvalidState {
                id: id.to_string(),
                state: Self::state_str(state),
                expected: ProposalState::Proposed.as_str().to_string(),
            });
        }
        match baseline_id {
            None => {
                self.set_state(id, ProposalState::EvalPassed, ProposalState::Proposed)?;
                Ok(EvalResult::NoBaseline)
            }
            Some(bid) => {
                let (baseline_value, beats) = {
                    let baseline = self.get_baseline(&bid)?;
                    let candidate = expected.ok_or_else(|| ImproveError::Malformed {
                        detail: format!("proposal {id} ({kind_str}) missing expected_value"),
                    })?;
                    (baseline.value, baseline.beats(candidate))
                };
                let candidate = expected.ok_or_else(|| ImproveError::Malformed {
                    detail: format!("proposal {id} ({kind_str}) missing expected_value"),
                })?;
                if !beats {
                    self.set_state(id, ProposalState::Rejected, ProposalState::Proposed)?;
                    return Err(ImproveError::RegressionDetected {
                        id: id.to_string(),
                        baseline: baseline_value,
                        candidate,
                    });
                }
                self.set_state(id, ProposalState::EvalPassed, ProposalState::Proposed)?;
                Ok(EvalResult::Compared(EvalOutcome {
                    id: id.to_string(),
                    baseline_value,
                    candidate_value: candidate,
                    beats_baseline: true,
                }))
            }
        }
    }

    pub fn approve(&mut self, id: &str, grant: Option<&OperatorGrant>) -> Result<(), ImproveError> {
        self.require_operator(grant, id)?;
        let state = self.get_proposal(id)?.state;
        match state {
            ProposalState::Approved => return Ok(()),
            ProposalState::Proposed => return Err(ImproveError::EvalNotRun { id: id.to_string() }),
            ProposalState::EvalPassed => {}
            ProposalState::Rejected => {
                return Err(ImproveError::InvalidState {
                    id: id.to_string(),
                    state: state.as_str().to_string(),
                    expected: ProposalState::EvalPassed.as_str().to_string(),
                })
            }
            ProposalState::Applied => {
                return Err(ImproveError::InvalidState {
                    id: id.to_string(),
                    state: state.as_str().to_string(),
                    expected: ProposalState::EvalPassed.as_str().to_string(),
                })
            }
        }
        self.set_state(id, ProposalState::Approved, ProposalState::EvalPassed)
    }

    pub fn reject(&mut self, id: &str, grant: Option<&OperatorGrant>) -> Result<(), ImproveError> {
        self.require_operator(grant, id)?;
        let state = self.get_proposal(id)?.state;
        if state == ProposalState::Applied {
            return Err(ImproveError::InvalidState {
                id: id.to_string(),
                state: state.as_str().to_string(),
                expected: ProposalState::EvalPassed.as_str().to_string(),
            });
        }
        if state == ProposalState::Rejected {
            return Ok(());
        }
        let env = self.proposals.get_mut(id).expect("checked above");
        env.payload.state = ProposalState::Rejected;
        env.digest = env.payload.digest();
        env.verify().map_err(map_asset)
    }

    pub fn apply(
        &mut self,
        id: &str,
        target: &dyn ApplyTarget,
        grant: Option<&OperatorGrant>,
    ) -> Result<AppliedChange, ImproveError> {
        self.require_operator(grant, id)?;
        {
            let p = self.get_proposal(id)?;
            match p.state {
                ProposalState::Approved => {}
                ProposalState::Rejected | ProposalState::Applied => {
                    return Err(ImproveError::InvalidState {
                        id: id.to_string(),
                        state: p.state.as_str().to_string(),
                        expected: ProposalState::Approved.as_str().to_string(),
                    })
                }
                _ => return Err(ImproveError::NotApproved { id: id.to_string() }),
            }
            if target.kind() != p.target_kind || target.id() != p.target_id {
                return Err(ImproveError::Malformed {
                    detail: format!(
                        "apply target {} does not match proposal {} ({}/{})",
                        target.id(),
                        p.id,
                        p.target_kind.as_str(),
                        p.target_id
                    ),
                });
            }
            if p.target_kind.is_protected() {
                return Err(ImproveError::ProtectedTarget {
                    id: p.id.clone(),
                    target: p.target_kind.as_str().to_string(),
                });
            }
            target.apply(p)?;
        }
        self.set_state(id, ProposalState::Applied, ProposalState::Approved)?;
        let p = self.get_proposal(id)?;
        Ok(AppliedChange {
            proposal_id: p.id.clone(),
            target_kind: p.target_kind.as_str().to_string(),
            target_id: p.target_id.clone(),
            digest: p.digest(),
        })
    }

    // ---- self-debugging ----

    pub fn record_debug(&mut self, report: DebugReport) -> Result<(), ImproveError> {
        report.provenance.validate().map_err(map_asset)?;
        ensure_path_safe_id(&report.id).map_err(map_asset)?;
        if self.debugs.contains_key(&report.id) {
            return Err(ImproveError::Duplicate {
                kind: "debug".into(),
                id: report.id.clone(),
            });
        }
        let id = report.id.clone();
        self.debugs.insert(id, AssetEnvelope::seal(report));
        Ok(())
    }

    pub fn get_debug(&self, id: &str) -> Result<&DebugReport, ImproveError> {
        let env = self.debugs.get(id).ok_or_else(|| ImproveError::NotFound {
            kind: "debug".into(),
            id: id.to_string(),
        })?;
        env.verify().map_err(map_asset)?;
        Ok(&env.payload)
    }

    // ---- research / review ----

    pub fn record_research(&mut self, note: ResearchNote) -> Result<(), ImproveError> {
        note.provenance.validate().map_err(map_asset)?;
        ensure_path_safe_id(&note.id).map_err(map_asset)?;
        if self.research.contains_key(&note.id) {
            return Err(ImproveError::Duplicate {
                kind: "research".into(),
                id: note.id.clone(),
            });
        }
        let id = note.id.clone();
        self.research.insert(id, AssetEnvelope::seal(note));
        Ok(())
    }

    pub fn get_research(&self, id: &str) -> Result<&ResearchNote, ImproveError> {
        let env = self
            .research
            .get(id)
            .ok_or_else(|| ImproveError::NotFound {
                kind: "research".into(),
                id: id.to_string(),
            })?;
        env.verify().map_err(map_asset)?;
        Ok(&env.payload)
    }

    // ---- learning from execution history ----

    pub fn derive_learnings(
        &mut self,
        summary: &RunSummary,
        provenance: Provenance,
    ) -> Result<Vec<LearningRecord>, ImproveError> {
        let records = LearningEngine::derive(summary, provenance)?;
        let mut out = Vec::new();
        for record in records {
            ensure_path_safe_id(&record.id).map_err(map_asset)?;
            if let Some(existing) = self.learnings.get(&record.id) {
                existing.verify().map_err(map_asset)?;
                out.push(existing.payload.clone());
                continue;
            }
            let id = record.id.clone();
            self.learnings
                .insert(id.clone(), AssetEnvelope::seal(record));
            out.push(self.learnings[&id].payload.clone());
        }
        Ok(out)
    }

    pub fn get_learning(&self, id: &str) -> Result<&LearningRecord, ImproveError> {
        let env = self
            .learnings
            .get(id)
            .ok_or_else(|| ImproveError::NotFound {
                kind: "learning".into(),
                id: id.to_string(),
            })?;
        env.verify().map_err(map_asset)?;
        Ok(&env.payload)
    }

    // ---- persistence ----

    fn seal_record<T: CanonicalForm + Serialize + serde::de::DeserializeOwned>(
        payload: &T,
    ) -> Result<serde_json::Value, ImproveError> {
        let mut value = serde_json::to_value(payload).map_err(|e| {
            ImproveError::Store(format!("serialize {}: {}", payload.asset_kind(), e))
        })?;
        // SEC-05 redaction-at-seal: scrub secret-like keys before the record
        // is digested and written (same policy as memory/event boundaries).
        // Digest is recomputed from the REDACTED form so reopen verify
        // (open_record) matches the stored bytes exactly.
        openrt_core::RedactionPolicy::new().redact_value(&mut value);
        let sealed: T = serde_json::from_value(value.clone()).map_err(|e| {
            ImproveError::Store(format!("re-open redacted {}: {}", payload.asset_kind(), e))
        })?;
        Ok(json!({ "payload": value, "digest": sealed.digest() }))
    }

    fn open_record<T: CanonicalForm + DeserializeOwned>(
        value: &serde_json::Value,
        kind: &str,
        id: &str,
    ) -> Result<T, ImproveError> {
        let digest = value
            .get("digest")
            .and_then(|d| d.as_str())
            .ok_or_else(|| ImproveError::DigestMismatch {
                kind: kind.to_string(),
                id: id.to_string(),
            })?
            .to_string();
        let payload_value =
            value
                .get("payload")
                .cloned()
                .ok_or_else(|| ImproveError::DigestMismatch {
                    kind: kind.to_string(),
                    id: id.to_string(),
                })?;
        let payload: T = serde_json::from_value(payload_value)
            .map_err(|e| ImproveError::Store(format!("deserialize {kind}/{id}: {e}")))?;
        let env: AssetEnvelope<T> = AssetEnvelope { payload, digest };
        env.verify().map_err(map_asset)?;
        Ok(env.payload)
    }

    // ---- episodes (Phase 12, M12.12) ----

    /// Record an experience episode (sealed + digest-verified on read-back).
    pub fn record_episode(&mut self, episode: ExperienceEpisode) -> Result<(), ImproveError> {
        episode.provenance.validate().map_err(map_asset)?;
        ensure_path_safe_id(&episode.id).map_err(map_asset)?;
        if self.episodes.contains_key(&episode.id) {
            return Err(ImproveError::Duplicate {
                kind: "episode".into(),
                id: episode.id.clone(),
            });
        }
        let env = AssetEnvelope::seal(episode);
        self.episodes.insert(env.payload.id.clone(), env);
        Ok(())
    }

    pub fn get_episode(&self, id: &str) -> Result<&ExperienceEpisode, ImproveError> {
        self.episodes
            .get(id)
            .map(|e| &e.payload)
            .ok_or_else(|| ImproveError::NotFound {
                kind: "episode".into(),
                id: id.to_string(),
            })
    }

    pub fn list_episodes(&self) -> Vec<&ExperienceEpisode> {
        self.episodes.values().map(|e| &e.payload).collect()
    }

    /// Evaluate a stored episode into measured/inferred metrics (M12.12).
    pub fn evaluate_episode(&self, id: &str) -> Result<EpisodeEvaluation, ImproveError> {
        Ok(crate::episode::evaluate(self.get_episode(id)?))
    }

    // ---- promotion records (Phase 12, M12.17/M12.19) ----

    /// Record a promotion decision. `Promote` without measured benchmark
    /// metrics is refused — promotion evidence must be real numbers.
    pub fn record_promotion(&mut self, record: PromotionRecord) -> Result<(), ImproveError> {
        record.provenance.validate().map_err(map_asset)?;
        ensure_path_safe_id(&record.id).map_err(map_asset)?;
        if record.decision == crate::episode::PromotionDecision::Promote
            && record.benchmark.candidate_metrics.is_empty()
        {
            return Err(ImproveError::Malformed {
                detail: format!(
                    "promotion `{}`: PROMOTE requires measured candidate metrics (M12.17)",
                    record.id
                ),
            });
        }
        if self.promotions.contains_key(&record.id) {
            return Err(ImproveError::Duplicate {
                kind: "promotion".into(),
                id: record.id.clone(),
            });
        }
        let env = AssetEnvelope::seal(record);
        self.promotions.insert(env.payload.id.clone(), env);
        Ok(())
    }

    pub fn get_promotion(&self, id: &str) -> Result<&PromotionRecord, ImproveError> {
        self.promotions
            .get(id)
            .map(|e| &e.payload)
            .ok_or_else(|| ImproveError::NotFound {
                kind: "promotion".into(),
                id: id.to_string(),
            })
    }

    pub fn list_promotions(&self) -> Vec<&PromotionRecord> {
        self.promotions.values().map(|e| &e.payload).collect()
    }

    pub async fn save(&self, store: &dyn PersistenceStore) -> Result<(), ImproveError> {
        for (id, env) in &self.baselines {
            store
                .put_record(BASELINES_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
        }
        for (id, env) in &self.proposals {
            store
                .put_record(PROPOSALS_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
        }
        for (id, env) in &self.debugs {
            store
                .put_record(DEBUG_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
        }
        for (id, env) in &self.research {
            store
                .put_record(RESEARCH_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
        }
        for (id, env) in &self.learnings {
            store
                .put_record(LEARNINGS_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
        }
        for (id, env) in &self.episodes {
            store
                .put_record(EPISODES_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
        }
        for (id, env) in &self.promotions {
            store
                .put_record(PROMOTIONS_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
        }
        let meta_value = serde_json::to_value(&self.meta)
            .map_err(|e| ImproveError::Store(format!("serialize meta: {e}")))?;
        store
            .put_record(META_NS, "meta", &meta_value)
            .await
            .map_err(store_err)?;
        Ok(())
    }

    pub async fn load(store: &dyn PersistenceStore) -> Result<Self, ImproveError> {
        let meta_value = store
            .get_record(META_NS, "meta")
            .await
            .map_err(store_err)?
            .ok_or_else(|| ImproveError::Store("missing improve.meta/meta".into()))?;
        let meta: ImproveMeta = serde_json::from_value(meta_value)
            .map_err(|e| ImproveError::Store(format!("deserialize meta: {e}")))?;
        let mut out = Self {
            baselines: BTreeMap::new(),
            proposals: BTreeMap::new(),
            debugs: BTreeMap::new(),
            research: BTreeMap::new(),
            learnings: BTreeMap::new(),
            episodes: BTreeMap::new(),
            promotions: BTreeMap::new(),
            meta,
        };
        for (key, value) in store.list_records(BASELINES_NS).await.map_err(store_err)? {
            let baseline: Baseline = Self::open_record(&value, "baseline", &key)?;
            baseline.provenance.validate().map_err(map_asset)?;
            ensure_path_safe_id(&baseline.id).map_err(map_asset)?;
            out.baselines
                .insert(baseline.id.clone(), AssetEnvelope::seal(baseline));
        }
        for (key, value) in store.list_records(PROPOSALS_NS).await.map_err(store_err)? {
            let proposal: ImprovementProposal = Self::open_record(&value, "proposal", &key)?;
            proposal.provenance.validate().map_err(map_asset)?;
            ensure_path_safe_id(&proposal.id).map_err(map_asset)?;
            out.proposals
                .insert(proposal.id.clone(), AssetEnvelope::seal(proposal));
        }
        for (key, value) in store.list_records(DEBUG_NS).await.map_err(store_err)? {
            let report: DebugReport = Self::open_record(&value, "debug", &key)?;
            report.provenance.validate().map_err(map_asset)?;
            out.debugs
                .insert(report.id.clone(), AssetEnvelope::seal(report));
        }
        for (key, value) in store.list_records(RESEARCH_NS).await.map_err(store_err)? {
            let note: ResearchNote = Self::open_record(&value, "research", &key)?;
            note.provenance.validate().map_err(map_asset)?;
            out.research
                .insert(note.id.clone(), AssetEnvelope::seal(note));
        }
        for (key, value) in store.list_records(LEARNINGS_NS).await.map_err(store_err)? {
            let record: LearningRecord = Self::open_record(&value, "learning", &key)?;
            record.provenance.validate().map_err(map_asset)?;
            out.learnings
                .insert(record.id.clone(), AssetEnvelope::seal(record));
        }
        for (key, value) in store.list_records(EPISODES_NS).await.map_err(store_err)? {
            let episode: ExperienceEpisode = Self::open_record(&value, "episode", &key)?;
            episode.provenance.validate().map_err(map_asset)?;
            ensure_path_safe_id(&episode.id).map_err(map_asset)?;
            out.episodes
                .insert(episode.id.clone(), AssetEnvelope::seal(episode));
        }
        for (key, value) in store.list_records(PROMOTIONS_NS).await.map_err(store_err)? {
            let record: PromotionRecord = Self::open_record(&value, "promotion", &key)?;
            record.provenance.validate().map_err(map_asset)?;
            ensure_path_safe_id(&record.id).map_err(map_asset)?;
            out.promotions
                .insert(record.id.clone(), AssetEnvelope::seal(record));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod phase12_episode_promotion_tests {
    use super::*;
    use crate::episode::{
        evaluate, EpisodeOutcome, PromotionDecision, PromotionKind, Verification,
    };
    use crate::{EpisodeEvaluation, ExperienceEpisode, PromotionRecord};
    use openrt_workspace_assets::Provenance;

    fn episode(id: &str) -> ExperienceEpisode {
        ExperienceEpisode {
            id: id.to_string(),
            task: "summarize report".into(),
            context: "workspace:docs".into(),
            model: "mock".into(),
            prompt_ref: Some("prompts/sum".into()),
            tools: vec!["read".into(), "write".into()],
            actions: vec!["read a".into()],
            outcome: EpisodeOutcome::Success,
            verification: Verification::Passed,
            errors: 0,
            retries: 1,
            latency_ms: Some(1200),
            feedback: Some(1),
            artifact: Some("art.1".into()),
            provenance: Provenance::new("runtime", "phase12-test"),
            created_at: "2026-09-27T00:00:00Z".into(),
        }
    }

    #[test]
    fn episode_evaluation_splits_measured_and_inferred() {
        let e = episode("ep.1");
        let ev: EpisodeEvaluation = evaluate(&e);
        assert_eq!(ev.measured["verification_passed"], 1.0);
        assert_eq!(ev.measured["success"], 1.0);
        assert_eq!(ev.measured["retries"], 1.0);
        assert!(ev.inferred.contains_key("heuristic_quality"));
        assert_eq!(ev.method, "measured+inferred/v1");
    }

    #[tokio::test]
    async fn episode_and_promotion_roundtrip_persist() {
        use openrt_persistence::MemoryStore;
        let mem = MemoryStore::new();
        let mut s = ImproveStore::new(&["alice".into()], false);
        s.record_episode(episode("ep.1")).unwrap();
        assert!(s.record_episode(episode("ep.1")).is_err(), "duplicate id");
        assert_eq!(s.get_episode("ep.1").unwrap().task, "summarize report");
        assert_eq!(s.list_episodes().len(), 1);
        let ev = s.evaluate_episode("ep.1").unwrap();
        assert_eq!(ev.episode_id, "ep.1");

        let mut rec = PromotionRecord {
            id: "prom.1".into(),
            kind: PromotionKind::Skill,
            artifact: "skill.summarize".into(),
            decision: PromotionDecision::Promote,
            benchmark: crate::episode::BenchmarkEvidence {
                baseline_ref: Some("bench.base".into()),
                candidate_metrics: Default::default(),
                deltas: Default::default(),
                source: "bench.results".into(),
            },
            rationale: "looks good".into(),
            operator: Some("alice".into()),
            provenance: Provenance::new("runtime", "phase12-test"),
            created_at: "2026-09-27T00:00:00Z".into(),
        };
        let err = s.record_promotion(rec.clone()).unwrap_err();
        assert!(
            matches!(&err, ImproveError::Malformed { detail } if detail.contains("PROMOTE requires")),
            "{err:?}"
        );
        rec.decision = PromotionDecision::Hold;
        s.record_promotion(rec.clone()).unwrap();
        rec.id = "prom.2".into();
        rec.decision = PromotionDecision::Promote;
        rec.benchmark
            .candidate_metrics
            .insert("accuracy".into(), 0.91);
        s.record_promotion(rec).unwrap();
        assert_eq!(s.list_promotions().len(), 2);

        s.save(&mem).await.unwrap();
        let back = ImproveStore::load(&mem).await.unwrap();
        assert_eq!(back.list_episodes().len(), 1);
        assert_eq!(back.list_promotions().len(), 2);
        assert_eq!(
            back.get_promotion("prom.2").unwrap().decision,
            PromotionDecision::Promote
        );
    }
}

#[cfg(test)]
mod seal_redaction_tests {
    use super::*;
    use openrt_core::REDACTED;

    /// Production `seal_record` must scrub secret-like keys before the
    /// proposal is digested/written (SEC-05) so reopen verify matches.
    #[tokio::test]
    async fn seal_record_redacts_secret_keys_in_change_payload() {
        use openrt_persistence::{MemoryStore, PersistenceStore};
        let mem = MemoryStore::default();
        let mut s = ImproveStore::new(&["alice".into()], false);
        s.record_baseline(
            crate::Baseline::new(
                "baseline.redact",
                "latency_ms",
                100.0,
                crate::MetricDirection::LowerIsBetter,
                "run.r",
                crate::Provenance::new("run.r", "src.r"),
            )
            .unwrap(),
        )
        .unwrap();
        let mut p = crate::ImprovementProposal::new(
            "prop.redact",
            crate::ProposalKind::SelfOptimize,
            "route faster",
            crate::TargetKind::Router,
            "router.main",
            Some("baseline.redact".into()),
            Some(80.0),
            None,
            crate::Provenance::new("run.r", "src.r"),
        )
        .unwrap();
        p = p.map_change(Some(serde_json::json!({
            "op": "activate_skill",
            "target_id": "skill.x",
            "api_key": "sk-live-do-not-persist",
            "nested": { "password": "hunter2" },
        })));
        s.propose(p).unwrap();
        s.save(&mem).await.unwrap();
        let row = mem
            .get_record(PROPOSALS_NS, "prop.redact")
            .await
            .unwrap()
            .unwrap();
        let stored = row.to_string();
        assert!(
            !stored.contains("sk-live-do-not-persist"),
            "cleartext api_key leaked into improve store: {stored}"
        );
        assert!(
            !stored.contains("hunter2"),
            "cleartext password leaked into improve store: {stored}"
        );
        assert!(
            stored.contains(REDACTED),
            "REDACTED must be present: {stored}"
        );
        // Reopen verifies digest over the redacted form.
        let loaded = ImproveStore::load(&mem).await.unwrap();
        let re = loaded.get_proposal("prop.redact").unwrap();
        let change = re.change.clone().unwrap();
        assert_eq!(change["api_key"], REDACTED);
        assert_eq!(change["nested"]["password"], REDACTED);
        assert_eq!(change["op"], "activate_skill");
    }
}
