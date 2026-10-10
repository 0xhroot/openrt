//! Content-addressed model cache metadata + LRU policy (ADR-009).
//!
//! This crate tracks *metadata only*: digests, tier placement, pinned flag and
//! an access stamp. Content bytes live elsewhere (provider-local storage or the
//! hierarchy store). The cache is keyed by digest — content is immutable, so a
//! re-`put` with the same digest is a "touch", not a new entry. The access
//! `counter` is a deterministic monotonic stamp, **not** wall-clock time, so
//! tests and replay are reproducible.

use std::collections::BTreeMap;

use openrt_hierarchy::Tier;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

/// Namespace for cache metadata records.
pub const CACHE_NS: &str = "cache.meta";

/// Cache metadata errors.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("tier {tier:?} full: required {required} bytes, available {available}")]
    TierFull {
        tier: Tier,
        required: u64,
        available: u64,
    },
    #[error("cache entry `{0}` not found")]
    EntryNotFound(String),
    #[error("digest mismatch on tier {tier:?}: {reason}")]
    ExpectedDigestMismatch { tier: Tier, reason: String },
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
}

/// One cached model digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub digest: String,
    pub tier: Tier,
    pub size_bytes: u64,
    pub pinned: bool,
    pub last_access: u64,
    pub created_at: String,
}

/// LRU metadata cache with tier capacity accounting.
///
/// `BTreeMap`s everywhere for deterministic ordering; `counter` is a
/// monotonic access stamp (deterministic, not wall clock).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCache {
    capacities: BTreeMap<Tier, u64>,
    entries: BTreeMap<String, CacheEntry>,
    used: BTreeMap<Tier, u64>,
    counter: u64,
}

impl ModelCache {
    pub fn new(capacities: BTreeMap<Tier, u64>) -> Self {
        Self {
            capacities,
            entries: BTreeMap::new(),
            used: BTreeMap::new(),
            counter: 0,
        }
    }

    fn capacity(&self, tier: Tier) -> u64 {
        self.capacities.get(&tier).copied().unwrap_or(0)
    }

    fn used_of(&self, tier: Tier) -> u64 {
        self.used.get(&tier).copied().unwrap_or(0)
    }

    /// Insert a fresh entry or "touch" an existing one (content-addressed:
    /// same digest ⇒ same immutable content).
    pub fn put(
        &mut self,
        digest: String,
        tier: Tier,
        size_bytes: u64,
        pinned: bool,
    ) -> Result<(), CacheError> {
        if size_bytes == 0 {
            return Err(CacheError::InvalidAmount(
                "size_bytes must be non-zero".into(),
            ));
        }
        let capacity = self.capacity(tier);
        if size_bytes > capacity {
            return Err(CacheError::TierFull {
                tier,
                required: size_bytes,
                available: capacity,
            });
        }

        if let Some(entry) = self.entries.get_mut(&digest) {
            self.counter += 1;
            let stamp = self.counter;
            // Re-insert is a touch. If the tier moved, fix accounting.
            let old_tier = entry.tier;
            let old_size = entry.size_bytes;
            if old_tier != tier {
                let old_used = self.used.entry(old_tier).or_insert(0);
                *old_used = old_used.saturating_sub(old_size);
                let new_used = self.used.entry(tier).or_insert(0);
                *new_used += size_bytes;
            } else if size_bytes >= old_size {
                let cur = self.used.entry(tier).or_insert(0);
                *cur += size_bytes - old_size;
            } else {
                let cur = self.used.entry(tier).or_insert(0);
                *cur = cur.saturating_sub(old_size - size_bytes);
            }
            entry.tier = tier;
            entry.size_bytes = size_bytes;
            entry.pinned = pinned;
            entry.last_access = stamp;
            Ok(())
        } else {
            self.counter += 1;
            let stamp = self.counter;
            self.entries.insert(
                digest.clone(),
                CacheEntry {
                    digest,
                    tier,
                    size_bytes,
                    pinned,
                    last_access: stamp,
                    created_at: openrt_core::now().to_rfc3339(),
                },
            );
            let cur = self.used.entry(tier).or_insert(0);
            *cur += size_bytes;
            Ok(())
        }
    }

