//! Extension (plugin) manifest model (Phase 11, M11.1).
//!
//! An extension is a *package*, not code execution: it declares the skills it
//! ships (payloads install into the ONE skill authority `assets.skills`),
//! declarative skill-backed tool bindings (registered into the ONE
//! ToolRegistry only while the extension is Active), optional MCP server
//! declarations (merged into the ONE MCP manager at build), permissions,
//! sandbox/network/filesystem/model requirements, dependencies, compatibility
//! and provenance. The manifest deliberately mirrors the Phase 10 skill
//! manifest vocabulary so nothing is duplicated — extensions *reference*
//! skills, they never become a second skill registry.

use serde::{Deserialize, Serialize};

use crate::asset::{sorted_join, ActivationState, CanonicalForm, Provenance};
use crate::canonical::ensure_path_safe_id;
use crate::skill::{
    SkillDependency, SkillId, SkillVersion, TRUST_SIGNED, TRUST_UNVETTED, TRUST_VETTED,
};
use crate::AssetError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExtensionId(pub String);

impl std::fmt::Display for ExtensionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExtensionVersion(pub u32);

impl std::fmt::Display for ExtensionVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Declarative tool binding: while the extension is Active the runtime
/// registers this tool name into the one ToolRegistry; running it invokes the
/// referenced skill through the existing skill execution path (gate → policy
/// → sandbox → shield). Extensions never ship executable code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionTool {
    /// ToolRegistry tool name (path-safe `[a-z0-9._-]`).
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Capabilities the binding requires; intersected with the caller's
    /// authority at the gate — never an expansion.
    #[serde(default)]
    pub required_caps: Vec<String>,
    /// Skill this binding executes.
    pub skill: SkillId,
    /// Minimum skill version (0 = whatever resolves, never substituted down).
    #[serde(default)]
    pub min_version: SkillVersion,
    /// JSON schema for the tool arguments (empty object = free-form).
    #[serde(default)]
    pub parameters_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionDependency {
    pub id: ExtensionId,
    pub min_version: ExtensionVersion,
}

/// An MCP server an extension declares. Env *values* are never part of a
/// package (secret hygiene) — only key names, and command/args are scanned
/// by the one AgentShield before activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionMcpServer {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub declared_capabilities: Vec<String>,
    /// Declared trust intent (`""` ≡ untrusted). Trust never bypasses
    /// permissions/policy/sandbox/mode at the gate.
    #[serde(default)]
    pub trust: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionCompat {
    /// Minimum openrt version supported ("" = unspecified).
    #[serde(default)]
    pub openrt_min: String,
    /// Platform profile ids this extension supports (empty = any).
    #[serde(default)]
    pub platforms: Vec<String>,
}

/// Security scan verdict recorded on the record after the one AgentShield
/// scan runs (never trusted from the package itself — reset on install).
pub const SECURITY_UNSCANNED: &str = "unscanned";
pub const SECURITY_CLEAN: &str = "clean";
pub const SECURITY_FLAGGED: &str = "flagged";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extension {
    pub id: ExtensionId,
    pub version: ExtensionVersion,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// Where the package came from (marketplace id, path, operator export…).
    #[serde(default)]
    pub source: String,
    pub provenance: Provenance,
    pub activation: ActivationState,
    /// Declared entrypoint ids (metadata only — never executed directly).
    #[serde(default)]
    pub entrypoints: Vec<String>,
    /// Skill-backed tool bindings (see [`ExtensionTool`]).
    #[serde(default)]
    pub tools: Vec<ExtensionTool>,
    /// Skills this extension requires (payloads or already-installed).
    #[serde(default)]
    pub skill_refs: Vec<SkillDependency>,
    /// Claimed permissions — must be a subset of the store's capabilities and
    /// of the caller's authority; an extension can never expand authority.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Required capability ids for the extension's tools.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Sandbox profile requirement (None = policy default).
    #[serde(default)]
    pub sandbox: Option<String>,
    /// Network requirements (exact capability names; Sovereign denies them).
    #[serde(default)]
    pub network: Vec<String>,
    /// Filesystem scope requirements (declared scope ids, never raw paths).
    #[serde(default)]
    pub filesystem: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<ExtensionDependency>,
    #[serde(default)]
    pub compat: ExtensionCompat,
    /// Trust label: `""`/unvetted/vetted/signed (same vocabulary as skills).
    #[serde(default)]
    pub trust: String,
    /// MCP servers this extension contributes (merged at build, gated by the
    /// MCP ecosystem's trust/lifecycle state).
    #[serde(default)]
    pub mcp_servers: Vec<ExtensionMcpServer>,
    /// Security scan verdict — local, reset to `unscanned` on install and set
    /// only by the one AgentShield scan path.
    #[serde(default)]
    pub security_status: String,
}

