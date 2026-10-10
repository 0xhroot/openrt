//! `browser.*` tools — every call flows through the same
//! PolicyEvaluator → Sovereign mode → resource gate → audit path as any
//! other tool (this crate only supplies the implementation behind the gate).
//!
//! Network egress is checked here as well: [`ensure_url_allowed`] runs
//! *before* any backend interaction for URL-taking operations (Sovereign
//! keeps loopback/file/data URLs, refuses external egress — ADR-003).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};

use openrt_fabric::artifacts::ArtifactStore;
use openrt_fabric::net_guard::{ensure_url_allowed, SCOPE_BROWSER};
use openrt_fabric::{
    fail, ErrorCode, FabricError, FabricMeta, RiskLevel, SupportState, ToolCategory,
};
use openrt_modes::ModeManager;
use openrt_netpol::NetworkPolicy;
use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};

use crate::backend::unwrap_stringified;
use crate::manager::BrowserManager;

/// Hard cap for `browser.download` / `browser.upload` payloads (bytes):
/// the base64-over-JS pipeline stays inside script engine limits.
const MAX_TRANSFER_BYTES: usize = 8 * 1024 * 1024;

pub struct BrowserToolCtx {
    pub manager: Arc<BrowserManager>,
    pub artifacts: Arc<ArtifactStore>,
    pub modes: Arc<ModeManager>,
    pub netpol: Arc<NetworkPolicy>,
}

// ------------------------------------------------------------- JS helpers --

