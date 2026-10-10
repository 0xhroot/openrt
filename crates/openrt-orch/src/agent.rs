//! Role sub-agents for multi-agent orchestration (M2.1).

use std::fmt;

use openrt_agents::Agent;
use serde::{Deserialize, Serialize};

/// The fixed role inventory the phase-2 roadmap assigns to sub-agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentKind {
    Planner,
    Researcher,
    Coder,
    Verifier,
    Critic,
}

impl SubAgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Researcher => "researcher",
            Self::Coder => "coder",
            Self::Verifier => "verifier",
            Self::Critic => "critic",
        }
    }

    /// Stable system-prompt template for the role. Role identity is the whole
    /// "personality" the engine injects; providers stay interchangeable.
    pub fn role_prompt(&self) -> &'static str {
        match self {
            Self::Planner => {
                "You are the planner. Break the user's goal into concrete, \
                 ordered steps and produce a short plan. Do not implement."
            }
            Self::Researcher => {
                "You are the researcher. Gather the facts and constraints needed \
                 for the plan. Report findings concisely."
            }
            Self::Coder => {
                "You are the coder. Implement the requested change from the plan, \
                 in minimal, tested code."
            }
            Self::Verifier => {
                "You are the verifier. Check the produced work against the stated \
                 requirements and report pass/fail with reasons."
            }
            Self::Critic => {
                "You are the critic. Identify risks, gaps and better alternatives \
                 in the proposed work. Be specific and constructive."
            }
        }
    }
}

impl fmt::Display for SubAgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Phase 11 M11.4: map a room role (workbench configuration) onto the
/// existing [`SubAgentKind`] identity that executes it. Roles never invent
/// new authorities — they select which already-gated sub-agent identity a
/// room member joins under. Unknown roles fail closed (`None`).
///
/// Mapping (documented in `docs/architecture/phase11/`):
/// researcher→Researcher, analyst→Planner, coder→Coder,
/// reviewer→Critic, verifier→Verifier.
pub fn room_role_sub_agent(role: &str) -> Option<SubAgentKind> {
    match role.trim().to_ascii_lowercase().as_str() {
        "researcher" => Some(SubAgentKind::Researcher),
        "analyst" => Some(SubAgentKind::Planner),
        "coder" => Some(SubAgentKind::Coder),
        "reviewer" => Some(SubAgentKind::Critic),
        "verifier" => Some(SubAgentKind::Verifier),
        _ => None,
    }
}

/// A role bound to a concrete agent implementation. The engine invokes it via
/// the shared [`Agent`] contract; the runtime decides which provider/executor
/// backs each role.
pub struct SubAgent {
    pub kind: SubAgentKind,
    pub agent: Box<dyn Agent>,
}

impl SubAgent {
    pub fn new(kind: SubAgentKind, agent: Box<dyn Agent>) -> Self {
        Self { kind, agent }
    }

    pub fn kind(&self) -> SubAgentKind {
        self.kind
    }
}

#[cfg(test)]
mod p11_room_role_tests {
    use super::*;

    /// M11.4: every configured room role maps onto exactly one existing
    /// SubAgentKind identity; unknown roles fail closed.
    #[test]
    fn room_roles_map_onto_existing_sub_agent_identities() {
        assert_eq!(
            room_role_sub_agent("researcher"),
            Some(SubAgentKind::Researcher)
        );
        assert_eq!(room_role_sub_agent("analyst"), Some(SubAgentKind::Planner));
        assert_eq!(room_role_sub_agent("coder"), Some(SubAgentKind::Coder));
        assert_eq!(room_role_sub_agent("reviewer"), Some(SubAgentKind::Critic));
        assert_eq!(
            room_role_sub_agent("verifier"),
            Some(SubAgentKind::Verifier)
        );
        // case-insensitive, whitespace-tolerant.
        assert_eq!(
            room_role_sub_agent("  VERIFIER "),
            Some(SubAgentKind::Verifier)
        );
        // unknown roles never map (fail closed — no default).
        assert_eq!(room_role_sub_agent("pirate"), None);
        assert_eq!(room_role_sub_agent(""), None);
    }
}
