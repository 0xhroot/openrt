//! M10.6 skill loader + execution — the single production path a skill may
//! take. Loading validates digest, manifest (permissions/trust/compat), tools,
//! models, sandbox profile, and workflow executability fail-closed; execution
//! requires the Active state and runs only through the existing
//! WorkflowEngine/ToolRegistry/PolicyEvaluator/SandboxManager authorities
//! (never a parallel engine, never a privilege the gate would deny).
//!
//! Effective-permission intersection: a skill runs with
//! `declared ∩ policy-allowed` (this loader) ∩ tool-requirements ∩
//! sandbox-profile (both enforced later by the gate during the run).

use openrt_permissions::{Capability, PermissionRequest, PolicyEvaluator};
use openrt_workspace_assets::{
    CanonicalForm, Marketplace, MarketplaceEntry, Skill, SkillId, SkillTest, SkillTestKind,
    SkillVersion,
};

use crate::runtime::Runtime;
use crate::Error;

/// Engine capability ids a skill manifest may declare in `compat.engines`.
const KNOWN_ENGINES: &[&str] = &["agent", "workflow", "tools"];

fn check(name: &str, ok: bool, detail: impl Into<String>) -> crate::skill::SkillLoadCheck {
    crate::skill::SkillLoadCheck {
        name: name.to_string(),
        ok,
        detail: detail.into(),
    }
}

