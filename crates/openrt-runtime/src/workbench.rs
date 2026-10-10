//! Phase 11 (Tracks B–E): **workbench runtime surface** — the single
//! authoritative wrapper around rooms, materials, steer and the workflow
//! pause/resume verbs.
//!
//! Invariants:
//! * every mutating call audits through the runtime's one audit log
//!   (actor = operator, never anonymous);
//! * storage lives exclusively in `openrt-workspace` (rooms/materials) and
//!   `openrt-checkpoint` (runs) — this module holds NO second copy and no
//!   UI-local state;
//! * steer is append-only; hydration into the workflow blackboard happens
//!   once, at run start, in `checkpoint.rs`.

use openrt_workspace::{MaterialKind, MaterialReference, Room, RoomStatus, SteerEntry};

use crate::{Error, Runtime};

impl Runtime {
    /// Create a room in an existing workbench (fail-closed on unknown
    /// workbench).
    pub async fn create_room(
        &self,
        workbench_code: &str,
        title: &str,
        operator: &str,
    ) -> Result<Room, Error> {
        let now = openrt_core::now().to_rfc3339();
        let room =
            openrt_workspace::create_room(&*self.store, workbench_code, title, operator, &now)
                .await?;
        self.audit(
            operator,
            "workbench.room.create",
            &room.id,
            "created",
            serde_json::json!({ "workbench": workbench_code, "title": title }),
        )
        .await?;
        Ok(room)
    }

    /// Fetch one room (fail-closed).
    pub async fn room(&self, id: &str) -> Result<Room, Error> {
        Ok(openrt_workspace::get_room(&*self.store, id).await?)
    }

    /// All rooms, deterministically ordered.
    pub async fn list_rooms(&self) -> Result<Vec<Room>, Error> {
        Ok(openrt_workspace::list_rooms(&*self.store).await?)
    }

    /// Set room lifecycle status (append-only history records the change).
    pub async fn set_room_status(
        &self,
        id: &str,
        status: RoomStatus,
        operator: &str,
    ) -> Result<Room, Error> {
        let now = openrt_core::now().to_rfc3339();
        let room =
            openrt_workspace::set_room_status(&*self.store, id, status, operator, &now).await?;
        self.audit(
            operator,
            "workbench.room.status",
            id,
            "updated",
            serde_json::json!({ "status": status.to_string() }),
        )
        .await?;
        Ok(room)
    }

    /// Point a room at (or off) its active workflow — bookkeeping only.
    pub async fn set_room_workflow(
        &self,
        id: &str,
        workflow_id: Option<&str>,
        operator: &str,
    ) -> Result<Room, Error> {
        let now = openrt_core::now().to_rfc3339();
        let room =
            openrt_workspace::set_active_workflow(&*self.store, id, workflow_id, operator, &now)
                .await?;
        self.audit(
            operator,
            "workbench.room.workflow",
            id,
            "updated",
            serde_json::json!({ "workflow": workflow_id }),
        )
        .await?;
        Ok(room)
    }

    /// Link a session into a room (idempotent).
    pub async fn link_room_session(
        &self,
        id: &str,
        session_id: &str,
        operator: &str,
    ) -> Result<Room, Error> {
        let now = openrt_core::now().to_rfc3339();
        Ok(
            openrt_workspace::link_room_session(&*self.store, id, session_id, operator, &now)
                .await?,
        )
    }

    /// Assign an agent to a room under a configured role (M11.4). The role
    /// must resolve to an existing `SubAgentKind` identity (validated via
    /// `openrt_orch::room_role_sub_agent`) — roles never grant new authority.
    pub async fn link_room_agent(
        &self,
        id: &str,
        agent: &str,
        role: &str,
        operator: &str,
    ) -> Result<Room, Error> {
        // Fail-closed: the role must map onto an existing sub-agent identity
        // BEFORE any record is written.
        let kind = openrt_orch::room_role_sub_agent(role).ok_or_else(|| {
            Error::Workbench(openrt_workspace::WorkspaceError::Invalid(format!(
                "unknown room role `{role}` (researcher|analyst|coder|reviewer|verifier)"
            )))
        })?;
        let now = openrt_core::now().to_rfc3339();
        let room = openrt_workspace::link_room_agent(&*self.store, id, agent, role, operator, &now)
            .await?;
        self.audit(
            operator,
            "room.agent.link",
            id,
            "linked",
            serde_json::json!({ "agent": agent, "role": role, "sub_agent": kind.as_str() }),
        )
        .await?;
        Ok(room)
    }

