//! `openrt` — OpenRT command-line interface.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use openrt_checkpoint::{RecoveryPolicy, RecoveryScope};
use openrt_config::Config;
use openrt_core::{CheckpointId, SessionId};
use openrt_orch::{Workflow, DAG_NS};
use openrt_runtime::{CheckpointOptions, Runtime, RuntimeBuilder};
use openrt_webui::CommandPalette;

fn main() {
    let cli = Cli::parse();
    let level = cli.log.as_deref().unwrap_or("info");
    openrt_telemetry::logging::init_logging(level, log_json());
    match run_cli(&cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn log_json() -> bool {
    std::env::var("OPENRT_LOG_JSON")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

#[derive(Parser)]
#[command(name = "openrt", version, about = "Sovereign, local-first, agentic AI runtime", long_about = None)]
struct Cli {
    /// Log level (trace|debug|info|warn|error).
    #[arg(long, global = true)]
    log: Option<String>,

    /// Use the in-memory store (ephemeral, no ~/.openrt writes).
    #[arg(long, global = true)]
    memory: bool,

    /// Force the deterministic mock provider (offline demo, no network).
    #[arg(long, global = true)]
    mock: bool,

    /// Omit the subcommand to start the Agent TUI (`openrt agent` also works).
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Diagnose the local environment (provider, store, resources, policy).
    Doctor,
    /// Version + build info.
    Version,
    /// Model provider introspection (current provider only in Phase 1).
    Model {
        #[command(subcommand)]
        cmd: ModelCmd,
    },
    /// Tool registry introspection.
    Tool {
        #[command(subcommand)]
        cmd: ToolCmd,
    },
    /// Session management.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Persistent workspace (M5.1): create, open, reopen, list.
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// Phase 11 workbench (Tracks B–D): agent rooms and material records
    /// layered over workspaces (extend, never duplicate the M5.1 records).
    Workbench {
        #[command(subcommand)]
        cmd: WorkbenchCmd,
    },
    /// Agent rooms (M11.10 `room *` — the same handlers as `workbench room`).
    Room {
        #[command(subcommand)]
        cmd: RoomCmd,
    },
    /// Material records (M11.10 `material *` — same handlers as
    /// `workbench material`).
    Material {
        #[command(subcommand)]
        cmd: MaterialCmd,
    },
    /// Phase 11 ComputerUse (M11.10 `computer *`): observe / resolve /
    /// execute one action through the single `computer.use` gate.
    Computer {
        #[command(subcommand)]
        cmd: ComputerCmd,
    },
    /// Launch the full-screen TUI workbench (M5.1 session/workspace UX).
    Tui,
    /// The Agent TUI (chat-first terminal AI IDE). Also the bare default.
    Agent,
    /// Run a task against an agent.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Config inspection.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Checkpoint management (P2.6): list, inspect, delete.
    Checkpoint {
        #[command(subcommand)]
        cmd: CheckpointCmd,
    },
    /// Operating mode (ADR-003): Provisioning / Sovereign.
    Mode {
        #[command(subcommand)]
        cmd: ModeCmd,
    },
    /// Knowledge operations: persist chunks, retrieve evidence, and get an
    /// honest, fail-closed RAG answer grounded ONLY on verified persisted
    /// evidence. Citations are never fabricated.
    Knowledge {
        #[command(subcommand)]
        cmd: KnowledgeCmd,
    },
    /// Document ingestion (M9.1): content-based inspection with honest
    /// SUPPORTED/UNSUPPORTED/CORRUPT/AMBIGUOUS statuses, corpus intake,
    /// listing, and inspection of stored documents.
    Ingest {
        #[command(subcommand)]
        cmd: IngestCmd,
    },
    /// Vision capabilities and image analysis (M9.3): honest VLM status,
    /// image description through the images transport, and the
    /// engineering-drawing route (real OCR regions + VLM parts).
    Vision {
        #[command(subcommand)]
        cmd: VisionCmd,
    },
    /// Multimodal capability matrix (M9.10): honest per-modality intake
    /// statuses plus the Phase 9 backend rows — the SAME probes `openrt
    /// doctor` uses (one capability authority, never a greener second claim).
    Multimodal {
        #[command(subcommand)]
        cmd: MultimodalCmd,
    },
    /// Answer verification + hallucination firewall (M9.8/M9.9): `claim`
    /// runs the production firewall over one claim; `answer` walks the full
    /// hardened answer path (scan -> trust-labeled context -> grounded
    /// answer -> firewall). Exit codes: 0 pass, 2 flag, 3 block.
    Verify {
        #[command(subcommand)]
        cmd: VerifyCmd,
    },
    /// Execution history (M5.2): verify, replay (time machine), fork.
    History {
        #[command(subcommand)]
        cmd: HistoryCmd,
    },
    /// Phase 10 crash recovery (M10.4): scan, records, restore preview,
    /// authorized rollback, interrupted-task resume.
    Recovery {
        #[command(subcommand)]
        cmd: RecoveryCmd,
    },
    /// AgentShield (M10.9): advisory scans, recorded findings, inspection.
    Shield {
        #[command(subcommand)]
        cmd: ShieldCmd,
    },
    /// Skills (M10.6): inventory, loader reports, execution, marketplace.
    Skill {
        #[command(subcommand)]
        cmd: SkillCmd,
    },
    /// Extensions/plugins (Phase 11 M11.1/M11.2): inventory, install,
    /// validate, scan, operator-gated lifecycle (activate/deactivate/
    /// invalidate/rollback/remove). Packages install Registered/unscanned —
    /// never Active, never pre-scanned.
    Extension {
        #[command(subcommand)]
        cmd: ExtensionCmd,
    },
    /// MCP server registry (Phase 11 M11.2): list/inspect/register,
    /// scan-first enable, disable, 4-level trust, health probe over the
    /// durable `mcp.servers` records (spawn gate fail-closed).
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Instincts (M10.7): inventory and inspection.
    Instinct {
        #[command(subcommand)]
        cmd: InstinctCmd,
    },
    /// Agent genomes (M10.8): inventory, inspection, approval-gated evolution.
    Genome {
        #[command(subcommand)]
        cmd: GenomeCmd,
    },
    /// Memory (M5.3): remember/recall semantic facts and episodic records
    /// with verified citations (fail-closed; never invents evidence).
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Workspace assets (M5.4/M5.5): skills, hooks, rules, genome, packaging.
    Assets {
        #[command(subcommand)]
        cmd: AssetsCmd,
    },
    /// Self-improvement (M5.6/M5.7): baselines, proposals, approval-gated
    /// apply, and learning derived from execution history.
    Improve {
        #[command(subcommand)]
        cmd: ImproveCmd,
    },
    /// Local Web UI (M5.8): localhost-only command palette.
    Webui {
        #[command(subcommand)]
        cmd: WebuiCmd,
    },
    /// Phase 6 workflow compiler: compile / inspect / validate / explain / run
    /// a natural-language goal through the production pipeline.
    Workflow {
        #[command(subcommand)]
        cmd: WorkflowCmd,
    },
    /// ADR-002 audit log: verify the chain, search entries, retention status.
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
    /// Versioned security policy document (M6.2): operator-only appends.
    Policy {
        #[command(subcommand)]
        cmd: PolicyCmd,
    },
    /// Encrypted secrets at rest (M6.3): set/get/delete/list/rotate.
    Secret {
        #[command(subcommand)]
        cmd: SecretCmd,
    },
    /// Default-deny egress grants (M6.3): grant/revoke/list.
    Netpol {
        #[command(subcommand)]
        cmd: NetpolCmd,
    },
    /// Sandbox authority (Phase 8): capabilities, profiles, instances.
    Sandbox {
        #[command(subcommand)]
        cmd: SandboxCmd,
    },
    /// Enclave workers (M6.4, ADR-004): enroll, nodes, quarantine, migrate.
    Enclave {
        #[command(subcommand)]
        cmd: EnclaveCmd,
    },
    /// Production observability (M6.4, ADR-005): health, retention, export.
    Obs {
        #[command(subcommand)]
        cmd: ObsCmd,
    },
    /// Autonomous unattended runs (M6.1 remainder): create, list, stop, takeover.
    Autonomous {
        #[command(subcommand)]
        cmd: AutonomousCmd,
    },
    /// Phase 12 industrial intelligence (M12.1–M12.7): advisory assets,
    /// graph, telemetry analysis, RCA, FMEA, risk, scenarios. Advisory
    /// only — no physical-control command exists here.
    Industrial {
        #[command(subcommand)]
        cmd: IndustrialCmd,
    },
    /// Phase 12 research-only trading (M12.8–M12.11): instruments,
    /// datasets, strategies, backtests, walk-forward, experiments. No
    /// broker; `execute` always refuses (the one audit point).
    Trading {
        #[command(subcommand)]
        cmd: TradingCmd,
    },
    /// Phase 12 self-improvement (M12.12–M12.20): experience episodes,
    /// measured benchmarks, benchmark-gated promotions, rollbacks, model
    /// experiments (honest no-backend failures), routing signals.
    Learn {
        #[command(subcommand)]
        cmd: LearnCmd,
    },
    /// Phase 13 Parts L–T: the CyberSaarthi + OpenRT SIH hero demo —
    /// a synthetic case through ingest, graph, retrieval, workflow,
    /// verification, injection defense, and a five-file report.
    Sih {
        #[command(subcommand)]
        cmd: SihCmd,
    },
}

#[derive(Subcommand)]
enum WebuiCmd {
    /// Serve the loopback-only workbench UI. Prints a one-time operator
    /// token for approval commands (server-issued; never client-asserted).
    Serve {
        /// TCP port (0 = ephemeral).
        #[arg(long, default_value_t = 7788)]
        port: u16,
        /// Operator id the printed token authenticates.
        #[arg(long, default_value = "")]
        operator: String,
    },
}

#[derive(Subcommand)]
enum AssetsCmd {
    /// List skill id/activation pairs (or full JSON).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Register a skill (Registered until validate+activate).
    Register {
        id: String,
        #[arg(long)]
        description: String,
        /// Capability the skill requires (repeatable).
        #[arg(long = "perm")]
        permissions: Vec<String>,
    },
    /// Generate a history-derived skill candidate (real ledger evidence).
    Generate {
        /// Unique request id for this generation.
        request_id: String,
        #[arg(long)]
        description: String,
        #[arg(long = "perm")]
        permissions: Vec<String>,
        /// Session id to pull tool evidence from (empty = whole ledger).
        #[arg(long, default_value = "")]
        session: String,
    },
    /// Validate a skill's dependencies (Registered → Validated).
    Validate { id: String },
    /// Activate a validated skill under an operator grant.
    Activate {
        id: String,
        /// Operator id (must be in the asset store allowlist).
        #[arg(long)]
        operator: String,
    },
    /// Validate a history-derived candidate (Generated → Validated).
    CandidateValidate {
        /// Candidate id (`cand.…`).
        candidate: String,
    },
    /// Operator-approve a validated candidate (Validated → Approved).
    CandidateApprove {
        candidate: String,
        /// Operator id (must be in the asset store allowlist).
        #[arg(long)]
        operator: String,
    },
    /// Scan-gated activation of an approved candidate: operator grant AND
    /// the AgentShield content scan BEFORE the skill enters the registry.
    CandidateActivate {
        candidate: String,
        /// Operator id (must be in the asset store allowlist).
        #[arg(long)]
        operator: String,
    },
    /// Run one hook by id (permission consults the gate policy).
    Hook { id: String },
    /// Evaluate a rule decision (allow/deny/conflict) for scope+subject.
    Rule { scope: String, subject: String },
    /// Apply a genome: activate every referenced skill/hook workspace-wide,
    /// or pin it to a single session's reference with `--session`.
    Genome {
        id: String,
        #[arg(long)]
        operator: String,
        /// Session id: pin the genome as that session's reference instead
        /// of the workspace-wide apply.
        #[arg(long)]
        session: Option<String>,
    },
    /// Export every sealed asset as a JSON package on stdout.
    Export,
    /// Import a JSON package (digest re-verified fail-closed).
    Import { path: String },
}

#[derive(Subcommand)]
enum ImproveCmd {
    /// List proposals with their review state.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Record a metric baseline.
    Baseline {
        id: String,
        #[arg(long)]
        metric: String,
        #[arg(long)]
        value: f64,
        /// `higher_is_better` or `lower_is_better`.
        #[arg(long, default_value = "lower_is_better")]
        direction: String,
        #[arg(long)]
        run: String,
    },
    /// Propose a change against a target (self_optimize needs baseline+expected).
    Propose {
        id: String,
        /// Proposal kind: self_debug | self_optimize | research_change.
        #[arg(long, default_value = "self_optimize")]
        kind: String,
        #[arg(long)]
        summary: String,
        /// Target kind: router | tool | plan | prompt | workflow | policy | ...
        #[arg(long, default_value = "router")]
        target_kind: String,
        #[arg(long)]
        target_id: String,
        #[arg(long)]
        baseline: Option<String>,
        #[arg(long)]
        expected: Option<f64>,
    },
    /// Evaluate a proposal against its baseline (regression → rejected).
    Evaluate { id: String },
    /// Operator approval gate (optional decision note is persisted).
    Approve {
        id: String,
        #[arg(long)]
        operator: String,
        /// Optional free-text decision note recorded on the proposal.
        #[arg(long)]
        note: Option<String>,
    },
    /// Operator rejection (optional decision note is persisted).
    Reject {
        id: String,
        #[arg(long)]
        operator: String,
        /// Optional free-text decision note recorded on the proposal.
        #[arg(long)]
        note: Option<String>,
    },
    /// Apply an approved proposal through the production apply target.
    Apply {
        id: String,
        #[arg(long)]
        operator: String,
    },
    /// Derive learnings from the history ledger for a session.
    Learn {
        #[arg(long)]
        run: String,
        #[arg(long)]
        session: String,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// Register a durable origin-run so memory citations can re-resolve.
    Origin {
        origin_run: String,
        canonical_origin: String,
    },
    /// Persist a semantic fact (digest-sealed, citation re-validated).
    Remember {
        /// Stable id for the fact (path-safe; used as the retrieval document id).
        id: String,
        /// Origin run id (must be registered via `memory origin` first).
        #[arg(long)]
        origin_run: String,
        /// Source label the citation re-resolves against.
        #[arg(long)]
        origin_source: String,
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        value: String,
        #[arg(long)]
        occurred_at: String,
        /// Explicitly supersede an existing fact with the same id.
        #[arg(long)]
        r#override: bool,
    },
    /// Query-backed recall through Phase-4 retrieval (verified citations only).
    Recall {
        query: String,
        #[arg(long, default_value_t = 10)]
        top_k: usize,
        #[arg(long, default_value_t = 0.0)]
        threshold: f32,
        #[arg(long)]
        json: bool,
    },
    /// Persist an episodic run record (digest-sealed, citation re-resolved).
    Episode {
        run_id: String,
        session_id: String,
        category: String,
        occurred_at: String,
        payload: String,
        #[arg(long)]
        origin_run: String,
        #[arg(long)]
        origin_source: String,
        #[arg(long)]
        r#override: bool,
    },
    /// Recall one episodic record by digest (fail-closed digest + citation).
    RecallEpisode {
        digest: String,
        #[arg(long)]
        json: bool,
    },
    /// List every durable semantic item that re-validates (honest empty when none).
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum HistoryCmd {
    /// Recompute every ledger digest and confirm the chain (fail-closed).
    Verify,
    /// Current tail sequence of the parent ledger.
    Tail,
    /// Time-machine view: materialize the ledger as of a sequence number
    /// (read-only; never rewrites later history).
    Replay {
        /// Sequence number to materialize as-of (inclusive).
        as_of: u64,
        #[arg(long)]
        json: bool,
    },
    /// Fork an isolated append-only branch from the head (or `--base seq.N`).
    Fork {
        branch: String,
        /// Base label, e.g. `seq.000000000003`; empty = current head.
        #[arg(long, default_value = "")]
        base: String,
    },
    /// Record a what-if row on a forked branch (append-only; the parent
    /// ledger is never rewritten).
    ForkCommit {
        branch: String,
        /// Node id to anchor the row on the branch.
        node: String,
        /// Status stamp for the row (e.g. `ok`, `divergent`).
        #[arg(long)]
        status: String,
        /// Event kind recorded on the branch row.
        #[arg(long)]
        event: String,
        /// Free-text note recorded on the row.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Verify a fork branch's own chain (the parent chain stays
    /// `history verify`).
    ForkVerify { branch: String },
    /// Compare a fork branch against the parent ledger (read-only
    /// divergence verdict: shared prefix + per-sequence differences).
    Compare { branch: String },
    /// Leave the what-if view and return to the current head (tail +
    /// fork catalog).
    Return,
    /// Show the last `--limit` ledger rows (defaults to full replay).
    List {
        #[arg(long, default_value_t = 50)]
        limit: u64,
        #[arg(long)]
        json: bool,
    },
    /// M10.5 chronological timeline of the ledger (optional session filter,
    /// read-only — the same verified rows `history verify` chains).
    Timeline {
        /// Only rows correlated to this session id.
        #[arg(long)]
        session: Option<String>,
        /// First sequence to include (0 or 1 = genesis).
        #[arg(long, default_value_t = 0)]
        from_seq: u64,
        /// Cap to the most recent N rows of the filtered window.
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
}

/// Phase 10 crash recovery (M10.4): one authority for scan/records/rollback.
#[derive(Subcommand)]
enum RecoveryCmd {
    /// Run the boot-time recovery scan now (writes a fresh evidence record).
    Scan,
    /// List boot-recovery records (one per scan/boot).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one boot-recovery record in full (interrupted tasks, incomplete
    /// runs, stale sandboxes).
    Inspect {
        /// Record id (`boot-…`) or boot id.
        record: String,
        #[arg(long)]
        json: bool,
    },
    /// Preview a checkpoint restore: integrity verdict, policy compatibility,
    /// and the planned rollback ops — read-only (apply with `recovery rollback`).
    Restore {
        checkpoint: String,
        #[arg(long)]
        json: bool,
    },
    /// Authorized production rollback: operator-authenticated, fail-closed on
    /// policy drift (unless acknowledged), history ledger never rewritten.
    Rollback {
        checkpoint: String,
        /// Trusted operator identity (checked against the asset operator set).
        #[arg(long)]
        operator: String,
        /// Acknowledge that the checkpoint was sealed under a different (or
        /// unknown) permission-policy fingerprint.
        #[arg(long)]
        ack_policy_drift: bool,
        /// Workspace root for file restores (blackboard/metadata always restore).
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Resume an interrupted live task after crash recovery.
    Resume {
        task: String,
        #[arg(long)]
        json: bool,
    },
}

/// AgentShield (M10.9): advisory scans through the gate's own layer.
#[derive(Subcommand)]
enum ShieldCmd {
    /// Scan a tool call WITHOUT executing it (same evaluator as the gate;
    /// nothing is recorded — enforcement happens only on real gate calls).
    Scan {
        /// Tool id to scan for (e.g. `terminal.exec`).
        #[arg(long)]
        tool: String,
        /// JSON arguments, e.g. `'{"command":"rm -rf /"}'`.
        #[arg(long)]
        args: String,
        #[arg(long)]
        json: bool,
    },
    /// List recorded findings (newest first).
    Findings {
        /// Filter by status: open, warned, approval_required, denied, halted, reported.
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Inspect one finding by id.
    Inspect {
        finding: String,
        #[arg(long)]
        json: bool,
    },
}

/// Skills (M10.6): inventory → loader → execution → marketplace.
#[derive(Subcommand)]
enum SkillCmd {
    /// List installed skill versions and activation states.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Run the production loader's full check suite against one version.
    Inspect {
        id: String,
        #[arg(long)]
        version: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Resolve a skill (digest-checked) and show effective permissions.
    Load {
        id: String,
        #[arg(long)]
        version: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Execute an ACTIVE skill (activation is operator-gated — fail-closed).
    Execute {
        id: String,
        #[arg(long)]
        version: Option<u32>,
        /// Session id; a `cli-skill` session is created if omitted.
        #[arg(long)]
        session: Option<String>,
    },
    /// Compile a skill's goal into a workflow and link it (operator action).
    Compile {
        id: String,
        #[arg(long)]
        version: Option<u32>,
    },
    /// Run a skill's attached tests (read-only verdicts).
    Test {
        id: String,
        #[arg(long)]
        version: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Skill marketplace (local catalog works; remote is honest UNAVAILABLE
    /// under Sovereign Mode — never a fake empty catalog).
    Marketplace {
        #[command(subcommand)]
        cmd: MarketplaceCmd,
    },
}

#[derive(Subcommand)]
enum MarketplaceCmd {
    /// List catalog entries.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Fetch a remote catalog URL (refused under Sovereign Mode with the
    /// real reason).
    Fetch { url: String },
}

/// Extensions (Phase 11 M11.1/M11.2): declarative packages over the ONE
/// skill/registry/permission/sandbox/audit authority — extensions never ship
/// executable code and never arrive Active.
#[derive(Subcommand)]
enum ExtensionCmd {
    /// List every installed extension version (activation + security status).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one extension version's full manifest (defaults to latest;
    /// always printed as pretty JSON).
    Inspect {
        id: String,
        #[arg(long)]
        version: Option<u32>,
    },
    /// Install a package JSON file (payload skills + extension land
    /// Registered/unscanned — activation stays a separate operator action).
    Install {
        /// Path to an exported `ExtensionPackage` JSON file.
        path: String,
    },
    /// Validate a version (dependencies, conflicts, capabilities — full
    /// analysis; `Registered/Inactive → Validated`).
    Validate {
        id: String,
        #[arg(long)]
        version: Option<u32>,
    },
    /// Run the ONE AgentShield scan over an extension manifest and record
    /// the verdict (clean/flagged + findings). Exit: 0 clean/warn,
    /// 2 require-approval, 3 deny/halt.
    Scan {
        id: String,
        #[arg(long)]
        version: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Operator activation (scan-first, fail-closed on flagged; bindings
    /// take effect on the next build).
    Activate {
        id: String,
        #[arg(long)]
        version: Option<u32>,
        /// Operator id (must be in the asset store allowlist).
        #[arg(long)]
        operator: String,
    },
    /// Operator deactivation of the Active version.
    Deactivate {
        id: String,
        /// Operator id (must be in the asset store allowlist).
        #[arg(long)]
        operator: String,
    },
    /// Tombstone the latest version (activation/rollback refuse it; history
    /// is never deleted).
    Invalidate {
        id: String,
        /// Operator id (must be in the asset store allowlist).
        #[arg(long)]
        operator: String,
    },
    /// Roll back to a specific (or next-lower) version through the full
    /// activation gate — all versions are preserved.
    Rollback {
        id: String,
        /// Target version (default: next-lower than Active).
        #[arg(long)]
        target: Option<u32>,
        /// Operator id (must be in the asset store allowlist).
        #[arg(long)]
        operator: String,
    },
    /// Remove every stored version — refused while Active or depended on.
    Remove {
        id: String,
        /// Operator id (must be in the asset store allowlist).
        #[arg(long)]
        operator: String,
    },
}

/// MCP server registry (Phase 11 M11.2): the durable `mcp.servers`
/// records — registration, enable/disable, 4-level trust
/// (untrusted/review/trusted/blocked), ONE AgentShield scan, health
/// probes. Spawn gate is fail-closed: enabled + connectable trust + not
/// flagged; trust never bypasses permissions/policy/sandbox/mode.
#[derive(Subcommand)]
enum McpCmd {
    /// List every registered MCP server (trust, enabled, security, health).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one server's full record + live connection view + sources.
    Inspect {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Register a stdio server: unscanned + disabled + untrusted (scan →
    /// enable → trust are separate operator actions; remote/HTTP MCP is
    /// honest UNAVAILABLE and refused here).
    Register {
        /// Registry id (lowercase `[a-z0-9_-]`; tools import as `mcp.<id>.*`).
        name: String,
        /// Executable to spawn (stdio only — no shell).
        #[arg(long)]
        command: String,
        /// Extra argument for the command (repeatable, order preserved).
        #[arg(long = "arg")]
        args: Vec<String>,
        /// Environment variable NAME resolved from the process environment
        /// at spawn (values are never stored or logged).
        #[arg(long = "env-key")]
        env_keys: Vec<String>,
        /// Declared capability for the review surface (repeatable).
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        /// Operator id (recorded on the record + audit log).
        #[arg(long)]
        operator: String,
    },
    /// Scan-first enable (flagged → fail closed; connects on the next
    /// build through the registry spawn gate).
    Enable {
        name: String,
        /// Operator id (recorded in the audit log).
        #[arg(long)]
        operator: String,
    },
    /// Disable (stops connecting on the next build).
    Disable {
        name: String,
        /// Operator id (recorded in the audit log).
        #[arg(long)]
        operator: String,
    },
    /// Set trust: `untrusted | review | trusted | blocked` (escalation to
    /// `trusted` requires a clean scan verdict).
    Trust {
        name: String,
        /// New trust level (untrusted|review|trusted|blocked).
        level: String,
        /// Operator id (recorded in the audit log).
        #[arg(long)]
        operator: String,
    },
    /// Run the ONE AgentShield scan over the server's declaration and
    /// record the verdict. Exit: 0 clean/warn, 2 require-approval, 3 deny/halt.
    Scan {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Health probe: connect + initialize + tools/list, then shutdown —
    /// persists the verdict and discovered-tools snapshot.
    /// Exit: 0 healthy, 1 unreachable.
    Health {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

/// Instincts (M10.7): stored learned behaviors.
#[derive(Subcommand)]
enum InstinctCmd {
    /// List every stored instinct.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one instinct in full (trigger, recommendation, evidence).
    Inspect {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Derive a new instinct from a session's real ledger evidence (lands
    /// Registered — validate + activate through the asset store next).
    Generate {
        /// Session id to learn from.
        #[arg(long)]
        session: String,
    },
    /// Validate a generated instinct (Registered → Validated).
    Validate { id: String },
    /// Activate a validated instinct under an operator grant (the instinct
    /// never grants permissions — it only becomes eligible for influence).
    Activate {
        id: String,
        /// Operator id (must be in the asset store allowlist).
        #[arg(long)]
        operator: String,
    },
}

/// Agent genomes (M10.8): profiles, validation, approval-gated evolution.
#[derive(Subcommand)]
enum GenomeCmd {
    /// List every stored genome.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one genome in full (profile, tools, permissions, provenance).
    Inspect {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Propose an evolution of a genome (approval-gated — nothing applies
    /// until an operator approves through `improve approve/apply`).
    Evolve {
        /// Genome id to evolve.
        id: String,
        /// Human-readable rationale recorded on the proposal.
        summary: String,
        /// Proposal id (generated when omitted).
        #[arg(long)]
        proposal: Option<String>,
    },
}

#[derive(Subcommand)]
enum CheckpointCmd {
    /// List checkpoints, newest first (optionally filtered by workflow).
    List {
        /// Filter by workflow id.
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one checkpoint in full (node states, retries, chain links).
    Inspect { checkpoint: String },
    /// Delete a checkpoint record.
    Delete { checkpoint: String },
}

#[derive(Subcommand)]
enum ModelCmd {
    /// Show the default provider and model capabilities.
    List,
    /// Show details (kind, default model, capabilities).
    Info,
    /// Registry card + manifest + lifecycle for one artifact.
    Show { id: String },
    /// List registry cards (installed + curated), optionally one artifact.
    Status {
        /// Artifact id; list everything when omitted.
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Search the curated library + installed registry (offline). With `--hf`,
    /// also query the Hugging Face hub — a network op that requires
    /// Provisioning mode (refused before any socket in Sovereign).
    Search {
        query: String,
        /// Enable the (mode-gated) Hugging Face discovery leg.
        #[arg(long)]
        hf: bool,
    },
    /// Register a local artifact **by reference**: stream its sha256, inspect
    /// its header, and record a path in the registry without copying a byte.
    ///
    /// This is the route for artifacts larger than available RAM. It stops at
    /// `Verified` — registration is not loading, and activation still requires a
    /// scanned, materialized artifact. Requires Provisioning mode.
    Register {
        /// Registry artifact id (e.g. `local/chat/qwen3.5-9b-q8_0`).
        id: String,
        /// Path to the artifact file.
        path: String,
    },
    /// Import a local artifact: verify sha256, persist blobs, publish the
    /// immutable manifest, and (with a signed scan bundle) run the scan gate.
    ///
    /// Materializes the artifact into the content store, which means holding it
    /// in memory: for a multi-gigabyte model use `model register` instead.
    /// Requires Provisioning mode.
    Install {
        /// Registry artifact id (e.g. `curated/llama3.1-8b`).
        id: String,
        /// Path to the artifact file (single-file install).
        path: String,
        /// Path to a signed `SignatureBundle` JSON; runs the scan gate.
        #[arg(long)]
        scan_bundle: Option<String>,
        /// Ed25519 public key (32 raw bytes) trusted to sign bundles. Pass to
        /// verify bundles before trusting them; with no key, bundles are used
        /// as-is (documented trust decision).
        #[arg(long)]
        trust_key: Vec<String>,
    },
    /// Activate a scanned artifact (`Scanned -> Active`), making it routable.
    Activate { id: String },
    /// Soft-remove by retiring the artifact (terminal state). Bytes stay
    /// stored; the artifact is no longer activatable or routable.
    Remove {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Route-check one artifact; prints whether it is eligible to serve.
    Route { id: String },
    /// Benchmark an active artifact through the configured provider and record
    /// the results (`bench.results`).
    Benchmark {
        id: String,
        #[arg(long)]
        repetitions: Option<u8>,
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
    /// Hardware compatibility report for an artifact: declared size vs explicit
    /// VRAM / RAM budgets (default 8 GiB VRAM, 32 GiB RAM) plus the runtime's
    /// M6 capability-aware memory plan (registry metadata + provider
    /// capabilities + ResourceGate posture). Observation-only — never
    /// fabricates hardware measurements and never moves memory.
    Compatibility {
        id: String,
        #[arg(long)]
        vram_mb: Option<u64>,
        #[arg(long)]
        ram_mb: Option<u64>,
        /// Request the model run with this context window (tokens). Reports
        /// REQUESTED vs EFFECTIVE; honest `unknown` for the KV cache of a
        /// remote (non-configurable) model, never a fabricated residency.
        #[arg(long)]
        ctx: Option<usize>,
        /// Deterministic JSON of the M6 runtime memory plan.
        #[arg(long)]
        json: bool,
    },
    /// Bind a registry artifact to a provider tag (durable, audited).
    ///
    /// The artifact id and the provider tag are different naming realms; this
    /// makes the mapping explicit so a request for the artifact is never
    /// silently answered by a provider default. Bindings never move bytes and
    /// never change routing authority. Provider defaults to the configured one.
    Bind {
        /// Registry artifact id (e.g. `local/chat/Qwen_Qwen3.5-9B-Q8_0`).
        id: String,
        /// Provider model tag (e.g. `qwen3.5:9b-q8_0`).
        tag: String,
        /// Provider id; defaults to the configured provider.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value = "operator")]
        actor: String,
    },
    /// Remove the artifact → provider-tag mapping (idempotent, audited).
    Unbind {
        /// Registry artifact id.
        id: String,
        /// Provider id; defaults to the configured provider.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value = "operator")]
        actor: String,
    },
}

#[derive(Subcommand)]
enum ToolCmd {
    /// List registered tools and their declared capabilities.
    List,
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Create a session.
    Create { title: String },
    /// List sessions (or `--json`).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Switch/show is TUI-only; CLI inspects one session by id.
    Show { id: String },
}

/// Durable workspace subcommands (foundation store, ADR-006 one store).
#[derive(Subcommand)]
enum WorkspaceCmd {
    /// Create a workspace and make it active (store round-trip).
    Create {
        /// Path-safe address code (e.g. `acct/alpha` → keys `w.<sha>`).
        address: String,
        #[arg(long, default_value = "")]
        title: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// Open an existing workspace by address code (fail-closed NotFound).
    Open { address: String },
    /// Reopen the currently active workspace pointer (post-restart UX).
    Reopen,
    /// List persisted workspaces (or `--json`).
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum WorkbenchCmd {
    /// Agent rooms (Phase 11 Track C): create/list/show, lifecycle, steer,
    /// material + session links — the collaboration surface over workspaces.
    Room {
        #[command(subcommand)]
        cmd: RoomCmd,
    },
    /// Material records (Phase 11 Track D): register/list/verify typed
    /// inputs & outputs by reference (never copies), workbench attach.
    Material {
        #[command(subcommand)]
        cmd: MaterialCmd,
    },
}

#[derive(Subcommand)]
enum RoomCmd {
    /// Create a room in an existing workbench (fail-closed unknown workbench).
    Create {
        /// Workbench address code.
        #[arg(long)]
        workbench: String,
        /// Room title.
        #[arg(long)]
        title: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// List all rooms.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one room (full record: links, history, steer log).
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Set room lifecycle status (`active` | `paused`).
    Status {
        id: String,
        status: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Point the room at a workflow (bookkeeping — checkpoints remain the
    /// single run/recovery authority; steer hydrates into this run).
    SetWorkflow {
        id: String,
        workflow_id: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Clear the room's active workflow pointer.
    ClearWorkflow {
        id: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Append an operator steer directive (append-only; hydrated into the
    /// next run's blackboard, never rewritten).
    Steer {
        id: String,
        input: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Attach an existing material to the room (validated, idempotent).
    AttachMaterial {
        id: String,
        material: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Detach a material (the record itself is never destroyed).
    DetachMaterial {
        id: String,
        material: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Link a session into the room (idempotent).
    LinkSession {
        id: String,
        session: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Role-configured room agents (M11.4): roles are configuration mapped
    /// onto existing sub-agent identities, never new authorities.
    Agent {
        #[command(subcommand)]
        cmd: RoomAgentCmd,
    },
}

#[derive(Subcommand)]
enum RoomAgentCmd {
    /// Assign an agent to the room under a role
    /// (researcher|analyst|coder|reviewer|verifier — fail-closed).
    Add {
        id: String,
        agent: String,
        /// Room role (maps onto an existing SubAgentKind identity).
        role: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove the role assignment (identity untouched).
    Remove {
        id: String,
        agent: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// List the room's role-configured agents.
    List {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum MaterialCmd {
    /// Register a material by reference (exactly one of --ref-ns/--ref-key
    /// or --ref-path). Fail-closed: the content must exist and digest now.
    Create {
        /// Kind: document|image|dataset|evidence|artifact|workflow|knowledge|session|task.
        #[arg(long)]
        kind: String,
        #[arg(long)]
        title: String,
        /// Free-form provenance.
        #[arg(long, default_value = "cli")]
        source: String,
        /// Store-backed reference: namespace…
        #[arg(long)]
        ref_ns: Option<String>,
        /// …and key (both required together).
        #[arg(long)]
        ref_key: Option<String>,
        /// File-backed reference (mutually exclusive with ns/key).
        #[arg(long)]
        ref_path: Option<String>,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// List all materials.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one material (full record incl. digest + links).
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Re-derive the digest from the live reference and compare.
    /// Exit codes: 0 intact, 2 drifted/unresolvable (fail-closed).
    Verify {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Attach the material to a workbench (validated, idempotent).
    Attach {
        id: String,
        #[arg(long)]
        workbench: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Detach from a workbench (never destroys the record).
    Detach {
        id: String,
        #[arg(long)]
        workbench: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ComputerCmd {
    /// Environment profile + capability matrix + resolver decisions
    /// (read-only platform introspection).
    Env {
        #[arg(long)]
        json: bool,
    },
    /// One observation bundle (screenshot/windows/elements/focus/cursor/
    /// profile; clipboard only with `--clipboard`).
    Observe {
        /// Include clipboard content in the bundle (on-demand read).
        #[arg(long)]
        clipboard: bool,
        #[arg(long)]
        json: bool,
    },
    /// Resolve a semantic target through the full chain (accessibility →
    /// window tree → elements → vision → coordinate) and print the trace.
    Resolve {
        /// Semantic target (window/element name); omit with --x/--y for a
        /// pure coordinate resolution.
        target: Option<String>,
        #[arg(long)]
        x: Option<i64>,
        #[arg(long)]
        y: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Execute one computer-use action through the ONE `computer.use` gate
    /// (policy + AgentShield + audit; requires an allow rule for
    /// `computer.use.all` — deny-by-default like every other tool).
    Use {
        /// focus | launch | click | type | hotkey | scroll | drag | workspace
        /// | clipboard | close | navigate
        #[arg(long)]
        action: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        x: Option<i64>,
        #[arg(long)]
        y: Option<i64>,
        #[arg(long)]
        from_x: Option<i64>,
        #[arg(long)]
        from_y: Option<i64>,
        #[arg(long)]
        to_x: Option<i64>,
        #[arg(long)]
        to_y: Option<i64>,
        #[arg(long, default_value_t = 1)]
        button: u32,
        #[arg(long)]
        delta: Option<i64>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Run a prompt to completion in a session.
    Run {
        prompt: String,
        /// Session id; created (title `cli`) if omitted.
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List tasks for a session (or `--json`).
    List {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Resume a workflow run from a checkpoint. Runs every eligible step —
    /// completed ones are never re-executed, failed/uncertain-clean ones are
    /// retried with their cumulative attempt budgets.
    Resume {
        /// Session id owning the run.
        #[arg(long)]
        session: String,
        /// Workflow id (must match the checkpoint's workflow).
        #[arg(long)]
        workflow: String,
        /// Checkpoint id to resume from.
        #[arg(long)]
        checkpoint: String,
    },
    /// Retry ONLY the failed nodes of a checkpoint; never enqueues new pending
    /// work. Replays only nodes whose side-effect classification is clean.
    Retry {
        #[arg(long)]
        session: String,
        #[arg(long)]
        workflow: String,
        #[arg(long)]
        checkpoint: String,
    },
    /// Part C: cooperatively pause a live in-process task — it stops at its
    /// next turn boundary and persists as `Paused` (not terminal; tool rounds
    /// are folded into the session).
    Pause {
        task_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Part C: resume a `Paused`/`Interrupted` task. Distinct from `task
    /// resume`, which resumes a workflow checkpoint chain.
    ResumeTask {
        task_id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum WorkflowCmd {
    /// Compile a natural-language goal into a persisted, validated workflow
    /// (capability/permission/Sovereign/resource gates applied; fails closed).
    Compile {
        /// Natural-language goal.
        goal: String,
        #[arg(long, default_value = "compile")]
        mode: String,
        #[arg(long)]
        json: bool,
    },
    /// Inspect a compiled workflow (record + DAG + provenance).
    Inspect {
        workflow_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Re-run the 15-point validation suite for a compiled workflow.
    Validate {
        workflow_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Explain why a compiled workflow's nodes exist (provenance).
    Explain {
        workflow_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Compile (Execute mode) and hand off to the existing orchestrator.
    Run {
        /// Natural-language goal.
        goal: String,
        /// Session id; created (title `workflow`) if omitted.
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List compiled workflow records.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Phase 11 (Track E): request a cooperative pause of the in-process
    /// run — the engine stops spawning new steps at its next checkpoint,
    /// drains in-flight steps, and records `paused` (partial history
    /// preserved; never a failure). Fails closed when no run is active.
    Pause {
        workflow_id: String,
        /// Operator id recorded in the audit log.
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Phase 11 (Track E): explicitly resume the latest incomplete run from
    /// its newest checkpoint — the ONLY resume path (boot never auto-resumes).
    Resume {
        workflow_id: String,
        /// Operator id recorded in the audit log.
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the effective config (JSON).
    Show,
}

#[derive(Subcommand)]
enum KnowledgeCmd {
    /// Persist a real chunk (its real text sha256) into a KB's corpus and
    /// FAIL-CLOSED on the pain: a chunk is only reported stored after the
    /// persistence round-trip returns Ok.
    PersistChunk {
        /// Which knowledge corpus the chunk belongs to.
        kb: String,
        /// Document id for the chunk.
        document_id: String,
        /// Chunk id within the document.
        chunk_id: String,
        /// The chunk's real text; sha256 is computed from THIS text.
        text: String,
    },
    /// Retrieve scored evidence hits for a query. Fail-closed: a hit enters
    /// the report ONLY if it re-validates against a real persisted chunk.
    /// (`openrt knowledge search` is an alias for this command.)
    #[command(alias = "search")]
    Retrieve {
        /// Which persisted corpus to query.
        kb: String,
        /// The query text.
        query: String,
        #[arg(long, default_value_t = 3)]
        top_k: usize,
        #[arg(long, default_value_t = 0.6)]
        threshold: f32,
    },
    /// Dense (neural) retrieval over the derived dense vector index (M9.5).
    /// Probe-first honesty: refuses with the backend's real status label
    /// (UNAVAILABLE / MODEL_MISSING) instead of degrading to hash similarity.
    SemanticSearch {
        /// Which persisted corpus to query.
        kb: String,
        /// The query text (embedded by the real neural backend).
        query: String,
        #[arg(long, default_value_t = 3)]
        top_k: usize,
        #[arg(long, default_value_t = 0.6)]
        threshold: f32,
    },
    /// BM25 + dense hybrid retrieval with reranking (M9.6/TEST 6). Reports
    /// per-hit `retrieval_source` provenance and an honest rerank outcome —
    /// never fused scores without provenance, never fabricated rerank scores.
    HybridSearch {
        /// Which persisted corpus to query.
        kb: String,
        /// The query text (lexical + dense + rerank all use this text).
        query: String,
        #[arg(long, default_value_t = 3)]
        top_k: usize,
        #[arg(long, default_value_t = 0.0)]
        threshold: f32,
        /// Metadata filter: only hits whose chunk belongs to this document.
        #[arg(long)]
        document: Option<String>,
        /// Metadata filter: only hits on this page.
        #[arg(long)]
        page: Option<u32>,
        /// Reranker to apply after fusion (default: deterministic).
        #[arg(long, value_enum, default_value_t = RerankArg::Deterministic)]
        rerank: RerankArg,
        /// Model id for `--rerank model` (default: the provider's default).
        #[arg(long)]
        model: Option<String>,
    },
    /// Ask the RAG oracle. It answers ONLY from verified, persisted evidence
    /// and FAILS CLOSED (honest `None` + empty citations) when no evidence
    /// passed retrieval — it never fabricates an answer.
    Answer {
        /// Which persisted corpus to ground the answer on.
        kb: String,
        /// The question to answer from verified evidence only.
        query: String,
        #[arg(long, default_value_t = 3)]
        top_k: usize,
        #[arg(long, default_value_t = 0.6)]
        threshold: f32,
    },
    /// Evidence graph + trust inspection (M9.7/M9.9): append-only integrity
    /// report, claims, UNTRUSTED-label records with injection flags, and
    /// citation resolution for `--cite document:chunk` pairs. Exit 1 when
    /// integrity has problems or a `--cite` does not resolve.
    Evidence {
        /// Which knowledge corpus to inspect.
        kb: String,
        /// Resolve `document:chunk` citations against persisted evidence
        /// (repeatable).
        #[arg(long = "cite")]
        cites: Vec<String>,
        /// Machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Knowledge-graph entities and relations on the shared store (M9.7):
    /// list (default), upsert an entity, add a relation — the graph evolves
    /// in place, never duplicates (ADR-006).
    Graph {
        /// Which knowledge corpus's graph to operate on.
        kb: String,
        /// Action to perform (default: `list`).
        #[command(subcommand)]
        action: Option<GraphCmd>,
    },
}

/// Write/read actions on the knowledge graph (M9.7).
#[derive(Subcommand)]
enum GraphCmd {
    /// List entities and relations (the default action).
    List,
    /// Create or update an entity — idempotent upsert, `created_at`
    /// preserved; the graph record is only reported after the round-trip.
    UpsertEntity {
        /// Stable entity id (content-addressed key, e.g. `launch-window`).
        entity_id: String,
        /// Entity kind (e.g. `event`, `place`, `org`).
        #[arg(long, default_value = "thing")]
        kind: String,
        /// Human-readable label for the entity.
        #[arg(long)]
        label: String,
        /// Alternate names (repeatable).
        #[arg(long = "alias")]
        aliases: Vec<String>,
    },
    /// Add a subject/predicate/object relation — idempotent on the
    /// content-addressed triple key.
    AddRelation {
        /// Relation subject (entity id).
        subject: String,
        /// Relation predicate (e.g. `located_at`, `part_of`).
        predicate: String,
        /// Relation object (entity id or literal).
        object: String,
    },
}

/// CLI choice of reranker — mapped to the library's `RerankMode`; never
/// implies a backend exists (absence is reported honestly at runtime).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum RerankArg {
    /// Keep fused order; no reranking claimed or attempted.
    None,
    /// Deterministic lexical-coverage reranker (always available).
    Deterministic,
    /// Model-backed reranker (real model or honest `UNAVAILABLE` reason).
    Model,
}

#[derive(Subcommand)]
enum IngestCmd {
    /// Detect format + intake status for a file. Read-only: nothing is
    /// persisted; the report carries the honest status reason for every
    /// non-supported outcome.
    Inspect {
        /// File to inspect (read from anywhere — no persistence involved).
        path: PathBuf,
    },
    /// Extract and store a document into the corpus. SEC-01 jail applies:
    /// paths must resolve inside --root (default: current directory).
    Extract {
        /// Document to extract (absolute, or relative to --root).
        path: PathBuf,
        /// Jail root; defaults to the current directory.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// List stored documents (newest first).
    List,
    /// Show a stored document: format, status, provenance, block summary.
    Show {
        /// Document id (`doc/<sha256>`).
        doc_id: String,
        /// Print each block's full text instead of a preview.
        #[arg(long)]
        full: bool,
    },
    /// Remove a document and all of its versions (audited).
    Remove {
        /// Document id (`doc/<sha256>`).
        doc_id: String,
    },
    /// Run real OCR on an image or PDF pages and print word/region
    /// provenance (bbox + confidence from the backend). Read-only.
    Ocr {
        /// Image (png/jpeg/tiff) or PDF to recognize.
        path: PathBuf,
        /// 1-based PDF page (default: every page).
        #[arg(long)]
        page: Option<u32>,
        /// Page-segmentation mode: 3 auto | 4 column | 6 block | 11 sparse.
        #[arg(long, default_value_t = 3)]
        psm: u8,
        /// Also print per-word boxes and confidences.
        #[arg(long)]
        words: bool,
    },
    /// Layout analysis from the backend's real boxes: regions, lines, and
    /// words with normalized bboxes + confidences. Read-only.
    Analyze {
        /// Image or PDF to analyze.
        path: PathBuf,
        /// 1-based PDF page (default: every page).
        #[arg(long)]
        page: Option<u32>,
    },
}

#[derive(Subcommand)]
enum VisionCmd {
    /// Honest vision capability matrix: VLM status (AVAILABLE / UNAVAILABLE /
    /// MODEL_MISSING with reason) plus the image-OCR and page-render rows.
    Capabilities {
        /// Vision model to probe (default: moondream).
        #[arg(long)]
        model: Option<String>,
    },
    /// Describe an image with the vision model through the images transport.
    /// Fails closed with the structured status when the model is absent.
    Describe {
        /// Image to describe (png/jpeg/tiff).
        path: PathBuf,
        /// Vision model to use (default: moondream).
        #[arg(long)]
        model: Option<String>,
    },
    /// Engineering-drawing route: real OCR regions/labels with bbox and
    /// confidence, plus semantic part description from the vision model when
    /// it is genuinely available (otherwise an honest status line).
    Drawing {
        /// Drawing image (png/jpeg/tiff).
        path: PathBuf,
        /// Vision model to use (default: moondream).
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum MultimodalCmd {
    /// Honest multimodal capability matrix: per-modality intake statuses
    /// (which file kinds this build can actually take in, and with which
    /// backends) plus the Phase 9 doctor backend rows — the same probes,
    /// never a second capability claim.
    Capabilities,
}

#[derive(Subcommand)]
enum VerifyCmd {
    /// Hallucination firewall over one claim (M9.8): deterministic evidence
    /// gate runs first (never spends a model call on an ungrounded claim),
    /// model verdicts may only demote, cross-model independence is reported
    /// only for genuinely distinct models. Evidence comes from `--cite
    /// document:chunk` (persisted chunk text only) or — without `--cite` —
    /// from retrieval over the claim. Injection flags on the evidence are
    /// surfaced before the verdict. Exit: 0 pass / 2 flag / 3 block.
    Claim {
        /// Which persisted corpus the claim is checked against.
        kb: String,
        /// The claim to verify.
        claim: String,
        /// Explicit citation `document:chunk` (repeatable). Without any,
        /// evidence is retrieved from the corpus for this claim.
        #[arg(long = "cite")]
        cites: Vec<String>,
        /// Evidence top_k when retrieving (ignored with --cite).
        #[arg(long, default_value_t = 3)]
        top_k: usize,
        /// Real model for the model verdict (repeatable: two distinct
        /// models make the verdict INDEPENDENT). Absent = deterministic
        /// verification only (honestly labeled).
        #[arg(long)]
        model: Vec<String>,
        /// Ollama endpoint for `--model` backends (default 127.0.0.1:11434).
        #[arg(long)]
        endpoint: Option<String>,
        /// Emit the full firewall report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Full hardened answer path (M9.9 + M9.8) over a proposed answer:
    /// retrieve -> scan evidence (flags persist, UNTRUSTED labels) ->
    /// trust-labeled context builder (injection categories audited through
    /// the chained audit log) -> hallucination firewall over the answer
    /// itself. Use `openrt knowledge answer` to GET an evidence-grounded
    /// answer first; use this to check one before presenting it.
    /// Exit: 0 pass / 2 flag / 3 block.
    Answer {
        /// Which persisted corpus the answer must be grounded in.
        kb: String,
        /// The proposed answer text to verify.
        answer: String,
        #[arg(long, default_value_t = 8)]
        top_k: usize,
        #[arg(long, default_value_t = 0.0)]
        threshold: f32,
        /// Print the hardened (framed + labeled + marked) evidence context.
        #[arg(long)]
        show_context: bool,
        /// Emit report JSON (answer verdict + injection summary).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ModeCmd {
    /// Current operating mode + audited transition history.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Audited transition to `provisioning` or `sovereign`.
    Set {
        mode: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
enum AuditCmd {
    /// Recompute every chain hash and confirm links (fail-closed).
    Verify,
    /// Search entries (aggregation/search; ADR-002 additive).
    Query {
        /// Action prefix (e.g. `permission.`).
        #[arg(long)]
        action_prefix: Option<String>,
        /// Exact action match.
        #[arg(long)]
        action: Option<String>,
        /// Subject substring.
        #[arg(long)]
        subject: Option<String>,
        /// Result exact match (`allowed`, `denied`, …).
        #[arg(long)]
        result: Option<String>,
        /// Actor exact match.
        #[arg(long)]
        actor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u64,
        #[arg(long)]
        json: bool,
    },
    /// Retention / head-tail status of the verifiable chain.
    Retention {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PolicyCmd {
    /// Show the active versioned policy document.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Ensure a deny-by-default document exists (idempotent).
    Init,
    /// Append a rule (operator-only; creates a new immutable revision).
    Append {
        /// Capability or tool pattern (`echo`, `filesystem.read`, `*.ok`).
        pattern: String,
        /// allow | deny | ask
        #[arg(long)]
        action: String,
        /// Optional scope glob.
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// List all revisions.
    Versions,
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Store a secret value (encrypted at rest; never printed).
    Set {
        name: String,
        /// Plaintext value (prefer env/stdin for real secrets).
        value: String,
    },
    /// Fetch a secret value as raw bytes to stdout (operator-only).
    Get { name: String },
    /// Delete a secret.
    Delete { name: String },
    /// List secret metadata (names only; no ciphertext).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Rotate the master key (re-seals every secret under a new key).
    Rotate,
}

#[derive(Subcommand)]
enum NetpolCmd {
    /// Grant default-deny egress for a scope (repeatable allow pattern).
    Grant {
        scope: String,
        #[arg(long = "allow")]
        allow: Vec<String>,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Revoke a scope's grant.
    Revoke { scope: String },
    /// List grants.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Check whether a destination is allowed under a scope (mode + grant).
    Check { scope: String, destination: String },
}

#[derive(Subcommand)]
enum SandboxCmd {
    /// What this host can actually enforce (honest backend matrix).
    Capabilities {
        #[arg(long)]
        json: bool,
    },
    /// Admission policy per sandbox profile (digest, network, isolation).
    Policy {
        #[arg(long)]
        json: bool,
    },
    /// List sandbox instances from the persistent registry.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Inspect one sandbox instance by id.
    Inspect {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Admit a profile WITHOUT executing (name, policy, backend check).
    Check { profile: String },
    /// Execute a python script inside the sandbox (operator debug path;
    /// goes through the same admission + limits + verified cleanup flow).
    Run {
        /// Sandbox profile (read_only / development / networked / strict /
        /// untrusted).
        #[arg(long, default_value = "strict")]
        profile: String,
        /// Inline script code.
        #[arg(long, conflicts_with = "file")]
        code: Option<String>,
        /// Read the script from a file.
        #[arg(long)]
        file: Option<String>,
        /// Wall-clock timeout in seconds.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
    },
    /// Destroy a sandbox instance (verified cleanup; refuses unknown ids).
    Destroy { id: String },
}

#[derive(Subcommand)]
enum EnclaveCmd {
    /// Enroll a node (operator-generated identity fingerprint).
    Enroll {
        node_id: String,
        #[arg(long)]
        fingerprint: String,
        #[arg(long = "cap")]
        capabilities: Vec<String>,
    },
    /// List enrolled nodes.
    Nodes {
        #[arg(long)]
        json: bool,
    },
    /// Quarantine a node (rogue/anomaly response).
    Quarantine {
        node_id: String,
        #[arg(long)]
        reason: String,
    },
    /// Record a verified model migration between nodes.
    Migrate {
        artifact_id: String,
        #[arg(long)]
        sha256: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        provenance: Option<String>,
    },
}

#[derive(Subcommand)]
enum ObsCmd {
    /// Compute health from the same telemetry event pipeline (ADR-005).
    Health {
        #[arg(long)]
        json: bool,
    },
    /// Show/update retention policy.
    Retention {
        #[arg(long)]
        max_age_secs: Option<u64>,
        #[arg(long)]
        max_events: Option<u64>,
    },
    /// Queue an export batch (blocked in Sovereign).
    Export {
        destination: String,
        #[arg(long)]
        json_payload: Option<String>,
    },
}

#[derive(Subcommand)]
enum AutonomousCmd {
    /// Create an unattended run envelope (budgets/stop conditions/mat).
    Create {
        workflow_id: String,
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 0)]
        max_wall_secs: u64,
        #[arg(long, default_value_t = 0)]
        max_budget: u64,
        #[arg(long, default_value_t = 0)]
        max_errors: u32,
        /// Pre-approved capability patterns (repeatable).
        #[arg(long = "approve")]
        approve: Vec<String>,
        /// Always-denied capability patterns (repeatable).
        #[arg(long = "deny")]
        deny: Vec<String>,
    },
    /// List autonomous runs.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Operator stop.
    Stop {
        run_id: String,
        #[arg(long, default_value = "operator")]
        reason: String,
    },
    /// Take over a run (crash recovery / supervisor handoff).
    Takeover { run_id: String },
}

/// Phase 12 industrial intelligence commands (advisory only).
#[derive(Subcommand)]
enum IndustrialCmd {
    /// Upsert an industrial asset (extension attrs repeatable `key=value`).
    AssetPut {
        id: String,
        /// plant|unit|equipment|sensor|alarm|process|incident|work_order|inspection|procedure.
        kind: String,
        label: String,
        #[arg(long = "attr")]
        attrs: Vec<String>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Add one canonical graph relation (predicate-restricted set).
    Relate {
        subject: String,
        predicate: String,
        object: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// List assets (optional kind/plant/unit/text filters).
    Assets {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        plant: Option<String>,
        #[arg(long)]
        unit: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read one asset by id.
    Asset {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Append telemetry points (`--point t_ms,value` repeatable).
    SeriesAppend {
        series_id: String,
        asset_id: String,
        #[arg(long, default_value = "unit")]
        unit: String,
        #[arg(long = "point")]
        points: Vec<String>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Aggregate a series over a window (missing buckets stay missing).
    SeriesQuery {
        id: String,
        #[arg(long, default_value_t = 0)]
        from_ms: i64,
        #[arg(long)]
        to_ms: Option<i64>,
        #[arg(long, default_value_t = 60_000)]
        bucket_ms: i64,
        #[arg(long)]
        json: bool,
    },
    /// Least-squares trend over a stored series.
    Trend {
        asset_id: String,
        series_id: String,
        #[arg(long)]
        window: Option<usize>,
        #[arg(long)]
        min_points: Option<usize>,
        #[arg(long)]
        low: Option<f64>,
        #[arg(long)]
        high: Option<f64>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Run one detector: statistical|rule|time_series|model_assisted
    /// (model_assisted reports UNAVAILABLE without a model backend).
    Anomaly {
        asset_id: String,
        series_id: String,
        #[arg(long, default_value = "statistical")]
        method: String,
        #[arg(long)]
        z: Option<f64>,
        #[arg(long)]
        low: Option<f64>,
        #[arg(long)]
        high: Option<f64>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Bounded, evidence-labelled root-cause report.
    Rca {
        subject: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// FMEA table for the asset's declared equipment type.
    Fmea {
        asset_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Deterministic heuristic risk band (labelled `heuristic`).
    Risk {
        asset_id: String,
        #[arg(long)]
        series: Option<String>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// What-if scenario over observed graph state (`--assume` repeatable).
    Scenario {
        asset_id: String,
        #[arg(long, default_value = "failure")]
        event: String,
        #[arg(long = "assume")]
        assumptions: Vec<String>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Evidence-linked investigation answer for one asset.
    Investigate {
        asset_id: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
}

/// Phase 12 research-only trading commands (no broker; execution refuses).
#[derive(Subcommand)]
enum TradingCmd {
    /// Register a research instrument (metadata only — no market feed).
    InstrumentPut {
        id: String,
        symbol: String,
        #[arg(long, default_value = "")]
        venue: String,
        /// equity|index|future|fx|crypto|other.
        #[arg(long, default_value = "equity")]
        kind: String,
        #[arg(long, default_value = "USD")]
        currency: String,
        #[arg(long, default_value_t = 0.01)]
        tick: f64,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// List registered instruments.
    Instruments {
        #[arg(long)]
        json: bool,
    },
    /// Build a dataset from `--bar t_ms,o,h,l,c,v` rows (research input).
    DatasetBuild {
        id: String,
        #[arg(long, default_value = "v1")]
        version: String,
        #[arg(long)]
        instrument: String,
        #[arg(long = "bar")]
        bars: Vec<String>,
        /// Declare look-ahead contamination deliberately (for leakage demos).
        #[arg(long)]
        declared_lookahead: bool,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Import a CSV file (`t_ms,open,high,low,close,volume`) as a dataset.
    DatasetImport {
        id: String,
        #[arg(long, default_value = "v1")]
        version: String,
        #[arg(long)]
        instrument: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        declared_lookahead: bool,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// List research datasets (content-addressed digests).
    Datasets {
        #[arg(long)]
        json: bool,
    },
    /// Register a declarative strategy from a JSON file (causal DSL).
    StrategyPut {
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// List registered strategies.
    Strategies {
        #[arg(long)]
        json: bool,
    },
    /// Run a deterministic simulated backtest (persisted + audited).
    Backtest {
        strategy_id: String,
        dataset_id: String,
        #[arg(long)]
        cash: Option<f64>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Walk-forward folds over one dataset (in-sample-only flagged honestly).
    WalkForward {
        strategy_id: String,
        dataset_id: String,
        #[arg(long, default_value_t = 0.6)]
        train: f64,
        #[arg(long, default_value_t = 0.2)]
        validation: f64,
        #[arg(long)]
        cash: Option<f64>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Record a research experiment from a JSON file.
    ExperimentPut {
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// List research experiments.
    Experiments {
        #[arg(long)]
        json: bool,
    },
    /// Attempt broker execution — ALWAYS denied (research only, audited).
    /// Exit code 3 marks the refusal.
    Execute {
        /// Order JSON (recorded in the denial audit entry).
        #[arg(long)]
        order: String,
    },
}

/// Phase 12 self-improvement commands (operator-gated; never self-activating).
#[derive(Subcommand)]
enum LearnCmd {
    /// Capture an experience episode from a JSON file.
    EpisodeCapture {
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// List sealed experience episodes.
    Episodes {
        #[arg(long)]
        json: bool,
    },
    /// Evaluate an episode: MEASURED vs INFERRED metrics (labelled).
    EpisodeEvaluate {
        id: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Run + record the measured bench probe for one artifact.
    Benchmark {
        artifact_id: String,
        #[arg(long = "prompt")]
        prompts: Vec<String>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Benchmark-gated skill promotion (baseline pairs like
    /// `error_rate=1.0,ok_count=0`; without a baseline the verdict is hold).
    PromoteSkill {
        id: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        baseline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Benchmark-gated instinct promotion.
    PromoteInstinct {
        id: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        baseline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Benchmark-gated genome promotion (apply only on measured gain).
    PromoteGenome {
        genome_id: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        baseline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Roll a skill back to inactive (audited + recorded).
    RollbackSkill {
        id: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Roll a genome back to a previous version.
    RollbackGenome {
        current_id: String,
        previous_id: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        json: bool,
    },
    /// Derive a candidate skill from execution history (candidate only —
    /// activation stays a separate operator action).
    CandidateSkill {
        request_id: String,
        description: String,
        #[arg(long = "permission")]
        permissions: Vec<String>,
        #[arg(long)]
        session_id: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Derive a candidate instinct from execution history.
    CandidateInstinct {
        #[arg(long)]
        session_id: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Propose a genome evolution (proposal only — applying stays gated).
    CandidateGenome {
        id: String,
        genome_id: String,
        summary: String,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// Create a training experiment from a JSON `ExperimentSpec` file
    /// (planned — Phase 12 has no training backend).
    ModelCreate {
        #[arg(long)]
        path: String,
        #[arg(long)]
        json: bool,
    },
    /// Start a training experiment — fails honestly with the probe's
    /// reason when no backend is configured (emits TrainJobFailed).
    ModelStart {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Get one training experiment run.
    ModelGet {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// List training experiment runs.
    ModelList {
        #[arg(long)]
        json: bool,
    },
    /// Record one routing observation (subordinate signal, never authority).
    RoutingRecord {
        task_type: String,
        artifact_id: String,
        #[arg(long)]
        success: bool,
        #[arg(long)]
        latency_ms: Option<u64>,
        #[arg(long, default_value = "operator")]
        actor: String,
        #[arg(long)]
        json: bool,
    },
    /// List recorded routing signals.
    RoutingSignals {
        #[arg(long)]
        json: bool,
    },
    /// List persisted promotion/rollback decisions (measured evidence).
    Promotions {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SihCmd {
    /// Run the SIH hero demo end-to-end (hermetic: its own store +
    /// workspace under --dir, mock provider, embedded synthetic data).
    /// Writes five deterministic artifacts and exits non-zero if any
    /// stage fails — never a fake success.
    Demo {
        /// Output directory for the store, workspace, and artifacts.
        #[arg(long, default_value = "sih-demo")]
        dir: PathBuf,
        /// Emit the machine-readable report on stdout.
        #[arg(long)]
        json: bool,
        /// Investigator query for the retrieval stage.
        #[arg(long)]
        query: Option<String>,
    },
}

fn run_cli(cli: &Cli) -> Result<i32, String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let result = match &cli.cmd {
        None => rt.block_on(cmd_agent(cli)),
        Some(Cmd::Doctor) => rt.block_on(cmd_doctor(cli)),
        Some(Cmd::Version) => {
            println!("openrt {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some(Cmd::Model { cmd }) => rt.block_on(cmd_model(cli, cmd)),
        Some(Cmd::Tool { cmd }) => rt.block_on(cmd_tool(cli, cmd)),
        Some(Cmd::Session { cmd }) => rt.block_on(cmd_session(cli, cmd)),
        Some(Cmd::Workspace { cmd }) => rt.block_on(cmd_workspace(cli, cmd)),
        Some(Cmd::Workbench { cmd }) => rt.block_on(cmd_workbench(cli, cmd)),
        Some(Cmd::Room { cmd }) => rt.block_on(cmd_room_top(cli, cmd)),
        Some(Cmd::Material { cmd }) => rt.block_on(cmd_material_top(cli, cmd)),
        Some(Cmd::Computer { cmd }) => rt.block_on(cmd_computer(cli, cmd)),
        Some(Cmd::Tui) => rt.block_on(cmd_tui(cli)),
        Some(Cmd::Agent) => rt.block_on(cmd_agent(cli)),
        Some(Cmd::Task { cmd }) => rt.block_on(cmd_task(cli, cmd)),
        Some(Cmd::Config { cmd }) => cmd_config(cmd),
        Some(Cmd::Checkpoint { cmd }) => rt.block_on(cmd_checkpoint(cli, cmd)),
        Some(Cmd::Mode { cmd }) => rt.block_on(cmd_mode(cli, cmd)),
        Some(Cmd::Knowledge { cmd }) => rt.block_on(cmd_knowledge(cli, cmd)),
        Some(Cmd::Ingest { cmd }) => rt.block_on(cmd_ingest(cli, cmd)),
        Some(Cmd::Vision { cmd }) => rt.block_on(cmd_vision(cli, cmd)),
        Some(Cmd::Multimodal { cmd }) => rt.block_on(cmd_multimodal(cli, cmd)),
        Some(Cmd::Verify { cmd }) => rt.block_on(cmd_verify(cli, cmd)),
        Some(Cmd::History { cmd }) => rt.block_on(cmd_history(cli, cmd)),
        Some(Cmd::Recovery { cmd }) => rt.block_on(cmd_recovery(cli, cmd)),
        Some(Cmd::Shield { cmd }) => rt.block_on(cmd_shield(cli, cmd)),
        Some(Cmd::Skill { cmd }) => rt.block_on(cmd_skill(cli, cmd)),
        Some(Cmd::Extension { cmd }) => rt.block_on(cmd_extension(cli, cmd)),
        Some(Cmd::Mcp { cmd }) => rt.block_on(cmd_mcp(cli, cmd)),
        Some(Cmd::Instinct { cmd }) => rt.block_on(cmd_instinct(cli, cmd)),
        Some(Cmd::Genome { cmd }) => rt.block_on(cmd_genome(cli, cmd)),
        Some(Cmd::Memory { cmd }) => rt.block_on(cmd_memory(cli, cmd)),
        Some(Cmd::Assets { cmd }) => rt.block_on(cmd_assets(cli, cmd)),
        Some(Cmd::Improve { cmd }) => rt.block_on(cmd_improve(cli, cmd)),
        Some(Cmd::Webui { cmd }) => rt.block_on(cmd_webui(cli, cmd)),
        Some(Cmd::Workflow { cmd }) => rt.block_on(cmd_workflow(cli, cmd)),
        Some(Cmd::Audit { cmd }) => rt.block_on(cmd_audit(cli, cmd)),
        Some(Cmd::Policy { cmd }) => rt.block_on(cmd_policy(cli, cmd)),
        Some(Cmd::Secret { cmd }) => rt.block_on(cmd_secret(cli, cmd)),
        Some(Cmd::Netpol { cmd }) => rt.block_on(cmd_netpol(cli, cmd)),
        Some(Cmd::Sandbox { cmd }) => rt.block_on(cmd_sandbox(cli, cmd)),
        Some(Cmd::Enclave { cmd }) => rt.block_on(cmd_enclave(cli, cmd)),
        Some(Cmd::Obs { cmd }) => rt.block_on(cmd_obs(cli, cmd)),
        Some(Cmd::Autonomous { cmd }) => rt.block_on(cmd_autonomous(cli, cmd)),
        Some(Cmd::Industrial { cmd }) => rt.block_on(cmd_industrial(cli, cmd)),
        Some(Cmd::Trading { cmd }) => rt.block_on(cmd_trading(cli, cmd)),
        Some(Cmd::Learn { cmd }) => rt.block_on(cmd_learn(cli, cmd)),
        Some(Cmd::Sih { cmd }) => rt.block_on(cmd_sih(cli, cmd)),
    };
    // Runtime::drop already flushed the bus on multi-thread; this is a final
    // no-op safety net for current-thread edge cases before process exit.
    result
}

/// Operator identity used for audit action attribution.
fn actor_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "openrt-cli".to_string())
}

/// Build the registry over the wired runtime's store. A fresh `ModeManager`
/// over the same store is coherent with `rt.modes`: both read the persisted
/// mode on every call, so CLI transitions are visible immediately.
fn registry_for(rt: &Runtime) -> openrt_registry::ModelRegistry {
    openrt_registry::ModelRegistry::new(
        rt.store.clone(),
        openrt_modes::ModeManager::new(rt.store.clone()),
    )
}

fn effective_config() -> Result<Config, String> {
    // Fail-closed resolution: an explicitly set OPENRT_CONFIG must exist
    // (see Config::load_effective); only an implicit default path may fall
    // back to built-in defaults for zero-config first runs.
    Config::load_effective().map_err(|e| e.to_string())
}

fn builder_for(cli: &Cli) -> Result<RuntimeBuilder, String> {
    let config = effective_config()?;
    let mut b = RuntimeBuilder::new().config(config);
    if cli.memory {
        b = b.store(Arc::new(openrt_persistence::MemoryStore::new()));
    }
    let forced = cli.mock
        || std::env::var("OPENRT_MOCK")
            .map(|v| v == "1")
            .unwrap_or(false);
    if forced {
        b = b.with_provider(Arc::new(openrt_models::mock::MockProvider::tool_sequence(
            "mock",
        )));
    }
    Ok(b)
}

async fn build_runtime(cli: &Cli) -> Result<Arc<Runtime>, String> {
    builder_for(cli)?.build().await.map_err(|e| e.to_string())
}

async fn cmd_doctor(cli: &Cli) -> Result<i32, String> {
    // M9: the doctor carries a live VRAM observer so `openrt doctor` reports
    // real device usage when a trusted vendor probe is on-path. No probe
    // (no nvidia-smi) degrades to an honest UNAVAILABLE row — never a guess.
    let builder = builder_for(cli)?;
    let rt = builder
        .gpu_observer(Arc::new(openrt_resources::NvidiaSmiObserver::default()))
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let report = rt.doctor().await;
    for check in &report.checks {
        println!(
            "{}  {:<12}  {}",
            check.status.as_str(),
            check.name,
            check.detail
        );
    }
    let fails = report
        .checks
        .iter()
        .filter(|c| c.status == openrt_runtime::DoctorStatus::Fail)
        .count();
    let warns = report
        .checks
        .iter()
        .filter(|c| c.status == openrt_runtime::DoctorStatus::Warn)
        .count();
    if report.is_healthy() {
        println!("\nenvironment healthy");
        Ok(0)
    } else {
        println!("\n{fails} fail / {warns} warn — see rows above");
        if fails == 0 {
            Ok(0)
        } else {
            Ok(1)
        }
    }
}

async fn cmd_sih(_cli: &Cli, cmd: &SihCmd) -> Result<i32, String> {
    let SihCmd::Demo { dir, json, query } = cmd;
    let out_dir = dir.clone();
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;

    // Hermetic: the demo owns its store + workspace under --dir so a clean
    // checkout reproduces the same run without touching ~/.openrt.
    let config = Config {
        runtime: openrt_config::RuntimeConfig {
            workspace_dir: Some(out_dir.join("workspace")),
            ..openrt_config::RuntimeConfig::default()
        },
        persistence: openrt_config::PersistenceConfig {
            kind: openrt_config::StoreKind::File,
            dir: Some(out_dir.join("store")),
        },
        ..Config::default()
    };
    let rt = RuntimeBuilder::new()
        .config(config)
        .with_provider(Arc::new(openrt_models::mock::MockProvider::echo("mock")))
        .build()
        .await
        .map_err(|e| format!("runtime build failed: {e}"))?;

    let report = rt.doctor().await;
    let fails: Vec<&openrt_runtime::DoctorCheck> = report
        .checks
        .iter()
        .filter(|c| matches!(c.status, openrt_runtime::DoctorStatus::Fail))
        .collect();
    if !fails.is_empty() {
        for c in &fails {
            eprintln!("{}  {:<12}  {}", c.status.as_str(), c.name, c.detail);
        }
        eprintln!("sih demo prerequisites unmet: doctor reports FAIL rows");
        return Ok(1);
    }

    let opts = openrt_runtime::sih::SihOptions {
        out_dir: out_dir.clone(),
        kb_id: openrt_runtime::sih::SIH_KB.to_string(),
        query: query
            .clone()
            .unwrap_or_else(|| openrt_runtime::sih::DEMO_CLAIM.to_string()),
    };
    let demo = match openrt_runtime::sih::run_demo(&rt, &opts).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("sih demo failed: {e}");
            return Ok(1);
        }
    };

    if *json {
        let v = serde_json::to_value(&demo).map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
        );
        return Ok(0);
    }

    println!("CyberSaarthi + OpenRT SIH hero demo — SYNTHETIC DATA ONLY");
    println!(
        "case:         {} (workspace {})",
        demo.case_id, demo.workspace
    );
    println!("room:         {}", demo.room);
    println!(
        "evidence:     {} documents, {} chunks, {} retrieval hits",
        demo.documents, demo.chunks, demo.hits
    );
    println!(
        "graph:        {} entities, {} relationships",
        demo.entities, demo.relations
    );
    println!(
        "workflow:     {} steps — {}",
        demo.workflow_steps, demo.workflow_status
    );
    println!(
        "verification: decision {}, verdict {} (lexical coverage {:.3})",
        demo.decision, demo.verdict, demo.grounding_coverage
    );
    println!(
        "injection:    {} flag(s) on the hostile document: {}",
        demo.injection_flags,
        if demo.injection_categories.is_empty() {
            "none".to_string()
        } else {
            demo.injection_categories.join(", ")
        }
    );
    println!("artifacts:");
    for a in &demo.artifacts {
        let sha = &a.sha256;
        let short = &sha[..12.min(sha.len())];
        println!("  - {:<12} {short}…  {}", a.kind, a.path);
    }

    println!("security/audit events (sih.*):");
    let rows = rt
        .audit_query(
            &openrt_audit::AuditQuery::new()
                .with_action_prefix("sih.")
                .with_limit(100),
        )
        .await
        .map_err(|e| format!("audit query failed: {e}"))?;
    for r in &rows {
        println!(
            "  seq {:>4}  {:<26}  {:<8}  {}",
            r.seq, r.entry.action, r.entry.result, r.entry.subject
        );
    }
    let findings = openrt_shield::store::list_findings(&*rt.store, None)
        .await
        .map(|f| f.len())
        .unwrap_or(0);
    println!("shield findings on record: {findings}");
    println!(
        "audit chain:  {} sih.* entries, {} total entries verified",
        demo.audit_entries, demo.audit_verified
    );
    for (label, ms) in &demo.timings_ms {
        println!("  {label:<14} {ms:>6} ms");
    }
    println!("done in {} ms", demo.total_ms);
    Ok(0)
}

async fn cmd_model(cli: &Cli, cmd: &ModelCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        ModelCmd::Info => {
            let info = rt.provider.info();
            let caps = rt.provider.capabilities();
            println!("provider:  {}", info.id);
            println!("kind:      {:?}", info.kind);
            println!("display:   {}", info.display_name);
            println!(
                "model:     {}",
                info.default_model
                    .clone()
                    .unwrap_or_else(|| "<default>".into())
            );
            println!(
                "tools:     {}",
                if caps.supports_tools { "yes" } else { "no" }
            );
            println!(
                "streaming: {}",
                if caps.supports_streaming { "yes" } else { "no" }
            );
            if let Some(w) = caps.context_window {
                println!("context:   {w} tokens");
            }
            Ok(0)
        }
        ModelCmd::List => {
            let info = rt.provider.info();
            println!(
                "{:<12} {:<10} {}",
                info.id,
                format!("{:?}", info.kind).to_lowercase(),
                info.default_model.clone().unwrap_or_default()
            );
            Ok(0)
        }
        ModelCmd::Show { id } => {
            let registry = registry_for(&rt);
            let card = registry
                .status(id)
                .await
                .map_err(|e| format!("`{id}`: {e}"))?;
            println!("artifact:      {}", card.artifact_id);
            println!("display:       {}", card.display_name);
            println!("family:        {}", card.family);
            println!("tags:          {}", card.tags.join(", "));
            println!("source:        {}", card.source.as_str());
            println!("state:         {}", card.state.as_str());
            println!("installed:     {} bytes", card.installed_bytes);
            println!(
                "digest:        {}",
                card.digest_prefix.as_deref().unwrap_or("<none>")
            );
            if let Some(manifest) = registry.manifest(id).await.map_err(|e| e.to_string())? {
                println!(
                    "manifest:      v{} kind={:?}",
                    manifest.schema_version, manifest.kind
                );
                for blob in &manifest.blobs {
                    println!(
                        "  file   {:<24} {} bytes  sha256={}",
                        blob.file_key,
                        blob.size_bytes,
                        &blob.digest[..blob.digest.len().min(16)]
                    );
                }
            } else {
                println!("manifest:      (not installed)");
            }
            if let Some(lc) = registry.lifecycle(id).await.map_err(|e| e.to_string())? {
                println!("lifecycle:     seq={} updated={}", lc.seq, lc.updated_at);
                for t in lc.history.iter().rev().take(3) {
                    println!(
                        "  {} -> {}  by {} ({})",
                        t.from.as_str(),
                        t.to.as_str(),
                        t.actor,
                        t.reason
                    );
                }
            }
            let binds = registry.bound_tags(id).await.map_err(|e| e.to_string())?;
            if binds.is_empty() {
                println!("bound tags:    (none — provider tags are never guessed)");
            } else {
                for b in &binds {
                    println!(
                        "bound tag:     {provider}:{tag}  by {actor} at {at}",
                        provider = b.provider,
                        tag = b.tag,
                        actor = b.actor,
                        at = b.bound_at
                    );
                }
            }
            Ok(0)
        }
        ModelCmd::Status { id, json } => {
            let registry = registry_for(&rt);
            let provider = rt.provider.info().id.to_string();
            let cards = match id {
                Some(id) => vec![registry
                    .status(id)
                    .await
                    .map_err(|e| format!("{id}: {e}"))?],
                None => registry.list().await.map_err(|e| e.to_string())?,
            };
            if *json {
                let mut v: Vec<_> = Vec::new();
                for c in &cards {
                    let bindings = registry
                        .bound_tags(&c.artifact_id)
                        .await
                        .map(|bs| {
                            bs.iter()
                                .map(|b| (b.provider.clone(), b.tag.clone()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    v.push(serde_json::json!({
                        "artifact_id": c.artifact_id,
                        "display_name": c.display_name,
                        "family": c.family,
                        "tags": c.tags,
                        "state": c.state.as_str(),
                        "source": c.source.as_str(),
                        "installed_bytes": c.installed_bytes,
                        "digest_prefix": c.digest_prefix,
                        "bindings": bindings,
                    }));
                }
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!(
                    "{:<24} {:<28} {:<6} {:<10} {:<9} BOUND",
                    "ARTIFACT", "DISPLAY NAME", "STATE", "SOURCE", "BYTES"
                );
                for c in &cards {
                    let bound = registry
                        .bound_tag(&c.artifact_id, &provider)
                        .await
                        .ok()
                        .flatten()
                        .map(|b| b.tag)
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "{:<24} {:<28} {:<6} {:<10} {:<9} {}",
                        c.artifact_id,
                        c.display_name,
                        c.state.as_str(),
                        c.source.as_str(),
                        c.installed_bytes,
                        bound
                    );
                }
            }
            Ok(0)
        }
        ModelCmd::Search { query, hf } => {
            let registry = registry_for(&rt);
            let client = hf_client();
            let results = registry.search(query, hf.then_some(&client)).await;
            println!(
                "{:<40} {:<32} {:<10} INSTALLED",
                "ARTIFACT", "DISPLAY NAME", "SOURCE"
            );
            for r in results {
                println!(
                    "{:<40} {:<32} {:<10} {}",
                    r.artifact_id,
                    r.display_name,
                    r.source.as_str(),
                    if r.installed { "yes" } else { "no" }
                );
            }
            Ok(0)
        }
        ModelCmd::Register { id, path } => {
            let registry = registry_for(&rt);
            let meta = std::fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path))?;
            if !meta.is_file() {
                return Err(format!("{path} is not a file"));
            }
            let size = meta.len();
            if size == 0 {
                return Err("refusing to register an empty file".to_string());
            }

            // Bounded inspection: a few hundred KiB of header, whatever the
            // artifact's size.
            let ip = path.clone();
            let artifact_md = tokio::task::spawn_blocking(move || {
                openrt_models::artifact::read_metadata(std::path::Path::new(&ip))
            })
            .await
            .map_err(|e| format!("inspect task crashed: {e}"))?
            .map_err(|e| format!("inspect {path}: {e}"))?;
            let profile =
                openrt_models::artifact::ModelResourceProfile::from_metadata(&artifact_md, size);

            // Streamed digest, same function the materializing path uses.
            let dp = path.clone();
            let (digest, hashed) = tokio::task::spawn_blocking(move || {
                openrt_verify::sha256_file(std::path::Path::new(&dp))
            })
            .await
            .map_err(|e| format!("digest task crashed: {e}"))?
            .map_err(|e| format!("digest {path}: {e}"))?;
            if hashed != size {
                return Err(format!(
                    "{path} changed while being read ({size} bytes stat'd, {hashed} hashed) — retry"
                ));
            }

            let sys = openrt_resources::SystemInfo::collect();
            let fit = openrt_models::artifact::assess_execution_fit(
                &profile,
                Some(sys.mem.available_kb * 1024),
            );

            let manifest = openrt_registry::ManifestV1 {
                schema_version: openrt_registry::MANIFEST_SCHEMA_VERSION,
                artifact_id: id.clone(),
                kind: openrt_scan::ArtifactKind::Binary,
                blobs: vec![openrt_registry::BlobRef {
                    file_key: std::path::Path::new(&path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .filter(|n| !n.is_empty())
                        .unwrap_or("artifact.bin")
                        .to_string(),
                    digest: digest.clone(),
                    size_bytes: size,
                    location: openrt_registry::BlobLocation::External { path: path.clone() },
                }],
                base_digest: None,
                created_at: openrt_core::now().to_rfc3339(),
            };
            let lr = registry
                .register_local(manifest, &path, &actor_user())
                .await
                .map_err(|e| format!("register `{id}` failed: {e}"))?;

            println!(
                "registered `{id}` by reference -> state {}",
                lr.state.as_str()
            );
            println!("  path      {path}");
            println!(
                "  size      {} (0 bytes copied into the content store)",
                openrt_models::artifact::human_bytes(size)
            );
            println!("  sha256    {}", &digest[..digest.len().min(16)]);
            match artifact_md.architecture.as_deref() {
                Some(a) => println!(
                    "  metadata  arch {a}, {}",
                    artifact_md
                        .quantization
                        .as_deref()
                        .map(|q| format!("quantization {q}"))
                        .unwrap_or_else(|| "quantization NOT DECLARED".into())
                ),
                None => println!("  metadata  no readable header (nothing probed)"),
            }
            println!("  inspected {} bytes", artifact_md.bytes_read);
            println!("  needs     {}", profile.requirement_basis());
            match &fit {
                openrt_models::artifact::ExecutionFit::Ready {
                    required,
                    available,
                } => println!(
                    "  execution WOULD FIT (advisory: {} needed, {} available)",
                    openrt_models::artifact::human_bytes(required.bytes),
                    openrt_models::artifact::human_bytes(*available)
                ),
                openrt_models::artifact::ExecutionFit::ResourceLimited {
                    required,
                    available,
                    ..
                } => println!(
                    "  execution RESOURCE-LIMITED (advisory: {} needed, {} available)",
                    openrt_models::artifact::human_bytes(required.bytes),
                    openrt_models::artifact::human_bytes(*available)
                ),
                openrt_models::artifact::ExecutionFit::Unknown { reason } => {
                    println!("  execution UNKNOWN ({reason})")
                }
            }
            println!("  not loaded — registration is not execution");
            for n in &profile.notes {
                println!("  note      {n}");
            }
            Ok(0)
        }
        ModelCmd::Install {
            id,
            path,
            scan_bundle,
            trust_key,
        } => {
            let registry = registry_for(&rt);
            let meta = std::fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path))?;
            // Materializing means holding the artifact in memory: the content
            // store takes a byte slice. Say so before a multi-gibabyte read,
            // and point at the route that does not copy.
            if meta.len() > 2 * 1024 * 1024 * 1024 {
                return Err(format!(
                    "{} is {}; `model install` copies the artifact into the content \
                     store and needs it in memory. Use `openrt model register {} {}` \
                     to record it by reference instead.",
                    path,
                    openrt_models::artifact::human_bytes(meta.len()),
                    id,
                    path
                ));
            }
            let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path))?;
            if bytes.is_empty() {
                return Err("refusing to install an empty file".to_string());
            }
            let act = actor_user();
            let manifest = single_file_manifest(id, &bytes)?;

            let scanner = build_scanner(scan_bundle.as_deref(), trust_key)?;
            let scan = scanner.as_ref().map(|s| (s, 1u32));
            let lr = registry
                .import_local(manifest, &single_file_map(&bytes), &act, scan)
                .await
                .map_err(|e| format!("install failed: {e}"))?;
            println!("installed `{id}` -> state {}", lr.state.as_str());
            if lr.state == openrt_registry::LifecycleState::Scanned {
                println!("run `openrt model activate {id}` to make it routable");
            } else if lr.state == openrt_registry::LifecycleState::Verified {
                println!(
                    "verified but not scanned; supply a `--scan-bundle` to pass the scan gate"
                );
            }
            Ok(0)
        }
        ModelCmd::Activate { id } => {
            let registry = registry_for(&rt);
            let lr = registry
                .activate(id, &actor_user())
                .await
                .map_err(|e| format!("activate `{id}` failed: {e}"))?;
            println!("`{id}` -> {}", lr.state.as_str());
            Ok(0)
        }
        ModelCmd::Remove { id, reason } => {
            let registry = registry_for(&rt);
            let reason = reason.clone().unwrap_or_else(|| "operator removal".into());
            let lr = registry
                .retire(id, &actor_user(), &reason)
                .await
                .map_err(|e| format!("remove `{id}` failed: {e}"))?;
            println!(
                "`{id}` retired -> {} (soft-remove; bytes remain stored)",
                lr.state.as_str()
            );
            Ok(0)
        }
        ModelCmd::Route { id } => {
            let registry = registry_for(&rt);
            let router = openrt_router::Router::new();
            match router.is_eligible(&registry, id).await {
                Ok(d) => {
                    println!(
                        "`{}` is routable (rank {}; {} bytes)",
                        d.artifact_id, d.rank, d.installed_bytes
                    );
                    Ok(0)
                }
                Err(reasons) => {
                    for r in &reasons {
                        println!("rejected: {r}");
                    }
                    Err(format!("`{id}` is not routable locally"))
                }
            }
        }
        ModelCmd::Benchmark {
            id,
            repetitions,
            timeout_secs,
        } => {
            let registry = registry_for(&rt);
            let router = openrt_router::Router::new();
            let decision = router
                .is_eligible(&registry, id)
                .await
                .map_err(|reasons| format!("not routable: {}", reasons.join("; ")))?;
            let params = openrt_bench::BenchParams {
                repetitions: repetitions.unwrap_or(3),
                timeout_secs: timeout_secs.unwrap_or(60),
            };
            let tasks = vec![openrt_bench::BenchTask::new(
                "default",
                format!(
                    "benchmark prompt for `{}` — local, no fabricated numbers",
                    id
                ),
            )];
            let runner = openrt_bench::BenchRunner::new(rt.provider.as_ref(), params, tasks);
            let result = runner.run(&decision.artifact_id).await;
            openrt_bench::record_result(&*rt.store, &result)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "benchmark `{}` -> ok {}/{} error_rate {:.3} median {}ms p95 {}ms tokens {}",
                result.artifact_id,
                result.aggregate.ok_count,
                result.aggregate.count,
                result.aggregate.error_rate,
                result.aggregate.median_latency_ms.unwrap_or(0),
                result.aggregate.p95_latency_ms.unwrap_or(0),
                result.aggregate.total_output_tokens
            );
            Ok(0)
        }
        ModelCmd::Compatibility {
            id,
            vram_mb,
            ram_mb,
            ctx,
            json,
        } => {
            let vram_mb = vram_mb.unwrap_or(8 * 1024);
            let ram_mb = ram_mb.unwrap_or(32 * 1024);

            // M6: the real runtime's capability-aware planning entry point
            // (registry metadata + provider capabilities + gate posture).
            // Read-only: no model load, no inference, no memory movement.
            let out = rt
                .propose_memory_plan(id, Some(vram_mb * 1024 * 1024), None)
                .await
                .map_err(|e| format!("{id}: {e}"))?;
            let plan = &out.plan;

            if *json {
                let v = serde_json::to_value(&out).map_err(|e| e.to_string())?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
                );
                return Ok(0);
            }

            // Curated-only rows have no declared size; keep the historical
            // "not installed" signal (never fabricate one).
            let Some(size) = out.artifact_bytes else {
                println!(
                    "`{id}` is not installed (curated catalog row only) — no declared size to assess.\n\
                     Install a verified copy first with `openrt model install`."
                );
                return Ok(1);
            };
            let size_mb = size / (1024 * 1024);
            println!("artifact:        {}", plan.artifact_id);
            println!("declared size:   {size} bytes (~{size_mb} MiB)");
            println!("budgets:         {vram_mb} MiB VRAM / {ram_mb} MiB RAM (advisory)");
            let fit = classify_fit(size, vram_mb, ram_mb);
            println!(
                "best-fit tier:   {}",
                match fit {
                    Some(t) => t.as_str(),
                    None => "none (exceeds even NVMe budget — needs offload/merge)",
                }
            );

            let info = rt.provider.info();
            println!(
                "backend:         {} ({:?})",
                info.id,
                format!("{:?}", info.kind).to_lowercase()
            );
            // Hardening-pass Part B: report the requested vs effective context
            // window. An explicit `--ctx` is honoured VERBATIM by configurable
            // providers (Ollama) and refused ONLY above a certified hard max;
            // a provider with no declared window is reported as such — the KV
            // cache of a remote model is never assigned a fabricated figure.
            let cc = rt.provider.context_capability();
            if let Some(requested) = ctx {
                let requested = *requested;
                let effective = cc
                    .effective_window(Some(requested as u32))
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unset".to_string());
                let configurable = if cc.configurable { "yes" } else { "no" };
                let hard_max = if cc.maximum_known {
                    format!("{} tokens", cc.maximum_context.unwrap_or(0))
                } else {
                    "none declared".to_string()
                };
                let admitted = !cc.breaches_certified_maximum(requested);
                println!("context-window: requested={requested} effective={effective} configurable={configurable} hard_max={hard_max} admitted={admitted}");
                if cc.maximum_known && !admitted {
                    println!(
                        "  NOTE: requested {requested} exceeds the provider's certified hard\
                         maximum ({}) — the agent refuses this verbatim, never silently truncates.",
                        cc.maximum_context.unwrap_or(0)
                    );
                }
                println!(
                    "  KV cache residency is {} for this provider — not counted in the memory\
                     plan, no fabricated figure.",
                    if cc.maximum_known {
                        "declared"
                    } else {
                        "unknown (remote model; no hyperparameters)"
                    }
                );
            } else {
                println!(
                    "context-window: provider={} configurable={} hard_max={}",
                    cc.default_context
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "unset".to_string()),
                    cc.configurable,
                    if cc.maximum_known {
                        cc.maximum_context.unwrap_or(0).to_string()
                    } else {
                        "none declared".to_string()
                    }
                );
            }
            println!(
                "capabilities:    gpu_offload={} cpu_offload={} mmap={} file_backed={}",
                out.backend.supports_gpu_offload.as_str(),
                out.backend.supports_cpu_offload.as_str(),
                out.backend.supports_mmap.as_str(),
                out.backend.supports_file_backed_models.as_str(),
            );
            println!(
                "anatomy:         {} (registry record, never re-read from the payload)",
                if out.anatomy_available {
                    "available"
                } else {
                    "unavailable"
                }
            );
            // M8: a versioned-table length sum is reported, never certified.
            if let Some(advisory) = out.advisory_resident_bytes {
                println!(
                    "quant estimate:  ~{advisory} bytes (ggml table v1 — advisory, not certified residency)"
                );
            }
            // M7: show the storage layout when the registry recorded one. This
            // is metadata display only — file layout is never residency.
            if let Ok(Some(anatomy)) = rt.model_registry.anatomy(id).await {
                if anatomy.is_multi_shard() {
                    println!(
                        "layout:          {} files, {} bytes total (storage, not residency)",
                        anatomy.shard_count(),
                        anatomy.total_bytes
                    );
                    for shard in &anatomy.shard_files {
                        println!(
                            "  shard {:<3} {:<34} {:>10} B",
                            shard.shard, shard.file, shard.size_bytes
                        );
                    }
                } else if anatomy.format != openrt_models::AnatomyFormat::Unknown {
                    println!("layout:          single file");
                }
            }
            println!("memory plan:");
            for tier in [
                openrt_resources::Tier::Vram,
                openrt_resources::Tier::RamOwned,
                openrt_resources::Tier::PageCache,
                openrt_resources::Tier::Nvme,
            ] {
                let b = &plan.budget.by_tier[&tier];
                println!("  {:<14} {}", format!("{}:", tier.label()), b.summary());
            }
            println!(
                "  recommended:    {}",
                plan.recommended_tier.map(|t| t.label()).unwrap_or("none")
            );
            println!("  status:         {}", plan.status.as_str());
            for line in &plan.rationale {
                println!("  - {line}");
            }
            Ok(0)
        }
        ModelCmd::Bind {
            id,
            tag,
            provider,
            actor,
        } => {
            let registry = registry_for(&rt);
            let provider = provider
                .clone()
                .unwrap_or_else(|| rt.provider.info().id.to_string());
            registry
                .bind_provider_tag(id, &provider, tag, actor)
                .await
                .map_err(|e| format!("{id}: {e}"))?;
            println!(
                "bound `{id}` → {provider}:{tag} (operator decision; resolution still probes the exact tag)"
            );
            Ok(0)
        }
        ModelCmd::Unbind {
            id,
            provider,
            actor,
        } => {
            let registry = registry_for(&rt);
            let provider = provider
                .clone()
                .unwrap_or_else(|| rt.provider.info().id.to_string());
            let removed = registry
                .unbind_provider_tag(id, &provider, actor)
                .await
                .map_err(|e| format!("{id}: {e}"))?;
            println!(
                "{}",
                if removed {
                    format!("unbound `{id}` from {provider}")
                } else {
                    format!("`{id}` was not bound to {provider} (nothing to remove)")
                }
            );
            Ok(0)
        }
    }
}

/// Single-file manifest: real sha256 + size computed from bytes actually read.
fn single_file_manifest(id: &str, bytes: &[u8]) -> Result<openrt_registry::ManifestV1, String> {
    Ok(openrt_registry::ManifestV1 {
        schema_version: openrt_registry::MANIFEST_SCHEMA_VERSION,
        artifact_id: id.to_string(),
        kind: openrt_scan::ArtifactKind::Binary,
        blobs: vec![openrt_registry::BlobRef {
            file_key: "artifact.bin".to_string(),
            digest: openrt_verify::sha256_hex(bytes),
            size_bytes: bytes.len() as u64,
            // `model install` hands the bytes to the registry inline, so they
            // are materialized in the CAS. Registration by path
            // (`BlobLocation::External`) is a separate route.
            location: openrt_registry::BlobLocation::Stored,
        }],
        base_digest: None,
        created_at: openrt_core::now().to_rfc3339(),
    })
}

fn single_file_map(bytes: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut map = std::collections::BTreeMap::new();
    map.insert("artifact.bin".to_string(), bytes.to_vec());
    map
}

/// Load an optional signed bundle from a JSON file. If `trust_key` files are
/// given, every loaded bundle must verify against at least one key or the
/// install fails closed (no unverified bundle is ever trusted).
fn build_scanner(
    bundle_path: Option<&str>,
    trust_key_paths: &[String],
) -> Result<Option<openrt_scan::Scanner>, String> {
    let Some(path) = bundle_path else {
        if !trust_key_paths.is_empty() {
            return Err("--trust-key requires --scan-bundle".to_string());
        }
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let bundle: openrt_scan::SignatureBundle =
        serde_json::from_str(&text).map_err(|e| format!("invalid bundle JSON: {e}"))?;

    if !trust_key_paths.is_empty() {
        let mut trusted = false;
        for key_path in trust_key_paths {
            let key = load_verifying_key(key_path)?;
            if openrt_scan::verify_signature(&bundle, &key).is_ok() {
                trusted = true;
                break;
            }
        }
        if !trusted {
            return Err(
                "bundle did not verify against any supplied --trust-key; refusing to trust it"
                    .to_string(),
            );
        }
    }

    let mut scanner = openrt_scan::Scanner::new();
    scanner.add_bundle(bundle);
    Ok(Some(scanner))
}

fn load_verifying_key(path: &str) -> Result<ed25519_dalek::VerifyingKey, String> {
    let raw = std::fs::read(path).map_err(|e| format!("read key {path}: {e}"))?;
    let bytes: [u8; 32] = raw
        .try_into()
        .map_err(|_| format!("key file {path} must be exactly 32 bytes"))?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes)
        .map_err(|e| format!("invalid ed25519 public key {path}: {e}"))
}

/// Deterministic best-fit storage tier from declared bytes and explicit
/// operator budgets — never from fabricated hardware measurements.
fn classify_fit(size_bytes: u64, vram_mb: u64, ram_mb: u64) -> Option<openrt_hierarchy::Tier> {
    let size_mb = size_bytes / (1024 * 1024);
    let fits = |budget_mb: u64| size_mb <= budget_mb;
    if fits(vram_mb) {
        Some(openrt_hierarchy::Tier::Vram)
    } else if fits(ram_mb) {
        Some(openrt_hierarchy::Tier::Ram)
    } else {
        // NVMe is slow-sovereign storage; a size that cannot fit RAM is still
        // placeable on disk unless it exceeds a sanity floor.
        match size_mb > 16 * 1024 {
            true => None,
            false => Some(openrt_hierarchy::Tier::Nvme),
        }
    }
}

fn hf_client() -> openrt_registry::HfClient {
    openrt_registry::HfClient::new("", 15)
}

async fn cmd_mode(cli: &Cli, cmd: &ModeCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    let modes = &rt.modes;
    match cmd {
        ModeCmd::Status { json } => {
            let status = modes.status().await.map_err(|e| e.to_string())?;
            if *json {
                let v = serde_json::json!({
                    "current": status.current.as_str(),
                    "since": status.since.to_rfc3339(),
                    "seq": status.seq,
                    "transitions": status
                        .transitions
                        .iter()
                        .map(|t| {
                            serde_json::json!({
                                "seq": t.seq,
                                "at": t.at.to_rfc3339(),
                                "from": t.from,
                                "to": t.to,
                                "actor": t.actor,
                            })
                        })
                        .collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!(
                    "mode:  {} (since {}, audit seq {})",
                    status.current.as_str(),
                    status.since.to_rfc3339(),
                    status.seq
                );
                println!("history:");
                for t in status.transitions.iter().rev() {
                    println!(
                        "  seq {:>3}  {} -> {}  by {}  {}",
                        t.seq,
                        t.from,
                        t.to,
                        t.actor,
                        t.at.to_rfc3339()
                    );
                }
            }
            Ok(0)
        }
        ModeCmd::Set { mode, reason } => {
            let target = match mode.to_ascii_lowercase().as_str() {
                "provisioning" => openrt_modes::ExecutionMode::Provisioning,
                "sovereign" => openrt_modes::ExecutionMode::Sovereign,
                other => {
                    return Err(format!(
                        "invalid mode `{other}` (use provisioning|sovereign)"
                    ))
                }
            };
            let reason = reason.clone().unwrap_or_else(|| "cli request".into());
            let seq = modes
                .transition(&actor_user(), target, &reason)
                .await
                .map_err(|e| e.to_string())?;
            println!("mode is now `{}` (audit seq {seq})", target.as_str());
            Ok(0)
        }
    }
}

async fn cmd_tool(cli: &Cli, _cmd: &ToolCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    // Advertisement = the SAME deny-only filter the agent attaches to every
    // ModelRequest (`Runtime::advertised_tools`), so this diagnostic can
    // never disagree with what the model actually sees.
    let advertised: std::collections::HashSet<String> = rt
        .advertised_tools()
        .into_iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    use openrt_permissions::{Decision, PermissionContext, PermissionRequest, PolicyEvaluator};
    let evaluator = PolicyEvaluator::new(&rt.permission_policy);
    let pctx = PermissionContext::new();
    let empty = serde_json::json!({});
    let verb = |d: Decision| match d {
        Decision::Allow => "allow",
        Decision::Ask => "ask",
        Decision::Deny => "deny",
    };
    let specs = rt.registry.specs();
    println!(
        "{:<20} {:<8} {:<6} {:<10} {:<24} CAPABILITIES",
        "NAME", "VERSION", "POLICY", "ADVERTISED", "BACKEND"
    );
    for s in &specs {
        // Mirror the gate's two-step evaluation (tool id, then each declared
        // capability): most restrictive wins.
        let mut d = evaluator.evaluate(&PermissionRequest::tool(&s.name, &empty, &pctx));
        for cap in &s.required_caps {
            let cd = evaluator.evaluate(&PermissionRequest::capability(&s.name, cap.clone(), None));
            d = match (d, cd) {
                (Decision::Deny, _) | (_, Decision::Deny) => Decision::Deny,
                (Decision::Ask, _) | (_, Decision::Ask) => Decision::Ask,
                _ => Decision::Allow,
            };
        }
        let caps = s
            .required_caps
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{:<20} {:<8} {:<6} {:<10} {:<24} {}",
            s.name,
            s.version,
            verb(d),
            if advertised.contains(&s.name) {
                "yes"
            } else {
                "no"
            },
            tool_backend(&s.name),
            caps
        );
    }
    let hidden = specs
        .iter()
        .filter(|s| !advertised.contains(&s.name))
        .count();
    println!(
        "\n{} registered · {} advertised to the model · {} hidden by policy (deny-by-default)",
        specs.len(),
        advertised.len(),
        hidden
    );
    Ok(0)
}

/// Which subsystem executes a tool (the registering crate — where the tool
/// body runs after the gate, shield, resource gate and sandbox admit it).
/// Subsystem labels only; runtime-resolved backends (ComputerControl kind,
/// browser support, soffice) are reported by `openrt doctor`.
fn tool_backend(name: &str) -> &'static str {
    if name.starts_with("computer.") {
        "openrt-platform"
    } else if name.starts_with("browser.") {
        "openrt-browser"
    } else if name.starts_with("screen.")
        || name.starts_with("input.")
        || name.starts_with("terminal.")
    {
        "openrt-computer"
    } else if name.starts_with("docx.") || name.starts_with("excel.") || name.starts_with("ppt.") {
        "openrt-office"
    } else if name.starts_with("database.") {
        "openrt-database"
    } else if name == "python.run" {
        "openrt-sandbox"
    } else if name.starts_with("mcp.") {
        "openrt-mcp"
    } else if name.starts_with("industrial.")
        || name.starts_with("trading.")
        || name.starts_with("learn.")
    {
        "openrt-runtime"
    } else if name.starts_with("application.")
        || name.starts_with("artifact.")
        || name == "tools.list"
    {
        "openrt-fabric"
    } else if name.starts_with("filesystem.") {
        // `filesystem.read` is the builtin; list/write/delete/move are the
        // computer-use filesystem tools.
        if name == "filesystem.read" {
            "openrt-tools"
        } else {
            "openrt-computer"
        }
    } else {
        "openrt-tools"
    }
}

async fn cmd_session(cli: &Cli, cmd: &SessionCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        SessionCmd::Create { title } => {
            if title.trim().is_empty() {
                return Err("session title must not be empty".to_string());
            }
            let session = rt.create_session(title).await.map_err(|e| e.to_string())?;
            println!("created session {} ({title})", session.id);
            Ok(0)
        }
        SessionCmd::Show { id } => {
            let sid = parse_session_id(id)?;
            let s = rt.load_session(sid).await.map_err(|e| e.to_string())?;
            println!("session:     {}", s.id);
            println!("title:       {}", s.title);
            println!("status:      {:?}", s.status);
            println!("messages:    {}", s.content.len());
            println!("created_at:  {}", s.created_at.to_rfc3339());
            println!("updated_at:  {}", s.updated_at.to_rfc3339());
            let tasks = rt
                .store
                .list_tasks(Some(s.id))
                .await
                .map_err(|e| e.to_string())?;
            println!("tasks:       {}", tasks.len());
            for t in tasks.iter().take(10) {
                println!(
                    "  {:<38} {:?}  {}",
                    t.id,
                    t.status,
                    t.prompt.chars().take(48).collect::<String>()
                );
            }
            Ok(0)
        }
        SessionCmd::List { json } => {
            let sessions = rt.list_sessions().await.map_err(|e| e.to_string())?;
            if *json {
                let v: Vec<_> = sessions
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "id": s.id.to_string(),
                            "title": s.title,
                            "messages": s.content.len(),
                            "status": format!("{:?}", s.status),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!("{:<38} {:<20} {:<6} STATUS", "ID", "TITLE", "MSGS");
                for s in &sessions {
                    println!(
                        "{:<38} {:<20} {:<6} {:?}",
                        s.id,
                        s.title,
                        s.content.len(),
                        s.status
                    );
                }
                println!("{} session(s)", sessions.len());
            }
            Ok(0)
        }
    }
}

async fn cmd_workspace(cli: &Cli, cmd: &WorkspaceCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    let now = openrt_core::now().to_rfc3339();
    match cmd {
        WorkspaceCmd::Create {
            address,
            title,
            description,
        } => {
            if address.trim().is_empty() {
                return Err("workspace address must not be empty".to_string());
            }
            let id =
                openrt_workspace::create_workspace(&*rt.store, address, title, description, &now)
                    .await
                    .map_err(|e| e.to_string())?;
            println!("created workspace {address} (id {id})");
            Ok(0)
        }
        WorkspaceCmd::Open { address } => {
            let ws = openrt_workspace::open_workspace(&*rt.store, address, &now)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "opened {} — {} ({})",
                ws.address_code, ws.title, ws.description
            );
            println!("id:    {}", ws.id);
            println!("digest:{}", ws.digest);
            Ok(0)
        }
        WorkspaceCmd::Reopen => {
            let ws = openrt_workspace::reopen_current(&*rt.store, &now)
                .await
                .map_err(|e| e.to_string())?;
            println!("reopened {} — {}", ws.address_code, ws.title);
            Ok(0)
        }
        WorkspaceCmd::List { json } => {
            let rows = openrt_workspace::list_workspaces(&*rt.store)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                println!("{:<40} {:<24} DESCRIPTION", "ADDRESS", "TITLE");
                for (_key, value) in &rows {
                    let address = value
                        .get("address_code")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let title = value.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let description = value
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    println!("{address:<40} {title:<24} {description}");
                }
                println!("{} workspace(s)", rows.len());
            }
            Ok(0)
        }
    }
}

async fn cmd_workbench(cli: &Cli, cmd: &WorkbenchCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        WorkbenchCmd::Room { cmd } => cmd_room(&rt, cmd).await,
        WorkbenchCmd::Material { cmd } => cmd_material(&rt, cmd).await,
    }
}

/// Top-level `openrt room *` (M11.10) — identical handlers, one authority.
async fn cmd_room_top(cli: &Cli, cmd: &RoomCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    cmd_room(&rt, cmd).await
}

/// Top-level `openrt material *` (M11.10) — identical handlers.
async fn cmd_material_top(cli: &Cli, cmd: &MaterialCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    cmd_material(&rt, cmd).await
}

/// Phase 11 M11.10 `openrt computer *`: read-only observation/resolution
/// plus gated action execution (the ONE `computer.use` tool — never a
/// second authority).
async fn cmd_computer(cli: &Cli, cmd: &ComputerCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        ComputerCmd::Env { json } => {
            let payload = serde_json::json!({
                "summary": rt.computer.summary(),
                "profile": rt.computer.profile().to_value(),
                "resolver": rt.computer.decisions(),
                "capabilities": rt.computer.capability_matrix().rows(),
            });
            if *json {
                println!("{}", to_pretty(&payload)?);
            } else {
                println!(
                    "screen backend: {}  computer backend: {}",
                    rt.screen.kind(),
                    rt.computer.summary().backend
                );
                for row in rt.computer.capability_matrix().rows() {
                    println!(
                        "  {:<28} {:<20} {}",
                        row.capability,
                        row.status.as_str(),
                        row.reason
                    );
                }
            }
            Ok(0)
        }
        ComputerCmd::Observe { clipboard, json } => {
            let engine = computer_engine(&rt)?;
            let bundle = engine
                .observe(*clipboard)
                .await
                .map_err(|e| format!("observe: {e}"))?;
            let v = serde_json::to_value(&bundle).map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&v)?);
            } else {
                println!(
                    "captured_at={} screen={} computer={} {}x{} windows={} elements={} \
                     focused={} cursor={:?} clipboard={}",
                    bundle.captured_at,
                    bundle.screen_backend,
                    bundle.computer_backend,
                    bundle.width,
                    bundle.height,
                    bundle.windows.len(),
                    bundle.elements.len(),
                    bundle.focused_window.as_deref().unwrap_or("<none>"),
                    bundle.cursor,
                    if bundle.clipboard_requested {
                        bundle.clipboard.as_deref().unwrap_or("<unavailable>")
                    } else {
                        "<not requested>"
                    },
                );
            }
            Ok(0)
        }
        ComputerCmd::Resolve { target, x, y, json } => {
            let at = match (x, y) {
                (Some(a), Some(b)) => Some((*a, *b)),
                (None, None) => None,
                _ => return Err("`--x` and `--y` must be provided together".into()),
            };
            let engine = computer_engine(&rt)?;
            let res = engine
                .resolve_target(target.as_deref().unwrap_or(""), at)
                .await
                .map_err(|e| format!("resolve: {e}"))?;
            let v = serde_json::to_value(&res).map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&v)?);
            } else {
                println!(
                    "resolved `{}` via {} (confidence {:.2}, point {:?})",
                    res.target,
                    res.source.as_str(),
                    res.confidence,
                    res.point
                );
                for step in &res.chain {
                    println!("  {:<15} {:<12} {}", step.source, step.status, step.detail);
                }
            }
            Ok(0)
        }
        ComputerCmd::Use {
            action,
            target,
            x,
            y,
            from_x,
            from_y,
            to_x,
            to_y,
            button,
            delta,
            text,
            key,
            url,
            app,
            name,
            json,
        } => {
            let mut args = serde_json::Map::new();
            args.insert("action".into(), serde_json::Value::String(action.clone()));
            put_opt_str(&mut args, "target", target);
            put_opt_i64(&mut args, "x", x);
            put_opt_i64(&mut args, "y", y);
            put_opt_i64(&mut args, "from_x", from_x);
            put_opt_i64(&mut args, "from_y", from_y);
            put_opt_i64(&mut args, "to_x", to_x);
            put_opt_i64(&mut args, "to_y", to_y);
            put_opt_i64(&mut args, "delta", delta);
            put_opt_str(&mut args, "text", text);
            put_opt_str(&mut args, "key", key);
            put_opt_str(&mut args, "url", url);
            put_opt_str(&mut args, "app", app);
            put_opt_str(&mut args, "name", name);
            if *button != 1 {
                args.insert("button".into(), serde_json::json!(button));
            }
            let args = serde_json::Value::Object(args);
            // Execute through the ONE gate: policy → AgentShield → sandbox/
            // kill scope `computer` → audit. Deny-by-default: the operator's
            // config must allow `computer.use.all`.
            let ctx = openrt_tools::ToolExecutionContext {
                session_id: None,
                task_id: None,
                channel: Some("cli".into()),
            };
            let out = rt
                .executor
                .execute("computer.use", args, &ctx)
                .await
                .map_err(|e| format!("computer.use: {e}"))?;
            match out {
                openrt_tools::ToolOutput::Json(v) => {
                    if *json {
                        println!("{}", to_pretty(&v)?);
                    } else if let Some(report) = v.as_object() {
                        println!(
                            "action={} ok={} attempts={} verified={}",
                            report.get("action").and_then(|a| a.as_str()).unwrap_or("?"),
                            report.get("ok").and_then(|o| o.as_bool()).unwrap_or(false),
                            report.get("attempts").and_then(|a| a.as_u64()).unwrap_or(0),
                            match report.get("verified").and_then(|x| x.as_bool()) {
                                Some(b) => b.to_string(),
                                None => "unverifiable".into(),
                            },
                        );
                        if let Some(effect) = report
                            .get("advisory")
                            .and_then(|a| a.get("predicted_effect"))
                            .and_then(|e| e.as_str())
                        {
                            println!("predicted: {effect}");
                        }
                        if let Some(source) = report
                            .get("resolved")
                            .and_then(|r| r.get("source"))
                            .and_then(|s| s.as_str())
                        {
                            println!("resolved via: {source}");
                        }
                    } else {
                        println!("{}", to_pretty(&v)?);
                    }
                }
                openrt_tools::ToolOutput::Text(t) => println!("{t}"),
            }
            Ok(0)
        }
    }
}

fn put_opt_str(map: &mut serde_json::Map<String, serde_json::Value>, k: &str, v: &Option<String>) {
    if let Some(s) = v {
        map.insert(k.into(), serde_json::Value::String(s.clone()));
    }
}

fn put_opt_i64(map: &mut serde_json::Map<String, serde_json::Value>, k: &str, v: &Option<i64>) {
    if let Some(n) = v {
        map.insert(k.into(), serde_json::json!(n));
    }
}

/// One ComputerUse engine per CLI invocation (read-only commands; `use`
/// goes through the registered tool + gate instead of this engine).
fn computer_engine(rt: &Arc<Runtime>) -> Result<Arc<openrt_computer::ComputerUse>, String> {
    // The ONE engine the `computer.use` tool was registered with — the CLI
    // shares its observation state / digital twin instead of minting a
    // second engine per call (M11.8: no duplicate authority per surface).
    Ok(rt.computer_engine.clone())
}

fn parse_room_status(status: &str) -> Result<openrt_workspace::RoomStatus, String> {
    match status {
        "active" => Ok(openrt_workspace::RoomStatus::Active),
        "paused" => Ok(openrt_workspace::RoomStatus::Paused),
        other => Err(format!("invalid status `{other}` (expected active|paused)")),
    }
}

fn parse_material_kind(kind: &str) -> Result<openrt_workspace::MaterialKind, String> {
    serde_json::from_value::<openrt_workspace::MaterialKind>(serde_json::Value::String(
        kind.to_string(),
    ))
    .map_err(|_| {
        format!(
            "invalid kind `{kind}` (expected document|image|dataset|evidence|artifact|\
             workflow|knowledge|session|task)"
        )
    })
}

async fn cmd_room(
    rt: &std::sync::Arc<openrt_runtime::Runtime>,
    cmd: &RoomCmd,
) -> Result<i32, String> {
    match cmd {
        RoomCmd::Create {
            workbench,
            title,
            operator,
            json,
        } => {
            let room = rt
                .create_room(workbench, title, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                println!(
                    "created room {} — `{}` in workbench {workbench}",
                    room.id, room.title
                );
            }
            Ok(0)
        }
        RoomCmd::List { json } => {
            let rooms = rt.list_rooms().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&rooms).unwrap())?);
            } else {
                println!("{:<66} {:<24} {:<8} TITLE", "ROOM", "WORKBENCH", "STATUS");
                for r in &rooms {
                    println!(
                        "{:<66} {:<24} {:<8} {}",
                        r.id,
                        r.workbench_code,
                        r.status.to_string(),
                        r.title
                    );
                }
                println!("{} room(s)", rooms.len());
            }
            Ok(0)
        }
        RoomCmd::Show { id, json } => {
            let room = rt.room(id).await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                println!("room {}", room.id);
                println!("  title:     {}", room.title);
                println!("  workbench: {}", room.workbench_code);
                println!("  status:    {}", room.status);
                println!("  created:   {} by {}", room.created_at, room.created_by);
                println!(
                    "  sessions:  {}  tasks: {}  workflows: {}  materials: {}  artifacts: {}",
                    room.sessions.len(),
                    room.tasks.len(),
                    room.workflows.len(),
                    room.materials.len(),
                    room.artifacts.len()
                );
                println!(
                    "  active workflow: {}",
                    room.active_workflow.as_deref().unwrap_or("-")
                );
                println!("  history:");
                for h in &room.history {
                    println!("    {} {} {}: {}", h.at, h.actor, h.action, h.detail);
                }
                println!("  steer:");
                for s in &room.steer {
                    println!("    {} {}: {}", s.at, s.operator, s.input);
                }
            }
            Ok(0)
        }
        RoomCmd::Status {
            id,
            status,
            operator,
            json,
        } => {
            let status = parse_room_status(status)?;
            let room = rt
                .set_room_status(id, status, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                println!("room {id} status → {status}");
            }
            Ok(0)
        }
        RoomCmd::SetWorkflow {
            id,
            workflow_id,
            operator,
            json,
        } => {
            let room = rt
                .set_room_workflow(id, Some(workflow_id), operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                println!("room {id} active workflow → {workflow_id}");
            }
            Ok(0)
        }
        RoomCmd::ClearWorkflow { id, operator, json } => {
            let room = rt
                .set_room_workflow(id, None, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                println!("room {id} active workflow cleared");
            }
            Ok(0)
        }
        RoomCmd::Steer {
            id,
            input,
            operator,
            json,
        } => {
            let entry = rt
                .room_steer(id, operator, input)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&entry).unwrap())?);
            } else {
                println!("steered room {id} at {}", entry.at);
            }
            Ok(0)
        }
        RoomCmd::AttachMaterial {
            id,
            material,
            operator,
            json,
        } => {
            let room = rt
                .attach_material_to_room(id, material, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                println!("material {material} attached to room {id}");
            }
            Ok(0)
        }
        RoomCmd::DetachMaterial {
            id,
            material,
            operator,
            json,
        } => {
            let room = rt
                .detach_material_from_room(id, material, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                println!("material {material} detached from room {id}");
            }
            Ok(0)
        }
        RoomCmd::LinkSession {
            id,
            session,
            operator,
            json,
        } => {
            let session_id = parse_session_id(session)?;
            let room = rt
                .link_room_session(id, &session_id.to_string(), operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                println!("session {session_id} linked to room {id}");
            }
            Ok(0)
        }
        RoomCmd::Agent { cmd } => cmd_room_agent(rt, cmd).await,
    }
}

async fn cmd_room_agent(rt: &Arc<Runtime>, cmd: &RoomAgentCmd) -> Result<i32, String> {
    match cmd {
        RoomAgentCmd::Add {
            id,
            agent,
            role,
            operator,
            json,
        } => {
            let room = rt
                .link_room_agent(id, agent, role, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                let kind = openrt_orch::room_role_sub_agent(role)
                    .map(|k| k.as_str())
                    .unwrap_or("?");
                println!("{agent} linked to room {id} as {role} ({kind})");
            }
            Ok(0)
        }
        RoomAgentCmd::Remove {
            id,
            agent,
            operator,
            json,
        } => {
            let room = rt
                .unlink_room_agent(id, agent, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&room).unwrap())?);
            } else {
                println!("{agent} unlinked from room {id}");
            }
            Ok(0)
        }
        RoomAgentCmd::List { id, json } => {
            let room = rt.room(id).await.map_err(|e| e.to_string())?;
            if *json {
                println!(
                    "{}",
                    to_pretty(&serde_json::to_value(&room.agents).unwrap())?
                );
            } else if room.agents.is_empty() {
                println!("room {id} has no agents");
            } else {
                for a in &room.agents {
                    println!("{}\t{}\tadded {}", a.id, a.role, a.added_at);
                }
            }
            Ok(0)
        }
    }
}

async fn cmd_material(
    rt: &std::sync::Arc<openrt_runtime::Runtime>,
    cmd: &MaterialCmd,
) -> Result<i32, String> {
    match cmd {
        MaterialCmd::Create {
            kind,
            title,
            source,
            ref_ns,
            ref_key,
            ref_path,
            operator,
            json,
        } => {
            let kind = parse_material_kind(kind)?;
            let reference = openrt_workspace::parse_reference(
                ref_ns.as_deref(),
                ref_key.as_deref(),
                ref_path.as_deref(),
            )
            .map_err(|e| e.to_string())?;
            let material = rt
                .create_material(kind, title, source, reference, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&material).unwrap())?);
            } else {
                println!(
                    "created material {} ({kind}) — {}",
                    material.id, material.title
                );
                println!("  ref:    {}", material.reference);
                println!("  digest: {}", material.digest);
            }
            Ok(0)
        }
        MaterialCmd::List { json } => {
            let materials = rt.list_materials().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&materials).unwrap())?);
            } else {
                println!("{:<66} {:<10} {:<20} TITLE", "MATERIAL", "KIND", "SOURCE");
                for m in &materials {
                    println!(
                        "{:<66} {:<10} {:<20} {}",
                        m.id,
                        m.kind.to_string(),
                        m.source,
                        m.title
                    );
                }
                println!("{} material(s)", materials.len());
            }
            Ok(0)
        }
        MaterialCmd::Show { id, json } => {
            let m = rt.material(id).await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&m).unwrap())?);
            } else {
                println!("material {}", m.id);
                println!("  kind:      {}", m.kind);
                println!("  title:     {}", m.title);
                println!("  source:    {}", m.source);
                println!("  ref:       {}", m.reference);
                println!("  digest:    {}", m.digest);
                println!("  created:   {} by {}", m.created_at, m.created_by);
                println!(
                    "  attached:  {}",
                    if m.attached_workbenches.is_empty() {
                        "-".to_string()
                    } else {
                        m.attached_workbenches.join(", ")
                    }
                );
            }
            Ok(0)
        }
        MaterialCmd::Verify { id, json } => {
            let intact = openrt_workspace::verify_material(&*rt.store, id)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                let v = serde_json::json!({ "material": id, "intact": intact });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else if intact {
                println!("material {id}: intact");
            } else {
                println!("material {id}: DRIFTED (reference content no longer matches the sealed digest)");
            }
            Ok(if intact { 0 } else { 2 })
        }
        MaterialCmd::Attach {
            id,
            workbench,
            operator,
            json,
        } => {
            let m = rt
                .attach_material_to_workbench(id, workbench, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&m).unwrap())?);
            } else {
                println!("material {id} attached to workbench {workbench}");
            }
            Ok(0)
        }
        MaterialCmd::Detach {
            id,
            workbench,
            operator,
            json,
        } => {
            let m = rt
                .detach_material_from_workbench(id, workbench, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&m).unwrap())?);
            } else {
                println!("material {id} detached from workbench {workbench}");
            }
            Ok(0)
        }
    }
}

async fn cmd_workflow(cli: &Cli, cmd: &WorkflowCmd) -> Result<i32, String> {
    let _ = cli;
    let rt = build_runtime(cli).await?;
    match cmd {
        WorkflowCmd::Compile { goal, mode, json } => {
            if goal.trim().is_empty() {
                return Err("goal must not be empty".to_string());
            }
            let mode: openrt_compiler::CompileMode = mode.parse().map_err(|e: String| e)?;
            let out = rt
                .compile_workflow(goal, mode)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                let v = serde_json::json!({
                    "workflow_id": out.workflow_id,
                    "mode": out.mode.as_str(),
                    "stage": out.stage_reached.as_str(),
                    "persisted": out.persisted,
                    "steps": out.workflow.steps.len(),
                    "goal_id": out.goal.id,
                    "nodes": out.provenance.nodes.len(),
                    "diagnostics": out.diagnostics,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!("compiled {}", out.workflow_id);
                println!(
                    "  mode={} stage={} steps={} persisted={}",
                    out.mode.as_str(),
                    out.stage_reached.as_str(),
                    out.workflow.steps.len(),
                    out.persisted
                );
                for n in &out.provenance.nodes {
                    println!("  - {} ({}): {}", n.step_id, n.subgoal_id, n.why_exists);
                }
                for d in &out.diagnostics {
                    println!("  warn: {d}");
                }
            }
            Ok(0)
        }
        WorkflowCmd::Inspect { workflow_id, json } => {
            let (rec, wf) = rt
                .load_compiled_workflow(workflow_id)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                let v = serde_json::json!({
                    "workflow_id": wf.id,
                    "goal": rec.goal,
                    "steps": wf.steps,
                    "provenance": rec.provenance,
                    "validation": rec.validation,
                    "compiler_version": rec.compiler_version,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!("workflow {}", wf.id);
                println!("  goal: {}", rec.goal.objective);
                println!("  original: {}", rec.goal.original_request);
                println!("  compiler: {}", rec.compiler_version);
                println!("  steps: {}", wf.steps.len());
                for s in &wf.steps {
                    println!("    {} [{:?}] deps={:?}", s.id, s.kind, s.depends_on);
                }
            }
            Ok(0)
        }
        WorkflowCmd::Validate { workflow_id, json } => {
            let (rec, _wf) = rt
                .load_compiled_workflow(workflow_id)
                .await
                .map_err(|e| e.to_string())?;
            let report = rec.validation.ok_or_else(|| {
                "compiled record has no validation report (recompile with --mode validate|execute)"
                    .to_string()
            })?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!(
                    "workflow {} valid={}",
                    report.workflow_id,
                    report.is_valid()
                );
                for c in &report.checks {
                    println!("  [{:?}] {}: {}", c.severity, c.name, c.what);
                }
            }
            Ok(if report.is_valid() { 0 } else { 1 })
        }
        WorkflowCmd::Explain { workflow_id, json } => {
            let (rec, _wf) = rt
                .load_compiled_workflow(workflow_id)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rec.provenance).unwrap());
            } else {
                println!(
                    "goal {} (original: {})",
                    rec.provenance.goal_id, rec.provenance.original_request
                );
                for n in &rec.provenance.nodes {
                    println!("\nnode {}", n.step_id);
                    println!("  why exists: {}", n.why_exists);
                    println!("  why capability: {}", n.why_capability);
                    println!("  deps: {:?}", n.dependencies);
                    println!("  inputs: {:?}", n.inputs);
                    println!("  outputs: {:?}", n.outputs);
                    if let Some(v) = &n.verification {
                        println!("  verify: {v}");
                    }
                }
            }
            Ok(0)
        }
        WorkflowCmd::Run {
            goal,
            session,
            json,
        } => {
            if goal.trim().is_empty() {
                return Err("goal must not be empty".to_string());
            }
            let session_id = match session {
                Some(id) => parse_session_id(id)?,
                None => {
                    rt.create_session("workflow")
                        .await
                        .map_err(|e| e.to_string())?
                        .id
                }
            };
            let (out, report) = rt
                .compile_and_run(session_id, goal)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                let v = serde_json::json!({
                    "workflow_id": out.workflow_id,
                    "session_id": session_id.to_string(),
                    "status": report.status.to_string(),
                    "steps": report.steps,
                    "persisted": out.persisted,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!(
                    "compiled {} → {} ({} steps)",
                    out.workflow_id,
                    report.status,
                    report.steps.len()
                );
                for s in &report.steps {
                    println!("  {} {:?} attempts={}", s.step, s.status, s.attempts);
                }
            }
            Ok(if report.status == openrt_orch::WorkflowStatus::Completed {
                0
            } else {
                1
            })
        }
        WorkflowCmd::List { json } => {
            let list = openrt_compiler::list_compiled(&rt.store)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                let rows: Vec<_> = list
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "workflow_id": r.workflow_id,
                            "goal_id": r.goal.id,
                            "objective": r.goal.objective,
                            "steps": r.decomposition.subgoals.len(),
                            "compiled_at": r.compiled_at,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                println!("{:<40} {:<16} OBJECTIVE", "WORKFLOW", "GOAL");
                for r in &list {
                    println!(
                        "{:<40} {:<16} {}",
                        r.workflow_id, r.goal.id, r.goal.objective
                    );
                }
                println!("{} compiled workflow(s)", list.len());
            }
            Ok(0)
        }
        WorkflowCmd::Pause {
            workflow_id,
            operator,
            json,
        } => {
            rt.workflow_pause(workflow_id, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                let v = serde_json::json!({
                    "workflow_id": workflow_id,
                    "pause": "requested",
                    "operator": operator,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!(
                    "pause requested for {workflow_id} (cooperative — stops at the next checkpoint)"
                );
            }
            Ok(0)
        }
        WorkflowCmd::Resume {
            workflow_id,
            operator,
            json,
        } => {
            let report = rt
                .workflow_resume(workflow_id, operator)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                let v = serde_json::json!({
                    "workflow_id": workflow_id,
                    "status": report.status.to_string(),
                    "steps": report.steps,
                    "operator": operator,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!(
                    "resumed {} → {} ({} steps)",
                    workflow_id,
                    report.status,
                    report.steps.len()
                );
                for s in &report.steps {
                    println!("  {} {:?} attempts={}", s.step, s.status, s.attempts);
                }
            }
            Ok(if report.status == openrt_orch::WorkflowStatus::Completed {
                0
            } else {
                1
            })
        }
    }
}

async fn cmd_tui(cli: &Cli) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    let app = openrt_tui::App::new(rt).await.map_err(|e| e.to_string())?;
    openrt_tui::run_tui(app).await.map_err(|e| e.to_string())?;
    Ok(0)
}

/// Agent TUI assembly: the one Runtime plus the two UI wiring points (event
/// sink + `Ask` resolver) and the per-request model switch wrapper.
fn agent_builder(
    cli: &Cli,
) -> Result<(RuntimeBuilder, Arc<openrt_tui::SwitchableProvider>), String> {
    let config = effective_config()?;
    let mut b = RuntimeBuilder::new().config(config.clone());
    if cli.memory {
        b = b.store(Arc::new(openrt_persistence::MemoryStore::new()));
    }
    let forced = cli.mock
        || std::env::var("OPENRT_MOCK")
            .map(|v| v == "1")
            .unwrap_or(false);
    let inner: Arc<dyn openrt_models::ModelProvider> = if forced {
        Arc::new(openrt_models::mock::MockProvider::tool_sequence("mock"))
    } else {
        openrt_runtime::runtime::build_default_provider(&config).map_err(|e| e.to_string())?
    };
    let switcher = openrt_tui::SwitchableProvider::new(inner);
    b = b.with_provider(switcher.clone());
    Ok((b, switcher))
}

async fn cmd_agent(cli: &Cli) -> Result<i32, String> {
    let (builder, switcher) = agent_builder(cli)?;
    let (sink, rx) = openrt_tui::agent::UiSink::channel();
    let resolver = openrt_tui::agent::TuiResolver::new();
    let rt = builder
        .sink(sink)
        .permission_resolver(resolver.clone())
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let workbench = openrt_tui::App::new(rt.clone())
        .await
        .map_err(|e| e.to_string())?;
    let app = openrt_tui::AgentApp::new(rt.clone(), workbench, Some(switcher), resolver, rx)
        .await
        .map_err(|e| e.to_string())?;
    let res = openrt_tui::run_agent(app).await.map_err(|e| e.to_string());
    // Flush the bus (events + history ledger) before process exit.
    rt.shutdown().await;
    res?;
    Ok(0)
}

async fn cmd_task(cli: &Cli, cmd: &TaskCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        TaskCmd::Run {
            prompt,
            session,
            json,
        } => {
            if prompt.trim().is_empty() {
                return Err("task prompt must not be empty".to_string());
            }
            let session_id = match session {
                Some(id) => parse_session_id(id)?,
                None => {
                    rt.create_session("cli")
                        .await
                        .map_err(|e| e.to_string())?
                        .id
                }
            };
            let task = rt
                .run_task(session_id, prompt)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                let v = serde_json::json!({
                    "task_id": task.id.to_string(),
                    "session_id": session_id.to_string(),
                    "status": format!("{:?}", task.status),
                    "result": task.result,
                    "error": task.error,
                    "turns": task.turns.len(),
                    "segments": task.segments,
                    "cumulative": task.cumulative(),
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else if task.status == openrt_core::TaskStatus::Completed {
                let segs = task.segments.len();
                let cum = task.cumulative();
                if segs > 1 {
                    println!(
                        "{} [{} segments: {} turns / {} tool calls / {} retries]",
                        task.result.unwrap_or_default(),
                        segs,
                        cum.turns,
                        cum.tool_calls,
                        cum.retries
                    );
                } else {
                    println!("{}", task.result.unwrap_or_default());
                }
            } else {
                println!(
                    "task {:?}: {}",
                    task.status,
                    task.error.as_deref().unwrap_or_default()
                );
            }
            // P2.7: a run that did not complete is a failure — exit 1 so
            // scripts and automation observe it in the exit code.
            Ok(if task.status == openrt_core::TaskStatus::Completed {
                0
            } else {
                1
            })
        }
        TaskCmd::List { session, json } => {
            let session_id = match session {
                Some(id) => parse_session_id(id)?,
                None => {
                    let sessions = rt.list_sessions().await.map_err(|e| e.to_string())?;
                    sessions.last().map(|s| s.id).ok_or_else(|| {
                        "no sessions exist — run `openrt session create` first".to_string()
                    })?
                }
            };
            if let Some(sid) = session.as_deref() {
                let exists = rt
                    .list_sessions()
                    .await
                    .map_err(|e| e.to_string())?
                    .iter()
                    .any(|s| s.id == session_id);
                if !exists {
                    return Err(format!("session `{sid}` not found"));
                }
            }
            let tasks = rt
                .store
                .list_tasks(Some(session_id))
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                let v: Vec<_> = tasks
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "task_id": t.id.to_string(),
                            "session_id": session_id.to_string(),
                            "status": format!("{:?}", t.status),
                            "result": t.result,
                            "error": t.error,
                            "turns": t.turns.len(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!("{:<36} {:<10} {:<6} RESULT", "TASK ID", "STATUS", "TURNS");
                for t in &tasks {
                    let result = t.result.clone().unwrap_or_default();
                    let snippet: String = result.chars().take(40).collect();
                    println!(
                        "{:<36} {:<10} {:<6} {}",
                        t.id,
                        format!("{:?}", t.status),
                        t.turns.len(),
                        snippet
                    );
                }
            }
            Ok(0)
        }
        TaskCmd::Resume {
            session,
            workflow,
            checkpoint,
        } => run_recovery(&rt, session, workflow, checkpoint, RecoveryScope::Resume).await,
        TaskCmd::Retry {
            session,
            workflow,
            checkpoint,
        } => run_recovery(&rt, session, workflow, checkpoint, RecoveryScope::RetryOnly).await,
        TaskCmd::Pause { task_id, json } => {
            let tid: openrt_core::TaskId = task_id
                .parse()
                .map_err(|e| format!("invalid task id `{task_id}`: {e}"))?;
            let t = rt.task_pause(tid).await.map_err(|e| e.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "task_id": t.id.to_string(),
                        "status": format!("{:?}", t.status),
                        "paused": t.status == openrt_core::TaskStatus::Paused,
                    }))
                    .unwrap()
                );
            } else {
                println!(
                    "task {} pause signalled: status={:?} (cooperative; persists at the next turn boundary)",
                    t.id, t.status
                );
            }
            Ok(0)
        }
        TaskCmd::ResumeTask { task_id, json } => {
            let tid: openrt_core::TaskId = task_id
                .parse()
                .map_err(|e| format!("invalid task id `{task_id}`: {e}"))?;
            let t = rt.task_resume(tid).await.map_err(|e| e.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "task_id": t.id.to_string(),
                        "status": format!("{:?}", t.status),
                        "result": t.result,
                        "error": t.error,
                    }))
                    .unwrap()
                );
            } else {
                match t.status {
                    openrt_core::TaskStatus::Completed => {
                        println!("{}", t.result.unwrap_or_default())
                    }
                    _ => println!("task {} resumed: status={:?}", t.id, t.status),
                }
            }
            Ok(if t.status == openrt_core::TaskStatus::Completed {
                0
            } else {
                1
            })
        }
    }
}

fn parse_session_id(id: &str) -> Result<openrt_core::SessionId, String> {
    let sid: openrt_core::SessionId = id
        .parse()
        .map_err(|e| format!("invalid session id `{id}`: {e}"))?;
    Ok(sid)
}

fn cmd_config(cmd: &ConfigCmd) -> Result<i32, String> {
    match cmd {
        ConfigCmd::Show => {
            let config = effective_config()?;
            println!("{}", serde_json::to_string_pretty(&config).unwrap());
            Ok(0)
        }
    }
}

fn parse_checkpoint_id(id: &str) -> Result<CheckpointId, String> {
    id.parse()
        .map_err(|e| format!("invalid checkpoint id `{id}`: {e}"))
}

async fn cmd_checkpoint(cli: &Cli, cmd: &CheckpointCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        CheckpointCmd::List { workflow, json } => {
            let metas = rt
                .list_checkpoints(workflow.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&metas).unwrap());
            } else {
                println!(
                    "{:<38} {:<24} {:<9} {:<11} {:<7} CREATED_AT",
                    "CHECKPOINT", "WORKFLOW", "KIND", "STATUS", "NODES"
                );
                for m in &metas {
                    println!(
                        "{:<38} {:<24} {:<9} {:<11} {:<7} {}",
                        m.checkpoint_id,
                        m.workflow_id,
                        format!("{:?}", m.kind).to_lowercase(),
                        format!("{:?}", m.status).to_lowercase(),
                        format!("{}/{}", m.nodes_completed, m.nodes_total),
                        m.created_at
                    );
                }
                println!("{} checkpoint(s)", metas.len());
            }
            Ok(0)
        }
        CheckpointCmd::Inspect { checkpoint } => {
            let id = parse_checkpoint_id(checkpoint)?;
            let cp = rt.load_checkpoint(&id).await.map_err(|e| e.to_string())?;
            println!(
                "checkpoint   {}",
                cp.execution
                    .previous_checkpoint_id
                    .as_ref()
                    .map(|p| format!("(previous: {p})"))
                    .unwrap_or_default()
            );
            println!("id:          {}", cp.checkpoint_id);
            println!("schema:      v{}", cp.schema_version);
            println!("session:     {}", cp.session_id);
            println!("workflow:    {}", cp.workflow_id);
            println!("kind:        {:?}", cp.kind);
            println!("status:      {:?}", cp.status);
            println!("created:     {}", cp.created_at);
            println!(
                "nodes:       {} completed, {} pending, {} failed, {} uncertain",
                cp.completed.len(),
                cp.pending.len(),
                cp.failed.len(),
                cp.uncertain.len()
            );
            if let Some(note) = &cp.execution.note {
                println!("note:        {note}");
            }
            println!("node states:");
            for ns in &cp.node_states {
                let err = ns
                    .error
                    .as_deref()
                    .map(|e| format!("  error={e}"))
                    .unwrap_or_default();
                println!(
                    "  {:<24} {:<10} attempts={:>2} effect={:?}{}",
                    ns.step,
                    format!("{:?}", ns.state),
                    ns.attempts,
                    ns.effect,
                    err
                );
            }
            Ok(0)
        }
        CheckpointCmd::Delete { checkpoint } => {
            let id = parse_checkpoint_id(checkpoint)?;
            rt.delete_checkpoint(&id).await.map_err(|e| e.to_string())?;
            println!("deleted checkpoint {id}");
            Ok(0)
        }
    }
}

async fn run_recovery(
    rt: &Runtime,
    session: &str,
    workflow_id: &str,
    checkpoint: &str,
    scope: RecoveryScope,
) -> Result<i32, String> {
    let session_id: SessionId = session
        .parse()
        .map_err(|e| format!("invalid session id `{session}`: {e}"))?;
    let checkpoint_id = parse_checkpoint_id(checkpoint)?;
    let dag = rt
        .store
        .get_record(DAG_NS, workflow_id)
        .await
        .map_err(|e| format!("could not load workflow `{workflow_id}`: {e}"))?
        .ok_or_else(|| format!("workflow `{workflow_id}` not found (no DAG persisted)"))?;
    let workflow: Workflow = serde_json::from_value(dag)
        .map_err(|e| format!("invalid workflow definition for `{workflow_id}`: {e}"))?;
    let options = CheckpointOptions::default();
    let policy = RecoveryPolicy::default();
    let report = match scope {
        RecoveryScope::Resume => {
            rt.resume_workflow(session_id, workflow, checkpoint_id, options, policy)
                .await
        }
        RecoveryScope::RetryOnly => {
            rt.retry_failed(session_id, workflow, checkpoint_id, options, policy)
                .await
        }
    }
    .map_err(|e| e.to_string())?;
    println!("workflow `{workflow_id}` -> {}", report.status);
    if report.steps.is_empty() {
        println!("  (no steps executed — fully seeded resume)");
    }
    for s in &report.steps {
        let artifact = s.artifact_key.as_deref().unwrap_or("-");
        let error = s.error.as_deref().unwrap_or("");
        println!(
            "  {:<24} {:<12} attempts={:>2}  artifact={:<42} {}",
            s.step, s.status, s.attempts, artifact, error
        );
    }
    Ok(0)
}

use openrt_knowledge::PersistedChunk;

/// Read a document with a hard size cap (inspection/OCR are read-only, but
/// a multi-gigabyte input must fail with a reason, not OOM).
/// Endpoint of the Ollama-compatible vision provider from config.
fn vision_endpoint() -> Result<String, String> {
    let config = effective_config()?;
    Ok(config
        .providers
        .iter()
        .find(|p| p.kind == openrt_config::ProviderKind::Ollama)
        .and_then(|p| p.base_url.clone())
        .unwrap_or_else(|| openrt_vlm::DEFAULT_VLM_ENDPOINT.to_string()))
}

/// Endpoint of the Ollama-compatible embedding provider from config (same
/// provider block the vision endpoint reads from).
fn embed_endpoint() -> Result<String, String> {
    let config = effective_config()?;
    Ok(config
        .providers
        .iter()
        .find(|p| p.kind == openrt_config::ProviderKind::Ollama)
        .and_then(|p| p.base_url.clone())
        .unwrap_or_else(|| openrt_embeddings::DEFAULT_EMBED_ENDPOINT.to_string()))
}

fn vision_model_arg(model: &Option<String>) -> String {
    model
        .clone()
        .unwrap_or_else(|| openrt_vlm::DEFAULT_VLM_MODEL.to_string())
}

/// Phase 13 (Part E): the CLI surface of the ONE model-egress authority.
/// Loopback/local endpoints stay reachable in Sovereign (ADR-003);
/// everything else must pass mode + egress grants before any socket.
async fn model_egress(rt: &Runtime, endpoint: &str) -> Result<(), String> {
    let np = rt.netpol();
    openrt_fabric::net_guard::ensure_url_allowed(
        endpoint,
        openrt_fabric::net_guard::SCOPE_MODEL,
        &rt.modes,
        &np,
    )
    .await
    .map_err(|e| e.to_string())
}

async fn cmd_vision(cli: &Cli, cmd: &VisionCmd) -> Result<i32, String> {
    let endpoint = vision_endpoint()?;
    // Phase 13: every vision arm probes the network — build the runtime and
    // gate the endpoint through the same authority as the provider guard.
    let rt = build_runtime(cli).await?;
    model_egress(&rt, &endpoint).await?;
    match cmd {
        VisionCmd::Capabilities { model } => {
            let model = vision_model_arg(model);
            let status = openrt_vlm::probe_vision(&endpoint, &model).await;
            println!("endpoint:   {endpoint}");
            println!("vision:     {status}");
            match openrt_ingest::capabilities::ocr_capability() {
                Some(c) => println!(
                    "image ocr:  AVAILABLE — {} ({})",
                    c.binary,
                    c.version.as_deref().unwrap_or("version unknown")
                ),
                None => println!("image ocr:  UNAVAILABLE — no tesseract on PATH"),
            }
            match openrt_ingest::capabilities::render_capability() {
                Some(c) => println!(
                    "pdf render: AVAILABLE — {} ({})",
                    c.binary,
                    c.version.as_deref().unwrap_or("version unknown")
                ),
                None => println!("pdf render: UNAVAILABLE — no pdftoppm on PATH"),
            }
            Ok(0)
        }
        VisionCmd::Describe { path, model } => {
            let model = vision_model_arg(model);
            let bytes = read_capped(path)?;
            let answer =
                openrt_vlm::ask_vision(&endpoint, &model, &bytes, openrt_vlm::DESCRIBE_PROMPT)
                    .await
                    .map_err(|e| e.to_string())?;
            println!("endpoint:   {}", answer.endpoint);
            println!("model:      {}", answer.model);
            println!("elapsed:    {}ms", answer.elapsed_ms);
            println!("answer:     {}", answer.text);
            Ok(0)
        }
        VisionCmd::Drawing { path, model } => {
            let model = vision_model_arg(model);
            let bytes = read_capped(path)?;
            let name = path.file_name().and_then(|f| f.to_str());
            let report = openrt_vlm::drawing::analyze(
                &bytes,
                name,
                Some((endpoint.as_str(), model.as_str())),
            )
            .await
            .map_err(|e| e.to_string())?;
            if let (Some(w), Some(h)) = (report.width, report.height) {
                println!("geometry:   {w}x{h} px");
            }
            println!("ocr:        {}", report.ocr_backend);
            println!("regions:    {}", report.regions.len());
            for r in &report.regions {
                println!(
                    "region b{}p{}  bbox=[{:.3},{:.3} {:.3},{:.3}]  conf={}%  \"{}\"",
                    r.block,
                    r.par,
                    r.bbox.left,
                    r.bbox.top,
                    r.bbox.right,
                    r.bbox.bottom,
                    (r.confidence * 100.0).round(),
                    r.text
                );
            }
            match &report.vlm {
                openrt_vlm::drawing::VlmPart::NotRequested => {
                    println!("vlm:        NOT_REQUESTED")
                }
                openrt_vlm::drawing::VlmPart::Answered(a) => {
                    println!("vlm:        AVAILABLE — {} ({}ms)", a.model, a.elapsed_ms);
                    println!("{}", a.text);
                }
                openrt_vlm::drawing::VlmPart::Failed { error } => match error {
                    openrt_vlm::VlmError::Unavailable { reason } => {
                        println!("vlm:        UNAVAILABLE — {reason}")
                    }
                    openrt_vlm::VlmError::ModelMissing {
                        model,
                        reason,
                        available,
                    } => {
                        println!("vlm:        MODEL_MISSING — {model}: {reason}");
                        if !available.is_empty() {
                            println!("installed:  {}", available.join(", "));
                        }
                    }
                    other => println!("vlm:        ERROR — {other}"),
                },
            }
            Ok(0)
        }
    }
}

fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    if meta.len() > 50 * 1024 * 1024 {
        return Err(format!(
            "{}: too large to process ({} bytes > 50 MiB)",
            path.display(),
            meta.len()
        ));
    }
    std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))
}

fn parse_psm(v: u8) -> Result<openrt_ingest::Psm, String> {
    match v {
        3 => Ok(openrt_ingest::Psm::Auto),
        4 => Ok(openrt_ingest::Psm::SingleColumn),
        6 => Ok(openrt_ingest::Psm::SingleBlock),
        11 => Ok(openrt_ingest::Psm::SparseText),
        other => Err(format!(
            "unsupported --psm {other}: use 3 (auto), 4 (column), 6 (block), or 11 (sparse)"
        )),
    }
}

/// Region/line/word tree with the backend's real geometry.
fn print_ocr_tree(page: &openrt_ingest::OcrPage, with_words: bool) {
    let regions = page.regions();
    if regions.is_empty() {
        println!("(no text recognized)");
        return;
    }
    for r in &regions {
        println!(
            "region b{}p{}  bbox=[{:.3},{:.3} {:.3},{:.3}]  conf={:.0}%  \"{}\"",
            r.block,
            r.par,
            r.bbox.left,
            r.bbox.top,
            r.bbox.right,
            r.bbox.bottom,
            r.confidence * 100.0,
            r.text.replace("\n", " / ")
        );
        for l in &r.lines {
            println!("  line  conf={:.0}%  \"{}\"", l.confidence * 100.0, l.text);
            if with_words {
                for &i in &l.words {
                    let w = &page.words[i];
                    println!(
                        "    word conf={:.0}%  bbox=[{:.3},{:.3} {:.3},{:.3}]  \"{}\"",
                        w.confidence * 100.0,
                        w.bbox.left,
                        w.bbox.top,
                        w.bbox.right,
                        w.bbox.bottom,
                        w.text
                    );
                }
            }
        }
    }
}

/// `openrt ingest *` (M9.1): detection is read-only; extraction goes through
/// the jailed corpus intake with honest per-status failures.
async fn cmd_ingest(cli: &Cli, cmd: &IngestCmd) -> Result<i32, String> {
    match cmd {
        IngestCmd::Inspect { path } => {
            let meta = std::fs::metadata(path).map_err(|e| format!("{}: {}", path.display(), e))?;
            if meta.len() > 50 * 1024 * 1024 {
                return Err(format!(
                    "{}: too large to inspect ({} bytes > 50 MiB)",
                    path.display(),
                    meta.len()
                ));
            }
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))?;
            let name = path.file_name().and_then(|f| f.to_str());
            let r = openrt_ingest::detect(&bytes, name);
            println!("path:       {}", path.display());
            println!(
                "format:     {} ({})",
                r.kind.label(),
                format!("{:?}", r.kind).to_lowercase()
            );
            println!("status:     {}", r.status.label().to_uppercase());
            if let Some(reason) = r.status.reason() {
                println!("reason:     {reason}");
            }
            println!(
                "confidence: {}",
                match r.confidence {
                    openrt_ingest::Confidence::High => "high",
                    openrt_ingest::Confidence::Medium => "medium",
                    openrt_ingest::Confidence::None => "none",
                }
            );
            println!("magic:      {}", if r.magic_ok { "yes" } else { "no" });
            println!("size:       {} bytes", r.size_bytes);
            Ok(0)
        }
        IngestCmd::Extract { path, root } => {
            let rt = build_runtime(cli).await?;
            let jail_root = match root {
                Some(r) => r.clone(),
                None => std::env::current_dir().map_err(|e| e.to_string())?,
            };
            let jail =
                openrt_ingest::jail::IngestJail::new(jail_root).map_err(|e| e.to_string())?;
            let outcome = rt
                .corpus
                .ingest_path(&jail, path, &actor_user())
                .await
                .map_err(|e| e.to_string())?;
            println!("state:      {}", outcome.state.label());
            println!("doc_id:     {}", outcome.doc.doc_id);
            println!("version:    {}", outcome.doc.version);
            println!("format:     {}", outcome.doc.format.label());
            println!(
                "modality:   {}",
                format!("{:?}", outcome.doc.modality).to_lowercase()
            );
            println!("pages:      {}", outcome.doc.pages);
            println!("blocks:     {}", outcome.doc.blocks.len());
            println!("extractor:  {}", outcome.doc.extractor);
            if let Some(v) = outcome.duplicate_of {
                println!("duplicate:  existing version {v}");
            }
            Ok(0)
        }
        IngestCmd::Ocr {
            path,
            page,
            psm,
            words,
        } => {
            let psm = parse_psm(*psm)?;
            let bytes = read_capped(path)?;
            let name = path.file_name().and_then(|f| f.to_str());
            let report = openrt_ingest::detect(&bytes, name);
            match report.kind {
                openrt_ingest::FormatKind::Png
                | openrt_ingest::FormatKind::Jpeg
                | openrt_ingest::FormatKind::Tiff => {
                    let t = openrt_ingest::Tesseract::detect().ok_or(
                        "backend `tesseract` unavailable for ocr: no `tesseract` binary on PATH",
                    )?;
                    let ext = match report.kind {
                        openrt_ingest::FormatKind::Png => "png",
                        openrt_ingest::FormatKind::Jpeg => "jpg",
                        _ => "tif",
                    };
                    let page = t
                        .recognize_bytes(&bytes, ext, psm)
                        .map_err(|e| e.to_string())?;
                    println!("backend:    {}", t.label());
                    println!("geometry:   {}x{} px", page.width_px, page.height_px);
                    print_ocr_tree(&page, *words);
                    Ok(0)
                }
                openrt_ingest::FormatKind::Pdf => {
                    let total = openrt_ingest::pdf_page_count(&bytes);
                    let pages: Vec<u32> = match page {
                        Some(p) if *p >= 1 && *p <= total => vec![*p],
                        Some(p) => {
                            return Err(format!("page {p} out of range (pdf has {total} page(s))"))
                        }
                        None => (1..=total).collect(),
                    };
                    let res = openrt_ingest::ocr_pdf_page_data(&bytes, &pages, psm)
                        .map_err(|e| e.to_string())?;
                    println!("backend:    {}", res.label);
                    println!("pages:      {} requested", pages.len());
                    for (n, pg) in &res.pages {
                        println!("--- page {n} ---");
                        print_ocr_tree(pg, *words);
                    }
                    Ok(0)
                }
                other => Err(format!(
                    "`ingest ocr` reads images and pdfs; this content is {} — use `ingest extract`",
                    other.label()
                )),
            }
        }
        IngestCmd::Analyze { path, page } => {
            let bytes = read_capped(path)?;
            let name = path.file_name().and_then(|f| f.to_str());
            let report = openrt_ingest::detect(&bytes, name);
            match report.kind {
                openrt_ingest::FormatKind::Png
                | openrt_ingest::FormatKind::Jpeg
                | openrt_ingest::FormatKind::Tiff => {
                    let t = openrt_ingest::Tesseract::detect().ok_or(
                        "backend `tesseract` unavailable for layout analysis: no `tesseract` binary on PATH",
                    )?;
                    let ext = match report.kind {
                        openrt_ingest::FormatKind::Png => "png",
                        openrt_ingest::FormatKind::Jpeg => "jpg",
                        _ => "tif",
                    };
                    let pg = t
                        .recognize_bytes(&bytes, ext, openrt_ingest::Psm::Auto)
                        .map_err(|e| e.to_string())?;
                    println!("format:     {}", report.kind.label());
                    println!("backend:    {}", t.label());
                    println!(
                        "geometry:   {}x{} px",
                        pg.width_px, pg.height_px
                    );
                    print_ocr_tree(&pg, true);
                    Ok(0)
                }
                openrt_ingest::FormatKind::Pdf => {
                    let total = openrt_ingest::pdf_page_count(&bytes);
                    let pages: Vec<u32> = match page {
                        Some(p) if *p >= 1 && *p <= total => vec![*p],
                        Some(p) => {
                            return Err(format!(
                                "page {p} out of range (pdf has {total} page(s))"
                            ))
                        }
                        None => (1..=total).collect(),
                    };
                    let res = openrt_ingest::ocr_pdf_page_data(
                        &bytes,
                        &pages,
                        openrt_ingest::Psm::Auto,
                    )
                    .map_err(|e| e.to_string())?;
                    println!("format:     {}", report.kind.label());
                    println!("backend:    {}", res.label);
                    for (n, pg) in &res.pages {
                        println!("--- page {n} ---");
                        print_ocr_tree(pg, true);
                    }
                    Ok(0)
                }
                other => Err(format!(
                    "layout geometry requires image/pdf input (real boxes); this content is {} — see `ingest extract` for its blocks",
                    other.label()
                )),
            }
        }
        IngestCmd::List => {
            let rt = build_runtime(cli).await?;
            let list = rt.corpus.list().await.map_err(|e| e.to_string())?;
            if list.is_empty() {
                println!("corpus is empty");
                return Ok(0);
            }
            for m in list {
                println!(
                    "{}  {:<12} {:>10} bytes  {}  ingested {}",
                    m.doc_id,
                    format!("{:?}", m.format).to_lowercase(),
                    m.size_bytes,
                    m.status.label(),
                    m.first_ingested_at
                );
            }
            Ok(0)
        }
        IngestCmd::Show { doc_id, full } => {
            let rt = build_runtime(cli).await?;
            let doc = rt.corpus.inspect(doc_id).await.map_err(|e| e.to_string())?;
            println!("doc_id:     {}", doc.doc_id);
            println!("version:    {}", doc.version);
            println!("format:     {}", doc.format.label());
            println!(
                "modality:   {}",
                format!("{:?}", doc.modality).to_lowercase()
            );
            println!("pages:      {}", doc.pages);
            println!("blocks:     {}", doc.blocks.len());
            println!("extractor:  {}", doc.extractor);
            println!("sha256:     {}", doc.content_sha256);
            if let Some(media) = &doc.media {
                println!(
                    "media:      {}",
                    serde_json::to_string(media).unwrap_or_default()
                );
            }
            println!("---");
            for b in &doc.blocks {
                let page = b
                    .page
                    .map(|p| format!("p{p}"))
                    .unwrap_or_else(|| "-".into());
                if *full {
                    println!(
                        "[{}] {} {} {}",
                        b.key,
                        format!("{:?}", b.kind).to_lowercase(),
                        page,
                        b.source.label()
                    );
                    println!("{}", b.text);
                } else {
                    let mut preview: String = b.text.chars().take(120).collect();
                    if b.text.chars().count() > 120 {
                        preview.push('…');
                    }
                    println!(
                        "[{}] {:<10} {:<4} {:<28} {}",
                        b.key,
                        format!("{:?}", b.kind).to_lowercase(),
                        page,
                        b.source.label(),
                        preview
                    );
                }
            }
            Ok(0)
        }
        IngestCmd::Remove { doc_id } => {
            let rt = build_runtime(cli).await?;
            rt.corpus
                .remove(doc_id, &actor_user())
                .await
                .map_err(|e| e.to_string())?;
            println!("removed {doc_id}");
            Ok(0)
        }
    }
}

/// Persist a chunk into a KB's corpus — fail-closed: the CLI reports the
/// chunk stored ONLY when the oracle's real persist round-trip returned Ok.
async fn cmd_knowledge(cli: &Cli, cmd: &KnowledgeCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        KnowledgeCmd::PersistChunk {
            kb,
            document_id,
            chunk_id,
            text,
        } => {
            let sha = openrt_embeddings::sha256_hex(text.as_bytes());
            let chunk = PersistedChunk {
                chunk_id: chunk_id.clone(),
                document_id: document_id.clone(),
                document_version: 0,
                block_keys: Vec::new(),
                page: None,
                text_sha256: sha,
                text: Some(text.clone()), // real text: enables dense indexing
            };
            openrt_knowledge::persist_chunk(&*rt.store, &kb, &chunk)
                .await
                .map_err(|e| e.to_string())?;
            println!("persisted chunk {kb}.{document_id}.{chunk_id}");
            Ok(0)
        }
        KnowledgeCmd::Retrieve {
            kb,
            query,
            top_k,
            threshold,
        } => {
            let req = openrt_knowledge::RetrieveRequest {
                query_text: query.clone(),
                top_k: *top_k,
                threshold: *threshold,
            };
            let hits = openrt_knowledge::retrieve(&*rt.store, &kb, &req)
                .await
                .map_err(|e| e.to_string())?;
            if hits.is_empty() {
                println!("no verified evidence passed retrieval for {kb}; no hits reported");
            } else {
                for (i, h) in hits.iter().enumerate() {
                    println!(
                        "  {}: chunk={} score={:.4} (revalidated)",
                        i + 1,
                        h.citation.chunk_id,
                        h.score
                    );
                }
            }
            Ok(0)
        }
        KnowledgeCmd::SemanticSearch {
            kb,
            query,
            top_k,
            threshold,
        } => {
            let endpoint = embed_endpoint()?;
            model_egress(&rt, &endpoint).await?;
            let model = openrt_embeddings::DEFAULT_EMBED_MODEL;
            // Probe-first: real embed probe, measured dims — never assume.
            let status = openrt_embeddings::probe_embedding(&endpoint, model);
            println!("endpoint:   {endpoint}");
            println!("embeddings: {status}");
            let dims = match &status {
                openrt_embeddings::EmbeddingStatus::Available { dims, .. } => *dims,
                other => {
                    return Err(format!(
                        "semantic search refused: embeddings backend {} (model {model})",
                        other.label()
                    ));
                }
            };
            let provider: std::sync::Arc<dyn openrt_embeddings::EmbeddingProvider> =
                std::sync::Arc::new(openrt_embeddings::OllamaEmbedder::new(
                    &endpoint, model, dims,
                ));
            let req = openrt_knowledge::RetrieveRequest {
                query_text: query.clone(),
                top_k: *top_k,
                threshold: *threshold,
            };
            let report = openrt_knowledge::semantic_search(
                std::sync::Arc::clone(&rt.store),
                kb,
                provider,
                &req,
            )
            .await
            .map_err(|e| e.to_string())?;
            println!("provider:   {}", report.provider);
            println!(
                "indexed:    {} chunks ({} skipped: no stored text)",
                report.indexed_chunks, report.skipped_no_text
            );
            println!("hits:       {}", report.hits.len());
            for h in &report.hits {
                println!(
                    "  {:.4} doc={} chunk={} page={}",
                    h.score,
                    h.citation.document_id,
                    h.citation.chunk_id,
                    h.citation.page.map_or("-".to_string(), |p| p.to_string())
                );
            }
            Ok(0)
        }
        KnowledgeCmd::HybridSearch {
            kb,
            query,
            top_k,
            threshold,
            document,
            page,
            rerank,
            model,
        } => {
            // Dense half: probe-first. Available -> real neural backend;
            // anything else -> honest lexical-only degradation (never a
            // hash-embedder pretending to be semantic).
            let endpoint = embed_endpoint()?;
            model_egress(&rt, &endpoint).await?;
            let embed_model = openrt_embeddings::DEFAULT_EMBED_MODEL;
            let status = openrt_embeddings::probe_embedding(&endpoint, embed_model);
            println!("endpoint:   {endpoint}");
            println!("embeddings: {status}");
            let dense: Option<std::sync::Arc<dyn openrt_embeddings::EmbeddingProvider>> =
                match &status {
                    openrt_embeddings::EmbeddingStatus::Available { dims, .. } => {
                        Some(std::sync::Arc::new(openrt_embeddings::OllamaEmbedder::new(
                            &endpoint,
                            embed_model,
                            *dims,
                        )))
                    }
                    other => {
                        println!(
                            "dense:      {} (model {embed_model}) - continuing lexical-only",
                            other.label()
                        );
                        None
                    }
                };
            let rerank_mode = match rerank {
                RerankArg::None => openrt_knowledge::RerankMode::Off,
                RerankArg::Deterministic => openrt_knowledge::RerankMode::Deterministic,
                RerankArg::Model => openrt_knowledge::RerankMode::Model,
            };
            let model_pair = match rerank_mode {
                openrt_knowledge::RerankMode::Model => {
                    let id = model
                        .clone()
                        .or_else(|| rt.provider.info().default_model.clone());
                    match id {
                        Some(id) => {
                            println!("rerank-model: {id}");
                            Some((std::sync::Arc::clone(&rt.provider), id))
                        }
                        None => {
                            println!(
                                "rerank:     UNAVAILABLE - no --model given and the provider \
                                 has no default_model (keeping fused order)"
                            );
                            None
                        }
                    }
                }
                _ => None,
            };
            let opts = openrt_knowledge::HybridOptions {
                filter: openrt_knowledge::RetrievalFilter {
                    document_id: document.clone(),
                    page: *page,
                },
                dense,
                rerank: rerank_mode,
                model: model_pair,
                model_options: openrt_knowledge::ModelRerankOptions::default(),
            };
            let req = openrt_knowledge::RetrieveRequest {
                query_text: query.clone(),
                top_k: *top_k,
                threshold: *threshold,
            };
            let report =
                openrt_knowledge::hybrid_search(std::sync::Arc::clone(&rt.store), kb, &req, &opts)
                    .await
                    .map_err(|e| e.to_string())?;
            println!("backend:    {}", report.dense_backend);
            match &report.rerank {
                openrt_knowledge::RerankOutcome::NotRequested => {
                    println!("rerank:     not requested (fused order)")
                }
                openrt_knowledge::RerankOutcome::Applied(k) => {
                    println!("rerank:     applied ({k:?})")
                }
                openrt_knowledge::RerankOutcome::Unavailable { reason } => {
                    println!("rerank:     UNAVAILABLE - {reason} (fused order kept)")
                }
            }
            println!(
                "corpus:     {} textful chunks ({} skipped: no stored text)",
                report.lexical_chunks, report.skipped_no_text
            );
            println!("hits:       {}", report.hits.len());
            for h in &report.hits {
                let bm25 = h.bm25_score.map_or("-".to_string(), |v| format!("{v:.3}"));
                let dense_s = h.dense_score.map_or("-".to_string(), |v| format!("{v:.3}"));
                let rr = h
                    .rerank_score
                    .map_or("-".to_string(), |v| format!("{v:.3}"));
                println!(
                    "  {:.4} [{}] bm25={} dense={} rerank={} doc={} chunk={} page={}",
                    h.fused_score,
                    h.source.as_str(),
                    bm25,
                    dense_s,
                    rr,
                    h.citation.document_id,
                    h.citation.chunk_id,
                    h.citation.page.map_or("-".to_string(), |p| p.to_string())
                );
            }
            Ok(0)
        }
        KnowledgeCmd::Answer {
            kb,
            query,
            top_k,
            threshold,
        } => {
            let oracle = openrt_knowledge::answer(&*rt.store, &kb, &query, *top_k, *threshold)
                .await
                .map_err(|e| e.to_string())?;
            match oracle.answer {
                Some(a) => {
                    println!("{a}");
                    for c in &oracle.verified_citations {
                        println!(
                            "  [verified] doc={} chunk={} page={}",
                            c.document_id,
                            c.chunk_id,
                            c.page.map_or("-".to_string(), |p| p.to_string())
                        );
                    }
                    println!("honesty: {}", oracle.honesty);
                }
                None => {
                    println!("no verified evidence grounded an answer (fail-closed)");
                    println!("honesty: {}", oracle.honesty);
                }
            }
            Ok(0)
        }
        KnowledgeCmd::Graph { kb, action } => match action.as_ref() {
            None | Some(GraphCmd::List) => {
                let entities = openrt_evidence::list_entities(&*rt.store, kb)
                    .await
                    .map_err(|e| e.to_string())?;
                let relations = openrt_evidence::list_relations(&*rt.store, kb)
                    .await
                    .map_err(|e| e.to_string())?;
                println!("entities:  {}", entities.len());
                for e in &entities {
                    let aliases = if e.aliases.is_empty() {
                        String::new()
                    } else {
                        format!(" aliases={}", e.aliases.join(","))
                    };
                    println!(
                        "  {} kind={} label=\"{}\"{}",
                        e.entity_id, e.kind, e.label, aliases
                    );
                }
                println!("relations: {}", relations.len());
                for r in &relations {
                    println!("  {} --{}--> {}", r.subject, r.predicate, r.object);
                }
                Ok(0)
            }
            Some(GraphCmd::UpsertEntity {
                entity_id,
                kind,
                label,
                aliases,
            }) => {
                let e = openrt_evidence::upsert_entity(
                    &*rt.store,
                    kb,
                    entity_id,
                    kind,
                    label,
                    aliases.clone(),
                )
                .await
                .map_err(|e| e.to_string())?;
                println!(
                    "entity:    {} kind={} label=\"{}\" (updated {})",
                    e.entity_id, e.kind, e.label, e.updated_at
                );
                Ok(0)
            }
            Some(GraphCmd::AddRelation {
                subject,
                predicate,
                object,
            }) => {
                let r = openrt_evidence::add_relation(&*rt.store, kb, subject, predicate, object)
                    .await
                    .map_err(|e| e.to_string())?;
                println!("relation:  {} --{}--> {}", r.subject, r.predicate, r.object);
                Ok(0)
            }
        },
        KnowledgeCmd::Evidence { kb, cites, json } => {
            let integrity = openrt_evidence::verify_integrity(&*rt.store, kb)
                .await
                .map_err(|e| e.to_string())?;
            let claims = openrt_evidence::list_claims(&*rt.store, kb)
                .await
                .map_err(|e| e.to_string())?;
            let trust = openrt_evidence::list_trust(&*rt.store, kb)
                .await
                .map_err(|e| e.to_string())?;
            let verdicts = openrt_verifier::list_verdicts(&*rt.store, kb)
                .await
                .map_err(|e| e.to_string())?;
            // Citation resolution: persisted chunk -> citation identity ->
            // evidence-graph edges (fail-closed: an absent chunk NEVER
            // resolves, however the operator spells it).
            let mut resolved: Vec<String> = Vec::new();
            let mut unresolved: Vec<String> = Vec::new();
            for spec in cites {
                let (doc, chunk_id) = parse_cite(spec)?;
                match openrt_knowledge::load_chunk(&*rt.store, kb, &doc, &chunk_id)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    None => unresolved.push(format!("{doc}:{chunk_id} — no persisted chunk")),
                    Some(chunk) => {
                        let cit = citation_of(kb, &chunk);
                        match openrt_evidence::resolve_citation(&*rt.store, kb, &cit).await {
                            Ok(edges) => resolved.push(format!(
                                "{doc}:{chunk_id} — resolves ({} evidence edge(s))",
                                edges.len()
                            )),
                            Err(e) => unresolved.push(format!("{doc}:{chunk_id} — {e}")),
                        }
                    }
                }
            }
            let flagged = trust.iter().filter(|r| !r.categories.is_empty()).count();
            let untrusted = trust
                .iter()
                .filter(|r| matches!(r.trust, openrt_core::TrustLabel::Untrusted))
                .count();
            if *json {
                let doc = serde_json::json!({
                    "integrity": {
                        "intact": integrity.intact(),
                        "claims_checked": integrity.claims_checked,
                        "edges_checked": integrity.edges_checked,
                        "problems": integrity.problems,
                    },
                    "claims": claims.len(),
                    "verdicts": {
                        "records": verdicts.len(),
                    },
                    "trust": {
                        "records": trust.len(),
                        "flagged": flagged,
                        "untrusted": untrusted,
                        "operator_trusted": trust.len() - untrusted,
                    },
                    "resolved": resolved,
                    "unresolved": unresolved,
                });
                println!("{}", serde_json::to_string_pretty(&doc).unwrap());
            } else {
                if integrity.intact() {
                    println!(
                        "integrity:   INTACT ({} claim(s), {} edge(s))",
                        integrity.claims_checked, integrity.edges_checked
                    );
                } else {
                    println!(
                        "integrity:   PROBLEMS ({} claim(s), {} edge(s))",
                        integrity.claims_checked, integrity.edges_checked
                    );
                }
                for p in &integrity.problems {
                    println!("  problem: {p}");
                }
                println!("claims:      {}", claims.len());
                println!("verdicts:    {} recorded (verify.verdicts)", verdicts.len());
                let trusted = trust.len() - untrusted;
                println!(
                    "trust:       {} record(s) — {untrusted} UNTRUSTED, {trusted} operator-trusted, {flagged} flagged",
                    trust.len()
                );
                for r in &trust {
                    let cats: Vec<&str> = r.categories.iter().map(|c| c.as_str()).collect();
                    println!(
                        "  {}:{} trust={} scanned={} flags={}",
                        r.document_id,
                        r.chunk_id,
                        r.trust.as_str(),
                        r.scanned,
                        if cats.is_empty() {
                            "-".to_string()
                        } else {
                            cats.join(",")
                        }
                    );
                }
                for line in &resolved {
                    println!("  [resolved] {line}");
                }
                for line in &unresolved {
                    println!("  [UNRESOLVED] {line}");
                }
            }
            Ok(if integrity.intact() && unresolved.is_empty() {
                0
            } else {
                1
            })
        }
    }
}

/// Parse a `document:chunk` citation spec.
fn parse_cite(spec: &str) -> Result<(String, String), String> {
    match spec.split_once(':') {
        Some((d, c)) if !d.is_empty() && !c.is_empty() => Ok((d.to_string(), c.to_string())),
        _ => Err(format!("--cite expects `document:chunk`, got `{spec}`")),
    }
}

/// Citation identity from a PERSISTED chunk — the sha/page/block identity
/// comes from the store record, never from command-line text.
fn citation_of(kb: &str, chunk: &openrt_knowledge::PersistedChunk) -> openrt_knowledge::Citation {
    openrt_knowledge::Citation {
        kb_id: kb.to_string(),
        document_id: chunk.document_id.clone(),
        document_version: chunk.document_version,
        chunk_id: chunk.chunk_id.clone(),
        block_keys: chunk.block_keys.clone(),
        page: chunk.page,
        text_sha256: chunk.text_sha256.clone(),
    }
}

/// Firewall exit code contract: 0 pass / 2 flag / 3 block (1 stays the
/// generic command-error code).
fn decision_exit(decision: &openrt_verifier::FirewallAction) -> i32 {
    match decision {
        openrt_verifier::FirewallAction::Pass => 0,
        openrt_verifier::FirewallAction::Flag => 2,
        openrt_verifier::FirewallAction::Block => 3,
    }
}

/// Print one firewall report the way an operator re-checks it: decision,
/// basis, independence (honest), citation resolution, model verdicts vs
/// failures, and the reasons.
fn print_firewall_report(report: &openrt_verifier::FirewallReport) {
    println!("decision:    {:?}", report.decision);
    println!("verdict:     {:?}", report.verdict);
    println!("basis:       {:?}", report.basis);
    println!("grounding:   {:?}", report.grounding);
    println!(
        "coverage:    {:.2} ({}/{} claim tokens in evidence)",
        report.deterministic.lexical_coverage,
        report.deterministic.matched_tokens,
        report.deterministic.claim_tokens
    );
    println!(
        "citations:   {}/{} resolved",
        report.deterministic.citations_resolved, report.deterministic.citations_total
    );
    println!("independence: {}", report.independence.describe());
    for v in &report.model_verdicts {
        println!(
            "  model {} -> {:?} ({})",
            v.backend_label(),
            v.verdict,
            v.reason
        );
    }
    for f in &report.model_failures {
        println!("  model {} FAILED: {}", f.backend_label(), f.error);
    }
    for r in &report.reasons {
        println!("reason:      {r}");
    }
    println!("verdict-id:  {}", report.verdict_id);
}

async fn cmd_multimodal(cli: &Cli, cmd: &MultimodalCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        MultimodalCmd::Capabilities => {
            // Modality intake: what THIS build can actually take in.
            let ocr = openrt_ingest::capabilities::ocr_capability();
            let render = openrt_ingest::capabilities::render_capability();
            let probe = openrt_ingest::capabilities::media_probe_capability();
            let decode = openrt_ingest::capabilities::media_decode_capability();
            let asr = openrt_ingest::capabilities::audio_capability();
            println!("modality intake:");
            println!("  text/markdown/csv/json   SUPPORTED    normalized text path (no backend)");
            println!("  pdf (born-digital)       SUPPORTED    text-layer extraction (no backend)");
            match (&render, &ocr) {
                (Some(_), Some(_)) => println!(
                    "  pdf (scanned)            SUPPORTED    pdftoppm render + tesseract OCR"
                ),
                _ => println!(
                    "  pdf (scanned)            UNAVAILABLE  needs pdftoppm AND tesseract (MODEL_MISSING)"
                ),
            }
            println!("  html                     SUPPORTED    normalized HTML path (no backend)");
            println!("  office (docx/xlsx/pptx)  SUPPORTED    OOXML readers (openrt-office)");
            println!(
                "  image (png/jpeg/tiff)    SUPPORTED    header metadata + dimensions; OCR/VLM rows below"
            );
            match (&probe, &decode) {
                (Some(_), Some(_)) => {
                    println!(
                        "  audio                    SUPPORTED    ffprobe/ffmpeg probe + decode"
                    )
                }
                _ => println!("  audio                    UNAVAILABLE  needs ffprobe AND ffmpeg"),
            }
            match decode {
                Some(_) => println!(
                    "  video                    SUPPORTED    frame sampling + audio-track extraction"
                ),
                None => println!("  video                    UNAVAILABLE  needs ffmpeg decode"),
            }
            match asr {
                Some(c) => println!(
                    "  audio -> transcript      SUPPORTED    {} at {}",
                    c.name, c.binary
                ),
                None => println!(
                    "  audio -> transcript      UNAVAILABLE  no whisper engine (MODEL_MISSING)"
                ),
            }
            println!("\nbackends (same probes as `openrt doctor`):");
            let report = rt.doctor().await;
            let phase9 = [
                "document.",
                "vision.",
                "media.",
                "embeddings.",
                "retrieval.",
                "rerank.",
                "knowledge.",
                "evidence.",
                "verifier.",
                "defense.",
            ];
            for check in report
                .checks
                .iter()
                .filter(|c| phase9.iter().any(|p| c.name.starts_with(p)))
            {
                println!(
                    "  {:<10} {:<22} {}",
                    check.status.as_str(),
                    check.name,
                    check.detail
                );
            }
            Ok(0)
        }
    }
}

async fn cmd_verify(cli: &Cli, cmd: &VerifyCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        VerifyCmd::Claim {
            kb,
            claim,
            cites,
            top_k,
            model,
            endpoint,
            json,
        } => {
            // 1. Evidence: explicit persisted citations, else retrieval.
            let mut citations = Vec::new();
            let mut texts: Vec<String> = Vec::new();
            if !cites.is_empty() {
                for spec in cites {
                    let (doc, chunk_id) = parse_cite(spec)?;
                    let chunk = openrt_knowledge::load_chunk(&*rt.store, kb, &doc, &chunk_id)
                        .await
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| {
                            format!(
                                "citation {doc}:{chunk_id} is not persisted in `{kb}` (fail-closed: never verifies against absent evidence)"
                            )
                        })?;
                    let text = chunk.text.clone().ok_or_else(|| {
                        format!("chunk {doc}:{chunk_id} has no stored text — cannot verify")
                    })?;
                    citations.push(citation_of(kb, &chunk));
                    texts.push(text);
                }
            } else {
                let report = openrt_knowledge::hybrid_search(
                    Arc::clone(&rt.store),
                    kb,
                    &openrt_knowledge::RetrieveRequest {
                        query_text: claim.clone(),
                        top_k: *top_k,
                        threshold: 0.0,
                    },
                    &openrt_knowledge::HybridOptions::default(),
                )
                .await
                .map_err(|e| e.to_string())?;
                for hit in report.hits {
                    if let Some(chunk) = openrt_knowledge::load_chunk(
                        &*rt.store,
                        kb,
                        &hit.citation.document_id,
                        &hit.citation.chunk_id,
                    )
                    .await
                    .map_err(|e| e.to_string())?
                    {
                        if let Some(text) = chunk.text {
                            citations.push(hit.citation);
                            texts.push(text);
                        }
                    }
                }
            }

            // 2. M9.9: scan every evidence passage — flags surface BEFORE
            // the verdict (persisted on knowledge.evidence; UNTRUSTED).
            let mut flagged = 0usize;
            let mut categories: Vec<String> = Vec::new();
            for (cit, text) in citations.iter().zip(texts.iter()) {
                let rec = openrt_evidence::scan_evidence(&*rt.store, kb, cit, text)
                    .await
                    .map_err(|e| e.to_string())?;
                if !rec.categories.is_empty() {
                    flagged += 1;
                    for c in &rec.categories {
                        let name = c.as_str().to_string();
                        if !categories.contains(&name) {
                            categories.push(name);
                        }
                    }
                }
            }

            // 3. Fire the production firewall.
            let mut opts = rt.verify_options();
            if !model.is_empty() {
                // Explicit cross-model backends replace the default spec so
                // independence reflects exactly what the operator asked for.
                opts.backends.clear();
                let ep = endpoint
                    .clone()
                    .unwrap_or_else(|| "http://127.0.0.1:11434".to_string());
                #[cfg(feature = "ollama")]
                for (i, m) in model.iter().enumerate() {
                    let p = openrt_models::ollama::OllamaProvider::new(
                        format!("verify-{i}"),
                        ep.clone(),
                        Some(m.clone()),
                    )
                    .map_err(|e| e.to_string())?;
                    opts.backends
                        .push(openrt_verifier::BackendSpec::new(Arc::new(p), m.clone()));
                }
                #[cfg(not(feature = "ollama"))]
                {
                    let _ = (ep,);
                    return Err(
                        "`--model` cross-model verification requires a CLI built with the                          `ollama` feature"
                            .to_string(),
                    );
                }
                #[cfg(not(feature = "ollama"))]
                let _ = model;
            }

            let input = openrt_verifier::VerifyInput {
                kb_id: kb.clone(),
                claim: claim.clone(),
                citations: citations.clone(),
                evidence_texts: texts.clone(),
            };
            let report = rt
                .verify_claim(input, &opts)
                .await
                .map_err(|e| e.to_string())?;

            // 4. Surface injection findings + citation resolution + verdict.
            if *json {
                let doc = serde_json::json!({
                    "injection": { "flagged": flagged, "categories": categories },
                    "report": report,
                });
                println!("{}", serde_json::to_string_pretty(&doc).unwrap());
            } else {
                if flagged > 0 {
                    println!(
                        "injection:   {flagged} evidence passage(s) FLAGGED ({})",
                        categories.join(", ")
                    );
                } else {
                    println!("injection:   clean (0 flagged passages)");
                }
                println!("evidence:    {} passage(s)", citations.len());
                print_firewall_report(&report);
            }
            Ok(decision_exit(&report.decision))
        }

        VerifyCmd::Answer {
            kb,
            answer,
            top_k,
            threshold,
            show_context,
            json,
        } => {
            // 1. M9.9 answer path: retrieval -> per-passage scan (flags
            // persist on knowledge.evidence; UNTRUSTED labels) -> hardened
            // context builder -> chained audit of injection categories.
            let mut req = openrt_runtime::EvidenceContextRequest::new(kb.clone(), answer.clone());
            req.top_k = *top_k;
            req.threshold = *threshold;
            let ctx = rt.evidence_context(req).await.map_err(|e| e.to_string())?;
            if *show_context {
                println!("{}", ctx.context.text);
            }

            let injection = serde_json::json!({
                "flagged": ctx.flagged,
                "categories": ctx.categories.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
                "audit_seq": ctx.audit_seq,
                "included": ctx.context.included,
                "excluded_flagged": ctx.context.excluded_flagged,
            });

            // 2. Firewall over the proposed answer, with the REAL persisted
            // texts behind the retrieved citations (never CLI-supplied text).
            if ctx.citations.is_empty() {
                if *json {
                    let doc = serde_json::json!({
                        "answer": answer,
                        "injection": injection,
                        "blocked": "no verified evidence retrieved for this answer (fail-closed)",
                    });
                    println!("{}", serde_json::to_string_pretty(&doc).unwrap());
                } else {
                    println!("no verified evidence retrieved for this answer (fail-closed)");
                    print_injection(&ctx);
                }
                return Ok(3);
            }
            let mut texts = Vec::with_capacity(ctx.citations.len());
            for cit in &ctx.citations {
                let chunk =
                    openrt_knowledge::load_chunk(&*rt.store, kb, &cit.document_id, &cit.chunk_id)
                        .await
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| {
                            format!(
                                "retrieved citation {}/{} vanished from the store (fail-closed)",
                                cit.document_id, cit.chunk_id
                            )
                        })?;
                texts.push(chunk.text.unwrap_or_default());
            }
            let input = openrt_verifier::VerifyInput {
                kb_id: kb.clone(),
                claim: answer.clone(),
                citations: ctx.citations.clone(),
                evidence_texts: texts,
            };
            let report = rt
                .verify_claim(input, &rt.verify_options())
                .await
                .map_err(|e| e.to_string())?;

            if *json {
                let doc = serde_json::json!({
                    "answer": answer,
                    "injection": injection,
                    "report": report,
                });
                println!("{}", serde_json::to_string_pretty(&doc).unwrap());
            } else {
                println!("answer:      {answer}");
                print_injection(&ctx);
                print_firewall_report(&report);
            }
            Ok(decision_exit(&report.decision))
        }
    }
}

fn print_injection(ctx: &openrt_runtime::EvidenceContextReport) {
    if ctx.flagged > 0 {
        let cats: Vec<&str> = ctx.categories.iter().map(|c| c.as_str()).collect();
        println!(
            "injection:   {} passage(s) FLAGGED ({})",
            ctx.flagged,
            cats.join(", ")
        );
        match ctx.audit_seq {
            Some(seq) => {
                println!("audit:       evidence.injection_flagged seq={seq} (ADR-002 chained)")
            }
            None => println!("audit:       NOT RECORDED (unexpected for flagged evidence)"),
        }
    } else {
        println!("injection:   clean (0 flagged passages; all evidence still UNTRUSTED-labeled)");
    }
}

async fn cmd_history(cli: &Cli, cmd: &HistoryCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        HistoryCmd::Verify => {
            let n = rt.history_verify().await.map_err(|e| e.to_string())?;
            println!("chain verified: {n} record(s)");
            Ok(0)
        }
        HistoryCmd::Tail => {
            let n = rt.history_tail().await.map_err(|e| e.to_string())?;
            println!("tail={n}");
            Ok(0)
        }
        HistoryCmd::Replay { as_of, json } => {
            let rows = rt.history_replay(*as_of).await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                for r in &rows {
                    println!(
                        "{:>6}  {:<38}  {:<22}  {:<10}  {}",
                        r.seq, r.session_id, r.event_kind, r.status, r.note
                    );
                }
                println!("{} row(s) as-of seq {as_of}", rows.len());
            }
            Ok(0)
        }
        HistoryCmd::Fork { branch, base } => {
            rt.history_fork(branch, base)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "forked branch `{branch}` from base `{}`",
                if base.is_empty() { "head" } else { base }
            );
            Ok(0)
        }
        HistoryCmd::ForkCommit {
            branch,
            node,
            status,
            event,
            note,
        } => {
            let seq = rt
                .history_fork_commit(branch, node, status, event, note)
                .await
                .map_err(|e| e.to_string())?;
            println!("committed `{node}` on branch `{branch}` (fork seq {seq})");
            Ok(0)
        }
        HistoryCmd::ForkVerify { branch } => {
            let n = rt
                .history_fork_verify(branch)
                .await
                .map_err(|e| e.to_string())?;
            println!("fork chain verified: {n} record(s) on branch `{branch}`");
            Ok(0)
        }
        HistoryCmd::Compare { branch } => {
            let c = rt
                .history_branch_compare(branch)
                .await
                .map_err(|e| e.to_string())?;
            let diverged = c.entries.iter().filter(|e| e.diverged).count();
            println!(
                "branch `{}` base={} shared_prefix={} parent_tail={} fork_verified={} diverged_row(s)={}",
                c.branch, c.base_seq, c.shared_prefix_len, c.parent_tail_seq,
                c.fork_records_verified, diverged
            );
            Ok(0)
        }
        HistoryCmd::Return => {
            let view = rt
                .history_return_to_current()
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "returned to current head (tail={} {} fork(s))",
                view.tail_seq,
                view.forks.len()
            );
            Ok(0)
        }
        HistoryCmd::List { limit, json } => {
            let tail = rt.history_tail().await.map_err(|e| e.to_string())?;
            let as_of = tail.min(*limit);
            let rows = rt.history_replay(as_of).await.map_err(|e| e.to_string())?;
            let start = rows.len().saturating_sub(*limit as usize);
            let slice = &rows[start..];
            if *json {
                println!("{}", serde_json::to_string_pretty(slice).unwrap());
            } else {
                for r in slice {
                    println!(
                        "{:>6}  {:<38}  {:<22}  {:<10}  {}",
                        r.seq, r.session_id, r.event_kind, r.status, r.note
                    );
                }
                println!("{} row(s) (tail={tail})", slice.len());
            }
            Ok(0)
        }
        HistoryCmd::Timeline {
            session,
            from_seq,
            limit,
            json,
        } => {
            let opts = openrt_runtime::timemachine::TimelineOptions {
                session: session.clone(),
                from_seq: *from_seq,
                limit: *limit,
            };
            let timeline = rt
                .history_timeline(&opts)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&timeline).unwrap());
            } else {
                for r in &timeline.records {
                    println!(
                        "{:>6}  {:<38}  {:<22}  {:<10}  {}",
                        r.seq, r.session_id, r.event_kind, r.status, r.note
                    );
                }
                println!(
                    "{} row(s) (tail={})",
                    timeline.records.len(),
                    timeline.tail_seq
                );
            }
            Ok(0)
        }
    }
}

// ── Phase 10 CLI surfaces (M10.4–M10.9) ─────────────────────────────────────

async fn cmd_recovery(cli: &Cli, cmd: &RecoveryCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        RecoveryCmd::Scan => {
            rt.boot_recover().await.map_err(|e| e.to_string())?;
            println!("recovery scan complete — evidence record written");
            Ok(0)
        }
        RecoveryCmd::List { json } => {
            let records = rt.recovery_records().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&records).unwrap());
            } else {
                for r in &records {
                    println!(
                        "{}  boot={}  findings={}",
                        r.record_id,
                        r.boot_id,
                        r.total_findings()
                    );
                }
                println!("{} record(s)", records.len());
            }
            Ok(0)
        }
        RecoveryCmd::Inspect { record, json } => {
            let records = rt.recovery_records().await.map_err(|e| e.to_string())?;
            let found = records
                .iter()
                .find(|r| &r.record_id == record || &r.boot_id == record)
                .ok_or_else(|| format!("no recovery record matches `{record}`"))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(found).unwrap());
            } else {
                println!("record:      {}", found.record_id);
                println!("boot:        {}", found.boot_id);
                println!("boot at:     {}", found.boot_at);
                println!("findings:    {}", found.total_findings());
                println!("interrupted tasks:  {}", found.interrupted_tasks.len());
                for t in &found.interrupted_tasks {
                    println!("  - {} ({:?})", t.task_id, t.previous_status);
                }
                println!("incomplete runs:    {}", found.incomplete_runs.len());
                for r in &found.incomplete_runs {
                    println!("  - {}", r.workflow_id);
                }
                println!("stale sandboxes:    {}", found.stale_sandboxes.len());
                for s in &found.stale_sandboxes {
                    println!("  - {}", s.sandbox_id);
                }
            }
            Ok(0)
        }
        RecoveryCmd::Restore { checkpoint, json } => {
            let id = parse_checkpoint_id(checkpoint)?;
            let cp = rt.load_checkpoint(&id).await.map_err(|e| e.to_string())?;
            // `load_checkpoint` already ran schema + digest validation: a
            // corrupt record never reaches this point.
            let current_fp = rt.policy_fingerprint();
            let compat = match cp.refs.policy_version.as_deref() {
                Some(fp) if fp == current_fp => "compatible",
                Some(_) => "mismatched — `recovery rollback` requires --ack-policy-drift",
                None => "unknown — `recovery rollback` requires --ack-policy-drift",
            };
            let ops = openrt_checkpoint::rollback::plan_rollbacks(&cp);
            if *json {
                let ops: Vec<String> = ops.iter().map(|o| rollback_op_brief(o)).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "checkpoint": cp.checkpoint_id.to_string(),
                        "session": cp.session_id.to_string(),
                        "task": cp.task_id.as_ref().map(|t| t.to_string()),
                        "integrity": "verified",
                        "policy_compat": compat,
                        "planned_ops": ops,
                    }))
                    .unwrap()
                );
            } else {
                println!("checkpoint:  {}", cp.checkpoint_id);
                println!("session:     {}", cp.session_id);
                println!(
                    "task:        {}",
                    cp.task_id
                        .as_ref()
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "-".into())
                );
                println!("integrity:   verified (schema + digest)");
                println!("policy:      {compat}");
                println!("planned ops ({}):", ops.len());
                for op in &ops {
                    println!("  - {}", rollback_op_brief(op));
                }
                println!("read-only preview — apply with `recovery rollback`");
            }
            Ok(0)
        }
        RecoveryCmd::Rollback {
            checkpoint,
            operator,
            ack_policy_drift,
            workspace_root,
            json,
        } => {
            let id = parse_checkpoint_id(checkpoint)?;
            let auth = openrt_checkpoint::RollbackAuthorization {
                operator: operator.clone(),
                acknowledge_policy_drift: *ack_policy_drift,
            };
            let report = rt
                .rollback_checkpoint(&id, &auth, workspace_root.clone())
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!("rollback of {} by {}", report.checkpoint_id, operator);
                if let Some(compat) = report.policy_compat {
                    println!("policy:      {compat:?}");
                }
                if let Some(true) = report.history_immutable {
                    println!("history:     ledger immutable (prefix unchanged)");
                }
                for r in &report.results {
                    println!("  [{:?}] {}", r.status, rollback_op_brief(&r.op));
                    println!("         {}", r.detail);
                }
            }
            let incomplete = report
                .results
                .iter()
                .filter(|r| !matches!(r.status, openrt_checkpoint::RollbackStatus::Done))
                .count();
            if incomplete > 0 {
                Err(format!(
                    "{incomplete} rollback op(s) not completed — partial restore is not success"
                ))
            } else {
                Ok(0)
            }
        }
        RecoveryCmd::Resume { task, json } => {
            let tid: openrt_core::TaskId = task
                .parse()
                .map_err(|e| format!("invalid task id `{task}`: {e}"))?;
            let t = rt.task_recover(tid).await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&t).unwrap());
            } else {
                println!("task {} resumed: status={:?}", t.id, t.status);
            }
            Ok(0)
        }
    }
}

fn rollback_op_brief(op: &openrt_checkpoint::RollbackOp) -> String {
    match op {
        openrt_checkpoint::RollbackOp::FileRestore { rel_path, .. } => {
            format!("file-restore `{rel_path}`")
        }
        openrt_checkpoint::RollbackOp::BlackboardRestore { keys } => {
            format!("blackboard-restore {} key(s)", keys.len())
        }
        openrt_checkpoint::RollbackOp::RunMetadataRestore => "run-metadata-restore".to_string(),
    }
}

async fn cmd_shield(cli: &Cli, cmd: &ShieldCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        ShieldCmd::Scan { tool, args, json } => {
            let value: serde_json::Value =
                serde_json::from_str(args).map_err(|e| format!("--args is not valid JSON: {e}"))?;
            let report = rt
                .shield_scan(tool, value)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!(
                    "action: {}  severity: {}",
                    report.action.as_str(),
                    report.highest_severity.map(|s| s.as_str()).unwrap_or("-")
                );
                for f in &report.findings {
                    println!(
                        "  [{}|{}] {} — {}",
                        f.severity.as_str(),
                        f.category.as_str(),
                        f.source,
                        f.reason
                    );
                }
                if report.is_clean() {
                    println!("clean — no findings");
                }
                println!("(advisory scan — nothing executed, nothing recorded)");
            }
            Ok(0)
        }
        ShieldCmd::Findings { status, json } => {
            let status = match status {
                Some(s) => Some(parse_finding_status(s)?),
                None => None,
            };
            let findings = openrt_shield::list_findings(&*rt.store, status)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&findings).unwrap());
            } else {
                for f in &findings {
                    println!(
                        "{}  [{}]  {}  {}",
                        f.id,
                        f.status.as_str(),
                        f.severity.as_str(),
                        f.reason
                    );
                }
                println!("{} finding(s)", findings.len());
            }
            Ok(0)
        }
        ShieldCmd::Inspect { finding, json } => {
            let f = openrt_shield::get_finding(&*rt.store, finding)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no finding `{finding}`"))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&f).unwrap());
            } else {
                println!("id:        {}", f.id);
                println!("category:  {}", f.category.as_str());
                println!("severity:  {}", f.severity.as_str());
                println!("action:    {}", f.recommended_action.as_str());
                println!("status:    {}", f.status.as_str());
                println!("source:    {}", f.source);
                println!("target:    {}", f.target);
                println!("reason:    {}", f.reason);
                println!("evidence:  {}", f.evidence);
                println!("created:   {}", f.created_at);
            }
            Ok(0)
        }
    }
}

fn parse_finding_status(s: &str) -> Result<openrt_shield::FindingStatus, String> {
    use openrt_shield::FindingStatus as S;
    match s {
        "open" => Ok(S::Open),
        "warned" => Ok(S::Warned),
        "approval_required" => Ok(S::ApprovalRequired),
        "denied" => Ok(S::Denied),
        "halted" => Ok(S::Halted),
        "reported" => Ok(S::Reported),
        other => Err(format!(
            "unknown status `{other}` (open|warned|approval_required|denied|halted|reported)"
        )),
    }
}

async fn cmd_skill(cli: &Cli, cmd: &SkillCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        SkillCmd::List { json } => {
            let assets = rt.load_assets().await.map_err(|e| e.to_string())?;
            let states = assets.skill_states_all();
            if *json {
                println!("{}", serde_json::to_string_pretty(&states).unwrap());
            } else {
                for (id, version, activation) in &states {
                    println!("{id}@{version}  {activation}");
                }
                println!("{} skill version(s)", states.len());
            }
            Ok(0)
        }
        SkillCmd::Inspect { id, version, json } => {
            let report = rt
                .skill_inspect(id, *version)
                .await
                .map_err(|e| e.to_string())?;
            print_skill_report(&report, *json);
            Ok(if report.passed { 0 } else { 1 })
        }
        SkillCmd::Load { id, version, json } => {
            let report = rt
                .skill_load(id, *version)
                .await
                .map_err(|e| e.to_string())?;
            print_skill_report(&report, *json);
            Ok(0)
        }
        SkillCmd::Execute {
            id,
            version,
            session,
        } => {
            let sid = match session {
                Some(s) => parse_session_id(s)?,
                None => {
                    rt.create_session("cli-skill")
                        .await
                        .map_err(|e| e.to_string())?
                        .id
                }
            };
            let run = rt
                .skill_execute(sid, id, *version)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "skill {}@{} executed (mode={})",
                run.id, run.version, run.mode
            );
            println!("effective permissions: {:?}", run.effective_permissions);
            if let Some(wf) = &run.workflow_report {
                println!(
                    "workflow: {} status={:?} step(s)={}",
                    wf.workflow_id,
                    wf.status,
                    wf.steps.len()
                );
            }
            if let Some(task) = &run.task {
                println!("task: {} status={:?}", task.id, task.status);
            }
            Ok(0)
        }
        SkillCmd::Compile { id, version } => {
            let workflow = rt
                .skill_compile(id, *version)
                .await
                .map_err(|e| e.to_string())?;
            println!("skill {id} compiled → workflow `{workflow}`");
            Ok(0)
        }
        SkillCmd::Test { id, version, json } => {
            let report = rt
                .skill_test_run(id, *version)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                for r in &report.results {
                    println!(
                        "{}  {}  {}",
                        if r.ok { "PASS" } else { "FAIL" },
                        r.name,
                        r.detail
                    );
                }
                println!(
                    "{}: {} ({} test(s))",
                    report.skill_id,
                    if report.passed { "passed" } else { "FAILED" },
                    report.results.len()
                );
            }
            Ok(if report.passed { 0 } else { 1 })
        }
        SkillCmd::Marketplace { cmd } => match cmd {
            MarketplaceCmd::List { json } => {
                let entries = rt
                    .skill_marketplace_catalog()
                    .await
                    .map_err(|e| e.to_string())?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&entries).unwrap());
                } else {
                    for e in &entries {
                        println!(
                            "{}@{}  {}  [{}] ({})",
                            e.id, e.version, e.name, e.activation, e.source
                        );
                    }
                    println!("{} catalog entr(y/ies)", entries.len());
                }
                Ok(0)
            }
            MarketplaceCmd::Fetch { url } => match rt.skill_marketplace_fetch(url).await {
                Ok(entries) => {
                    for e in &entries {
                        println!("{}@{}  {}  [{}]", e.id, e.version, e.name, e.source);
                    }
                    println!("{} entr(y/ies) fetched", entries.len());
                    Ok(0)
                }
                Err(e) => {
                    println!("UNAVAILABLE: {e}");
                    Ok(1)
                }
            },
        },
    }
}

