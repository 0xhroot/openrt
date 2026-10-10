//! Phase 6B production integration: every M6.1-remainder / M6.2–M6.4 entry
//! point exercised through the production `Runtime` façade (never a test-only
//! shim). Trading (M6.5–M6.8) remains roadmap-optional and is not exercised
//! here; the reserved `KillScope::Trading` classification is unit-tested in
//! `openrt-safety`.

use std::sync::Arc;

use openrt_config::{Config, OperatingMode, PermissionsConfig, RuleConfig, RuleEffect};
use openrt_persistence::MemoryStore;
use openrt_runtime::Runtime;

fn allow_rule(cap: &str) -> RuleConfig {
    RuleConfig {
        effect: RuleEffect::Allow,
        capability: cap.to_string(),
        resource: None,
    }
}

fn write_allowed_config() -> Config {
    let mut c = Config::default();
    c.permissions = PermissionsConfig {
        rules: vec![
            allow_rule("echo"),
            allow_rule("system.info"),
            allow_rule("system.info.all"),
            allow_rule("filesystem.read"),
            allow_rule("filesystem.read.all"),
            allow_rule("filesystem.write"),
            allow_rule("filesystem.write.all"),
            allow_rule("fs.read"),
            allow_rule("tool.run"),
            allow_rule("tool.other"),
        ],
    };
    c
}

async fn build_with(config: Config) -> Arc<Runtime> {
    openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(openrt_models::mock::MockProvider::echo("mock")))
        .build()
        .await
        .unwrap()
}

// Alias so call sites read naturally at the 6B entry points.
async fn audit_append(
    rt: &Runtime,
    actor: &str,
    action: &str,
    subject: &str,
    result: &str,
    ext: serde_json::Value,
) {
    rt.audit(actor, action, subject, result, ext).await.unwrap();
}

// ---------------------------------------------------------------------------
// M6.2: versioned policy document
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_document_bootstraps_denies_by_default_and_versions() {
    let rt = build_with(write_allowed_config()).await;

    // Uninitialized store: load fails closed (doctor will bootstrap).
    assert!(rt.policy_document().await.is_err());

    let doc = rt.policy_document_ensure().await.unwrap();
    assert_eq!(doc.version, 1);
    assert!(
        doc.rules.is_empty(),
        "bootstrap is deny-by-default empty rules"
    );

    let doc2 = rt
        .policy_append_rule(
            "operator",
            "allow echo for demos",
            "echo",
            openrt_permissions::RuleAction::Allow,
            None,
        )
        .await
        .unwrap();
    assert_eq!(doc2.version, 2);
    assert_eq!(doc2.rules.len(), 1);
    assert_eq!(doc2.rules[0].pattern, "echo");

    let versions = rt.policy_versions().await.unwrap();
    assert_eq!(versions, vec![1, 2], "prior revisions remain readable");

    // Loaded current matches last append.
    let cur = rt.policy_document().await.unwrap();
    assert_eq!(cur.version, 2);
    assert_eq!(cur.to_policy().rules.len(), 1);

    // Audit recorded the append.
    let rows = rt
        .audit_query(&openrt_audit::AuditQuery::new().with_action_prefix("policy.append"))
        .await
        .unwrap();
    assert!(!rows.is_empty(), "policy.append must be audited");
}

// ---------------------------------------------------------------------------
// M6.2: audit verify / query / retention
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_verify_query_retention_through_runtime() {
    let rt = build_with(write_allowed_config()).await;

    audit_append(
        &rt,
        "operator",
        "test.action",
        "subject-1",
        "ok",
        serde_json::json!({}),
    )
    .await;
    audit_append(
        &rt,
        "operator",
        "test.action",
        "subject-2",
        "ok",
        serde_json::json!({}),
    )
    .await;
    audit_append(
        &rt,
        "operator",
        "other.action",
        "subject-3",
        "denied",
        serde_json::json!({}),
    )
    .await;

    let n = rt.audit_verify().await.unwrap();
    assert!(n >= 3, "chain must verify after appends (got {n})");

    let rows = rt
        .audit_query(&openrt_audit::AuditQuery::new().with_action_prefix("test."))
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "action prefix filter");

    let denied = rt
        .audit_query(&openrt_audit::AuditQuery::new().with_result("denied"))
        .await
        .unwrap();
    assert!(!denied.is_empty());

    let st = rt.audit_retention().await.unwrap();
    assert_eq!(st.chain_mode, "chained");
    assert!(st.total >= 3);
    // head = newest still present; tail = oldest still present (seq starts at 1).
    assert!(st.head_seq >= st.tail_seq);
}

