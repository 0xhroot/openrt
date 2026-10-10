//! Extension registry operations on the ONE asset store (Phase 11, M11.1).
//!
//! Reuses the skill state machine vocabulary: `Registered → Validated →
//! Active` with operator-gated activation, multi-version storage with no
//! silent substitution, dependency validation with cycle detection, and a
//! single-active-version invariant that makes update/rollback exact.

use std::collections::BTreeSet;

use crate::asset::{ActivationState, AssetEnvelope, CanonicalForm, OperatorGrant};
use crate::canonical::ensure_path_safe_id;
use crate::extension::{
    Extension, ExtensionId, ExtensionPackage, ExtensionTool, ExtensionVersion, SECURITY_CLEAN,
    SECURITY_UNSCANNED,
};
use crate::store::AssetStore;
use crate::{AssetError, CANDIDATES_NS};

use super::store::is_network_capability;

impl AssetStore {
    // ---- helpers ----

    fn ext_env(
        &self,
        id: &ExtensionId,
        version: ExtensionVersion,
    ) -> Result<&AssetEnvelope<Extension>, AssetError> {
        let env = self
            .extensions
            .get(id)
            .and_then(|m| m.get(&version))
            .ok_or_else(|| AssetError::NotFound {
                kind: "extension.version".into(),
                id: format!("{}@{}", id.0, version.0),
            })?;
        env.verify()?;
        Ok(env)
    }

    /// Highest stored version, or exact when `version` is given.
    pub fn resolve_extension(
        &self,
        id: &ExtensionId,
        version: Option<ExtensionVersion>,
    ) -> Result<&Extension, AssetError> {
        let versions = self
            .extensions
            .get(id)
            .ok_or_else(|| AssetError::NotFound {
                kind: "extension".into(),
                id: id.0.clone(),
            })?;
        let env = match version {
            Some(v) => versions.get(&v).ok_or_else(|| AssetError::NotFound {
                kind: "extension.version".into(),
                id: format!("{}@{}", id.0, v.0),
            })?,
            None => versions
                .values()
                .next_back()
                .ok_or_else(|| AssetError::NotFound {
                    kind: "extension".into(),
                    id: id.0.clone(),
                })?,
        };
        env.verify()?;
        Ok(&env.payload)
    }

    /// Stored versions of `id`, ascending.
    pub fn extension_versions(
        &self,
        id: &ExtensionId,
    ) -> Result<Vec<ExtensionVersion>, AssetError> {
        let m = self
            .extensions
            .get(id)
            .ok_or_else(|| AssetError::NotFound {
                kind: "extension".into(),
                id: id.0.clone(),
            })?;
        Ok(m.keys().copied().collect())
    }

    /// Every stored extension id, sorted.
    pub fn extension_ids(&self) -> Vec<ExtensionId> {
        self.extensions.keys().cloned().collect()
    }

    /// The version of `id` currently Active (at most one per id).
    pub fn active_extension_version(&self, id: &ExtensionId) -> Option<ExtensionVersion> {
        let versions = self.extensions.get(id)?;
        versions
            .values()
            .find(|e| e.payload.activation == ActivationState::Active)
            .map(|e| e.payload.version)
    }

    /// All Active extensions across the store (runtime build input).
    pub fn active_extensions(&self) -> Vec<&Extension> {
        self.extensions
            .values()
            .flat_map(|m| m.values())
            .filter(|e| e.payload.activation == ActivationState::Active)
            .map(|e| &e.payload)
            .collect()
    }

    // ---- install / register ----

    /// Register one extension version. Incoming activation/security state is
    /// coerced to `Registered`/`unscanned` — imported content can never ship
    /// pre-Active or pre-scanned past the operator gate (A3).
    pub fn register_extension(&mut self, mut ext: Extension) -> Result<(), AssetError> {
        ext.activation = ActivationState::Registered;
        ext.security_status = SECURITY_UNSCANNED.to_string();
        ext.provenance.validate()?;
        ext.validate_manifest()?;
        ensure_path_safe_id(&ext.id.0)?;
        for p in &ext.permissions {
            self.require_capability(
                p,
                format!(
                    "extension {} declares permission {} outside the operator allowlist",
                    ext.id, p
                ),
            )?;
        }
        let versions = self.extensions.entry(ext.id.clone()).or_default();
        if let Some(existing) = versions.get(&ext.version) {
            // Idempotent for identical content; divergent content fails closed.
            let coerced = ext.digest();
            if existing.digest != coerced {
                return Err(AssetError::Duplicate {
                    kind: "extension".into(),
                    id: format!("{}@{}", ext.id, ext.version),
                });
            }
            return Ok(());
        }
        versions.insert(ext.version, AssetEnvelope::seal(ext));
        Ok(())
    }

