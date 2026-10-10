# Model Library — the MODELS view end to end

**Status:** Accepted · **Date:** 2026-10-01
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md) · **Detail:** [MODEL_BROWSER.md](./MODEL_BROWSER.md) · **Registration vs execution:** [MODEL_REGISTRATION.md](./MODEL_REGISTRATION.md)

`Ctrl+←/→` or the palette (`Ctrl+K` → `MODELS`) — four panes over what
models exist, where they came from, and which one is answering. Every
value is a real read (registry cards, files on disk, provider facts);
missing facts render `UNKNOWN` with the reason.

## 1. The four panes (`Tab` / `Shift+Tab`)

| Pane | Content | Scope line shown in the view |
|---|---|---|
| `[Local]` | Registry cards + files on this machine | "local registry catalog (cards + files on this machine)" |
| `[Hugging Face]` | Live hub listing (metadata only — **no weights are downloaded by browsing**) | "live Hugging Face Hub (metadata only)" — or "…blocked by current mode" when the network is off |
| `[Installed]` | Artifacts registered on this machine | "registered on disk (execution readiness shown below)" — being present is not being loaded; the resource rows say which it is |
| `[Recommended]` | Registry recommendations for this host | derived from real capability probes |

Sovereign posture: with network blocked, the Hub pane states the block
instead of failing silently — external access never happens behind the
operator's back.

## 2. Keys

| Key | Action |
|---|---|
| `Tab` / `BackTab` | Cycle the four panes |
| `Enter` | Apply the selected model — staged, then committed once the provider answers a probe for it; a failed switch keeps the previous model |
| type a tag | Override: type a model tag + `Enter` to apply that exact id |
| `Ctrl+F` | Model search box (registry) / hub query box (Hub pane, when active) |
| `i` | Register flow: streamed digest, bounded header inspection, registered **by reference** with no copy — see [MODEL_REGISTRATION.md](./MODEL_REGISTRATION.md) |
| `s` | Drop-in scan of `models/<category>/` |
| `d` | (Hub) detail overlay — sizes, files, gating, `UNKNOWN (detail not loaded)` until fetched |
| `p` | (Hub) cycle pipeline facet filter |
| `f` | (Hub) filter overlay (facet dimensions) |
| `[` / `]` | (Hub) page the listing |
| `Ctrl+R` | Refresh the current pane from its real source |

`Ctrl+M` opens the model overlay from anywhere (same data, quicker
path); `/model` does the same from the input line.

## 3. Switching semantics

* The header always shows the **active** model — the override when
  switched, otherwise the provider default (never hidden).
* Switches go through `SwitchableProvider` → per-request override of the
  ONE provider; the base provider is untouched.
* Applying is confirmed by re-reading the provider: what the header shows
  is what will answer.

## 4. Honesty rules

* No invented sizes/quantization/downloads — `hf_sizes` fills in as
  probes land, otherwise `UNKNOWN (…)`.
* Import failures render the structured `MODEL IMPORT FAILED` card with
  the real reason (test: `unsupported_model_file_is_a_structured_refusal`).
* Hub search happens in a background join — the UI stays responsive
  (`tui_stays_responsive_during_hub_operations`).
* Remote download of weights from the TUI is **not** part of this view
  (see the capability matrix in the acceptance doc: UNAVAILABLE by
  design; import and drop-in scan are the local paths).

## 5. Related docs

* [MODEL_BROWSER.md](./MODEL_BROWSER.md) — hub browser detail: facets,
  pagination, `UNKNOWN` policy, directories, import/scanner, tests.
