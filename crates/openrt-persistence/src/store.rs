//! The store contract and Phase 1 backends.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use openrt_core::{
    DomainEvent, ExecutionId, ExecutionRecord, Session, SessionId, Task, TaskId, Timestamp,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::PersistenceError;

/// Options for listing persisted events.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListOptions {
    pub limit: Option<usize>,
    pub since: Option<Timestamp>,
}

/// The single contract consumers use to persist and query runtime state.
///
/// Sessions, tasks, executions and events are the only Phase 1 namespaces.
/// Later phases append namespaces (never parallel stores) per ADR-006.
#[async_trait]
pub trait PersistenceStore: Send + Sync {
    // ---- sessions ----
    async fn create_session(&self, session: &Session) -> Result<(), PersistenceError>;
    async fn update_session(&self, session: &Session) -> Result<(), PersistenceError>;
    async fn load_session(&self, id: SessionId) -> Result<Session, PersistenceError>;
    async fn list_sessions(&self) -> Result<Vec<Session>, PersistenceError>;
    /// Operator-initiated delete of ONE session document. Backends that
    /// cannot express a delete answer [`PersistenceError::Unsupported`];
    /// `MemoryStore` and `LocalFileStore` implement it for real (no default
    /// no-op that would pretend the session is gone).
    async fn delete_session(&self, id: SessionId) -> Result<(), PersistenceError> {
        let _ = id;
        Err(PersistenceError::Unsupported("delete_session".into()))
    }

    // ---- tasks ----
    async fn save_task(&self, task: &Task) -> Result<(), PersistenceError>;
    async fn load_task(&self, id: TaskId) -> Result<Task, PersistenceError>;
    async fn list_tasks(
        &self,
        session_id: Option<SessionId>,
    ) -> Result<Vec<Task>, PersistenceError>;

    // ---- executions ----
    async fn save_execution(&self, record: &ExecutionRecord) -> Result<(), PersistenceError>;
    async fn load_execution(&self, id: ExecutionId) -> Result<ExecutionRecord, PersistenceError>;

    // ---- events ----
    /// Append one event to the append-only event log.
    async fn append_event(&self, event: &DomainEvent) -> Result<(), PersistenceError>;
    async fn list_events(&self, opts: ListOptions) -> Result<Vec<DomainEvent>, PersistenceError>;

