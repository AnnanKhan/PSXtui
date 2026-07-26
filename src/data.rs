//! Background data worker.
//!
//! Owns every network call and every cache write, so the render loop never
//! blocks. The guiding rule is *cache first, network second*: each request
//! emits whatever is already stored before going to PSX, so screens paint
//! instantly and then sharpen when the fetch lands.

use std::sync::Arc;

use chrono::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::app::{BENCHMARK, DataEvent, DataRequest};
use crate::cache::Store;
use crate::psx::{self, PsxClient};

/// How long a cached company profile stays fresh. Filings appear a few times a
/// quarter, so a day is generous without being stale.
const COMPANY_TTL_DAYS: i64 = 1;

/// Calendar days of true-OHLC history to backfill on first run. ~3 months of
/// trading days, enough to warm up every indicator the app computes.
pub const BACKFILL_DAYS: i64 = 120;

/// Cheap to clone — the client and store are shared, so a clone runs against
/// the same rate limiter and the same cache.
#[derive(Clone)]
pub struct Worker {
    client: Arc<PsxClient>,
    store: Arc<Store>,
    tx: UnboundedSender<DataEvent>,
}

impl Worker {
    pub fn new(client: Arc<PsxClient>, store: Arc<Store>, tx: UnboundedSender<DataEvent>) -> Self {
        Self { client, store, tx }
    }

    fn emit(&self, ev: DataEvent) {
        let _ = self.tx.send(ev);
    }

    fn status(&self, msg: impl Into<String>) {
        self.emit(DataEvent::Status(msg.into()));
    }

    /// Report a failure without tearing down the session — PSX being briefly
    /// unreachable should degrade to cached data, not exit.
    fn fail(&self, context: &str, e: anyhow::Error) {
        self.emit(DataEvent::Error(format!("{context}: {e}")));
    }

    /// Serve cached state immediately so the first frame is never empty.
    pub fn prime_from_cache(&self) {
        if let Ok(syms) = self.store.symbols()
            && !syms.is_empty()
        {
            self.emit(DataEvent::Symbols(syms));
        }
        if let Ok(quotes) = self.store.quotes()
            && !quotes.is_empty()
        {
            self.emit(DataEvent::Quotes(quotes));
            self.status("Showing cached quotes — refreshing…");
        }
        if let Ok(bars) = self.store.bars(BENCHMARK, None)
            && !bars.is_empty()
        {
            self.emit(DataEvent::Bars {
                symbol: BENCHMARK.into(),
                bars,
            });
        }
    }

    /// Main loop: drain interactive requests until the UI drops the sender.
    ///
    /// Backfill is deliberately *not* served here. It is a long sequence of
    /// one-request-per-day fetches, and running it on this queue would park
    /// every chart and profile load behind it for the best part of a minute.
    /// [`Worker::run_backfill`] carries it on its own task instead; both share
    /// the client's rate limiter, so the two interleave politely.
    pub async fn run(self, mut rx: UnboundedReceiver<DataRequest>) {
        while let Some(req) = rx.recv().await {
            match req {
                DataRequest::RefreshMarket => self.refresh_market().await,
                DataRequest::LoadSymbol(sym) => self.load_symbol(&sym).await,
                DataRequest::LoadCompany(sym) => self.load_company(&sym).await,
                DataRequest::Backfill(days) => {
                    // Hand off rather than block this queue. The spawned task
                    // signals its own completion, so don't double-count it.
                    tokio::spawn(self.clone().run_backfill(days));
                    continue;
                }
            }
            self.emit(DataEvent::Done);
        }
    }

    /// Run the OHLC backfill on a dedicated task.
    pub async fn run_backfill(self, days: i64) {
        self.backfill(days).await;
        self.emit(DataEvent::Done);
    }

    async fn refresh_market(&self) {
        match psx::symbols(&self.client).await {
            Ok(syms) => {
                let _ = self.store.put_symbols(&syms);
                self.emit(DataEvent::Symbols(syms));
            }
            Err(e) => self.fail("symbol list", e),
        }

        match psx::market_watch(&self.client).await {
            Ok(quotes) => {
                let _ = self.store.put_quotes(&quotes);
                self.emit(DataEvent::Quotes(quotes));
            }
            Err(e) => self.fail("market watch", e),
        }

        match psx::indices(&self.client).await {
            Ok(idx) => self.emit(DataEvent::Indices(idx)),
            Err(e) => self.fail("indices", e),
        }

        // The benchmark drives beta and relative performance everywhere.
        self.load_series(BENCHMARK).await;
    }

    async fn load_symbol(&self, symbol: &str) {
        // Cached bars first — the chart paints before the network answers.
        if let Ok(bars) = self.store.bars(symbol, None)
            && !bars.is_empty()
        {
            self.emit(DataEvent::Bars {
                symbol: symbol.into(),
                bars,
            });
        }

        self.load_series(symbol).await;

        match psx::intraday(&self.client, symbol).await {
            Ok(ticks) => self.emit(DataEvent::Ticks {
                symbol: symbol.into(),
                ticks,
            }),
            Err(e) => self.fail(&format!("{symbol} intraday"), e),
        }
    }

    /// Fetch daily history, merge it into the cache, and emit the merged
    /// result — which may carry true high/low from earlier backfills.
    async fn load_series(&self, symbol: &str) {
        match psx::eod(&self.client, symbol).await {
            Ok(bars) => {
                let _ = self.store.put_eod_bars(symbol, &bars);
                let merged = self.store.bars(symbol, None).unwrap_or(bars);
                self.emit(DataEvent::Bars {
                    symbol: symbol.into(),
                    bars: merged,
                });
            }
            Err(e) => self.fail(&format!("{symbol} history"), e),
        }
    }

    async fn load_company(&self, symbol: &str) {
        if let Ok(Some(c)) = self.store.company(symbol, Duration::days(COMPANY_TTL_DAYS)) {
            self.emit(DataEvent::Company(Box::new(c)));
            return;
        }
        match psx::company(&self.client, symbol).await {
            Ok(c) => {
                let _ = self.store.put_company(&c);
                self.emit(DataEvent::Company(Box::new(c)));
            }
            Err(e) => self.fail(&format!("{symbol} profile"), e),
        }
    }

    /// Ingest whole-market OHLC snapshots for any trading day not yet cached.
    ///
    /// One request covers every symbol for a date, so this is the cheapest way
    /// to obtain true intraday ranges — but it is still one request per day, so
    /// progress is reported and days are marked done (including holidays) to
    /// keep subsequent launches quiet.
    async fn backfill(&self, days: i64) {
        let Ok(missing) = self.store.missing_historical_days(days) else {
            return;
        };
        if missing.is_empty() {
            return;
        }

        let total = missing.len();
        for (i, day) in missing.iter().enumerate() {
            match psx::historical(&self.client, day).await {
                Ok(rows) if rows.is_empty() => {
                    // A holiday: record it so it is never refetched.
                    let _ = self.store.mark_historical_empty(day);
                }
                Ok(rows) => {
                    let _ = self.store.put_historical(day, &rows);
                    self.status(format!(
                        "Backfilling OHLC {}/{} — {day} ({} symbols)",
                        i + 1,
                        total,
                        rows.len()
                    ));
                }
                Err(e) => {
                    self.fail(&format!("backfill {day}"), e);
                    // Network trouble won't fix itself mid-loop; stop and let
                    // the next launch resume where this left off.
                    return;
                }
            }
        }

        let count = self.store.bar_count().unwrap_or(0);
        self.status(format!("Backfill complete — {count} daily bars cached"));
    }
}
