//! `openrt-trading` — research-only trading analytics (Phase 12, M12.8–M12.11).
//!
//! Reproducible datasets, deterministic indicators, declarative causal
//! strategies, deterministic backtests with leakage detection and
//! walk-forward splits. **No broker, no order path, no execution tool
//! exists in this crate** — `execute_order_denied` is the one refusal
//! point and results are always marked simulated.

use thiserror::Error;

pub mod backtest;
pub mod dataset;
pub mod experiment;
pub mod indicator;
pub mod model;
pub mod store;
pub mod strategy;

pub use backtest::{
    detect_leakage, execute_order_denied, run, sma_crossover, split_folds, walk_forward,
    BacktestConfig, BacktestResult, Fold, FoldResult, Leak, LeakKind, LookAheadReport, Metrics,
    SampleKind, Trade, WalkForwardParams, WalkForwardResult,
};
pub use dataset::{
    build_dataset, get_bars, get_dataset, list_datasets, parse_bars_csv, put_dataset,
    verify_dataset, DatasetConfig, ResearchDataset,
};
pub use experiment::{
    get_experiment, list_experiments, put_experiment, ResearchExperiment, EXPERIMENTS_NS,
};
pub use indicator::{
    closes, ema, eval, max_drawdown, momentum, returns, rolling_volume, sma, volatility, Indicator,
};
pub use model::{Bar, Instrument, InstrumentKind, Provenance, ResearchEvent};
pub use store::{
    get_instrument, get_strategy, list_backtests, list_instruments, list_strategies, put_backtest,
    put_instrument, put_strategy,
};
pub use strategy::{
    eval_condition, precompute, referenced_indicators, Condition, CostSpec, RiskSpec, Strategy,
};

pub const INSTRUMENTS_NS: &str = "trading.instruments";
pub const DATASETS_NS: &str = "trading.datasets";
pub const STRATEGIES_NS: &str = "trading.strategies";
pub const BACKTESTS_NS: &str = "trading.backtests";

