# Agent Recovery — interrupted sessions and the boot scan

**Status:** Accepted · **Date:** 2026-10-01
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md) · **Companion:** [PRODUCTION_HARNESS.md](./PRODUCTION_HARNESS.md)

What happens when a previous OpenRT session died with work in flight, and
how the operator sees and resolves it. The invariant everywhere:
**recovery records and reconciles — it never claims unfinished work was
successful, and it never auto-resumes anything.**

## 1. The boot scan

`RuntimeBuilder::build` runs `Runtime::boot_recover()` **before the
runtime serves anything** (fail-closed: a scan error fails the build):

1. Finds tasks persisted in a live status (`Running`, `Queued`, …) whose
   owning process is gone → transitions them to **`Interrupted`**
   (`completed_at` stays `None` — not success, not failure).
2. Finds incomplete workflow runs and computes a resume checkpoint
   (planner output only — nothing executes).
3. Finds stale sandboxes and reconciles them.
4. Persists a `RecoveryRecord` for **this boot** and emits
   `EventKind::RecoveryStarted` (`phase: "boot"`, `record_id`).

```rust
struct RecoveryRecord {
    record_id: String,          // boot-<uuid>
    boot_id: String,
    boot_at: Timestamp,
    interrupted_tasks: Vec<InterruptedTaskInfo>,  // task/session id + previous status
    incomplete_runs: Vec<IncompleteRunInfo>,      // resume checkpoint (computed, not run)
    stale_sandboxes: Vec<StaleSandboxInfo>,
}
impl RecoveryRecord {
    fn is_clean(&self) -> bool;      // all three lists empty
    fn total_findings(&self) -> usize;
}
```

A clean boot still records the record (evidence without noise) — it just
has zero findings.

## 2. Surfacing in the TUI

### The `SESSION INTERRUPTED` card

`AgentApp::new` reads `runtime.recovery_records()` at startup. For every
**non-clean** record it pushes plain chat notes (real record contents,
never invented):

```
SESSION INTERRUPTED — boot recovery found work left behind
2 interrupted task(s) · 1 incomplete run(s) · 0 stale sandbox(es)
  nothing was auto-resumed (fail-closed)
  <task-id> was Running at last boot (session <id>)        (up to 3)
  resume: `openrt workflow resume` · review: SECURITY view / `openrt recovery list`
```

* A clean boot renders **nothing** (`clean_boot_stays_silent_about_recovery`).
* The status line switches to `session interrupted — boot recovery found
  work left behind` unless the first-run setup wizard owns it.

### Browsable evidence

* **SECURITY view** → `recovery` section: every record with its counts
  and `clean=` flag (see [SECURITY_CONTROL_CENTER.md](./SECURITY_CONTROL_CENTER.md)).
* **Workbench Ops → Recovery** (`4`): the same records as rows.

## 3. Resume paths (explicit only)

| Surface | Command | Semantics |
|---|---|---|
| CLI | `openrt recovery scan` | Re-run the scan; writes evidence |
| CLI | `openrt recovery list [--json]` | All records + finding counts |
| CLI | `openrt recovery inspect <record\|boot> [--json]` | One record, full detail |
| CLI | `openrt workflow resume` | The **only** resume path for an incomplete run — resumes the latest incomplete run from its newest checkpoint, operator-driven |
| TUI | card pointers above | Read-only guidance; resume happens through the CLI/API |

Interrupted **tasks** are not silently restarted: the operator re-runs
them (task inspect links the task back to its `record_id`).

## 4. Honesty rules

* `previous_status` in the record is what boot actually discovered — the
  card prints it verbatim (`was Running at last boot`).
* A recovered workflow run, when explicitly resumed, is a real run: it
  goes through the same bookkeeping (metrics, genome consumption,
  policy/gate) as any other run.
* Boot never resumes, never marks interrupted work as done, and never
  clears findings — only the operator's explicit actions do.

## 5. Tests

| Test | Asserts |
|---|---|
| `boot_recovery_interrupts_stale_tasks_and_records_them` (runtime) | Running → Interrupted, `completed_at = None`, record links the task, event emitted |
| `boot_recovery_clean_boot_records_evidence_without_noise` (runtime) | clean boot ⇒ clean record, no noise |
| `interrupted_boot_surfaces_session_interrupted_card` (tui) | card shows the real task id + "nothing was auto-resumed"; store reconciled to Interrupted |
| `clean_boot_stays_silent_about_recovery` (tui) | no card on a clean boot |
| `ops_recovery_lists_boot_records` (tui) | Ops recovery tab lists the same records the runtime reads |
