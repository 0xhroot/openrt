# Model registration and execution boundaries

**Status:** Accepted · **Date:** 2026-10-05
**Parent:** [MODEL_LIBRARY.md](./MODEL_LIBRARY.md) · **Companion:** [MODEL_BROWSER.md](./MODEL_BROWSER.md)

## The claim this document exists to prevent

> *"The file is 9.35 GiB and this machine has 10 GiB free, so the model is
> installed and ready to run."*

Three separate facts are welded together in that sentence, and they have
different answers:

| Fact | Measured how | Source |
| --- | --- | --- |
| The artifact exists on disk and is catalogued | `stat` + streamed sha256 | registration |
| The artifact's bytes are inside the content store | byte count written to the CAS | materialization |
| The artifact can execute on this host right now | resource admission at load | execution |

A file can satisfy the first and fail the second and third. OpenRT used to
report one number for all three, and refused a perfectly catalogable file
because its *size* was compared to *free RAM* at import time.

## Registration is not execution

Registration is bounded, advisory, and allowed for any readable artifact:

```bash
openrt model register local/chat/qwen3.5-9b-q8_0 models/chat/Qwen_Qwen3.5-9B-Q8_0.gguf
openrt model status local/chat/qwen3.5-9b-q8_0
```

Registration performs exactly four bounded operations:

1. `stat` the path.
2. Walk the container header with a fixed 256 KiB read buffer
   (`openrt_models::artifact::read_metadata`).
3. Stream the whole file through sha256 in 1 MiB chunks
   (`openrt_verify::sha256_file`) — one buffer regardless of artifact size.
4. Publish a manifest whose `BlobRef` carries
   `BlobLocation::External { path }` and write a lifecycle card.

**Zero bytes are copied.** The registry records where the artifact lives and what
it hashes to. Lifecycle stops at `Verified`.

Registration is not loading, and the surfaces say so:

```
REGISTERED ON DISK `local/chat/qwen3.5-9b-q8_0` · verified · 9.13 GiB ·
sha256 b58fe056b543… · bytes not copied (registry holds a reference) ·
execution: RESOURCE-LIMITED (advisory — needs ADVISORY 9.64 GiB, host reports
9.21 GiB free; registered, not loaded)
```

## Where the memory rule actually belongs

The old import path did this before registering anything:

```rust
// removed — file size compared against free RAM, at import time
if size > mem_available / 2 { return Err("refusing to hold more than half …"); }
```

That rule is gone from registration, and it is **not** re-introduced at
registration under a different name. What replaces it:

- **Registration** compares nothing. It records facts.
- **Metadata inspection** reports what the artifact declares, with provenance.
- **Execution** is where resource admission belongs, and admission stays with
  `openrt-resources` at the load boundary. Nothing in the registry, TUI, or CLI
  grants an override; `Runtime::grant_resource_override(reason, operator)` is
  still the only route, still policy-gated, still audited, still one-shot.

An important honesty note about this codebase: **OpenRT does not load model
weights in-process.** `openrt-provider-llamacpp` and `openrt-provider-vllm` are
HTTP clients to server processes the operator runs, and `openrt-model-cache`
evicts metadata, not weights. There is therefore no in-process weight allocation
for an execution-side rule to govern today. When one exists, that is where the
rule goes — not in the import path, which is where it was doing the most damage.

## What "needs 9.64 GiB" is made of

`ModelResourceProfile` keeps each number separate and tags how it was obtained
(`MEASURED` / `ADVISORY` / `NOT PROBED`):

| Field | Provenance | How |
| --- | --- | --- |
| `artifact_bytes` | MEASURED | `stat` |
| `weight_bytes` | MEASURED | file size minus the header we walked — the tensor payload is the rest of the file |
| `bits_per_weight` | MEASURED | `general.file_type` mapped to its quantization name |
| `kv_bytes_per_token` | ADVISORY | `layers × kv_heads × (key_length + value_length) × 2` (f16), from declared geometry |
| `kv_cache_bytes` | ADVISORY | per-token cost × **an assumed window**, never the declared maximum |
| `vram_bytes` | `None` | never invented: there is no GPU probe |
| `ram_bytes` | ADVISORY | weights + KV at the assumed window |

Two honesty rules are enforced in code:

- **The declared context is not the working context.** Qwen3.5 declares
  262144 tokens; priced at that maximum, every session would appear to need
  33 GiB of KV. Estimates use `ASSUMED_CONTEXT_TOKENS` (4096), state the
  assumption in the UI, and carry the per-token cost so any other window can be
  priced from the same number.
- **Unknown is an answer.** A format with no readable header, a missing
  `parameter_count`, or an unprobed GPU yields `None` and a `NOT PROBED` note,
  never a plausible-looking default.

