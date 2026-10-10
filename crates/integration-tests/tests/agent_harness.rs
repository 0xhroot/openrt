//! Real agent harness end-to-end (see `docs/architecture/REAL_AGENT_HARNESS.md`):
//! through the ONE runtime → gate → policy path, the model receives tool
//! definitions, denied calls come back as structured model-visible feedback,
//! tool rounds persist into the session transcript, and iteration limits
//! fail honestly instead of hanging or pretending completion.

use std::sync::{Arc, Mutex};

use openrt_config::{Config, PermissionsConfig, RuleConfig, RuleEffect, RuntimeConfig};
use openrt_core::{EventKind, MessageRole, TaskStatus};
use openrt_models::error::ModelError;
use openrt_models::mock::MockProvider;
use openrt_models::provider::{ModelCapabilities, ModelProvider};
use openrt_models::types::{ModelInfo, ModelRequest, ModelResponse};
use openrt_persistence::MemoryStore;
use openrt_telemetry::ChannelSink;

/// Wraps the scripted mock and records every `ModelRequest.tools` payload
/// the agent actually sent — proof the harness advertises real schemas.
struct RecordingProvider {
    inner: MockProvider,
    seen: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
    /// system-prompt + tools payload bytes per generate call (local-model
    /// length budget).
    payload_bytes: Arc<Mutex<Vec<usize>>>,
}

#[async_trait::async_trait]
impl ModelProvider for RecordingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn info(&self) -> ModelInfo {
        self.inner.info()
    }
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let system_len = req.messages.first().map(|m| m.content.len()).unwrap_or(0);
        let tools_bytes: usize = req
            .tools
            .iter()
            .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
            .sum();
        self.payload_bytes
            .lock()
            .unwrap()
            .push(system_len + tools_bytes);
        self.seen.lock().unwrap().push(req.tools.clone());
        self.inner.generate(req).await
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }
    async fn probe_tool_support(&self, model: &str) -> Result<Option<bool>, ModelError> {
        self.inner.probe_tool_support(model).await
    }
}

fn recording(
    inner: MockProvider,
) -> (
    RecordingProvider,
    Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
    Arc<Mutex<Vec<usize>>>,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let payload_bytes = Arc::new(Mutex::new(Vec::new()));
    (
        RecordingProvider {
            inner,
            seen: seen.clone(),
            payload_bytes: payload_bytes.clone(),
        },
        seen,
        payload_bytes,
    )
}

async fn runtime_with(
    provider: Arc<dyn ModelProvider>,
    config: Config,
) -> Arc<openrt_runtime::Runtime> {
    openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(provider)
        .bus_capacity(32)
        .build()
        .await
        .unwrap()
}

/// The model request carries real tool schemas, filtered by policy: the
/// interactive developer profile advertises reads (allow) and mutations
/// (ask — visible so the operator decides), while deny-by-default tools
/// are withheld.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_receives_policy_filtered_tool_definitions() {
    let (rec, seen, _payload) = recording(MockProvider::tool_sequence("mock"));
    let rt = runtime_with(Arc::new(rec), Config::default()).await;
    let session = rt.create_session("tools").await.unwrap();
    let task = rt.run_task(session.id, "echo hello").await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");

    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty(), "at least one generate call");
    let first = &seen[0];
    let names: Vec<&str> = first
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(
        names.contains(&"echo"),
        "allowed tool advertised: {names:?}"
    );
    assert!(names.contains(&"filesystem.read"), "{names:?}");
    // Ask tools stay VISIBLE (the gate asks at execution).
    assert!(
        names.contains(&"terminal.run"),
        "ask tool visible to the model: {names:?}"
    );
    // Deny-by-default tools are withheld (nothing allows them).
    assert!(
        !names.contains(&"industrial.assets"),
        "deny-by-default tool withheld: {names:?}"
    );
    // Schemas are function-calling shaped, not bare names.
    let echo = first
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("echo"))
        .unwrap();
    assert!(echo.get("description").is_some(), "{echo:?}");
    assert!(echo.get("parameters").is_some(), "{echo:?}");
}