/// Parse the leading `major[.minor[.patch]]` of a version-ish string.
fn version_tuple(v: &str) -> (u64, u64, u64) {
    let mut parts = v.split(['.', '-', '+']);
    let mut next = || {
        parts
            .next()
            .and_then(|p| p.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let major = next();
    let minor = next();
    let patch = next();
    (major, minor, patch)
}

/// True when `current` >= `min` (empty `min` = unspecified = always true).
fn version_at_least(current: &str, min: &str) -> bool {
    if min.is_empty() {
        return true;
    }
    version_tuple(current) >= version_tuple(min)
}

/// One loader check verdict (M10.6) — surfaced verbatim by CLI/doctor/TUI.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillLoadCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Full loader verdict for one exact skill version.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillLoadReport {
    pub id: String,
    pub version: u32,
    pub digest: String,
    pub description: String,
    pub goal: String,
    pub activation: String,
    pub trust: String,
    pub declared_permissions: Vec<String>,
    /// Declared ∩ policy-allowed — the only permissions execution may use.
    pub effective_permissions: Vec<String>,
    pub tools: Vec<String>,
    pub missing_tools: Vec<String>,
    pub models: Vec<String>,
    pub model_ok: bool,
    pub sandbox: Option<String>,
    pub sandbox_ok: bool,
    pub compat_ok: bool,
    pub engines_ok: bool,
    pub workflow: Option<String>,
    pub workflow_executable: bool,
    pub checks: Vec<SkillLoadCheck>,
    pub passed: bool,
}

impl SkillLoadReport {
    fn failed(&self) -> Vec<&SkillLoadCheck> {
        self.checks.iter().filter(|c| !c.ok).collect()
    }
}

/// Result of a production skill execution run.
#[derive(Debug)]
pub struct SkillExecution {
    pub id: String,
    pub version: u32,
    pub mode: String,
    pub effective_permissions: Vec<String>,
    pub workflow_report: Option<openrt_orch::WorkflowRunReport>,
    pub task: Option<openrt_core::Task>,
}

impl Runtime {
    /// Inspect one exact skill version through the loader without requiring
    /// every check to pass (operator surfaces need to SEE failures). Unknown
    /// id/version fails closed (`NotFound`); a version that is not stored is
    /// never substituted.
    pub async fn skill_inspect(
        &self,
        id: &str,
        version: Option<u32>,
    ) -> Result<SkillLoadReport, Error> {
        let assets = self.load_assets().await?;
        let sid = SkillId(id.to_string());
        let requested = version.map(SkillVersion);
        // Store-side resolve verifies the envelope digest (tampered records
        // surface as DigestMismatch before anything else runs).
        let skill: &Skill = assets.resolve_skill(&sid, requested)?;

        let mut checks: Vec<SkillLoadCheck> = Vec::new();
        checks.push(check(
            "digest",
            true,
            format!("seal digest {}", skill.digest()),
        ));

        match skill.validate_manifest() {
            Ok(()) => checks.push(check("manifest", true, "manifest fields valid")),
            Err(e) => checks.push(check("manifest", false, e.to_string())),
        }

        // Same PolicyEvaluator the gate enforces — no second policy authority.
        let policy = self.permission_policy.clone();
        let mut effective: Vec<String> = Vec::new();
        let mut denied: Vec<String> = Vec::new();
        for p in &skill.permissions {
            let allows = PolicyEvaluator::new(&policy)
                .evaluate(&PermissionRequest::capability(
                    "skill.load",
                    Capability::from_str_unchecked(p),
                    None,
                ))
                .allows_execution();
            if allows {
                effective.push(p.clone());
            } else {
                denied.push(p.clone());
            }
        }
        checks.push(check(
            "permissions",
            denied.is_empty(),
            if denied.is_empty() {
                "all declared permissions allowed by policy".to_string()
            } else {
                format!("denied by policy: {}", denied.join(","))
            },
        ));

        let missing_tools: Vec<String> = skill
            .tools
            .iter()
            .filter(|t| self.registry.get(t).is_none())
            .cloned()
            .collect();
        checks.push(check(
            "tools",
            missing_tools.is_empty(),
            if missing_tools.is_empty() {
                "all declared tools registered".to_string()
            } else {
                format!("not registered: {}", missing_tools.join(","))
            },
        ));

        let provider_id = self.provider.id();
        let model_ok = skill.models.is_empty() || skill.models.iter().any(|m| m == provider_id);
        checks.push(check(
            "models",
            model_ok,
            if skill.models.is_empty() {
                format!("unspecified; runtime serves `{provider_id}`")
            } else if model_ok {
                format!("served by runtime provider `{provider_id}`")
            } else {
                format!(
                    "requires [{}] but runtime serves `{provider_id}`",
                    skill.models.join(",")
                )
            },
        ));

        let sandbox_ok = match &skill.sandbox {
            None => true,
            Some(name) => openrt_sandbox::SandboxProfile::from_name(name).is_some(),
        };
        checks.push(check(
            "sandbox",
            sandbox_ok,
            match &skill.sandbox {
                None => "no profile declared (policy default)".to_string(),
                Some(name) if sandbox_ok => format!("profile `{name}` exists"),
                Some(name) => format!("unknown sandbox profile `{name}`"),
            },
        ));

        let compat_ok = version_at_least(env!("CARGO_PKG_VERSION"), &skill.compat.openrt_min);
        checks.push(check(
            "compat",
            compat_ok,
            if skill.compat.openrt_min.is_empty() {
                "no minimum declared".to_string()
            } else {
                format!(
                    "requires openrt >= {}; runtime {}",
                    skill.compat.openrt_min,
                    env!("CARGO_PKG_VERSION")
                )
            },
        ));

        let engines_ok = skill
            .compat
            .engines
            .iter()
            .all(|e| KNOWN_ENGINES.contains(&e.as_str()));
        checks.push(check(
            "engines",
            engines_ok,
            if skill.compat.engines.is_empty() {
                "no engine constraints".to_string()
            } else if engines_ok {
                format!("engines [{}] known", skill.compat.engines.join(","))
            } else {
                format!(
                    "unknown engines [{}]; known: {}",
                    skill.compat.engines.join(","),
                    KNOWN_ENGINES.join(",")
                )
            },
        ));

        // Workflow executability: the ONLY execution path for a workflow-backed
        // skill is the compiled DAG (WorkflowEngine). No compiled graph = the
        // skill cannot run — reported, never improvised.
        let mut workflow_executable = true;
        let mut workflow_detail = "no workflow declared (agent mode)".to_string();
        if let Some(wf) = &skill.workflow {
            match openrt_compiler::load_dag_workflow(&self.store, wf).await? {
                Some(_) => {
                    workflow_detail = format!("compiled graph `{wf}` present");
                }
                None => {
                    workflow_executable = false;
                    workflow_detail = format!("no compiled graph for `{wf}` in namespace `dag`");
                }
            }
        }
        checks.push(check("workflow", workflow_executable, workflow_detail));

        let passed = checks.iter().all(|c| c.ok);
        Ok(SkillLoadReport {
            id: skill.id.0.clone(),
            version: skill.version.0,
            digest: skill.digest(),
            description: skill.description.clone(),
            goal: skill.goal.clone(),
            activation: skill.activation.as_str().to_string(),
            trust: skill.trust_label().to_string(),
            declared_permissions: skill.permissions.clone(),
            effective_permissions: effective,
            tools: skill.tools.clone(),
            missing_tools,
            models: skill.models.clone(),
            model_ok,
            sandbox: skill.sandbox.clone(),
            sandbox_ok,
            compat_ok,
            engines_ok,
            workflow: skill.workflow.clone(),
            workflow_executable,
            checks,
            passed,
        })
    }

    /// Fail-closed loader: every check must pass or the skill is refused
    /// with the failing checks named.
    pub async fn skill_load(
        &self,
        id: &str,
        version: Option<u32>,
    ) -> Result<SkillLoadReport, Error> {
        let report = self.skill_inspect(id, version).await?;
        if report.passed {
            return Ok(report);
        }
        let failed = report
            .failed()
            .iter()
            .map(|c| format!("{}: {}", c.name, c.detail))
            .collect::<Vec<_>>()
            .join("; ");
        Err(Error::Skill(format!(
            "skill {}@{} refused by loader — {failed}",
            report.id, report.version
        )))
    }

    /// Execute an ACTIVE skill through the production path only:
    /// - workflow-backed skills run their compiled DAG via `run_compiled`
    ///   (WorkflowEngine + gate + checkpoints + resource admission);
    /// - otherwise the goal/description drives a normal gated `run_task`.
    /// Imported/registered-but-not-activated skills can never execute.
    pub async fn skill_execute(
        &self,
        session_id: openrt_core::SessionId,
        id: &str,
        version: Option<u32>,
    ) -> Result<SkillExecution, Error> {
        self.skill_execute_input(session_id, id, version, None)
            .await
    }

    /// [`Runtime::skill_execute`] with optional invoker *input* appended to
    /// the agent-mode prompt. Workflow-backed skills accept no input (their
    /// DAG is fixed) — providing input fails closed instead of silently
    /// dropping it. Phase 11: the ONE execution path behind extension tool
    /// bindings.
    pub async fn skill_execute_input(
        &self,
        session_id: openrt_core::SessionId,
        id: &str,
        version: Option<u32>,
        input: Option<&str>,
    ) -> Result<SkillExecution, Error> {
        let report = self.skill_load(id, version).await?;
        if report.activation != "active" {
            return Err(Error::Skill(format!(
                "skill {}@{} is `{}` — an operator must activate it before execution",
                report.id, report.version, report.activation
            )));
        }
        match &report.workflow {
            Some(wf) => {
                if input.is_some() {
                    return Err(Error::Skill(format!(
                        "skill {}@{} is workflow-backed — it accepts no input",
                        report.id, report.version
                    )));
                }
                let workflow_report = self.run_compiled(session_id, wf).await?;
                Ok(SkillExecution {
                    id: report.id,
                    version: report.version,
                    mode: "workflow".to_string(),
                    effective_permissions: report.effective_permissions,
                    workflow_report: Some(workflow_report),
                    task: None,
                })
            }
            None => {
                let mut prompt = if !report.goal.is_empty() {
                    report.goal.clone()
                } else {
                    report.description.clone()
                };
                if let Some(input) = input {
                    prompt.push_str("\n\nInput:\n");
                    prompt.push_str(input);
                }
                let task = self.run_task(session_id, &prompt).await?;
                Ok(SkillExecution {
                    id: report.id,
                    version: report.version,
                    mode: "agent".to_string(),
                    effective_permissions: report.effective_permissions,
                    workflow_report: None,
                    task: Some(task),
                })
            }
        }
    }
}

/// One skill-test verdict (M10.6).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillTestResult {
    pub id: String,
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Full run of every test attached to one exact skill version.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillTestReport {
    pub skill_id: String,
    pub version: u32,
    pub passed: bool,
    pub results: Vec<SkillTestResult>,
}

impl Runtime {
    /// Install a declarative skill test (attached to an exact skill version;
    /// the referenced skill/version must already exist — fail-closed).
    pub async fn assets_install_skill_test(&self, test: SkillTest) -> Result<(), Error> {
        let mut assets = self.load_assets().await?;
        assets.register_skill_test(test)?;
        self.save_assets(&assets).await?;
        Ok(())
    }

    /// M10.6 skill→workflow compilation: compile the skill's goal through the
    /// production WorkflowCompiler, then link the resulting DAG back onto the
    /// exact skill version (reseal + Active → Validated demotion: the operator
    /// must re-approve relinked content before it can execute).
    pub async fn skill_compile(&self, id: &str, version: Option<u32>) -> Result<String, Error> {
        let report = self.skill_load(id, version).await?;
        let goal = if !report.goal.is_empty() {
            report.goal.clone()
        } else {
            report.description.clone()
        };
        let out = self
            .compile_workflow(&goal, openrt_compiler::CompileMode::Compile)
            .await?;
        let mut assets = self.load_assets().await?;
        assets.link_skill_workflow(
            &SkillId(report.id.clone()),
            SkillVersion(report.version),
            out.workflow_id.clone(),
        )?;
        self.save_assets(&assets).await?;
        Ok(out.workflow_id)
    }

    /// Run every test attached to the resolved skill version through the
    /// production loader/registry (read-only verdicts — tests can fail a
    /// skill, never widen it).
    pub async fn skill_test_run(
        &self,
        skill_id: &str,
        version: Option<u32>,
    ) -> Result<SkillTestReport, Error> {
        let assets = self.load_assets().await?;
        let sid = SkillId(skill_id.to_string());
        let skill = assets.resolve_skill(&sid, version.map(SkillVersion))?;
        let ver = skill.version.0;
        let trust = skill.trust_label().to_string();
        let activation = skill.activation.as_str().to_string();
        let declared: Vec<String> = skill.permissions.clone();
        let tests = assets.skill_tests_for(&sid, SkillVersion(ver));
        drop(assets);

        let mut results = Vec::new();
        for t in tests {
            let (ok, detail) = match &t.kind {
                SkillTestKind::Loads => match self.skill_inspect(skill_id, Some(ver)).await {
                    Ok(r) => (
                        r.passed,
                        if r.passed {
                            "loader passed".to_string()
                        } else {
                            format!(
                                "loader failed: {}",
                                r.checks
                                    .iter()
                                    .filter(|c| !c.ok)
                                    .map(|c| c.name.clone())
                                    .collect::<Vec<_>>()
                                    .join(",")
                            )
                        },
                    ),
                    Err(e) => (false, e.to_string()),
                },
                SkillTestKind::TrustIs(exp) => {
                    (trust == *exp, format!("trust={trust}, expected={exp}"))
                }
                SkillTestKind::ToolRegistered(name) => {
                    let ok = self.registry.get(name).is_some();
                    (
                        ok,
                        if ok {
                            format!("tool `{name}` registered")
                        } else {
                            format!("tool `{name}` missing")
                        },
                    )
                }
                SkillTestKind::PermissionDeclared(p) => {
                    let ok = declared.iter().any(|d| d == p);
                    (
                        ok,
                        if ok {
                            format!("permission `{p}` declared")
                        } else {
                            format!("permission `{p}` not declared")
                        },
                    )
                }
                SkillTestKind::ActivationIs(a) => (
                    activation == *a,
                    format!("activation={activation}, expected={a}"),
                ),
            };
            results.push(SkillTestResult {
                id: t.id.clone(),
                name: t.name.clone(),
                ok,
                detail,
            });
        }
        let passed = results.iter().all(|r| r.ok);
        Ok(SkillTestReport {
            skill_id: skill_id.to_string(),
            version: ver,
            passed,
            results,
        })
    }

    /// Local (offline) marketplace catalog of installed skills.
    pub async fn skill_marketplace_catalog(&self) -> Result<Vec<MarketplaceEntry>, Error> {
        let assets = self.load_assets().await?;
        Ok(Marketplace::local_catalog(&assets))
    }

    /// Remote marketplace fetch — honestly UNAVAILABLE in this build
    /// (no network client; Sovereign policy would block it anyway).
    pub async fn skill_marketplace_fetch(&self, url: &str) -> Result<Vec<MarketplaceEntry>, Error> {
        Marketplace::remote_fetch(url)
            .await
            .map_err(|e| Error::Skill(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use openrt_models::MockProvider;
    use openrt_persistence::MemoryStore;
    use openrt_workspace_assets::{
        Provenance, Skill, SkillId, SkillVersion, TRUST_SIGNED, TRUST_UNVETTED, TRUST_VETTED,
    };

    async fn rt() -> Arc<Runtime> {
        crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::echo("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap()
    }

    fn prov() -> Provenance {
        Provenance::new("run.skill", "src.skill")
    }

    fn manifest_skill(id: &str) -> Skill {
        let mut s = Skill::new(
            SkillId(id.to_string()),
            SkillVersion(1),
            "demo manifest skill",
            vec!["fs.read".to_string()],
            vec![],
            prov(),
        )
        .unwrap();
        s.goal = "say hello".to_string();
        s.tools = vec!["echo".to_string()];
        s.sandbox = Some("strict".to_string());
        s.trust = TRUST_VETTED.to_string();
        s
    }

    #[tokio::test]
    async fn loader_passes_for_healthy_skill() {
        let rt = rt().await;
        rt.assets_install_skill(manifest_skill("skill.ok"))
            .await
            .unwrap();
        let report = rt.skill_load("skill.ok", None).await.unwrap();
        assert!(report.passed, "checks: {:?}", report.checks);
        // Effective = declared ∩ policy: fs.read allowed by default config.
        assert_eq!(report.effective_permissions, vec!["fs.read".to_string()]);
        assert_eq!(report.trust, TRUST_VETTED);
        assert_eq!(report.sandbox.as_deref(), Some("strict"));
        assert_eq!(report.activation, "registered");
        assert_eq!(report.checks.len(), 9);
    }

    #[tokio::test]
    async fn neg_loader_refuses_bad_tool_model_compat() {
        let rt = rt().await;
        let mut s = manifest_skill("skill.bad");
        s.tools = vec!["nope.tool".to_string()];
        s.models = vec!["gpt.x".to_string()];
        s.compat.openrt_min = "99.0.0".to_string();
        rt.assets_install_skill(s).await.unwrap();

        let report = rt.skill_inspect("skill.bad", None).await.unwrap();
        assert!(!report.passed);
        assert!(!report.model_ok);
        assert!(!report.compat_ok);
        assert_eq!(report.missing_tools, vec!["nope.tool".to_string()]);
        let failed: Vec<&str> = report
            .checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(failed, vec!["tools", "models", "compat"]);

        let err = rt.skill_load("skill.bad", None).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("tools"), "{msg}");
        assert!(msg.contains("models"), "{msg}");
        assert!(msg.contains("compat"), "{msg}");
    }

    #[tokio::test]
    async fn neg_unknown_version_is_never_substituted() {
        let rt = rt().await;
        rt.assets_install_skill(manifest_skill("skill.ver"))
            .await
            .unwrap();
        let err = rt.skill_load("skill.ver", Some(7)).await.unwrap_err();
        assert!(err.to_string().contains("skill.ver@7"), "{err}");
        // Default resolution still works.
        let ok = rt.skill_load("skill.ver", None).await.unwrap();
        assert_eq!(ok.version, 1);
    }

    #[tokio::test]
    async fn neg_execution_requires_operator_activation() {
        let rt = rt().await;
        rt.assets_install_skill(manifest_skill("skill.inactive"))
            .await
            .unwrap();
        let session = rt.create_session("skill exec").await.unwrap();
        let err = rt
            .skill_execute(session.id, "skill.inactive", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("activate"), "{err}");

        // Operator activation unlocks execution through the gated run path.
        let op = std::env::var("USER").unwrap_or_else(|_| "openrt".to_string());
        rt.assets_validate_skill("skill.inactive").await.unwrap();
        rt.assets_activate_skill("skill.inactive", &op)
            .await
            .unwrap();
        let exec = rt
            .skill_execute(session.id, "skill.inactive", None)
            .await
            .unwrap();
        assert_eq!(exec.mode, "agent");
        assert_eq!(exec.effective_permissions, vec!["fs.read".to_string()]);
        let task = exec.task.expect("agent mode returns a task");
        assert_eq!(task.status, openrt_core::TaskStatus::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn skill_compile_links_graph_and_execution_runs_it() {
        use openrt_config::{PermissionsConfig, RuleConfig, RuleEffect};

        let mut cfg = openrt_config::Config::default();
        cfg.permissions = PermissionsConfig {
            rules: [
                "echo",
                "system.info",
                "system.info.all",
                "filesystem.read",
                "filesystem.read.all",
                "filesystem.write",
                "filesystem.write.all",
                "fs.read",
                "tool.run",
                "tool.other",
            ]
            .iter()
            .map(|c| RuleConfig {
                effect: RuleEffect::Allow,
                capability: c.to_string(),
                resource: None,
            })
            .collect(),
        };
        let rt = crate::RuntimeBuilder::new()
            .config(cfg)
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::echo("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();

        let mut s = manifest_skill("skill.wf");
        s.goal = "Read report.txt then write notes.md".to_string();
        rt.assets_install_skill(s).await.unwrap();
        rt.assets_validate_skill("skill.wf").await.unwrap();
        let op = std::env::var("USER").unwrap_or_else(|_| "openrt".to_string());
        rt.assets_activate_skill("skill.wf", &op).await.unwrap();

        // Production skill→workflow compilation: links the DAG back onto the
        // exact version and DEMOTES the approval (content changed under an
        // old approval — execution must stop until re-approved).
        let wf = rt.skill_compile("skill.wf", None).await.unwrap();
        let report = rt.skill_inspect("skill.wf", None).await.unwrap();
        assert_eq!(report.activation, "validated");
        assert_eq!(report.workflow.as_deref(), Some(wf.as_str()));
        assert!(report.workflow_executable, "checks: {:?}", report.checks);

        // Execution stays refused until the operator re-approves.
        let session = rt.create_session("skill wf").await.unwrap();
        let err = rt
            .skill_execute(session.id, "skill.wf", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("activate"), "{err}");

        rt.assets_activate_skill("skill.wf", &op).await.unwrap();
        let exec = rt
            .skill_execute(session.id, "skill.wf", None)
            .await
            .unwrap();
        assert_eq!(exec.mode, "workflow");
        let wfr = exec.workflow_report.expect("workflow mode runs the DAG");
        assert_eq!(wfr.workflow_id, wf);
    }

    #[tokio::test]
    async fn skill_test_run_reports_named_verdicts() {
        let rt = rt().await;
        rt.assets_install_skill(manifest_skill("skill.st"))
            .await
            .unwrap();
        rt.assets_validate_skill("skill.st").await.unwrap();

        let prov = Provenance::new("run.st", "src.st");
        let sid = SkillId("skill.st".into());
        let mk = |tid: &str, kind: SkillTestKind| {
            SkillTest::new(tid, sid.clone(), SkillVersion(1), tid, kind, prov.clone()).unwrap()
        };
        rt.assets_install_skill_test(mk("st.loads", SkillTestKind::Loads))
            .await
            .unwrap();
        rt.assets_install_skill_test(mk("st.tool", SkillTestKind::ToolRegistered("echo".into())))
            .await
            .unwrap();
        rt.assets_install_skill_test(mk(
            "st.badtool",
            SkillTestKind::ToolRegistered("nope.tool".into()),
        ))
        .await
        .unwrap();
        rt.assets_install_skill_test(mk("st.act", SkillTestKind::ActivationIs("active".into())))
            .await
            .unwrap();

        let rep = rt.skill_test_run("skill.st", None).await.unwrap();
        assert_eq!(rep.version, 1);
        assert_eq!(rep.results.len(), 4);
        assert!(!rep.passed);
        let failed: Vec<&str> = rep
            .results
            .iter()
            .filter(|r| !r.ok)
            .map(|r| r.id.as_str())
            .collect();
        // Deterministic (id) order: activation is "validated" here, so st.act
        // and the missing tool fail while loads/tool pass.
        assert_eq!(failed, vec!["st.act", "st.badtool"]);

        // Unknown skill / unknown version fail closed.
        assert!(rt.skill_test_run("skill.none", None).await.is_err());
        assert!(rt.skill_test_run("skill.st", Some(9)).await.is_err());
        // Attaching a test to a non-existent version is refused up front.
        let orphan = SkillTest::new(
            "st.orphan",
            SkillId("skill.st".into()),
            SkillVersion(9),
            "orphan",
            SkillTestKind::Loads,
            Provenance::new("run.st", "src.st"),
        )
        .unwrap();
        assert!(rt.assets_install_skill_test(orphan).await.is_err());
    }

    #[tokio::test]
    async fn marketplace_local_catalog_and_remote_unavailable() {
        let rt = rt().await;
        rt.assets_install_skill(manifest_skill("skill.mk"))
            .await
            .unwrap();
        let cat = rt.skill_marketplace_catalog().await.unwrap();
        assert!(cat
            .iter()
            .any(|e| e.id == "skill.mk" && e.source == "local" && e.trust == TRUST_VETTED));
        let err = rt
            .skill_marketplace_fetch("https://example.invalid/catalog")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("UNAVAILABLE"), "{err}");
    }

    #[tokio::test]
    async fn neg_workflow_skill_without_compiled_graph_fails_closed() {
        let rt = rt().await;
        let mut s = manifest_skill("skill.wfmiss");
        s.workflow = Some("wf.never.compiled".to_string());
        rt.assets_install_skill(s).await.unwrap();
        let report = rt.skill_inspect("skill.wfmiss", None).await.unwrap();
        assert!(!report.passed);
        assert!(!report.workflow_executable);
        assert!(rt.skill_load("skill.wfmiss", None).await.is_err());
    }

    #[tokio::test]
    async fn version_gate_semantics() {
        assert!(version_at_least("0.1.0", ""));
        assert!(version_at_least("1.2.3", "1.2.3"));
        assert!(version_at_least("1.3.0", "1.2.9"));
        assert!(!version_at_least("1.2.0", "1.3.0"));
        assert!(!version_at_least("0.9.0", "1.0.0"));
        // Legacy/unspecified trust collapses to unvetted everywhere.
        let mut s = manifest_skill("skill.t");
        s.trust = String::new();
        assert_eq!(s.trust_label(), TRUST_UNVETTED);
        assert_eq!(TRUST_SIGNED, "signed");
    }
}
