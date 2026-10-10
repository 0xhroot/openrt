//! Shared prompt-injection / evidence-trust vocabulary (Phase 9, M9.9).
//!
//! **Types only** — this module exists so the three M9.9 surfaces share ONE
//! trust model without a dependency cycle (`openrt-core` is the workspace's
//! dependency-inversion anchor: free of sibling-crate dependencies):
//!
//! * the DETECTOR + `knowledge.evidence` trust records live in
//!   `openrt-evidence`;
//! * the hardened context builder lives in `openrt-retrieval`;
//! * answer-path enforcement (incl. chained-audit recording) lives in
//!   `openrt-runtime`.

use serde::{Deserialize, Serialize};

/// Trust label carried on evidence. `Untrusted` is the DEFAULT for every
/// ingested/retrieved document; `OperatorTrusted` only ever comes from an
/// explicit operator action — never from the document's own content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLabel {
    #[default]
    Untrusted,
    OperatorTrusted,
}

impl TrustLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustLabel::Untrusted => "untrusted",
            TrustLabel::OperatorTrusted => "operator_trusted",
        }
    }
}

/// Prompt-injection detection categories (M9.9/TEST 9). Deterministic
/// pattern classes — never model-generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionCategory {
    /// "ignore previous instructions", "you are now…", system-prompt
    /// override bait inside document text.
    InstructionOverride,
    /// Bait to exfiltrate data or credentials ("send your API key…",
    /// "forward the contents of…").
    ExfiltrationBait,
    /// Fake tool-call / function-call syntax embedded as document text
    /// (a document cannot invoke tools).
    ToolCallsAsText,
    /// Fake role markers ("SYSTEM:", "ASSISTANT:", `<|system|>`) inside
    /// document text — the document is not the operator.
    RoleConfusion,
    /// Bait to leak the operator/system instructions ("reveal your system
    /// prompt", "repeat your instructions").
    PromptExtraction,
}

impl InjectionCategory {
    /// All categories, in stable order (records/audit lists are sorted by it).
    pub const ALL: [InjectionCategory; 5] = [
        InjectionCategory::InstructionOverride,
        InjectionCategory::ExfiltrationBait,
        InjectionCategory::ToolCallsAsText,
        InjectionCategory::RoleConfusion,
        InjectionCategory::PromptExtraction,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            InjectionCategory::InstructionOverride => "instruction_override",
            InjectionCategory::ExfiltrationBait => "exfiltration_bait",
            InjectionCategory::ToolCallsAsText => "tool_calls_as_text",
            InjectionCategory::RoleConfusion => "role_confusion",
            InjectionCategory::PromptExtraction => "prompt_extraction",
        }
    }
}

/// One detected pattern in UNTRUSTED text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityFlag {
    pub category: InjectionCategory,
    /// Byte offset of the first matched byte (always a char boundary —
    /// detectors match ASCII needles only).
    pub offset: usize,
    /// Bounded excerpt around the match, for review/records (never used as
    /// an instruction, and never sent anywhere off-box).
    pub excerpt: String,
}

/// Deterministic detection outcome for one text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectionReport {
    /// Every match found (bounded by the detector's per-text cap).
    pub flags: Vec<SecurityFlag>,
    /// Sorted, deduplicated categories (stable for records and audit).
    pub categories: Vec<InjectionCategory>,
    /// Characters actually scanned (the detector is bounded).
    pub checked_chars: usize,
}

impl InjectionReport {
    /// True when no pattern matched (never assumed — always computed).
    pub fn is_clean(&self) -> bool {
        self.flags.is_empty()
    }

    /// Category names as stable strings (audit ext / CLI display).
    pub fn category_names(&self) -> Vec<&'static str> {
        self.categories.iter().map(|c| c.as_str()).collect()
    }
}
