//! Commodity, FX and global-index series from the Yahoo Finance chart API.
//!
//! `GET https://query1.finance.yahoo.com/v8/finance/chart/{SYMBOL}?range=1y&interval=1d`
//! returns, unauthenticated, a `chart.result[0]` object holding a `meta` block,
//! a `timestamp` array and parallel `indicators.quote[0].{open,high,low,close,
//! volume}` arrays.
//!
//! Two shapes have to be survived. Entries in the quote arrays are `null` for
//! sessions with no print — a holiday on one exchange but not another — and
//! must be skipped rather than read as zero, because a zero close would show up
//! as a −100% day and poison every correlation. And an unknown symbol returns
//! `chart.result: null` with `chart.error` set, which is a successful HTTP 200,
//! so it has to be detected in the body.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::client::{ExtClient, encode_segment};
use crate::model::Bar;

const CHART_BASE: &str = "https://query1.finance.yahoo.com/v8/finance/chart";

/// A year of daily bars: long enough for a meaningful correlation against a
/// PSX scrip, short enough to stay one modest response per series.
const RANGE: &str = "1y";
const INTERVAL: &str = "1d";

/// How a series is grouped on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Energy,
    Metals,
    Agri,
    Freight,
    Currency,
    Equity,
    Crypto,
}

impl Group {
    /// Section order on the Macro screen — roughly by how directly each moves
    /// the PSX index.
    pub const ALL: [Group; 7] = [
        Group::Energy,
        Group::Metals,
        Group::Agri,
        Group::Freight,
        Group::Currency,
        Group::Equity,
        Group::Crypto,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Group::Energy => "Energy",
            Group::Metals => "Metals",
            Group::Agri => "Agriculture",
            Group::Freight => "Freight",
            Group::Currency => "Currency",
            Group::Equity => "Equities",
            Group::Crypto => "Crypto",
        }
    }
}

/// A series worth watching, and what it is called on Yahoo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacroSpec {
    /// Stable identifier used for cache keys and the PSX association lookup.
    pub key: &'static str,
    /// Display name. For proxies this states plainly that it is a proxy.
    pub name: &'static str,
    pub symbol: &'static str,
    pub unit: &'static str,
    pub group: Group,
}

