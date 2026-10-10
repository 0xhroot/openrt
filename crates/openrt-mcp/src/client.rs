//! One MCP server connection over **stdio JSON-RPC** (newline-delimited).
//!
//! Security/lifecycle rules (mission-locked):
//! - connect + `initialize` + `tools/list` happen at wiring time with short
//!   timeouts; a server that fails any step is **not imported** (fail-closed
//!   per server) and the reason is kept for `mcp.servers`/`mcp.discover`.
//! - Server `env` values are passed to the child process only — never
//!   logged, never returned in any tool output.
//! - Every `tools/call` validates arguments against the server-provided
//!   schema (client-side subset validator) *before* the request is sent.
//! - Server stdout is read by a dedicated thread; pending requests are
//!   correlated by JSON-RPC id with per-call timeouts, so a hung or dead
//!   server produces structured `TIMEOUT`/`BACKEND_FAILURE` — never a hang.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use openrt_config::McpServerConfig;
use openrt_fabric::{FabricError, SupportState};

use crate::schema;

pub const INIT_TIMEOUT: Duration = Duration::from_secs(5);
pub const LIST_TIMEOUT: Duration = Duration::from_secs(5);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A tool as advertised by the server (`tools/list`).
#[derive(Debug, Clone)]
pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Set when the tool was refused at import (insane schema / no name);
    /// it is kept only so `mcp.discover` can report the rejection.
    pub rejected: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

pub struct McpClient {
    pub server: String,
    pub command: String,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    tools: Arc<Mutex<Vec<RemoteTool>>>,
    /// Last server stderr lines (bounded) for diagnostics — server `env`
    /// values are never written here (we only mirror the server's own output).
    stderr_tail: Arc<Mutex<Vec<String>>>,
    /// `name version (protocol X)` from `initialize`.
    server_info: Mutex<String>,
}

fn conn_err(server: &str, e: impl std::fmt::Display) -> FabricError {
    FabricError::backend_failure(
        format!("MCP server `{server}`"),
        e.to_string(),
        "mcp.*",
        "check the server process; fix or remove it in fabric.mcp.servers",
    )
}

fn unavailable(server: &str, e: impl std::fmt::Display) -> FabricError {
    FabricError::unavailable(
        format!("MCP server `{server}`"),
        e.to_string(),
        "mcp.*",
        "fix fabric.mcp.servers config (command exists, args correct) or remove the server",
    )
}