fn jstr(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn js_find(selector: &str) -> String {
    format!(
        r#"JSON.stringify((()=>{{const els=Array.from(document.querySelectorAll({sel}));return {{count:els.length,texts:els.slice(0,5).map(el=>(el.innerText||el.textContent||"").trim().slice(0,120))}};}})())"#,
        sel = jstr(selector)
    )
}

fn js_click(selector: &str) -> String {
    format!(
        r#"JSON.stringify((()=>{{const el=document.querySelector({sel});if(!el)throw new Error("element not found: "+{sel});el.click();return {{clicked:true}};}})())"#,
        sel = jstr(selector)
    )
}

fn js_type(selector: &str, text: &str, submit: bool) -> String {
    format!(
        r#"JSON.stringify((()=>{{const el=document.querySelector({sel});if(!el)throw new Error("element not found: "+{sel});el.focus();if("value" in el){{el.value={txt};el.dispatchEvent(new Event("input",{{bubbles:true}}));el.dispatchEvent(new Event("change",{{bubbles:true}}));}}else{{el.textContent={txt};}}let submitted=false;if({sub}){{const f=el.form;if(f){{try{{f.requestSubmit();submitted=true;}}catch(_e){{}}}}}}return {{typed:true,submitted:submitted}};}})())"#,
        sel = jstr(selector),
        txt = jstr(text),
        sub = if submit { "true" } else { "false" }
    )
}

fn js_select(selector: &str, value: &str) -> String {
    format!(
        r#"JSON.stringify((()=>{{const el=document.querySelector({sel});if(!el)throw new Error("element not found: "+{sel});el.value={val};el.dispatchEvent(new Event("change",{{bubbles:true}}));return {{selected:el.value==={val},value:el.value}};}})())"#,
        sel = jstr(selector),
        val = jstr(value)
    )
}

fn js_submit(selector: &str) -> String {
    format!(
        r#"JSON.stringify((()=>{{const el=document.querySelector({sel});if(!el)throw new Error("element not found: "+{sel});const f=el.form||(el.tagName==="FORM"?el:null);if(!f)throw new Error("element is not in a form: "+{sel});try{{f.requestSubmit();}}catch(_e){{f.submit();}}return {{submitted:true}};}})())"#,
        sel = jstr(selector)
    )
}

/// Fetch in-page and return base64. `redirect:"error"` keeps egress policy
/// honest: a redirect to another origin is refused instead of followed
/// unchecked (documented in PHASE7B_IMPLEMENTATION).
fn js_download(url: &str) -> String {
    format!(
        r#"JSON.stringify(fetch({url},{{redirect:"error"}}).then(r=>r.ok?r.arrayBuffer().then(b=>{{if(b.byteLength>{max})return {{ok:false,error:"payload exceeds "+{max}+" bytes",bytes:b.byteLength}};const u=new Uint8Array(b);let s="";for(let i=0;i<u.length;i+=8192){{s+=String.fromCharCode.apply(null,u.subarray(i,i+8192));}}return {{ok:true,b64:window.btoa(s),bytes:b.byteLength}};}}):{{ok:false,status:r.status}}).catch(e=>({{ok:false,error:String(e)}})))"#,
        url = jstr(url),
        max = MAX_TRANSFER_BYTES
    )
}

fn js_upload(selector: &str, filename: &str, b64: &str, bytes: usize) -> String {
    format!(
        r#"JSON.stringify((()=>{{const el=document.querySelector({sel});if(!el)throw new Error("element not found: "+{sel});const raw=window.atob({b64});const arr=new Uint8Array(raw.length);for(let i=0;i<raw.length;i++){{arr[i]=raw.charCodeAt(i);}}const f=new File([arr],{name},{{type:"application/octet-stream"}});const dt=new DataTransfer();dt.items.add(f);el.files=dt.files;el.dispatchEvent(new Event("change",{{bubbles:true}}));return {{uploaded:true,size:{bytes}}};}})())"#,
        sel = jstr(selector),
        b64 = jstr(b64),
        name = jstr(filename),
        bytes = bytes
    )
}

// ------------------------------------------------------------------ tool ---

#[derive(Clone, Copy)]
pub enum BrowserOp {
    Navigate,
    Find,
    Click,
    Type,
    Select,
    Submit,
    Screenshot,
    Download,
    Upload,
    Status,
    Close,
}

pub struct BrowserTool {
    pub spec: ToolSpec,
    pub ctx: Arc<BrowserToolCtx>,
    pub op: BrowserOp,
}

/// Re-map JS "element not found" exceptions to the honest NOT_FOUND code.
fn map_js_error(e: FabricError, what: &str) -> FabricError {
    if e.code == ErrorCode::BackendFailure && e.why.contains("element not found") {
        FabricError::not_found(
            what,
            e.why.clone(),
            "call browser.find first to confirm the selector matches an element",
        )
    } else if e.code == ErrorCode::BackendFailure && e.why.contains("is not in a form") {
        FabricError::not_found(
            what,
            e.why.clone(),
            "target a form element or an input inside a form",
        )
    } else {
        e
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, FabricError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            FabricError::invalid_input(
                "browser tool",
                format!("missing `{key}` string"),
                &format!("pass `\"{key}\": \"...\"`"),
            )
        })
}

async fn guard(ctx: &BrowserToolCtx, url: &str) -> Result<(), FabricError> {
    ensure_url_allowed(url, SCOPE_BROWSER, &ctx.modes, &ctx.netpol).await
}

fn write_artifact_file(path: &Path, bytes: &[u8]) -> Result<(), FabricError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                FabricError::backend_failure(
                    "browser download",
                    format!("cannot create `{}`: {e}", parent.display()),
                    "browser.download",
                    "use a writable directory",
                )
            })?;
        }
    }
    std::fs::write(path, bytes).map_err(|e| {
        FabricError::backend_failure(
            "browser download",
            format!("cannot write `{}`: {e}", path.display()),
            "browser.download",
            "check permissions and disk space",
        )
    })
}

#[async_trait]
impl Tool for BrowserTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let name = self.spec.name.clone();
        let res = self.run_inner(&args).await;
        match res {
            Ok(v) => Ok(ToolOutput::Json(v)),
            Err(e) => fail(&name, e),
        }
    }
}