fn print_skill_report(report: &openrt_runtime::SkillLoadReport, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(report).unwrap());
        return;
    }
    println!(
        "skill {}@{} (digest {})",
        report.id, report.version, report.digest
    );
    println!("activation: {}", report.activation);
    println!("trust:      {}", report.trust);
    println!("passed:     {}", report.passed);
    println!("effective permissions: {:?}", report.effective_permissions);
    if !report.missing_tools.is_empty() {
        println!("missing tools: {:?}", report.missing_tools);
    }
    for c in &report.checks {
        println!(
            "  [{}] {} — {}",
            if c.ok { "ok" } else { "FAIL" },
            c.name,
            c.detail
        );
    }
}

async fn cmd_extension(cli: &Cli, cmd: &ExtensionCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        ExtensionCmd::List { json } => {
            let states = rt.extension_states().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&states).unwrap());
            } else {
                for s in &states {
                    println!(
                        "{}@{}  {}  {}",
                        s["id"].as_str().unwrap_or("?"),
                        s["version"],
                        s["activation"].as_str().unwrap_or("?"),
                        s["security"].as_str().unwrap_or("?"),
                    );
                }
                println!("{} extension version(s)", states.len());
            }
            Ok(0)
        }
        ExtensionCmd::Inspect { id, version } => {
            let ext = rt
                .extension_inspect(id, *version)
                .await
                .map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&ext).unwrap());
            Ok(0)
        }
        ExtensionCmd::Install { path } => {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let pkg: openrt_workspace_assets::ExtensionPackage =
                serde_json::from_str(&text).map_err(|e| e.to_string())?;
            let (eid, ever) = (pkg.extension.id.0.clone(), pkg.extension.version.0);
            let skills = rt.extension_install(pkg).await.map_err(|e| e.to_string())?;
            println!("extension {eid}@{ever} installed → Registered/unscanned");
            if !skills.is_empty() {
                println!("payload skill(s): {}", skills.join(", "));
            }
            println!(
                "next: `openrt extension validate {eid}` then `scan` + `activate --operator <op>`"
            );
            Ok(0)
        }
        ExtensionCmd::Validate { id, version } => {
            let v = rt
                .extension_validate(id, *version)
                .await
                .map_err(|e| e.to_string())?;
            println!("extension {id}@{v} validated");
            Ok(0)
        }
        ExtensionCmd::Scan { id, version, json } => {
            let report = rt
                .extension_scan(id, *version)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!("scan: {}", report.action.as_str());
                if report.findings.is_empty() {
                    println!("  no findings");
                }
                for f in &report.findings {
                    println!(
                        "  [{}] {} → {}: {}",
                        f.severity.as_str(),
                        f.category.as_str(),
                        f.recommended_action.as_str(),
                        f.reason
                    );
                }
            }
            Ok(match report.action {
                openrt_shield::ShieldAction::Allow | openrt_shield::ShieldAction::Warn => 0,
                openrt_shield::ShieldAction::RequireApproval => 2,
                openrt_shield::ShieldAction::Deny | openrt_shield::ShieldAction::Halt => 3,
            })
        }
        ExtensionCmd::Activate {
            id,
            version,
            operator,
        } => {
            let v = rt
                .extension_activate(id, *version, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("extension {id}@{v} active (tool/MCP bindings bind on the next build)");
            Ok(0)
        }
        ExtensionCmd::Deactivate { id, operator } => {
            let v = rt
                .extension_deactivate(id, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("extension {id}@{v} inactive (bindings unbind on the next build)");
            Ok(0)
        }
        ExtensionCmd::Invalidate { id, operator } => {
            let v = rt
                .extension_invalidate(id, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("extension {id}@{v} invalidated");
            Ok(0)
        }
        ExtensionCmd::Rollback {
            id,
            target,
            operator,
        } => {
            let (from, to) = rt
                .extension_rollback(id, *target, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("extension {id} rolled back {from} → {to}");
            Ok(0)
        }
        ExtensionCmd::Remove { id, operator } => {
            let removed = rt
                .extension_remove(id, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("extension {id} removed ({} version(s))", removed.len());
            Ok(0)
        }
    }
}

fn to_pretty(v: &serde_json::Value) -> Result<String, String> {
    serde_json::to_string_pretty(v).map_err(|e| e.to_string())
}

async fn cmd_mcp(cli: &Cli, cmd: &McpCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        McpCmd::List { json } => {
            let states = rt.mcp_states().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&states)?);
            } else {
                let summary = &states["summary"];
                let count = states["servers"].as_array().map(Vec::len).unwrap_or(0);
                println!(
                    "{count} mcp server record(s): {} enabled, {} connectable, {} review, \
                     {} blocked, {} flagged",
                    summary["enabled"],
                    summary["connectable"],
                    summary["review"],
                    summary["blocked"],
                    summary["flagged"],
                );
                if let Some(servers) = states["servers"].as_array() {
                    for s in servers {
                        println!(
                            "  {:<20} trust={:<9} {:<8} sec={:<9} tools={} src={}",
                            s["name"].as_str().unwrap_or("?"),
                            s["trust"].as_str().unwrap_or("?"),
                            if s["enabled"] == serde_json::json!(true) {
                                "enabled"
                            } else {
                                "disabled"
                            },
                            s["security_status"].as_str().unwrap_or("?"),
                            s["discovered_tool_count"],
                            s["declared_by"].as_str().unwrap_or("?"),
                        );
                    }
                }
            }
            Ok(0)
        }
        McpCmd::Inspect { name, json } => {
            let value = rt.mcp_inspect(name).await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&value)?);
            } else {
                println!("{}", to_pretty(&value)?);
            }
            Ok(0)
        }
        McpCmd::Register {
            name,
            command,
            args,
            env_keys,
            capabilities,
            operator,
        } => {
            rt.mcp_register(
                name,
                command,
                args.clone(),
                env_keys.clone(),
                capabilities.clone(),
                operator,
            )
            .await
            .map_err(|e| e.to_string())?;
            println!("mcp server `{name}` registered → unscanned / disabled / untrusted");
            println!(
                "next: `openrt mcp scan {name}` → `openrt mcp enable {name} --operator <op>` \
                 → `openrt mcp trust {name} trusted --operator <op>` (clean scan required)"
            );
            Ok(0)
        }
        McpCmd::Enable { name, operator } => {
            rt.mcp_enable(name, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("mcp server `{name}` enabled (scan clean; connects on the next build)");
            Ok(0)
        }
        McpCmd::Disable { name, operator } => {
            rt.mcp_disable(name, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("mcp server `{name}` disabled (stops connecting on the next build)");
            Ok(0)
        }
        McpCmd::Trust {
            name,
            level,
            operator,
        } => {
            rt.mcp_set_trust(name, level, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("mcp server `{name}` trust = {level}");
            println!(
                "trust never bypasses policy: every call still requires \
                 mcp.invoke.all + mcp.{name}.invoke at the gate"
            );
            Ok(0)
        }
        McpCmd::Scan { name, json } => {
            let report = rt.mcp_scan(name).await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&serde_json::to_value(&report).unwrap())?);
            } else {
                println!("scan: {}", report.action.as_str());
                if report.findings.is_empty() {
                    println!("  no findings");
                }
                for f in &report.findings {
                    println!(
                        "  [{}] {} → {}: {}",
                        f.severity.as_str(),
                        f.category.as_str(),
                        f.recommended_action.as_str(),
                        f.reason
                    );
                }
            }
            Ok(match report.action {
                openrt_shield::ShieldAction::Allow | openrt_shield::ShieldAction::Warn => 0,
                openrt_shield::ShieldAction::RequireApproval => 2,
                openrt_shield::ShieldAction::Deny | openrt_shield::ShieldAction::Halt => 3,
            })
        }
        McpCmd::Health { name, json } => {
            let probe = rt.mcp_health(name).await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", to_pretty(&probe)?);
            } else {
                let p = &probe["probe"];
                if p["ok"] == serde_json::json!(true) {
                    println!(
                        "health: ok — {} ({} tool(s) discovered)",
                        p["detail"].as_str().unwrap_or(""),
                        p["tool_count"],
                    );
                } else {
                    println!(
                        "health: FAIL — {} ({} tool(s))",
                        p["detail"].as_str().unwrap_or(""),
                        p["tool_count"],
                    );
                }
            }
            Ok(if probe["probe"]["ok"] == serde_json::json!(true) {
                0
            } else {
                1
            })
        }
    }
}

