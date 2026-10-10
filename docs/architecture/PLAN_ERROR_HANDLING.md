# `/plan` error handling — structured, session-alive, never a crash

**Status:** Accepted · **Date:** 2026-09-30
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md)

`/plan <goal>` compiles through the ONE workflow compiler
(`Runtime::compile_workflow`). Compiler and policy rejections are **results**,
not fatal errors: the event loop keeps running, the session keeps accepting
input, and the failure is rendered as a structured block in the chat stream.

## Contract

1. **`compile_plan` returns `Ok(())` on every failure path.** A bad goal,
   a denied tool, a missing backend — none of these may reach `handle_key`
   as an `Err` (that used to take the process down). Only genuine I/O of the
   UI layer itself can error.
2. **On failure** the app pushes `MsgKind::PlanFailed { goal, code, reason, fix }`
   and clears `plan`, `compiled`, and `plan_goal` — there is never a stale
   plan left behind for `/run` to execute.
3. **`code`** is the compiler's real `DiagnosticCode` string
   (`unsupported_goal`, `ambiguous_goal`, `permission_denied`,
   `unavailable_tool`, `missing_capability`, …) — never an invented label.
   **`reason`** lists every diagnostic with its `what/why/at/fix`;
   **`fix`** is the first error's `how_to_fix`.
4. **Status line** becomes
   `PLAN FAILED — r = retry · e = edit · Esc = dismiss`.
5. **Recovery keys** (Chat view, empty input, no filter/prompt active):

   | Key | Action |
   |---|---|
   | `r` | retry — re-runs `compile_plan` with the same goal |
   | `e` / `Enter` | restores `/plan <goal>` into the input for editing |
   | `Esc` | dismisses the hook; the PLAN FAILED block stays in history |

6. **Fail closed, on purpose.** A rejection caused by policy (e.g. an
   artifact backend that is `ask`/not allowed under the active profile) is
   shown as `permission_denied` with the real tool id in the reason — the
   fix is an operator policy decision, not a compiler workaround.
   `unavailable_tool` (e.g. an image/pdf generation backend that does not
   exist on this install) is likewise uncorrectable by the LLM replan list:
   the plan does not compile until the tool exists.

## Render

```
✗ PLAN FAILED [permission_denied]
  <diagnostics: [code] what · why · at · fix …>
  [r] retry · [e] edit · [Esc] dismiss
```

The block is part of the chat stream (scrolls with it, persists after
dismissal); the error path never writes to stdout and never unwinds the
event loop.

## Tests

- `plan_rejection_is_structured_and_session_stays_alive` — code
  `permission_denied`, reason names the tool, plan cleared, typing still
  works.
- `plan_failure_recovery_r_e_esc` — `r` retries (failure count grows via the
  scripted provider, not a panic), `e` restores the goal for editing, `Esc`
  dismisses without rewriting history.
- `plan_compiles_when_artifact_backends_are_allowed` — with the backend
  allowed the same goal compiles to real `generate`/`persist` steps, proving
  the failure above is a policy answer, not a vocabulary gap.

Compiler-side coverage lives in
`crates/openrt-compiler` (`artifact_create_synonyms_compile_to_backend_plan`,
`pdf_without_office_backend_fails_closed_as_unavailable_tool`,
`image_generation_backend_is_honestly_unavailable`,
`bare_topic_without_action_or_artifact_fails_closed`).
