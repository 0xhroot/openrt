pub mod palette;
pub mod server;

pub use palette::{CommandPalette, DispatchRequest, PaletteCommand, PaletteOutcome};
pub use server::{serve, ServerHandle, WebUiConfig};

pub use openrt_selfimprove::{ImproveStore, OperatorGrant};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WebUiError {
    #[error("web UI must bind to loopback only, refused {addr}")]
    NonLocalBind { addr: String },
    #[error("bind failed: {0}")]
    BindFailed(String),
    #[error("command not found: {id}")]
    CommandNotFound { id: String },
    #[error("unauthorized command: {id}")]
    UnauthorizedCommand { id: String },
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("io error: {0}")]
    Io(String),
}
