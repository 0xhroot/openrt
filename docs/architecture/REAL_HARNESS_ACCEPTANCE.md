# Real harness acceptance — what was measured, and what is still NOT proven

**Scope:** the Ollama tool-calling work. This document is deliberately explicit
about the boundary between *measured* and *asserted*, because the difference is
the whole point of the work.

## The claim being made

> A real local model performs multiple **native** tool calls across **multiple**
> turns, inside OpenRT, without the TUI or the process terminating.

**Status: SUPPORTED** — with the caveat recorded under `qwen2.5:7b` / `D_code`
below (the calls were made and round-tripped; they were refused by the
ResourceGate on this host).

**Status: NOT SUPPORTED for every model.** `qwen2.5-coder:7b` does not produce
native tool calls at all. It is `PARTIAL_TEXT_ONLY`. No claim is made for it.

## Measured evidence

| # | Claim | Evidence | Where |
|---|-------|----------|-------|
| 1 | Native tool calls reach the executor and round-trip back to the provider | `tool` role present in a later request, correlated by `tool_call_id` | `runs/L3__llama3_1_8b__exec/ground_truth.json` |
| 2 | Multiple native calls in one task across multiple exchanges | 5 calls / 4 exchanges | `runs/D_code__qwen2_5_7b__code/ground_truth.json` |
| 3 | Session continuity across separate `openrt` invocations | messages per request 2→4→6 (P1→P3), 2,4→6,9→11,13 (L1→L3), same session id | `evidence/battery_results.json` |
| 4 | Denials are enforced and surfaced as model feedback | model text quotes the refusal verbatim | `E_deny_read`, `F_deny_write` result strings |
| 5 | An empty provider response fails the task instead of completing it | `MockMode::Empty` → `TaskStatus::Failed`, `result: None`, session reusable | `openrt-runtime` test `empty_response_failure_fails_task_and_session_stays_usable` |
| 6 | Malformed ids/arguments never execute | 12 `toolcall.rs` tests + 2 agent-loop tests (`raw_text_arguments_never_execute`, `scalar_arguments_never_execute`) | unit suite |

## Not proven — stated plainly

* **Not** proven for `qwen2.5-coder:7b`. It emits tool calls as prose JSON. OpenRT
  does not execute that text (by design), so there is no executed tool call to
  point at for this model.
* **No successful file mutation** is proven on this host: 11 of 42 cases were
  refused by the ResourceGate at ~50.4% host memory against a 50.0% ceiling. The
  tool-calling path is proven; the *side effect* is not.
* **Vision** rows are `UNKNOWN` for every model — no image was sent. No vision
  claim is made.
* **Streaming** tool calls were not exercised; the battery uses non-streaming
  `/api/chat`. The `streaming` row reports advertised capability, not a measured
  streaming transcript.
* **Long-context** behaviour is untested — the largest transcript was 13 messages.
* **`moondream:latest` / `all-minilm:latest`** were not battery-tested. Their rows
  are `UNKNOWN`, not `UNAVAILABLE`.

## Gates run

| Gate | Result |
|------|--------|
| `cargo fmt --all` | clean |
| `cargo check --workspace --all-targets --offline` | 0 warnings, 0 errors |
| `cargo test --workspace --offline` | 1728 passed, 0 failed |
| `openrt doctor` | provider / tool-calling / tool-schema / tool-response-parsing rows present and honest |

Known-flaky integration tests (`integration-tests` package) intermittently fail
with `ResourceDenied { reason: "host memory usage 50.x% exceeds limit 50.0%"}` and
`Checkpoint(Refused("required resources are not available"))`. This is the same
host-memory ceiling as above, is unrelated to the tool-calling changes, and is
reproducible by running the suite while the host sits at the limit. It is
recorded rather than suppressed.

## If you want to close the gaps

1. Free host memory below 50% (or raise `resources.max_memory_bytes` in the
   config) and re-run `battery.py matrix` — the "calls denied" cases should flip
   to executed mutations.
2. Add a streaming battery case (`/api/chat` with `"stream": true`) to convert the
   `streaming` row from advertised to measured.
3. Send one image per model to convert the `vision` rows from `UNKNOWN`.
4. Drive `qwen2.5-coder:7b` with a prompt that induces the native `tool_calls`
   field (some coder templates emit it only when the tool schema uses their exact
   parameter names) — if it still returns prose, `PARTIAL_TEXT_ONLY` is the honest
   permanent classification.