    /// Install a full package: payload skills land in `assets.skills` as
    /// Registered (identical re-installs are skipped, divergent ones refuse),
    /// then the extension version registers.
    pub fn install_extension_package(
        &mut self,
        pkg: &ExtensionPackage,
    ) -> Result<Vec<String>, AssetError> {
        pkg.validate()?;
        let mut installed_skills = Vec::new();
        for s in &pkg.skills {
            match self.skills.get(&s.id).and_then(|m| m.get(&s.version)) {
                Some(existing) => {
                    existing.verify()?;
                    let mut coerced = s.clone();
                    coerced.activation = ActivationState::Registered;
                    if existing.digest != coerced.digest() {
                        return Err(AssetError::Duplicate {
                            kind: "package.skill".into(),
                            id: format!("{}@{}", s.id, s.version),
                        });
                    }
                }
                None => {
                    let mut coerced = s.clone();
                    coerced.activation = ActivationState::Registered;
                    self.register_skill(coerced)?;
                    installed_skills.push(s.id.0.clone());
                }
            }
        }
        let ext_id = pkg.extension.id.clone();
        let ext_ver = pkg.extension.version;
        self.register_extension(pkg.extension.clone())?;
        // Cross-store coverage: every skill ref must be satisfiable now.
        {
            let ext = self.resolve_extension(&ext_id, Some(ext_ver))?;
            for r in &ext.skill_refs {
                let env = self
                    .skills
                    .get(&r.id)
                    .and_then(|m| m.values().next_back())
                    .ok_or_else(|| AssetError::MissingDependency {
                        id: ext_id.0.clone(),
                        dep: r.id.0.clone(),
                    })?;
                env.verify()?;
                if env.payload.version < r.min_version {
                    return Err(AssetError::IncompatibleDependency {
                        id: ext_id.0.clone(),
                        dep: r.id.0.clone(),
                        required: r.min_version.0,
                        found: env.payload.version.0,
                    });
                }
            }
        }
        Ok(installed_skills)
    }

    // ---- validate ----

