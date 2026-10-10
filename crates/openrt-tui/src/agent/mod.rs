//! Agent Mode — the always-on chat-first surface of the OPENRT terminal AI
//! IDE. One terminal, two modes: **Agent** (this module) and the existing
//! **Workbench** (`crate::app`/`crate::ui`), toggled with `Ctrl+W`.
//!
//! Wiring (no second authorities):
//! * events: `agent::sink::UiSink` attached to the runtime bus at build time
//! * approvals: `agent::sink::TuiResolver` attached as the `Ask` resolver
//! * model switch: `agent::provider::SwitchableProvider` fills the existing
//!   `ModelRequest.model_id` field (per-request, honoured by the transport)
//! * everything else is a direct read/call of the ONE `Runtime`.

pub mod input;
pub mod provider;
pub mod render;
pub mod run;
pub mod setup;
pub mod sink;
pub mod state;
#[cfg(test)]
mod tests;
pub mod views;

pub use provider::SwitchableProvider;
pub use run::run_agent;
pub use sink::{TuiResolver, UiSink};
pub use state::{AgentApp, Overlay, UiMode, View};
