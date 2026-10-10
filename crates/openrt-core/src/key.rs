//! Namespace/key encoding helpers shared by Phase 2/3 record namespaces.
//!
//! The one store's `put_record`/`get_record`/`list_records` contract (ADR-006)
//! restricts keys to a **path-safe charset** — lowercase `a-z`, digits, and
//! `.`, `_`, `-` (see `openrt_persistence`'s `validate_ns_key`). Artifact
//! identifiers (Hugging Face ids such as `meta-llama/Llama-3.1-8B`, digest
//! keys, expert shard keys) legitimately contain `/`, `:`, uppercase letters
//! and non-ASCII bytes, which the store rejects outright.
//!
//! The encoding below is deterministic, injective, locale-free, and stays
//! *within* the store's charset:
//!
//! * a byte in `[a-z0-9._]` is emitted verbatim;
//! * every other byte — including `-`, uppercase letters, `/`, `:`, and each
//!   UTF-8 byte of multibyte characters — becomes `-` followed by two lowercase
//!   hex digits (`-` 0x2d → `-2d`, `/` 0x2f → `-2f`, `L` 0x4c → `-4c`).
//!
//! Because a literal `-` can never appear unescaped in the output, the stream
//! `-hh` is unambiguous: `-` in encoded form is *always* an escape marker.
//! `decode_safe_key` is the total fallible inverse.

/// Encode a string into a store-safe key using only `[a-z0-9._-]`.
///
/// Example: `meta-llama/Llama-3.1-8B` becomes
/// `meta-llama-2f-4clama-3.1-8-42-49nstruct` (`/` 0x2f → `-2f`, `L` 0x4c → `-4c`).
pub fn encode_safe_key(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        let verbatim = b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_');
        if verbatim {
            out.push(b as char);
        } else {
            out.push('-');
            out.push(hex_lower(b >> 4));
            out.push(hex_lower(b & 0x0f));
        }
    }
    out
}

/// Decode [`encode_safe_key`] output. Returns `None` on malformed input (a
/// trailing/truncated escape or a byte outside the safe charset) or invalid
/// UTF-8, never panics.
pub fn decode_safe_key(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'-' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_val(bytes[i + 1])?;
            let lo = hex_val(bytes[i + 2])?;
            out.push(hi << 4 | lo);
            i += 3;
        } else if b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_') {
            out.push(b);
            i += 1;
        } else {
            return None;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_lower(nibble: u8) -> char {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    HEX[nibble as usize & 0x0f] as char
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_artifact_ids() {
        for id in [
            "meta-llama/Llama-3.1-8B-Instruct",
            "mistralai/Mixtral-8x7B-v0.1",
            "plain_ascii.key-v1",
            "ends/slash/",
            "snowman-\u{2603}-model",
            "QWEN2.5:14B@v2",
            "contains-dash",
        ] {
            let encoded = encode_safe_key(id);
            // never any byte outside the store's safe charset
            for b in encoded.bytes() {
                assert!(
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'),
                    "unsafe byte in encoded key {encoded:?}"
                );
            }
            assert_eq!(
                decode_safe_key(&encoded).as_deref(),
                Some(id),
                "roundtrip for {id}"
            );
        }
    }

    #[test]
    fn decode_rejects_malformed_input() {
        assert_eq!(decode_safe_key("a-"), None);
        assert_eq!(decode_safe_key("a-2"), None);
        assert_eq!(decode_safe_key("a-2G"), None); // uppercase hex not produced by encode
        assert_eq!(decode_safe_key("has/space"), None); // not a valid encoded form
        assert_eq!(decode_safe_key("~"), None);
    }

    #[test]
    fn encoding_is_injective_despite_dash() {
        // `-` is always escaped, so hyphen-bearing ids can't collide with escapes.
        assert_eq!(encode_safe_key("a-b"), "a-2db");
        assert_eq!(encode_safe_key("a-2db"), "a-2d2db");
        assert_eq!(decode_safe_key("a-2db").as_deref(), Some("a-b"));
        assert_ne!(encode_safe_key("/b"), encode_safe_key("-62b"));
    }
}
