# PHASE 7B IMPLEMENTATION — Remaining Tool Fabric

**Phase 7B (ROADMAP v0.4) implementation record.** Written 2026-09-25,
offline workspace, host = Omarchy / Hyprland / Wayland (LibreOffice
26.8.0.3, Firefox 155.0.1, python3 3.14).

Phase 7B adds five tool families — **browser automation, MCP integration,
database tools, application tools, Excel/PPT/DOCX automation** — plus the
cross-tool orchestration and discovery surfaces that tie them together. All
of it runs through the Phase 7A execution stack unchanged: one `ToolRegistry`,
one `GateExecutor`, one `PolicyEvaluator`, one Sovereign-Mode authority, one
`ResourceManager`, one audit log, one verification posture.

Phase 7A (`openrt-platform`, `computer.*`) is **frozen**; this phase only
adds registrations, metadata, and two scope-prefix extensions.

## 1. Architecture

```
                    RuntimeBuilder::build (runtime.rs)
                              │
   ┌──────────────┬───────────┼────────────┬──────────────┬─────────────┐
   │              │           │            │              │             │
 database.*     excel/ppt/   browser.*     mcp.*         application.* artifact.list
 (openrt-       docx.*       (openrt-     (openrt-mcp)   + tools.list   (openrt-
  database)     (openrt-      browser)                     (openrt-      fabric)
                 office)                                      fabric)
   └──────────────┴───────────┴────────────┴──────────────┴─────────────┘
                              │
                    ONE ToolRegistry ── GateExecutor
                              │   ├─ policy (deny-by-default, glob rules)
                              │   ├─ mode (Sovereign blocks egress)
                              │   ├─ resource gate
                              │   ├─ kill switch (Computer scope)
                              │   └─ chained audit (ADR-002)
                              ▼
                      registry.provider_tools() / agent
```

Registration order inside `build()` (after the frozen 7A computer tools,
before the executor is constructed):

1. `FabricState` (catalog + artifact store + application adapters) over the
   runtime's `PersistenceStore`.
2. `database_tools` → `excel/ppt/docx` tools → `browser_tools` →
   `mcp.imported_tools()` + `mcp.servers`/`mcp.discover` →
   `application.list`/`application.verify` → `artifact.list`.
3. Metadata: `computer_metas` (7A) + `fabric_metas` + `database_metas` +
   `office_metas` + `browser_metas` + `McpManager::metas` all register into
   the single `FabricCatalog`.
4. `tools.list` registers **last**, then snapshots the completed registry
   (`set_registry`) so discovery includes `tools.list` itself.

Wiring is **fail-soft**: a subsystem that cannot start registers zero tools
or honest UNAVAILABLE tools; startup never blocks on a backend probe (LibreOffice
and MCP probing are bounded; MCP timeouts are 5 s initialize / 5 s list /
30 s call, and only run for *configured* servers — none by default).

## 2. Crates

| Crate | Contents |
|---|---|
| `openrt-fabric` | `FabricMeta`/`FabricCatalog` (`tools.list` discovery), `ArtifactStore` (SHA-256 provenance), `net_guard` (mode → network-policy egress check), application adapters (`application.list`/`application.verify`), `fail()` error mapping |
| `openrt-database` | SQLite (rusqlite, bundled) + PostgreSQL (`psql` argv, structured UNAVAILABLE without a server) behind 11 `database.*` tools |
| `openrt-office` | Pure-Rust OOXML writers (stored-ZIP xlsx/pptx/docx, self-verified read-back), `soffice --headless --convert-to` render/export with unique temp profiles |
| `openrt-browser` | RFC 6455 WebSocket client, RFC 3174 SHA-1 handshake, Firefox WebDriver BiDi backend, deterministic mock backend, 11 in-page `browser.*` tools |
| `openrt-mcp` | Stdio JSON-RPC MCP client (initialize → tools/list → tools/call), fail-closed per server, client-side schema subset validation, `McpManager` inventory/drift discovery |

## 3. Permission model (no second system)

* Every tool keeps its `ToolSpec.required_caps`; the gate evaluates the
  **tool id first**, then **each declared capability** as its own permission
  request (M6.2 semantics — capability annotations are enforcement).
  Policy rules are **glob patterns** matched against both.
* Caps are exactly 3 segments (`Capability::from_str`); `with_caps` silently
  drops invalid entries, so wiring tests assert parseability.
* **MCP dual cap (deny-by-default):** every imported tool requires BOTH
  `mcp.invoke.all` (global) AND `mcp.<server>.invoke` (per server). A 4-segment
  `mcp.<server>.invoke.all` can never parse, so the per-server cap is the
  3-segment form. `mcp.servers`/`mcp.discover` use `mcp.status.all`.
