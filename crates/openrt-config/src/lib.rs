//! OpenRT configuration model, loading and validation.
//!
//! Config is authoritative for how a runtime is wired: providers, permission
//! defaults, resource limits, persistence backend, telemetry level and the
//! operating-mode concept (`sovereign` / `provisioning`). Phase 1 only carries
//! the concept of the mode; the full engine is Phase 3 (ADR-003).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Runtime-level settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Identifier of the provider port to use by default.
    pub default_provider: String,
    /// Identifier of the agent to use by default.
    pub default_agent: String,
    /// Maximum tool/model turns per task.
    pub max_turns: u32,
    /// Maximum tool calls per task before the agent stops at
    /// `AgentEndReason::LimitReached` (bounds a tool-looping model).
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    /// Timeout for a single model invocation, in seconds.
    pub model_timeout_secs: u64,
    /// Workspace directory tools are allowed to touch (scoped fs.read).
    pub workspace_dir: Option<PathBuf>,
    /// `terminal.run` wall-clock timeout in seconds (default 30).
    pub terminal_timeout_secs: Option<u64>,
    /// Per-stream output byte cap for `terminal.run` (default 65536).
    pub terminal_max_output_bytes: Option<usize>,
    /// Optional overrides for the `python.run` sandbox (defaults when `None`).
    pub python_sandbox: Option<PythonSandboxConfig>,
    /// Screen backend for the M2.5 `screen.*`/`input.*`/`browser.*` tools:
    /// `"x11"` (Xvfb/xdotool when a display is available) or `"sim"`
    /// (deterministic in-memory screen; the default and only choice on hosts
    /// without X tooling).
    pub screen_backend: Option<String>,
    /// P7 Tool Fabric backend override for the `computer.*` tools:
    /// `"sim"` forces the deterministic harness (tests/CI); `None`/`"auto"`
    /// discovers the environment and resolves through the six-layer priority
    /// (native compositor → a11y → portal → display server → input injection
    /// → configured shortcut). Explicit ids: `"hyprland"`, `"sway"`, `"i3"`,
    /// `"bspwm"`, `"kde"`, `"gnome"`, `"wayland"`, `"x11"`, `"windows"`.
    pub computer_backend: Option<String>,
    /// Part C: initial backoff (ms) before the first provider retry after a
    /// transient error (default 500). Retries delay `base * 2^(attempt-1)`,
    /// capped at [`Self::agent_retry_backoff_max_ms`], and stay bounded so a
    /// degraded provider never spins hot.
    #[serde(default = "default_agent_retry_backoff_base_ms")]
    pub agent_retry_backoff_base_ms: u64,
    /// Part C: ceiling (ms) for the exponential retry backoff (default
    /// 30000). `>= base` is validated at load.
    #[serde(default = "default_agent_retry_backoff_max_ms")]
    pub agent_retry_backoff_max_ms: u64,
    /// Part C: per-generation idle budget (ms) for *streaming* providers —
    /// no token delta for this long is a transient error subject to backoff
    /// retries, never a silent completion. `None` disables the watchdog
    /// (the default): a long-running agent is never terminated by an
    /// artificial 5-10 s deadline. Applies only when the active provider
    /// advertises `supports_streaming`; otherwise the model timeout governs.
    pub agent_idle_timeout_ms: Option<u64>,
    /// Part C: fraction of the provider's *declared* context window treated
    /// as the guard budget the assembled request may not exceed (default
    /// 0.7). When exceeded, the agent compacts the oldest turns with an
    /// explicit marker — never silent truncation, never a fabricated window.
    #[serde(default = "default_agent_context_guard_ratio")]
    pub agent_context_guard_ratio: f32,
    /// GLOBAL task safety budgets (the hardening pass supervisor). The
    /// per-segment agent budgets (`max_turns` / `max_tool_calls`) bound ONE
    /// execution segment; these four bound the WHOLE task across the
    /// automatically-chained segments. Finite defaults keep the default
    /// configuration from being an unbounded runaway; `None` relaxes a
    /// ceiling for an operator who wants it. A segment that hits its per-run
    /// budget no longer fails the task — it checkpoints and continues, so a
    /// task keeps running until it completes, is cancelled/paused, hits an
    /// unrecoverable error, or exhausts one of these global budgets.
    #[serde(default = "default_max_task_turns")]
    pub max_task_turns: Option<u32>,
    /// GLOBAL tool-call ceiling across all segments (default 8192).
    #[serde(default = "default_max_task_tool_calls")]
    pub max_task_tool_calls: Option<u32>,
    /// GLOBAL wall-clock ceiling (ms) for the whole task (default 24 h). A
    /// task over this is Failed with an explicit "global task execution
    /// budget exhausted" reason — never silently truncated.
    #[serde(default = "default_max_task_wall_time")]
    pub max_task_wall_time: Option<u64>,
    /// GLOBAL segment-count ceiling for automatic continuation (default 128).
    /// This is the hard anti-runaway bound: a model that never completes and
    /// keeps burning segments stops here, with all state preserved.
    #[serde(default = "default_max_task_iterations")]
    pub max_task_iterations: Option<u64>,
    /// Generation steering for agent provider calls: temperature, output cap
    /// / `num_predict`, `num_ctx`, seed and stop sequences. Every field is
    /// optional so a config that omits this section keeps each provider's own
    /// defaults exactly — this is an *explicit* override surface, never a
    /// silently-changed default. The effective `context_window` is bounded by
    /// the provider's declared window when one is advertised (an unsafe
    /// request is never forced).
    #[serde(default)]
    pub generation: GenerationConfig,
}