    /// Refresh the access stamp of an existing entry.
    pub fn touch(&mut self, digest: &str) -> Result<(), CacheError> {
        self.counter += 1;
        let stamp = self.counter;
        let entry = self
            .entries
            .get_mut(digest)
            .ok_or_else(|| CacheError::EntryNotFound(digest.to_string()))?;
        entry.last_access = stamp;
        Ok(())
    }

    pub fn pin(&mut self, digest: &str) -> Result<(), CacheError> {
        let entry = self
            .entries
            .get_mut(digest)
            .ok_or_else(|| CacheError::EntryNotFound(digest.to_string()))?;
        entry.pinned = true;
        Ok(())
    }

    pub fn unpin(&mut self, digest: &str) -> Result<(), CacheError> {
        let entry = self
            .entries
            .get_mut(digest)
            .ok_or_else(|| CacheError::EntryNotFound(digest.to_string()))?;
        entry.pinned = false;
        Ok(())
    }

    /// Evict the least-recently-accessed *unpinned* entry on `tier`.
    pub fn evict_lru(&mut self, tier: Tier) -> Result<CacheEntry, CacheError> {
        let victim = self
            .entries
            .values()
            .filter(|e| e.tier == tier && !e.pinned)
            .min_by_key(|e| e.last_access)
            .map(|e| e.digest.clone())
            .ok_or_else(|| CacheError::TierFull {
                tier,
                required: 0,
                available: self.remaining(tier),
            })?;

        let entry = self.entries.remove(&victim).expect("victim exists");
        let cur = self.used.entry(tier).or_insert(0);
        *cur = cur.saturating_sub(entry.size_bytes);
        Ok(entry)
    }

    /// Evict LRU entries on `tier` until `used + required <= capacity`.
    pub fn evict_lru_to_fit(&mut self, tier: Tier, required: u64) -> Result<(), CacheError> {
        while self.used_of(tier) + required > self.capacity(tier) {
            match self.evict_lru(tier) {
                Ok(_) => {}
                Err(CacheError::TierFull { .. }) => {
                    return Err(CacheError::TierFull {
                        tier,
                        required,
                        available: self.used_of(tier),
                    });
                }
                Err(other) => return Err(other),
            }
        }
        Ok(())
    }

    pub fn lookup(&self, digest: &str) -> Option<&CacheEntry> {
        self.entries.get(digest)
    }

    pub fn used(&self, tier: Tier) -> u64 {
        self.used_of(tier)
    }

    pub fn remaining(&self, tier: Tier) -> u64 {
        self.capacity(tier).saturating_sub(self.used_of(tier))
    }

    pub fn pinned(&self, digest: &str) -> bool {
        self.entries.get(digest).map(|e| e.pinned).unwrap_or(false)
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Serializable snapshot used for persistence.
    pub fn snapshot(&self) -> CacheState {
        CacheState {
            capacities: self.capacities.iter().map(|(&t, &c)| (t, c)).collect(),
            entries: self.entries.values().cloned().collect(),
            counter: self.counter,
        }
    }

    /// Save the cache state as one record under [`CACHE_NS`].
    pub async fn save(&self, store: &dyn PersistenceStore) -> Result<(), CacheError> {
        let value = serde_json::to_value(self.snapshot()).map_err(cache_serde_error)?;
        store.put_record(CACHE_NS, "state", &value).await?;
        Ok(())
    }

    /// Load cache state from [`CACHE_NS`]; `used` is recomputed from entries.
    pub async fn load(store: &dyn PersistenceStore) -> Result<Option<ModelCache>, CacheError> {
        match store.get_record(CACHE_NS, "state").await? {
            Some(v) => {
                let state: CacheState = serde_json::from_value(v).map_err(cache_serde_error)?;
                Ok(Some(state.into_cache()))
            }
            None => Ok(None),
        }
    }
}

fn cache_serde_error(e: serde_json::Error) -> CacheError {
    CacheError::Persistence(openrt_persistence::PersistenceError::Serialization(e))
}

/// Serde-friendly cache snapshot (deterministic ordering preserved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheState {
    pub capacities: Vec<(Tier, u64)>,
    pub entries: Vec<CacheEntry>,
    pub counter: u64,
}