The fit assessment (`assess_execution_fit`) is **advisory**. It compares the
requirement to host memory and returns `Ready` / `ResourceLimited` / `Unknown`.
It never blocks anything; `openrt-resources` decides at load.

Real output for the 9.13 GiB artifact in this repository:

```
  needs     9.12 GiB measured weights + 528.0 MiB KV at 4096 tokens (advisory)
  execution RESOURCE-LIMITED (advisory: 9.64 GiB needed, 9.21 GiB available)
  note      KV cache counted at an assumed 4096 tokens (528.0 MiB total,
            132 KiB per token, advisory); the file declares up to 262144
            tokens, which would need 33.00 GiB
```

## Header inspection is bounded, and stops when it knows enough

`read_metadata` walks the GGUF key/value block sequentially with one 256 KiB
buffer, stepping over arrays instead of collecting them, under a 64 MiB read
ceiling and a 1 MiB string-length limit. It exits as soon as it holds the
fields a profile needs (`architecture`, `file_type`, layers, embedding, context,
KV head count), which for real files is a few hundred bytes in — long before
the tokenizer vocabulary.

Measured on `models/chat/Qwen_Qwen3.5-9B-Q8_0.gguf` (9.13 GiB):

| Metric | Value |
| --- | --- |
| Bytes read | 10,485,558 (0.11% of the file) |
| Wall time | ~19 ms |
| Recovered | `qwen35`, `Q8_0`, 33 layers, 4096 embedding, 262144 declared context, 16 heads, 4 KV heads, 256 key/value length |

Two parser bugs this measurement exposed, both fixed and both covered by tests
in `crates/openrt-models/src/artifact.rs`:

- Array payloads were sized from a fixed-width table in which strings had width
  zero, so the cursor never advanced past `general.tags` and every later key was
  read as garbage (`unknown gguf type 18`). Real models were unreadable.
- A skip larger than the read buffer seeked zero bytes and looped forever,
  because the condition used `>=` where the buffer refill needed `>`.

## Materialization, when you actually want the bytes in the store

`openrt model install` copies the artifact into the content store, which means
holding it in memory: `PersistenceStore::put_blob` takes a `&[u8]`. The command
now refuses artifacts over 2 GiB and names the by-reference route rather than
attempting a multi-gibabyte read:

```text
/path/to/model.gguf is 9.13 GiB; `model install` copies the artifact into the
content store and needs it in memory. Use `openrt model register <id> <path>` to
record it by reference instead.
```

Making the copy itself stream would mean adding a chunked writer to
`PersistenceStore` and its implementations. That is not done, and is listed
below as remaining work rather than implied to be finished.

## Lifecycle

`openrt-registry` states are unchanged and still authoritative:

```
discovered -> downloaded -> verified -> scanned -> active | idle -> retired
```

- **Registration** produces `Verified` with an `External` blob location.
- **Activation fails closed** for external artifacts:

  ```text
  `local/chat/x` is registered by reference (/path/to/x.gguf); activation
  requires a materialized, scanned artifact — import it with `openrt model
  install` to store the bytes
  ```

  An unscanned artifact is not routable. Registration deliberately cannot
  promote itself to `Active`, because scanning is a gate that registration has
  no business satisfying.

## Model switching is commit-on-success

Entering a model id **stages** the override. It becomes the active model only
after the provider answers a capability probe for it. A probe that fails leaves
the previous model in place and says so:

```text
switch to bad:model refused: connection refused — still on good:model
```

Previously the override was written on Enter and the error reported afterwards,
so every later request carried a model nobody had confirmed.

## Measured costs, and what is not claimed

| Operation | Cost |
| --- | --- |
| Register 9.13 GiB artifact (release) | ~19 ms metadata + ~8.6 s sha256 (1.14 GB/s) |
| Register 9.13 GiB artifact (debug build) | ~200 s of sha256; the opt-in test that does this is `#[ignore]`d |
| Bytes held in memory during registration | 256 KiB header buffer + 1 MiB digest chunk |

Not claimed:

- **No true mmap or lazy weight paging in OpenRT.** If the external llama.cpp or
  vLLM server maps the file, that is the server's behaviour, on a path OpenRT
  hands it — not an OpenRT implementation.
- **No streaming of model weights.** Token streaming is an HTTP response
  property of the backend; see `PROVIDER_COMPATIBILITY.md` for what a provider
  must actually do.
- **No live-TTY validation** of any of the above.

## Remaining work

- Streaming materialization: add a chunked writer to `PersistenceStore` so
  `model install` never holds a whole artifact, then lift the 2 GiB refusal.
- Execution-side admission for in-process loading, when such a loader exists.
- A real GPU probe, so `vram_bytes` stops being `None`.
- An external-registration route to `Scanned` for artifacts whose scan bundle is
  supplied by reference, without materializing.