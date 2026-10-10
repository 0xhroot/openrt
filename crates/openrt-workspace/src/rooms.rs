//! Phase 11 (Track C): **agent rooms** — the workbench's collaboration
//! surface over the M5.1 workspace foundation (extend, never duplicate:
//! a room always hangs off an existing `workspace.workspaces` record).
//!
//! OWNERSHIP: namespace `workbench.rooms` (key `r.<hex32>`), record id =
//! `sha256(workbench|title|created_at)` — deterministic, never random.
//!
//! Invariants:
//! * single writer = this module (the runtime wraps these calls and audits);
//! * `history` and `steer` are append-only (no update path removes entries);
//! * `active_workflow` is bookkeeping, never a second workflow authority —
//!   the checkpoint store remains the one run/recovery record;
//! * a room cannot exist without its workbench (fail-closed validation).

use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::WorkspaceError;

/// Namespace owned by this module (Phase 11 OWNERSHIP).
pub const ROOMS_NS: &str = "workbench.rooms";

/// Lifecycle of a room. `Paused` here is a *room* state (no new work
/// accepted), distinct from a paused *workflow run* (checkpoint stop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoomStatus {
    Active,
    Paused,
}

impl std::fmt::Display for RoomStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Active => "active",
            Self::Paused => "paused",
        };
        f.write_str(s)
    }
}

/// Append-only audit line inside a room (who did what, when). Not a
/// substitute for the runtime's tamper-evident audit log — a readable
/// conversation history for the workbench UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomEvent {
    pub at: String,
    pub actor: String,
    pub action: String,
    pub detail: String,
}

/// One operator steer directive, appended by the room and hydrated into the
/// workflow blackboard on (re)start — append-only, never rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerEntry {
    pub at: String,
    pub operator: String,
    pub input: String,
}

/// Phase 11 M11.4: a room role — pure *configuration* that maps onto an
/// existing `SubAgentKind` identity (the orchestrator owns the mapping via
/// `room_role_sub_agent`; the runtime validates every assignment). Roles are
/// never new security authorities: permissions/sandbox/policy always resolve
/// through the same per-identity gates the sub-agent already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoomRole {
    Researcher,
    Analyst,
    Coder,
    Reviewer,
    Verifier,
}

impl RoomRole {
    /// The five spec'd roles (M11.4), in stable order.
    pub const ALL: [RoomRole; 5] = [
        Self::Researcher,
        Self::Analyst,
        Self::Coder,
        Self::Reviewer,
        Self::Verifier,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Researcher => "researcher",
            Self::Analyst => "analyst",
            Self::Coder => "coder",
            Self::Reviewer => "reviewer",
            Self::Verifier => "verifier",
        }
    }

    /// Parse a role name (case-insensitive). Unknown roles fail closed —
    /// no role outside the five ever enters a room record.
    pub fn parse(raw: &str) -> Option<RoomRole> {
        let s = raw.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

impl std::fmt::Display for RoomRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A role-configured agent inside a room (M11.4): the agent id is an
/// existing identity; the role only says which configuration it joins under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomAgent {
    pub id: String,
    pub role: RoomRole,
    pub added_at: String,
}

/// A room inside a workbench: the persistent container linking agents,
/// sessions, tasks, workflows, materials and artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    pub id: String,
    /// Workbench (workspace) id — `sha256(address_code)`.
    pub workbench: String,
    /// Workbench address code — the human handle used to create it.
    pub workbench_code: String,
    pub title: String,
    pub status: RoomStatus,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    /// Role-configured agents (M11.4 — roles are configuration mapped onto
    /// existing identities, never new authorities).
    pub agents: Vec<RoomAgent>,
    pub sessions: Vec<String>,
    pub tasks: Vec<String>,
    pub workflows: Vec<String>,
    /// The workflow currently driving this room (bookkeeping pointer —
    /// checkpoints remain the single run/recovery authority).
    pub active_workflow: Option<String>,
    /// Attached material ids (`mat.*`).
    pub materials: Vec<String>,
    /// Attached artifact identifiers (opaque — artifact store ids/keys).
    pub artifacts: Vec<String>,
    pub history: Vec<RoomEvent>,
    pub steer: Vec<SteerEntry>,
}

