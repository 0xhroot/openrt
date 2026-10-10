//! Phase 11 (M11.2): the durable MCP server registry over the ONE store —
//! namespace `mcp.servers` (the Phase 7B row's claimed namespace, now
//! implemented as real records).
//!
//! One authority for every MCP server (operator config, extension-declared,
//! and CLI-registered):
//! * **connection details** (command/args/env keys/capabilities) are live-
//!   sourced from the declaring source on every sync — the record mirrors
//!   them for inspection and never spawns stale declarations;
//! * **operator state** (enabled, 4-level trust, security verdict, health,
//!   provenance) lives in the registry: seeded once at first sight, then
//!   only explicit operator actions change it (config edits never silently
//!   override CLI lifecycle decisions, and CLI decisions are audited);
//! * **spawn gate** (fail-closed): a server connects only when it is
//!   enabled, its trust is `untrusted`/`trusted`, and its security status
//!   is not `flagged`. Trust never bypasses permissions, policy, sandbox,
//!   or mode — imported tools always require `mcp.invoke.all` +
//!   `mcp.<server>.invoke` at the gate.
//!
//! Secret hygiene: only env **key names** are ever persisted or rendered;
//! values come from the operator config or the process environment at spawn
//! and are never stored, logged, or serialized into any output.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use openrt_config::{McpConfig, McpServerConfig, McpTrust};
use openrt_persistence::PersistenceStore;

/// Registry namespace (ADR-006: one store, namespaced records).
pub const MCP_SERVERS_NS: &str = "mcp.servers";

pub const SECURITY_UNSCANNED: &str = "unscanned";
pub const SECURITY_CLEAN: &str = "clean";
pub const SECURITY_FLAGGED: &str = "flagged";

/// Where a server's connection details are declared (live-sourced).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpOrigin {
    /// `config.fabric.mcp` — removed from config ⇒ auto-disabled.
    Config,
    /// Declared by an Active extension manifest — deactivated ⇒ disabled.
    Extension,
    /// Registered through `openrt mcp register` — operator lifecycle only.
    #[default]
    Cli,
}

/// One discovered remote tool (persisted snapshot from `tools/list`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpDiscoveredTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_as: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<String>,
}

/// Result of a health probe (`openrt mcp health`): connect + initialize +
/// `tools/list` within the fail-closed timeouts, then shutdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpHealthReport {
    pub ok: bool,
    pub detail: String,
    pub at_ms: u64,
    #[serde(default)]
    pub server_info: String,
    #[serde(default)]
    pub tool_count: u32,
}

/// One registered MCP server record (env key names only — never values).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerRecord {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variable NAMES only; values are resolved at spawn from
    /// the operator config or the process environment and never persisted.
    #[serde(default)]
    pub env_keys: Vec<String>,
    pub trust: McpTrust,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub declared_capabilities: Vec<String>,
    /// `unscanned` | `clean` | `flagged` (only the ONE AgentShield scan
    /// path ever changes it).
    #[serde(default = "unscanned")]
    pub security_status: String,
    #[serde(default)]
    pub security_findings: Vec<String>,
    #[serde(default)]
    pub origin: McpOrigin,
    /// Human-readable declaring source (`config.fabric.mcp`, extension id,
    /// or `cli`).
    #[serde(default)]
    pub declared_by: String,
    /// Operator (or source) that created this record.
    #[serde(default)]
    pub registered_by: String,
    #[serde(default)]
    pub discovered_tools: Vec<McpDiscoveredTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<McpHealthReport>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

fn unscanned() -> String {
    SECURITY_UNSCANNED.to_string()
}

impl McpServerRecord {
    /// The one spawn gate (fail-closed): enabled + connectable trust +
    /// not flagged by a scan.
    pub fn is_connectable(&self) -> bool {
        self.enabled
            && !matches!(self.trust, McpTrust::Review | McpTrust::Blocked)
            && self.security_status != SECURITY_FLAGGED
    }
}

