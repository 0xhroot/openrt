# OpenRT Documentation Index

OpenRT V1 is a large system; the documents below are the engineering record
that explains how it is built, what was validated, and where the honest
limits are. Start with the [project README](../README.md) for the
introduction, then use this index to navigate.

Validation vocabulary used throughout:

| Label | Meaning |
|---|---|
| **Implemented** | Code complete in this repository |
| **Tested (mock)** | Exercised by the unit/integration suite (deterministic mock provider) |
| **Verified (real provider)** | Additionally exercised against a real local provider (Ollama) on the development host |
| **Experimental** | Implemented with limited validation |
| **Not validated** | Known gap; see [Known limitations](#known-limitations) |

---

## Architecture

| Document | Contents |
|---|---|
| [OWNERSHIP.md](architecture/OWNERSHIP.md) | which phase owns which crate / namespace (allocation map) |
| [FINAL_ARCHITECTURE.md](architecture/phase13/FINAL_ARCHITECTURE.md) | final layer stack + domain mapping |
| [PLAN_ERROR_HANDLING.md](architecture/PLAN_ERROR_HANDLING.md) | cross-cutting error model plan |
| [adr/ADR-001…ADR-010](architecture/adr/) | architecture decision records: artifact provenance, audit envelope, offline scanning, enclave, observability, storage ownership, naming, kill switches, expert cache, model fabric & modes |
| [phase1/](architecture/phase1/) | core runtime: lifecycle, error model, event model, audit graph/flows, testing |
| [MILESTONE_C_LONG_RUNNING_AGENT.md](architecture/MILESTONE_C_LONG_RUNNING_AGENT.md) | long-running agent milestone design |

## Agent execution

| Document | Contents |
|---|---|
| [AGENT_TOOL_CALLING.md](architecture/AGENT_TOOL_CALLING.md) | agent ↔ tool-call protocol and parsing |
| [AGENT_RECOVERY.md](architecture/AGENT_RECOVERY.md) | task pause/resume/recover semantics |
| [PRODUCTION_HARNESS.md](architecture/PRODUCTION_HARNESS.md) | production test harness design |
| [REAL_AGENT_HARNESS.md](architecture/REAL_AGENT_HARNESS.md) | real-provider agent harness |
| [REAL_HARNESS_ACCEPTANCE.md](architecture/REAL_HARNESS_ACCEPTANCE.md) | real-provider harness acceptance evidence |
| [phase2/](architecture/phase2/) | agent orchestration, computer use, sandbox, kill switch, checkpoint recovery, final integration audit |
| [phase6/](architecture/phase6/) | workflow compiler (compile / inspect / validate / run) |
| [phase7/](architecture/phase7/) | computer control (observe → resolve → execute under one gate) |
| [phase10/](architecture/phase10/) | boot recovery, skills, AgentShield, rooms |

## Agent TUI (terminal workbench)

| Document | Contents |
|---|---|
| [AGENT_TUI.md](architecture/AGENT_TUI.md) | Agent Mode TUI overview |
| [AGENT_TUI_ARCHITECTURE.md](architecture/AGENT_TUI_ARCHITECTURE.md) | TUI internals |
| [AGENT_TUI_KEYBINDINGS.md](architecture/AGENT_TUI_KEYBINDINGS.md) | keybinding reference |
| [AGENT_TUI_ACCEPTANCE.md](architecture/AGENT_TUI_ACCEPTANCE.md) | TUI acceptance evidence |
| [TUI_ACCEPTANCE.md](architecture/TUI_ACCEPTANCE.md) | Workbench TUI acceptance |
| [KEYBOARD.md](architecture/KEYBOARD.md) | keyboard accessibility notes |
| [tui-design-system.md](tui-design-system.md) | design tokens, layout primitives |
| [tui-layout.md](tui-layout.md) | layout system |
| [tui-themes.md](tui-themes.md) | theming |
| [tui-interaction.md](tui-interaction.md) | interaction model |
| [tui-accessibility.md](tui-accessibility.md) | accessibility guarantees |

## Memory Fabric

| Document | Contents |
|---|---|
| [MEMORY_FABRIC.md](architecture/MEMORY_FABRIC.md) | Memory Fabric design |
| [m6/M6_RUNTIME_MEMORY_HANDOFF.md](m6/M6_RUNTIME_MEMORY_HANDOFF.md) | runtime + memory handoff record |
| [m7/M7_MULTI_SHARD_ANATOMY.md](m7/M7_MULTI_SHARD_ANATOMY.md) | multi-shard memory anatomy |
| [phase5/](architecture/phase5/) | workspaces, sessions, history, memory (M5.x) |

## Security and permissions

| Document | Contents |
|---|---|
| [SECURITY_THREAT_MODEL.md](architecture/phase13/SECURITY_THREAT_MODEL.md) | assets, threats, mitigations, residual risks |
| [SECURITY_CONTROL_CENTER.md](architecture/SECURITY_CONTROL_CENTER.md) | security control center design |
| [PERMISSIONS_UX.md](architecture/PERMISSIONS_UX.md) | permission prompts and operator UX |
| [phase2/P2_1…P2_2](architecture/phase2/) | security hardening; permissions & policy |
| [phase8/](architecture/phase8/) | sandbox authority (bubblewrap-backed) |
| [phase9/](architecture/phase9/) | knowledge security, injection detection, hallucination firewall |

## Providers and model management

| Document | Contents |
|---|---|
| [MODEL_REGISTRATION.md](architecture/MODEL_REGISTRATION.md) | model registration + artifact provenance (ADR-001) |
| [MODEL_LIBRARY.md](architecture/MODEL_LIBRARY.md) | curated offline model library |
| [MODEL_BROWSER.md](architecture/MODEL_BROWSER.md) | in-TUI model browser |
| [PROVIDER_COMPATIBILITY.md](architecture/PROVIDER_COMPATIBILITY.md) | provider compatibility notes (Ollama / llama.cpp / vLLM) |
| [OLLAMA_TOOL_CALLING.md](architecture/OLLAMA_TOOL_CALLING.md) | Ollama tool-calling wire format |
| [phase3/](architecture/phase3/) | model fabric, registry lifecycle DAG, provisioning/sovereign modes |

## Knowledge, retrieval, verification

| Document | Contents |
|---|---|
| [phase9/](architecture/phase9/) | ingest, hybrid retrieval, evidence graph, verifier |
| [m5/M5_VERIFY_HANDOFF.md](m5/M5_VERIFY_HANDOFF.md) | verification subsystem handoff |

## Milestone reports (Phases 1–13)

Each phase directory under [`architecture/`](architecture/) contains an
implementation record and an acceptance/gate report:

| Phase | Theme |
|---|---|
| [phase1](architecture/phase1/) | core runtime + audit |
| [phase2](architecture/phase2/) | agent + computer use |
| [phase3](architecture/phase3/) | model fabric + sovereign infrastructure |
| [phase4](architecture/phase4/) | phase-4 planning, interfaces, K6 gate audit |
| [phase5](architecture/phase5/) | workspaces / sessions / memory / webui |
| [phase6](architecture/phase6/) | workflow compiler |
| [phase7](architecture/phase7/) | computer control |
| [phase8](architecture/phase8/) | sandbox |
| [phase9](architecture/phase9/) | knowledge / RAG / verification |
| [phase10](architecture/phase10/) | recovery / skills / shield |
| [phase11](architecture/phase11/) | workbench / extensions / MCP |
| [phase12](architecture/phase12/) | industrial + trading research + learning |
| [phase13](architecture/phase13/) | SIH hero demo + final architecture & acceptance |

Additional reports moved from the repository root into
[`milestone-reports/`](milestone-reports/):

- [`PHASE2_FINAL_REPORT.md`](milestone-reports/PHASE2_FINAL_REPORT.md) — Phase 2 gate decision
- [`PHASE3_FINAL_REPORT.md`](milestone-reports/PHASE3_FINAL_REPORT.md) — Phase 3 gate decision
- [`P2_6_FINAL_REPORT.md`](milestone-reports/P2_6_FINAL_REPORT.md) — checkpoint/recovery milestone report
- [`project-complete-plan.txt`](milestone-reports/project-complete-plan.txt) — original ASTRA Local master plan (SIH 26117)

Roadmap: [`roadmap/ROADMAP.md`](roadmap/ROADMAP.md).

## Testing and validation

| Document | Contents |
|---|---|
| [phase1/TESTING.md](architecture/phase1/TESTING.md) | core testing strategy |
| [PRODUCTION_HARNESS.md](architecture/PRODUCTION_HARNESS.md) | production harness |
| [REAL_HARNESS_ACCEPTANCE.md](architecture/REAL_HARNESS_ACCEPTANCE.md) | real-provider acceptance runs |
| [TUI_ACCEPTANCE.md](architecture/TUI_ACCEPTANCE.md) / [AGENT_TUI_ACCEPTANCE.md](architecture/AGENT_TUI_ACCEPTANCE.md) | TUI gates |
| [docs/RELEASE_AUDIT.md](RELEASE_AUDIT.md) | V1 release preflight: secret scan, large files, validation commands, staging plan |

## Screenshots

Genuine captures of the running application (Agent Mode, Models view,
permission gate, task completion, Workbench timeline):
[`assets/screenshots/`](assets/screenshots/).

## Known limitations

The authoritative list lives in the
[README § Known limitations](../README.md#known-limitations) and in
[RELEASE_AUDIT.md](RELEASE_AUDIT.md). Headlines:

- Linux x86_64 only; Windows/macOS untested, no claim made.
- No CI configuration; all gates were run manually on the development host
  and recorded in the acceptance docs.
- Live streaming and live context-compaction paths are **not validated**
  end-to-end against the current Ollama provider build (see README).
- Capabilities honestly report `UNAVAILABLE` (e.g. missing backend) instead
  of faking green; `openrt doctor` is the posture authority.
