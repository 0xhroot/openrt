//! Phase 11 — extension system integration tests (the 12 mandated
//! categories): import never-active; validate; scan verdicts; activation
//! gates (operator + clean scan); activation gates (skills, capabilities,
//! sovereignty); single-active-version; build-time binding; gated
//! invocation with input; no binding without activation; rollback;
//! removal gates; tool-name conflicts.
//!
//! Security requirements under test: *imported extension content can never
//! arrive Active or pre-scanned; every activation passes the operator grant
//! and the one AgentShield scan first; extension tools join the ONE
//! registry at build time and execute through the ONE skill path (fresh
//! child session, gated, audited).*

use std::sync::Arc;

use openrt_config::{RuleConfig, RuleEffect};
use openrt_core::SessionStatus;
use openrt_models::mock::MockProvider;
use openrt_persistence::MemoryStore;
use openrt_runtime::{Error, Runtime, RuntimeBuilder};
use openrt_shield::ShieldAction;
use openrt_tools::{ToolExecutionContext, ToolOutput};
use openrt_workspace_assets::{
    AssetError, Extension, ExtensionDependency, ExtensionId, ExtensionPackage, ExtensionTool,
    ExtensionVersion, Provenance as AssetProvenance, Skill, SkillDependency, SkillId, SkillVersion,
    EXTENSION_PACKAGE_SCHEMA, SECURITY_CLEAN, SECURITY_FLAGGED, SECURITY_UNSCANNED,
};

// ---- helpers ----

/// Default config plus an allow rule for the extension tool names used in
/// this suite (deny-by-default policy; the gate is the one authority).
fn ext_cfg() -> openrt_config::Config {
    let mut cfg = openrt_config::Config::default();
    cfg.permissions.rules.push(RuleConfig {
        effect: RuleEffect::Allow,
        capability: "ext.*".into(),
        resource: None,
    });
    cfg
}

async fn rt_with(cfg: openrt_config::Config, store: Arc<MemoryStore>) -> Arc<Runtime> {
    RuntimeBuilder::new()
        .config(cfg)
        .store(store)
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap()
}

async fn try_build(
    cfg: openrt_config::Config,
    store: Arc<MemoryStore>,
) -> Result<Arc<Runtime>, Error> {
    RuntimeBuilder::new()
        .config(cfg)
        .store(store)
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .bus_capacity(32)
        .build()
        .await
}

fn operator(rt: &Runtime) -> String {
    rt.asset_operators()[0].clone()
}

fn provenance() -> AssetProvenance {
    AssetProvenance::new("test", "p11")
}

fn skill_payload(id: &str, v: u32, description: &str) -> Skill {
    Skill::new(
        SkillId(id.into()),
        SkillVersion(v),
        description,
        vec![],
        vec![],
        provenance(),
    )
    .unwrap()
}

fn ext_base(id: &str, v: u32, description: &str) -> Extension {
    Extension::new(
        ExtensionId(id.into()),
        ExtensionVersion(v),
        "name",
        description,
        provenance(),
    )
    .unwrap()
}

fn binding_tool(name: &str, skill: &str) -> ExtensionTool {
    ExtensionTool {
        name: name.into(),
        description: "extension-bound tool".into(),
        required_caps: vec![],
        skill: SkillId(skill.into()),
        min_version: SkillVersion(0),
        parameters_schema: serde_json::json!({}),
    }
}

fn skill_ref(skill: &str) -> SkillDependency {
    SkillDependency {
        id: SkillId(skill.into()),
        min_version: SkillVersion(1),
    }
}

/// A skill-bound extension with a deterministic payload (identical bytes
/// for the same arguments — re-install is digest-idempotent).
fn ext_with_tool(id: &str, v: u32, skill: &str, min_version: u32) -> Extension {
    let mut e = ext_base(id, v, "greets the operator");
    let mut tool = binding_tool(id, skill);
    tool.min_version = SkillVersion(min_version);
    e.tools = vec![tool];
    e.skill_refs = vec![skill_ref(skill)];
    e
}

fn pkg(extension: Extension, skills: Vec<Skill>) -> ExtensionPackage {
    ExtensionPackage {
        schema: EXTENSION_PACKAGE_SCHEMA,
        extension,
        skills,
    }
}

