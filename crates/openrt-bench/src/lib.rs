//! Deterministic benchmark harness recording results into the `bench.results`
//! namespace.
//!
//! A [`BenchRunner`] drives a [`ModelProvider`] through a fixed [`BenchParams`]
//! (repetitions per task × per-attempt timeout) across a list of
//! [`BenchTask`]s. Every attempt produces a [`SampleResult`]; the pure
//! [`aggregate`] function reduces a sample slice into a [`SampleAggregate`]
//! (median/p95 computed over OK samples that reported a latency). Results are
//! [`BenchResult`]s that can be persisted to and reloaded from the shared
//! foundation store via [`record_result`] / [`load_results`].

use std::time::Duration;

use openrt_core::{encode_safe_key, now, Message};
use openrt_models::{ModelParameters, ModelProvider, ModelRequest};
use openrt_persistence::{PersistenceError, PersistenceStore};
use serde::{Deserialize, Serialize};

/// Namespace under which benchmark results are recorded.
pub const BENCH_NS: &str = "bench.results";

/// Benchmark parameters: repetitions per task and per-attempt timeout.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BenchParams {
    pub repetitions: u8,
    pub timeout_secs: u64,
}

impl Default for BenchParams {
    fn default() -> Self {
        Self {
            repetitions: 3,
            timeout_secs: 60,
        }
    }
}

/// One benchmark task: a named prompt fed to the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchTask {
    pub name: String,
    pub prompt: String,
    pub max_tokens: Option<u32>,
}

impl BenchTask {
    pub fn new(name: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            prompt: prompt.into(),
            max_tokens: None,
        }
    }
}

/// A benchmark specification: the list of tasks to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchSpec {
    pub tasks: Vec<BenchTask>,
}

/// The outcome of a single (task, repetition) attempt.
///
/// Failed or aborted attempts carry `ok = false`, no latency, and a formatted
/// error; successful attempts carry a measured latency and the reported output
/// tokens. Every field is deterministic for the same observable run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleResult {
    pub task_name: String,
    pub repetition: u8,
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
    pub output_tokens: Option<u64>,
}

/// Statistical reduction of a bench run's samples.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SampleAggregate {
    pub count: u64,
    pub ok_count: u64,
    pub error_rate: f32,
    pub median_latency_ms: Option<u64>,
    pub p95_latency_ms: Option<u64>,
    pub total_output_tokens: u64,
}

/// The full result of one bench run against one artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchResult {
    pub artifact_id: String,
    pub params: BenchParams,
    pub finished_at: String,
    pub samples: Vec<SampleResult>,
    pub aggregate: SampleAggregate,
}

/// Drives a provider through a fixed set of tasks and repetitions.
pub struct BenchRunner<'a> {
    provider: &'a dyn ModelProvider,
    params: BenchParams,
    tasks: Vec<BenchTask>,
}

impl<'a> BenchRunner<'a> {
    pub fn new(
        provider: &'a dyn ModelProvider,
        params: BenchParams,
        tasks: Vec<BenchTask>,
    ) -> Self {
        Self {
            provider,
            params,
            tasks,
        }
    }

