# Permissions UX — the approval card and its tiers

**Status:** Accepted · **Date:** 2026-10-01
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md) · **Companion:** [PRODUCTION_HARNESS.md](./PRODUCTION_HARNESS.md),
[SECURITY_CONTROL_CENTER.md](./SECURITY_CONTROL_CENTER.md)

How a gated tool call reaches the operator, what each answer means, and
where the answer is enforced. The enforcement point is the ONE gate —
`GateExecutor::resolve_ask` — nothing here bypasses or forks it.

## 1. The card

When the policy (or a tool with no matching rule) resolves to **Ask**, the
request is queued to the TUI and drawn as a card:

```
┌ PERMISSION REQUIRED ───────────────────────────────┐
│ OpenRT wants to run                                │
│ tool      fs.write                                 │
│ scope     workspace ~/Projects/openrt              │
│ reason    outside-workspace write policy           │
│ 1 allow once · 2 session · 3 workspace · 4 deny    │
│ args      {"path":"…"}                             │
└────────────────────────────────────────────────────┘
```

* Amber border normally, **red** for a dangerous/outside-workspace request.
* In **every** non-Chat view the card also renders as an overlay
  (`draw_approval_overlay`) — a pending permission is never hidden behind
  a different screen.
* The status line switches to the key legend while an approval is queued.

## 2. The keys (and the fail-closed default)

| Key | Decision | Meaning |
|---|---|---|
| `1` or `y` | `Allow` | This one call only |
| `2` | `AllowSession` | Identical requests (same tool + scope) auto-answer for the session, **without re-asking** |
| `3` | `AllowWorkspace` | Same, across sessions, pinned to this workspace |
| `4`, `n`, `Esc`, **`Enter`** | `Deny` | Refuse |

`Enter = Deny` is a deliberate, documented deviation from "Enter = default
action": on an approval card a reflexive Enter must never grant. Any
answer the operator never gives stays unanswered — `app.shutdown()` runs
on every exit path and unanswered requests are denied, not granted.

## 3. What a tier grant is (and is not)

Grants live in the gate as an in-memory table:

```rust
struct GrantEntry { workspace: PathBuf, sid: Option<String>, tool: String, scope: Option<String> }
```

* **Process lifetime, in-memory.** Not persisted, not exported, not
  editable from any view. A restart asks again.
* **Exact match required:** same workspace + session id + tool + scope.
  A broader tool or a different scope asks again.
* Keyed by the permission context's **raw** `session_id` (the gate's
  typed correlation path UUID-parses ids; non-UUID session ids would
  silently mismatch, so the grant path deliberately uses the raw string).
* Consulted **only inside the Ask path** — a rule that resolves to Deny
  still denies; a grant can never turn a deny into an allow.

### Why tiers are not policy rules

`Policy::evaluate` is first-match-wins: a rule appended with
`append_rule` lands at the END of the rule list, so it can never
overtake an earlier `Ask` rule. Tier grants therefore live in the gate's
ask-resolution step, not in the policy document. Policy remains
append-only + audited (see [SECURITY_CONTROL_CENTER.md](./SECURITY_CONTROL_CENTER.md)).

## 4. Decision type

```rust
enum PermissionDecision {
    Allow,           // serde: "allow"
    Deny,            // serde: "deny"
    AllowSession,    // serde: "allow_session"
    AllowWorkspace,  // serde: "allow_workspace"
}
```

Wiring: TUI answers map keys → `PermissionDecision` → the pending
`PermissionContext`'s answer channel → gate records the grant (for tiers)
→ the tool executes under the original resolution.

## 5. Tests

| Test | Asserts |
|---|---|
| `approval_tier_keys_answer_session_and_workspace` (tui) | keys `1/2/3/4` produce the matching decision |
| `session_tier_grant_releases_identical_request_without_reasking` (runtime) | tier-2: resolver consulted **exactly once**; the identical request releases from the recorded grant; a *different* session id asks again (raw-string keying, non-UUID ids included) |
| `workspace_tier_grant_releases_across_sessions` (runtime) | tier-3: the grant holds for a different session id in the same workspace |
| shutdown path (tui) | exiting with unanswered approvals denies them |