impl CacheState {
    /// Rebuild a [`ModelCache`], recomputing per-tier `used`.
    pub fn into_cache(self) -> ModelCache {
        let mut cache = ModelCache {
            capacities: self.capacities.into_iter().collect(),
            entries: BTreeMap::new(),
            used: BTreeMap::new(),
            counter: self.counter,
        };
        for e in self.entries {
            *cache.used.entry(e.tier).or_insert(0) += e.size_bytes;
            cache.entries.insert(e.digest.clone(), e);
        }
        cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_hierarchy::Tier as T;
    use openrt_persistence::MemoryStore;

    fn caps(v: u64, r: u64, n: u64) -> BTreeMap<Tier, u64> {
        BTreeMap::from([(Tier::Vram, v), (Tier::Ram, r), (Tier::Nvme, n)])
    }

    fn digest(c: char, n: usize) -> String {
        std::iter::repeat(c).take(n).collect()
    }

    #[test]
    fn put_then_duplicate_is_touch() {
        let mut cache = ModelCache::new(caps(100, 100, 100));
        cache.put(digest('a', 64), Tier::Vram, 10, false).unwrap();
        cache.put(digest('a', 64), Tier::Vram, 10, false).unwrap();
        assert_eq!(cache.entry_count(), 1);
        assert_eq!(cache.used(Tier::Vram), 10);

        // Same digest re-put on a different tier moves accounting.
        cache.put(digest('a', 64), Tier::Ram, 10, false).unwrap();
        assert_eq!(cache.entry_count(), 1);
        assert_eq!(cache.used(Tier::Vram), 0);
        assert_eq!(cache.used(Tier::Ram), 10);
        assert_eq!(cache.lookup(&digest('a', 64)).unwrap().tier, Tier::Ram);
    }

    #[test]
    fn insert_full_rejected() {
        let mut cache = ModelCache::new(caps(10, 100, 100));
        match cache.put(digest('a', 64), Tier::Vram, 20, false) {
            Err(CacheError::TierFull {
                tier,
                required,
                available,
            }) => {
                assert_eq!(tier, Tier::Vram);
                assert_eq!(required, 20);
                assert_eq!(available, 10);
            }
            other => panic!("expected TierFull, got {:?}", other.map(|_| ())),
        }
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn zero_size_rejected() {
        let mut cache = ModelCache::new(caps(100, 100, 100));
        assert!(matches!(
            cache.put(digest('a', 64), Tier::Vram, 0, false),
            Err(CacheError::InvalidAmount(_))
        ));
    }

    #[test]
    fn lru_eviction_order() {
        let mut cache = ModelCache::new(caps(1000, 100, 100));
        for (i, c) in ['a', 'b', 'c'].iter().enumerate() {
            cache
                .put(digest(*c, 64), Tier::Vram, 10 * (i as u64 + 1), false)
                .unwrap();
        }
        // Touch 'b' so LRU order becomes a < c < b.
        cache.touch(&digest('b', 64)).unwrap();
        assert_eq!(cache.evict_lru(Tier::Vram).unwrap().digest, digest('a', 64));
        assert_eq!(cache.evict_lru(Tier::Vram).unwrap().digest, digest('c', 64));
        assert_eq!(cache.evict_lru(Tier::Vram).unwrap().digest, digest('b', 64));
    }

    #[test]
    fn pinned_not_evicted() {
        let mut cache = ModelCache::new(caps(1000, 100, 100));
        cache.put(digest('a', 64), Tier::Vram, 10, true).unwrap();
        cache.put(digest('b', 64), Tier::Vram, 10, false).unwrap();
        let evicted = cache.evict_lru(Tier::Vram).unwrap();
        assert_eq!(evicted.digest, digest('b', 64));
        assert!(cache.pinned(&digest('a', 64)));
    }

    #[test]
    fn all_pinned_no_eviction() {
        let mut cache = ModelCache::new(caps(1000, 100, 100));
        cache.put(digest('a', 64), Tier::Vram, 10, true).unwrap();
        assert!(matches!(
            cache.evict_lru(Tier::Vram),
            Err(CacheError::TierFull { .. })
        ));
    }

    #[test]
    fn evict_lru_to_fit_frees_exactly_enough() {
        let mut cache = ModelCache::new(caps(100, 100, 100));
        cache.put(digest('a', 64), Tier::Vram, 30, false).unwrap();
        cache.put(digest('b', 64), Tier::Vram, 40, false).unwrap();
        cache.put(digest('c', 64), Tier::Vram, 20, false).unwrap();
        // Used: 90. Need to fit 50 → must free 40 (evicts 'a' 30, then 'b' 40).
        cache.evict_lru_to_fit(Tier::Vram, 50).unwrap();
        assert_eq!(cache.used(Tier::Vram), 20);
        assert_eq!(cache.remaining(Tier::Vram), 80);
        assert!(cache.lookup(&digest('a', 64)).is_none());
        assert!(cache.lookup(&digest('b', 64)).is_none());
        assert!(cache.lookup(&digest('c', 64)).is_some());
    }

    #[test]
    fn evict_lru_to_fit_impossible() {
        let mut cache = ModelCache::new(caps(100, 100, 100));
        cache.put(digest('b', 64), Tier::Vram, 10, true).unwrap();
        let err = cache.evict_lru_to_fit(Tier::Vram, 200).unwrap_err();
        assert!(matches!(
            err,
            CacheError::TierFull {
                tier: Tier::Vram,
                required: 200,
                available: 10
            }
        ));
    }

    #[test]
    fn remaining_and_used_accounting_after_eviction() {
        let mut cache = ModelCache::new(caps(100, 100, 100));
        cache.put(digest('a', 64), Tier::Vram, 60, false).unwrap();
        assert_eq!(cache.used(Tier::Vram), 60);
        assert_eq!(cache.remaining(Tier::Vram), 40);
        cache.evict_lru(Tier::Vram).unwrap();
        assert_eq!(cache.used(Tier::Vram), 0);
        assert_eq!(cache.remaining(Tier::Vram), 100);
    }

    #[test]
    fn pinned_and_unpin() {
        let mut cache = ModelCache::new(caps(100, 100, 100));
        cache.put(digest('a', 64), Tier::Vram, 10, false).unwrap();
        cache.pin(&digest('a', 64)).unwrap();
        assert!(cache.pinned(&digest('a', 64)));
        cache.unpin(&digest('a', 64)).unwrap();
        assert!(!cache.pinned(&digest('a', 64)));
        assert!(matches!(
            cache.touch(&digest('z', 64)),
            Err(CacheError::EntryNotFound(_))
        ));
    }

    #[tokio::test]
    async fn save_load_roundtrip() {
        let store = MemoryStore::new();
        let mut cache = ModelCache::new(caps(100, 100, 100));
        cache.put(digest('a', 64), Tier::Vram, 30, false).unwrap();
        cache.put(digest('b', 64), Tier::Ram, 40, true).unwrap();
        cache.touch(&digest('a', 64)).unwrap();

        cache.save(&store).await.unwrap();

        let mut loaded = ModelCache::load(&store).await.unwrap().unwrap();
        assert_eq!(loaded.entry_count(), 2);
        assert_eq!(loaded.used(Tier::Vram), 30);
        assert_eq!(loaded.used(Tier::Ram), 40);
        assert!(loaded.pinned(&digest('b', 64)));
        assert_eq!(loaded.lookup(&digest('a', 64)).unwrap().tier, Tier::Vram);

        // Counter restored: new puts stamp higher than the restored one.
        loaded.put(digest('c', 64), Tier::Vram, 10, false).unwrap();
        let max_old = cache
            .snapshot()
            .entries
            .iter()
            .map(|e| e.last_access)
            .max()
            .unwrap();
        let new_stamp = loaded.lookup(&digest('c', 64)).unwrap().last_access;
        assert!(new_stamp > max_old);
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let store = MemoryStore::new();
        assert!(ModelCache::load(&store).await.unwrap().is_none());
    }

    #[test]
    fn exported_tier_uses_hierarchy() {
        // Confirm the openrt-hierarchy tier type is the cache's tier type.
        let t: Tier = "nvme".parse().unwrap();
        assert_eq!(t, T::Nvme);
        assert_eq!(Tier::Vram, T::Vram);
    }
}
