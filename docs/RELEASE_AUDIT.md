# OpenRT V1 — Release Audit

**Prepared:** 2026-10-10 · **Target:** `https://github.com/0xhroot/openrt` (public)
**State:** local release candidate only — **nothing was committed, pushed, or published.**

---

## 1. Repository status and remotes

| Item | Value |
|---|---|
| Working tree | `~/Projects/openrt` (clean of secrets, see §3) |
| Branch | `master` (fresh `git init`, **zero commits — no baseline exists**) |
| Remotes | **none configured** (`git remote -v` empty) |
| Tags | none |
| LFS | not installed, no `.gitattributes`, no LFS pointers |
| Destination check | `gh` authenticated as `0xhroot`; `0xhroot/openrt` **does not exist yet** (GraphQL: could not resolve) — safe to create later |
| Staging | **579 files staged** against the §2 allowlist (10,134,681 bytes ≈ 9.66 MiB); **not committed**. Unstaged working tree contains only `Pictures/` and `black_image.png` |
| Untracked | none outside the two excluded junk items above |

Because there is no history, review the snapshot with
`git diff --cached --name-only` against the explicit include/exclude plan
in §2.

## 2. Files included and excluded by category

### Include (initial commit allowlist)

| Category | Items | Size |
|---|---|---|
| Build manifest | `Cargo.toml`, `Cargo.lock` (403 pinned packages) | ~108 KB |
| License / entry | `LICENSE`, `README.md`, `.gitignore` | ~30 KB |
| Source | `crates/` — 64 crates, 398 `.rs` files | ~9 MB |
| Documentation | `docs/` — 101 files: ADRs, phase 1–13 implementation & acceptance, TUI design docs, roadmap, milestone reports, subsystem handoffs, index, screenshots, this audit | ~1.3 MB |
| Screenshots | `docs/assets/screenshots/*.png` (5 genuine captures) | ~440 KB |

**Total staged: 579 files, ~10 MB.** Largest single file: `crates/openrt-tui/src/agent/tests.rs` (458 KB); largest doc: `docs/milestone-reports/project-complete-plan.txt` (249 KB).

### Exclude — gitignored (never staged)

| Item | Reason |
|---|---|
| `target/` | build artifacts (includes a 344 MB debug binary) |
| `models/` | **~96 GB of local GGUF model weights** (4+ files, e.g. `Qwen_Qwen3.5-9B-Q8_0.gguf`) — never publish weights |
| `.opencode/` | ECC/opencode agent configuration — out of scope, explicitly not to be modified or published |
| `.openrt/` (runtime home, any depth), `/openrt-workspace/` (repo root only — the source crate `crates/openrt-workspace/` is **tracked**, see §9), `*.local`, `tmp/` | local runtime state if ever created inside the repo |
| `.env`, `.env.*` | secrets (no such files currently exist) |

### Exclude — untracked, deliberately not staged (NOT deleted)

| Item | Reason |
|---|---|
| `black_image.png` | solid-white 1366×682 PNG, no content, orphan test artifact |
| `Pictures/tree.png` | 25-byte text file masquerading as a PNG (broken) |

### Moved for organization (content preserved)

| From (repo root) | To |
|---|---|
| `PHASE2_FINAL_REPORT.md`, `PHASE3_FINAL_REPORT.md`, `P2_6_FINAL_REPORT.md`, `project-complete-plan.txt` | `docs/milestone-reports/` |

Nothing else was moved, rewritten, or deleted. No git history rewriting.

## 3. Secret scan result (values redacted)

Scanned all candidate files (tracked-eligible set; excludes `target/`,
`models/`, `.opencode/`, `.git/`) for API keys, tokens, private keys,
credential assignments, and private URLs.

