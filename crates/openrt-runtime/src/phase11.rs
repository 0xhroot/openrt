//! Phase 11 runtime surface (M11.2 first): operator-facing lifecycle over
//! the durable `mcp.servers` registry — registration, enable/disable,
//! 4-level trust, AgentShield scan, and health probes.
//!
//! Every mutating action is audited through the ONE audit log; every scan
//! runs through the ONE AgentShield scanner and persists through the ONE
//! shield store; nothing here creates a second registry, policy, or store.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use openrt_config::{McpConfig, McpServerConfig, McpTrust};
use openrt_mcp::store::{
    now_ms, resolve_env_keys, McpDiscoveredTool, McpHealthReport, McpOrigin, McpRegistry,
    SourceDecl, SECURITY_CLEAN, SECURITY_FLAGGED,
};
use openrt_persistence::PersistenceStore;
use openrt_workspace_assets::AssetStore;

use crate::doctor::{DoctorCheck, DoctorStatus};
use crate::runtime::Runtime;
use crate::Error;

/// Sync the declarative sources (operator config + Active extension MCP
/// servers) into the `mcp.servers` registry and return the effective spawn
/// config. Called from `Runtime::build` — the registry is the ONE view of
/// MCP servers; the manager connects only what the registry's fail-closed
/// gate allows (enabled + connectable trust + not flagged).
pub(crate) async fn sync_mcp_registry(
    store: &dyn PersistenceStore,
    config: &McpConfig,
    assets: &AssetStore,
) -> Result<McpConfig, Error> {
    let mut reg = McpRegistry::load(store)
        .await
        .map_err(|e| Error::Mcp(e.to_string()))?;
    let ext_decls: Vec<SourceDecl> = assets
        .active_extension_mcp_servers()
        .into_iter()
        .map(|(id, s)| SourceDecl {
            name: s.name,
            command: s.command,
            args: s.args,
            env_keys: Vec::new(),
            env: BTreeMap::new(),
            declared_capabilities: s.declared_capabilities,
            declared_by: format!("extension {}", id.0),
            origin: McpOrigin::Extension,
            // Extensions can never self-elevate trust — seeded untrusted.
            first_sight_trust: McpTrust::Untrusted,
        })
        .collect();
    let (effective, dirty) = reg.sync_and_effective(&config.servers, &ext_decls, now_ms());
    if dirty {
        reg.save(store)
            .await
            .map_err(|e| Error::Mcp(e.to_string()))?;
    }
    Ok(effective)
}

impl Runtime {
    // ---- registry plumbing ----

    /// Load the durable MCP registry (fail-closed on corrupt records).
    pub async fn mcp_registry_load(&self) -> Result<McpRegistry, Error> {
        McpRegistry::load(&*self.store)
            .await
            .map_err(|e| Error::Mcp(e.to_string()))
    }

    async fn mcp_registry_save(&self, reg: &McpRegistry) -> Result<(), Error> {
        reg.save(&*self.store)
            .await
            .map_err(|e| Error::Mcp(e.to_string()))
    }

    // ---- M11.2 lifecycle ----

    /// Inventory of every registered MCP server (trust, enabled, security
    /// verdict, health, provenance — env key names only).
    pub async fn mcp_states(&self) -> Result<Value, Error> {
        let reg = self.mcp_registry_load().await?;
        Ok(reg.status_value())
    }