impl BrowserTool {
    async fn run_inner(&self, args: &Value) -> Result<Value, FabricError> {
        let source_task = args
            .get("source_task")
            .and_then(Value::as_str)
            .map(str::to_string);
        match self.op {
            BrowserOp::Status => {
                let mut st = self.ctx.manager.status().await;
                st["support"] = json!(format!("{:?}", self.ctx.manager.support()).to_lowercase());
                st["ok"] = json!(true);
                Ok(st)
            }
            BrowserOp::Close => {
                let out = self.ctx.manager.close().await?;
                Ok(out)
            }
            BrowserOp::Navigate => {
                let url = arg(args, "url")?;
                guard(&self.ctx, url).await?;
                let backend = self.ctx.manager.backend().await?;
                let result = backend.navigate(url).await?;
                Ok(json!({
                    "ok": true,
                    "url": result.get("url").cloned().unwrap_or(Value::String(url.into())),
                    "title": result.get("title").cloned().unwrap_or(Value::Null),
                    "backend": backend.name(),
                }))
            }
            BrowserOp::Find => {
                let selector = arg(args, "selector")?;
                let backend = self.ctx.manager.backend().await?;
                let raw = backend
                    .evaluate(&js_find(selector))
                    .await
                    .map_err(|e| map_js_error(e, "browser.find"))?;
                let v = unwrap_stringified(&raw);
                Ok(json!({
                    "ok": true,
                    "selector": selector,
                    "count": v.get("count").cloned().unwrap_or(json!(0)),
                    "texts": v.get("texts").cloned().unwrap_or(json!([])),
                    "backend": backend.name(),
                }))
            }
            BrowserOp::Click => {
                let selector = arg(args, "selector")?;
                let backend = self.ctx.manager.backend().await?;
                let raw = backend
                    .evaluate(&js_click(selector))
                    .await
                    .map_err(|e| map_js_error(e, "browser.click"))?;
                let v = unwrap_stringified(&raw);
                Ok(json!({
                    "ok": v.get("clicked").and_then(Value::as_bool).unwrap_or(false),
                    "selector": selector,
                    "backend": backend.name(),
                }))
            }
            BrowserOp::Type => {
                let selector = arg(args, "selector")?;
                let text = arg(args, "text")?;
                let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
                let backend = self.ctx.manager.backend().await?;
                let raw = backend
                    .evaluate(&js_type(selector, text, submit))
                    .await
                    .map_err(|e| map_js_error(e, "browser.type"))?;
                let v = unwrap_stringified(&raw);
                Ok(json!({
                    "ok": v.get("typed").and_then(Value::as_bool).unwrap_or(false),
                    "selector": selector,
                    "submitted": v.get("submitted").cloned().unwrap_or(json!(false)),
                    "backend": backend.name(),
                }))
            }
            BrowserOp::Select => {
                let selector = arg(args, "selector")?;
                let value = arg(args, "value")?;
                let backend = self.ctx.manager.backend().await?;
                let raw = backend
                    .evaluate(&js_select(selector, value))
                    .await
                    .map_err(|e| map_js_error(e, "browser.select"))?;
                let v = unwrap_stringified(&raw);
                if v.get("selected").and_then(Value::as_bool) != Some(true) {
                    return Err(FabricError::not_found(
                        "select option",
                        format!("no option with value `{value}` in `{selector}`"),
                        "call browser.find on the <select> to list available options",
                    ));
                }
                Ok(json!({
                    "ok": true,
                    "selector": selector,
                    "value": v.get("value").cloned().unwrap_or(Value::String(value.into())),
                    "backend": backend.name(),
                }))
            }
            BrowserOp::Submit => {
                let selector = arg(args, "selector")?;
                let backend = self.ctx.manager.backend().await?;
                let raw = backend
                    .evaluate(&js_submit(selector))
                    .await
                    .map_err(|e| map_js_error(e, "browser.submit"))?;
                let v = unwrap_stringified(&raw);
                Ok(json!({
                    "ok": v.get("submitted").and_then(Value::as_bool).unwrap_or(false),
                    "selector": selector,
                    "backend": backend.name(),
                }))
            }
            BrowserOp::Screenshot => {
                let backend = self.ctx.manager.backend().await?;
                let bytes = backend.screenshot().await?;
                if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
                    return Err(FabricError::verification_failed(
                        "browser screenshot",
                        "backend returned bytes that are not a PNG",
                        "retry; if it persists the backend screenshot path is broken",
                    ));
                }
                let path: PathBuf = match args.get("path").and_then(Value::as_str) {
                    Some(p) => PathBuf::from(p),
                    None => std::env::temp_dir()
                        .join("openrt-browser")
                        .join(format!("screenshot-{}.png", uuid::Uuid::new_v4())),
                };
                write_artifact_file(&path, &bytes)?;
                let artifact = self
                    .ctx
                    .artifacts
                    .record_file(
                        &path,
                        "png",
                        "browser.screenshot",
                        source_task,
                        json!({ "backend": backend.name(), "format": "png" }),
                    )
                    .await
                    .map(|r| r.to_json())
                    .map_err(|e| {
                        FabricError::new(
                            ErrorCode::IntegrityFailure,
                            "artifact provenance record",
                            e,
                            "fabric.artifacts",
                            "the screenshot was written but not recorded; fix the store and re-run",
                        )
                    })?;
                Ok(json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "bytes": bytes.len(),
                    "backend": backend.name(),
                    "verification": { "ok": true, "method": "PNG signature + artifact sha256" },
                    "artifact": artifact,
                }))
            }
            BrowserOp::Download => {
                let url = arg(args, "url")?;
                let path = PathBuf::from(arg(args, "path")?);
                guard(&self.ctx, url).await?;
                let backend = self.ctx.manager.backend().await?;
                let raw = backend.evaluate(&js_download(url)).await?;
                let v = unwrap_stringified(&raw);
                if v.get("ok").and_then(Value::as_bool) != Some(true) {
                    let why = match (
                        v.get("status").and_then(Value::as_u64),
                        v.get("error").and_then(Value::as_str),
                    ) {
                        (Some(code), _) => format!("fetch of `{url}` failed with HTTP {code}"),
                        (None, Some(err)) => format!("fetch of `{url}` failed: {err}"),
                        _ => format!("fetch of `{url}` failed"),
                    };
                    return Err(FabricError::backend_failure(
                        "browser download",
                        why,
                        "browser.download",
                        "check the URL is reachable from the page (redirects are refused by design)",
                    ));
                }
                let b64 = v.get("b64").and_then(Value::as_str).unwrap_or("");
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| {
                        FabricError::verification_failed(
                            "browser download",
                            format!("invalid base64 payload from page fetch: {e}"),
                            "retry the download",
                        )
                    })?;
                if bytes.len() > MAX_TRANSFER_BYTES {
                    return Err(FabricError::invalid_input(
                        "browser download",
                        format!(
                            "payload {} bytes exceeds cap {MAX_TRANSFER_BYTES}",
                            bytes.len()
                        ),
                        "download a smaller file, or fetch it outside the fabric",
                    ));
                }
                write_artifact_file(&path, &bytes)?;
                let artifact = self
                    .ctx
                    .artifacts
                    .record_file(
                        &path,
                        "download",
                        "browser.download",
                        source_task,
                        json!({ "url": url, "backend": backend.name(), "bytes": bytes.len() }),
                    )
                    .await
                    .map(|r| r.to_json())
                    .map_err(|e| {
                        FabricError::new(
                            ErrorCode::IntegrityFailure,
                            "artifact provenance record",
                            e,
                            "fabric.artifacts",
                            "the file was written but not recorded; fix the store and re-run",
                        )
                    })?;
                Ok(json!({
                    "ok": true,
                    "url": url,
                    "path": path.display().to_string(),
                    "bytes": bytes.len(),
                    "backend": backend.name(),
                    "verification": { "ok": true, "method": "sha256 over written bytes (artifact record)" },
                    "artifact": artifact,
                }))
            }
            BrowserOp::Upload => {
                let selector = arg(args, "selector")?;
                let file = PathBuf::from(arg(args, "file")?);
                let bytes = std::fs::read(&file).map_err(|e| {
                    FabricError::not_found(
                        "upload file",
                        format!("cannot read `{}`: {e}", file.display()),
                        "pass an existing readable file path",
                    )
                })?;
                if bytes.len() > MAX_TRANSFER_BYTES {
                    return Err(FabricError::invalid_input(
                        "browser upload",
                        format!(
                            "file {} bytes exceeds cap {MAX_TRANSFER_BYTES}",
                            bytes.len()
                        ),
                        "upload a smaller file",
                    ));
                }
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let filename = file
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "upload.bin".into());
                let backend = self.ctx.manager.backend().await?;
                let raw = backend
                    .evaluate(&js_upload(selector, &filename, &b64, bytes.len()))
                    .await
                    .map_err(|e| map_js_error(e, "browser.upload"))?;
                let v = unwrap_stringified(&raw);
                Ok(json!({
                    "ok": v.get("uploaded").and_then(Value::as_bool).unwrap_or(false),
                    "selector": selector,
                    "file": file.display().to_string(),
                    "bytes": bytes.len(),
                    "backend": backend.name(),
                }))
            }
        }
    }
}

