//! Market-wide research protocol for `strategies/psx-pre-breakout.toml`.
//!
//! This is intentionally pooled: one parameter set is selected on the same
//! liquid universe, then applied unchanged to every symbol in the next,
//! untouched time window. It does not fit a different story to every chart.
//!
//! ```text
//! cargo run --release --example pre_breakout_research
//! ```

use std::cmp::Ordering;
use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use psxtui::backtest::{Config, Strategy, engine};
use psxtui::cache::Store;
use psxtui::model::Bar;

const STRATEGY: &str = include_str!("../strategies/psx-pre-breakout.toml");
const BENCHMARK: &str = "KSE100";
const MIN_BARS: usize = 900;
const MIN_UNIVERSE_TURNOVER: f64 = 3_000_000.0;
const MIN_TRUE_RANGE: f64 = 0.90;
const MAX_STEP: f64 = 0.40;
const UNIVERSE_LIMIT: usize = 150;
const INITIAL_TRAIN: usize = 620;
const FOLDS: usize = 3;
const WARMUP: usize = 300;
const MIN_FIT_TRADES: usize = 10;

#[derive(Clone)]
struct Symbol {
    name: String,
    bars: Vec<Bar>,
    median_turnover: f64,
}

#[derive(Default, Clone)]
struct Metrics {
    symbols: usize,
    mean_return: f64,
    median_return: f64,
    profitable_pct: f64,
    mean_drawdown: f64,
    median_drawdown: f64,
    trades: usize,
    confirmed: usize,
    win_rate: f64,
    exposure: f64,
    rows: Vec<(String, f64, usize)>,
    trade_returns: Vec<f64>,
    confirmed_returns: Vec<f64>,
}

fn main() -> Result<()> {
    let store = Store::open_default()?;
    let strategy = Strategy::parse(STRATEGY)?;
    let benchmark = store.bars(BENCHMARK, None)?;
    if benchmark.len() < INITIAL_TRAIN + 100 {
        bail!("only {} benchmark bars; need more history", benchmark.len());
    }

    let universe = load_universe(&store, benchmark[200].ts)?;
    println!(
        "PSX Pre-Breakout research\n{} symbols | benchmark {} | {} to {} | 15bp/side costs\n",
        universe.len(),
        BENCHMARK,
        day(benchmark.first().unwrap().ts),
        day(benchmark.last().unwrap().ts),
    );

    let defaults = strategy.defaults();
    let full = evaluate(&strategy, &defaults, &universe, &benchmark, None, None);
    println!("FULL-SAMPLE DEFAULTS (descriptive, not validation)");
    print_metrics(&full);
    println!("  params {}\n", fmt_params(&defaults));
    if full.trades == 0 || std::env::args().any(|a| a == "--diagnose") {
        diagnose(&strategy, &defaults, &universe, &benchmark);
        if std::env::args().any(|a| a == "--diagnose") {
            return Ok(());
        }
    }

    let grid = research_grid(&strategy);
    println!(
        "WALK-FORWARD: {} pooled parameter sets, anchored training, {} OOS folds",
        grid.len(),
        FOLDS
    );

    let available = benchmark.len().saturating_sub(INITIAL_TRAIN);
    let test_len = available / FOLDS;
    let mut compounded = 1.0;
    let mut all_oos_rows = Vec::new();
    let mut all_trade_returns = Vec::new();
    let mut all_confirmed_returns = Vec::new();
    let mut all_oos_trades = 0usize;
    let mut weighted_wins = 0.0;

    for fold in 0..FOLDS {
        let test_start_i = INITIAL_TRAIN + fold * test_len;
        let test_end_i = if fold + 1 == FOLDS {
            benchmark.len()
        } else {
            test_start_i + test_len
        };
        let test_start = benchmark[test_start_i].ts;
        let test_end = benchmark[test_end_i - 1].ts;

        let (params, train_metrics, score) = fit(
            &strategy,
            &grid,
            &universe,
            &benchmark,
            benchmark.first().map(|b| b.ts),
            benchmark.get(test_start_i.saturating_sub(1)).map(|b| b.ts),
        );
        let test_metrics = evaluate(
            &strategy,
            &params,
            &universe,
            &benchmark,
            Some(test_start),
            Some(test_end),
        );

        compounded *= 1.0 + test_metrics.mean_return / 100.0;
        all_oos_trades += test_metrics.trades;
        weighted_wins += test_metrics.win_rate * test_metrics.trades as f64;
        all_oos_rows.extend(test_metrics.rows.clone());
        all_trade_returns.extend(test_metrics.trade_returns.iter().copied());
        all_confirmed_returns.extend(test_metrics.confirmed_returns.iter().copied());

        println!(
            "\nFOLD {}  train {}..{}  test {}..{}",
            fold + 1,
            day(benchmark[0].ts),
            day(benchmark[test_start_i - 1].ts),
            day(test_start),
            day(test_end),
        );
        println!("  chosen {}  score {:.3}", fmt_params(&params), score);
        println!(
            "  train mean {:>6.2}% median {:>6.2}% | {} trades",
            train_metrics.mean_return, train_metrics.median_return, train_metrics.trades
        );
        print_metrics(&test_metrics);
    }

    let compounded_pct = (compounded - 1.0) * 100.0;
    let aggregate_win = if all_oos_trades == 0 {
        0.0
    } else {
        weighted_wins / all_oos_trades as f64
    };
    let benchmark_oos =
        (benchmark.last().unwrap().close / benchmark[INITIAL_TRAIN].close - 1.0) * 100.0;
    println!(
        "\nWALK-FORWARD SUMMARY\n  compounded equal-weight OOS return {:>7.2}%\n  KSE100 over the OOS span             {:>7.2}%\n  OOS trades {} | trade-weighted win rate {:.1}%",
        compounded_pct, benchmark_oos, all_oos_trades, aggregate_win
    );
    println!(
        "  all trades avg {:+.2}% median {:+.2}% | confirmed avg {:+.2}% median {:+.2}% ({} trades)",
        mean(&all_trade_returns),
        median(all_trade_returns.clone()),
        mean(&all_confirmed_returns),
        median(all_confirmed_returns.clone()),
        all_confirmed_returns.len(),
    );
    print_extremes(&all_oos_rows);
    println!(
        "\nInterpretation: each symbol receives an equal static capital sleeve; unused sleeve capital stays in cash. Results include 10bp commission + 5bp slippage per side. Current listings only, so survivorship bias remains."
    );
    Ok(())
}