// ---------------------------------------------------------------------------
// M6.3: secrets never land as plaintext in the store
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_are_encrypted_at_rest() {
    let rt = build_with(write_allowed_config()).await;
    // Use an explicit key (not the home keyfile) so the test is hermetic.
    let key = openrt_secrets::generate_key_bytes();
    let store = openrt_secrets::SecretStore::with_key(rt.store.clone(), key).unwrap();

    let secret = b"super-secret-broker-password-xyz";
    store.put("broker.password", secret).await.unwrap();

    // Read back decrypts.
    let got = store.get("broker.password").await.unwrap();
    assert_eq!(got, secret);

    // Raw store bytes must NOT contain the plaintext.
    let raw = rt
        .store
        .get_record(openrt_secrets::SECRETS_NS, "broker.password")
        .await
        .unwrap()
        .expect("sealed record present");
    let blob = serde_json::to_string(&raw).unwrap();
    assert!(
        !blob.as_bytes().windows(secret.len()).any(|w| w == secret),
        "plaintext secret must never appear in the sealed record"
    );
    assert!(blob.contains("ciphertext_b64"));
    assert!(blob.contains("chacha20-poly1305"));

    // list / contains / delete production paths.
    assert!(store.contains("broker.password").await.unwrap());
    let listed = store.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    store.delete("broker.password").await.unwrap();
    assert!(!store.contains("broker.password").await.unwrap());
}

