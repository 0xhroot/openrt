//! Minimal RFC 6455 WebSocket client (text frames only) over `std::net::TcpStream`.
//!
//! Scope: local Firefox BiDi connection (loopback). The workspace tokio build
//! has no `net` feature, so the blocking socket lives behind `spawn_blocking`
//! in the Firefox backend. Client-to-server frames are always masked (spec
//! requirement); the handshake response is verified with `Sec-WebSocket-Accept`
//! so we never talk to a non-WebSocket endpoint.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use base64::Engine;
use url::Url;

use crate::sha1::sha1;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Debug)]
pub enum WsError {
    /// Connection-level failure (IO, protocol, handshake).
    Io(String),
    /// Peer closed the connection.
    Closed(String),
    /// Read timed out waiting for a frame/message.
    Timeout(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Io(e) => write!(f, "websocket io: {e}"),
            WsError::Closed(e) => write!(f, "websocket closed: {e}"),
            WsError::Timeout(e) => write!(f, "websocket timeout: {e}"),
        }
    }
}

fn io_err(e: impl std::fmt::Display) -> WsError {
    WsError::Io(e.to_string())
}

/// Compute the expected `Sec-WebSocket-Accept` for a client key.
pub fn accept_for(key: &str) -> String {
    let mut data = key.as_bytes().to_vec();
    data.extend_from_slice(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sha1(&data))
}