/// One live declaration (config or extension) as seen by a sync pass.
#[derive(Debug, Clone)]
pub struct SourceDecl {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Env key NAMES (values are carried in `env` for config only).
    pub env_keys: Vec<String>,
    /// Inline env values from the operator config — spawn-time only, never
    /// written to the registry.
    pub env: BTreeMap<String, String>,
    pub declared_capabilities: Vec<String>,
    pub declared_by: String,
    pub origin: McpOrigin,
    /// Trust seeded at first sight (config declares it; extensions are
    /// always seeded `untrusted` — never self-elevated).
    pub first_sight_trust: McpTrust,
}

/// Registry errors (fail-closed — every one of these blocks an operation).
#[derive(Debug, thiserror::Error)]
pub enum McpRegistryError {
    #[error("mcp registry persistence failed: {0}")]
    Store(#[from] openrt_persistence::PersistenceError),
    #[error("mcp server `{0}` is already registered")]
    Duplicate(String),
    #[error("mcp server `{0}` is not registered")]
    NotFound(String),
    #[error("invalid mcp server name `{0}`: use [a-z0-9_-] only (lowercase)")]
    InvalidName(String),
    #[error("invalid command for mcp server `{0}`: command is empty")]
    EmptyCommand(String),
    #[error(
        "remote/HTTP MCP transport is UNAVAILABLE for `{0}` — stdio only \
         (remote/HTTP MCP is honest UNAVAILABLE; netpol SCOPE_MCP is reserved, never silently enabled)"
    )]
    HttpTransportUnavailable(String),
    #[error(
        "mcp server `{0}` cannot be trusted: security status is `{1}` — \
         run `openrt mcp scan {0}` until clean first"
    )]
    TrustRequiresCleanScan(String, String),
    #[error("mcp registry serde failed: {0}")]
    Serde(String),
}

/// Durable registry: an in-memory map over the `mcp.servers` records.
#[derive(Debug, Default)]
pub struct McpRegistry {
    records: BTreeMap<String, McpServerRecord>,
}

impl McpRegistry {
    /// Load every persisted record (fail-closed on corrupt JSON).
    pub async fn load(store: &dyn PersistenceStore) -> Result<Self, McpRegistryError> {
        let rows = store.list_records(MCP_SERVERS_NS).await?;
        let mut records = BTreeMap::new();
        for (key, value) in rows {
            let rec: McpServerRecord = serde_json::from_value(value)
                .map_err(|e| McpRegistryError::Serde(format!("`{key}`: {e}")))?;
            records.insert(key, rec);
        }
        Ok(Self { records })
    }

    /// Persist every record (one `put_record` per server; no deletes —
    /// removal is never an operation on this registry, so history stays).
    pub async fn save(&self, store: &dyn PersistenceStore) -> Result<(), McpRegistryError> {
        for (name, rec) in &self.records {
            let value = serde_json::to_value(rec)
                .map_err(|e| McpRegistryError::Serde(format!("`{name}`: {e}")))?;
            store.put_record(MCP_SERVERS_NS, name, &value).await?;
        }
        Ok(())
    }

    pub fn records(&self) -> impl Iterator<Item = &McpServerRecord> {
        self.records.values()
    }

    pub fn get(&self, name: &str) -> Result<&McpServerRecord, McpRegistryError> {
        self.records
            .get(name)
            .ok_or_else(|| McpRegistryError::NotFound(name.to_string()))
    }

    pub fn get_mut(&mut self, name: &str) -> Result<&mut McpServerRecord, McpRegistryError> {
        self.records
            .get_mut(name)
            .ok_or_else(|| McpRegistryError::NotFound(name.to_string()))
    }