fn room_key(id: &str) -> String {
    format!("r.{}", id)
}

fn now_event(actor: &str, action: &str, detail: impl Into<String>, now: &str) -> RoomEvent {
    RoomEvent {
        at: now.to_string(),
        actor: actor.to_string(),
        action: action.to_string(),
        detail: detail.into(),
    }
}

async fn load(store: &dyn PersistenceStore, id: &str) -> Result<Room, WorkspaceError> {
    let Some(value) = store.get_record(ROOMS_NS, &room_key(id)).await? else {
        return Err(WorkspaceError::RoomNotFound(id.to_string()));
    };
    serde_json::from_value(value).map_err(|e| WorkspaceError::Serde(e.to_string()))
}

async fn save(store: &dyn PersistenceStore, room: &Room) -> Result<(), WorkspaceError> {
    let value = serde_json::to_value(room).map_err(|e| WorkspaceError::Serde(e.to_string()))?;
    store
        .put_record(ROOMS_NS, &room_key(&room.id), &value)
        .await?;
    Ok(())
}

/// Create a room inside an existing workbench. Fail-closed: the workbench
/// must exist (validated through the same `workspace.workspaces` records the
/// M5.1 layer owns — no parallel workbench store).
pub async fn create_room(
    store: &dyn PersistenceStore,
    workbench_code: &str,
    title: &str,
    operator: &str,
    now: &str,
) -> Result<Room, WorkspaceError> {
    if title.trim().is_empty() {
        return Err(WorkspaceError::Invalid(
            "room title must not be empty".into(),
        ));
    }
    // Validate through the M5.1 authority (NotFound propagates).
    let ws = crate::open_workspace(store, workbench_code, now).await?;
    let created_at = now.to_string();
    let id =
        openrt_embeddings::sha256_hex(format!("{}|{}|{}", ws.id, title, created_at).as_bytes());
    let key = room_key(&id);
    if store.get_record(ROOMS_NS, &key).await?.is_some() {
        return Err(WorkspaceError::AlreadyExists(format!("room {id}")));
    }
    let mut history = Vec::new();
    history.push(now_event(
        operator,
        "room.created",
        format!("title `{title}` in workbench `{workbench_code}`"),
        now,
    ));
    let room = Room {
        id: id.clone(),
        workbench: ws.id,
        workbench_code: workbench_code.to_string(),
        title: title.to_string(),
        status: RoomStatus::Active,
        created_by: operator.to_string(),
        created_at: created_at.clone(),
        updated_at: created_at,
        agents: Vec::new(),
        sessions: Vec::new(),
        tasks: Vec::new(),
        workflows: Vec::new(),
        active_workflow: None,
        materials: Vec::new(),
        artifacts: Vec::new(),
        history,
        steer: Vec::new(),
    };
    save(store, &room).await?;
    Ok(room)
}

/// Fetch one room (fail-closed NotFound).
pub async fn get_room(store: &dyn PersistenceStore, id: &str) -> Result<Room, WorkspaceError> {
    load(store, id).await
}

/// Deterministically list all rooms (sorted by id).
pub async fn list_rooms(store: &dyn PersistenceStore) -> Result<Vec<Room>, WorkspaceError> {
    let mut raw = store.list_records(ROOMS_NS).await?;
    raw.sort_by(|a, b| a.0.cmp(&b.0));
    let mut rooms = Vec::with_capacity(raw.len());
    for (_key, value) in raw {
        rooms
            .push(serde_json::from_value(value).map_err(|e| WorkspaceError::Serde(e.to_string()))?);
    }
    Ok(rooms)
}

/// Set the room's lifecycle status (append-only history records the change).
pub async fn set_room_status(
    store: &dyn PersistenceStore,
    id: &str,
    status: RoomStatus,
    operator: &str,
    now: &str,
) -> Result<Room, WorkspaceError> {
    let mut room = load(store, id).await?;
    room.status = status;
    room.updated_at = now.to_string();
    room.history.push(now_event(
        operator,
        "room.status",
        format!("status -> {status}"),
        now,
    ));
    save(store, &room).await?;
    Ok(room)
}