// ---------------------------------------------------------------------------
// M6.3: netpol default-deny + Sovereign fail-closed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn netpol_sovereign_denies_then_grant_allows_in_provisioning() {
    let mut cfg = write_allowed_config();
    cfg.mode = OperatingMode::Sovereign;
    let rt = build_with(cfg).await;
    let netpol = rt.netpol();

    // Sovereign: deny regardless of grants.
    let g = netpol
        .grant(
            "operator",
            "default",
            vec!["api.internal".into()],
            "test grant",
        )
        .await
        .unwrap();
    assert_eq!(g.scope, "default");

    let d = netpol
        .ensure_egress("default", "api.internal")
        .await
        .unwrap();
    match d {
        openrt_netpol::EgressDecision::Deny { reason, .. } => {
            assert!(reason.contains("sovereign"), "got: {reason}");
        }
        other => panic!("Sovereign must deny, got {other:?}"),
    }

    // Transition to Provisioning: grant now allows matching dest, other denied.
    rt.modes
        .transition(
            "operator",
            openrt_modes::ExecutionMode::Provisioning,
            "test",
        )
        .await
        .unwrap();

    let allow = netpol
        .ensure_egress("default", "api.internal")
        .await
        .unwrap();
    assert!(matches!(allow, openrt_netpol::EgressDecision::Allow { .. }));

    let deny_other = netpol
        .ensure_egress("default", "evil.example.com")
        .await
        .unwrap();
    // default-deny without a matching grant.
    assert!(matches!(
        deny_other,
        openrt_netpol::EgressDecision::Deny { .. }
    ));

    // No grant for unknown scope → deny even in Provisioning.
    let no_grant = netpol
        .ensure_egress("other-scope", "api.internal")
        .await
        .unwrap();
    assert!(matches!(
        no_grant,
        openrt_netpol::EgressDecision::Deny { .. }
    ));

    let listed = netpol.list().await.unwrap();
    assert_eq!(listed.len(), 1);

    netpol.revoke("operator", "default").await.unwrap();
    assert!(netpol.grant_for("default").await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// M6.4: enclave enroll / quarantine / authenticate fail-closed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enclave_enroll_quarantine_authenticate_fail_closed() {
    let rt = build_with(write_allowed_config()).await;
    let enclave = rt.enclave();

    let fp = "a".repeat(64);
    let node = enclave
        .enroll("operator", "worker-1", &fp, vec!["python.run".into()])
        .await
        .unwrap();
    assert_eq!(node.node_id, "worker-1");
    assert_eq!(node.status, openrt_enclave::NodeStatus::Active);

    // Correct fingerprint authenticates.
    let auth = enclave.authenticate("worker-1", &fp).await.unwrap();
    assert_eq!(auth.identity_fp, fp);

    // Mismatch → IdentityMismatch + automatic quarantine.
    let bad = enclave.authenticate("worker-1", &"b".repeat(64)).await;
    assert!(matches!(
        bad,
        Err(openrt_enclave::EnclaveError::IdentityMismatch(_))
    ));

    // After spoof attempt the node is quarantined; further auth fails closed.
    let nodes = enclave.list_nodes().await.unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].status, openrt_enclave::NodeStatus::Quarantined);

    // Operator quarantine of a second node + clear path.
    let node2 = enclave
        .enroll("operator", "worker-2", &"c".repeat(64), vec![])
        .await
        .unwrap();
    assert_eq!(node2.status, openrt_enclave::NodeStatus::Active);
    enclave
        .quarantine("operator", "worker-2", "anomaly")
        .await
        .unwrap();
    let auth2 = enclave.authenticate("worker-2", &"c".repeat(64)).await;
    assert!(matches!(
        auth2,
        Err(openrt_enclave::EnclaveError::Quarantined(_, _))
    ));
    enclave
        .clear_quarantine("operator", "worker-2")
        .await
        .unwrap();
    assert!(enclave
        .authenticate("worker-2", &"c".repeat(64))
        .await
        .is_ok());

    // Non-enclave contact is always refused (ADR-004).
    assert!(matches!(
        enclave.ensure_enclave_contact().await,
        Err(openrt_enclave::EnclaveError::NonEnclaveContact)
    ));

    // Unknown node → fail closed.
    assert!(matches!(
        enclave.authenticate("nope", &fp).await,
        Err(openrt_enclave::EnclaveError::NodeNotFound(_))
    ));

    let counts = enclave.status_counts().await.unwrap();
    assert!(counts.values().sum::<usize>() >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enclave_migrate_requires_active_nodes_and_valid_sha() {
    let rt = build_with(write_allowed_config()).await;
    let enclave = rt.enclave();

    enclave
        .enroll("operator", "n1", &"a".repeat(64), vec![])
        .await
        .unwrap();
    enclave
        .enroll("operator", "n2", &"b".repeat(64), vec![])
        .await
        .unwrap();

    // Invalid sha → fail closed.
    let bad = enclave
        .migrate_model("operator", "model-1", "not-hex", "n1", "n2", None)
        .await;
    assert!(matches!(bad, Err(openrt_enclave::EnclaveError::Invalid(_))));

    // Missing node → fail closed.
    let missing = enclave
        .migrate_model("operator", "model-1", &"d".repeat(64), "n1", "ghost", None)
        .await;
    assert!(matches!(
        missing,
        Err(openrt_enclave::EnclaveError::NodeNotFound(_))
    ));

    // Quarantined source → fail closed.
    enclave.quarantine("operator", "n1", "hold").await.unwrap();
    let q = enclave
        .migrate_model("operator", "model-1", &"d".repeat(64), "n1", "n2", None)
        .await;
    assert!(matches!(
        q,
        Err(openrt_enclave::EnclaveError::Quarantined(_, _))
    ));

    // Happy path: clear quarantine, valid 64-hex sha, no registry manifest
    // for this artifact id → transfer recorded as verified (content-address
    // only checked when a manifest exists).
    enclave.clear_quarantine("operator", "n1").await.unwrap();
    let sha = "e".repeat(64);
    let rec = enclave
        .migrate_model(
            "operator",
            "artifact-absent-from-registry",
            &sha,
            "n1",
            "n2",
            Some("prov".into()),
        )
        .await
        .unwrap();
    assert!(rec.verified);
    assert_eq!(rec.sha256, sha);
    let transfers = enclave.list_transfers().await.unwrap();
    assert_eq!(transfers.len(), 1);
}

// ---------------------------------------------------------------------------
// M6.4: observability health / retention / export blocked in Sovereign
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observability_health_retention_export_sovereign_blocked() {
    let mut cfg = write_allowed_config();
    cfg.mode = OperatingMode::Sovereign;
    let rt = build_with(cfg).await;
    let obs = rt.observability();

    // Health computes from existing events (no parallel counters).
    let health = obs.health().await.unwrap();
    assert!(health.generated_at.len() > 0);
    // Fresh runtime may be healthy; just ensure the path runs.
    let _ = health.healthy;

    // Retention set/get round-trip.
    obs.set_retention(openrt_observability::RetentionPolicy {
        max_age_secs: 3600,
        max_events: 100,
        updated_at: String::new(),
    })
    .await
    .unwrap();
    let pol = obs.retention().await.unwrap();
    assert_eq!(pol.max_age_secs, 3600);
    assert_eq!(pol.max_events, 100);

    // apply_retention_cutoff records a status row (no rewrite of audit).
    let _removed = obs.apply_retention_cutoff().await.unwrap();

    // Sovereign: export queue refused (ADR-005 egress opt-in).
    let blocked = obs
        .queue_export("https://example.invalid/otlp", serde_json::json!({"n": 1}))
        .await;
    assert!(matches!(
        blocked,
        Err(openrt_observability::ObsError::ExportBlocked)
    ));
    assert!(obs.ensure_export_allowed().await.is_err());
    let exports = obs.list_exports().await.unwrap();
    assert!(exports.is_empty());

    // Provisioning: export queues (destination grant still required at send).
    rt.modes
        .transition(
            "operator",
            openrt_modes::ExecutionMode::Provisioning,
            "test",
        )
        .await
        .unwrap();
    let batch = obs
        .queue_export("https://example.invalid/otlp", serde_json::json!({"n": 1}))
        .await
        .unwrap();
    assert!(!batch.exported);
    let exports = obs.list_exports().await.unwrap();
    assert_eq!(exports.len(), 1);
    obs.mark_exported(&batch.id).await.unwrap();
    let exports = obs.list_exports().await.unwrap();
    assert!(exports[0].exported);
}

// ---------------------------------------------------------------------------
// M6.1 remainder: supervisor create / lease / takeover / stop / list
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_create_lease_takeover_stop_list() {
    let rt = build_with(write_allowed_config()).await;

    let sup_a = rt.supervisor("holder-a");
    let run = sup_a
        .create_run(
            "wf-1",
            "sess-1",
            Some("task-1".into()),
            None,
            openrt_supervisor::StopConditions {
                max_wall_secs: 60,
                max_budget: 10,
                max_errors: 3,
                stop_on_complete: true,
            },
            openrt_supervisor::ApprovalMat::default(),
        )
        .await
        .unwrap();
    assert_eq!(run.workflow_id, "wf-1");
    assert_eq!(run.state, openrt_supervisor::RunState::Created);

    // Invalid create fails closed.
    let bad = sup_a
        .create_run(
            "",
            "sess",
            None,
            None,
            Default::default(),
            Default::default(),
        )
        .await;
    assert!(matches!(
        bad,
        Err(openrt_supervisor::SupervisorError::Invalid(_))
    ));

    // Lease acquisition flips Created → Running.
    let lease = sup_a.acquire_lease(&run.run_id, 300).await.unwrap();
    assert_eq!(lease.holder, "holder-a");
    assert_eq!(lease.generation, 1);
    let after = sup_a.load_run(&run.run_id).await.unwrap();
    assert_eq!(after.state, openrt_supervisor::RunState::Running);

    // Second holder cannot steal an unexpired lease.
    let sup_b = rt.supervisor("holder-b");
    let steal = sup_b.acquire_lease(&run.run_id, 300).await;
    assert!(matches!(
        steal,
        Err(openrt_supervisor::SupervisorError::LeaseHeld { .. })
    ));

    // Release then takeover by B bumps generation (no double execution).
    sup_a.release_lease(&run.run_id).await.unwrap();
    let taken = sup_b.takeover(&run.run_id).await.unwrap();
    assert_eq!(taken.generation, 2);
    assert_eq!(taken.state, openrt_supervisor::RunState::Running);

    // Stop conditions: budget exceeded → Failed + reason.
    let reason = sup_b
        .check_stops(&run.run_id, 0, 10, 0, false)
        .await
        .unwrap();
    assert_eq!(reason, Some(openrt_supervisor::StopReason::BudgetExceeded));
    let stopped = sup_b.load_run(&run.run_id).await.unwrap();
    assert_eq!(stopped.state, openrt_supervisor::RunState::Failed);

    // Operator stop on a fresh run.
    let run2 = sup_a
        .create_run(
            "wf-2",
            "sess-2",
            None,
            None,
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap();
    let stopped2 = sup_a
        .operator_stop(&run2.run_id, "operator halt")
        .await
        .unwrap();
    assert_eq!(stopped2.state, openrt_supervisor::RunState::Stopped);
    assert_eq!(
        stopped2.stop_reason,
        Some(openrt_supervisor::StopReason::OperatorStop)
    );

    // check_stops on a stopped run fails closed.
    let poll = sup_a.check_stops(&run2.run_id, 0, 0, 0, false).await;
    assert!(matches!(
        poll,
        Err(openrt_supervisor::SupervisorError::NotRunning(_))
    ));

    // list_runs returns both.
    let runs = sup_a.list_runs().await.unwrap();
    assert_eq!(runs.len(), 2);

    // Approval mat: always_deny blocks unattended capability.
    let mut mat = openrt_supervisor::ApprovalMat::default();
    mat.always_deny.push("trading.order.place".into());
    let run3 = sup_a
        .create_run("wf-3", "sess-3", None, None, Default::default(), mat)
        .await
        .unwrap();
    assert!(openrt_supervisor::Supervisor::check_mat(&run3, "trading.order.place").is_err());
    // Resume identity preserves workflow/session (ADR-008 / M6.1 DoD).
    let id = openrt_supervisor::Supervisor::resume_identity(&run3);
    assert_eq!(id.workflow_id, "wf-3");
    assert_eq!(id.session_id, "sess-3");
}

// ---------------------------------------------------------------------------
// Doctor: audit / air-gap / policy-doc checks present and passing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_includes_audit_airgap_policy_doc_checks() {
    let mut cfg = write_allowed_config();
    cfg.mode = OperatingMode::Sovereign;
    let rt = build_with(cfg).await;

    // Produce some audited activity first.
    audit_append(
        &rt,
        "operator",
        "test.action",
        "s",
        "ok",
        serde_json::json!({}),
    )
    .await;

    let report = rt.doctor().await;
    let find = |name: &str| {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("doctor must include `{name}`; got {:?}", report.checks))
    };

    let audit = find("audit");
    assert_eq!(
        audit.status,
        openrt_runtime::DoctorStatus::Pass,
        "audit: {}",
        audit.detail
    );

    let airgap = find("air-gap");
    assert_eq!(
        airgap.status,
        openrt_runtime::DoctorStatus::Pass,
        "air-gap in Sovereign must pass: {}",
        airgap.detail
    );

    let policy_doc = find("policy-doc");
    assert_eq!(
        policy_doc.status,
        openrt_runtime::DoctorStatus::Pass,
        "policy-doc: {}",
        policy_doc.detail
    );
    assert!(
        policy_doc.detail.contains("policy.engine") || policy_doc.detail.contains("bootstrapped"),
        "{}",
        policy_doc.detail
    );
}

