use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;

use crate::palette::{CommandPalette, DispatchRequest, PaletteOutcome};
use crate::WebUiError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUiConfig {
    pub bind: IpAddr,
    pub port: u16,
}

impl WebUiConfig {
    pub fn localhost(port: u16) -> Self {
        Self {
            bind: "127.0.0.1".parse().expect("loopback literal"),
            port,
        }
    }

    pub fn new(bind: IpAddr, port: u16) -> Result<Self, WebUiError> {
        if !bind.is_loopback() {
            return Err(WebUiError::NonLocalBind {
                addr: bind.to_string(),
            });
        }
        Ok(Self { bind, port })
    }
}

#[derive(Debug)]
pub struct ServerHandle {
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

pub fn serve(config: WebUiConfig, mut palette: CommandPalette) -> Result<ServerHandle, WebUiError> {
    if !config.bind.is_loopback() {
        return Err(WebUiError::NonLocalBind {
            addr: config.bind.to_string(),
        });
    }
    let listener = TcpListener::bind((config.bind, config.port))
        .map_err(|e| WebUiError::BindFailed(e.to_string()))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| WebUiError::BindFailed(e.to_string()))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| WebUiError::BindFailed(e.to_string()))?;
    // Surface the REAL bound address (not a hardcoded placeholder) so
    // `session.info` / `/api/health` report the actual socket.
    palette.set_bind(local_addr.to_string());
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let join = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = handle_connection(stream, &mut palette);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    Ok(ServerHandle {
        local_addr,
        stop,
        join: Some(join),
    })
}

fn handle_connection(
    mut stream: TcpStream,
    palette: &mut CommandPalette,
) -> Result<(), WebUiError> {
    let request = read_request(&mut stream)?;
    let response = route(&request, palette);
    stream
        .write_all(response.as_bytes())
        .map_err(|e| WebUiError::Io(e.to_string()))?;
    let _ = stream.flush();
    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn bearer_token(&self) -> Option<String> {
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("authorization") {
                let v = value.trim();
                if let Some(tok) = v
                    .strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
                {
                    let tok = tok.trim();
                    if !tok.is_empty() {
                        return Some(tok.to_string());
                    }
                }
            }
        }
        None
    }
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, WebUiError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream
            .read(&mut chunk)
            .map_err(|e| WebUiError::Io(e.to_string()))?;
        if n == 0 {
            return Err(WebUiError::BadRequest("incomplete request".into()));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 64 * 1024 {
            return Err(WebUiError::BadRequest("request too large".into()));
        }
    };
    let header_bytes = buf[..header_end].to_vec();
    let rest = buf[header_end + 4..].to_vec();
    let headers_str = String::from_utf8_lossy(&header_bytes);
    let mut lines = headers_str.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| WebUiError::BadRequest("missing request line".into()))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| WebUiError::BadRequest("missing method".into()))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| WebUiError::BadRequest("missing path".into()))?
        .to_string();
    let mut content_length = 0usize;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value
                    .parse()
                    .map_err(|_| WebUiError::BadRequest("bad content-length".into()))?;
            }
            headers.push((name, value));
        }
    }
    let mut body = rest;
    while body.len() < content_length {
        let n = stream
            .read(&mut chunk)
            .map_err(|e| WebUiError::Io(e.to_string()))?;
        if n == 0 {
            return Err(WebUiError::BadRequest("incomplete body".into()));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn http_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn route(req: &HttpRequest, palette: &mut CommandPalette) -> String {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => http_response("200 OK", "text/html; charset=utf-8", INDEX_HTML),
        ("GET", "/api/health") => {
            let body = json!({
                "status": "ok",
                "bind": palette.bind(),
                "loopback": true,
            })
            .to_string();
            http_response("200 OK", "application/json", &body)
        }
        ("GET", "/api/commands") => {
            let cmds: Vec<serde_json::Value> = palette
                .commands()
                .into_iter()
                .map(|c| json!({"id": c.id, "title": c.title, "description": c.description}))
                .collect();
            let body = serde_json::Value::Array(cmds).to_string();
            http_response("200 OK", "application/json", &body)
        }
        ("GET", "/api/sih") => match palette.sih_dashboard() {
            Ok(body) => http_response("200 OK", "application/json", &body),
            Err(e) => {
                let body = json!({"error": e.to_string()}).to_string();
                http_response("500 Internal Server Error", "application/json", &body)
            }
        },
        ("GET", "/sih") => http_response("200 OK", "text/html; charset=utf-8", SIH_HTML),
        ("POST", "/api/execute") => {
            let parsed: serde_json::Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(e) => {
                    let body = json!({"error": format!("invalid json: {e}")}).to_string();
                    return http_response("400 Bad Request", "application/json", &body);
                }
            };
            let command_id = match parsed.get("command_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => {
                    let body = json!({"error": "missing command_id"}).to_string();
                    return http_response("400 Bad Request", "application/json", &body);
                }
            };
            // Operator identity comes ONLY from the Authorization bearer
            // token (server-issued). A body `operator` field is never used.
            let auth_token = req.bearer_token();
            let mut args = std::collections::BTreeMap::new();
            if let Some(obj) = parsed.get("args").and_then(|v| v.as_object()) {
                for (k, v) in obj {
                    if let Some(s) = v.as_str() {
                        args.insert(k.clone(), s.to_string());
                    }
                }
            }
            let dispatch = DispatchRequest {
                command_id,
                args,
                auth_token,
            };
            match palette.dispatch(&dispatch) {
                Ok(PaletteOutcome::Completed { id, detail }) => {
                    let body = json!({"ok": true, "command_id": id, "detail": detail}).to_string();
                    http_response("200 OK", "application/json", &body)
                }
                Ok(PaletteOutcome::Denied { id, reason }) => {
                    let body = json!({"ok": false, "command_id": id, "denied": reason}).to_string();
                    http_response("403 Forbidden", "application/json", &body)
                }
                Err(WebUiError::CommandNotFound { id }) => {
                    let body = json!({"error": "command not found", "command_id": id}).to_string();
                    http_response("404 Not Found", "application/json", &body)
                }
                Err(WebUiError::UnauthorizedCommand { id }) => {
                    let body = json!({
                        "error": "operator token required",
                        "command_id": id,
                    })
                    .to_string();
                    http_response("403 Forbidden", "application/json", &body)
                }
                Err(WebUiError::BadRequest(msg)) => {
                    let body = json!({"error": msg}).to_string();
                    http_response("400 Bad Request", "application/json", &body)
                }
                Err(other) => {
                    let body = json!({"error": other.to_string()}).to_string();
                    http_response("500 Internal Server Error", "application/json", &body)
                }
            }
        }
        _ => {
            let body = json!({"error": "not found"}).to_string();
            http_response("404 Not Found", "application/json", &body)
        }
    }
}

