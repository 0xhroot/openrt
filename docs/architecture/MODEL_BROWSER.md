# Model browser (Models view) — sections, Hugging Face honesty

**Status:** Accepted · **Date:** 2026-09-30
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md)

The Models view is a four-section browser over the ONE `ModelRegistry`
(`runtime.model_registry`) plus Hugging Face **metadata**. There is no second
registry, no parallel store, and no row that is not a real fact: every card
comes from `registry.list()`, every hub row from the hub API, every byte
count from bytes that were actually read (locally or from declared hub
metadata).

## Sections (`Tab` / `BackTab` cycle, cursor resets on every switch)

| Section | Rows | Source |
|---|---|---|
| `[Local]` | every registry card, grouped by `models/<category>` | `registry.list()` sorted by category rank |
| `[Hugging Face]` | marketplace rows (metadata only) | hub `search`, background, mode-gated |
| `[Installed]` | cards with `installed_bytes > 0` | derived from the same list |
| `[Recommended]` | cards with `source == curated` | derived from the same list |

Every section renders a **scope line** under `provider` so LOCAL and
INSTALLED can never be confused: Local = "local registry catalog (cards +
files on this machine)", Installed = "registered with real bytes on disk
(usable now)", Recommended = "curated offline catalog entries", Hub = "live
Hugging Face Hub (metadata only)" (or "… — blocked by current mode").

`list_len` (navigation bounds) and `section_card` (Enter target) always read
the ACTIVE section, so a cursor position can never address a row from a
different list.

## The Hugging Face section

