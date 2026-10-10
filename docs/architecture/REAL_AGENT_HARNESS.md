# Real agent harness (native tool calling)

**Status:** Implemented and accepted against a live model
**Date:** 2026-09-29
**Companion docs:** [AGENT_TOOL_CALLING.md](./AGENT_TOOL_CALLING.md) (operator
guide) · [AGENT_TUI_ARCHITECTURE.md](./AGENT_TUI_ARCHITECTURE.md) ·
[OWNERSHIP.md](./OWNERSHIP.md)

## Mission

Until this change, OpenRT's agent stack was a *simulated* harness: tools,
gates, shield, sandbox and approvals were all real, but the model never saw a
tool schema, so a live provider could never emit `tool_calls` — tool behavior
was proven only with scripted providers. This work closes exactly that gap:

> The model receives the policy-filtered tool schemas → answers with native
> tool calls → every call is validated through the existing
> `ToolRegistry` → `GateExecutor` → `Policy` → `AgentShield` → `Sandbox` →
> the structured result (or structured denial) goes back to the model →
> multi-turn loop until a final answer, within honest operator-configured
> limits.

## Hard constraints (honored)

No second runtime, agent engine, model router, tool registry, policy engine,
shield, sandbox or persistence layer was created, and no security test was
weakened. Every addition extends exactly one existing authority:

| Area | Single authority extended |
|---|---|
| Tool schemas on the wire | `openrt-models` (`ModelRequest.tools`, ollama/openai payloads) |
| Tool loop | `openrt-agents::minimal::MinimalAgent` (the one agent) |
| Runtime limits | `openrt-config::RuntimeConfig` + `Runtime` turn limits |
| Advertisement filtering | `openrt-runtime` (policy evaluation, deny-only) |
| Capability probing | `ModelProvider::probe_tool_support` + `openrt-orch` doctor row |
| Redaction before the model | `openrt-core::redaction::RedactionPolicy` |
| TUI surfaces | `openrt-tui/src/agent/` (search mode, native-tools line) |

## Flow

```
prompt ──► MinimalAgent
             │  .with_tools(policy-filtered ToolSpec list)
             ▼
        provider.generate(ModelRequest{messages, tools})   ← wire contract
             │
             ├─ no tool_calls ──────────────────────────────► final answer
             │
             └─ tool_calls[] ──► per call, sequentially:
                   GateExecutor (policy → Sovereign → shield → sandbox)
                     ├─ denied ─► structured JSON {"ok":false,…} fed back
                     ├─ error   ─► structured JSON {"ok":false,…} fed back
                     └─ ok      ─► redacted ToolOutput::render() fed back
                                 (only KillSwitch/Cancellation is fatal)
             loop, bounded by runtime.max_turns / runtime.max_tool_calls
```

Session transcript for one tool round is four messages:
`User → Assistant(tool_calls) → Tool → Assistant(final)`. The pipeline folds
the transcript delta into the session (or falls back to user + final answer
when the provider produced no delta).

## Design decisions

1. **Tool failures are model-visible, not fatal.** Execution errors, denials
   and gate rejections all come back as structured tool-result JSON so the
   model can adapt. Only `ToolError::Cancelled` (and kill-switch refusal,
   which the gate re-blocks on the next call) ends the loop.
2. **Denied calls complete the task.** A policy denial is an *answer* the
   model reports to the operator (mission PART 16/20): the task completes
   with the refusal in the result; the tool never executes and
   `PermissionDenied` is audited.
3. **Iteration limits fail honestly.** Hitting `runtime.max_tool_calls`
   (default 8, validated ≥ 1) ends the task as **Failed** with an explicit
   "stopped at the configured iteration limit" error and a
   `[task Failed: …]` marker in the session — never a silent truncation.
4. **Advertisement is deny-only policy filtering.** A tool id (or any
   required capability) evaluating `Deny` is withheld from the schema list;
   `Ask` and `Allow` stay visible so the operator's approval handshake can
   happen. Empty-args calls cannot be predicted by the filter — the gate
   remains the enforcement point; advertisement is only hygiene.
5. **Redaction is bidirectional.** `ToolOutput::render()` passes through
   `RedactionPolicy::redact_text` before it reaches the model or the
   transcript (JSON-by-field, line-wise otherwise).
6. **Capability probes are gated network calls.** `probe_tool_support`
   goes through `EgressGuardedProvider::allow_transport` — Sovereign refuses
   a non-loopback probe pre-socket with the same audited `model.egress`
   reason as `generate`.