    /// Remove a role assignment from a room (identity untouched).
    pub async fn unlink_room_agent(
        &self,
        id: &str,
        agent: &str,
        operator: &str,
    ) -> Result<Room, Error> {
        let now = openrt_core::now().to_rfc3339();
        let room =
            openrt_workspace::unlink_room_agent(&*self.store, id, agent, operator, &now).await?;
        self.audit(
            operator,
            "room.agent.unlink",
            id,
            "unlinked",
            serde_json::json!({ "agent": agent }),
        )
        .await?;
        Ok(room)
    }

    /// Register a material (content reference must resolve and digest now).
    pub async fn create_material(
        &self,
        kind: MaterialKind,
        title: &str,
        source: &str,
        reference: MaterialReference,
        operator: &str,
    ) -> Result<openrt_workspace::Material, Error> {
        let now = openrt_core::now().to_rfc3339();
        let material = openrt_workspace::create_material(
            &*self.store,
            kind,
            title,
            source,
            reference,
            operator,
            &now,
        )
        .await?;
        self.audit(
            operator,
            "workbench.material.create",
            &material.id,
            "created",
            serde_json::json!({ "kind": kind.to_string(), "title": title }),
        )
        .await?;
        Ok(material)
    }

    /// All materials, deterministically ordered.
    pub async fn list_materials(&self) -> Result<Vec<openrt_workspace::Material>, Error> {
        Ok(openrt_workspace::list_materials(&*self.store).await?)
    }

    /// Fetch one material (fail-closed).
    pub async fn material(&self, id: &str) -> Result<openrt_workspace::Material, Error> {
        Ok(openrt_workspace::get_material(&*self.store, id).await?)
    }

    /// Attach a material to a workbench (validated, idempotent).
    pub async fn attach_material_to_workbench(
        &self,
        material_id: &str,
        workbench_code: &str,
        operator: &str,
    ) -> Result<openrt_workspace::Material, Error> {
        let now = openrt_core::now().to_rfc3339();
        let material =
            openrt_workspace::attach_to_workbench(&*self.store, material_id, workbench_code, &now)
                .await?;
        self.audit(
            operator,
            "workbench.material.attach",
            material_id,
            "attached",
            serde_json::json!({ "workbench": workbench_code }),
        )
        .await?;
        Ok(material)
    }

    /// Attach a material to a room (validated, idempotent).
    pub async fn attach_material_to_room(
        &self,
        room_id: &str,
        material_id: &str,
        operator: &str,
    ) -> Result<Room, Error> {
        let now = openrt_core::now().to_rfc3339();
        let room =
            openrt_workspace::attach_material(&*self.store, room_id, material_id, operator, &now)
                .await?;
        self.audit(
            operator,
            "workbench.room.material.attach",
            room_id,
            "attached",
            serde_json::json!({ "material": material_id }),
        )
        .await?;
        Ok(room)
    }

    /// Detach a material from a room (records are never destroyed by
    /// link removal).
    pub async fn detach_material_from_room(
        &self,
        room_id: &str,
        material_id: &str,
        operator: &str,
    ) -> Result<Room, Error> {
        let now = openrt_core::now().to_rfc3339();
        let room =
            openrt_workspace::detach_material(&*self.store, room_id, material_id, operator, &now)
                .await?;
        self.audit(
            operator,
            "workbench.room.material.detach",
            room_id,
            "detached",
            serde_json::json!({ "material": material_id }),
        )
        .await?;
        Ok(room)
    }

    /// Detach a material from a workbench (records are never destroyed by
    /// link removal).
    pub async fn detach_material_from_workbench(
        &self,
        material_id: &str,
        workbench_code: &str,
        operator: &str,
    ) -> Result<openrt_workspace::Material, Error> {
        let material =
            openrt_workspace::detach_from_workbench(&*self.store, material_id, workbench_code)
                .await?;
        self.audit(
            operator,
            "workbench.material.detach",
            material_id,
            "detached",
            serde_json::json!({ "workbench": workbench_code }),
        )
        .await?;
        Ok(material)
    }

    /// Append an operator steer directive to a room (append-only; hydrated
    /// into the next run's blackboard at run start — never rewritten).
    pub async fn room_steer(
        &self,
        room_id: &str,
        operator: &str,
        input: &str,
    ) -> Result<SteerEntry, Error> {
        let now = openrt_core::now().to_rfc3339();
        let entry =
            openrt_workspace::steer_room(&*self.store, room_id, operator, input, &now).await?;
        self.audit(
            operator,
            "room.steer",
            room_id,
            "steered",
            serde_json::json!({ "input_len": input.len() }),
        )
        .await?;
        Ok(entry)
    }
}
