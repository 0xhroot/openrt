//! M10.6 skill tests — small declarative checks an operator (or CI) attaches
//! to a skill version and re-runs through the production loader. Tests never
//! grant permissions; they only observe loader/registry/policy verdicts.

use serde::{Deserialize, Serialize};

use crate::asset::{ActivationState, CanonicalForm, Provenance};
use crate::canonical::ensure_path_safe_id;
use crate::skill::{SkillId, SkillVersion};
use crate::AssetError;

pub const SKILL_TESTS_NS: &str = "assets.skill_tests";

/// What a skill test observes. All kinds are read-only verdict checks — a
/// test can FAIL a skill, never widen what it may do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SkillTestKind {
    /// The loader must accept the skill (all checks pass).
    Loads,
    /// The effective trust label must equal this label.
    TrustIs(String),
    /// The named tool must be registered in the runtime ToolRegistry.
    ToolRegistered(String),
    /// The named permission must be declared by the skill manifest.
    PermissionDeclared(String),
    /// The skill must be in this activation state when the test runs.
    ActivationIs(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillTest {
    pub id: String,
    pub skill_id: SkillId,
    pub skill_version: SkillVersion,
    pub name: String,
    pub kind: SkillTestKind,
    pub provenance: Provenance,
    pub activation: ActivationState,
}

impl SkillTest {
    pub fn new(
        id: &str,
        skill_id: SkillId,
        skill_version: SkillVersion,
        name: &str,
        kind: SkillTestKind,
        provenance: Provenance,
    ) -> Result<Self, AssetError> {
        ensure_path_safe_id(id)?;
        provenance.validate()?;
        if name.trim().is_empty() {
            return Err(AssetError::Malformed {
                detail: "skill test name must not be empty".into(),
            });
        }
        if let SkillTestKind::ActivationIs(state) = &kind {
            const KNOWN: &[&str] = &[
                "registered",
                "validated",
                "active",
                "inactive",
                "invalidated",
            ];
            if !KNOWN.contains(&state.as_str()) {
                return Err(AssetError::Malformed {
                    detail: format!("skill test declares unknown activation state {state:?}"),
                });
            }
        }
        Ok(Self {
            id: id.to_string(),
            skill_id,
            skill_version,
            name: name.to_string(),
            kind,
            provenance,
            activation: ActivationState::Registered,
        })
    }
}

impl CanonicalForm for SkillTest {
    fn asset_kind(&self) -> &'static str {
        "skill_test"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let kind = match &self.kind {
            SkillTestKind::Loads => "loads".to_string(),
            SkillTestKind::TrustIs(t) => format!("trust_is|{t}"),
            SkillTestKind::ToolRegistered(t) => format!("tool_registered|{t}"),
            SkillTestKind::PermissionDeclared(p) => format!("permission_declared|{p}"),
            SkillTestKind::ActivationIs(a) => format!("activation_is|{a}"),
        };
        vec![
            ("kind".into(), "skill_test".into()),
            ("id".into(), self.id.clone()),
            ("skill_id".into(), self.skill_id.0.clone()),
            ("skill_version".into(), self.skill_version.0.to_string()),
            ("name".into(), self.name.clone()),
            ("test".into(), kind),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
            ("activation".into(), self.activation.as_str().into()),
        ]
    }
}

/// Offline marketplace catalog entry (M10.6). Local entries describe skills
/// already installed; remote entries are never fetched under Sovereign/offline
/// policy — `Marketplace::remote_fetch` returns an honest UNAVAILABLE error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    pub id: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    pub trust: String,
    pub activation: String,
    pub source: String,
}

/// Honest remote-marketplace failure (M10.6): this build ships no network
/// marketplace client, and Sovereign mode forbids one — callers receive the
/// UNAVAILABLE reason verbatim instead of a fake empty catalog.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("remote marketplace UNAVAILABLE: {0}")]
pub struct RemoteMarketplaceUnavailable(pub String);

pub struct Marketplace;

