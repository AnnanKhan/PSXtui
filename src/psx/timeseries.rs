//! The `/timeseries` JSON feeds: long-run daily history and intraday ticks.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::client::PsxClient;
use crate::model::{Bar, Tick};

#[derive(Debug, Deserialize)]
struct TimeseriesEnvelope {
    status: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Vec<Vec<f64>>,
}

/// Daily history for a symbol.
///
/// The feed yields `[timestamp, close, volume, open]` — note there is no high
/// or low, so those are seeded from `open`/`close` and are meant to be
/// upgraded with true OHLC from the `/historical` snapshot where the cache has
/// it. Bars come back newest-first from PSX and are returned oldest-first.
pub async fn eod(client: &PsxClient, symbol: &str) -> Result<Vec<Bar>> {
    let body = client
        .get_text(&format!("/timeseries/eod/{symbol}"))
        .await
        .with_context(|| format!("fetching EOD series for {symbol}"))?;
    parse_eod(&body).with_context(|| format!("parsing EOD series for {symbol}"))
}

/// Intraday trade ticks for the current session: `[timestamp, price, volume]`.
/// Returned oldest-first.
pub async fn intraday(client: &PsxClient, symbol: &str) -> Result<Vec<Tick>> {
    let body = client
        .get_text(&format!("/timeseries/int/{symbol}"))
        .await
        .with_context(|| format!("fetching intraday series for {symbol}"))?;
    parse_intraday(&body).with_context(|| format!("parsing intraday series for {symbol}"))
}

fn decode(body: &str) -> Result<Vec<Vec<f64>>> {
    let env: TimeseriesEnvelope = serde_json::from_str(body).context("decoding timeseries JSON")?;
    if env.status != 1 {
        bail!(
            "PSX returned status {}{}",
            env.status,
            if env.message.is_empty() {
                String::new()
            } else {
                format!(": {}", env.message)
            }
        );
    }
    Ok(env.data)
}

pub fn parse_eod(body: &str) -> Result<Vec<Bar>> {
    let mut bars: Vec<Bar> = decode(body)?
        .into_iter()
        .filter_map(|row| {
            // [ts, close, volume, open]
            let (&ts, &close, &volume) = (row.first()?, row.get(1)?, row.get(2)?);
            let open = row.get(3).copied().filter(|o| *o > 0.0).unwrap_or(close);
            if close <= 0.0 {
                return None;
            }
            Some(Bar {
                ts: ts as i64,
                open,
                // Placeholder extremes — the true intraday range is only
                // available from the /historical snapshot.
                high: open.max(close),
                low: open.min(close),
                close,
                volume,
            })
        })
        .collect();

    bars.sort_by_key(|b| b.ts);
    bars.dedup_by_key(|b| b.ts);
    Ok(bars)
}

pub fn parse_intraday(body: &str) -> Result<Vec<Tick>> {
    let mut ticks: Vec<Tick> = decode(body)?
        .into_iter()
        .filter_map(|row| {
            let (&ts, &price, &volume) = (row.first()?, row.get(1)?, row.get(2)?);
            if price <= 0.0 {
                return None;
            }
            Some(Tick {
                ts: ts as i64,
                price,
                volume,
            })
        })
        .collect();

    ticks.sort_by_key(|t| t.ts);
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_eod_newest_first_into_oldest_first_bars() {
        let body = r#"{"status":1,"message":"","data":[
            [1784890800,292,962250,294.7],
            [1784804400,295.49,1013311,300]]}"#;
        let bars = parse_eod(body).unwrap();
        assert_eq!(bars.len(), 2);
        assert!(bars[0].ts < bars[1].ts, "bars must be oldest-first");

        let latest = &bars[1];
        assert_eq!(latest.open, 294.7);
        assert_eq!(latest.close, 292.0);
        assert_eq!(latest.volume, 962_250.0);
        // Without a /historical upgrade, the range brackets open..close.
        assert_eq!(latest.high, 294.7);
        assert_eq!(latest.low, 292.0);
    }

    #[test]
    fn eod_falls_back_to_close_when_open_missing_or_zero() {
        let body = r#"{"status":1,"data":[[1784890800,292,1000,0],[1784804400,300,1000]]}"#;
        let bars = parse_eod(body).unwrap();
        assert_eq!(bars.len(), 2);
        assert!(bars.iter().all(|b| b.open > 0.0));
    }

    #[test]
    fn parses_intraday_ticks() {
        let body = r#"{"status":1,"data":[[1784893405,292,120],[1784892991,292.5,3000]]}"#;
        let ticks = parse_intraday(body).unwrap();
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].price, 292.5);
        assert_eq!(ticks[1].volume, 120.0);
    }

    #[test]
    fn non_ok_status_is_an_error() {
        let body = r#"{"status":0,"message":"no such symbol","data":[]}"#;
        assert!(parse_eod(body).is_err());
    }

    #[test]
    fn malformed_rows_are_skipped_not_fatal() {
        let body = r#"{"status":1,"data":[[1784890800,292,1000,294],[1784804400]]}"#;
        let bars = parse_eod(body).unwrap();
        assert_eq!(bars.len(), 1);
    }
}