    /// `Registered/Inactive → Validated` with full dependency + conflict
    /// analysis. Active is a no-op; Invalidated refuses.
    pub fn validate_extension(
        &mut self,
        id: &ExtensionId,
        version: Option<ExtensionVersion>,
    ) -> Result<ExtensionVersion, AssetError> {
        let version = match version {
            Some(v) => v,
            None => self.resolve_extension(id, None)?.version,
        };
        {
            let ext = self.ext_env(id, version)?.payload.clone();
            ext.provenance.validate()?;
            ext.validate_manifest()?;
            match ext.activation {
                ActivationState::Invalidated => {
                    return Err(AssetError::Invalidated { id: id.0.clone() })
                }
                ActivationState::Active => return Ok(version),
                _ => {}
            }
            // Extension dependencies: present, compatible, acyclic.
            for dep in &ext.dependencies {
                let d = self
                    .extensions
                    .get(&dep.id)
                    .and_then(|m| m.values().next_back())
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
            self.check_extension_cycle(id)?;
            // Skill refs must resolve (payload installed or pre-existing).
            for r in &ext.skill_refs {
                let env = self
                    .skills
                    .get(&r.id)
                    .and_then(|m| m.values().next_back())
                    .ok_or_else(|| AssetError::MissingDependency {
                        id: id.0.clone(),
                        dep: r.id.0.clone(),
                    })?;
                env.verify()?;
                if env.payload.version < r.min_version {
                    return Err(AssetError::IncompatibleDependency {
                        id: id.0.clone(),
                        dep: r.id.0.clone(),
                        required: r.min_version.0,
                        found: env.payload.version.0,
                    });
                }
            }
            // Tool bindings must reference resolvable skills.
            for t in &ext.tools {
                self.resolve_skill(
                    &t.skill,
                    if t.min_version.0 == 0 {
                        None
                    } else {
                        Some(t.min_version)
                    },
                )?;
            }
            // Claimed permissions must be within the store capability set.
            for p in &ext.permissions {
                self.require_capability(
                    p,
                    format!(
                        "extension {} requires permission {} outside the operator allowlist",
                        id, p
                    ),
                )?;
            }
            // Conflicting tool bindings against other extensions' payloads
            // (capability conflict detection — A8).
            self.check_extension_tool_conflicts(id, version, &ext.tools)?;
        }
        let env = self
            .extensions
            .get_mut(id)
            .expect("checked above")
            .get_mut(&version)
            .expect("checked above");
        env.payload.activation = ActivationState::Validated;
        env.digest = env.payload.digest();
        env.verify()?;
        Ok(version)
    }

    fn check_extension_cycle(&self, start: &ExtensionId) -> Result<(), AssetError> {
        let mut visited: BTreeSet<ExtensionId> = BTreeSet::new();
        let mut stack: Vec<ExtensionId> = vec![start.clone()];
        while let Some(cur) = stack.pop() {
            if !visited.insert(cur.clone()) {
                return Err(AssetError::DependencyCycle {
                    id: start.0.clone(),
                });
            }
            if let Some(versions) = self.extensions.get(&cur) {
                if let Some(env) = versions.values().next_back() {
                    env.verify()?;
                    for dep in &env.payload.dependencies {
                        stack.push(dep.id.clone());
                    }
                }
            }
        }
        Ok(())
    }

    /// Two extensions must not bind the same tool name (conflicting
    /// capability). Only Active extensions block; a validated-but-inactive
    /// payload is checked against Active ones at activation time.
    fn check_extension_tool_conflicts(
        &self,
        id: &ExtensionId,
        version: ExtensionVersion,
        tools: &[ExtensionTool],
    ) -> Result<(), AssetError> {
        for (oid, versions) in &self.extensions {
            if oid == id {
                continue;
            }
            for env in versions.values() {
                env.verify()?;
                if env.payload.activation != ActivationState::Active {
                    continue;
                }
                for t in tools {
                    if env.payload.tools.iter().any(|o| o.name == t.name) {
                        return Err(AssetError::PolicyViolation {
                            detail: format!(
                                "extension {} tool `{}` conflicts with active extension {}",
                                id, t.name, env.payload.id
                            ),
                        });
                    }
                }
            }
        }
        let _ = version;
        Ok(())
    }

    // ---- activate / deactivate / invalidate ----

    /// `Validated/Inactive → Active` under an operator grant. Requires a
    /// clean security scan (set only by the one AgentShield path), active
    /// dependencies, capability subset, and sovereignty; sets every other
    /// version `Inactive` (single-active-version invariant).
    pub fn activate_extension(
        &mut self,
        id: &ExtensionId,
        version: Option<ExtensionVersion>,
        grant: Option<&OperatorGrant>,
    ) -> Result<ExtensionVersion, AssetError> {
        self.require_operator(grant, &id.0)?;
        let version = match version {
            Some(v) => v,
            None => self.resolve_extension(id, None)?.version,
        };
        {
            let ext = self.ext_env(id, version)?.payload.clone();
            ext.provenance.validate()?;
            ext.validate_manifest()?;
            match ext.activation {
                ActivationState::Invalidated => {
                    return Err(AssetError::Invalidated { id: id.0.clone() })
                }
                ActivationState::Registered => {
                    return Err(AssetError::NotValidated { id: id.0.clone() })
                }
                ActivationState::Active => return Ok(version),
                ActivationState::Validated | ActivationState::Inactive => {}
            }
            if ext.security_status != SECURITY_CLEAN {
                return Err(AssetError::PolicyViolation {
                    detail: format!(
                        "extension {}@{} has not passed the security scan (status `{}`) — activation refused",
                        id, version, ext.security_status
                    ),
                });
            }
            for dep in &ext.dependencies {
                let d = self
                    .extensions
                    .get(&dep.id)
                    .and_then(|m| m.values().next_back())
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
            for r in &ext.skill_refs {
                let env = self
                    .skills
                    .get(&r.id)
                    .and_then(|m| m.values().next_back())
                    .ok_or_else(|| AssetError::MissingDependency {
                        id: id.0.clone(),
                        dep: r.id.0.clone(),
                    })?;
                env.verify()?;
                if env.payload.version < r.min_version {
                    return Err(AssetError::IncompatibleDependency {
                        id: id.0.clone(),
                        dep: r.id.0.clone(),
                        required: r.min_version.0,
                        found: env.payload.version.0,
                    });
                }
                if env.payload.activation != ActivationState::Active {
                    return Err(AssetError::DependencyNotActive {
                        id: id.0.clone(),
                        dep: r.id.0.clone(),
                    });
                }
            }
            for t in &ext.tools {
                let env = self.skill_env(&t.skill)?;
                if t.min_version.0 != 0 && env.payload.version < t.min_version {
                    return Err(AssetError::IncompatibleDependency {
                        id: id.0.clone(),
                        dep: t.skill.0.clone(),
                        required: t.min_version.0,
                        found: env.payload.version.0,
                    });
                }
                if env.payload.activation != ActivationState::Active {
                    return Err(AssetError::DependencyNotActive {
                        id: id.0.clone(),
                        dep: t.skill.0.clone(),
                    });
                }
            }
            for p in ext.permissions.iter().chain(ext.capabilities.iter()) {
                self.require_capability(p, format!("extension {} requires capability {}", id, p))?;
            }
            if self.meta.sovereign {
                let net = ext
                    .permissions
                    .iter()
                    .chain(ext.capabilities.iter())
                    .chain(ext.network.iter())
                    .find(|p| is_network_capability(p));
                if let Some(p) = net {
                    return Err(AssetError::SovereignViolation {
                        detail: format!("extension {} requires {} in sovereign mode", id, p),
                    });
                }
            }
            self.check_extension_tool_conflicts(id, version, &ext.tools)?;
        }
        // Single-active-version: demote siblings first, then promote.
        if let Some(versions) = self.extensions.get_mut(id) {
            for env in versions.values_mut() {
                if env.payload.version != version
                    && env.payload.activation == ActivationState::Active
                {
                    env.payload.activation = ActivationState::Inactive;
                    env.digest = env.payload.digest();
                }
            }
            let env = versions.get_mut(&version).expect("checked above");
            env.payload.activation = ActivationState::Active;
            env.digest = env.payload.digest();
            env.verify()?;
        }
        Ok(version)
    }

    /// Deactivate the Active version (or the highest when none is Active).
    pub fn deactivate_extension(
        &mut self,
        id: &ExtensionId,
        grant: Option<&OperatorGrant>,
    ) -> Result<ExtensionVersion, AssetError> {
        self.require_operator(grant, &id.0)?;
        let version = match self.active_extension_version(id) {
            Some(v) => v,
            None => self.resolve_extension(id, None)?.version,
        };
        let env = self
            .extensions
            .get_mut(id)
            .expect("checked above")
            .get_mut(&version)
            .expect("checked above");
        env.verify()?;
        if env.payload.activation == ActivationState::Invalidated {
            return Err(AssetError::Invalidated { id: id.0.clone() });
        }
        env.payload.activation = ActivationState::Inactive;
        env.digest = env.payload.digest();
        env.verify()?;
        Ok(version)
    }

    /// Terminal invalidation (operator-gated).
    pub fn invalidate_extension(
        &mut self,
        id: &ExtensionId,
        grant: Option<&OperatorGrant>,
    ) -> Result<ExtensionVersion, AssetError> {
        self.require_operator(grant, &id.0)?;
        let version = self.resolve_extension(id, None)?.version;
        let env = self
            .extensions
            .get_mut(id)
            .expect("checked above")
            .get_mut(&version)
            .expect("checked above");
        env.verify()?;
        env.payload.activation = ActivationState::Invalidated;
        env.digest = env.payload.digest();
        env.verify()?;
        Ok(version)
    }

    // ---- update / rollback ----

    /// Roll back to a specific (or the next-lower) version. The target must
    /// already be Validated and scan-clean; history (all versions) is
    /// preserved — rollback never deletes records or rewrites history.
    pub fn rollback_extension(
        &mut self,
        id: &ExtensionId,
        target: Option<ExtensionVersion>,
        grant: Option<&OperatorGrant>,
    ) -> Result<(ExtensionVersion, ExtensionVersion), AssetError> {
        self.require_operator(grant, &id.0)?;
        let versions = self.extension_versions(id)?;
        let current = self
            .active_extension_version(id)
            .unwrap_or(*versions.last().expect("non-empty"));
        let target = match target {
            Some(t) => {
                if !versions.contains(&t) {
                    return Err(AssetError::NotFound {
                        kind: "extension.version".into(),
                        id: format!("{}@{}", id.0, t.0),
                    });
                }
                t
            }
            None => versions
                .iter()
                .rev()
                .find(|v| **v < current)
                .copied()
                .ok_or_else(|| AssetError::PolicyViolation {
                    detail: format!(
                        "extension {} has no earlier version to roll back to (active {})",
                        id, current
                    ),
                })?,
        };
        if target == current {
            return Ok((current, target));
        }
        // Activate the target through the full gate (deps, scan status,
        // capabilities, sovereignty, conflicts) — then it becomes the single
        // Active version (siblings demoted by activate_extension).
        let activated = self.activate_extension(id, Some(target), grant)?;
        debug_assert_eq!(activated, target);
        Ok((current, target))
    }

    /// Remove every stored version of an extension. Refused while Active and
    /// while any other extension depends on it ("remove where allowed").
    /// Removal does not touch installed skills (they are the skill
    /// authority's records) and is audited by the runtime wrapper.
    pub fn remove_extension(
        &mut self,
        id: &ExtensionId,
        grant: Option<&OperatorGrant>,
    ) -> Result<Vec<ExtensionVersion>, AssetError> {
        self.require_operator(grant, &id.0)?;
        if self.active_extension_version(id).is_some() {
            return Err(AssetError::PolicyViolation {
                detail: format!("extension {} is active — deactivate it before removal", id),
            });
        }
        for (oid, versions) in &self.extensions {
            if oid == id {
                continue;
            }
            for env in versions.values() {
                env.verify()?;
                if env.payload.dependencies.iter().any(|d| &d.id == id) {
                    return Err(AssetError::PolicyViolation {
                        detail: format!(
                            "extension {} is still depended on by {}",
                            id, env.payload.id
                        ),
                    });
                }
            }
        }
        let removed: Vec<ExtensionVersion> = self
            .extensions
            .remove(id)
            .map(|m| m.into_keys().collect())
            .unwrap_or_default();
        Ok(removed)
    }

    // ---- scan status (runtime scan path only) ----

    /// Record the AgentShield verdict on a version (called by the runtime
    /// scan wrapper — never by model output).
    pub fn set_extension_security_status(
        &mut self,
        id: &ExtensionId,
        version: ExtensionVersion,
        status: &str,
    ) -> Result<(), AssetError> {
        let env = self
            .extensions
            .get_mut(id)
            .and_then(|m| m.get_mut(&version))
            .ok_or_else(|| AssetError::NotFound {
                kind: "extension.version".into(),
                id: format!("{}@{}", id.0, version.0),
            })?;
        env.verify()?;
        env.payload.security_status = status.to_string();
        env.digest = env.payload.digest();
        env.verify()
    }

    // ---- runtime build inputs ----

    /// Tool bindings of every Active extension (deduplicated by name —
    /// conflicts were already refused at activation).
    pub fn active_extension_tools(&self) -> Vec<(ExtensionId, ExtensionVersion, ExtensionTool)> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for ext in self.active_extensions() {
            for t in &ext.tools {
                if seen.insert(t.name.clone()) {
                    out.push((ext.id.clone(), ext.version, t.clone()));
                }
            }
        }
        out
    }

    /// MCP server declarations of every Active extension.
    pub fn active_extension_mcp_servers(
        &self,
    ) -> Vec<(ExtensionId, crate::extension::ExtensionMcpServer)> {
        let mut out = Vec::new();
        for ext in self.active_extensions() {
            for m in &ext.mcp_servers {
                out.push((ext.id.clone(), m.clone()));
            }
        }
        out
    }
}

// Silence unused-import lint when the constant is only used in docs.
#[allow(unused_imports)]
use CANDIDATES_NS as _;

#[cfg(test)]
mod tests {
    use crate::asset::{OperatorGrant, Provenance};
    use crate::extension::{
        Extension, ExtensionDependency, ExtensionId, ExtensionPackage, ExtensionTool,
        ExtensionVersion, SECURITY_CLEAN, SECURITY_UNSCANNED,
    };
    use crate::skill::{Skill, SkillId, SkillVersion};
    use crate::ActivationState;
    use crate::AssetStore;