    /// CLI registration: `Registered/unscanned`, **disabled**, `untrusted`
    /// — a registered server never arrives connectable (M11.2's analogue of
    /// "extensions never arrive Active").
    pub fn register(
        &mut self,
        name: &str,
        command: &str,
        args: Vec<String>,
        env_keys: Vec<String>,
        declared_capabilities: Vec<String>,
        operator: &str,
        now_ms: u64,
    ) -> Result<(), McpRegistryError> {
        validate_name(name)?;
        if command.trim().is_empty() {
            return Err(McpRegistryError::EmptyCommand(name.to_string()));
        }
        if command.starts_with("http://") || command.starts_with("https://") {
            return Err(McpRegistryError::HttpTransportUnavailable(name.to_string()));
        }
        if self.records.contains_key(name) {
            return Err(McpRegistryError::Duplicate(name.to_string()));
        }
        self.records.insert(
            name.to_string(),
            McpServerRecord {
                name: name.to_string(),
                command: command.to_string(),
                args,
                env_keys,
                trust: McpTrust::Untrusted,
                enabled: false,
                declared_capabilities,
                security_status: SECURITY_UNSCANNED.to_string(),
                security_findings: Vec::new(),
                origin: McpOrigin::Cli,
                declared_by: "cli".to_string(),
                registered_by: operator.to_string(),
                discovered_tools: Vec::new(),
                health: None,
                created_at: now_ms,
                updated_at: now_ms,
            },
        );
        Ok(())
    }

    /// Operator trust change. Escalation to `trusted` requires a clean scan
    /// verdict; demotion (`review`/`blocked`) is always allowed — trust
    /// never bypasses permissions/policy/sandbox/mode in any direction.
    pub fn set_trust(
        &mut self,
        name: &str,
        trust: McpTrust,
        now_ms: u64,
    ) -> Result<McpTrust, McpRegistryError> {
        let rec = self.get_mut(name)?;
        if trust == McpTrust::Trusted && rec.security_status != SECURITY_CLEAN {
            return Err(McpRegistryError::TrustRequiresCleanScan(
                name.to_string(),
                rec.security_status.clone(),
            ));
        }
        let previous = rec.trust;
        rec.trust = trust;
        rec.updated_at = now_ms;
        Ok(previous)
    }

    pub fn set_enabled(
        &mut self,
        name: &str,
        enabled: bool,
        now_ms: u64,
    ) -> Result<(), McpRegistryError> {
        let rec = self.get_mut(name)?;
        rec.enabled = enabled;
        rec.updated_at = now_ms;
        Ok(())
    }

    /// Record a scan verdict (only the ONE AgentShield path calls this).
    /// A flagged verdict demotes `trusted` → `review` (fail-closed: new
    /// findings invalidate recorded trust; nothing is ever escalated here).
    pub fn set_scan_result(
        &mut self,
        name: &str,
        status: &str,
        findings: Vec<String>,
        now_ms: u64,
    ) -> Result<McpTrust, McpRegistryError> {
        let rec = self.get_mut(name)?;
        rec.security_status = status.to_string();
        rec.security_findings = findings;
        if rec.security_status == SECURITY_FLAGGED && rec.trust == McpTrust::Trusted {
            rec.trust = McpTrust::Review;
        }
        rec.updated_at = now_ms;
        Ok(rec.trust)
    }

    /// Persist a health probe result + discovered-tools snapshot.
    pub fn set_probe_result(
        &mut self,
        name: &str,
        health: McpHealthReport,
        tools: Vec<McpDiscoveredTool>,
        now_ms: u64,
    ) -> Result<(), McpRegistryError> {
        let rec = self.get_mut(name)?;
        rec.health = Some(health);
        rec.discovered_tools = tools;
        rec.updated_at = now_ms;
        Ok(())
    }