/// The watched set.
///
/// Every entry is one HTTP request behind a rate limiter, so each has to earn
/// its row by mapping onto a real PSX sector — the associations in [`psx_link`]
/// are the test of that. Crypto is the exception: it maps to no listed sector,
/// but Pakistan has heavy retail participation and it reads as a risk-appetite
/// gauge, so it is grouped separately rather than dressed up as a sector driver.
///
/// The Baltic Dry Index (`^BDI`) is **not** quoted by this API. Dry-bulk
/// freight is represented by the BDRY ETF instead, and is labelled as a proxy
/// rather than passed off as the index itself.
pub const CATALOG: &[MacroSpec] = &[
    // --- Energy ---
    MacroSpec {
        key: "brent",
        name: "Brent crude",
        symbol: "BZ=F",
        unit: "USD/bbl",
        group: Group::Energy,
    },
    MacroSpec {
        key: "wti",
        name: "WTI crude",
        symbol: "CL=F",
        unit: "USD/bbl",
        group: Group::Energy,
    },
    MacroSpec {
        key: "natgas",
        name: "Natural gas",
        symbol: "NG=F",
        unit: "USD/MMBtu",
        group: Group::Energy,
    },
    // --- Metals ---
    MacroSpec {
        key: "gold",
        name: "Gold",
        symbol: "GC=F",
        unit: "USD/oz",
        group: Group::Metals,
    },
    MacroSpec {
        key: "silver",
        name: "Silver",
        symbol: "SI=F",
        unit: "USD/oz",
        group: Group::Metals,
    },
    MacroSpec {
        key: "copper",
        name: "Copper",
        symbol: "HG=F",
        unit: "USD/lb",
        group: Group::Metals,
    },
    MacroSpec {
        key: "steel",
        name: "Steel (HRC)",
        symbol: "HRC=F",
        unit: "USD/ton",
        group: Group::Metals,
    },
    MacroSpec {
        key: "aluminium",
        name: "Aluminium",
        symbol: "ALI=F",
        unit: "USD/ton",
        group: Group::Metals,
    },
    // --- Agriculture ---
    MacroSpec {
        key: "cotton",
        name: "Cotton",
        symbol: "CT=F",
        unit: "USX/lb",
        group: Group::Agri,
    },
    MacroSpec {
        key: "wheat",
        name: "Wheat",
        symbol: "ZW=F",
        unit: "USX/bu",
        group: Group::Agri,
    },
    MacroSpec {
        key: "sugar",
        name: "Sugar #11",
        symbol: "SB=F",
        unit: "USX/lb",
        group: Group::Agri,
    },
    MacroSpec {
        key: "soyoil",
        name: "Soybean oil",
        symbol: "ZL=F",
        unit: "USX/lb",
        group: Group::Agri,
    },
    // --- Freight ---
    MacroSpec {
        key: "freight",
        name: "Dry bulk freight (BDRY ETF, proxy)",
        symbol: "BDRY",
        unit: "USD",
        group: Group::Freight,
    },
    // --- Currency & equities ---
    MacroSpec {
        key: "usdpkr",
        name: "USD / PKR",
        symbol: "PKR=X",
        unit: "PKR",
        group: Group::Currency,
    },
    MacroSpec {
        key: "sp500",
        name: "S&P 500",
        symbol: "^GSPC",
        unit: "index",
        group: Group::Equity,
    },
    // --- Crypto ---
    MacroSpec {
        key: "btc",
        name: "Bitcoin",
        symbol: "BTC-USD",
        unit: "USD",
        group: Group::Crypto,
    },
    MacroSpec {
        key: "eth",
        name: "Ethereum",
        symbol: "ETH-USD",
        unit: "USD",
        group: Group::Crypto,
    },
    MacroSpec {
        key: "sol",
        name: "Solana",
        symbol: "SOL-USD",
        unit: "USD",
        group: Group::Crypto,
    },
    MacroSpec {
        key: "bnb",
        name: "BNB",
        symbol: "BNB-USD",
        unit: "USD",
        group: Group::Crypto,
    },
    MacroSpec {
        key: "xrp",
        name: "XRP",
        symbol: "XRP-USD",
        unit: "USD",
        group: Group::Crypto,
    },
];

/// Why a PSX investor should care about a given series.
///
/// Kept beside the catalogue rather than in the renderer so the association is
/// part of the data model: a row without a reason to be on screen is decoration.
pub fn psx_link(key: &str) -> &'static str {
    match key {
        "brent" | "wti" => "refineries & OMCs — ATRL, PSO, APL",
        "natgas" => "fertiliser & power feedstock — FFC, EFERT",
        "gold" => "safe-haven flows, jewellery demand",
        "silver" => "jewellery demand, industrial use",
        "copper" => "cables & electrical goods — PAEL, PCAL",
        "steel" => "long & flat steel — ASTL, ISL, MUGHAL",
        "aluminium" => "engineering & auto parts input",
        "cotton" => "textile input cost — NML, GATM, ILP",
        "wheat" => "flour mills & food inflation",
        "sugar" => "sugar & allied — JDWS, ALNRS",
        "soyoil" => "edible oil imports — UNITY, PAKD",
        "freight" => "shipping & cement exports — PNSC, LUCK",
        "usdpkr" => "importers vs exporters, external debt",
        "sp500" => "global risk appetite, foreign flows",
        // Crypto drives no listed sector; it is a retail risk-appetite gauge,
        // and saying so is more useful than inventing a linkage.
        "btc" | "eth" | "sol" | "bnb" | "xrp" => "retail risk appetite (no listed sector)",
        _ => "",
    }
}

/// One external series: its latest level, its last move, and daily history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacroSeries {
    pub key: String,
    pub name: String,
    pub symbol: String,
    pub unit: String,
    /// Latest level. Yahoo's `regularMarketPrice` when present, otherwise the
    /// last usable close.
    pub last: f64,
    /// Move from the previous session's close, in percent.
    pub change_pct: f64,
    pub bars: Vec<Bar>,
}

impl MacroSeries {
    /// Closes, oldest first — the input to sparklines and correlation.
    pub fn closes(&self) -> Vec<f64> {
        self.bars.iter().map(|b| b.close).collect()
    }