// -------------------------------------------------------------- factory ---

/// All `browser.*` tools over one shared context.
pub fn browser_tools(ctx: Arc<BrowserToolCtx>) -> Vec<Arc<dyn Tool>> {
    let mk = |name: &str,
              desc: &str,
              caps: &[&str],
              schema: Value,
              op: BrowserOp,
              ctx: &Arc<BrowserToolCtx>| {
        Arc::new(BrowserTool {
            spec: ToolSpec::new(name, desc)
                .with_caps(caps)
                .with_schema(schema),
            ctx: ctx.clone(),
            op,
        }) as Arc<dyn Tool>
    };
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();

    tools.push(mk(
        "browser.navigate",
        "Navigate the managed browser session to a URL (http/https/loopback/file/data URLs; \
         network policy + Sovereign mode checked first). Use for every \"navigate/open <URL>\" \
         request, e.g. \"Navigate Firefox to http://127.0.0.1:1\" - it starts the browser \
         session itself. Launching apps is computer.app.",
        &["browser.navigate.all"],
        json!({
            "type": "object",
            "required": ["url"],
            "properties": { "url": { "type": "string" } }
        }),
        BrowserOp::Navigate,
        &ctx,
    ));
    tools.push(mk(
        "browser.find",
        "Query the current page for a CSS selector; match count + up to 5 text snippets \
         (read-only; navigate first). Explore with broad selectors first (`button`, `a`, \
         `input`, `h1`) before narrowing.",
        &["browser.find.all"],
        json!({
            "type": "object",
            "required": ["selector"],
            "properties": { "selector": { "type": "string" } }
        }),
        BrowserOp::Find,
        &ctx,
    ));
    tools.push(mk(
        "browser.click",
        "Click the first element matching a CSS selector on the current page (structured \
         NOT_FOUND when missing; navigate first).",
        &["browser.click.all"],
        json!({
            "type": "object",
            "required": ["selector"],
            "properties": { "selector": { "type": "string" } }
        }),
        BrowserOp::Click,
        &ctx,
    ));
    tools.push(mk(
        "browser.type",
        "Type into the focused input/textarea/contenteditable on the current page (fires \
         input+change events; optional submit; navigate first).",
        &["browser.type.all"],
        json!({
            "type": "object",
            "required": ["selector", "text"],
            "properties": {
                "selector": { "type": "string" },
                "text": { "type": "string" },
                "submit": { "type": "boolean", "default": false }
            }
        }),
        BrowserOp::Type,
        &ctx,
    ));
    tools.push(mk(
        "browser.select",
        "Choose an <select> option by value (fires change; NOT_FOUND when missing; navigate \
         first).",
        &["browser.select.all"],
        json!({
            "type": "object",
            "required": ["selector", "value"],
            "properties": {
                "selector": { "type": "string" },
                "value": { "type": "string" }
            }
        }),
        BrowserOp::Select,
        &ctx,
    ));
    tools.push(mk(
        "browser.submit",
        "Submit the form containing (or equal to) the selected element (navigate first).",
        &["browser.submit.all"],
        json!({
            "type": "object",
            "required": ["selector"],
            "properties": { "selector": { "type": "string" } }
        }),
        BrowserOp::Submit,
        &ctx,
    ));
    tools.push(mk(
        "browser.screenshot",
        "Capture the managed browser viewport as a PNG artifact (verified signature + sha256; \
         navigate first) - the whole screen is computer.screen.",
        &["browser.screenshot.all"],
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } }
        }),
        BrowserOp::Screenshot,
        &ctx,
    ));
    tools.push(mk(
        "browser.download",
        "Fetch a URL in-page and write the bytes to `path` as a recorded artifact (egress policy checked; redirects refused).",
        &["browser.download.all"],
        json!({
            "type": "object",
            "required": ["url", "path"],
            "properties": {
                "url": { "type": "string" },
                "path": { "type": "string" },
                "source_task": { "type": "string" }
            }
        }),
        BrowserOp::Download,
        &ctx,
    ));
    tools.push(mk(
        "browser.upload",
        "Attach a local file to an <input type=file> via DataTransfer and fire change (payload cap applies).",
        &["browser.upload.all"],
        json!({
            "type": "object",
            "required": ["selector", "file"],
            "properties": {
                "selector": { "type": "string" },
                "file": { "type": "string" }
            }
        }),
        BrowserOp::Upload,
        &ctx,
    ));
    tools.push(mk(
        "browser.status",
        "Report browser session state (active backend, configured choice, platform support) without starting anything.",
        &["browser.status.all"],
        json!({ "type": "object", "properties": {} }),
        BrowserOp::Status,
        &ctx,
    ));
    tools.push(mk(
        "browser.close",
        "End the browser session and kill any spawned browser process (idempotent).",
        &["browser.close.all"],
        json!({ "type": "object", "properties": {} }),
        BrowserOp::Close,
        &ctx,
    ));

    tools
}

