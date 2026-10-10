use std::collections::{BTreeMap, BTreeSet};

use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::asset::{ActivationState, AssetEnvelope, CanonicalForm, OperatorGrant};
use crate::canonical::ensure_path_safe_id;
use crate::generate::{ReviewState, SkillCandidate};
use crate::genome::AgentGenome;
use crate::hook::{Hook, HookId, HookOutcome, HookTarget};
use crate::instinct::{Instinct, InstinctId};
use crate::rule::{Rule, RuleDecision, RuleEffect, RuleId};
use crate::skill::{Skill, SkillId, SkillVersion};
use crate::skilltest::SkillTest;
use crate::workflow::Workflow;
use crate::{
    AssetError, CANDIDATES_NS, EXTENSIONS_NS, GENOMES_NS, HOOKS_NS, INSTINCTS_NS, RULES_NS,
    SKILLS_NS, SKILL_TESTS_NS, STORE_META_NS, WORKFLOWS_NS,
};

fn store_err(e: impl std::fmt::Display) -> AssetError {
    AssetError::Store(e.to_string())
}

/// Explicit network-capability classification for Sovereign Mode (ADR-003).
/// Exact names only — never a string-prefix heuristic on arbitrary ids.
pub(crate) fn is_network_capability(cap: &str) -> bool {
    matches!(
        cap,
        "network.http" | "network.egress" | "network.ingress" | "network.dns" | "http" | "net"
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreMeta {
    pub grants: Vec<String>,
    pub capabilities: Vec<String>,
    pub sovereign: bool,
    /// Id of the genome currently applied (consumed by the runtime on every
    /// session/task lifecycle — `None` until `assets_apply_genome` runs).
    #[serde(default)]
    pub active_genome: Option<String>,
}

impl StoreMeta {
    pub fn new(operators: &[String], capabilities: &[String], sovereign: bool) -> Self {
        let mut grants: Vec<String> = operators.to_vec();
        grants.sort();
        grants.dedup();
        let mut caps: Vec<String> = capabilities.to_vec();
        caps.sort();
        caps.dedup();
        Self {
            grants,
            capabilities: caps,
            sovereign,
            active_genome: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AssetStore {
    pub(crate) skills: BTreeMap<SkillId, BTreeMap<SkillVersion, AssetEnvelope<Skill>>>,
    pub(crate) extensions: BTreeMap<
        crate::extension::ExtensionId,
        BTreeMap<crate::extension::ExtensionVersion, AssetEnvelope<crate::extension::Extension>>,
    >,
    pub(crate) instincts: BTreeMap<InstinctId, AssetEnvelope<Instinct>>,
    pub(crate) hooks: BTreeMap<HookId, AssetEnvelope<Hook>>,
    pub(crate) rules: BTreeMap<RuleId, AssetEnvelope<Rule>>,
    pub(crate) genomes: BTreeMap<crate::GenomeId, AssetEnvelope<AgentGenome>>,
    pub(crate) candidates: BTreeMap<String, AssetEnvelope<SkillCandidate>>,
    pub(crate) workflows: BTreeMap<String, AssetEnvelope<Workflow>>,
    pub(crate) skill_tests: BTreeMap<String, AssetEnvelope<SkillTest>>,
    pub(crate) meta: StoreMeta,
}

impl AssetStore {
    pub fn new(operators: &[String], capabilities: &[String], sovereign: bool) -> Self {
        Self {
            skills: BTreeMap::new(),
            extensions: BTreeMap::new(),
            instincts: BTreeMap::new(),
            hooks: BTreeMap::new(),
            rules: BTreeMap::new(),
            genomes: BTreeMap::new(),
            candidates: BTreeMap::new(),
            workflows: BTreeMap::new(),
            skill_tests: BTreeMap::new(),
            meta: StoreMeta::new(operators, capabilities, sovereign),
        }
    }

    pub fn meta(&self) -> &StoreMeta {
        &self.meta
    }

    /// Mutable view of `StoreMeta` so the runtime can re-sync the sovereign
    /// flag from the single ModeManager authority on every load (ADR-003).
    pub fn meta_mut(&mut self) -> &mut StoreMeta {
        &mut self.meta
    }

    pub(crate) fn require_operator(
        &self,
        grant: Option<&OperatorGrant>,
        id: &str,
    ) -> Result<(), AssetError> {
        let grant =
            grant.ok_or_else(|| AssetError::UnauthorizedActivation { id: id.to_string() })?;
        if !self.meta.grants.iter().any(|g| g == &grant.operator) {
            return Err(AssetError::UnauthorizedActivation { id: id.to_string() });
        }
        Ok(())
    }

    pub(crate) fn require_capability(
        &self,
        permission: &str,
        detail: String,
    ) -> Result<(), AssetError> {
        if !self.meta.capabilities.iter().any(|c| c == permission) {
            return Err(AssetError::PolicyViolation { detail });
        }
        Ok(())
    }

    // ---- skills (multi-version, M10.6: no silent substitution) ----

    /// Highest stored version of `id`, or None if the id is unknown.
    fn dep_envelope(&self, dep_id: &SkillId) -> Option<&AssetEnvelope<Skill>> {
        self.skills.get(dep_id)?.values().next_back()
    }

    /// The skill a bare id resolves to: highest stored version.
    pub(crate) fn skill_env(&self, id: &SkillId) -> Result<&AssetEnvelope<Skill>, AssetError> {
        let env = self
            .skills
            .get(id)
            .and_then(|m| m.values().next_back())
            .ok_or_else(|| AssetError::NotFound {
                kind: "skill".into(),
                id: id.0.clone(),
            })?;
        env.verify()?;
        Ok(env)
    }

    fn skill_env_mut(&mut self, id: &SkillId) -> Result<&mut AssetEnvelope<Skill>, AssetError> {
        // Verify the current highest before handing out a mutable borrow.
        let env = self.skill_env(id)?;
        let version = env.payload.version;
        let env = self
            .skills
            .get_mut(id)
            .and_then(|m| m.get_mut(&version))
            .expect("checked above");
        Ok(env)
    }

    pub fn register_skill(&mut self, skill: Skill) -> Result<(), AssetError> {
        skill.provenance.validate()?;
        skill.validate_manifest()?;
        ensure_path_safe_id(&skill.id.0)?;
        for p in &skill.permissions {
            self.require_capability(
                p,
                format!(
                    "skill {} declares permission {} outside the operator allowlist",
                    skill.id, p
                ),
            )?;
        }
        let versions = self.skills.entry(skill.id.clone()).or_default();
        if versions.contains_key(&skill.version) {
            return Err(AssetError::Duplicate {
                kind: "skill".into(),
                id: skill.id.0.clone(),
            });
        }
        versions.insert(skill.version, AssetEnvelope::seal(skill));
        Ok(())
    }

    /// Bare-id resolution: the highest stored version. Explicit-version
    /// requests must go through [`AssetStore::get_skill_version`] /
    /// [`AssetStore::resolve_skill`] — those never substitute.
    pub fn get_skill(&self, id: &SkillId) -> Result<&Skill, AssetError> {
        Ok(&self.skill_env(id)?.payload)
    }

    /// Exact-version lookup. A requested version that is not stored is an
    /// error — the store never silently substitutes another version.
    pub fn get_skill_version(
        &self,
        id: &SkillId,
        version: SkillVersion,
    ) -> Result<&Skill, AssetError> {
        let env = self
            .skills
            .get(id)
            .and_then(|m| m.get(&version))
            .ok_or_else(|| AssetError::NotFound {
                kind: "skill.version".into(),
                id: format!("{}@{}", id.0, version.0),
            })?;
        env.verify()?;
        Ok(&env.payload)
    }

    /// Resolution entry point (M10.6): `Some(version)` is exact (fail-closed),
    /// `None` picks the highest stored version.
    pub fn resolve_skill(
        &self,
        id: &SkillId,
        requested: Option<SkillVersion>,
    ) -> Result<&Skill, AssetError> {
        match requested {
            Some(v) => self.get_skill_version(id, v),
            None => self.get_skill(id),
        }
    }

    /// Stored versions of `id`, ascending. Unknown id is an error.
    pub fn skill_versions(&self, id: &SkillId) -> Result<Vec<SkillVersion>, AssetError> {
        let m = self.skills.get(id).ok_or_else(|| AssetError::NotFound {
            kind: "skill".into(),
            id: id.0.clone(),
        })?;
        Ok(m.keys().copied().collect())
    }

    pub fn validate_skill(&mut self, id: &SkillId) -> Result<(), AssetError> {
        let version = self.skill_env(id)?.payload.version;
        self.validate_skill_at(id, version)
    }

    /// Exact-version validation (M10.6).
    pub fn validate_skill_at(
        &mut self,
        id: &SkillId,
        version: SkillVersion,
    ) -> Result<(), AssetError> {
        {
            let env = self
                .skills
                .get(id)
                .and_then(|m| m.get(&version))
                .ok_or_else(|| AssetError::NotFound {
                    kind: "skill.version".into(),
                    id: format!("{}@{}", id.0, version.0),
                })?;
            env.verify()?;
            let skill = &env.payload;
            skill.provenance.validate()?;
            skill.validate_manifest()?;
            match skill.activation {
                ActivationState::Invalidated => {
                    return Err(AssetError::Invalidated { id: id.0.clone() })
                }
                ActivationState::Active => return Ok(()),
                _ => {}
            }
            for dep in &skill.dependencies {
                let d =
                    self.dep_envelope(&dep.id)
                        .ok_or_else(|| AssetError::MissingDependency {
                            id: id.0.clone(),
                            dep: dep.id.0.clone(),
                        })?;
                d.verify()?;
                if d.payload.version < dep.min_version {
                    return Err(AssetError::IncompatibleDependency {
                        id: id.0.clone(),
                        dep: dep.id.0.clone(),
                        required: dep.min_version.0,
                        found: d.payload.version.0,
                    });
                }
            }
            self.check_cycle(id)?;
        }
        let env = self
            .skills
            .get_mut(id)
            .expect("checked above")
            .get_mut(&version)
            .expect("checked above");
        env.payload.activation = ActivationState::Validated;
        env.digest = env.payload.digest();
        env.verify()
    }

    fn check_cycle(&self, start: &SkillId) -> Result<(), AssetError> {
        let mut visited: BTreeSet<SkillId> = BTreeSet::new();
        let mut stack: Vec<SkillId> = vec![start.clone()];
        while let Some(cur) = stack.pop() {
            if !visited.insert(cur.clone()) {
                return Err(AssetError::DependencyCycle {
                    id: start.0.clone(),
                });
            }
            if let Some(versions) = self.skills.get(&cur) {
                if let Some(env) = versions.values().next_back() {
                    // Corruption propagates (fail-closed); a missing dep id is
                    // reported by the dependency checks that run before us.
                    env.verify()?;
                    for dep in &env.payload.dependencies {
                        stack.push(dep.id.clone());
                    }
                }
            }
        }
        Ok(())
    }

    pub fn activation_order(&self, id: &SkillId) -> Result<Vec<SkillId>, AssetError> {
        let mut visited: BTreeSet<SkillId> = BTreeSet::new();
        let mut order: Vec<SkillId> = Vec::new();
        self.visit_postorder(id, &mut visited, &mut order)?;
        Ok(order)
    }

    fn visit_postorder(
        &self,
        id: &SkillId,
        visited: &mut BTreeSet<SkillId>,
        order: &mut Vec<SkillId>,
    ) -> Result<(), AssetError> {
        if visited.contains(id) {
            return Ok(());
        }
        visited.insert(id.clone());
        let env = self.skill_env(id)?;
        let mut deps: Vec<SkillId> = env
            .payload
            .dependencies
            .iter()
            .map(|d| d.id.clone())
            .collect();
        deps.sort();
        for dep in deps {
            self.visit_postorder(&dep, visited, order)?;
        }
        order.push(id.clone());
        Ok(())
    }

    pub fn activate_skill(
        &mut self,
        id: &SkillId,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        let version = self.skill_env(id)?.payload.version;
        self.activate_skill_at(id, version, grant)
    }

    /// Exact-version activation (M10.6): the operator names the version that
    /// gets activated — never a different one.
    pub fn activate_skill_at(
        &mut self,
        id: &SkillId,
        version: SkillVersion,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, &id.0)?;
        {
            let env = self
                .skills
                .get(id)
                .and_then(|m| m.get(&version))
                .ok_or_else(|| AssetError::NotFound {
                    kind: "skill.version".into(),
                    id: format!("{}@{}", id.0, version.0),
                })?;
            env.verify()?;
            let skill = &env.payload;
            skill.provenance.validate()?;
            skill.validate_manifest()?;
            match skill.activation {
                ActivationState::Invalidated => {
                    return Err(AssetError::Invalidated { id: id.0.clone() })
                }
                ActivationState::Registered => {
                    return Err(AssetError::NotValidated { id: id.0.clone() })
                }
                ActivationState::Active => return Ok(()),
                ActivationState::Validated | ActivationState::Inactive => {}
            }
            for dep in &skill.dependencies {
                let d =
                    self.dep_envelope(&dep.id)
                        .ok_or_else(|| AssetError::MissingDependency {
                            id: id.0.clone(),
                            dep: dep.id.0.clone(),
                        })?;
                d.verify()?;
                if d.payload.version < dep.min_version {
                    return Err(AssetError::IncompatibleDependency {
                        id: id.0.clone(),
                        dep: dep.id.0.clone(),
                        required: dep.min_version.0,
                        found: d.payload.version.0,
                    });
                }
                if d.payload.activation != ActivationState::Active {
                    return Err(AssetError::DependencyNotActive {
                        id: id.0.clone(),
                        dep: dep.id.0.clone(),
                    });
                }
            }
            for p in &skill.permissions {
                self.require_capability(
                    p,
                    format!("skill {} requires permission {}", skill.id, p),
                )?;
            }
            // Sovereign Mode (re-synced from ModeManager on every load):
            // network capabilities are denied by exact classification —
            // never a string-prefix security decision.
            if self.meta.sovereign {
                if let Some(p) = skill.permissions.iter().find(|p| is_network_capability(p)) {
                    return Err(AssetError::SovereignViolation {
                        detail: format!("skill {} requires {} in sovereign mode", skill.id, p),
                    });
                }
            }
        }
        let env = self
            .skills
            .get_mut(id)
            .expect("checked above")
            .get_mut(&version)
            .expect("checked above");
        env.payload.activation = ActivationState::Active;
        env.digest = env.payload.digest();
        env.verify()
    }

    pub fn deactivate_skill(
        &mut self,
        id: &SkillId,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, &id.0)?;
        let env = self.skill_env_mut(id)?;
        env.verify()?;
        if env.payload.activation == ActivationState::Invalidated {
            return Err(AssetError::Invalidated { id: id.0.clone() });
        }
        env.payload.activation = ActivationState::Inactive;
        env.digest = env.payload.digest();
        env.verify()
    }

    pub fn invalidate_skill(
        &mut self,
        id: &SkillId,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, &id.0)?;
        let env = self.skill_env_mut(id)?;
        env.verify()?;
        env.payload.activation = ActivationState::Invalidated;
        env.digest = env.payload.digest();
        env.verify()
    }

    // ---- instincts ----

    pub fn register_instinct(&mut self, instinct: Instinct) -> Result<(), AssetError> {
        instinct.provenance.validate()?;
        instinct.validate_schema()?;
        ensure_path_safe_id(&instinct.id.0)?;
        if self.instincts.contains_key(&instinct.id) {
            return Err(AssetError::Duplicate {
                kind: "instinct".into(),
                id: instinct.id.0.clone(),
            });
        }
        self.instincts
            .insert(instinct.id.clone(), AssetEnvelope::seal(instinct));
        Ok(())
    }

    pub fn get_instinct(&self, id: &InstinctId) -> Result<&Instinct, AssetError> {
        let env = self.instincts.get(id).ok_or_else(|| AssetError::NotFound {
            kind: "instinct".into(),
            id: id.0.clone(),
        })?;
        env.verify()?;
        Ok(&env.payload)
    }

    pub fn invalidate_instinct(
        &mut self,
        id: &InstinctId,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, &id.0)?;
        let env = self
            .instincts
            .get_mut(id)
            .ok_or_else(|| AssetError::NotFound {
                kind: "instinct".into(),
                id: id.0.clone(),
            })?;
        env.verify()?;
        env.payload.activation = ActivationState::Invalidated;
        env.digest = env.payload.digest();
        env.verify()
    }

    /// Validate a registered instinct (Registered → Validated).
    pub fn validate_instinct(&mut self, id: &InstinctId) -> Result<(), AssetError> {
        {
            let env = self.instincts.get(id).ok_or_else(|| AssetError::NotFound {
                kind: "instinct".into(),
                id: id.0.clone(),
            })?;
            env.verify()?;
            let instinct = &env.payload;
            instinct.provenance.validate()?;
            instinct.validate_schema()?;
            match instinct.activation {
                ActivationState::Invalidated => {
                    return Err(AssetError::Invalidated { id: id.0.clone() })
                }
                ActivationState::Active => return Ok(()),
                ActivationState::Registered
                | ActivationState::Validated
                | ActivationState::Inactive => {}
            }
        }
        let env = self.instincts.get_mut(id).expect("checked above");
        env.payload.activation = ActivationState::Validated;
        env.digest = env.payload.digest();
        env.verify()
    }

    /// Activate a validated instinct under an operator grant (same gate as
    /// skills/hooks — no second activation mechanism).
    pub fn activate_instinct(
        &mut self,
        id: &InstinctId,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, &id.0)?;
        {
            let env = self.instincts.get(id).ok_or_else(|| AssetError::NotFound {
                kind: "instinct".into(),
                id: id.0.clone(),
            })?;
            env.verify()?;
            let instinct = &env.payload;
            instinct.provenance.validate()?;
            match instinct.activation {
                ActivationState::Invalidated => {
                    return Err(AssetError::Invalidated { id: id.0.clone() })
                }
                ActivationState::Registered => {
                    return Err(AssetError::NotValidated { id: id.0.clone() })
                }
                ActivationState::Active => return Ok(()),
                ActivationState::Validated | ActivationState::Inactive => {}
            }
        }
        let env = self.instincts.get_mut(id).expect("checked above");
        env.payload.activation = ActivationState::Active;
        env.digest = env.payload.digest();
        env.verify()
    }

    /// Deactivate an active instinct under an operator grant.
    pub fn deactivate_instinct(
        &mut self,
        id: &InstinctId,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, &id.0)?;
        let env = self
            .instincts
            .get_mut(id)
            .ok_or_else(|| AssetError::NotFound {
                kind: "instinct".into(),
                id: id.0.clone(),
            })?;
        env.verify()?;
        env.payload.activation = ActivationState::Inactive;
        env.digest = env.payload.digest();
        env.verify()
    }

    /// Deterministic (id, activation) listing for operator surfaces.
    pub fn instinct_states(&self) -> Vec<(String, &'static str)> {
        self.instincts
            .values()
            .map(|env| (env.payload.id.0.clone(), env.payload.activation.as_str()))
            .collect()
    }

    /// Active instincts sorted by weight (desc), then id — the patterns the
    /// runtime consults when composing a session/task context.
    pub fn active_instincts(&self) -> Vec<Instinct> {
        let mut out: Vec<Instinct> = self
            .instincts
            .values()
            .filter(|env| env.payload.activation == ActivationState::Active)
            .map(|env| env.payload.clone())
            .collect();
        out.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.id.0.cmp(&b.id.0)));
        out
    }

    // ---- hooks ----

    pub fn register_hook(&mut self, hook: Hook) -> Result<(), AssetError> {
        hook.provenance.validate()?;
        if self.hooks.contains_key(&hook.id) {
            return Err(AssetError::Duplicate {
                kind: "hook".into(),
                id: hook.id.0.clone(),
            });
        }
        self.hooks
            .insert(hook.id.clone(), AssetEnvelope::seal(hook));
        Ok(())
    }

    pub fn get_hook(&self, id: &HookId) -> Result<&Hook, AssetError> {
        let env = self.hooks.get(id).ok_or_else(|| AssetError::NotFound {
            kind: "hook".into(),
            id: id.0.clone(),
        })?;
        env.verify()?;
        Ok(&env.payload)
    }

    pub fn activate_hook(
        &mut self,
        id: &HookId,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, &id.0)?;
        let env = self.hooks.get_mut(id).ok_or_else(|| AssetError::NotFound {
            kind: "hook".into(),
            id: id.0.clone(),
        })?;
        env.verify()?;
        if env.payload.activation == ActivationState::Invalidated {
            return Err(AssetError::Invalidated { id: id.0.clone() });
        }
        env.payload.activation = ActivationState::Active;
        env.digest = env.payload.digest();
        env.verify()
    }

    pub fn deactivate_hook(
        &mut self,
        id: &HookId,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, &id.0)?;
        let env = self.hooks.get_mut(id).ok_or_else(|| AssetError::NotFound {
            kind: "hook".into(),
            id: id.0.clone(),
        })?;
        env.verify()?;
        env.payload.activation = ActivationState::Inactive;
        env.digest = env.payload.digest();
        env.verify()
    }

    pub fn run_hook(
        &self,
        id: &HookId,
        permission_granted: bool,
    ) -> Result<HookOutcome, AssetError> {
        let env = self.hooks.get(id).ok_or_else(|| AssetError::NotFound {
            kind: "hook".into(),
            id: id.0.clone(),
        })?;
        env.verify()?;
        if env.payload.activation != ActivationState::Active {
            return Err(AssetError::HookNotActive { id: id.0.clone() });
        }
        if let Some(required) = &env.payload.requires_permission {
            if !permission_granted {
                return Err(AssetError::UnauthorizedHook { id: id.0.clone() });
            }
            if !self.meta.capabilities.iter().any(|c| c == required) {
                return Err(AssetError::UnauthorizedHook { id: id.0.clone() });
            }
        }
        Ok(HookOutcome {
            hook: id.0.clone(),
            target: env.payload.target.as_str().to_string(),
            fired: true,
        })
    }

    pub fn hook_target(&self, id: &HookId) -> Result<HookTarget, AssetError> {
        Ok(self.get_hook(id)?.target)
    }

    /// Active hook ids registered for `target` (production fire path).
    pub fn active_hooks_for(&self, target: HookTarget) -> Vec<HookId> {
        self.hooks
            .values()
            .filter(|env| {
                env.payload.activation == ActivationState::Active && env.payload.target == target
            })
            .map(|env| env.payload.id.clone())
            .collect()
    }

    /// Deterministic (id, activation) listing for operator surfaces — the
    /// resolved (highest) version of each id.
    pub fn skill_states(&self) -> Vec<(String, &'static str)> {
        self.skills
            .values()
            .filter_map(|m| m.values().next_back())
            .map(|env| (env.payload.id.0.clone(), env.payload.activation.as_str()))
            .collect()
    }

    /// Every stored version: (id, version, activation), ascending by id then version.
    pub fn skill_states_all(&self) -> Vec<(String, u32, &'static str)> {
        self.skills
            .iter()
            .flat_map(|(id, versions)| {
                versions.values().map(move |env| {
                    (
                        id.0.clone(),
                        env.payload.version.0,
                        env.payload.activation.as_str(),
                    )
                })
            })
            .collect()
    }

    /// M10.7: read-only inventory of every stored instinct (id, payload) —
    /// operator surfaces list/inspect through this, never by re-parsing
    /// raw records themselves.
    pub fn instincts(&self) -> Vec<(&crate::InstinctId, &crate::Instinct)> {
        self.instincts
            .iter()
            .map(|(id, env)| (id, &env.payload))
            .collect()
    }

    /// M10.8: read-only inventory of every stored genome (id, payload).
    pub fn genomes(&self) -> Vec<(&crate::GenomeId, &crate::AgentGenome)> {
        self.genomes
            .iter()
            .map(|(id, env)| (id, &env.payload))
            .collect()
    }

    // ---- skill tests (M10.6) ----

    /// Attach a declarative test to an exact skill version. The referenced
    /// skill must exist at that exact version — tests never float across
    /// versions (fail-closed).
    pub fn register_skill_test(&mut self, test: SkillTest) -> Result<(), AssetError> {
        test.provenance.validate()?;
        ensure_path_safe_id(&test.id)?;
        let versions = self
            .skills
            .get(&test.skill_id)
            .ok_or_else(|| AssetError::NotFound {
                kind: "skill".into(),
                id: test.skill_id.0.clone(),
            })?;
        if !versions.contains_key(&test.skill_version) {
            return Err(AssetError::NotFound {
                kind: "skill.version".into(),
                id: format!("{}@{}", test.skill_id.0, test.skill_version.0),
            });
        }
        if self.skill_tests.contains_key(&test.id) {
            return Err(AssetError::Duplicate {
                kind: "skill_test".into(),
                id: test.id.clone(),
            });
        }
        self.skill_tests
            .insert(test.id.clone(), AssetEnvelope::seal(test));
        Ok(())
    }

    /// Tests registered for one exact skill version, ordered by id.
    pub fn skill_tests_for(&self, skill_id: &SkillId, version: SkillVersion) -> Vec<SkillTest> {
        self.skill_tests
            .values()
            .filter(|env| env.payload.skill_id == *skill_id && env.payload.skill_version == version)
            .filter_map(|env| env.verify().ok().map(|_| env.payload.clone()))
            .collect()
    }

    /// Link a compiled workflow graph to an exact skill version (M10.6
    /// skill→workflow compilation). Content changes reseal the skill and
    /// demote Active → Validated: an operator must re-activate after
    /// relinking (fail-closed — execution never runs stale-authorized
    /// content under an old approval).
    pub fn link_skill_workflow(
        &mut self,
        id: &SkillId,
        version: SkillVersion,
        workflow: String,
    ) -> Result<(), AssetError> {
        ensure_path_safe_id(&workflow)?;
        let env = self
            .skills
            .get_mut(id)
            .and_then(|m| m.get_mut(&version))
            .ok_or_else(|| AssetError::NotFound {
                kind: "skill.version".into(),
                id: format!("{}@{}", id.0, version.0),
            })?;
        env.verify()?;
        if env.payload.activation == ActivationState::Invalidated {
            return Err(AssetError::Invalidated { id: id.0.clone() });
        }
        env.payload.workflow = Some(workflow);
        env.payload.validate_manifest()?;
        if env.payload.activation == ActivationState::Active {
            env.payload.activation = ActivationState::Validated;
        }
        env.digest = env.payload.digest();
        env.verify()
    }

    pub fn hook_ids(&self) -> Vec<String> {
        self.hooks.keys().map(|k| k.0.clone()).collect()
    }

    pub fn rule_ids(&self) -> Vec<String> {
        self.rules.keys().map(|k| k.0.clone()).collect()
    }

    pub fn workflow_ids(&self) -> Vec<String> {
        self.workflows.keys().cloned().collect()
    }

    // ---- rules ----

    pub fn register_rule(&mut self, rule: Rule) -> Result<(), AssetError> {
        rule.provenance.validate()?;
        if self.rules.contains_key(&rule.id) {
            return Err(AssetError::Duplicate {
                kind: "rule".into(),
                id: rule.id.0.clone(),
            });
        }
        self.rules
            .insert(rule.id.clone(), AssetEnvelope::seal(rule));
        Ok(())
    }

    pub fn evaluate_rule(&self, scope: &str, subject: &str) -> Result<RuleDecision, AssetError> {
        let mut matched: Vec<&Rule> = self
            .rules
            .values()
            .filter(|env| {
                env.payload.enabled && env.payload.scope == scope && env.payload.subject == subject
            })
            .map(|env| &env.payload)
            .collect();
        for env in self.rules.values() {
            env.verify()?;
        }
        matched.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.0.cmp(&b.id.0)));
        if let Some(top) = matched.first() {
            let same_priority: Vec<&Rule> = matched
                .iter()
                .filter(|r| r.priority == top.priority)
                .copied()
                .collect();
            let has_allow = same_priority.iter().any(|r| r.effect == RuleEffect::Allow);
            let has_deny = same_priority.iter().any(|r| r.effect == RuleEffect::Deny);
            if has_allow && has_deny {
                let mut ids: Vec<String> = same_priority.iter().map(|r| r.id.0.clone()).collect();
                ids.sort();
                return Err(AssetError::RuleConflict {
                    a: ids[0].clone(),
                    b: ids[1].clone(),
                });
            }
        }
        if !self.meta.capabilities.iter().any(|c| c == subject) {
            return Ok(RuleDecision {
                allowed: false,
                decided_by: "security-policy".into(),
                rule: None,
            });
        }
        match matched.first() {
            Some(top) => Ok(RuleDecision {
                allowed: top.effect == RuleEffect::Allow,
                decided_by: "rule".into(),
                rule: Some(top.id.0.clone()),
            }),
            None => Ok(RuleDecision {
                allowed: false,
                decided_by: "default-deny".into(),
                rule: None,
            }),
        }
    }

    /// M10.7 ECC: every *enabled* `Deny` rule flattened for the runtime's
    /// deny-only gate layer. Exact `(scope, subject)` matching mirrors
    /// [`AssetStore::evaluate_rule`] — the layer only ever restricts.
    pub fn deny_rules(&self) -> Vec<(String, String, String)> {
        self.rules
            .values()
            .filter(|env| env.payload.enabled && env.payload.effect == RuleEffect::Deny)
            .map(|env| {
                (
                    env.payload.scope.clone(),
                    env.payload.subject.clone(),
                    env.payload.id.0.clone(),
                )
            })
            .collect()
    }

    // ---- genome ----

    pub fn register_genome(&mut self, genome: AgentGenome) -> Result<(), AssetError> {
        genome.provenance.validate()?;
        // M10.8: profile validation fails closed at registration.
        genome.validate_profile()?;
        self.validate_genome_refs(&genome)?;
        if self.genomes.contains_key(&genome.id) {
            return Err(AssetError::Duplicate {
                kind: "genome".into(),
                id: genome.id.0.clone(),
            });
        }
        self.genomes
            .insert(genome.id.clone(), AssetEnvelope::seal(genome));
        Ok(())
    }

    pub fn get_genome(&self, id: &crate::GenomeId) -> Result<&AgentGenome, AssetError> {
        let env = self.genomes.get(id).ok_or_else(|| AssetError::NotFound {
            kind: "genome".into(),
            id: id.0.clone(),
        })?;
        env.verify()?;
        Ok(&env.payload)
    }

    fn validate_genome_refs(&self, genome: &AgentGenome) -> Result<(), AssetError> {
        for (sid, sver) in &genome.skills {
            let found = self
                .skills
                .get(sid)
                .and_then(|m| m.get(sver))
                .ok_or_else(|| AssetError::InvalidGenomeReference {
                    detail: format!(
                        "genome {} references unknown skill {}@{}",
                        genome.id, sid, sver
                    ),
                })?;
            found.verify()?;
        }
        for (iid, iver) in &genome.instincts {
            let found = self
                .instincts
                .get(&InstinctId(iid.clone()))
                .ok_or_else(|| AssetError::InvalidGenomeReference {
                    detail: format!("genome {} references unknown instinct {}", genome.id, iid),
                })?;
            found.verify()?;
            if found.payload.version != *iver {
                return Err(AssetError::InvalidGenomeReference {
                    detail: format!(
                        "genome {} references instinct {}@{}, found {}",
                        genome.id, iid, iver, found.payload.version
                    ),
                });
            }
        }
        for (hid, hver) in &genome.hooks {
            let found = self.hooks.get(&HookId(hid.clone())).ok_or_else(|| {
                AssetError::InvalidGenomeReference {
                    detail: format!("genome {} references unknown hook {}", genome.id, hid),
                }
            })?;
            found.verify()?;
            if found.payload.version != *hver {
                return Err(AssetError::InvalidGenomeReference {
                    detail: format!(
                        "genome {} references hook {}@{}, found {}",
                        genome.id, hid, hver, found.payload.version
                    ),
                });
            }
        }
        for (rid, rver) in &genome.rules {
            let found = self.rules.get(&RuleId(rid.clone())).ok_or_else(|| {
                AssetError::InvalidGenomeReference {
                    detail: format!("genome {} references unknown rule {}", genome.id, rid),
                }
            })?;
            found.verify()?;
            if found.payload.version != *rver {
                return Err(AssetError::InvalidGenomeReference {
                    detail: format!(
                        "genome {} references rule {}@{}, found {}",
                        genome.id, rid, rver, found.payload.version
                    ),
                });
            }
        }
        Ok(())
    }

    // ---- candidates (M5.5) ----

    pub fn submit_candidate(&mut self, candidate: SkillCandidate) -> Result<(), AssetError> {
        candidate.provenance.validate()?;
        candidate.skill.provenance.validate()?;
        for p in &candidate.skill.permissions {
            self.require_capability(
                p,
                format!(
                    "candidate {} declares permission {} outside the operator allowlist",
                    candidate.id, p
                ),
            )?;
        }
        if self.candidates.contains_key(&candidate.id) {
            return Err(AssetError::Duplicate {
                kind: "candidate".into(),
                id: candidate.id.clone(),
            });
        }
        let id = candidate.id.clone();
        self.candidates.insert(id, AssetEnvelope::seal(candidate));
        Ok(())
    }

    pub fn get_candidate(&self, id: &str) -> Result<&SkillCandidate, AssetError> {
        let env = self
            .candidates
            .get(id)
            .ok_or_else(|| AssetError::NotFound {
                kind: "candidate".into(),
                id: id.to_string(),
            })?;
        env.verify()?;
        Ok(&env.payload)
    }

    fn set_candidate_review(&mut self, id: &str, review: ReviewState) -> Result<(), AssetError> {
        let env = self
            .candidates
            .get_mut(id)
            .ok_or_else(|| AssetError::NotFound {
                kind: "candidate".into(),
                id: id.to_string(),
            })?;
        env.verify()?;
        env.payload.review = review;
        env.digest = env.payload.digest();
        env.verify()
    }

    pub fn validate_candidate(&mut self, id: &str) -> Result<(), AssetError> {
        {
            let env = self
                .candidates
                .get(id)
                .ok_or_else(|| AssetError::NotFound {
                    kind: "candidate".into(),
                    id: id.to_string(),
                })?;
            env.verify()?;
            if env.payload.review == ReviewState::Rejected {
                return Err(AssetError::CandidateRejected { id: id.to_string() });
            }
            let skill = &env.payload.skill;
            for dep in &skill.dependencies {
                let d =
                    self.dep_envelope(&dep.id)
                        .ok_or_else(|| AssetError::MissingDependency {
                            id: id.to_string(),
                            dep: dep.id.0.clone(),
                        })?;
                if d.payload.version < dep.min_version {
                    return Err(AssetError::IncompatibleDependency {
                        id: id.to_string(),
                        dep: dep.id.0.clone(),
                        required: dep.min_version.0,
                        found: d.payload.version.0,
                    });
                }
            }
        }
        self.set_candidate_review(id, ReviewState::Validated)
    }

    pub fn approve_candidate(
        &mut self,
        id: &str,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, id)?;
        let current = self.get_candidate(id)?.review;
        match current {
            ReviewState::Validated => {}
            ReviewState::Rejected => {
                return Err(AssetError::CandidateRejected { id: id.to_string() })
            }
            ReviewState::Approved => return Ok(()),
            ReviewState::Generated => return Err(AssetError::NotValidated { id: id.to_string() }),
        }
        self.set_candidate_review(id, ReviewState::Approved)
    }

    pub fn reject_candidate(
        &mut self,
        id: &str,
        grant: Option<&OperatorGrant>,
    ) -> Result<(), AssetError> {
        self.require_operator(grant, id)?;
        self.set_candidate_review(id, ReviewState::Rejected)
    }

    pub fn activate_candidate(
        &mut self,
        id: &str,
        grant: Option<&OperatorGrant>,
    ) -> Result<SkillId, AssetError> {
        self.require_operator(grant, id)?;
        let skill = {
            let env = self
                .candidates
                .get(id)
                .ok_or_else(|| AssetError::NotFound {
                    kind: "candidate".into(),
                    id: id.to_string(),
                })?;
            env.verify()?;
            if env.payload.review == ReviewState::Generated {
                return Err(AssetError::NotValidated { id: id.to_string() });
            }
            if env.payload.review == ReviewState::Validated {
                return Err(AssetError::CandidateNotApproved { id: id.to_string() });
            }
            if env.payload.review == ReviewState::Rejected {
                return Err(AssetError::CandidateRejected { id: id.to_string() });
            }
            env.payload.skill.clone()
        };
        let sid = skill.id.clone();
        self.register_skill(skill)?;
        self.validate_skill(&sid)?;
        self.activate_skill(&sid, grant)?;
        Ok(sid)
    }

    // ---- workflows (M5.5) ----

    pub fn register_workflow(&mut self, workflow: Workflow) -> Result<(), AssetError> {
        workflow.provenance.validate()?;
        for p in &workflow.permissions {
            self.require_capability(
                p,
                format!(
                    "workflow {} declares permission {} outside the operator allowlist",
                    workflow.id, p
                ),
            )?;
        }
        for (dep, min_ver) in &workflow.dependencies {
            let found = self
                .workflows
                .get(dep)
                .ok_or_else(|| AssetError::MissingDependency {
                    id: workflow.id.clone(),
                    dep: dep.clone(),
                })?;
            if found.payload.version < *min_ver {
                return Err(AssetError::IncompatibleDependency {
                    id: workflow.id.clone(),
                    dep: dep.clone(),
                    required: *min_ver,
                    found: found.payload.version,
                });
            }
        }
        if self.workflows.contains_key(&workflow.id) {
            return Err(AssetError::Duplicate {
                kind: "workflow".into(),
                id: workflow.id.clone(),
            });
        }
        let id = workflow.id.clone();
        self.workflows.insert(id, AssetEnvelope::seal(workflow));
        Ok(())
    }

    pub fn get_workflow(&self, id: &str) -> Result<&Workflow, AssetError> {
        let env = self.workflows.get(id).ok_or_else(|| AssetError::NotFound {
            kind: "workflow".into(),
            id: id.to_string(),
        })?;
        env.verify()?;
        Ok(&env.payload)
    }

    pub fn workflow_step_names(&self, id: &str) -> Result<Vec<String>, AssetError> {
        Ok(self.get_workflow(id)?.step_names())
    }

    // ---- persistence ----

    fn seal_record<T: CanonicalForm + Serialize + serde::de::DeserializeOwned>(
        payload: &T,
    ) -> Result<serde_json::Value, AssetError> {
        let mut value = serde_json::to_value(payload)
            .map_err(|e| AssetError::Store(format!("serialize {}: {}", payload.asset_kind(), e)))?;
        // SEC-05 redaction-at-seal: scrub secret-like keys before the record
        // is digested and written. Digest is recomputed from the REDACTED
        // form so reopen verify (open_record) matches the stored bytes.
        openrt_core::RedactionPolicy::new().redact_value(&mut value);
        let sealed: T = serde_json::from_value(value.clone()).map_err(|e| {
            AssetError::Store(format!("re-open redacted {}: {}", payload.asset_kind(), e))
        })?;
        Ok(json!({ "payload": value, "digest": sealed.digest() }))
    }

    fn open_record<T: CanonicalForm + DeserializeOwned>(
        value: &serde_json::Value,
        kind: &str,
        id: &str,
    ) -> Result<T, AssetError> {
        let digest = value
            .get("digest")
            .and_then(|d| d.as_str())
            .ok_or_else(|| AssetError::DigestMismatch {
                kind: kind.to_string(),
                id: id.to_string(),
            })?
            .to_string();
        let payload_value =
            value
                .get("payload")
                .cloned()
                .ok_or_else(|| AssetError::DigestMismatch {
                    kind: kind.to_string(),
                    id: id.to_string(),
                })?;
        let payload: T = serde_json::from_value(payload_value)
            .map_err(|e| AssetError::Store(format!("deserialize {kind}/{id}: {e}")))?;
        let env: AssetEnvelope<T> = AssetEnvelope { payload, digest };
        env.verify()?;
        Ok(env.payload)
    }

    /// Persist the full asset snapshot. PUTs write current state; the
    /// trailing reconcile DELETES records absent from memory, so removal is
    /// only real when it survives a save (a put-only save would resurrect
    /// removed assets on the next load). Only AssetStore-owned namespaces
    /// are reconciled.
    pub async fn save(&self, store: &dyn PersistenceStore) -> Result<(), AssetError> {
        let mut keep: Vec<(&str, BTreeSet<String>)> = Vec::new();
        let mut keys = BTreeSet::new();
        for (id, versions) in &self.skills {
            for env in versions.values() {
                // Record key must be unique per (id, version) — persistence
                // keys allow only [a-z0-9._-], so `id.vN` (path-safe id +
                // version suffix) is the canonical form.
                let key = format!("{}.v{}", id.0, env.payload.version.0);
                store
                    .put_record(SKILLS_NS, &key, &Self::seal_record(&env.payload)?)
                    .await
                    .map_err(store_err)?;
                keys.insert(key);
            }
        }
        keep.push((SKILLS_NS, keys));

        let mut keys = BTreeSet::new();
        for (id, env) in &self.instincts {
            store
                .put_record(INSTINCTS_NS, &id.0, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
            keys.insert(id.0.clone());
        }
        keep.push((INSTINCTS_NS, keys));

        let mut keys = BTreeSet::new();
        for (id, env) in &self.hooks {
            store
                .put_record(HOOKS_NS, &id.0, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
            keys.insert(id.0.clone());
        }
        keep.push((HOOKS_NS, keys));

        let mut keys = BTreeSet::new();
        for (id, env) in &self.rules {
            store
                .put_record(RULES_NS, &id.0, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
            keys.insert(id.0.clone());
        }
        keep.push((RULES_NS, keys));

        let mut keys = BTreeSet::new();
        for (id, env) in &self.genomes {
            store
                .put_record(GENOMES_NS, &id.0, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
            keys.insert(id.0.clone());
        }
        keep.push((GENOMES_NS, keys));

        let mut keys = BTreeSet::new();
        for (id, env) in &self.candidates {
            store
                .put_record(CANDIDATES_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
            keys.insert(id.to_string());
        }
        keep.push((CANDIDATES_NS, keys));

        let mut keys = BTreeSet::new();
        for (id, env) in &self.workflows {
            store
                .put_record(WORKFLOWS_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
            keys.insert(id.to_string());
        }
        keep.push((WORKFLOWS_NS, keys));

        let mut keys = BTreeSet::new();
        for (id, env) in &self.skill_tests {
            store
                .put_record(SKILL_TESTS_NS, id, &Self::seal_record(&env.payload)?)
                .await
                .map_err(store_err)?;
            keys.insert(id.to_string());
        }
        keep.push((SKILL_TESTS_NS, keys));

        let mut keys = BTreeSet::new();
        for (id, versions) in &self.extensions {
            for env in versions.values() {
                let key = format!("{}.v{}", id.0, env.payload.version.0);
                store
                    .put_record(EXTENSIONS_NS, &key, &Self::seal_record(&env.payload)?)
                    .await
                    .map_err(store_err)?;
                keys.insert(key);
            }
        }
        keep.push((EXTENSIONS_NS, keys));

        let meta_value = serde_json::to_value(&self.meta)
            .map_err(|e| AssetError::Store(format!("serialize meta: {e}")))?;
        store
            .put_record(STORE_META_NS, "meta", &meta_value)
            .await
            .map_err(store_err)?;

        // Snapshot reconcile: delete what the in-memory state no longer
        // contains (removal APIs mutate memory — this is what makes the
        // deletion durable).
        for (ns, keep_keys) in &keep {
            for (key, _) in store.list_records(ns).await.map_err(store_err)? {
                if !keep_keys.contains(&key) {
                    store.delete_record(ns, &key).await.map_err(store_err)?;
                }
            }
        }
        Ok(())
    }

    pub async fn load(store: &dyn PersistenceStore) -> Result<Self, AssetError> {
        let meta_value = store
            .get_record(STORE_META_NS, "meta")
            .await
            .map_err(store_err)?
            .ok_or_else(|| AssetError::Store("missing assets.meta/meta".into()))?;
        let meta: StoreMeta = serde_json::from_value(meta_value)
            .map_err(|e| AssetError::Store(format!("deserialize meta: {e}")))?;
        let mut out = Self {
            skills: BTreeMap::new(),
            extensions: BTreeMap::new(),
            instincts: BTreeMap::new(),
            hooks: BTreeMap::new(),
            rules: BTreeMap::new(),
            genomes: BTreeMap::new(),
            candidates: BTreeMap::new(),
            workflows: BTreeMap::new(),
            skill_tests: BTreeMap::new(),
            meta,
        };
        for (key, value) in store.list_records(SKILLS_NS).await.map_err(store_err)? {
            let skill: Skill = Self::open_record(&value, "skill", &key)?;
            skill.provenance.validate()?;
            skill.validate_manifest()?;
            ensure_path_safe_id(&skill.id.0)?;
            let env = AssetEnvelope::seal(skill);
            match out
                .skills
                .entry(env.payload.id.clone())
                .or_default()
                .entry(env.payload.version)
            {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(env);
                }
                std::collections::btree_map::Entry::Occupied(o) => {
                    // Two records for the same (id, version): identical
                    // re-keyed copies are benign (upgrade from id-keyed save),
                    // divergent content is corruption — fail closed.
                    if o.get().digest != env.digest {
                        return Err(AssetError::Store(format!(
                            "conflicting records for skill {}@{}",
                            env.payload.id, env.payload.version
                        )));
                    }
                }
            }
        }
        for (key, value) in store.list_records(INSTINCTS_NS).await.map_err(store_err)? {
            let instinct: Instinct = Self::open_record(&value, "instinct", &key)?;
            instinct.provenance.validate()?;
            instinct.validate_schema()?;
            out.instincts
                .insert(instinct.id.clone(), AssetEnvelope::seal(instinct));
        }
        for (key, value) in store.list_records(HOOKS_NS).await.map_err(store_err)? {
            let hook: Hook = Self::open_record(&value, "hook", &key)?;
            hook.provenance.validate()?;
            out.hooks.insert(hook.id.clone(), AssetEnvelope::seal(hook));
        }
        for (key, value) in store.list_records(RULES_NS).await.map_err(store_err)? {
            let rule: Rule = Self::open_record(&value, "rule", &key)?;
            rule.provenance.validate()?;
            out.rules.insert(rule.id.clone(), AssetEnvelope::seal(rule));
        }
        for (key, value) in store.list_records(GENOMES_NS).await.map_err(store_err)? {
            let genome: AgentGenome = Self::open_record(&value, "genome", &key)?;
            genome.provenance.validate()?;
            out.genomes
                .insert(genome.id.clone(), AssetEnvelope::seal(genome));
        }
        for (key, value) in store.list_records(CANDIDATES_NS).await.map_err(store_err)? {
            let candidate: SkillCandidate = Self::open_record(&value, "candidate", &key)?;
            candidate.provenance.validate()?;
            out.candidates
                .insert(candidate.id.clone(), AssetEnvelope::seal(candidate));
        }
        for (key, value) in store.list_records(WORKFLOWS_NS).await.map_err(store_err)? {
            let workflow: Workflow = Self::open_record(&value, "workflow", &key)?;
            workflow.provenance.validate()?;
            out.workflows
                .insert(workflow.id.clone(), AssetEnvelope::seal(workflow));
        }
        for (key, value) in store
            .list_records(SKILL_TESTS_NS)
            .await
            .map_err(store_err)?
        {
            let test: SkillTest = Self::open_record(&value, "skill_test", &key)?;
            test.provenance.validate()?;
            ensure_path_safe_id(&test.id)?;
            out.skill_tests
                .insert(test.id.clone(), AssetEnvelope::seal(test));
        }
        for (key, value) in store.list_records(EXTENSIONS_NS).await.map_err(store_err)? {
            let ext: crate::extension::Extension = Self::open_record(&value, "extension", &key)?;
            ext.provenance.validate()?;
            ext.validate_manifest()?;
            ensure_path_safe_id(&ext.id.0)?;
            let env = AssetEnvelope::seal(ext);
            match out
                .extensions
                .entry(env.payload.id.clone())
                .or_default()
                .entry(env.payload.version)
            {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(env);
                }
                std::collections::btree_map::Entry::Occupied(o) => {
                    if o.get().digest != env.digest {
                        return Err(AssetError::Store(format!(
                            "conflicting records for extension {}@{}",
                            env.payload.id, env.payload.version
                        )));
                    }
                }
            }
        }
        Ok(out)
    }

    // ---- packaging round-trip (export / import as artifacts) ----

    /// Serialize every sealed asset (and the store meta) into a single JSON
    /// artifact. Digests travel with the payloads so `import_package` can
    /// re-verify fail-closed (never trusts unsigned/unsealed content).
    pub fn export_package(&self) -> Result<serde_json::Value, AssetError> {
        let mut skills = Vec::new();
        for env in self.skills.values().flat_map(|versions| versions.values()) {
            skills.push(Self::seal_record(&env.payload)?);
        }
        let mut instincts = Vec::new();
        for env in self.instincts.values() {
            instincts.push(Self::seal_record(&env.payload)?);
        }
        let mut hooks = Vec::new();
        for env in self.hooks.values() {
            hooks.push(Self::seal_record(&env.payload)?);
        }
        let mut rules = Vec::new();
        for env in self.rules.values() {
            rules.push(Self::seal_record(&env.payload)?);
        }
        let mut genomes = Vec::new();
        for env in self.genomes.values() {
            genomes.push(Self::seal_record(&env.payload)?);
        }
        let mut candidates = Vec::new();
        for env in self.candidates.values() {
            candidates.push(Self::seal_record(&env.payload)?);
        }
        let mut workflows = Vec::new();
        for env in self.workflows.values() {
            workflows.push(Self::seal_record(&env.payload)?);
        }
        let mut skill_tests = Vec::new();
        for env in self.skill_tests.values() {
            skill_tests.push(Self::seal_record(&env.payload)?);
        }
        let mut extensions = Vec::new();
        for env in self
            .extensions
            .values()
            .flat_map(|versions| versions.values())
        {
            extensions.push(Self::seal_record(&env.payload)?);
        }
        let meta = serde_json::to_value(&self.meta)
            .map_err(|e| AssetError::Store(format!("serialize meta: {e}")))?;
        Ok(json!({
            "schema": 1,
            "meta": meta,
            "skills": skills,
            "instincts": instincts,
            "hooks": hooks,
            "rules": rules,
            "genomes": genomes,
            "candidates": candidates,
            "workflows": workflows,
            "skill_tests": skill_tests,
            "extensions": extensions,
        }))
    }

    /// Import a previously exported package into an existing store. Every
    /// payload is digest-re-verified (`open_record`) before it is accepted;
    /// a corrupt/tampered entry fails closed and nothing from that entry is
    /// installed. Existing ids with matching digests are left alone; a
    /// *different* digest for the same id is an explicit conflict error
    /// (never silent overwrite).
    pub fn import_package(&mut self, package: &serde_json::Value) -> Result<usize, AssetError> {
        let schema = package
            .get("schema")
            .and_then(|s| s.as_u64())
            .ok_or_else(|| AssetError::Malformed {
                detail: "package missing schema version".into(),
            })?;
        if schema != 1 {
            return Err(AssetError::Malformed {
                detail: format!("unsupported package schema {schema}"),
            });
        }
        let mut installed = 0usize;
        let mut install = |existing_digest: Option<String>,
                           incoming_digest: String,
                           kind: &str,
                           id: &str|
         -> Result<bool, AssetError> {
            match existing_digest {
                None => {
                    installed += 1;
                    Ok(true)
                }
                Some(d) if d == incoming_digest => Ok(false),
                Some(_) => Err(AssetError::Duplicate {
                    kind: kind.to_string(),
                    id: id.to_string(),
                }),
            }
        };

        // Skills — activation state is policy-owned, not import data: force
        // Registered so a crafted package cannot ship pre-Active assets past
        // the operator activation gate (digest is recomputed after coercion).
        let skills = package
            .get("skills")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for value in skills {
            let mut skill: Skill = Self::open_record(&value, "skill", "package")?;
            skill.provenance.validate()?;
            skill.validate_manifest()?;
            ensure_path_safe_id(&skill.id.0)?;
            skill.activation = ActivationState::Registered;
            let digest = skill.digest();
            let existing = self
                .skills
                .get(&skill.id)
                .and_then(|m| m.get(&skill.version))
                .map(|e| e.digest.clone());
            if install(existing, digest.clone(), "skill", &skill.id.0)? {
                self.skills
                    .entry(skill.id.clone())
                    .or_default()
                    .insert(skill.version, AssetEnvelope::seal(skill));
            }
        }
        // Instincts — same activation-boundary rule.
        let instincts = package
            .get("instincts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for value in instincts {
            let mut instinct: Instinct = Self::open_record(&value, "instinct", "package")?;
            instinct.provenance.validate()?;
            instinct.activation = ActivationState::Registered;
            let digest = instinct.digest();
            let existing = self.instincts.get(&instinct.id).map(|e| e.digest.clone());
            if install(existing, digest, "instinct", &instinct.id.0)? {
                self.instincts
                    .insert(instinct.id.clone(), AssetEnvelope::seal(instinct));
            }
        }
        // Skill tests — activation is policy-owned: coerce to Registered.
        let skill_tests = package
            .get("skill_tests")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for value in skill_tests {
            let mut test: SkillTest = Self::open_record(&value, "skill_test", "package")?;
            test.provenance.validate()?;
            ensure_path_safe_id(&test.id)?;
            test.activation = ActivationState::Registered;
            let digest = test.digest();
            let existing = self.skill_tests.get(&test.id).map(|e| e.digest.clone());
            if install(existing, digest.clone(), "skill_test", &test.id)? {
                self.skill_tests
                    .insert(test.id.clone(), AssetEnvelope::seal(test));
            }
        }
        // Hooks — same activation-boundary rule.
        let hooks = package
            .get("hooks")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for value in hooks {
            let mut hook: Hook = Self::open_record(&value, "hook", "package")?;
            hook.provenance.validate()?;
            hook.activation = ActivationState::Registered;
            let digest = hook.digest();
            let existing = self.hooks.get(&hook.id).map(|e| e.digest.clone());
            if install(existing, digest, "hook", &hook.id.0)? {
                self.hooks
                    .insert(hook.id.clone(), AssetEnvelope::seal(hook));
            }
        }
        // Rules
        let rules = package
            .get("rules")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for value in rules {
            let rule: Rule = Self::open_record(&value, "rule", "package")?;
            rule.provenance.validate()?;
            let digest = rule.digest();
            let existing = self.rules.get(&rule.id).map(|e| e.digest.clone());
            if install(existing, digest, "rule", &rule.id.0)? {
                self.rules
                    .insert(rule.id.clone(), AssetEnvelope::seal(rule));
            }
        }
        // Genomes
        let genomes = package
            .get("genomes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for value in genomes {
            let genome: AgentGenome = Self::open_record(&value, "genome", "package")?;
            genome.provenance.validate()?;
            let digest = genome.digest();
            let existing = self.genomes.get(&genome.id).map(|e| e.digest.clone());
            if install(existing, digest, "genome", &genome.id.0)? {
                self.genomes
                    .insert(genome.id.clone(), AssetEnvelope::seal(genome));
            }
        }
        // Candidates
        let candidates = package
            .get("candidates")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for value in candidates {
            let candidate: SkillCandidate = Self::open_record(&value, "candidate", "package")?;
            candidate.provenance.validate()?;
            let digest = candidate.digest();
            let existing = self.candidates.get(&candidate.id).map(|e| e.digest.clone());
            if install(existing, digest, "candidate", &candidate.id)? {
                self.candidates
                    .insert(candidate.id.clone(), AssetEnvelope::seal(candidate));
            }
        }
        // Workflows
        let workflows = package
            .get("workflows")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for value in workflows {
            let workflow: Workflow = Self::open_record(&value, "workflow", "package")?;
            workflow.provenance.validate()?;
            let digest = workflow.digest();
            let existing = self.workflows.get(&workflow.id).map(|e| e.digest.clone());
            if install(existing, digest, "workflow", &workflow.id)? {
                self.workflows
                    .insert(workflow.id.clone(), AssetEnvelope::seal(workflow));
            }
        }
        // Extensions — activation AND scan status are policy-owned, never
        // import data: force Registered/unscanned (recomputed digest).
        let extensions = package
            .get("extensions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for value in extensions {
            let mut ext: crate::extension::Extension =
                Self::open_record(&value, "extension", "package")?;
            ext.provenance.validate()?;
            ext.validate_manifest()?;
            ensure_path_safe_id(&ext.id.0)?;
            ext.activation = ActivationState::Registered;
            ext.security_status = crate::extension::SECURITY_UNSCANNED.to_string();
            let digest = ext.digest();
            let existing = self
                .extensions
                .get(&ext.id)
                .and_then(|m| m.get(&ext.version))
                .map(|e| e.digest.clone());
            if install(existing, digest, "extension", &ext.id.0)? {
                self.extensions
                    .entry(ext.id.clone())
                    .or_default()
                    .insert(ext.version, AssetEnvelope::seal(ext));
            }
        }
        Ok(installed)
    }
}

use serde::de::DeserializeOwned;

#[cfg(test)]
mod seal_redaction_tests {
    use super::*;
    use crate::asset::Provenance;
    use crate::skill::{Skill, SkillId, SkillVersion};
    use openrt_core::REDACTED;

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    struct SecretfulSkill {
        id: SkillId,
        version: SkillVersion,
        description: String,
        permissions: Vec<String>,
        provenance: Provenance,
        activation: ActivationState,
        #[serde(default)]
        api_key: Option<String>,
    }

    impl CanonicalForm for SecretfulSkill {
        fn asset_kind(&self) -> &'static str {
            "skill"
        }
        fn asset_id(&self) -> String {
            self.id.0.clone()
        }
        fn canonical_pairs(&self) -> Vec<(String, String)> {
            vec![
                ("kind".into(), "skill".into()),
                ("id".into(), self.id.0.clone()),
                ("version".into(), self.version.0.to_string()),
                ("description".into(), self.description.clone()),
            ]
        }
    }

    /// Production `seal_record` must run `RedactionPolicy` so a secret-like
    /// key never lands in the durable record (SEC-05), and reopen verify
    /// must still succeed against the REDACTED form.
    #[tokio::test]
    async fn seal_record_redacts_secret_keys_before_write() {
        use openrt_persistence::{MemoryStore, PersistenceStore};
        let mem = MemoryStore::default();
        let skill = Skill::new(
            SkillId("skill.redact".into()),
            SkillVersion(1),
            "legit skill",
            vec!["fs.read".into()],
            vec![],
            Provenance::new("run.r", "src.r"),
        )
        .unwrap();
        // Persist a normal skill first to prove the happy path.
        let mut s = AssetStore::new(&["alice".into()], &["fs.read".into()], false);
        s.register_skill(skill).unwrap();
        s.save(&mem).await.unwrap();
        let row = mem
            .get_record(SKILLS_NS, "skill.redact.v1")
            .await
            .unwrap()
            .unwrap();
        let stored = row.to_string();
        assert!(
            !stored.contains("sk-"),
            "no secret in normal skill: {stored}"
        );

        // Direct seal of a payload that carries an api_key field: the key
        // must be replaced with REDACTED in the sealed JSON.
        let payload = SecretfulSkill {
            id: SkillId("skill.secret".into()),
            version: SkillVersion(1),
            description: "has secret".into(),
            permissions: vec![],
            provenance: Provenance::new("run.r", "src.r"),
            activation: ActivationState::Registered,
            api_key: Some("sk-live-do-not-persist".into()),
        };
        let sealed = AssetStore::seal_record(&payload).unwrap();
        let json = sealed.to_string();
        assert!(
            !json.contains("sk-live-do-not-persist"),
            "cleartext secret leaked: {json}"
        );
        assert_eq!(sealed["payload"]["api_key"], REDACTED);
        // Reopen path: open_record verifies digest over the REDACTED form.
        let reopened: SecretfulSkill =
            AssetStore::open_record(&sealed, "skill", "skill.secret").unwrap();
        assert_eq!(reopened.api_key.as_deref(), Some(REDACTED));
    }

    /// `save` is a full snapshot of every AssetStore-owned namespace:
    /// records absent from memory are deleted, so a removal survives
    /// save → load (a put-only save would resurrect the removed record).
    #[tokio::test]
    async fn save_reconciles_removals_into_the_snapshot() {
        use crate::extension::{Extension, ExtensionId, ExtensionVersion};
        use openrt_persistence::{MemoryStore, PersistenceStore};

        let mem = MemoryStore::default();
        let mut s = AssetStore::new(&["alice".into()], &[], false);
        let make = |id: &str| {
            Extension::new(
                ExtensionId(id.into()),
                ExtensionVersion(1),
                "name",
                "description",
                Provenance::new("r", "s"),
            )
            .unwrap()
        };
        s.register_extension(make("ext.keep")).unwrap();
        s.register_extension(make("ext.gone")).unwrap();
        s.save(&mem).await.unwrap();
        assert!(mem
            .get_record(EXTENSIONS_NS, "ext.gone.v1")
            .await
            .unwrap()
            .is_some());

        let grant = OperatorGrant::new("alice").unwrap();
        s.remove_extension(&ExtensionId("ext.gone".into()), Some(&grant))
            .unwrap();
        s.save(&mem).await.unwrap();
        assert!(
            mem.get_record(EXTENSIONS_NS, "ext.gone.v1")
                .await
                .unwrap()
                .is_none(),
            "a removed extension must not resurrect on save"
        );
        assert!(mem
            .get_record(EXTENSIONS_NS, "ext.keep.v1")
            .await
            .unwrap()
            .is_some());

        let loaded = AssetStore::load(&mem).await.unwrap();
        assert!(loaded
            .resolve_extension(&ExtensionId("ext.keep".into()), None)
            .is_ok());
        assert!(loaded
            .resolve_extension(&ExtensionId("ext.gone".into()), None)
            .is_err());
    }
}