    /// Change over the whole cached window, in percent. `0.0` when there is
    /// nothing to compare against.
    pub fn window_change_pct(&self) -> f64 {
        match (self.bars.first(), self.bars.last()) {
            (Some(a), Some(b)) if a.close > 0.0 && a.close.is_finite() && b.close.is_finite() => {
                let v = (b.close / a.close - 1.0) * 100.0;
                if v.is_finite() { v } else { 0.0 }
            }
            _ => 0.0,
        }
    }
}

/// Fetch and parse one series.
pub async fn fetch_series(client: &ExtClient, spec: &MacroSpec) -> Result<MacroSeries> {
    let url = format!(
        "{CHART_BASE}/{}?range={RANGE}&interval={INTERVAL}",
        encode_segment(spec.symbol)
    );
    let body = client
        .get_text(&url)
        .await
        .with_context(|| format!("fetching {} ({})", spec.name, spec.symbol))?;
    parse_chart(&body, spec)
}

/// Fetch the whole catalogue, keeping whatever succeeds.
///
/// One dead symbol must not blank the screen, so failures are collected and
/// returned alongside the series rather than short-circuiting.
pub async fn fetch_all(client: &ExtClient) -> (Vec<MacroSeries>, Vec<String>) {
    let mut out = Vec::with_capacity(CATALOG.len());
    let mut errors = Vec::new();
    for spec in CATALOG {
        match fetch_series(client, spec).await {
            Ok(s) => out.push(s),
            Err(e) => errors.push(format!("{}: {e}", spec.name)),
        }
    }
    (out, errors)
}