/// The transport-visible subset of generation parameters an operator may
/// steer from config. Mirrors `openrt_models::ModelParameters`' Ollama
/// surface (`options.temperature`/`top_p`/`num_predict`/`num_ctx`/`seed`/
/// `stop`); `None`/empty means the provider default — this config section is
/// the explicit override surface, so omitting fields changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationConfig {
    /// Sampling temperature (`options.temperature`). `0` is legal (greedy).
    pub temperature: Option<f32>,
    /// Nucleus sampling cutoff (`options.top_p`).
    pub top_p: Option<f32>,
    /// Output cap (`options.num_predict`). When `unbounded_output` is set,
    /// `num_predict = -1` wins and this is ignored.
    pub max_tokens: Option<u32>,
    /// Explicit no-cap generation (`options.num_predict = -1`). Ollama-only.
    #[serde(default)]
    pub unbounded_output: bool,
    /// Per-request context window (`options.num_ctx`). The agent bounds this
    /// by the provider's declared window when one is advertised, so a request
    /// like 65536 is never forced against a backend that certifies less.
    pub context_window: Option<u32>,
    /// Deterministic sampling seed (`options.seed`).
    pub seed: Option<i64>,
    /// Stop sequences passed through to the provider. Default empty = the
    /// provider's own stop handling.
    #[serde(default)]
    pub stop: Vec<String>,
}

/// Serde default for the retry-backoff base (ms).
pub fn default_agent_retry_backoff_base_ms() -> u64 {
    500
}

/// Serde default for the retry-backoff ceiling (ms).
pub fn default_agent_retry_backoff_max_ms() -> u64 {
    30_000
}

/// Serde default for the context-guard ratio.
pub fn default_agent_context_guard_ratio() -> f32 {
    0.7
}

/// Serde default for the global task-turn ceiling (finite by default — the
/// supervisor must never be an unbounded runaway out of the box; `None` is
/// the operator's explicit relaxation).
pub fn default_max_task_turns() -> Option<u32> {
    Some(4096)
}

/// Serde default for the global task tool-call ceiling.
pub fn default_max_task_tool_calls() -> Option<u32> {
    Some(8192)
}

/// Serde default for the global task wall-clock ceiling (ms; 24 h).
pub fn default_max_task_wall_time() -> Option<u64> {
    Some(86_400_000)
}

/// Serde default for the global automatic-continuation segment ceiling.
pub fn default_max_task_iterations() -> Option<u64> {
    Some(128)
}

/// Serde default for [`RuntimeConfig::max_tool_calls`] (keeps pre-existing
/// configs valid — the field is optional on the wire).
pub fn default_max_tool_calls() -> u32 {
    8
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            default_provider: "ollama".into(),
            default_agent: "minimal".into(),
            max_turns: 6,
            max_tool_calls: default_max_tool_calls(),
            model_timeout_secs: 90,
            workspace_dir: None,
            terminal_timeout_secs: None,
            terminal_max_output_bytes: None,
            python_sandbox: None,
            screen_backend: None,
            computer_backend: None,
            agent_retry_backoff_base_ms: default_agent_retry_backoff_base_ms(),
            agent_retry_backoff_max_ms: default_agent_retry_backoff_max_ms(),
            agent_idle_timeout_ms: None,
            agent_context_guard_ratio: default_agent_context_guard_ratio(),
            max_task_turns: default_max_task_turns(),
            max_task_tool_calls: default_max_task_tool_calls(),
            max_task_wall_time: default_max_task_wall_time(),
            max_task_iterations: default_max_task_iterations(),
            generation: GenerationConfig::default(),
        }
    }
}

/// Overrides for the Python sandbox (`python.run`). Every field is optional;
/// `None` falls back to `SandboxLimits`/`/usr/bin/python3` defaults.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonSandboxConfig {
    /// Absolute interpreter path (default `/usr/bin/python3`).
    pub python: Option<PathBuf>,
    /// Wall-clock timeout in seconds (default 30).
    pub wall_timeout_secs: Option<u64>,
    /// Cumulative CPU seconds before the kernel sends SIGXCPU (default 10).
    pub cpu_seconds: Option<u32>,
    /// Address-space limit in bytes (default 1 GiB).
    pub memory_bytes: Option<u64>,
    /// Process/thread count limit (default 32).
    pub max_procs: Option<u32>,
    /// Captured output byte cap, per stream (default 65536).
    pub output_bytes: Option<usize>,
    /// Allow the degraded `subprocess` fallback when bubblewrap is not
    /// installed (default `false`): without this opt-in, `python.run` refuses
    /// to run rather than executing without OS-level confinement. Set true
    /// only when you accept an unconfined, isolated-interpreter-only mode.
    pub allow_unconfined_fallback: Option<bool>,
}

/// Kind of a model provider port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Ollama,
    Mock,
    /// llama.cpp `server` over the OpenAI-compatible HTTP API (P3).
    Llamacpp,
    /// vLLM OpenAI-compatible server (P3).
    Vllm,
}

/// A configured provider port. Ids must be unique within `providers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub id: String,
    pub kind: ProviderKind,
    /// Base URL for `Ollama` providers.
    pub base_url: Option<String>,
    /// Optional default model for this provider.
    pub default_model: Option<String>,
}

/// A permission policy rule effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleEffect {
    Allow,
    Deny,
    /// Allow only with human / resolver approval (ASK boundary, P2.2). Denied
    /// when no resolver is attached to the runtime.
    Ask,
}

/// Explicit capability allow/deny rule. Patterns support a `*` wildcard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub effect: RuleEffect,
    pub capability: String,
    /// Optional resource pattern (e.g. path prefix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// Permission configuration: deny-by-default unless a matching rule allows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsConfig {
    pub rules: Vec<RuleConfig>,
}