impl Extension {
    pub fn new(
        id: ExtensionId,
        version: ExtensionVersion,
        name: &str,
        description: &str,
        provenance: Provenance,
    ) -> Result<Self, AssetError> {
        ensure_path_safe_id(&id.0)?;
        provenance.validate()?;
        if name.trim().is_empty() {
            return Err(AssetError::Malformed {
                detail: "extension name must not be empty".into(),
            });
        }
        if description.trim().is_empty() {
            return Err(AssetError::Malformed {
                detail: "extension description must not be empty".into(),
            });
        }
        Ok(Self {
            id,
            version,
            name: name.to_string(),
            description: description.to_string(),
            author: String::new(),
            source: String::new(),
            provenance,
            activation: ActivationState::Registered,
            entrypoints: Vec::new(),
            tools: Vec::new(),
            skill_refs: Vec::new(),
            permissions: Vec::new(),
            capabilities: Vec::new(),
            sandbox: None,
            network: Vec::new(),
            filesystem: Vec::new(),
            models: Vec::new(),
            dependencies: Vec::new(),
            compat: ExtensionCompat::default(),
            trust: String::new(),
            mcp_servers: Vec::new(),
            security_status: SECURITY_UNSCANNED.to_string(),
        })
    }

    /// Effective trust label: legacy "" collapses to `unvetted`.
    pub fn trust_label(&self) -> &str {
        if self.trust.is_empty() {
            TRUST_UNVETTED
        } else {
            &self.trust
        }
    }

    /// Manifest structure validation — fail-closed on every id-shaped field,
    /// trust label, MCP server name and compatibility string.
    pub fn validate_manifest(&self) -> Result<(), AssetError> {
        ensure_path_safe_id(&self.id.0)?;
        for p in self
            .permissions
            .iter()
            .chain(self.capabilities.iter())
            .chain(self.network.iter())
            .chain(self.filesystem.iter())
            .chain(self.models.iter())
            .chain(self.entrypoints.iter())
        {
            ensure_path_safe_id(p)?;
        }
        if let Some(sb) = &self.sandbox {
            ensure_path_safe_id(sb)?;
        }
        for t in &self.tools {
            ensure_path_safe_id(&t.name)?;
            ensure_path_safe_id(&t.skill.0)?;
        }
        for d in &self.skill_refs {
            ensure_path_safe_id(&d.id.0)?;
        }
        for d in &self.dependencies {
            ensure_path_safe_id(&d.id.0)?;
            if d.id == self.id {
                return Err(AssetError::Malformed {
                    detail: format!("extension {} depends on itself", self.id),
                });
            }
        }
        for m in &self.mcp_servers {
            if m.name.is_empty()
                || !m
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
            {
                return Err(AssetError::Malformed {
                    detail: format!(
                        "extension {} declares invalid mcp server name {:?} (allowed: [a-z0-9_-])",
                        self.id, m.name
                    ),
                });
            }
            if m.command.trim().is_empty() {
                return Err(AssetError::Malformed {
                    detail: format!(
                        "extension {} mcp server `{}` has an empty command",
                        self.id, m.name
                    ),
                });
            }
            if m.trust != ""
                && m.trust != TRUST_UNVETTED
                && m.trust != TRUST_VETTED
                && m.trust != TRUST_SIGNED
            {
                return Err(AssetError::Malformed {
                    detail: format!(
                        "extension {} mcp server `{}` declares unknown trust {:?}",
                        self.id, m.name, m.trust
                    ),
                });
            }
        }
        if !self.compat.openrt_min.is_empty()
            && !self
                .compat
                .openrt_min
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        {
            return Err(AssetError::Malformed {
                detail: format!(
                    "extension {} compat.openrt_min {:?} must be version-like [A-Za-z0-9._-]",
                    self.id, self.compat.openrt_min
                ),
            });
        }
        for p in &self.compat.platforms {
            ensure_path_safe_id(p)?;
        }
        match self.trust.as_str() {
            "" | TRUST_UNVETTED | TRUST_VETTED | TRUST_SIGNED => Ok(()),
            other => Err(AssetError::Malformed {
                detail: format!(
                    "extension {} declares unknown trust label {:?} (allowed: {TRUST_UNVETTED}, {TRUST_VETTED}, {TRUST_SIGNED})",
                    self.id, other
                ),
            }),
        }
    }

    /// Declared platform list matches this profile id (""/empty = any).
    pub fn platform_matches(&self, profile_id: &str) -> bool {
        self.compat.platforms.is_empty() || self.compat.platforms.iter().any(|p| p == profile_id)
    }

    pub fn dep_key(dep: &ExtensionDependency) -> String {
        format!("{}@{}", dep.id.0, dep.min_version.0)
    }
}

