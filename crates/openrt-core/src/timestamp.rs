use chrono::{DateTime, Utc};

/// Absolute timestamp used across the domain.
///
/// Serialized as RFC 3339 for human-readable, deterministic persistence and
/// event payloads.
pub type Timestamp = DateTime<Utc>;

/// Current UTC timestamp.
pub fn now() -> Timestamp {
    Utc::now()
}

/// Truncate a timestamp to milliseconds precision (stable for round-trips).
pub fn to_millis(ts: Timestamp) -> i64 {
    ts.timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_parseable() {
        let t = now();
        let s = t.to_rfc3339();
        let back: Timestamp = s.parse().unwrap();
        assert_eq!(t.timestamp_millis(), back.timestamp_millis());
    }
}
