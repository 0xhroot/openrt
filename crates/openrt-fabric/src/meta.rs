//! Authoritative tool-fabric metadata (category, risk, platform support,
//! verification, side effects) + the `tools.list` discovery tool.
//!
//! The metadata *enriches* the one existing `ToolRegistry` — it is never a
//! second registry: names, descriptions, schemas, and required capabilities
//! always come from the registered [`ToolSpec`]; `FabricMeta` adds the
//! fabric-level columns the mission requires.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use openrt_tools::{Tool, ToolError, ToolOutput, ToolRegistry, ToolSpec};

/// Coarse tool category (grouping for discovery and policy review).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    Computer,
    Browser,
    Database,
    Office,
    Application,
    Mcp,
    Discovery,
    Utility,
}

/// Risk classification used by policy review surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

/// Per-environment support claim for one tool (7A matrix vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SupportState {
    /// Real backend proven by tests in this environment.
    Green,
    /// Works with documented limitations.
    Partial,
    /// Backend missing or feature not implemented here; structured reason.
    Unavailable,
    /// Environment cannot be supported (e.g. unsafe-forbidden host).
    Blocked,
}

/// Fabric-level metadata for one registered tool.
#[derive(Debug, Clone, Serialize)]
pub struct FabricMeta {
    /// Tool id (must match the registered `ToolSpec.name`).
    pub tool: String,
    pub category: ToolCategory,
    pub risk: RiskLevel,
    /// Environment key (`hyprland`, `x11`, `windows`, `host`, …) → state.
    pub platforms: BTreeMap<String, SupportState>,
    /// Tool result contains observation/state data.
    pub supports_observation: bool,
    /// Tool performs a post-condition check (focus-verify, digest, re-read…).
    pub supports_verification: bool,
    /// Human-readable side effects ("writes files", "external network", …).
    pub side_effects: Vec<String>,
    /// How the side effect can be undone ("delete file", "none", …).
    pub rollback: String,
    /// Live backend status filled in by the producing subsystem at wiring
    /// time (`{available, backend, reason}`); `None` = not probed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_status: Option<Value>,
}

impl FabricMeta {
    pub fn new(tool: impl Into<String>, category: ToolCategory, risk: RiskLevel) -> Self {
        Self {
            tool: tool.into(),
            category,
            risk,
            platforms: BTreeMap::new(),
            supports_observation: false,
            supports_verification: false,
            side_effects: Vec::new(),
            rollback: "none".into(),
            runtime_status: None,
        }
    }

    pub fn with_platform(mut self, env: &str, state: SupportState) -> Self {
        self.platforms.insert(env.to_string(), state);
        self
    }

    pub fn observe(mut self) -> Self {
        self.supports_observation = true;
        self
    }

    pub fn verify(mut self) -> Self {
        self.supports_verification = true;
        self
    }

    pub fn side_effects(mut self, effects: &[&str]) -> Self {
        self.side_effects = effects.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn rollback(mut self, rollback: impl Into<String>) -> Self {
        self.rollback = rollback.into();
        self
    }
}

/// The authoritative metadata catalog (in-memory, wiring-time filled).
#[derive(Default)]
pub struct FabricCatalog {
    metas: Mutex<BTreeMap<String, FabricMeta>>,
}

impl FabricCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register metadata. The tool must exist in the ToolRegistry (checked by
    /// the wiring tests) — the catalog never owns the tool itself.
    pub fn register(&self, meta: FabricMeta) {
        self.metas
            .lock()
            .expect("fabric catalog poisoned")
            .insert(meta.tool.clone(), meta);
    }

    pub fn get(&self, tool: &str) -> Option<FabricMeta> {
        self.metas
            .lock()
            .expect("fabric catalog poisoned")
            .get(tool)
            .cloned()
    }

    pub fn set_status(&self, tool: &str, status: Value) {
        if let Some(m) = self
            .metas
            .lock()
            .expect("fabric catalog poisoned")
            .get_mut(tool)
        {
            m.runtime_status = Some(status);
        }
    }

    /// Merge live registry specs with catalog metadata into discovery rows.
    pub fn discovery_rows(&self, registry: &ToolRegistry) -> Vec<Value> {
        let metas = self.metas.lock().expect("fabric catalog poisoned");
        let mut rows: Vec<Value> = registry
            .specs()
            .into_iter()
            .map(|spec| {
                let meta = metas.get(&spec.name);
                json!({
                    "name": spec.name,
                    "description": spec.description,
                    "required_caps": spec.required_caps,
                    "parameters": spec.parameters_schema,
                    "category": meta.map(|m| serde_json::to_value(m.category).unwrap_or_default()),
                    "risk": meta.map(|m| serde_json::to_value(m.risk).unwrap_or_default()),
                    "platforms": meta.map(|m| {
                        m.platforms
                            .iter()
                            .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or_default()))
                            .collect::<BTreeMap<String, Value>>()
                    }),
                    "supports_observation": meta.map(|m| m.supports_observation).unwrap_or(false),
                    "supports_verification": meta.map(|m| m.supports_verification).unwrap_or(false),
                    "side_effects": meta.map(|m| m.side_effects.clone()).unwrap_or_default(),
                    "rollback": meta.map(|m| m.rollback.clone()).unwrap_or_else(|| "unknown".into()),
                    "runtime_status": meta.and_then(|m| m.runtime_status.clone()),
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["name"].as_str().unwrap_or_default())
        });
        rows
    }
}