    /// One server's full record, plus this build's live connection view and
    /// source presence (honest about config/extension declarations).
    pub async fn mcp_inspect(&self, name: &str) -> Result<Value, Error> {
        let reg = self.mcp_registry_load().await?;
        let rec = reg
            .get(name)
            .map_err(|e| Error::Mcp(e.to_string()))?
            .clone();
        let mut value = serde_json::to_value(&rec).map_err(|e| Error::Mcp(e.to_string()))?;
        let live = self.mcp.status_value();
        if let Some(entry) = live
            .get("servers")
            .and_then(|s| s.as_array())
            .and_then(|a| {
                a.iter()
                    .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(name))
            })
        {
            value["live"] = entry.clone();
        }
        let assets = self.load_assets().await?;
        let ext_declared = assets
            .active_extension_mcp_servers()
            .iter()
            .any(|(_, s)| s.name == name);
        value["source"] = json!({
            "in_config": self
                .config
                .fabric
                .mcp
                .servers
                .iter()
                .any(|s| s.name == name),
            "in_active_extension": ext_declared,
            "origin": rec.origin,
        });
        Ok(value)
    }

    /// CLI registration: `Registered/unscanned` + **disabled** + `untrusted`
    /// — a registered server never arrives connectable (scan → enable →
    /// trust are separate operator actions).
    pub async fn mcp_register(
        &self,
        name: &str,
        command: &str,
        args: Vec<String>,
        env_keys: Vec<String>,
        declared_capabilities: Vec<String>,
        operator: &str,
    ) -> Result<(), Error> {
        if operator.trim().is_empty() {
            return Err(Error::Mcp(format!(
                "operator id required to register mcp server `{name}`"
            )));
        }
        if self
            .config
            .fabric
            .mcp
            .servers
            .iter()
            .any(|s| s.name == name)
        {
            return Err(Error::Mcp(format!(
                "mcp server `{name}` is declared in config.fabric.mcp — config owns its \
                 connection details; use `openrt mcp scan|trust|enable|disable {name}`"
            )));
        }
        let mut reg = self.mcp_registry_load().await?;
        reg.register(
            name,
            command,
            args,
            env_keys,
            declared_capabilities,
            operator,
            now_ms(),
        )
        .map_err(|e| Error::Mcp(e.to_string()))?;
        self.mcp_registry_save(&reg).await?;
        self.audit(
            operator,
            "mcp.register",
            name,
            "registered",
            json!({ "origin": "cli", "enabled": false, "trust": "untrusted" }),
        )
        .await?;
        Ok(())
    }

    /// Operator enable: runs the ONE AgentShield scan FIRST (flagged →
    /// fail closed, flag persisted), then enables. Connection still happens
    /// on the next build through the registry spawn gate.
    pub async fn mcp_enable(&self, name: &str, operator: &str) -> Result<(), Error> {
        if operator.trim().is_empty() {
            return Err(Error::Mcp(format!(
                "operator id required to enable mcp server `{name}`"
            )));
        }
        let mut reg = self.mcp_registry_load().await?;
        // Fails fast with NotFound before any mutation.
        reg.get(name).map_err(|e| Error::Mcp(e.to_string()))?;
        let report = match self.mcp_scan_update(&mut reg, name).await {
            Ok(r) => r,
            Err(e) => {
                // Persist whatever state was recorded (fail-closed durable).
                self.mcp_registry_save(&reg).await?;
                return Err(e);
            }
        };
        if report.action >= openrt_shield::ShieldAction::RequireApproval {
            self.mcp_registry_save(&reg).await?;
            return Err(Error::Mcp(format!(
                "mcp server `{name}` blocked by security scan ({}): {}",
                report.action.as_str(),
                report
                    .findings
                    .iter()
                    .map(|f| format!("{}: {}", f.category.as_str(), f.reason))
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
        reg.set_enabled(name, true, now_ms())
            .map_err(|e| Error::Mcp(e.to_string()))?;
        self.mcp_registry_save(&reg).await?;
        self.audit(operator, "mcp.enable", name, "enabled", json!({}))
            .await?;
        Ok(())
    }

    /// Operator disable (connection stops on the next build).
    pub async fn mcp_disable(&self, name: &str, operator: &str) -> Result<(), Error> {
        if operator.trim().is_empty() {
            return Err(Error::Mcp(format!(
                "operator id required to disable mcp server `{name}`"
            )));
        }
        let mut reg = self.mcp_registry_load().await?;
        reg.set_enabled(name, false, now_ms())
            .map_err(|e| Error::Mcp(e.to_string()))?;
        self.mcp_registry_save(&reg).await?;
        self.audit(operator, "mcp.disable", name, "disabled", json!({}))
            .await?;
        Ok(())
    }

    /// Operator trust change: `untrusted | review | trusted | blocked`.
    /// Escalation to `trusted` requires a clean scan verdict; trust never
    /// bypasses permissions/policy/sandbox/mode at the gate.
    pub async fn mcp_set_trust(
        &self,
        name: &str,
        level: &str,
        operator: &str,
    ) -> Result<(), Error> {
        if operator.trim().is_empty() {
            return Err(Error::Mcp(format!(
                "operator id required to change trust for mcp server `{name}`"
            )));
        }
        let trust = McpTrust::parse(level).ok_or_else(|| {
            Error::Mcp(format!(
                "unknown trust level `{level}`: use untrusted | review | trusted | blocked"
            ))
        })?;
        let mut reg = self.mcp_registry_load().await?;
        let previous = reg
            .set_trust(name, trust, now_ms())
            .map_err(|e| Error::Mcp(e.to_string()))?;
        self.mcp_registry_save(&reg).await?;
        self.audit(
            operator,
            "mcp.trust",
            name,
            trust.as_str(),
            json!({ "previous": previous.as_str() }),
        )
        .await?;
        Ok(())
    }

    /// Advisory scan with the ONE AgentShield scanner: records the verdict
    /// on the registry record (flagged demotes `trusted` → `review` — new
    /// findings never leave stale trust standing). Returns the report so
    /// the caller can SEE the findings; the CLI maps action → exit.
    pub async fn mcp_scan(&self, name: &str) -> Result<openrt_shield::ShieldReport, Error> {
        let mut reg = self.mcp_registry_load().await?;
        let report = self.mcp_scan_update(&mut reg, name).await;
        // Persist the record update even when the caller will fail on it —
        // fail-closed state must be durable.
        self.mcp_registry_save(&reg).await?;
        report
    }

    /// Health probe: connect + initialize + `tools/list` within the
    /// fail-closed timeouts, then shutdown. Persists the verdict and the
    /// discovered-tools snapshot (registry-recorded status/health/tools).
    pub async fn mcp_health(&self, name: &str) -> Result<Value, Error> {
        let mut reg = self.mcp_registry_load().await?;
        let rec = reg
            .get(name)
            .map_err(|e| Error::Mcp(e.to_string()))?
            .clone();
        let env = match self
            .config
            .fabric
            .mcp
            .servers
            .iter()
            .find(|s| s.name == name)
        {
            Some(c) => c.env.clone(),
            None => resolve_env_keys(&rec.env_keys),
        };
        let cfg = McpServerConfig {
            name: rec.name.clone(),
            command: rec.command.clone(),
            args: rec.args.clone(),
            env,
            trust: rec.trust,
            declared_capabilities: rec.declared_capabilities.clone(),
        };
        let at = now_ms();
        let (health, tools) = match openrt_mcp::McpClient::start(&cfg) {
            Ok(client) => {
                let listed = client.listed_tools();
                let server_info = client.server_info();
                client.shutdown();
                let sanitize = |t: &str| -> String {
                    t.chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect()
                };
                let tools: Vec<McpDiscoveredTool> = listed
                    .iter()
                    .map(|t| McpDiscoveredTool {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        registered_as: t
                            .rejected
                            .is_none()
                            .then(|| format!("mcp.{}.{}", rec.name, sanitize(&t.name))),
                        rejected: t.rejected.clone(),
                    })
                    .collect();
                let tool_count = tools.len() as u32;
                (
                    McpHealthReport {
                        ok: true,
                        detail: "connected + initialize + tools/list succeeded".to_string(),
                        at_ms: at,
                        server_info,
                        tool_count,
                    },
                    tools,
                )
            }
            Err(e) => (
                McpHealthReport {
                    ok: false,
                    detail: format!("{} — {}", e.what, e.why),
                    at_ms: at,
                    server_info: String::new(),
                    tool_count: 0,
                },
                Vec::new(),
            ),
        };
        let value = serde_json::to_value(&health).map_err(|e| Error::Mcp(e.to_string()))?;
        reg.set_probe_result(name, health, tools, at)
            .map_err(|e| Error::Mcp(e.to_string()))?;
        self.mcp_registry_save(&reg).await?;
        Ok(json!({ "name": name, "probe": value }))
    }

    /// Run the ONE AgentShield scan over one server's declaration and
    /// record the verdict on the registry record (mutates `reg`; caller
    /// saves). Mirrors `extension_scan_update` — only this path ever sets
    /// a server's security status.
    async fn mcp_scan_update(
        &self,
        reg: &mut McpRegistry,
        name: &str,
    ) -> Result<openrt_shield::ShieldReport, Error> {
        let rec = reg
            .get(name)
            .map_err(|e| Error::Mcp(e.to_string()))?
            .clone();
        let report = openrt_shield::scan_extension(
            &openrt_shield::ExtensionScanInput::new(&rec.name, "mcp").with_manifest(json!({
                "name": rec.name,
                "command": rec.command,
                "args": rec.args,
                "declared_capabilities": rec.declared_capabilities,
                "trust": rec.trust.as_str(),
                "origin": rec.origin,
            })),
        );
        let persist = |status: openrt_shield::FindingStatus| {
            let report = report.clone();
            async move {
                openrt_shield::store::record_enforced(&*self.store, &report, status)
                    .await
                    .map_err(|e| {
                        Error::Mcp(format!(
                            "shield mcp-scan persistence failed (fail closed): {e}"
                        ))
                    })
            }
        };
        if report.action >= openrt_shield::ShieldAction::RequireApproval {
            persist(openrt_shield::FindingStatus::Denied).await?;
        } else if !report.is_clean() {
            persist(openrt_shield::FindingStatus::Reported).await?;
        }
        let status = if report.action >= openrt_shield::ShieldAction::RequireApproval {
            SECURITY_FLAGGED
        } else {
            SECURITY_CLEAN
        };
        let findings = report
            .findings
            .iter()
            .map(|f| format!("{}: {}", f.category.as_str(), f.reason))
            .collect();
        reg.set_scan_result(name, status, findings, now_ms())
            .map_err(|e| Error::Mcp(e.to_string()))?;
        Ok(report)
    }

    // ---- doctor rows (M11.2) ----

    /// MCP registry/connections/transport doctor rows (never a `Fail` for
    /// absence — pending reviews and probe failures surface as `Warn`,
    /// remote/HTTP transport as honest `Unavailable`).
    pub async fn mcp_doctor_checks(&self) -> Vec<DoctorCheck> {
        let mut out = Vec::new();
        let reg = match self.mcp_registry_load().await {
            Ok(r) => r,
            Err(e) => {
                return vec![DoctorCheck {
                    name: "mcp.registry".to_string(),
                    status: DoctorStatus::Fail,
                    detail: format!("registry records unreadable (fail closed): {e}"),
                }];
            }
        };
        let summary = reg
            .status_value()
            .get("summary")
            .cloned()
            .unwrap_or_default();
        let total = summary.get("total").and_then(Value::as_u64).unwrap_or(0);
        let review = summary.get("review").and_then(Value::as_u64).unwrap_or(0);
        let blocked = summary.get("blocked").and_then(Value::as_u64).unwrap_or(0);
        let flagged = summary.get("flagged").and_then(Value::as_u64).unwrap_or(0);
        let enabled = summary.get("enabled").and_then(Value::as_u64).unwrap_or(0);
        let connectable = summary
            .get("connectable")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let (status, detail) = if review > 0 || flagged > 0 {
            (
                DoctorStatus::Warn,
                format!(
                    "{total} record(s): {enabled} enabled, {connectable} connectable; \
                     {review} awaiting review, {flagged} flagged, {blocked} blocked \
                     (operator action pending — nothing escalates itself)"
                ),
            )
        } else {
            (
                DoctorStatus::Pass,
                format!(
                    "{total} record(s): {enabled} enabled, {connectable} connectable, \
                     {blocked} blocked (deny-by-default policy gate unchanged: \
                     mcp.invoke.all + mcp.<server>.invoke required per call)"
                ),
            )
        };
        out.push(DoctorCheck {
            name: "mcp.registry".to_string(),
            status,
            detail,
        });

        let live = self.mcp.status_value();
        let configured = live.get("connected").and_then(Value::as_u64).unwrap_or(0);
        let failed = live.get("failed").and_then(Value::as_u64).unwrap_or(0);
        let attempted = live.get("configured").and_then(Value::as_u64).unwrap_or(0);
        let (status, detail) = if failed > 0 {
            (
                DoctorStatus::Warn,
                format!("{failed} of {attempted} configured server(s) failed to connect (per-server fail-closed: their tools are simply absent)"),
            )
        } else {
            (
                DoctorStatus::Pass,
                format!(
                    "{attempted} server(s) attempted at wiring, {configured} connected, \
                     0 failed (fail-closed per server)"
                ),
            )
        };
        out.push(DoctorCheck {
            name: "mcp.connections".to_string(),
            status,
            detail,
        });

        out.push(DoctorCheck {
            name: "mcp.remote-transport".to_string(),
            status: DoctorStatus::Unavailable,
            detail:
                "remote/HTTP MCP is UNAVAILABLE (stdio only) — netpol SCOPE_MCP is reserved \
                 and never silently enabled; registry registration of http(s):// commands is refused"
                    .to_string(),
        });
        out
    }

    /// Phase 11 M11.10 ComputerUse rows — `computer.vision` (the ONE
    /// grounding backend), `computer.grounding` (resolution-chain sources),
    /// `computer.twin` (in-memory observation model), `computer.verify`
    /// (observe→compare readiness with bounded retry ≤3). Probes, never
    /// Fail: an honest `Unavailable` carries the real reason — no
    /// fabricated availability.
    /// Read-only ComputerUse environment snapshot for UI surfaces (M11.6 /
    /// M11.7): backend kinds, resolved profile + resolver decisions, and the
    /// honest capability matrix — the same data `openrt computer env` prints.
    /// Views only: no mutation, no clipboard read, no second authority.
    pub fn computer_env_snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "summary": self.computer.summary(),
            "profile": self.computer.profile().to_value(),
            "resolver": self.computer.decisions(),
            "capabilities": self.computer.capability_matrix().rows(),
            "screen_backend": self.screen.kind(),
        })
    }

    /// Gate-dispatched `computer.use` for UI surfaces (M11.7): the SAME
    /// registered tool, permission policy, AgentShield scan, sandbox/kill
    /// scope `computer`, and audit row as the CLI. Surfaces never execute
    /// [`openrt_computer::ComputerUse`] directly — GUI is never a bypass.
    pub async fn computer_use_dispatch(
        &self,
        channel: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, openrt_tools::ToolError> {
        use openrt_tools::{ToolExecutionContext, ToolOutput};
        let out = self
            .executor
            .execute(
                "computer.use",
                args,
                &ToolExecutionContext {
                    session_id: None,
                    task_id: None,
                    channel: Some(channel.to_string()),
                },
            )
            .await?;
        Ok(match out {
            ToolOutput::Json(v) => v,
            ToolOutput::Text(t) => serde_json::json!({ "text": t }),
        })
    }

    pub async fn computeruse_doctor_checks(&self) -> Vec<DoctorCheck> {
        let mut out = Vec::new();

        // computer.vision: the single VLM grounding pipeline (points only —
        // the VLM never executes; same endpoint probe as `vision.vlm`).
        // Phase 13: egress is checked against the ONE authority first — a
        // policy refusal is reported as the honest reason, never probed.
        let endpoint = self
            .config
            .providers
            .iter()
            .find(|p| p.kind == openrt_config::ProviderKind::Ollama)
            .and_then(|p| p.base_url.clone())
            .unwrap_or_else(|| openrt_vlm::DEFAULT_VLM_ENDPOINT.to_string());
        let np = self.netpol();
        let (available, detail) =
            match openrt_fabric::net_guard::ensure_url_allowed(
                &endpoint,
                openrt_fabric::net_guard::SCOPE_MODEL,
                &self.modes,
                &np,
            )
            .await
            {
                Err(e) => (
                    false,
                    format!(
                        "grounding backend (points only, never executes): egress refused — not probed: {e}"
                    ),
                ),
                Ok(()) => {
                    let vstatus =
                        openrt_vlm::probe_vision(&endpoint, openrt_vlm::DEFAULT_VLM_MODEL).await;
                    (
                        vstatus.is_available(),
                        format!(
                            "grounding backend (points only, never executes) model `{}`: {vstatus}",
                            openrt_vlm::DEFAULT_VLM_MODEL
                        ),
                    )
                }
            };
        out.push(DoctorCheck {
            name: "computer.vision".into(),
            status: if available {
                DoctorStatus::Pass
            } else {
                DoctorStatus::Unavailable
            },
            detail,
        });

        // computer.grounding: which chain sources actually exist right now.
        let screen_kind = self.screen.kind();
        let a11y = self
            .computer
            .capability_matrix()
            .rows()
            .iter()
            .find(|r| {
                r.capability == openrt_platform::ComputerCapability::AccessibilityTree.as_str()
            })
            .map(|r| r.status.as_str())
            .unwrap_or("UNAVAILABLE");
        let (status, detail) = match self.screen.capture().await {
            Ok(cap) => {
                let structured = cap.elements.len();
                let vision_ok = available;
                if structured == 0 && !vision_ok {
                    (
                        DoctorStatus::Unavailable,
                        format!(
                            "screen `{screen_kind}`: 0 structured elements and vision \
                             unavailable — only explicit-coordinate resolution remains"
                        ),
                    )
                } else {
                    (
                        DoctorStatus::Pass,
                        format!(
                            "chain: accessibility={a11y} (no a11y source wired), \
                             window/elements={structured} structured, vision={}, \
                             coordinate=fallback; capture {}x{} image={}",
                            if vision_ok {
                                "available"
                            } else {
                                "unavailable"
                            },
                            cap.width,
                            cap.height,
                            if cap.image_path.is_some() {
                                "yes"
                            } else {
                                "no"
                            },
                        ),
                    )
                }
            }
            Err(e) => (
                DoctorStatus::Unavailable,
                format!("screen capture unavailable: {e}"),
            ),
        };
        out.push(DoctorCheck {
            name: "computer.grounding".into(),
            status,
            detail,
        });

        // computer.twin: probe one fresh observation bundle (the twin is a
        // derived, per-session in-memory model — invalidated after every
        // action; never persisted as a second store).
        let twin = openrt_computer::ComputerUse::new(self.screen.clone(), self.computer.clone());
        match twin.observe(false).await {
            Ok(b) => out.push(DoctorCheck {
                name: "computer.twin".into(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "digital twin probe: generation {} at {} — in-memory per session, \
                     invalidated after every action (windows={} elements={} cursor={:?})",
                    b.twin_generation,
                    b.captured_at,
                    b.windows.len(),
                    b.elements.len(),
                    b.cursor
                ),
            }),
            Err(e) => out.push(DoctorCheck {
                name: "computer.twin".into(),
                status: DoctorStatus::Unavailable,
                detail: format!("twin observation unavailable: {e}"),
            }),
        }

        // computer.verify: the observe→compare signal the verification step
        // relies on, with the bounded-retry contract surfaced.
        match self.computer.observe().await {
            Ok(obs) => out.push(DoctorCheck {
                name: "computer.verify".into(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "verification ready: observe→compare with bounded retry ≤{} \
                     (active_window={} cursor={:?} monitors={})",
                    openrt_computer::MAX_ATTEMPTS,
                    obs.active_window
                        .as_ref()
                        .map(|w| w.id.as_str())
                        .unwrap_or("<none>"),
                    obs.cursor,
                    obs.monitors.len()
                ),
            }),
            Err(e) => out.push(DoctorCheck {
                name: "computer.verify".into(),
                status: DoctorStatus::Unavailable,
                detail: format!("observation unavailable: {e}"),
            }),
        }

        out
    }

    /// Phase 11 Part Q: workbench posture rows — real counts from the ONE
    /// Phase 11 Part Q: M11.10 surface rows — extensions, marketplace,
    /// MCP ecosystem, and the workbench model. Real counts from the ONE
    /// store (records, not backends: readable = healthy; a digest drift or
    /// unreadable namespace is WARN, never a hidden state; rows never Fail).
    pub async fn phase11_doctor_checks(&self) -> Vec<DoctorCheck> {
        let mut out = Vec::new();

        // extensions (M11.1): inventory + security/activation honesty.
        match self.extension_states().await {
            Ok(states) => {
                let total = states.len();
                let active = states
                    .iter()
                    .filter(|s| s["activation"].as_str() == Some("active"))
                    .count();
                let flagged = states
                    .iter()
                    .filter(|s| {
                        s["security"]
                            .as_str()
                            .is_some_and(|sec| sec.contains("flagged") || sec.contains("denied"))
                    })
                    .count();
                let (status, detail) = if flagged > 0 {
                    (
                        DoctorStatus::Warn,
                        format!(
                            "{total} extension version(s): {active} active, {flagged} flagged \
                             (operator action pending — nothing activates itself; imported \
                             extensions land Registered/unscanned)"
                        ),
                    )
                } else {
                    (
                        DoctorStatus::Pass,
                        format!(
                            "{total} extension version(s): {active} active, 0 flagged \
                             (deny-by-default: activation requires validate + scan + \
                             operator approval)"
                        ),
                    )
                };
                out.push(DoctorCheck {
                    name: "extensions".to_string(),
                    status,
                    detail,
                });
            }
            Err(e) => out.push(DoctorCheck {
                name: "extensions".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("extension registry unreadable: {e}"),
            }),
        }

        // marketplace (M11.10): local catalog GREEN; remote honest
        // UNAVAILABLE/BLOCKED — never a fake empty remote catalog.
        match self.skill_marketplace_catalog().await {
            Ok(entries) => out.push(DoctorCheck {
                name: "marketplace".to_string(),
                status: DoctorStatus::Pass,
                detail: format!(
                    "local catalog: {} entr(y/ies); remote fetch: UNAVAILABLE/BLOCKED \
                     (this build ships no remote client — Sovereign policy + netpol refuse \
                     network catalog fetches; never a fabricated listing)",
                    entries.len()
                ),
            }),
            Err(e) => out.push(DoctorCheck {
                name: "marketplace".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("local marketplace catalog unreadable: {e}"),
            }),
        }

        // mcp.ecosystem (M11.10 aggregate): trust + lifecycle in one row.
        match self.mcp_registry_load().await {
            Ok(reg) => {
                let summary = reg
                    .status_value()
                    .get("summary")
                    .cloned()
                    .unwrap_or_default();
                let total = summary.get("total").and_then(Value::as_u64).unwrap_or(0);
                let review = summary.get("review").and_then(Value::as_u64).unwrap_or(0);
                let blocked = summary.get("blocked").and_then(Value::as_u64).unwrap_or(0);
                let flagged = summary.get("flagged").and_then(Value::as_u64).unwrap_or(0);
                let enabled = summary.get("enabled").and_then(Value::as_u64).unwrap_or(0);
                let connectable = summary
                    .get("connectable")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let (status, detail) = if review > 0 || flagged > 0 {
                    (
                        DoctorStatus::Warn,
                        format!(
                            "{total} server(s): {enabled} enabled, {connectable} connectable; \
                             {review} awaiting review, {flagged} flagged, {blocked} blocked; \
                             transport: stdio only (remote HTTP refused, netpol SCOPE_MCP \
                             reserved)"
                        ),
                    )
                } else {
                    (
                        DoctorStatus::Pass,
                        format!(
                            "{total} server(s): {enabled} enabled, {connectable} connectable, \
                             {blocked} blocked; transport: stdio only (remote HTTP refused, \
                             netpol SCOPE_MCP reserved)"
                        ),
                    )
                };
                out.push(DoctorCheck {
                    name: "mcp.ecosystem".to_string(),
                    status,
                    detail,
                });
            }
            Err(e) => out.push(DoctorCheck {
                name: "mcp.ecosystem".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("mcp registry unreadable: {e}"),
            }),
        }

        // rooms + workspaces (extend, never duplicate the M5.1 records).
        match (
            openrt_workspace::list_rooms(&*self.store).await,
            openrt_workspace::list_workspaces(&*self.store).await,
        ) {
            (Ok(rooms), Ok(wss)) => {
                let active = rooms
                    .iter()
                    .filter(|r| matches!(r.status, openrt_workspace::RoomStatus::Active))
                    .count();
                let driving = rooms.iter().filter(|r| r.active_workflow.is_some()).count();
                out.push(DoctorCheck {
                    name: "rooms".to_string(),
                    status: DoctorStatus::Pass,
                    detail: format!(
                        "{active} active / {} room(s) across {} workspace(s); \
                         {driving} driving a workflow (bookkeeping pointers only — \
                         checkpoints remain the run authority)",
                        rooms.len(),
                        wss.len()
                    ),
                });
            }
            (Err(e), _) | (_, Err(e)) => out.push(DoctorCheck {
                name: "rooms".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("room/workspace records unreadable on the store: {e}"),
            }),
        }

        // materials + live digest re-derivation (fail-closed drift detection).
        match openrt_workspace::list_materials(&*self.store).await {
            Ok(materials) => {
                let mut drifted = 0usize;
                let mut unresolvable = 0usize;
                for m in &materials {
                    match openrt_workspace::verify_material(&*self.store, &m.id).await {
                        Ok(true) => {}
                        Ok(false) => drifted += 1,
                        Err(_) => unresolvable += 1,
                    }
                }
                let (status, detail) = if drifted == 0 && unresolvable == 0 {
                    (
                        DoctorStatus::Pass,
                        format!(
                            "{} material(s) sealed and intact (digest re-derived from the live \
                             reference just now)",
                            materials.len()
                        ),
                    )
                } else {
                    (
                        DoctorStatus::Warn,
                        format!(
                            "{} material(s): {drifted} drifted, {unresolvable} unresolvable — \
                             verification failed closed (re-register or repair the reference)",
                            materials.len()
                        ),
                    )
                };
                out.push(DoctorCheck {
                    name: "workbench.materials".to_string(),
                    status,
                    detail,
                });
            }
            Err(e) => out.push(DoctorCheck {
                name: "workbench.materials".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("material records unreadable on the store: {e}"),
            }),
        }

        // pause/recovery posture: chains whose latest checkpoint is still
        // Running (paused/interrupted — boot RECORDS them, never resumes)
        // plus any pause flag live in THIS process.
        let flags = self
            .workflow_pause
            .lock()
            .map(|m| m.values().filter(|(_, active)| *active).count())
            .unwrap_or(0);
        match self.list_checkpoints(None).await {
            Ok(mut cps) => {
                cps.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                let mut incomplete: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                for cp in &cps {
                    if matches!(cp.status, openrt_checkpoint::CheckpointStatus::Running) {
                        incomplete.insert(cp.workflow_id.clone());
                    } else {
                        incomplete.remove(&cp.workflow_id);
                    }
                }
                out.push(DoctorCheck {
                    name: "workbench.recovery".to_string(),
                    status: DoctorStatus::Pass,
                    detail: format!(
                        "{flags} pause flag(s) active in this process; {} incomplete \
                         chain(s) awaiting explicit `workflow resume` (boot recovery records \
                         them as interrupted — never auto-resumes)",
                        incomplete.len()
                    ),
                });
            }
            Err(e) => out.push(DoctorCheck {
                name: "workbench.recovery".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("checkpoint chain unreadable on the store: {e}"),
            }),
        }

        // steer: append-only operator directives hydrated at run start.
        match openrt_workspace::list_rooms(&*self.store).await {
            Ok(rooms) => {
                let total: usize = rooms.iter().map(|r| r.steer.len()).sum();
                let with_steer = rooms.iter().filter(|r| !r.steer.is_empty()).count();
                out.push(DoctorCheck {
                    name: "workbench.steer".to_string(),
                    status: DoctorStatus::Pass,
                    detail: format!(
                        "{total} directive(s) across {with_steer} room(s) (append-only; \
                         hydrated idempotently into `steer.<room>.<index>` blackboard keys \
                         at run start)"
                    ),
                });
            }
            Err(e) => out.push(DoctorCheck {
                name: "workbench.steer".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("room records unreadable on the store: {e}"),
            }),
        }

        // `workbench` umbrella (M11.10): one line that names the whole
        // model — workspace + rooms + materials + steer, all from the ONE
        // store (no second database, no UI-local state).
        let rooms_total = openrt_workspace::list_rooms(&*self.store).await;
        let materials_total = openrt_workspace::list_materials(&*self.store).await;
        let workspaces_total = openrt_workspace::list_workspaces(&*self.store).await;
        match (rooms_total, materials_total, workspaces_total) {
            (Ok(rooms), Ok(materials), Ok(wss)) => {
                let steer_total: usize = rooms.iter().map(|r| r.steer.len()).sum();
                out.push(DoctorCheck {
                    name: "workbench".to_string(),
                    status: DoctorStatus::Pass,
                    detail: format!(
                        "{} workspace(s), {} room(s), {} material(s), {steer_total} steer \
                         directive(s) — overlay on the one store (ADR-006: references + \
                         digests, never copies; reconstruction reads these records)",
                        wss.len(),
                        rooms.len(),
                        materials.len()
                    ),
                });
            }
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => out.push(DoctorCheck {
                name: "workbench".to_string(),
                status: DoctorStatus::Warn,
                detail: format!("workbench records unreadable on the store: {e}"),
            }),
        }

        out
    }
}