/// Catalog metadata for every browser tool. `support` comes from the
/// manager's honest probe (Green = real Firefox available, Partial = mock
/// only, Unavailable = backend cannot start).
pub fn browser_metas(host: &str, support: SupportState) -> Vec<FabricMeta> {
    let mk_meta = |tool: &str, risk: RiskLevel, observe: bool| {
        let m = FabricMeta::new(tool, ToolCategory::Browser, risk).with_platform(host, support);
        if observe {
            m.observe()
                .side_effects(&["none (read-only page inspection)"])
        } else {
            m.verify()
                .side_effects(&["acts on the live page/session"])
                .rollback("browser.close + reload")
        }
    };
    vec![
        mk_meta("browser.navigate", RiskLevel::Medium, false),
        mk_meta("browser.find", RiskLevel::Low, true),
        mk_meta("browser.click", RiskLevel::Medium, false),
        mk_meta("browser.type", RiskLevel::Medium, false),
        mk_meta("browser.select", RiskLevel::Medium, false),
        mk_meta("browser.submit", RiskLevel::Medium, false),
        mk_meta("browser.screenshot", RiskLevel::Low, false),
        FabricMeta::new("browser.download", ToolCategory::Browser, RiskLevel::High)
            .with_platform(host, support)
            .verify()
            .side_effects(&["writes a file from a remote fetch (egress-gated)"])
            .rollback("delete the downloaded file"),
        FabricMeta::new("browser.upload", ToolCategory::Browser, RiskLevel::High)
            .with_platform(host, support)
            .verify()
            .side_effects(&["sends local file bytes into the page (file input)"])
            .rollback("reset the form / reload the page"),
        FabricMeta::new("browser.status", ToolCategory::Browser, RiskLevel::Low)
            .with_platform(host, support)
            .observe()
            .side_effects(&["none (read-only session status)"]),
        FabricMeta::new("browser.close", ToolCategory::Browser, RiskLevel::Medium)
            .with_platform(host, support)
            .verify()
            .side_effects(&["ends the browser session, kills the process"])
            .rollback("start a new session (browser.navigate)"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_modes::ModeManager;
    use openrt_netpol::NetworkPolicy;
    use openrt_persistence::MemoryStore;

    fn ctx(backend: &str) -> Arc<BrowserToolCtx> {
        let store = Arc::new(MemoryStore::new());
        Arc::new(BrowserToolCtx {
            manager: Arc::new(BrowserManager::new(&openrt_config::BrowserConfig {
                backend: Some(backend.into()),
                binary: None,
            })),
            artifacts: Arc::new(ArtifactStore::new(store.clone())),
            modes: Arc::new(ModeManager::new(store.clone())),
            netpol: Arc::new(NetworkPolicy::new(store)),
        })
    }

    fn tool(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
        tools
            .iter()
            .find(|t| t.spec().name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .clone()
    }

    async fn json_out(t: &Arc<dyn Tool>, args: Value) -> Value {
        match t.run(args).await.expect("tool must succeed") {
            ToolOutput::Json(v) => v,
            ToolOutput::Text(s) => json!({ "text": s }),
        }
    }

    /// `browser.navigate` must route application-launch intent to
    /// `computer.app` (it only handles URLs/websites), and the page-interaction
    /// tools must state the navigate-first precondition.
    #[test]
    fn browser_descriptions_route_intent_and_preconditions() {
        let tools = browser_tools(ctx("mock"));
        let nav = tool(&tools, "browser.navigate");
        let d = &nav.spec().description;
        assert!(d.starts_with("Navigate the managed browser session"), "{d}");
        assert!(d.contains("computer.app"), "{d}");
        assert!(d.contains("Navigate Firefox to http://127.0.0.1:1"), "{d}");
        for name in [
            "browser.click",
            "browser.type",
            "browser.find",
            "browser.submit",
        ] {
            let t = tool(&tools, name);
            assert!(
                t.spec().description.contains("navigate first"),
                "{name}: {}",
                t.spec().description
            );
        }
        let shot = tool(&tools, "browser.screenshot");
        assert!(
            shot.spec().description.contains("computer.screen"),
            "browser-only viewport vs whole screen must be disambiguated"
        );
    }

    #[tokio::test]
    async fn every_tool_has_the_expected_cap() {
        let tools = browser_tools(ctx("mock"));
        let expected: Vec<(&str, &str)> = vec![
            ("browser.navigate", "browser.navigate.all"),
            ("browser.find", "browser.find.all"),
            ("browser.click", "browser.click.all"),
            ("browser.type", "browser.type.all"),
            ("browser.select", "browser.select.all"),
            ("browser.submit", "browser.submit.all"),
            ("browser.screenshot", "browser.screenshot.all"),
            ("browser.download", "browser.download.all"),
            ("browser.upload", "browser.upload.all"),
            ("browser.status", "browser.status.all"),
            ("browser.close", "browser.close.all"),
        ];
        assert_eq!(tools.len(), expected.len());
        for (name, cap) in expected {
            let t = tool(&tools, name);
            // with_caps silently drops unparsable caps — matching == valid.
            let caps: Vec<&str> = t.spec().required_caps.iter().map(|c| c.as_str()).collect();
            assert_eq!(caps, vec![cap], "{name}");
        }
    }

    #[tokio::test]
    async fn mock_session_flow_status_screenshot_close() {
        let tools = browser_tools(ctx("mock"));
        let status = json_out(&tool(&tools, "browser.status"), json!({})).await;
        assert_eq!(status["active"], json!(false));
        assert_eq!(status["support"], json!("partial"));

        let nav = json_out(
            &tool(&tools, "browser.navigate"),
            json!({ "url": "about:blank" }),
        )
        .await;
        assert_eq!(nav["ok"], json!(true));
        assert_eq!(nav["backend"], json!("mock"));

        let shot = json_out(&tool(&tools, "browser.screenshot"), json!({})).await;
        assert_eq!(shot["ok"], json!(true));
        let p = PathBuf::from(shot["path"].as_str().unwrap());
        assert!(p.is_file());
        std::fs::remove_file(&p).ok();

        let closed = json_out(&tool(&tools, "browser.close"), json!({})).await;
        assert_eq!(closed["closed"], json!(true));
        // Backend refuses work after close (session restarts on next use).
        let status = json_out(&tool(&tools, "browser.status"), json!({})).await;
        assert_eq!(status["active"], json!(false));
    }

    #[tokio::test]
    async fn sovereign_mode_refuses_external_navigate_but_allows_local() {
        let tools = browser_tools(ctx("mock"));
        // Default mode is Sovereign (fail-closed): policy denials are
        // returned as structured ToolOutput (fail() keeps INVALID_INPUT as
        // Err, everything else as {ok:false, error:{code}}).
        let out = match tool(&tools, "browser.navigate")
            .run(json!({ "url": "https://example.com/" }))
            .await
        {
            Ok(ToolOutput::Json(v)) => v,
            other => panic!("expected structured POLICY_DENIED output, got {other:?}"),
        };
        assert_eq!(out["ok"], json!(false));
        assert_eq!(out["error"]["code"], json!("POLICY_DENIED"));

        let out = json_out(
            &tool(&tools, "browser.navigate"),
            json!({ "url": "file:///tmp/x.html" }),
        )
        .await;
        assert_eq!(out["ok"], json!(true));
    }

    #[tokio::test]
    async fn off_backend_is_structured_unavailable() {
        let tools = browser_tools(ctx("off"));
        let out = match tool(&tools, "browser.navigate")
            .run(json!({ "url": "about:blank" }))
            .await
        {
            Ok(ToolOutput::Json(v)) => v,
            other => panic!("expected structured output, got {other:?}"),
        };
        assert_eq!(out["ok"], json!(false));
        assert_eq!(out["error"]["code"], json!("UNAVAILABLE"));
    }

    #[tokio::test]
    async fn download_and_upload_roundtrip_on_mock() {
        let tools = browser_tools(ctx("mock"));
        let dir = std::env::temp_dir().join(format!("openrt-br-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Mock's evaluate returns {"mock": true} so download fails honestly
        // (no fake bytes) — assert the structured failure path.
        let out = match tool(&tools, "browser.download")
            .run(json!({ "url": "http://127.0.0.1:1/f", "path": dir.join("x.bin").display().to_string() }))
            .await
        {
            Ok(ToolOutput::Json(v)) => v,
            other => panic!("expected structured output, got {other:?}"),
        };
        assert_eq!(out["ok"], json!(false));

        // Upload reads the local file (real FS) and evaluates on the mock.
        let f = dir.join("up.txt");
        std::fs::write(&f, b"hello upload").unwrap();
        let up = json_out(
            &tool(&tools, "browser.upload"),
            json!({ "selector": "input[type=file]", "file": f.display().to_string() }),
        )
        .await;
        assert_eq!(up["ok"], json!(true));
        assert_eq!(up["bytes"], json!(12));
    }
}
