# PHASE 11 IMPLEMENTATION — Universal AI Workbench

Phase: **11** (OWNERSHIP `## Phase 11 — UNIVERSAL AI WORKBENCH`,
milestones M11.1–M11.10; consolidates the original Extension/Plugin System +
Persistent AI Workbench + Advanced Computer Use mega-phases).
Acceptance evidence (gates, TEST A–H, audits, capability table):
[`PHASE11_ACCEPTANCE.md`](./PHASE11_ACCEPTANCE.md).

## 1. Scope and allocation

Phase 11 adds the universal workbench — extensions, an MCP ecosystem, rooms
+ materials, workflow pause/resume/steer, and the full computer-use loop —
**without allocating a single new crate and without creating a second
authority** (OWNERSHIP Phase 11 reconciliation note, ADR-006):

| Subsystem (milestone) | Crate(s) | Store namespace |
|---|---|---|
| Extension manifest + registry + install pipeline (M11.1) | `openrt-workspace-assets` (extended) | `extensions.packages` (skills payload installs into `assets.skills`) |
| Extension security (M11.1) | `openrt-shield` (extended `scan.rs`) | `shield.findings` (unchanged) |
| Extension→ToolRegistry bindings (M11.1) | `openrt-runtime` (build wiring) | — |
| MCP ecosystem (M11.2) | `openrt-mcp` (extended) + `openrt-config` (`McpTrust`) | `mcp.servers` (the Phase 7B row's claimed namespace) |
| Workbench model + materials (M11.3) | `openrt-workspace` (extended) | `workbench.rooms`, `materials.records` |
| Agent rooms + roles (M11.4) | `openrt-workspace` (records) + `openrt-runtime` (wiring) | `workbench.rooms` |
| Pause / resume / steer (M11.5) | `openrt-orch` (engine pause) + `openrt-workspace` (room state) + `openrt-runtime` (handle) | `workbench.rooms`; checkpoints unchanged |
| Surfaces: WebUI palette (M11.6) | `openrt-webui` | — (views only) |
| Surfaces: TUI panels (M11.7) | `openrt-tui` | — (views only) |
| ComputerUse loop (M11.8) | `openrt-computer` (`computeruse` module) + `openrt-platform` (unchanged `ComputerControl`) + `openrt-vlm` (grounding only) | — (twin is in-memory/derived) |
| Computer-use security (M11.9) | `openrt-shield` (GUI typed-text detector) + `openrt-runtime` gate | `shield.findings` (unchanged) |
| CLI + doctor rows + docs (M11.10) | `openrt-cli`, `openrt-runtime`, `docs/` | — |

Single authorities preserved: one `ToolRegistry`/gate; `openrt-permissions`/
`openrt-policy` remain the permission authorities; `openrt-audit` the only
audit chain (ADR-002); `openrt-safety` the only kill switch (ADR-008, scope
`computer`); `openrt-shield` the only security evaluator; `openrt-workspace`
the only workspace/room/material authority (overlay references + digests,
never copies); `openrt-orch` the only workflow engine; `openrt-platform`
the only computer-control backend; `openrt-workspace-assets` the only
package authority (extensions extend it); `openrt-mcp` the only MCP
registry (stdio only); marketplace/local catalog has no network backend.

## 2. The fail-closed contract

1. **Deny by default** — `computer.use`, extension-bound tools, and MCP
   tools all require explicit allow rules (`computer.use(.all)`, `ext.*`,
   `mcp.invoke.all` + `mcp.<server>.invoke` + tool-id). Default configs
   grant none of them.
2. **Import never Active** — extensions land `Registered`/unscanned;
   activation is a separate operator action after validate + scan
   (`extension activate` is scan-first, fail-closed on flagged).
3. **Trust never bypasses the gate** — MCP trust only unlocks the *spawn*
   gate; every invocation still runs policy + shield + resource admission.
4. **Non-ok results fail closed** — `OperationResult{ok:false}` maps to a
   `ComputerUseError` (Unavailable/Control) before any verification loop,
   so the operator never sees a misleading "verification failed" for a
   denied/unavailable backend (`fn ctrl` in `computeruse.rs`).
5. **Honest UNAVAILABLE** — pointer injection without `ydotool`/`dotool`,
   screenshot capture without a host tool, remote/HTTP MCP, and remote
   marketplace fetch all report `UNAVAILABLE`/`BLOCKED` with reasons; the
   doctor never emits `Fail` rows.
6. **Bounded recovery** — verification retries `MAX_ATTEMPTS = 3`, then
   `ComputerUseError::Verify`.
7. **Payloads never persist** — typed `text`/`content` is reported by
   length only in plans/results; shield findings record `target = tool`;
   audit entries carry tool + outcome; and the durable event log receives a
   length marker (`<redacted: N chars>`) for GUI tools' typed payloads at
   the gate emit (`gate.rs` `PermissionRequested`).

## 3. Key design decisions

### 3.1 Capability and execution routing (M11.8)

- `Capability = computer.use.all` gates the single `computer.use` tool
  (three-part dotted; `computer.use` + `computer.use.all` rules both match).
- Action routing: focus/type/key/hotkey/workspace/clipboard/close/launch/
  navigate → existing `ControlOp`s; click = `MovePointer` + `ClickPointer`
  (button 1 only, else `Invalid`); scroll = `MovePointer` + wheel events
  (1 step/100, clamp 1..=10); drag = `pointer_drag` single attempt verified
  `None`. `openrt-platform::ComputerControl` is unchanged — no new op
  variants were allocated.
- Resolution chain: accessibility (honestly unavailable on this host) →
  window-tree → elements → vision (`VISION_CONFIDENCE = 0.6`) → coordinate.
  Failures carry the full `chain:` trace (CLI `computer resolve` exits 1).
- The digital twin is invalidated after every execute attempt; the shared
  engine exposes `twin_status()` so every surface proves it hit the one
  engine.
- `Plan.structured_alternative` prefers the structured tool
  (`computer.window`, `computer.app`, `computer.input`, `input.mouse`,
  `computer.clipboard`) per action; scroll/workspace have none.

### 3.2 One shared engine, one gate

- `Runtime::computer_engine: Arc<ComputerUse>` is the single engine
  (constructed once in `runtime.rs`); the registered `ComputerUseTool`
  holds the same `Arc`. Doctor's `computer.twin` row builds a transient
  instance for an observe-only probe (never executes).
- Production callers: CLI `computer {env,observe,resolve}` read through the
  shared engine; CLI `computer use`, the palette `computer.use` command
  (`Runtime::computer_use_dispatch`, channel recorded), and every agent
  tool call all execute through `rt.executor` — the ONE gate (policy →
  AgentShield → resources → sandbox admission → kill scope → execute →
  audit). TUI renders `computer_env_snapshot()` only (read-only).
- `Runtime::computer_use_dispatch(channel, args)` is the canonical
  UI-facing entry: it forwards to `executor.execute("computer.use", …)` so
  no surface can skip the gate.

### 3.3 AgentShield integration (M11.9)

- The gate's existing M10.9 step evaluates every call; for GUI tools
  (`computer.*`/`input.*`/`screen.*`) leaf `text`/`content` fields feed the
  ONE detector (`openrt_shield::evaluate_command_strings`, `is_gui_tool`).
- Most restrictive action maps onto existing authorities: `Deny` →
  `PermissionDenied` (reason names AgentShield), `Halt` → kill switch trip
  + `KillSwitchEngaged`, `RequireApproval` → the approval resolver. A scan
  failure blocks the call closed.

### 3.4 Workbench overlay (M11.3–M11.5)

- Rooms/materials are records + references + digests over the one store;
  `verify_material(store, id)` re-derives digests from live references.
- Roles (`researcher`/`analyst`/`coder`/`reviewer`/`verifier`) map onto
  existing `SubAgentKind` identities — configuration, never new authorities.
- Pause is cooperative and durable (paused runs survive restart and are
  never auto-resumed by boot recovery); steer appends only; the room's
  workflow pointer is bookkeeping — checkpoints remain the run authority.

### 3.5 Doctor rows (M11.10)

Mandated rows exist and never `Fail`: `extensions`, `marketplace`,
`mcp.ecosystem`, `workbench`, `rooms`, `computer.vision`,
`computer.grounding`, `computer.twin`, `computer.verify`,
`workbench.recovery` (+ `mcp.remote-transport` honest `UNAVAILABLE`).

## 4. Surfaces

| Surface | Phase 11 surface area |
|---|---|
| CLI | `extension *`, `mcp *`, `workbench {room,material} *`, `room *`, `material *`, `computer {env,observe,resolve,use}`; all execution paths through the gate |
| TUI | Ops tabs: rooms (5), extensions (6), computer (7) — all rendered from `Runtime` reads (`ops_rooms`, extension states, `computer_env_snapshot`) |
| WebUI | Palette commands `extension.*`, `mcp.*`, `room.*`, `material.*`, `computer.{env,observe,use}` over `CommandPalette::with_runtime` (server-issued operator tokens; `observe` clipboard on demand only) |
| Doctor | Rows above + audit chain verify |

No feature exists only inside a UI: every palette/TUI action is a Runtime
API also reachable from the CLI, and none of them carries authoritative
state.

## 5. Test architecture

| Suite | Count | Covers |
|---|---|---|
| `p11_extensions` | 12 | M11.1 lifecycle, binding, rollback, single-active |
| `p11_workbench` | 12 | M11.3–M11.5 rooms/materials/pause/steer/restart |
| `p11_cross_surface` | 1 | M11.6/M11.7 shared-state proof (`h1_palette_room_visible_in_tui_and_steer_flows_both_ways`) |
| `p11_computeruse` | 13 | M11.8/M11.9 engine, resolution, verification, deny/halt/kill |
| `p11_integration` | 12 | t37–t48 mandated integration categories |
| `p11_e2e` | 8 | TEST A–H full-journey E2E |
| crate units | runtime 78, computer 25, shield 39, mcp 14, tui 12, webui (tests/) 28, workspace/assets 33, orch 19 | phase internals |

Dedicated Phase 11 integration total: **58 tests** (12+12+1+13+12+8).
Workspace total: **1380 passed / 0 failed** (Phase 10 baseline 1283; the
delta is the 58 `p11_*` tests plus Phase 11 crate units — computeruse
engine units, runtime phase11 wiring/doctor units, TUI ops panels, WebUI
palette commands — with zero regressions). See
[`PHASE11_ACCEPTANCE.md`](./PHASE11_ACCEPTANCE.md).