impl PermissionsConfig {
    /// The built-in **interactive developer profile** — what `openrt agent`
    /// (and every zero-config run) advertises to the model:
    ///
    /// * **Allow** read-only inspection (`filesystem.list/read`, `system.info`,
    ///   `tools.list`, browser/computer observation, office/document reads).
    /// * **Ask** every mutation or powerful action (writes/deletes/moves,
    ///   `terminal.run`, `python.run`, computer/app/input, browser actions,
    ///   artifact creation, PDF export) — the operator answers exactly ONE
    ///   approval card per call (the tool id carries the `Ask`; its declared
    ///   capability is `Allow` so the gate's per-check evaluation does not
    ///   ask twice).
    /// * **Deny-by-default** for everything unlisted (domain tools, MCP,
    ///   databases, learning — a tool is never allowed merely because it
    ///   exists in the registry).
    ///
    /// AgentShield, the sandbox, the resource gate, Sovereign egress and the
    /// audit chain still adjudicate every call: `Ask` without a resolver fails
    /// closed, and dangerous shell patterns halt regardless of these rules.
    pub fn developer_profile() -> Self {
        use RuleEffect::{Allow, Ask};
        let mut rules: Vec<RuleConfig> = Vec::new();
        let mut push = |effect: RuleEffect, capability: &str| {
            rules.push(RuleConfig {
                effect,
                capability: capability.to_string(),
                resource: None,
            })
        };
        // Read-only / safe: Allow (tool id + declared capability).
        for (id, cap) in [
            ("echo", None),
            ("system.info", Some("system.info.all")),
            ("filesystem.read", Some("filesystem.read.all")),
            ("filesystem.list", Some("filesystem.list.all")),
            ("tools.list", Some("tools.discovery.all")),
            ("application.list", Some("application.list.all")),
            ("application.verify", Some("application.verify.all")),
            ("artifact.list", Some("fabric.artifacts.read")),
            ("computer.env", Some("computer.env.all")),
            ("computer.screen", Some("computer.screen.all")),
            ("screen.ground", Some("screen.ground.all")),
            ("browser.status", Some("browser.status.all")),
            ("browser.list", Some("browser.list.all")),
            ("browser.find", Some("browser.find.all")),
            ("docx.read", Some("artifact.docx.read")),
            ("excel.read", Some("artifact.excel.read")),
            ("ppt.render", Some("artifact.ppt.read")),
            // Phase-5 asset/hook capability aliases: the gate policy is the
            // single authority for tool calls and hook permission checks.
            ("fs.read", None),
            ("tool.run", None),
            ("tool.other", None),
        ] {
            push(Allow, id);
            if let Some(c) = cap {
                push(Allow, c);
            }
        }
        // Mutations / powerful actions: Ask on the id, Allow on the declared
        // capability (one approval card per call, never two).
        for (id, cap) in [
            ("filesystem.write", "filesystem.write.all"),
            ("filesystem.delete", "filesystem.delete.all"),
            ("filesystem.move", "filesystem.move.all"),
            ("terminal.run", "terminal.run.all"),
            ("python.run", "python.run.all"),
            ("computer.app", "computer.app.all"),
            ("computer.input", "computer.input.all"),
            ("computer.clipboard", "computer.clipboard.all"),
            ("computer.use", "computer.use.all"),
            ("computer.window", "computer.window.all"),
            ("input.keyboard", "input.keyboard.all"),
            ("input.mouse", "input.mouse.all"),
            ("browser.navigate", "browser.navigate.all"),
            ("browser.click", "browser.click.all"),
            ("browser.type", "browser.type.all"),
            ("browser.select", "browser.select.all"),
            ("browser.submit", "browser.submit.all"),
            ("browser.activate", "browser.activate.all"),
            ("browser.upload", "browser.upload.all"),
            ("browser.download", "browser.download.all"),
            ("browser.close", "browser.close.all"),
            ("browser.screenshot", "browser.screenshot.all"),
            ("screen.capture", "screen.capture.all"),
            ("docx.create", "artifact.docx.write"),
            ("excel.create", "artifact.excel.write"),
            ("excel.write", "artifact.excel.write"),
            ("ppt.create", "artifact.ppt.write"),
            // PDF export: `Ask` on the id (the capability is already `Allow`
            // via the read rules above, so each call asks exactly once).
            ("docx.export_pdf", "artifact.docx.read"),
            ("ppt.export_pdf", "artifact.ppt.read"),
        ] {
            push(Ask, id);
            push(Allow, cap);
        }
        Self { rules }
    }
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self::developer_profile()
    }
}

/// Resource limits for the resource gate (Phase 1 foundation; scheduling is
/// Phase 3). `None` means "no explicit limit".
///
/// The memory rule is **headroom-based**, not an absolute "used < X%" ceiling:
/// another process (a local model server, a browser) legitimately holding
/// memory must not refuse a 20 KB tool call. See
/// `openrt_resources::ResourceLimits::min_headroom_ratio`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResourcesConfig {
    /// Historical field: percentage (0–100) of host memory that must remain
    /// **free**. Kept for compatibility; prefer `min_memory_headroom_percent`.
    pub max_memory_bytes: Option<u64>,
    pub max_disk_bytes: Option<u64>,
    /// Percentage (0–100) of total host memory that must remain available
    /// before work is admitted. Default 15.
    pub min_memory_headroom_percent: Option<u64>,
    /// Absolute minimum available bytes. Overrides the percentage when set.
    pub min_free_memory_bytes: Option<u64>,
    /// Percentage (0–100) of total host memory *above* the floor that still
    /// reports as constrained. Never blocks; only informs. Default 30.
    pub tight_memory_headroom_percent: Option<u64>,
    /// Whether an operator may bypass a resource refusal for a single
    /// admission. Default `true` — the override is one-shot and audited.
    pub allow_operator_override: Option<bool>,
}

/// Persistence backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    /// JSON-file foundation store on local disk.
    File,
    /// In-memory store (used in tests / ephemeral runs).
    Memory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistenceConfig {
    pub kind: StoreKind,
    /// Store root directory (for `File`). Default: `~/.openrt`.
    pub dir: Option<PathBuf>,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            kind: StoreKind::File,
            dir: None,
        }
    }
}

/// Telemetry output level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    pub level: LogLevel,
    /// `human` for console output, `json` for machine-readable.
    pub format: String,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            format: "human".into(),
        }
    }
}

/// Operating-mode concept (ADR-003). Phase 1 records it; the enforcement engine
/// and network guard are introduced in Phase 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperatingMode {
    /// Network is allowed for explicit materialization only (discovery,
    /// downloads) under operator opt-in.
    Provisioning,
    /// No external/egress network; everything local. Note: this means no
    /// outbound/egress network operations — not "zero network interfaces"
    /// (local IPC is unaffected).
    Sovereign,
}

