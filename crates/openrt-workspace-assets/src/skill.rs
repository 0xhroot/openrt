use serde::{Deserialize, Serialize};

use crate::asset::{sorted_join, ActivationState, CanonicalForm, Provenance};
use crate::canonical::ensure_path_safe_id;
use crate::AssetError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SkillId(pub String);

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SkillVersion(pub u32);

impl std::fmt::Display for SkillVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDependency {
    pub id: SkillId,
    pub min_version: SkillVersion,
}

/// Engine/toolchain compatibility declared by a skill manifest (M10.6).
/// Empty fields mean "unspecified" — never an implicit grant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCompat {
    /// Minimum openrt version the skill supports ("" = unspecified).
    #[serde(default)]
    pub openrt_min: String,
    /// Engine/toolchain capability ids the skill requires (empty = none).
    #[serde(default)]
    pub engines: Vec<String>,
}

/// Trust labels a skill manifest may declare. Anything else fails closed at
/// registration; "" is tolerated only as the legacy/unspecified value and is
/// treated exactly like `unvetted` by every consumer.
pub const TRUST_UNVETTED: &str = "unvetted";
pub const TRUST_VETTED: &str = "vetted";
pub const TRUST_SIGNED: &str = "signed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub version: SkillVersion,
    pub description: String,
    pub permissions: Vec<String>,
    pub dependencies: Vec<SkillDependency>,
    pub provenance: Provenance,
    pub activation: ActivationState,
    /// Display name ("" = fall back to the id).
    #[serde(default)]
    pub name: String,
    /// What the skill is for (free text; empty = unspecified).
    #[serde(default)]
    pub goal: String,
    /// Workflow id this skill compiles to / executes (None = ad-hoc).
    #[serde(default)]
    pub workflow: Option<String>,
    /// ToolRegistry tool names the skill may use.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Model ids the skill may call.
    #[serde(default)]
    pub models: Vec<String>,
    /// Sandbox profile the skill's execution must run under (None = policy default).
    #[serde(default)]
    pub sandbox: Option<String>,
    /// Trust label — `""` (legacy) is equivalent to `unvetted`.
    #[serde(default)]
    pub trust: String,
    /// Engine compatibility constraints.
    #[serde(default)]
    pub compat: SkillCompat,
    /// Skill test ids (assets.skill_tests) run at validation/CI time.
    #[serde(default)]
    pub tests: Vec<String>,
}

impl Skill {
    pub fn new(
        id: SkillId,
        version: SkillVersion,
        description: &str,
        permissions: Vec<String>,
        dependencies: Vec<SkillDependency>,
        provenance: Provenance,
    ) -> Result<Self, AssetError> {
        ensure_path_safe_id(&id.0)?;
        provenance.validate()?;
        if description.trim().is_empty() {
            return Err(AssetError::Malformed {
                detail: "skill description must not be empty".into(),
            });
        }
        for p in &permissions {
            ensure_path_safe_id(p)?;
        }
        Ok(Self {
            id,
            version,
            description: description.to_string(),
            permissions,
            dependencies,
            provenance,
            activation: ActivationState::Registered,
            name: String::new(),
            goal: String::new(),
            workflow: None,
            tools: Vec::new(),
            models: Vec::new(),
            sandbox: None,
            // "" ≡ unvetted (trust_label) and keeps default-constructed skills
            // on the legacy canonical form until a manifest is declared.
            trust: String::new(),
            compat: SkillCompat::default(),
            tests: Vec::new(),
        })
    }

    /// Effective trust label: legacy "" collapses to `unvetted`.
    pub fn trust_label(&self) -> &str {
        if self.trust.is_empty() {
            TRUST_UNVETTED
        } else {
            &self.trust
        }
    }

    /// Display name with id fallback.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id.0
        } else {
            &self.name
        }
    }

    /// Manifest structure validation (M10.6): referenced ids/path-like fields
    /// must match `[a-z0-9._-]`, trust must be a known label, compatibility
    /// strings must be version-ish. Called on register/import/load — fail-closed.
    pub fn validate_manifest(&self) -> Result<(), AssetError> {
        if let Some(wf) = &self.workflow {
            ensure_path_safe_id(wf)?;
        }
        if let Some(sb) = &self.sandbox {
            ensure_path_safe_id(sb)?;
        }
        for t in self
            .tools
            .iter()
            .chain(self.models.iter())
            .chain(self.tests.iter())
        {
            ensure_path_safe_id(t)?;
        }
        if !self.compat.openrt_min.is_empty()
            && !self
                .compat
                .openrt_min
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        {
            return Err(AssetError::Malformed {
                detail: format!(
                    "skill {} compat.openrt_min {:?} must be version-like [A-Za-z0-9._-]",
                    self.id, self.compat.openrt_min
                ),
            });
        }
        for e in &self.compat.engines {
            ensure_path_safe_id(e)?;
        }
        match self.trust.as_str() {
            "" | TRUST_UNVETTED | TRUST_VETTED | TRUST_SIGNED => Ok(()),
            other => Err(AssetError::Malformed {
                detail: format!(
                    "skill {} declares unknown trust label {:?} (allowed: {TRUST_UNVETTED}, {TRUST_VETTED}, {TRUST_SIGNED})",
                    self.id, other
                ),
            }),
        }
    }

    pub fn dep_key(dep: &SkillDependency) -> String {
        format!("{}@{}", dep.id.0, dep.min_version.0)
    }
}

impl CanonicalForm for Skill {
    fn asset_kind(&self) -> &'static str {
        "skill"
    }

    fn asset_id(&self) -> String {
        self.id.0.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let deps: Vec<String> = self.dependencies.iter().map(Skill::dep_key).collect();
        let mut pairs = vec![
            ("kind".into(), "skill".into()),
            ("id".into(), self.id.0.clone()),
            ("version".into(), self.version.0.to_string()),
            ("description".into(), self.description.clone()),
            ("permissions".into(), sorted_join(&self.permissions)),
            ("dependencies".into(), sorted_join(&deps)),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
            ("activation".into(), self.activation.as_str().into()),
        ];
        // M10.6 manifest fields join the canonical identity — but ONLY when
        // set: legacy pre-manifest records (all fields absent) keep hashing to
        // exactly the digest they were sealed with, so upgrades never break
        // stored envelopes.
        if !self.name.is_empty() {
            pairs.push(("name".into(), self.name.clone()));
        }
        if !self.goal.is_empty() {
            pairs.push(("goal".into(), self.goal.clone()));
        }
        if let Some(wf) = &self.workflow {
            pairs.push(("workflow".into(), wf.clone()));
        }
        if !self.tools.is_empty() {
            pairs.push(("tools".into(), sorted_join(&self.tools)));
        }
        if !self.models.is_empty() {
            pairs.push(("models".into(), sorted_join(&self.models)));
        }
        if let Some(sb) = &self.sandbox {
            pairs.push(("sandbox".into(), sb.clone()));
        }
        if !self.trust.is_empty() {
            pairs.push(("trust".into(), self.trust.clone()));
        }
        if !self.compat.openrt_min.is_empty() || !self.compat.engines.is_empty() {
            pairs.push((
                "compat".into(),
                format!(
                    "{}|{}",
                    self.compat.openrt_min,
                    sorted_join(&self.compat.engines)
                ),
            ));
        }
        if !self.tests.is_empty() {
            pairs.push(("tests".into(), sorted_join(&self.tests)));
        }
        pairs
    }
}
