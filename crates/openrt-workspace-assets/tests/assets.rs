use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_workspace_assets::{
    ActivationState, AgentGenome, AssetEnvelope, AssetError, AssetStore, CanonicalForm, GenomeId,
    Hook, HookId, HookTarget, Instinct, InstinctId, OperatorGrant, Provenance, ReviewState, Rule,
    RuleEffect, RuleId, Skill, SkillCandidate, SkillDependency, SkillGenerator, SkillId,
    SkillVersion, Workflow, WorkflowStep, TRUST_VETTED,
};

fn prov() -> Provenance {
    Provenance::new("run.test-1", "src.test")
}

fn grant() -> OperatorGrant {
    OperatorGrant::new("alice").unwrap()
}

fn store(capabilities: &[&str]) -> AssetStore {
    let caps: Vec<String> = capabilities.iter().map(|s| s.to_string()).collect();
    AssetStore::new(&["alice".to_string()], &caps, false)
}

fn skill(id: &str, deps: Vec<SkillDependency>, perms: Vec<&str>) -> Skill {
    Skill::new(
        SkillId(id.to_string()),
        SkillVersion(1),
        &format!("skill {id}"),
        perms.iter().map(|s| s.to_string()).collect(),
        deps,
        prov(),
    )
    .unwrap()
}

// ---------- M5.4 positive ----------

#[test]
fn m54_skill_identity_version_digest_deterministic() {
    let a = skill("skill.a", vec![], vec![]);
    let b = skill("skill.a", vec![], vec![]);
    assert_eq!(a.id, b.id);
    assert_eq!(a.version, b.version);
    assert_eq!(a.digest(), b.digest());
    let c = skill("skill.a", vec![], vec!["fs.read"]);
    assert_ne!(a.digest(), c.digest(), "content change must change digest");
}

#[test]
fn m54_registration_validation_activation_deactivation() {
    let mut s = store(&["fs.read"]);
    s.register_skill(skill("skill.a", vec![], vec!["fs.read"]))
        .unwrap();
    assert_eq!(
        s.get_skill(&SkillId("skill.a".into())).unwrap().activation,
        ActivationState::Registered
    );
    s.validate_skill(&SkillId("skill.a".into())).unwrap();
    assert_eq!(
        s.get_skill(&SkillId("skill.a".into())).unwrap().activation,
        ActivationState::Validated
    );
    s.activate_skill(&SkillId("skill.a".into()), Some(&grant()))
        .unwrap();
    assert_eq!(
        s.get_skill(&SkillId("skill.a".into())).unwrap().activation,
        ActivationState::Active
    );
    s.deactivate_skill(&SkillId("skill.a".into()), Some(&grant()))
        .unwrap();
    assert_eq!(
        s.get_skill(&SkillId("skill.a".into())).unwrap().activation,
        ActivationState::Inactive
    );
}

#[test]
fn m54_dependency_resolution_order_deterministic() {
    let mut s = store(&[]);
    s.register_skill(skill("skill.base", vec![], vec![]))
        .unwrap();
    s.register_skill(skill(
        "skill.mid",
        vec![SkillDependency {
            id: SkillId("skill.base".into()),
            min_version: SkillVersion(1),
        }],
        vec![],
    ))
    .unwrap();
    s.register_skill(skill(
        "skill.top",
        vec![
            SkillDependency {
                id: SkillId("skill.mid".into()),
                min_version: SkillVersion(1),
            },
            SkillDependency {
                id: SkillId("skill.base".into()),
                min_version: SkillVersion(1),
            },
        ],
        vec![],
    ))
    .unwrap();
    let order = s.activation_order(&SkillId("skill.top".into())).unwrap();
    assert_eq!(
        order,
        vec![
            SkillId("skill.base".into()),
            SkillId("skill.mid".into()),
            SkillId("skill.top".into())
        ]
    );
    let again = s.activation_order(&SkillId("skill.top".into())).unwrap();
    assert_eq!(order, again, "resolution must be deterministic");
}

#[test]
fn m54_instinct_registration_invalidation() {
    let mut s = store(&[]);
    let inst = Instinct::new(
        InstinctId("instinct.pref".into()),
        1,
        "prefer short answers",
        70,
        prov(),
    )
    .unwrap();
    s.register_instinct(inst).unwrap();
    let before = s.meta().capabilities.clone();
    s.invalidate_instinct(&InstinctId("instinct.pref".into()), Some(&grant()))
        .unwrap();
    assert_eq!(
        s.get_instinct(&InstinctId("instinct.pref".into()))
            .unwrap()
            .activation,
        ActivationState::Invalidated
    );
    assert_eq!(
        before,
        s.meta().capabilities,
        "instinct state change must not alter security policy"
    );
}