| Finding | Verdict |
|---|---|
| Key-pattern matches in `crates/openrt-core/src/redaction.rs` and `crates/openrt-shield/src/lib.rs` | **test fixtures only** — deliberately fake values (`ghp_xxxx…`, `password: hunter2…`) asserting that redaction works. Not credentials. |
| `.env*` files | none exist |
| Databases / logs / session stores in candidate set | none |
| Root reports & master plan (`docs/milestone-reports/*`) | prose matches for words like "token"/"secret" only — no values |
| Local absolute paths (`/home/<user>/…`) | were found in **3 docs** (`PHASE4_PREFLIGHT.md`, `PHASE4_K6_GATE_AUDIT.md`, `PERMISSIONS_UX.md`) — **replaced with portable `~/Projects/openrt` examples in the final gate**; technical evidence and box-art alignment preserved, nothing deleted. Repo-wide scan after the fix: 0 matches. |
| Screenshots | **re-captured in the final gate** against an isolated `HOME` *and* a portable working directory (`/tmp/openrt-demo`), fresh store, real first-run setup + real tool task. OCR text scan of all 5 PNGs: **0 occurrences of the local username, 0 occurrences of `/home/…`**. No credentials, no private session content. |
| Runtime home `~/.openrt` | lives **outside** the repository; contains real session history and is not part of the candidate |

**No secret values are printed anywhere in this audit or in README.**

## 4. Large-file findings

| File/dir | Size | Disposition |
|---|---|---|
| `models/` (GGUF weights) | **~96 GB** | gitignored — excluded |
| `target/` (build output) | multi-GB | gitignored — excluded |
| `.opencode/node_modules/` | small | gitignored via `.opencode/` — excluded |
| `.git/` | trivial (empty repo) | baseline to be created |
| Candidate set (>500 KB) | none | — |
| Git LFS | not in use; not needed at candidate size (~10 MB) | — |

## 5. Licensing findings — including full dependency audit

### 5.0 Project license

| Item | Result |
|---|---|
| Project license | **MIT** — root `LICENSE` (standard MIT text); `workspace.package.license = "MIT"` in the workspace manifest (line 76), inherited via `license.workspace = true` by **all 64 crates** (one gap — `crates/openrt-safety` lacked the field — **fixed in this final gate**, same workspace MIT, not a new license choice) |
| License headers | none used anywhere (0 of 398 `.rs` files carry SPDX/copyright headers) — nothing to reconcile with the root LICENSE |
| NOTICE / vendored / generated | no `NOTICE`/`COPYING` files in the project, no `vendor/` or `third_party/` trees, no `build.rs`, no generated-code directories |
| Copyright line | `Copyright (c) 2026 OpenRT` (workspace `authors = ["OpenRT"]`) — MIT is appropriate: original first-party code, no conflicting inbound licenses identified |
| Supply-chain check | `openrt doctor` → `PASS supply-chain.lockfile` (Cargo.lock resolved: 403 pinned packages, sha256 recorded) |
| `.opencode/` third-party node packages | excluded from release entirely |

### 5.1 Tooling status (exact commands and output)

`cargo-deny` and `cargo-license` are **not installed** on this host, and this
audit does not install tools or change dependency versions:

```console
$ cargo deny --version
error: no such command: `deny`

$ which cargo-deny cargo-license
(none found; ~/.cargo/bin contains only rustup-managed toolchain binaries)
```

A proposed `deny.toml` for future use is in §5.9 (prepared for review only —
**not** created in the repository, not staged).

### 5.2 Method — exact commands used

