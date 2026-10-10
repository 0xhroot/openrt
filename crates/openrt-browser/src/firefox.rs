//! Real browser backend: headless Firefox driven over **WebDriver BiDi**
//! (JSON-RPC 2.0 on a loopback WebSocket implemented in [`crate::ws`]).
//!
//! Launch: `firefox --headless --no-remote --profile <tmp> --remote-debugging-port=0`
//! prints `WebDriver BiDi listening on ws://…` on stderr; we read that line,
//! connect, `initialize`, open a tab, and issue:
//! `browsingContext.navigate`, `script.evaluate`, `browsingContext.captureScreenshot`.
//! Dropping the backend kills the process (no orphan headless instances).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};

use openrt_fabric::FabricError;

use crate::backend::{unwrap_stringified, BrowserBackend};
use crate::ws::{WsClient, WsError};

const STDERR_WAIT: Duration = Duration::from_secs(20);

/// Kill the Firefox child on drop so sessions never leak processes.
struct ChildGuard(Mutex<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(mut child) = self.0.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct Inner {
    client: Mutex<WsClient>,
    context: Mutex<Option<String>>,
    next_id: AtomicU64,
    _child: ChildGuard,
}

fn map_ws(e: WsError, what: &str, fix: &str) -> FabricError {
    match e {
        WsError::Timeout(t) => FabricError::timeout(what, t, fix, "retry; check system load"),
        WsError::Closed(c) => FabricError::backend_failure(
            what,
            c,
            fix,
            "restart the browser tool (browser.close then retry)",
        ),
        WsError::Io(i) => FabricError::backend_failure(
            what,
            i,
            fix,
            "retry; if it persists the browser process died — call browser.close and retry",
        ),
    }
}

impl Inner {
    /// One JSON-RPC request/response cycle (ignores server events).
    fn rpc(&self, method: &str, params: Value) -> Result<Value, FabricError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let text = serde_json::to_string(&msg).map_err(|e| {
            FabricError::backend_failure(
                "browser rpc",
                e.to_string(),
                "browser.*",
                "report this as an openrt-browser bug",
            )
        })?;
        let mut client = self.client.lock().expect("ws client poisoned");
        client
            .send_text(&text)
            .map_err(|e| map_ws(e, "browser rpc send", "browser.*"))?;
        loop {
            let raw = client
                .recv_text()
                .map_err(|e| map_ws(e, "browser rpc receive", "browser.*"))?;
            let parsed: Value = serde_json::from_str(&raw).map_err(|e| {
                FabricError::backend_failure(
                    "browser rpc",
                    format!("non-JSON frame from browser: {e}"),
                    "browser.*",
                    "the BiDi endpoint sent unexpected data; restart the session",
                )
            })?;
            if parsed.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // server event or unrelated response
            }
            // Firefox speaks WebDriver-style envelopes:
            //   {"type":"success","id":N,"result":{...}}
            //   {"type":"error","id":N,"error":"...","message":"..."}
            // while classic JSON-RPC uses {"jsonrpc":"2.0","id":N,...}.
            // Accept both.
            match parsed.get("type").and_then(Value::as_str) {
                Some("success") => return Ok(parsed.get("result").cloned().unwrap_or(Value::Null)),
                Some("error") => {
                    let msg = parsed
                        .get("message")
                        .and_then(Value::as_str)
                        .or_else(|| parsed.get("error").and_then(Value::as_str))
                        .unwrap_or("unknown browser error");
                    return Err(FabricError::backend_failure(
                        "browser rpc",
                        format!("{method}: {msg}"),
                        "browser.*",
                        "check the selector/arguments; see the browser error for details",
                    ));
                }
                _ => {}
            }
            if let Some(err) = parsed.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(FabricError::backend_failure(
                    "browser rpc",
                    format!("{method}: {msg}"),
                    "browser.*",
                    "check the selector/arguments; see the browser error for details",
                ));
            }
            if parsed.get("jsonrpc").is_some() || parsed.get("result").is_some() {
                return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
            }
            return Err(FabricError::backend_failure(
                "browser rpc",
                format!("{method}: unrecognized response envelope: {raw}"),
                "browser.*",
                "report this as an openrt-browser bug with the firefox version",
            ));
        }
    }

    fn context_id(&self) -> Result<String, FabricError> {
        self.context
            .lock()
            .expect("context poisoned")
            .clone()
            .ok_or_else(|| {
                FabricError::backend_failure(
                    "browser session",
                    "no browsing context (tab) is open",
                    "browser.*",
                    "restart the session: call browser.navigate again",
                )
            })
    }
}

pub struct FirefoxBiDi {
    inner: Arc<Inner>,
}

