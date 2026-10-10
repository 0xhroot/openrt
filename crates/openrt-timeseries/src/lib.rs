use openrt_persistence::{PersistenceError, PersistenceStore};
use serde::Serialize;

pub const HISTORY_LEDGER_NS: &str = "history.ledger";
pub const HISTORY_FORKS_NS: &str = "history.forks";

fn safe_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => out.push(ch),
            _ => {
                out.push('_');
                for b in ch.to_string().as_bytes() {
                    out.push_str(&format!("{:02x}", b));
                }
            }
        }
    }
    out
}

fn commit_key(seq: u64) -> String {
    format!("seq.{:012}", seq)
}

fn branch_ref_key(branch: &str) -> String {
    format!("br.{}.ref", safe_key(branch))
}

fn fork_commit_key(branch: &str, n: u64) -> String {
    format!("br.{}.c.{:012}", safe_key(branch), n)
}

// ---------------------------------------------------------------------------
// CANONICAL BYTES — THE SINGLE SOURCE OF DIGEST TRUTH
//
// append_event and verify_chain MUST hash the EXACT same canonical bytes.
// `digest` and `prev_digest` are CHAIN METADATA, not payload: they are
// EXCLUDED from the canonical form so that (a) a record can be self-describing
// without a chicken-and-egg digest, and (b) verification recomputes the digest
// over byte-identical input to what append_event hashed.
// ---------------------------------------------------------------------------
pub fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(m) => {
            let mut kvs: Vec<(String, String)> = m
                .iter()
                .filter(|(k, _)| *k != "digest" && *k != "prev_digest")
                .map(|(k, v)| (k.clone(), canonical(v)))
                .collect();
            kvs.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = String::from("{");
            for (i, (k, v)) in kvs.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&k);
                out.push_str("\":");
                out.push_str(&v);
            }
            out.push('}');
            out
        }
        other => other.to_string(),
    }
}

