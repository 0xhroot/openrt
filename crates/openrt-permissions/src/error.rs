//! Permission errors.

/// Errors produced by the permission subsystem.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PermissionError {
    #[error("invalid capability `{0}`: must match `domain.resource.action`")]
    InvalidCapability(String),
    #[error("invalid glob `{0}`: only `*` wildcards are supported")]
    InvalidGlob(String),
    #[error("invalid rule: {0}")]
    InvalidRule(String),
    #[error("no suitable resolver for an `ask` decision: {0}")]
    NoResolver(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages() {
        let e = PermissionError::InvalidCapability("bad".into());
        assert_eq!(
            e.to_string(),
            "invalid capability `bad`: must match `domain.resource.action`"
        );
    }
}