/// An explicit Deny rule removes the tool from the advertisement — the
/// gate's policy is the same authority that shaped the model's view.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_tool_is_withheld_from_advertisement() {
    let config = Config {
        permissions: PermissionsConfig {
            rules: vec![RuleConfig {
                effect: RuleEffect::Deny,
                capability: "echo".into(),
                resource: None,
            }],
        },
        ..Config::default()
    };
    let (rec, seen, _payload) = recording(MockProvider::tool_sequence("mock"));
    let rt = runtime_with(Arc::new(rec), config).await;
    let session = rt.create_session("deny-ad").await.unwrap();
    let _ = rt.run_task(session.id, "try echo").await.unwrap();

    let seen = seen.lock().unwrap();
    let names: Vec<&str> = seen[0]
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(!names.contains(&"echo"), "denied tool withheld: {names:?}");
}

/// The full tool round is persisted: user → assistant(tool_calls) → tool
/// result → final answer, all visible to follow-up tasks in the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_round_persists_in_session_transcript() {
    let rt = runtime_with(
        Arc::new(MockProvider::tool_sequence("mock")),
        Config::default(),
    )
    .await;
    let session = rt.create_session("round").await.unwrap();
    let task = rt.run_task(session.id, "use echo").await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");

    let s = rt.load_session(session.id).await.unwrap();
    assert_eq!(
        s.content.len(),
        4,
        "user + tool call + tool result + answer: {s:?}"
    );
    assert_eq!(s.content[0].role, MessageRole::User);
    assert_eq!(s.content[0].content, "use echo");
    assert!(
        !s.content[1].tool_calls.is_empty(),
        "assistant tool call turn"
    );
    assert_eq!(s.content[2].role, MessageRole::Tool);
    assert!(!s.content[3].content.is_empty(), "final answer");

    // Follow-up task sees the tool history as context.
    let task2 = rt.run_task(session.id, "and again").await.unwrap();
    assert_eq!(task2.status, TaskStatus::Completed);
    let s2 = rt.load_session(session.id).await.unwrap();
    assert!(
        s2.content.len() > s.content.len(),
        "transcript grows across tasks"
    );
}