fn load_universe(store: &Store, tradable_from: i64) -> Result<Vec<Symbol>> {
    let mut out = Vec::new();
    let mut short = 0;
    let mut thin = 0;
    let mut dirty = 0;
    let mut range = 0;
    for info in store.symbols()? {
        if info.is_debt || info.is_etf {
            continue;
        }
        let bars = store.bars(&info.symbol, None).unwrap_or_default();
        if bars.len() < MIN_BARS {
            short += 1;
            continue;
        }
        let turnover = median(bars.iter().map(|b| b.close * b.volume).collect());
        if turnover < MIN_UNIVERSE_TURNOVER {
            thin += 1;
            continue;
        }
        if worst_step(&bars) > MAX_STEP {
            dirty += 1;
            continue;
        }
        let (known, total) = store.hl_coverage_since(&info.symbol, tradable_from)?;
        if total == 0 || known as f64 / (total as f64) < MIN_TRUE_RANGE {
            range += 1;
            continue;
        }
        out.push(Symbol {
            name: info.symbol,
            bars,
            median_turnover: turnover,
        });
    }
    out.sort_by(|a, b| {
        b.median_turnover
            .partial_cmp(&a.median_turnover)
            .unwrap_or(Ordering::Equal)
    });
    out.truncate(UNIVERSE_LIMIT);
    eprintln!(
        "universe exclusions: {short} short history, {thin} illiquid, {dirty} corporate-action gaps, {range} low true-range coverage"
    );
    Ok(out)
}

fn research_grid(strategy: &Strategy) -> Vec<HashMap<String, f64>> {
    let base = strategy.defaults();
    let axes: [(&str, &[f64]); 4] = [
        ("base_days", &[15.0, 20.0, 25.0]),
        ("proximity_pct", &[1.0, 2.0, 3.0]),
        ("base_range_pct", &[8.0, 10.0, 12.0]),
        ("rs_min_pct", &[0.0, 3.0, 6.0]),
    ];
    let mut grid = vec![base];
    for (name, values) in axes {
        let mut next = Vec::new();
        for p in grid {
            for value in values {
                let mut q = p.clone();
                q.insert(name.into(), *value);
                next.push(q);
            }
        }
        grid = next;
    }
    grid
}