    // ---- generic namespaced records (ADR-006: all phases share ONE store) ----
    /// Store a JSON value under `namespace`/`key`. Namespaces are the P2+
    /// execution namespaces (`dag`, `blackboard`, `runs`, ...); keys must be
    /// path-safe (`a-z0-9._-`).
    async fn put_record(
        &self,
        namespace: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), PersistenceError>;
    async fn get_record(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, PersistenceError>;
    async fn list_records(
        &self,
        namespace: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, PersistenceError>;
    async fn delete_record(&self, namespace: &str, key: &str) -> Result<(), PersistenceError>;

    // ---- binary blobs (Phase 3+: content-addressed model artifacts, ADR-010) ----
    //
    // Weight blobs live in the ONE store (ADR-006), under their own namespace
    // (`registry.blobs`), atomically written and content-addressed by sha256.
    // The default implementations reject binaries so the existing test-only
    // fake stores keep compiling unchanged; the in-memory and file backends
    // override them.
    async fn put_blob(
        &self,
        namespace: &str,
        key: &str,
        bytes: &[u8],
    ) -> Result<(), PersistenceError> {
        let _ = (namespace, key, bytes);
        Err(PersistenceError::Unsupported("blob storage".into()))
    }

    async fn get_blob(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, PersistenceError> {
        let _ = (namespace, key);
        Err(PersistenceError::Unsupported("blob storage".into()))
    }

    async fn has_blob(&self, namespace: &str, key: &str) -> Result<bool, PersistenceError> {
        let _ = (namespace, key);
        Err(PersistenceError::Unsupported("blob storage".into()))
    }

    /// Wipe everything (used by tests and `openrt doctor`).
    async fn clear(&self) -> Result<(), PersistenceError>;

    /// Exclusive read-modify-write section spanning several store calls
    /// (e.g. the audit chain append: tail-read → record-write → tail-write,
    /// or a `seq = len + 1` allocator). While the returned guard lives,
    /// other holders of the same store's section are excluded across tasks
    /// and processes; ordinary per-operation store calls are NOT blocked by
    /// the holder (they stay internally consistent through their own locking)
    /// and may freely run inside the section.
    ///
    /// Default: a no-op guard, which is correct for single-owner test doubles
    /// and stores that are never shared. Real backends override it.
    async fn lock_exclusive(&self) -> Result<Box<dyn ExclusiveLock>, PersistenceError> {
        struct Noop;
        impl ExclusiveLock for Noop {}
        Ok(Box::new(Noop))
    }
}

/// Guard returned by [`PersistenceStore::lock_exclusive`]: holds the store's
/// exclusive section until dropped.
pub trait ExclusiveLock: Send + Sync {}

/// In-memory store for tests and ephemeral runs.
#[derive(Debug, Default)]
pub struct MemoryStore {
    sessions: Mutex<std::collections::HashMap<SessionId, Session>>,
    tasks: Mutex<std::collections::HashMap<TaskId, Task>>,
    executions: Mutex<std::collections::HashMap<ExecutionId, ExecutionRecord>>,
    events: Mutex<Vec<DomainEvent>>,
    records: Mutex<
        std::collections::HashMap<String, std::collections::HashMap<String, serde_json::Value>>,
    >,
    blobs: Mutex<std::collections::HashMap<String, std::collections::HashMap<String, Vec<u8>>>>,
    /// In-process section for `lock_exclusive` (multi-call RMW protocols
    /// such as the audit chain append). Independent `MemoryStore`s are
    /// independent worlds and need no cross-instance lock.
    section: std::sync::Arc<tokio::sync::Mutex<()>>,
}

struct MemoryExclusiveGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}
impl ExclusiveLock for MemoryExclusiveGuard {}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PersistenceStore for MemoryStore {
    async fn create_session(&self, session: &Session) -> Result<(), PersistenceError> {
        self.sessions
            .lock()
            .unwrap()
            .insert(session.id, session.clone());
        Ok(())
    }

    async fn update_session(&self, session: &Session) -> Result<(), PersistenceError> {
        self.create_session(session).await
    }

    async fn load_session(&self, id: SessionId) -> Result<Session, PersistenceError> {
        self.sessions
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(PersistenceError::SessionNotFound(id))
    }

    async fn list_sessions(&self) -> Result<Vec<Session>, PersistenceError> {
        let m = self.sessions.lock().unwrap();
        let mut v: Vec<_> = m.values().cloned().collect();
        v.sort_by_key(|s| s.created_at);
        Ok(v)
    }

    async fn delete_session(&self, id: SessionId) -> Result<(), PersistenceError> {
        let removed = self.sessions.lock().unwrap().remove(&id);
        if removed.is_none() {
            return Err(PersistenceError::SessionNotFound(id));
        }
        Ok(())
    }

    async fn save_task(&self, task: &Task) -> Result<(), PersistenceError> {
        self.tasks.lock().unwrap().insert(task.id, task.clone());
        Ok(())
    }

    async fn load_task(&self, id: TaskId) -> Result<Task, PersistenceError> {
        self.tasks
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(PersistenceError::TaskNotFound(id))
    }

    async fn list_tasks(
        &self,
        session_id: Option<SessionId>,
    ) -> Result<Vec<Task>, PersistenceError> {
        let m = self.tasks.lock().unwrap();
        let mut v: Vec<_> = m
            .values()
            .filter(|t| session_id.map_or(true, |sid| t.session_id == sid))
            .cloned()
            .collect();
        v.sort_by_key(|t| t.created_at);
        Ok(v)
    }

    async fn save_execution(&self, record: &ExecutionRecord) -> Result<(), PersistenceError> {
        self.executions
            .lock()
            .unwrap()
            .insert(record.execution_id, record.clone());
        Ok(())
    }

    async fn load_execution(&self, id: ExecutionId) -> Result<ExecutionRecord, PersistenceError> {
        self.executions
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(PersistenceError::ExecutionNotFound(id))
    }

    async fn append_event(&self, event: &DomainEvent) -> Result<(), PersistenceError> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    async fn list_events(&self, opts: ListOptions) -> Result<Vec<DomainEvent>, PersistenceError> {
        let mut v: Vec<_> = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| opts.since.map_or(true, |s| e.at >= s))
            .cloned()
            .collect();
        v.sort_by_key(|e| e.at);
        if let Some(limit) = opts.limit {
            v.truncate(limit);
        }
        Ok(v)
    }

    async fn lock_exclusive(&self) -> Result<Box<dyn ExclusiveLock>, PersistenceError> {
        let guard = self.section.clone().lock_owned().await;
        Ok(Box::new(MemoryExclusiveGuard { _guard: guard }))
    }

    async fn clear(&self) -> Result<(), PersistenceError> {
        self.sessions.lock().unwrap().clear();
        self.tasks.lock().unwrap().clear();
        self.executions.lock().unwrap().clear();
        self.events.lock().unwrap().clear();
        self.records.lock().unwrap().clear();
        self.blobs.lock().unwrap().clear();
        Ok(())
    }

    async fn put_record(
        &self,
        namespace: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), PersistenceError> {
        validate_ns_key(namespace, key)?;
        self.records
            .lock()
            .unwrap()
            .entry(namespace.to_string())
            .or_default()
            .insert(key.to_string(), value.clone());
        Ok(())
    }

    async fn get_record(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, PersistenceError> {
        validate_ns_key(namespace, key)?;
        let map = self.records.lock().unwrap();
        Ok(map.get(namespace).and_then(|m| m.get(key)).cloned())
    }

    async fn list_records(
        &self,
        namespace: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, PersistenceError> {
        validate_ns_key(namespace, "k")?;
        let map = self.records.lock().unwrap();
        let mut out: Vec<_> = map
            .get(namespace)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn delete_record(&self, namespace: &str, key: &str) -> Result<(), PersistenceError> {
        validate_ns_key(namespace, key)?;
        if let Some(map) = self.records.lock().unwrap().get_mut(namespace) {
            map.remove(key);
        }
        Ok(())
    }

    async fn put_blob(
        &self,
        namespace: &str,
        key: &str,
        bytes: &[u8],
    ) -> Result<(), PersistenceError> {
        validate_ns_key(namespace, key)?;
        self.blobs
            .lock()
            .unwrap()
            .entry(namespace.to_string())
            .or_default()
            .insert(key.to_string(), bytes.to_vec());
        Ok(())
    }

    async fn get_blob(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, PersistenceError> {
        validate_ns_key(namespace, key)?;
        let map = self.blobs.lock().unwrap();
        Ok(map.get(namespace).and_then(|m| m.get(key)).cloned())
    }

    async fn has_blob(&self, namespace: &str, key: &str) -> Result<bool, PersistenceError> {
        validate_ns_key(namespace, key)?;
        let map = self.blobs.lock().unwrap();
        Ok(map.get(namespace).is_some_and(|m| m.contains_key(key)))
    }
}

const SCHEMA_VERSION: u32 = 1;

/// Sidecar manifest holding a per-record SHA-256 (SEC-03). Maps a relative
/// store path (e.g. `sessions/<id>.json`) to the hex digest of the record
/// bytes at last write. Detection only covers records written since this file
/// was introduced; tamper-evidence is the ADR-002 audit chain's job.
const CHECKSUMS_FILE: &str = "checksums.json";
/// Crash-consistency marker (FIX-5): written (atomically) before each
/// record mutation inside the store's critical section and removed once the
/// manifest update has landed. If the process dies in between, the next
/// `open()` replays the marker to reconcile the file/manifest pair instead
/// of leaving a permanent fail-closed checksum mismatch on that record.
const PENDING_FILE: &str = "checksums.pending.json";

/// Cross-process advisory lock file (`flock`) guarding record-write + manifest
/// critical sections. Without it two processes on the same store could
/// interleave record writes with whole-manifest rewrites and leave SEC-03
/// checksum mismatches (silent corruption). Held only inside write/delete/
/// init sections — never for a store's lifetime — so concurrent readers and
/// side-by-side surfaces (CLI alongside `webui serve`) keep working.
const STORE_LOCK_FILE: &str = ".store.lock";

type Checksums = std::collections::HashMap<String, String>;

#[derive(Debug, Serialize, Deserialize)]
struct SchemaMarker {
    schema_version: u32,
}

/// Durable JSON-file foundation store.
///
/// Layout under the store root:
///
/// ```text
/// <root>/
///   schema.json          version marker (schema_version = 1)
///   sessions/<id>.json   session documents
///   tasks/<id>.json      task documents
///   executions/<id>.json execution records
///   events/<date>.jsonl  append-only event log, one JSON object per line
/// ```
///
/// Writes are atomic (write to a temp file, then rename). Directories are
/// created `0700` and files `0600` by default (roadmap security baseline).
pub struct LocalFileStore {
    root: PathBuf,
    events_lock: tokio::sync::Mutex<()>,
    /// Serializes manifest read-modify-write cycles (in-process).
    manifest_lock: tokio::sync::Mutex<()>,
    /// In-memory copy of the checksum manifest (SEC-03).
    checksums: std::sync::Mutex<Checksums>,
    /// In-process section mutex for `lock_exclusive`.
    rmw_section: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// >0 while an `lock_exclusive` section on this instance holds the
    /// cross-process flock; per-operation calls then skip their own flock
    /// (the section already excludes every other process for its lifetime).
    section_depth: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl std::fmt::Debug for LocalFileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalFileStore")
            .field("root", &self.root)
            .finish()
    }
}

impl LocalFileStore {
    /// Open (creating if needed) a file store rooted at `dir`.
    ///
    /// Directories are created private from the start (`0o700` on unix, applied
    /// at creation so the process umask cannot widen them), then tightened
    /// again for a pre-existing store that was created with a permissive mode
    /// (SEC-02). Files are created `0o600` at open (see `atomic_write` and
    /// `append_event`).
    pub async fn open(dir: &Path) -> Result<Self, PersistenceError> {
        create_dir_private(dir).await?;
        restrict_dir(dir)?;
        for ns in ["sessions", "tasks", "executions", "events"] {
            let d = dir.join(ns);
            create_dir_private(&d).await?;
            restrict_dir(&d)?;
        }

        // Cross-process: serialize the whole init sequence (schema marker,
        // temp sweep, manifest load) behind the store lock so racing opens
        // cannot observe a half-built store. The guard is released before
        // returning — the lock is never held for the store's lifetime, so
        // side-by-side surfaces (CLI alongside `webui serve`) still work.
        let lock_fd = open_store_lock(dir).await?;
        let init_guard = StoreFlock::acquire(&lock_fd, &dir.join(STORE_LOCK_FILE)).await?;

        let marker_path = dir.join("schema.json");
        if !exists(&marker_path).await {
            let marker = SchemaMarker {
                schema_version: SCHEMA_VERSION,
            };
            atomic_write(&marker_path, &serde_json::to_vec_pretty(&marker)?).await?;
        } else {
            let raw = tokio::fs::read(&marker_path)
                .await
                .map_err(|e| PersistenceError::Io {
                    path: marker_path.clone(),
                    source: e,
                })?;
            let parsed: SchemaMarker = serde_json::from_slice(&raw).map_err(|e| {
                PersistenceError::Corrupt(marker_path.display().to_string(), e.to_string())
            })?;
            if parsed.schema_version != SCHEMA_VERSION {
                return Err(PersistenceError::Corrupt(
                    "schema.json".into(),
                    format!(
                        "unsupported schema version {} (expected {})",
                        parsed.schema_version, SCHEMA_VERSION
                    ),
                ));
            }
        }

        // SEC-12: sweep stale atomic-write temp files left by a crash so they
        // can never be picked up by listings.
        sweep_tmp_files(dir).await;

        let mut checksums = read_manifest_disk(dir).await?;
        // FIX-5: consume a marker left by a crash inside a critical section
        // (still under the init flock, so no other process can be writing).
        if reconcile_pending(dir, &mut checksums).await? {
            persist_manifest(dir, &checksums).await?;
        }
        drop(init_guard);

        Ok(Self {
            root: dir.to_path_buf(),
            events_lock: tokio::sync::Mutex::new(()),
            manifest_lock: tokio::sync::Mutex::new(()),
            checksums: std::sync::Mutex::new(checksums),
            rmw_section: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            section_depth: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    fn path_for(&self, ns: &str, id: &str) -> PathBuf {
        self.root.join(ns).join(format!("{id}.json"))
    }

    async fn write_record<T: Serialize>(
        &self,
        path: &Path,
        val: &T,
    ) -> Result<(), PersistenceError> {
        let bytes = serde_json::to_vec(val)?;
        // SEC-03 + FIX-1: the record write and the manifest read-modify-write
        // form one cross-process critical section (manifest_lock serializes
        // in-process writers, the flock serializes other processes). The
        // manifest is re-read from disk inside the section so entries written
        // by other processes are merged instead of clobbered. Flock is taken
        // FIRST: never wait for it while holding manifest_lock (deadlock).
        let _flock = self.op_flock().await?;
        let _guard = self.manifest_lock.lock().await;
        let key = self.rel_key(path);
        let digest = sha256_hex(&bytes);
        // FIX-5: durable intent before the mutation. A SIGKILL anywhere
        // after this marker is replayed by the next `open()` — record landed
        // (hash matches) → seal it into the manifest; record never landed →
        // consume the marker, nothing to heal.
        write_pending_marker(&self.root, &key, "write", Some(&digest)).await?;
        atomic_write(path, &bytes).await?;
        let mut map = read_manifest_disk(&self.root).await?;
        map.insert(key, digest);
        persist_manifest(&self.root, &map).await?;
        if let Ok(mut mem) = self.checksums.lock() {
            mem.clone_from(&map);
        }
        remove_pending_marker(&self.root).await;
        Ok(())
    }

    async fn read_record<T: for<'de> serde::Deserialize<'de>>(
        &self,
        path: &Path,
    ) -> Result<T, PersistenceError> {
        // Observe (file, manifest) as a consistent pair: exclude in-process
        // writers (manifest_lock) and other processes (flock) for the whole
        // read+verify. Otherwise a read can land between a writer's record
        // write and its manifest update and report a FALSE checksum mismatch
        // (the FIX-1 reader-writer window). Flock first — see write_record.
        let _flock = self.op_flock().await?;
        let _guard = self.manifest_lock.lock().await;
        let raw = tokio::fs::read(path)
            .await
            .map_err(|e| PersistenceError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
        // SEC-03: verify integrity before trusting the bytes. On a mismatch
        // the on-disk manifest is consulted first (FIX-1): another process
        // may have rewritten this record since we cached the manifest, which
        // is a normal handoff — not corruption.
        let key = self.rel_key(path);
        self.verify_record_digest(&key, &raw, "checksum mismatch")
            .await?;
        serde_json::from_slice(&raw)
            .map_err(|e| PersistenceError::Corrupt(path.display().to_string(), e.to_string()))
    }

    /// Write raw bytes (a blob) atomically with a recorded checksum.
    async fn write_record_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), PersistenceError> {
        // Same critical section discipline as `write_record` (FIX-1): record
        // write + disk-merged manifest update under one cross-process lock.
        let _flock = self.op_flock().await?;
        let _guard = self.manifest_lock.lock().await;
        let key = self.rel_key(path);
        let digest = sha256_hex(bytes);
        // FIX-5: same durable-intent protocol as `write_record`.
        write_pending_marker(&self.root, &key, "write", Some(&digest)).await?;
        atomic_write(path, bytes).await?;
        let mut map = read_manifest_disk(&self.root).await?;
        map.insert(key, digest);
        persist_manifest(&self.root, &map).await?;
        if let Ok(mut mem) = self.checksums.lock() {
            mem.clone_from(&map);
        }
        remove_pending_marker(&self.root).await;
        Ok(())
    }

    /// Read raw blob bytes, verifying the recorded checksum first (SEC-03).
    async fn read_blob_bytes(&self, path: &Path) -> Result<Vec<u8>, PersistenceError> {
        // Same consistent-pair discipline as read_record (FIX-1 window).
        let _flock = self.op_flock().await?;
        let _guard = self.manifest_lock.lock().await;
        let raw = tokio::fs::read(path)
            .await
            .map_err(|e| PersistenceError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
        let key = self.rel_key(path);
        self.verify_record_digest(&key, &raw, "blob checksum mismatch")
            .await?;
        Ok(raw)
    }

    /// SEC-03 verification with FIX-1 reconciliation: compare against the
    /// cached manifest first (fast path); on mismatch or miss, re-read the
    /// on-disk manifest before declaring corruption. If disk agrees with the
    /// file content, the cache was stale (writer handoff) — refresh and pass.
    /// If disk disagrees too, the record or manifest was tampered with —
    /// fail closed.
    async fn verify_record_digest(
        &self,
        key: &str,
        raw: &[u8],
        kind: &str,
    ) -> Result<(), PersistenceError> {
        let actual = sha256_hex(raw);
        let cached = self.checksums.lock().ok().and_then(|m| m.get(key).cloned());
        if cached.as_deref() == Some(actual.as_str()) {
            return Ok(());
        }
        let disk = read_manifest_disk(&self.root).await?;
        match disk.get(key) {
            Some(d) if d == &actual => {
                if let Ok(mut mem) = self.checksums.lock() {
                    mem.insert(key.to_string(), actual);
                }
                Ok(())
            }
            Some(d) => Err(PersistenceError::Corrupt(
                key.to_string(),
                format!("{kind} (expected {d}, got {actual})"),
            )),
            None => match cached {
                // Never tracked (legacy record predating the manifest).
                None => Ok(()),
                Some(e) => Err(PersistenceError::Corrupt(
                    key.to_string(),
                    format!("{kind} (expected {e}, got {actual})"),
                )),
            },
        }
    }

    fn rel_key(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned())
    }

    /// Per-operation cross-process flock. Every call opens `.store.lock`
    /// FRESH — a distinct open file description — so concurrent in-flight
    /// operations on this instance each hold their own kernel lock and one
    /// guard's drop can never release the lock while another operation is
    /// still inside its critical section (FIX-4: the shared-fd unlock race
    /// that let a second process interleave a manifest read-modify-write,
    /// observed as a checksum mismatch under the concurrent `sih demo`
    /// battery). Returns `None` while a `lock_exclusive` section on this
    /// instance holds the flock (the section's independent fd already
    /// excludes every other process).
    async fn op_flock(&self) -> Result<Option<OwnedFlock>, PersistenceError> {
        if self
            .section_depth
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
        {
            return Ok(None);
        }
        Ok(Some(
            OwnedFlock::acquire(&self.root.join(STORE_LOCK_FILE)).await?,
        ))
    }

    async fn list_ns(&self, ns: &str) -> Result<Vec<PathBuf>, PersistenceError> {
        let mut out = Vec::new();
        let mut rd =
            tokio::fs::read_dir(self.root.join(ns))
                .await
                .map_err(|e| PersistenceError::Io {
                    path: self.root.join(ns),
                    source: e,
                })?;
        while let Some(entry) = rd.next_entry().await.map_err(|e| PersistenceError::Io {
            path: self.root.join(ns),
            source: e,
        })? {
            out.push(entry.path());
        }
        Ok(out)
    }
}

/// Open (creating if needed) the cross-process lock file under `dir`.
async fn open_store_lock(dir: &Path) -> Result<std::fs::File, PersistenceError> {
    let path = dir.join(STORE_LOCK_FILE);
    let fd = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| PersistenceError::Io {
            path: path.clone(),
            source,
        })?;
    restrict_file(&path)?;
    Ok(fd)
}

/// Bounded async retry loop acquiring an exclusive flock on `fd`
/// (~2ms sleep x 2000 ≈ 4s ceiling; contended sections last ms). Never
/// parks a runtime thread inside a blocking syscall.
async fn lock_fd_retry(fd: &std::fs::File, path: &Path) -> Result<(), PersistenceError> {
    for _ in 0..2000u32 {
        match fd.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(PersistenceError::Io {
                    path: path.to_path_buf(),
                    source: e,
                });
            }
        }
    }
    Err(PersistenceError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "store is in use by another process (timed out waiting for .store.lock)",
        ),
    })
}

/// RAII advisory lock (flock-backed via `File::try_lock`) on a caller-supplied
/// lock-file fd. Used by `open()` for the one-shot init section (no other
/// operation on that fd exists yet, so a shared fd is safe there). Released on
/// drop so every early-return path still unlocks; a crashed holder is released
/// by the kernel with the fd.
struct StoreFlock<'a>(&'a std::fs::File);

impl<'a> StoreFlock<'a> {
    async fn acquire(fd: &'a std::fs::File, path: &Path) -> Result<Self, PersistenceError> {
        lock_fd_retry(fd, path).await?;
        Ok(Self(fd))
    }
}

impl Drop for StoreFlock<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Owns its fd (a FRESH open of `.store.lock`, a distinct open file
/// description): used by every per-operation critical section (FIX-4 — each
/// operation's flock must be independent so dropping one guard can never
/// release another in-flight operation's lock) and by `lock_exclusive`
/// sections.
struct OwnedFlock {
    fd: std::fs::File,
}

impl OwnedFlock {
    async fn acquire(path: &Path) -> Result<Self, PersistenceError> {
        let fd = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|source| PersistenceError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        lock_fd_retry(&fd, path).await?;
        Ok(Self { fd })
    }
}

impl Drop for OwnedFlock {
    fn drop(&mut self) {
        let _ = self.fd.unlock();
    }
}

/// Guard for `LocalFileStore::lock_exclusive`. Drop order matters: the drop
/// body first decrements `depth` (new per-operation calls then re-take the
/// flock before ours is released — no exclusion gap), then `_flock` unlocks,
/// then `_section` releases the in-process mutex.
struct FileExclusiveGuard {
    depth: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    _flock: OwnedFlock,
    _section: tokio::sync::OwnedMutexGuard<()>,
}

impl ExclusiveLock for FileExclusiveGuard {}

impl Drop for FileExclusiveGuard {
    fn drop(&mut self) {
        self.depth.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Read the SEC-03 manifest straight from disk — the source of truth
/// whenever more than one process may be writing. A missing file is an
/// empty manifest (new store, or post-`clear()`).
async fn read_manifest_disk(root: &Path) -> Result<Checksums, PersistenceError> {
    let path = root.join(CHECKSUMS_FILE);
    if !exists(&path).await {
        return Ok(Checksums::new());
    }
    let raw = tokio::fs::read(&path)
        .await
        .map_err(|e| PersistenceError::Io {
            path: path.clone(),
            source: e,
        })?;
    serde_json::from_slice(&raw)
        .map_err(|e| PersistenceError::Corrupt(CHECKSUMS_FILE.to_string(), e.to_string()))
}

/// Write `map` to the manifest file atomically. Caller must hold
/// `manifest_lock` **and** the cross-process store flock.
async fn persist_manifest(root: &Path, map: &Checksums) -> Result<(), PersistenceError> {
    let bytes = serde_json::to_vec(map)?;
    atomic_write(&root.join(CHECKSUMS_FILE), &bytes).await
}

/// FIX-5: durably record the mutation that is about to run. Written
/// (atomically) BEFORE the record/delete so a crash at any later point in
/// the critical section leaves the next `open()` enough intent to reconcile
/// the file/manifest pair. Caller must hold the store's critical section.
async fn write_pending_marker(
    root: &Path,
    key: &str,
    op: &str,
    digest: Option<&str>,
) -> Result<(), PersistenceError> {
    let val = serde_json::json!({ "key": key, "op": op, "digest": digest });
    atomic_write(&root.join(PENDING_FILE), &serde_json::to_vec(&val)?).await
}

/// FIX-5: drop the marker once its mutation is fully durable (manifest
/// updated). Best-effort — a marker left behind is consumed by the next
/// `open()`, never by a racing reader.
async fn remove_pending_marker(root: &Path) {
    let _ = tokio::fs::remove_file(root.join(PENDING_FILE)).await;
}

/// FIX-5: replay a marker left by a crash inside a critical section.
/// Returns whether the manifest changed (the caller persists it under the
/// init flock). Trust rules:
/// - `write` marker whose digest matches the file on disk → the record
///   rename landed but the manifest update did not → seal it in.
/// - any other state (record never landed, digest differs) → the pair was
///   never made inconsistent by this marker → keep the manifest as-is
///   (genuine tamper still fails closed through the normal verify path).
/// - `delete` marker with the file gone → drop the stale entry.
/// An unparseable marker is left untouched for forensics.
async fn reconcile_pending(root: &Path, map: &mut Checksums) -> Result<bool, PersistenceError> {
    let path = root.join(PENDING_FILE);
    if !exists(&path).await {
        return Ok(false);
    }
    let Ok(raw) = tokio::fs::read(&path).await else {
        return Ok(false);
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return Ok(false);
    };
    let (Some(key), Some(op)) = (
        v.get("key").and_then(|k| k.as_str()),
        v.get("op").and_then(|o| o.as_str()),
    ) else {
        return Ok(false);
    };
    let key = key.to_string();
    let rec_path = root.join(&key);
    let mut changed = false;
    match op {
        "write" => {
            if let Some(digest) = v.get("digest").and_then(|d| d.as_str()) {
                if let Ok(bytes) = tokio::fs::read(&rec_path).await {
                    if sha256_hex(&bytes) == digest
                        && map.get(&key).map(String::as_str) != Some(digest)
                    {
                        map.insert(key, digest.to_string());
                        changed = true;
                    }
                }
            }
        }
        "delete" => {
            if !exists(&rec_path).await && map.remove(&key).is_some() {
                changed = true;
            }
        }
        _ => {}
    }
    // Consume the intent: the pair is now consistent (or the marker never
    // made it inconsistent).
    let _ = tokio::fs::remove_file(&path).await;
    Ok(changed)
}

#[async_trait]
impl PersistenceStore for LocalFileStore {
    async fn create_session(&self, session: &Session) -> Result<(), PersistenceError> {
        let path = self.path_for("sessions", &session.id.to_string());
        self.write_record(&path, session).await
    }

    async fn update_session(&self, session: &Session) -> Result<(), PersistenceError> {
        self.create_session(session).await
    }

    async fn load_session(&self, id: SessionId) -> Result<Session, PersistenceError> {
        let path = self.path_for("sessions", &id.to_string());
        if !exists(&path).await {
            return Err(PersistenceError::SessionNotFound(id));
        }
        self.read_record(&path).await
    }

    async fn list_sessions(&self) -> Result<Vec<Session>, PersistenceError> {
        let mut out = Vec::new();
        for path in self.list_ns("sessions").await? {
            if !path.is_dir() && path.extension().map(|e| e == "json").unwrap_or(false) {
                out.push(self.read_record::<Session>(&path).await?);
            }
        }
        out.sort_by_key(|s| s.created_at);
        Ok(out)
    }

    async fn delete_session(&self, id: SessionId) -> Result<(), PersistenceError> {
        let path = self.path_for("sessions", &id.to_string());
        if !exists(&path).await {
            return Err(PersistenceError::SessionNotFound(id));
        }
        // Same durable path as any record delete: flock → pending marker →
        // unlink → manifest update (checksums stay consistent).
        PersistenceStore::delete_record(self, "sessions", &id.to_string()).await
    }

    async fn save_task(&self, task: &Task) -> Result<(), PersistenceError> {
        let path = self.path_for("tasks", &task.id.to_string());
        self.write_record(&path, task).await
    }

    async fn load_task(&self, id: TaskId) -> Result<Task, PersistenceError> {
        let path = self.path_for("tasks", &id.to_string());
        if !exists(&path).await {
            return Err(PersistenceError::TaskNotFound(id));
        }
        self.read_record(&path).await
    }

    async fn list_tasks(
        &self,
        session_id: Option<SessionId>,
    ) -> Result<Vec<Task>, PersistenceError> {
        let mut out = Vec::new();
        for path in self.list_ns("tasks").await? {
            if !path.is_dir() && path.extension().map(|e| e == "json").unwrap_or(false) {
                let t: Task = self.read_record(&path).await?;
                if session_id.map_or(true, |sid| t.session_id == sid) {
                    out.push(t);
                }
            }
        }
        out.sort_by_key(|t| t.created_at);
        Ok(out)
    }

    async fn save_execution(&self, record: &ExecutionRecord) -> Result<(), PersistenceError> {
        let path = self.path_for("executions", &record.execution_id.to_string());
        self.write_record(&path, record).await
    }

    async fn load_execution(&self, id: ExecutionId) -> Result<ExecutionRecord, PersistenceError> {
        let path = self.path_for("executions", &id.to_string());
        if !exists(&path).await {
            return Err(PersistenceError::ExecutionNotFound(id));
        }
        self.read_record(&path).await
    }

    async fn append_event(&self, event: &DomainEvent) -> Result<(), PersistenceError> {
        let _guard = self.events_lock.lock().await;
        let date = event.at.format("%Y-%m-%d").to_string();
        let path = self.root.join("events").join(format!("{date}.jsonl"));
        // One write() for the whole line (payload + newline): with O_APPEND
        // each write is atomic w.r.t. other writers, so two processes can
        // never interleave half-lines into each other's events.
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        let mut opts = tokio::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            // Create the log file `0o600` at open so the umask cannot widen it.
            opts.mode(0o600);
        }
        let mut file = opts.open(&path).await.map_err(|e| PersistenceError::Io {
            path: path.clone(),
            source: e,
        })?;
        restrict_file(&path)?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| PersistenceError::Io {
                path: path.clone(),
                source: e,
            })?;
        file.flush().await.map_err(|e| PersistenceError::Io {
            path: path.clone(),
            source: e,
        })
    }

    async fn list_events(&self, opts: ListOptions) -> Result<Vec<DomainEvent>, PersistenceError> {
        let mut out = Vec::new();
        for path in self.list_ns("events").await? {
            if path.is_dir() || !path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                continue;
            }
            let raw = tokio::fs::read(&path)
                .await
                .map_err(|e| PersistenceError::Io {
                    path: path.clone(),
                    source: e,
                })?;
            for line in raw.split(|b| *b == b'\n') {
                if line.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                // SEC-12: never silently skip a corrupt line — surface it as a
                // structured error so divergence cannot hide.
                let ev = serde_json::from_slice::<DomainEvent>(line).map_err(|e| {
                    PersistenceError::Corrupt(
                        path.display().to_string(),
                        format!("unparsable event line ({} bytes): {e}", line.len()),
                    )
                })?;
                if opts.since.map_or(true, |s| ev.at >= s) {
                    out.push(ev);
                }
            }
        }
        out.sort_by_key(|e| e.at);
        if let Some(limit) = opts.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn clear(&self) -> Result<(), PersistenceError> {
        // Sweep every namespace dir (base + P2 generic namespaces); remove
        // stray files at the root (e.g. checksums.json). schema.json is kept —
        // the store remains valid and re-usable after a clear.
        let mut rd = tokio::fs::read_dir(&self.root)
            .await
            .map_err(|e| PersistenceError::Io {
                path: self.root.clone(),
                source: e,
            })?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                let mut inner =
                    tokio::fs::read_dir(&path)
                        .await
                        .map_err(|e| PersistenceError::Io {
                            path: path.clone(),
                            source: e,
                        })?;
                while let Ok(Some(item)) = inner.next_entry().await {
                    let item = item.path();
                    if item.is_dir() {
                        tokio::fs::remove_dir_all(&item).await.map_err(|e| {
                            PersistenceError::Io {
                                path: item.clone(),
                                source: e,
                            }
                        })?;
                    } else {
                        tokio::fs::remove_file(&item)
                            .await
                            .map_err(|e| PersistenceError::Io {
                                path: item.clone(),
                                source: e,
                            })?;
                    }
                }
            } else if path
                .file_name()
                // schema.json keeps the store valid; .store.lock pins
                // cross-process exclusion between opens (FIX-1).
                .map(|n| n != "schema.json" && n != STORE_LOCK_FILE)
                .unwrap_or(true)
            {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| PersistenceError::Io {
                        path: path.clone(),
                        source: e,
                    })?;
            }
        }
        if let Ok(mut map) = self.checksums.lock() {
            map.clear();
        }
        Ok(())
    }

    async fn lock_exclusive(&self) -> Result<Box<dyn ExclusiveLock>, PersistenceError> {
        use std::sync::atomic::Ordering;
        let section = self.rmw_section.clone().lock_owned().await;
        // A FRESH open of `.store.lock` (distinct open file description):
        // this flock excludes other processes and other in-process store
        // instances, while this instance's per-operation calls skip their own
        // flock (section_depth > 0) and run under its protection.
        let flock = OwnedFlock::acquire(&self.root.join(STORE_LOCK_FILE)).await?;
        self.section_depth.fetch_add(1, Ordering::Release);
        Ok(Box::new(FileExclusiveGuard {
            depth: self.section_depth.clone(),
            _flock: flock,
            _section: section,
        }))
    }

    async fn put_record(
        &self,
        namespace: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), PersistenceError> {
        validate_ns_key(namespace, key)?;
        let dir = self.root.join(namespace);
        create_dir_private(&dir).await?;
        restrict_dir(&dir)?;
        let path = dir.join(format!("{key}.json"));
        self.write_record(&path, value).await
    }

    async fn get_record(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, PersistenceError> {
        validate_ns_key(namespace, key)?;
        let path = self.root.join(namespace).join(format!("{key}.json"));
        if !exists(&path).await {
            return Ok(None);
        }
        Ok(Some(self.read_record::<serde_json::Value>(&path).await?))
    }

    async fn list_records(
        &self,
        namespace: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, PersistenceError> {
        validate_ns_key(namespace, "k")?;
        let mut out = Vec::new();
        let dir = self.root.join(namespace);
        // An untouched namespace may have no directory yet: that is an empty
        // namespace, not an error (unlike the base namespaces created at open).
        if !exists(&dir).await {
            return Ok(out);
        }
        for path in self.list_ns(namespace).await? {
            if path.is_dir() || !path.extension().map(|e| e == "json").unwrap_or(false) {
                continue;
            }
            let key = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let value = self.read_record::<serde_json::Value>(&path).await?;
            out.push((key, value));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn delete_record(&self, namespace: &str, key: &str) -> Result<(), PersistenceError> {
        validate_ns_key(namespace, key)?;
        let path = self.root.join(namespace).join(format!("{key}.json"));
        // Critical section per FIX-1: unlink + disk-merged manifest update
        // atomically w.r.t. other processes' writes (flock first, then the
        // in-process manifest mutex — see write_record).
        let _flock = self.op_flock().await?;
        let _guard = self.manifest_lock.lock().await;
        let rel = self.rel_key(&path);
        // FIX-5: durable intent for the unlink (replayed at the next open:
        // file gone → drop the stale manifest entry).
        write_pending_marker(&self.root, &rel, "delete", None).await?;
        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| PersistenceError::Io {
                path: path.clone(),
                source: e,
            })?;
        let mut map = read_manifest_disk(&self.root).await?;
        map.remove(&rel);
        persist_manifest(&self.root, &map).await?;
        if let Ok(mut mem) = self.checksums.lock() {
            mem.remove(&rel);
        }
        remove_pending_marker(&self.root).await;
        Ok(())
    }

    async fn put_blob(
        &self,
        namespace: &str,
        key: &str,
        bytes: &[u8],
    ) -> Result<(), PersistenceError> {
        validate_ns_key(namespace, key)?;
        let dir = self.root.join(namespace);
        create_dir_private(&dir).await?;
        restrict_dir(&dir)?;
        let path = dir.join(format!("{key}.bin"));
        self.write_record_bytes(&path, bytes).await
    }

    async fn get_blob(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, PersistenceError> {
        validate_ns_key(namespace, key)?;
        let path = self.root.join(namespace).join(format!("{key}.bin"));
        if !exists(&path).await {
            return Ok(None);
        }
        Ok(Some(self.read_blob_bytes(&path).await?))
    }

    async fn has_blob(&self, namespace: &str, key: &str) -> Result<bool, PersistenceError> {
        validate_ns_key(namespace, key)?;
        Ok(exists(&self.root.join(namespace).join(format!("{key}.bin"))).await)
    }
}

/// Obtain the default store root directory.
pub fn default_store_dir() -> PathBuf {
    home_dir()
        .map(|h| h.join(".openrt"))
        .unwrap_or_else(|| PathBuf::from(".openrt"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Validate a generic-record (namespace, key) pair: both must be non-empty
/// path-safe identifiers (`a-z0-9._-`), so a caller can never escape the
/// namespace directory or smuggle separators (ADR-006 single store).
fn validate_ns_key(namespace: &str, key: &str) -> Result<(), PersistenceError> {
    for (what, value) in [("namespace", namespace), ("key", key)] {
        let valid = !value.is_empty()
            && value.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            });
        if !valid {
            return Err(PersistenceError::Corrupt(
                "record".into(),
                format!("invalid {what} `{value}` (must be a-z0-9._-)"),
            ));
        }
    }
    Ok(())
}

/// Write bytes atomically: temp file + rename (same filesystem).
async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PersistenceError> {
    let tmp = path.with_extension(format!("{}.tmp", unique_suffix()));
    {
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            // Create the temp file `0o600` at open so the rename target is
            // never wider than the intended mode (SEC-02).
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).await.map_err(|e| PersistenceError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.write_all(bytes).await.map_err(|e| PersistenceError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.flush().await.map_err(|e| PersistenceError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        restrict_file(&tmp)?;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| PersistenceError::Io {
            path: path.to_path_buf(),
            source: e,
        })
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{ts:x}-{:x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// SHA-256 hex digest of a byte slice (SEC-03 record checksums).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Best-effort removal of stale atomic-write temp files (`*.tmp`) left by a
/// crash. Runs at open so listings can never be wedged by a poisoned temp
/// (SEC-12). Failures to sweep are ignored — they are re-surfaced, if relevant,
/// by the extension-filtered listings.
async fn sweep_tmp_files(root: &Path) {
    let mut dirs = vec![root.to_path_buf()];
    for ns in ["sessions", "tasks", "executions", "events"] {
        dirs.push(root.join(ns));
    }
    for dir in dirs {
        if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(".tmp") {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
    }
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<(), PersistenceError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
        PersistenceError::Io {
            path: path.to_path_buf(),
            source: e,
        }
    })
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<(), PersistenceError> {
    Ok(())
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> Result<(), PersistenceError> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            PersistenceError::Io {
                path: path.to_path_buf(),
                source: e,
            }
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> Result<(), PersistenceError> {
    Ok(())
}

/// Recursively create a directory with restrictive permissions from the start
/// (mode `0o700` on unix, applied at creation so the process umask cannot
/// widen it). Non-unix falls back to the ordinary recursive create. Succeeds
/// when the directory (or its parents) already exist.
async fn create_dir_private(dir: &Path) -> Result<(), PersistenceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
        {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(PersistenceError::Io {
                path: dir.to_path_buf(),
                source: e,
            }),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| PersistenceError::Io {
                path: dir.to_path_buf(),
                source: e,
            })
    }
}

/// Async existence check for a path.
async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::{DomainEvent, EventKind, ExecutionRecord, Message, Session, Task};
    use std::sync::Arc;

    fn unique_dir() -> PathBuf {
        std::env::temp_dir().join(format!("openrt-persist-test-{}", unique_suffix()))
    }

    /// P2.3: generic namespaced records round-trip in both backends, are
    /// integrity-checked by the SEC-03 manifest on disk, and reject unsafe
    /// (namespace, key) identifiers.
    #[tokio::test]
    async fn namespaced_records_round_trip() {
        for store in backend_list().await {
            store
                .put_record("blackboard", "facts.x", &serde_json::json!({ "a": 1 }))
                .await
                .unwrap();
            store
                .put_record("blackboard", "facts.y", &serde_json::json!([1, 2]))
                .await
                .unwrap();
            store
                .put_record("dag", "run-1", &serde_json::json!({ "ok": true }))
                .await
                .unwrap();

            assert_eq!(
                store.get_record("blackboard", "facts.x").await.unwrap(),
                Some(serde_json::json!({ "a": 1 }))
            );
            let all = store.list_records("blackboard").await.unwrap();
            assert_eq!(all.len(), 2);
            assert_eq!(all[0].0, "facts.x");
            assert!(matches!(
                store
                    .put_record("../evil", "x", &serde_json::Value::Null)
                    .await,
                Err(PersistenceError::Corrupt(_, _))
            ));
            assert!(matches!(
                store
                    .put_record("blackboard", "../x", &serde_json::Value::Null)
                    .await,
                Err(PersistenceError::Corrupt(_, _))
            ));

            store.delete_record("blackboard", "facts.x").await.unwrap();
            assert_eq!(
                store.get_record("blackboard", "facts.x").await.unwrap(),
                None
            );
        }
    }

    /// P2.3: on the durable backend, generic records inherit SEC-03 checksum
    /// protection; an edited record fails closed on read.
    #[tokio::test]
    async fn namespaced_records_tamper_detect_on_disk() {
        let dir = unique_dir();
        let store = LocalFileStore::open(&dir).await.unwrap();
        store
            .put_record("blackboard", "facts.y", &serde_json::json!([1, 2]))
            .await
            .unwrap();
        let path = dir.join("blackboard").join("facts.y.json");
        std::fs::write(&path, r#"[9,9]"#).unwrap();
        assert!(matches!(
            store.get_record("blackboard", "facts.y").await,
            Err(PersistenceError::Corrupt(m, _)) if m.contains("facts.y")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P2.3: `clear()` wipes generic namespaces too.
    #[tokio::test]
    async fn clear_wipes_generic_namespaces() {
        for store in backend_list().await {
            store
                .put_record("runs", "r1", &serde_json::json!({}))
                .await
                .unwrap();
            store.clear().await.unwrap();
            assert_eq!(store.list_records("runs").await.unwrap().len(), 0);
        }
    }

    /// Returns memory + (temporary, filesystem) backends. The temp dir is
    /// cleaned by the OS temp semantics on test exit.
    async fn backend_list() -> Vec<Arc<dyn PersistenceStore>> {
        let dir = unique_dir();
        let file = Arc::new(LocalFileStore::open(&dir).await.unwrap());
        let mem = Arc::new(MemoryStore::new());
        vec![mem, file]
    }

    /// P3: binary blobs round-trip in both backends and are not visible as
    /// JSON records (separate namespace/extension from records).
    #[tokio::test]
    async fn blob_round_trip() {
        for store in backend_list().await {
            assert!(!store.has_blob("registry.blobs", "abc").await.unwrap());
            store
                .put_blob("registry.blobs", "abc", b"model-weights-v1")
                .await
                .unwrap();
            assert!(store.has_blob("registry.blobs", "abc").await.unwrap());
            assert_eq!(
                store
                    .get_blob("registry.blobs", "abc")
                    .await
                    .unwrap()
                    .unwrap(),
                b"model-weights-v1"
            );
            assert_eq!(store.get_blob("registry.blobs", "zzz").await.unwrap(), None);
            // blobs are not records: the generic listings must not expose them
            assert!(store
                .list_records("registry.blobs")
                .await
                .unwrap()
                .is_empty());

            // reject unsafe keys like the record API
            assert!(store
                .put_blob("registry.blobs", "../x", b"x")
                .await
                .is_err());
        }
    }

    /// P3: blob tampering is caught by the SEC-03 checksum manifest on the
    /// file backend and fails closed.
    #[tokio::test]
    async fn blob_corruption_fails_closed() {
        let dir = unique_dir();
        let store = Arc::new(LocalFileStore::open(&dir).await.unwrap());
        store
            .put_blob("registry.blobs", "k1", b"trusted")
            .await
            .unwrap();
        let path = dir.join("registry.blobs").join("k1.bin");
        tokio::fs::write(&path, b"tampered").await.unwrap();
        assert!(matches!(
            store.get_blob("registry.blobs", "k1").await,
            Err(PersistenceError::Corrupt(_, _))
        ));
    }

    async fn round_trip(store: &dyn PersistenceStore) {
        let mut session = Session::new("hello");
        session.push(Message::user("hi"));
        store.create_session(&session).await.unwrap();
        let loaded = store.load_session(session.id).await.unwrap();
        assert_eq!(loaded, session);

        let mut task = Task::new(session.id, "run probe");
        task.transition(openrt_core::TaskStatus::Running).unwrap();
        store.save_task(&task).await.unwrap();
        let t2 = store.load_task(task.id).await.unwrap();
        assert_eq!(t2, task);

        let exec = ExecutionRecord::new(session.id, task.id, None);
        store.save_execution(&exec).await.unwrap();
        let e2 = store.load_execution(exec.execution_id).await.unwrap();
        assert_eq!(e2.execution_id, exec.execution_id);

        store
            .append_event(
                &DomainEvent::new(EventKind::TaskStarted)
                    .with_session(session.id)
                    .with_task(task.id),
            )
            .await
            .unwrap();
        let events = store.list_events(ListOptions::default()).await.unwrap();
        assert_eq!(events.len(), 1);

        assert_eq!(store.list_sessions().await.unwrap().len(), 1);
        assert_eq!(store.list_tasks(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn memory_store_round_trip() {
        let store = MemoryStore::new();
        round_trip(&store).await;
    }

    #[tokio::test]
    async fn file_store_round_trip() {
        let dir = unique_dir();
        let store = LocalFileStore::open(&dir).await.unwrap();
        round_trip(&store).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_store_persists_across_reopen() {
        let dir = unique_dir();
        {
            let store = LocalFileStore::open(&dir).await.unwrap();
            let session = Session::new("survive");
            store.create_session(&session).await.unwrap();
        }
        {
            let store = LocalFileStore::open(&dir).await.unwrap();
            let sessions = store.list_sessions().await.unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].title, "survive");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn event_log_is_append_only_and_ordered() {
        let dir = unique_dir();
        let store = LocalFileStore::open(&dir).await.unwrap();
        for i in 0..5 {
            store
                .append_event(
                    &DomainEvent::new(EventKind::TaskCompleted)
                        .with_data(serde_json::json!({"i": i})),
                )
                .await
                .unwrap();
        }
        let events = store.list_events(ListOptions::default()).await.unwrap();
        assert_eq!(events.len(), 5);
        for (idx, e) in events.iter().enumerate() {
            assert_eq!(e.data["i"], idx as u64);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SEC-02: fresh directories are `0o700` and record files `0o600`,
    /// regardless of the process umask.
    #[cfg(unix)]
    #[tokio::test]
    async fn fresh_cut_dirs_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_dir();
        // Widen the umask as if the parent creation happened under it.
        let store = LocalFileStore::open(&dir).await.unwrap();

        let root_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(root_mode & 0o777, 0o700, "root mode was {root_mode:o}");

        for ns in ["sessions", "tasks", "executions", "events"] {
            let p = dir.join(ns);
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{ns} mode was {mode:o}");
        }

        let session = Session::new("perms");
        store.create_session(&session).await.unwrap();
        let spath = dir.join("sessions").join(format!("{}.json", session.id));
        let smode = std::fs::metadata(&spath).unwrap().permissions().mode();
        assert_eq!(smode & 0o777, 0o600, "session file mode was {smode:o}");

        store
            .append_event(&DomainEvent::new(EventKind::RuntimeStarted))
            .await
            .unwrap();
        let events = store.list_events(ListOptions::default()).await.unwrap();
        assert_eq!(events.len(), 1);
        let dpath = dir
            .join("events")
            .join(format!("{}.jsonl", events[0].at.format("%Y-%m-%d")));
        let dmode = std::fs::metadata(&dpath).unwrap().permissions().mode();
        assert_eq!(dmode & 0o777, 0o600, "event log mode was {dmode:o}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SEC-02: a pre-existing, permissively-created store is tightened on open.
    #[cfg(unix)]
    #[tokio::test]
    async fn existing_permissive_store_is_tightened_on_open() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_dir();
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(dir.join("sessions"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let _store = LocalFileStore::open(&dir).await.unwrap();

        for p in [dir.clone(), dir.join("sessions"), dir.join("tasks")] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{p:?} mode was {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SEC-03: a semantically-valid but edited record fails closed on read.
    #[tokio::test]
    async fn record_checksum_detects_tampering() {
        let dir = unique_dir();
        let store = LocalFileStore::open(&dir).await.unwrap();

        let session = Session::new("hello");
        store.create_session(&session).await.unwrap();
        let path = dir.join("sessions").join(format!("{}.json", session.id));

        // Rewrite the file with different-but-valid JSON (title changed).
        let json = std::fs::read(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        value["title"] = serde_json::Value::String("EDITED".into());
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let err = store.load_session(session.id).await.unwrap_err();
        assert!(
            matches!(&err, PersistenceError::Corrupt(_, msg) if msg.contains("checksum mismatch")),
            "expected checksum mismatch, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SEC-03: records written without a manifest entry remain readable
    /// (backward compatibility with pre-P2.2 stores), and re-writing a record
    /// registers it.
    #[tokio::test]
    async fn checksum_backfill_compatible() {
        let dir = unique_dir();
        let store = LocalFileStore::open(&dir).await.unwrap();
        let session = Session::new("hello");
        store.create_session(&session).await.unwrap();

        // A store with no manifest yet (simulate an old store by moving it away).
        let manifest = dir.join(CHECKSUMS_FILE);
        std::fs::remove_file(&manifest).unwrap();
        let loaded = store.load_session(session.id).await.unwrap();
        assert_eq!(loaded.title, "hello");

        // Re-write registers an entry again.
        let mut s2 = loaded;
        s2.push(openrt_core::Message::user("hi"));
        store.update_session(&s2).await.unwrap();
        assert!(manifest.exists());

        // Now a tamper is caught.
        let path = dir.join("sessions").join(format!("{}.json", session.id));
        let json = std::fs::read(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        value["title"] = serde_json::Value::String("EDITED".into());
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let err = store.load_session(session.id).await.unwrap_err();
        assert!(matches!(&err, PersistenceError::Corrupt(_, m) if m.contains("checksum mismatch")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SEC-12: a corrupt event line surfaces as a structured `Corrupt` error
    /// instead of being silently dropped.
    #[tokio::test]
    async fn corrupt_event_line_surfaces_as_error() {
        let dir = unique_dir();
        let store = LocalFileStore::open(&dir).await.unwrap();
        store
            .append_event(
                &DomainEvent::new(EventKind::TaskCompleted).with_data(serde_json::json!({"i": 1})),
            )
            .await
            .unwrap();

        // Inject a garbage line into the newest event file.
        let events_dir = dir.join("events");
        let mut rd = tokio::fs::read_dir(&events_dir).await.unwrap();
        let mut target = None;
        while let Ok(Some(entry)) = rd.next_entry().await {
            if entry
                .path()
                .extension()
                .map(|e| e == "jsonl")
                .unwrap_or(false)
            {
                target = Some(entry.path());
            }
        }
        let path = target.expect("event file present");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{ not valid json }\n");
        std::fs::write(&path, bytes).unwrap();

        let err = store.list_events(ListOptions::default()).await.unwrap_err();
        assert!(
            matches!(&err, PersistenceError::Corrupt(_, msg) if msg.contains("unparsable event line")),
            "expected unparsable-line Corrupt, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SEC-12: a stale `.tmp` file wedges nothing — it is swept on open and
    /// filtered out of record listings even if left behind.
    #[tokio::test]
    async fn stale_tmp_files_swept_on_open() {
        let dir = unique_dir();
        {
            let store = LocalFileStore::open(&dir).await.unwrap();
            let session = Session::new("sweep");
            store.create_session(&session).await.unwrap();
        }
        let poison = dir
            .join("tasks")
            .join("deadbeef.json")
            .with_extension("json.abc123.tmp");
        std::fs::write(&poison, b"{}").unwrap();

        let store = LocalFileStore::open(&dir).await.unwrap();
        assert!(!poison.exists(), "stale tmp should have been swept");
        assert_eq!(store.list_sessions().await.unwrap().len(), 1);
        assert_eq!(store.list_tasks(None).await.unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SEC-03: `clear()` wipes records and the checksum manifest together.
    #[tokio::test]
    async fn clear_removes_checksum_manifest() {
        let dir = unique_dir();
        let store = LocalFileStore::open(&dir).await.unwrap();
        let session = Session::new("wipe");
        store.create_session(&session).await.unwrap();
        assert!(dir.join(CHECKSUMS_FILE).exists());

        store.clear().await.unwrap();
        assert_eq!(store.list_sessions().await.unwrap().len(), 0);
        assert!(!dir.join(CHECKSUMS_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX-1: two live stores on the same directory (the cross-process writer
    /// scenario, reproducible with two concurrent `sih demo` loops) must be
    /// able to interleave record writes and manifest updates without leaving
    /// SEC-03 checksum mismatches. Every record stays readable from both
    /// handles — including the reader whose cached manifest is stale — and
    /// from a fresh open afterwards.
    #[tokio::test]
    async fn interleaved_writers_on_one_dir_stay_consistent() {
        let dir = unique_dir();
        let a = LocalFileStore::open(&dir).await.unwrap();
        let b = LocalFileStore::open(&dir).await.unwrap();

        a.put_record("blackboard", "k1", &serde_json::json!({ "v": 1 }))
            .await
            .unwrap();
        b.put_record("blackboard", "k2", &serde_json::json!({ "v": 2 }))
            .await
            .unwrap();
        // Same-key handoff: B rewrites a record A created (the repro case).
        b.put_record("blackboard", "k1", &serde_json::json!({ "v": 3 }))
            .await
            .unwrap();

        // A's cached manifest is stale for k1 — the read must reconcile
        // against the on-disk manifest instead of failing closed.
        let ra = a.get_record("blackboard", "k1").await.unwrap().unwrap();
        let rb = b.get_record("blackboard", "k1").await.unwrap().unwrap();
        assert_eq!(ra["v"], 3);
        assert_eq!(rb["v"], 3);
        assert!(a.get_record("blackboard", "k2").await.unwrap().is_some());
        assert!(b.get_record("blackboard", "k2").await.unwrap().is_some());

        // Cross-process delete merges into the shared manifest too.
        b.delete_record("blackboard", "k2").await.unwrap();
        assert!(a.get_record("blackboard", "k2").await.unwrap().is_none());

        drop(a);
        drop(b);
        let fresh = LocalFileStore::open(&dir).await.unwrap();
        assert_eq!(
            fresh.get_record("blackboard", "k1").await.unwrap().unwrap()["v"],
            3
        );
        assert!(fresh
            .get_record("blackboard", "k2")
            .await
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX-1: `clear()` keeps `.store.lock` alive (its open fd is what pins
    /// cross-process exclusion between opens) alongside `schema.json`, and
    /// the store stays fully usable afterwards.
    #[tokio::test]
    async fn clear_preserves_store_lock_file() {
        let dir = unique_dir();
        let store = LocalFileStore::open(&dir).await.unwrap();
        store
            .put_record("blackboard", "k", &serde_json::json!(1))
            .await
            .unwrap();

        store.clear().await.unwrap();
        assert!(dir.join(STORE_LOCK_FILE).exists());
        assert!(dir.join("schema.json").exists());

        drop(store);
        let fresh = LocalFileStore::open(&dir).await.unwrap();
        assert!(fresh.get_record("blackboard", "k").await.unwrap().is_none());
        fresh
            .put_record("blackboard", "k2", &serde_json::json!(2))
            .await
            .unwrap();
        assert!(fresh
            .get_record("blackboard", "k2")
            .await
            .unwrap()
            .is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX-4: each per-operation flock is a FRESH open of `.store.lock`
    /// (distinct open file description), so a second in-flight operation
    /// WAITS for the first instead of idempotently "sharing" it. The old
    /// shared-fd design let both acquire instantly, and the first guard's
    /// drop then released the lock while the second was still in its
    /// critical section — the window another process used to interleave a
    /// manifest RMW (false checksum mismatches under the concurrent
    /// `sih demo` battery).
    #[tokio::test]
    async fn per_op_flocks_unlock_independently() {
        let dir = unique_dir();
        let store = LocalFileStore::open(&dir).await.unwrap();

        let g1 = store.op_flock().await.unwrap().expect("first op flock");

        // External lockers are excluded while any operation guard is held.
        let ext = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(STORE_LOCK_FILE))
            .unwrap();
        assert!(matches!(
            ext.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));

        // A second operation must block on the distinct OFD, not acquire
        // the shared lock instantly (the FIX-4 regression signature).
        let second =
            tokio::time::timeout(std::time::Duration::from_millis(200), store.op_flock()).await;
        assert!(
            second.is_err(),
            "second op flock must wait while the first holds the lock"
        );

        // First operation finishes: the waiter acquires and still excludes
        // the external locker.
        drop(g1);
        let g2 = tokio::time::timeout(std::time::Duration::from_secs(6), store.op_flock())
            .await
            .expect("second flock acquires after the first drops")
            .expect("acquire")
            .expect("second op flock");
        assert!(
            matches!(ext.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "g2 must exclude external lockers"
        );

        drop(g2);
        ext.try_lock()
            .expect("lock must be free once every operation guard dropped");
        let _ = ext.unlock();
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX-5: SIGKILL between the record's atomic rename and the manifest
    /// update must not brick the store. Rebuild the exact crash state
    /// (marker + new record, stale manifest): the next open reconciles, the
    /// record reads back correctly, and the heal is durable across a later
    /// open with no marker left behind.
    #[tokio::test]
    async fn crash_between_record_and_manifest_heals_on_open() {
        let dir = unique_dir();
        {
            let store = LocalFileStore::open(&dir).await.unwrap();
            store
                .put_record("blackboard", "k", &serde_json::json!({ "v": 1 }))
                .await
                .unwrap();
        }
        // Exact interrupted critical section: intent marker + record rename
        // landed; manifest update never ran.
        let new = serde_json::to_vec(&serde_json::json!({ "v": 2 })).unwrap();
        let digest = sha256_hex(&new);
        write_pending_marker(&dir, "blackboard/k.json", "write", Some(&digest))
            .await
            .unwrap();
        atomic_write(&dir.join("blackboard").join("k.json"), &new)
            .await
            .unwrap();

        let healed = LocalFileStore::open(&dir).await.unwrap();
        let got = healed.get_record("blackboard", "k").await.unwrap().unwrap();
        assert_eq!(
            got["v"], 2,
            "interrupted write must be visible after reconcile"
        );
        drop(healed);

        let later = LocalFileStore::open(&dir).await.unwrap();
        assert_eq!(
            later.get_record("blackboard", "k").await.unwrap().unwrap()["v"],
            2,
            "heal must have been persisted"
        );
        assert!(!dir.join(PENDING_FILE).exists(), "marker must be consumed");
        drop(later);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX-5: a leftover marker must NOT turn tampering into a silent heal.
    /// File content that does not match the crashed write's intent keeps the
    /// normal fail-closed verify path.
    #[tokio::test]
    async fn stale_marker_does_not_mask_tampering() {
        let dir = unique_dir();
        {
            let store = LocalFileStore::open(&dir).await.unwrap();
            store
                .put_record("blackboard", "k", &serde_json::json!({ "v": 1 }))
                .await
                .unwrap();
        }
        let evil = serde_json::to_vec(&serde_json::json!({ "v": 666 })).unwrap();
        write_pending_marker(&dir, "blackboard/k.json", "write", Some("deadbeef"))
            .await
            .unwrap();
        atomic_write(&dir.join("blackboard").join("k.json"), &evil)
            .await
            .unwrap();

        let store = LocalFileStore::open(&dir).await.unwrap();
        let res = store.get_record("blackboard", "k").await;
        assert!(res.is_err(), "file/marker disagreement must fail closed");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX-5: a marker whose record write never landed (crash before the
    /// rename) changes nothing: the old consistent state stays readable and
    /// the marker is consumed.
    #[tokio::test]
    async fn marker_without_record_write_keeps_old_state() {
        let dir = unique_dir();
        {
            let store = LocalFileStore::open(&dir).await.unwrap();
            store
                .put_record("blackboard", "k", &serde_json::json!({ "v": 1 }))
                .await
                .unwrap();
        }
        let new = serde_json::to_vec(&serde_json::json!({ "v": 2 })).unwrap();
        write_pending_marker(&dir, "blackboard/k.json", "write", Some(&sha256_hex(&new)))
            .await
            .unwrap();

        let store = LocalFileStore::open(&dir).await.unwrap();
        assert_eq!(
            store.get_record("blackboard", "k").await.unwrap().unwrap()["v"],
            1,
            "unlanded write must not change the visible state"
        );
        assert!(!dir.join(PENDING_FILE).exists(), "marker must be consumed");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX-5: SIGKILL between the unlink and the manifest removal leaves a
    /// stale entry for a record that no longer exists — the next open drops
    /// it, and the record reads as absent.
    #[tokio::test]
    async fn crash_between_delete_and_manifest_heals_on_open() {
        let dir = unique_dir();
        {
            let store = LocalFileStore::open(&dir).await.unwrap();
            store
                .put_record("blackboard", "k", &serde_json::json!({ "v": 1 }))
                .await
                .unwrap();
        }
        // Interrupted delete: intent + unlink landed, manifest removal did not.
        write_pending_marker(&dir, "blackboard/k.json", "delete", None)
            .await
            .unwrap();
        tokio::fs::remove_file(dir.join("blackboard").join("k.json"))
            .await
            .unwrap();

        let store = LocalFileStore::open(&dir).await.unwrap();
        assert!(
            store.get_record("blackboard", "k").await.unwrap().is_none(),
            "deleted record must read as absent after reconcile"
        );
        assert!(!dir.join(PENDING_FILE).exists(), "marker must be consumed");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