/// Phase 7B browser backend selection:
/// `None`/`"auto"` = probe for a real backend (Firefox BiDi),
/// `"mock"` = deterministic in-memory backend (tests/CI),
/// `"off"` = do not spawn a backend (structured UNAVAILABLE on use),
/// an explicit id pins that backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BrowserConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Override the Firefox binary (default: probe `firefox`/`firefox-esr`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<std::path::PathBuf>,
}

/// MCP trust level for one configured server — 4 levels (Phase 11 M11.2).
/// Imported tools are always deny-by-default in policy (`mcp.invoke.all` +
/// `mcp.<server>.invoke`); trust only records operator intent and gates
/// **connection eligibility**, never permissions/policy/sandbox/mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpTrust {
    /// No special trust recorded; the server may connect (when enabled) but
    /// policy still requires explicit `mcp.invoke.all` + `mcp.<server>.invoke`
    /// grants for every call.
    #[default]
    Untrusted,
    /// A scan or import flagged this server (or it awaits its first review):
    /// never connected until the operator resolves it. Fail-closed.
    Review,
    /// Operator asserts this server is reviewed; caps still enforced —
    /// trust never bypasses permissions, policy, sandbox, or mode.
    Trusted,
    /// Operator refuses this server: never connected, regardless of
    /// configuration or scan state.
    Blocked,
}

impl McpTrust {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Review => "review",
            Self::Trusted => "trusted",
            Self::Blocked => "blocked",
        }
    }

    /// Parse a trust level from CLI/operator input (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "untrusted" => Some(Self::Untrusted),
            "review" => Some(Self::Review),
            "trusted" => Some(Self::Trusted),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

/// One configured MCP server (stdio transport).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Server id (tool ids become `mcp.<name>.<tool>`; `[a-z0-9_-]` only).
    pub name: String,
    /// Executable to spawn (no shell).
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the child process. Values are never logged.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub trust: McpTrust,
    /// Capabilities this server's tools may need (discovery/review surface).
    #[serde(default)]
    pub declared_capabilities: Vec<String>,
}

/// MCP configuration (fail-closed: no servers ⇒ no imported tools).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// Database backend kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Sqlite,
    Postgres,
}

/// One operator-configured database connection. Tools select connections by
/// id only — callers can never pass raw paths/DSNs (credentials stay out of
/// tool args, logs, and records).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConnectionConfig {
    pub id: String,
    pub kind: DbKind,
    /// SQLite database file (required when `kind = "sqlite"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite_path: Option<std::path::PathBuf>,
    /// Name of the environment variable holding the PostgreSQL DSN
    /// (the DSN itself is never stored or logged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_dsn_env: Option<String>,
}

/// Database configuration (fail-closed: no connections ⇒ structured
/// UNAVAILABLE on every `database.*` tool).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub connections: Vec<DatabaseConnectionConfig>,
    /// Default connection id when a tool omits `connection`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Office automation configuration (LibreOffice/soffice for render/export).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfficeConfig {
    /// Override the soffice binary path (default: probe `soffice` on PATH).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soffice: Option<std::path::PathBuf>,
}

/// Phase 7B tool fabric configuration (browser / MCP / database / office).
/// All sections default so existing configs keep loading unchanged.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FabricConfig {
    #[serde(default)]
    pub browser: BrowserConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub office: OfficeConfig,
}

/// Canonical configuration for one runtime instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub runtime: RuntimeConfig,
    pub providers: Vec<ProviderConfig>,
    pub permissions: PermissionsConfig,
    pub resources: ResourcesConfig,
    pub persistence: PersistenceConfig,
    pub telemetry: TelemetryConfig,
    pub mode: OperatingMode,
    /// Phase 7B tool fabric (browser/MCP/database/office). Defaults when
    /// absent so pre-7B configs keep loading.
    #[serde(default)]
    pub fabric: FabricConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            runtime: RuntimeConfig::default(),
            providers: vec![ProviderConfig {
                id: "ollama".into(),
                kind: ProviderKind::Ollama,
                base_url: Some("http://127.0.0.1:11434".into()),
                default_model: None,
            }],
            permissions: PermissionsConfig::default(),
            resources: ResourcesConfig::default(),
            persistence: PersistenceConfig::default(),
            telemetry: TelemetryConfig::default(),
            mode: OperatingMode::Sovereign,
            fabric: FabricConfig::default(),
        }
    }
}

