# Security Control Center — the SECURITY view

**Status:** Accepted · **Date:** 2026-10-01
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md) · **Companion:** [PERMISSIONS_UX.md](./PERMISSIONS_UX.md)

`Ctrl+,`-adjacent sibling of Settings: `Ctrl+←/→` or the palette
(`Ctrl+K` → `SECURITY`) opens a read-write surface over the **one**
policy document, the shield findings store, and the boot recovery
records. Every row is a real read; every write is an append-only,
audited action.

## 1. The rows

```
policy document     version <n> · sha256 <…>          (or "UNKNOWN: <why>")
rule <pattern>      Allow|Deny|Ask · scope <…>        (live rules, in order)
shield findings
  [CRITICAL] <category>   <reason> · <status>
recovery
  record boot-<uuid>      N interrupted · N incomplete · N stale sandboxes · clean=<bool>
```

* An unreadable policy document renders `UNKNOWN` with the reason — it is
  never faked or skipped.
* Findings come from `openrt_shield::store::list_findings` (the same
  store the CLI reads).
* Recovery rows come from `Runtime::recovery_records` (see
  [AGENT_RECOVERY.md](./AGENT_RECOVERY.md)).
* `Ctrl+F` filters across patterns, actions, severities, reasons and
  record ids.

## 2. The actions

| Key | Action | Safety rail |
|---|---|---|
| `Enter` | Open the policy document (read-only view: text, version, digest) | none needed — read-only |
| `1` | Append the **read-only** preset (allow rules derived from registered read-only capabilities) | confirm (`y`), rule count shown before applying |
| `2` | Append the **full-trust** preset (every registered tool + capability) | confirm (`y`), count shown, explicit wording |
| `n` | New rule: `<pattern> <allow\|deny\|ask> [scope]` | Enter reviews → confirm; invalid input fails closed |
| `l` | Security audit (runs the audit action and surfaces the result) | — |
| `Ctrl+F` | Filter the rows | — |

Presets and rules are appended with `append_rule` — **immutable
appends**. The live gate keeps the policy it was built with until the
next runtime build (stated in the view footer, not hidden).

## 3. Why appends only

1. **Auditability** — every change produces an audit record (ADR-002
   chain, `openrt audit verify`).
2. **Order semantics** — `Policy::evaluate` is first-match-wins over the
   ordered rule list; reordering or editing in place would silently
   change which rule wins. Appends keep the history legible.
3. **Tier grants don't live here** — session/workspace allow tiers are
   gate-scoped, in-memory, and cannot beat an earlier `Ask` rule
   (see [PERMISSIONS_UX.md](./PERMISSIONS_UX.md) §3).

## 4. What it deliberately does not do

* No delete/edit of existing rules (append-only document).
* No network or provider changes (mode + model surfaces own those).
* No "trust everything" one-key action — preset 2 is spelled out and
  confirmed.
* No hiding of shield findings or recovery records (they render as-is,
  including empty states).