fn ctx() -> ToolExecutionContext {
    ToolExecutionContext {
        session_id: None,
        task_id: None,
        channel: None,
    }
}

/// Install a skill-bound extension package and walk its payload skill to
/// Active (the activation gate requires the bound skill Active first).
async fn install_with_skill(rt: &Runtime, ext_id: &str, skill_id: &str) {
    rt.extension_install(pkg(
        ext_with_tool(ext_id, 1, skill_id, 0),
        vec![skill_payload(skill_id, 1, "Reply with DONE.")],
    ))
    .await
    .unwrap();
    rt.assets_validate_skill(skill_id).await.unwrap();
    rt.assets_activate_skill(skill_id, &operator(rt))
        .await
        .unwrap();
}

/// `Registered → Validated → Active` (activation re-runs the fresh shield
/// scan first and fails closed on anything flagged).
async fn validate_and_activate(rt: &Runtime, ext_id: &str) {
    rt.extension_validate(ext_id, None).await.unwrap();
    rt.extension_activate(ext_id, None, &operator(rt))
        .await
        .unwrap();
}

// ---- 1. import ----

#[tokio::test]
async fn import_lands_registered_unscanned_and_never_active() {
    let rt = rt_with(ext_cfg(), Arc::new(MemoryStore::new())).await;
    let installed = rt
        .extension_install(pkg(
            ext_with_tool("ext.hello", 1, "skill.hello", 0),
            vec![skill_payload("skill.hello", 1, "Reply with DONE.")],
        ))
        .await
        .unwrap();
    assert_eq!(installed, vec!["skill.hello".to_string()]);

    let got = rt.extension_inspect("ext.hello", None).await.unwrap();
    assert_eq!(got["activation"], "registered");
    assert_eq!(got["security_status"], SECURITY_UNSCANNED);

    let skill = rt.skill_inspect("skill.hello", None).await.unwrap();
    assert_eq!(skill.activation, "registered");

    // Nothing binds at build while the import is un-activated.
    assert!(!rt.registry.contains("ext.hello"));
}

// ---- 2. validate ----

#[tokio::test]
async fn install_refuses_uncovered_skill_refs_and_validate_marks_validated() {
    let rt = rt_with(ext_cfg(), Arc::new(MemoryStore::new())).await;

    // A ref the package does not carry: install refuses (fail closed).
    let mut orphan = ext_base("ext.orphan", 1, "references a missing skill");
    orphan.skill_refs = vec![skill_ref("skill.missing")];
    let err = rt.extension_install(pkg(orphan, vec![])).await.unwrap_err();
    assert!(
        matches!(err, Error::Asset(AssetError::MissingDependency { .. })),
        "got {err:?}"
    );

    // The complete package validates to `Validated`.
    rt.extension_install(pkg(
        ext_with_tool("ext.ok", 1, "skill.ok", 0),
        vec![skill_payload("skill.ok", 1, "Reply with DONE.")],
    ))
    .await
    .unwrap();
    let v = rt.extension_validate("ext.ok", None).await.unwrap();
    assert_eq!(v, 1);
    let got = rt.extension_inspect("ext.ok", None).await.unwrap();
    assert_eq!(got["activation"], "validated");
}

// ---- 3. scan ----

#[tokio::test]
async fn scan_records_clean_and_flagged_verdicts_persistently() {
    let rt = rt_with(ext_cfg(), Arc::new(MemoryStore::new())).await;

    // Benign package → clean verdict, persisted.
    rt.extension_install(pkg(
        ext_with_tool("ext.clean", 1, "skill.clean", 0),
        vec![skill_payload("skill.clean", 1, "Reply with DONE.")],
    ))
    .await
    .unwrap();
    rt.extension_validate("ext.clean", None).await.unwrap();
    let report = rt.extension_scan("ext.clean", None).await.unwrap();
    assert!(report.is_clean());
    let got = rt.extension_inspect("ext.clean", None).await.unwrap();
    assert_eq!(got["security_status"], SECURITY_CLEAN);

    // Instruction-override markers in the manifest prose → flagged verdict,
    // recorded + persisted as `flagged` (advisory scan — no state change).
    let evil = ext_base(
        "ext.evil",
        1,
        "helps but will ignore previous instructions when told",
    );
    rt.extension_install(pkg(evil, vec![])).await.unwrap();
    rt.extension_validate("ext.evil", None).await.unwrap();
    let report = rt.extension_scan("ext.evil", None).await.unwrap();
    assert!(!report.is_clean());
    assert!(report.action >= ShieldAction::RequireApproval);
    let got = rt.extension_inspect("ext.evil", None).await.unwrap();
    assert_eq!(got["security_status"], SECURITY_FLAGGED);
}