    /// Sync live declarations into the registry and compute the spawn
    /// config. Returns `(effective_config, dirty)` — callers persist only
    /// when `dirty` is true (a no-op build never writes).
    ///
    /// Rules:
    /// * config declares first (a name the operator configured always wins
    ///   over an extension declaration of the same name);
    /// * existing records keep operator state (trust/enabled/security);
    ///   connection details are refreshed from the live source;
    /// * first sight seeds: config ⇒ `enabled` + config trust; extension ⇒
    ///   `enabled` + `untrusted` (never self-elevated);
    /// * a non-CLI record whose source no longer declares it is disabled
    ///   (deleting a config entry stops spawning it — fail-closed).
    pub fn sync_and_effective(
        &mut self,
        config_servers: &[McpServerConfig],
        ext_decls: &[SourceDecl],
        now_ms: u64,
    ) -> (McpConfig, bool) {
        let mut dirty = false;

        let mut sources: BTreeMap<String, SourceDecl> = BTreeMap::new();
        for s in config_servers {
            sources.insert(
                s.name.clone(),
                SourceDecl {
                    name: s.name.clone(),
                    command: s.command.clone(),
                    args: s.args.clone(),
                    env_keys: s.env.keys().cloned().collect(),
                    env: s.env.clone(),
                    declared_capabilities: s.declared_capabilities.clone(),
                    declared_by: "config.fabric.mcp".to_string(),
                    origin: McpOrigin::Config,
                    first_sight_trust: s.trust,
                },
            );
        }
        for d in ext_decls {
            // Config wins on name conflict (extensions never override
            // operator config — same rule as the Phase 7B merge).
            sources.entry(d.name.clone()).or_insert_with(|| d.clone());
        }

        let mut seen: BTreeSet<&String> = BTreeSet::new();
        for (name, decl) in &sources {
            seen.insert(name);
            match self.records.get_mut(name) {
                Some(rec) => {
                    let changed = rec.command != decl.command
                        || rec.args != decl.args
                        || rec.declared_capabilities != decl.declared_capabilities
                        || rec.env_keys != decl.env_keys
                        || rec.declared_by != decl.declared_by
                        || rec.origin != decl.origin;
                    if changed {
                        rec.command = decl.command.clone();
                        rec.args = decl.args.clone();
                        rec.declared_capabilities = decl.declared_capabilities.clone();
                        rec.env_keys = decl.env_keys.clone();
                        rec.declared_by = decl.declared_by.clone();
                        rec.origin = decl.origin;
                        rec.updated_at = now_ms;
                        dirty = true;
                    }
                }
                None => {
                    self.records.insert(
                        name.clone(),
                        McpServerRecord {
                            name: name.clone(),
                            command: decl.command.clone(),
                            args: decl.args.clone(),
                            env_keys: decl.env_keys.clone(),
                            trust: decl.first_sight_trust,
                            enabled: true,
                            declared_capabilities: decl.declared_capabilities.clone(),
                            security_status: SECURITY_UNSCANNED.to_string(),
                            security_findings: Vec::new(),
                            origin: decl.origin,
                            declared_by: decl.declared_by.clone(),
                            registered_by: decl.declared_by.clone(),
                            discovered_tools: Vec::new(),
                            health: None,
                            created_at: now_ms,
                            updated_at: now_ms,
                        },
                    );
                    dirty = true;
                }
            }
        }

        for (name, rec) in self.records.iter_mut() {
            if rec.origin != McpOrigin::Cli && !seen.contains(name) && rec.enabled {
                rec.enabled = false;
                rec.updated_at = now_ms;
                dirty = true;
            }
        }

        let mut spawn: Vec<McpServerConfig> = Vec::new();
        for rec in self.records.values() {
            if !rec.is_connectable() {
                continue;
            }
            let env = match rec.origin {
                McpOrigin::Config => sources
                    .get(&rec.name)
                    .map(|d| d.env.clone())
                    .unwrap_or_default(),
                _ => resolve_env_keys(&rec.env_keys),
            };
            spawn.push(McpServerConfig {
                name: rec.name.clone(),
                command: rec.command.clone(),
                args: rec.args.clone(),
                env,
                trust: rec.trust,
                declared_capabilities: rec.declared_capabilities.clone(),
            });
        }
        (McpConfig { servers: spawn }, dirty)
    }

