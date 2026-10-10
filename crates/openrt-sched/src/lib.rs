//! GPU memory scheduler: first-fit allocation across discovered devices,
//! oversubscription denial and utilization-cap enforcement.
//!
//! [`GpuScheduler`] hands out memory in MiB from a fixed set of
//! [`GpuDevice`]s. Each device contributes at most `memory_mb * utilization_cap`
//! (floored to an integer) so driver/context overhead keeps headroom; the
//! scheduler never overcommits beyond that cap. Devices whose capacity is
//! unknown (`memory_mb` is `None`) are skipped and contribute zero capacity.
//!
//! State can be persisted through any [`PersistenceStore`] under [`SCHED_NS`]
//! with [`save`] / [`load`]. Loading restores allocations and device snapshots
//! but does not probe hardware; device kinds are reconstructed as
//! [`GpuKind::Other`].

use openrt_core::now;
use openrt_persistence::PersistenceStore;
use openrt_resources::gpu::{GpuDevice, GpuKind};
use serde::{Deserialize, Serialize};

/// Store namespace used for GPU-scheduler state.
pub const SCHED_NS: &str = "sched.gpu";

/// Key under which [`GpuSchedulerState`] is persisted in [`SCHED_NS`].
const STATE_KEY: &str = "state";

/// Errors from the GPU scheduler.
#[derive(Debug, thiserror::Error)]
pub enum SchedError {
    #[error("insufficient GPU memory: required {required_mb} MiB but {available_mb} MiB free")]
    InsufficientMemory { required_mb: u64, available_mb: u64 },
    #[error("gpu device {0} not found")]
    DeviceNotFound(usize),
    #[error("unknown allocation `{0}`")]
    UnknownAllocation(String),
    #[error("invalid allocation `{0}`")]
    InvalidAllocation(String),
    #[error("internal scheduler error: {0}")]
    Internal(String),
}

/// One handed-out slice of a device's memory budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocation {
    pub id: String,
    /// Index into the scheduler's `devices` slice.
    pub device_index: usize,
    pub allocated_mb: u64,
    pub owner: String,
    pub created_at: String,
}

/// First-fit GPU memory allocator.
///
/// `devices` are addressed by their index in the slice; the index is stable
/// for the lifetime of the scheduler and is what allocations reference.
#[derive(Debug, Clone)]
pub struct GpuScheduler {
    devices: Vec<GpuDevice>,
    allocations: Vec<Allocation>,
    /// Fraction of each device's memory available for allocations. Default
    /// 0.95 leaves headroom for driver/context memory.
    utilization_cap: f64,
}

impl GpuScheduler {
    /// Build a scheduler over `devices` with a default utilization cap of
    /// 0.95 — headroom for driver/context memory; allocations never overcommit
    /// beyond it.
    pub fn new(devices: Vec<GpuDevice>) -> Self {
        Self {
            devices,
            allocations: Vec::new(),
            utilization_cap: 0.95,
        }
    }

    /// Set the utilization cap, clamped into the open interval `(0.0, 1.0]`.
    ///
    /// Values outside the range are clamped to the nearest valid bound; `NaN`
    /// is treated as `1.0` so the scheduler invariant "cap in (0.0, 1.0]"
    /// always holds.
    pub fn set_utilization_cap(&mut self, frac: f64) {
        let frac = if frac.is_nan() { 1.0 } else { frac };
        self.utilization_cap = frac.clamp(f64::EPSILON, 1.0);
    }

    /// Capacity of `dev` in MiB after applying the utilization cap, floored
    /// so capacity arithmetic stays integral and deterministic.
    fn capacity_mb(dev: &GpuDevice, cap: f64) -> u64 {
        (dev.memory_mb.unwrap_or(0) as f64 * cap).floor() as u64
    }