async fn cmd_instinct(cli: &Cli, cmd: &InstinctCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    if let InstinctCmd::Generate { session } = cmd {
        let id = rt
            .instinct_submit_from_history(session)
            .await
            .map_err(|e| e.to_string())?;
        println!("generated instinct `{id}` from session `{session}` (state=generated)");
        return Ok(0);
    }
    match cmd {
        InstinctCmd::Validate { id } => {
            rt.assets_validate_instinct(id)
                .await
                .map_err(|e| e.to_string())?;
            println!("instinct `{id}` validated (Registered → Validated)");
            return Ok(0);
        }
        InstinctCmd::Activate { id, operator } => {
            rt.assets_activate_instinct(id, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("instinct `{id}` active under operator `{operator}`");
            return Ok(0);
        }
        _ => {}
    }
    let assets = rt.load_assets().await.map_err(|e| e.to_string())?;
    let instincts = assets.instincts();
    match cmd {
        InstinctCmd::List { json } => {
            if *json {
                let rows: Vec<&openrt_workspace_assets::Instinct> =
                    instincts.iter().map(|(_, i)| *i).collect();
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                for (id, i) in &instincts {
                    println!(
                        "{}  conf={:.2}  scope={}  {}",
                        id.0, i.confidence, i.scope, i.trigger
                    );
                }
                println!("{} instinct(s)", instincts.len());
            }
            Ok(0)
        }
        InstinctCmd::Generate { .. }
        | InstinctCmd::Validate { .. }
        | InstinctCmd::Activate { .. } => unreachable!("handled above"),
        InstinctCmd::Inspect { id, json } => {
            let found = instincts
                .iter()
                .find(|(iid, _)| &iid.0 == id)
                .ok_or_else(|| format!("no instinct `{id}`"))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(found.1).unwrap());
            } else {
                println!("id:      {}", found.0 .0);
                println!("scope:   {}", found.1.scope);
                println!("trigger: {}", found.1.trigger);
                println!("confidence: {:.2}", found.1.confidence);
                println!("recommendation: {}", found.1.recommendation);
                println!("evidence: {:?}", found.1.evidence);
            }
            Ok(0)
        }
    }
}