    /// Inventory for `mcp list` / doctor / `mcp.servers` status surfaces
    /// (env key names only; values never rendered).
    pub fn status_value(&self) -> Value {
        let mut enabled = 0usize;
        let mut connectable = 0usize;
        let mut review = 0usize;
        let mut blocked = 0usize;
        let mut flagged = 0usize;
        let servers: Vec<Value> = self
            .records
            .values()
            .map(|r| {
                if r.enabled {
                    enabled += 1;
                }
                if r.is_connectable() {
                    connectable += 1;
                }
                if r.trust == McpTrust::Review {
                    review += 1;
                }
                if r.trust == McpTrust::Blocked {
                    blocked += 1;
                }
                if r.security_status == SECURITY_FLAGGED {
                    flagged += 1;
                }
                json!({
                    "name": r.name,
                    "origin": r.origin,
                    "declared_by": r.declared_by,
                    "registered_by": r.registered_by,
                    "command": r.command,
                    "args": r.args,
                    "env_keys": r.env_keys,
                    "trust": r.trust.as_str(),
                    "enabled": r.enabled,
                    "connectable": r.is_connectable(),
                    "security_status": r.security_status,
                    "security_findings": r.security_findings,
                    "declared_capabilities": r.declared_capabilities,
                    "discovered_tool_count": r.discovered_tools.len(),
                    "health": r.health,
                    "created_at": r.created_at,
                    "updated_at": r.updated_at,
                })
            })
            .collect();
        json!({
            "ok": true,
            "namespace": MCP_SERVERS_NS,
            "summary": {
                "total": self.records.len(),
                "enabled": enabled,
                "connectable": connectable,
                "review": review,
                "blocked": blocked,
                "flagged": flagged,
            },
            "servers": servers,
        })
    }
}

/// Validate a server id (lowercase `[a-z0-9_-]`, non-empty) — the same
/// shape the `mcp.<server>.<tool>` namespacing and capability ids require.
pub fn validate_name(name: &str) -> Result<(), McpRegistryError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(McpRegistryError::InvalidName(name.to_string()))
    }
}

/// Resolve env key names from the process environment (values are used for
/// the child process only — never persisted, never logged).
pub fn resolve_env_keys(keys: &[String]) -> BTreeMap<String, String> {
    keys.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
        .collect()
}

