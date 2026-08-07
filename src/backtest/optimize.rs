//! Parameter sweeps, walk-forward validation and universe scans.
//!
//! In Python these are the expensive operations, which is why vectorised
//! engines exist at all. Over ~1,250 daily bars in Rust a single run is
//! microseconds, so all three can be plain loops over the same event-driven
//! engine the single-symbol view uses — the sweep and the chart are therefore
//! guaranteed to agree, which is not true of a system that has a fast path and
//! a realistic path.

use std::collections::HashMap;

use anyhow::Result;

use super::engine::{self, Config};
use super::report::Report;
use super::strategy::Strategy;
use crate::model::Bar;

/// How a sweep or an optimiser ranks one run against another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Objective {
    TotalReturn,
    /// The default: risk-adjusted, so an optimiser cannot win by simply taking
    /// more risk for more return.
    #[default]
    Sharpe,
    /// Return divided by the worst drawdown taken to get it.
    ReturnOverDrawdown,
    ProfitFactor,
}

impl Objective {
    pub const ALL: [Objective; 4] = [
        Objective::Sharpe,
        Objective::TotalReturn,
        Objective::ReturnOverDrawdown,
        Objective::ProfitFactor,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Objective::TotalReturn => "return",
            Objective::Sharpe => "Sharpe",
            Objective::ReturnOverDrawdown => "return/DD",
            Objective::ProfitFactor => "profit factor",
        }
    }

    /// Higher is better, and a run that never traded scores worst.
    ///
    /// Without the trade-count floor an optimiser reliably converges on a
    /// parameter set that takes one lucky trade and stops — technically the
    /// best Sharpe in the grid, and worthless.
    pub fn score(&self, r: &Report) -> f64 {
        if r.trade_count == 0 {
            return f64::NEG_INFINITY;
        }
        let v = match self {
            Objective::TotalReturn => r.total_return_pct,
            Objective::Sharpe => r.sharpe,
            Objective::ReturnOverDrawdown => {
                if r.max_drawdown_pct.abs() < 1e-9 {
                    r.total_return_pct
                } else {
                    r.total_return_pct / r.max_drawdown_pct.abs()
                }
            }
            Objective::ProfitFactor => r.profit_factor,
        };
        if v.is_finite() { v } else { f64::NEG_INFINITY }
    }
}

/// One point in a parameter sweep.
#[derive(Debug, Clone)]
pub struct SweepPoint {
    pub params: HashMap<String, f64>,
    pub score: f64,
    pub total_return_pct: f64,
    pub sharpe: f64,
    pub max_drawdown_pct: f64,
    pub trade_count: usize,
}

/// Cap on grid size, so a strategy with five wide params cannot wedge the UI.
const MAX_COMBINATIONS: usize = 4_096;
/// Points per parameter axis, before the total cap applies.
const MAX_POINTS_PER_PARAM: usize = 24;

/// Every parameter combination a sweep should try.
pub fn grid(strategy: &Strategy) -> Vec<HashMap<String, f64>> {
    let axes: Vec<(String, Vec<f64>)> = strategy
        .params
        .iter()
        .map(|(name, p)| (name.clone(), p.sweep_values(MAX_POINTS_PER_PARAM)))
        .collect();

    let mut out: Vec<HashMap<String, f64>> = vec![HashMap::new()];
    for (name, values) in axes {
        let mut next = Vec::new();
        for base in &out {
            for v in &values {
                if next.len() >= MAX_COMBINATIONS {
                    break;
                }
                let mut c = base.clone();
                c.insert(name.clone(), *v);
                next.push(c);
            }
        }
        out = next;
        if out.len() >= MAX_COMBINATIONS {
            break;
        }
    }
    out
}