async fn cmd_genome(cli: &Cli, cmd: &GenomeCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    let assets = rt.load_assets().await.map_err(|e| e.to_string())?;
    match cmd {
        GenomeCmd::List { json } => {
            let genomes = assets.genomes();
            if *json {
                let rows: Vec<&openrt_workspace_assets::AgentGenome> =
                    genomes.iter().map(|(_, g)| *g).collect();
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                for (id, g) in &genomes {
                    println!(
                        "{} v{}  model={}  tools={}  permissions={}",
                        id.0,
                        g.version,
                        g.model.as_deref().unwrap_or("-"),
                        g.tools.len(),
                        g.permissions.len()
                    );
                }
                println!("{} genome(s)", genomes.len());
            }
            Ok(0)
        }
        GenomeCmd::Inspect { id, json } => {
            let genome = assets
                .get_genome(&openrt_workspace_assets::GenomeId(id.clone()))
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(genome).unwrap());
            } else {
                println!("id:      {}", id);
                println!("version: {}", genome.version);
                println!("model:   {}", genome.model.as_deref().unwrap_or("-"));
                println!("tools:   {:?}", genome.tools);
                println!("permissions: {:?}", genome.permissions);
                println!("verifier: {}", genome.verifier.as_deref().unwrap_or("-"));
                println!("provenance: {:?}", genome.provenance);
            }
            Ok(0)
        }
        GenomeCmd::Evolve {
            id,
            summary,
            proposal,
        } => {
            let proposal_id = proposal.clone().unwrap_or_else(|| {
                format!(
                    "genome-evolve-{}-{}",
                    id,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                )
            });
            rt.genome_evolve_propose(&proposal_id, id, summary)
                .await
                .map_err(|e| e.to_string())?;
            println!("proposal {proposal_id} created for genome `{id}`");
            println!(
                "approval required — nothing applied until `improve approve` + `improve apply`"
            );
            Ok(0)
        }
    }
}

