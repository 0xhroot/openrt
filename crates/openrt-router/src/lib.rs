//! Model router (ADR-010): **filter-then-rank** selection over the local,
//! installed registry only.
//!
//! # Policy
//!
//! * **No remote fallback.** Routing consults only verified, scanned, activated
//!   artifacts already materialized in the registry. A request that cannot be
//!   served locally is *refused with explicit reasons*, never silently
//!   redirected to a network model.
//! * **Filter-then-rank.** Every candidate card is filtered against the
//!   request (exact artifact id / query, family, tags, size budget). Only
//!   artifacts in lifecycle state `Active` pass the eligibility gate — an
//!   artifact that has never been verified+scanned cannot be routed
//!   (fail-closed, ADR-010).
//! * **Deterministic ranking.** Among eligible matches, the lightest
//!   artifact is preferred (fewer resident bytes → more room on the device),
//!   ties broken by artifact id. No learned weights, no side effects.
//! * **Mode-independent.** Routing is a local operation: it runs in both
//!   Provisioning and Sovereign, and never touches the network.
//!
//! The caller supplies a [`ModelRegistry`] reference; the router reads
//! lifecycle/cards from it via the store.

use openrt_modes::ExecutionMode;
use openrt_registry::{RegistryCard, RegistryError};

pub mod signals;
use serde::{Deserialize, Serialize};
pub use signals::{
    list_signals, preference, record_signal, RoutingSignal, SignalPreference, SIGNALS_NS,
};

/// A routing request. Fields are additive filters; a `None`/empty field does
/// not constrain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRequest {
    /// Exact artifact id (e.g. `curated/llama3.1-8b`). When set, matching is
    /// by key-normalized identity, not substring.
    pub artifact_id: Option<String>,
    /// Free-text query matched against artifact id, display name, family and
    /// tags (lowercased substring).
    pub query: Option<String>,
    /// Only artifacts whose family equals this value.
    pub family: Option<String>,
    /// Every tag in this list must be present on the artifact.
    pub tags: Vec<String>,
    /// Only artifacts whose installed bytes fit this budget.
    pub max_bytes: Option<u64>,
}

/// A successfully selected artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    pub artifact_id: String,
    pub display_name: String,
    pub installed_bytes: u64,
    /// Deterministic rank (1-based) among the eligible candidates.
    pub rank: usize,
    /// Execution mode at routing time (observability only).
    pub mode: ExecutionMode,
}

/// Why one candidate was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub artifact_id: String,
    pub reasons: Vec<String>,
}