fn random_key() -> String {
    // uuid v4 bytes come from the OS CSPRNG — enough entropy for the
    // handshake nonce (this is not a secret; it only proves liveness).
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let bytes: Vec<u8> = a
        .as_bytes()
        .iter()
        .chain(b.as_bytes())
        .take(16)
        .copied()
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 4-byte frame masking key (OS entropy via uuid v4; not a secret).
fn mask4() -> [u8; 4] {
    let u = uuid::Uuid::new_v4();
    let b = u.as_bytes();
    [b[0], b[1], b[2], b[3]]
}

/// A blocking WebSocket client for text messages.
pub struct WsClient {
    stream: TcpStream,
    partial: Vec<u8>,
}

impl WsClient {
    /// Connect (`ws://` only — BiDi is loopback HTTP) and complete the
    /// opening handshake, verifying `Sec-WebSocket-Accept`.
    pub fn connect(url_str: &str, timeout: Duration) -> Result<Self, WsError> {
        let url = Url::parse(url_str).map_err(|e| io_err(format!("bad ws url: {e}")))?;
        if url.scheme() != "ws" {
            return Err(io_err(format!(
                "only `ws://` is supported by this client (got `{}`)",
                url.scheme()
            )));
        }
        let host = url.host_str().ok_or_else(|| io_err("ws url has no host"))?;
        let port = url.port_or_known_default().unwrap_or(80);
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|e| io_err(format!("resolve {host}:{port}: {e}")))?
            .next()
            .ok_or_else(|| io_err(format!("no addresses for {host}:{port}")))?;
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .map_err(|e| WsError::Timeout(format!("connect to {addr} failed: {e}")))?;
        stream.set_read_timeout(Some(timeout)).map_err(io_err)?;
        stream.set_write_timeout(Some(timeout)).map_err(io_err)?;
        stream.set_nodelay(true).ok();

        let key = random_key();
        let path = if url.path().is_empty() {
            "/".to_string()
        } else {
            format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            )
        };
        let mut req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}:{port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        let mut stream = stream;
        stream.write_all(req.as_bytes()).map_err(io_err)?;
        stream.flush().map_err(io_err)?;

        // Read response headers byte-wise until CRLF CRLF (small).
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while head.len() < 16 * 1024 {
            stream.read_exact(&mut byte).map_err(map_read)?;
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head_str = String::from_utf8_lossy(&head);
        let status_line = head_str.lines().next().unwrap_or("");
        if !status_line.contains(" 101") {
            return Err(io_err(format!("handshake rejected: {status_line}")));
        }
        let expect = accept_for(&key);
        let mut got_accept = None;
        for line in head_str.lines().skip(1) {
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("sec-websocket-accept") {
                    got_accept = Some(v.trim().to_string());
                }
            }
        }
        match got_accept {
            Some(v) if v == expect => {}
            Some(v) => {
                return Err(io_err(format!(
                    "Sec-WebSocket-Accept mismatch (got `{v}`, want `{expect}`)"
                )))
            }
            None => return Err(io_err("missing Sec-WebSocket-Accept header")),
        }
        req.clear();
        Ok(Self {
            stream,
            partial: Vec::new(),
        })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), WsError> {
        self.stream.set_read_timeout(timeout).map_err(io_err)
    }

    /// Send one masked text frame.
    pub fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        let payload = text.as_bytes();
        let mask = mask4();
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x81); // FIN + text
        let b0 = 0x80; // masked
        match payload.len() {
            n if n < 126 => frame.push(b0 | n as u8),
            n if n < 65_536 => {
                frame.push(b0 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(b0 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        self.stream.write_all(&frame).map_err(io_err)?;
        self.stream.flush().map_err(io_err)
    }

    fn read_more(&mut self) -> Result<(), WsError> {
        let mut buf = [0u8; 4096];
        let n = self.stream.read(&mut buf).map_err(map_read)?;
        if n == 0 {
            return Err(WsError::Closed("peer closed the connection".into()));
        }
        self.partial.extend_from_slice(&buf[..n]);
        Ok(())
    }

    fn need(&mut self, n: usize) -> Result<(), WsError> {
        while self.partial.len() < n {
            self.read_more()?;
        }
        Ok(())
    }

    /// Receive the next complete text message (transparently answers pings,
    /// skips pongs, errors on close frames).
    pub fn recv_text(&mut self) -> Result<String, WsError> {
        let mut message: Option<Vec<u8>> = None;
        loop {
            self.need(2)?;
            let b0 = self.partial[0];
            let b1 = self.partial[1];
            let fin = b0 & 0x80 != 0;
            let opcode = b0 & 0x0F;
            let masked = b1 & 0x80 != 0;
            let mut len = (b1 & 0x7F) as usize;
            let mut offset = 2;
            if len == 126 {
                self.need(4)?;
                len = u16::from_be_bytes([self.partial[2], self.partial[3]]) as usize;
                offset = 4;
            } else if len == 127 {
                self.need(10)?;
                len = u64::from_be_bytes([
                    self.partial[2],
                    self.partial[3],
                    self.partial[4],
                    self.partial[5],
                    self.partial[6],
                    self.partial[7],
                    self.partial[8],
                    self.partial[9],
                ]) as usize;
                offset = 10;
            }
            let mask_len = if masked { 4 } else { 0 };
            self.need(offset + mask_len + len)?;
            let mut payload = self.partial[offset + mask_len..offset + mask_len + len].to_vec();
            if masked {
                let mask = [
                    self.partial[offset],
                    self.partial[offset + 1],
                    self.partial[offset + 2],
                    self.partial[offset + 3],
                ];
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            self.partial.drain(..offset + mask_len + len);

            match opcode {
                0x1 => {
                    // text
                    if fin {
                        return Ok(String::from_utf8_lossy(&payload).into_owned());
                    }
                    message = Some(payload);
                }
                0x0 => {
                    // continuation
                    let acc = message.get_or_insert_with(Vec::new);
                    acc.extend_from_slice(&payload);
                    if fin {
                        return Ok(String::from_utf8_lossy(acc).into_owned());
                    }
                }
                0x8 => {
                    let reason = String::from_utf8_lossy(&payload).into_owned();
                    return Err(WsError::Closed(format!("peer sent close frame: {reason}")));
                }
                0x9 => {
                    // ping -> pong
                    let mut frame = Vec::with_capacity(payload.len() + 2);
                    frame.push(0x8A);
                    let mask = mask4();
                    match payload.len() {
                        n if n < 126 => frame.push(0x80 | n as u8),
                        n => {
                            frame.push(0x80 | 126);
                            frame.extend_from_slice(&(n as u16).to_be_bytes());
                        }
                    }
                    frame.extend_from_slice(&mask);
                    for (i, b) in payload.iter().enumerate() {
                        frame.push(b ^ mask[i % 4]);
                    }
                    self.stream.write_all(&frame).map_err(io_err)?;
                }
                0xA => { /* pong: ignore */ }
                other => return Err(WsError::Io(format!("unsupported opcode {other}"))),
            }
        }
    }
}

fn map_read(e: std::io::Error) -> WsError {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            WsError::Timeout(format!("read timed out: {e}"))
        }
        _ => WsError::Io(format!("read failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn accept_vector_from_rfc6457() {
        // The worked example from RFC 6455 §1.3.
        assert_eq!(
            accept_for("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn handshake_and_frame_roundtrip_against_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // Read request headers.
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let key = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
                .unwrap()
                .split(':')
                .nth(1)
                .unwrap()
                .trim()
                .to_string();
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                accept_for(&key)
            );
            s.write_all(resp.as_bytes()).unwrap();
            // Read one masked client frame, then reply with a text frame.
            let mut hdr = [0u8; 2];
            s.read_exact(&mut hdr).unwrap();
            let len = (hdr[1] & 0x7F) as usize;
            let mut mask = [0u8; 4];
            s.read_exact(&mut mask).unwrap();
            let mut payload = vec![0u8; len];
            s.read_exact(&mut payload).unwrap();
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
            assert_eq!(String::from_utf8_lossy(&payload), "ping-msg");
            let reply = b"hello-back";
            let mut frame = vec![0x81u8, reply.len() as u8];
            frame.extend_from_slice(reply);
            s.write_all(&frame).unwrap();
        });

        let mut client =
            WsClient::connect(&format!("ws://{addr}/socket"), Duration::from_secs(5)).unwrap();
        client.send_text("ping-msg").unwrap();
        assert_eq!(client.recv_text().unwrap(), "hello-back");
        server.join().unwrap();
    }

    #[test]
    fn handshake_rejects_non_websocket_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if s.read_exact(&mut b).is_err() {
                    return;
                }
                head.push(b[0]);
            }
            s.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let err = WsClient::connect(&format!("ws://{addr}/"), Duration::from_secs(5))
            .err()
            .expect("handshake must be rejected");
        assert!(err.to_string().contains("handshake rejected"), "got: {err}");
        server.join().unwrap();
    }
}