fn fit(
    strategy: &Strategy,
    grid: &[HashMap<String, f64>],
    universe: &[Symbol],
    benchmark: &[Bar],
    start: Option<i64>,
    end: Option<i64>,
) -> (HashMap<String, f64>, Metrics, f64) {
    let mut best: Option<(HashMap<String, f64>, Metrics, f64)> = None;
    for (i, params) in grid.iter().enumerate() {
        let m = evaluate(strategy, params, universe, benchmark, start, end);
        // Breadth first: reward the typical sleeve, mildly reward total
        // return, and charge for drawdown. Sparse one-hit parameter sets are
        // rejected rather than allowed to win by luck.
        if m.trades < MIN_FIT_TRADES {
            continue;
        }
        let score = m.median_return + 0.25 * m.mean_return + 0.10 * m.mean_drawdown;
        if best.as_ref().is_none_or(|(_, _, old)| score > *old) {
            best = Some((params.clone(), m, score));
        }
        if (i + 1).is_multiple_of(20) {
            eprint!("\r  fitted {}/{}", i + 1, grid.len());
        }
    }
    eprintln!("\r  fitted {}/{}", grid.len(), grid.len());
    best.unwrap_or_else(|| {
        let p = strategy.defaults();
        let m = evaluate(strategy, &p, universe, benchmark, start, end);
        (p, m, f64::NEG_INFINITY)
    })
}

fn evaluate(
    strategy: &Strategy,
    params: &HashMap<String, f64>,
    universe: &[Symbol],
    benchmark: &[Bar],
    trade_start: Option<i64>,
    end: Option<i64>,
) -> Metrics {
    let config = Config::default();
    let mut returns = Vec::new();
    let mut drawdowns = Vec::new();
    let mut rows = Vec::new();
    let mut trades = 0usize;
    let mut wins = 0usize;
    let mut confirmed = 0usize;
    let mut exposure = 0.0;
    let mut trade_returns = Vec::new();
    let mut confirmed_returns = Vec::new();

    for symbol in universe {
        let end_i = end
            .and_then(|ts| symbol.bars.iter().rposition(|b| b.ts <= ts).map(|i| i + 1))
            .unwrap_or(symbol.bars.len());
        if end_i < 60 {
            continue;
        }
        let start_i = trade_start
            .and_then(|ts| symbol.bars.iter().position(|b| b.ts >= ts))
            .map(|i| i.saturating_sub(WARMUP))
            .unwrap_or(0);
        let bars = &symbol.bars[start_i..end_i];
        let Ok(report) = engine::run_with_benchmark_from(
            strategy,
            bars,
            Some(benchmark),
            params,
            &config,
            trade_start,
        ) else {
            continue;
        };
        returns.push(report.total_return_pct);
        drawdowns.push(report.max_drawdown_pct);
        trades += report.trade_count;
        wins += report.trades.iter().filter(|t| t.is_win()).count();
        confirmed += report.trades.iter().filter(|t| t.adds > 0).count();
        trade_returns.extend(report.trades.iter().map(|t| t.return_pct));
        confirmed_returns.extend(
            report
                .trades
                .iter()
                .filter(|t| t.adds > 0)
                .map(|t| t.return_pct),
        );
        exposure += report.exposure_pct;
        rows.push((
            symbol.name.clone(),
            report.total_return_pct,
            report.trade_count,
        ));
    }

    let symbols = returns.len();
    let profitable = returns.iter().filter(|r| **r > 0.0).count();
    Metrics {
        symbols,
        mean_return: mean(&returns),
        median_return: median(returns),
        profitable_pct: ratio(profitable, symbols),
        mean_drawdown: mean(&drawdowns),
        median_drawdown: median(drawdowns),
        trades,
        confirmed,
        win_rate: ratio(wins, trades),
        exposure: if symbols == 0 {
            0.0
        } else {
            exposure / symbols as f64
        },
        rows,
        trade_returns,
        confirmed_returns,
    }
}

fn print_metrics(m: &Metrics) {
    println!(
        "  equal-weight mean {:>7.2}% | median {:>7.2}% | profitable {:>5.1}%",
        m.mean_return, m.median_return, m.profitable_pct
    );
    println!(
        "  mean maxDD {:>7.2}% | median maxDD {:>7.2}% | exposure {:>5.1}%",
        m.mean_drawdown, m.median_drawdown, m.exposure
    );
    println!(
        "  {} symbols | {} trades | {:>5.1}% confirmed | win rate {:>5.1}%",
        m.symbols,
        m.trades,
        ratio(m.confirmed, m.trades),
        m.win_rate
    );
    if m.trades > 0 {
        println!(
            "  trade return avg {:+.2}% | median {:+.2}%",
            mean(&m.trade_returns),
            median(m.trade_returns.clone())
        );
    }
}

