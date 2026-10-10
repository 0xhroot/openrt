//! Phase 7B browser automation (M7B.6): headless Firefox over WebDriver BiDi
//! with a pure-Rust WebSocket client, plus policy-gated `browser.*` tools.
//!
//! Design decisions (mission-locked):
//! - **No fake browser.** `auto`/`firefox` require a real Firefox binary and
//!   fail closed with structured `UNAVAILABLE`; `mock` is explicit operator
//!   choice for tests/CI; `off` never spawns anything.
//! - Egress funnels through `openrt_fabric::net_guard::ensure_url_allowed`
//!   (mode first — Sovereign refuses external URLs before any socket — then
//!   netpol grants; loopback/file/data URLs always allowed).
//! - All interactions (find/click/type/select/submit/upload/download) run as
//!   in-page JavaScript over `script.evaluate`; screenshots come from
//!   `browsingContext.captureScreenshot` and are verified by PNG signature +
//!   sha256 artifact records.
//! - Downloads refuse redirects (`redirect: "error"`) so a redirect can never
//!   dodge the egress check performed on the requested URL.

pub mod backend;
pub mod firefox;
pub mod manager;
pub mod mock;
pub mod sha1;
pub mod tools;
pub mod ws;

pub use backend::BrowserBackend;
pub use manager::BrowserManager;
pub use tools::{browser_metas, browser_tools, BrowserToolCtx};
