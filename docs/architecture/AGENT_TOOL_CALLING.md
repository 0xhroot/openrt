# Agent tool calling — operator guide

**Date:** 2026-09-29
**Companion docs:** [REAL_AGENT_HARNESS.md](./REAL_AGENT_HARNESS.md) (design
+ acceptance evidence) · [AGENT_TUI.md](./AGENT_TUI.md)

## What happens when a model uses a tool

1. The agent builds the tool list from the `ToolRegistry`, filtering out
   anything your permission policy evaluates as **Deny** (tool id *or* any
   required capability). `Allow` and `Ask` tools stay visible.
2. The schemas are attached to the provider request (`ModelRequest.tools`).
3. The model answers with native tool calls; each call runs through the one
   `GateExecutor` — policy → mode → shield → sandbox → execute.
4. Results (redacted) and denials (structured) go back to the model as tool
   messages; the loop continues until the model answers, or a configured
   limit stops it.

A tool round in the session transcript is four messages:
`User → Assistant(tool_calls) → Tool → Assistant(final)`.

## Your policy decides what the model can see

- **Deny** → the tool is not advertised at all.
- **Ask** → the tool is advertised; calling it opens the approval handshake
  (TUI: `y`/`n`; headless: no resolver ⇒ fails closed to a denial).
- **Allow** → the tool runs under the gate like always.

Default config allows `echo`, `system.info*` and filesystem reads only.
Tools such as `terminal.*` are deny-by-default in a stock profile — if you
want them usable, grant them explicitly (tool id and/or capability, e.g.
`terminal.run.all`):

```json
{ "permissions": { "rules": [
  { "effect": "allow", "capability": "terminal.run" },
  { "effect": "allow", "capability": "terminal.run.all" }
] } }
```

## When a call is denied or fails

Nothing crashes. The model receives a structured tool result
(`{"ok":false,…}`), typically reports the refusal, and the task completes
with the denial in its result. The audit log records `PermissionDenied`;
the tool never executes. Denials are answers, not errors.

## Limits (honest, operator-configured)

| Config | Default | Meaning |
|---|---|---|
| `runtime.max_turns` | 6 | model↔tool rounds per run |
| `runtime.max_tool_calls` | 8 | tool invocations (must be ≥ 1; `0` is a config error) |

Hitting a limit ends the task as **Failed** with an explicit
"stopped at the configured iteration limit" error and a
`[task Failed: …]` marker in the session. Nothing is silently truncated.

## Generation parameters (`runtime.generation`)

Per-run provider-side sampling/output can be pinned in `openrt.json` under
`runtime.generation`. Every field is optional (serde-defaulted); anything set
here is sent as `ModelParameters` on every provider call and is consumed at the
request boundary of `staged_request` — it is never a runtime-side preference
that silently overrides the provider.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `runtime.generation.temperature` | number `[0.0, 2.0]` (finite) | unset (provider default) | sampling temperature |
| `runtime.generation.top_p` | number `[0.0, 1.0]` (finite) | unset | nucleus sampling |
| `runtime.generation.max_tokens` | integer `≥ 1` | unset | max output tokens (wins over `unbounded_output` when both set — `ModelParameters` rule, not re-validated here) |
| `runtime.generation.unbounded_output` | boolean | `false` | let the provider choose output length |
| `runtime.generation.context_window` | integer `≥ 1` | unset | requested context window, **clamped at the request boundary to the provider's declared window when that declaration is smaller** — never forces a window a backend certifies lower (Ollama declares 8192) |
| `runtime.generation.seed` | integer | unset | deterministic sampling seed (provider-supported) |
| `runtime.generation.stop` | string list | `[]` | stop sequences |

Invalid ranges are config load errors (`invalid configuration:
runtime.generation.temperature must be finite in [0.0, 2.0]`, etc.), never
silently accepted. Serialized defaults stay stable on the wire: an unset
`generation` block round-trips as all-null fields.

## Knowing whether your model can call tools

- **`openrt doctor`** → row `model.tools` probes the provider's real model
  metadata (through the same egress gate as generation):
  - `PASS … native tool calling supported`
  - `WARN … no native tool calling` (the model exists, but has no tools —
    tool turns cannot run with it)
  - `UNAVAILABLE … tool support unknown / provider cannot tell / no default
    model` (honest unknown — never a guessed `yes`)
- **TUI, Models view** — the `native tools` line shows `yes` / `no` /
  `unknown` / `unavailable(probe …)` for the selected model, probed live
  through the runtime provider.

## Finding models

- **TUI:** `Ctrl+K → models`, then `Ctrl+F` to enter search mode; type a
  query, `Enter` runs the real registry search, `Esc` leaves search mode.
  Applying a model still means typing a real provider tag
  (`llama3.1:8b`, not the curated card name).
- **CLI:** `openrt model search <query> --hf` lists curated + local models;
  the Hugging Face discovery leg is feature-compiled and mode/network
  gated — under Sovereign/offline it is skipped honestly (curated rows only).

## Evidence

Real-model acceptance runs (happy path, denial, probe, search, limit
validation) and the full gate results are recorded in
[REAL_AGENT_HARNESS.md](./REAL_AGENT_HARNESS.md).
