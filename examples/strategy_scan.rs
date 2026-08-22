//! Scan one strategy across every cached symbol and summarise it.
//!
//! A single symbol cannot tell you whether a rule has an edge or whether it
//! happened to suit one chart. This runs the file over the whole cached
//! universe and reports the distribution, plus the true-OHLC coverage that
//! qualifies it.
//!
//! ```text
//! cargo run --release --example strategy_scan -- strategies/swing-checklist.toml
//! ```

use anyhow::Result;
use psxtui::backtest::{Config, Strategy, engine, optimize};
use psxtui::cache::Store;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "strategies/swing-checklist.toml".into());

    let store = Store::open_default()?;
    let s = Strategy::parse(&std::fs::read_to_string(&path)?)?;
    let params = s.defaults();
    let config = Config::default();

    let symbols: Vec<String> = store.symbols()?.into_iter().map(|s| s.symbol).collect();
    let rows = optimize::scan(
        &s,
        &params,
        &symbols,
        &config,
        optimize::Silent::Drop,
        |sym| store.bars(sym, None).ok(),
    );
    let summary = optimize::summarize(&rows);

    let traded: Vec<_> = rows.iter().filter(|r| r.trade_count > 0).collect();
    let trades: usize = rows.iter().map(|r| r.trade_count).sum();
    let win = traded.iter().map(|r| r.win_rate_pct).sum::<f64>() / traded.len().max(1) as f64;
    let mut dd: Vec<f64> = traded.iter().map(|r| r.max_drawdown_pct).collect();
    dd.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    println!("{}  ({})\n", s.name, path);
    println!("symbols scanned    {}", summary.symbols);
    println!("symbols that fired {}", traded.len());
    println!("total trades       {trades}");
    println!("median return      {:.1}%", summary.median_return_pct);
    println!(
        "profitable         {} / {}",
        summary.profitable, summary.symbols
    );
    println!(
        "beat buy & hold    {} / {}",
        summary.beat_buy_hold, summary.symbols
    );
    println!("avg win rate       {win:.0}%");
    println!(
        "median max DD      {:.1}%",
        dd.get(dd.len() / 2).copied().unwrap_or(0.0)
    );

    let mut active = rows.clone();
    active.sort_by_key(|r| std::cmp::Reverse(r.trade_count));
    println!(
        "\n{:<9} {:>7} {:>9} {:>9} {:>7}",
        "symbol", "trades", "ret%", "B&H%", "win%"
    );
    for r in active.iter().take(12) {
        println!(
            "{:<9} {:>7} {:>9.1} {:>9.1} {:>7.0}",
            r.symbol, r.trade_count, r.total_return_pct, r.buy_hold_return_pct, r.win_rate_pct
        );
    }

    // Holding periods on the busiest name, as a sanity check that the exit
    // rule is not closing trades on the bar after entry.
    if let Some(top) = active.first()
        && top.trade_count > 0
        && let Ok(bars) = store.bars(&top.symbol, None)
    {
        let rep = engine::run(&s, &bars, &params, &config)?;
        let holds: Vec<usize> = rep.trades.iter().map(|t| t.bars_held).collect();
        println!("\n{} holding periods (bars): {holds:?}", top.symbol);
        for c in rep.caveats() {
            println!("  ! {c}");
        }
    }

    Ok(())
}
