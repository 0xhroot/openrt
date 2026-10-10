use serde::{Deserialize, Serialize};

use crate::asset::{ActivationState, CanonicalForm, Provenance};
use crate::canonical::{ensure_path_safe_id, sha256_hex};
use crate::AssetError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InstinctId(pub String);

impl std::fmt::Display for InstinctId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// M10.7 instinct schema: trigger (what fires it), recommendation (what to
/// do), confidence (0.0..=1.0), scope (where it applies), evidence (the
/// history rows that justify it). All fields are additive with legacy
/// defaults; instincts never grant permissions — they are consulted context
/// only (bounded influence through the active-genome run path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instinct {
    pub id: InstinctId,
    pub version: u32,
    pub pattern: String,
    pub weight: u32,
    pub provenance: Provenance,
    pub activation: ActivationState,
    /// What fires this instinct (e.g. the observed tool name).
    #[serde(default)]
    pub trigger: String,
    /// What the instinct recommends doing.
    #[serde(default)]
    pub recommendation: String,
    /// Confidence in 0.0..=1.0 (0.0 = unspecified/legacy).
    #[serde(default)]
    pub confidence: f64,
    /// Where the instinct applies (e.g. `session`, `task`).
    #[serde(default)]
    pub scope: String,
    /// History rows backing the instinct (provenance that survives review).
    #[serde(default)]
    pub evidence: Vec<String>,
}

impl Instinct {
    pub fn new(
        id: InstinctId,
        version: u32,
        pattern: &str,
        weight: u32,
        provenance: Provenance,
    ) -> Result<Self, AssetError> {
        ensure_path_safe_id(&id.0)?;
        provenance.validate()?;
        if pattern.trim().is_empty() {
            return Err(AssetError::Malformed {
                detail: "instinct pattern must not be empty".into(),
            });
        }
        if weight > 100 {
            return Err(AssetError::Malformed {
                detail: "instinct weight must be 0..=100".into(),
            });
        }
        Ok(Self {
            id,
            version,
            pattern: pattern.to_string(),
            weight,
            provenance,
            activation: ActivationState::Registered,
            trigger: String::new(),
            recommendation: String::new(),
            confidence: 0.0,
            scope: String::new(),
            evidence: Vec::new(),
        })
    }

    /// Structural validation of the extended schema (fail-closed): confidence
    /// must be a real value in range, scope must be path-safe when set.
    pub fn validate_schema(&self) -> Result<(), AssetError> {
        if self.confidence.is_nan() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(AssetError::Malformed {
                detail: format!(
                    "instinct {} confidence {} must be in 0.0..=1.0",
                    self.id, self.confidence
                ),
            });
        }
        if !self.scope.is_empty() {
            ensure_path_safe_id(&self.scope)?;
        }
        Ok(())
    }
}

impl CanonicalForm for Instinct {
    fn asset_kind(&self) -> &'static str {
        "instinct"
    }

    fn asset_id(&self) -> String {
        self.id.0.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("kind".into(), "instinct".into()),
            ("id".into(), self.id.0.clone()),
            ("version".into(), self.version.to_string()),
            ("pattern".into(), self.pattern.clone()),
            ("weight".into(), self.weight.to_string()),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
            ("activation".into(), self.activation.as_str().into()),
        ];
        // M10.7 schema fields join the canonical identity ONLY when set —
        // legacy pre-schema records keep hashing to their original digest.
        if !self.trigger.is_empty() {
            pairs.push(("trigger".into(), self.trigger.clone()));
        }
        if !self.recommendation.is_empty() {
            pairs.push(("recommendation".into(), self.recommendation.clone()));
        }
        if self.confidence != 0.0 {
            pairs.push(("confidence".into(), format!("{:.3}", self.confidence)));
        }
        if !self.scope.is_empty() {
            pairs.push(("scope".into(), self.scope.clone()));
        }
        if !self.evidence.is_empty() {
            let mut ev = self.evidence.clone();
            ev.sort();
            pairs.push(("evidence".into(), ev.join(",")));
        }
        pairs
    }
}

/// History-derived instinct extraction (M10.7): the top observed tool seeds
/// the trigger/recommendation, confidence is the observed share of calls,
/// and the raw counts become durable evidence. Empty or zero evidence is
/// rejected — extraction never invents provenance.
pub struct InstinctGenerator;