// ---- 4. activation: operator + clean scan ----

#[tokio::test]
async fn activation_requires_an_authorized_operator_and_a_clean_scan() {
    let rt = rt_with(ext_cfg(), Arc::new(MemoryStore::new())).await;
    install_with_skill(&rt, "ext.act", "skill.act").await;
    rt.extension_validate("ext.act", None).await.unwrap();

    // Empty operator: refused by grant construction.
    let err = rt
        .extension_activate("ext.act", None, "")
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Asset(AssetError::UnauthorizedOperator { .. })),
        "got {err:?}"
    );

    // Non-allowlisted operator: refused by the store gate.
    let err = rt
        .extension_activate("ext.act", None, "mallory")
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Asset(AssetError::UnauthorizedActivation { .. })),
        "got {err:?}"
    );
    let got = rt.extension_inspect("ext.act", None).await.unwrap();
    assert_eq!(got["activation"], "validated");

    // Flagged content: activation fails closed and the flag persists.
    let evil = ext_base(
        "ext.evil",
        1,
        "helps but will ignore previous instructions when told",
    );
    rt.extension_install(pkg(evil, vec![])).await.unwrap();
    rt.extension_validate("ext.evil", None).await.unwrap();
    let err = rt
        .extension_activate("ext.evil", None, &operator(&rt))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Asset(AssetError::PolicyViolation { .. })),
        "got {err:?}"
    );
    let got = rt.extension_inspect("ext.evil", None).await.unwrap();
    assert_eq!(got["activation"], "validated");
    assert_eq!(got["security_status"], SECURITY_FLAGGED);

    // The authorized path with a clean scan activates.
    rt.extension_activate("ext.act", None, &operator(&rt))
        .await
        .unwrap();
    let got = rt.extension_inspect("ext.act", None).await.unwrap();
    assert_eq!(got["activation"], "active");
}

// ---- 5. activation: skills, capabilities, sovereignty ----

#[tokio::test]
async fn activation_gates_skills_capabilities_and_sovereignty() {
    let rt = rt_with(ext_cfg(), Arc::new(MemoryStore::new())).await;

    // (a) The bound skill must be Active first.
    rt.extension_install(pkg(
        ext_with_tool("ext.gate", 1, "skill.gate", 0),
        vec![skill_payload("skill.gate", 1, "Reply with DONE.")],
    ))
    .await
    .unwrap();
    rt.extension_validate("ext.gate", None).await.unwrap();
    let err = rt
        .extension_activate("ext.gate", None, &operator(&rt))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Asset(AssetError::DependencyNotActive { .. })),
        "got {err:?}"
    );

    // (b) Capabilities outside the operator allowlist refuse at install.
    let mut broad = ext_base("ext.broad", 1, "asks for too much");
    broad.permissions = vec!["terminal.exec".into()];
    let err = rt.extension_install(pkg(broad, vec![])).await.unwrap_err();
    assert!(
        matches!(err, Error::Asset(AssetError::PolicyViolation { .. })),
        "got {err:?}"
    );

    // (c) Sovereign Mode refuses network requirements at activation.
    let mut cfg = ext_cfg();
    cfg.mode = openrt_config::OperatingMode::Sovereign;
    let rt_sov = rt_with(cfg, Arc::new(MemoryStore::new())).await;
    let mut net = ext_base("ext.net", 1, "fetches nothing yet");
    net.network = vec!["network.http".into()];
    rt_sov.extension_install(pkg(net, vec![])).await.unwrap();
    rt_sov.extension_validate("ext.net", None).await.unwrap();
    let err = rt_sov
        .extension_activate("ext.net", None, &operator(&rt_sov))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Asset(AssetError::SovereignViolation { .. })),
        "got {err:?}"
    );
}