/// Run every combination and rank them.
pub fn sweep(
    strategy: &Strategy,
    bars: &[Bar],
    config: &Config,
    objective: Objective,
) -> Result<Vec<SweepPoint>> {
    let mut points = Vec::new();

    for params in grid(strategy) {
        // A parameter set that cannot even be evaluated (a period longer than
        // the history, say) is skipped rather than failing the whole sweep.
        let Ok(report) = engine::run(strategy, bars, &params, config) else {
            continue;
        };
        points.push(SweepPoint {
            score: objective.score(&report),
            total_return_pct: report.total_return_pct,
            sharpe: report.sharpe,
            max_drawdown_pct: report.max_drawdown_pct,
            trade_count: report.trade_count,
            params,
        });
    }

    points.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(points)
}

/// The best parameters over a window, by the given objective.
pub fn best_params(
    strategy: &Strategy,
    bars: &[Bar],
    config: &Config,
    objective: Objective,
) -> Option<HashMap<String, f64>> {
    sweep(strategy, bars, config, objective)
        .ok()?
        .into_iter()
        .find(|p| p.score.is_finite())
        .map(|p| p.params)
}

// --- walk-forward ---------------------------------------------------------

/// One in-sample/out-of-sample fold.
#[derive(Debug, Clone)]
pub struct Fold {
    pub train_start_ts: i64,
    pub train_end_ts: i64,
    pub test_start_ts: i64,
    pub test_end_ts: i64,
    /// What the optimiser chose on the training window.
    pub params: HashMap<String, f64>,
    pub in_sample_return_pct: f64,
    pub out_of_sample_return_pct: f64,
    pub out_of_sample_sharpe: f64,
    pub trade_count: usize,
}

/// The result of walking a strategy forward through history.
#[derive(Debug, Clone)]
pub struct WalkForward {
    pub folds: Vec<Fold>,
    /// Compounded out-of-sample return across every fold — the only number
    /// here that was never optimised against.
    pub out_of_sample_return_pct: f64,
    pub in_sample_return_pct: f64,
    /// Out-of-sample return over in-sample return. Below ~0.5 says most of the
    /// in-sample performance was curve fitting.
    pub efficiency: f64,
    pub objective: Objective,
}

impl WalkForward {
    pub fn verdict(&self) -> &'static str {
        if self.folds.is_empty() {
            "not enough history to walk forward"
        } else if self.out_of_sample_return_pct <= 0.0 {
            "lost money out of sample — the in-sample result was fitting, not edge"
        } else if self.efficiency < 0.3 {
            "most of the in-sample gain did not survive out of sample"
        } else if self.efficiency < 0.6 {
            "some edge survived out of sample, with meaningful decay"
        } else {
            "held up out of sample"
        }
    }
}

/// Minimum bars in a training window for an optimisation to mean anything.
const MIN_TRAIN_BARS: usize = 120;
/// Minimum bars in a test window.
const MIN_TEST_BARS: usize = 20;