fn probe_firefox_binary() -> Result<PathBuf, FabricError> {
    let path = std::env::var_os("PATH").ok_or_else(|| {
        FabricError::unavailable(
            "browser backend",
            "PATH is not set",
            "browser.*",
            "install firefox or set fabric.browser.backend to mock",
        )
    })?;
    for dir in std::env::split_paths(&path) {
        for name in ["firefox", "firefox-esr"] {
            let c = dir.join(name);
            if c.is_file() {
                return Ok(c);
            }
        }
    }
    Err(FabricError::unavailable(
        "browser backend",
        "no `firefox` binary found on PATH",
        "browser.*",
        "install Firefox, or set fabric.browser.backend to `mock` for tests / `off` to disable",
    ))
}

/// Launch a headless Firefox and connect over BiDi.
pub async fn launch(binary: Option<PathBuf>) -> Result<FirefoxBiDi, FabricError> {
    let bin = match binary {
        Some(b) if b.is_file() => b,
        Some(b) => {
            return Err(FabricError::unavailable(
                "browser backend",
                format!("configured firefox binary `{}` does not exist", b.display()),
                "browser.*",
                "fix fabric.browser.binary or remove the override to probe PATH",
            ))
        }
        None => probe_firefox_binary()?,
    };
    let profile = std::env::temp_dir().join(format!("openrt-ff-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&profile).map_err(|e| {
        FabricError::backend_failure(
            "browser backend",
            format!("cannot create profile dir: {e}"),
            "browser.*",
            "check temp dir permissions",
        )
    })?;
    // Keep a first-run-clean headless profile.
    let _ = std::fs::write(
        profile.join("user.js"),
        "user_pref(\"browser.shell.checkDefaultBrowser\", false);\n\
         user_pref(\"browser.startup.homepage\", \"about:blank\");\n\
         user_pref(\"datareporting.policy.dataSubmissionEnabled\", false);\n\
         user_pref(\"toolkit.telemetry.reportingpolicy.firstRun\", false);\n",
    );

    let mut child = Command::new(&bin)
        .arg("--headless")
        .arg("--no-remote")
        .arg("--profile")
        .arg(&profile)
        .arg("--remote-debugging-port=0")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            FabricError::backend_failure(
                "browser backend",
                format!("cannot spawn `{}`: {e}", bin.display()),
                "browser.*",
                "check the firefox installation",
            )
        })?;

    // Read stderr in a thread until the BiDi URL appears (or timeout).
    let stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Err(format!("stderr read failed: {e}")));
                    return;
                }
            };
            if let Some(pos) = line.find("WebDriver BiDi listening on ws://") {
                let url = line[pos + "WebDriver BiDi listening on ".len()..]
                    .trim()
                    .to_string();
                let _ = tx.send(Ok(url));
                return;
            }
            if line.contains("no debugging socket") || line.contains("Firefox is already running") {
                let _ = tx.send(Err(line));
                return;
            }
        }
        let _ = tx.send(Err("firefox exited before opening a BiDi socket".into()));
    });
    let ws_url = match rx.recv_timeout(STDERR_WAIT) {
        Ok(Ok(url)) => url,
        Ok(Err(reason)) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&profile);
            return Err(FabricError::unavailable(
                "browser backend",
                reason,
                "browser.*",
                "check that no other Firefox instance blocks --no-remote, or set fabric.browser.backend to mock/off",
            ));
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&profile);
            return Err(FabricError::timeout(
                "browser backend startup",
                format!("firefox did not announce a BiDi socket within {STDERR_WAIT:?}"),
                "browser.*",
                "check firefox launch manually (`firefox --headless`); use mock backend in CI",
            ));
        }
    };

    // Firefox prints the BiDi URL without a path; the actual WebSocket
    // endpoint lives at `/session` (root `/` is httpd.js's info page).
    let ws_url = match url::Url::parse(&ws_url) {
        Ok(mut u) if u.path().is_empty() || u.path() == "/" => {
            u.set_path("/session");
            u.to_string()
        }
        Ok(u) => u.to_string(),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&profile);
            return Err(FabricError::backend_failure(
                "browser backend",
                format!("firefox announced a malformed BiDi URL: {e}"),
                "browser.*",
                "check firefox stderr manually; report this as an openrt-browser bug",
            ));
        }
    };
    let client = WsClient::connect(&ws_url, Duration::from_secs(10)).map_err(|e| {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&profile);
        map_ws(e, "browser backend connect", "browser.*")
    })?;

    let inner = Arc::new(Inner {
        client: Mutex::new(client),
        context: Mutex::new(None),
        next_id: AtomicU64::new(0),
        _child: ChildGuard(Mutex::new(child)),
    });

    // Session bootstrap. Firefox 155 rejects the legacy top-level
    // `initialize` method ("Invalid method format") and accepts `session.new`
    // directly; older builds require `initialize` first. Try both shapes.
    let mut boot_err = None;
    let mut bootstrapped = false;
    match inner.rpc("session.new", json!({ "capabilities": {} })) {
        Ok(_) => bootstrapped = true,
        Err(e) => boot_err = Some(e),
    }
    if !bootstrapped {
        for version in ["1.0", "2.0", "3.0"] {
            match inner.rpc(
                "initialize",
                json!({ "protocolVersion": version, "capabilities": {} }),
            ) {
                Ok(_) => {
                    bootstrapped = true;
                    break;
                }
                Err(e) => boot_err = Some(e),
            }
        }
    }
    if !bootstrapped {
        drop(inner); // ChildGuard kills firefox on drop
        let _ = std::fs::remove_dir_all(&profile);
        return Err(boot_err.expect("bootstrap attempts ran at least once"));
    }

    // Open our own tab.
    let created = inner.rpc("browsingContext.create", json!({ "type": "tab" }))?;
    let ctx = created
        .get("context")
        .or_else(|| created.get("contextId"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            FabricError::backend_failure(
                "browser backend",
                "browsingContext.create returned no contextId",
                "browser.*",
                "check firefox BiDi support; report this as an openrt-browser bug",
            )
        })?;
    *inner.context.lock().expect("context poisoned") = Some(ctx);

    Ok(FirefoxBiDi { inner })
}