    /// Run every task `params.repetitions` times and reduce the samples.
    pub async fn run(&self, artifact_id: &str) -> BenchResult {
        let mut samples = Vec::with_capacity(
            self.tasks
                .len()
                .saturating_mul(self.params.repetitions as usize),
        );

        for task in &self.tasks {
            for repetition in 1..=self.params.repetitions {
                let request = ModelRequest {
                    parameters: ModelParameters {
                        max_tokens: task.max_tokens,
                        ..Default::default()
                    },
                    ..ModelRequest::new(vec![Message::user(task.prompt.clone())])
                };

                let start = std::time::Instant::now();
                let outcome = tokio::time::timeout(
                    Duration::from_secs(self.params.timeout_secs),
                    self.provider.generate(request),
                )
                .await;

                let sample = match outcome {
                    Ok(Ok(response)) => SampleResult {
                        task_name: task.name.clone(),
                        repetition,
                        ok: true,
                        latency_ms: Some(start.elapsed().as_millis() as u64),
                        error: None,
                        output_tokens: response.usage.output_tokens,
                    },
                    Ok(Err(e)) => SampleResult {
                        task_name: task.name.clone(),
                        repetition,
                        ok: false,
                        latency_ms: None,
                        error: Some(e.to_string()),
                        output_tokens: None,
                    },
                    Err(_elapsed) => SampleResult {
                        task_name: task.name.clone(),
                        repetition,
                        ok: false,
                        latency_ms: None,
                        error: Some("timeout".to_string()),
                        output_tokens: None,
                    },
                };
                samples.push(sample);
            }
        }

        let aggregate = aggregate(&samples);
        BenchResult {
            artifact_id: artifact_id.to_string(),
            params: self.params,
            finished_at: now().to_rfc3339(),
            samples,
            aggregate,
        }
    }
}

/// Pure, deterministic reduction of a sample slice.
///
/// The median and p95 are computed over OK samples that reported a latency,
/// sorted ascending; p95 uses the nearest-rank method (`ceil(count * 0.95)`).
/// `error_rate` is the fraction of failed attempts.
pub fn aggregate(samples: &[SampleResult]) -> SampleAggregate {
    let count = samples.len() as u64;
    let ok_count = samples.iter().filter(|s| s.ok).count() as u64;

    let mut latencies: Vec<u64> = samples
        .iter()
        .filter(|s| s.ok)
        .filter_map(|s| s.latency_ms)
        .collect();
    latencies.sort_unstable();

    let total_output_tokens = samples
        .iter()
        .filter(|s| s.ok)
        .fold(0u64, |acc, s| acc + s.output_tokens.unwrap_or(0));

    let error_rate = if count == 0 {
        0.0
    } else {
        (count - ok_count) as f32 / count as f32
    };

    SampleAggregate {
        count,
        ok_count,
        error_rate,
        median_latency_ms: median(&latencies),
        p95_latency_ms: p95(&latencies),
        total_output_tokens,
    }
}

fn median(sorted: &[u64]) -> Option<u64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let mid = n / 2;
    if n % 2 == 1 {
        Some(sorted[mid])
    } else {
        Some((sorted[mid - 1].saturating_add(sorted[mid])) / 2)
    }
}

fn p95(sorted: &[u64]) -> Option<u64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let rank = (n.saturating_mul(95).saturating_add(99)) / 100;
    Some(sorted[rank.saturating_sub(1).min(n - 1)])
}

/// Record one bench result into the store under `bench.results`.
///
/// The record key is `<encoded-artifact-id>.<run-<ms>>` — the store contract
/// restricts keys to `[a-z0-9._-]`, so the hierarchy between artifact and run
/// uses `.` rather than `/`.
pub async fn record_result(
    store: &dyn PersistenceStore,
    result: &BenchResult,
) -> Result<(), PersistenceError> {
    let artifact_key = encode_safe_key(&result.artifact_id);
    let run_key = format!("run-{}", now().timestamp_millis());
    let key = format!("{artifact_key}.{run_key}");
    let value = serde_json::to_value(result)?;
    store.put_record(BENCH_NS, &key, &value).await
}

/// Load every recorded bench result for an artifact id, in key order.
pub async fn load_results(
    store: &dyn PersistenceStore,
    artifact_id: &str,
) -> Result<Vec<BenchResult>, PersistenceError> {
    let artifact_key = encode_safe_key(artifact_id);
    let prefix = format!("{artifact_key}.");
    let mut out = Vec::new();
    for (key, value) in store.list_records(BENCH_NS).await? {
        if let Some(suffix) = key.strip_prefix(&prefix) {
            if is_run_key(suffix) {
                out.push(serde_json::from_value(value)?);
            }
        }
    }
    Ok(out)
}