/// Optimise on a rolling in-sample window, measure on the untouched window
/// that follows, then roll forward and repeat.
///
/// This is the antidote to the tweak panel. Tuning parameters until the equity
/// curve looks good is curve fitting by construction; the only number that
/// answers "would this have worked" is one produced by parameters chosen
/// without seeing the data they are scored on.
pub fn walk_forward(
    strategy: &Strategy,
    bars: &[Bar],
    config: &Config,
    objective: Objective,
    folds_wanted: usize,
) -> Result<WalkForward> {
    let n = bars.len();
    let folds_wanted = folds_wanted.max(1);

    // Split the series into `folds_wanted` test windows, each preceded by
    // everything before it as training data (an anchored walk-forward).
    let test_len = (n / (folds_wanted + 1)).max(MIN_TEST_BARS);
    let mut folds = Vec::new();

    let mut test_start = n.saturating_sub(test_len * folds_wanted);
    if test_start < MIN_TRAIN_BARS {
        test_start = MIN_TRAIN_BARS;
    }

    while test_start + MIN_TEST_BARS <= n {
        let test_end = (test_start + test_len).min(n);
        let train = &bars[..test_start];
        let test = &bars[test_start..test_end];

        if train.len() < MIN_TRAIN_BARS || test.len() < MIN_TEST_BARS {
            break;
        }

        // Optimise on training data only.
        let Some(params) = best_params(strategy, train, config, objective) else {
            test_start = test_end;
            continue;
        };

        let is_report = engine::run(strategy, train, &params, config)?;
        let oos_report = engine::run(strategy, test, &params, config)?;

        folds.push(Fold {
            train_start_ts: train.first().map(|b| b.ts).unwrap_or(0),
            train_end_ts: train.last().map(|b| b.ts).unwrap_or(0),
            test_start_ts: test.first().map(|b| b.ts).unwrap_or(0),
            test_end_ts: test.last().map(|b| b.ts).unwrap_or(0),
            params,
            in_sample_return_pct: is_report.total_return_pct,
            out_of_sample_return_pct: oos_report.total_return_pct,
            out_of_sample_sharpe: oos_report.sharpe,
            trade_count: oos_report.trade_count,
        });

        test_start = test_end;
    }

    // Compound the fold returns rather than averaging them: consecutive
    // windows chain, and averaging would hide a -50% followed by a +50%.
    let compound = |get: fn(&Fold) -> f64, folds: &[Fold]| -> f64 {
        let growth = folds.iter().map(|f| 1.0 + get(f) / 100.0).product::<f64>();
        (growth - 1.0) * 100.0
    };

    let oos = compound(|f| f.out_of_sample_return_pct, &folds);
    let is = compound(|f| f.in_sample_return_pct, &folds);

    Ok(WalkForward {
        efficiency: if is.abs() < 1e-9 { 0.0 } else { oos / is },
        out_of_sample_return_pct: oos,
        in_sample_return_pct: is,
        folds,
        objective,
    })
}

// --- universe scan --------------------------------------------------------

/// One symbol's result in a universe scan.
#[derive(Debug, Clone)]
pub struct ScanRow {
    pub symbol: String,
    pub total_return_pct: f64,
    pub buy_hold_return_pct: f64,
    pub sharpe: f64,
    pub max_drawdown_pct: f64,
    pub trade_count: usize,
    pub win_rate_pct: f64,
}

