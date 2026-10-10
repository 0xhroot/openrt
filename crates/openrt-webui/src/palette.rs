use std::collections::BTreeMap;
use std::sync::Arc;

use openrt_runtime::Runtime;
use openrt_selfimprove::{ImproveStore, OperatorGrant, ProposalState};
use serde_json::json;

use crate::WebUiError;

#[derive(Debug, Clone, PartialEq)]
pub struct PaletteCommand {
    pub id: String,
    pub title: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DispatchRequest {
    pub command_id: String,
    pub args: BTreeMap<String, String>,
    /// Server-issued bearer token (from `Authorization: Bearer …`). This is
    /// the ONLY authority for operator identity — a body `operator` field is
    /// never trusted (SEC: no client-asserted identity).
    pub auth_token: Option<String>,
}

impl DispatchRequest {
    pub fn new(command_id: &str) -> Self {
        Self {
            command_id: command_id.to_string(),
            args: BTreeMap::new(),
            auth_token: None,
        }
    }

    pub fn with_arg(mut self, key: &str, value: &str) -> Self {
        self.args.insert(key.to_string(), value.to_string());
        self
    }

    pub fn with_auth_token(mut self, token: &str) -> Self {
        self.auth_token = Some(token.to_string());
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaletteOutcome {
    Completed { id: String, detail: String },
    Denied { id: String, reason: String },
}

/// Fetch a required string arg (missing args fail closed).
fn arg(req: &DispatchRequest, key: &str) -> Result<String, WebUiError> {
    req.args
        .get(key)
        .cloned()
        .ok_or_else(|| WebUiError::BadRequest(format!("missing arg {key}")))
}

/// Shared core the palette dispatches against. When a production `Runtime`
/// is attached, approval mutations go through the runtime's durable
/// `improve_*` methods (same store the headless CLI uses). The local
/// `ImproveStore` clone remains for read-only listing when no runtime is
/// present (tests / offline palette construction).
pub struct CommandPalette {
    commands: BTreeMap<String, PaletteCommand>,
    store: ImproveStore,
    /// Production runtime (M5.8): durable approve/reject through the same
    /// store-backed path as headless. `None` only for pure unit tests.
    runtime: Option<Arc<Runtime>>,
    /// Tokio handle for blocking calls from the sync server thread.
    rt_handle: Option<tokio::runtime::Handle>,
    /// Server-issued bearer token → allowlisted operator id. Tokens are
    /// minted by [`CommandPalette::issue_token`] (CSPRNG) and never come
    /// from the client body.
    tokens: BTreeMap<String, String>,
    /// Real bound address (set by `serve` after listen); surfaced by
    /// `session.info` so the UI never reports a hardcoded placeholder.
    bind: String,
    /// True while the process is in Sovereign Mode (set by the runtime).
    sovereign: bool,
}

impl CommandPalette {
    /// Construct with a local ImproveStore only (unit tests / offline).
    pub fn new(store: ImproveStore) -> Self {
        let mut commands: BTreeMap<String, PaletteCommand> = BTreeMap::new();
        for cmd in [
            PaletteCommand {
                id: "session.info".into(),
                title: "Session info".into(),
                description: "Show real bind address, mode, and operator allowlist size".into(),
            },
            PaletteCommand {
                id: "improve.proposals".into(),
                title: "List improvement proposals".into(),
                description: "Show every proposal id and review state".into(),
            },
            PaletteCommand {
                id: "improve.approve".into(),
                title: "Approve proposal".into(),
                description: "Operator review gate: approve a proposal (requires bearer token)"
                    .into(),
            },
            PaletteCommand {
                id: "improve.reject".into(),
                title: "Reject proposal".into(),
                description: "Operator review gate: reject a proposal (requires bearer token)"
                    .into(),
            },
            PaletteCommand {
                id: "workspace.info".into(),
                title: "Workspace info".into(),
                description: "List durable workspaces from the shared store".into(),
            },
            PaletteCommand {
                id: "session.list".into(),
                title: "List sessions".into(),
                description: "Show sessions on the shared runtime store".into(),
            },
            PaletteCommand {
                id: "history.tail".into(),
                title: "History tail".into(),
                description: "Show the append-only ledger tail sequence".into(),
            },
            PaletteCommand {
                id: "history.timeline".into(),
                title: "History timeline".into(),
                description: "M10.5 time machine: verified session-scoped ledger timeline (seq, status, event kind)"
                    .into(),
            },
            PaletteCommand {
                id: "history.fork".into(),
                title: "Fork history branch".into(),
                description: "M10.5 time machine: fork an append-only what-if branch from the head (args: branch, base?; operator token)"
                    .into(),
            },
            PaletteCommand {
                id: "history.compare".into(),
                title: "Compare fork branch".into(),
                description: "M10.5: divergence verdict of a branch vs the parent ledger (arg: branch)"
                    .into(),
            },
            PaletteCommand {
                id: "history.state".into(),
                title: "State as-of sequence".into(),
                description: "M10.5: materialize the ledger as-of a sequence, read-only (arg: as_of)"
                    .into(),
            },
            PaletteCommand {
                id: "history.return".into(),
                title: "Return to current head".into(),
                description: "M10.5: leave the what-if view; tail + fork catalog of the parent ledger"
                    .into(),
            },
            PaletteCommand {
                id: "recovery.list".into(),
                title: "Boot recovery records".into(),
                description: "M10.2: list recovery records (interrupted/incomplete/stale counts per boot)"
                    .into(),
            },
            PaletteCommand {
                id: "shield.findings".into(),
                title: "AgentShield findings".into(),
                description: "M10.9: list recorded security findings (optional status filter)"
                    .into(),
            },
            PaletteCommand {
                id: "skill.inspect".into(),
                title: "Inspect skill".into(),
                description: "M10.6: full loader report for a skill id (checks, effective permissions, workflow readiness)"
                    .into(),
            },
            PaletteCommand {
                id: "memory.working.has".into(),
                title: "Working memory probe".into(),
                description: "Check whether a working-memory slot is live for a session".into(),
            },
            PaletteCommand {
                id: "assets.skills".into(),
                title: "List asset skills".into(),
                description: "Show skill id/activation pairs from the durable AssetStore".into(),
            },
            PaletteCommand {
                id: "task.list".into(),
                title: "List tasks".into(),
                description: "Show recent tasks from the shared store".into(),
            },
            PaletteCommand {
                id: "improve.states".into(),
                title: "Improve states".into(),
                description: "List proposal id/state pairs (same as improve.proposals)".into(),
            },
            PaletteCommand {
                id: "workflow.compile".into(),
                title: "Compile workflow".into(),
                description: "Compile a natural-language goal into a validated DAG (fails closed)"
                    .into(),
            },
            PaletteCommand {
                id: "workflow.list".into(),
                title: "List compiled workflows".into(),
                description: "Show compiled workflow ids and objectives from the shared store"
                    .into(),
            },
            PaletteCommand {
                id: "workflow.inspect".into(),
                title: "Inspect compiled workflow".into(),
                description: "Load a compiled record + DAG and show steps/provenance".into(),
            },
            PaletteCommand {
                id: "workflow.validate".into(),
                title: "Validate compiled workflow".into(),
                description: "Re-surface the 15-point validation report for a compiled id".into(),
            },
            // Phase 11 M11.6 workbench surfaces (views over the ONE store).
            PaletteCommand {
                id: "workbench.info".into(),
                title: "Workbench info".into(),
                description: "M11.6: one-glance workbench model totals (workspaces, rooms, materials, steer) from the shared store"
                    .into(),
            },
            PaletteCommand {
                id: "room.list".into(),
                title: "List rooms".into(),
                description: "M11.6: list workbench rooms (id, workbench, status, active workflow)".into(),
            },
            PaletteCommand {
                id: "room.create".into(),
                title: "Create room".into(),
                description: "M11.6: create a room in an existing workbench (args: workbench, title; operator token)"
                    .into(),
            },
            PaletteCommand {
                id: "room.show".into(),
                title: "Show room".into(),
                description: "M11.6: full room record — agents, links, steer, history (arg: id)".into(),
            },
            PaletteCommand {
                id: "room.status".into(),
                title: "Set room status".into(),
                description: "M11.6: set room lifecycle status (args: id, status=active|paused; operator token)"
                    .into(),
            },
            PaletteCommand {
                id: "room.steer".into(),
                title: "Steer room".into(),
                description: "M11.6: append an operator steer directive — append-only, hydrated at run start (args: id, input; operator token)"
                    .into(),
            },
            PaletteCommand {
                id: "room.agent".into(),
                title: "Assign room agent".into(),
                description: "M11.4: assign an agent to a room under a configured role (args: id, agent, role=researcher|analyst|coder|reviewer|verifier; operator token)"
                    .into(),
            },
            PaletteCommand {
                id: "material.list".into(),
                title: "List materials".into(),
                description: "M11.6: list material records (id, kind, title, source)".into(),
            },
            PaletteCommand {
                id: "material.verify".into(),
                title: "Verify material".into(),
                description: "M11.6: re-derive a material's digest from its live reference (arg: id)".into(),
            },
            PaletteCommand {
                id: "extension.list".into(),
                title: "List extensions".into(),
                description: "M11.6: installed extension versions with activation + security status (never Active on import)"
                    .into(),
            },
            PaletteCommand {
                id: "extension.inspect".into(),
                title: "Inspect extension".into(),
                description: "M11.1/M11.6: full extension manifest (arg: id; optional: version)".into(),
            },
            PaletteCommand {
                id: "mcp.list".into(),
                title: "List MCP servers".into(),
                description: "M11.2/M11.6: MCP server registry states (trust, enabled, health)".into(),
            },
            PaletteCommand {
                id: "mcp.trust".into(),
                title: "Set MCP trust".into(),
                description: "M11.2: set a server's trust level (args: name, trust=untrusted|review|trusted|blocked; operator token)"
                    .into(),
            },
            PaletteCommand {
                id: "mcp.enable".into(),
                title: "Enable MCP server".into(),
                description: "M11.2: enable a registry server (arg: name; operator token)".into(),
            },
            PaletteCommand {
                id: "mcp.disable".into(),
                title: "Disable MCP server".into(),
                description: "M11.2: disable a registry server (arg: name; operator token)".into(),
            },
            PaletteCommand {
                id: "computer.env".into(),
                title: "Computer environment".into(),
                description: "M11.7: screen/computer backends, resolver decisions, capability matrix (read-only)"
                    .into(),
            },
            PaletteCommand {
                id: "computer.observe".into(),
                title: "Computer observation".into(),
                description: "M11.7: one observation bundle through the SHARED engine; clipboard read ON DEMAND only (arg: clipboard=true)"
                    .into(),
            },
            PaletteCommand {
                id: "computer.use".into(),
                title: "Computer use action".into(),
                description: "M11.7: plan+execute one action through the single gate (args: action[, target,x,y,text,key,url,app,name,clipboard]; operator token)"
                    .into(),
            },
            // Phase 12 (M12): industrial intelligence + trading research +
            // learning surfaces — views over the ONE store, honest statuses.
            PaletteCommand {
                id: "phase12.info".into(),
                title: "Phase 12 overview".into(),
                description: "M12: one-glance industrial/trading/learning record counts + phase12 doctor tallies (read-only)"
                    .into(),
            },
            PaletteCommand {
                id: "sih.info".into(),
                title: "SIH case overview".into(),
                description: "Phase 13: CyberSaarthi case, evidence, graph, verification, security posture (read-only)"
                    .into(),
            },
            PaletteCommand {
                id: "industrial.assets".into(),
                title: "List industrial assets".into(),
                description: "M12.1: advisory industrial assets (id, kind, label) from the ONE store"
                    .into(),
            },
            PaletteCommand {
                id: "trading.datasets".into(),
                title: "List trading datasets".into(),
                description: "M12.9: research datasets (id, version, bars, instrument) from the ONE store"
                    .into(),
            },
            PaletteCommand {
                id: "trading.execute".into(),
                title: "Broker execute (always denied)".into(),
                description: "M12.13: research-only surface — always DENIED, never places an order (args: order)"
                    .into(),
            },
            PaletteCommand {
                id: "learn.episodes".into(),
                title: "List experience episodes".into(),
                description: "M12.16: sealed experience episodes (id, task, errors) from improve.episodes"
                    .into(),
            },
            PaletteCommand {
                id: "learn.promotions".into(),
                title: "List promotion decisions".into(),
                description: "M12.17: persisted promotion/rollback decisions with measured evidence"
                    .into(),
            },
        ] {
            commands.insert(cmd.id.clone(), cmd);
        }
        Self {
            commands,
            store,
            runtime: None,
            rt_handle: None,
            tokens: BTreeMap::new(),
            bind: "loopback".to_string(),
            sovereign: false,
        }
    }

    /// Production construction: attach the live `Runtime` (M5.8). Approvals
    /// and other mutations go through the runtime's durable methods.
    pub fn with_runtime(
        store: ImproveStore,
        runtime: Arc<Runtime>,
        rt_handle: tokio::runtime::Handle,
    ) -> Self {
        let mut p = Self::new(store);
        p.runtime = Some(runtime);
        p.rt_handle = Some(rt_handle);
        p
    }

    /// Mint a one-time server-issued CSPRNG bearer token that authenticates
    /// `operator` for approval commands. The token — not any body field —
    /// is the authority for who is calling. Uses the OS CSPRNG via `uuid`
    /// v4 (non-deterministic across processes/starts).
    pub fn issue_token(&mut self, operator: &str) -> String {
        let token = format!("oprt-{}", uuid::Uuid::new_v4());
        self.tokens.insert(token.clone(), operator.to_string());
        token
    }

    /// Override the bound address reported by `session.info` (set by
    /// `serve` once the real socket is known).
    pub fn set_bind(&mut self, bind: impl Into<String>) {
        self.bind = bind.into();
    }

    /// Override the Sovereign flag reported by `session.info`.
    pub fn set_sovereign(&mut self, sovereign: bool) {
        self.sovereign = sovereign;
    }

    pub fn bind(&self) -> &str {
        &self.bind
    }

    pub fn store(&self) -> &ImproveStore {
        &self.store
    }

    pub fn commands(&self) -> Vec<PaletteCommand> {
        self.commands.values().cloned().collect()
    }

    /// Phase 13: the SIH dashboard JSON behind `GET /api/sih` (read-only,
    /// no operator token — it exposes no mutation surface).
    pub fn sih_dashboard(&mut self) -> Result<String, WebUiError> {
        let rt = self
            .runtime
            .clone()
            .ok_or_else(|| WebUiError::BadRequest("sih dashboard requires a runtime".into()))?;
        let value = self.block_on(async move { openrt_runtime::sih::dashboard(&rt).await })?;
        Ok(value.to_string())
    }

    pub fn register(&mut self, command: PaletteCommand) -> Result<(), WebUiError> {
        if command.id.trim().is_empty() {
            return Err(WebUiError::BadRequest(
                "command id must not be empty".into(),
            ));
        }
        if self.commands.contains_key(&command.id) {
            return Err(WebUiError::BadRequest(format!(
                "command {} already registered",
                command.id
            )));
        }
        self.commands.insert(command.id.clone(), command);
        Ok(())
    }

    fn require_operator(&self, req: &DispatchRequest) -> Result<OperatorGrant, WebUiError> {
        let token = req
            .auth_token
            .as_deref()
            .ok_or_else(|| WebUiError::UnauthorizedCommand {
                id: req.command_id.clone(),
            })?;
        let operator = self
            .tokens
            .get(token)
            .ok_or_else(|| WebUiError::UnauthorizedCommand {
                id: req.command_id.clone(),
            })?;
        OperatorGrant::new(operator).map_err(|_| WebUiError::UnauthorizedCommand {
            id: req.command_id.clone(),
        })
    }

    fn block_on<F, T>(&self, fut: F) -> Result<T, WebUiError>
    where
        F: std::future::Future<Output = T>,
    {
        let handle = self.rt_handle.as_ref().ok_or_else(|| {
            WebUiError::BadRequest("runtime handle unavailable for this command".into())
        })?;
        Ok(handle.block_on(fut))
    }

    pub fn dispatch(&mut self, req: &DispatchRequest) -> Result<PaletteOutcome, WebUiError> {
        if !self.commands.contains_key(&req.command_id) {
            return Err(WebUiError::CommandNotFound {
                id: req.command_id.clone(),
            });
        }
        match req.command_id.as_str() {
            "session.info" => Ok(PaletteOutcome::Completed {
                id: req.command_id.clone(),
                detail: json!({
                    "bind": self.bind,
                    "operators": self.store.meta().operators.len(),
                    "sovereign": self.sovereign || self.store.meta().sovereign,
                })
                .to_string(),
            }),
            "improve.proposals" | "improve.states" => {
                let list: Vec<serde_json::Value> = self
                    .store
                    .proposal_states()
                    .into_iter()
                    .map(|(id, state)| json!({"id": id, "state": state}))
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(list).to_string(),
                })
            }
            "improve.approve" | "improve.reject" => {
                let grant = self.require_operator(req)?;
                let proposal_id = req
                    .args
                    .get("proposal_id")
                    .cloned()
                    .ok_or_else(|| WebUiError::BadRequest("missing arg proposal_id".into()))?;
                let note = req.args.get("note").cloned();
                let approve = req.command_id == "improve.approve";

                // Durable path: go through the production Runtime (M5.8) so
                // the shared store — not a palette-local clone — records the
                // decision. Falls back to the local store when no runtime
                // is attached (unit tests / offline palette).
                if let (Some(rt), Some(handle)) = (self.runtime.clone(), self.rt_handle.clone()) {
                    let result = handle.block_on({
                        let rt = rt.clone();
                        let proposal_id = proposal_id.clone();
                        let note = note.clone();
                        let operator = grant.operator.clone();
                        async move {
                            if approve {
                                rt.improve_approve_with_note(
                                    &proposal_id,
                                    &operator,
                                    note.as_deref(),
                                )
                                .await
                            } else {
                                rt.improve_reject_with_note(
                                    &proposal_id,
                                    &operator,
                                    note.as_deref(),
                                )
                                .await
                            }
                        }
                    });
                    match result {
                        Ok(()) => {
                            // Refresh the local view from the durable store.
                            if let Ok(fresh) = handle.block_on(async { rt.load_improve().await }) {
                                self.store = fresh;
                            }
                            let state = self
                                .store
                                .get_proposal(&proposal_id)
                                .map(|p| p.state)
                                .unwrap_or(ProposalState::Proposed);
                            return Ok(PaletteOutcome::Completed {
                                id: req.command_id.clone(),
                                detail: json!({
                                    "proposal_id": proposal_id,
                                    "state": state.as_str(),
                                    "durable": true,
                                })
                                .to_string(),
                            });
                        }
                        Err(e) => {
                            return Ok(PaletteOutcome::Denied {
                                id: req.command_id.clone(),
                                reason: e.to_string(),
                            });
                        }
                    }
                }

                // Local-only fallback (no runtime attached).
                let result = if approve {
                    self.store.approve(&proposal_id, Some(&grant))
                } else {
                    self.store.reject(&proposal_id, Some(&grant))
                };
                match result {
                    Ok(()) => {
                        if let Some(n) = &note {
                            let _ = self.store.set_operator_decision(&proposal_id, n);
                        }
                        let state = self
                            .store
                            .get_proposal(&proposal_id)
                            .map(|p| p.state)
                            .unwrap_or(ProposalState::Proposed);
                        Ok(PaletteOutcome::Completed {
                            id: req.command_id.clone(),
                            detail: json!({
                                "proposal_id": proposal_id,
                                "state": state.as_str(),
                                "durable": false,
                            })
                            .to_string(),
                        })
                    }
                    Err(e) => Ok(PaletteOutcome::Denied {
                        id: req.command_id.clone(),
                        reason: e.to_string(),
                    }),
                }
            }
            "workspace.info" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("workspace.info requires a runtime".into())
                })?;
                let handle = self.rt_handle.as_ref().ok_or_else(|| {
                    WebUiError::BadRequest("workspace.info requires a runtime".into())
                })?;
                let workspaces = handle
                    .block_on(async {
                        let store = rt.store.clone();
                        openrt_workspace::list_workspaces(&*store).await
                    })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({ "count": workspaces.len() }).to_string(),
                })
            }
            "session.list" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("session.list requires a runtime".into())
                })?;
                let sessions = self
                    .block_on(async { rt.list_sessions().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({ "count": sessions.len() }).to_string(),
                })
            }
            "history.tail" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("history.tail requires a runtime".into())
                })?;
                let tail = self
                    .block_on(async { rt.history_tail().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({ "tail": tail }).to_string(),
                })
            }
            // M10.5 time machine — verified timeline over the ONE history
            // ledger (same read the CLI `history timeline` uses).
            "history.timeline" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("history.timeline requires a runtime".into())
                })?;
                let session = req.args.get("session").cloned();
                let from_seq: u64 = match req.args.get("from_seq") {
                    Some(v) => v
                        .parse()
                        .map_err(|_| WebUiError::BadRequest("bad from_seq".into()))?,
                    None => 0,
                };
                let limit: Option<usize> = match req.args.get("limit") {
                    Some(v) => Some(
                        v.parse()
                            .map_err(|_| WebUiError::BadRequest("bad limit".into()))?,
                    ),
                    None => Some(20),
                };
                let timeline = self
                    .block_on(async move {
                        rt.history_timeline(&openrt_runtime::TimelineOptions {
                            session,
                            from_seq,
                            limit,
                        })
                        .await
                    })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let records: Vec<serde_json::Value> = timeline
                    .records
                    .iter()
                    .map(|r| {
                        json!({
                            "seq": r.seq,
                            "status": r.status,
                            "event_kind": r.event_kind,
                            "node_id": r.node_id,
                            "note": r.note,
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({
                        "tail_seq": timeline.tail_seq,
                        "count": records.len(),
                        "records": records,
                    })
                    .to_string(),
                })
            }
            // M10.5 time machine — operator-gated write side: fork an
            // append-only what-if branch (the parent ledger is untouched).
            "history.fork" => {
                let _grant = self.require_operator(req)?;
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("history.fork requires a runtime".into())
                })?;
                let branch = req
                    .args
                    .get("branch")
                    .cloned()
                    .ok_or_else(|| WebUiError::BadRequest("missing arg branch".into()))?;
                let base = req.args.get("base").cloned().unwrap_or_default();
                self.block_on({
                    let branch = branch.clone();
                    let base = base.clone();
                    async move { rt.history_fork(&branch, &base).await }
                })
                .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({ "branch": branch, "base": base }).to_string(),
                })
            }
            "history.compare" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("history.compare requires a runtime".into())
                })?;
                let branch = req
                    .args
                    .get("branch")
                    .cloned()
                    .ok_or_else(|| WebUiError::BadRequest("missing arg branch".into()))?;
                let cmp = self
                    .block_on(async move { rt.history_branch_compare(&branch).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::to_string(&cmp)
                        .map_err(|e| WebUiError::BadRequest(e.to_string()))?,
                })
            }
            "history.state" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("history.state requires a runtime".into())
                })?;
                let as_of: u64 = req
                    .args
                    .get("as_of")
                    .ok_or_else(|| WebUiError::BadRequest("missing arg as_of".into()))?
                    .parse()
                    .map_err(|_| WebUiError::BadRequest("bad as_of".into()))?;
                let state = self
                    .block_on(async move { rt.history_state_as_of(as_of).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::to_string(&state)
                        .map_err(|e| WebUiError::BadRequest(e.to_string()))?,
                })
            }
            "history.return" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("history.return requires a runtime".into())
                })?;
                let view = self
                    .block_on(async move { rt.history_return_to_current().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::to_string(&view)
                        .map_err(|e| WebUiError::BadRequest(e.to_string()))?,
                })
            }
            // M10.2 boot recovery — one row per boot, real finding counts.
            "recovery.list" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("recovery.list requires a runtime".into())
                })?;
                let records = self
                    .block_on(async move { rt.recovery_records().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let rows: Vec<serde_json::Value> = records
                    .iter()
                    .map(|r| {
                        json!({
                            "record_id": r.record_id,
                            "boot_id": r.boot_id,
                            "interrupted": r.interrupted_tasks.len(),
                            "incomplete": r.incomplete_runs.len(),
                            "stale_sandboxes": r.stale_sandboxes.len(),
                            "clean": r.is_clean(),
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({ "count": rows.len(), "records": rows }).to_string(),
                })
            }
            // M10.9 AgentShield — findings from the SAME store the gate
            // records to; optional lifecycle filter, fail-closed on typos.
            "shield.findings" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("shield.findings requires a runtime".into())
                })?;
                let status = match req.args.get("status") {
                    Some(s) => Some(match s.as_str() {
                        "open" => openrt_shield::FindingStatus::Open,
                        "warned" => openrt_shield::FindingStatus::Warned,
                        "approval_required" => openrt_shield::FindingStatus::ApprovalRequired,
                        "denied" => openrt_shield::FindingStatus::Denied,
                        "halted" => openrt_shield::FindingStatus::Halted,
                        "reported" => openrt_shield::FindingStatus::Reported,
                        other => {
                            return Err(WebUiError::BadRequest(format!(
                                "bad status {other} (open|warned|approval_required|denied|halted|reported)"
                            )))
                        }
                    }),
                    None => None,
                };
                let findings = self
                    .block_on(async move {
                        openrt_shield::store::list_findings(&*rt.store, status).await
                    })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let rows: Vec<serde_json::Value> = findings
                    .iter()
                    .map(|f| {
                        json!({
                            "id": f.id,
                            "severity": f.severity.as_str(),
                            "category": f.category.as_str(),
                            "reason": f.reason,
                            "status": f.status.as_str(),
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({ "count": rows.len(), "findings": rows }).to_string(),
                })
            }
            // M10.6 skills — the full production loader report; unknown ids
            // fail closed through the same `skill_inspect` authority.
            "skill.inspect" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("skill.inspect requires a runtime".into())
                })?;
                let id = req
                    .args
                    .get("id")
                    .cloned()
                    .ok_or_else(|| WebUiError::BadRequest("missing arg id".into()))?;
                if id.trim().is_empty() {
                    return Err(WebUiError::BadRequest("id must not be empty".into()));
                }
                let version: Option<u32> = match req.args.get("version") {
                    Some(v) => Some(
                        v.parse()
                            .map_err(|_| WebUiError::BadRequest("bad version".into()))?,
                    ),
                    None => None,
                };
                let report = self
                    .block_on(async move { rt.skill_inspect(id.as_str(), version).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({
                        "id": report.id,
                        "version": report.version,
                        "passed": report.passed,
                        "workflow_executable": report.workflow_executable,
                        "effective_permissions": report.effective_permissions,
                        "missing_tools": report.missing_tools,
                        "checks": report
                            .checks
                            .iter()
                            .map(|c| json!({"name": c.name, "ok": c.ok, "detail": c.detail}))
                            .collect::<Vec<_>>(),
                    })
                    .to_string(),
                })
            }
            "memory.working.has" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("memory.working.has requires a runtime".into())
                })?;
                let session = req
                    .args
                    .get("session")
                    .cloned()
                    .ok_or_else(|| WebUiError::BadRequest("missing arg session".into()))?;
                let key = req
                    .args
                    .get("key")
                    .cloned()
                    .ok_or_else(|| WebUiError::BadRequest("missing arg key".into()))?;
                let sid: openrt_core::SessionId = session
                    .parse()
                    .map_err(|_| WebUiError::BadRequest("bad session id".into()))?;
                let key_for_async = key.clone();
                let has = self
                    .block_on(async move { rt.memory_working_has(sid, &key_for_async).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({ "session": session, "key": key, "has": has }).to_string(),
                })
            }
            "assets.skills" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("assets.skills requires a runtime".into())
                })?;
                let states = self
                    .block_on(async { rt.assets_skill_states().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let list: Vec<serde_json::Value> = states
                    .into_iter()
                    .map(|(id, s)| json!({"id": id, "state": s}))
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(list).to_string(),
                })
            }
            "task.list" => {
                let rt = self
                    .runtime
                    .clone()
                    .ok_or_else(|| WebUiError::BadRequest("task.list requires a runtime".into()))?;
                let tasks = self
                    .block_on(async { rt.store.list_tasks(None).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({ "count": tasks.len() }).to_string(),
                })
            }
            "workflow.compile" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("workflow.compile requires a runtime".into())
                })?;
                let goal = req
                    .args
                    .get("goal")
                    .cloned()
                    .ok_or_else(|| WebUiError::BadRequest("missing arg goal".into()))?;
                if goal.trim().is_empty() {
                    return Err(WebUiError::BadRequest("goal must not be empty".into()));
                }
                let mode = req
                    .args
                    .get("mode")
                    .cloned()
                    .unwrap_or_else(|| "compile".into());
                let mode: openrt_compiler::CompileMode = mode
                    .parse()
                    .map_err(|e: String| WebUiError::BadRequest(e))?;
                let out = self
                    .block_on(async move { rt.compile_workflow(&goal, mode).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({
                        "workflow_id": out.workflow_id,
                        "mode": out.mode.as_str(),
                        "stage": out.stage_reached.as_str(),
                        "steps": out.workflow.steps.len(),
                        "persisted": out.persisted,
                    })
                    .to_string(),
                })
            }
            "workflow.list" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("workflow.list requires a runtime".into())
                })?;
                let list = self
                    .block_on(async { openrt_compiler::list_compiled(&rt.store).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let rows: Vec<serde_json::Value> = list
                    .iter()
                    .map(|r| {
                        json!({
                            "workflow_id": r.workflow_id,
                            "goal_id": r.goal.id,
                            "objective": r.goal.objective,
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(rows).to_string(),
                })
            }
            "workflow.inspect" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("workflow.inspect requires a runtime".into())
                })?;
                let id = req
                    .args
                    .get("workflow_id")
                    .cloned()
                    .ok_or_else(|| WebUiError::BadRequest("missing arg workflow_id".into()))?;
                let (rec, wf) = self
                    .block_on(async move { rt.load_compiled_workflow(&id).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({
                        "workflow_id": wf.id,
                        "goal": rec.goal.objective,
                        "steps": wf.steps.iter().map(|s| &s.id).collect::<Vec<_>>(),
                        "nodes": rec.provenance.nodes.len(),
                    })
                    .to_string(),
                })
            }
            "workflow.validate" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("workflow.validate requires a runtime".into())
                })?;
                let id = req
                    .args
                    .get("workflow_id")
                    .cloned()
                    .ok_or_else(|| WebUiError::BadRequest("missing arg workflow_id".into()))?;
                let (rec, _wf) = self
                    .block_on(async move { rt.load_compiled_workflow(&id).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                match rec.validation {
                    Some(report) => Ok(PaletteOutcome::Completed {
                        id: req.command_id.clone(),
                        detail: json!({
                            "workflow_id": report.workflow_id,
                            "valid": report.is_valid(),
                            "checks": report.checks.len(),
                        })
                        .to_string(),
                    }),
                    None => Ok(PaletteOutcome::Denied {
                        id: req.command_id.clone(),
                        reason: "compiled record has no validation report (recompile with mode validate|execute)"
                            .into(),
                    }),
                }
            }
            // Phase 11 M11.6: workbench surfaces over the ONE store — the
            // same Runtime methods the CLI and TUI call (no UI-local state).
            "workbench.info" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("workbench.info requires a runtime".into())
                })?;
                let value = self
                    .block_on(async move {
                        let rooms = openrt_workspace::list_rooms(&*rt.store).await?;
                        let materials = openrt_workspace::list_materials(&*rt.store).await?;
                        let workspaces = openrt_workspace::list_workspaces(&*rt.store).await?;
                        let steer: usize = rooms.iter().map(|r| r.steer.len()).sum();
                        let agents: usize = rooms.iter().map(|r| r.agents.len()).sum();
                        Ok::<_, openrt_workspace::WorkspaceError>(json!({
                            "workspaces": workspaces.len(),
                            "rooms": rooms.len(),
                            "agents": agents,
                            "materials": materials.len(),
                            "steer": steer,
                        }))
                    })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: value.to_string(),
                })
            }
            "room.list" => {
                let rt = self
                    .runtime
                    .clone()
                    .ok_or_else(|| WebUiError::BadRequest("room.list requires a runtime".into()))?;
                let rooms = self
                    .block_on(async { rt.list_rooms().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let rows: Vec<serde_json::Value> = rooms
                    .iter()
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "title": r.title,
                            "workbench": r.workbench_code,
                            "status": r.status.to_string(),
                            "active_workflow": r.active_workflow,
                            "agents": r.agents.len(),
                            "materials": r.materials.len(),
                            "steer": r.steer.len(),
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(rows).to_string(),
                })
            }
            "room.create" => {
                let grant = self.require_operator(req)?;
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("room.create requires a runtime".into())
                })?;
                let workbench = arg(req, "workbench")?;
                let title = arg(req, "title")?;
                let room = self
                    .block_on(
                        async move { rt.create_room(&workbench, &title, &grant.operator).await },
                    )
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({"id": room.id, "title": room.title, "workbench": room.workbench_code})
                        .to_string(),
                })
            }
            "room.show" => {
                let rt = self
                    .runtime
                    .clone()
                    .ok_or_else(|| WebUiError::BadRequest("room.show requires a runtime".into()))?;
                let id = arg(req, "id")?;
                let room = self
                    .block_on(async move { rt.room(&id).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let value = serde_json::to_value(&room)
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: value.to_string(),
                })
            }
            "room.status" => {
                let grant = self.require_operator(req)?;
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("room.status requires a runtime".into())
                })?;
                let id = arg(req, "id")?;
                let status = arg(req, "status")?;
                let status = match status.trim().to_ascii_lowercase().as_str() {
                    "active" => openrt_workspace::RoomStatus::Active,
                    "paused" => openrt_workspace::RoomStatus::Paused,
                    other => {
                        return Err(WebUiError::BadRequest(format!(
                            "unknown room status `{other}` (active|paused)"
                        )))
                    }
                };
                let room = self
                    .block_on(async move { rt.set_room_status(&id, status, &grant.operator).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({"id": room.id, "status": room.status.to_string()}).to_string(),
                })
            }
            "room.steer" => {
                let grant = self.require_operator(req)?;
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("room.steer requires a runtime".into())
                })?;
                let id = arg(req, "id")?;
                let input = arg(req, "input")?;
                if input.trim().is_empty() {
                    return Err(WebUiError::BadRequest(
                        "steer input must not be empty".into(),
                    ));
                }
                let entry = self
                    .block_on(async move { rt.room_steer(&id, &grant.operator, &input).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({"appended": true, "at": entry.at, "operator": entry.operator})
                        .to_string(),
                })
            }
            "room.agent" => {
                let grant = self.require_operator(req)?;
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("room.agent requires a runtime".into())
                })?;
                let id = arg(req, "id")?;
                let agent = arg(req, "agent")?;
                let role = arg(req, "role")?;
                let room = self
                    .block_on(async move {
                        rt.link_room_agent(&id, &agent, &role, &grant.operator)
                            .await
                    })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({
                        "id": room.id,
                        "agents": room
                            .agents
                            .iter()
                            .map(|a| json!({"id": a.id, "role": a.role.as_str()}))
                            .collect::<Vec<_>>(),
                    })
                    .to_string(),
                })
            }
            "material.list" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("material.list requires a runtime".into())
                })?;
                let materials = self
                    .block_on(async { rt.list_materials().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let rows: Vec<serde_json::Value> = materials
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.id,
                            "kind": m.kind.to_string(),
                            "title": m.title,
                            "source": m.source,
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(rows).to_string(),
                })
            }
            "material.verify" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("material.verify requires a runtime".into())
                })?;
                let id = arg(req, "id")?;
                let intact = self
                    .block_on(async { openrt_workspace::verify_material(&*rt.store, &id).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                if intact {
                    Ok(PaletteOutcome::Completed {
                        id: req.command_id.clone(),
                        detail: json!({"id": id, "intact": true}).to_string(),
                    })
                } else {
                    Ok(PaletteOutcome::Denied {
                        id: req.command_id.clone(),
                        reason: format!(
                            "material {id} digest DRIFTED — reference changed since sealing"
                        ),
                    })
                }
            }
            "extension.list" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("extension.list requires a runtime".into())
                })?;
                let states = self
                    .block_on(async { rt.extension_states().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(states).to_string(),
                })
            }
            "extension.inspect" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("extension.inspect requires a runtime".into())
                })?;
                let id = arg(req, "id")?;
                let version = req.args.get("version").and_then(|v| v.parse::<u32>().ok());
                let manifest = self
                    .block_on(async move { rt.extension_inspect(&id, version).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: manifest.to_string(),
                })
            }
            "mcp.list" => {
                let rt = self
                    .runtime
                    .clone()
                    .ok_or_else(|| WebUiError::BadRequest("mcp.list requires a runtime".into()))?;
                let states = self
                    .block_on(async { rt.mcp_states().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: states.to_string(),
                })
            }
            "mcp.trust" => {
                let grant = self.require_operator(req)?;
                let rt = self
                    .runtime
                    .clone()
                    .ok_or_else(|| WebUiError::BadRequest("mcp.trust requires a runtime".into()))?;
                let name = arg(req, "name")?;
                let trust = arg(req, "trust")?;
                self.block_on(async { rt.mcp_set_trust(&name, &trust, &grant.operator).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({"name": name, "trust": trust, "operator": grant.operator})
                        .to_string(),
                })
            }
            "mcp.enable" | "mcp.disable" => {
                let grant = self.require_operator(req)?;
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("mcp.enable/disable require a runtime".into())
                })?;
                let name = arg(req, "name")?;
                let enable = req.command_id == "mcp.enable";
                let operator = grant.operator.clone();
                self.block_on(async {
                    if enable {
                        rt.mcp_enable(&name, &operator).await
                    } else {
                        rt.mcp_disable(&name, &operator).await
                    }
                })
                .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: json!({"name": name, "enabled": enable, "operator": grant.operator})
                        .to_string(),
                })
            }
            "computer.env" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("computer.env requires a runtime".into())
                })?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: rt.computer_env_snapshot().to_string(),
                })
            }
            "computer.observe" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("computer.observe requires a runtime".into())
                })?;
                // Clipboard is ON DEMAND only — never read implicitly.
                let clipboard = req
                    .args
                    .get("clipboard")
                    .is_some_and(|v| v == "true" || v == "1");
                let bundle = self
                    .block_on(async move { rt.computer_engine.observe(clipboard).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let value = serde_json::to_value(&bundle)
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: value.to_string(),
                })
            }
            "computer.use" => {
                // Operator identity proven by the server-minted token; the
                // action itself still crosses the ONE gate (policy +
                // AgentShield + kill scope `computer` + audit).
                let _grant = self.require_operator(req)?;
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("computer.use requires a runtime".into())
                })?;
                let action = arg(req, "action")?;
                let mut args = json!({ "action": action });
                for key in ["target", "text", "key", "url", "app", "name", "clipboard"] {
                    if let Some(v) = req.args.get(key) {
                        args[key] = json!(v);
                    }
                }
                for key in [
                    "x", "y", "from_x", "from_y", "to_x", "to_y", "button", "delta",
                ] {
                    if let Some(v) = req.args.get(key) {
                        let n: i64 = v.parse().map_err(|_| {
                            WebUiError::BadRequest(format!("`{key}` must be an integer"))
                        })?;
                        args[key] = json!(n);
                    }
                }
                let outcome = self
                    .block_on(async move { rt.computer_use_dispatch("webui", args).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                match outcome {
                    Ok(v) => Ok(PaletteOutcome::Completed {
                        id: req.command_id.clone(),
                        detail: v.to_string(),
                    }),
                    // Gate refusals surface as an honest denial, never as a
                    // silent UI-local execution.
                    Err(openrt_tools::ToolError::PermissionDenied { reason, .. })
                    | Err(openrt_tools::ToolError::KillSwitchEngaged { reason, .. }) => {
                        Ok(PaletteOutcome::Denied {
                            id: req.command_id.clone(),
                            reason,
                        })
                    }
                    Err(e) => Err(WebUiError::BadRequest(e.to_string())),
                }
            }
            // Phase 12 (M12): industrial + trading + learning views over
            // the ONE store — same Runtime methods the CLI and TUI call.
            "phase12.info" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("phase12.info requires a runtime".into())
                })?;
                let value = self
                    .block_on(async move {
                        let assets = rt.industrial_assets(&Default::default()).await?;
                        let instruments = rt.trading_instruments().await?;
                        let datasets = rt.trading_datasets().await?;
                        let strategies = rt.trading_strategies().await?;
                        let episodes = rt.learn_episodes().await?;
                        let promotions = rt.learn_promotions().await?;
                        let signals = rt.learn_routing_signals().await?;
                        let cache = rt.cache_state().await?;
                        let checks = rt.phase12_doctor_checks().await;
                        let mut pass = 0usize;
                        let mut warn = 0usize;
                        let mut other = 0usize;
                        for c in &checks {
                            match c.status.as_str() {
                                "PASS" => pass += 1,
                                "WARN" => warn += 1,
                                _ => other += 1,
                            }
                        }
                        Ok::<_, openrt_runtime::Error>(json!({
                            "industrial_assets": assets.len(),
                            "trading_instruments": instruments.len(),
                            "trading_datasets": datasets.len(),
                            "trading_strategies": strategies.len(),
                            "trading_execute": "DENIED (research only)",
                            "experience_episodes": episodes.len(),
                            "promotion_decisions": promotions.len(),
                            "routing_signals": signals.len(),
                            "model_cache_entries": cache.entries.len(),
                            "doctor": {"pass": pass, "warn": warn, "other": other},
                        }))
                    })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: value.to_string(),
                })
            }
            "sih.info" => {
                let rt = self
                    .runtime
                    .clone()
                    .ok_or_else(|| WebUiError::BadRequest("sih.info requires a runtime".into()))?;
                let value =
                    self.block_on(async move { openrt_runtime::sih::dashboard(&rt).await })?;
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: value.to_string(),
                })
            }
            "industrial.assets" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("industrial.assets requires a runtime".into())
                })?;
                let assets = self
                    .block_on(async move { rt.industrial_assets(&Default::default()).await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let rows: Vec<serde_json::Value> = assets
                    .iter()
                    .map(|a| {
                        json!({
                            "id": a.id,
                            "kind": a.kind.as_str(),
                            "label": a.label,
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(rows).to_string(),
                })
            }
            "trading.datasets" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("trading.datasets requires a runtime".into())
                })?;
                let datasets = self
                    .block_on(async move { rt.trading_datasets().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let rows: Vec<serde_json::Value> = datasets
                    .iter()
                    .map(|d| {
                        json!({
                            "id": d.id,
                            "version": d.version,
                            "bars": d.bar_count,
                            "instrument": d.config.instrument,
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(rows).to_string(),
                })
            }
            "trading.execute" => {
                // The ONE refusal point, surfaced honestly: the runtime
                // method always fails (research only) and audits the attempt.
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("trading.execute requires a runtime".into())
                })?;
                let order: serde_json::Value = serde_json::from_str(arg(req, "order")?.trim())
                    .map_err(|e| WebUiError::BadRequest(format!("order must be JSON: {e}")))?;
                match self.block_on(async move { rt.trading_execute(&order).await }) {
                    Err(e) => Err(WebUiError::BadRequest(e.to_string())),
                    Ok(Ok(_)) => Err(WebUiError::BadRequest(
                        "invariant violated: trading execution must always refuse".into(),
                    )),
                    Ok(Err(e)) => Ok(PaletteOutcome::Denied {
                        id: req.command_id.clone(),
                        reason: e.to_string(),
                    }),
                }
            }
            "learn.episodes" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("learn.episodes requires a runtime".into())
                })?;
                let episodes = self
                    .block_on(async move { rt.learn_episodes().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let rows: Vec<serde_json::Value> = episodes
                    .iter()
                    .map(|e| {
                        json!({
                            "id": e.id,
                            "task": e.task,
                            "errors": e.errors,
                            "retries": e.retries,
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(rows).to_string(),
                })
            }
            "learn.promotions" => {
                let rt = self.runtime.clone().ok_or_else(|| {
                    WebUiError::BadRequest("learn.promotions requires a runtime".into())
                })?;
                let records = self
                    .block_on(async move { rt.learn_promotions().await })
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?
                    .map_err(|e| WebUiError::BadRequest(e.to_string()))?;
                let rows: Vec<serde_json::Value> = records
                    .iter()
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "kind": r.kind.as_str(),
                            "decision": r.decision.as_str(),
                            "operator": r.operator,
                            "source": r.benchmark.source,
                        })
                    })
                    .collect();
                Ok(PaletteOutcome::Completed {
                    id: req.command_id.clone(),
                    detail: serde_json::Value::Array(rows).to_string(),
                })
            }
            other => Err(WebUiError::CommandNotFound {
                id: other.to_string(),
            }),
        }
    }
}