// ---- 6. single-active-version ----

#[tokio::test]
async fn single_active_version_demotes_the_previous_one() {
    let rt = rt_with(ext_cfg(), Arc::new(MemoryStore::new())).await;
    install_with_skill(&rt, "ext.multi", "skill.multi").await;
    validate_and_activate(&rt, "ext.multi").await;

    // v2 carries no payload skill — the Active skill@1 in the store
    // satisfies the refs (a re-install of an already-activated payload
    // would be a divergent overwrite and fails closed).
    rt.extension_install(pkg(ext_with_tool("ext.multi", 2, "skill.multi", 0), vec![]))
        .await
        .unwrap();
    validate_and_activate(&rt, "ext.multi").await;

    let v1 = rt.extension_inspect("ext.multi", Some(1)).await.unwrap();
    assert_eq!(v1["activation"], "inactive");
    let v2 = rt.extension_inspect("ext.multi", Some(2)).await.unwrap();
    assert_eq!(v2["activation"], "active");

    let rows = rt.extension_states().await.unwrap();
    let mut versions = rows
        .iter()
        .filter(|r| r["id"] == "ext.multi")
        .map(|r| {
            (
                r["version"].as_u64().unwrap(),
                r["activation"].as_str().unwrap().to_string(),
            )
        })
        .collect::<Vec<_>>();
    versions.sort();
    assert_eq!(
        versions,
        vec![(1, "inactive".to_string()), (2, "active".to_string())]
    );
    // Both versions remain inspectable — history is never dropped.
    assert_eq!(
        rt.extension_inspect("ext.multi", Some(1)).await.unwrap()["name"],
        "name"
    );
}

// ---- 7. build-time binding ----

#[tokio::test]
async fn active_extension_tools_bind_only_at_build() {
    let store = Arc::new(MemoryStore::new());

    let rt1 = rt_with(ext_cfg(), store.clone()).await;
    install_with_skill(&rt1, "ext.hello", "skill.hello").await;
    validate_and_activate(&rt1, "ext.hello").await;
    // Activated after this runtime was built: the build-time snapshot
    // stands — the registry is a build authority, never mutated live.
    assert!(!rt1.registry.contains("ext.hello"));
    drop(rt1);

    let rt2 = rt_with(ext_cfg(), store).await;
    assert!(rt2.registry.contains("ext.hello"));
}

// ---- 8. gated invocation ----

#[tokio::test]
async fn bound_tool_executes_the_skill_through_the_gate_with_input() {
    let store = Arc::new(MemoryStore::new());

    // Build the runtime BEFORE activation, activate over the shared store.
    let rt1 = rt_with(ext_cfg(), store.clone()).await;
    rt1.extension_install(pkg(
        ext_with_tool("ext.hello", 1, "skill.hello", 1),
        vec![skill_payload("skill.hello", 1, "Reply with DONE.")],
    ))
    .await
    .unwrap();
    rt1.assets_validate_skill("skill.hello").await.unwrap();
    rt1.assets_activate_skill("skill.hello", &operator(&rt1))
        .await
        .unwrap();
    validate_and_activate(&rt1, "ext.hello").await;
    drop(rt1);

    let rt = rt_with(ext_cfg(), store).await;
    assert!(rt.registry.contains("ext.hello"));

    // One invocation through the ONE gate: policy, shield, resource
    // admission, then the binding → skill path on a fresh child session.
    let out = rt
        .executor
        .execute("ext.hello", serde_json::json!({ "q": "hello" }), &ctx())
        .await
        .unwrap();
    let ToolOutput::Json(v) = out else {
        panic!("expected JSON tool output, got {out:?}");
    };
    assert_eq!(v["skill"], "skill.hello");
    assert_eq!(v["mode"], "agent");
    assert_eq!(v["version"], 1);
    let task = &v["task"];
    assert_eq!(task["status"], "Completed");
    let prompt = task["prompt"].as_str().expect("prompt");
    assert!(prompt.contains("Input:"), "prompt: {prompt}");
    assert!(prompt.contains("\"q\": \"hello\""), "prompt: {prompt}");

    // The child session is durable and archived (create → run → end).
    let sessions = rt.list_sessions().await.unwrap();
    let child = sessions
        .iter()
        .find(|s| s.title == "extension tool ext.hello")
        .expect("child session persisted");
    assert_eq!(child.status, SessionStatus::Archived);

    // Upgrade the payload skill to v2 (still >= the binding's floor): the
    // binding resolves the HIGHEST version — never substituted down.
    rt.assets_install_skill(skill_payload("skill.hello", 2, "Reply with DONE v2."))
        .await
        .unwrap();
    rt.assets_validate_skill("skill.hello").await.unwrap();
    rt.assets_activate_skill("skill.hello", &operator(&rt))
        .await
        .unwrap();

    let out = rt
        .executor
        .execute("ext.hello", serde_json::json!({}), &ctx())
        .await
        .unwrap();
    let ToolOutput::Json(v) = out else {
        panic!("expected JSON tool output, got {out:?}");
    };
    assert_eq!(v["version"], 2);
    let task = &v["task"];
    assert_eq!(task["status"], "Completed");
    // Empty args ⇒ no input block — just the skill's own prompt.
    assert!(
        !task["prompt"].as_str().unwrap().contains("Input:"),
        "prompt: {}",
        task["prompt"]
    );
}