async fn cmd_memory(cli: &Cli, cmd: &MemoryCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        MemoryCmd::Origin {
            origin_run,
            canonical_origin,
        } => {
            rt.memory_remember_origin(origin_run, canonical_origin)
                .await
                .map_err(|e| e.to_string())?;
            println!("registered origin `{origin_run}`");
            Ok(0)
        }
        MemoryCmd::Remember {
            id,
            origin_run,
            origin_source,
            scope,
            key,
            value,
            occurred_at,
            r#override,
        } => {
            let item = rt
                .memory_remember(
                    id,
                    origin_run,
                    origin_source,
                    scope,
                    key,
                    value,
                    occurred_at,
                    *r#override,
                )
                .await
                .map_err(|e| e.to_string())?;
            item.reopen().map_err(|e| e.to_string())?;
            println!(
                "remembered `{}` digest={} origin={}",
                item.id, item.digest, item.citation.origin_run
            );
            Ok(0)
        }
        MemoryCmd::Recall {
            query,
            top_k,
            threshold,
            json,
        } => {
            let set = rt
                .memory_recall(query, *top_k, *threshold)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&set.items).unwrap());
            } else if set.items.is_empty() {
                println!("no verified evidence (fail-closed; empty citation report)");
            } else {
                for item in &set.items {
                    println!(
                        "  [verified] {}  {}={}  origin={}  digest={}",
                        item.id, item.key, item.value, item.citation.origin_run, item.digest
                    );
                }
                println!("{} item(s) with re-validated citations", set.items.len());
            }
            Ok(0)
        }
        MemoryCmd::Episode {
            run_id,
            session_id,
            category,
            occurred_at,
            payload,
            origin_run,
            origin_source,
            r#override,
        } => {
            let rec = rt
                .memory_remember_episode(
                    run_id,
                    session_id,
                    category,
                    occurred_at,
                    payload,
                    origin_run,
                    origin_source,
                    *r#override,
                )
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "episode digest={} origin={}",
                rec.digest, rec.citation.origin_run
            );
            Ok(0)
        }
        MemoryCmd::RecallEpisode { digest, json } => {
            let rec = rt
                .memory_recall_episode(digest)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rec).unwrap());
            } else {
                println!(
                    "run={} session={} category={} payload={}",
                    rec.run_id, rec.session_id, rec.category, rec.payload
                );
                println!(
                    "citation origin={} source={} digest={}",
                    rec.citation.origin_run, rec.citation.origin_source, rec.digest
                );
            }
            Ok(0)
        }
        MemoryCmd::List { json } => {
            let set = openrt_memory::load_verified_semantic(&*rt.store)
                .await
                .map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&set.items).unwrap());
            } else if set.items.is_empty() {
                println!("no verified semantic items (fail-closed)");
            } else {
                for item in &set.items {
                    println!(
                        "  [verified] {}  {}={}  origin={}",
                        item.id, item.key, item.value, item.citation.origin_run
                    );
                }
                println!("{} item(s)", set.items.len());
            }
            Ok(0)
        }
    }
}

