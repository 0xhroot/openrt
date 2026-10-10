//! Deterministic, causal backtest engine: signals computed from data up to
//! bar `i`, executed at bar `i+1` open with slippage and commission.
//! Results carry full lineage (dataset digest, strategy digest, config) and
//! are always marked simulated. Leakage detection audits both the dataset
//! and the strategy declaration; walk-forward splits expose in-sample vs
//! out-of-sample evaluation.

use serde::{Deserialize, Serialize};

use crate::dataset::ResearchDataset;
use crate::indicator::max_drawdown;
use crate::model::Bar;
use crate::strategy::{eval_condition, precompute, Condition, Strategy};
use crate::TradingError;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BacktestConfig {
    pub initial_cash: f64,
    pub execution_next_open: bool,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_cash: 100_000.0,
            execution_next_open: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeakKind {
    DeclaredDatasetLookahead,
    DeclaredStrategyLookahead,
    UnsortedBars,
    ShortHistory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leak {
    pub kind: LeakKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookAheadReport {
    pub leaks: Vec<Leak>,
    pub clean: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub entry_ms: i64,
    pub exit_ms: Option<i64>,
    pub qty: f64,
    pub entry_price: f64,
    pub exit_price: Option<f64>,
    pub pnl: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub total_return_pct: f64,
    pub annualized_vol_pct: f64,
    pub max_drawdown_pct: f64,
    pub trades: usize,
    pub closed_trades: usize,
    pub win_rate_pct: Option<f64>,
    pub exposure_pct: f64,
    pub turnover_pct: f64,
    pub final_equity: f64,
    pub sharpe: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestResult {
    pub id: String,
    pub strategy_id: String,
    pub strategy_digest: String,
    pub dataset_id: String,
    pub dataset_digest: String,
    pub config: BacktestConfig,
    pub metrics: Metrics,
    pub equity_curve: Vec<(i64, f64)>,
    pub trades: Vec<Trade>,
    pub look_ahead: LookAheadReport,
    pub assumptions: Vec<String>,
    pub simulated: bool,
    pub computed_at: String,
}

/// Audit the dataset + strategy for look-ahead risk. The strategy DSL is
/// causal by construction, so only declarations and ordering can leak.
pub fn detect_leakage(
    strategy: &Strategy,
    dataset: &ResearchDataset,
    bars: &[Bar],
) -> LookAheadReport {
    let mut leaks = Vec::new();
    if dataset.config.declared_lookahead {
        leaks.push(Leak {
            kind: LeakKind::DeclaredDatasetLookahead,
            detail: "dataset declares future-derived columns (declared_lookahead=true)".into(),
        });
    }
    if strategy.declared_lookahead {
        leaks.push(Leak {
            kind: LeakKind::DeclaredStrategyLookahead,
            detail: "strategy declares use of future-derived inputs (declared_lookahead=true)"
                .into(),
        });
    }
    if bars.windows(2).any(|w| w[0].t_ms >= w[1].t_ms) {
        leaks.push(Leak {
            kind: LeakKind::UnsortedBars,
            detail: "bars are not strictly ascending — ordering hazard".into(),
        });
    }
    if bars.len() < 3 {
        leaks.push(Leak {
            kind: LeakKind::ShortHistory,
            detail: "fewer than 3 bars — results would not be meaningful".into(),
        });
    }
    LookAheadReport {
        clean: leaks.is_empty(),
        leaks,
    }
}

fn condition_signals(strategy: &Strategy, bars: &[Bar]) -> (Vec<bool>, Vec<bool>) {
    let series = precompute(strategy, bars);
    let mut entry = Vec::with_capacity(bars.len());
    let mut exit = Vec::with_capacity(bars.len());
    for i in 0..bars.len() {
        entry.push(eval_condition(&strategy.entry, bars, &series, i));
        exit.push(eval_condition(&strategy.exit, bars, &series, i));
    }
    (entry, exit)
}

fn quantize(qty: f64, tick: f64) -> f64 {
    if tick <= 0.0 {
        qty
    } else {
        ((qty / tick).floor()) * tick
    }
}

/// Run a deterministic backtest. Same dataset + strategy + config ⇒ same
/// result (`id` is content-addressed).
pub fn run(
    strategy: &Strategy,
    dataset: &ResearchDataset,
    bars: &[Bar],
    config: BacktestConfig,
) -> Result<BacktestResult, TradingError> {
    if bars.len() < 3 {
        return Err(TradingError::Invalid("backtest requires >= 3 bars".into()));
    }
    if !strategy.universe.is_empty() && !strategy.universe.contains(&dataset.config.instrument) {
        return Err(TradingError::Invalid(format!(
            "dataset instrument `{}` not in strategy universe {:?}",
            dataset.config.instrument, strategy.universe
        )));
    }
    let look_ahead = detect_leakage(strategy, dataset, bars);
    let (entry, exit) = condition_signals(strategy, bars);

    let mut cash = config.initial_cash;
    let mut qty = 0.0f64;
    let mut trades: Vec<Trade> = Vec::new();
    let mut equity: Vec<(i64, f64)> = Vec::with_capacity(bars.len());
    let mut bars_in_position = 0usize;
    let mut open_trade: Option<Trade> = None;
    let mut gross_turnover = 0.0f64;
    for i in 0..bars.len() {
        let bar = bars[i];
        let want_entry = entry[i] && qty == 0.0;
        let want_exit = exit[i] && qty > 0.0;

        if i + 1 < bars.len() && (want_entry || want_exit) {
            let fill = bars[i + 1];
            let slippage = fill.open * strategy.costs.slippage_bps / 10_000.0;
            if want_entry {
                let budget = cash * strategy.risk.max_position_frac;
                let px = fill.open + slippage;
                if budget > 0.0 && px > 0.0 {
                    let target_qty = quantize(budget / px, 0.0);
                    if target_qty > 0.0 {
                        let cost = target_qty * px;
                        let commission = cost * strategy.costs.commission_bps / 10_000.0;
                        cash -= cost + commission;
                        gross_turnover += cost;
                        qty = target_qty;
                        open_trade = Some(Trade {
                            entry_ms: fill.t_ms,
                            exit_ms: None,
                            qty: target_qty,
                            entry_price: px,
                            exit_price: None,
                            pnl: None,
                        });
                    }
                }
            } else if want_exit && qty > 0.0 {
                let px = fill.open - slippage;
                let proceeds = qty * px;
                let commission = proceeds * strategy.costs.commission_bps / 10_000.0;
                cash += proceeds - commission;
                gross_turnover += proceeds;
                if let Some(mut t) = open_trade.take() {
                    t.exit_ms = Some(fill.t_ms);
                    t.exit_price = Some(px);
                    let costs = t.qty * t.entry_price * strategy.costs.commission_bps / 10_000.0
                        + t.qty * px * strategy.costs.commission_bps / 10_000.0
                        + t.qty * (t.entry_price * strategy.costs.slippage_bps / 10_000.0)
                        + t.qty * (px * strategy.costs.slippage_bps / 10_000.0);
                    t.pnl = Some(t.qty * (px - t.entry_price) - costs);
                    trades.push(t);
                }
                qty = 0.0;
            }
        }

        if qty > 0.0 {
            bars_in_position += 1;
        }
        equity.push((bar.t_ms, cash + qty * bar.close));
    }

    if let Some(mut t) = open_trade.take() {
        let last = *bars.last().expect("checked length");
        t.exit_ms = Some(last.t_ms);
        t.exit_price = Some(last.close);
        let costs = t.qty * t.entry_price * strategy.costs.commission_bps / 10_000.0
            + t.qty * last.close * strategy.costs.commission_bps / 10_000.0;
        t.pnl = Some(t.qty * (last.close - t.entry_price) - costs);
        trades.push(t);
    }

    let values: Vec<f64> = equity.iter().map(|(_, v)| *v).collect();
    let final_equity = *values.last().expect("equity curve non-empty");
    let total_return_pct = (final_equity / config.initial_cash - 1.0) * 100.0;
    let rets: Vec<f64> = values
        .windows(2)
        .map(|w| (w[1] - w[0]) / w[0])
        .filter(|r| r.is_finite())
        .collect();
    let mean_r = if rets.is_empty() {
        0.0
    } else {
        rets.iter().sum::<f64>() / rets.len() as f64
    };
    let var = if rets.len() < 2 {
        0.0
    } else {
        rets.iter()
            .map(|r| (r - mean_r) * (r - mean_r))
            .sum::<f64>()
            / rets.len() as f64
    };
    let vol_per_bar = var.sqrt();
    let bars_per_year: f64 = 252.0;
    let annualized_vol_pct = vol_per_bar * bars_per_year.sqrt() * 100.0;
    let sharpe = if vol_per_bar > 0.0 {
        Some((mean_r / vol_per_bar) * bars_per_year.sqrt())
    } else {
        None
    };
    let closed: Vec<&Trade> = trades.iter().filter(|t| t.exit_ms.is_some()).collect();
    let wins = closed
        .iter()
        .filter(|t| t.pnl.map_or(false, |p| p > 0.0))
        .count();
    let win_rate_pct = if closed.is_empty() {
        None
    } else {
        Some(wins as f64 / closed.len() as f64 * 100.0)
    };
    let metrics = Metrics {
        total_return_pct,
        annualized_vol_pct,
        max_drawdown_pct: max_drawdown(&values) * 100.0,
        trades: trades.len(),
        closed_trades: closed.len(),
        win_rate_pct,
        exposure_pct: bars_in_position as f64 / bars.len() as f64 * 100.0,
        turnover_pct: gross_turnover / config.initial_cash * 100.0,
        final_equity,
        sharpe,
    };

    let canonical = format!(
        "{}|{}|{:?}|{initial_cash}|{n}",
        strategy.digest(),
        dataset.digest,
        strategy.entry,
        initial_cash = config.initial_cash,
        n = bars.len()
    );
    let id = openrt_core::sha256_hex(canonical.as_bytes());
    Ok(BacktestResult {
        id,
        strategy_id: strategy.id.clone(),
        strategy_digest: strategy.digest(),
        dataset_id: dataset.id.clone(),
        dataset_digest: dataset.digest.clone(),
        config,
        metrics,
        equity_curve: equity,
        trades,
        look_ahead,
        assumptions: vec![
            "fills at next bar open with slippage and commission".into(),
            "no borrow, margin, dividends or corporate actions applied".into(),
            "results are SIMULATED research output, not forecasts".into(),
        ],
        simulated: true,
        computed_at: openrt_core::now().to_rfc3339(),
    })
}

// ---- walk-forward ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleKind {
    Train,
    Validation,
    OutOfSample,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fold {
    pub index: usize,
    pub kind: SampleKind,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bars: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FoldResult {
    pub fold: Fold,
    pub metrics: Metrics,
    pub look_ahead_clean: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WalkForwardParams {
    pub train_frac: f64,
    pub validation_frac: f64,
    pub initial_cash: f64,
}

impl Default for WalkForwardParams {
    fn default() -> Self {
        Self {
            train_frac: 0.6,
            validation_frac: 0.2,
            initial_cash: 100_000.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkForwardResult {
    pub dataset_id: String,
    pub strategy_id: String,
    pub folds: Vec<FoldResult>,
    /// True when no out-of-sample fold was evaluated — an honest warning
    /// that only in-sample results exist.
    pub evaluated_only_in_sample: bool,
    pub generated_at: String,
}

/// Deterministic contiguous folds: train / validation / out-of-sample.
pub fn split_folds(bars: &[Bar], params: WalkForwardParams) -> Result<Vec<Fold>, TradingError> {
    if !(0.0 < params.train_frac && params.train_frac < 1.0) {
        return Err(TradingError::Invalid("train_frac must be in (0,1)".into()));
    }
    if params.train_frac + params.validation_frac >= 1.0 {
        return Err(TradingError::Invalid(
            "train_frac + validation_frac must be < 1".into(),
        ));
    }
    if bars.len() < 10 {
        return Err(TradingError::Invalid(
            "walk-forward requires >= 10 bars".into(),
        ));
    }
    let n = bars.len();
    let train_end = ((n as f64 * params.train_frac) as usize).max(1);
    let val_end = (train_end + ((n as f64 * params.validation_frac) as usize)).min(n - 1);
    let kinds = [
        (0, train_end, SampleKind::Train),
        (train_end, val_end, SampleKind::Validation),
        (val_end, n, SampleKind::OutOfSample),
    ];
    Ok(kinds
        .iter()
        .enumerate()
        .map(|(index, &(from, to, kind))| Fold {
            index,
            kind,
            from_ms: bars[from].t_ms,
            to_ms: bars[to - 1].t_ms,
            bars: to - from,
        })
        .collect())
}

/// Run each fold independently over its slice of bars.
pub fn walk_forward(
    strategy: &Strategy,
    dataset: &ResearchDataset,
    bars: &[Bar],
    params: WalkForwardParams,
) -> Result<WalkForwardResult, TradingError> {
    let folds = split_folds(bars, params)?;
    let mut results = Vec::new();
    for fold in folds {
        let slice = &bars[fold_from(bars, &fold)..fold_to(bars, &fold)];
        let cfg = BacktestConfig {
            initial_cash: params.initial_cash,
            execution_next_open: true,
        };
        let r = run(strategy, dataset, slice, cfg)?;
        results.push(FoldResult {
            fold,
            metrics: r.metrics,
            look_ahead_clean: r.look_ahead.clean,
        });
    }
    let evaluated_only_in_sample = !results
        .iter()
        .any(|f| f.fold.kind == SampleKind::OutOfSample);
    Ok(WalkForwardResult {
        dataset_id: dataset.id.clone(),
        strategy_id: strategy.id.clone(),
        folds: results,
        evaluated_only_in_sample,
        generated_at: openrt_core::now().to_rfc3339(),
    })
}

fn fold_from(bars: &[Bar], fold: &Fold) -> usize {
    bars.iter()
        .position(|b| b.t_ms >= fold.from_ms)
        .unwrap_or(0)
}

fn fold_to(bars: &[Bar], fold: &Fold) -> usize {
    bars.iter()
        .position(|b| b.t_ms > fold.to_ms)
        .unwrap_or(bars.len())
}

/// Explicit denial point for any attempt to reach a broker. Kept as a
/// function so tests and runtime surfaces share one honest refusal.
pub fn execute_order_denied(order: &serde_json::Value) -> Result<(), TradingError> {
    Err(TradingError::ResearchOnly(format!(
        "broker execution is out of scope for OpenRT trading research (order ignored: {order})"
    )))
}

/// Convenience for strategy authors: a crossover strategy over one instrument.
pub fn sma_crossover(
    id: &str,
    instrument: &str,
    fast: usize,
    slow: usize,
    costs: crate::strategy::CostSpec,
) -> Strategy {
    use crate::model::Provenance;
    Strategy {
        id: id.to_string(),
        name: format!("SMA{fast}/SMA{slow} crossover"),
        universe: vec![instrument.to_string()],
        entry: Condition::Crossover {
            fast: crate::indicator::Indicator::Sma { period: fast },
            slow: crate::indicator::Indicator::Sma { period: slow },
        },
        exit: Condition::CrossUnder {
            fast: crate::indicator::Indicator::Sma { period: fast },
            slow: crate::indicator::Indicator::Sma { period: slow },
        },
        risk: Default::default(),
        costs,
        params: Default::default(),
        declared_lookahead: false,
        provenance: Provenance::derived(
            "trading.strategy.sma_crossover",
            openrt_core::now().to_rfc3339(),
        ),
        created_at: openrt_core::now().to_rfc3339(),
    }
}
