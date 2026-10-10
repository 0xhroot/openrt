//! Core-level error types.
//!
//! Every sibling crate owns its domain error (`*Error` in its crate). This
//! module only holds errors for types defined here (serialization, invalid
//! identifiers, malformed input payloads).

/// Errors produced by the shared primitives themselves.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CoreError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unparsable value: {0}")]
    Malformed(String),
    #[error("serialization failure: {0}")]
    Serialization(String),
    #[error("unknown variant `{0}`")]
    UnknownVariant(String),
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        CoreError::Serialization(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_error_display() {
        let e = CoreError::InvalidInput("nope".into());
        assert!(e.to_string().contains("nope"));
    }
}