/// `tools.list` — the single discovery surface over the one ToolRegistry,
/// enriched with fabric metadata. `registry` is set once after every other
/// tool has been registered (so the snapshot includes `tools.list` itself).
pub struct ToolsListTool {
    pub spec: ToolSpec,
    pub catalog: Arc<FabricCatalog>,
    registry: OnceLock<ToolRegistry>,
}

impl ToolsListTool {
    pub fn new(catalog: Arc<FabricCatalog>) -> Self {
        Self {
            spec: ToolSpec::new(
                "tools.list",
                "List every registered tool with description, required capabilities, schema, category, risk, platform support, verification support, side effects, and live backend status.",
            )
            .with_caps(&["tools.discovery.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "prefix": { "type": "string", "description": "only list tools starting with this prefix" },
                    "category": { "type": "string", "description": "only list tools in this category" }
                }
            })),
            catalog,
            registry: OnceLock::new(),
        }
    }

    /// Called once by runtime wiring after the registry is fully built.
    pub fn set_registry(&self, registry: ToolRegistry) {
        let _ = self.registry.set(registry);
    }
}

#[async_trait]
impl Tool for ToolsListTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let registry = self.registry.get().ok_or_else(|| {
            ToolError::Internal(
                "tools.list".into(),
                "registry snapshot not initialized (wiring bug)".into(),
            )
        })?;
        let prefix = args.get("prefix").and_then(Value::as_str);
        let category = args.get("category").and_then(Value::as_str);
        let mut rows = self.catalog.discovery_rows(registry);
        if let Some(p) = prefix {
            rows.retain(|r| r["name"].as_str().unwrap_or_default().starts_with(p));
        }
        if let Some(c) = category {
            rows.retain(|r| {
                r["category"].as_str().unwrap_or_default() == c
                    || serde_json::to_value(r["category"].clone())
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s == c))
                        .unwrap_or(false)
            });
        }
        Ok(ToolOutput::Json(json!({
            "ok": true,
            "count": rows.len(),
            "tools": rows,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(name, "test tool").with_caps(&["test.cap.all"])
    }

    struct Dummy(ToolSpec);
    #[async_trait]
    impl Tool for Dummy {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }
        async fn run(&self, _: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::Json(json!({"ok": true})))
        }
    }

    #[tokio::test]
    async fn discovery_merges_spec_and_meta() {
        let catalog = Arc::new(FabricCatalog::new());
        catalog.register(
            FabricMeta::new("demo.tool", ToolCategory::Database, RiskLevel::High)
                .with_platform("host", SupportState::Green)
                .verify()
                .side_effects(&["writes files"])
                .rollback("delete file"),
        );
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Dummy(spec("demo.tool")))).unwrap();
        let tool = ToolsListTool::new(catalog.clone());
        tool.set_registry(reg);
        let out = tool.run(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&out.render()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["count"], 1);
        let row = &v["tools"][0];
        assert_eq!(row["name"], "demo.tool");
        assert_eq!(row["category"], "database");
        assert_eq!(row["risk"], "high");
        assert_eq!(row["required_caps"][0], "test.cap.all");
        assert_eq!(row["supports_verification"], true);
        assert_eq!(row["platforms"]["host"], "GREEN");
        assert_eq!(row["rollback"], "delete file");
    }

    #[tokio::test]
    async fn discovery_filters_by_prefix() {
        let catalog = Arc::new(FabricCatalog::new());
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Dummy(spec("database.query"))))
            .unwrap();
        reg.register(Arc::new(Dummy(spec("excel.read")))).unwrap();
        let tool = ToolsListTool::new(catalog);
        tool.set_registry(reg);
        let out = tool.run(json!({"prefix": "database."})).await.unwrap();
        let v: Value = serde_json::from_str(&out.render()).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["tools"][0]["name"], "database.query");
    }

    #[tokio::test]
    async fn uninitialized_registry_fails_closed() {
        let tool = ToolsListTool::new(Arc::new(FabricCatalog::new()));
        let err = tool.run(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::Internal(_, _)));
    }
}
