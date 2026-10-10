# ADR-009: MoE / Expert Cache Content-Addressing

**Status:** Accepted
**Date:** 2026-09-17
**Applies to:** Phase 3 (MoE expert streaming, expert cache, memory hierarchy)

## Context

Phase 3 streams MoE expert weights across a VRAM → RAM → NVMe hierarchy and
caches frequently used experts. If cached/streamed weights are addressed by
name or slot, a swapped or poisoned shard could be loaded and defeat the
SHA256 verification performed at install time.

Prior conflict: expert streaming/cache specified adjacent to the immutable
verified store but with no stated relationship between cache keys and artifact
verification.

## Decision

1. **Content addressing.** Expert weights are retrieved by **content digest of
   the expert shard** (SHA256), derived from the verified model artifact
   manifest (ADR-001). Names and slot indices are not addresses.

2. **Load path.** On load from any tier (VRAM/RAM/NVMe), the cache key must
   equal the expert digest. A cache miss re-verifies from the verified artifact
   store before caching. A cache hit is only trusted if the key matches the
   expected expert digest for the model revision in use.

3. **Immutability.** Cache entries are immutable; eviction deletes; promotion/
   demotion moves the same digest rather than creating copies under new key
   aliases.

4. **Failure semantics.** A verification failure invalidates the cache line and
   falls back to the verified source, or fails safe for inference (policy:
   prefer recompute over a poisoned read). Never serve an unverified shard.

5. **Permissions and offline mode.** The memory-hierarchy store follows the
   foundation-store rules (restrictive file permissions — never world-readable
   weights, ADR-006). Offline verification of cache lines is possible from the
   bundled signature sets (ADR-003) when enabled.

## Consequences

- Slightly larger retrieval keys; robust against swap/poisoning.
- One content graph (manifests → shards → cache lines) simplifies integrity
  reasoning and Phase 6 supply-chain verification.
- Cache and registry are cleanly separated: metadata in the registry namespace,
  content-addressed shards in the hierarchy store.

**References:** ROADMAP.md Phase 3 (expert cache, MoE streaming, security
requirements, tests); ADR-001, ADR-003, ADR-006; OWNERSHIP.md.