// ---------------------------------------------------------------------------
// Cross-cutting: production façade exposes all six 6B domains
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_exposes_all_6b_production_entry_points() {
    let rt = build_with(write_allowed_config()).await;

    // Methods resolve (compile-time surface check exercised at runtime).
    let _ = rt.audit_verify().await.unwrap();
    let _ = rt
        .audit_query(&openrt_audit::AuditQuery::new())
        .await
        .unwrap();
    let _ = rt.audit_retention().await.unwrap();
    let _ = rt.policy_document_ensure().await.unwrap();
    let _ = rt.policy_versions().await.unwrap();
    let _ = rt.netpol();
    let _ = rt.enclave();
    let _ = rt.observability();
    let _ = rt.supervisor("holder");

    // Secrets surface via production open (uses default keyfile path —
    // hermetic because OPENRT_HOME may be unset and HOME is writable in CI;
    // the encrypted-at-rest property is asserted in the dedicated test above).
    // Skip opening here to avoid touching $HOME; the type is still reachable
    // through openrt_secrets::SecretStore::with_key over rt.store.
    let key = openrt_secrets::generate_key_bytes();
    let secrets = openrt_secrets::SecretStore::with_key(rt.store.clone(), key).unwrap();
    assert!(!secrets.key_id().is_empty());
}