const SIH_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OpenRT — CyberSaarthi SIH Case</title>
<style>
body{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:#0b0f14;color:#d7e0ea;margin:0;padding:2rem;max-width:60rem}
h1{font-size:1.2rem;border-bottom:1px solid #243447;padding-bottom:.5rem}
h2{font-size:1rem;color:#7fd1ff;margin-top:1.5rem}
section{background:#111823;border:1px solid #243447;border-radius:6px;padding:1rem;margin-top:1rem}
.k{color:#8b9bb0}.v{color:#e6edf3}.warn{color:#ffcc66}.ok{color:#7ee787}
pre{white-space:pre-wrap;word-break:break-all;margin:.4rem 0}
a{color:#7fd1ff}
</style>
</head>
<body>
<h1>OpenRT — CyberSaarthi SIH case <span class="warn">(SYNTHETIC DEMO DATA)</span></h1>
<p class="k"><a href="/">workbench</a> · source: <code>GET /api/sih</code> (read-only)</p>
<div id="out" class="k">loading…</div>
<script>
async function j(u){const r=await fetch(u);if(!r.ok)throw new Error(u+" → "+r.status);return r.json()}
function kv(o){return Object.entries(o).map(([k,v])=>'<span class="k">'+k+':</span> <span class="v">'+JSON.stringify(v)+'</span>').join("<br>")}
async function main(){
 try{
  const d=await j("/api/sih");
  let h="";
  h+="<section><h2>case</h2>"+kv(d.case)+"</section>";
  h+="<section><h2>evidence</h2>"+kv(d.evidence)+"</section>";
  h+="<section><h2>entities & graph</h2>"+kv({count:d.entities.count,relations:d.entities.relations})+"<pre>"+JSON.stringify(d.entities.graph.slice(0,12),null,1)+"</pre></section>";
  h+="<section><h2>agent</h2>"+kv(d.agent)+"</section>";
  h+="<section><h2>verification</h2>"+kv(d.verification)+"</section>";
  h+="<section><h2>artifacts</h2><pre>"+JSON.stringify(d.artifacts,null,1)+"</pre></section>";
  h+="<section><h2>security</h2>"+kv(d.security)+"</section>";
  h+="<section><h2>audit</h2>"+kv({chain_verified:d.audit.chain_verified})+"<pre>"+JSON.stringify(d.audit.recent.slice(0,12),null,1)+"</pre></section>";
  document.getElementById("out").innerHTML=h;
 }catch(e){document.getElementById("out").textContent="failed to load: "+e.message}
}
main();
</script>
</body>
</html>
"#;

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OpenRT Workbench</title>
<style>
body { font-family: ui-sans-serif, system-ui, sans-serif; margin: 2rem auto; max-width: 44rem; padding: 0 1rem; color: #111; background: #fafafa; }
h1 { font-size: 1.4rem; }
label { display: block; font-size: 0.85rem; margin-bottom: 0.25rem; color: #444; }
input { width: 100%; padding: 0.55rem 0.7rem; font-size: 1rem; border: 1px solid #ccc; border-radius: 6px; box-sizing: border-box; }
ul { list-style: none; padding: 0; margin-top: 1rem; }
li { border: 1px solid #ddd; border-radius: 6px; padding: 0.6rem 0.8rem; margin-bottom: 0.5rem; background: #fff; cursor: pointer; }
li:hover { border-color: #888; }
li b { display: block; }
li span { color: #555; font-size: 0.85rem; }
#out { margin-top: 1rem; padding: 0.75rem; background: #111; color: #9fe89f; border-radius: 6px; white-space: pre-wrap; min-height: 1.5rem; font-size: 0.9rem; }
.grid { display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem; }
</style>
</head>
<body>
<h1>OpenRT Workbench &mdash; local command palette</h1>
<p>Binds to loopback only. Approval commands require a server-issued bearer token (same operator grant gate as headless).</p>
<div class="grid">
  <div>
    <label for="tok">Operator bearer token (approval commands)</label>
    <input id="tok" placeholder="Authorization: Bearer …" autocomplete="off">
  </div>
  <div>
    <label for="q">Filter</label>
    <input id="q" placeholder="type to filter&hellip;" autocomplete="off" autofocus>
  </div>
</div>
<ul id="list"></ul>
<div id="out">ready.</div>
<script>
let commands = [];
const list = document.getElementById('list');
const out = document.getElementById('out');
const q = document.getElementById('q');
async function load() {
  const r = await fetch('/api/commands');
  commands = await r.json();
  render('');
}
function render(filter) {
  list.innerHTML = '';
  commands
    .filter(c => !filter || (c.id + ' ' + c.title + ' ' + c.description).toLowerCase().includes(filter))
    .forEach(c => {
      const li = document.createElement('li');
      const b = document.createElement('b');
      b.textContent = c.id + ' — ' + c.title;
      const s = document.createElement('span');
      s.textContent = c.description;
      li.appendChild(b); li.appendChild(s);
      li.onclick = () => run(c.id);
      list.appendChild(li);
    });
}
async function run(id) {
  const token = document.getElementById('tok').value || null;
  const args = {};
  if (id === 'improve.approve' || id === 'improve.reject') {
    args.proposal_id = window.prompt('proposal id?') || '';
    if (!args.proposal_id) return;
  }
  const headers = {'Content-Type': 'application/json'};
  if (token) headers['Authorization'] = 'Bearer ' + token;
  const r = await fetch('/api/execute', {
    method: 'POST',
    headers,
    body: JSON.stringify({command_id: id, args})
  });
  const data = await r.json();
  out.textContent = r.status + ' ' + JSON.stringify(data, null, 2);
}
q.addEventListener('input', () => render(q.value.toLowerCase()));
load().catch(e => { out.textContent = 'load failed: ' + e; });
</script>
</body>
</html>
"#;
