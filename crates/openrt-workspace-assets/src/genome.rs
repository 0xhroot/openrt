use serde::{Deserialize, Serialize};

use crate::asset::{CanonicalForm, Provenance};
use crate::canonical::ensure_path_safe_id;
use crate::skill::{SkillId, SkillVersion};
use crate::AssetError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GenomeId(pub String);

impl std::fmt::Display for GenomeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGenome {
    pub id: GenomeId,
    pub version: u32,
    pub skills: Vec<(SkillId, SkillVersion)>,
    pub instincts: Vec<(String, u32)>,
    pub hooks: Vec<(String, u32)>,
    pub rules: Vec<(String, u32)>,
    /// M10.8 profile: preferred model (provider id or its default model).
    #[serde(default)]
    pub model: Option<String>,
    /// M10.8 profile: base prompt / system strategy the genome declares.
    #[serde(default)]
    pub prompt: Option<String>,
    /// M10.8 profile: tool ids this genome expects to use (declarative —
    /// validated against the registry at apply, never registered by the
    /// genome itself).
    #[serde(default)]
    pub tools: Vec<String>,
    /// M10.8 profile: memory strategy label (declarative).
    #[serde(default)]
    pub memory_strategy: Option<String>,
    /// M10.8 profile: resource strategy label (declarative).
    #[serde(default)]
    pub resource_strategy: Option<String>,
    /// M10.8 profile: named verifier (a registered tool used as the
    /// genome's verification step — validated at apply).
    #[serde(default)]
    pub verifier: Option<String>,
    /// M10.8 profile: DECLARED permissions — a genome can declare but never
    /// grant permissions; each must already be allowed by the gate policy.
    #[serde(default)]
    pub permissions: Vec<String>,
    pub provenance: Provenance,
}

fn sorted_refs<T: std::fmt::Display, V: std::fmt::Display>(refs: &[(T, V)]) -> Vec<String> {
    let mut v: Vec<String> = refs
        .iter()
        .map(|(i, ver)| format!("{}@{}", i, ver))
        .collect();
    v.sort();
    v.dedup();
    v
}

fn sorted_strings(items: &[String]) -> Vec<String> {
    let mut v = items.to_vec();
    v.sort();
    v.dedup();
    v
}

impl AgentGenome {
    pub fn new(
        id: GenomeId,
        version: u32,
        skills: Vec<(SkillId, SkillVersion)>,
        instincts: Vec<(String, u32)>,
        hooks: Vec<(String, u32)>,
        rules: Vec<(String, u32)>,
        provenance: Provenance,
    ) -> Result<Self, AssetError> {
        ensure_path_safe_id(&id.0)?;
        provenance.validate()?;
        for (iid, _) in &instincts {
            ensure_path_safe_id(iid)?;
        }
        for (hid, _) in &hooks {
            ensure_path_safe_id(hid)?;
        }
        for (rid, _) in &rules {
            ensure_path_safe_id(rid)?;
        }
        Ok(Self {
            id,
            version,
            skills,
            instincts,
            hooks,
            rules,
            model: None,
            prompt: None,
            tools: Vec::new(),
            memory_strategy: None,
            resource_strategy: None,
            verifier: None,
            permissions: Vec::new(),
            provenance,
        })
    }

    /// M10.8: attach the runtime profile (model/prompt/tools/strategies/
    /// verifier/declared permissions) and re-validate the whole genome.
    pub fn with_profile(
        mut self,
        model: Option<String>,
        prompt: Option<String>,
        tools: Vec<String>,
        memory_strategy: Option<String>,
        resource_strategy: Option<String>,
        verifier: Option<String>,
        permissions: Vec<String>,
    ) -> Result<Self, AssetError> {
        self.model = model;
        self.prompt = prompt;
        self.tools = tools;
        self.memory_strategy = memory_strategy;
        self.resource_strategy = resource_strategy;
        self.verifier = verifier;
        self.permissions = permissions;
        self.validate_profile()?;
        Ok(self)
    }

    /// M10.8 fail-closed profile validation: path-safe tool/permission ids,
    /// no empty-string strategy/model labels.
    pub fn validate_profile(&self) -> Result<(), AssetError> {
        for t in &self.tools {
            ensure_path_safe_id(t)?;
        }
        for p in &self.permissions {
            ensure_path_safe_id(p)?;
        }
        for field in [
            self.model.as_ref(),
            self.prompt.as_ref(),
            self.memory_strategy.as_ref(),
            self.resource_strategy.as_ref(),
            self.verifier.as_ref(),
        ] {
            if let Some(v) = field {
                if v.trim().is_empty() {
                    return Err(AssetError::Malformed {
                        detail: format!("genome `{}` profile strings must not be blank", self.id.0),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn with_changes(
        mut self,
        skills: Vec<(SkillId, SkillVersion)>,
        instincts: Vec<(String, u32)>,
        hooks: Vec<(String, u32)>,
        rules: Vec<(String, u32)>,
    ) -> Self {
        self.skills = skills;
        self.instincts = instincts;
        self.hooks = hooks;
        self.rules = rules;
        self.version += 1;
        self
    }
}

impl CanonicalForm for AgentGenome {
    fn asset_kind(&self) -> &'static str {
        "genome"
    }

    fn asset_id(&self) -> String {
        self.id.0.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("kind".into(), "genome".into()),
            ("id".into(), self.id.0.clone()),
            ("version".into(), self.version.to_string()),
            ("skills".into(), sorted_refs(&self.skills).join(",")),
            ("instincts".into(), sorted_refs(&self.instincts).join(",")),
            ("hooks".into(), sorted_refs(&self.hooks).join(",")),
            ("rules".into(), sorted_refs(&self.rules).join(",")),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
        ];
        // M10.8 conditional profile pairs: only present when set, so legacy
        // genomes keep their digest (upgrade-safe identity).
        if let Some(m) = &self.model {
            pairs.push(("model".into(), m.clone()));
        }
        if let Some(p) = &self.prompt {
            pairs.push(("prompt".into(), p.clone()));
        }
        if !self.tools.is_empty() {
            pairs.push(("tools".into(), sorted_strings(&self.tools).join(",")));
        }
        if let Some(m) = &self.memory_strategy {
            pairs.push(("memory_strategy".into(), m.clone()));
        }
        if let Some(r) = &self.resource_strategy {
            pairs.push(("resource_strategy".into(), r.clone()));
        }
        if let Some(v) = &self.verifier {
            pairs.push(("verifier".into(), v.clone()));
        }
        if !self.permissions.is_empty() {
            pairs.push((
                "permissions".into(),
                sorted_strings(&self.permissions).join(","),
            ));
        }
        pairs
    }
}
