# M7 — Multi-Shard Model Anatomy — HANDOFF

## Executive summary

M7 extends the M2 anatomy so a **multi-file** model (safetensors shard files +
`*.safetensors.index.json`) is described exactly like a single-file model:
unity of storage layout, tensor locations, and format — **metadata only**. The
schema change is purely additive (`shard_files` with `#[serde(default)]`), the
version stays `1`, and pre-M7 records still load unchanged. The runtime planner
sizes storage from the shard sum for multi-shard models while the certified
resident working set still comes from tensor-payload lengths — storage layout,
residency, and execution remain distinct axes. No mmap, streaming, offload,
prefetch/eviction, KV-spill, quantization, multi-SSD execution, memory
movement, or backend execution change was made.

## Files changed

| file | change |
|---|---|
| `crates/openrt-models/src/anatomy.rs` | M7 schema (`ShardAnatomy`, `shard_files`, helpers), multi-shard machinery (`build_sharded_safetensors_anatomy`, `sharded_anatomy_for_index_file`, `is_safetensors_index_file`, `validate_shard_name`, `read_shard_header`), shared `parse_safetensors_header_bytes`, +14 tests |
| `crates/openrt-models/src/lib.rs` | re-export `ShardAnatomy`, `is_safetensors_index_file`, `sharded_anatomy_for_index_file`, `ANATOMY_MAX_INDEX_BYTES`, `ANATOMY_MAX_SHARDS` |
| `crates/openrt-registry/src/lib.rs` | `capture_anatomy` multi-shard flow (index sniff → sharded capture → fail-open Unknown parity) +5 tests |
| `crates/openrt-runtime/src/runtime_memory.rs` | `propose_memory_plan` picks aggregate shard size for multi-shard artifacts (single-file path unchanged) +4 tests |
| `crates/openrt-cli/src/main.rs` | `model compatibility` prints the shard layout for multi-shard records |
| `docs/architecture/MEMORY_FABRIC.md` | new M7 section; Future list is now M8–M13 |

## Schema changes (additive, version-stable)

```rust
pub struct ShardAnatomy { shard: u32, name: String, file: String,
                          size_bytes: u64, checksum: Option<String> }

pub struct ModelAnatomy {
    // unchanged v1 fields …
    #[serde(default)]
    pub shard_files: Vec<ShardAnatomy>,   // NEW — empty for pre-M7 records
}
```

- `ANATOMY_SCHEMA_VERSION` stays **1** (registry const and models const both).
  `#[serde(default)]` means old records (no `shard_files`) and new records
  deserialize into the same type — backward compatible by construction, no
  migration step, no second registry.
- `TensorAnatomy.shard` becomes the index into `shard_files`; `offset`/`length`
  are within `shard_files[shard]`. For a v1 record `shard` is still `0` and
  names the sole file.
- `total_bytes` for a sharded model = sum of shard sizes (storage). Helpers:
  `shard_count()`, `is_multi_shard()`, `shard_bytes()` (checked sum, `None`
  when no shard records — never a fabricated sum).

## Supported formats

- **Safetensors multi-file** — via a content-sniffed index (`weight_map` JSON
  object), per-shard headers bounded to 16 MiB, payloads never read.
- **Safetensors single-file** and **GGUF** — unchanged `anatomy_for_file`.
  GGUF is deliberately **not** multi-file; a registered file is that file.
  A lone member of a sharded safetensors model is treated as an ordinary
  single-file artifact (regression-tested).

## Persistence changes

`capture_anatomy` (fail-open during `register_local`, which stays single-file
by-reference — the registered artifact is the index):

1. single-file parser → store when a format is recognized;
2. else sniff for an index (`is_safetensors_index_file`) → `sharded_anatomy_for_index_file`;
3. else parity fallback: store the Unknown record or nothing, exactly as M2.

The `AnatomyRecord.size_bytes` keeps the registered member's size; the
multi-shard geometry lives in `anatomy.total_bytes`/`shard_files`. No shard is
ever re-hashed during registration (`checksum: None` is the honest unknown).

## Bounded-read measurements

