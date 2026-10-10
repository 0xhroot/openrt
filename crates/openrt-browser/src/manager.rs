//! Lazy browser session manager: resolves `fabric.browser.backend`
//! (`auto` | `firefox` | `mock` | `off`) and starts a backend on first use.
//!
//! Honesty rules:
//! - `auto` probes for a **real** Firefox and fails with structured
//!   `UNAVAILABLE` when absent — never a silent mock fallback.
//! - `mock` is an explicit, operator-chosen deterministic backend.
//! - `off` is structured `UNAVAILABLE` on use (no spawn attempts).

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use openrt_config::BrowserConfig;
use openrt_fabric::{FabricError, SupportState};

use crate::backend::BrowserBackend;
use crate::firefox;

pub struct Session {
    pub backend: Arc<dyn BrowserBackend>,
    pub started_at: String,
}

pub struct BrowserManager {
    choice: String,
    binary: Option<PathBuf>,
    session: tokio::sync::Mutex<Option<Session>>,
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

impl BrowserManager {
    pub fn new(cfg: &BrowserConfig) -> Self {
        Self {
            choice: cfg.backend.clone().unwrap_or_else(|| "auto".into()),
            binary: cfg.binary.clone(),
            session: tokio::sync::Mutex::new(None),
        }
    }

    /// Real Firefox binary (configured override or PATH probe), if any.
    pub fn firefox_binary(&self) -> Option<PathBuf> {
        self.binary
            .clone()
            .or_else(|| which("firefox").or_else(|| which("firefox-esr")))
    }

    /// Platform-matrix support claim for `fabric.metas` (no session start).
    pub fn support(&self) -> SupportState {
        match self.choice.as_str() {
            "off" => SupportState::Unavailable,
            "mock" => SupportState::Partial, // deterministic test backend only
            _ if self.firefox_binary().is_some() => SupportState::Green,
            _ => SupportState::Unavailable,
        }
    }

    /// The structured failure a tool would produce right now without
    /// starting anything (used to keep status/off honest).
    fn disabled_error(&self) -> Option<FabricError> {
        match self.choice.as_str() {
            "off" => Some(FabricError::unavailable(
                "browser backend",
                "browser backend is disabled by config (fabric.browser.backend: off)",
                "browser.*",
                "set fabric.browser.backend to auto/firefox to enable, or mock for tests",
            )),
            "auto" | "firefox" if self.firefox_binary().is_none() => Some(FabricError::unavailable(
                "browser backend",
                "no `firefox` binary found (PATH/config)",
                "browser.*",
                "install Firefox, or set fabric.browser.backend to `mock` for tests / `off` to disable",
            )),
            other if !matches!(other, "auto" | "firefox" | "mock") => Some(FabricError::unavailable(
                "browser backend",
                format!("unknown fabric.browser.backend `{other}`"),
                "browser.*",
                "use auto|firefox|mock|off (config validation should reject others)",
            )),
            _ => None,
        }
    }

    /// Start (or reuse) the browser session. Structured `UNAVAILABLE` when
    /// the configured choice cannot produce a backend — no silent fallback.
    pub async fn backend(&self) -> Result<Arc<dyn BrowserBackend>, FabricError> {
        if let Some(e) = self.disabled_error() {
            return Err(e);
        }
        let mut slot = self.session.lock().await;
        if let Some(s) = slot.as_ref() {
            return Ok(s.backend.clone());
        }
        let backend: Arc<dyn BrowserBackend> = match self.choice.as_str() {
            "mock" => Arc::new(crate::mock::MockBackend::new()),
            "firefox" => Arc::new(firefox::launch(self.binary.clone()).await?),
            // "auto": real browser only — probe already passed above.
            _ => Arc::new(firefox::launch(self.binary.clone()).await?),
        };
        *slot = Some(Session {
            backend: backend.clone(),
            started_at: chrono_lite_now(),
        });
        Ok(backend)
    }

    /// Session state without starting anything.
    pub async fn status(&self) -> Value {
        let slot = self.session.lock().await;
        match slot.as_ref() {
            Some(s) => json!({
                "active": true,
                "backend": s.backend.name(),
                "started_at": s.started_at,
                "configured": self.choice,
            }),
            None => json!({
                "active": false,
                "backend": Value::Null,
                "started_at": Value::Null,
                "configured": self.choice,
                "support": format!("{:?}", self.support()).to_lowercase(),
            }),
        }
    }

    /// End the session (backend.close + drop). Idempotent.
    pub async fn close(&self) -> Result<Value, FabricError> {
        let mut slot = self.session.lock().await;
        if let Some(s) = slot.take() {
            s.backend.close().await?;
            Ok(json!({ "ok": true, "backend": s.backend.name(), "closed": true }))
        } else {
            Ok(
                json!({ "ok": true, "backend": Value::Null, "closed": false, "detail": "no active session" }),
            )
        }
    }
}

/// RFC3339 UTC timestamp without pulling chrono into this crate's deps
/// (persisted sessions only need a readable clock label).
fn chrono_lite_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{now}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(backend: &str) -> BrowserConfig {
        BrowserConfig {
            backend: Some(backend.into()),
            binary: None,
        }
    }

    #[tokio::test]
    async fn off_is_structured_unavailable() {
        let m = BrowserManager::new(&cfg("off"));
        assert_eq!(format!("{:?}", m.support()), "Unavailable");
        let err = m.backend().await.err().expect("must fail");
        assert_eq!(err.code, openrt_fabric::ErrorCode::Unavailable);
        // status works without starting
        assert_eq!(m.status().await["active"], json!(false));
    }

    #[tokio::test]
    async fn mock_starts_and_closes() {
        let m = BrowserManager::new(&cfg("mock"));
        assert_eq!(format!("{:?}", m.support()), "Partial");
        let b1 = m.backend().await.unwrap();
        let b2 = m.backend().await.unwrap();
        assert!(Arc::ptr_eq(&b1, &b2), "same session reused");
        assert_eq!(b1.name(), "mock");
        let closed = m.close().await.unwrap();
        assert_eq!(closed["closed"], json!(true));
        assert_eq!(m.status().await["active"], json!(false));
        // Close is idempotent.
        assert_eq!(m.close().await.unwrap()["closed"], json!(false));
    }

    #[tokio::test]
    async fn auto_uses_real_firefox_or_reports_unavailable() {
        let m = BrowserManager::new(&cfg("auto"));
        match m.firefox_binary() {
            Some(bin) => {
                assert!(bin.is_file());
                assert_eq!(format!("{:?}", m.support()), "Green");
            }
            None => {
                assert_eq!(format!("{:?}", m.support()), "Unavailable");
                let err = m.backend().await.err().expect("must fail");
                assert_eq!(err.code, openrt_fabric::ErrorCode::Unavailable);
            }
        }
    }
}