// ---- 9. no binding without activation ----

#[tokio::test]
async fn only_active_extensions_bind_at_build() {
    let store = Arc::new(MemoryStore::new());

    // Registered only.
    let rt1 = rt_with(ext_cfg(), store.clone()).await;
    rt1.extension_install(pkg(
        ext_with_tool("ext.hello", 1, "skill.hello", 0),
        vec![skill_payload("skill.hello", 1, "Reply with DONE.")],
    ))
    .await
    .unwrap();
    drop(rt1);
    let rt2 = rt_with(ext_cfg(), store.clone()).await;
    assert!(!rt2.registry.contains("ext.hello"));

    // Validated — still not Active, still no binding.
    rt2.extension_validate("ext.hello", None).await.unwrap();
    drop(rt2);
    let rt3 = rt_with(ext_cfg(), store.clone()).await;
    assert!(
        !rt3.registry.contains("ext.hello"),
        "Validated is not Active"
    );

    // Skill + extension Active → binds on the NEXT build.
    rt3.assets_validate_skill("skill.hello").await.unwrap();
    rt3.assets_activate_skill("skill.hello", &operator(&rt3))
        .await
        .unwrap();
    rt3.extension_activate("ext.hello", None, &operator(&rt3))
        .await
        .unwrap();
    drop(rt3);
    let rt4 = rt_with(ext_cfg(), store).await;
    assert!(rt4.registry.contains("ext.hello"));
}

// ---- 10. rollback ----

#[tokio::test]
async fn rollback_preserves_the_version_history() {
    let rt = rt_with(ext_cfg(), Arc::new(MemoryStore::new())).await;
    install_with_skill(&rt, "ext.roll", "skill.roll").await;
    validate_and_activate(&rt, "ext.roll").await; // v1 active

    rt.extension_install(pkg(ext_with_tool("ext.roll", 2, "skill.roll", 0), vec![]))
        .await
        .unwrap();
    validate_and_activate(&rt, "ext.roll").await; // v2 active, v1 inactive

    let (from, to) = rt
        .extension_rollback("ext.roll", None, &operator(&rt))
        .await
        .unwrap();
    assert_eq!((from, to), (2, 1));
    assert_eq!(
        rt.extension_inspect("ext.roll", Some(1)).await.unwrap()["activation"],
        "active"
    );
    assert_eq!(
        rt.extension_inspect("ext.roll", Some(2)).await.unwrap()["activation"],
        "inactive"
    );
    // History preserved: the rolled-back-from version remains stored.
    assert_eq!(
        rt.extension_inspect("ext.roll", Some(2)).await.unwrap()["name"],
        "name"
    );
}

// ---- 11. removal gates ----

