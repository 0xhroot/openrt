# ADR-001: Model Artifact Provenance (Base vs Derived)

**Status:** Accepted
**Date:** 2026-09-17
**Applies to:** Phase 3 (base artifacts, registry), Phase 4 (derived artifacts)

## Context

Phase 3 mandates a content-addressed, immutable model store keyed by SHA256.
Phase 4 adapts models through SFT/LoRA/QLoRA/DPO, merging and quantization.
An adapted, merged, or quantized artifact can never equal the hash of its base
model. Without an explicit provenance model, the Phase 3 verification chain
breaks for any Phase 4 output, and Phase 6 supply-chain verification (provenance
signatures, SBOM, reproducible verification) has nothing to lineage.

Prior conflict: the roadmap required "all adapted/quantized artifacts are
SHA256-verifiable and registry-managed" without defining how a derived artifact
is verified against a base.

## Decision

1. **Two artifact classes.**
   - **Base artifacts** — immutable inputs installed/imported in Phase 3.
     Content-addressed by SHA256 of full content; never modified in place; any
     change produces a new digest.
   - **Derived artifacts** — outputs of a transformation (SFT, LoRA, QLoRA, DPO,
     merge, quantize). Never content-identical to a base.

2. **Derived-artifact manifest.** Every derived artifact MUST record:
   - base model SHA256 (each immutable input),
   - adapter hashes (for adapted artifacts),
   - training recipe/config digest (deterministic serialization of
     hyperparameters, framework, and code version),
   - dataset identity (manifest digest of the exact dataset revision used),
   - parent artifact digests (transitive closure),
   - transformation metadata (type — `sft|lora|qlora|dpo|merge|quantize`;
     tool version; quantization scheme and parameters),
   - operator identity and timestamp (signed).

3. **Provenance chain.** Derivation is a DAG of (input digests → output digest)
   edges. A derived artifact's **provenance digest** = hash of (inputs + recipe).
   Verification recomputes the recipe and verifies every input's contract
   recursively to immutable base roots. Verification is computed locally, never
   by an external service.

4. **Registry behavior.** Manifests are immutable after write; one manifest per
   artifact. No in-place "upgrade". A derived artifact whose chain is broken or
   whose manifest is missing mandatory fields is refused activation (fail-closed
   in Sovereign Mode, ADR-003).

5. **Reproducibility caveat.** If the toolchain is deterministic, identical
   inputs + identical recipe digest ⇒ identical output digest. Non-deterministic
   training (e.g., GPU nondeterminism) is recorded as metadata and validated by
   evaluation (Phase 4), not by digest equality.

## Consequences

- Derived artifacts are **never** compared directly to a base hash; they verify
  through the chain.
- Phase 3 registry schema includes both artifact classes and manifest fields
  from the start.
- Phase 4 must emit provenance manifests at training/conversion time, not
  retroactively.
- Slight manifest storage growth; negligible vs model weight size.
- Phase 6 supply-chain verification lines up cleanly: artifact = digests + chain;
  update = chained delta, never a replacement overwrite.

**References:** ROADMAP.md Phase 3 (registry, SHA256, derived artifacts,
security), Phase 4 (adapter management, quantization, registry integration);
OWNERSHIP.md.