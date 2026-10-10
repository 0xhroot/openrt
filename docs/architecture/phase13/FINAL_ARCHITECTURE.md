# FINAL ARCHITECTURE — OpenRT (Phase 13, Part Z)

The complete final architecture of OpenRT as built: the layer stack, the
one authority behind each layer, and how the four flagship capabilities map
onto it. Companion docs:
[`PHASE13_ACCEPTANCE.md`](./PHASE13_ACCEPTANCE.md),
[`SECURITY_THREAT_MODEL.md`](./SECURITY_THREAT_MODEL.md),
[`SIH_HERO_DEMO.md`](./SIH_HERO_DEMO.md),
[`../OWNERSHIP.md`](../OWNERSHIP.md) (per-subsystem ownership).

## 1. The stack

```
USER
 ↓                     operator at a terminal, browser, or editor —
                       nothing runs without the operator's mode posture (ADR-003)
TUI / WebUI / CLI      one CLI (`openrt`), one WebUI (loopback + server-issued
 ↓                     token), one TUI — all views over the ONE Runtime
WORKBENCH              persistent workspaces, rooms, materials (ADR-006
 ↓                     references+digests, never copies)
AGENTS                 master + role sub-agents (researcher/analyst/coder/
 ↓                     reviewer/verifier) — advisors, never authorities
MODEL ROUTER           one registry/router: policy → mode → eligibility →
 ↓                     ranking; providers wrapped by the egress guard (ADR-010)
KNOWLEDGE / RAG /      one ingest jail → chunks → hybrid retrieval → citations
EVIDENCE               → claims graph (`openrt-knowledge`/`openrt-evidence`)
 ↓
WORKFLOW               one engine (`openrt-orch`): DAG execution, checkpoints,
 ↓                     pause/resume/steer, blackboard
TOOLS / MCP /          one `ToolRegistry` + Tool Fabric metadata; MCP/DB/Office/
EXTENSIONS             browser/extension tools register here — deny-by-default caps
 ↓
POLICY                 `openrt-permissions` + versioned `openrt-policy` document
 ↓                     + `openrt-netpol` grants: every call evaluated, default deny
AGENTSHIELD            `openrt-shield`: one detector/finding layer (injection,
 ↓                     secrets, commands, packages…) feeding the ONE gate
SANDBOX                `openrt-sandbox`: one request/policy/handle/result authority;
 ↓                     tool→profile requirement step in the gate
COMPUTERCONTROL        `openrt-platform` backends + `openrt-computer` loop;
 ↓                     VLM grounds only — it never executes
EXECUTION              gate-approved actions run under mode + kill-switch (ADR-008)
 ↓                     + resource limits + jail
VERIFICATION           `openrt-verify` (digest/provenance, ADR-001) +
 ↓                     `openrt-verifier` (claims/answers, citation coverage)
ARTIFACT               generated outputs recorded with sha256 + provenance
 ↓                     (`fabric.artifacts`), scanned before trust
AUDIT                  `openrt-audit`: chained, tamper-evident, append-only
 ↓                     (ADR-002) — verify fail-closed
PERSISTENCE            ONE `PersistenceStore` (ADR-006): namespaced records,
 ↓                     digests, permissions (SEC-02/03)
RECOVERY               `openrt-checkpoint` + recovery records: crash/boot
                        reconciliation, operator-gated rollback, never
                        auto-resume of paused or uncertain work
```

Cross-cutting authorities that the stack leans on everywhere:

- **Mode engine** (`openrt-modes`, ADR-003): Sovereign default, fail-closed
  egress, audited transitions; loopback/file allowed.
- **Kill switch** (`openrt-safety`, ADR-008): first-tripper-wins, halt never
  auto-clears.
- **Registry** (`openrt-registry`, ADR-010): immutable manifests, one-hop
  lifecycle, signed-bundle scan gate.
- **Expert hierarchy** (`openrt-hierarchy`, ADR-009): content-addressed
  expert shards.
- **Events/telemetry** (`openrt-events`/`openrt-telemetry`): typed domain
  events, value-pattern redaction at write.

## 2. Domain mapping

### Industrial Intelligence (Phase 12, M12.1–M12.7)

Enters at **KNOWLEDGE / RAG / EVIDENCE** and **WORKFLOW**: plants/units/
equipment/sensors live in `industrial.assets` with relations mirrored into
the one `knowledge.graph`; RCA/FMEA/risk/scenarios are advisory analysis
records in `industrial.analysis` produced by `openrt-industrial` over the
same evidence graph; documents arrive through the jailed ingest; any
"investigate" answer is evidence-linked. **No physical-control tool
exists** — execution never gains an industrial write path. Production
surface: CLI `industrial *`, WebUI palette `industrial.*`, doctor rows.
Tests: `p12_integration`, `p12_e2e::test_a`.

### Trading Research (Phase 12, M12.8–M12.11)

Enters at **KNOWLEDGE** (datasets via the ONE ingest) and **WORKFLOW**
(backtest runs as gated jobs): instruments/datasets/strategies in
`trading.*` namespaces, deterministic backtests in `trading.backtests`/
`trading.experiments` — always marked simulated. **Research only**: there
is no order path; `trading.execute` refuses (exit 3) and no `trading.broker.*`
tool is registered (deny-by-default). Production surface: CLI `trading *`,
palette `trading.*`. Tests: `p12_integration`, audits L/M, E2E.

### Self-Improvement (Phases 5/10/12)

Lives across **AGENTS → POLICY → PERSISTENCE**: episodes/experience
(`improve.episodes`), candidates (skills/instincts/genome in `assets.*`),
validate → shield → benchmark → **operator approval** → activation, with
versioned rollback (`improve.promotions`). Learned artifacts never
self-activate and never touch permissions/policy; the phase-10 lifecycle is
the only path. Production surface: CLI `learn *`/`improve *`/`skill *`/
`genome *`, WebUI time-machine/skills views. Tests: `p5_workbench`,
`p10_skills`, `p12_integration` (rollback evidence).

### CyberSaarthi (Phase 13, the SIH hero demo)

The **end-to-end vertical slice** through the entire stack: CLI/WebUI →
Workbench room → role agents (`retrieve → analyze → verify`) → model (offline
mock by default, egress-guarded) → RAG over the synthetic case → graph →
workflow engine → shield flags the hostile note → verifier returns a
citation-covered verdict → 5 artifacts with sha256 → audit chain verified →
one store. Production surfaces: `openrt sih demo`, `GET /sih` + `/api/sih`,
palette `sih.info`. Tests: `p13_e2e::test_f`, `test_h`, WebUI suite.

## 3. Mode / egress posture (final)

| Scope | Authority | Sovereign default |
|---|---|---|
| browser, database, mcp, **model**, marketplace, telemetry, datasets | `net_guard::ensure_url_allowed` + `NetworkPolicy` + `ModeManager` | external → `POLICY_DENIED` (ADR-003 reason + mode hint); loopback/file → allowed |
| provider generate / VLM ground | `openrt-runtime::model_egress` wrappers (Part E) | refused **before** any client call, audited (`model.egress`) |
| sandbox processes | `openrt-sandbox` network modes | `NONE`/`LOCAL_ONLY` unless granted |

No surface — CLI, doctor, WebUI, agent, workflow node — reaches the network
except through these gates.