async fn cmd_assets(cli: &Cli, cmd: &AssetsCmd) -> Result<i32, String> {
    use openrt_workspace_assets::{Provenance, Skill, SkillId, SkillVersion};
    let rt = build_runtime(cli).await?;
    match cmd {
        AssetsCmd::List { json } => {
            let states = rt.assets_skill_states().await.map_err(|e| e.to_string())?;
            let assets = rt.load_assets().await.map_err(|e| e.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "skills": states,
                        "hooks": assets.hook_ids(),
                        "rules": assets.rule_ids(),
                        "workflows": assets.workflow_ids(),
                    }))
                    .unwrap()
                );
            } else if states.is_empty() {
                println!("no skills registered");
            } else {
                for (id, state) in &states {
                    println!("  skill {id}  {state}");
                }
                for id in assets.hook_ids() {
                    println!("  hook  {id}");
                }
                for id in assets.rule_ids() {
                    println!("  rule  {id}");
                }
                for id in assets.workflow_ids() {
                    println!("  workflow {id}");
                }
            }
            Ok(0)
        }
        AssetsCmd::Register {
            id,
            description,
            permissions,
        } => {
            let skill = Skill::new(
                SkillId(id.clone()),
                SkillVersion(1),
                description,
                permissions.clone(),
                Vec::new(),
                Provenance::new("run.cli", "src.assets_register"),
            )
            .map_err(|e| e.to_string())?;
            let mut assets = rt.load_assets().await.map_err(|e| e.to_string())?;
            assets.register_skill(skill).map_err(|e| e.to_string())?;
            rt.save_assets(&assets).await.map_err(|e| e.to_string())?;
            println!("registered skill `{id}` (state=registered — validate + activate next)");
            Ok(0)
        }
        AssetsCmd::Generate {
            request_id,
            description,
            permissions,
            session,
        } => {
            let cand = rt
                .skill_submit_from_history(request_id, description, permissions.clone(), session)
                .await
                .map_err(|e| e.to_string())?;
            println!("generated candidate `{cand}` from history evidence (session=`{session}`)");
            Ok(0)
        }
        AssetsCmd::Validate { id } => {
            rt.assets_validate_skill(id)
                .await
                .map_err(|e| e.to_string())?;
            println!("skill `{id}` validated");
            Ok(0)
        }
        AssetsCmd::Activate { id, operator } => {
            rt.assets_activate_skill(id, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("skill `{id}` active under operator `{operator}`");
            Ok(0)
        }
        AssetsCmd::Hook { id } => {
            let outcome = rt.assets_run_hook(id).await.map_err(|e| e.to_string())?;
            println!(
                "hook `{}` fired={} target={}",
                outcome.hook, outcome.fired, outcome.target
            );
            Ok(0)
        }
        AssetsCmd::Rule { scope, subject } => {
            let d = rt
                .assets_evaluate_rule(scope, subject)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "decision allowed={} decided_by={} rule={:?}",
                d.allowed, d.decided_by, d.rule
            );
            Ok(0)
        }
        AssetsCmd::CandidateValidate { candidate } => {
            rt.assets_validate_candidate(candidate)
                .await
                .map_err(|e| e.to_string())?;
            println!("candidate `{candidate}` validated (Generated → Validated)");
            Ok(0)
        }
        AssetsCmd::CandidateApprove {
            candidate,
            operator,
        } => {
            rt.assets_approve_candidate(candidate, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!("candidate `{candidate}` approved by `{operator}`");
            Ok(0)
        }
        AssetsCmd::CandidateActivate {
            candidate,
            operator,
        } => {
            let skill = rt
                .assets_activate_candidate(candidate, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "candidate `{candidate}` active as `{}` under operator `{operator}`",
                skill.0
            );
            Ok(0)
        }
        AssetsCmd::Genome {
            id,
            operator,
            session,
        } => {
            if let Some(session) = session {
                let sid = parse_session_id(session)?;
                let applied = rt
                    .session_apply_genome(sid, id, operator)
                    .await
                    .map_err(|e| e.to_string())?;
                println!(
                    "genome `{id}` pinned to session `{sid}` (version={} digest={})",
                    applied.version, applied.digest
                );
                return Ok(0);
            }
            let grant =
                openrt_workspace_assets::OperatorGrant::new(operator).map_err(|e| e.to_string())?;
            let activated = rt
                .assets_apply_genome(id, Some(&grant))
                .await
                .map_err(|e| e.to_string())?;
            if activated.is_empty() {
                println!("genome `{id}`: all referenced assets already active");
            } else {
                for a in &activated {
                    println!("genome `{id}` activated `{a}`");
                }
            }
            Ok(0)
        }
        AssetsCmd::Export => {
            let pkg = rt
                .assets_export_package()
                .await
                .map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&pkg).unwrap());
            Ok(0)
        }
        AssetsCmd::Import { path } => {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let pkg: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            let n = rt
                .assets_import_package(&pkg)
                .await
                .map_err(|e| e.to_string())?;
            println!("imported {n} asset(s) from `{path}` (digests re-verified)");
            Ok(0)
        }
    }
}

