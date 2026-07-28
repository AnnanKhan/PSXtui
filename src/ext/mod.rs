//! External market context: the world PSX trades inside.
//!
//! A Pakistani equity book is a leveraged bet on things that are not priced in
//! Karachi. Refinery and OMC margins track Brent; spinners' input cost is the
//! ICE cotton contract; every importer's cost base is the rupee; cement and
//! shipping earnings ride freight rates. None of that is on the PSX portal, so
//! this module fetches it from public, unauthenticated sources:
//!
//! - **Commodities and FX** — Yahoo Finance's chart API
//!   (`query1.finance.yahoo.com/v8/finance/chart/{SYMBOL}`), daily bars.
//! - **News** — the Business Recorder and Dawn RSS feeds.
//! - **The policy rate** — scraped from the State Bank of Pakistan homepage,
//!   which is what the risk-adjusted statistics should be measured against.
//!
//! Every parser degrades rather than fails: a missing field, a null entry in a
//! quote array, or an unknown symbol yields fewer rows, never a panic. Results
//! are cached by [`crate::cache`] so the Macro screen opens instantly offline.
//!
//! Honesty note: the Baltic Dry Index itself has no free quote endpoint. The
//! dry-bulk row is the **BDRY ETF**, a freight *proxy*, and is labelled as such
//! everywhere it appears.

pub mod client;
pub mod macros;
pub mod news;
pub mod quotes;

pub use client::ExtClient;
pub use macros::{MacroRates, fetch_rates, parse_policy_rate};
pub use news::{FEEDS, Headline, fetch_headlines, parse_rss};
pub use quotes::{CATALOG, Group, MacroSeries, MacroSpec, fetch_series, parse_chart, psx_link};