| read | ceiling | behaviour |
|---|---|---|
| index file | `ANATOMY_MAX_INDEX_BYTES` = 16 MiB | refused outright if larger (never JSON-parsed) |
| per-shard header | `ANATOMY_MAX_SAFETENSORS_HEADER_BYTES` = 16 MiB | length checked before allocation; truncated/implausible lengths error before any buffer is built |
| payload bytes | **0** | offsets/sizes come from headers and `metadata.len()`; structural file reads never touch the data block |
| distinct shards / tensors | `ANATOMY_MAX_SHARDS` = 4096 / `ANATOMY_MAX_TENSORS` = 2^20 | checked pre-allocation |

## Security / path validation

Shard references are untrusted input. `validate_shard_name` rejects absolute
paths, `..`/`.`/empty components, backslashes, and NULs. The fs resolver
canonicalizes the index directory and each shard, refusing anything that
resolves outside (symlink escapes fail closed). Consistency is strict:
`weight_map` must **exactly** equal the union of shard-header tensors —
missing shards, duplicate/extra/mis-mapped tensors, truncated shards, malformed
indexes, and size/count overflows all error (the registry then persists the
Unknown fallback, never a wrong layout). Ordering is deterministic: shard order
by sorted distinct names, tensors by `(shard, offset, name)`.

## Runtime planning impact

- `artifact_bytes` for a multi-shard model = `shard_bytes()` (aggregate storage;
  the manifest/card knows only the index member). Single-file path is the M6
  card-based size, bit-for-bit unchanged.
- The resident working set (tensor-payload length sum, only when *every*
  length is known) and the `ResourceGate` posture are entirely untouched —
  storage ≠ residency ≠ execution. Tested: 9 GiB split as 3 × 3 GiB shards with
  a 6 GiB resident estimate — plan reports 9 GiB storage and a 6 GiB certified
  request on the resident-estimate basis.

## Tests

+23 tests (M6 workspace total 2474 → **2497**):

- **models** (14): stable aggregation, determinism, duplicates, unmapped extra,
  absent shard, wrong-shard mapping, empty/non-object/non-string indexes,
  traversal names, truncated/oversized shards, no-allocation implausible
  length, disk e2e, content-led sniffing (incl. oversized refusal), symlink
  escape, single-member-is-single-file.
- **registry** (5): register_local captures multi-shard anatomy, single-member
  stays single-file, inconsistent index fails closed to Unknown, v1 record
  without `shard_files` loads, multi-shard anatomy persists/reloads through a
  fresh registry instance.
- **runtime** (4): aggregate storage vs index-member size, determinism,
  resident estimate ignores shard sizes, v1 single-file keeps card sizing.

## Gate tally

| command | result |
|---|---|
| `cargo fmt --all -- --check` | GREEN |
| `cargo check --workspace --offline` | GREEN (0 errors) |
| `cargo clippy --workspace --all-targets --offline` | 0 errors; no new warnings from M7 code (remaining warnings are pre-existing in untouched crates) |
| `cargo test --offline --workspace` | **2497 passed; 0 failed; 0 ignored** (+23 vs M6) |
| `cargo test -p openrt-provider-llamacpp --features llamacpp` | 4/4 |
| `cargo test -p openrt-provider-vllm --features vllm` | 4/4 |
| CLI end-to-end (mock) | `model register` of a 3-shard index → `model compatibility` prints `layout: 3 files` with per-shard sizes |

## Remaining limitations

- Execution is still not implemented — no component hands a `MemoryPlan` to a
  backend, and nothing moves bytes (necessarily unchanged; M13).
- Shard `checksum` is always `None` (shards are never re-hashed during
  registration); a production verifier for shards is outside this milestone.
- GGUF is single-file only, by documented boundary.
- The register status card's `assess_execution_fit` notes ("metadata NOT
  PROBED") express that this separate legacy assessment cannot read parameter
  geometry from a JSON index; the anatomy capture itself succeeds. Wording is
  out of scope for this milestone.
- Real device-residency observation for a certified VRAM fit remains M9.

Milestone stops here. Next: M8 (quantized-length derivation) — does **not**
begin in this session.