fn print_extremes(rows: &[(String, f64, usize)]) {
    let mut rows = rows.to_vec();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    let top = rows
        .iter()
        .take(5)
        .map(fmt_row)
        .collect::<Vec<_>>()
        .join(", ");
    let bottom = rows
        .iter()
        .rev()
        .take(5)
        .map(fmt_row)
        .collect::<Vec<_>>()
        .join(", ");
    println!("  best fold-symbols   {top}");
    println!("  worst fold-symbols  {bottom}");
}

fn diagnose(
    strategy: &Strategy,
    params: &HashMap<String, f64>,
    universe: &[Symbol],
    benchmark: &[Bar],
) {
    let names = [
        "mid_rising",
        "liquid",
        "near_pivot",
        "base_width",
        "atr_pct",
        "volume_dry",
        "relative_strength",
        "upper_half_close",
        "base_ready",
    ];
    let mut counts: HashMap<&str, usize> = names.iter().map(|n| (*n, 0)).collect();
    let mut defined: HashMap<&str, usize> = names.iter().map(|n| (*n, 0)).collect();
    let mut entries = 0usize;
    let mut filters = 0usize;
    let mut both = 0usize;
    let mut entry_trend = 0usize;
    let mut entry_trend_liquid = 0usize;
    let mut entry_trend_liquid_rs = 0usize;
    for symbol in universe {
        let Ok(signals) = strategy.signals_with_benchmark(&symbol.bars, Some(benchmark), params)
        else {
            continue;
        };
        for i in 300..symbol.bars.len() {
            for name in names {
                if let Some(value) = signals.series.get(name).and_then(|s| s[i]) {
                    *defined.get_mut(name).unwrap() += 1;
                    let pass = match name {
                        "base_width" => value <= params["base_range_pct"],
                        "atr_pct" => value <= params["atr_max_pct"],
                        "relative_strength" => value >= params["rs_min_pct"],
                        _ => value != 0.0,
                    };
                    if pass {
                        *counts.get_mut(name).unwrap() += 1;
                    }
                }
            }
            let entry = signals.entry[i].is_some_and(|v| v != 0.0);
            let filter = signals
                .filter
                .as_ref()
                .and_then(|s| s[i])
                .is_some_and(|v| v != 0.0);
            entries += usize::from(entry);
            filters += usize::from(filter);
            both += usize::from(entry && filter);
            let trend = signals.series["trend_ma"][i]
                .zip(signals.series["mid_ma"][i])
                .zip(signals.series["mid_rising"][i])
                .is_some_and(|((trend, mid), rising)| {
                    symbol.bars[i].close > trend && mid > trend && rising != 0.0
                });
            let liquid = signals.series["liquid"][i].is_some_and(|v| v != 0.0);
            let rs =
                signals.series["relative_strength"][i].is_some_and(|v| v >= params["rs_min_pct"]);
            entry_trend += usize::from(entry && trend);
            entry_trend_liquid += usize::from(entry && trend && liquid);
            entry_trend_liquid_rs += usize::from(entry && trend && liquid && rs);
        }
    }
    println!("GATE DIAGNOSTIC (bar observations after warm-up)");
    for name in names {
        println!(
            "  {:<20} {:>7}/{:<7} pass",
            name, counts[name], defined[name]
        );
    }
    println!("  entry composite      {entries:>7}");
    println!("  entry + trend        {entry_trend:>7}");
    println!("  + liquidity          {entry_trend_liquid:>7}");
    println!("  + relative strength  {entry_trend_liquid_rs:>7}");
    println!("  filter composite     {filters:>7}");
    println!("  entry + filter       {both:>7}\n");
}

fn fmt_row(row: &(String, f64, usize)) -> String {
    format!("{} {:+.1}%/{}t", row.0, row.1, row.2)
}

fn fmt_params(params: &HashMap<String, f64>) -> String {
    ["base_days", "proximity_pct", "base_range_pct", "rs_min_pct"]
        .iter()
        .filter_map(|key| params.get(*key).map(|v| format!("{key}={v:.2}")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn day(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .context("invalid timestamp")
        .map(|d| d.date_naive().to_string())
        .unwrap_or_else(|_| ts.to_string())
}

fn worst_step(bars: &[Bar]) -> f64 {
    bars.windows(2)
        .filter(|w| w[0].close > 0.0)
        .map(|w| ((w[1].close / w[0].close) - 1.0).abs())
        .fold(0.0, f64::max)
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64 * 100.0
    }
}