impl Config {
    /// Validate the whole config, returning a `ConfigurationError` on the first
    /// problem found.
    pub fn validate(&self) -> Result<(), ConfigurationError> {
        if self.runtime.max_turns == 0 {
            return Err(ConfigurationError::Invalid(
                "runtime.max_turns must be >= 1".into(),
            ));
        }
        if self.runtime.max_tool_calls == 0 {
            return Err(ConfigurationError::Invalid(
                "runtime.max_tool_calls must be >= 1".into(),
            ));
        }
        if self.runtime.model_timeout_secs == 0 {
            return Err(ConfigurationError::Invalid(
                "runtime.model_timeout_secs must be >= 1".into(),
            ));
        }
        if self.runtime.agent_retry_backoff_base_ms == 0 {
            return Err(ConfigurationError::Invalid(
                "runtime.agent_retry_backoff_base_ms must be >= 1".into(),
            ));
        }
        if self.runtime.agent_retry_backoff_max_ms < self.runtime.agent_retry_backoff_base_ms {
            return Err(ConfigurationError::Invalid(
                "runtime.agent_retry_backoff_max_ms must be >= runtime.agent_retry_backoff_base_ms"
                    .into(),
            ));
        }
        if let Some(idle) = self.runtime.agent_idle_timeout_ms {
            if idle < 250 {
                return Err(ConfigurationError::Invalid(
                    "runtime.agent_idle_timeout_ms must be >= 250 or None".into(),
                ));
            }
        }
        let ratio = self.runtime.agent_context_guard_ratio;
        if !(ratio > 0.0 && ratio <= 1.0) {
            return Err(ConfigurationError::Invalid(
                "runtime.agent_context_guard_ratio must be in (0.0, 1.0]".into(),
            ));
        }
        for field in [
            ("runtime.max_task_turns", self.runtime.max_task_turns),
            (
                "runtime.max_task_tool_calls",
                self.runtime.max_task_tool_calls,
            ),
        ] {
            if let Some(v) = field.1 {
                if v == 0 {
                    return Err(ConfigurationError::Invalid(format!(
                        "{} must be >= 1 or None",
                        field.0
                    )));
                }
            }
        }
        for field in [
            (
                "runtime.max_task_wall_time",
                self.runtime.max_task_wall_time,
            ),
            (
                "runtime.max_task_iterations",
                self.runtime.max_task_iterations,
            ),
        ] {
            if let Some(v) = field.1 {
                if v == 0 {
                    return Err(ConfigurationError::Invalid(format!(
                        "{} must be >= 1 or None",
                        field.0
                    )));
                }
            }
        }
        let gen = &self.runtime.generation;
        if let Some(t) = gen.temperature {
            if !t.is_finite() || !(0.0..=2.0).contains(&t) {
                return Err(ConfigurationError::Invalid(
                    "runtime.generation.temperature must be finite in [0.0, 2.0]".into(),
                ));
            }
        }
        if let Some(t) = gen.top_p {
            if !t.is_finite() || !(0.0..=1.0).contains(&t) {
                return Err(ConfigurationError::Invalid(
                    "runtime.generation.top_p must be finite in [0.0, 1.0]".into(),
                ));
            }
        }
        if let Some(n) = gen.max_tokens {
            if n == 0 {
                return Err(ConfigurationError::Invalid(
                    "runtime.generation.max_tokens must be >= 1".into(),
                ));
            }
        }
        if let Some(w) = gen.context_window {
            if w == 0 {
                return Err(ConfigurationError::Invalid(
                    "runtime.generation.context_window must be >= 1".into(),
                ));
            }
        }
        if self.providers.is_empty() {
            return Err(ConfigurationError::Invalid(
                "at least one provider is required".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.providers {
            if p.id.trim().is_empty() {
                return Err(ConfigurationError::Invalid(
                    "provider id must not be empty".into(),
                ));
            }
            if !seen.insert(p.id.clone()) {
                return Err(ConfigurationError::Invalid(format!(
                    "duplicate provider id `{}`",
                    p.id
                )));
            }
            if p.kind == ProviderKind::Ollama {
                let url = p.base_url.as_deref().ok_or_else(|| {
                    ConfigurationError::Invalid(format!(
                        "ollama provider `{}` requires base_url",
                        p.id
                    ))
                })?;
                let parsed = url::Url::parse(url)
                    .map_err(|e| ConfigurationError::Invalid(format!("invalid base_url: {e}")))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(ConfigurationError::Invalid(format!(
                        "base_url must be http(s), got `{}`",
                        parsed.scheme()
                    )));
                }
            }
        }
        if !self
            .providers
            .iter()
            .any(|p| p.id == self.runtime.default_provider)
        {
            return Err(ConfigurationError::Invalid(format!(
                "runtime.default_provider `{}` not found in providers",
                self.runtime.default_provider
            )));
        }
        if self.telemetry.format != "human" && self.telemetry.format != "json" {
            return Err(ConfigurationError::Invalid(
                "telemetry.format must be `human` or `json`".into(),
            ));
        }
        if let Some(dir) = &self.runtime.workspace_dir {
            if dir.as_os_str().is_empty() {
                return Err(ConfigurationError::Invalid(
                    "workspace_dir must not be empty".into(),
                ));
            }
        }
        // ---- Phase 7B fabric validation (fail-closed at config time) ----
        let valid_id = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
        };
        if let Some(b) = self.fabric.browser.backend.as_deref() {
            if !matches!(b, "auto" | "mock" | "off" | "firefox") {
                return Err(ConfigurationError::Invalid(format!(
                    "fabric.browser.backend must be auto|mock|off|firefox, got `{b}`"
                )));
            }
        }
        let mut db_ids = std::collections::HashSet::new();
        for c in &self.fabric.database.connections {
            if !valid_id(&c.id) {
                return Err(ConfigurationError::Invalid(format!(
                    "database connection id `{}` must match [a-z0-9_-]+",
                    c.id
                )));
            }
            if !db_ids.insert(c.id.clone()) {
                return Err(ConfigurationError::Invalid(format!(
                    "duplicate database connection id `{}`",
                    c.id
                )));
            }
            match c.kind {
                DbKind::Sqlite => {
                    if c.sqlite_path.is_none() {
                        return Err(ConfigurationError::Invalid(format!(
                            "sqlite connection `{}` requires sqlite_path",
                            c.id
                        )));
                    }
                }
                DbKind::Postgres => {
                    if c.pg_dsn_env.is_none() {
                        return Err(ConfigurationError::Invalid(format!(
                            "postgres connection `{}` requires pg_dsn_env (env var name; DSN never stored)",
                            c.id
                        )));
                    }
                }
            }
        }
        if let Some(d) = self.fabric.database.default.as_deref() {
            if !db_ids.contains(d) {
                return Err(ConfigurationError::Invalid(format!(
                    "fabric.database.default `{d}` not found in connections"
                )));
            }
        }
        let mut mcp_names = std::collections::HashSet::new();
        for s in &self.fabric.mcp.servers {
            if !valid_id(&s.name) {
                return Err(ConfigurationError::Invalid(format!(
                    "mcp server name `{}` must match [a-z0-9_-]+",
                    s.name
                )));
            }
            if !mcp_names.insert(s.name.clone()) {
                return Err(ConfigurationError::Invalid(format!(
                    "duplicate mcp server name `{}`",
                    s.name
                )));
            }
            if s.command.trim().is_empty() {
                return Err(ConfigurationError::Invalid(format!(
                    "mcp server `{}` requires a non-empty command",
                    s.name
                )));
            }
        }
        // Empty allow rules are fine (strict deny-by-default), so nothing
        // further to check here.
        Ok(())
    }

    /// Load from a path. Missing file ⇒ `ConfigurationError::NotFound`.
    pub fn load_from(path: &Path) -> Result<Self, ConfigurationError> {
        let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ConfigurationError::NotFound(path.to_path_buf()),
            _ => ConfigurationError::Io {
                path: path.to_path_buf(),
                source: e,
            },
        })?;
        let cfg: Config = serde_json::from_str(&text)
            .map_err(|e| ConfigurationError::Invalid(format!("{}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load the configuration in effect for this process (FIX: fail-closed
    /// config resolution).
    ///
    /// * `OPENRT_CONFIG` explicitly set ⇒ that path must exist and load;
    ///   a missing/unreadable file is an error, never a silent fallback to
    ///   built-in defaults (which would run with the wrong store, policy,
    ///   or mode while the operator believes their config is in effect).
    /// * Otherwise: discovered default path (`<cwd>/openrt.json`, else
    ///   `~/.openrt/config.json`) when present, else built-in defaults —
    ///   zero-config first-run UX stays intact.
    ///
    /// After resolution, a config without an explicit `runtime.workspace_dir`
    /// gets the **detected workspace root** (see [`detect_workspace_root`]):
    /// the interactive agent must know where it is instead of failing every
    /// filesystem tool with "no workspace root configured".
    pub fn load_effective() -> Result<Self, ConfigurationError> {
        Self::load_effective_from(std::env::var_os("OPENRT_CONFIG").map(PathBuf::from))
    }

    /// Pure resolution core (explicit override vs. discovered default),
    /// split out so the fail-closed rule is unit-testable without touching
    /// process environment.
    pub fn load_effective_from(explicit: Option<PathBuf>) -> Result<Self, ConfigurationError> {
        if let Some(path) = explicit {
            let mut cfg = Self::load_from(&path)?;
            cfg.apply_workspace_detection();
            return Ok(cfg);
        }
        let path = Config::discovered_path();
        let mut cfg = if path.exists() {
            Self::load_from(&path)?
        } else {
            Config::default()
        };
        cfg.apply_workspace_detection();
        Ok(cfg)
    }

    /// Fill `runtime.workspace_dir` from detection when the operator did not
    /// configure one explicitly. An explicit setting always wins; detection
    /// never invents a path (see [`detect_workspace_root`]).
    fn apply_workspace_detection(&mut self) {
        if self.runtime.workspace_dir.is_none() {
            if let Ok(cwd) = std::env::current_dir() {
                let (root, _source) = detect_workspace_root(&cwd);
                self.runtime.workspace_dir = Some(root);
            }
        }
    }

    /// Path resolution WITHOUT the env override: `<cwd>/openrt.json` when
    /// present, else `~/.openrt/config.json` (cwd path as final fallback).
    fn discovered_path() -> PathBuf {
        let cwd = std::env::current_dir().unwrap_or_default();
        let local = cwd.join("openrt.json");
        if local.exists() {
            return local;
        }
        home_dir()
            .map(|d| d.join(".openrt").join("config.json"))
            .unwrap_or(local)
    }

    /// Resolve the default config path: `OPENRT_CONFIG` env, else
    /// `<cwd>/openrt.json`, else `~/.openrt/config.json`.
    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("OPENRT_CONFIG") {
            return PathBuf::from(p);
        }
        Self::discovered_path()
    }
}

/// Where a detected workspace root came from (reported by `openrt doctor`
/// so the operator can tell a real project root from a plain cwd fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceSource {
    /// The start directory (or an ancestor) contains `.git`.
    GitRoot,
    /// The start directory contains project marker files.
    ProjectFiles,
    /// No git root and no markers: the working directory itself (honest —
    /// it is where the operator launched OpenRT, never an invented path).
    WorkingDirectory,
}

impl WorkspaceSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GitRoot => "git root",
            Self::ProjectFiles => "project files",
            Self::WorkingDirectory => "working directory",
        }
    }
}