impl McpClient {
    /// Spawn the child, complete the MCP handshake, and list tools.
    /// All steps must succeed within their timeouts or nothing is imported.
    pub fn start(cfg: &McpServerConfig) -> Result<Arc<Self>, FabricError> {
        if cfg.command.trim().is_empty() {
            return Err(unavailable(&cfg.name, "empty command"));
        }
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(&cfg.env); // env values go to the child only — never logged
        let mut child = cmd
            .spawn()
            .map_err(|e| unavailable(&cfg.name, format!("spawn `{}`: {e}", cfg.command)))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| unavailable(&cfg.name, "child stdout unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| unavailable(&cfg.name, "child stdin unavailable"))?;
        let stderr = child.stderr.take();

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        // stderr drain thread (bounded, diagnostics only).
        if let Some(stderr) = stderr {
            let tail = stderr_tail.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut t = tail.lock().expect("stderr tail poisoned");
                    t.push(line);
                    if t.len() > 50 {
                        t.remove(0);
                    }
                }
            });
        }

        let stdin = Arc::new(Mutex::new(stdin));
        let reader_stdin = stdin.clone();
        let reader_pending = pending.clone();
        // stdout reader thread: correlate responses by id, refuse
        // server-initiated requests, drop notifications, fail all pending
        // requests on EOF.
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                let n = match reader.read_line(&mut line) {
                    Ok(n) => n,
                    Err(e) => {
                        let mut p = reader_pending.lock().expect("pending poisoned");
                        for (_, tx) in p.drain() {
                            let _ = tx.send(Err(format!("server stdout read failed: {e}")));
                        }
                        return;
                    }
                };
                if n == 0 {
                    let mut p = reader_pending.lock().expect("pending poisoned");
                    for (_, tx) in p.drain() {
                        let _ = tx.send(Err("server closed its stdout (process exited)".into()));
                    }
                    return;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let parsed: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => continue, // tolerate non-JSON noise on stdout
                };
                let Some(id) = parsed.get("id").and_then(Value::as_u64) else {
                    continue; // notification (no id) — ignored on purpose
                };
                if parsed.get("method").is_none() {
                    // Response to one of our requests.
                    let tx = reader_pending.lock().expect("pending poisoned").remove(&id);
                    if let Some(tx) = tx {
                        if let Some(err) = parsed.get("error") {
                            let msg = err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown server error");
                            let _ = tx.send(Err(msg.to_string()));
                        } else {
                            let _ =
                                tx.send(Ok(parsed.get("result").cloned().unwrap_or(Value::Null)));
                        }
                    }
                    continue;
                }
                // Server-initiated request: refuse politely (no client
                // capabilities such as sampling are offered).
                let reply = json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601,
                        "message": "method not found (openrt-mcp client does not implement server->client requests)" }
                });
                if let Ok(mut w) = reader_stdin.lock() {
                    let _ = writeln!(w, "{reply}");
                    let _ = w.flush();
                }
            }
        });

        let client = Arc::new(Self {
            server: cfg.name.clone(),
            command: cfg.command.clone(),
            child: Arc::new(Mutex::new(child)),
            stdin,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            tools: Arc::new(Mutex::new(Vec::new())),
            stderr_tail,
            server_info: Mutex::new(String::new()),
        });

        // initialize → notifications/initialized → tools/list (paged).
        let init = client.rpc_sync(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "openrt", "version": env!("CARGO_PKG_VERSION") }
            }),
            INIT_TIMEOUT,
        )?;
        client.notify("notifications/initialized", json!({}))?;

        let protocol = init
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let server_name = init
            .pointer("/serverInfo/name")
            .and_then(Value::as_str)
            .unwrap_or(&cfg.name);
        let version = init
            .pointer("/serverInfo/version")
            .and_then(Value::as_str)
            .unwrap_or("?");
        *client.server_info.lock().expect("info poisoned") =
            format!("{server_name} {version} (protocol {protocol})");

        *client.tools.lock().expect("tools poisoned") = client.list_tools_sync()?;

        Ok(client)
    }

    /// `tools/list` with paging (16 pages max) + import-time rejection of
    /// tools whose schemas can never be enforced. Runs blocking — call from
    /// startup or `spawn_blocking`.
    fn list_tools_sync(&self) -> Result<Vec<RemoteTool>, FabricError> {
        let mut tools: Vec<RemoteTool> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..16 {
            let mut params = json!({});
            if let Some(c) = cursor.take() {
                params["cursor"] = json!(c);
            }
            let page = self.rpc_sync("tools/list", params, LIST_TIMEOUT)?;
            let list = page
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for t in &list {
                let name = match t.get("name").and_then(Value::as_str) {
                    Some(n) if !n.is_empty() => n.to_string(),
                    other => {
                        tools.push(RemoteTool {
                            name: format!("__rejected__unnamed{}", tools.len()),
                            description: "REJECTED at import: tool has no name".into(),
                            input_schema: json!({ "type": "object" }),
                            rejected: Some(format!("tool has no name: {other:?}")),
                        });
                        continue;
                    }
                };
                let input = t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object" }));
                if let Err(why) = schema::schema_is_sane(&input) {
                    // Fail-closed for THIS tool: not imported, reported later
                    // under its real name with the rejection reason.
                    tools.push(RemoteTool {
                        name: name.clone(),
                        description: format!("REJECTED at import: {why}"),
                        input_schema: json!({ "type": "object" }),
                        rejected: Some(format!("{name}: {why}")),
                    });
                    continue;
                }
                tools.push(RemoteTool {
                    name,
                    description: t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("(no description)")
                        .to_string(),
                    input_schema: input,
                    rejected: None,
                });
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// Re-run `tools/list` (for `mcp.discover` drift reporting).
    pub async fn refresh_tools(self: &Arc<Self>) -> Result<Vec<RemoteTool>, FabricError> {
        let this = self.clone();
        let tools = tokio::task::spawn_blocking(move || this.list_tools_sync())
            .await
            .map_err(|e| {
                FabricError::backend_failure(
                    format!("MCP server `{}`", self.server),
                    format!("tools/list worker failed: {e}"),
                    "mcp.discover",
                    "retry; if it persists the runtime is unhealthy",
                )
            })??;
        *self.tools.lock().expect("tools poisoned") = tools.clone();
        Ok(tools)
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), FabricError> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_line(&msg)
    }

    fn write_line(&self, msg: &Value) -> Result<(), FabricError> {
        let mut w = self.stdin.lock().expect("stdin poisoned");
        writeln!(w, "{msg}")
            .and_then(|_| w.flush())
            .map_err(|e| conn_err(&self.server, format!("write to server stdin: {e}")))
    }

    /// One request/response with timeout (startup/sync contexts; tool calls
    /// go through [`McpClient::call_tool`], which wraps this in
    /// `spawn_blocking`).
    fn rpc_sync(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, FabricError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending
            .lock()
            .expect("pending poisoned")
            .insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write_line(&msg) {
            self.pending.lock().expect("pending poisoned").remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(conn_err(&self.server, format!("{method}: {e}"))),
            Err(_) => {
                self.pending.lock().expect("pending poisoned").remove(&id);
                Err(FabricError::timeout(
                    format!("MCP server `{}` {method}", self.server),
                    format!("no response within {timeout:?}"),
                    "mcp.*",
                    "the server hung — fix the server or remove it from fabric.mcp.servers",
                ))
            }
        }
    }

    pub fn listed_tools(&self) -> Vec<RemoteTool> {
        self.tools.lock().expect("tools poisoned").clone()
    }

    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().expect("stderr poisoned").clone()
    }

    pub fn server_info(&self) -> String {
        self.server_info.lock().expect("info poisoned").clone()
    }

    pub fn support(&self) -> SupportState {
        SupportState::Green
    }

    /// Validate arguments, then `tools/call` with a timeout. `isError`
    /// results become structured BACKEND_FAILURE (ok:false), never success.
    pub async fn call_tool(
        self: &Arc<Self>,
        mcp_name: &str,
        args: Value,
    ) -> Result<Value, FabricError> {
        let tool = self
            .listed_tools()
            .into_iter()
            .find(|t| t.name == mcp_name && t.rejected.is_none())
            .ok_or_else(|| {
                FabricError::not_found(
                    format!("MCP tool `{mcp_name}`"),
                    format!(
                        "server `{}` does not list it (drift or rejected at import)",
                        self.server
                    ),
                    "call mcp.discover to see the server's current tools",
                )
            })?;
        if let Err(why) = schema::validate(&tool.input_schema, &args) {
            return Err(FabricError::invalid_input(
                format!("MCP tool `{mcp_name}`"),
                format!("arguments rejected by server schema: {why}"),
                "fix the arguments to match the tool's inputSchema (mcp.servers shows schemas)",
            ));
        }
        let this = self.clone();
        let name = mcp_name.to_string();
        let result = tokio::task::spawn_blocking(move || {
            this.rpc_sync(
                "tools/call",
                json!({ "name": name, "arguments": args }),
                CALL_TIMEOUT,
            )
        })
        .await
        .map_err(|e| {
            FabricError::backend_failure(
                format!("MCP server `{}`", self.server),
                format!("call worker failed: {e}"),
                "mcp.*",
                "retry; if it persists the runtime is unhealthy",
            )
        })??;

        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let text = extract_text(&result);
            return Err(FabricError::backend_failure(
                format!("MCP tool `{mcp_name}` on server `{}`", self.server),
                if text.is_empty() {
                    "server reported isError without text".into()
                } else {
                    text
                },
                "mcp.*",
                "fix the arguments or the server; see the server's error text above",
            ));
        }
        Ok(result)
    }

    /// Ensure the child leaves no orphan (stdin close is limited by the
    /// reader thread's handle, so kill/wait is the reliable path).
    pub fn shutdown(&self) {
        if let Ok(mut child) = self.child.lock() {
            for _ in 0..5 {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Arc normally keeps this alive until the manager drops it; make
        // sure a mid-handshake failure never leaks the process.
        self.shutdown();
    }
}

/// Pull text contents out of an MCP `tools/call` result.
pub fn extract_text(result: &Value) -> String {
    let mut out = String::new();
    if let Some(list) = result.get("content").and_then(Value::as_array) {
        for c in list {
            match c.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(c.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                Some("image") => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str("<image>");
                }
                _ => {}
            }
        }
    }
    out
}
