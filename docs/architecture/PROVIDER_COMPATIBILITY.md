# Provider & model compatibility matrix

**Status:** live-measured. **Date of measurement:** see `evidence/battery_results.json`
`started` timestamps. **Ground truth:** a transparent logging HTTP proxy
(`/tmp/opencode/bat/proxy.py`, `11436 → 11434`) that records one JSON line per
exchange — request tools, message roles, and the provider's actual response
(`message.content`, `message.tool_calls`, `done_reason`). Nothing in the pipeline
is inferred from OpenRT's own reporting.

## Classification rules

| Class | Meaning |
|-------|---------|
| `AVAILABLE` | Native `message.tool_calls` observed **on the wire**, and the tool result round-tripped back to the provider in a later request |
| `PARTIAL_TEXT_ONLY` | Non-empty text, **zero** native tool calls. The model expresses tool intent as prose JSON; nothing was executed |
| `PARTIAL` | Non-empty text **plus** at least one empty assistant response |
| `UNAVAILABLE` | No provider exchange, or a hard failure |
| `BLOCKED` | The gate/policy/sandbox refused the call (observed denial) |
| `TIMEOUT` | Provider did not answer within the budget |
| `PROCESS_EXIT` | `openrt` exited with an unexpected status |
| `UNKNOWN` | Not probed. Always carries the reason it is unknown |

`AVAILABLE` is **only** awarded on wire-observed native tool calls. A model that
talks convincingly about calling a tool is never `AVAILABLE`.

## Battery cases

| Case | Profile | Prompt intent |
|------|---------|----------------|
| `A_read` | read | `filesystem.read notes.txt`, report the three lines |
| `B_list` | read | `filesystem.list` the workspace, report entries |
| `C_write` | exec | `filesystem.write` a new file, confirm |
| `E_terminal` | exec | `terminal.run echo …`, report output |
| `E_deny_read` | exec | `filesystem.read /etc/passwd` — must be **denied** |
| `F_deny_write` | exec | `filesystem.write` outside the workspace — must be **denied** |
| `D_chain` | code | read **then** list in one task (multi-tool, multi-turn) |
| `D_code` | code | write a file **then** read it back |

## Results

| Model | A_read | B_list | C_write | E_terminal | E_deny_read | F_deny_write | D_chain | D_code |
|-------|--------|--------|---------|------------|-------------|--------------|---------|--------|
| `llama3.1:8b` | AVAILABLE | AVAILABLE | AVAILABLE | AVAILABLE | AVAILABLE (denied) | AVAILABLE (denied) | AVAILABLE | AVAILABLE |
| `qwen2.5-coder:7b` | PARTIAL_TEXT_ONLY | PARTIAL_TEXT_ONLY | PARTIAL_TEXT_ONLY | PARTIAL_TEXT_ONLY | PARTIAL_TEXT_ONLY | PARTIAL_TEXT_ONLY | PARTIAL_TEXT_ONLY | PARTIAL_TEXT_ONLY |
| `qwen2.5:7b` | AVAILABLE | AVAILABLE | AVAILABLE | AVAILABLE | AVAILABLE (denied) | AVAILABLE (denied) | AVAILABLE | AVAILABLE (calls denied) |
| `qwen2.5:0.5b` | AVAILABLE | AVAILABLE | AVAILABLE | AVAILABLE | AVAILABLE (denied) | AVAILABLE (denied) | AVAILABLE | AVAILABLE |

"AVAILABLE (denied)" on the two failure-path cases means: the model *did* issue the
native tool call, the gate/sandbox *did* refuse it, and the refusal was fed back to
the model as structured feedback — which the model then reported honestly. That is
the correct outcome for those cases; the classification records that the tool-calling
path itself works.

### Multi-turn evidence (same session, message count per request)

| Suite | Model | Requests | Messages per request | Native tool calls |
|-------|-------|----------|----------------------|-------------------|
| P1→P3 | `qwen2.5-coder:7b` | 1 each | 2 → 4 → 6 | 0 |
| L1→L3 | `llama3.1:8b` | 2 each | 2,4 → 6,9 → 11,13 | 1, 2, 1 |

