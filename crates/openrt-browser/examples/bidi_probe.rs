//! Dev probe (not shipped): launch Firefox BiDi and print raw initialize
//! responses for several protocol versions to discover the accepted shape.
//!
//! Usage: cargo run -p openrt-browser --example bidi_probe

use std::io::BufRead;
use std::process::{Command, Stdio};
use std::time::Duration;

use openrt_browser::ws::WsClient;

fn main() {
    let profile = std::env::temp_dir().join(format!("bidi-probe-{}", std::process::id()));
    std::fs::create_dir_all(&profile).unwrap();
    let mut child = Command::new("firefox")
        .arg("--headless")
        .arg("--no-remote")
        .arg("--profile")
        .arg(&profile)
        .arg("--remote-debugging-port=0")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn firefox");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            if let Some(pos) = line.find("WebDriver BiDi listening on ws://") {
                let _ = tx.send(
                    line[pos + "WebDriver BiDi listening on ".len()..]
                        .trim()
                        .to_string(),
                );
                return;
            }
        }
    });
    let ws_url = rx.recv_timeout(Duration::from_secs(20)).expect("bidi url");
    let mut u = url::Url::parse(&ws_url).unwrap();
    if u.path().is_empty() || u.path() == "/" {
        u.set_path("/session");
    }
    eprintln!("connecting to {u}");
    let mut client = WsClient::connect(&u.to_string(), Duration::from_secs(10)).expect("connect");

    let variants: Vec<serde_json::Value> = vec![
        serde_json::json!({"protocolVersion": "1.0", "capabilities": {}}),
        serde_json::json!({"protocolVersion": "2.0", "capabilities": {}}),
        serde_json::json!({"protocolVersion": "3.0", "capabilities": {}}),
        serde_json::json!({"capabilities": {}}),
        serde_json::json!({}),
        serde_json::json!({"protocolVersion": "1.0"}),
    ];
    for (i, params) in variants.iter().enumerate() {
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": i + 1, "method": "initialize", "params": params});
        client.send_text(&msg.to_string()).expect("send");
        // print up to 3 frames until we see our id (skip events)
        for _ in 0..5 {
            match client.recv_text() {
                Ok(t) => {
                    eprintln!("params {params} -> {t}");
                    if serde_json::from_str::<serde_json::Value>(&t)
                        .ok()
                        .and_then(|v| v.get("id").and_then(serde_json::Value::as_u64))
                        == Some((i + 1) as u64)
                    {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("recv error: {e}");
                    break;
                }
            }
        }
    }

    // Also try session.new / browsingContext.create without initialize.
    for (id, method, params) in [
        (
            100u64,
            "session.new",
            serde_json::json!({"capabilities": {}}),
        ),
        (
            101,
            "browsingContext.create",
            serde_json::json!({"type": "tab"}),
        ),
    ] {
        let msg =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        client.send_text(&msg.to_string()).expect("send");
        for _ in 0..5 {
            match client.recv_text() {
                Ok(t) => {
                    eprintln!("{method} -> {t}");
                    if serde_json::from_str::<serde_json::Value>(&t)
                        .ok()
                        .and_then(|v| v.get("id").and_then(serde_json::Value::as_u64))
                        == Some(id)
                    {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("recv error: {e}");
                    break;
                }
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&profile);
}
