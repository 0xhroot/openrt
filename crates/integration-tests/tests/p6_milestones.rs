//! Phase 6 brief-milestone integration: M6.2 decomposition, M6.3 capability
//! planning negatives, M6.4 bounded replan provenance, M6.5 fine-grained
//! scheduling evidence, M6.6 final production gate.
//!
//! All callers go through the production Runtime / WorkflowCompiler entry
//! points (never a test-only shim).

use std::sync::Arc;

use openrt_config::{Config, PermissionsConfig, RuleConfig, RuleEffect};
use openrt_orch::WorkflowStatus;
use openrt_persistence::MemoryStore;
use openrt_resources::{ResourceGate, ResourceLimits};

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

async fn build_with(config: Config) -> Arc<openrt_runtime::Runtime> {
    openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(openrt_models::mock::MockProvider::echo("mock")))
        .build()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// M6.2: five decomposition cases through the production compile path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m62_sequential_goal_decomposes_to_ordered_dag() {
    let rt = build_with(write_allowed_config()).await;
    let out = rt
        .compile_workflow(
            "Read report.txt then create a summary report",
            openrt_compiler::CompileMode::Validate,
        )
        .await
        .unwrap();
    let order: Vec<String> = out
        .workflow
        .topological_sort()
        .unwrap()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    assert!(order.len() >= 2, "sequential goal needs multiple steps");
    // Every dependency edge must respect topological order (ordered, not flat).
    let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
    for s in &out.workflow.steps {
        for d in &s.depends_on {
            assert!(
                pos(d) < pos(&s.id),
                "{d} must precede {} in topological order",
                s.id
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m62_parallel_inputs_create_independent_branches() {
    let rt = build_with(write_allowed_config()).await;
    let out = rt
        .compile_workflow(
            "Analyze report1.txt and report2.txt independently, then create a verified summary report",
            openrt_compiler::CompileMode::Validate,
        )
        .await
        .unwrap();
    let branch_ids: Vec<&str> = out
        .workflow
        .steps
        .iter()
        .map(|s| s.id.as_str())
        .filter(|id| id.starts_with("analyze."))
        .collect();
    assert!(
        branch_ids.len() >= 2,
        "expected independent branches, got {branch_ids:?}"
    );
    for id in &branch_ids {
        let s = out.workflow.find_step(id).unwrap();
        for other in &branch_ids {
            if other != id {
                assert!(
                    !s.depends_on.iter().any(|d| d == other),
                    "{id} must not depend on sibling {other}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m62_ambiguous_goal_fails_closed() {
    let rt = build_with(write_allowed_config()).await;
    let err = rt
        .compile_workflow(
            "Create a file or delete the old file",
            openrt_compiler::CompileMode::Compile,
        )
        .await
        .expect_err("unsequenced create+delete must fail closed");
    match err {
        openrt_runtime::Error::Compile(diags) => {
            assert!(
                diags
                    .iter()
                    .any(|d| d.code == openrt_compiler::DiagnosticCode::AmbiguousGoal),
                "expected AmbiguousGoal, got {diags:?}"
            );
            assert!(diags.iter().all(|d| !d.how_to_fix.is_empty()));
        }
        other => panic!("expected Compile error, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m62_unsupported_goal_fails_closed() {
    let rt = build_with(write_allowed_config()).await;
    let err = rt
        .compile_workflow(
            "purple monkey dishwasher",
            openrt_compiler::CompileMode::Compile,
        )
        .await
        .expect_err("no actionable signal must fail closed");
    match err {
        openrt_runtime::Error::Compile(diags) => {
            assert!(
                diags.iter().any(
                    |d| d.code == openrt_compiler::DiagnosticCode::UnsupportedGoal
                        || d.code == openrt_compiler::DiagnosticCode::EmptyGoal
                ),
                "expected Unsupported/Empty, got {diags:?}"
            );
        }
        other => panic!("expected Compile error, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m62_distinct_goals_produce_distinct_decompositions() {
    let rt = build_with(write_allowed_config()).await;
    let a = rt
        .compile_workflow(
            "Review the workspace files and confirm the project status",
            openrt_compiler::CompileMode::Validate,
        )
        .await
        .unwrap();
    let b = rt
        .compile_workflow(
            "Analyze report1.txt and report2.txt independently then combine findings",
            openrt_compiler::CompileMode::Validate,
        )
        .await
        .unwrap();
    let c = rt
        .compile_workflow(
            "Create a file containing the current project status",
            openrt_compiler::CompileMode::Validate,
        )
        .await
        .unwrap();
    let ids = |o: &openrt_compiler::CompileOutput| {
        let mut v: Vec<String> = o.workflow.steps.iter().map(|s| s.id.clone()).collect();
        v.sort();
        v.join(",")
    };
    let (ia, ib, ic) = (ids(&a), ids(&b), ids(&c));
    assert_ne!(ia, ib);
    assert_ne!(ib, ic);
    assert_ne!(ia, ic);
}

// ---------------------------------------------------------------------------
// M6.3: capability planning — five negative cases fail closed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m63_negative_empty_goal_fails_closed() {
    let rt = build_with(write_allowed_config()).await;
    let err = rt
        .compile_workflow("   ", openrt_compiler::CompileMode::Compile)
        .await
        .expect_err("empty goal fails closed");
    match err {
        openrt_runtime::Error::Compile(diags) => {
            assert!(diags
                .iter()
                .any(|d| d.code == openrt_compiler::DiagnosticCode::EmptyGoal));
        }
        other => panic!("expected Compile error, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m63_negative_permission_denied_fails_closed() {
    // Default permissions: filesystem.write NOT allowed.
    let rt = build_with(Config::default()).await;
    let err = rt
        .compile_workflow(
            "Create a file containing the current project status report",
            openrt_compiler::CompileMode::Compile,
        )
        .await
        .expect_err("missing write allow rule fails closed");
    match err {
        openrt_runtime::Error::Compile(diags) => {
            assert!(diags.iter().any(|d| {
                matches!(
                    d.code,
                    openrt_compiler::DiagnosticCode::PermissionDenied
                        | openrt_compiler::DiagnosticCode::MissingCapability
                )
            }));
        }
        other => panic!("expected Compile error, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m63_negative_resource_denied_fails_closed() {
    let mut rt = build_with(write_allowed_config()).await;
    let mut si = openrt_resources::system::SystemInfo::collect();
    if si.mem.total_kb == 0 {
        si.mem.total_kb = 1000;
    }
    si.mem.available_kb = 1;
    Arc::get_mut(&mut rt)
        .expect("runtime uniquely owned in this test")
        .resource_gate = Arc::new(ResourceGate::new(ResourceLimits::default(), si));
    let err = rt
        .compile_workflow(
            "Review the workspace files and confirm the project status",
            openrt_compiler::CompileMode::Compile,
        )
        .await
        .expect_err("resource denial fails closed");
    match err {
        openrt_runtime::Error::Compile(diags) => {
            assert!(diags.iter().any(|d| {
                d.where_.contains("resource") || d.what.to_lowercase().contains("resource")
            }));
        }
        other => panic!("expected Compile error, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m63_negative_sovereign_blocks_network_goal() {
    let rt = build_with(write_allowed_config()).await;
    let mode = rt.modes.current().await.unwrap().mode;
    assert!(!mode.network_allowed(), "fresh store must start Sovereign");
    let err = rt
        .compile_workflow(
            "Download the latest status page from https://example.com/report",
            openrt_compiler::CompileMode::Compile,
        )
        .await
        .expect_err("network goal in Sovereign fails closed");
    match err {
        openrt_runtime::Error::Compile(diags) => {
            assert!(diags.iter().any(|d| {
                d.code == openrt_compiler::DiagnosticCode::SovereignConflict
                    || d.where_.contains("sovereign")
                    || d.why.to_lowercase().contains("network")
            }));
        }
        other => panic!("expected Compile error, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m63_negative_unavailable_model_fails_closed() {
    use openrt_compiler::{CompileInput, NaturalLanguageGoal, WorkflowCompiler};
    let input = CompileInput::new(NaturalLanguageGoal::new(
        "Analyze the project status and summarize findings",
    ))
    .with_tools(vec!["echo".to_string(), "filesystem.read".to_string()])
    .with_policy(Arc::new(|_| true))
    .with_network(true)
    .with_model_available(false);
    let err = WorkflowCompiler::new()
        .compile(&input, None)
        .await
        .expect_err("unavailable model fails closed");
    assert!(err.iter().any(|d| {
        matches!(
            d.code,
            openrt_compiler::DiagnosticCode::UnavailableModel
                | openrt_compiler::DiagnosticCode::MissingCapability
        ) || d.what.to_lowercase().contains("model")
            || d.why.to_lowercase().contains("model")
    }));
    assert!(err.iter().all(|d| !d.how_to_fix.is_empty()));
}

// ---------------------------------------------------------------------------
// M6.4: bounded replanning preserves original + correction + final + provenance
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m64_llm_capability_gap_replans_with_full_provenance() {
    use openrt_compiler::{CompileInput, CompileMode, NaturalLanguageGoal, WorkflowCompiler};
    let input = CompileInput::new(NaturalLanguageGoal::new("Analyze the project status"))
        .with_tools(vec!["filesystem.read".to_string(), "echo".to_string()])
        .with_policy(Arc::new(|_| true))
        .with_network(true)
        .with_model_available(true)
        .with_llm_plan(Some(
            r#"{"subgoals":[
            {"id":"shell","objective":"Run shell commands to inspect the system"},
            {"id":"analyze","objective":"Analyze findings","depends_on":["shell"]}
        ]}"#
            .into(),
        ))
        .with_mode(CompileMode::Execute);

    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    let out = WorkflowCompiler::new()
        .compile(&input, Some(&store))
        .await
        .expect("replan must succeed");

    let rep = out.replan.expect("ReplanRecord must be present");
    assert_eq!(rep.attempts, 1);
    assert!(
        rep.correction_reason.contains("failed build")
            || rep.correction_reason.contains("failed validation"),
        "reason: {}",
        rep.correction_reason
    );
    assert!(rep.original.find("shell").is_some(), "original preserved");
    assert!(
        rep.final_decomp.find("shell").is_none(),
        "final is deterministic fallback"
    );
    assert!(out.workflow.find_step("shell").is_none());
    let v = out.validation.expect("validated");
    assert!(v.is_valid(), "failures: {:?}", v.failures());
    // Provenance still ties to the original NL request.
    assert!(out.provenance.original_request.contains("project status"));
    assert!(!out.workflow.steps.is_empty());
    // Persisted with replan trail.
    let rec = openrt_compiler::load_compiled(&store, &out.workflow_id)
        .await
        .unwrap()
        .expect("persisted");
    let prec = rec.replan.expect("persisted ReplanRecord");
    assert_eq!(prec.attempts, 1);
    assert!(prec.original.find("shell").is_some());
    assert!(prec.final_decomp.find("shell").is_none());
    assert_eq!(rec.goal.original_request, "Analyze the project status");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m64_max_replan_zero_fails_closed() {
    use openrt_compiler::{CompileInput, NaturalLanguageGoal, WorkflowCompiler};
    let input = CompileInput::new(NaturalLanguageGoal::new("Analyze the project status"))
        .with_tools(vec!["filesystem.read".to_string()])
        .with_policy(Arc::new(|_| true))
        .with_network(true)
        .with_model_available(true)
        .with_llm_plan(Some(
            r#"{"subgoals":[
            {"id":"shell","objective":"Run shell commands to inspect the system"},
            {"id":"analyze","objective":"Analyze findings","depends_on":["shell"]}
        ]}"#
            .into(),
        ))
        .with_max_replan(0);
    let err = WorkflowCompiler::new()
        .compile(&input, None)
        .await
        .expect_err("max_replan=0 must fail closed, not silently accept");
    assert!(err.iter().any(|d| d.is_error()));
}

// ---------------------------------------------------------------------------
// M6.5: fine-grained scheduling evidence through the production run path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m65_fine_grained_scheduler_runs_diamond_and_joins() {
    let rt = build_with(write_allowed_config()).await;
    let session = rt.create_session("m65").await.unwrap();
    let goal =
        "Analyze report1.txt and report2.txt independently, then create a verified summary report";
    let (out, report) = rt
        .compile_and_run(session.id, goal)
        .await
        .expect("diamond workflow must compile and run");
    assert_eq!(report.status, WorkflowStatus::Completed);
    assert_eq!(report.steps.len(), out.workflow.steps.len());
    // Every node reported (join waited for all).
    for s in &out.workflow.steps {
        assert!(
            report.steps.iter().any(|r| r.step == s.id),
            "missing run report for step {}",
            s.id
        );
    }
    // Scheduler bounded concurrency remains the engine's single authority
    // (unit-level timing proof: openrt-orch diamond + max_concurrency tests).
    // Here we prove the production path produces a runnable parallel graph.
    let branches: Vec<_> = out
        .workflow
        .steps
        .iter()
        .filter(|s| s.id.starts_with("analyze."))
        .collect();
    assert!(branches.len() >= 2);
    for b in &branches {
        assert!(b.depends_on.iter().all(|d| !d.starts_with("analyze.")));
    }
}

// ---------------------------------------------------------------------------
// M6.6: final production gate — restart recovery, security, invalid plan,
// cross-surface visibility
// ---------------------------------------------------------------------------

fn temp_dir() -> std::path::PathBuf {
    let uniq = uuid::Uuid::new_v4();
    std::env::temp_dir().join(format!("openrt-p6-gate-{uniq}"))
}

async fn runtime_on(dir: &std::path::Path, config: Config) -> Arc<openrt_runtime::Runtime> {
    openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(
            openrt_persistence::LocalFileStore::open(dir).await.unwrap(),
        ))
        .with_provider(Arc::new(openrt_models::mock::MockProvider::echo("mock")))
        .build()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m66_final_gate_full_e2e_with_restart_recovery() {
    let dir = temp_dir();
    let goal = "Review the workspace files and confirm the project status";
    let workflow_id;

    // Incarnation 1: NL → compile → persist → execute.
    {
        let rt = runtime_on(&dir, write_allowed_config()).await;
        let session = rt.create_session("m66").await.unwrap();
        let (out, report) = rt
            .compile_and_run(session.id, goal)
            .await
            .expect("full e2e must succeed");
        assert_eq!(report.status, WorkflowStatus::Completed);
        assert!(out.persisted);
        workflow_id = out.workflow_id.clone();

        // Integrity markers on the persisted record.
        let rec = openrt_compiler::load_compiled(&rt.store, &workflow_id)
            .await
            .unwrap()
            .expect("compiled record present");
        assert_eq!(rec.compiler_version, openrt_compiler::COMPILER_VERSION);
        assert_eq!(rec.goal.original_request, goal);
        let v = rec.validation.as_ref().expect("validation stored");
        assert!(v.is_valid(), "failures: {:?}", v.failures());
        // Goal id is a deterministic digest of the request (integrity linkage).
        assert_eq!(rec.goal.id, openrt_compiler::GoalSpec::id_for(goal));
        // DAG graph present alongside the compiled record.
        assert!(openrt_compiler::load_dag_workflow(&rt.store, &workflow_id)
            .await
            .unwrap()
            .is_some());
        // Run report present.
        assert!(rt
            .store
            .get_record("runs", &workflow_id)
            .await
            .unwrap()
            .is_some());
    }

    // Incarnation 2: reopen same directory, load + re-execute without recompile.
    {
        let rt = runtime_on(&dir, write_allowed_config()).await;
        let (rec, wf) = rt.load_compiled_workflow(&workflow_id).await.unwrap();
        assert_eq!(rec.goal.original_request, goal);
        assert_eq!(rec.compiler_version, openrt_compiler::COMPILER_VERSION);
        assert!(rec.validation.as_ref().unwrap().is_valid());
        assert_eq!(wf.id, workflow_id);

        let session = rt.create_session("m66-restart").await.unwrap();
        let report = rt.run_workflow(session.id, wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);
        assert_eq!(report.steps.len(), rec.decomposition.subgoals.len());
    }

    // Cross-surface visibility: list_compiled on the reopened store (the same
    // method CLI `workflow list`, TUI, and WebUI palette dispatch to).
    {
        let rt = runtime_on(&dir, write_allowed_config()).await;
        let list = openrt_compiler::list_compiled(&rt.store).await.unwrap();
        assert!(
            list.iter().any(|r| r.workflow_id == workflow_id),
            "compiled workflow must be visible after restart"
        );
        let missing = rt.load_compiled_workflow("wf.nope").await;
        assert!(missing.is_err(), "unknown id fails closed");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m66_security_negatives_fail_closed() {
    let rt = build_with(write_allowed_config()).await;

    // Empty goal.
    let e1 = rt
        .compile_workflow("   ", openrt_compiler::CompileMode::Compile)
        .await
        .expect_err("empty");
    assert!(matches!(e1, openrt_runtime::Error::Compile(_)));

    // Unsupported / no actionable.
    let e2 = rt
        .compile_workflow("xyzzy plugh", openrt_compiler::CompileMode::Compile)
        .await
        .expect_err("unsupported");
    assert!(matches!(e2, openrt_runtime::Error::Compile(_)));

    // Sovereign network block.
    let e3 = rt
        .compile_workflow(
            "Download the latest status page from https://example.com/report",
            openrt_compiler::CompileMode::Compile,
        )
        .await
        .expect_err("network");
    assert!(matches!(e3, openrt_runtime::Error::Compile(_)));

    // Permission (write not allowed under default) via a second runtime.
    let rt2 = build_with(Config::default()).await;
    let e4 = rt2
        .compile_workflow(
            "Create a file containing the current project status report",
            openrt_compiler::CompileMode::Compile,
        )
        .await
        .expect_err("permission");
    assert!(matches!(e4, openrt_runtime::Error::Compile(_)));

    // Resource admission deny.
    let mut rt3 = build_with(write_allowed_config()).await;
    let mut si = openrt_resources::system::SystemInfo::collect();
    if si.mem.total_kb == 0 {
        si.mem.total_kb = 1000;
    }
    si.mem.available_kb = 1;
    Arc::get_mut(&mut rt3)
        .expect("runtime uniquely owned in this test")
        .resource_gate = Arc::new(ResourceGate::new(ResourceLimits::default(), si));
    let e5 = rt3
        .compile_workflow(
            "Review the workspace files and confirm the project status",
            openrt_compiler::CompileMode::Compile,
        )
        .await
        .expect_err("resource");
    assert!(matches!(e5, openrt_runtime::Error::Compile(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m66_invalid_llm_plan_rejected_on_production_path() {
    use openrt_compiler::{CompileInput, NaturalLanguageGoal, WorkflowCompiler};
    let rt = build_with(write_allowed_config()).await;

    let input = CompileInput::new(NaturalLanguageGoal::new("do anything useful"))
        .with_tools(rt.registry.names())
        .with_policy(Arc::new(|_| true))
        .with_network(true)
        .with_model_available(true)
        .with_llm_plan(Some("{not json".into()));
    let err = WorkflowCompiler::new()
        .compile(&input, Some(&rt.store))
        .await
        .expect_err("malformed LLM plan fails closed");
    assert!(err
        .iter()
        .any(|d| d.code == openrt_compiler::DiagnosticCode::InvalidLlmPlan));
    // Nothing was persisted for the rejected plan.
    let list = openrt_compiler::list_compiled(&rt.store).await.unwrap();
    assert!(
        list.iter()
            .all(|r| !r.goal.original_request.contains("do anything")),
        "invalid plan must not appear in compiled store"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m66_cross_surface_visibility_via_shared_store() {
    // CLI / TUI / WebUI all list via `list_compiled(&store)` + Runtime methods.
    let rt = build_with(write_allowed_config()).await;
    let a = rt
        .compile_workflow(
            "Review the workspace files and confirm the project status",
            openrt_compiler::CompileMode::Validate,
        )
        .await
        .unwrap();
    let b = rt
        .compile_workflow(
            "Create a file containing the current project status",
            openrt_compiler::CompileMode::Validate,
        )
        .await
        .unwrap();
    assert_ne!(a.workflow_id, b.workflow_id);

    let list = openrt_compiler::list_compiled(&rt.store).await.unwrap();
    assert!(list.len() >= 2);
    for id in [a.workflow_id.as_str(), b.workflow_id.as_str()] {
        let (rec, wf) = rt.load_compiled_workflow(id).await.unwrap();
        assert_eq!(wf.id, id);
        assert!(rec.validation.as_ref().unwrap().is_valid());
        assert_eq!(rec.compiler_version, openrt_compiler::COMPILER_VERSION);
    }
}