fn is_run_key(suffix: &str) -> bool {
    match suffix.strip_prefix("run-") {
        Some(ms) => !ms.is_empty() && ms.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_models::mock::{MockMode, MockProvider};
    use openrt_persistence::MemoryStore;

    fn params(repetitions: u8) -> BenchParams {
        BenchParams {
            repetitions,
            timeout_secs: 60,
        }
    }

    fn task(name: &str, prompt: &str) -> BenchTask {
        BenchTask::new(name, prompt)
    }

    fn sample(ok: bool, latency_ms: Option<u64>, output_tokens: Option<u64>) -> SampleResult {
        SampleResult {
            task_name: "t".to_string(),
            repetition: 1,
            ok,
            latency_ms,
            error: None,
            output_tokens,
        }
    }

    #[tokio::test]
    async fn echo_repeats_repetitions_ok() {
        let provider = MockProvider::echo("mock-echo");
        let runner = BenchRunner::new(
            &provider,
            params(2),
            vec![task("t1", "hello"), task("t2", "world")],
        );
        let result = runner.run("bench/echo").await;

        assert_eq!(result.samples.len(), 4);
        assert!(result.samples.iter().all(|s| s.ok));
        assert_eq!(result.aggregate.count, 4);
        assert_eq!(result.aggregate.ok_count, 4);
        assert!(result.aggregate.median_latency_ms.is_some());
        // Each echo response reports 4 output tokens.
        assert_eq!(result.aggregate.total_output_tokens, 16);
    }

    #[tokio::test]
    async fn failed_provider_counts_error_rate() {
        let provider = MockProvider::new("mock-fail", MockMode::Fail);
        let runner = BenchRunner::new(&provider, params(2), vec![task("t", "ping")]);
        let result = runner.run("bench/fail").await;

        assert_eq!(result.samples.len(), 2);
        assert!(result.samples.iter().all(|s| !s.ok));
        assert!(result.samples.iter().all(|s| s.latency_ms.is_none()));
        assert!(result.aggregate.ok_count == 0);
        assert_eq!(result.aggregate.error_rate, 1.0);
        assert_eq!(result.aggregate.median_latency_ms, None);
    }

    #[tokio::test]
    async fn timeout_bounded() {
        let provider = MockProvider::new("mock-hang", MockMode::Hang);
        let runner = BenchRunner::new(
            &provider,
            BenchParams {
                repetitions: 1,
                timeout_secs: 1,
            },
            vec![task("t", "boo")],
        );
        let result = runner.run("bench/hang").await;

        assert_eq!(result.samples.len(), 1);
        let s = &result.samples[0];
        assert!(!s.ok);
        assert!(s.latency_ms.is_none());
        let err = s.error.as_deref().unwrap_or("");
        assert!(err.contains("timeout"), "expected timeout, got {err}");
    }

    #[test]
    fn aggregation_median_p95() {
        let samples = vec![
            sample(true, Some(10), Some(1)),
            sample(true, Some(20), Some(2)),
            sample(true, Some(30), Some(3)),
            sample(true, Some(40), Some(4)),
            sample(true, Some(50), Some(5)),
            sample(false, None, None),
        ];
        let agg = aggregate(&samples);

        assert_eq!(agg.count, 6);
        assert_eq!(agg.ok_count, 5);
        assert_eq!(agg.median_latency_ms, Some(30));
        assert_eq!(agg.p95_latency_ms, Some(50));
        assert_eq!(agg.total_output_tokens, 15);
        assert!((agg.error_rate - 1.0 / 6.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn record_load_roundtrip() {
        let store = MemoryStore::new();
        let provider = MockProvider::echo("mock-echo");
        let runner = BenchRunner::new(&provider, params(2), vec![task("t", "hello")]);
        let result = runner.run("bench-roundtrip").await;

        record_result(&store, &result).await.unwrap();
        let loaded = load_results(&store, "bench-roundtrip").await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], result);

        let keys: Vec<String> = store
            .list_records(BENCH_NS)
            .await
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys.len(), 1);
        assert!(!keys[0].contains('/'));
        assert!(keys[0].starts_with(&format!("{}.run-", encode_safe_key("bench-roundtrip"))));
    }
}