async fn cmd_improve(cli: &Cli, cmd: &ImproveCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        ImproveCmd::List { json } => {
            let states = rt.improve_states().await.map_err(|e| e.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &states
                            .iter()
                            .map(|(id, s)| serde_json::json!({"id": id, "state": s}))
                            .collect::<Vec<_>>()
                    )
                    .unwrap()
                );
            } else if states.is_empty() {
                println!("no improvement proposals");
            } else {
                for (id, state) in &states {
                    println!("  {id}  {state}");
                }
            }
            Ok(0)
        }
        ImproveCmd::Baseline {
            id,
            metric,
            value,
            direction,
            run,
        } => {
            let dir = match direction.as_str() {
                "higher_is_better" => openrt_selfimprove::MetricDirection::HigherIsBetter,
                "lower_is_better" => openrt_selfimprove::MetricDirection::LowerIsBetter,
                other => return Err(format!("unknown direction `{other}`")),
            };
            rt.improve_record_baseline(id, metric, *value, dir, run)
                .await
                .map_err(|e| e.to_string())?;
            println!("baseline `{id}` recorded ({metric}={value} {direction})");
            Ok(0)
        }
        ImproveCmd::Propose {
            id,
            kind,
            summary,
            target_kind,
            target_id,
            baseline,
            expected,
        } => {
            let k = match kind.as_str() {
                "self_debug" => openrt_selfimprove::ProposalKind::SelfDebug,
                "self_optimize" => openrt_selfimprove::ProposalKind::SelfOptimize,
                "research_change" => openrt_selfimprove::ProposalKind::ResearchChange,
                other => return Err(format!("unknown proposal kind `{other}`")),
            };
            let tk = match target_kind.as_str() {
                "router" => openrt_selfimprove::TargetKind::Router,
                "tool" => openrt_selfimprove::TargetKind::Tool,
                "plan" => openrt_selfimprove::TargetKind::Plan,
                "prompt" => openrt_selfimprove::TargetKind::Prompt,
                "workflow" => openrt_selfimprove::TargetKind::Workflow,
                "policy" => openrt_selfimprove::TargetKind::Policy,
                "permission_policy" => openrt_selfimprove::TargetKind::PermissionPolicy,
                "security_scan" => openrt_selfimprove::TargetKind::SecurityScan,
                "verification" => openrt_selfimprove::TargetKind::Verification,
                other => return Err(format!("unknown target kind `{other}`")),
            };
            rt.improve_propose(id, k, summary, tk, target_id, baseline.clone(), *expected)
                .await
                .map_err(|e| e.to_string())?;
            println!("proposed `{id}` ({kind} → {target_kind}:{target_id})");
            Ok(0)
        }
        ImproveCmd::Evaluate { id } => {
            let outcome = rt.improve_evaluate(id).await.map_err(|e| e.to_string())?;
            println!("evaluated `{id}`: {outcome}");
            Ok(0)
        }
        ImproveCmd::Approve { id, operator, note } => {
            match note {
                Some(note) => {
                    rt.improve_approve_with_note(id, operator, Some(note.as_str()))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                None => {
                    rt.improve_approve(id, operator)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            println!("approved `{id}` by `{operator}`");
            Ok(0)
        }
        ImproveCmd::Reject { id, operator, note } => {
            rt.improve_reject_with_note(id, operator, note.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            println!("rejected `{id}` by `{operator}`");
            Ok(0)
        }
        ImproveCmd::Apply { id, operator } => {
            let change = rt
                .improve_apply(id, operator)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "applied `{}` → target `{}` digest={}",
                change.proposal_id, change.target_id, change.digest
            );
            Ok(0)
        }
        ImproveCmd::Learn { run, session } => {
            let records = rt
                .learn_from_history(run, session)
                .await
                .map_err(|e| e.to_string())?;
            if records.is_empty() {
                println!("no learnings derived from session `{session}`");
            } else {
                for r in &records {
                    println!(
                        "  learning {} kind={} run={}",
                        r.id,
                        r.kind.as_str(),
                        r.run_id
                    );
                }
                println!("{} learning(s) derived from history", records.len());
            }
            Ok(0)
        }
    }
}

async fn cmd_webui(cli: &Cli, cmd: &WebuiCmd) -> Result<i32, String> {
    match cmd {
        WebuiCmd::Serve { port, operator } => {
            let rt = build_runtime(cli).await?;
            let improve = rt.load_improve().await.map_err(|e| e.to_string())?;
            let rt_handle = tokio::runtime::Handle::current();
            let mut palette = CommandPalette::with_runtime(improve, rt.clone(), rt_handle);
            let op = if operator.is_empty() {
                std::env::var("USER").unwrap_or_else(|_| "openrt".to_string())
            } else {
                operator.clone()
            };
            let token = palette.issue_token(&op);
            let config = openrt_webui::WebUiConfig::localhost(*port);
            let handle = openrt_webui::serve(config, palette).map_err(|e| e.to_string())?;
            println!("web UI listening on http://{}", handle.local_addr());
            println!("operator `{op}` token (Authorization: Bearer <token>):");
            println!("{token}");
            println!("loopback only — paste the token into the UI operator-token field");
            // Park forever until Ctrl-C; ServerHandle stops on drop. Keep the
            // Arc<Runtime> alive so palette approvals stay durable.
            let _rt = rt;
            let _handle = handle;
            std::future::pending::<()>().await;
            Ok(0)
        }
    }
}

async fn cmd_audit(cli: &Cli, cmd: &AuditCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        AuditCmd::Verify => {
            let n = rt.audit_verify().await.map_err(|e| e.to_string())?;
            println!("audit chain verified ({n} record(s), ADR-002)");
            Ok(0)
        }
        AuditCmd::Query {
            action_prefix,
            action,
            subject,
            result,
            actor,
            limit,
            json,
        } => {
            let filter = openrt_audit::AuditQuery {
                actor: actor.clone(),
                action: action.clone(),
                action_prefix: action_prefix.clone(),
                subject_contains: subject.clone(),
                result: result.clone(),
                limit: Some(*limit),
                ..Default::default()
            };
            let rows = rt.audit_query(&filter).await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                for r in &rows {
                    println!(
                        "{:>6}  {}  {:<20} {:<32} {:<40} {}",
                        r.seq,
                        r.entry.ts,
                        r.entry.actor,
                        r.entry.action,
                        r.entry.subject,
                        r.entry.result
                    );
                }
                println!("{} record(s)", rows.len());
            }
            Ok(0)
        }
        AuditCmd::Retention { json } => {
            let st = rt.audit_retention().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&st).unwrap());
            } else {
                println!(
                    "chain={} total={} head_seq={} tail_seq={}",
                    st.chain_mode, st.total, st.head_seq, st.tail_seq
                );
            }
            Ok(0)
        }
    }
}

async fn cmd_policy(cli: &Cli, cmd: &PolicyCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        PolicyCmd::Show { json } => {
            let doc = rt.policy_document().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&doc).unwrap());
            } else {
                println!(
                    "policy.engine v{} (schema {}, {} rule(s)) author={} note={:?}",
                    doc.version,
                    doc.schema,
                    doc.rules.len(),
                    doc.author,
                    doc.note
                );
                for (i, r) in doc.rules.iter().enumerate() {
                    println!(
                        "  [{i}] {:<6} {}{}",
                        match r.action {
                            openrt_permissions::RuleAction::Allow => "allow",
                            openrt_permissions::RuleAction::Deny => "deny",
                            openrt_permissions::RuleAction::Ask => "ask",
                        },
                        r.pattern,
                        r.scope
                            .as_deref()
                            .map(|s| format!("  scope={s}"))
                            .unwrap_or_default()
                    );
                }
            }
            Ok(0)
        }
        PolicyCmd::Init => {
            let doc = rt
                .policy_document_ensure()
                .await
                .map_err(|e| e.to_string())?;
            println!("policy document ready at v{}", doc.version);
            Ok(0)
        }
        PolicyCmd::Append {
            pattern,
            action,
            scope,
            note,
        } => {
            let action = openrt_policy::parse_rule_action(action).map_err(|e| e.to_string())?;
            let doc = rt
                .policy_append_rule(&actor_user(), note, pattern, action, scope.clone())
                .await
                .map_err(|e| e.to_string())?;
            println!("appended rule `{pattern}` → policy.engine v{}", doc.version);
            Ok(0)
        }
        PolicyCmd::Versions => {
            let vs = rt.policy_versions().await.map_err(|e| e.to_string())?;
            for v in &vs {
                println!("v{v}");
            }
            println!("{} revision(s)", vs.len());
            Ok(0)
        }
    }
}

async fn cmd_secret(cli: &Cli, cmd: &SecretCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    let store = rt.secrets().await.map_err(|e| e.to_string())?;
    match cmd {
        SecretCmd::Set { name, value } => {
            store
                .put(name, value.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "sealed `{name}` (key_id={} generation recorded)",
                store.key_id()
            );
            Ok(0)
        }
        SecretCmd::Get { name } => {
            let v = store.get(name).await.map_err(|e| e.to_string())?;
            use std::io::Write;
            let mut out = std::io::stdout();
            out.write_all(&v).map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
            Ok(0)
        }
        SecretCmd::Delete { name } => {
            store.delete(name).await.map_err(|e| e.to_string())?;
            println!("deleted `{name}`");
            Ok(0)
        }
        SecretCmd::List { json } => {
            let rows = store.list().await.map_err(|e| e.to_string())?;
            if *json {
                // metadata only — no ciphertext
                let meta: Vec<_> = rows
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "key_id": s.key_id,
                            "algorithm": s.algorithm,
                            "sealed_at": s.sealed_at,
                            "generation": s.generation,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&meta).unwrap());
            } else {
                for s in &rows {
                    println!(
                        "{:<40} gen={} key={} sealed_at={}",
                        s.name, s.generation, s.key_id, s.sealed_at
                    );
                }
                println!("{} secret(s)", rows.len());
            }
            Ok(0)
        }
        SecretCmd::Rotate => {
            let mut key = [0u8; 32];
            use std::io::Read;
            std::io::stdin()
                .read_exact(&mut key)
                .map_err(|e| format!("read 32-byte key from stdin: {e}"))?;
            let kid = store.rotate(key).await.map_err(|e| e.to_string())?;
            println!("rotated master key → {kid}");
            Ok(0)
        }
    }
}

