//! Run every bundled strategy over real cached history and print the table.
//!
//! Synthetic sine waves prove the engine arithmetic; only real PSX data shows
//! whether a strategy fires at a sane rate, whether the costs bite, and
//! whether anything beats buy-and-hold. Reads the local cache and touches no
//! network, so it is safe to run offline — but it needs a populated database,
//! which means running the app at least once first.
//!
//! ```text
//! cargo run --example backtest_report -- OGDC
//! ```

use anyhow::Result;
use psxtui::backtest::{self, Config, Objective, Strategy, engine, optimize};
use psxtui::cache::Store;

fn main() -> Result<()> {
    let symbol = std::env::args().nth(1).unwrap_or_else(|| "OGDC".into());

    let store = Store::open_default()?;
    let bars = store.bars(&symbol, None)?;

    if bars.len() < 250 {
        eprintln!(
            "only {} bars cached for {symbol} — run the app once to backfill, or pick a more liquid scrip",
            bars.len()
        );
        return Ok(());
    }

    let first = bars.first().unwrap();
    let last = bars.last().unwrap();
    println!(
        "{symbol}: {} bars, {} to {}\n",
        bars.len(),
        psxtui::cache::trading_day(first.ts),
        psxtui::cache::trading_day(last.ts),
    );

    let config = Config::default();
    let buy_hold = (last.close - first.close) / first.close * 100.0;

    println!(
        "{:<24} {:>9} {:>8} {:>8} {:>7} {:>7} {:>6}",
        "strategy", "return%", "CAGR%", "maxDD%", "Sharpe", "trades", "win%"
    );
    println!("{}", "-".repeat(74));

    for (_file, body) in backtest::BUILTIN {
        let s = Strategy::parse(body)?;
        let r = engine::run(&s, &bars, &s.defaults(), &config)?;
        println!(
            "{:<24} {:>9.1} {:>8.1} {:>8.1} {:>7.2} {:>7} {:>6.0}",
            s.name,
            r.total_return_pct,
            r.cagr_pct,
            r.max_drawdown_pct,
            r.sharpe,
            r.trade_count,
            r.win_rate_pct,
        );
        for c in r.caveats() {
            println!("    ! {c}");
        }
    }

    println!("{}", "-".repeat(74));
    println!("{:<24} {:>9.1}   (buy and hold)", "—", buy_hold);

    // Walk-forward on one strategy, since that is the number that actually
    // means something.
    let s = Strategy::parse(backtest::BUILTIN[0].1)?;
    let wf = optimize::walk_forward(&s, &bars, &config, Objective::Sharpe, 3)?;
    println!(
        "\nwalk-forward ({}): in-sample {:.1}%, out-of-sample {:.1}%, efficiency {:.2}\n  {} folds — {}",
        s.name,
        wf.in_sample_return_pct,
        wf.out_of_sample_return_pct,
        wf.efficiency,
        wf.folds.len(),
        wf.verdict(),
    );

    Ok(())
}
