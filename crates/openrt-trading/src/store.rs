//! Instrument and strategy persistence in `trading.instruments` and
//! `trading.strategies`.

use openrt_knowledge::safe_key;
use openrt_persistence::PersistenceStore;

use crate::model::Instrument;
use crate::strategy::Strategy;
use crate::{TradingError, INSTRUMENTS_NS, STRATEGIES_NS};

fn inst_key(id: &str) -> String {
    format!("instrument.{}", safe_key(id))
}

fn strat_key(id: &str) -> String {
    format!("strategy.{}", safe_key(id))
}

pub async fn put_instrument(
    store: &dyn PersistenceStore,
    inst: &Instrument,
) -> Result<(), TradingError> {
    if inst.id.trim().is_empty() {
        return Err(TradingError::Invalid(
            "instrument id must not be empty".into(),
        ));
    }
    let v = serde_json::to_value(inst)
        .map_err(|e| TradingError::Serde(format!("serialize instrument: {e}")))?;
    store
        .put_record(INSTRUMENTS_NS, &inst_key(&inst.id), &v)
        .await?;
    Ok(())
}

pub async fn get_instrument(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Option<Instrument>, TradingError> {
    match store.get_record(INSTRUMENTS_NS, &inst_key(id)).await? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| TradingError::Corrupt(inst_key(id), e.to_string())),
    }
}

pub async fn list_instruments(
    store: &dyn PersistenceStore,
) -> Result<Vec<Instrument>, TradingError> {
    let mut out: Vec<Instrument> = Vec::new();
    for (key, value) in store.list_records(INSTRUMENTS_NS).await? {
        if !key.starts_with("instrument.") {
            continue;
        }
        out.push(
            serde_json::from_value(value).map_err(|e| TradingError::Corrupt(key, e.to_string()))?,
        );
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

pub async fn put_strategy(
    store: &dyn PersistenceStore,
    strategy: &Strategy,
) -> Result<(), TradingError> {
    if strategy.id.trim().is_empty() {
        return Err(TradingError::Invalid(
            "strategy id must not be empty".into(),
        ));
    }
    let v = serde_json::to_value(strategy)
        .map_err(|e| TradingError::Serde(format!("serialize strategy: {e}")))?;
    store
        .put_record(STRATEGIES_NS, &strat_key(&strategy.id), &v)
        .await?;
    Ok(())
}

pub async fn get_strategy(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Option<Strategy>, TradingError> {
    match store.get_record(STRATEGIES_NS, &strat_key(id)).await? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| TradingError::Corrupt(strat_key(id), e.to_string())),
    }
}

pub async fn list_strategies(store: &dyn PersistenceStore) -> Result<Vec<Strategy>, TradingError> {
    let mut out: Vec<Strategy> = Vec::new();
    for (key, value) in store.list_records(STRATEGIES_NS).await? {
        if !key.starts_with("strategy.") {
            continue;
        }
        out.push(
            serde_json::from_value(value).map_err(|e| TradingError::Corrupt(key, e.to_string()))?,
        );
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Bar, InstrumentKind, Provenance};
    use crate::{
        build_dataset, get_bars, get_dataset, list_datasets, put_dataset, run, verify_dataset,
        BacktestConfig, DatasetConfig, ResearchDataset,
    };
    use openrt_persistence::MemoryStore;

    fn inst() -> Instrument {
        Instrument {
            id: "test.eq".into(),
            symbol: "TEST".into(),
            venue: "SIM".into(),
            kind: InstrumentKind::Equity,
            currency: "USD".into(),
            tick_size: 0.01,
            provenance: Provenance::derived("unit-test", "2026-01-01T00:00:00Z"),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn instrument_roundtrip() {
        let store = MemoryStore::new();
        put_instrument(&store, &inst()).await.unwrap();
        let back = get_instrument(&store, "test.eq").await.unwrap().unwrap();
        assert_eq!(back.symbol, "TEST");
        assert_eq!(list_instruments(&store).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn dataset_store_roundtrip_and_verify() {
        let store = MemoryStore::new();
        let bars: Vec<Bar> = (0..20)
            .map(|i| Bar {
                t_ms: 1_700_000_000_000 + i * 86_400_000,
                open: 100.0 + i as f64,
                high: 101.0 + i as f64,
                low: 99.0 + i as f64,
                close: 100.5 + i as f64,
                volume: 1000.0,
            })
            .collect();
        let config = DatasetConfig {
            instrument: "test.eq".into(),
            from_ms: 0,
            to_ms: i64::MAX,
            filters: Default::default(),
            params: Default::default(),
            source: "unit".into(),
            transform: "as_recorded".into(),
            declared_lookahead: false,
        };
        let (ds, bars) = build_dataset("ds1", "v1", config, vec!["src".into()], bars).unwrap();
        put_dataset(&store, &ds, &bars).await.unwrap();
        assert!(verify_dataset(&store, "ds1").await.unwrap());
        let loaded: ResearchDataset = get_dataset(&store, "ds1").await.unwrap().unwrap();
        assert_eq!(loaded.digest, ds.digest);
        assert_eq!(get_bars(&store, "ds1").await.unwrap().unwrap().len(), 20);
        assert_eq!(list_datasets(&store).await.unwrap().len(), 1);

        let strat = crate::sma_crossover("s1", "test.eq", 3, 8, Default::default());
        put_strategy(&store, &strat).await.unwrap();
        assert_eq!(list_strategies(&store).await.unwrap().len(), 1);
        let bars2 = get_bars(&store, "ds1").await.unwrap().unwrap();
        let r = run(&strat, &ds, &bars2, BacktestConfig::default()).unwrap();
        assert!(r.simulated);
    }
}

/// Persist a backtest/walk-forward result under `trading.backtests`.
pub async fn put_backtest<T: serde::Serialize>(
    store: &dyn PersistenceStore,
    id: &str,
    value: &T,
) -> Result<(), TradingError> {
    let v = serde_json::to_value(value)
        .map_err(|e| TradingError::Serde(format!("serialize backtest: {e}")))?;
    store
        .put_record(
            crate::BACKTESTS_NS,
            &format!("backtest.{}", safe_key(id)),
            &v,
        )
        .await?;
    Ok(())
}

/// All persisted backtest records (payloads as stored).
pub async fn list_backtests(
    store: &dyn PersistenceStore,
) -> Result<Vec<serde_json::Value>, TradingError> {
    let mut out = Vec::new();
    for (key, value) in store.list_records(crate::BACKTESTS_NS).await? {
        if key.starts_with("backtest.") {
            out.push(value);
        }
    }
    Ok(out)
}