/// sha256 over the exact canonical bytes. The ONE identity function.
fn compute_digest(rec: &serde_json::Value) -> String {
    openrt_core::sha256_hex(canonical(rec).as_bytes())
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct HistoryRecord {
    pub seq: u64,
    pub session_id: String,
    pub node_id: String,
    pub status: String,
    pub event_kind: String,
    pub note: String,
    pub digest: String,
    pub prev_digest: String,
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("integrity: {0}")]
    Integrity(String),
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("branch exists: {0}")]
    BranchExists(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("store: {0}")]
    Store(#[from] PersistenceError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

fn from_stored(v: serde_json::Value) -> Result<HistoryRecord, HistoryError> {
    serde_json::from_value(v).map_err(HistoryError::Json)
}

/// Append ONE deterministic commit to the parent ledger: seq is derived from
/// the persisted tail (never caller-supplied), `prev_digest` links to the
/// current head, and `digest` is recomputed from canonical bytes so that
/// `verify_chain` can recompute EXACTLY the same digest.
pub async fn append_event(
    store: &dyn PersistenceStore,
    session_id: &str,
    node_id: &str,
    status: &str,
    event_kind: &str,
    note: &str,
) -> Result<u64, HistoryError> {
    // seq = tail.len() + 1 is a read-modify-write: serialize it across tasks
    // and processes or two appenders allocate the same seq and fork the chain.
    let _section = store.lock_exclusive().await?;
    let mut tail: Vec<(String, serde_json::Value)> = store.list_records(HISTORY_LEDGER_NS).await?;
    tail.sort_by(|a, b| a.0.cmp(&b.0));
    let seq = tail.len() as u64 + 1;
    let prev_digest = match tail.last() {
        Some((_, v)) => v["digest"].as_str().unwrap_or_default().to_string(),
        None => String::new(),
    };
    let mut rec = serde_json::json!({
        "seq": seq,
        "session_id": session_id,
        "node_id": node_id,
        "status": status,
        "event_kind": event_kind,
        "note": note,
        "prev_digest": prev_digest,
    });
    let digest = compute_digest(&rec);
    rec["digest"] = serde_json::json!(digest);
    store
        .put_record(HISTORY_LEDGER_NS, &commit_key(seq), &rec)
        .await?;
    Ok(seq)
}

/// Recompute EVERY stored digest from canonical bytes and confirm each
/// `prev_digest` chains to the PREVIOUS record's digest. Fail-closed on the
/// first mismatch. Returns the number of records verified.
pub async fn verify_chain(store: &dyn PersistenceStore) -> Result<u64, HistoryError> {
    let mut records: Vec<(String, serde_json::Value)> =
        store.list_records(HISTORY_LEDGER_NS).await?;
    records.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected_prev = String::new();
    let mut count = 0u64;
    for (k, rec) in &records {
        let Some(seq) = k.strip_prefix("seq.").and_then(|s| s.parse::<u64>().ok()) else {
            return Err(HistoryError::Corrupt(format!("key `{k}`")));
        };
        let stored_digest = rec["digest"].as_str().unwrap_or_default().to_string();
        let recomputed = compute_digest(rec); // EXACT append_event bytes
        if stored_digest.is_empty() || stored_digest != recomputed {
            return Err(HistoryError::Integrity(format!(
                "record {k}: digest mismatch"
            )));
        }
        let prev = rec["prev_digest"].as_str().unwrap_or_default().to_string();
        if seq == 1 {
            if !prev.is_empty() {
                return Err(HistoryError::Integrity(format!(
                    "record {k}: genesis must have empty prev_digest"
                )));
            }
        } else if prev != expected_prev {
            return Err(HistoryError::Integrity(format!(
                "record {k}: prev_digest {prev} != chain {expected_prev}"
            )));
        }
        expected_prev = stored_digest.clone();
        count += 1;
    }
    Ok(count)
}

/// Deterministic, byte-identical replay as of a persisted point. READ-ONLY:
/// the ledger is never rewritten.
pub async fn materialize_as_of(
    store: &dyn PersistenceStore,
    as_of_seq: u64,
) -> Result<Vec<HistoryRecord>, HistoryError> {
    verify_chain(store).await?;
    let mut records: Vec<(String, serde_json::Value)> =
        store.list_records(HISTORY_LEDGER_NS).await?;
    records.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (k, v) in records {
        let Some(seq) = k.strip_prefix("seq.").and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        if seq > as_of_seq {
            break;
        }
        out.push(from_stored(v)?);
    }
    Ok(out)
}

/// Isolated append-only fork branch. The parent ledger is NEVER written; the
/// fork references the parent head digest in ITS OWN namespace.
pub async fn create_fork(
    store: &dyn PersistenceStore,
    branch: &str,
    base_label: &str,
) -> Result<(), HistoryError> {
    // Branch-ref existence check + creation is a read-modify-write.
    let _section = store.lock_exclusive().await?;
    let ref_key = branch_ref_key(branch);
    if store
        .get_record(HISTORY_FORKS_NS, &ref_key)
        .await?
        .is_some()
    {
        return Err(HistoryError::BranchExists(branch.to_string()));
    }
    let base_seq = match base_label {
        "" => store.list_records(HISTORY_LEDGER_NS).await?.len() as u64,
        l => l
            .strip_prefix("seq.")
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| HistoryError::Corrupt(format!("base label `{l}`")))?,
    };
    let base_digest = if base_seq > 0 {
        store
            .get_record(HISTORY_LEDGER_NS, &commit_key(base_seq))
            .await?
            .map(|v| v["digest"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    store
        .put_record(
            HISTORY_FORKS_NS,
            &ref_key,
            &serde_json::json!({
                "branch": branch,
                "base_seq": base_seq,
                "base_digest": base_digest,
                "commits": 0,
            }),
        )
        .await?;
    Ok(())
}

pub async fn fork_verify_chain(
    store: &dyn PersistenceStore,
    branch: &str,
) -> Result<u64, HistoryError> {
    let ref_key = branch_ref_key(branch);
    let Some(ref_v) = store.get_record(HISTORY_FORKS_NS, &ref_key).await? else {
        return Err(HistoryError::NotFound(format!("branch `{branch}`")));
    };
    let base_digest = ref_v["base_digest"].as_str().unwrap_or_default();
    let commits: Vec<(String, serde_json::Value)> = store
        .list_records(HISTORY_FORKS_NS)
        .await?
        .into_iter()
        .filter(|(k, _)| k.starts_with(&format!("br.{}.c.", safe_key(branch))))
        .collect();
    let mut commits: Vec<(String, serde_json::Value)> = commits;
    commits.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected_prev = base_digest.to_string();
    let mut count = 0u64;
    for (k, rec) in &commits {
        let Some(n) = k.rsplit('.').next().and_then(|s| s.parse::<u64>().ok()) else {
            return Err(HistoryError::Corrupt(format!("key `{k}`")));
        };
        let stored_digest = rec["digest"].as_str().unwrap_or_default().to_string();
        let recomputed = compute_digest(rec);
        if stored_digest.is_empty() || stored_digest != recomputed {
            return Err(HistoryError::Integrity(format!(
                "fork `{branch}` commit {k}: digest mismatch"
            )));
        }
        let prev = rec["prev_digest"].as_str().unwrap_or_default().to_string();
        if prev != expected_prev {
            return Err(HistoryError::Integrity(format!(
                "fork `{branch}` commit {k}: prev_digest {prev} != chain {expected_prev}"
            )));
        }
        expected_prev = stored_digest.clone();
        count += 1;
        if n == 1 && !base_digest.is_empty() && prev != base_digest {
            return Err(HistoryError::Integrity(format!(
                "fork `{branch}` commit {k}: genesis must chain to base_digest"
            )));
        }
    }
    Ok(count)
}

/// Append one commit onto an existing fork branch. The fork chain stays inside
/// `HISTORY_FORKS_NS`; the parent ledger is untouched.
pub async fn fork_commit(
    store: &dyn PersistenceStore,
    branch: &str,
    node_id: &str,
    status: &str,
    event_kind: &str,
    note: &str,
) -> Result<u64, HistoryError> {
    // commits = ref.commits + 1 (then two writes) is a read-modify-write.
    let _section = store.lock_exclusive().await?;
    let ref_key = branch_ref_key(branch);
    let Some(ref_rec_v) = store.get_record(HISTORY_FORKS_NS, &ref_key).await? else {
        return Err(HistoryError::NotFound(format!("branch `{branch}`")));
    };
    let ref_rec = ref_rec_v;
    let commits = ref_rec["commits"].as_u64().unwrap_or(0) + 1;
    let base_seq = ref_rec["base_seq"].as_u64().unwrap_or(0);
    let prev_digest = if commits == 1 {
        ref_rec["base_digest"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    } else {
        let prev = store
            .get_record(HISTORY_FORKS_NS, &fork_commit_key(branch, commits - 1))
            .await?
            .ok_or_else(|| {
                HistoryError::NotFound(format!("fork `{branch}` commit {}", commits - 1))
            })?;
        prev["digest"].as_str().unwrap_or_default().to_string()
    };
    let mut rec = serde_json::json!({
        "seq": base_seq + commits,
        "branch": branch,
        "node_id": node_id,
        "status": status,
        "event_kind": event_kind,
        "note": note,
        "prev_digest": prev_digest,
    });
    let digest = compute_digest(&rec);
    rec["digest"] = serde_json::json!(digest);
    store
        .put_record(HISTORY_FORKS_NS, &fork_commit_key(branch, commits), &rec)
        .await?;
    let mut updated = ref_rec;
    updated["commits"] = serde_json::json!(commits);
    store
        .put_record(HISTORY_FORKS_NS, &ref_key, &updated)
        .await?;
    Ok(commits)
}

/// Tail seq of the PARENT ledger only.
pub async fn tail_seq(store: &dyn PersistenceStore) -> Result<u64, HistoryError> {
    Ok(store.list_records(HISTORY_LEDGER_NS).await?.len() as u64)
}

// ---------------------------------------------------------------------------
// PHASE 10 M10.5 — typed readers for the time-machine model.
// Read-only over the same namespaces; every reader preserves the append-only
// contract (the parent ledger is never written here).
// ---------------------------------------------------------------------------

/// Typed fork-branch reference (the `br.<branch>.ref` record).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ForkRef {
    pub branch: String,
    pub base_seq: u64,
    pub base_digest: String,
    pub commits: u64,
}

/// One commit on a fork branch — the same chain shape as a ledger record,
/// with `branch` where the parent row has `session_id`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ForkCommit {
    pub seq: u64,
    pub branch: String,
    pub node_id: String,
    pub status: String,
    pub event_kind: String,
    pub note: String,
    pub digest: String,
    pub prev_digest: String,
}

/// Full verified parent ledger (chain-checked, chronological).
pub async fn ledger_records(
    store: &dyn PersistenceStore,
) -> Result<Vec<HistoryRecord>, HistoryError> {
    materialize_as_of(store, u64::MAX).await
}

/// The fork-branch reference record. Fail-closed: unknown branch is
/// `NotFound`, never an empty default.
pub async fn fork_ref(store: &dyn PersistenceStore, branch: &str) -> Result<ForkRef, HistoryError> {
    let ref_key = branch_ref_key(branch);
    let Some(v) = store.get_record(HISTORY_FORKS_NS, &ref_key).await? else {
        return Err(HistoryError::NotFound(format!("branch `{branch}`")));
    };
    Ok(ForkRef {
        branch: v["branch"].as_str().unwrap_or_default().to_string(),
        base_seq: v["base_seq"].as_u64().unwrap_or_default(),
        base_digest: v["base_digest"].as_str().unwrap_or_default().to_string(),
        commits: v["commits"].as_u64().unwrap_or_default(),
    })
}

/// Every fork branch reference, sorted by branch name.
pub async fn list_forks(store: &dyn PersistenceStore) -> Result<Vec<ForkRef>, HistoryError> {
    let rows = store.list_records(HISTORY_FORKS_NS).await?;
    let mut out = Vec::new();
    for (k, v) in rows {
        if !k.starts_with("br.") || !k.ends_with(".ref") {
            continue;
        }
        out.push(ForkRef {
            branch: v["branch"].as_str().unwrap_or_default().to_string(),
            base_seq: v["base_seq"].as_u64().unwrap_or_default(),
            base_digest: v["base_digest"].as_str().unwrap_or_default().to_string(),
            commits: v["commits"].as_u64().unwrap_or_default(),
        });
    }
    out.sort_by(|a, b| a.branch.cmp(&b.branch));
    Ok(out)
}

/// Commits of one fork branch in chain order. Fail-closed: unknown branch
/// is `NotFound`.
pub async fn fork_commits(
    store: &dyn PersistenceStore,
    branch: &str,
) -> Result<Vec<ForkCommit>, HistoryError> {
    // Ensure the branch exists before reading its commits.
    let _ = fork_ref(store, branch).await?;
    let prefix = format!("br.{}.c.", safe_key(branch));
    let mut rows: Vec<(String, serde_json::Value)> = store
        .list_records(HISTORY_FORKS_NS)
        .await?
        .into_iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::with_capacity(rows.len());
    for (k, v) in rows {
        let rec: ForkCommit = serde_json::from_value(v)
            .map_err(|e| HistoryError::Corrupt(format!("fork commit `{k}`: {e}")))?;
        out.push(rec);
    }
    Ok(out)
}

#[cfg(test)]
mod p5_tests {
    use super::*;
    use openrt_persistence::MemoryStore;
    use std::sync::Arc;

    fn mem() -> Arc<dyn PersistenceStore> {
        Arc::new(MemoryStore::new()) as Arc<dyn PersistenceStore>
    }

    #[tokio::test]
    async fn append_and_materialize_are_deterministic() {
        let s = mem();
        let a = append_event(&*s, "s1", "n1", "ok", "task.step", "")
            .await
            .unwrap();
        let b = append_event(&*s, "s1", "n2", "ok", "task.step", "")
            .await
            .unwrap();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(verify_chain(&*s).await.unwrap(), 2);
        let asof1 = materialize_as_of(&*s, 1).await.unwrap();
        assert_eq!(asof1.len(), 1);
        assert_eq!(asof1[0].seq, 1);
        let asof1b = materialize_as_of(&*s, 1).await.unwrap();
        assert_eq!(
            serde_json::to_vec(&asof1).unwrap(),
            serde_json::to_vec(&asof1b).unwrap()
        );
        assert_eq!(materialize_as_of(&*s, 2).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn tampered_middle_fails_verify() {
        let s = mem();
        append_event(&*s, "s1", "n1", "ok", "task.step", "")
            .await
            .unwrap();
        append_event(&*s, "s1", "n2", "ok", "task.step", "")
            .await
            .unwrap();
        append_event(&*s, "s1", "n3", "ok", "task.step", "")
            .await
            .unwrap();
        let mut rec = s
            .get_record(HISTORY_LEDGER_NS, &commit_key(2))
            .await
            .unwrap()
            .unwrap();
        rec["digest"] =
            serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000");
        s.put_record(HISTORY_LEDGER_NS, &commit_key(2), &rec)
            .await
            .unwrap();
        assert!(matches!(
            verify_chain(&*s).await,
            Err(HistoryError::Integrity(_))
        ));
    }

    #[tokio::test]
    async fn fork_is_isolated() {
        let dbg_keys = || async {
            let s = mem();
            append_event(&*s, "s1", "n1", "ok", "task.step", "")
                .await
                .unwrap();
            create_fork(&*s, "experiment", "").await.unwrap();
            fork_commit(&*s, "experiment", "nX", "ok", "task.step", "try-1")
                .await
                .unwrap();
            let fk: Vec<String> = s
                .list_records(HISTORY_FORKS_NS)
                .await
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            let lk: Vec<String> = s
                .list_records(HISTORY_LEDGER_NS)
                .await
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            println!("FORKS({}) {fk:?} LEDGER({}) {lk:?}", fk.len(), lk.len());
        };
        dbg_keys().await;

        let s = mem();
        append_event(&*s, "s1", "n1", "ok", "task.step", "")
            .await
            .unwrap();
        create_fork(&*s, "experiment", "").await.unwrap();
        fork_commit(&*s, "experiment", "nX", "ok", "task.step", "try-1")
            .await
            .unwrap();
        assert_eq!(tail_seq(&*s).await.unwrap(), 1);
        assert!(verify_chain(&*s).await.is_ok());
        assert_eq!(s.list_records(HISTORY_FORKS_NS).await.unwrap().len(), 2);
        assert_eq!(s.list_records(HISTORY_LEDGER_NS).await.unwrap().len(), 1);
        assert_eq!(
            canonical(&serde_json::json!({"seq":1,"prev_digest":""})).len(),
            9
        );
    }
}

#[cfg(test)]
mod m10_time_machine_tests {
    use super::*;
    use openrt_persistence::MemoryStore;
    use std::sync::Arc;

    fn mem() -> Arc<dyn PersistenceStore> {
        Arc::new(MemoryStore::new()) as Arc<dyn PersistenceStore>
    }

    #[tokio::test]
    async fn fork_readers_round_trip_and_fail_closed() {
        let s = mem();
        append_event(&*s, "s1", "n1", "ok", "task.step", "")
            .await
            .unwrap();
        append_event(&*s, "s1", "n2", "ok", "task.step", "")
            .await
            .unwrap();

        // Unknown branch → NotFound (never an empty default).
        assert!(matches!(
            fork_ref(&*s, "missing").await,
            Err(HistoryError::NotFound(_))
        ));
        assert!(matches!(
            fork_commits(&*s, "missing").await,
            Err(HistoryError::NotFound(_))
        ));
        assert!(list_forks(&*s).await.unwrap().is_empty());

        create_fork(&*s, "branch.a", "seq.000000000001")
            .await
            .unwrap();
        fork_commit(&*s, "branch.a", "nX", "ok", "task.step", "try-1")
            .await
            .unwrap();
        fork_commit(&*s, "branch.a", "nY", "ok", "task.step", "try-2")
            .await
            .unwrap();

        let r = fork_ref(&*s, "branch.a").await.unwrap();
        assert_eq!(r.branch, "branch.a");
        assert_eq!(r.base_seq, 1);
        assert_eq!(r.commits, 2);
        assert!(!r.base_digest.is_empty());

        let commits = fork_commits(&*s, "branch.a").await.unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].seq, 2);
        assert_eq!(commits[0].note, "try-1");
        assert_eq!(commits[1].seq, 3);
        assert_eq!(commits[1].prev_digest, commits[0].digest);

        let forks = list_forks(&*s).await.unwrap();
        assert_eq!(forks.len(), 1);
        assert_eq!(forks[0], r);
    }

    #[tokio::test]
    async fn ledger_records_is_the_verified_full_timeline() {
        let s = mem();
        append_event(&*s, "s1", "n1", "ok", "task.step", "")
            .await
            .unwrap();
        append_event(&*s, "s2", "n2", "ok", "task.step", "")
            .await
            .unwrap();
        let rows = ledger_records(&*s).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(rows[1].seq, 2);

        // Fail-closed: a tampered parent ledger never yields a timeline.
        let mut rec = s
            .get_record(HISTORY_LEDGER_NS, &commit_key(2))
            .await
            .unwrap()
            .unwrap();
        rec["digest"] =
            serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000");
        s.put_record(HISTORY_LEDGER_NS, &commit_key(2), &rec)
            .await
            .unwrap();
        assert!(matches!(
            ledger_records(&*s).await,
            Err(HistoryError::Integrity(_))
        ));
    }

    #[tokio::test]
    async fn fork_ref_reads_zero_commit_branch() {
        let s = mem();
        append_event(&*s, "s1", "n1", "ok", "task.step", "")
            .await
            .unwrap();
        create_fork(&*s, "branch.empty", "").await.unwrap();
        let r = fork_ref(&*s, "branch.empty").await.unwrap();
        assert_eq!(r.base_seq, 1);
        assert_eq!(r.commits, 0);
        assert!(fork_commits(&*s, "branch.empty").await.unwrap().is_empty());
    }
}
