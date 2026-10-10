//! Phase 10 M10.7/M10.8 — the ECC gaps not covered by `p10_ecc`:
//! instinct approval lifecycle, instinct application durability across a
//! runtime restart, and genome version isolation (exact pins, per-session
//! references). Rule/hook enforcement, bounded instinct influence,
//! instinct creation from history, genome validation, fresh-session
//! application, approval-gated evolution, and ledger-backed learning
//! derivation live in `p10_ecc.rs` and `openrt-runtime::phase5` tests.

use std::sync::Arc;
use std::time::Duration;

use openrt_models::mock::MockProvider;
use openrt_persistence::MemoryStore;
use openrt_runtime::{Runtime, RuntimeBuilder};
use openrt_workspace_assets::{
    ActivationState, AgentGenome, CanonicalForm, GenomeId, InstinctId, OperatorGrant,
    Provenance as AssetProvenance, Skill, SkillId, SkillVersion,
};

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

fn operator(rt: &Runtime) -> String {
    rt.asset_operators()[0].clone()
}

fn prov() -> AssetProvenance {
    AssetProvenance::new("run.p10ecc", "src.p10_ecc_extra")
}

async fn poll_evidence(rt: &Runtime, session: &str) -> Vec<(String, u64)> {
    for _ in 0..200 {
        let evidence = rt.history_tool_evidence(session).await.unwrap();
        if !evidence.is_empty() {
            return evidence;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no tool evidence landed on the ledger for `{session}`");
}

// ---- instinct creation + approval -----------------------------------------

/// History-derived extraction creates a REGISTERED instinct; activation
/// demands validation first and an allowlisted operator second — the same
/// gate as skills/hooks, no second activation mechanism. Approval is
/// durable across a runtime restart.
#[tokio::test]
async fn instinct_creation_and_approval_lifecycle() {
    let store = Arc::new(MemoryStore::new());
    let rt = rt_with(openrt_config::Config::default(), store.clone()).await;
    let op = operator(&rt);

    let session = rt.create_session("p10 instinct").await.unwrap();
    rt.run_task(session.id, "use tools to solve the task")
        .await
        .unwrap();
    let sid = session.id.to_string();
    poll_evidence(&rt, &sid).await;

    let id = rt.instinct_submit_from_history(&sid).await.unwrap();
    let iid = InstinctId(id.clone());
    let assets = rt.load_assets().await.unwrap();
    let created = assets.get_instinct(&iid).expect("durable instinct");
    assert!(
        matches!(created.activation, ActivationState::Registered),
        "extraction must never activate: {:?}",
        created.activation
    );
    assert_ne!(created.provenance.source, "", "provenance is mandatory");
    drop(assets);

    // Approval gate: unvalidated first, unauthorized operator second.
    let grant = OperatorGrant::new(&op).unwrap();
    let mallory = OperatorGrant::new("mallory").unwrap();
    {
        let mut assets = rt.load_assets().await.unwrap();
        let err = assets.activate_instinct(&iid, Some(&grant)).unwrap_err();
        assert!(err.to_string().contains("not validated"), "{err}");
        let err = assets.activate_instinct(&iid, Some(&mallory)).unwrap_err();
        assert!(err.to_string().contains("unauthorized activation"), "{err}");
    }

    // Production wrappers: the CLI/runtime path — validate then operator
    // activation (fires the AssetActivated lifecycle hook).
    let err = rt.assets_activate_instinct(&id, &op).await.unwrap_err();
    assert!(err.to_string().contains("not validated"), "{err}");
    rt.assets_validate_instinct(&id).await.unwrap();
    rt.assets_activate_instinct(&id, &op).await.unwrap();
    let assets = rt.load_assets().await.unwrap();
    assert!(matches!(
        assets.get_instinct(&iid).unwrap().activation,
        ActivationState::Active
    ));
    drop(assets);

    // Durable approval: a fresh runtime sees the SAME active state.
    let rt2 = rt_with(openrt_config::Config::default(), store.clone()).await;
    let assets = rt2.load_assets().await.unwrap();
    assert!(matches!(
        assets.get_instinct(&iid).unwrap().activation,
        ActivationState::Active
    ));
    assert!(assets.active_instincts().iter().any(|i| i.id.0 == id));
}

// ---- instinct application durability --------------------------------------

/// An active instinct shapes the run context of a FRESH runtime (loaded
/// from the store, not process memory); deactivating it restores the
/// byte-for-byte prompt.
#[tokio::test]
async fn instinct_application_is_durable_across_restart() {
    let store = Arc::new(MemoryStore::new());
    let rt1 = rt_with(openrt_config::Config::default(), store.clone()).await;
    let op = operator(&rt1);
    let grant = OperatorGrant::new(&op).unwrap();

    let mut inst = openrt_workspace_assets::Instinct::new(
        InstinctId("instinct.p10.restart".into()),
        1,
        "prefer the echo tool",
        90,
        prov(),
    )
    .unwrap();
    inst.trigger = "any task".into();
    inst.recommendation = "use echo before anything else".into();
    inst.confidence = 0.9;
    {
        let mut assets = rt1.load_assets().await.unwrap();
        assets.register_instinct(inst).unwrap();
        assets
            .validate_instinct(&InstinctId("instinct.p10.restart".into()))
            .unwrap();
        assets
            .activate_instinct(&InstinctId("instinct.p10.restart".into()), Some(&grant))
            .unwrap();
        rt1.save_assets(&assets).await.unwrap();
    }
    drop(rt1);

    // Fresh runtime, same store: the application still happens.
    let rt2 = rt_with(openrt_config::Config::default(), store.clone()).await;
    let session = rt2.create_session("p10 restart inst").await.unwrap();
    let task = rt2.run_task(session.id, "base prompt").await.unwrap();
    assert!(task.prompt.starts_with("base prompt"));
    assert!(
        task.prompt.contains("[instincts"),
        "durable active instinct must append context: {}",
        task.prompt
    );
    assert!(task.prompt.contains("use echo before anything else"));

    // Deactivate (operator path) → another fresh runtime: prompt unchanged.
    let op2 = operator(&rt2);
    let grant2 = OperatorGrant::new(&op2).unwrap();
    {
        let mut assets = rt2.load_assets().await.unwrap();
        assets
            .deactivate_instinct(&InstinctId("instinct.p10.restart".into()), Some(&grant2))
            .unwrap();
        rt2.save_assets(&assets).await.unwrap();
    }
    drop(rt2);

    let rt3 = rt_with(openrt_config::Config::default(), store).await;
    let session = rt3.create_session("p10 no inst").await.unwrap();
    let task = rt3.run_task(session.id, "plain prompt").await.unwrap();
    assert_eq!(task.prompt, "plain prompt");
}

// ---- genome version isolation ---------------------------------------------

/// A genome pins EXACT skill versions — a newer skill version never
/// rewrites the pin — and every session records the exact genome
/// id/version/digest that was applied to IT, isolated from every other
/// genome's reference.
#[tokio::test]
async fn genome_version_isolation_pins_exact_references() {
    let store = Arc::new(MemoryStore::new());
    let rt = rt_with(openrt_config::Config::default(), store).await;
    let op = operator(&rt);

    // skill v1 registered + validated, then G1 pins skill@1.
    rt.assets_register_skill("skill.p10iso", "version one skill", vec!["fs.read".into()])
        .await
        .unwrap();
    rt.assets_validate_skill("skill.p10iso").await.unwrap();
    {
        let mut assets = rt.load_assets().await.unwrap();
        assets
            .register_genome(
                AgentGenome::new(
                    GenomeId("genome.p10iso.one".into()),
                    1,
                    vec![(SkillId("skill.p10iso".into()), SkillVersion(1))],
                    vec![],
                    vec![],
                    vec![],
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        rt.save_assets(&assets).await.unwrap();
    }

    // A newer skill version arrives — the genome's pin must not move.
    let mut v2 = Skill::new(
        SkillId("skill.p10iso".into()),
        SkillVersion(2),
        "version two skill",
        vec!["fs.read".into()],
        vec![],
        prov(),
    )
    .unwrap();
    v2.goal = "echo v2".to_string();
    rt.assets_install_skill(v2).await.unwrap();
    rt.assets_validate_skill("skill.p10iso").await.unwrap();
    {
        let assets = rt.load_assets().await.unwrap();
        let g = assets
            .get_genome(&GenomeId("genome.p10iso.one".into()))
            .unwrap();
        assert_eq!(
            g.skills,
            vec![(SkillId("skill.p10iso".into()), SkillVersion(1))],
            "a newer skill version must never rewrite the genome pin"
        );
        let pinned = assets
            .get_skill_version(&SkillId("skill.p10iso".into()), SkillVersion(1))
            .unwrap();
        assert_eq!(pinned.description, "version one skill");
        assert!(matches!(pinned.activation, ActivationState::Validated));
    }

    // Apply G1 on session A: reference records genome v1 + its digest,
    // and the pin is still exact afterwards.
    let a = rt.create_session("p10 iso a").await.unwrap();
    let ref1 = rt
        .session_apply_genome(a.id, "genome.p10iso.one", &op)
        .await
        .unwrap();
    assert_eq!(ref1.id, "genome.p10iso.one");
    assert_eq!(ref1.version, 1);
    assert!(!ref1.digest.is_empty());
    {
        let assets = rt.load_assets().await.unwrap();
        let g = assets
            .get_genome(&GenomeId("genome.p10iso.one".into()))
            .unwrap();
        assert_eq!(
            g.skills,
            vec![(SkillId("skill.p10iso".into()), SkillVersion(1))]
        );
        assert_eq!(ref1.digest, g.digest());
    }

    // A second genome declares version 5: session B records ITS exact
    // version/digest — session A's reference is untouched.
    {
        let mut assets = rt.load_assets().await.unwrap();
        assets
            .register_genome(
                AgentGenome::new(
                    GenomeId("genome.p10iso.two".into()),
                    5,
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        rt.save_assets(&assets).await.unwrap();
    }
    let b = rt.create_session("p10 iso b").await.unwrap();
    let ref2 = rt
        .session_apply_genome(b.id, "genome.p10iso.two", &op)
        .await
        .unwrap();
    assert_eq!(ref2.id, "genome.p10iso.two");
    assert_eq!(
        ref2.version, 5,
        "each genome records its own declared version"
    );
    assert_ne!(ref2.digest, ref1.digest);

    let reloaded_a = rt.store.load_session(a.id).await.unwrap();
    assert_eq!(
        reloaded_a.genome,
        Some(ref1),
        "session A keeps its own reference after session B is re-genomed"
    );
    let reloaded_b = rt.store.load_session(b.id).await.unwrap();
    assert_eq!(reloaded_b.genome, Some(ref2));
}