#[async_trait]
impl BrowserBackend for FirefoxBiDi {
    fn name(&self) -> &'static str {
        "firefox-bidi"
    }

    async fn navigate(&self, url: &str) -> Result<Value, FabricError> {
        let inner = self.inner.clone();
        let url = url.to_string();
        tokio::task::spawn_blocking(move || {
            let ctx = inner.context_id()?;
            let result = inner.rpc(
                "browsingContext.navigate",
                json!({ "context": ctx, "url": url, "wait": "complete" }),
            )?;
            let final_url = result
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Title from the page (best-effort; about: pages return quickly).
            let title = match inner.rpc(
                "script.evaluate",
                json!({
                    "expression": "JSON.stringify(document.title)",
                    "target": { "context": ctx },
                    "awaitPromise": true,
                    "resultOwnership": "none"
                }),
            ) {
                // Firefox wraps command results as
                // {"realm":…,"result":<RemoteValue>,"type":"success"}.
                Ok(r) => {
                    let v = r.get("result").cloned().unwrap_or(r);
                    match unwrap_stringified(&v) {
                        Value::String(s) => s,
                        other => format!("<unparsed title: {other}>"),
                    }
                }
                Err(e) => format!("<title fetch failed: {}>", e.why),
            };
            Ok::<_, FabricError>(json!({ "url": final_url, "title": title }))
        })
        .await
        .map_err(join_err)?
    }

    async fn evaluate(&self, expr: &str) -> Result<Value, FabricError> {
        let inner = self.inner.clone();
        let expr = expr.to_string();
        tokio::task::spawn_blocking(move || {
            let ctx = inner.context_id()?;
            let params = json!({
                "expression": expr,
                "target": { "context": ctx },
                "awaitPromise": true,
                "resultOwnership": "none"
            });
            match inner.rpc("script.evaluate", params) {
                Ok(r) => {
                    // Exception path: BiDi returns an error object instead of throwing.
                    if let Some(exc) = r.get("exceptionDetails") {
                        let text = exc
                            .get("exception")
                            .and_then(|e| e.get("value"))
                            .and_then(Value::as_str)
                            .or_else(|| exc.get("text").and_then(Value::as_str))
                            .unwrap_or("script threw an exception")
                            .to_string();
                        return Err(FabricError::backend_failure(
                            "browser script",
                            text,
                            "browser.*",
                            "fix the selector/expression and retry",
                        ));
                    }
                    Ok(r.get("result").cloned().unwrap_or(Value::Null))
                }
                Err(e) => Err(e),
            }
        })
        .await
        .map_err(join_err)?
    }

    async fn screenshot(&self) -> Result<Vec<u8>, FabricError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let ctx = inner.context_id()?;
            let result = inner.rpc(
                "browsingContext.captureScreenshot",
                json!({ "context": ctx, "format": { "type": "png" } }),
            )?;
            let b64 = result.get("data").and_then(Value::as_str).ok_or_else(|| {
                FabricError::verification_failed(
                    "browser screenshot",
                    "captureScreenshot returned no data field",
                    "retry; check browser window state",
                )
            })?;
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| {
                    FabricError::verification_failed(
                        "browser screenshot",
                        format!("invalid base64 screenshot data: {e}"),
                        "retry the screenshot",
                    )
                })
        })
        .await
        .map_err(join_err)?
    }

    async fn close(&self) -> Result<(), FabricError> {
        let inner = self.inner.clone();
        // Best-effort polite shutdown; ChildGuard kills on drop regardless.
        tokio::task::spawn_blocking(move || {
            let _ = inner.rpc("browser.close", json!({}));
            let _ = inner.rpc("session.end", json!({}));
            Ok::<(), FabricError>(())
        })
        .await
        .map_err(join_err)?
    }
}

fn join_err(e: tokio::task::JoinError) -> FabricError {
    FabricError::backend_failure(
        "browser backend",
        format!("browser worker task failed: {e}"),
        "browser.*",
        "retry; if it persists restart the runtime",
    )
}
