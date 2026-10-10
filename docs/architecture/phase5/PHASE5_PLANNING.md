# OPENRT — Phase 5 Planning (semantic-RAG deferral decision)

Status: planning only, honest, no implementation started.
Date: 2026-09-20.
Source of truth: `docs/roadmap/ROADMAP.md` (line 660 `## PHASE 5 — AI WORKBENCH + MEMORY + SELF-IMPROVEMENT`, milestones at lines 730–741); `docs/architecture/OWNERSHIP.md` Phase 5 rows.

---

## 1. Roadmap-derived verdict on "next phase"

The roadmap's next phase is **PHASE 5 — AI WORKBENCH + MEMORY + SELF-IMPROVEMENT**, NOT a semantic-RAG phase.

- Phase 4 delivered (and this session's K6 gate verified) a **deterministic, fail-closed** knowledge/RAG surface: sha256-verified chunk persistence, deterministic hash retrieval, honest no-evidence oracle. Trained/derived neural "semantic" retrieval was **explicitly NOT delivered** and is recorded as such (PHASE4_K6_GATE_AUDIT.md).
- The roadmap text for Phase 5 (lines 660–799) defines components:
  - TUI, Web UI, command palette, sessions;
  - persistent workspace, execution history, replay, fork, workflow time machine;
  - working / episodic / semantic / workflow memory (semantic memory = "facts/knowledge accumulated and retrievable (builds on Phase 4 retrieval)");
  - skills, instincts, hooks, rules, agent genome; skill generation; reusable workflows;
  - self-debugging, self-optimization, research/review loops (approval-gated), local telemetry, learning from execution history.
- Milestones M5.1–M5.9 (roadmap lines 730–741) are the roadmap's own ordering; extending a phase or renumbering is not allowed.

No roadmap renumbering. No new phase invented)Skip.

## 2. Current capabilities inventory (what actually exists — honest)

| Capability | Crate / surface | Reality (verified in this session) |
|---|---|---|
| KB chunk persistence w/ real sha256 over real text | `openrt-knowledge` (lib.rs) | implemented; `cargo test -p openrt-knowledge` → **12 passed / 0 failed** |
| Deterministic retrieval (fail-closed: blank query, top_k=0, bad threshold) | `openrt-knowledge` (retrieval.rs) | implemented; tests cover fail-closed paths |
| Fail-closed RAG oracle (no fabricated citations/answers) | `openrt-knowledge` (retrieval.rs) | implemented; citation-integrity tests |
| Deterministic embeddings (hash-based, explicitly **not** semantic/neural) | `openrt-embeddings` | `sha256_hex` + deterministic embedder; no neural path |
| CLI knowledge surface | `openrt-cli` (main.rs cmd_knowledge) | wired; `cargo check -p openrt-cli` RC=0 |
| Workspace | `cargo check --workspace` | RC=0 |
| Phase 4 docs incl. K6 gate audit | `docs/architecture/phase4/PHASE4_K6_GATE_AUDIT.md` | present; verdicts recorded |

**Not claimed / not present:** semantic/neural embeddings, real vector similarity search, OCR/VLM ingest, SFT/LoRA training — all remain **deferred**; the audit explicitly does not claim them.

## 3. Where the Phase-4 "semantic / vector RAG" gap belongs

The roadmap is explicit that semantic retrieval capabilities remain deferred, but the roadmap gate that opens **after** deterministic retrieval is **Phase 5.3 memory / semantic memory**, which "builds on Phase 4 retrieval". So the semantic-RAG work is **NOT** a separate milestone inside Phase 4; it belongs to the Phase 5 memory layer and must ride on the Phase 5 workspace/history substrate. Forcing it now would duplicate Phase 5 substrate work and would skip the roadmap’s TUI/workbench milestone.

Therefore the honest next phase plan = **Phase 5, milestone-first**: workbench + replay/fork + memory, with semantic embeddings deferred to the Phase 5.3 memory sub-goal (still deterministic retrieval underneath).

## 4. Phase 5 acceptance criteria (from roadmap’s own DoD, Phase 5 section)

- A non-trivial multi-step task completed in the TUI is fully **replayable**; a run can be **forked** at any checkpoint and diverge cleanly into a new session (roadmap Phase 5 DoD).
- The time machine can restore view/state at any recorded point **without corrupting** later history.
- Memory layers answer recall queries with **citations to their origin runs/sources**.
- Semantic memory is backed by Phase 4 retrieval; cross-user memory is out of scope.
- Self-optimization proposes a change against a recorded baseline and is **approved by the operator** (never silent); no self-improvement feature can modify its own policy without operator approval; network/sovereign constraints (Sovereign Mode) intact (ADR-003, ADR-008).
- TUI/Web UI bind to localhost only by default (security requirement).

## 5. Proposed roadmap-consistent milestones (derived from the roadmap’s M5.x)

| Slice | Milestone | Scope (honest, minimal) |
|---|---|---|
| 1 | M5.1 | Persistent workspace + session UX in TUI; sessions create/switch/resume over the existing session system |
| 2 | M5.2 | Execution history, replay, fork, workflow time machine over the existing phase-2 checkpoint plumbing (exec.runs / exec.dag / checkpoint crates) |
| 3 | M5.3 | Memory layer: working / episodic / semantic(/workflow); semantic uses Phase 4 deterministic retrieval + model-provider interface |
| 4 | M5.4 | Skills, instincts, hooks, rules, agent genome (asset crates; executable-content policy per roadmap security reqs) |
| 5 | M5.5 | Skill generation + reusable workflows |
| 6 | M5.6 | Self-debugging + self-optimization research/review loops (approval-gated) |
| 7 | M5.7 | Local telemetry + learning from execution history (extends Phase 1 event bus + Phase 5 observability conventions) |
| 8 | M5.8 | Web UI + command palette (localhost-only) |
| 9 | M5.9 | Phase gate audit — self-improvement only with operator approval |

Sequence stays as the roadmap lists: TUI/sessions → history/replay/fork → memory → genome/skills → self-improvement → telemetry → Web GUI → gate.

## 6. Recommended first implementation slice

**M5.1 + M5.2 scaffolding — "Workspace + history sandbox"** as first next step because:
- It has immediate, verifiable returns (replay fidelity, fork isolation) and does not require new model backends;
- It builds directly on the existing Phase 1 session/persistence + Phase 2 checkpoint/DAG crates (no new phase scaffolding);
- It keeps semantic embeddings out of scope until M5.3 (no capability overclaim).

**Success criteria (first slice):**
- `cargo test -p openrt-<workspace>` + `cargo test -p openrt-<history>` pass; deterministic fixtures.
- Replay is byte-identical for a deterministic fixture; fork produces independent history.
- Time-machine restore does not corrupt later records; audit chain intact.
- TUI binds localhost-only; no network path in Sovereign Mode.
- Semantic embeddings: NOT claimed; documented as deferred to M5.3.

## 7. Deferred/optional (explicitly NOT in this phase gate)

- Semantic/neural embeddings, real vector search, OCR/VLM ingest, SFT/LoRA training — **deferred** (roadmap Phase 4 “what must not ship yet” + K6 audit classifications). They are optional follow-on work, not part of Phase 5 gate.

## 8. Hard stop

Planning only. No source edits, no commits, no new phase documents beyond this planning note, no implementation started, Phase 5 not started. Verified by live gates:
`cargo check --workspace` RC=0; `cargo check -p openrt-cli` RC=0; `cargo test -p openrt-knowledge` 12 passed.
