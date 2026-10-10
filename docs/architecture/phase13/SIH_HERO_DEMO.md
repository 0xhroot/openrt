# SIH HERO DEMO — CyberSaarthi × OpenRT (final hero workflow)

Phase: **13** (M13.12–M13.17). The SIH (Smart India Hackathon) hero demo is
**CyberSaarthi**: a complete synthetic financial-crime investigation run
end-to-end through the real OpenRT pipeline — case → evidence → graph →
agent workflow → verification → security audit → artifacts. Runbook and
measured numbers below; acceptance evidence in
[`PHASE13_ACCEPTANCE.md`](./PHASE13_ACCEPTANCE.md).

## 1. What it demonstrates (honest scope)

One command drives **every OpenRT authority the roadmap built**:

```
USER → CLI/WebUI → WORKBENCH (room + materials) → AGENTS (workflow roles)
 → MODEL (offline mock backend by default) → RAG (ingest/chunk/retrieve)
 → GRAPH (entities/relations) → WORKFLOW (retrieve→analyze→verify)
 → TOOL/SHIELD (injection defense) → VERIFICATION (citations/coverage)
 → ARTIFACT (5 reports) → AUDIT (chain verified) → PERSISTENCE (one store)
```

It is **not** a fake backend demo: the store, gate, shield, verifier, audit
chain and artifact system are the production implementations. The only
synthetic part is the *data* — a marked, embedded fixture — and the default
*model* backend (the documented offline mock; a real local model is optional
and never faked).

## 2. The dataset (M13.13) — synthetic, embedded, marked

`crates/openrt-runtime/src/sih_data/`:

| File | Content |
|---|---|
| `case.json` | case `case.sih-demo` (title, hypothesis, query) |
| `entities.json` | **9 entities** — suspects, accounts, locations, orgs |
| `relationships.json` | **10 relations** with provenance into `knowledge.graph` |
| `transactions.json`, `incidents.json` | supporting synthetic records |
| `docs/witness_statement.md` | ingestible evidence document |
| `docs/financial_ledger.md` | ingestible evidence document |
| `docs/association_memo.md` | ingestible evidence document |
| `docs/hostile_note.md` | **deliberately hostile** document — drives the injection stage (7 flags / 5 categories) |

Everything is synthetic: no real personal data, no real financial data,
no network fetch. The documents enter through the ONE jailed ingest
pipeline (`corpus.*`), relations through the ONE graph authority
(`knowledge.graph` via `openrt-evidence`).

## 3. Pipeline stages (measured, fresh run)

`openrt sih demo --dir <dir> --json` executes, in order (stage → ms):

| Stage | What it does | ms |
|---|---|---|
| prerequisites | hermetic store/workspace/model/doctor posture check | 141 |
| case | load synthetic case record | 1 |
| room | create investigation room (Workbench, `openrt-workspace`) | 1 |
| ingest | ingest 4 documents through the jailed pipeline | 9 |
| chunk | 24 chunks | 11 |
| graph | 9 entities / 10 relations into `knowledge.graph` | 13 |
| materials | seal case materials (digest references, ADR-006) | 9 |
| retrieve | hybrid retrieval for the investigator query (4 hits) | 3 |
| workflow | `cybersaarthi-investigation`: retrieve → analyze → verify (3 steps, Completed) | 34 |
| verify | claim verification with citations — verdict **Supported**, decision **Pass**, grounding coverage **1.000** | 20 |
| injection | scan the hostile note through the shield/detector — **7 flags**, categories: `instruction_override`, `exfiltration_bait`, `role_confusion`, `prompt_extraction`, `tool_calls_as_text` | 14 |
| claims | record claims ↔ passages ↔ documents (`knowledge.evidence`) | 4 |
| artifacts | write 5 deterministic reports with sha256 | 19 |
| **total** | | **292** |

CLI wall time: **0.736 s** (fresh), **0.837 s** (rerun); peak RSS ≈ 69 MiB
(`/tmp/opencode/p13_perf.log`). Audit: 18 demo-scoped entries, chain
verified (>0 — asserted in tests).

## 4. Running the demo

### CLI

```console
$ openrt sih demo --dir /tmp/sih --json | head -c 400
{"case_id":"case.sih-demo","documents":4,"chunks":24,"entities":9,
 "relations":10,"hits":4,"workflow_steps":3,"workflow_status":"Completed",
 "verdict":"Supported","decision":"Pass","grounding_coverage":1.0,...
```

Flags: `--dir DIR` (hermetic store + workspace + artifacts), `--json`
(machine-readable report), `--query Q` (investigator query override).
Exit code is **non-zero if any stage fails** — never a fake success.

### WebUI

- `GET /api/sih` — JSON dashboard (rooms, corpus, claims, entities,
  relations, sessions, runs, verdicts, artifacts, shield findings, audit
  chain-verified count) read from the one Runtime.
- `GET /sih` — the dashboard page.
- Command palette `sih.info` — same data through the palette protocol.

### Artifacts (M13.16) — existing artifact system, 5 files

Under `<dir>/artifacts/`, each recorded with sha256:

| Artifact | File | Kind |
|---|---|---|
| Investigation report | `investigation_report.md` | report |
| Evidence summary | `evidence_summary.json` | evidence |
| Relationship graph | `relationship_graph.json` | graph |
| Verification report | `verification_report.json` | verification |
| Audit summary | `audit_summary.json` | audit |

## 5. Security posture asserted during the demo

- Runs **Sovereign** (fresh store default): no external egress anywhere
  (Part E guard covers providers, vision, doctor, CLI).
- The hostile document is **flagged, not obeyed** — its content never
  reaches a privileged action; flags are persisted as shield findings.
- The verdict is **evidence-linked** (citations + grounding coverage),
  produced by the one `openrt-verifier` authority.
- The audit chain verifies at the end (`chain_verified > 0`).
- Dashboard never shows `Fail`; posture rows use the honest vocabulary.

## 6. Reproducibility (M13.17)

`p13_e2e::test_h_capstone_reproducible_demo` runs the demo **twice from
independent clean environments** and asserts equality of: case id,
documents, chunks, hits, entities, relations, workflow steps/status,
decision, verdict, grounding coverage, injection flags + categories, and
the artifact file-name set (every artifact carries a 64-hex sha256).
No hidden manual state; two fresh runs are indistinguishable at the
report level.

CLI gate transcript (this host): `/tmp/opencode/p13_final_sih.log` (RC=0).
