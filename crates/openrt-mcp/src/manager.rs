//! MCP manager: connects every configured server **fail-closed at startup**,
//! derives one `mcp.<server>.<tool>` tool per advertised (and sane) remote
//! tool, and provides the `mcp.servers` / `mcp.discover` views.
//!
//! Capabilities (3-segment rule from `openrt-permissions` — a 4-segment
//! `mcp.<server>.invoke.all` can never parse, so the per-server cap is the
//! 3-segment `mcp.<server>.invoke`):
//! * every imported tool requires **both** `mcp.invoke.all` AND
//!   `mcp.<server>.invoke` (deny-by-default: both must be granted);
//! * `mcp.servers` / `mcp.discover` require `mcp.status.all`.
//!
//! Drift (M7B.5): tools registered at startup are fixed — a server that
//! later lists new/removed tools is reported by `mcp.discover` (post-hoc
//! registration would require a GateExecutor refactor; documented limitation,
//! never silently ignored).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};

use openrt_config::{McpConfig, McpServerConfig};
use openrt_fabric::{FabricMeta, RiskLevel, SupportState, ToolCategory};
use openrt_tools::{Tool, ToolSpec};

use crate::client::{McpClient, RemoteTool};

/// One configured remote tool (imported or refused).
#[derive(Debug, Clone)]
pub struct ImportedTool {
    pub server: String,
    pub mcp_name: String,
    /// Registered name in the one ToolRegistry: `mcp.<server>.<sanitized>`.
    pub registered_name: String,
    pub description: String,
    pub schema: Value,
    /// `Some(reason)` when refused at import (still reported by discover).
    pub rejected: Option<String>,
}

