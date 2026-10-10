//! Structured logging setup (tracing substrate).

use tracing_subscriber::EnvFilter;

/// Install the global tracing subscriber for OpenRT.
///
/// * `level` — the maximum log level (an explicit `RUST_LOG` env var always
///   wins).
/// * `json` — emit JSON lines instead of human-readable text.
///
/// Safe to call more than once; if a subscriber is already installed the call
/// is a no-op so an embedding host controls logging.
pub fn init_logging(level: &str, json: bool) {
    let filter = match std::env::var("RUST_LOG") {
        Ok(_) => EnvFilter::from_default_env(),
        Err(_) => EnvFilter::new(level),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    let result = if json {
        builder.json().try_init()
    } else {
        builder.try_init()
    };
    // EAGERLY ignore: another process/embedder already registered a subscriber.
    let _ = result;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        // In test environments a test subscriber may already be installed;
        // both calls must return without panicking.
        init_logging("warn", false);
        init_logging("warn", true);
    }
}