`llama3.1:8b` L3's final request carries roles
`system, user, assistant, tool, assistant, user, assistant, tool, tool, assistant, user, assistant, tool`
— the tool-result round trip is real, with `tool_call_id` correlation intact.

### Strongest single result

`qwen2.5:7b` / `D_code`: **5 native tool calls across 4 provider exchanges**
(`write, read, write, write, read`) in a single task, without TUI or process
termination. This satisfies the harness completion criterion — multiple native
tool calls across multiple turns.

**Honest caveat on that run:** all five calls were *refused by the ResourceGate*,
not satisfied — this host sits at ~50.4% host memory against a 50.0% limit, so
admission was denied. The result string says so plainly ("the resource limit is
consistently preventing both the write and read operations due to the current
memory usage"). The criterion being demonstrated is the **tool-calling and
tool-result round-trip path**, not successful file mutation.

### Denials across the battery

21 of 42 cases ended with the model reporting a denial or resource limitation;
11 of those cited the host-memory ResourceGate rather than a policy rule. This is
an environment property (15 GB host, 50% default memory ceiling), not a provider
defect, and the refusals were always surfaced to the model as structured feedback
and reported honestly. A host under the memory ceiling would flip most of those
to executed calls; the classification deliberately separates "the model emitted a
native call" from "the call was admitted".

## Per-model notes

### `llama3.1:8b` — AVAILABLE
Native tool calling across every case. Correctly reports denials verbatim
("refused due to a permission denied by the AgentShield approval").

### `qwen2.5:7b` — AVAILABLE
The strongest model in the battery: multi-tool, multi-turn, and honest about
sandbox scope. One observed denial cited the ResourceGate host-memory limit
(`exceeding the host memory usage limit of 50.0%`) — an environment-dependent
refusal reason, reported accurately by the model.

### `qwen2.5:0.5b` — AVAILABLE
Surprising but reproducible: the smallest model in the set emits correct native
tool calls. One `D_chain` run took 71 s (first-load cost), not a hang.

### `qwen2.5-coder:7b` — PARTIAL_TEXT_ONLY
Returns tool calls as **JSON in the message content**, e.g.

```json
{"name": "filesystem.read", "arguments": {"path": "/tmp/opencode/bat/store/notes.txt"}}
```

with `done_reason: stop` and no `message.tool_calls`. OpenRT does not execute this —
the task completes with the raw text as the result and **no tool ever runs**. That
is the intended safety behaviour, not a bug, but it means the model is not
`AVAILABLE`. Notably, an earlier run of the same case (before the permission rules
were corrected) shows exactly the danger: the model *hallucinated* the tool output
inline (`{"ok":true,"items":[...]}`) when no tools were advertised at all. OpenRT
must never execute that text.

## Reproducing

```bash
python3 /tmp/opencode/bat/proxy.py 11436 /tmp/opencode/bat/proxy.log &
python3 /tmp/opencode/bat/gen_cfg.py            # per-model configs -> proxy
python3 /tmp/opencode/bat/battery.py matrix     # A/B/C/E/F across 4 models
python3 /tmp/opencode/bat/battery.py code       # D chain + code
python3 /tmp/opencode/bat/battery.py coder3     # 3 prompts, one session
python3 /tmp/opencode/bat/battery.py llama25    # llama3.1 regression, one session
```

Per-run artifacts land in `/tmp/opencode/bat/runs/<case>__<model>__<profile>/`
(`record.json`, `ground_truth.json`); the aggregate is
`/tmp/opencode/bat/evidence/battery_results.json` and
`/tmp/opencode/bat/evidence/model_matrix.json`.

## Not covered

* Vision / multimodal rows: `UNKNOWN` for every model here — no image was sent, so
  no vision claim is made. Probe with `openrt model` / the Models view instead.
* Streaming tool calls: the battery uses non-streaming `/api/chat`. The
  `streaming` row in the Models view reports the provider's advertised capability,
  not a measured streaming transcript.
* Context-window limits at scale: no run exceeded ~13 messages.
* Models not present on this host (`moondream:latest`, `all-minilm:latest`) were not
  battery-tested: they are embedding/small-vision models, not tool callers. Their
  rows are `UNKNOWN`, not `UNAVAILABLE`.