/// Point the room at (or off) a workflow — bookkeeping only; the checkpoint
/// store stays the single run/recovery authority.
pub async fn set_active_workflow(
    store: &dyn PersistenceStore,
    id: &str,
    workflow_id: Option<&str>,
    operator: &str,
    now: &str,
) -> Result<Room, WorkspaceError> {
    let mut room = load(store, id).await?;
    let wf = workflow_id.map(str::to_string);
    if let Some(w) = &wf {
        if !room.workflows.iter().any(|x| x == w) {
            room.workflows.push(w.clone());
        }
    }
    room.active_workflow = wf.clone();
    room.updated_at = now.to_string();
    room.history.push(now_event(
        operator,
        "room.active_workflow",
        wf.unwrap_or_else(|| "(cleared)".into()),
        now,
    ));
    save(store, &room).await?;
    Ok(room)
}

/// Append a session id (idempotent) — link, never duplicate.
pub async fn link_room_session(
    store: &dyn PersistenceStore,
    id: &str,
    session_id: &str,
    operator: &str,
    now: &str,
) -> Result<Room, WorkspaceError> {
    let mut room = load(store, id).await?;
    if !room.sessions.iter().any(|s| s == session_id) {
        room.sessions.push(session_id.to_string());
        room.history
            .push(now_event(operator, "room.session.linked", session_id, now));
        room.updated_at = now.to_string();
        save(store, &room).await?;
    }
    Ok(room)
}

/// Append an artifact identifier (idempotent).
pub async fn link_room_artifact(
    store: &dyn PersistenceStore,
    id: &str,
    artifact: &str,
    operator: &str,
    now: &str,
) -> Result<Room, WorkspaceError> {
    let mut room = load(store, id).await?;
    if !room.artifacts.iter().any(|a| a == artifact) {
        room.artifacts.push(artifact.to_string());
        room.history
            .push(now_event(operator, "room.artifact.linked", artifact, now));
        room.updated_at = now.to_string();
        save(store, &room).await?;
    }
    Ok(room)
}

/// Assign an agent to a room under a role (M11.4). The role must be one of
/// the five configured roles (fail-closed); re-linking an existing agent
/// updates its role (explicit, recorded in history) and never duplicates.
pub async fn link_room_agent(
    store: &dyn PersistenceStore,
    id: &str,
    agent: &str,
    role: &str,
    operator: &str,
    now: &str,
) -> Result<Room, WorkspaceError> {
    if agent.trim().is_empty() {
        return Err(WorkspaceError::Invalid("agent id must not be empty".into()));
    }
    let role = RoomRole::parse(role).ok_or_else(|| {
        WorkspaceError::Invalid(format!(
            "unknown room role `{role}` (roles: researcher, analyst, coder, reviewer, verifier)"
        ))
    })?;
    let mut room = load(store, id).await?;
    match room.agents.iter_mut().find(|a| a.id == agent) {
        Some(existing) => {
            if existing.role != role {
                existing.role = role;
                existing.added_at = now.to_string();
                room.updated_at = now.to_string();
                room.history.push(now_event(
                    operator,
                    "room.agent.role",
                    format!("{agent} -> {role}"),
                    now,
                ));
                save(store, &room).await?;
            }
        }
        None => {
            room.agents.push(RoomAgent {
                id: agent.to_string(),
                role,
                added_at: now.to_string(),
            });
            room.updated_at = now.to_string();
            room.history.push(now_event(
                operator,
                "room.agent.linked",
                format!("{agent} ({role})"),
                now,
            ));
            save(store, &room).await?;
        }
    }
    Ok(room)
}

/// Remove an agent from a room (the identity itself is untouched — records
/// are never destroyed by link removal).
pub async fn unlink_room_agent(
    store: &dyn PersistenceStore,
    id: &str,
    agent: &str,
    operator: &str,
    now: &str,
) -> Result<Room, WorkspaceError> {
    let mut room = load(store, id).await?;
    if room.agents.iter().any(|a| a.id == agent) {
        room.agents.retain(|a| a.id != agent);
        room.updated_at = now.to_string();
        room.history
            .push(now_event(operator, "room.agent.unlinked", agent, now));
        save(store, &room).await?;
    }
    Ok(room)
}

