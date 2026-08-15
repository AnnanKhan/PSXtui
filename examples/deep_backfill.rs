//! Run the true-OHLC deep backfill headlessly.
//!
//! The Backtest screen triggers this with `O`; this is the same job without a
//! TUI, for when it is more convenient to leave it running in a terminal.
//! Every cached session still missing true high/low is fetched from the daily
//! `/historical` snapshot, one request per session, rate-limited.
//!
//! ```text
//! cargo run --release --example deep_backfill
//! ```

use std::sync::Arc;

use anyhow::Result;
use psxtui::app::DataEvent;
use psxtui::cache::Store;
use psxtui::data::Worker;
use psxtui::psx::PsxClient;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    psxtui::psx::client::install_crypto_provider();

    let store = Arc::new(Store::open_default()?);
    let client = Arc::new(PsxClient::new()?);
    let (tx, mut rx) = mpsc::unbounded_channel::<DataEvent>();

    let pending = store.days_missing_true_range()?.len();
    println!(
        "{pending} sessions missing true intraday range; ~{} min",
        pending / 60
    );

    let worker = Worker::new(client, store.clone(), tx);
    let job = tokio::spawn(worker.run_deep_backfill());

    let mut filled = 0usize;
    while let Some(ev) = rx.recv().await {
        match ev {
            DataEvent::Backfill(p) => {
                if let Some(rows) = p.rows {
                    filled += 1;
                    if filled.is_multiple_of(25) || p.done == p.total {
                        println!("  {}/{}  {}  ({rows} rows)", p.done, p.total, p.day);
                    }
                }
            }
            DataEvent::BackfillDone => {
                println!("backfill finished");
                break;
            }
            DataEvent::Error(e) => println!("! {e}"),
            DataEvent::Status(s) => println!("{s}"),
            _ => {}
        }
    }

    let _ = job.await;
    println!(
        "{} sessions still missing",
        store.days_missing_true_range()?.len()
    );
    Ok(())
}