/// A denied call is model-visible structured feedback, not a dead task:
/// PermissionDenied is audited and the model gets `{"ok":false,...}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_call_returns_structured_feedback_and_stays_alive() {
    let config = Config {
        permissions: PermissionsConfig { rules: Vec::new() },
        ..Config::default()
    };
    let sink = Arc::new(ChannelSink::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .sink(sink.clone())
        .bus_capacity(32)
        .build()
        .await
        .unwrap();
    let session = rt.create_session("deny-fb").await.unwrap();
    let task = rt.run_task(session.id, "use echo").await.unwrap();

    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    let result = task.result.as_deref().unwrap_or_default();
    assert!(result.contains("\"ok\":false"), "{result}");
    assert!(result.contains("permission denied"), "{result}");

    // The denial went through the gate's real audit path.
    let mut denied = false;
    for _ in 0..200 {
        if sink
            .snapshot()
            .iter()
            .any(|e| e.kind == EventKind::PermissionDenied)
        {
            denied = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(denied, "PermissionDenied must be audited");
    assert!(
        !sink
            .snapshot()
            .iter()
            .any(|e| e.kind == EventKind::ToolInvocationStarted),
        "denied tool never executed"
    );

    // The structured failure is in the persisted transcript as a tool turn.
    let s = rt.load_session(session.id).await.unwrap();
    let tool_msgs: Vec<_> = s
        .content
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 1, "{s:?}");
    assert!(tool_msgs[0].content.contains("\"ok\":false"));
}

/// Per-segment iteration limits no longer fail the task: the supervisor
/// checkpoints, continues, and the task completes honestly (hardening pass,
/// Part A). Global task ceilings are what fail a task explicitly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_segment_iteration_limit_continues_and_completes() {
    // max_tool_calls = 1: the first batch executes, the segment stops before
    // the second model turn, then the supervisor continues automatically.
    let config = Config {
        runtime: RuntimeConfig {
            max_turns: 6,
            max_tool_calls: 1,
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let rt = runtime_with(Arc::new(MockProvider::tool_sequence("mock")), config).await;
    let session = rt.create_session("limit").await.unwrap();
    let task = rt.run_task(session.id, "loop me").await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    assert!(task
        .result
        .as_deref()
        .unwrap_or_default()
        .contains("mock final"));

    // Two segments chained by the supervisor, each recorded in the ledger.
    assert_eq!(task.segments.len(), 2, "{:?}", task.segments);
    assert_eq!(task.segments[0].turns, 1);
    assert_eq!(task.segments[0].tool_calls, 1);
    assert_eq!(
        task.segments[1].outcome,
        openrt_core::SegmentOutcome::Completed
    );
    // The user instruction is persisted exactly once across segments.
    let s = rt.load_session(session.id).await.unwrap();
    let prompt_occurrences = s
        .content
        .iter()
        .filter(|m| m.role == MessageRole::User && m.content.contains("loop me"))
        .count();
    assert_eq!(prompt_occurrences, 1, "instruction duplicated: {s:?}");
}

/// A GLOBAL task turn ceiling fails the task with an honest reason — no hang,
/// no fake completion, no fabricating a result (hardening pass, Part A
/// `runtime.max_task_turns`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_task_budget_fails_task_honestly() {
    let config = Config {
        runtime: RuntimeConfig {
            max_turns: 6,
            max_tool_calls: 1,
            max_task_turns: Some(2),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let rt = runtime_with(Arc::new(MockProvider::tool_sequence("mock")), config).await;
    let session = rt.create_session("limit").await.unwrap();
    let task = rt.run_task(session.id, "loop me").await.unwrap();
    assert_eq!(task.status, TaskStatus::Failed, "{task:?}");
    let err = task.error.as_deref().unwrap_or_default();
    assert!(
        err.contains("global task execution budget exhausted"),
        "{err}"
    );
    assert!(err.contains("global turn ceiling 2"), "{err}");
    assert!(
        task.result.is_none(),
        "no fabricated result on a budget failure"
    );

    // The partial transcript is persisted with the failure marker — the
    // operator can see exactly where it stopped.
    let s = rt.load_session(session.id).await.unwrap();
    assert!(
        s.content.iter().any(|m| m.content.contains("task Failed")),
        "{s:?}"
    );
}

/// `runtime.max_tool_calls = 0` is a configuration error, not a silent
/// infinite loop.
#[test]
fn zero_max_tool_calls_is_rejected() {
    let config = Config {
        runtime: RuntimeConfig {
            max_tool_calls: 0,
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("max_tool_calls"), "{err}");
}

/// Local-model payload budget: llama3.1:8b via Ollama measurably degrades
/// (malformed tool-call JSON leaked as plain text, wrong tool chosen,
/// planning prose instead of calls) once the system + tools payload passes
/// roughly 19.5 KB. Every request the agent sends must stay under that
/// cliff. See `environment_system_prompt_stays_within_local_model_budget`
/// for the prompt half and this for the tools half.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_payload_stays_within_local_model_budget() {
    let (rec, _seen, payload) = recording(MockProvider::tool_sequence("mock"));
    let mut config = Config::default();
    config.runtime.workspace_dir = Some(std::path::PathBuf::from(
        "/tmp/opencode/workspace-with-a-realistic-long-path",
    ));
    let rt = runtime_with(Arc::new(rec), config).await;
    let session = rt.create_session("budget").await.unwrap();
    let task = rt.run_task(session.id, "echo hello").await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");

    let sizes = payload.lock().unwrap();
    assert!(!sizes.is_empty(), "at least one generate call");
    for (i, bytes) in sizes.iter().enumerate() {
        assert!(
            *bytes <= 19_700,
            "request {i} payload {bytes} bytes exceeds the local-model budget (cliff ~19.8 KB)"
        );
    }
}