impl InstinctGenerator {
    pub fn from_history(
        evidence: &[(String, u64)],
        provenance: Provenance,
    ) -> Result<Instinct, AssetError> {
        if evidence.is_empty() {
            return Err(AssetError::Malformed {
                detail: "history-derived instinct requires non-empty evidence".into(),
            });
        }
        if evidence
            .iter()
            .any(|(tool, n)| tool.trim().is_empty() || *n == 0)
        {
            return Err(AssetError::Malformed {
                detail: "instinct evidence entries must be non-zero tool counts".into(),
            });
        }
        // Deterministic ranking: highest count first, name order as tiebreak.
        let mut rows: Vec<&(String, u64)> = evidence.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let (top_tool, top_count) = rows[0];
        let total: u64 = evidence.iter().map(|(_, n)| *n).sum();
        let confidence = (((*top_count as f64 / total as f64) * 1000.0).round()) / 1000.0;
        let weight = (confidence * 100.0).round() as u32;

        let mut seed_rows: Vec<String> = evidence.iter().map(|(t, n)| format!("{t}:{n}")).collect();
        seed_rows.sort();
        let seed = format!("instinct|{}", seed_rows.join(","));
        let id = format!("instinct.{}", &sha256_hex(seed.as_bytes())[..16]);

        let mut instinct = Instinct::new(
            InstinctId(id),
            1,
            &format!("prefer {top_tool} for similar tasks"),
            weight.min(100),
            provenance,
        )?;
        instinct.trigger = top_tool.clone();
        instinct.recommendation = format!("prefer `{top_tool}` for similar tasks");
        instinct.confidence = confidence;
        instinct.scope = "session".to_string();
        instinct.evidence = evidence
            .iter()
            .map(|(t, n)| format!("tool:{t}:{n}"))
            .collect();
        instinct.validate_schema()?;
        Ok(instinct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov() -> Provenance {
        Provenance::new("run.inst", "src.inst")
    }

    #[test]
    fn extraction_derives_content_evidence_and_fail_closed() {
        let ev = vec![
            ("filesystem.read".to_string(), 7u64),
            ("echo".to_string(), 2u64),
        ];
        let a = InstinctGenerator::from_history(&ev, prov()).unwrap();
        assert_eq!(a.trigger, "filesystem.read");
        assert!(a.recommendation.contains("filesystem.read"));
        assert_eq!(a.confidence, 0.778); // 7/9 rounded to 3 decimals
        assert_eq!(a.weight, 78);
        assert_eq!(a.scope, "session");
        assert_eq!(a.evidence.len(), 2);
        assert!(a.evidence.contains(&"tool:filesystem.read:7".to_string()));

        // Different evidence → different identity (no collision).
        let b = InstinctGenerator::from_history(&[("echo".into(), 1)], prov()).unwrap();
        assert_ne!(a.id, b.id);

        // Fail-closed inputs.
        assert!(InstinctGenerator::from_history(&[], prov()).is_err());
        assert!(InstinctGenerator::from_history(&[("".into(), 3)], prov()).is_err());
        assert!(InstinctGenerator::from_history(&[("echo".into(), 0)], prov()).is_err());
    }

    #[test]
    fn schema_validates_confidence_and_scope() {
        let mut i = InstinctGenerator::from_history(&[("echo".into(), 1)], prov()).unwrap();
        i.confidence = 1.5;
        assert!(i.validate_schema().is_err());
        i.confidence = f64::NAN;
        assert!(i.validate_schema().is_err());
        i.confidence = 0.5;
        i.scope = "Bad Scope".into();
        assert!(i.validate_schema().is_err());
        i.scope = "session".into();
        i.validate_schema().unwrap();
    }

    #[test]
    fn legacy_record_keeps_digest_and_new_fields_join_identity() {
        // Legacy shape: original fields only.
        let legacy = serde_json::json!({
            "id": "instinct.legacy",
            "version": 1,
            "pattern": "old pattern",
            "weight": 10,
            "provenance": { "origin": "run.legacy", "source": "src.legacy" },
            "activation": "registered"
        });
        let loaded: Instinct = serde_json::from_value(legacy).unwrap();
        let digest = loaded.digest();
        assert_eq!(loaded.confidence, 0.0);
        assert_eq!(loaded.trigger, "");
        loaded.validate_schema().unwrap();
        // Resealing the legacy form yields the same digest (conditional pairs).
        assert_eq!(Instinct::clone(&loaded).digest(), digest);

        // Setting a schema field changes the identity.
        let mut enriched = loaded.clone();
        enriched.trigger = "echo".into();
        assert_ne!(enriched.digest(), digest);
    }
}