/// Outcome of a routing attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteVerdict {
    Selected(RouteDecision),
    /// No local artifact could serve the request. Every candidate that was
    /// considered carries an explicit rejection reason; `summary` names the
    /// overall cause.
    Refused {
        rejections: Vec<Rejection>,
        summary: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("registry error: {0}")]
    Registry(#[from] RegistryError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("internal: {0}")]
    Internal(String),
    #[error("store error: {0}")]
    Store(#[from] openrt_persistence::PersistenceError),
}

/// Deterministic filter-then-rank router.
#[derive(Debug, Clone, Default)]
pub struct Router {
    /// Maximum number of candidates considered (safety bound against an
    /// unbounded catalog).
    pub max_candidates: usize,
}

impl Router {
    pub fn new() -> Self {
        Self {
            max_candidates: 512,
        }
    }

    /// Route the request to exactly one local artifact, or refuse with
    /// reasons. Fail-closed: infra errors propagate; "no route" is a
    /// [`RouteVerdict::Refused`], never a fabricated selection.
    pub async fn route(
        &self,
        registry: &openrt_registry::ModelRegistry,
        request: &RouteRequest,
    ) -> Result<RouteVerdict, RouterError> {
        let cards = registry.list().await?;
        let mode = registry.mode().await?;
        let mut considered: Vec<(RegistryCard, Vec<String>)> = Vec::new();

        for card in cards {
            let mut reasons: Vec<String> = Vec::new();

            // Identity filters decide *whether the request is about this
            // candidate at all*. A card that does not match the requested id
            // or query is simply not addressed — it is excluded silently, so
            // refusal reasons stay specific to what was actually asked for.
            if let Some(id) = &request.artifact_id {
                if openrt_core::encode_safe_key(id)
                    != openrt_core::encode_safe_key(&card.artifact_id)
                {
                    continue;
                }
            }
            if let Some(q) = &request.query {
                let needle = q.to_ascii_lowercase();
                let hay = format!(
                    "{} {} {} {}",
                    card.artifact_id,
                    card.display_name,
                    card.family,
                    card.tags.join(" ")
                )
                .to_ascii_lowercase();
                if !hay.contains(&needle) {
                    continue;
                }
            }
            if let Some(want) = &request.family {
                if &card.family != want {
                    reasons.push(format!("family `{}` != requested `{}`", card.family, want));
                }
            }
            for want in &request.tags {
                if !card.tags.iter().any(|t| t == want) {
                    reasons.push(format!("missing tag `{want}`"));
                }
            }
            if let Some(budget) = request.max_bytes {
                if card.installed_bytes == 0 || card.installed_bytes > budget {
                    reasons.push(format!(
                        "installed {}/{} bytes > budget {budget}",
                        card.installed_bytes, budget
                    ));
                }
            }
            if card.state != openrt_registry::LifecycleState::Active {
                reasons.push(format!(
                    "not active (state `{}`); must be verified, scanned and activated",
                    card.state.as_str()
                ));
            }
            considered.push((card, reasons));
        }

        if considered.len() > self.max_candidates {
            return Err(RouterError::Internal(format!(
                "candidate set exceeded safety bound {}/{}",
                considered.len(),
                self.max_candidates
            )));
        }

        // Rank: lightest first, ties by artifact id — fully deterministic.
        let mut eligible: Vec<(RegistryCard, usize)> = considered
            .iter()
            .enumerate()
            .filter(|(_, (_, reasons))| reasons.is_empty())
            .map(|(i, (card, _))| (card.clone(), i))
            .collect();
        eligible.sort_by(|a, b| {
            a.0.installed_bytes
                .cmp(&b.0.installed_bytes)
                .then_with(|| a.0.artifact_id.cmp(&b.0.artifact_id))
        });

        if let Some((card, _)) = eligible.first() {
            let rank = 1 + eligible
                .iter()
                .position(|(c, _)| c.artifact_id == card.artifact_id)
                .unwrap_or(0);
            return Ok(RouteVerdict::Selected(RouteDecision {
                artifact_id: card.artifact_id.clone(),
                display_name: card.display_name.clone(),
                installed_bytes: card.installed_bytes,
                rank,
                mode,
            }));
        }

        let rejections = considered
            .into_iter()
            .map(|(card, reasons)| Rejection {
                artifact_id: card.artifact_id,
                reasons,
            })
            .collect();
        let summary = if request.artifact_id.is_some() {
            "no locally activated artifact matches the requested id".to_string()
        } else {
            "no locally activated artifact matches the query/filters".to_string()
        };
        Ok(RouteVerdict::Refused {
            rejections,
            summary,
        })
    }

    /// Phase 12 (M12.18): route with a learned-signal overlay. The base
    /// router decision is authoritative; a signal preference may only
    /// redirect to an artifact that passes the SAME routing checks. If the
    /// preferred artifact is refused (policy/mode/eligibility/bytes), the
    /// base decision stands — learned signals never override eligibility.
    pub async fn route_with_signals(
        &self,
        registry: &openrt_registry::ModelRegistry,
        request: &RouteRequest,
        signals: &[RoutingSignal],
        task_type: &str,
        min_samples: usize,
    ) -> Result<RouteVerdict, RouterError> {
        let base = self.route(registry, request).await?;
        let pref = signals::preference(signals, task_type, min_samples);
        let Some(preferred) = pref.ranking.first().cloned() else {
            return Ok(base);
        };
        let RouteVerdict::Selected(base_dec) = &base else {
            return Ok(base);
        };
        if preferred == base_dec.artifact_id {
            return Ok(base);
        }
        let alt = RouteRequest {
            artifact_id: Some(preferred.clone()),
            ..request.clone()
        };
        match self.route(registry, &alt).await? {
            RouteVerdict::Selected(d) => Ok(RouteVerdict::Selected(d)),
            RouteVerdict::Refused { .. } => Ok(base),
        }
    }

    /// Direct eligibility check for one artifact id (used by exact-addressed
    /// callers / CLI). Returns the deterministic decision or the explicit
    /// refusal reason list.
    pub async fn is_eligible(
        &self,
        registry: &openrt_registry::ModelRegistry,
        artifact_id: &str,
    ) -> Result<RouteDecision, Vec<String>> {
        match self
            .route(
                registry,
                &RouteRequest {
                    artifact_id: Some(artifact_id.to_string()),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(RouteVerdict::Selected(d)) => Ok(d),
            Ok(RouteVerdict::Refused { rejections, .. }) => {
                let reasons = rejections
                    .iter()
                    .flat_map(|r| r.reasons.iter().cloned())
                    .collect::<Vec<_>>();
                if reasons.is_empty() {
                    Err(vec![format!(
                        "artifact `{artifact_id}` is not installed locally"
                    )])
                } else {
                    Err(reasons)
                }
            }
            Err(e) => Err(vec![format!("{e}")]),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use openrt_modes::{ExecutionMode, ModeManager};
    use openrt_persistence::MemoryStore;
    use openrt_registry::ModelRegistry;
    use openrt_scan::ArtifactKind;

    use super::*;

    fn store() -> Arc<MemoryStore> {
        Arc::new(MemoryStore::new())
    }

    async fn provisioned(store: Arc<MemoryStore>) -> ModeManager {
        let m = ModeManager::new(store);
        m.transition("test-operator", ExecutionMode::Provisioning, "test")
            .await
            .unwrap();
        m
    }

    fn manifest(
        id: &str,
        files: &[(&str, &[u8])],
    ) -> (openrt_registry::ManifestV1, BTreeMap<String, Vec<u8>>) {
        let mut map = BTreeMap::new();
        let mut refs = Vec::new();
        for (file_key, bytes) in files {
            let digest = openrt_verify::sha256_hex(bytes);
            refs.push(openrt_registry::BlobRef {
                file_key: file_key.to_string(),
                digest,
                size_bytes: bytes.len() as u64,
                // These fixtures hand the bytes to the registry inline, so the
                // blobs are materialized in the CAS rather than referenced.
                location: openrt_registry::BlobLocation::Stored,
            });
            map.insert(file_key.to_string(), bytes.to_vec());
        }
        (
            openrt_registry::ManifestV1 {
                schema_version: openrt_registry::MANIFEST_SCHEMA_VERSION,
                artifact_id: id.to_string(),
                kind: ArtifactKind::Binary,
                blobs: refs,
                base_digest: None,
                created_at: openrt_core::now().to_rfc3339(),
            },
            map,
        )
    }

    /// A scanner primed with a signed bundle whose policy clears the fixture
    /// bytes and allows a default safetensors policy.
    fn clean_scanner() -> openrt_scan::Scanner {
        let mut sc = openrt_scan::Scanner::new();
        let secret = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let content = openrt_scan::BundleContent {
            generated_at: "fixture".into(),
            scheme_version: 1,
            known_bad: vec![],
            safetensors_policy: Some(openrt_scan::SafetensorsPolicy::default()),
        };
        sc.add_bundle(openrt_scan::sign_bundle(
            &secret,
            "router-test-key",
            content,
        ));
        sc
    }

    async fn install_and_activate(registry: &ModelRegistry, id: &str, bytes: &[u8]) {
        let (m, files) = manifest(id, &[("model.bin", bytes)]);
        let sc = clean_scanner();
        registry
            .import_local(m, &files, "t", Some((&sc, 1)))
            .await
            .unwrap();
        registry.activate(id, "t").await.unwrap();
    }

    #[tokio::test]
    async fn never_routes_unverified_artifacts() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), provisioned(s.clone()).await);
        let bytes = b"unscanned".to_vec();
        let (m, files) = manifest("curated/llama3.1-8b", &[("model.bin", &bytes)]);
        registry.import_local(m, &files, "t", None).await.unwrap(); // state: Verified

        let router = Router::new();
        let verdict = router
            .route(
                &registry,
                &RouteRequest {
                    artifact_id: Some("curated/llama3.1-8b".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        match verdict {
            RouteVerdict::Refused {
                rejections,
                summary,
            } => {
                assert!(!summary.is_empty());
                let reason = rejections
                    .iter()
                    .flat_map(|r| r.reasons.iter())
                    .find(|r| r.contains("not active"))
                    .expect("must cite the activation gate");
                assert!(reason.contains("not active"));
            }
            RouteVerdict::Selected(_) => panic!("must not route a never-activated artifact"),
        }
    }

    #[tokio::test]
    async fn routes_exact_active_artifact() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), provisioned(s.clone()).await);
        install_and_activate(&registry, "curated/llama3.1-8b", b"model-8b").await;

        let router = Router::new();
        let verdict = router
            .route(
                &registry,
                &RouteRequest {
                    artifact_id: Some("curated/llama3.1-8b".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        match verdict {
            RouteVerdict::Selected(d) => {
                assert_eq!(d.artifact_id, "curated/llama3.1-8b");
                assert_eq!(d.rank, 1);
            }
            RouteVerdict::Refused { .. } => panic!("expected a route"),
        }
    }

    #[test]
    fn signal_preference_needs_min_samples_and_ranks_deterministically() {
        let mut sigs = Vec::new();
        for _ in 0..3 {
            sigs.push(RoutingSignal::new(
                "summarize",
                "curated/b",
                false,
                None,
                "test",
            ));
            sigs.push(RoutingSignal::new(
                "summarize",
                "curated/a",
                true,
                None,
                "test",
            ));
        }
        let empty = preference(&sigs, "summarize", 10);
        assert!(empty.ranking.is_empty());
        assert_eq!(empty.samples, 6);

        let pref = preference(&sigs, "summarize", 5);
        assert_eq!(
            pref.ranking,
            vec!["curated/a".to_string(), "curated/b".to_string()]
        );
        assert_eq!(pref.success_rates[0].1, 1.0);
        assert_eq!(pref.method, "signal-preference/v1");

        let none = preference(&sigs, "other-task", 1);
        assert!(none.ranking.is_empty());
    }

    #[tokio::test]
    async fn signal_records_persist_and_list() {
        let store = MemoryStore::new();
        let sig = RoutingSignal::new("classify", "curated/m", true, Some(42), "test");
        record_signal(&store, &sig).await.unwrap();
        record_signal(&store, &sig).await.unwrap();
        let all = list_signals(&store).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].latency_ms, Some(42));
    }

    #[tokio::test]
    async fn learned_signals_only_redirect_to_eligible_artifacts() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), provisioned(s.clone()).await);
        install_and_activate(&registry, "curated/fixture-big", b"big-big-big").await;
        install_and_activate(&registry, "curated/fixture-small", b"tiny").await;
        let router = Router::new();
        let req = RouteRequest {
            query: Some("curated".into()),
            ..Default::default()
        };

        // Base decision without signals: lightest eligible artifact wins.
        let base = router.route(&registry, &req).await.unwrap();
        let base_id = match &base {
            RouteVerdict::Selected(d) => d.artifact_id.clone(),
            RouteVerdict::Refused { .. } => panic!("expected base route"),
        };
        assert_eq!(base_id, "curated/fixture-small");

        // Strong signal preference for the (eligible) heavier artifact
        // redirects the decision.
        let mut sigs = Vec::new();
        for _ in 0..5 {
            sigs.push(RoutingSignal::new(
                "summarize",
                "curated/fixture-big",
                true,
                Some(12),
                "test",
            ));
        }
        let redir = router
            .route_with_signals(&registry, &req, &sigs, "summarize", 5)
            .await
            .unwrap();
        match redir {
            RouteVerdict::Selected(d) => assert_eq!(d.artifact_id, "curated/fixture-big"),
            RouteVerdict::Refused { .. } => panic!("signal should redirect to eligible artifact"),
        }

        // A signal preferring an INELIGIBLE (not installed) artifact loses:
        // the base decision stands — signals are subordinate.
        let mut ghost = Vec::new();
        for _ in 0..5 {
            ghost.push(RoutingSignal::new(
                "summarize",
                "curated/ghost",
                true,
                None,
                "test",
            ));
        }
        let kept = router
            .route_with_signals(&registry, &req, &ghost, "summarize", 5)
            .await
            .unwrap();
        match kept {
            RouteVerdict::Selected(d) => assert_eq!(d.artifact_id, base_id),
            RouteVerdict::Refused { .. } => panic!("base decision must stand"),
        }

        // Below min_samples the overlay never engages.
        let one = vec![RoutingSignal::new(
            "summarize",
            "curated/fixture-big",
            true,
            None,
            "test",
        )];
        let noop = router
            .route_with_signals(&registry, &req, &one, "summarize", 5)
            .await
            .unwrap();
        match noop {
            RouteVerdict::Selected(d) => assert_eq!(d.artifact_id, base_id),
            RouteVerdict::Refused { .. } => panic!("base decision must stand"),
        }
    }

    #[tokio::test]
    async fn routes_by_query_and_prefers_lightest_deterministically() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), provisioned(s.clone()).await);
        install_and_activate(&registry, "curated/fixture-big", b"big-big-big-big").await;
        install_and_activate(&registry, "curated/fixture-small", b"tiny").await;
        install_and_activate(&registry, "curated/fixture-tie1", b"eq").await;
        install_and_activate(&registry, "curated/fixture-tie2", b"eq").await;

        let router = Router::new();
        let verdict = router
            .route(
                &registry,
                &RouteRequest {
                    query: Some("curated".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        match verdict {
            // Lightest is 2 bytes ("eq"); among the two equal-size ties the
            // lexicographically smallest id wins: fixture-tie1 < fixture-tie2.
            RouteVerdict::Selected(d) => {
                assert_eq!(d.artifact_id, "curated/fixture-tie1");
                assert_eq!(d.rank, 1);
            }
            RouteVerdict::Refused {
                rejections,
                summary,
            } => {
                eprintln!("REFUSED: {summary}");
                for r in rejections {
                    eprintln!("  {}: {:?}", r.artifact_id, r.reasons);
                }
                panic!("expected a route");
            }
        }

        let big_only = router
            .route(
                &registry,
                &RouteRequest {
                    query: Some("curated".into()),
                    max_bytes: Some(100),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let RouteVerdict::Selected(d) = big_only else {
            panic!("expected a route");
        };
        // Determinism: an identical request always selects the same artifact.
        let big_only_again = router
            .route(
                &registry,
                &RouteRequest {
                    query: Some("curated".into()),
                    max_bytes: Some(100),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            d.artifact_id,
            match big_only_again {
                RouteVerdict::Selected(a) => a.artifact_id,
                RouteVerdict::Refused { .. } => unreachable!(),
            }
        );
    }

    #[tokio::test]
    async fn family_and_tag_filter_applies() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), provisioned(s.clone()).await);
        install_and_activate(&registry, "curated/qwen2.5-7b", b"model-7b").await;
        install_and_activate(&registry, "curated/llama3.1-8b", b"model-8b").await;

        let router = Router::new();
        let verdict = router
            .route(
                &registry,
                &RouteRequest {
                    query: Some("chat".into()),
                    tags: vec!["instruct".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let RouteVerdict::Selected(d) = verdict else {
            panic!("expected a route");
        };
        assert_eq!(d.artifact_id, "curated/llama3.1-8b");

        let verdict = router
            .route(
                &registry,
                &RouteRequest {
                    query: Some("chat".into()),
                    family: Some("qwen".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let RouteVerdict::Selected(d) = verdict else {
            panic!("expected a route");
        };
        assert_eq!(d.artifact_id, "curated/qwen2.5-7b");
    }

    #[tokio::test]
    async fn size_budget_rejects_too_large() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), provisioned(s.clone()).await);
        install_and_activate(&registry, "curated/llama3.1-8b", b"model-8b").await;

        let router = Router::new();
        let verdict = router
            .route(
                &registry,
                &RouteRequest {
                    artifact_id: Some("curated/llama3.1-8b".into()),
                    max_bytes: Some(2),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        match verdict {
            RouteVerdict::Refused { rejections, .. } => {
                assert!(rejections
                    .iter()
                    .flat_map(|r| r.reasons.iter())
                    .any(|r| r.contains("budget")));
            }
            RouteVerdict::Selected(_) => panic!("must refuse over-budget artifact"),
        }
    }

    #[tokio::test]
    async fn unknown_artifact_refused_with_reason() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), provisioned(s.clone()).await);
        let router = Router::new();
        let verdict = router
            .route(
                &registry,
                &RouteRequest {
                    artifact_id: Some("curated/does-not-exist".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        match verdict {
            RouteVerdict::Refused { summary, .. } => {
                assert!(
                    summary.contains("id"),
                    "summary should name the id: {summary}"
                );
            }
            RouteVerdict::Selected(_) => panic!("unknown model must not be routed"),
        }
        // Direct eligibility reports a concrete "not installed" reason.
        let reasons = router
            .is_eligible(&registry, "curated/does-not-exist")
            .await
            .unwrap_err();
        assert!(reasons.iter().any(|r| r.contains("not installed locally")));
    }

    #[tokio::test]
    async fn routing_works_in_sovereign_mode() {
        // Routing is local: it must work in Sovereign too. Materialize under
        // Provisioning, then drop to Sovereign before routing. ModeManager
        // always reads the persisted state, so the registry honors a
        // transition made through another manager on the same store.
        let s = store();
        let modes = ModeManager::new(s.clone());
        modes
            .transition("t", ExecutionMode::Provisioning, "bootstrap")
            .await
            .unwrap();
        let registry = ModelRegistry::new(s.clone(), ModeManager::new(s.clone()));
        install_and_activate(&registry, "curated/llama3.1-8b", b"model-8b").await;
        assert_eq!(registry.mode().await.unwrap(), ExecutionMode::Provisioning);

        modes
            .transition("t", ExecutionMode::Sovereign, "go dark")
            .await
            .unwrap();
        assert_eq!(registry.mode().await.unwrap(), ExecutionMode::Sovereign);

        let router = Router::new();
        let verdict = router
            .route(
                &registry,
                &RouteRequest {
                    artifact_id: Some("curated/llama3.1-8b".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let RouteVerdict::Selected(d) = verdict else {
            panic!("expected a route in sovereign mode");
        };
        assert_eq!(d.artifact_id, "curated/llama3.1-8b");
    }

    #[tokio::test]
    async fn is_eligible_reports_reasons() {
        let s = store();
        let registry = ModelRegistry::new(s.clone(), provisioned(s.clone()).await);
        let router = Router::new();
        let reasons = router
            .is_eligible(&registry, "curated/llama3.1-8b")
            .await
            .unwrap_err();
        assert!(!reasons.is_empty());
    }
}