    /// Allocate `required_mb` on the first device (index order) with enough
    /// remaining budget. Devices without known capacity are skipped.
    pub fn allocate(&mut self, required_mb: u64, owner: &str) -> Result<Allocation, SchedError> {
        if required_mb == 0 {
            return Err(SchedError::InvalidAllocation(
                "requested allocation must be > 0 MiB".to_string(),
            ));
        }
        let total_free: u64 = self.free_mb().iter().map(|(_, free)| free).sum();
        for (idx, dev) in self.devices.iter().enumerate() {
            let capacity = Self::capacity_mb(dev, self.utilization_cap);
            let used: u64 = self
                .allocations
                .iter()
                .filter(|a| a.device_index == idx)
                .map(|a| a.allocated_mb)
                .sum();
            if required_mb <= capacity.saturating_sub(used) {
                let allocation = Allocation {
                    id: format!("alloc-{}", uuid::Uuid::new_v4()),
                    device_index: idx,
                    allocated_mb: required_mb,
                    owner: owner.to_string(),
                    created_at: now().to_rfc3339(),
                };
                self.allocations.push(allocation.clone());
                return Ok(allocation);
            }
        }
        Err(SchedError::InsufficientMemory {
            required_mb,
            available_mb: total_free,
        })
    }

    /// Release a previously granted allocation by id.
    pub fn release(&mut self, id: &str) -> Result<(), SchedError> {
        let pos = self
            .allocations
            .iter()
            .position(|a| a.id == id)
            .ok_or_else(|| SchedError::UnknownAllocation(id.to_string()))?;
        self.allocations.remove(pos);
        Ok(())
    }

    /// Free (cap-adjusted) MiB remaining on each device, in index order.
    pub fn free_mb(&self) -> Vec<(usize, u64)> {
        self.devices
            .iter()
            .enumerate()
            .map(|(idx, dev)| {
                let used: u64 = self
                    .allocations
                    .iter()
                    .filter(|a| a.device_index == idx)
                    .map(|a| a.allocated_mb)
                    .sum();
                (
                    idx,
                    Self::capacity_mb(dev, self.utilization_cap).saturating_sub(used),
                )
            })
            .collect()
    }

    /// All live allocations.
    pub fn allocations(&self) -> &[Allocation] {
        &self.allocations
    }
}

/// Device captured at save time; hardware is never probed on [`load`]. Device
/// kinds are not carried (delta to the in-memory model), so they are restored
/// as [`GpuKind::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub device_index: usize,
    pub name: String,
    pub memory_mb: Option<u64>,
}

/// Serialized GPU-scheduler state (namespace `sched.gpu`, key `state`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuSchedulerState {
    pub devices: Vec<DeviceSnapshot>,
    pub allocations: Vec<Allocation>,
}

/// Persist scheduler state (devices + allocations) under [`SCHED_NS`]/`state`.
pub async fn save(
    store: &dyn PersistenceStore,
    sched: &GpuScheduler,
) -> Result<(), openrt_persistence::PersistenceError> {
    let state = GpuSchedulerState {
        devices: sched
            .devices
            .iter()
            .enumerate()
            .map(|(idx, dev)| DeviceSnapshot {
                device_index: idx,
                name: dev.name.clone(),
                memory_mb: dev.memory_mb,
            })
            .collect(),
        allocations: sched.allocations.clone(),
    };
    store
        .put_record(SCHED_NS, STATE_KEY, &serde_json::to_value(state)?)
        .await
}

