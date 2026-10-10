//! Phase 9 M9.8: the production answer-path hallucination firewall.
//!
//! One firewall, applied through the runtime's answer paths (OWNERSHIP): a
//! claim is verified by `openrt-verifier` (deterministic evidence checks
//! first, bounded model verification, honest cross-model independence) and
//! its verdict is persisted under `verify.verdicts` before the report is
//! returned. No second firewall and no second verdict store exist.

use std::sync::Arc;

use openrt_verifier::{BackendSpec, FirewallReport, VerifyInput, VerifyOptions};

use crate::Runtime;

impl Runtime {
    /// Default verification options for this runtime: the runtime's own
    /// provider with its explicit default model. When the provider
    /// advertises no model, NO backend is configured — the firewall then
    /// runs in honest deterministic-only mode (reported as a degradation,
    /// never as a model verdict).
    pub fn verify_options(&self) -> VerifyOptions {
        let mut opts = VerifyOptions::default();
        if let Some(model) = self.provider.info().default_model.clone() {
            opts.backends
                .push(BackendSpec::new(Arc::clone(&self.provider), model));
        }
        opts
    }

    /// M9.8 / TEST 8 — run the hallucination firewall over ONE claim at a
    /// production answer path. The verdict record is persisted before `Ok`
    /// is returned; an `Err` never means "verified".
    ///
    /// M9.7 production path: the adjudicated claim also lands in the
    /// evidence graph (`knowledge.evidence`), linked to exactly those
    /// citations that RESOLVED against persisted chunks — an unresolved
    /// citation can never become an edge (`link_claim` re-validates).
    pub async fn verify_claim(
        &self,
        input: VerifyInput,
        opts: &VerifyOptions,
    ) -> Result<FirewallReport, crate::Error> {
        let claim_text = input.claim.clone();
        let citations = input.citations.clone();
        let report = openrt_verifier::verify_claim(&*self.store, input, opts).await?;
        self.record_claim_graph(&report.kb_id, &claim_text, &citations, &report)
            .await?;
        Ok(report)
    }

    /// Bounded batch variant (`limits.max_claims` enforced — an over-limit
    /// batch is an error, never silently truncated).
    pub async fn verify_claims(
        &self,
        inputs: Vec<VerifyInput>,
        opts: &VerifyOptions,
    ) -> Result<Vec<FirewallReport>, crate::Error> {
        let snapshot: Vec<(String, Vec<openrt_knowledge::Citation>)> = inputs
            .iter()
            .map(|i| (i.claim.clone(), i.citations.clone()))
            .collect();
        let reports = openrt_verifier::verify_claims(&*self.store, inputs, opts).await?;
        for (report, (claim, citations)) in reports.iter().zip(snapshot.iter()) {
            self.record_claim_graph(&report.kb_id, claim, citations, report)
                .await?;
        }
        Ok(reports)
    }

    /// Append the claim + its resolved-citation links to the evidence graph
    /// (idempotent, append-only; same-crate internal helper for the two
    /// verify entry points above).
    async fn record_claim_graph(
        &self,
        kb_id: &str,
        claim: &str,
        citations: &[openrt_knowledge::Citation],
        report: &FirewallReport,
    ) -> Result<(), crate::Error> {
        let rec = openrt_evidence::record_claim(&*self.store, kb_id, claim).await?;
        for (cit, status) in citations.iter().zip(report.deterministic.citations.iter()) {
            if status.resolved {
                openrt_evidence::link_claim(&*self.store, kb_id, &rec.claim_id, cit).await?;
            }
        }
        Ok(())
    }
}