- **Search box** — `Ctrl+F` types into the shared query field; `Enter` runs
  the hub query (empty query = the hub's recent listing). The results land in
  the `[Hugging Face]` section, not in a parallel list.
- **Sovereign / blocked modes** — the section is always present (never
  hidden). The status **card** states the block, the fact, and the way
  forward:

  ```
   STATUS: BLOCKED BY SOVEREIGN MODE
   EXTERNAL NETWORK: OFF
   ACTION: Switch to Provisioning Mode to browse
           the live Hugging Face Hub
   OFFLINE NOW: i = IMPORT LOCAL MODEL · s = drop-in scan of models/<category>/
  ```

  The authority check (`modes.ensure_network_allowed()`) runs **before
  anything is staged**: no background task is spawned, no socket is opened,
  and the section says `hub not contacted — <mode reason>` with **zero rows**
  — never a "refused" error for a request that was never attempted, never a
  hang, never a fabricated row. The same authority runs before every detail
  fetch (the detail pane shows the mode's own reason instead).
- **Pagination** — `[` / `]` page the listing by 10 rows through the hub
  API's own `limit`/`skip` (`search_page`); the status line shows the
  current `skip`. The gate still runs first: under a blocked mode the offset
  advances but nothing is fetched (and the section says so).
- **Filters (`f`)** — a client-side overlay over the LOADED rows with nine
  dimensions: Task, Provider, Parameters, Quantization, Context, Vision,
  Tool calling, Format, Runtime. Values are derived only from real row
  metadata (`pipeline_tag`, `author`, `params`, quantization tags, vision /
  tool-calling tags); a dimension the loaded rows cannot answer (Context,
  Format, Runtime) renders `no data in loaded rows` and cannot be set —
  never a fabricated value. `↑/↓` picks the dimension, `←/→` cycles its
  value (`(all)` first), `c` clears every dimension, `Enter`/`Esc` closes.
- **Rows** — `id · pipeline tag · real size (hf_sizes probe) · gated marker`.
  Sizes come from the hub's declared file sizes (top 8 rows, background,
  same gate); until they land the row says `size …`, a failed probe says
  `size: fetch failed`.
- **Detail facts** (`d` opens the same facts as a Preview overlay; wide
  terminals ≥ 100 cols also render them in a right-hand `detail` pane) —
  only declared facts, and **every unknown field prints `UNKNOWN` with why**:

  ```
  id        author      pipeline      license      params      tags
  context   downloads   likes         gated         sha (7 chars)
  files     format      quant         vision        tools       runtime
  size      install     gpu           fit
  ```

  `vision` / `tools` are derived from the REAL pipeline and tag lists
  (`yes (vision pipeline/tags)`, `no (not in tags)`, else
  `UNKNOWN (no metadata)` / `UNKNOWN (no tags)`); `runtime` is always
  `UNKNOWN (no installer metadata)` — the browser never guesses capability
  or runtime support from a model name. `install metadata only — Remote:
  UNAVAILABLE (no downloader) · i = import local`; `gpu n/a (not probed)`;
  `fit <size> vs local VRAM` only when BOTH are known (a size check,
  explicitly "not a guarantee").
- **Install honesty** — Enter on a hub row NEVER claims a remote install.
  Installed row → apply its provider tag (only when `suggest_provider_tag`
  can derive one; `curated/llama3.1-8b` → `llama3.1:8b`, never the hyphen
  form). Not installed → `metadata only; weights are never fetched — press i
  to import a local copy`. Remote weight download does not exist in the TUI
  and is never implied.
- **`p`** cycles a pipeline facet (`All → each distinct pipeline_tag in
  listing order → All`); the facet AND every active `f` filter together
  drive `hub_indices()` (what the cursor navigates).
- **Empty hub** (e.g. a blocked mode or failed query) — `Enter` re-runs
  the query (never a no-op).

## Hardware facts

`gpu` line and the `fit` line read from a real background probe
(`nvidia-smi --query-gpu=name,memory.total,memory.used …`, spawned on first
entry into the Hub section, once). No tool, no output, or a parse failure →
`n/a`/`probe failed` — the browser never displays a device that was not
observed. The `fit` comparison is raw bytes vs raw VRAM and is labeled a
size check, not a compatibility verdict.

## Local model dirs & drop-in scan (`s`)

Entering the Models view **auto-creates** (never deletes) the 12 category
dirs under `models/` (`chat coding vision image video audio enhance
embedding reranker ocr multimodal utility`). The Local section's empty
hint says `NO MODELS FOUND — drop weight files into models/<category>/ and
press s`; `s` walks those dirs for `safetensors · gguf · bin · pt · pth ·
onnx · npz` and imports each file through the ONE registry (`import_local`
— same helper as `i`): real bytes, real sha256, `MemAvailable/2` size
guard, mode-gated like every materialization (`Provisioning` allows,
`Sovereign` refuses). An empty walk reports
`NO MODELS FOUND under <root> (created N category dirs — drop weight files
in and press s)`. Per-file outcomes are collected into the status line
(`drop-in: imported k/n …` / `drop-in: 0/n imported — first error: …`);
nothing is skipped silently.

## Local import (`i`) — errors & scanning

Import failures are **structured, shown, and cleared**: any failed import
sets `import_error`, which renders as a
` MODEL IMPORT FAILED` card with `Reason: <exact error>` above the list
(success or starting a new import clears it). Refusals include:

- unsupported extension / no extension → `unsupported model file format
  "<ext>" — supported: safetensors · gguf · bin · pt · pth · onnx · npz`
  (checked BEFORE anything is spawned),
- missing path, size guard, digest, and mode refusals as before.

When `OPENRT_SCAN_BUNDLE` points at a valid signature-bundle JSON (same
semantics as the CLI's `--scan-bundle`), the import runs the ONE
`openrt_scan::Scanner` (min scheme version `1`); without a bundle the
import still succeeds but the status says `verified but not scanned (set
OPENRT_SCAN_BUNDLE…)` — an empty scanner would refuse, so it is never
wired. Success messages carry the real `sha256` (`import sha256 <first 12>…`).

## Single background jobs

`refresh_join` (searches/list loads, latest-wins), `hf_size_join`,
`hf_detail_join` (single-flight, superseded when the cursor moves),
`hw_join`, `import_join` (manual import and drop-in scan share the slot —
a second start is refused with a status message, never a dropped task).

## Registry metadata (PART G)

`HfModelLite` carries `pipeline_tag · tags · license · params · author`
(`#[serde(default)]`, so older payloads still parse) and `HfModelInfo` adds
`context_length` (`config.max_position_embeddings`, root or `text_config`),
`used_storage`, and the same tag/license/params/author fields. All
extraction is pure helpers (`extract_license` / `extract_params` /
`extract_context` / `lite_from`) with unit tests — facts only; a field the
hub does not declare stays `None` and renders as unknown.

## Tests

`cargo test --offline -p openrt-tui` — section cycling, sovereign no-fetch,
hub query dispatch + empty-hub requery, section partition, metadata-only
install status, detail/GPU honesty (incl. the pre-socket gate), drop-in
scan (sovereign refusal → provisioning import with real size + digest),
pipeline facet, and a render test (tabs, banner, status, detail pane at
120×40, smoke at 80×24). The OPENRT HF-surface battery adds: real search
dispatch without blocking the UI, `NO MODELS FOUND` + auto-created dirs,
local import with the real `sha256`, structured refusals (missing path,
unsupported format), the `MODEL IMPORT FAILED` card, `UNKNOWN` in every
missing detail field, client-side filters + `[`/`]` pagination, and the
exact `llama3.1:8b` provider tag.
`cargo test --offline -p openrt-registry` covers the HF parsing helpers,
legacy-JSON defaults, and `search_page` (`limit`/`skip` clamps).