    fn store() -> AssetStore {
        AssetStore::new(
            &["op".into()],
            &["skill.execute".into(), "skill.write".into()],
            false,
        )
    }

    fn skill(id: &str, v: u32) -> Skill {
        Skill::new(
            SkillId(id.into()),
            SkillVersion(v),
            "test skill",
            vec![],
            vec![],
            Provenance::new("test", "unit"),
        )
        .unwrap()
    }

    fn ext(id: &str, v: u32) -> Extension {
        Extension::new(
            ExtensionId(id.into()),
            ExtensionVersion(v),
            "name",
            "description",
            Provenance::new("test", "unit"),
        )
        .unwrap()
    }

    fn grant() -> OperatorGrant {
        OperatorGrant::new("op").unwrap()
    }

    #[test]
    fn register_coerces_to_registered_and_unscanned() {
        let mut s = store();
        let mut e = ext("ext.a", 1);
        e.activation = ActivationState::Active;
        e.security_status = SECURITY_CLEAN.into();
        s.register_extension(e).unwrap();
        let got = s
            .resolve_extension(&ExtensionId("ext.a".into()), None)
            .unwrap();
        assert_eq!(got.activation, ActivationState::Registered);
        assert_eq!(got.security_status, SECURITY_UNSCANNED);
    }