/// Project markers that identify a working directory as a project even
/// without git (checked in `from` only, not ancestors — an ancestor without
/// markers but with `.git` is already covered by the git walk).
const PROJECT_MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "openrt.json",
];

/// Detect the workspace root for the interactive agent, walking UP from
/// `from`: first a directory containing `.git` (repo root — "git root where
/// appropriate"), else `from` itself when it contains project marker files,
/// else `from` (the current working directory — a real path, never invented).
///
/// An explicit `runtime.workspace_dir` configuration always wins over this
/// detection (see `Config::apply_workspace_detection`).
pub fn detect_workspace_root(from: &std::path::Path) -> (PathBuf, WorkspaceSource) {
    let mut dir = Some(from);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return (d.to_path_buf(), WorkspaceSource::GitRoot);
        }
        dir = d.parent();
    }
    if PROJECT_MARKERS.iter().any(|m| from.join(m).exists()) {
        return (from.to_path_buf(), WorkspaceSource::ProjectFiles);
    }
    (from.to_path_buf(), WorkspaceSource::WorkingDirectory)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Errors produced while loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error("config file not found: {0}")]
    NotFound(PathBuf),
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        let c = Config::default();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn reject_duplicate_providers() {
        let mut c = Config::default();
        c.providers.push(c.providers[0].clone());
        assert!(matches!(
            c.validate().unwrap_err(),
            ConfigurationError::Invalid(_)
        ));
    }

    #[test]
    fn reject_missing_default_provider() {
        let mut c = Config::default();
        c.runtime.default_provider = "nope".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn reject_bad_ollama_url() {
        let mut c = Config::default();
        c.providers[0].base_url = Some("ftp://nope".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn round_trip_serde() {
        let c = Config::default();
        let json = serde_json::to_string_pretty(&c).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    // ---- global task budgets (supervisor hardening) ---------------------

    #[test]
    fn global_task_budgets_default_finite_not_unbounded() {
        let c = Config::default();
        // The default configuration must never be an unbounded runaway: every
        // global ceiling is finite unless an operator explicitly relaxes it.
        assert_eq!(c.runtime.max_task_turns, Some(4096));
        assert_eq!(c.runtime.max_task_tool_calls, Some(8192));
        assert_eq!(c.runtime.max_task_wall_time, Some(86_400_000));
        assert_eq!(c.runtime.max_task_iterations, Some(128));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn global_task_budgets_round_trip_and_none_relaxes() {
        let mut c = Config::default();
        c.runtime.max_task_turns = Some(64);
        c.runtime.max_task_tool_calls = Some(128);
        c.runtime.max_task_wall_time = Some(3_600_000);
        c.runtime.max_task_iterations = Some(5);
        assert!(c.validate().is_ok());
        let back: Config = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.runtime.max_task_turns, Some(64));
        assert_eq!(back.runtime.max_task_tool_calls, Some(128));
        assert_eq!(back.runtime.max_task_wall_time, Some(3_600_000));
        assert_eq!(back.runtime.max_task_iterations, Some(5));

        // `None` is the operator's explicit relaxation (e.g. a long soak).
        c.runtime.max_task_wall_time = None;
        c.runtime.max_task_iterations = None;
        let back: Config = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.runtime.max_task_wall_time, None);
        assert_eq!(back.runtime.max_task_iterations, None);
        assert!(c.validate().is_ok());
    }

    /// An old config (no new fields) loads the finite defaults — a
    /// pre-existing file must never silently become unbounded.
    #[test]
    fn old_config_without_global_budgets_loads_finite_defaults() {
        let mut json = serde_json::to_string(&Config::default()).unwrap();
        for field in [
            "\"max_task_turns\":4096,",
            "\"max_task_tool_calls\":8192,",
            "\"max_task_wall_time\":86400000,",
            "\"max_task_iterations\":128,",
        ] {
            assert!(json.contains(field), "missing {field}:\n{json}");
            json = json.replace(field, "");
        }
        let c: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(c.runtime.max_task_turns, Some(4096));
        let back: Config =
            serde_json::from_str(&serde_json::to_string(&Config::default()).unwrap()).unwrap();
        assert_eq!(back.runtime.max_task_turns, Some(4096));
    }

    #[test]
    fn global_task_budgets_reject_zero() {
        let mut c = Config::default();
        c.runtime.max_task_turns = Some(0);
        assert!(c
            .validate()
            .unwrap_err()
            .to_string()
            .contains("runtime.max_task_turns must be >= 1 or None"));
        let mut c = Config::default();
        c.runtime.max_task_iterations = Some(0);
        assert!(c
            .validate()
            .unwrap_err()
            .to_string()
            .contains("runtime.max_task_iterations must be >= 1 or None"));
        let mut c = Config::default();
        c.runtime.max_task_wall_time = Some(0);
        assert!(c
            .validate()
            .unwrap_err()
            .to_string()
            .contains("runtime.max_task_wall_time must be >= 1 or None"));
        let mut c = Config::default();
        c.runtime.max_task_tool_calls = Some(0);
        assert!(c
            .validate()
            .unwrap_err()
            .to_string()
            .contains("runtime.max_task_tool_calls must be >= 1 or None"));
    }

    // ---- runtime.generation --------------------------------------------

    #[test]
    fn generation_defaults_leave_provider_defaults_untouched() {
        let g = GenerationConfig::default();
        assert!(g.temperature.is_none());
        assert!(g.top_p.is_none());
        assert!(g.max_tokens.is_none());
        assert!(!g.unbounded_output);
        assert!(g.context_window.is_none());
        assert!(g.seed.is_none());
        assert!(g.stop.is_empty());
    }

    /// Old config files without a `generation` section load unchanged with
    /// provider defaults — this surface is an explicit opt-in, never a
    /// silently-applied default.
    #[test]
    fn generation_is_optional_on_the_wire() {
        let mut json = serde_json::to_string(&Config::default()).unwrap();
        let needle = r#","generation":{"temperature":null,"top_p":null,"max_tokens":null,"unbounded_output":false,"context_window":null,"seed":null,"stop":[]}"#;
        assert!(
            json.contains(needle),
            "generation block serializes as expected:\n{json}"
        );
        json = json.replace(needle, "");
        let c: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(c.runtime.generation, GenerationConfig::default(),);
    }

    #[test]
    fn generation_configures_predictable_generation() {
        let mut c = Config::default();
        c.runtime.generation = GenerationConfig {
            temperature: Some(0.7),
            top_p: None,
            max_tokens: None,
            unbounded_output: true,
            context_window: Some(64_000),
            seed: Some(7),
            stop: vec!["</s>".into()],
        };
        assert!(c.validate().is_ok());
        let back: Config = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.runtime.generation, c.runtime.generation);
    }

    #[test]
    fn generation_rejects_bad_ranges() {
        let mut c = Config::default();
        c.runtime.generation.temperature = Some(3.0);
        let err = c.validate().unwrap_err().to_string();
        assert!(
            err.contains("runtime.generation.temperature must be finite in [0.0, 2.0]"),
            "{err}"
        );
        let mut c = Config::default();
        c.runtime.generation.top_p = Some(1.5);
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.runtime.generation.max_tokens = Some(0);
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.runtime.generation.context_window = Some(0);
        assert!(c.validate().is_err());
    }

    #[test]
    fn generation_rejects_unknown_fields() {
        // The `generation` block mirrors its transport type strictly:
        // an unrecognized key fails closed instead of being ignored.
        let mut c = Config::default();
        let mut json = serde_json::to_string(&c).unwrap();
        let needle = r#""context_window":null"#;
        assert!(
            json.contains(needle),
            "generation serializes with the block: {json}"
        );
        json = json.replace(needle, r#""context_window":null,"bogus_option":true"#);
        assert!(
            serde_json::from_str::<Config>(&json).is_err(),
            "deny_unknown_fields inside generation"
        );
    }

    #[test]
    fn reject_unknown_fields() {
        let c = Config::default();
        let mut json = serde_json::to_string(&c).unwrap();
        // inject unknown top-level key
        json.pop();
        json.push_str(",\"bogus\":1}");
        assert!(serde_json::from_str::<Config>(&json).is_err());
    }

    #[test]
    fn load_missing_file() {
        let err = Config::load_from(Path::new("/nonexistent/openrt.json")).unwrap_err();
        assert!(matches!(err, ConfigurationError::NotFound(_)));
    }

    /// FIX: an EXPLICITLY configured path (OPENRT_CONFIG) must fail closed
    /// when missing — never silently fall back to built-in defaults, which
    /// would run with the wrong store/policy/mode while the operator
    /// believes their config is in effect.
    #[test]
    fn explicit_missing_config_fails_closed() {
        let err = Config::load_effective_from(Some(PathBuf::from("/nonexistent/openrt.json")))
            .unwrap_err();
        assert!(matches!(err, ConfigurationError::NotFound(_)));
    }

    #[test]
    fn default_path_honors_env() {
        // guard: don't clobber a real env in dev
        if std::env::var_os("OPENRT_CONFIG").is_none() {
            let p = Config::default_path();
            assert!(p.ends_with("openrt.json") || p.ends_with("config.json"));
            let _ = p;
        }
    }

    // ---- interactive developer profile ---------------------------------

    /// The zero-config profile the interactive agent runs under: safe reads
    /// are Allowed, mutations are Ask (visible — the operator decides), and
    /// unlisted tools stay deny-by-default.
    #[test]
    fn developer_profile_advertises_reads_asks_mutations() {
        let p = PermissionsConfig::developer_profile();
        let decision = |capability: &str| {
            p.rules
                .iter()
                .find(|r| r.capability == capability)
                .map(|r| r.effect)
        };
        // Read-only: Allow.
        for allow in [
            "filesystem.list",
            "filesystem.list.all",
            "filesystem.read",
            "system.info",
            "tools.list",
            "computer.env",
            "browser.find",
        ] {
            assert_eq!(
                decision(allow),
                Some(RuleEffect::Allow),
                "{allow} must be Allowed"
            );
        }
        // Mutations / powerful actions: Ask on the tool id, Allow on the
        // declared capability (exactly one approval card per call).
        for ask in [
            "filesystem.write",
            "filesystem.delete",
            "filesystem.move",
            "terminal.run",
            "python.run",
            "computer.app",
            "computer.use",
            "browser.navigate",
            "screen.capture",
            "docx.create",
            "docx.export_pdf",
        ] {
            assert_eq!(decision(ask), Some(RuleEffect::Ask), "{ask} must Ask");
        }
        for cap in [
            "filesystem.write.all",
            "terminal.run.all",
            "computer.app.all",
            "browser.navigate.all",
            "artifact.docx.write",
        ] {
            assert_eq!(
                decision(cap),
                Some(RuleEffect::Allow),
                "{cap} must Allow (no double-Ask)"
            );
        }
        // Deny-by-default: nothing for unlisted domain tools, ever.
        for deny in [
            "industrial.assets",
            "trading.backtest",
            "database.insert",
            "mcp.servers",
        ] {
            assert_eq!(decision(deny), None, "{deny} must stay unlisted");
        }
        assert_eq!(
            PermissionsConfig::default(),
            PermissionsConfig::developer_profile()
        );
    }

    // ---- workspace-root detection --------------------------------------

    fn tmp(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "openrt-cfgdetect-{prefix}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detect_workspace_root_prefers_git_root() {
        let repo = tmp("git");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("crates").join("openrt-tools");
        std::fs::create_dir_all(&nested).unwrap();
        let (root, source) = detect_workspace_root(&nested);
        assert_eq!(root, repo);
        assert_eq!(source, WorkspaceSource::GitRoot);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn detect_workspace_root_uses_project_files() {
        let proj = tmp("proj");
        std::fs::write(proj.join("package.json"), b"{}").unwrap();
        let (root, source) = detect_workspace_root(&proj);
        assert_eq!(root, proj);
        assert_eq!(source, WorkspaceSource::ProjectFiles);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn detect_workspace_root_falls_back_to_working_directory() {
        let plain = tmp("plain");
        let (root, source) = detect_workspace_root(&plain);
        assert_eq!(root, plain);
        assert_eq!(source, WorkspaceSource::WorkingDirectory);
        let _ = std::fs::remove_dir_all(&plain);
    }

    /// An explicit `runtime.workspace_dir` survives load; a config without
    /// one gets the detected root instead of `None` (which used to leave the
    /// interactive agent with jailed-to-nothing filesystem tools).
    #[test]
    fn load_effective_applies_detected_workspace_root() {
        let dir = tmp("load");
        let base = serde_json::to_value(Config::default()).unwrap();

        let explicit = dir.join("elsewhere");
        std::fs::create_dir_all(&explicit).unwrap();
        let mut with_root = base.clone();
        with_root["runtime"]["workspace_dir"] = explicit.display().to_string().into();
        let with_root_path = dir.join("with-root.json");
        std::fs::write(&with_root_path, with_root.to_string()).unwrap();
        let cfg = Config::load_effective_from(Some(with_root_path)).unwrap();
        assert_eq!(
            cfg.runtime.workspace_dir.as_deref(),
            Some(explicit.as_path()),
            "an explicit workspace_dir must not be overwritten by detection"
        );

        let mut no_root = base;
        no_root["runtime"]["workspace_dir"] = serde_json::Value::Null;
        let no_root_path = dir.join("no-root.json");
        std::fs::write(&no_root_path, no_root.to_string()).unwrap();
        let cfg = Config::load_effective_from(Some(no_root_path)).unwrap();
        let detected = detect_workspace_root(&std::env::current_dir().unwrap()).0;
        assert_eq!(
            cfg.runtime.workspace_dir.as_deref(),
            Some(detected.as_path()),
            "missing workspace_dir must be detected from the working directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