/// Attach an existing material to the room (idempotent; the material must
/// exist — fail-closed, no dangling references).
pub async fn attach_material(
    store: &dyn PersistenceStore,
    id: &str,
    material_id: &str,
    operator: &str,
    now: &str,
) -> Result<Room, WorkspaceError> {
    let mut room = load(store, id).await?;
    // Validate the material through the materials authority.
    crate::materials::get_material(store, material_id).await?;
    if room.materials.iter().any(|m| m == material_id) {
        return Ok(room);
    }
    room.materials.push(material_id.to_string());
    room.updated_at = now.to_string();
    room.history.push(now_event(
        operator,
        "room.material.attached",
        material_id,
        now,
    ));
    save(store, &room).await?;
    Ok(room)
}

/// Detach a material (idempotent; the material record itself is untouched —
/// records are never destroyed by link removal).
pub async fn detach_material(
    store: &dyn PersistenceStore,
    id: &str,
    material_id: &str,
    operator: &str,
    now: &str,
) -> Result<Room, WorkspaceError> {
    let mut room = load(store, id).await?;
    if room.materials.iter().any(|m| m == material_id) {
        room.materials.retain(|m| m != material_id);
        room.updated_at = now.to_string();
        room.history.push(now_event(
            operator,
            "room.material.detached",
            material_id,
            now,
        ));
        save(store, &room).await?;
    }
    Ok(room)
}

/// Append an operator steer directive (append-only — the runtime hydrates
/// entries into the workflow blackboard at run start, never rewriting them).
pub async fn steer_room(
    store: &dyn PersistenceStore,
    id: &str,
    operator: &str,
    input: &str,
    now: &str,
) -> Result<SteerEntry, WorkspaceError> {
    if input.trim().is_empty() {
        return Err(WorkspaceError::Invalid(
            "steer input must not be empty".into(),
        ));
    }
    let mut room = load(store, id).await?;
    let entry = SteerEntry {
        at: now.to_string(),
        operator: operator.to_string(),
        input: input.to_string(),
    };
    room.steer.push(entry.clone());
    room.updated_at = now.to_string();
    room.history.push(now_event(
        operator,
        "room.steer",
        input.chars().take(200).collect::<String>(),
        now,
    ));
    save(store, &room).await?;
    Ok(entry)
}

/// Rooms of a workbench (deterministic order).
pub async fn rooms_for_workbench(
    store: &dyn PersistenceStore,
    workbench_code: &str,
) -> Result<Vec<Room>, WorkspaceError> {
    Ok(list_rooms(store)
        .await?
        .into_iter()
        .filter(|r| r.workbench_code == workbench_code)
        .collect())
}

#[cfg(test)]
mod p11_room_tests {
    use super::*;
    use openrt_persistence::MemoryStore;
    use std::sync::Arc;

    async fn store() -> Arc<MemoryStore> {
        let store = Arc::new(MemoryStore::new());
        crate::create_workspace(&*store, "acct/lab", "Lab", "", "2026-09-26T00:00:00Z")
            .await
            .unwrap();
        store
    }

    /// A room round-trips with deterministic identity and requires its
    /// workbench to exist (fail-closed without it).
    #[tokio::test]
    async fn create_roundtrip_and_workbench_gate() {
        let store = store().await;
        let room = create_room(
            &*store,
            "acct/lab",
            "Triage",
            "alice",
            "2026-09-26T00:01:00Z",
        )
        .await
        .unwrap();
        assert_eq!(room.status, RoomStatus::Active);
        assert_eq!(room.workbench_code, "acct/lab");
        let again = get_room(&*store, &room.id).await.unwrap();
        assert_eq!(again, room);
        let err = create_room(&*store, "acct/ghost", "X", "alice", "2026-09-26T00:02:00Z").await;
        assert!(matches!(err, Err(WorkspaceError::NotFound(_))));
        let err = create_room(&*store, "acct/lab", "  ", "alice", "t").await;
        assert!(matches!(err, Err(WorkspaceError::Invalid(_))));
    }

