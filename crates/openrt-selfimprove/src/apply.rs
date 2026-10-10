use serde::{Deserialize, Serialize};

use crate::proposal::{ImprovementProposal, TargetKind};
use crate::ImproveError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedChange {
    pub proposal_id: String,
    pub target_kind: String,
    pub target_id: String,
    pub digest: String,
}

pub trait ApplyTarget {
    fn kind(&self) -> TargetKind;
    fn id(&self) -> &str;
    fn apply(&self, proposal: &ImprovementProposal) -> Result<(), ImproveError>;
}