impl CanonicalForm for Extension {
    fn asset_kind(&self) -> &'static str {
        "extension"
    }

    fn asset_id(&self) -> String {
        self.id.0.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let deps: Vec<String> = self.dependencies.iter().map(Extension::dep_key).collect();
        let skill_refs: Vec<String> = self
            .skill_refs
            .iter()
            .map(crate::skill::Skill::dep_key)
            .collect();
        let tools: Vec<String> = self
            .tools
            .iter()
            .map(|t| {
                format!(
                    "{}->{}@{}[{}]",
                    t.name,
                    t.skill.0,
                    t.min_version.0,
                    sorted_join(&t.required_caps)
                )
            })
            .collect();
        let mcps: Vec<String> = self
            .mcp_servers
            .iter()
            .map(|m| {
                format!(
                    "{}={}#{}[{}]",
                    m.name,
                    m.command,
                    m.args.join(" "),
                    sorted_join(&m.declared_capabilities)
                )
            })
            .collect();
        vec![
            ("kind".into(), "extension".into()),
            ("id".into(), self.id.0.clone()),
            ("version".into(), self.version.0.to_string()),
            ("name".into(), self.name.clone()),
            ("description".into(), self.description.clone()),
            ("author".into(), self.author.clone()),
            ("source".into(), self.source.clone()),
            ("permissions".into(), sorted_join(&self.permissions)),
            ("capabilities".into(), sorted_join(&self.capabilities)),
            ("network".into(), sorted_join(&self.network)),
            ("filesystem".into(), sorted_join(&self.filesystem)),
            ("models".into(), sorted_join(&self.models)),
            ("sandbox".into(), self.sandbox.clone().unwrap_or_default()),
            ("dependencies".into(), sorted_join(&deps)),
            ("skill_refs".into(), sorted_join(&skill_refs)),
            ("tools".into(), sorted_join(&tools)),
            ("mcp_servers".into(), sorted_join(&mcps)),
            ("entrypoints".into(), sorted_join(&self.entrypoints)),
            ("compat.openrt_min".into(), self.compat.openrt_min.clone()),
            (
                "compat.platforms".into(),
                sorted_join(&self.compat.platforms),
            ),
            ("trust".into(), self.trust_label().to_string()),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
            ("activation".into(), self.activation.as_str().into()),
            ("security_status".into(), self.security_status.clone()),
        ]
    }
}

/// Install package: `{"schema":1,"extension":{…},"skills":[…]}`.
/// Skills travel with the package so an extension's payloads land in the ONE
/// skill authority as Registered records (never Active).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionPackage {
    pub schema: u32,
    pub extension: Extension,
    #[serde(default)]
    pub skills: Vec<crate::skill::Skill>,
}

pub const EXTENSION_PACKAGE_SCHEMA: u32 = 1;

impl ExtensionPackage {
    /// Structural validation of the package itself (schema, payload skill
    /// manifests, coverage of `skill_refs`). Cross-store checks happen in the
    /// store/runtime install path.
    pub fn validate(&self) -> Result<(), AssetError> {
        if self.schema != EXTENSION_PACKAGE_SCHEMA {
            return Err(AssetError::Malformed {
                detail: format!(
                    "extension package schema {} unsupported (expected {EXTENSION_PACKAGE_SCHEMA})",
                    self.schema
                ),
            });
        }
        self.extension.validate_manifest()?;
        let mut seen = std::collections::BTreeSet::new();
        for s in &self.skills {
            s.provenance.validate()?;
            s.validate_manifest()?;
            crate::canonical::ensure_path_safe_id(&s.id.0)?;
            if !seen.insert(s.id.clone()) {
                return Err(AssetError::Duplicate {
                    kind: "package.skill".into(),
                    id: s.id.0.clone(),
                });
            }
            if s.activation != ActivationState::Registered {
                return Err(AssetError::Malformed {
                    detail: format!(
                        "package skill {} must be registered-state payload (found {:?})",
                        s.id, s.activation
                    ),
                });
            }
        }
        for r in &self.extension.skill_refs {
            let covered = self
                .skills
                .iter()
                .any(|s| s.id == r.id && s.version >= r.min_version);
            if !covered {
                // May still be satisfied by an already-installed skill; that
                // check is store-side. Here we only reject refs with a
                // provided-but-too-old payload.
                if let Some(s) = self.skills.iter().find(|s| s.id == r.id) {
                    return Err(AssetError::IncompatibleDependency {
                        id: self.extension.id.0.clone(),
                        dep: r.id.0.clone(),
                        required: r.min_version.0,
                        found: s.version.0,
                    });
                }
            }
        }
        Ok(())
    }
}