* **Sovereign Mode:** local schemes (`file`/`data`/`about`/`blob`/
  `moz-extension`) and loopback are always allowed; anything else consults
  the operating mode first (Sovereign ⇒ `POLICY_DENIED` before any packet)
  and then `NetworkPolicy::ensure_egress(scope, dest)` with scopes
  `browser`/`database`/`mcp`.
* **Kill scope (ADR-008):** `tool_scope()` now maps the prefixes `browser.`,
  `database.`, `excel.`, `ppt.`, `docx.`, `mcp.`, `application.` to
  `KillScope::Computer` (read-only `tools.*`/`artifact.*` stay unscoped so a
  computer HALT never blinds the operator's inspection surface).
  `is_computer_tool()` (audit) extends the same prefixes.

## 4. Configuration

`Config.fabric: FabricConfig` (all sections `#[serde(default)]`, pre-7B
configs load unchanged, `deny_unknown_fields`):

* `fabric.browser` — `backend: Option<String>` (`auto`/`firefox`/`mock`/`off`),
  `binary: Option<PathBuf>`.
* `fabric.mcp.servers[]` — `name` (`[a-z0-9_-]`), `command`, `args`,
  `env` (values passed to the child only — never logged, never serialized;
  outputs show key names only), `trust` (records operator intent; caps
  still enforced), `declared_capabilities`.
* `fabric.database` — `connections[]` (`sqlite_path` or `pg_dsn_env`) +
  `default`; no connections ⇒ every `database.*` tool reports structured
  UNAVAILABLE.
* `fabric.office.soffice` — binary override (default: PATH probe).

## 5. Protocol findings recorded during implementation

**Firefox 155 BiDi:** stderr announces `ws://127.0.0.1:<port>` with **no
path**; the real endpoint is **`/session`** (root `/` serves httpd.js info).
Top-level `initialize` is rejected (`Invalid method format`) — the client
sends `session.new` with `{capabilities:{}}` directly and keeps `initialize`
(protocol versions 1.0/2.0/3.0) only as a fallback. Responses are
WebDriver-style envelopes `{type:success|error,id,result|error,message}`
(classic JSON-RPC also accepted). `browsingContext.create` returns
`context`; `script.evaluate` returns `{realm,result,type}` — the value is
`.result` before stringification.

**MCP stdio:** newline-delimited JSON-RPC; server-side `isError:true`
results map to structured `BACKEND_FAILURE`; client-side schema violations
(`type: "banana"` etc.) are rejected **before** any request as
`InvalidArguments`; a tool the server later drops yields structured
`NOT_FOUND`.

**LibreOffice:** `soffice --headless --convert-to <fmt> --outdir <tmp>`
with a unique `-env:UserInstallation` profile, 90 s timeout, non-empty
output verification; the source file is never the output (input guards run
before the backend probe).

**MockProvider:** `with_tool_script(vec![(tool, args), …])` scripts the
`ToolSequence` mode (one entry per model turn, then a stop) so integration
tests can drive real 7B tools through the full agent → gate → tool path.

## 6. Error taxonomy

All five families report through `openrt_fabric::fail()`:
`INVALID_INPUT` → `InvalidArguments` (host-side refusal, nothing sent);
everything else → structured `Ok(Json{ok:false, error:{code,what,why,where,fix}})`
with `UNAVAILABLE` / `POLICY_DENIED` / `PERMISSION_DENIED` / `RESOURCE_DENIED` /
`NOT_FOUND` / `TIMEOUT` / `BACKEND_FAILURE` / `VERIFICATION_FAILED` /
`INTEGRITY_FAILURE`. Tests assert on output payloads, never on fake success.

## 7. Honest limitations (deferred, not claimed)

* **Post-hoc MCP tool registration** (a tool appearing after startup) is not
  possible without refactoring `GateExecutor`; `mcp.discover` therefore
  *reports* added/removed/rejected drift instead of silently registering.
  Startup registration remains the only registration path.
* **PostgreSQL** works only through structured `psql` argv with a DSN env
  var; session transactions report UNAVAILABLE (one-shot psql), and no
  server exists on this host to prove the live path — the honest
  UNAVAILABLE path is what is tested.
* **Downloads** use `fetch()` with `redirect:"error"` (refusing redirects
  keeps egress policy honest) and an 8 MB cap; no streaming resume.
* **`auto` never falls back to the mock backend** — absence of a real backend
  yields UNAVAILABLE, never a fake session.
* The **mock browser backend emulates marker contracts** (it is not a
  browser); real-backend evidence comes from `openrt-browser`'s
  `tests/real_firefox.rs` (live BiDi session on this host).
* Office tools are **file-level automation** (create/read/render/export via
  real OOXML + soffice), not GUI macros.