```console
# 1. Full cargo metadata for every resolved package (403 = 64 members + 339 external),
#    with --locked so Cargo.lock cannot change; network used only to fetch
#    Windows/Redox/WASM platform manifests missing from the local cache.
$ md5sum Cargo.lock > /tmp/lock.md5
$ cargo metadata --format-version 1 --locked > /tmp/cargo-metadata.json
#    result: exit 0; only crossterm_winapi/winapi/redox_syscall/wasi targets downloaded;
#    md5sum -c /tmp/lock.md5 → Cargo.lock: OK (lockfile byte-identical before/after)

# 2. License inventory over all 339 external (registry) packages:
$ jq -r '.packages[] | select(.source != null) | (.license // .license_file // "<MISSING>")' \
    /tmp/cargo-metadata.json | sort | uniq -c | sort -rn

# 3. Missing / ambiguous data:
$ jq -r '.packages[] | select(.source != null and (.license == null) and (.license_file == null))
         | "\(.name) \(.version)"' /tmp/cargo-metadata.json          # → empty
$ jq -r '.packages[] | select(.source != null and .license == null and .license_file != null)
         | ...' /tmp/cargo-metadata.json                             # → empty
$ jq -r '.packages[] | select(.source != null and (.source|startswith("git")))
         | ...' /tmp/cargo-metadata.json                             # → empty (no git deps)

# 4. Restricted-term scan over license strings (GPL/AGPL/SSPL/BUSL/Commons Clause/
#    Elastic/non-commercial/CC-BY/proprietary/UNLICENSED) → only r-efi's electable
#    "LGPL-2.1-or-later" OR-arm (see §5.5)

# 5. Independent cross-check against every downloaded registry manifest
#    (~/.cargo/registry/src/*/{name}-{version}/Cargo.toml) — 339/339 compared

# 6. Direct-vs-transitive split via cargo metadata's resolve graph
#    (84 direct external deps of the 64 workspace members)
```

### 5.3 Coverage — every package accounted for

| Population | Count | License data |
|---|---|---|
| Packages in `Cargo.lock` | 403 | pinned |
| `cargo metadata` packages | 403 | matches lock exactly |
| Workspace members (first-party) | 64 | MIT via workspace inheritance — **64/64 complete** |
| External registry dependencies | 339 | SPDX `license` field present — **339/339 complete** |
| — of which direct deps of members | 84 | all permissive (see §5.4) |
| Git dependencies | 0 | — |
| Missing / `license-file`-only / unknown | **0** | — |
| Local-manifest cross-check mismatches | **0 of 339** | metadata == on-disk manifest license for every crate |

### 5.4 Full license-expression inventory (all 339 external packages)

| License expression | Count |
|---|---|
| MIT OR Apache-2.0 | 167 |
| MIT | 79 |
| Apache-2.0 OR MIT | 19 |
| Unicode-3.0 | 18 |
| MIT/Apache-2.0 *(deprecated slash syntax for `OR`)* | 17 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 5 |
| BSD-3-Clause | 3 |
| Zlib OR Apache-2.0 OR MIT | 2 |
| Zlib | 2 |
| Unlicense OR MIT | 2 |
| MIT OR Apache-2.0 OR LGPL-2.1-or-later | 2 |
| ISC | 2 |
| BSD-2-Clause OR Apache-2.0 OR MIT | 2 |
| Apache-2.0 OR ISC OR MIT | 2 |
| Apache-2.0/MIT *(deprecated slash syntax)* | 2 |
| Apache-2.0 | 2 |
| WTFPL | 1 |
| MIT OR Zlib OR Apache-2.0 | 1 |
| MIT OR MPL-2.0 | 1 |
| MIT OR Apache-2.0 OR Zlib | 1 |
| MIT OR Apache-2.0 OR BSD-1-Clause | 1 |
| (MIT OR Apache-2.0) AND Unicode-DFS-2016 | 1 |
| (MIT OR Apache-2.0) AND Unicode-3.0 | 1 |
| MIT AND Unicode-DFS-2016 | 1 |
| CDLA-Permissive-2.0 | 1 |
| Apache-2.0 OR BSL-1.0 | 1 |
| Apache-2.0 / MIT *(deprecated slash syntax)* | 1 |
| Apache-2.0 AND ISC | 1 |
| 0BSD OR MIT OR Apache-2.0 | 1 |

Every expression above is a **permissive** license family. The 20
slash-syntax entries (`MIT/Apache-2.0` etc.) are deprecated SPDX spelling of
`OR` — semantically unambiguous, flagged only for tooling hygiene.

### 5.5 Notable packages requiring a human-readable verdict