/// Sanitize a remote tool name for use as `mcp.<server>.<tool>`:
/// keep `[A-Za-z0-9_-]`, replace anything else with `_` (dots would create
/// extra name segments and collide with our namespacing).
fn sanitize(tool: &str) -> String {
    tool.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub struct McpManager {
    configured: Vec<McpServerConfig>,
    clients: BTreeMap<String, Arc<McpClient>>,
    failures: BTreeMap<String, String>,
    imported: Vec<ImportedTool>,
}

impl McpManager {
    /// Connect all configured servers (each with its own fail-closed
    /// outcome). Blocking — wrap in `spawn_blocking` at wiring; tests may
    /// call it directly. No servers ⇒ empty manager (fail-closed: no tools).
    pub fn connect_all(cfg: &McpConfig) -> Arc<Self> {
        let mut clients = BTreeMap::new();
        let mut failures = BTreeMap::new();
        let mut imported: Vec<ImportedTool> = Vec::new();

        for server in &cfg.servers {
            match McpClient::start(server) {
                Ok(client) => {
                    let mut seen: Vec<String> = Vec::new();
                    for remote in client.listed_tools() {
                        if let Some(reason) = &remote.rejected {
                            imported.push(ImportedTool {
                                server: server.name.clone(),
                                mcp_name: remote.name.clone(),
                                registered_name: String::new(),
                                description: remote.description.clone(),
                                schema: json!({ "type": "object" }),
                                rejected: Some(reason.clone()),
                            });
                            continue;
                        }
                        let clean = sanitize(&remote.name);
                        let registered = format!("mcp.{}.{}", server.name, clean);
                        if seen.iter().any(|s| *s == registered) {
                            imported.push(ImportedTool {
                                server: server.name.clone(),
                                mcp_name: remote.name.clone(),
                                registered_name: String::new(),
                                description: remote.description.clone(),
                                schema: remote.input_schema.clone(),
                                rejected: Some(format!(
                                    "name collision after sanitization (`{registered}`)"
                                )),
                            });
                            continue;
                        }
                        seen.push(registered.clone());
                        imported.push(ImportedTool {
                            server: server.name.clone(),
                            mcp_name: remote.name.clone(),
                            registered_name: registered,
                            description: remote.description.clone(),
                            schema: remote.input_schema.clone(),
                            rejected: None,
                        });
                    }
                    clients.insert(server.name.clone(), client);
                }
                Err(e) => {
                    failures.insert(server.name.clone(), format!("{} — {}", e.what, e.why));
                }
            }
        }

        Arc::new(Self {
            configured: cfg.servers.clone(),
            clients,
            failures,
            imported,
        })
    }

    pub fn configured(&self) -> &[McpServerConfig] {
        &self.configured
    }

    pub fn imported_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.imported
            .iter()
            .filter(|t| t.rejected.is_none())
            .map(|t| {
                let server = self
                    .clients
                    .get(&t.server)
                    .expect("imported tool's client must be connected");
                let per_server = format!("mcp.{}.invoke", t.server);
                let caps = ["mcp.invoke.all", per_server.as_str()];
                Arc::new(McpTool {
                    spec: ToolSpec::new(
                        t.registered_name.clone(),
                        format!(
                            "MCP tool `{}` from server `{}`: {} (invoke requires mcp.invoke.all + mcp.{}.invoke)",
                            t.mcp_name, t.server, t.description, t.server
                        ),
                    )
                    .with_caps(&caps)
                    .with_schema(t.schema.clone()),
                    client: server.clone(),
                    server: t.server.clone(),
                    mcp_name: t.mcp_name.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    /// Catalog metadata for every imported tool + the two management tools.
    pub fn metas(&self, host: &str) -> Vec<FabricMeta> {
        let mut out: Vec<FabricMeta> = self
            .imported
            .iter()
            .filter(|t| t.rejected.is_none()) // rejected tools never enter the registry
            .map(|t| {
                FabricMeta::new(&t.registered_name, ToolCategory::Mcp, RiskLevel::Medium)
                    .with_platform(host, SupportState::Green)
                    .verify()
                    .side_effects(&[
                        "runs a third-party server tool (side effects are the server's)",
                    ])
                    .rollback("none known — the server owns its effects")
            })
            .collect();
        out.push(
            FabricMeta::new("mcp.servers", ToolCategory::Mcp, RiskLevel::Low)
                .with_platform(host, SupportState::Green)
                .observe()
                .side_effects(&["none (read-only connection inventory; env values never shown)"]),
        );
        out.push(
            FabricMeta::new("mcp.discover", ToolCategory::Mcp, RiskLevel::Low)
                .with_platform(host, SupportState::Green)
                .observe()
                .side_effects(&["re-runs tools/list against connected servers (read-only)"]),
        );
        out
    }

    /// Read-only inventory behind `mcp.servers`.
    pub fn status_value(&self) -> Value {
        let servers: Vec<Value> = self
            .configured
            .iter()
            .map(|c| {
                let connected = self.clients.contains_key(&c.name);
                let failure = self.failures.get(&c.name).cloned();
                let client = self.clients.get(&c.name);
                let tools: Vec<Value> = self
                    .imported
                    .iter()
                    .filter(|t| t.server == c.name)
                    .map(|t| {
                        let mut tool = json!({
                            "name": t.mcp_name,
                            "description": t.description,
                        });
                        if !t.registered_name.is_empty() {
                            tool["registered_as"] = json!(t.registered_name);
                        }
                        if let Some(r) = &t.rejected {
                            tool["rejected"] = json!(r);
                        }
                        tool
                    })
                    .collect();
                json!({
                    "id": c.name,
                    "command": c.command,
                    "args": c.args,
                    // Env KEYS only — values are never rendered anywhere.
                    "env_keys": c.env.keys().cloned().collect::<Vec<_>>(),
                    "trust": c.trust.as_str(),
                    "declared_capabilities": c.declared_capabilities,
                    "connected": connected,
                    "failure": failure,
                    "server_info": client.map(|c| c.server_info()).unwrap_or_default(),
                    "tools": tools,
                })
            })
            .collect();
        json!({
            "ok": true,
            "configured": self.configured.len(),
            "connected": self.clients.len(),
            "failed": self.failures.len(),
            "servers": servers,
        })
    }

    /// Drift report behind `mcp.discover`: re-lists each connected server
    /// and compares against what was registered at startup.
    pub async fn discover(&self) -> Value {
        let mut out = Vec::new();
        for c in &self.configured {
            let registered: Vec<String> = self
                .imported
                .iter()
                .filter(|t| t.server == c.name && t.rejected.is_none())
                .map(|t| t.mcp_name.clone())
                .collect();
            let rejected: Vec<String> = self
                .imported
                .iter()
                .filter(|t| t.server == c.name)
                .filter_map(|t| t.rejected.clone())
                .collect();
            match self.clients.get(&c.name) {
                None => {
                    out.push(json!({
                        "id": c.name,
                        "connected": false,
                        "connect_failure": self.failures.get(&c.name),
                        "drift": { "added": [], "removed": [], "rejected": rejected },
                    }));
                }
                Some(client) => match client.refresh_tools().await {
                    Ok(list) => {
                        let live: Vec<&RemoteTool> =
                            list.iter().filter(|t| t.rejected.is_none()).collect();
                        let live_names: Vec<&str> = live.iter().map(|t| t.name.as_str()).collect();
                        let added: Vec<&str> = live_names
                            .iter()
                            .copied()
                            .filter(|n| !registered.iter().any(|r| r == *n))
                            .collect();
                        let removed: Vec<&str> = registered
                            .iter()
                            .map(String::as_str)
                            .filter(|r| !live_names.contains(r))
                            .collect();
                        out.push(json!({
                            "id": c.name,
                            "connected": true,
                            "server_info": client.server_info(),
                            "drift": {
                                // Present on the server now, not registered
                                // (startup snapshot is fixed — see module docs).
                                "added": added,
                                "removed": removed,
                                "rejected": rejected,
                                "note": if added.is_empty() && removed.is_empty() {
                                    "no drift"
                                } else {
                                    "drift detected; added tools become callable only after a runtime restart (registration happens at wiring)"
                                },
                            },
                        }));
                    }
                    Err(e) => {
                        out.push(json!({
                            "id": c.name,
                            "connected": false,
                            "refresh_failure": format!("{} — {}", e.what, e.why),
                            "drift": { "added": [], "removed": [], "rejected": rejected },
                        }));
                    }
                },
            }
        }
        json!({ "ok": true, "servers": out })
    }

    pub fn shutdown(&self) {
        for client in self.clients.values() {
            client.shutdown();
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One imported MCP tool behind the same Tool/ToolSpec contract.
pub struct McpTool {
    pub spec: ToolSpec,
    pub client: Arc<McpClient>,
    pub server: String,
    pub mcp_name: String,
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<openrt_tools::ToolOutput, openrt_tools::ToolError> {
        let name = self.spec.name.clone();
        match self.client.call_tool(&self.mcp_name, args).await {
            Ok(result) => Ok(openrt_tools::ToolOutput::Json(json!({
                "ok": true,
                "server": self.server,
                "tool": self.mcp_name,
                "text": crate::client::extract_text(&result),
                "content": result.get("content").cloned().unwrap_or(Value::Null),
                "verification": {
                    "ok": true,
                    "method": "client-side schema validation before call; server isError checked"
                },
            }))),
            Err(e) => openrt_fabric::fail(&name, e),
        }
    }
}

/// `mcp.servers` / `mcp.discover` management tools.
pub fn management_tools(manager: Arc<McpManager>) -> Vec<Arc<dyn Tool>> {
    let servers = {
        let manager = manager.clone();
        struct ServersTool {
            spec: ToolSpec,
            manager: Arc<McpManager>,
        }
        #[async_trait::async_trait]
        impl Tool for ServersTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn run(
                &self,
                _args: Value,
            ) -> Result<openrt_tools::ToolOutput, openrt_tools::ToolError> {
                Ok(openrt_tools::ToolOutput::Json(self.manager.status_value()))
            }
        }
        Arc::new(ServersTool {
            spec: ToolSpec::new(
                "mcp.servers",
                "List configured MCP servers: connection state, tools (schemas visible), env KEY names only, and fail-closed startup reasons.",
            )
            .with_caps(&["mcp.status.all"])
            .with_schema(json!({ "type": "object", "properties": {} })),
            manager,
        }) as Arc<dyn Tool>
    };
    let discover = {
        let manager = manager.clone();
        struct DiscoverTool {
            spec: ToolSpec,
            manager: Arc<McpManager>,
        }
        #[async_trait::async_trait]
        impl Tool for DiscoverTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn run(
                &self,
                _args: Value,
            ) -> Result<openrt_tools::ToolOutput, openrt_tools::ToolError> {
                Ok(openrt_tools::ToolOutput::Json(
                    self.manager.discover().await,
                ))
            }
        }
        Arc::new(DiscoverTool {
            spec: ToolSpec::new(
                "mcp.discover",
                "Re-run tools/list on every connected MCP server and report drift against the startup registration (added/removed/rejected).",
            )
            .with_caps(&["mcp.status.all"])
            .with_schema(json!({ "type": "object", "properties": {} })),
            manager,
        }) as Arc<dyn Tool>
    };
    vec![servers, discover]
}
