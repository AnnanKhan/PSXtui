//! Live smoke test against the real PSX portal.
//!
//! Unit tests only prove the parsers handle markup we wrote ourselves. This
//! exercises them against what PSX actually serves today, which is the only
//! way to catch a layout change.
//!
//! Run with: `cargo run --example live_smoke`

use anyhow::Result;
use psxtui::psx::{self, PsxClient};

#[tokio::main]
async fn main() -> Result<()> {
    let client = PsxClient::new()?;

    println!("== /symbols ==");
    let syms = psx::symbols(&client).await?;
    let equities = syms.iter().filter(|s| !s.is_debt && !s.is_etf).count();
    println!(
        "{} instruments ({} equities, {} ETFs, {} debt)",
        syms.len(),
        equities,
        syms.iter().filter(|s| s.is_etf).count(),
        syms.iter().filter(|s| s.is_debt).count()
    );
    println!("  sample: {:?}", syms.iter().find(|s| s.symbol == "HBL"));

    println!("\n== /market-watch ==");
    let quotes = psx::market_watch(&client).await?;
    println!("{} quotes", quotes.len());
    let mut movers = quotes.clone();
    movers.sort_by(|a, b| b.change_pct.partial_cmp(&a.change_pct).unwrap());
    for q in movers.iter().take(3) {
        println!(
            "  GAIN {:<10} {:>10.2} {:>+8.2}% vol {:>14.0} [{}]",
            q.symbol, q.current, q.change_pct, q.volume, q.sector
        );
    }
    for q in movers.iter().rev().take(3) {
        println!(
            "  LOSS {:<10} {:>10.2} {:>+8.2}% vol {:>14.0}",
            q.symbol, q.current, q.change_pct, q.volume
        );
    }
    let sane = quotes
        .iter()
        .filter(|q| q.current > 0.0 && q.high >= q.low)
        .count();
    println!("  {sane}/{} rows have sane OHLC", quotes.len());

    println!("\n== /indices ==");
    let idx = psx::indices(&client).await?;
    println!("{} indices", idx.len());
    for i in idx.iter().take(5) {
        println!(
            "  {:<12} {:>14.2} {:>+10.2} ({:>+6.2}%)",
            i.name, i.value, i.change, i.change_pct
        );
    }

    println!("\n== /timeseries/eod/HBL ==");
    let bars = psx::eod(&client, "HBL").await?;
    println!("{} daily bars", bars.len());
    if let (Some(first), Some(last)) = (bars.first(), bars.last()) {
        println!("  oldest ts={} close={:.2}", first.ts, first.close);
        println!(
            "  newest ts={} close={:.2} vol={:.0}",
            last.ts, last.close, last.volume
        );
    }

    println!("\n== /timeseries/int/HBL ==");
    let ticks = psx::intraday(&client, "HBL").await?;
    println!("{} intraday ticks", ticks.len());
    if let Some(t) = ticks.last() {
        println!(
            "  last tick ts={} price={:.2} vol={:.0}",
            t.ts, t.price, t.volume
        );
    }

    println!("\n== POST /historical ==");
    // Walk back to the most recent trading day.
    for back in 0..7 {
        let date = (chrono::Utc::now() - chrono::Duration::days(back))
            .format("%Y-%m-%d")
            .to_string();
        let rows = psx::historical(&client, &date).await?;
        println!("  {date}: {} rows", rows.len());
        if !rows.is_empty() {
            let hbl = rows.iter().find(|r| r.symbol == "HBL");
            println!("    HBL: {hbl:?}");
            break;
        }
    }

    println!("\n== /company/HBL ==");
    let c = psx::company(&client, "HBL").await?;
    println!("  name           : {}", c.name);
    println!("  sector         : {}", c.sector);
    println!("  P/E            : {:?}", c.pe_ratio);
    println!(
        "  52w range      : {:?} .. {:?}",
        c.week52_low, c.week52_high
    );
    println!(
        "  circuit        : {:?} .. {:?}",
        c.circuit_low, c.circuit_high
    );
    println!(
        "  1Y / YTD       : {:?} / {:?}",
        c.change_1y_pct, c.change_ytd_pct
    );
    println!("  mkt cap (000s) : {:?}", c.market_cap_000);
    println!("  shares         : {:?}", c.shares);
    println!(
        "  free float     : {:?} ({:?}%)",
        c.free_float, c.free_float_pct
    );
    println!("  website        : {}", c.website);
    println!("  auditor        : {}", c.auditor);
    println!("  FY end         : {}", c.fiscal_year_end);
    println!("  key people     : {:?}", c.key_people);
    println!("  description    : {:.120}...", c.business_description);
    println!("  annual periods : {:?}", periods(&c.financials_annual));
    if let Some(y) = c.financials_annual.first() {
        println!("    {} rows: {:?}", y.period, y.rows);
    }
    println!("  qtr periods    : {:?}", periods(&c.financials_quarterly));
    println!("  ratio periods  : {}", c.ratios.len());
    if let Some(r) = c.ratios.first() {
        println!("    {} rows: {:?}", r.period, r.rows);
    }
    println!("  announcements  : {}", c.announcements.len());
    for a in c.announcements.iter().take(4) {
        println!(
            "    [{}] {} — {:.70} pdf={}",
            a.category.label(),
            a.date,
            a.title,
            a.pdf_url.is_some()
        );
    }

    Ok(())
}

fn periods(v: &[psxtui::model::FinancialPeriod]) -> Vec<&str> {
    v.iter().map(|p| p.period.as_str()).collect()
}