    /// Status + active workflow changes are recorded append-only in history.
    #[tokio::test]
    async fn status_and_workflow_history_append() {
        let store = store().await;
        let room = create_room(&*store, "acct/lab", "Ops", "bob", "t0")
            .await
            .unwrap();
        let room = set_room_status(&*store, &room.id, RoomStatus::Paused, "bob", "t1")
            .await
            .unwrap();
        assert_eq!(room.status, RoomStatus::Paused);
        let room = set_active_workflow(&*store, &room.id, Some("wf1"), "bob", "t2")
            .await
            .unwrap();
        assert_eq!(room.active_workflow.as_deref(), Some("wf1"));
        assert!(room.workflows.iter().any(|w| w == "wf1"));
        // history only grows: created + status + workflow = 3
        assert_eq!(room.history.len(), 3);
        assert_eq!(room.history[0].action, "room.created");
        assert_eq!(room.history[2].action, "room.active_workflow");
        let rooms = rooms_for_workbench(&*store, "acct/lab").await.unwrap();
        assert_eq!(rooms.len(), 1);
    }

    /// Steer entries are append-only and never empty.
    #[tokio::test]
    async fn steer_appends_only() {
        let store = store().await;
        let room = create_room(&*store, "acct/lab", "S", "op", "t0")
            .await
            .unwrap();
        steer_room(&*store, &room.id, "op", "prefer batch size 8", "t1")
            .await
            .unwrap();
        steer_room(&*store, &room.id, "op", "halt after step 3", "t2")
            .await
            .unwrap();
        let room = get_room(&*store, &room.id).await.unwrap();
        assert_eq!(room.steer.len(), 2);
        assert_eq!(room.steer[0].input, "prefer batch size 8");
        assert!(steer_room(&*store, &room.id, "op", "  ", "t3")
            .await
            .is_err());
        let missing = get_room(&*store, "r-nope").await;
        assert!(matches!(missing, Err(WorkspaceError::RoomNotFound(_))));
    }

    /// Roles are configuration (M11.4): exactly the five roles link, unknown
    /// roles fail closed, re-link updates instead of duplicating, unlink
    /// records history without destroying anything.
    #[tokio::test]
    async fn agents_roles_are_configuration() {
        let store = store().await;
        let room = create_room(&*store, "acct/lab", "Crew", "op", "t0")
            .await
            .unwrap();
        let room = link_room_agent(&*store, &room.id, "a1", "researcher", "op", "t1")
            .await
            .unwrap();
        assert_eq!(room.agents.len(), 1);
        assert_eq!(room.agents[0].role, RoomRole::Researcher);
        // re-link with a different role updates in place (no duplicate).
        let room = link_room_agent(&*store, &room.id, "a1", "verifier", "op", "t2")
            .await
            .unwrap();
        assert_eq!(room.agents.len(), 1);
        assert_eq!(room.agents[0].role, RoomRole::Verifier);
        // every spec role parses; unknown roles fail closed.
        for r in RoomRole::ALL {
            assert_eq!(RoomRole::parse(r.as_str()), Some(r));
        }
        assert!(
            link_room_agent(&*store, &room.id, "a2", "pirate", "op", "t3")
                .await
                .is_err()
        );
        assert!(
            link_room_agent(&*store, &room.id, "  ", "coder", "op", "t3")
                .await
                .is_err()
        );
        // unlink removes the assignment, keeps history append-only.
        let room = unlink_room_agent(&*store, &room.id, "a1", "op", "t4")
            .await
            .unwrap();
        assert!(room.agents.is_empty());
        assert!(room
            .history
            .iter()
            .any(|e| e.action == "room.agent.unlinked"));
        let actions: Vec<&str> = room.history.iter().map(|e| e.action.as_str()).collect();
        assert_eq!(
            actions,
            vec![
                "room.created",
                "room.agent.linked",
                "room.agent.role",
                "room.agent.unlinked"
            ]
        );
    }
}
