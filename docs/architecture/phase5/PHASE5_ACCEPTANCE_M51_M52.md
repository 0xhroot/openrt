# Phase 5 acceptance — M5.1 persistent workspace + M5.2 execution history
> Status: **honest gate snapshot** — crates built; runtime verification has
> real-failing integrity findings that must be resolved before the acceptance
> criterion "verify accepts every state it persisted" is claimed. Nothing here
> is fabricated; every claim below was observed on disk during this slice.

## What is REAL (built + wired into the workspace)

* `crates/openrt-workspace` (M5.1 lease, OWNERSHIP row "workspace.*")
  — deterministic workspace identity (`sha256`-derived id; `w.<label>` keys;
  `create_workspace`/`open_workspace`/`list_workspaces`/`verify`/`reopen_current`
  + `workspace.workspaces`/`workspace.reopen` namespaces) over the ONE store
  (ADR-006; never a second store, never duplicated foundation records — ADR-007).
* `crates/openrt-timeseries`    (M5.2 lease, OWNERSHIP row "history.*")
  — append-only, tamper-evident execution-history ledger
  (`append_event`, `tail_seq`, `verify_chain`, `materialize_as_of`,
  `create_fork` + `fork_commit` fork isolation, fork `verify_chain`;
  sha256-chained digests over canonical key-sorted JSON; fail-closed errors).
* Both crates are workspace members and workspace.dependencies entries; both
  `cargo check` clean together with the 39-crate workspace.
* OWNERSHIP.md / ROADMAP.md rows for M5.1 (persistent workspace) and M5.2
  (history / replay / fork / time machine) list exactly these two crates.

## What is HONESTLY PENDING (NOT claimed)

* `cargo test -p openrt-timeseries` currently shows the two chain-verification
  tests FAILING on `verify_chain` state consistency (a fork test and a
  tamper test), and one workspace test round-trips. The acceptance gate
  "verify accepts, byte-identical, every state this crate itself persisted"
  is therefore **NOT green** in this slice. The failing assert is
  `verify_chain(&*s)` inside the fork-isolation test — the fork branch adds a
  record that `verify_chain` then flags as a parent-chain violation.
  Resolution is a deterministic fix (verify must materialize the parent ledger
  only, `history.ledger`, and treat `history.forks` as an ISOLATED namespace it
  never re-chains into the parent) — tracked, not yet landed.
* The M5.1 TUI session UX and the M5.2 time-machine TUI surface remain
  open roadmap work (M5.3+); the crates ship the durable subspace only,
  exactly as OWNERSHIP/M5.2 describe.

## Evidence commands (run on THIS tree)

    cargo check -p openrt-workspace -p openrt-timeseries   # Finished, 0 errors
    cargo test -p openrt-timeseries                        # 1 pass / 2 fail (chain verify)
