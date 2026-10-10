use serde::{Deserialize, Serialize};

use crate::asset::{ActivationState, CanonicalForm, Provenance};
use crate::canonical::ensure_path_safe_id;
use crate::AssetError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HookId(pub String);

impl std::fmt::Display for HookId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTarget {
    SessionCreated,
    SessionEnded,
    RunStarted,
    RunCompleted,
    TaskStarted,
    TaskCompleted,
    AssetActivated,
    /// M10.7 lifecycle: fired once a workflow run starts.
    WorkflowStarted,
    /// M10.7 lifecycle: fired when a workflow run reaches its report.
    WorkflowCompleted,
    /// Phase 11: fired when a workflow run stops at an operator pause —
    /// partial history preserved, explicitly resumable (never auto).
    WorkflowPaused,
    /// M10.7 lifecycle: fired after an operator activates a skill.
    SkillActivated,
    /// M10.7 lifecycle: fired when an asset package is exported.
    ArtifactCreated,
    /// M10.7 lifecycle: fired by the gate for every tool-call decision
    /// (allowed or denied — one observation point at the enforcement edge).
    ToolCall,
}

impl HookTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookTarget::SessionCreated => "session_created",
            HookTarget::SessionEnded => "session_ended",
            HookTarget::RunStarted => "run_started",
            HookTarget::RunCompleted => "run_completed",
            HookTarget::TaskStarted => "task_started",
            HookTarget::TaskCompleted => "task_completed",
            HookTarget::AssetActivated => "asset_activated",
            HookTarget::WorkflowStarted => "workflow_started",
            HookTarget::WorkflowCompleted => "workflow_completed",
            HookTarget::WorkflowPaused => "workflow_paused",
            HookTarget::SkillActivated => "skill_activated",
            HookTarget::ArtifactCreated => "artifact_created",
            HookTarget::ToolCall => "tool_call",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hook {
    pub id: HookId,
    pub version: u32,
    pub target: HookTarget,
    pub description: String,
    pub requires_permission: Option<String>,
    pub provenance: Provenance,
    pub activation: ActivationState,
}

impl Hook {
    pub fn new(
        id: HookId,
        version: u32,
        target: HookTarget,
        description: &str,
        requires_permission: Option<String>,
        provenance: Provenance,
    ) -> Result<Self, AssetError> {
        ensure_path_safe_id(&id.0).map_err(|e| AssetError::InvalidHook {
            detail: e.to_string(),
        })?;
        provenance.validate()?;
        if description.trim().is_empty() {
            return Err(AssetError::InvalidHook {
                detail: "hook description must not be empty".into(),
            });
        }
        if let Some(p) = &requires_permission {
            ensure_path_safe_id(p).map_err(|e| AssetError::InvalidHook {
                detail: e.to_string(),
            })?;
        }
        Ok(Self {
            id,
            version,
            target,
            description: description.to_string(),
            requires_permission,
            provenance,
            activation: ActivationState::Registered,
        })
    }
}

impl CanonicalForm for Hook {
    fn asset_kind(&self) -> &'static str {
        "hook"
    }

    fn asset_id(&self) -> String {
        self.id.0.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        vec![
            ("kind".into(), "hook".into()),
            ("id".into(), self.id.0.clone()),
            ("version".into(), self.version.to_string()),
            ("target".into(), self.target.as_str().into()),
            ("description".into(), self.description.clone()),
            (
                "requires_permission".into(),
                self.requires_permission.clone().unwrap_or_default(),
            ),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
            ("activation".into(), self.activation.as_str().into()),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookOutcome {
    pub hook: String,
    pub target: String,
    pub fired: bool,
}