#[derive(Debug, Error)]
pub enum TradingError {
    #[error("store error: {0}")]
    Store(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("corrupt record `{0}`: {1}")]
    Corrupt(String, String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("research only: {0}")]
    ResearchOnly(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bars(vals: &[f64]) -> Vec<Bar> {
        vals.iter()
            .enumerate()
            .map(|(i, v)| {
                let t = 1_700_000_000_000 + i as i64 * 86_400_000;
                Bar {
                    t_ms: t,
                    open: *v,
                    high: v * 1.01,
                    low: v * 0.99,
                    close: *v,
                    volume: 1000.0 + i as f64,
                }
            })
            .collect()
    }

    fn cfg() -> DatasetConfig {
        DatasetConfig {
            instrument: "test.eq".into(),
            from_ms: 0,
            to_ms: i64::MAX,
            filters: Default::default(),
            params: Default::default(),
            source: "unit-test".into(),
            transform: "as_recorded".into(),
            declared_lookahead: false,
        }
    }

    #[test]
    fn dataset_digest_is_reproducible() {
        let b = bars(&[10.0, 11.0, 12.0, 13.0]);
        let (d1, _) = build_dataset("ds1", "v1", cfg(), vec!["src".into()], b.clone()).unwrap();
        let (d2, _) = build_dataset("ds1", "v1", cfg(), vec!["src".into()], b).unwrap();
        assert_eq!(d1.digest, d2.digest);
        assert_eq!(d1.bar_count, 4);
    }

    #[test]
    fn dataset_rejects_duplicate_timestamps() {
        let mut b = bars(&[10.0, 11.0]);
        b[1].t_ms = b[0].t_ms;
        assert!(build_dataset("ds", "v1", cfg(), vec![], b).is_err());
    }

    #[test]
    fn indicators_warm_up_with_none() {
        let b = bars(&[10.0, 11.0, 12.0, 13.0, 14.0]);
        let s = sma(&b, 3);
        assert_eq!(s[0], None);
        assert_eq!(s[1], None);
        assert_eq!(s[2], Some(11.0));
        assert_eq!(s[4], Some(13.0));
    }

    #[test]
    fn momentum_and_volatility_are_deterministic() {
        let b = bars(&[10.0, 11.0, 10.5, 12.0, 11.0, 13.0, 12.5, 14.0, 13.5, 15.0]);
        let m1 = momentum(&b, 2);
        let m2 = momentum(&b, 2);
        assert_eq!(m1, m2);
        assert!(m1[0].is_none() && m1[1].is_none());
        assert!(m1[2].is_some());
        let v = volatility(&b, 3);
        assert!(v.iter().filter(|x| x.is_some()).count() > 0);
    }

    #[test]
    fn crossover_backtest_is_deterministic_and_marked_simulated() {
        let mut vals: Vec<f64> = Vec::new();
        for i in 0..60 {
            vals.push(100.0 + (i as f64 * 0.9) + if i > 30 { 5.0 } else { 0.0 });
        }
        let b = bars(&vals);
        let (ds, dbars) = build_dataset("ds.a", "v1", cfg(), vec!["src".into()], b).unwrap();
        let strat = sma_crossover("s1", "test.eq", 3, 10, CostSpec::default());
        let r1 = run(&strat, &ds, &dbars, BacktestConfig::default()).unwrap();
        let r2 = run(&strat, &ds, &dbars, BacktestConfig::default()).unwrap();
        assert_eq!(r1.id, r2.id);
        assert_eq!(r1.metrics.final_equity, r2.metrics.final_equity);
        assert!(r1.simulated);
        assert!(r1.look_ahead.clean);
        assert!(!r1.assumptions.is_empty());
    }

    #[test]
    fn leakage_detection_flags_declared_lookahead() {
        let b = bars(&[
            10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0, 18.0, 19.0, 20.0,
        ]);
        let mut config = cfg();
        config.declared_lookahead = true;
        let (mut ds, dbars) = build_dataset("ds.leak", "v1", config, vec![], b).unwrap();
        ds.config.declared_lookahead = true;
        let strat = sma_crossover("s", "test.eq", 2, 4, CostSpec::default());
        let report = detect_leakage(&strat, &ds, &dbars);
        assert!(!report.clean);
        assert!(report
            .leaks
            .iter()
            .any(|l| l.kind == LeakKind::DeclaredDatasetLookahead));
        let mut strat2 = strat.clone();
        strat2.declared_lookahead = true;
        let r2 = detect_leakage(&strat2, &ds, &dbars);
        assert!(r2
            .leaks
            .iter()
            .any(|l| l.kind == LeakKind::DeclaredStrategyLookahead));
    }

    #[test]
    fn walk_forward_reports_sample_kinds_and_in_sample_only_flag() {
        let vals: Vec<f64> = (0..50).map(|i| 100.0 + (i % 7) as f64).collect();
        let b = bars(&vals);
        let (ds, dbars) = build_dataset("ds.wf", "v1", cfg(), vec![], b).unwrap();
        let strat = sma_crossover("s", "test.eq", 2, 5, CostSpec::default());
        let wf = walk_forward(&strat, &ds, &dbars, WalkForwardParams::default()).unwrap();
        assert_eq!(wf.folds.len(), 3);
        assert_eq!(wf.folds[0].fold.kind, SampleKind::Train);
        assert_eq!(wf.folds[1].fold.kind, SampleKind::Validation);
        assert_eq!(wf.folds[2].fold.kind, SampleKind::OutOfSample);
        assert!(!wf.evaluated_only_in_sample);
    }

    #[test]
    fn broker_execution_is_denied() {
        let order = serde_json::json!({"symbol": "X", "side": "buy", "qty": 1});
        let err = execute_order_denied(&order).unwrap_err();
        assert!(matches!(err, TradingError::ResearchOnly(_)));
    }

    #[test]
    fn backtest_rejects_instrument_outside_universe() {
        let b = bars(&[10.0, 11.0, 12.0, 13.0, 14.0]);
        let (ds, dbars) = build_dataset("ds.u", "v1", cfg(), vec![], b).unwrap();
        let strat = sma_crossover("s", "other.eq", 2, 3, CostSpec::default());
        assert!(run(&strat, &ds, &dbars, BacktestConfig::default()).is_err());
    }

    #[test]
    fn csv_parser_roundtrips() {
        let text = "t_ms,o,h,l,c,v\n1000,10,11,9,10.5,500\n2000,10.5,12,10,11,600\n";
        let parsed = parse_bars_csv(text).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].close, 10.5);
        assert!(parsed.iter().all(|b| b.validate().is_ok()));
    }
}
