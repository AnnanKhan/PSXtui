//! Access layer for the PSX data portal.
//!
//! PSX publishes no API, so this module wraps the two JSON feeds it does expose
//! (`/symbols`, `/timeseries`) and scrapes the rest from server-rendered HTML.
//! Scraped markup is inherently fragile, so every parser degrades to missing
//! fields rather than hard failures — see the tests in each submodule for the
//! shapes they defend against.

pub mod client;
pub mod company;
pub mod market;
pub mod parse;
pub mod symbols;
pub mod timeseries;

pub use client::PsxClient;
pub use company::company;
pub use market::{HistoricalRow, historical, indices, market_watch};
pub use symbols::symbols;
pub use timeseries::{eod, intraday};