| Package | License | Verdict |
|---|---|---|
| `r-efi` 5.3.0, 6.0.0 | `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | **Elect MIT.** LGPL is only one OR-arm; never selected. (UEFI helper crate — not even linked on the Linux target.) |
| `termina` 0.3.3 | `MIT OR MPL-2.0` | **Elect MIT.** MPL-2.0 (file-level copyleft) is only one OR-arm; never selected. |
| `finl_unicode` 1.4.0 | `(MIT OR Apache-2.0) AND Unicode-DFS-2016` | **Both arms apply.** Unicode-DFS-2016 is a permissive data license (redistribution allowed with the notice). Compliant. |
| `wezterm-bidi` 0.2.3 | `MIT AND Unicode-DFS-2016` | Same — both permissive. Compliant. |
| `unicode-ident` 1.0.24 | `(MIT OR Apache-2.0) AND Unicode-3.0` | Both permissive. Compliant. |
| `terminfo` 0.9.0 | `WTFPL` | Public-domain-style unencumbered license (OSI-approved); redistribution unrestricted. |
| `webpki-roots` 1.0.9 | `CDLA-Permissive-2.0` | Permissive data license; redistribution allowed with notice. |
| `ring` 0.17.14 | `Apache-2.0 AND ISC` | Both permissive; both apply. Compliant. |
| `curve25519-dalek` 4.1.3, `ed25519-dalek` 2.2.0, `subtle` 2.6.1 | `BSD-3-Clause` | Permissive, MIT-compatible terms. `ed25519-dalek` is the **only direct dep** without a MIT/Apache arm. |
| `icu_*` (18 crates) + `litemap`/`tinystr`/`yoke`/`zerovec` family | `Unicode-3.0` | Permissive; allow redistribution with notice. |
| 20 crates (`bitflags` 1.3.2, `winapi`, …) | `MIT/Apache-2.0` etc. | Slash-syntax `OR`; permissive either way. |

**Direct dependencies (84 total):** all offer MIT or Apache-2.0 except
`ed25519-dalek` (BSD-3-Clause, fully permissive). Only `ring` carries an
`AND` clause (`Apache-2.0 AND ISC`, both permissive). No direct dependency
is GPL/LGPL/MPL/Unicode-restricted.

### 5.6 Restricted / incompatible / unknown terms — scan result

| Class | Hits |
|---|---|
| GPL, AGPL (any version, primary or sole license) | **0** |
| SSPL, BUSL, Elastic License, Commons Clause | **0** |
| Non-commercial (CC-BY-NC etc.), proprietary, UNLICENSED | **0** |
| Share-alike as the only option | **0** (MPL/LGPL appear only as electable OR-arms — §5.5) |
| Unknown / missing license metadata | **0 of 403** |
| Dependencies whose redistribution terms restrict OpenRT's MIT distribution | **0** |

**No compatibility problems were invented away and no license was changed**
— every verdict above reflects the dependency's own declared SPDX
expression, with OR-arms noted explicitly where an election exists.

### 5.7 Attribution obligations on distribution (pre-packaging follow-up, not a commit blocker)

- MIT / Apache-2.0 / BSD / ISC / Zlib / Unicode-family licenses require
  retaining copyright notices when redistributing binaries. Standard
  remedy before shipping a *binary* release: generate a third-party
  attribution file (e.g. `cargo install cargo-about && cargo about generate`).
  No obligation is triggered by publishing source via git.
- Exactly one dependency ships an extra notice file: `cfg_aliases`
  (`NOTICES.md` — an MIT attribution for a macro it vendors from tectonic).
  Permissive; note it in any future binary bundle.
- No dependency ships a `NOTICE` file in the Apache-2.0 aggregation sense;
  no Apache NOTICE-retention obligation beyond license texts.

### 5.8 Unresolved licensing issues

**None blocking.** The only open item is tooling: this host cannot run
`cargo deny check licenses` (tool not installed). The inventory above is
exhaustive over the lockfile (403/403 packages with license data,
cross-checked against 339/339 on-disk manifests), so the gap is
reproducibility of the *check*, not missing data. Recommended before the
first tag (requires network + tool install, deliberately not done here):

```console
cargo install cargo-deny   # pin a version in CI once CI exists
cargo deny check licenses
```

### 5.9 Proposed cargo-deny configuration (for review — NOT created, NOT staged)

```toml
# deny.toml — proposal only; add to the repo only after a maintainer review
[graph]
all-features = true