async fn cmd_sandbox(cli: &Cli, cmd: &SandboxCmd) -> Result<i32, String> {
    use openrt_sandbox::{
        capability_rows, SandboxLimits, SandboxProfile, SandboxRequest, DEFAULT_LIMITS,
    };
    let rt = build_runtime(cli).await?;
    match cmd {
        SandboxCmd::Capabilities { json } => {
            let caps = rt.sandbox.capabilities();
            if *json {
                println!("{}", serde_json::to_string_pretty(&caps).unwrap());
            } else {
                for row in capability_rows(&caps) {
                    println!("{:<28} {:?}", row.name, row.status);
                }
            }
            Ok(0)
        }
        SandboxCmd::Policy { json } => {
            let profiles = [
                SandboxProfile::ReadOnly,
                SandboxProfile::Development,
                SandboxProfile::Networked,
                SandboxProfile::Strict,
                SandboxProfile::Untrusted,
            ];
            if *json {
                let rows: Vec<_> = profiles
                    .iter()
                    .map(|p| {
                        let pol = rt.sandbox.policy(*p);
                        serde_json::json!({
                            "profile": p.as_str(),
                            "digest": pol.digest(),
                            "allowed_network": pol.allowed_network,
                            "required_isolation": pol.required_isolation,
                            "allow_unconfined_fallback": pol.allow_unconfined_fallback,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                for p in profiles {
                    let pol = rt.sandbox.policy(p);
                    let net: Vec<_> = pol.allowed_network.iter().map(|n| n.as_str()).collect();
                    println!(
                        "{:<12} digest={} network=[{}] isolation={:?} fallback={}",
                        p.as_str(),
                        &pol.digest()[..12],
                        net.join(","),
                        pol.required_isolation,
                        pol.allow_unconfined_fallback,
                    );
                }
            }
            Ok(0)
        }
        SandboxCmd::List { json } => {
            let rows = rt.sandbox.list().await.map_err(|f| f.render())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                for r in &rows {
                    println!(
                        "{:<38} {:<12} {:<10} pid={:?} profile={}",
                        r.id.to_string(),
                        r.state.as_str(),
                        r.backend.as_str(),
                        r.pid,
                        r.profile,
                    );
                }
                println!("{} instance(s)", rows.len());
            }
            Ok(0)
        }
        SandboxCmd::Inspect { id, json } => {
            let r = rt.sandbox.inspect(id).await.map_err(|f| f.render())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            } else {
                println!(
                    "id={} state={} backend={} profile={} network={} isolation={}\npolicy_digest={} exit_code={:?} pid={:?} failure={:?}",
                    r.id, r.state.as_str(), r.backend.as_str(), r.profile,
                    r.network.as_str(), r.isolation_level.as_str(), r.policy_digest,
                    r.exit_code, r.pid, r.failure,
                );
            }
            Ok(0)
        }
        SandboxCmd::Check { profile } => {
            rt.sandbox.check_profile(profile).map_err(|f| f.render())?;
            println!("profile `{profile}` admitted (policy valid, backend available)");
            Ok(0)
        }
        SandboxCmd::Run {
            profile,
            code,
            file,
            timeout_secs,
        } => {
            let profile = SandboxProfile::from_name(profile)
                .ok_or_else(|| format!("unknown profile `{profile}`"))?;
            let script = match (code, file) {
                (Some(c), _) => c.clone(),
                (None, Some(f)) => std::fs::read_to_string(f).map_err(|e| e.to_string())?,
                (None, None) => return Err("provide --code or --file".into()),
            };
            let cfg = effective_config()?;
            let python = cfg
                .runtime
                .python_sandbox
                .as_ref()
                .and_then(|s| s.python.clone())
                .unwrap_or_else(|| std::path::PathBuf::from("/usr/bin/python3"));
            let req = SandboxRequest::python_script(python.display().to_string(), script)
                .with_limits(SandboxLimits {
                    wall_timeout: std::time::Duration::from_secs(*timeout_secs),
                    ..DEFAULT_LIMITS
                });
            let res = rt
                .sandbox
                .run(&req, profile, "cli")
                .await
                .map_err(|f| f.render())?;
            println!(
                "sandbox={} state={} backend={} profile={} network={} cleanup_verified={} exit_code={:?}",
                res.id, res.state.as_str(), res.backend.as_str(), res.profile,
                res.network.as_str(), res.cleanup.verified, res.exit_code,
            );
            if !res.stdout.is_empty() {
                println!("--- stdout ---\n{}", res.stdout);
            }
            if !res.stderr.is_empty() {
                println!("--- stderr ---\n{}", res.stderr);
            }
            if res.state == openrt_sandbox::SandboxState::Succeeded {
                Ok(res.exit_code.unwrap_or(0))
            } else {
                if let Some(reason) = &res.kill_reason {
                    eprintln!("killed: {reason}");
                }
                Ok(1)
            }
        }
        SandboxCmd::Destroy { id } => {
            let outcome = rt.sandbox.destroy(id).await.map_err(|f| f.render())?;
            println!(
                "destroyed {id} (verified={}, processes_gone={}, workspace_removed={}, detail={:?})",
                outcome.verified, outcome.processes_gone, outcome.workspace_removed, outcome.detail
            );
            Ok(0)
        }
    }
}

async fn cmd_netpol(cli: &Cli, cmd: &NetpolCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    let np = rt.netpol();
    match cmd {
        NetpolCmd::Grant { scope, allow, note } => {
            let g = np
                .grant(&actor_user(), scope, allow.clone(), note)
                .await
                .map_err(|e| e.to_string())?;
            println!("granted scope `{}`: {:?}", g.scope, g.allow);
            Ok(0)
        }
        NetpolCmd::Revoke { scope } => {
            np.revoke(&actor_user(), scope)
                .await
                .map_err(|e| e.to_string())?;
            println!("revoked scope `{scope}`");
            Ok(0)
        }
        NetpolCmd::List { json } => {
            let rows = np.list().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                for g in &rows {
                    println!(
                        "{:<32} allow={:?} by={} note={:?}",
                        g.scope, g.allow, g.granted_by, g.note
                    );
                }
                println!("{} grant(s)", rows.len());
            }
            Ok(0)
        }
        NetpolCmd::Check { scope, destination } => {
            let d = np
                .ensure_egress(scope, destination)
                .await
                .map_err(|e| e.to_string())?;
            match d {
                openrt_netpol::EgressDecision::Allow { scope, destination } => {
                    println!("ALLOW scope={scope} dest={destination}");
                    Ok(0)
                }
                openrt_netpol::EgressDecision::Deny {
                    scope,
                    destination,
                    reason,
                } => {
                    println!("DENY scope={scope} dest={destination}: {reason}");
                    Ok(2)
                }
            }
        }
    }
}

async fn cmd_enclave(cli: &Cli, cmd: &EnclaveCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    let en = rt.enclave();
    match cmd {
        EnclaveCmd::Enroll {
            node_id,
            fingerprint,
            capabilities,
        } => {
            let n = en
                .enroll(&actor_user(), node_id, fingerprint, capabilities.clone())
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "enrolled `{}` fp={} caps={:?}",
                n.node_id, n.identity_fp, n.capabilities
            );
            Ok(0)
        }
        EnclaveCmd::Nodes { json } => {
            let rows = en.list_nodes().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                for n in &rows {
                    println!(
                        "{:<32} {:?} fp={} caps={:?}{}",
                        n.node_id,
                        n.status,
                        n.identity_fp,
                        n.capabilities,
                        n.quarantine_reason
                            .as_deref()
                            .map(|r| format!("  reason={r}"))
                            .unwrap_or_default()
                    );
                }
                println!("{} node(s)", rows.len());
            }
            Ok(0)
        }
        EnclaveCmd::Quarantine { node_id, reason } => {
            en.quarantine(&actor_user(), node_id, reason)
                .await
                .map_err(|e| e.to_string())?;
            println!("quarantined `{node_id}`: {reason}");
            Ok(0)
        }
        EnclaveCmd::Migrate {
            artifact_id,
            sha256,
            from,
            to,
            provenance,
        } => {
            let rec = en
                .migrate_model(
                    &actor_user(),
                    artifact_id,
                    sha256,
                    from,
                    to,
                    provenance.clone(),
                )
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "migration {} verified={} {}:{} → {}",
                rec.transfer_id, rec.verified, rec.from_node, rec.artifact_id, rec.to_node
            );
            Ok(0)
        }
    }
}

async fn cmd_obs(cli: &Cli, cmd: &ObsCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    let ob = rt.observability();
    match cmd {
        ObsCmd::Health { json } => {
            let h = ob.health().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&h).unwrap());
            } else {
                println!(
                    "healthy={} events={} errors={} permission_denials={} safety_halts={} workflow_failures={}",
                    h.healthy,
                    h.events_total,
                    h.errors_total,
                    h.permission_denials_total,
                    h.safety_halts_total,
                    h.workflow_failures_total
                );
            }
            Ok(if h.healthy { 0 } else { 1 })
        }
        ObsCmd::Retention {
            max_age_secs,
            max_events,
        } => {
            if max_age_secs.is_some() || max_events.is_some() {
                let mut p = ob.retention().await.map_err(|e| e.to_string())?;
                if let Some(a) = max_age_secs {
                    p.max_age_secs = *a;
                }
                if let Some(m) = max_events {
                    p.max_events = *m;
                }
                ob.set_retention(p).await.map_err(|e| e.to_string())?;
            }
            let p = ob.retention().await.map_err(|e| e.to_string())?;
            println!(
                "retention max_age_secs={} max_events={} updated_at={}",
                p.max_age_secs, p.max_events, p.updated_at
            );
            Ok(0)
        }
        ObsCmd::Export {
            destination,
            json_payload,
        } => {
            let payload = match json_payload {
                Some(s) => serde_json::from_str(s).map_err(|e| format!("bad json: {e}"))?,
                None => serde_json::json!({}),
            };
            match ob.queue_export(destination, payload).await {
                Ok(b) => {
                    println!("queued export {} → {}", b.id, b.destination);
                    Ok(0)
                }
                Err(e) => {
                    println!("export refused (fail closed): {e}");
                    Ok(2)
                }
            }
        }
    }
}

async fn cmd_autonomous(cli: &Cli, cmd: &AutonomousCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    let sup = rt.supervisor(actor_user());
    match cmd {
        AutonomousCmd::Create {
            workflow_id,
            session,
            max_wall_secs,
            max_budget,
            max_errors,
            approve,
            deny,
        } => {
            let conditions = openrt_supervisor::StopConditions {
                max_wall_secs: *max_wall_secs,
                max_budget: *max_budget,
                max_errors: *max_errors,
                stop_on_complete: true,
            };
            let mat = openrt_supervisor::ApprovalMat {
                approved: approve.clone(),
                always_deny: deny.clone(),
            };
            let run = sup
                .create_run(workflow_id, session, None, None, conditions, mat)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "created autonomous run {} workflow={} session={} wall={} budget={} errors={}",
                run.run_id,
                run.workflow_id,
                run.session_id,
                run.conditions.max_wall_secs,
                run.conditions.max_budget,
                run.conditions.max_errors
            );
            Ok(0)
        }
        AutonomousCmd::List { json } => {
            let rows = sup.list_runs().await.map_err(|e| e.to_string())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else {
                for r in &rows {
                    println!(
                        "{:<38} {:<16} {:?} wall={} budget={} errors={}{}",
                        r.run_id,
                        r.workflow_id,
                        r.state,
                        r.conditions.max_wall_secs,
                        r.conditions.max_budget,
                        r.errors,
                        r.stop_reason
                            .as_ref()
                            .map(|s| format!(" stop={s:?}"))
                            .unwrap_or_default()
                    );
                }
                println!("{} run(s)", rows.len());
            }
            Ok(0)
        }
        AutonomousCmd::Stop { run_id, reason } => {
            let r = sup
                .operator_stop(run_id, reason)
                .await
                .map_err(|e| e.to_string())?;
            println!("stopped {} reason={:?}", r.run_id, r.stop_reason);
            Ok(0)
        }
        AutonomousCmd::Takeover { run_id } => {
            let r = sup.takeover(run_id).await.map_err(|e| e.to_string())?;
            println!("took over {} generation={}", r.run_id, r.generation);
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 12 CLI handlers (Part H): industrial intelligence, research-only
// trading, experience-driven self-improvement. Every mutating call runs
// through the Runtime methods that audit the ONE chain.
// ---------------------------------------------------------------------------

fn parse_asset_kind(s: &str) -> Result<openrt_industrial::AssetKind, String> {
    use openrt_industrial::AssetKind as K;
    match s {
        "plant" => Ok(K::Plant),
        "unit" => Ok(K::Unit),
        "equipment" => Ok(K::Equipment),
        "sensor" => Ok(K::Sensor),
        "alarm" => Ok(K::Alarm),
        "process" => Ok(K::Process),
        "incident" => Ok(K::Incident),
        "work_order" | "workorder" => Ok(K::WorkOrder),
        "inspection" => Ok(K::Inspection),
        "procedure" => Ok(K::Procedure),
        other => Err(format!(
            "unknown asset kind `{other}` (plant|unit|equipment|sensor|alarm|process|incident|work_order|inspection|procedure)"
        )),
    }
}

fn parse_instrument_kind(s: &str) -> Result<openrt_trading::InstrumentKind, String> {
    use openrt_trading::InstrumentKind as K;
    match s {
        "equity" => Ok(K::Equity),
        "index" => Ok(K::Index),
        "future" => Ok(K::Future),
        "fx" => Ok(K::Fx),
        "crypto" => Ok(K::Crypto),
        "other" => Ok(K::Other),
        other => Err(format!(
            "unknown instrument kind `{other}` (equity|index|future|fx|crypto|other)"
        )),
    }
}

fn parse_kv_pairs(pairs: &[String]) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let mut out = serde_json::Map::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| format!("expected `key=value`, got `{p}`"))?;
        out.insert(
            k.trim().to_string(),
            serde_json::Value::from(v.trim().to_string()),
        );
    }
    Ok(out)
}

fn parse_baseline(spec: &str) -> Result<std::collections::BTreeMap<String, f64>, String> {
    let mut out = std::collections::BTreeMap::new();
    for pair in spec.split(',').filter(|p| !p.trim().is_empty()) {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("expected `metric=value`, got `{pair}`"))?;
        let val: f64 = v
            .trim()
            .parse()
            .map_err(|_| format!("metric `{k}` value `{v}` is not a number"))?;
        out.insert(k.trim().to_string(), val);
    }
    if out.is_empty() {
        return Err("baseline must contain at least one `metric=value` pair".to_string());
    }
    Ok(out)
}

fn parse_typed_points(points: &[String]) -> Result<Vec<openrt_industrial::Point>, String> {
    let mut out = Vec::new();
    for p in points {
        let parts: Vec<&str> = p.split(',').map(str::trim).collect();
        if parts.len() < 2 {
            return Err(format!("point `{p}` must be `t_ms,value[,quality,source]`"));
        }
        let t_ms: i64 = parts[0]
            .parse()
            .map_err(|_| format!("point `{p}`: t_ms is not an integer"))?;
        let value: f64 = parts[1]
            .parse()
            .map_err(|_| format!("point `{p}`: value is not a number"))?;
        let quality = match parts.get(2).copied().unwrap_or("good") {
            "good" => openrt_industrial::Quality::Good,
            "suspect" => openrt_industrial::Quality::Suspect,
            "missing" => openrt_industrial::Quality::Missing,
            other => return Err(format!("point `{p}`: unknown quality `{other}`")),
        };
        out.push(openrt_industrial::Point {
            t_ms,
            value,
            quality,
            source: parts.get(3).copied().unwrap_or("cli").to_string(),
        });
    }
    if out.is_empty() {
        return Err("at least one --point is required".to_string());
    }
    Ok(out)
}

fn parse_bars_arg(bars: &[String]) -> Result<Vec<openrt_trading::Bar>, String> {
    let mut out = Vec::new();
    for b in bars {
        let parts: Vec<&str> = b.split(',').map(str::trim).collect();
        if parts.len() != 6 {
            return Err(format!(
                "bar `{b}` must be `t_ms,open,high,low,close,volume`"
            ));
        }
        let mut nums = [0.0f64; 6];
        for (i, part) in parts.iter().enumerate() {
            nums[i] = part
                .parse()
                .map_err(|_| format!("bar `{b}`: field {i} is not a number"))?;
        }
        out.push(openrt_trading::Bar {
            t_ms: nums[0] as i64,
            open: nums[1],
            high: nums[2],
            low: nums[3],
            close: nums[4],
            volume: nums[5],
        });
    }
    if out.is_empty() {
        return Err("at least one --bar is required".to_string());
    }
    Ok(out)
}

fn read_json_file<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read `{path}`: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("parse `{path}`: {e}"))
}

fn print_json_or<T: serde::Serialize>(
    json: bool,
    value: &T,
    human: impl FnOnce(),
) -> Result<i32, String> {
    if json {
        let v = serde_json::to_value(value).map_err(|e| e.to_string())?;
        println!("{}", to_pretty(&v)?);
    } else {
        human();
    }
    Ok(0)
}

async fn cmd_industrial(cli: &Cli, cmd: &IndustrialCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        IndustrialCmd::AssetPut {
            id,
            kind,
            label,
            attrs,
            actor,
            json,
        } => {
            let k = parse_asset_kind(kind)?;
            let provenance = openrt_industrial::Provenance::derived(
                format!("operator:{actor}"),
                openrt_core::now().to_rfc3339(),
            );
            let mut asset = openrt_industrial::Asset::new(id, k, label, provenance);
            for (key, value) in parse_kv_pairs(attrs)? {
                asset.attrs.insert(key, value);
            }
            let saved = rt
                .industrial_asset_put(asset, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &saved, || {
                println!(
                    "upserted asset {} kind={} label={}",
                    saved.id,
                    saved.kind.as_str(),
                    saved.label
                );
            })
        }
        IndustrialCmd::Relate {
            subject,
            predicate,
            object,
            actor,
            json,
        } => {
            let rel = rt
                .industrial_relation_add(subject, predicate, object, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &rel, || {
                println!(
                    "related {} --{}--> {}",
                    rel.subject, rel.predicate, rel.object
                );
            })
        }
        IndustrialCmd::Assets {
            kind,
            plant,
            unit,
            text,
            json,
        } => {
            let filter = openrt_industrial::AssetFilter {
                kind: match kind {
                    Some(k) => Some(parse_asset_kind(k)?),
                    None => None,
                },
                plant: plant.clone(),
                unit: unit.clone(),
                text: text.clone(),
            };
            let assets = rt
                .industrial_assets(&filter)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &assets, || {
                println!("{} asset(s):", assets.len());
                for a in &assets {
                    println!("  {:<24} {:<11} {}", a.id, a.kind.as_str(), a.label);
                }
            })
        }
        IndustrialCmd::Asset { id, json } => {
            let asset = rt
                .industrial_asset(id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("asset `{id}` not found"))?;
            print_json_or(*json, &asset, || {
                println!(
                    "{} kind={} label={} attrs={}",
                    asset.id,
                    asset.kind.as_str(),
                    asset.label,
                    asset.attrs.len()
                );
            })
        }
        IndustrialCmd::SeriesAppend {
            series_id,
            asset_id,
            unit,
            points,
            actor,
            json,
        } => {
            let pts = parse_typed_points(points)?;
            let series = rt
                .industrial_series_append(series_id, asset_id, unit, &pts, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &series, || {
                println!(
                    "series {} now holds {} points (asset {})",
                    series.id,
                    series.points.len(),
                    series.asset_id
                );
            })
        }
        IndustrialCmd::SeriesQuery {
            id,
            from_ms,
            to_ms,
            bucket_ms,
            json,
        } => {
            let q = rt
                .industrial_series_query(id, *from_ms, to_ms.unwrap_or(i64::MAX), *bucket_ms)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &q, || {
                println!(
                    "series {} window: {} buckets, {} missing (never interpolated)",
                    id,
                    q.buckets.len(),
                    q.missing_buckets
                );
            })
        }
        IndustrialCmd::Trend {
            asset_id,
            series_id,
            window,
            min_points,
            low,
            high,
            actor,
            json,
        } => {
            let mut params = openrt_industrial::TrendParams::default();
            if let Some(w) = window {
                params.window = *w;
            }
            if let Some(m) = min_points {
                params.min_points = *m;
            }
            let threshold = match (low, high) {
                (None, None) => None,
                _ => Some(openrt_industrial::Threshold {
                    low: *low,
                    high: *high,
                }),
            };
            let r = rt
                .industrial_trend_analysis(asset_id, series_id, params, threshold, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &r, || {
                println!(
                    "trend {} on {}: {:?} over {} points (slope {:?})",
                    series_id, asset_id, r.direction, r.points_used, r.slope_per_point
                );
            })
        }
        IndustrialCmd::Anomaly {
            asset_id,
            series_id,
            method,
            z,
            low,
            high,
            actor,
            json,
        } => {
            let mut params = serde_json::Map::new();
            if let Some(z) = z {
                params.insert("z_threshold".into(), (*z).into());
            }
            if let Some(low) = low {
                params.insert("low".into(), (*low).into());
            }
            if let Some(high) = high {
                params.insert("high".into(), (*high).into());
            }
            let params = serde_json::Value::Object(params);
            let run = rt
                .industrial_anomaly_analysis(asset_id, series_id, method, &params, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &run, || {
                if let Some(reason) = &run.unavailable {
                    println!("detector {} UNAVAILABLE: {reason}", run.method.as_str());
                } else {
                    println!(
                        "detector {}: {} anomal(ies) over {} points",
                        run.method.as_str(),
                        run.anomalies.len(),
                        run.params
                    );
                }
            })
        }
        IndustrialCmd::Rca {
            subject,
            actor,
            json,
        } => {
            let report = rt
                .industrial_rca(subject, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &report, || {
                println!(
                    "RCA for {} ({} finding(s)):",
                    subject,
                    report.findings.len()
                );
                for f in &report.findings {
                    println!(
                        "  [{}] {} (evidence: {})",
                        f.label.as_str(),
                        f.statement,
                        f.evidence.len()
                    );
                }
            })
        }
        IndustrialCmd::Fmea { asset_id, json } => {
            let table = rt
                .industrial_fmea(asset_id)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &table, || {
                println!("{} failure mode(s):", table.len());
                for m in &table {
                    println!("  {} — {}", m.mode, m.effects.join("; "));
                }
            })
        }
        IndustrialCmd::Risk {
            asset_id,
            series,
            actor,
            json,
        } => {
            let r = rt
                .industrial_risk(asset_id, series.as_deref(), actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &r, || match (&r.band, &r.score) {
                (Some(band), Some(score)) => {
                    println!(
                        "risk {} band={:?} score={score:.3} (method {})",
                        asset_id, band, r.method
                    );
                    if let Some(note) = &r.insufficient {
                        println!("  note: {note}");
                    }
                }
                (None, _) => println!(
                    "risk {} INSUFFICIENT: {}",
                    asset_id,
                    r.insufficient.as_deref().unwrap_or("no verdict")
                ),
                (Some(band), None) => println!(
                    "risk {} band={:?} score unavailable (method {})",
                    asset_id, band, r.method
                ),
            })
        }
        IndustrialCmd::Scenario {
            asset_id,
            event,
            assumptions,
            actor,
            json,
        } => {
            let out = rt
                .industrial_scenario(asset_id, event, assumptions.clone(), actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &out, || {
                println!(
                    "scenario `{event}` for {asset_id} (simulated={}, advisory_only={}):",
                    out.simulated, out.advisory_only
                );
                for st in &out.statements {
                    println!("  [{:?}] {} ({})", st.kind, st.text, st.basis);
                }
            })
        }
        IndustrialCmd::Investigate {
            asset_id,
            actor,
            json,
        } => {
            let out = rt
                .industrial_investigate(asset_id, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &out, || {
                println!(
                    "investigation for {asset_id} ({} section(s)):",
                    out.sections.len()
                );
                for s in &out.sections {
                    println!("  {}: {}", s.section, s.text);
                }
                println!("  note: {}", out.advisory);
            })
        }
    }
}

async fn cmd_trading(cli: &Cli, cmd: &TradingCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        TradingCmd::InstrumentPut {
            id,
            symbol,
            venue,
            kind,
            currency,
            tick,
            actor,
            json,
        } => {
            let instrument = openrt_trading::Instrument {
                id: id.clone(),
                symbol: symbol.clone(),
                venue: venue.clone(),
                kind: parse_instrument_kind(kind)?,
                currency: currency.clone(),
                tick_size: *tick,
                provenance: openrt_trading::Provenance::derived(
                    format!("operator:{actor}"),
                    openrt_core::now().to_rfc3339(),
                ),
                created_at: openrt_core::now().to_rfc3339(),
            };
            rt.trading_instrument_put(instrument, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"id": id}), || {
                println!("registered instrument {id} ({symbol})");
            })
        }
        TradingCmd::Instruments { json } => {
            let instruments = rt.trading_instruments().await.map_err(|e| e.to_string())?;
            print_json_or(*json, &instruments, || {
                println!("{} instrument(s):", instruments.len());
                for i in &instruments {
                    println!("  {:<16} {:<8} {:?}", i.id, i.symbol, i.kind);
                }
            })
        }
        TradingCmd::DatasetBuild {
            id,
            version,
            instrument,
            bars,
            declared_lookahead,
            actor,
            json,
        } => {
            let bars = parse_bars_arg(bars)?;
            let config = openrt_trading::DatasetConfig {
                instrument: instrument.clone(),
                from_ms: 0,
                to_ms: i64::MAX,
                filters: Default::default(),
                params: Default::default(),
                source: "cli".into(),
                transform: "as_recorded".into(),
                declared_lookahead: *declared_lookahead,
            };
            let ds = rt
                .trading_dataset_build(id, version, config, vec!["cli".into()], bars, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &ds, || {
                println!(
                    "built dataset {} v{}: {} bars, digest {}{}",
                    ds.id,
                    ds.version,
                    ds.bar_count,
                    &ds.digest[..ds.digest.len().min(16)],
                    if ds.config.declared_lookahead {
                        " (DECLARED LOOKAHEAD)"
                    } else {
                        ""
                    }
                );
            })
        }
        TradingCmd::DatasetImport {
            id,
            version,
            instrument,
            path,
            declared_lookahead,
            actor,
            json,
        } => {
            let csv = std::fs::read_to_string(path).map_err(|e| format!("read `{path}`: {e}"))?;
            let config = openrt_trading::DatasetConfig {
                instrument: instrument.clone(),
                from_ms: 0,
                to_ms: i64::MAX,
                filters: Default::default(),
                params: Default::default(),
                source: format!("csv:{path}"),
                transform: "as_recorded".into(),
                declared_lookahead: *declared_lookahead,
            };
            let ds = rt
                .trading_dataset_import_csv(id, version, config, &csv, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &ds, || {
                println!(
                    "imported dataset {} v{}: {} bars, digest {}",
                    ds.id,
                    ds.version,
                    ds.bar_count,
                    &ds.digest[..ds.digest.len().min(16)]
                );
            })
        }
        TradingCmd::Datasets { json } => {
            let datasets = rt.trading_datasets().await.map_err(|e| e.to_string())?;
            print_json_or(*json, &datasets, || {
                println!("{} dataset(s):", datasets.len());
                for d in &datasets {
                    println!(
                        "  {:<16} {:<6} bars={:<6} instrument={}",
                        d.id, d.version, d.bar_count, d.config.instrument
                    );
                }
            })
        }
        TradingCmd::StrategyPut { path, actor, json } => {
            let strategy: openrt_trading::Strategy = read_json_file(path)?;
            let digest = rt
                .trading_strategy_put(strategy, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"digest": digest}), || {
                println!(
                    "registered strategy (digest {})",
                    &digest[..digest.len().min(16)]
                );
            })
        }
        TradingCmd::Strategies { json } => {
            let strategies = rt.trading_strategies().await.map_err(|e| e.to_string())?;
            print_json_or(*json, &strategies, || {
                println!("{} strategy(ies):", strategies.len());
                for s in &strategies {
                    println!("  {:<20} {}", s.id, s.name);
                }
            })
        }
        TradingCmd::Backtest {
            strategy_id,
            dataset_id,
            cash,
            actor,
            json,
        } => {
            let r = rt
                .trading_backtest(strategy_id, dataset_id, *cash, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &r, || {
                println!(
                    "backtest {} (SIMULATED): return={:.2}% maxDD={:.2}% trades={} leaks={}",
                    r.id,
                    r.metrics.total_return_pct,
                    r.metrics.max_drawdown_pct,
                    r.metrics.trades,
                    r.look_ahead.leaks.len()
                );
            })
        }
        TradingCmd::WalkForward {
            strategy_id,
            dataset_id,
            train,
            validation,
            cash,
            actor,
            json,
        } => {
            let params = openrt_trading::WalkForwardParams {
                train_frac: *train,
                validation_frac: *validation,
                initial_cash: cash.unwrap_or(100_000.0),
            };
            let r = rt
                .trading_walk_forward(strategy_id, dataset_id, params, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &r, || {
                println!(
                    "walk-forward over {} fold(s); evaluated_only_in_sample={}",
                    r.folds.len(),
                    r.evaluated_only_in_sample
                );
            })
        }
        TradingCmd::ExperimentPut { path, actor, json } => {
            let exp: openrt_trading::ResearchExperiment = read_json_file(path)?;
            rt.trading_experiment_put(exp, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"recorded": true}), || {
                println!("recorded experiment");
            })
        }
        TradingCmd::Experiments { json } => {
            let experiments = rt.trading_experiments().await.map_err(|e| e.to_string())?;
            print_json_or(*json, &experiments, || {
                println!("{} experiment(s):", experiments.len());
                for e in &experiments {
                    println!("  {:<20} {}", e.id, e.conclusion);
                }
            })
        }
        TradingCmd::Execute { order } => {
            let order: serde_json::Value =
                serde_json::from_str(order).map_err(|e| format!("order JSON: {e}"))?;
            let err = rt.trading_execute(&order).await.map_err(|e| e.to_string());
            match err {
                Ok(_) => {
                    println!("UNEXPECTED: execution was allowed — refusing to continue");
                    Ok(3)
                }
                Err(e) if e.contains("research only") || e.contains("broker") => {
                    println!("DENIED (research only): {e}");
                    Ok(3)
                }
                Err(e) => Err(e),
            }
        }
    }
}

async fn cmd_learn(cli: &Cli, cmd: &LearnCmd) -> Result<i32, String> {
    let rt = build_runtime(cli).await?;
    match cmd {
        LearnCmd::EpisodeCapture { path, actor, json } => {
            let episode: openrt_selfimprove::ExperienceEpisode = read_json_file(path)?;
            rt.learn_capture_episode(episode, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"recorded": true}), || {
                println!("episode sealed into improve.episodes");
            })
        }
        LearnCmd::Episodes { json } => {
            let episodes = rt.learn_episodes().await.map_err(|e| e.to_string())?;
            print_json_or(*json, &episodes, || {
                println!("{} episode(s):", episodes.len());
                for e in &episodes {
                    println!(
                        "  {:<24} outcome={} verification={}",
                        e.id,
                        e.outcome.as_str(),
                        format!("{:?}", e.verification).to_lowercase()
                    );
                }
            })
        }
        LearnCmd::EpisodeEvaluate { id, actor, json } => {
            let eval = rt
                .learn_evaluate_episode(id, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &eval, || {
                println!("episode {id} ({}) — MEASURED:", eval.method);
                for (k, v) in &eval.measured {
                    println!("  {k:<24} {v}");
                }
                println!("INFERRED (heuristic, labelled):");
                for (k, v) in &eval.inferred {
                    println!("  {k:<24} {v}");
                }
            })
        }
        LearnCmd::Benchmark {
            artifact_id,
            prompts,
            actor,
            json,
        } => {
            let result = rt
                .learn_benchmark(artifact_id, prompts, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &result, || {
                println!(
                    "bench {}: ok={}/{} error_rate={} median={:?}ms",
                    result.artifact_id,
                    result.aggregate.ok_count,
                    result.aggregate.count,
                    result.aggregate.error_rate,
                    result.aggregate.median_latency_ms
                );
            })
        }
        LearnCmd::PromoteSkill {
            id,
            operator,
            baseline,
            json,
        } => {
            let baseline = match baseline {
                Some(b) => Some(parse_baseline(b)?),
                None => None,
            };
            let decision = rt
                .learn_promote_skill(id, operator, baseline)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"decision": decision}), || {
                println!("skill `{id}` promotion decision: {decision}");
            })
        }
        LearnCmd::PromoteInstinct {
            id,
            operator,
            baseline,
            json,
        } => {
            let baseline = match baseline {
                Some(b) => Some(parse_baseline(b)?),
                None => None,
            };
            let decision = rt
                .learn_promote_instinct(id, operator, baseline)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"decision": decision}), || {
                println!("instinct `{id}` promotion decision: {decision}");
            })
        }
        LearnCmd::PromoteGenome {
            genome_id,
            operator,
            baseline,
            json,
        } => {
            let baseline = match baseline {
                Some(b) => Some(parse_baseline(b)?),
                None => None,
            };
            let decision = rt
                .learn_promote_genome(genome_id, operator, baseline)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"decision": decision}), || {
                println!("genome `{genome_id}` promotion decision: {decision}");
            })
        }
        LearnCmd::RollbackSkill { id, operator, json } => {
            let out = rt
                .learn_rollback_skill(id, operator)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"result": out}), || {
                println!("skill `{id}` rolled back (recorded + audited)");
            })
        }
        LearnCmd::RollbackGenome {
            current_id,
            previous_id,
            operator,
            json,
        } => {
            let out = rt
                .learn_genome_rollback(current_id, previous_id, operator)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"result": out}), || {
                println!(
                    "genome `{current_id}` rolled back to `{previous_id}` (recorded + audited)"
                );
            })
        }
        LearnCmd::CandidateSkill {
            request_id,
            description,
            permissions,
            session_id,
            actor,
            json,
        } => {
            let id = rt
                .learn_candidate_skill(
                    request_id,
                    description,
                    permissions.clone(),
                    session_id,
                    actor,
                )
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"candidate": id}), || {
                println!("candidate skill submitted: {id} (not activated)");
            })
        }
        LearnCmd::CandidateInstinct {
            session_id,
            actor,
            json,
        } => {
            let id = rt
                .learn_candidate_instinct(session_id, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"candidate": id}), || {
                println!("candidate instinct submitted: {id} (not activated)");
            })
        }
        LearnCmd::CandidateGenome {
            id,
            genome_id,
            summary,
            actor,
            json,
        } => {
            rt.learn_candidate_genome(id, genome_id, summary, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"proposal": id}), || {
                println!("genome evolution proposed: {id} (proposal only)");
            })
        }
        LearnCmd::ModelCreate { path, json } => {
            let spec: openrt_train::ExperimentSpec = read_json_file(path)?;
            let id = rt
                .learn_model_experiment_create(spec)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"run": id}), || {
                println!("training experiment created (planned): {id}");
            })
        }
        LearnCmd::ModelStart { id, json } => {
            let status = rt
                .learn_model_experiment_start(id)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"status": status}), || {
                println!("run `{id}` status: {status}");
            })
        }
        LearnCmd::ModelGet { id, json } => {
            let run = rt
                .learn_model_experiment_get(id)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &run, || {
                println!(
                    "run {} status={} backend={}",
                    run.id,
                    run.status.as_str(),
                    run.backend
                );
                if let Some(reason) = &run.failure_reason {
                    println!("  failure reason: {reason}");
                }
            })
        }
        LearnCmd::ModelList { json } => {
            let runs = rt
                .learn_model_experiment_list()
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &runs, || {
                println!("{} run(s):", runs.len());
                for r in &runs {
                    println!(
                        "  {:<20} {:<10} backend={}",
                        r.id,
                        r.status.as_str(),
                        r.backend
                    );
                }
            })
        }
        LearnCmd::RoutingRecord {
            task_type,
            artifact_id,
            success,
            latency_ms,
            actor,
            json,
        } => {
            rt.learn_routing_record(task_type, artifact_id, *success, *latency_ms, actor)
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &serde_json::json!({"recorded": true}), || {
                println!(
                    "recorded routing signal: {task_type} -> {artifact_id} ({})",
                    if *success { "success" } else { "failure" }
                );
            })
        }
        LearnCmd::RoutingSignals { json } => {
            let signals = rt
                .learn_routing_signals()
                .await
                .map_err(|e| e.to_string())?;
            print_json_or(*json, &signals, || {
                println!("{} routing signal(s):", signals.len());
                for s in &signals {
                    println!(
                        "  {:<16} {:<28} success={}",
                        s.task_type, s.artifact_id, s.success
                    );
                }
            })
        }
        LearnCmd::Promotions { json } => {
            let records = rt.learn_promotions().await.map_err(|e| e.to_string())?;
            print_json_or(*json, &records, || {
                println!("{} promotion decision(s):", records.len());
                for r in &records {
                    println!(
                        "  {:<28} {:<16} {} ({})",
                        r.id,
                        r.kind.as_str(),
                        r.decision.as_str(),
                        r.benchmark.source
                    );
                }
            })
        }
    }
}