#[test]
fn m54_hook_lifecycle_and_outcome() {
    let mut s = store(&["fs.read"]);
    let hook = Hook::new(
        HookId("hook.onrun".into()),
        1,
        HookTarget::RunCompleted,
        "notify on run complete",
        Some("fs.read".to_string()),
        prov(),
    )
    .unwrap();
    s.register_hook(hook).unwrap();
    assert!(s.run_hook(&HookId("hook.onrun".into()), true).is_err());
    s.activate_hook(&HookId("hook.onrun".into()), Some(&grant()))
        .unwrap();
    let out = s.run_hook(&HookId("hook.onrun".into()), true).unwrap();
    assert!(out.fired);
    assert_eq!(out.target, "run_completed");
    s.deactivate_hook(&HookId("hook.onrun".into()), Some(&grant()))
        .unwrap();
    let err = s.run_hook(&HookId("hook.onrun".into()), true).unwrap_err();
    assert!(matches!(err, AssetError::HookNotActive { .. }));
}

#[test]
fn m54_rule_evaluation_and_security_precedence() {
    let mut s = store(&["tool.run", "tool.other"]);
    s.register_rule(
        Rule::new(
            RuleId("rule.allow-run".into()),
            1,
            "tool.execute",
            "tool.run",
            RuleEffect::Allow,
            10,
            true,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    s.register_rule(
        Rule::new(
            RuleId("rule.deny-run".into()),
            1,
            "tool.execute",
            "tool.run",
            RuleEffect::Deny,
            5,
            true,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    let d = s.evaluate_rule("tool.execute", "tool.run").unwrap();
    assert!(d.allowed, "higher-priority allow wins among rules");
    assert_eq!(d.decided_by, "rule");
    let d2 = s.evaluate_rule("tool.execute", "tool.other").unwrap();
    assert!(!d2.allowed, "deny by default");
    assert_eq!(d2.decided_by, "default-deny");
    let d3 = s.evaluate_rule("tool.execute", "fs.write").unwrap();
    assert!(!d3.allowed, "security policy is authoritative over rules");
    assert_eq!(d3.decided_by, "security-policy");
}

#[test]
fn m54_genome_composition_deterministic_identity() {
    let mut s = store(&["fs.read"]);
    s.register_skill(skill("skill.a", vec![], vec![])).unwrap();
    let g1 = AgentGenome::new(
        GenomeId("genome.base".into()),
        1,
        vec![(SkillId("skill.a".into()), SkillVersion(1))],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    let g2 = AgentGenome::new(
        GenomeId("genome.base".into()),
        1,
        vec![(SkillId("skill.a".into()), SkillVersion(1))],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    assert_eq!(
        g1.digest(),
        g2.digest(),
        "identical genome = identical digest"
    );
    let g3 = g1.clone().with_changes(vec![], vec![], vec![], vec![]);
    assert_eq!(g3.version, 2, "genome change must be a new version");
    assert_ne!(g1.digest(), g3.digest(), "changed genome = new identity");
    s.register_genome(g1).unwrap();
    let loaded = s.get_genome(&GenomeId("genome.base".into())).unwrap();
    assert_eq!(loaded.digest(), g2.digest());
}

// ---------- M5.4 negative / security ----------

#[test]
fn m54_neg_tampered_asset_digest_mismatch() {
    let skill = skill("skill.tamper", vec![], vec![]);
    let env = AssetEnvelope::seal(skill);
    env.verify().unwrap();
    let mut tampered = env.clone();
    tampered.payload.description = "tampered".to_string();
    let err = tampered.verify().unwrap_err();
    assert!(matches!(err, AssetError::DigestMismatch { .. }));
    let mut tampered_digest = env.clone();
    tampered_digest.digest = "0000".to_string();
    assert!(matches!(
        tampered_digest.verify().unwrap_err(),
        AssetError::DigestMismatch { .. }
    ));
}

#[test]
fn m54_neg_unauthorized_activation() {
    let mut s = store(&[]);
    s.register_skill(skill("skill.a", vec![], vec![])).unwrap();
    s.validate_skill(&SkillId("skill.a".into())).unwrap();
    let err = s
        .activate_skill(&SkillId("skill.a".into()), None)
        .unwrap_err();
    assert!(matches!(err, AssetError::UnauthorizedActivation { .. }));
    let mallory = OperatorGrant::new("mallory").unwrap();
    let err2 = s
        .activate_skill(&SkillId("skill.a".into()), Some(&mallory))
        .unwrap_err();
    assert!(matches!(err2, AssetError::UnauthorizedActivation { .. }));
}

#[test]
fn m54_neg_missing_dependency() {
    let mut s = store(&[]);
    s.register_skill(skill(
        "skill.a",
        vec![SkillDependency {
            id: SkillId("skill.ghost".into()),
            min_version: SkillVersion(1),
        }],
        vec![],
    ))
    .unwrap();
    let err = s.validate_skill(&SkillId("skill.a".into())).unwrap_err();
    assert!(matches!(err, AssetError::MissingDependency { .. }));
}

#[test]
fn m54_neg_incompatible_dependency() {
    let mut s = store(&[]);
    let mut base = skill("skill.base", vec![], vec![]);
    base.version = SkillVersion(1);
    s.register_skill(base).unwrap();
    s.register_skill(skill(
        "skill.a",
        vec![SkillDependency {
            id: SkillId("skill.base".into()),
            min_version: SkillVersion(2),
        }],
        vec![],
    ))
    .unwrap();
    let err = s.validate_skill(&SkillId("skill.a".into())).unwrap_err();
    match err {
        AssetError::IncompatibleDependency {
            required, found, ..
        } => {
            assert_eq!(required, 2);
            assert_eq!(found, 1);
        }
        other => panic!("expected IncompatibleDependency, got {other:?}"),
    }
}

#[test]
fn m54_neg_inactive_dependency_fails_closed() {
    let mut s = store(&[]);
    s.register_skill(skill("skill.base", vec![], vec![]))
        .unwrap();
    s.validate_skill(&SkillId("skill.base".into())).unwrap();
    s.activate_skill(&SkillId("skill.base".into()), Some(&grant()))
        .unwrap();
    s.deactivate_skill(&SkillId("skill.base".into()), Some(&grant()))
        .unwrap();
    s.register_skill(skill(
        "skill.a",
        vec![SkillDependency {
            id: SkillId("skill.base".into()),
            min_version: SkillVersion(1),
        }],
        vec![],
    ))
    .unwrap();
    s.validate_skill(&SkillId("skill.a".into())).unwrap();
    let err = s
        .activate_skill(&SkillId("skill.a".into()), Some(&grant()))
        .unwrap_err();
    assert!(matches!(err, AssetError::DependencyNotActive { .. }));
}

#[test]
fn m54_neg_invalid_provenance() {
    let err = Skill::new(
        SkillId("skill.a".into()),
        SkillVersion(1),
        "desc",
        vec![],
        vec![],
        Provenance::new("", "src"),
    )
    .unwrap_err();
    assert!(matches!(err, AssetError::InvalidProvenance));
    let err2 = Instinct::new(
        InstinctId("instinct.a".into()),
        1,
        "pattern",
        50,
        Provenance::new("", ""),
    )
    .unwrap_err();
    assert!(matches!(err2, AssetError::InvalidProvenance));
}

#[test]
fn m54_neg_invalid_hook() {
    let err = Hook::new(
        HookId("BAD ID".into()),
        1,
        HookTarget::RunStarted,
        "desc",
        None,
        prov(),
    )
    .unwrap_err();
    assert!(matches!(err, AssetError::InvalidHook { .. }));
    let err2 = Hook::new(
        HookId("hook.a".into()),
        1,
        HookTarget::RunStarted,
        "   ",
        None,
        prov(),
    )
    .unwrap_err();
    assert!(matches!(err2, AssetError::InvalidHook { .. }));
}

#[test]
fn m54_neg_unauthorized_hook() {
    let mut s = store(&["fs.read"]);
    let hook = Hook::new(
        HookId("hook.secure".into()),
        1,
        HookTarget::TaskCompleted,
        "needs permission",
        Some("fs.read".to_string()),
        prov(),
    )
    .unwrap();
    s.register_hook(hook).unwrap();
    s.activate_hook(&HookId("hook.secure".into()), Some(&grant()))
        .unwrap();
    let err = s
        .run_hook(&HookId("hook.secure".into()), false)
        .unwrap_err();
    assert!(matches!(err, AssetError::UnauthorizedHook { .. }));
}

#[test]
fn m54_neg_rule_conflict() {
    let mut s = store(&["tool.run"]);
    s.register_rule(
        Rule::new(
            RuleId("rule.allow".into()),
            1,
            "tool.execute",
            "tool.run",
            RuleEffect::Allow,
            10,
            true,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    s.register_rule(
        Rule::new(
            RuleId("rule.deny".into()),
            1,
            "tool.execute",
            "tool.run",
            RuleEffect::Deny,
            10,
            true,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    let err = s.evaluate_rule("tool.execute", "tool.run").unwrap_err();
    assert!(matches!(err, AssetError::RuleConflict { .. }));
}

#[test]
fn m54_neg_malformed_rule() {
    let err = Rule::new(
        RuleId("rule.a".into()),
        1,
        "",
        "subject",
        RuleEffect::Allow,
        1,
        true,
        prov(),
    )
    .unwrap_err();
    assert!(matches!(err, AssetError::Malformed { .. }));
}

#[test]
fn m54_neg_invalid_genome_reference() {
    let mut s = store(&[]);
    let genome = AgentGenome::new(
        GenomeId("genome.bad".into()),
        1,
        vec![(SkillId("skill.ghost".into()), SkillVersion(1))],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    let err = s.register_genome(genome).unwrap_err();
    assert!(matches!(err, AssetError::InvalidGenomeReference { .. }));
}

#[test]
fn m54_neg_tampered_genome() {
    let genome = AgentGenome::new(
        GenomeId("genome.a".into()),
        1,
        vec![],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    let env = AssetEnvelope::seal(genome);
    env.verify().unwrap();
    let mut tampered = env.clone();
    tampered.payload.version = 99;
    assert!(matches!(
        tampered.verify().unwrap_err(),
        AssetError::DigestMismatch { .. }
    ));
}

#[test]
fn m54_neg_policy_bypass_skill_permission_outside_allowlist() {
    let mut s = store(&["fs.read"]);
    let err = s
        .register_skill(skill("skill.sneaky", vec![], vec!["network.egress"]))
        .unwrap_err();
    assert!(matches!(err, AssetError::PolicyViolation { .. }));
    assert!(s.get_skill(&SkillId("skill.sneaky".into())).is_err());
}

#[test]
fn m54_neg_sovereign_mode_blocks_network_skill() {
    let caps = ["network.egress", "fs.read"];
    let capsv: Vec<String> = caps.iter().map(|s| s.to_string()).collect();
    let mut s = AssetStore::new(&["alice".to_string()], &capsv, true);
    s.register_skill(skill("skill.net", vec![], vec!["network.egress"]))
        .unwrap();
    s.validate_skill(&SkillId("skill.net".into())).unwrap();
    let err = s
        .activate_skill(&SkillId("skill.net".into()), Some(&grant()))
        .unwrap_err();
    assert!(matches!(err, AssetError::SovereignViolation { .. }));
}

// ---------- M5.4 persistence ----------

#[tokio::test]
async fn m54_persist_reopen_verify() {
    let mut s = store(&["fs.read", "subject.a"]);
    s.register_skill(skill("skill.a", vec![], vec!["fs.read"]))
        .unwrap();
    s.validate_skill(&SkillId("skill.a".into())).unwrap();
    s.activate_skill(&SkillId("skill.a".into()), Some(&grant()))
        .unwrap();
    s.register_rule(
        Rule::new(
            RuleId("rule.a".into()),
            1,
            "scope.a",
            "subject.a",
            RuleEffect::Allow,
            1,
            true,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    let mem = MemoryStore::default();
    s.save(&mem).await.unwrap();
    let loaded = AssetStore::load(&mem).await.unwrap();
    assert_eq!(
        loaded
            .get_skill(&SkillId("skill.a".into()))
            .unwrap()
            .digest(),
        s.get_skill(&SkillId("skill.a".into())).unwrap().digest()
    );
    assert_eq!(
        loaded
            .get_skill(&SkillId("skill.a".into()))
            .unwrap()
            .activation,
        ActivationState::Active
    );
    assert_eq!(loaded.meta(), s.meta());
    assert!(
        loaded
            .evaluate_rule("scope.a", "subject.a")
            .unwrap()
            .allowed
    );
}

#[tokio::test]
async fn m54_neg_persist_tamper_reopen_reject() {
    let mut s = store(&[]);
    s.register_skill(skill("skill.a", vec![], vec![])).unwrap();
    let mem = MemoryStore::default();
    s.save(&mem).await.unwrap();
    let record = mem
        .get_record(openrt_workspace_assets::SKILLS_NS, "skill.a.v1")
        .await
        .unwrap()
        .unwrap();
    let mut tampered = record.clone();
    tampered["payload"]["description"] = serde_json::Value::String("tampered".into());
    mem.put_record(openrt_workspace_assets::SKILLS_NS, "skill.a.v1", &tampered)
        .await
        .unwrap();
    let err = AssetStore::load(&mem).await.unwrap_err();
    assert!(matches!(err, AssetError::DigestMismatch { .. }));
}

#[tokio::test]
async fn m54_neg_persist_missing_provenance_reject() {
    let mut s = store(&[]);
    s.register_skill(skill("skill.a", vec![], vec![])).unwrap();
    let mem = MemoryStore::default();
    s.save(&mem).await.unwrap();
    let mut record = mem
        .get_record(openrt_workspace_assets::SKILLS_NS, "skill.a.v1")
        .await
        .unwrap()
        .unwrap();
    record["payload"]["provenance"]["origin"] = serde_json::Value::String("".into());
    let payload = record["payload"].clone();
    let pairs: Vec<(String, String)> = vec![
        ("kind".into(), "skill".into()),
        ("id".into(), payload["id"].as_str().unwrap().to_string()),
        (
            "version".into(),
            payload["version"].as_u64().unwrap().to_string(),
        ),
        (
            "description".into(),
            payload["description"].as_str().unwrap().to_string(),
        ),
        ("permissions".into(), String::new()),
        ("dependencies".into(), String::new()),
        ("provenance.origin".into(), String::new()),
        (
            "provenance.source".into(),
            payload["provenance"]["source"]
                .as_str()
                .unwrap()
                .to_string(),
        ),
        ("activation".into(), "registered".into()),
    ];
    record["digest"] = serde_json::Value::String(openrt_workspace_assets::canonical_digest(&pairs));
    mem.put_record(openrt_workspace_assets::SKILLS_NS, "skill.a.v1", &record)
        .await
        .unwrap();
    let err = AssetStore::load(&mem).await.unwrap_err();
    assert!(matches!(err, AssetError::InvalidProvenance));
}

// ---------- M5.5 ----------

fn candidate() -> SkillCandidate {
    SkillGenerator::generate(
        "req.1",
        "generated formatter",
        vec!["fs.read".to_string()],
        vec![],
        prov(),
    )
    .unwrap()
}

#[test]
fn m55_candidate_generation_deterministic_identity() {
    let a = candidate();
    let b = candidate();
    assert_eq!(a.id, b.id, "identical request = identical candidate id");
    assert_eq!(a.skill.digest(), b.skill.digest());
    assert_eq!(a.review, ReviewState::Generated);
    let c = SkillGenerator::generate(
        "req.2",
        "generated formatter",
        vec!["fs.read".to_string()],
        vec![],
        prov(),
    )
    .unwrap();
    assert_ne!(a.id, c.id, "different request = different identity");
}

#[test]
fn m55_generation_is_not_activation() {
    let mut s = store(&["fs.read"]);
    let c = candidate();
    s.submit_candidate(c.clone()).unwrap();
    assert_eq!(
        s.get_candidate(&c.id).unwrap().review,
        ReviewState::Generated
    );
    assert!(
        s.get_skill(&c.skill.id.clone()).is_err(),
        "generated candidate must not exist as an active/registered skill"
    );
}

#[test]
fn m55_validation_review_approval_activation_pipeline() {
    let mut s = store(&["fs.read"]);
    let c = candidate();
    s.submit_candidate(c.clone()).unwrap();
    s.validate_candidate(&c.id).unwrap();
    assert_eq!(
        s.get_candidate(&c.id).unwrap().review,
        ReviewState::Validated
    );
    let err = s.activate_candidate(&c.id, Some(&grant())).unwrap_err();
    assert!(
        matches!(err, AssetError::CandidateNotApproved { .. }),
        "activation before approval must fail"
    );
    s.approve_candidate(&c.id, Some(&grant())).unwrap();
    assert_eq!(
        s.get_candidate(&c.id).unwrap().review,
        ReviewState::Approved
    );
    let sid = s.activate_candidate(&c.id, Some(&grant())).unwrap();
    assert_eq!(sid, c.skill.id);
    assert_eq!(
        s.get_skill(&sid).unwrap().activation,
        ActivationState::Active
    );
}

#[test]
fn m55_neg_rejected_candidate_cannot_activate() {
    let mut s = store(&["fs.read"]);
    let c = candidate();
    s.submit_candidate(c.clone()).unwrap();
    s.validate_candidate(&c.id).unwrap();
    s.reject_candidate(&c.id, Some(&grant())).unwrap();
    let err = s.activate_candidate(&c.id, Some(&grant())).unwrap_err();
    assert!(matches!(err, AssetError::CandidateRejected { .. }));
}

#[test]
fn m55_neg_candidate_permission_failure() {
    let mut s = store(&["fs.read"]);
    let c = SkillGenerator::generate(
        "req.perms",
        "wants network",
        vec!["network.egress".to_string()],
        vec![],
        prov(),
    )
    .unwrap();
    let err = s.submit_candidate(c).unwrap_err();
    assert!(matches!(err, AssetError::PolicyViolation { .. }));
}

#[test]
fn m55_neg_candidate_provenance_failure() {
    let err =
        SkillGenerator::generate("req.prov", "desc", vec![], vec![], Provenance::new("", "x"))
            .unwrap_err();
    assert!(matches!(err, AssetError::InvalidProvenance));
}

#[test]
fn m55_workflow_ordering_dependencies_permissions() {
    let mut s = store(&["fs.read"]);
    let wf = Workflow::new(
        "workflow.base",
        1,
        vec![
            WorkflowStep {
                name: "one".into(),
                tool: "fs.list".into(),
            },
            WorkflowStep {
                name: "two".into(),
                tool: "fs.read".into(),
            },
        ],
        vec![],
        vec!["path".into()],
        vec!["content".into()],
        vec!["fs.read".into()],
        prov(),
    )
    .unwrap();
    assert_eq!(wf.step_names(), vec!["one".to_string(), "two".to_string()]);
    s.register_workflow(wf).unwrap();
    let wf2 = Workflow::new(
        "workflow.child",
        1,
        vec![WorkflowStep {
            name: "three".into(),
            tool: "fs.read".into(),
        }],
        vec![("workflow.base".to_string(), 1)],
        vec![],
        vec![],
        vec!["fs.read".into()],
        prov(),
    )
    .unwrap();
    s.register_workflow(wf2).unwrap();
    assert_eq!(
        s.workflow_step_names("workflow.base").unwrap(),
        vec!["one".to_string(), "two".to_string()],
        "workflow order must be preserved deterministically"
    );
    let d1 = s.get_workflow("workflow.base").unwrap().digest();
    let mut reordered = s.get_workflow("workflow.base").unwrap().clone();
    reordered.steps.reverse();
    assert_ne!(d1, reordered.digest(), "step order is part of identity");
}

#[test]
fn m55_neg_invalid_workflow() {
    let err = Workflow::new(
        "workflow.empty",
        1,
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap_err();
    assert!(matches!(err, AssetError::Malformed { .. }));
    let err2 = Workflow::new(
        "workflow.gap",
        1,
        vec![WorkflowStep {
            name: "".into(),
            tool: "fs.read".into(),
        }],
        vec![],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap_err();
    assert!(matches!(err2, AssetError::Malformed { .. }));
}

#[test]
fn m55_neg_workflow_dependency_failure() {
    let mut s = store(&[]);
    let wf = Workflow::new(
        "workflow.child",
        1,
        vec![WorkflowStep {
            name: "s".into(),
            tool: "t".into(),
        }],
        vec![("workflow.ghost".to_string(), 1)],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    let err = s.register_workflow(wf).unwrap_err();
    assert!(matches!(err, AssetError::MissingDependency { .. }));
}

#[test]
fn m55_neg_workflow_permission_failure() {
    let mut s = store(&["fs.read"]);
    let wf = Workflow::new(
        "workflow.net",
        1,
        vec![WorkflowStep {
            name: "s".into(),
            tool: "net.call".into(),
        }],
        vec![],
        vec![],
        vec![],
        vec!["network.egress".into()],
        prov(),
    )
    .unwrap();
    let err = s.register_workflow(wf).unwrap_err();
    assert!(matches!(err, AssetError::PolicyViolation { .. }));
}

#[test]
fn m55_neg_tampered_candidate() {
    let c = candidate();
    let env = AssetEnvelope::seal(c);
    env.verify().unwrap();
    let mut tampered = env.clone();
    tampered.payload.review = ReviewState::Approved;
    assert!(matches!(
        tampered.verify().unwrap_err(),
        AssetError::DigestMismatch { .. }
    ));
}

// ---------- M10.6: multi-version skills + extended manifest ----------

#[test]
fn m10_multi_version_register_and_resolve() {
    let mut s = store(&[]);
    let v1 = skill("skill.mv", vec![], vec![]);
    let v2 = Skill::new(
        SkillId("skill.mv".into()),
        SkillVersion(2),
        "skill mv v2",
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    s.register_skill(v1).unwrap();
    s.register_skill(v2).unwrap();

    // Bare id resolves to the highest stored version — never a lower one.
    assert_eq!(
        s.get_skill(&SkillId("skill.mv".into())).unwrap().version,
        SkillVersion(2)
    );
    // Exact-version lookup stays exact.
    assert_eq!(
        s.get_skill_version(&SkillId("skill.mv".into()), SkillVersion(1))
            .unwrap()
            .version,
        SkillVersion(1)
    );
    assert_eq!(
        s.skill_versions(&SkillId("skill.mv".into())).unwrap(),
        vec![SkillVersion(1), SkillVersion(2)]
    );
    // (id, version) is the duplicate unit, not the bare id.
    let dup = Skill::new(
        SkillId("skill.mv".into()),
        SkillVersion(2),
        "again",
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    assert!(matches!(
        s.register_skill(dup),
        Err(AssetError::Duplicate { .. })
    ));
    assert_eq!(
        s.skill_states_all(),
        vec![
            ("skill.mv".to_string(), 1, "registered"),
            ("skill.mv".to_string(), 2, "registered"),
        ]
    );
}

#[test]
fn m10_neg_explicit_version_missing_never_substitutes() {
    let mut s = store(&[]);
    s.register_skill(skill("skill.ns", vec![], vec![])).unwrap();
    let id = SkillId("skill.ns".into());
    let missing = s.get_skill_version(&id, SkillVersion(7)).unwrap_err();
    assert!(matches!(&missing, AssetError::NotFound { kind, .. } if kind == "skill.version"));
    assert!(matches!(
        s.resolve_skill(&id, Some(SkillVersion(7))).unwrap_err(),
        AssetError::NotFound { .. }
    ));
    // Default (None) resolution still picks the stored version.
    assert_eq!(s.resolve_skill(&id, None).unwrap().version, SkillVersion(1));
}

#[test]
fn m10_dependency_resolves_across_versions() {
    let mut s = store(&[]);
    // dep b exists only at v1 while a requires b@2 → refused (found 1).
    s.register_skill(skill("skill.b", vec![], vec![])).unwrap();
    let dep = SkillDependency {
        id: SkillId("skill.b".into()),
        min_version: SkillVersion(2),
    };
    s.register_skill(skill("skill.a", vec![dep], vec![]))
        .unwrap();
    let err = s.validate_skill(&SkillId("skill.a".into())).unwrap_err();
    assert!(matches!(
        err,
        AssetError::IncompatibleDependency {
            required: 2,
            found: 1,
            ..
        }
    ));
    // A higher b arrives → the SAME dependency request now resolves.
    let b2 = Skill::new(
        SkillId("skill.b".into()),
        SkillVersion(2),
        "skill skill.b v2",
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    s.register_skill(b2).unwrap();
    s.validate_skill(&SkillId("skill.a".into())).unwrap();
}

#[test]
fn m10_manifest_fields_join_canonical_identity() {
    let plain = skill("skill.mf", vec![], vec![]);
    let mut rich = plain.clone();
    rich.goal = "compile the thing".into();
    rich.tools = vec!["filesystem.read".into()];
    rich.sandbox = Some("profile.strict".into());
    rich.trust = TRUST_VETTED.into();
    assert_ne!(plain.digest(), rich.digest());

    // Tampering a manifest field after sealing must break verification.
    let mut env = AssetEnvelope::seal(rich);
    env.payload.goal = "evil".into();
    assert!(matches!(
        env.verify(),
        Err(AssetError::DigestMismatch { .. })
    ));

    // Empty manifest fields never join the canonical form → legacy records
    // keep the digest they were sealed with.
    let pairs = plain.canonical_pairs();
    assert!(!pairs.iter().any(|(k, _)| matches!(
        k.as_str(),
        "name"
            | "goal"
            | "workflow"
            | "tools"
            | "models"
            | "sandbox"
            | "trust"
            | "compat"
            | "tests"
    )));
}

#[test]
fn m10_legacy_manifest_json_loads_with_fail_closed_defaults() {
    let s0 = skill("skill.legacy", vec![], vec![]);
    let legacy_digest = s0.digest();
    let mut val = serde_json::to_value(&s0).unwrap();
    // Simulate a record written before manifest fields existed.
    let obj = val.as_object_mut().unwrap();
    for k in [
        "name", "goal", "workflow", "tools", "models", "sandbox", "trust", "compat", "tests",
    ] {
        obj.remove(k);
    }
    let loaded: Skill = serde_json::from_value(val).unwrap();
    assert_eq!(loaded.trust_label(), "unvetted");
    assert!(loaded.workflow.is_none());
    assert!(loaded.tools.is_empty());
    assert_eq!(loaded.compat.openrt_min, "");
    // The old sealed digest still verifies after the manifest upgrade.
    let env = AssetEnvelope {
        payload: loaded,
        digest: legacy_digest,
    };
    env.verify().unwrap();
}

#[test]
fn m10_neg_register_rejects_bad_manifest() {
    let mut s = store(&[]);
    let mut bad_trust = skill("skill.bt", vec![], vec![]);
    bad_trust.trust = "supertrusted".into();
    assert!(matches!(
        s.register_skill(bad_trust),
        Err(AssetError::Malformed { .. })
    ));
    let mut bad_tool = skill("skill.btool", vec![], vec![]);
    bad_tool.tools = vec!["Bad Tool".into()];
    assert!(matches!(
        s.register_skill(bad_tool),
        Err(AssetError::Malformed { .. })
    ));
    let mut bad_compat = skill("skill.bcompat", vec![], vec![]);
    bad_compat.compat.openrt_min = ">=1.0".into();
    assert!(matches!(
        s.register_skill(bad_compat),
        Err(AssetError::Malformed { .. })
    ));
}

#[tokio::test]
async fn m10_multi_version_persist_round_trip() {
    let mut s = store(&[]);
    s.register_skill(skill("skill.pv", vec![], vec![])).unwrap();
    let v2 = Skill::new(
        SkillId("skill.pv".into()),
        SkillVersion(2),
        "skill pv v2",
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    s.register_skill(v2).unwrap();
    let mem = MemoryStore::default();
    s.save(&mem).await.unwrap();
    // Record keys are unique per (id, version).
    assert!(mem
        .get_record(openrt_workspace_assets::SKILLS_NS, "skill.pv.v1")
        .await
        .unwrap()
        .is_some());
    assert!(mem
        .get_record(openrt_workspace_assets::SKILLS_NS, "skill.pv.v2")
        .await
        .unwrap()
        .is_some());
    let loaded = AssetStore::load(&mem).await.unwrap();
    assert_eq!(
        loaded
            .get_skill_version(&SkillId("skill.pv".into()), SkillVersion(1))
            .unwrap()
            .version,
        SkillVersion(1)
    );
    assert_eq!(
        loaded
            .get_skill(&SkillId("skill.pv".into()))
            .unwrap()
            .version,
        SkillVersion(2)
    );
}

#[test]
fn m10_install_import_keeps_versions_and_coerces_activation() {
    let mut s = store(&[]);
    let id = SkillId("skill.imp".into());
    s.register_skill(skill("skill.imp", vec![], vec![]))
        .unwrap();
    let v2 = Skill::new(
        id.clone(),
        SkillVersion(2),
        "skill imp v2",
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    s.register_skill(v2).unwrap();
    s.validate_skill(&id).unwrap();
    s.activate_skill(&id, Some(&grant())).unwrap();

    let pkg = s.export_package().unwrap();
    let mut t = store(&[]);
    assert_eq!(t.import_package(&pkg).unwrap(), 2);
    // Imported can never arrive Active — the activation boundary is policy,
    // not package data.
    assert!(t
        .skill_states_all()
        .iter()
        .all(|(_, _, act)| *act == "registered"));
    assert_eq!(t.skill_versions(&id).unwrap().len(), 2);
    // Re-install of an identical package is a clean no-op.
    assert_eq!(t.import_package(&pkg).unwrap(), 0);
}

/// M10.7 ECC: `deny_rules` flattens ONLY enabled Deny rules (scope, subject,
/// id) for the runtime's deny-only gate layer — allow rules and disabled
/// rules never reach the gate.
#[test]
fn m10_deny_rules_take_only_enabled_denies() {
    let mut s = store(&["cap"]);
    s.register_rule(
        Rule::new(
            RuleId("rule.deny.on".into()),
            1,
            "gate",
            "echo",
            RuleEffect::Deny,
            10,
            true,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    s.register_rule(
        Rule::new(
            RuleId("rule.deny.off".into()),
            1,
            "gate",
            "system.info",
            RuleEffect::Deny,
            10,
            false,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    s.register_rule(
        Rule::new(
            RuleId("rule.allow.on".into()),
            1,
            "gate",
            "filesystem.read",
            RuleEffect::Allow,
            10,
            true,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    let mut rules = s.deny_rules();
    rules.sort();
    assert_eq!(
        rules,
        vec![(
            "gate".to_string(),
            "echo".to_string(),
            "rule.deny.on".to_string()
        )]
    );
}

/// M10.8: genome profile fields (model/prompt/tools/strategies/verifier/
/// permissions) round-trip; profile-less legacy genomes keep their digest
/// (upgrade-safe conditional pairs); profile content extends identity.
#[test]
fn m10_genome_profile_schema_is_upgrade_safe() {
    let g = AgentGenome::new(
        GenomeId("genome.p1".into()),
        1,
        vec![],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    let legacy_digest = g.digest();

    // Legacy JSON without profile fields loads with fail-closed defaults
    // and keeps the exact digest.
    let json = serde_json::json!({
        "id": "genome.p1",
        "version": 1,
        "skills": [],
        "instincts": [],
        "hooks": [],
        "rules": [],
        "provenance": { "origin": "run.test-1", "source": "src.test" }
    });
    let loaded: AgentGenome = serde_json::from_value(json).unwrap();
    assert_eq!(loaded.digest(), legacy_digest);
    assert!(loaded.model.is_none());
    assert!(loaded.tools.is_empty());
    assert!(loaded.permissions.is_empty());

    // Profile content changes identity (conditional canonical pairs).
    let withp = g
        .clone()
        .with_profile(
            Some("mock".into()),
            Some("be brief".into()),
            vec!["echo".into()],
            Some("recent".into()),
            Some("balanced".into()),
            Some("echo".into()),
            vec!["fs.read".into()],
        )
        .unwrap();
    assert_ne!(withp.digest(), legacy_digest);

    // Round-trip preserves fields + digest.
    let v = serde_json::to_value(&withp).unwrap();
    let back: AgentGenome = serde_json::from_value(v).unwrap();
    assert_eq!(back, withp);
    assert_eq!(back.digest(), withp.digest());
}

/// M10.8: blank profile strings and unsafe tool/permission ids fail closed
/// — at construction (`with_profile`) and again at registration.
#[test]
fn m10_genome_register_validates_profile() {
    let mut s = store(&[]);

    // with_profile rejects a blank model at construction.
    let blank = AgentGenome::new(
        GenomeId("genome.p2".into()),
        1,
        vec![],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap()
    .with_profile(Some("   ".into()), None, vec![], None, None, None, vec![]);
    assert!(matches!(blank, Err(AssetError::Malformed { .. })));

    // register_genome rejects a blank model set past the constructor.
    let mut blank2 = AgentGenome::new(
        GenomeId("genome.p2b".into()),
        1,
        vec![],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    blank2.model = Some("   ".to_string());
    assert!(matches!(
        s.register_genome(blank2),
        Err(AssetError::Malformed { .. })
    ));

    let mut unsafe_tool = AgentGenome::new(
        GenomeId("genome.p3".into()),
        1,
        vec![],
        vec![],
        vec![],
        vec![],
        prov(),
    )
    .unwrap();
    unsafe_tool.tools = vec!["bad/tool".to_string()];
    assert!(matches!(
        s.register_genome(unsafe_tool),
        Err(AssetError::Malformed { .. })
    ));
}