[licenses]
# Everything observed in Cargo.lock (audit 2026-10-10) is permissive.
# GPL/AGPL/SSPL/BUSL/CC-BY-NC are intentionally absent → denied by default.
allow = [
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "ISC",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "0BSD",
    "Zlib",
    "Unlicense",
    "WTFPL",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "CDLA-Permissive-2.0",
    "BSL-1.0",
]
# MPL-2.0 and LGPL-2.1-or-later deliberately NOT listed: they appear only as
# electable OR-arms (r-efi, termina). Verify cargo-deny's OR-arm evaluation
# accepts those crates via their MIT arms on first run; if its expression
# evaluator requires the copyleft arm to be listed, prefer replacing the
# crates over widening the allow list.
```

### 5.10 Recommendation

**READY** — the MIT project license is complete and consistent (64/64
crates), and all 339 external dependencies carry declared, permissive,
redistribution-compatible licenses with zero missing/ambiguous data and zero
conflicts. Residual follow-ups (cargo-deny confirmation run, third-party
attribution file before binary distribution) are process hardening, not
license blockers.

## 6. Screenshots captured and missing

All captured from the **real application** (binary built 2026-10-10 from this
tree) on the live Hyprland session, with a real local Ollama provider
(`llama3.1:8b`) bound through the actual first-run setup wizard, against an
isolated `HOME` **and** a portable working directory (`/tmp/openrt-demo`,
fresh store) so no machine-identifying data appears. All five were
**re-captured in the final gate** at 1280×1024 (fullscreen) and verified by
OCR text extraction: 0 hits for the local username or `/home/…` paths; the
file-write target reads `/tmp/openrt-demo/demo.txt`.

| Screenshot | Status | Shows |
|---|---|---|
| `docs/assets/screenshots/main-chat.png` | ✅ captured (final gate) | Agent Mode chat, completed task, live status rails |
| `docs/assets/screenshots/permission-request.png` | ✅ captured (final gate) | live permission card (`filesystem.write`, risk MEDIUM, scope choices) mid-task |
| `docs/assets/screenshots/models-view.png` | ✅ captured (final gate) | Models view: provider ollama, provider status reachable, resource-fit honesty, curated catalog, 5 registry cards |
| `docs/assets/screenshots/task-completion.png` | ✅ captured (final gate) | tool result green + agent confirmation + audit-linked status bar |
| `docs/assets/screenshots/workbench-timeline.png` | ✅ captured (final gate) | Workbench 4-panel + Ops timeline (SessionCreated → … → PermissionRequested → TaskCompleted) |

**Missing: none of the priority captures.** No fabricated or mock UI images
were produced; every capture is a real window of the running binary.

## 7. Validation commands and actual results

Run on 2026-10-10 on Linux x86_64 (16 CPU, 15.9 GB RAM, RTX-class GPU):

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | **exit 0**, no diffs |
| `cargo check --offline --workspace` | **exit 0**, `Finished dev profile` |
| `cargo clippy --workspace --all-targets --offline` | **exit 0** — 0 errors, **443 pre-existing warnings** across crates (no `-D warnings` gate configured; largest: `clippy::await_holding_lock` 69×, `field_reassign_with_default` 24×) |
| `cargo test --offline --workspace` | **2590 passed · 0 failed · 0 ignored-failures** (full log on dev host; includes the new segment-event and multi-segment-render tests) |
| `openrt --version` | `openrt 0.1.0` |
| `openrt doctor` | **86 PASS · 13 UNAVAILABLE · 0 FAIL · 2 WARN** (WARNs: air-gap posture in Provisioning mode; whisper not on PATH → ASR unavailable) |
| Real-provider live test (prior cycle) | `real_ollama_pause_then_resume_preserves_transcript` **PASS** against Ollama; tool-call flow re-verified visually via screenshots above |
| Dependency license audit (`cargo metadata --locked` + jq inventory + registry-manifest cross-check, §5.2) | **exit 0** — 403/403 packages with license data; 339/339 external manifests match; 0 restricted/unknown licenses |
| Live streaming / live compaction | **NOT VALIDATED** (see §8) |

## 8. Known release blockers

1. **Streaming path not validated** — Ollama streaming can end without a
   terminal `done` frame under some conditions; the client fails honestly.
   Documented in README § Known limitations. Not a security issue; do not
   claim streaming as production-ready.
2. **Live context compaction not demonstrated** — guard + marker are
   unit-tested; no real-provider run showed compaction this cycle.
3. **No CI** — all gate results in §7 were produced manually on one host.
4. **Clippy warnings ungated** — 443 pre-existing warnings exist; decide
   before tagging whether to fix or document them as accepted.
5. **Junk files at root** (`black_image.png`, `Pictures/`) — excluded from
   staging; untouched; delete only if the reviewer agrees.

Resolved in this final gate: local-path references scrubbed from the three
docs (§3), screenshots re-captured without identifying data (§6),
`openrt-safety` license field added (§5), staging performed against the
§2 allowlist (§9), and the **dependency license audit completed (§5) —
READY recommendation; run `cargo deny check licenses` once the tool is
available, before the first tag**.

## 9. Staging result and commands to publish (staged, NOT committed)

Staging was performed with the explicit allowlist (no `git add .`):

```console
git add .gitignore LICENSE README.md Cargo.toml Cargo.lock crates docs
```

Staged inspection results:

| Check | Result |
|---|---|
| `git status --short` | 579 `A ` entries; only `?? Pictures/` and `?? black_image.png` remain unstaged |
| `git diff --cached --stat` | **579 files changed, 258,908 insertions(+)** |
| Total staged size | **10,134,681 bytes (≈ 9.66 MiB)** |
| Composition | crates 473 · docs 101 · root 5 (`.gitignore LICENSE README.md Cargo.toml Cargo.lock`) |
| Source completeness | all **398** `.rs` files staged (a `.gitignore` pattern initially swallowed `crates/openrt-workspace/` — **caught by this inventory and fixed** by anchoring `/openrt-workspace/`) |
| Binary files staged | 5 screenshots (PNG) + 1 test fixture (`ocr_text.jpg`) — nothing else |
| Largest staged file | `crates/openrt-tui/src/agent/tests.rs` (458 KB); largest doc `project-complete-plan.txt` (249 KB) — no large binaries |
| Secret-pattern scan of staged content | only the two known **redaction/shield test fixtures** (`crates/openrt-core/src/redaction.rs`, `crates/openrt-shield/src/lib.rs`); 0 real credential matches |
| Excluded & confirmed ignored | `models/` (96 GB), `target/`, `.opencode/`, runtime `.openrt/` state (incl. a stray `crates/openrt-runtime/.openrt/secrets.key` generated by an old cwd run — **not** referenced by any test path, excluded as runtime state), `.env*` |
| Line endings / exec bits | 0 CRLF files; 0 executable files staged |

Remaining steps (prepared, **not executed**):

```console
# 1. Create the public repo (once, requires gh auth as 0xhroot)
gh repo create 0xhroot/openrt --public \
  --description "OpenRT — sovereign, local-first agentic AI runtime for Linux" \
  --source . --remote origin

# 2. Re-verify the staged snapshot before committing
git status --short                 # 579 staged + 2 excluded junk entries
git diff --cached --stat           # 579 files, ~9.66 MiB

# 3. First commit on a named branch
git branch -M main
git commit -m "OpenRT V1 — sovereign local-first agentic AI runtime (reference release)"

# 4. Push (only when explicitly approved)
git push -u origin main
```

**Stop condition met:** checks clean, allowlist staged for review; no remote
was created, no commit made, nothing pushed.