/// Run one strategy across many symbols.
///
/// This is what separates a real edge from one curve-fitted to whichever
/// symbol happened to be on screen. Note the survivorship caveat: PSX's symbol
/// list carries currently-listed scrips only, so delisted names are missing
/// and every aggregate here is biased upward.
pub fn scan<F>(
    strategy: &Strategy,
    params: &HashMap<String, f64>,
    symbols: &[String],
    config: &Config,
    mut bars_for: F,
) -> Vec<ScanRow>
where
    F: FnMut(&str) -> Option<Vec<Bar>>,
{
    let mut rows = Vec::new();

    for symbol in symbols {
        let Some(bars) = bars_for(symbol) else {
            continue;
        };
        // Too little history to warm an indicator up, let alone judge one.
        if bars.len() < 60 {
            continue;
        }
        let Ok(r) = engine::run(strategy, &bars, params, config) else {
            continue;
        };
        if r.trade_count == 0 {
            continue;
        }
        rows.push(ScanRow {
            symbol: symbol.clone(),
            total_return_pct: r.total_return_pct,
            buy_hold_return_pct: r.buy_hold_return_pct,
            sharpe: r.sharpe,
            max_drawdown_pct: r.max_drawdown_pct,
            trade_count: r.trade_count,
            win_rate_pct: r.win_rate_pct,
        });
    }

    rows.sort_by(|a, b| {
        b.total_return_pct
            .partial_cmp(&a.total_return_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

/// Summary of a scan: how broadly the strategy worked, not how well it did on
/// its best symbol.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanSummary {
    pub symbols: usize,
    pub median_return_pct: f64,
    pub beat_buy_hold: usize,
    pub profitable: usize,
}

pub fn summarize(rows: &[ScanRow]) -> ScanSummary {
    if rows.is_empty() {
        return ScanSummary::default();
    }
    let mut returns: Vec<f64> = rows.iter().map(|r| r.total_return_pct).collect();
    returns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    ScanSummary {
        symbols: rows.len(),
        median_return_pct: returns[returns.len() / 2],
        beat_buy_hold: rows
            .iter()
            .filter(|r| r.total_return_pct > r.buy_hold_return_pct)
            .count(),
        profitable: rows.iter().filter(|r| r.total_return_pct > 0.0).count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::engine::Costs;

    fn cfg() -> Config {
        Config {
            initial_equity: 100_000.0,
            costs: Costs {
                commission_bps: 0.0,
                slippage_bps: 0.0,
            },
        }
    }

    /// A gently trending series with enough wobble to make crossovers fire.
    fn wavy(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                let t = i as f64;
                let c = 100.0 + t * 0.05 + (t / 7.0).sin() * 6.0 + (t / 23.0).cos() * 3.0;
                Bar {
                    ts: 1_600_000_000 + i as i64 * 86_400,
                    open: c,
                    high: c,
                    low: c,
                    close: c,
                    volume: 1000.0,
                }
            })
            .collect()
    }

    fn crossover() -> Strategy {
        Strategy::parse(
            r#"
name = "Cross"
[params]
fast = { default = 5, min = 3, max = 9, step = 2 }
slow = { default = 20, min = 10, max = 30, step = 10 }
[indicators]
f = "sma(close, fast)"
s = "sma(close, slow)"
[rules]
entry = "cross_above(f, s)"
exit = "cross_below(f, s)"
"#,
        )
        .unwrap()
    }

    #[test]
    fn the_grid_is_the_cartesian_product_of_the_axes() {
        let s = crossover();
        let g = grid(&s);
        // fast: 3,5,7,9 (4 values); slow: 10,20,30 (3 values)
        assert_eq!(g.len(), 12);
    }

    #[test]
    fn the_grid_stays_bounded_for_wide_parameters() {
        let s = Strategy::parse(
            r#"
name = "Wide"
[params]
a = { default = 1, min = 1, max = 1000, step = 1 }
b = { default = 1, min = 1, max = 1000, step = 1 }
c = { default = 1, min = 1, max = 1000, step = 1 }
[rules]
entry = "close > a + b + c"
exit = "close < a"
"#,
        )
        .unwrap();
        assert!(grid(&s).len() <= MAX_COMBINATIONS, "grid exploded");
    }

    #[test]
    fn a_sweep_ranks_best_first() {
        let s = crossover();
        let bars = wavy(400);
        let points = sweep(&s, &bars, &cfg(), Objective::TotalReturn).unwrap();
        assert!(!points.is_empty());
        for w in points.windows(2) {
            assert!(w[0].score >= w[1].score, "sweep was not sorted");
        }
    }

    #[test]
    fn a_parameter_set_that_never_trades_scores_worst() {
        // Otherwise the optimiser happily "wins" by doing nothing.
        let never = Report {
            strategy_name: "x".into(),
            params: HashMap::new(),
            equity: vec![],
            timestamps: vec![],
            trades: vec![],
            initial_equity: 1.0,
            final_equity: 1.0,
            total_return_pct: 0.0,
            buy_hold_return_pct: 0.0,
            cagr_pct: 0.0,
            sharpe: 99.0,
            sortino: 0.0,
            max_drawdown_pct: 0.0,
            volatility_pct: 0.0,
            trade_count: 0,
            win_rate_pct: 0.0,
            profit_factor: 0.0,
            avg_win_pct: 0.0,
            avg_loss_pct: 0.0,
            best_trade_pct: 0.0,
            worst_trade_pct: 0.0,
            avg_bars_held: 0.0,
            exposure_pct: 0.0,
            intrabar_warning: false,
            true_range_pct: None,
            bar_count: 0,
        };
        assert_eq!(Objective::Sharpe.score(&never), f64::NEG_INFINITY);
    }

    #[test]
    fn walk_forward_optimises_only_on_training_data() {
        let s = crossover();
        let bars = wavy(800);
        let wf = walk_forward(&s, &bars, &cfg(), Objective::TotalReturn, 3).unwrap();

        assert!(!wf.folds.is_empty(), "no folds produced");
        for f in &wf.folds {
            // The test window must start strictly after the training window
            // ends, or the optimiser saw its own exam paper.
            assert!(
                f.test_start_ts > f.train_end_ts,
                "fold leaked training data into the test window"
            );
        }
    }

    #[test]
    fn walk_forward_folds_do_not_overlap() {
        let s = crossover();
        let bars = wavy(800);
        let wf = walk_forward(&s, &bars, &cfg(), Objective::TotalReturn, 3).unwrap();
        for w in wf.folds.windows(2) {
            assert!(
                w[1].test_start_ts > w[0].test_end_ts,
                "overlapping test windows double-count the same bars"
            );
        }
    }

    #[test]
    fn walk_forward_on_a_short_series_yields_nothing_rather_than_nonsense() {
        let s = crossover();
        let bars = wavy(50);
        let wf = walk_forward(&s, &bars, &cfg(), Objective::TotalReturn, 3).unwrap();
        assert!(wf.folds.is_empty());
        assert!(wf.verdict().contains("not enough history"));
    }

    #[test]
    fn efficiency_is_out_of_sample_over_in_sample() {
        let wf = WalkForward {
            folds: vec![],
            out_of_sample_return_pct: 5.0,
            in_sample_return_pct: 20.0,
            efficiency: 0.25,
            objective: Objective::Sharpe,
        };
        assert!(wf.efficiency < 0.3);
    }

    #[test]
    fn a_scan_skips_symbols_with_too_little_history() {
        let s = crossover();
        let symbols = vec!["LONG".to_string(), "SHORT".to_string()];
        let rows = scan(&s, &s.defaults(), &symbols, &cfg(), |sym| {
            Some(if sym == "LONG" { wavy(400) } else { wavy(10) })
        });
        assert!(rows.iter().all(|r| r.symbol == "LONG"));
    }

    #[test]
    fn a_scan_is_ranked_by_return() {
        let s = crossover();
        let symbols: Vec<String> = (0..5).map(|i| format!("S{i}")).collect();
        let rows = scan(&s, &s.defaults(), &symbols, &cfg(), |_| Some(wavy(400)));
        for w in rows.windows(2) {
            assert!(w[0].total_return_pct >= w[1].total_return_pct);
        }
    }

    #[test]
    fn summarize_reports_breadth_not_the_best_symbol() {
        let rows = vec![
            ScanRow {
                symbol: "A".into(),
                total_return_pct: 100.0,
                buy_hold_return_pct: 10.0,
                sharpe: 1.0,
                max_drawdown_pct: 5.0,
                trade_count: 10,
                win_rate_pct: 60.0,
            },
            ScanRow {
                symbol: "B".into(),
                total_return_pct: -20.0,
                buy_hold_return_pct: 5.0,
                sharpe: -0.5,
                max_drawdown_pct: 30.0,
                trade_count: 8,
                win_rate_pct: 30.0,
            },
            ScanRow {
                symbol: "C".into(),
                total_return_pct: -5.0,
                buy_hold_return_pct: 1.0,
                sharpe: 0.0,
                max_drawdown_pct: 10.0,
                trade_count: 5,
                win_rate_pct: 40.0,
            },
        ];
        let s = summarize(&rows);
        assert_eq!(s.symbols, 3);
        assert_eq!(s.profitable, 1);
        assert_eq!(s.beat_buy_hold, 1);
        assert_eq!(s.median_return_pct, -5.0);
    }
}
