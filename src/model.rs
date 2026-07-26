//! Core domain types shared across the data, analysis and UI layers.

use serde::{Deserialize, Serialize};

/// A single OHLCV bar. `ts` is a Unix timestamp in seconds (UTC).
///
/// PSX's EOD feed only carries open/close/volume, so for bars sourced from it
/// `high`/`low` are backfilled from the daily `POST /historical` snapshot when
/// available, and otherwise fall back to `max/min(open, close)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

impl Bar {
    /// Typical price — `(high + low + close) / 3`. Used by VWAP and CCI.
    pub fn typical(&self) -> f64 {
        (self.high + self.low + self.close) / 3.0
    }

    /// True range relative to an optional previous bar.
    pub fn true_range(&self, prev: Option<&Bar>) -> f64 {
        match prev {
            Some(p) => (self.high - self.low)
                .max((self.high - p.close).abs())
                .max((self.low - p.close).abs()),
            None => self.high - self.low,
        }
    }
}

/// One intraday trade tick from `/timeseries/int/<SYM>`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tick {
    pub ts: i64,
    pub price: f64,
    pub volume: f64,
}

/// A row of the market-watch table: the current session's state for one symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub symbol: String,
    pub sector: String,
    /// Indices this scrip is listed in (ALLSHR, KSE100, ...).
    pub indices: Vec<String>,
    /// Last day close price — the reference for today's change.
    pub ldcp: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub current: f64,
    pub change: f64,
    pub change_pct: f64,
    pub volume: f64,
}

impl Quote {
    /// Traded value proxy — volume x price. PSX doesn't publish value directly
    /// on market-watch, so this is used for "most active by value" ranking.
    pub fn turnover(&self) -> f64 {
        self.volume * self.current
    }
}

/// An entry from `/symbols` — the master list of listed instruments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub symbol: String,
    pub name: String,
    #[serde(rename = "sectorName")]
    pub sector_name: String,
    #[serde(rename = "isETF")]
    pub is_etf: bool,
    #[serde(rename = "isDebt")]
    pub is_debt: bool,
}

/// A market index reading from the index ticker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub value: f64,
    pub change: f64,
    pub change_pct: f64,
}

/// Company drill-down data scraped from `/company/<SYM>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Company {
    pub symbol: String,
    pub name: String,
    pub sector: String,
    pub business_description: String,
    pub key_people: Vec<(String, String)>,
    pub address: String,
    pub website: String,
    pub registrar: String,
    pub auditor: String,
    pub fiscal_year_end: String,

    // Equity profile
    /// Market capitalisation in thousands of PKR, as published.
    pub market_cap_000: Option<f64>,
    pub shares: Option<f64>,
    pub free_float: Option<f64>,
    pub free_float_pct: Option<f64>,

    // Quote-tab extras not present on market-watch
    pub week52_low: Option<f64>,
    pub week52_high: Option<f64>,
    pub circuit_low: Option<f64>,
    pub circuit_high: Option<f64>,
    pub pe_ratio: Option<f64>,
    pub change_1y_pct: Option<f64>,
    pub change_ytd_pct: Option<f64>,

    pub financials_annual: Vec<FinancialPeriod>,
    pub financials_quarterly: Vec<FinancialPeriod>,
    pub ratios: Vec<RatioPeriod>,
    pub announcements: Vec<Announcement>,
}

/// One column of the financials table (a fiscal year or quarter).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FinancialPeriod {
    /// e.g. "2025" or "Q1 2026".
    pub period: String,
    /// Row label varies by sector ("Mark-up Earned" for banks, "Sales" for
    /// industrials), so the raw label is preserved alongside the value.
    pub rows: Vec<(String, f64)>,
}

impl FinancialPeriod {
    pub fn get(&self, label: &str) -> Option<f64> {
        self.rows
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(label))
            .map(|(_, v)| *v)
    }

    pub fn eps(&self) -> Option<f64> {
        self.get("EPS")
    }
}

/// One column of the ratios table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RatioPeriod {
    pub period: String,
    pub rows: Vec<(String, f64)>,
}

/// A company announcement / filing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Announcement {
    pub date: String,
    pub title: String,
    pub category: AnnouncementKind,
    pub pdf_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnouncementKind {
    FinancialResults,
    BoardMeeting,
    Other,
}

impl AnnouncementKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::FinancialResults => "Financial Results",
            Self::BoardMeeting => "Board Meeting",
            Self::Other => "Other",
        }
    }
}