    #[test]
    fn register_rejects_duplicate_divergent_version() {
        let mut s = store();
        s.register_extension(ext("ext.a", 1)).unwrap();
        let mut dup = ext("ext.a", 1);
        dup.name = "different".into();
        assert!(s.register_extension(dup).is_err());
        // Identical re-registration is idempotent.
        s.register_extension(ext("ext.a", 1)).unwrap();
    }

    #[test]
    fn register_rejects_unknown_permission() {
        let mut s = store();
        let mut e = ext("ext.a", 1);
        e.permissions = vec!["network.http".into()];
        assert!(s.register_extension(e).is_err());
    }

    #[test]
    fn validate_requires_skill_ref_present() {
        let mut s = store();
        let mut e = ext("ext.a", 1);
        e.skill_refs = vec![crate::skill::SkillDependency {
            id: SkillId("skill.missing".into()),
            min_version: SkillVersion(1),
        }];
        s.register_extension(e).unwrap();
        assert!(s
            .validate_extension(&ExtensionId("ext.a".into()), None)
            .is_err());
    }

    #[test]
    fn install_package_installs_payload_skills_then_registers() {
        let mut s = store();
        let mut e = ext("ext.a", 1);
        e.skill_refs = vec![crate::skill::SkillDependency {
            id: SkillId("skill.pkg".into()),
            min_version: SkillVersion(1),
        }];
        let refs = e.skill_refs.clone();
        let pkg = ExtensionPackage {
            schema: crate::extension::EXTENSION_PACKAGE_SCHEMA,
            extension: e,
            skills: vec![skill("skill.pkg", 1)],
        };
        let installed = s.install_extension_package(&pkg).unwrap();
        assert_eq!(installed, vec!["skill.pkg".to_string()]);
        // Payload skill is Registered, not Active.
        assert_eq!(
            s.get_skill(&SkillId("skill.pkg".into()))
                .unwrap()
                .activation,
            ActivationState::Registered
        );
        s.validate_extension(&ExtensionId("ext.a".into()), None)
            .unwrap();
        // Divergent re-install of the same skill fails closed.
        let mut e2 = ext("ext.a", 2);
        e2.skill_refs = refs;
        let pkg2 = ExtensionPackage {
            schema: crate::extension::EXTENSION_PACKAGE_SCHEMA,
            extension: e2,
            skills: vec![skill("skill.pkg", 1)],
        };
        // Same digest: idempotent.
        s.install_extension_package(&pkg2).unwrap();
        let mut other = skill("skill.pkg", 1);
        other.description = "divergent".into();
        let pkg3 = ExtensionPackage {
            schema: crate::extension::EXTENSION_PACKAGE_SCHEMA,
            extension: ext("ext.b", 1),
            skills: vec![other],
        };
        assert!(s.install_extension_package(&pkg3).is_err());
    }

