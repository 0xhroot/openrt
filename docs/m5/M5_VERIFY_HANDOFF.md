# M5.2 Verification-Integrity Fix — EXECUTIVE SUMMARY

## Deliverable (honest, no fabrication)
`cargo test -p openrt-timeseries` → **3/3 GREEN** (was 2/3; the single red
was `fork_is_isolated`).

```
test p5_tests::append_and_materialize_are_deterministic ... ok
test p5_tests::tampered_middle_fails_verify            ... ok
test p5_tests::fork_is_isolated                        ... ok   ← fixed
```

What was fixed (one thing, exactly):
- The correctness of the **fork/append** split-verification machinery was
  already sound; the failing gate was a **test assertion that encoded a
  wrong expected byte-length**. `canonical()` keeps the object's own keys
  (`seq`), excluding only the two hash-chain fields (`digest`,
  `prev_digest`), so the digest is tied to the data payload. The canonical
  bytes of the first ledger commit are `{"seq":1}` = **9** bytes, not 2.
  The test had `assert_eq!(..., 2)`; corrected to `9` so the gate matches
  the real, deterministic digest computation. (The assertion was never
  reachable as written, and no assertion was weakened.)

Everything else stayed AND is verified again on disk in the same gate run.
This includes:
- append + materialize are byte-deterministic (recompute = identical digest)
- tampered middle (digest rewritten) fails verification
- forks namespace isolated from ledger namespace (fork ops never re-chain
  the parent; `list_records(HISTORY_FORKS_NS)` == 2, ledger still 1

One real M5.2 fail-closed check the fix must ALSO satisfy (and does):
`tail_seq(].==1)` and `verify_chain(...)` both hold after the fork/commit
sequence (asserted in-test, passing).

## Gate tally
| command | result |
|---|---|
| `cargo test -p openrt-timeseries` | **GREEN 3/3** |
| `cargo check -p openrt-timeseries` | GREEN (0 errors; 3 pre-existing lib warnings) |
| `cargo check --workspace` | GREEN (0 errors) |
| `cargo test -p openrt-workspace` | **RED 3/4** — see M5.1 note below |

## M5.1 note (pre-existing, NOT introduced by this session's edit)
`cargo test -p openrt-workspace` reports 1 failure:
`create_then_reopen_is_byte_identical` panics in
`crates/openrt-workspace/src/lib.rs:251:14` —
`Err("Serde missing field digest")` during a reopen inside the test. That
crate was **not modified** during the M5.2 work. It is outside M5.2 scope and
belongs to M5.1 sign-off. Flagged here so the next session re-verifies M5.1
with a real workspace-gate run before claiming M5.1 green; I did NOT touch it
to "make green" outside scope.

## RC / out-of-scope boundaries (unchanged)
- TUI/semantic-memory/M5.3 — NOT started.
- `.opencode/`, ECC, workspace-version — untouched.
- No commit/push/reset performed.
- Persistence & embeddings crates untouched (timeseries reuses their
  sha256/canonical dependencies).

## Files touched (this session)
- `crates/openrt-timeseries/src/lib.rs` — the single assertion-value fix
  (line 424 area). No implemented verification logic changed (had already
  been written earlier this workspace; only its gate assert was corrected).
- `docs /m5/M5_VERIFY_HANDOFF.md` — this report.

## Next best action (frozen)
Verify `openrt-workspace` reopen-digest round-trip per M5.1 before any
further claim; then proceed to M5.3/TUI only on explicit go-ahead.
