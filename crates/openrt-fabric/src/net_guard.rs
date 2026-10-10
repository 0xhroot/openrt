//! Network guard shared by every networked 7B tool (browser, remote DB, MCP).
//!
//! Policy: local schemes (`file:`, `data:`, `about:`, `blob:`) and loopback
//! hosts are local IPC and always allowed (Sovereign keeps loopback, ADR-003).
//! Everything else must pass the existing network authority:
//! [`openrt_netpol::NetworkPolicy::ensure_egress`] (which consults
//! [`openrt_modes::ModeManager`] first — Sovereign refuses before any socket).

use openrt_modes::ModeManager;
use openrt_netpol::{EgressDecision, NetworkPolicy};

use crate::error::{ErrorCode, FabricError};

/// Egress scope name used for browser/DB/MCP grants in `netpolicy.grants`.
pub const SCOPE_BROWSER: &str = "browser";
pub const SCOPE_DATABASE: &str = "database";
pub const SCOPE_MCP: &str = "mcp";
/// Egress scope for model transports (provider `generate`, vision grounding,
/// embeddings/vision probes). Grants for this scope live in the same
/// `netpolicy.grants` namespace as every other egress scope.
pub const SCOPE_MODEL: &str = "model";

fn loopback_host(host: Option<&str>) -> bool {
    match host {
        None | Some("") => false,
        Some(h) => {
            let h = h.to_ascii_lowercase();
            h == "localhost"
                || h == "127.0.0.1"
                || h == "::1"
                || h == "[::1]"
                || h.starts_with("127.")
        }
    }
}

/// Check whether `url` may be contacted under the current mode + egress grants.
///
/// Returns `Ok(())` for local/loopback targets; otherwise the netpol decision
/// (Sovereign ⇒ structured POLICY_DENIED before any socket is opened).
pub async fn ensure_url_allowed(
    url: &str,
    scope: &str,
    modes: &ModeManager,
    netpol: &NetworkPolicy,
) -> Result<(), FabricError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        FabricError::invalid_input(
            "network url",
            format!("`{url}` is not a valid URL: {e}"),
            "use an absolute URL such as `https://…`, `file://…`, or `data:…`",
        )
    })?;
    match parsed.scheme() {
        "file" | "data" | "about" | "blob" | "moz-extension" => return Ok(()),
        "http" | "https" | "ws" | "wss" => {}
        other => {
            return Err(FabricError::invalid_input(
                "network url",
                format!("scheme `{other}:` is not supported by the fabric"),
                "use http(s)/ws(s), file, data, or about URLs",
            ))
        }
    }
    if loopback_host(parsed.host_str()) {
        return Ok(());
    }
    // Authority 1: operating mode (Sovereign refuses before netpol, ADR-003).
    if let Err(e) = modes.ensure_network_allowed().await {
        return Err(FabricError::policy_denied(
            format!("egress to `{url}`"),
            format!("operating mode refused network access: {e}"),
            "switch to Provisioning mode (audited) for external network use; loopback and file URLs work in Sovereign mode",
        ));
    }
    let destination = match parsed.host_str() {
        Some(h) => {
            let port = parsed.port_or_known_default().unwrap_or(0);
            format!("{h}:{port}")
        }
        None => url.to_string(),
    };
    match netpol.ensure_egress(scope, &destination).await {
        Ok(EgressDecision::Allow { .. }) => Ok(()),
        Ok(EgressDecision::Deny { reason, .. }) => Err(FabricError::policy_denied(
            format!("egress to `{destination}`"),
            reason,
            "switch to Provisioning mode (audited) or add an egress grant for this scope; loopback/file URLs always work in Sovereign mode",
        )),
        Err(e) => Err(FabricError::new(
            ErrorCode::BackendFailure,
            "egress policy evaluation",
            e.to_string(),
            "netpol ensure_egress",
            "check mode/network policy store health",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection() {
        assert!(loopback_host(Some("localhost")));
        assert!(loopback_host(Some("127.0.0.1")));
        assert!(loopback_host(Some("127.42.0.9")));
        assert!(loopback_host(Some("::1")));
        assert!(!loopback_host(Some("example.com")));
        assert!(!loopback_host(Some("10.0.0.1")));
        assert!(!loopback_host(None));
    }

    #[tokio::test]
    async fn local_schemes_allowed_without_policy() {
        let modes = ModeManager::new(std::sync::Arc::new(openrt_persistence::MemoryStore::new()));
        let netpol =
            NetworkPolicy::new(std::sync::Arc::new(openrt_persistence::MemoryStore::new()));
        for u in [
            "file:///tmp/x.html",
            "data:text/html,hi",
            "about:blank",
            "http://127.0.0.1:9222/json",
            "http://localhost:1/json",
        ] {
            ensure_url_allowed(u, SCOPE_BROWSER, &modes, &netpol)
                .await
                .unwrap_or_else(|e| panic!("{u}: {e}"));
        }
    }

    #[tokio::test]
    async fn sovereign_denies_external_http() {
        let modes = ModeManager::new(std::sync::Arc::new(openrt_persistence::MemoryStore::new()));
        let netpol =
            NetworkPolicy::new(std::sync::Arc::new(openrt_persistence::MemoryStore::new()));
        // Default mode is Sovereign (fail-closed).
        let err = ensure_url_allowed("https://example.com/x", SCOPE_BROWSER, &modes, &netpol)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied);
    }

    #[tokio::test]
    async fn invalid_url_is_invalid_input() {
        let modes = ModeManager::new(std::sync::Arc::new(openrt_persistence::MemoryStore::new()));
        let netpol =
            NetworkPolicy::new(std::sync::Arc::new(openrt_persistence::MemoryStore::new()));
        let err = ensure_url_allowed("not a url", SCOPE_BROWSER, &modes, &netpol)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }
}