    #[test]
    fn activate_requires_clean_scan() {
        let mut s = store();
        s.register_extension(ext("ext.a", 1)).unwrap();
        s.validate_extension(&ExtensionId("ext.a".into()), None)
            .unwrap();
        // Not scanned yet.
        assert!(s
            .activate_extension(&ExtensionId("ext.a".into()), None, Some(&grant()))
            .is_err());
        s.set_extension_security_status(
            &ExtensionId("ext.a".into()),
            ExtensionVersion(1),
            SECURITY_CLEAN,
        )
        .unwrap();
        s.activate_extension(&ExtensionId("ext.a".into()), None, Some(&grant()))
            .unwrap();
        assert_eq!(
            s.active_extension_version(&ExtensionId("ext.a".into())),
            Some(ExtensionVersion(1))
        );
    }

    #[test]
    fn activate_requires_operator_grant() {
        let mut s = store();
        s.register_extension(ext("ext.a", 1)).unwrap();
        s.validate_extension(&ExtensionId("ext.a".into()), None)
            .unwrap();
        s.set_extension_security_status(
            &ExtensionId("ext.a".into()),
            ExtensionVersion(1),
            SECURITY_CLEAN,
        )
        .unwrap();
        assert!(s
            .activate_extension(&ExtensionId("ext.a".into()), None, None)
            .is_err());
    }