/// Current wall-clock time in milliseconds (record timestamps).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str, cmd: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: cmd.to_string(),
            args: vec!["--serve".to_string()],
            env: BTreeMap::new(),
            trust: McpTrust::Untrusted,
            declared_capabilities: vec!["files.read".to_string()],
        }
    }

    fn ext(name: &str, cmd: &str) -> SourceDecl {
        SourceDecl {
            name: name.to_string(),
            command: cmd.to_string(),
            args: vec![],
            env_keys: vec![],
            env: BTreeMap::new(),
            declared_capabilities: vec![],
            declared_by: "extension ext.x".to_string(),
            origin: McpOrigin::Extension,
            first_sight_trust: McpTrust::Untrusted,
        }
    }

    #[test]
    fn config_first_sight_seeds_enabled_unscanned_with_config_trust() {
        let mut reg = McpRegistry::default();
        let mut c = cfg("srv", "/bin/srv");
        c.trust = McpTrust::Trusted;
        let (eff, dirty) = reg.sync_and_effective(&[c], &[], 1);
        assert!(dirty);
        let rec = reg.get("srv").unwrap();
        assert!(rec.enabled);
        assert_eq!(rec.trust, McpTrust::Trusted);
        assert_eq!(rec.security_status, SECURITY_UNSCANNED);
        assert_eq!(rec.origin, McpOrigin::Config);
        assert_eq!(eff.servers.len(), 1);
    }

    #[test]
    fn spawn_gate_is_fail_closed() {
        let mut reg = McpRegistry::default();
        reg.register("cli", "/bin/cli", vec![], vec![], vec![], "op", 1)
            .unwrap();
        // registered ⇒ disabled ⇒ never spawned
        let (eff, _) = reg.sync_and_effective(&[], &[], 2);
        assert!(eff.servers.is_empty());

        reg.set_enabled("cli", true, 3).unwrap();
        let (eff, _) = reg.sync_and_effective(&[], &[], 4);
        assert_eq!(eff.servers.len(), 1);

        reg.set_trust("cli", McpTrust::Review, 5).unwrap();
        let (eff, _) = reg.sync_and_effective(&[], &[], 6);
        assert!(eff.servers.is_empty(), "review never connects");

        reg.set_trust("cli", McpTrust::Blocked, 7).unwrap();
        let (eff, _) = reg.sync_and_effective(&[], &[], 8);
        assert!(eff.servers.is_empty(), "blocked never connects");

        // flagged security status also blocks, even trusted+enabled
        reg.set_scan_result("cli", SECURITY_CLEAN, vec![], 9)
            .unwrap();
        reg.set_trust("cli", McpTrust::Trusted, 10).unwrap();
        reg.set_enabled("cli", true, 11).unwrap();
        reg.set_scan_result("cli", SECURITY_FLAGGED, vec!["x".into()], 12)
            .unwrap();
        let (eff, _) = reg.sync_and_effective(&[], &[], 12);
        assert!(eff.servers.is_empty(), "flagged never connects");
    }

    #[test]
    fn operator_state_survives_resync_and_connection_details_refresh() {
        let mut reg = McpRegistry::default();
        reg.sync_and_effective(&[cfg("srv", "/bin/old")], &[], 1);
        reg.set_scan_result("srv", SECURITY_CLEAN, vec![], 2)
            .unwrap();
        reg.set_trust("srv", McpTrust::Trusted, 3).unwrap();
        // config edits the command; trust (registry-authoritative) stays
        let (eff, dirty) = reg.sync_and_effective(&[cfg("srv", "/bin/new")], &[], 4);
        assert!(dirty);
        let rec = reg.get("srv").unwrap();
        assert_eq!(rec.command, "/bin/new");
        assert_eq!(
            rec.trust,
            McpTrust::Trusted,
            "CLI trust wins after first sight"
        );
        assert_eq!(eff.servers[0].command, "/bin/new");
        assert_eq!(eff.servers[0].trust, McpTrust::Trusted);
    }

    #[test]
    fn missing_config_source_disables_but_cli_records_stay() {
        let mut reg = McpRegistry::default();
        reg.sync_and_effective(&[cfg("gone", "/bin/gone")], &[], 1);
        reg.register("mine", "/bin/mine", vec![], vec![], vec![], "op", 2)
            .unwrap();
        reg.set_enabled("mine", true, 3).unwrap();
        let (eff, dirty) = reg.sync_and_effective(&[], &[], 4);
        assert!(dirty, "config-origin record auto-disabled");
        assert!(!reg.get("gone").unwrap().enabled);
        assert!(
            reg.get("mine").unwrap().enabled,
            "CLI records are never auto-disabled"
        );
        assert_eq!(eff.servers.len(), 1);
        assert_eq!(eff.servers[0].name, "mine");
    }

    #[test]
    fn config_wins_name_conflict_and_extensions_never_self_elevate() {
        let mut reg = McpRegistry::default();
        let exts = vec![ext("same", "/ext/bin"), ext("onlyext", "/ext/bin")];
        let (eff, _) = reg.sync_and_effective(&[cfg("same", "/cfg/bin")], &exts, 1);
        assert_eq!(reg.get("same").unwrap().command, "/cfg/bin");
        assert_eq!(reg.get("same").unwrap().trust, McpTrust::Untrusted);
        let onlyext = reg.get("onlyext").unwrap();
        assert_eq!(onlyext.origin, McpOrigin::Extension);
        assert_eq!(
            onlyext.trust,
            McpTrust::Untrusted,
            "extension seeded untrusted"
        );
        assert_eq!(eff.servers.len(), 2);
    }

    #[test]
    fn registry_roundtrips_through_the_store_without_env_values() {
        let mut reg = McpRegistry::default();
        let mut c = cfg("secret", "/bin/secret");
        c.env
            .insert("API_TOKEN".to_string(), "super-secret".to_string());
        reg.sync_and_effective(&[c], &[], 7);
        assert_eq!(
            reg.get("secret").unwrap().env_keys,
            vec!["API_TOKEN".to_string()]
        );

        let store = openrt_persistence::MemoryStore::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            reg.save(&store).await.unwrap();
            let loaded = McpRegistry::load(&store).await.unwrap();
            let rec = loaded.get("secret").unwrap();
            assert_eq!(rec.env_keys, vec!["API_TOKEN".to_string()]);
            let raw = store
                .get_record(MCP_SERVERS_NS, "secret")
                .await
                .unwrap()
                .unwrap();
            let rendered = raw.to_string();
            assert!(
                !rendered.contains("super-secret"),
                "env VALUES must never be persisted: {rendered}"
            );
        });
    }

    #[test]
    fn escalation_requires_clean_scan_demotion_never_does() {
        let mut reg = McpRegistry::default();
        reg.register("cli", "/bin/cli", vec![], vec![], vec![], "op", 1)
            .unwrap();
        let err = reg.set_trust("cli", McpTrust::Trusted, 2).unwrap_err();
        assert!(matches!(err, McpRegistryError::TrustRequiresCleanScan(..)));
        reg.set_scan_result("cli", SECURITY_CLEAN, vec![], 3)
            .unwrap();
        reg.set_trust("cli", McpTrust::Trusted, 4).unwrap();
        // flagged scan demotes trusted → review automatically
        reg.set_scan_result("cli", SECURITY_FLAGGED, vec!["bad".into()], 5)
            .unwrap();
        assert_eq!(reg.get("cli").unwrap().trust, McpTrust::Review);
        reg.set_trust("cli", McpTrust::Blocked, 6).unwrap();
        assert_eq!(reg.get("cli").unwrap().trust, McpTrust::Blocked);
    }

    #[test]
    fn register_validates_shape_and_refuses_remote_transports() {
        let mut reg = McpRegistry::default();
        assert!(matches!(
            reg.register("Bad.Name", "/bin/x", vec![], vec![], vec![], "op", 1),
            Err(McpRegistryError::InvalidName(_))
        ));
        assert!(matches!(
            reg.register(
                "httpish",
                "https://example.com/mcp",
                vec![],
                vec![],
                vec![],
                "op",
                1
            ),
            Err(McpRegistryError::HttpTransportUnavailable(_))
        ));
        reg.register("ok", "/bin/x", vec![], vec![], vec![], "op", 1)
            .unwrap();
        assert!(matches!(
            reg.register("ok", "/bin/x", vec![], vec![], vec![], "op", 2),
            Err(McpRegistryError::Duplicate(_))
        ));
        assert!(matches!(
            reg.get("nope"),
            Err(McpRegistryError::NotFound(_))
        ));
    }

    #[test]
    fn config_inline_env_reaches_the_spawn_config_but_never_the_record() {
        let mut reg = McpRegistry::default();
        let mut c = cfg("envsrv", "/bin/envsrv");
        c.env
            .insert("HOME_KEY".to_string(), "inline-value".to_string());
        let (eff, _) = reg.sync_and_effective(&[c], &[], 1);
        assert_eq!(
            eff.servers[0].env.get("HOME_KEY").map(String::as_str),
            Some("inline-value"),
            "config inline env spawns (config source)"
        );
        let rec = reg.get("envsrv").unwrap();
        assert_eq!(rec.env_keys, vec!["HOME_KEY".to_string()]);
        assert!(rec.env_keys.iter().all(|k| k != "inline-value"));
    }

    #[test]
    fn status_value_renders_keys_only_and_the_summary() {
        let mut reg = McpRegistry::default();
        reg.sync_and_effective(&[cfg("a", "/bin/a")], &[], 1);
        reg.register("b", "/bin/b", vec![], vec!["K".into()], vec![], "op", 2)
            .unwrap();
        let v = reg.status_value();
        assert_eq!(v["summary"]["total"], 2);
        assert_eq!(v["summary"]["enabled"], 1);
        assert_eq!(v["summary"]["connectable"], 1);
        let rendered = v.to_string();
        assert!(!rendered.contains("env\""));
    }
}