/// Restore a scheduler from [`SCHED_NS`]/`state`. Returns `None` when nothing
/// has been saved yet. Devices come from the snapshot (assigned device index,
/// name and capacity); kinds are reconstructed as [`GpuKind::Other`]. The
/// utilization cap resets to the default 0.95.
pub async fn load(store: &dyn PersistenceStore) -> Result<Option<GpuScheduler>, SchedError> {
    let Some(value) = store
        .get_record(SCHED_NS, STATE_KEY)
        .await
        .map_err(|e| SchedError::Internal(e.to_string()))?
    else {
        return Ok(None);
    };
    let state: GpuSchedulerState =
        serde_json::from_value(value).map_err(|e| SchedError::Internal(e.to_string()))?;
    let mut devices: Vec<GpuDevice> = Vec::with_capacity(state.devices.len());
    for snap in state.devices {
        devices.push(GpuDevice {
            name: snap.name,
            kind: GpuKind::Other,
            memory_mb: snap.memory_mb,
            used_mib: None,
        });
    }
    Ok(Some(GpuScheduler {
        devices,
        allocations: state.allocations,
        utilization_cap: 0.95,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn device(name: &str, memory_mb: Option<u64>) -> GpuDevice {
        GpuDevice {
            name: name.to_string(),
            kind: GpuKind::Other,
            memory_mb,
            used_mib: None,
        }
    }

    #[test]
    fn first_fit_two_devices() {
        let mut sched = GpuScheduler::new(vec![
            device("dev0", Some(24_000)),
            device("dev1", Some(20_000)),
        ]);
        sched.set_utilization_cap(1.0);

        let a0 = sched.allocate(8_000, "job-a").unwrap();
        assert_eq!(a0.device_index, 0);
        let a1 = sched.allocate(8_000, "job-b").unwrap();
        assert_eq!(a1.device_index, 0);
        let a2 = sched.allocate(10_000, "job-c").unwrap();
        assert_eq!(a2.device_index, 1);
        assert_eq!(sched.free_mb(), vec![(0, 8_000), (1, 10_000)]);

        sched.release(&a0.id).unwrap();
        let a3 = sched.allocate(8_000, "job-a").unwrap();
        assert_eq!(a3.device_index, 0);
    }

    #[test]
    fn oversubscription_denied() {
        let mut sched = GpuScheduler::new(vec![device("only", Some(8_000))]);
        sched.set_utilization_cap(1.0);

        sched.allocate(6_000, "job-a").unwrap();
        match sched.allocate(3_000, "job-b") {
            Err(SchedError::InsufficientMemory {
                required_mb,
                available_mb,
            }) => {
                assert_eq!(required_mb, 3_000);
                assert_eq!(available_mb, 2_000);
            }
            other => panic!("expected InsufficientMemory, got {other:?}"),
        }
    }

    #[test]
    fn release_unknown() {
        let mut sched = GpuScheduler::new(vec![device("only", Some(8_000))]);
        match sched.release("alloc-does-not-exist") {
            Err(SchedError::UnknownAllocation(id)) => {
                assert_eq!(id, "alloc-does-not-exist")
            }
            other => panic!("expected UnknownAllocation, got {other:?}"),
        }
    }

    #[test]
    fn device_without_memory_not_used() {
        let mut sched = GpuScheduler::new(vec![device("unknown-capacity", None)]);
        sched.set_utilization_cap(1.0);

        assert_eq!(sched.free_mb(), vec![(0, 0)]);
        match sched.allocate(4_000, "job-a") {
            Err(SchedError::InsufficientMemory {
                required_mb,
                available_mb,
            }) => {
                assert_eq!(required_mb, 4_000);
                assert_eq!(available_mb, 0);
            }
            other => panic!("expected InsufficientMemory, got {other:?}"),
        }
        assert!(sched.allocations().is_empty());
    }

    #[test]
    fn utilization_cap_respected() {
        let mut sched = GpuScheduler::new(vec![device("dev0", Some(10_000))]);
        sched.set_utilization_cap(0.5);

        sched.allocate(5_000, "job-a").unwrap();
        match sched.allocate(5_001, "job-b") {
            Err(SchedError::InsufficientMemory { available_mb, .. }) => {
                assert_eq!(available_mb, 0);
            }
            other => panic!("expected InsufficientMemory, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn save_load_roundtrip() {
        let store = MemoryStore::new();
        // Default cap 0.95 both before save and after load, so free_mb is
        // the same on both sides of the round trip.
        let mut sched = GpuScheduler::new(vec![
            device("dev0", Some(20_000)),
            device("dev1", Some(8_000)),
        ]);
        sched.allocate(8_000, "job-a").unwrap();
        sched.allocate(10_000, "job-b").unwrap();
        let expected = sched.allocations().to_vec();
        let expected_free = sched.free_mb();

        save(&store, &sched).await.unwrap();
        drop(sched);

        let restored = load(&store).await.unwrap().expect("state was saved");
        assert_eq!(restored.allocations(), expected.as_slice());
        assert_eq!(restored.free_mb(), expected_free);
        assert_eq!(restored.free_mb(), vec![(0, 1_000), (1, 7_600)]);
        assert_eq!(restored.allocations().len(), 2);
    }

    #[tokio::test]
    async fn load_absent_is_none() {
        let store = MemoryStore::new();
        assert!(load(&store).await.unwrap().is_none());
    }
}
