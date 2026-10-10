use serde::{Deserialize, Serialize};

use crate::asset::{CanonicalForm, Provenance};
use crate::canonical::{ensure_path_safe_id, sha256_hex};
use crate::skill::{Skill, SkillDependency, SkillId, SkillVersion};
use crate::AssetError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Generated,
    Validated,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCandidate {
    pub id: String,
    pub request_id: String,
    pub skill: Skill,
    pub review: ReviewState,
    pub provenance: Provenance,
}

impl CanonicalForm for SkillCandidate {
    fn asset_kind(&self) -> &'static str {
        "candidate"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let skill_digest = self.skill.digest();
        let review = match self.review {
            ReviewState::Generated => "generated",
            ReviewState::Validated => "validated",
            ReviewState::Approved => "approved",
            ReviewState::Rejected => "rejected",
        };
        vec![
            ("kind".into(), "candidate".into()),
            ("id".into(), self.id.clone()),
            ("request_id".into(), self.request_id.clone()),
            ("skill_digest".into(), skill_digest),
            ("review".into(), review.into()),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
        ]
    }
}

pub struct SkillGenerator;

impl SkillGenerator {
    /// Template-seeded generation (stable offline path). Prefer
    /// [`SkillGenerator::from_history`] whenever real execution evidence
    /// exists — the roadmap requires instincts/skills to be *demonstrably
    /// derived from execution history*.
    pub fn generate(
        request_id: &str,
        description: &str,
        permissions: Vec<String>,
        dependencies: Vec<SkillDependency>,
        provenance: Provenance,
    ) -> Result<SkillCandidate, AssetError> {
        Self::generate_inner(
            request_id,
            description,
            permissions,
            dependencies,
            provenance,
            None,
        )
    }

    /// Derive a skill candidate from REAL execution history evidence
    /// (tool usage counts / run digests from the timeseries ledger or a
    /// `RunSummary`). The seed hashes the evidence so two different
    /// histories cannot collide onto the same skill id; empty evidence is
    /// rejected (never invents provenance).
    pub fn from_history(
        request_id: &str,
        description: &str,
        permissions: Vec<String>,
        dependencies: Vec<SkillDependency>,
        provenance: Provenance,
        evidence: &[(String, u64)],
    ) -> Result<SkillCandidate, AssetError> {
        if evidence.is_empty() {
            return Err(AssetError::Malformed {
                detail: "history-derived generation requires non-empty evidence".into(),
            });
        }
        if evidence
            .iter()
            .any(|(tool, n)| tool.trim().is_empty() || *n == 0)
        {
            return Err(AssetError::Malformed {
                detail: "history evidence entries must be non-zero tool counts".into(),
            });
        }
        Self::generate_inner(
            request_id,
            description,
            permissions,
            dependencies,
            provenance,
            Some(evidence),
        )
    }

    fn generate_inner(
        request_id: &str,
        description: &str,
        permissions: Vec<String>,
        dependencies: Vec<SkillDependency>,
        provenance: Provenance,
        evidence: Option<&[(String, u64)]>,
    ) -> Result<SkillCandidate, AssetError> {
        if request_id.trim().is_empty() {
            return Err(AssetError::Malformed {
                detail: "generation request id must not be empty".into(),
            });
        }
        let mut perm_sorted = permissions.clone();
        perm_sorted.sort();
        perm_sorted.dedup();
        let mut dep_keys: Vec<String> = dependencies.iter().map(Skill::dep_key).collect();
        dep_keys.sort();
        let evidence_part = match evidence {
            None => String::new(),
            Some(ev) => {
                let mut rows: Vec<String> = ev.iter().map(|(t, n)| format!("{t}:{n}")).collect();
                rows.sort();
                format!("|hist={}", rows.join(","))
            }
        };
        let seed = format!(
            "gen|{}|{}|{}|{}{}",
            request_id,
            description,
            perm_sorted.join(","),
            dep_keys.join(","),
            evidence_part
        );
        let derived_id = format!("skill.{}", &sha256_hex(seed.as_bytes())[..16]);
        let mut skill = Skill::new(
            SkillId(derived_id),
            SkillVersion(1),
            description,
            permissions,
            dependencies,
            provenance.clone(),
        )?;
        // History-derived CONTENT, not just identity (M10.6): the observed
        // tools become the manifest's tool set and the goal carries the
        // request — two different histories produce both a different id AND
        // different content. Non-path-safe tool names can't be registry
        // names, so they never enter the manifest.
        if let Some(ev) = evidence {
            skill.goal = description.to_string();
            let mut tools: Vec<String> = ev
                .iter()
                .map(|(t, _)| t.clone())
                .filter(|t| ensure_path_safe_id(t).is_ok())
                .collect();
            tools.sort();
            tools.dedup();
            skill.tools = tools;
        }
        Ok(SkillCandidate {
            id: format!(
                "cand.{}",
                &sha256_hex(format!("cand|{}", seed).as_bytes())[..16]
            ),
            request_id: request_id.to_string(),
            skill,
            review: ReviewState::Generated,
            provenance,
        })
    }
}
