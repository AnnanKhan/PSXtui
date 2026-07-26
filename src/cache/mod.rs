//! Local SQLite cache.
//!
//! PSX is slow and rate-sensitive, and its EOD feed is incomplete (no true
//! high/low). This layer solves both: it persists everything fetched so
//! analysis runs offline and instantly, and it merges the two price sources so
//! a real intraday range always beats the placeholder one derived from
//! open/close.
//!
//! Bars are keyed by *trading day* rather than raw timestamp. The EOD feed
//! stamps each bar at 16:00 PKT while the `/historical` snapshot is addressed
//! by calendar date, so a shared day key is what lets the two merge at all.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{Bar, Company, Quote, SymbolInfo};
use crate::psx::HistoricalRow;

/// Pakistan Standard Time. PSX does not observe daylight saving, so a fixed
/// offset is exact rather than an approximation.
pub fn pkt() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600).expect("PKT offset is valid")
}

/// The PSX trading day a timestamp belongs to, as `YYYY-MM-DD`.
pub fn trading_day(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .unwrap_or_default()
        .with_timezone(&pkt())
        .format("%Y-%m-%d")
        .to_string()
}

/// Timestamp for a trading day's close (16:00 PKT), matching the EOD feed's
/// own convention so merged bars sort consistently.
pub fn day_close_ts(day: &str) -> i64 {
    NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(16, 0, 0))
        .and_then(|dt| pkt().from_local_datetime(&dt).single())
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) the cache at the platform data directory,
    /// e.g. `~/.local/share/psxtui/psx.db`.
    pub fn open_default() -> Result<Self> {
        let path = default_db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating cache directory {}", parent.display()))?;
        }
        Self::open(&path)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening cache database {}", path.display()))?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL keeps reads from blocking the background refresh writer.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS symbols (
                symbol      TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                sector      TEXT NOT NULL,
                is_etf      INTEGER NOT NULL,
                is_debt     INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS bars (
                symbol      TEXT NOT NULL,
                day         TEXT NOT NULL,
                ts          INTEGER NOT NULL,
                open        REAL NOT NULL,
                high        REAL NOT NULL,
                low         REAL NOT NULL,
                close       REAL NOT NULL,
                volume      REAL NOT NULL,
                -- 1 when high/low came from the /historical snapshot and are
                -- the true intraday extremes; 0 when derived from open/close.
                hl_known    INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (symbol, day)
            );
            CREATE INDEX IF NOT EXISTS idx_bars_symbol_ts ON bars (symbol, ts);

            CREATE TABLE IF NOT EXISTS companies (
                symbol      TEXT PRIMARY KEY,
                json        TEXT NOT NULL,
                fetched_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS quotes (
                symbol      TEXT PRIMARY KEY,
                json        TEXT NOT NULL,
                fetched_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS meta (
                key         TEXT PRIMARY KEY,
                value       TEXT NOT NULL
            );
            "#,
        )
        .context("initialising cache schema")?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // --- symbols ---------------------------------------------------------

    pub fn put_symbols(&self, syms: &[SymbolInfo]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO symbols (symbol, name, sector, is_etf, is_debt)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(symbol) DO UPDATE SET
                    name = excluded.name,
                    sector = excluded.sector,
                    is_etf = excluded.is_etf,
                    is_debt = excluded.is_debt",
            )?;
            for s in syms {
                stmt.execute(params![
                    s.symbol,
                    s.name,
                    s.sector_name,
                    s.is_etf as i32,
                    s.is_debt as i32
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn symbols(&self) -> Result<Vec<SymbolInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT symbol, name, sector, is_etf, is_debt FROM symbols ORDER BY symbol")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SymbolInfo {
                    symbol: r.get(0)?,
                    name: r.get(1)?,
                    sector_name: r.get(2)?,
                    is_etf: r.get::<_, i32>(3)? != 0,
                    is_debt: r.get::<_, i32>(4)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- bars ------------------------------------------------------------

    /// Store bars from the EOD feed.
    ///
    /// These carry no true high/low, so an existing row whose range came from
    /// the `/historical` snapshot keeps its extremes; only close, open and
    /// volume are refreshed.
    pub fn put_eod_bars(&self, symbol: &str, bars: &[Bar]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO bars (symbol, day, ts, open, high, low, close, volume, hl_known)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
                 ON CONFLICT(symbol, day) DO UPDATE SET
                    ts     = excluded.ts,
                    open   = excluded.open,
                    close  = excluded.close,
                    volume = excluded.volume,
                    -- Never let a derived range clobber a real one.
                    high   = CASE WHEN bars.hl_known = 1 THEN bars.high ELSE excluded.high END,
                    low    = CASE WHEN bars.hl_known = 1 THEN bars.low  ELSE excluded.low  END",
            )?;
            for b in bars {
                stmt.execute(params![
                    symbol,
                    trading_day(b.ts),
                    b.ts,
                    b.open,
                    b.high,
                    b.low,
                    b.close,
                    b.volume
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Store a whole-market `/historical` snapshot for one trading day.
    ///
    /// This is the authoritative source for intraday extremes and always wins.
    pub fn put_historical(&self, day: &str, rows: &[HistoricalRow]) -> Result<()> {
        let ts = day_close_ts(day);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO bars (symbol, day, ts, open, high, low, close, volume, hl_known)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)
                 ON CONFLICT(symbol, day) DO UPDATE SET
                    ts       = excluded.ts,
                    open     = excluded.open,
                    high     = excluded.high,
                    low      = excluded.low,
                    close    = excluded.close,
                    volume   = excluded.volume,
                    hl_known = 1",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.symbol, day, ts, r.open, r.high, r.low, r.close, r.volume
                ])?;
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![format!("historical:{day}"), "1"],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read a symbol's bars oldest-first, optionally limited to the most
    /// recent `limit` sessions.
    pub fn bars(&self, symbol: &str, limit: Option<usize>) -> Result<Vec<Bar>> {
        let conn = self.conn.lock().unwrap();
        let sql = match limit {
            Some(_) => {
                "SELECT ts, open, high, low, close, volume FROM bars
                 WHERE symbol = ?1 ORDER BY day DESC LIMIT ?2"
            }
            None => {
                "SELECT ts, open, high, low, close, volume FROM bars
                 WHERE symbol = ?1 ORDER BY day ASC"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        let map = |r: &rusqlite::Row| {
            Ok(Bar {
                ts: r.get(0)?,
                open: r.get(1)?,
                high: r.get(2)?,
                low: r.get(3)?,
                close: r.get(4)?,
                volume: r.get(5)?,
            })
        };

        let mut bars: Vec<Bar> = match limit {
            Some(n) => stmt
                .query_map(params![symbol, n as i64], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map(params![symbol], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        // The limited query walks backwards to take the newest rows.
        if limit.is_some() {
            bars.reverse();
        }
        Ok(bars)
    }

    /// Most recent cached trading day for a symbol, if any.
    pub fn last_day(&self, symbol: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let day: Option<String> = conn
            .query_row(
                "SELECT MAX(day) FROM bars WHERE symbol = ?1",
                params![symbol],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(day)
    }

    /// Whether the whole-market snapshot for `day` has already been ingested.
    pub fn has_historical(&self, day: &str) -> Result<bool> {
        Ok(self.get_meta(&format!("historical:{day}"))?.is_some())
    }

    /// Trading days, newest first, that still need a `/historical` backfill.
    ///
    /// Weekends are skipped outright; public holidays surface as empty
    /// snapshots and are marked done by [`Store::mark_historical_empty`] so
    /// they are not retried every launch.
    pub fn missing_historical_days(&self, lookback: i64) -> Result<Vec<String>> {
        let today = Utc::now().with_timezone(&pkt()).date_naive();
        let mut out = Vec::new();
        for back in 0..lookback {
            let d = today - Duration::days(back);
            // ISO weekday: Saturday is 6, Sunday is 7.
            let wd = d.format("%u").to_string();
            if wd == "6" || wd == "7" {
                continue;
            }
            let day = d.format("%Y-%m-%d").to_string();
            if !self.has_historical(&day)? {
                out.push(day);
            }
        }
        Ok(out)
    }

    /// Record that a day yielded no rows (holiday) so it is not refetched.
    pub fn mark_historical_empty(&self, day: &str) -> Result<()> {
        self.set_meta(&format!("historical:{day}"), "empty")
    }

    pub fn bar_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM bars", [], |r| r.get(0))?)
    }

    // --- quotes ----------------------------------------------------------

    /// Persist the latest market-watch board so the app opens with data even
    /// before the first refresh completes.
    pub fn put_quotes(&self, quotes: &[Quote]) -> Result<()> {
        let now = Utc::now().timestamp();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO quotes (symbol, json, fetched_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(symbol) DO UPDATE SET
                    json = excluded.json, fetched_at = excluded.fetched_at",
            )?;
            for q in quotes {
                stmt.execute(params![q.symbol, serde_json::to_string(q)?, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn quotes(&self) -> Result<Vec<Quote>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT json FROM quotes")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .iter()
            .filter_map(|j| serde_json::from_str(j).ok())
            .collect())
    }

    // --- companies -------------------------------------------------------

    pub fn put_company(&self, c: &Company) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO companies (symbol, json, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(symbol) DO UPDATE SET
                json = excluded.json, fetched_at = excluded.fetched_at",
            params![c.symbol, serde_json::to_string(c)?, Utc::now().timestamp()],
        )?;
        Ok(())
    }

    /// Read a cached company profile, ignoring entries older than `max_age`.
    pub fn company(&self, symbol: &str, max_age: Duration) -> Result<Option<Company>> {
        let conn = self.conn.lock().unwrap();
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT json, fetched_at FROM companies WHERE symbol = ?1",
                params![symbol],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let Some((json, fetched_at)) = row else {
            return Ok(None);
        };
        if Utc::now().timestamp() - fetched_at > max_age.num_seconds() {
            return Ok(None);
        }
        Ok(serde_json::from_str(&json).ok())
    }

    // --- meta ------------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

pub fn default_db_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "psxtui")
        .context("locating platform data directory")?;
    Ok(dirs.data_dir().join("psx.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(ts: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> Bar {
        Bar {
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    fn hist(symbol: &str, o: f64, h: f64, l: f64, c: f64, v: f64) -> HistoricalRow {
        HistoricalRow {
            symbol: symbol.into(),
            ldcp: 0.0,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    #[test]
    fn maps_timestamps_to_pkt_trading_days() {
        // 2026-07-24 16:00 PKT — the EOD feed's stamp for that session.
        assert_eq!(trading_day(1_784_890_800), "2026-07-24");
        assert_eq!(trading_day(1_784_804_400), "2026-07-23");
    }

    #[test]
    fn day_close_ts_round_trips_through_trading_day() {
        let day = "2026-07-24";
        assert_eq!(trading_day(day_close_ts(day)), day);
        assert_eq!(day_close_ts(day), 1_784_890_800);
    }

    #[test]
    fn stores_and_reads_bars_oldest_first() {
        let s = Store::open_in_memory().unwrap();
        s.put_eod_bars(
            "HBL",
            &[
                bar(1_784_804_400, 300.0, 300.0, 295.0, 295.49, 1000.0),
                bar(1_784_890_800, 294.7, 294.7, 292.0, 292.0, 962_250.0),
            ],
        )
        .unwrap();

        let bars = s.bars("HBL", None).unwrap();
        assert_eq!(bars.len(), 2);
        assert!(bars[0].ts < bars[1].ts);
        assert_eq!(bars[1].close, 292.0);
    }

    #[test]
    fn limited_read_returns_the_newest_bars_in_order() {
        let s = Store::open_in_memory().unwrap();
        let bars: Vec<Bar> = (0..10)
            .map(|i| {
                let ts = day_close_ts("2026-07-01") + i * 86_400;
                bar(ts, 10.0, 11.0, 9.0, 10.0 + i as f64, 100.0)
            })
            .collect();
        s.put_eod_bars("X", &bars).unwrap();

        let recent = s.bars("X", Some(3)).unwrap();
        assert_eq!(recent.len(), 3);
        assert!(recent[0].ts < recent[2].ts, "must be oldest-first");
        assert_eq!(recent[2].close, 19.0, "should end at the newest bar");
    }

    #[test]
    fn historical_snapshot_supplies_true_high_and_low() {
        let s = Store::open_in_memory().unwrap();
        // EOD first: range is only a placeholder derived from open/close.
        s.put_eod_bars(
            "HBL",
            &[bar(1_784_890_800, 294.7, 294.7, 292.0, 292.0, 962_250.0)],
        )
        .unwrap();
        assert_eq!(s.bars("HBL", None).unwrap()[0].high, 294.7);

        s.put_historical(
            "2026-07-24",
            &[hist("HBL", 294.7, 295.97, 290.0, 292.0, 962_250.0)],
        )
        .unwrap();

        let b = s.bars("HBL", None).unwrap();
        assert_eq!(b.len(), 1, "same trading day must merge, not duplicate");
        assert_eq!(b[0].high, 295.97);
        assert_eq!(b[0].low, 290.0);
    }

    #[test]
    fn eod_refresh_never_clobbers_a_known_range() {
        let s = Store::open_in_memory().unwrap();
        s.put_historical(
            "2026-07-24",
            &[hist("HBL", 294.7, 295.97, 290.0, 292.0, 962_250.0)],
        )
        .unwrap();

        // A later EOD refresh carries a narrower, derived range.
        s.put_eod_bars(
            "HBL",
            &[bar(1_784_890_800, 294.7, 294.7, 292.0, 292.0, 999_999.0)],
        )
        .unwrap();

        let b = &s.bars("HBL", None).unwrap()[0];
        assert_eq!(b.high, 295.97, "true high must survive an EOD refresh");
        assert_eq!(b.low, 290.0);
        assert_eq!(b.volume, 999_999.0, "but volume should still refresh");
    }

    #[test]
    fn tracks_which_historical_days_are_ingested() {
        let s = Store::open_in_memory().unwrap();
        assert!(!s.has_historical("2026-07-24").unwrap());
        s.put_historical("2026-07-24", &[]).unwrap();
        assert!(s.has_historical("2026-07-24").unwrap());

        s.mark_historical_empty("2026-07-23").unwrap();
        assert!(s.has_historical("2026-07-23").unwrap());
    }

    #[test]
    fn missing_days_skip_weekends_and_ingested_days() {
        let s = Store::open_in_memory().unwrap();
        let days = s.missing_historical_days(14).unwrap();
        assert!(!days.is_empty());
        for d in &days {
            let wd = NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .unwrap()
                .format("%u")
                .to_string();
            assert!(wd != "6" && wd != "7", "{d} is a weekend");
        }

        let first = days[0].clone();
        s.put_historical(&first, &[]).unwrap();
        assert!(!s.missing_historical_days(14).unwrap().contains(&first));
    }

    #[test]
    fn round_trips_symbols_and_companies() {
        let s = Store::open_in_memory().unwrap();
        s.put_symbols(&[SymbolInfo {
            symbol: "HBL".into(),
            name: "Habib Bank Limited".into(),
            sector_name: "COMMERCIAL BANKS".into(),
            is_etf: false,
            is_debt: false,
        }])
        .unwrap();
        assert_eq!(s.symbols().unwrap()[0].sector_name, "COMMERCIAL BANKS");

        let c = Company {
            symbol: "HBL".into(),
            name: "Habib Bank Limited".into(),
            pe_ratio: Some(6.82),
            ..Default::default()
        };
        s.put_company(&c).unwrap();
        let got = s.company("HBL", Duration::days(1)).unwrap().unwrap();
        assert_eq!(got.pe_ratio, Some(6.82));

        // An expired entry reads as absent.
        assert!(s.company("HBL", Duration::seconds(-1)).unwrap().is_none());
    }

    #[test]
    fn symbols_upsert_is_idempotent() {
        let s = Store::open_in_memory().unwrap();
        let sym = SymbolInfo {
            symbol: "HBL".into(),
            name: "Old Name".into(),
            sector_name: "BANKS".into(),
            is_etf: false,
            is_debt: false,
        };
        s.put_symbols(std::slice::from_ref(&sym)).unwrap();
        s.put_symbols(&[SymbolInfo {
            name: "Habib Bank Limited".into(),
            ..sym
        }])
        .unwrap();

        let all = s.symbols().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Habib Bank Limited");
    }
}
