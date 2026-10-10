# PHASE 7B ACCEPTANCE — Remaining Tool Fabric

**Phase 7B (ROADMAP v0.4, M7B.1–M7B.10) is COMPLETE.**
Gate run: 2026-09-25, offline workspace, host = Omarchy / Hyprland /
Wayland — LibreOffice 26.8.0.3, Firefox 155.0.1, python3 3.14.

## Final status

| Phase | Status |
|---|---|
| **PHASE 7A — Computer Control** | **GREEN** (frozen; regression `p7_tool_fabric` 8/0 unchanged) |
| **PHASE 7B — Remaining Tool Fabric** | **GREEN** |

## Gates

| Gate | Command | Result |
|---|---|---|
| Format | `cargo fmt --all -- --check` | **RC=0** |
| Check | `cargo check --workspace --offline` | **RC=0**, zero warnings |
| Tests | `cargo test --workspace --offline` | **RC=0 → 825 passed / 0 failed** (142 suites; up from 734 at Phase 7A) |

### 7B test surface (all new, all green)

| Suite | Count | Result |
|---|---|---|
| `p7b_fabric_deny.rs` | 4/0 | registration completeness (37 tool names), deny-by-default batch, `tools.list` metadata completeness (44 covered rows, 3-segment caps, self-snapshot), kill-scope matrix |
| `p7b_database_sqlite.rs` | 3/0 | SQLite connect→transaction→query→insert+verify round-trip, read-only enforcement, list/describe, session begin/rollback, no-connection UNAVAILABLE |
| `p7b_office_roundtrip.rs` | 4/0 | xlsx create/read/write + overwrite refusal, pptx/docx round-trips with OOXML read-back verification, real soffice PDF/PNG (or honest UNAVAILABLE), artifact SHA-256 provenance |
| `p7b_browser.rs` | 3/0 | mock interaction contract through the gate, Sovereign `POLICY_DENIED` on external egress, `auto` never selects the mock (+ real-firefox data: navigation on this host) |
| `p7b_mcp_runtime.rs` | 4/0 | dual-cap deny matrix (none/global-only/per-server-only), real python child invocation, schema refusal, secret hygiene (env values never in output), drift discovery |
| `p7b_cross_tool.rs` | 3/0 | database→excel pipeline in one runtime, agent-driven scripted excel round-trip (provider→agent→gate→tool), kill switch ⇒ `KillSwitchEngaged` |
| `openrt-fabric` unit | 14/0 | metas, artifacts + tamper detection, net_guard (Sovereign/egress), adapters, `tools.list` |
| `openrt-database` unit | 6/0 | sqlite backend (transactions, busy→timeout), postgres UNAVAILABLE honesty, tool args |
| `openrt-office` unit | 24/0 | zip/OOXML round-trips (crc32 == zlib), soffice probe/convert, overwrite guards, **real LibreOffice opens all three formats** |
| `openrt-browser` unit | 16/0 | WS handshake (RFC vector), frame codec, sha1 (RFC vectors), mock contract, **`tests/real_firefox.rs` live BiDi session** |
| `openrt-mcp` unit + e2e | 10/0 | schema subset validator, client RPC, fake-server fixture (import/invoke/isError/fail-closed/hang-timeout/drift) |
| `openrt-config` unit | 8/0 | FabricConfig validation incl. `browser.binary` |

### 7A regression (frozen — must stay green)

| Suite | Count | Result |
|---|---|---|
| `p7_tool_fabric.rs` | 8/0 | unchanged |
| `openrt-safety` | 6/0 | extended with 7B prefixes, all prior cases intact |
| `openrt-runtime` | 34/0 | unchanged behavior + wiring additions |

## Acceptance criteria (12/12 GREEN on this host)

| # | Criterion | Evidence |
|---|---|---|
| 1 | Production caller | All 7B tools registered in `runtime.rs` through the ONE `ToolRegistry` + `GateExecutor`; exercised via `rt.executor.execute` in all six `p7b_*` suites and through the full agent path in `p7b_cross_tool::agent_driven_excel_roundtrip` |
| 2 | Deny-by-default | `p7b_fabric_deny::fabric_tools_deny_by_default` (13-tool batch ⇒ `PermissionDenied`); MCP dual-cap matrix proves global AND per-server rules independently; forensic grep: no second registry/policy construct outside tests |
| 3 | Permission enforcement | Gate evaluates tool id then every declared cap (glob rules); `tools.list` runs only with `tools.discovery.all`, imported MCP tools only with `mcp.invoke.all` + `mcp.fake.invoke` |
| 4 | Sovereign Mode | `p7b_browser::sovereign_denies_external_navigation` ⇒ structured `POLICY_DENIED` before egress; `data:`/loopback remain usable; mode checked before network policy in `net_guard` |
| 5 | Real backends | LibreOffice converts our xlsx/pptx/docx to >1 KB real PDFs (`office::render_honesty…`, crate test `real_libreoffice_opens_all_three_formats`); Firefox 155 BiDi live session (`openrt-browser/tests/real_firefox.rs`); rusqlite real file round-trips |
| 6 | Observation + verification | Every write self-verifies (OOXML read-back, `verify_sql` re-read, PNG signature + SHA-256 artifact, BiDi DOM re-read); `artifact.list` digest matches `sha256_hex` of file bytes |
| 7 | Failure handling | Structured codes asserted: UNAVAILABLE (no db connection / no soffice), POLICY_DENIED (Sovereign), NOT_FOUND (missing element/sheet), BACKEND_FAILURE (server `isError`), INVALID_INPUT refusals (overwrite, schema) — no fake success anywhere |
| 8 | Secret hygiene | `mcp.servers`/`mcp.discover` outputs contain env key names but never values (asserted against a planted secret); DSNs referenced by env var only |
| 9 | Resource/kill consistency | `tool_scope` + `is_computer_tool` extended with `database.`/`excel.`/`ppt.`/`docx.`/`mcp.`/`application.`/`browser.`; `kill_switch_halts_fabric_tools` ⇒ `KillSwitchEngaged`; read-only discovery stays available |
| 10 | Discovery completeness | `tools.list` covers every 7B tool with category/risk/platforms and 3-segment caps (44 rows); `mcp.discover` reports startup drift (`added`/`removed`/`rejected`) |
| 11 | Cross-tool orchestration | `database_to_excel_pipeline` (sqlite → query → workbook → read-back), agent scripted round-trip, office + artifacts in one runtime — all through one gate |
| 12 | Integration tests | 21 new `p7b_*` integration tests + 70 crate tests; full workspace 825/0 |

## Honest limitations (recorded, not claimed GREEN)

* **MCP third-party interoperability** is proven against the in-repo fake
  stdio server (python), not against external servers (offline host).
* **PostgreSQL live path** unproven (no server on host): structured
  UNAVAILABLE is the tested behavior; session transactions are honestly
  UNAVAILABLE over one-shot psql.
* **Post-hoc MCP registration** refused by design (registration happens at
  wiring); `mcp.discover` surfaces drift for the operator instead.
* **Download redirects** are refused (`redirect:"error"`) rather than
  followed, so egress policy stays enforceable; 8 MB cap.
* **`auto` browser** resolves to Firefox here (Green) or UNAVAILABLE — the
  mock is only ever explicit (`backend: "mock"`), never a fallback.
* Office = file-level OOXML + headless soffice, **not** GUI macro control.
* Application adapters **describe and verify**; launching remains the frozen
  7A `computer.app` (no second launcher).
