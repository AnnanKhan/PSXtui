//! Search for a strategy that survives data it was not fitted to.
//!
//! The Backtest screen answers "what did this rule do". This answers the
//! harder question — "is there any reason to think it will keep working" — and
//! it is deliberately hard to please, because everything about backtesting
//! flatters the person doing it.
//!
//! Four rules are built into the protocol rather than left to the operator:
//!
//! 1. **The holdout is untouched.** History is split chronologically: the
//!    oldest 70% is the training window, the newest 30% is never looked at
//!    while choosing anything. `--phase fit` cannot see it at all.
//! 2. **Parameters are fitted pooled, not per symbol.** One parameter set for
//!    the whole universe, chosen on the median symbol. Fitting each scrip its
//!    own numbers produces a beautiful table and no evidence.
//! 3. **The verdict is cross-sectional.** How many symbols beat buy-and-hold,
//!    not what the best chart did. One symbol proves nothing.
//! 4. **Dirty series are excluded.** A close that moves more than 40% between
//!    two sessions cannot have happened under PSX's ±10% circuit breaker: it
//!    is an unadjusted corporate action, and every return computed across it
//!    is fiction. Those symbols are dropped, and counted, rather than quietly
//!    averaged in.
//!
//! ```text
//! cargo run --release --example strategy_lab -- fit  strategies/candidates
//! cargo run --release --example strategy_lab -- test strategies/candidates/a.toml
//! ```

use std::collections::HashMap;

use anyhow::{Result, bail};
use psxtui::backtest::{Config, Strategy, engine, optimize};
use psxtui::cache::Store;
use psxtui::model::Bar;

