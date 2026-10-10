use serde::{Deserialize, Serialize};

use crate::asset::{sorted_join, CanonicalForm, Provenance};
use crate::canonical::ensure_path_safe_id;
use crate::AssetError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub name: String,
    pub tool: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub version: u32,
    pub steps: Vec<WorkflowStep>,
    pub dependencies: Vec<(String, u32)>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub permissions: Vec<String>,
    pub provenance: Provenance,
    pub validated: bool,
}

impl Workflow {
    pub fn new(
        id: &str,
        version: u32,
        steps: Vec<WorkflowStep>,
        dependencies: Vec<(String, u32)>,
        inputs: Vec<String>,
        outputs: Vec<String>,
        permissions: Vec<String>,
        provenance: Provenance,
    ) -> Result<Self, AssetError> {
        ensure_path_safe_id(id)?;
        provenance.validate()?;
        if steps.is_empty() {
            return Err(AssetError::Malformed {
                detail: "workflow must have at least one step".into(),
            });
        }
        for s in &steps {
            if s.name.trim().is_empty() || s.tool.trim().is_empty() {
                return Err(AssetError::Malformed {
                    detail: "workflow step name/tool must not be empty".into(),
                });
            }
        }
        for p in &permissions {
            ensure_path_safe_id(p)?;
        }
        for (dep, _) in &dependencies {
            ensure_path_safe_id(dep)?;
        }
        Ok(Self {
            id: id.to_string(),
            version,
            steps,
            dependencies,
            inputs,
            outputs,
            permissions,
            provenance,
            validated: false,
        })
    }

    pub fn step_names(&self) -> Vec<String> {
        self.steps.iter().map(|s| s.name.clone()).collect()
    }
}

impl CanonicalForm for Workflow {
    fn asset_kind(&self) -> &'static str {
        "workflow"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let steps: Vec<String> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}.{}:{}", i, s.name, s.tool))
            .collect();
        let deps: Vec<String> = self
            .dependencies
            .iter()
            .map(|(d, v)| format!("{}@{}", d, v))
            .collect();
        vec![
            ("kind".into(), "workflow".into()),
            ("id".into(), self.id.clone()),
            ("version".into(), self.version.to_string()),
            ("steps".into(), steps.join(",")),
            ("dependencies".into(), sorted_join(&deps)),
            ("inputs".into(), sorted_join(&self.inputs)),
            ("outputs".into(), sorted_join(&self.outputs)),
            ("permissions".into(), sorted_join(&self.permissions)),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
            ("validated".into(), self.validated.to_string()),
        ]
    }
}
