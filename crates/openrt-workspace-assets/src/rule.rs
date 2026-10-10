use serde::{Deserialize, Serialize};

use crate::asset::{CanonicalForm, Provenance};
use crate::canonical::ensure_path_safe_id;
use crate::AssetError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RuleId(pub String);

impl std::fmt::Display for RuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleEffect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: RuleId,
    pub version: u32,
    pub scope: String,
    pub subject: String,
    pub effect: RuleEffect,
    pub priority: u32,
    pub enabled: bool,
    pub provenance: Provenance,
}

impl Rule {
    pub fn new(
        id: RuleId,
        version: u32,
        scope: &str,
        subject: &str,
        effect: RuleEffect,
        priority: u32,
        enabled: bool,
        provenance: Provenance,
    ) -> Result<Self, AssetError> {
        ensure_path_safe_id(&id.0)?;
        provenance.validate()?;
        if scope.trim().is_empty() {
            return Err(AssetError::Malformed {
                detail: "rule scope must not be empty".into(),
            });
        }
        if subject.trim().is_empty() {
            return Err(AssetError::Malformed {
                detail: "rule subject must not be empty".into(),
            });
        }
        Ok(Self {
            id,
            version,
            scope: scope.to_string(),
            subject: subject.to_string(),
            effect,
            priority,
            enabled,
            provenance,
        })
    }
}

impl CanonicalForm for Rule {
    fn asset_kind(&self) -> &'static str {
        "rule"
    }

    fn asset_id(&self) -> String {
        self.id.0.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let effect = match self.effect {
            RuleEffect::Allow => "allow",
            RuleEffect::Deny => "deny",
        };
        vec![
            ("kind".into(), "rule".into()),
            ("id".into(), self.id.0.clone()),
            ("version".into(), self.version.to_string()),
            ("scope".into(), self.scope.clone()),
            ("subject".into(), self.subject.clone()),
            ("effect".into(), effect.into()),
            ("priority".into(), self.priority.to_string()),
            ("enabled".into(), self.enabled.to_string()),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDecision {
    pub allowed: bool,
    pub decided_by: String,
    pub rule: Option<String>,
}