    #[test]
    fn activate_requires_active_dependency() {
        let mut s = store();
        s.register_extension(ext("ext.b", 1)).unwrap();
        let mut a = ext("ext.a", 1);
        a.dependencies = vec![ExtensionDependency {
            id: ExtensionId("ext.b".into()),
            min_version: ExtensionVersion(1),
        }];
        s.register_extension(a).unwrap();
        s.validate_extension(&ExtensionId("ext.a".into()), None)
            .unwrap();
        s.set_extension_security_status(
            &ExtensionId("ext.a".into()),
            ExtensionVersion(1),
            SECURITY_CLEAN,
        )
        .unwrap();
        // dep `ext.b` not active yet.
        assert!(s
            .activate_extension(&ExtensionId("ext.a".into()), None, Some(&grant()))
            .is_err());
        // Activate the dependency first.
        s.validate_extension(&ExtensionId("ext.b".into()), None)
            .unwrap();
        s.set_extension_security_status(
            &ExtensionId("ext.b".into()),
            ExtensionVersion(1),
            SECURITY_CLEAN,
        )
        .unwrap();
        s.activate_extension(&ExtensionId("ext.b".into()), None, Some(&grant()))
            .unwrap();
        s.activate_extension(&ExtensionId("ext.a".into()), None, Some(&grant()))
            .unwrap();
    }

    #[test]
    fn dependency_cycle_fails_validation() {
        let mut s = store();
        let mut a = ext("ext.a", 1);
        a.dependencies = vec![ExtensionDependency {
            id: ExtensionId("ext.b".into()),
            min_version: ExtensionVersion(1),
        }];
        let mut b = ext("ext.b", 1);
        b.dependencies = vec![ExtensionDependency {
            id: ExtensionId("ext.a".into()),
            min_version: ExtensionVersion(1),
        }];
        s.register_extension(a).unwrap();
        s.register_extension(b).unwrap();
        assert!(s
            .validate_extension(&ExtensionId("ext.a".into()), None)
            .is_err());
    }

    #[test]
    fn tool_name_conflict_between_active_extensions_refused() {
        let mut s = store();
        let mut a = ext("ext.a", 1);
        let mut t = ExtensionTool {
            name: "tool.shared".into(),
            description: String::new(),
            required_caps: vec![],
            skill: SkillId("skill.one".into()),
            min_version: SkillVersion(0),
            parameters_schema: serde_json::json!({}),
        };
        t.skill = SkillId("skill.one".into());
        a.tools = vec![t.clone()];
        a.skill_refs = vec![crate::skill::SkillDependency {
            id: SkillId("skill.one".into()),
            min_version: SkillVersion(1),
        }];
        s.register_skill(skill("skill.one", 1)).unwrap();
        s.validate_skill(&SkillId("skill.one".into())).unwrap();
        s.activate_skill(&SkillId("skill.one".into()), Some(&grant()))
            .unwrap();
        s.register_extension(a).unwrap();
        s.validate_extension(&ExtensionId("ext.a".into()), None)
            .unwrap();
        s.set_extension_security_status(
            &ExtensionId("ext.a".into()),
            ExtensionVersion(1),
            SECURITY_CLEAN,
        )
        .unwrap();
        s.activate_extension(&ExtensionId("ext.a".into()), None, Some(&grant()))
            .unwrap();

        let mut b = ext("ext.b", 1);
        b.tools = vec![t];
        b.skill_refs = vec![crate::skill::SkillDependency {
            id: SkillId("skill.one".into()),
            min_version: SkillVersion(1),
        }];
        s.register_extension(b).unwrap();
        assert!(s
            .validate_extension(&ExtensionId("ext.b".into()), None)
            .is_err());
    }

