use std::collections::BTreeMap;

use openrt_workspace_assets::{
    ensure_path_safe_id, sha256_hex, sorted_join, CanonicalForm, Provenance,
};
use serde::{Deserialize, Serialize};

use crate::baseline::map_asset;
use crate::ImproveError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningKind {
    InstinctCandidate,
    SkillSuggestion,
    WorkflowPattern,
}

impl LearningKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LearningKind::InstinctCandidate => "instinct_candidate",
            LearningKind::SkillSuggestion => "skill_suggestion",
            LearningKind::WorkflowPattern => "workflow_pattern",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub success: bool,
    pub tool_counts: BTreeMap<String, u64>,
    pub failure_signature: Option<String>,
}

impl RunSummary {
    pub fn new(run_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            success: false,
            tool_counts: BTreeMap::new(),
            failure_signature: None,
        }
    }

    pub fn with_success(mut self, success: bool) -> Self {
        self.success = success;
        self
    }

    pub fn with_tool(mut self, tool: &str, count: u64) -> Self {
        self.tool_counts.insert(tool.to_string(), count);
        self
    }

    pub fn with_failure_signature(mut self, sig: &str) -> Self {
        self.failure_signature = Some(sig.to_string());
        self
    }

    fn total_calls(&self) -> u64 {
        self.tool_counts.values().sum()
    }

    fn top_tool(&self) -> Option<(&String, &u64)> {
        self.tool_counts
            .iter()
            .max_by(|(name_a, count_a), (name_b, count_b)| {
                count_a.cmp(count_b).then(name_b.cmp(name_a))
            })
            .map(|(name, count)| (name, count))
    }

    fn evidence(&self) -> String {
        let mut parts = vec![
            format!("run_id={}", self.run_id),
            format!("success={}", self.success),
        ];
        for (tool, count) in &self.tool_counts {
            parts.push(format!("{tool}={count}"));
        }
        if let Some(sig) = &self.failure_signature {
            parts.push(format!("sig={sig}"));
        }
        sha256_hex(sorted_join(&parts).as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningRecord {
    pub id: String,
    pub run_id: String,
    pub kind: LearningKind,
    pub summary: String,
    pub evidence: Vec<String>,
    pub provenance: Provenance,
}

impl CanonicalForm for LearningRecord {
    fn asset_kind(&self) -> &'static str {
        "learning"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("kind".into(), "learning".into()),
            ("id".into(), self.id.clone()),
            ("run_id".into(), self.run_id.clone()),
            ("learning_kind".into(), self.kind.as_str().into()),
            ("summary".into(), self.summary.clone()),
            ("evidence".into(), sorted_join(&self.evidence)),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
        ];
        pairs.sort();
        pairs
    }
}

pub struct LearningEngine;

impl LearningEngine {
    pub fn derive(
        summary: &RunSummary,
        provenance: Provenance,
    ) -> Result<Vec<LearningRecord>, ImproveError> {
        provenance.validate().map_err(map_asset)?;
        let evidence = vec![summary.evidence()];
        let mut out = Vec::new();

        if summary.success && summary.total_calls() >= 4 {
            let mut tools: Vec<String> = summary
                .tool_counts
                .iter()
                .map(|(t, c)| format!("{t}:{c}"))
                .collect();
            tools.sort();
            out.push(Self::record(
                summary,
                LearningKind::WorkflowPattern,
                format!(
                    "successful run {} produced reusable workflow over tools [{}]",
                    summary.run_id,
                    tools.join(", ")
                ),
                evidence.clone(),
                provenance.clone(),
            ));
        }

        if let Some((tool, count)) = summary.top_tool() {
            if *count >= 3 {
                out.push(Self::record(
                    summary,
                    LearningKind::InstinctCandidate,
                    format!(
                        "prefer {tool} for similar tasks (used {count} times in {})",
                        summary.run_id
                    ),
                    evidence.clone(),
                    provenance.clone(),
                ));
            }
        }

        if !summary.success {
            if let Some(sig) = &summary.failure_signature {
                out.push(Self::record(
                    summary,
                    LearningKind::SkillSuggestion,
                    format!(
                        "diagnose failure signature {sig} before retrying pattern of {}",
                        summary.run_id
                    ),
                    evidence.clone(),
                    provenance.clone(),
                ));
            }
        }

        Ok(out)
    }

    fn record(
        summary: &RunSummary,
        kind: LearningKind,
        text: String,
        evidence: Vec<String>,
        provenance: Provenance,
    ) -> LearningRecord {
        let hash = sha256_hex(format!("{}|{}|{}", summary.run_id, kind.as_str(), text).as_bytes());
        let id = format!("learn.{}.{}", kind.as_str(), &hash[..16]);
        let _ = ensure_path_safe_id(&id);
        LearningRecord {
            id,
            run_id: summary.run_id.clone(),
            kind,
            summary: text,
            evidence,
            provenance,
        }
    }
}
