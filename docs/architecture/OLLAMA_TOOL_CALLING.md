# Ollama Tool Calling — wire contract, failure classes, and fixes

**Status:** implemented. **Owner:** `crates/openrt-models`, `crates/openrt-agents`.
**Evidence:** live battery against local Ollama (`/tmp/opencode/bat/evidence/`), unit
regressions in `crates/openrt-models/src/{toolcall,ollama,openai}.rs` and
`crates/openrt-agents/src/minimal.rs`.

## 1. What actually broke

The literal string `Expected 'id' to be a string.` does **not** exist in the
OpenRT source, binaries, cargo state, databases or logs. It exists in exactly two
places outside this repository:

* `~/.local/share/opencode/log/opencode.log` — Vercel AI SDK
  `AI_InvalidResponseDataError`, `providerID=opencode modelID=mimo-v2.6-flash-free`
* `streaming-tool-call-tracker.ts:163` in the Vercel AI SDK

So the message an operator saw came from a **different runtime** (opencode +
`mimo-v2.6-flash-free`), not from OpenRT. The OpenRT-relevant failure class it
names — *a provider returned a tool-call payload whose schema does not match, and
the failure surfaced as an opaque validation error instead of an actionable
model error* — was real in OpenRT and is what this work fixed. The regression
tests therefore target the **equivalent failure class**, not the literal string,
and the report states this origin honestly rather than claiming a false root
cause.

## 2. Failure classes OpenRT now handles explicitly

| # | Failure | Old behaviour | Now |
|---|---------|----------------|-----|
| 1 | Assistant response with **no text and no tool calls** | Task `Completed` with an **empty fabricated result** — a silent success | Bounded retry (`provider_empty_response_retries`, default 1) on the *same* conversation, then `AgentError::Model("MODEL_TOOL_CALL_PARSE_ERROR: provider returned an empty assistant response without a tool call (retries exhausted)")` → task `Failed`, no result, session stays usable |
| 2 | `tool_call.id` missing / `null` / `""` / `"   "` / boolean | Hard validation error (the `Expected 'id'` class) | Canonical string id minted: `openrt-call-{unix_ms}-{AtomicU64}` |
| 3 | `tool_call.id` numeric (`42`, `7.5`) | Hard validation error | Decimal string (`"42"`, `"7.5"`) |
| 4 | `arguments` missing / `null` | Hard validation error | Normalised to `{}` |
| 5 | `arguments` a JSON **string** | Executed as raw text — a prompt-injection execution surface | Parsed **only** when the result is an object or array; otherwise the raw value reaches `validate_tool_input()`, which rejects String/Number/Bool with `ToolError::InvalidArguments`. The executor is never reached |
| 6 | Tool-call JSON appearing in **normal assistant text** (observed live on `qwen2.5-coder:7b`) | Could be executed by a lenient parser | Never executed. The agent only runs what arrives in the provider's structured `tool_calls` field |
| 7 | Assistant message carrying both content and tool calls | Malformed wire | Request builder emits **content XOR tool_calls** |
| 8 | Tool role result without correlation | Uncorrelated transcript | Tool role always carries `tool_call_id` |
| 9 | `finish_reason` disagreeing with the payload | Tool calls dropped | Tool calls execute regardless of `finish_reason` |

## 3. Canonical id semantics (`crates/openrt-models/src/toolcall.rs`)

```
non-empty string   → verbatim
number             → decimal string
missing / null     → openrt-call-{unix_ms}-{counter}
"" / whitespace    → openrt-call-{unix_ms}-{counter}
boolean            → openrt-call-{unix_ms}-{counter}   (never "true"/"false")
```

Booleans are deliberately *not* stringified: `"true"` is a syntactically valid id
that silently correlates unrelated calls.

## 4. Argument guard (`normalize_arguments` + `validate_tool_input`)

```
missing / null            → {}
object                    → passthrough
array                     → passthrough
string that parses to {} or [] → parsed
string that parses to a scalar / does not parse  → kept raw, then REJECTED
number / bool / native scalar                    → kept raw, then REJECTED
```

Rejection is a **structured tool failure** (`ToolError::InvalidArguments`),
returned to the model as feedback. `tool_calls_used` increments regardless, so a
tool-looping model cannot evade the budget by sending malformed arguments.

## 5. Retry layering

| Layer | Field | Default | Owner |
|-------|-------|---------|-------|
| Agent, per turn | `AgentTurnLimits.provider_retries` | 1 (`role_agents()` sets 0) | interactive task path |
| Agent, per turn | `AgentTurnLimits.provider_empty_response_retries` | 1 | empty-response recovery (BUG 1) |
| Workflow engine | node retries / `RecoveryPolicy` | — | `role_agents()` sets `provider_retries: 0` so attempt counts stay coherent |

Permanent errors (`ModelError::is_permanent` — `NotConfigured`, `ModelNotFound`,
`InvalidResponse`) **never** retry. Retries reuse the same conversation and state,
so a retry cannot double-apply a side effect: no tool has run at that point.

## 6. Diagnosis surfaces

* `openrt doctor` — `provider.tool_calling`, `tool_schema`, `tool_response_parsing`
  rows. The parser self-test runs the *production* `parse_chat_response()` on a
  canned payload, so a parser regression fails in doctor, not at runtime.
* `/debug model-response` — the last raw provider body (64 KB cap), passed through
  `RedactionPolicy` before display.
* Models view rows: provider, scope, model id, context window, native tools,
  vision, streaming, provider status — each `AVAILABLE` / `PARTIAL` / `UNAVAILABLE` /
  `BLOCKED` / `UNKNOWN` **with the reason it is unknown**. Capability probing runs on
  a background thread (`spawn_probe()`); the TUI never blocks on it.
* TUI **MODEL ERROR** card on every provider/model failure, with
  `r` retry / `m` switch model / `v` details / `Esc` dismiss. Headline
  classification now covers parse failures including
  `MODEL_TOOL_CALL_PARSE_ERROR` → *"Could not parse the tool call."*

## 7. Known limitation, stated honestly

`qwen2.5-coder:7b` expresses tool calls as **prose JSON in the message content**
(`{"name": "filesystem.read", "arguments": {...}}`) rather than in Ollama's
`message.tool_calls` field. OpenRT does **not** execute this text — that is the
argument guard doing its job. Such a model is classified `PARTIAL_TEXT_ONLY`, not
`AVAILABLE`, because nothing was actually executed. See
[`PROVIDER_COMPATIBILITY.md`](PROVIDER_COMPATIBILITY.md).