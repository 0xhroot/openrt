//! Phase 7B MCP integration (M7B.5): stdio JSON-RPC servers whose tools are
//! imported into the **one existing** `ToolRegistry` — no parallel registry,
//! no second permission system.
//!
//! Design (mission-locked):
//! - Startup is fail-closed per server: connect + `initialize` +
//!   `tools/list` must all succeed within short timeouts or the server's
//!   tools are simply absent from the registry (reasons kept for
//!   `mcp.servers` / `mcp.discover`).
//! - Every imported tool requires **two** capabilities: `mcp.invoke.all`
//!   (global) and `mcp.<server>.invoke` (per-server) — the 3-segment
//!   capability rule makes a 4-segment form impossible; deny-by-default
//!   semantics are unchanged (both grants required).
//! - Arguments are validated client-side against the server's `inputSchema`
//!   (JSON-Schema subset) *before* any request is sent; schemas with
//!   unenforceable types are rejected at import and reported by discover.
//! - Server `env` values are passed to the child process only — never
//!   logged, never serialized into any output (env key names only).
//! - Drift: the startup registration is fixed; `mcp.discover` re-runs
//!   `tools/list` and reports added/removed/rejected tools honestly.

pub mod client;
pub mod manager;
pub mod schema;
pub mod store;

pub use client::{extract_text, McpClient, RemoteTool};
pub use manager::{management_tools, ImportedTool, McpManager, McpTool};
pub use store::{
    now_ms, resolve_env_keys, validate_name, McpDiscoveredTool, McpHealthReport, McpOrigin,
    McpRegistry, McpRegistryError, McpServerRecord, SourceDecl, MCP_SERVERS_NS, SECURITY_CLEAN,
    SECURITY_FLAGGED, SECURITY_UNSCANNED,
};