    #[test]
    fn rollback_switches_single_active_version_and_preserves_history() {
        let mut s = store();
        for v in 1..=2u32 {
            s.register_extension(ext("ext.a", v)).unwrap();
            s.validate_extension(&ExtensionId("ext.a".into()), Some(ExtensionVersion(v)))
                .unwrap();
            s.set_extension_security_status(
                &ExtensionId("ext.a".into()),
                ExtensionVersion(v),
                SECURITY_CLEAN,
            )
            .unwrap();
        }
        s.activate_extension(
            &ExtensionId("ext.a".into()),
            Some(ExtensionVersion(2)),
            Some(&grant()),
        )
        .unwrap();
        let (from, to) = s
            .rollback_extension(&ExtensionId("ext.a".into()), None, Some(&grant()))
            .unwrap();
        assert_eq!(from, ExtensionVersion(2));
        assert_eq!(to, ExtensionVersion(1));
        assert_eq!(
            s.active_extension_version(&ExtensionId("ext.a".into())),
            Some(ExtensionVersion(1))
        );
        // History preserved: both versions still stored.
        assert_eq!(
            s.extension_versions(&ExtensionId("ext.a".into())).unwrap(),
            vec![ExtensionVersion(1), ExtensionVersion(2)]
        );
        // v2 was demoted to Inactive.
        let v2 = s
            .resolve_extension(&ExtensionId("ext.a".into()), Some(ExtensionVersion(2)))
            .unwrap();
        assert_eq!(v2.activation, ActivationState::Inactive);
    }

    #[test]
    fn remove_refused_while_active_or_depended_on() {
        let mut s = store();
        s.register_extension(ext("ext.a", 1)).unwrap();
        s.validate_extension(&ExtensionId("ext.a".into()), None)
            .unwrap();
        s.set_extension_security_status(
            &ExtensionId("ext.a".into()),
            ExtensionVersion(1),
            SECURITY_CLEAN,
        )
        .unwrap();
        s.activate_extension(&ExtensionId("ext.a".into()), None, Some(&grant()))
            .unwrap();
        assert!(s
            .remove_extension(&ExtensionId("ext.a".into()), Some(&grant()))
            .is_err());
        s.deactivate_extension(&ExtensionId("ext.a".into()), Some(&grant()))
            .unwrap();
        // Now depended on by ext.b.
        let mut b = ext("ext.b", 1);
        b.dependencies = vec![ExtensionDependency {
            id: ExtensionId("ext.a".into()),
            min_version: ExtensionVersion(1),
        }];
        s.register_extension(b).unwrap();
        assert!(s
            .remove_extension(&ExtensionId("ext.a".into()), Some(&grant()))
            .is_err());
        s.remove_extension(&ExtensionId("ext.b".into()), Some(&grant()))
            .unwrap();
        let removed = s
            .remove_extension(&ExtensionId("ext.a".into()), Some(&grant()))
            .unwrap();
        assert_eq!(removed, vec![ExtensionVersion(1)]);
        assert!(s.extension_versions(&ExtensionId("ext.a".into())).is_err());
    }

    #[test]
    fn rollback_to_missing_version_fails_closed() {
        let mut s = store();
        s.register_extension(ext("ext.a", 1)).unwrap();
        assert!(s
            .rollback_extension(
                &ExtensionId("ext.a".into()),
                Some(ExtensionVersion(9)),
                Some(&grant())
            )
            .is_err());
        assert!(s
            .rollback_extension(&ExtensionId("ext.a".into()), None, Some(&grant()))
            .is_err());
    }

    #[test]
    fn tool_bindings_and_mcp_servers_collected_from_active_only() {
        let mut s = store();
        let mut e = ext("ext.a", 1);
        e.tools = vec![ExtensionTool {
            name: "tool.a".into(),
            description: String::new(),
            required_caps: vec![],
            skill: SkillId("skill.one".into()),
            min_version: SkillVersion(1),
            parameters_schema: serde_json::json!({}),
        }];
        e.skill_refs = vec![crate::skill::SkillDependency {
            id: SkillId("skill.one".into()),
            min_version: SkillVersion(1),
        }];
        e.mcp_servers = vec![crate::extension::ExtensionMcpServer {
            name: "srv".into(),
            command: "echo".into(),
            args: vec![],
            declared_capabilities: vec![],
            trust: String::new(),
        }];
        s.register_skill(skill("skill.one", 1)).unwrap();
        s.validate_skill(&SkillId("skill.one".into())).unwrap();
        s.activate_skill(&SkillId("skill.one".into()), Some(&grant()))
            .unwrap();
        s.register_extension(e).unwrap();
        assert!(s.active_extension_tools().is_empty());
        assert!(s.active_extension_mcp_servers().is_empty());
        s.validate_extension(&ExtensionId("ext.a".into()), None)
            .unwrap();
        s.set_extension_security_status(
            &ExtensionId("ext.a".into()),
            ExtensionVersion(1),
            SECURITY_CLEAN,
        )
        .unwrap();
        s.activate_extension(&ExtensionId("ext.a".into()), None, Some(&grant()))
            .unwrap();
        assert_eq!(s.active_extension_tools().len(), 1);
        assert_eq!(s.active_extension_mcp_servers().len(), 1);
    }
}