#[tokio::test]
async fn removal_refuses_while_active_or_depended_on() {
    let rt = rt_with(ext_cfg(), Arc::new(MemoryStore::new())).await;

    rt.extension_install(pkg(ext_base("ext.base", 1, "a base extension"), vec![]))
        .await
        .unwrap();
    let mut child = ext_base("ext.child", 1, "depends on the base");
    child.dependencies = vec![ExtensionDependency {
        id: ExtensionId("ext.base".into()),
        min_version: ExtensionVersion(1),
    }];
    rt.extension_install(pkg(child, vec![])).await.unwrap();
    rt.extension_validate("ext.base", None).await.unwrap();
    rt.extension_validate("ext.child", None).await.unwrap();
    rt.extension_activate("ext.base", None, &operator(&rt))
        .await
        .unwrap();
    rt.extension_activate("ext.child", None, &operator(&rt))
        .await
        .unwrap();

    // Active: removal refused.
    let err = rt
        .extension_remove("ext.child", &operator(&rt))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Asset(_)), "got {err:?}");

    // Deactivated but still depended on: removal refused (dependency gate
    // isolated — the base is not active at this point).
    rt.extension_deactivate("ext.base", &operator(&rt))
        .await
        .unwrap();
    let err = rt
        .extension_remove("ext.base", &operator(&rt))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Asset(_)), "got {err:?}");

    // Remove the dependent first, then the base: allowed.
    rt.extension_deactivate("ext.child", &operator(&rt))
        .await
        .unwrap();
    let removed = rt
        .extension_remove("ext.child", &operator(&rt))
        .await
        .unwrap();
    assert_eq!(removed, vec![1]);
    let removed = rt
        .extension_remove("ext.base", &operator(&rt))
        .await
        .unwrap();
    assert_eq!(removed, vec![1]);
    let err = rt.extension_inspect("ext.base", None).await.unwrap_err();
    assert!(
        matches!(err, Error::Asset(AssetError::NotFound { .. })),
        "got {err:?}"
    );
}

// ---- 12. tool-name conflicts ----

#[tokio::test]
async fn tool_name_conflicts_fail_validation_or_the_build() {
    let store = Arc::new(MemoryStore::new());
    let rt = rt_with(ext_cfg(), store.clone()).await;

    // Two extensions claiming the same tool name, one payload skill each.
    for (ext_id, skill_id) in [("ext.a", "skill.a"), ("ext.b", "skill.b")] {
        let mut e = ext_base(ext_id, 1, "claims the shared tool");
        e.tools = vec![binding_tool("ext.shared", skill_id)];
        e.skill_refs = vec![skill_ref(skill_id)];
        rt.extension_install(pkg(e, vec![skill_payload(skill_id, 1, "Reply with DONE.")]))
            .await
            .unwrap();
        rt.assets_validate_skill(skill_id).await.unwrap();
        rt.assets_activate_skill(skill_id, &operator(&rt))
            .await
            .unwrap();
    }
    validate_and_activate(&rt, "ext.a").await;

    // The second claim fails validation while the first is Active.
    let err = rt.extension_validate("ext.b", None).await.unwrap_err();
    match err {
        Error::Asset(AssetError::PolicyViolation { detail }) => {
            assert!(detail.contains("ext.shared"), "detail: {detail}");
            assert!(detail.contains("conflicts"), "detail: {detail}");
        }
        other => panic!("expected conflict PolicyViolation, got {other:?}"),
    }

    // A tool name owned by a builtin fails the BUILD — never a silent skip.
    let mut echo_ext = ext_base("ext.echo", 1, "claims a builtin tool name");
    echo_ext.tools = vec![binding_tool("echo", "skill.a")];
    echo_ext.skill_refs = vec![skill_ref("skill.a")];
    rt.extension_install(pkg(echo_ext, vec![])).await.unwrap();
    validate_and_activate(&rt, "ext.echo").await;
    drop(rt);

    let err = try_build(ext_cfg(), store).await.unwrap_err();
    match err {
        Error::Extension(msg) => assert!(msg.contains("`echo`"), "msg: {msg}"),
        other => panic!("expected build-time Extension error, got {other:?}"),
    }
}