/// Parse a chart response body into a [`MacroSeries`].
///
/// Errors on malformed JSON and on Yahoo's `chart.error` shape; otherwise it is
/// total — any bar it cannot make sense of is dropped.
pub fn parse_chart(body: &str, spec: &MacroSpec) -> Result<MacroSeries> {
    let root: Value = serde_json::from_str(body).context("decoding Yahoo chart JSON")?;
    let chart = root
        .get("chart")
        .context("response has no `chart` object")?;

    // An unknown symbol is a 200 with a null result and a populated error.
    let result = match chart.get("result").and_then(|r| r.as_array()) {
        Some(arr) if !arr.is_empty() => &arr[0],
        _ => {
            let msg = chart
                .get("error")
                .and_then(|e| {
                    e.get("description")
                        .or_else(|| e.get("code"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("no result")
                .to_string();
            anyhow::bail!("{}: {msg}", spec.symbol);
        }
    };

    let meta = result.get("meta");
    let stamps = result
        .get("timestamp")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    let quote = result
        .get("indicators")
        .and_then(|i| i.get("quote"))
        .and_then(|q| q.as_array())
        .and_then(|q| q.first());

    let col = |name: &str| -> Vec<Option<f64>> {
        quote
            .and_then(|q| q.get(name))
            .and_then(|c| c.as_array())
            .map(|a| a.iter().map(|v| v.as_f64()).collect())
            .unwrap_or_default()
    };
    let (opens, highs, lows, closes, volumes) = (
        col("open"),
        col("high"),
        col("low"),
        col("close"),
        col("volume"),
    );

    let mut bars: Vec<Bar> = Vec::with_capacity(stamps.len());
    for (i, ts) in stamps.iter().enumerate() {
        let Some(ts) = ts.as_i64() else { continue };
        // A null close means the session did not print. There is no honest way
        // to fill it, and a zero would read as a −100% day, so the bar is
        // dropped and the date-alignment step simply won't see it.
        let Some(close) = finite(closes.get(i).copied().flatten()) else {
            continue;
        };
        let open = finite(opens.get(i).copied().flatten()).unwrap_or(close);
        let high = finite(highs.get(i).copied().flatten()).unwrap_or(open.max(close));
        let low = finite(lows.get(i).copied().flatten()).unwrap_or(open.min(close));
        let volume = finite(volumes.get(i).copied().flatten()).unwrap_or(0.0);
        bars.push(Bar {
            ts,
            open,
            high: high.max(low),
            low: low.min(high),
            close,
            volume,
        });
    }
    bars.sort_by_key(|b| b.ts);

    // Prefer the live quote from `meta`; fall back to the last usable close so
    // an after-hours or stale response still shows a number.
    let last = meta
        .and_then(|m| m.get("regularMarketPrice"))
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite())
        .or_else(|| bars.last().map(|b| b.close))
        .unwrap_or(f64::NAN);

    // Day-over-day move from the last two closes. `meta` carries no previous
    // close for futures, so the series is the reliable source.
    let change_pct = match bars.len() {
        0 | 1 => 0.0,
        n => {
            let prev = bars[n - 2].close;
            if prev > 0.0 && prev.is_finite() && last.is_finite() {
                let v = (last / prev - 1.0) * 100.0;
                if v.is_finite() { v } else { 0.0 }
            } else {
                0.0
            }
        }
    };

    // The API's own name is used only when the catalogue has nothing better —
    // never for the freight proxy, whose label must stay explicit.
    let name = spec.name.to_string();
    let symbol = meta
        .and_then(|m| m.get("symbol"))
        .and_then(|v| v.as_str())
        .unwrap_or(spec.symbol)
        .to_string();

    Ok(MacroSeries {
        key: spec.key.to_string(),
        name,
        symbol,
        unit: spec.unit.to_string(),
        last,
        change_pct,
        bars,
    })
}

fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MacroSpec {
        MacroSpec {
            key: "brent",
            name: "Brent crude",
            symbol: "BZ=F",
            unit: "USD/bbl",
            group: Group::Energy,
        }
    }

    /// Shape of a real `v8/finance/chart` response, trimmed to four sessions
    /// with a null-filled holiday in the middle.
    const CHART: &str = r#"{
      "chart": {
        "result": [
          {
            "meta": {
              "currency": "USD",
              "symbol": "BZ=F",
              "shortName": "Brent Crude Oil Last Day Financ",
              "regularMarketPrice": 96.78,
              "fiftyTwoWeekHigh": 104.12,
              "fiftyTwoWeekLow": 61.05
            },
            "timestamp": [1784390400, 1784476800, 1784563200, 1784649600],
            "indicators": {
              "quote": [
                {
                  "open":   [94.10, null, 95.80, 96.20],
                  "high":   [95.02, null, 96.44, 97.10],
                  "low":    [93.55, null, 95.10, 95.90],
                  "close":  [94.90, null, 96.10, 96.78],
                  "volume": [120000, null, 98000, null]
                }
              ]
            }
          }
        ],
        "error": null
      }
    }"#;

    #[test]
    fn parses_a_chart_response_into_bars() {
        let s = parse_chart(CHART, &spec()).unwrap();
        assert_eq!(s.key, "brent");
        assert_eq!(s.symbol, "BZ=F");
        assert_eq!(s.unit, "USD/bbl");
        assert_eq!(s.last, 96.78);
        assert_eq!(s.bars.len(), 3, "the null session must be dropped");
        assert_eq!(s.bars[0].close, 94.90);
        assert_eq!(s.bars[2].close, 96.78);
        assert!(s.bars.windows(2).all(|w| w[0].ts < w[1].ts));
    }

    #[test]
    fn null_entries_never_become_zeros() {
        let s = parse_chart(CHART, &spec()).unwrap();
        for b in &s.bars {
            assert!(b.close > 0.0, "a null close must not survive as 0.0");
            assert!(b.open.is_finite() && b.high.is_finite() && b.low.is_finite());
            assert!(b.high >= b.low);
        }
        // A null volume degrades to zero — volume is not load-bearing here.
        assert_eq!(s.bars[2].volume, 0.0);
    }

    #[test]
    fn change_is_measured_against_the_previous_usable_close() {
        let s = parse_chart(CHART, &spec()).unwrap();
        let expected = (96.78 / 96.10 - 1.0) * 100.0;
        assert!((s.change_pct - expected).abs() < 1e-9);
        assert!(s.change_pct.is_finite());
    }

    #[test]
    fn unknown_symbols_report_the_api_error_rather_than_an_empty_series() {
        let body = r#"{"chart":{"result":null,"error":{
            "code":"Not Found",
            "description":"No data found, symbol may be delisted"}}}"#;
        let err = parse_chart(body, &spec()).unwrap_err().to_string();
        assert!(err.contains("BZ=F"), "got {err}");
        assert!(err.contains("delisted"), "got {err}");
    }

    #[test]
    fn an_empty_result_array_is_also_an_error() {
        let body = r#"{"chart":{"result":[],"error":null}}"#;
        assert!(parse_chart(body, &spec()).is_err());
    }

    #[test]
    fn missing_indicator_arrays_yield_an_empty_series_not_a_panic() {
        let body = r#"{"chart":{"result":[{"meta":{"regularMarketPrice":12.5}}],"error":null}}"#;
        let s = parse_chart(body, &spec()).unwrap();
        assert!(s.bars.is_empty());
        assert_eq!(s.last, 12.5);
        assert_eq!(s.change_pct, 0.0);
    }

    #[test]
    fn a_response_with_no_price_at_all_stays_non_finite_free_in_the_ui() {
        let body = r#"{"chart":{"result":[{"meta":{}}],"error":null}}"#;
        let s = parse_chart(body, &spec()).unwrap();
        // `last` may legitimately be unknown; the formatter renders it as "—".
        assert_eq!(crate::ui::theme::price(s.last), "—");
        assert_eq!(s.change_pct, 0.0);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(parse_chart("not json", &spec()).is_err());
        assert!(parse_chart("{}", &spec()).is_err());
    }

    #[test]
    fn window_change_handles_degenerate_series() {
        let mut s = parse_chart(CHART, &spec()).unwrap();
        assert!(s.window_change_pct().is_finite());
        s.bars.clear();
        assert_eq!(s.window_change_pct(), 0.0);
    }

    #[test]
    fn the_catalogue_is_well_formed_and_honest() {
        assert!(!CATALOG.is_empty());
        for spec in CATALOG {
            assert!(!spec.key.is_empty());
            assert!(!spec.name.is_empty());
            assert!(!spec.symbol.is_empty());
            assert!(
                !psx_link(spec.key).is_empty(),
                "{} needs a PSX association",
                spec.key
            );
        }
        // The Baltic Dry Index is not available; nothing may claim to be it.
        for spec in CATALOG {
            assert_ne!(spec.symbol, "^BDI");
            assert!(!spec.name.contains("Baltic"), "{} overclaims", spec.name);
        }
        let freight = CATALOG.iter().find(|s| s.key == "freight").unwrap();
        assert!(freight.name.contains("proxy"));
        assert_eq!(freight.symbol, "BDRY");
    }

    #[test]
    fn catalogue_keys_and_symbols_are_unique() {
        // A duplicate key would collide in the cache and in the group lookup.
        for (i, a) in CATALOG.iter().enumerate() {
            for b in CATALOG.iter().skip(i + 1) {
                assert_ne!(a.key, b.key, "duplicate key {}", a.key);
                assert_ne!(a.symbol, b.symbol, "duplicate symbol {}", a.symbol);
            }
        }
    }

    #[test]
    fn every_group_that_is_used_is_reachable_from_group_all() {
        for spec in CATALOG {
            assert!(
                Group::ALL.contains(&spec.group),
                "{} is in a group the screen never renders",
                spec.key
            );
        }
    }

    #[test]
    fn the_catalogue_covers_energy_metals_agri_and_crypto() {
        let has = |g: Group| CATALOG.iter().any(|s| s.group == g);
        for g in [Group::Energy, Group::Metals, Group::Agri, Group::Crypto] {
            assert!(has(g), "{} has no series", g.label());
        }
    }

    #[test]
    fn crypto_does_not_claim_a_psx_sector() {
        // Crypto maps to no listed sector. Saying so is more useful than
        // inventing a linkage, and the wording is asserted so it stays honest.
        for spec in CATALOG.iter().filter(|s| s.group == Group::Crypto) {
            let link = psx_link(spec.key);
            assert!(
                link.contains("no listed sector"),
                "{} implies a sector linkage it does not have: {link:?}",
                spec.key
            );
        }
    }

    #[test]
    fn yahoo_symbols_survive_url_encoding() {
        // Futures carry '=' and indices a leading '^'; crypto uses a plain
        // hyphen and must not be mangled.
        for spec in CATALOG {
            let encoded = encode_segment(spec.symbol);
            assert!(!encoded.contains('='), "{} left a raw '='", spec.symbol);
            assert!(!encoded.contains('^'), "{} left a raw '^'", spec.symbol);
            assert!(!encoded.is_empty());
        }
        assert_eq!(encode_segment("BTC-USD"), "BTC-USD");
    }
}