7. **Honesty over name-guessing.** The TUI `native tools` line and the
   `doctor model.tools` row report only what the provider's `/api/show`
   (or equivalent) actually answered: `yes` / `no` / `unknown` /
   `unavailable`.

## Surfaces

- **CLI:** `openrt task run --json`, `openrt session show`, `openrt doctor`
  (`model.tools` row), `openrt model search <q> --hf` (HF feature compiled
  in; the discovery leg itself stays mode/network gated and honest).
- **TUI Models view:** `Ctrl+F` toggles real registry search (Enter runs
  it, Esc leaves search mode); the `native tools` line is probed through
  the runtime provider.
- **Config:** `runtime.max_tool_calls` (serde-defaulted, validated).

## Security properties preserved (verified)

- Deny-by-default: default config still carries only allow rules for
  echo/system/filesystem read; a denied tool never reaches
  `ToolInvocationStarted` (`agent_harness::denied_tool_is_withheld…`,
  `…structured_feedback…`, `gate_deny`, `p2_2_security`).
- Ask without a resolver fails closed to `PermissionDenied` — in the TUI
  the resolver answers, in headless `task run` nothing does.
- Model outputs/tool outputs are redacted before they reach the model.
- No `openrt.json` is created at the repository root (acceptance ran under
  `OPENRT_CONFIG=/tmp/openrt-agent-accept/accept.json`).

## Acceptance evidence (live Ollama, `llama3.1:8b`)

Script: `/tmp/opencode/agent_acceptance.sh` (state isolated under
`/tmp/openrt-agent-accept`). Full pass, RC=0:

| # | Scenario | Evidence |
|---|---|---|
| T1 | Real tool call | Task **Completed**; model answered "The echo tool returned the text `AGENT-OK-HARNESS` as expected."; store contains `PermissionGranted` + `ToolInvocationCompleted`; session transcript = **4 messages** |
| T2 | Ask-without-resolver denial | Model reported: tool "requires approval … but no resolver is configured"; result contains the structured refusal; `PermissionDenied` audited; **no** `ToolInvocationStarted` |
| T3 | Capability probe | `PASS model.tools  model \`llama3.1:8b\`: native tool calling supported` |
| T4 | Model search (hf feature on) | Curated `llama3.1-8b`/`llama3.2-1b` rows listed; HF leg honestly mode-gated |
| T5 | Limit validation | `error: invalid configuration: runtime.max_tool_calls must be >= 1` |
| — | Honest negative | With `moondream:latest`: `WARN model.tools … no native tool calling — tool turns cannot run with this model` |

## Gates (2026-09-29)

```
cargo fmt --all                    RC=0
cargo check --workspace --offline  0 errors / 0 warnings
cargo test  --workspace --offline  1571 passed / 0 failed
openrt doctor                      89 rows: 76 PASS / 1 WARN / 12 UNAVAILABLE / 0 FAIL
```

Full 20-test live-model suite evidence and capability matrix:
[REAL_HARNESS_ACCEPTANCE.md](./REAL_HARNESS_ACCEPTANCE.md).

New tests this round (plus updates where behavior intentionally changed):
`integration-tests/tests/agent_harness.rs` (5: policy-filtered advertisement,
withheld denial, transcript persistence, structured-denial-alive, honest
iteration-limit failure + `zero_max_tool_calls_is_rejected`),
`minimal.rs` (tool rounds in transcript, denial-not-fatal, `max_tool_calls`,
tools reach the request, redaction before the model),
`redaction.rs` (`redact_text` ×3), `runtime.rs`
(`advertised_tools_filters_policy_denied`), `openrt-models`
(tools round-trip / wire envelope / pass-through / exact model id /
legacy request), `model_egress.rs` (probe forwarded; Sovereign refuses a
probe pre-socket), TUI (`ctrl_f_model_search_runs_real_registry_search`,
`native_tools_probe_is_honest_unknown_for_mock`).

## Residuals

1. **Advertisement filter is hygiene, not security** — an empty-args call on
   a still-advertised tool is evaluated by the gate like any other call.
2. **`probe_tool_support` is per-provider best effort** — providers without
   an implementation answer `unknown` (default trait impl), rendered as
   `unknown`/`unavailable`, never as `yes`.
3. **HF discovery** stays feature-gated + mode/network gated; acceptance ran
   with the feature compiled in under Sovereign, so only curated rows were
   listed (by design, honestly).
4. **Windows untested** (no claim) — Linux only, as everywhere else in this
   repository.