impl Marketplace {
    /// Local (offline) catalog: every installed skill version, deterministic.
    pub fn local_catalog(assets: &crate::AssetStore) -> Vec<MarketplaceEntry> {
        assets
            .skill_states_all()
            .into_iter()
            .filter_map(|(id, version, activation)| {
                let sid = SkillId(id.clone());
                let skill = assets.get_skill_version(&sid, SkillVersion(version)).ok()?;
                Some(MarketplaceEntry {
                    id: id.clone(),
                    version,
                    name: skill.display_name().to_string(),
                    description: skill.description.clone(),
                    trust: skill.trust_label().to_string(),
                    activation: activation.to_string(),
                    source: "local".to_string(),
                })
            })
            .collect()
    }

    /// Remote catalogs are honestly UNAVAILABLE (no network client exists in
    /// this build; Sovereign policy would block it regardless).
    pub async fn remote_fetch(
        _url: &str,
    ) -> Result<Vec<MarketplaceEntry>, RemoteMarketplaceUnavailable> {
        Err(RemoteMarketplaceUnavailable(
            "this build contains no remote marketplace client; Sovereign policy blocks network \
             catalog fetches. Use the local catalog (`skill marketplace --local`)."
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::Provenance as AssetProvenance;

    fn prov() -> AssetProvenance {
        AssetProvenance::new("run.mt", "src.mt")
    }

    #[test]
    fn skill_test_round_trip_and_digest() {
        let t = SkillTest::new(
            "st.ok",
            SkillId("skill.x".into()),
            SkillVersion(2),
            "loads the skill",
            SkillTestKind::Loads,
            prov(),
        )
        .unwrap();
        let sealed = crate::AssetEnvelope::seal(t.clone());
        sealed.verify().unwrap();
        let mut tampered = sealed;
        tampered.payload.name = "renamed".into();
        assert!(tampered.verify().is_err());
        let json = serde_json::to_value(&t).unwrap();
        let back: SkillTest = serde_json::from_value(json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn neg_skill_test_rejects_bad_input() {
        assert!(SkillTest::new(
            "Bad Id",
            SkillId("skill.x".into()),
            SkillVersion(1),
            "n",
            SkillTestKind::Loads,
            prov(),
        )
        .is_err());
        assert!(SkillTest::new(
            "st.a",
            SkillId("skill.x".into()),
            SkillVersion(1),
            "",
            SkillTestKind::Loads,
            prov(),
        )
        .is_err());
        assert!(SkillTest::new(
            "st.a",
            SkillId("skill.x".into()),
            SkillVersion(1),
            "n",
            SkillTestKind::ActivationIs("weird".into()),
            prov(),
        )
        .is_err());
    }

    #[tokio::test]
    async fn marketplace_remote_is_honestly_unavailable() {
        let err = Marketplace::remote_fetch("https://example.invalid/catalog")
            .await
            .unwrap_err();
        assert!(err.0.contains("UNAVAILABLE") || err.to_string().contains("UNAVAILABLE"));
    }

    #[test]
    fn history_derived_skill_carries_content_not_identity() {
        // Evidence-derived content: tools come from the observed history and
        // the goal from the request — not just a seeded id.
        let cand = crate::SkillGenerator::from_history(
            "req.mt",
            "summarize reports",
            vec!["fs.read".into()],
            vec![],
            prov(),
            &[("filesystem.read".into(), 3), ("echo".into(), 1)],
        )
        .unwrap();
        let mut expected_tools = vec!["filesystem.read".to_string(), "echo".to_string()];
        expected_tools.sort();
        assert_eq!(cand.skill.tools, expected_tools);
        assert_eq!(cand.skill.goal, "summarize reports");
        // The id is still evidence-seeded (different histories → different id).
        let other = crate::SkillGenerator::from_history(
            "req.mt",
            "summarize reports",
            vec!["fs.read".into()],
            vec![],
            prov(),
            &[("filesystem.write".into(), 9)],
        )
        .unwrap();
        assert_ne!(cand.skill.id, other.skill.id);
        // Never-valid provenance: empty evidence still refused.
        assert!(
            crate::SkillGenerator::from_history("req.mt", "x", vec![], vec![], prov(), &[])
                .is_err()
        );
    }
}