/// Minimum sessions a symbol needs to appear in the universe at all.
const MIN_BARS: usize = 900;
/// Minimum median daily traded value, in rupees. Below this a fill is a
/// fiction: the backtest assumes you traded at the close, and on a scrip that
/// prints twice a week you did not.
const MIN_VALUE: f64 = 3_000_000.0;
/// A close move this large between sessions is a corporate action the feed did
/// not adjust for — PSX's breaker is ±10%.
const MAX_STEP: f64 = 0.40;
/// Share of history used for fitting. The rest is the holdout.
const TRAIN_FRACTION: f64 = 0.70;
/// Bars of run-up before measurement starts, in either window.
///
/// A 250-day trend filter says nothing for 250 days. Two mistakes are possible
/// here and this constant exists to avoid both. Slice the window cold and half
/// of it is unusable — every candidate shows one trade a symbol, a verdict
/// about the slice rather than the strategy. Hand the strategy a run-up but
/// measure buy-and-hold across it too, and the benchmark is credited with a
/// rally the strategy was structurally unable to trade: on this data that
/// alone read as a 78-point deficit.
///
/// So both windows carry a run-up, and *neither* the strategy nor the
/// benchmark is measured until it has passed. The holdout's run-up necessarily
/// overlaps training, which costs nothing: no trade inside it is counted.
const WARMUP: usize = 260;
/// A candidate needs at least this many trades per symbol on average before
/// its numbers mean anything.
const MIN_TRADES_PER_SYMBOL: f64 = 5.0;
/// And it has to beat buy-and-hold on at least this share of the universe.
const ACCEPT_BEAT_RATE: f64 = 0.55;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let phase = args.next().unwrap_or_else(|| "fit".into());
    let path = args
        .next()
        .unwrap_or_else(|| "strategies/candidates".into());

    let store = Store::open_default()?;
    let universe = load_universe(&store)?;
    eprintln!(
        "universe: {} symbols, {} sessions each at most",
        universe.len(),
        universe.iter().map(|s| s.bars.len()).max().unwrap_or(0)
    );

    let files = collect(&path)?;
    if files.is_empty() {
        bail!("no .toml candidates at {path}");
    }

    let mut results = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file)?;
        let strategy = match Strategy::parse(&text) {
            Ok(s) => s,
            Err(e) => {
                println!("{:<28} REJECTED  {e}", short(file));
                continue;
            }
        };

        match phase.as_str() {
            "fit" => {
                let (params, train) = fit(&strategy, &universe);
                println!("{:<28} {}", strategy.name, train.line());
                results.push((strategy.name.clone(), train, params, file.clone()));
            }
            "test" => {
                // Fit on train exactly as the search did, then read the
                // holdout once.
                let (params, train) = fit(&strategy, &universe);
                let holdout = evaluate(&strategy, &params, &universe, Window::Holdout);
                println!("\n{}", strategy.name);
                println!("  params   {}", fmt_params(&params));
                println!("  train    {}", train.line());
                println!("  HOLDOUT  {}", holdout.line());
                println!("  verdict  {}", verdict(&train, &holdout));
                results.push((strategy.name.clone(), holdout, params, file.clone()));
            }
            other => bail!("unknown phase {other} — use fit or test"),
        }
    }

    // Ranked by the only thing that matters: how much of the universe it beat
    // holding the same scrip.
    results.sort_by(|a, b| {
        b.1.beat_rate
            .partial_cmp(&a.1.beat_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    println!("\n--- ranked ---");
    for (name, m, _, file) in &results {
        println!(
            "{:>6.0}%  {:<28} {}",
            m.beat_rate * 100.0,
            name,
            short(file)
        );
    }
    Ok(())
}

// --- the universe ---------------------------------------------------------

struct Symbol {
    #[allow(dead_code)]
    name: String,
    bars: Vec<Bar>,
}

impl Symbol {
    /// The bars a window runs over, and the index measurement starts at.
    fn window(&self, w: Window) -> (&[Bar], usize) {
        let cut = (self.bars.len() as f64 * TRAIN_FRACTION) as usize;
        match w {
            Window::Train => (&self.bars[..cut], WARMUP.min(cut / 2)),
            Window::Holdout => {
                let start = cut.saturating_sub(WARMUP);
                (&self.bars[start..], cut - start)
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Window {
    Train,
    Holdout,
}

fn load_universe(store: &Store) -> Result<Vec<Symbol>> {
    let mut out = Vec::new();
    let mut dropped_thin = 0;
    let mut dropped_dirty = 0;

    for info in store.symbols()? {
        // Debt instruments and ETFs are not what these rules are about.
        if info.is_debt || info.is_etf {
            continue;
        }
        let bars = store.bars(&info.symbol, None).unwrap_or_default();
        if bars.len() < MIN_BARS {
            continue;
        }
        if median_value(&bars) < MIN_VALUE {
            dropped_thin += 1;
            continue;
        }
        if let Some(step) = worst_step(&bars) {
            dropped_dirty += 1;
            if dropped_dirty <= 5 {
                eprintln!(
                    "  excluded {:<8} unadjusted corporate action: {:.0}% in one session",
                    info.symbol,
                    step * 100.0
                );
            }
            continue;
        }
        out.push(Symbol {
            name: info.symbol,
            bars,
        });
    }
    eprintln!("  excluded {dropped_thin} illiquid, {dropped_dirty} with broken series");
    Ok(out)
}

fn median_value(bars: &[Bar]) -> f64 {
    let mut v: Vec<f64> = bars.iter().map(|b| b.close * b.volume).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// The largest single-session close move, if it is beyond what a session can
/// do.
fn worst_step(bars: &[Bar]) -> Option<f64> {
    let mut worst = 0.0f64;
    for w in bars.windows(2) {
        if w[0].close > 0.0 {
            let step = ((w[1].close - w[0].close) / w[0].close).abs();
            worst = worst.max(step);
        }
    }
    (worst > MAX_STEP).then_some(worst)
}

// --- fitting and scoring --------------------------------------------------

/// Choose one parameter set for the whole universe, on the training window.
fn fit(strategy: &Strategy, universe: &[Symbol]) -> (HashMap<String, f64>, Metrics) {
    let grid = optimize::grid(strategy);

    let mut best: Option<(f64, HashMap<String, f64>, Metrics)> = None;
    for params in grid {
        let m = evaluate(strategy, &params, universe, Window::Train);
        // Rank by beat rate, with median excess as the tie-break: a rule that
        // works on more of the market is worth more than one that works
        // spectacularly on a few.
        let score = m.beat_rate * 100.0 + m.median_excess.clamp(-50.0, 50.0) / 100.0;
        if m.trades_per_symbol < MIN_TRADES_PER_SYMBOL {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _, _)| score > *b) {
            best = Some((score, params, m));
        }
    }

    match best {
        Some((_, params, m)) => (params, m),
        // Nothing in the grid traded enough to judge; report the defaults so
        // the candidate still appears in the table rather than vanishing.
        None => {
            let params = strategy.defaults();
            let m = evaluate(strategy, &params, universe, Window::Train);
            (params, m)
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Metrics {
    #[allow(dead_code)]
    symbols: usize,
    /// Share of symbols where the strategy beat holding that same symbol.
    beat_rate: f64,
    median_return: f64,
    median_excess: f64,
    median_drawdown: f64,
    buy_hold_drawdown: f64,
    trades_per_symbol: f64,
    exposure: f64,
}

impl Metrics {
    fn line(&self) -> String {
        format!(
            "beat {:>3.0}%  median {:>7.1}%  excess {:>7.1}%  maxDD {:>6.1}% (b&h {:>6.1}%)  {:>4.1} trades/sym  {:>3.0}% invested",
            self.beat_rate * 100.0,
            self.median_return,
            self.median_excess,
            self.median_drawdown,
            self.buy_hold_drawdown,
            self.trades_per_symbol,
            self.exposure
        )
    }
}

fn evaluate(
    strategy: &Strategy,
    params: &HashMap<String, f64>,
    universe: &[Symbol],
    window: Window,
) -> Metrics {
    let config = Config::default();
    let mut returns = Vec::new();
    let mut excess = Vec::new();
    let mut drawdowns = Vec::new();
    let mut bh_drawdowns = Vec::new();
    let mut trades = 0usize;
    let mut exposure = Vec::new();
    let mut beat = 0usize;

    for s in universe {
        let (bars, from) = s.window(window);
        if from >= bars.len() {
            continue;
        }
        let Ok(r) = engine::run(strategy, bars, params, &config) else {
            continue;
        };
        // Measured from the end of the run-up, for both sides. The report's
        // own totals run from the first bar, which would credit buy-and-hold
        // with a stretch no rule reading a 250-day average could have traded.
        let (Some(&open_equity), Some(&close_equity)) = (r.equity.get(from), r.equity.last())
        else {
            continue;
        };
        if open_equity <= 0.0 || bars[from].close <= 0.0 {
            continue;
        }
        let strat = (close_equity / open_equity - 1.0) * 100.0;
        let bh = (bars[bars.len() - 1].close / bars[from].close - 1.0) * 100.0;
        let since = bars[from].ts;

        returns.push(strat);
        excess.push(strat - bh);
        drawdowns.push(drawdown(&r.equity[from..]));
        bh_drawdowns.push(buy_hold_drawdown(&bars[from..]));
        exposure.push(r.exposure_pct);
        trades += r.trades.iter().filter(|t| t.entry_ts >= since).count();
        if strat > bh {
            beat += 1;
        }
    }

    let n = returns.len().max(1);
    Metrics {
        symbols: returns.len(),
        beat_rate: beat as f64 / n as f64,
        median_return: median(&mut returns),
        median_excess: median(&mut excess),
        median_drawdown: median(&mut drawdowns),
        buy_hold_drawdown: median(&mut bh_drawdowns),
        trades_per_symbol: trades as f64 / n as f64,
        exposure: median(&mut exposure),
    }
}

/// Worst peak-to-trough fall of an equity curve, as a negative percentage.
fn drawdown(equity: &[f64]) -> f64 {
    let mut peak = f64::MIN;
    let mut worst = 0.0f64;
    for v in equity {
        peak = peak.max(*v);
        if peak > 0.0 {
            worst = worst.min((v - peak) / peak * 100.0);
        }
    }
    worst
}

fn buy_hold_drawdown(bars: &[Bar]) -> f64 {
    let mut peak = f64::MIN;
    let mut worst = 0.0f64;
    for b in bars {
        peak = peak.max(b.close);
        if peak > 0.0 {
            worst = worst.min((b.close - peak) / peak * 100.0);
        }
    }
    worst
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

/// The pre-registered accept test, applied to the holdout.
fn verdict(train: &Metrics, holdout: &Metrics) -> String {
    let mut failures = Vec::new();
    if holdout.beat_rate < ACCEPT_BEAT_RATE {
        failures.push(format!(
            "beats only {:.0}% of the universe (need {:.0}%)",
            holdout.beat_rate * 100.0,
            ACCEPT_BEAT_RATE * 100.0
        ));
    }
    if holdout.trades_per_symbol < MIN_TRADES_PER_SYMBOL {
        failures.push(format!(
            "{:.1} trades per symbol is too thin to judge",
            holdout.trades_per_symbol
        ));
    }
    if holdout.median_drawdown < holdout.buy_hold_drawdown {
        failures.push(format!(
            "drawdown {:.0}% is worse than holding ({:.0}%)",
            holdout.median_drawdown, holdout.buy_hold_drawdown
        ));
    }
    if train.median_excess > 0.0 && holdout.median_excess < 0.0 {
        failures.push("the edge changed sign out of sample".into());
    }
    if failures.is_empty() {
        "PASSES".into()
    } else {
        format!("fails: {}", failures.join("; "))
    }
}

// --- odds and ends --------------------------------------------------------

fn collect(path: &str) -> Result<Vec<String>> {
    let p = std::path::Path::new(path);
    if p.is_file() {
        return Ok(vec![path.to_string()]);
    }
    let mut out: Vec<String> = std::fs::read_dir(p)?
        .filter_map(|e| e.ok())
        .map(|e| e.path().display().to_string())
        .filter(|f| f.ends_with(".toml"))
        .collect();
    out.sort();
    Ok(out)
}

fn short(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

fn fmt_params(p: &HashMap<String, f64>) -> String {
    let mut keys: Vec<&String> = p.keys().collect();
    keys.sort();
    keys.iter()
        .map(|k| format!("{k}={}", p[*k]))
        .collect::<Vec<_>>()
        .join(" ")
}
