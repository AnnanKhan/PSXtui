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

/// Timestamp of 1 January this year, in PKT — the cut-off for year-to-date.
pub fn year_start_ts() -> i64 {
    let year = Utc::now().with_timezone(&pkt()).format("%Y").to_string();
    day_close_ts(&format!("{year}-01-01")) - 16 * 3600
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

/// The widest a single session's high/low may be and still be believable, as a
/// ratio.
///
/// PSX runs circuit breakers on the ready market, so a scrip cannot double or
/// halve between two prints, let alone inside one session. A bar whose range is
/// wider than this did not happen: its extremes are quoted on a different price
/// basis from its close. Set well past any real session so that a genuinely
/// violent day — a new listing, a resumption after suspension — is never
/// mistaken for corrupt data.
const MAX_SESSION_RANGE: f64 = 2.0;

/// What a completed `/historical` snapshot is marked with.
///
/// Versioned, because the marker also answers "is there anything left to fetch
/// for this day": snapshots read before the cache kept raw closes left no way
/// to reconcile an adjusted series, so bumping this asks the deep backfill
/// ([`Store::days_missing_true_range`]) to read them again. Holidays are marked
/// `empty` and never re-read whatever this says — there is nothing there.
const SNAPSHOT_MARK: &str = "2";

/// Put a bar's open/high/low onto the same price basis as its close.
///
/// PSX publishes the same session through two feeds that do not agree.
/// `/timeseries/eod` gives a close adjusted for corporate actions beside an
/// *unadjusted* open; `/historical` is unadjusted throughout. Merge them
/// naively and a scrip that has since split eleven-for-one gets bars whose open
/// is eleven times their close — candles that span the whole plot and a y-axis
/// scaled to a price the scrip never traded at.
///
/// The close is the value to keep: it is the one that stays continuous across a
/// split, and the one today's quote agrees with. So:
///
/// * With the snapshot's raw close on hand, the ratio between the two closes
///   *is* the adjustment factor for that session. Scaling open/high/low by it
///   is exact — the range keeps its true shape and lands around the right
///   price.
/// * Without it, there is nothing to compute a factor from. A bar that is
///   internally impossible is reduced to its close, which says "this session
///   closed here, its range is unknown" — the truth — rather than inventing a
///   range from values on the wrong basis.
///
/// A bar that is already consistent is left exactly as it is, which is almost
/// all of them: adjustments are rare and only reach back past the last one.
fn reconcile(bar: &mut Bar, close_raw: Option<f64>) {
    if !bar.close.is_finite() || bar.close <= 0.0 {
        return;
    }

    if let Some(raw) = close_raw
        && raw.is_finite()
        && raw > 0.0
    {
        let factor = bar.close / raw;
        // Exactly 1.0 for every session not behind a corporate action, which
        // is the common case; skip the arithmetic and any float drift with it.
        if (factor - 1.0).abs() > f64::EPSILON {
            bar.open *= factor;
            bar.high *= factor;
            bar.low *= factor;
        }
        return;
    }

    if !plausible(bar) {
        bar.open = bar.close;
        bar.high = bar.close;
        bar.low = bar.close;
    }
}

/// Whether a bar's own numbers can describe one session.
fn plausible(bar: &Bar) -> bool {
    let all_finite = [bar.open, bar.high, bar.low, bar.close]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0);
    if !all_finite {
        return false;
    }
    // The close must sit inside the range, and the range must be a range a
    // session could actually have travelled.
    bar.low <= bar.open.min(bar.close)
        && bar.high >= bar.open.max(bar.close)
        && bar.high <= bar.low * MAX_SESSION_RANGE
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
        // WAL still serialises writers, and nothing stops a second psxtui
        // running against the same cache. Without a busy timeout SQLite fails
        // a contended write immediately rather than waiting, and the callers
        // that discard a write error — the backfill does — would drop the day
        // silently. Waiting is always the better answer here: every write in
        // this app is short.
        conn.busy_timeout(std::time::Duration::from_secs(10)).ok();

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
                -- The unadjusted close from the /historical snapshot, kept
                -- beside the adjusted one the EOD feed supplies. The ratio of
                -- the two is the corporate-action factor for that session,
                -- which is what puts open/high/low on the same basis as the
                -- close. NULL until a snapshot for the day has been read.
                close_raw   REAL,
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

            -- External market context (commodities, headlines, policy rate).
            -- One JSON blob per kind, so the Macro screen opens instantly and
            -- offline, exactly like the cached quote board.
            CREATE TABLE IF NOT EXISTS external (
                key         TEXT PRIMARY KEY,
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

        // Caches created before the adjusted-close reconciliation have no
        // `close_raw`. Adding it is the whole migration: the column is
        // nullable, and a NULL means "no snapshot read for this session yet",
        // which is exactly the state such a row is in. SQLite has no
        // ADD COLUMN IF NOT EXISTS, so the second run's error is the check.
        let _ = conn.execute("ALTER TABLE bars ADD COLUMN close_raw REAL", []);

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
    /// This is the authoritative source for intraday extremes and always wins
    /// on open/high/low — but never on the close. The snapshot is unadjusted
    /// throughout, while the EOD feed's close is adjusted for corporate
    /// actions; the adjusted one is the series that stays continuous across a
    /// split and the one the live quote agrees with, so an existing close is
    /// left alone and the raw one is kept beside it in `close_raw`. That pair
    /// is what [`Store::bars`] uses to put the range on the close's basis.
    pub fn put_historical(&self, day: &str, rows: &[HistoricalRow]) -> Result<()> {
        let ts = day_close_ts(day);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO bars
                     (symbol, day, ts, open, high, low, close, volume, hl_known, close_raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?7)
                 ON CONFLICT(symbol, day) DO UPDATE SET
                    ts       = excluded.ts,
                    open     = excluded.open,
                    high     = excluded.high,
                    low      = excluded.low,
                    volume   = excluded.volume,
                    hl_known = 1,
                    close_raw = excluded.close_raw",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.symbol, day, ts, r.open, r.high, r.low, r.close, r.volume
                ])?;
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![format!("historical:{day}"), SNAPSHOT_MARK],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read a symbol's bars oldest-first, optionally limited to the most
    /// recent `limit` sessions.
    ///
    /// Every bar comes back reconciled — see [`reconcile`] — so no caller has
    /// to know that PSX's two feeds quote different price bases.
    pub fn bars(&self, symbol: &str, limit: Option<usize>) -> Result<Vec<Bar>> {
        let conn = self.conn.lock().unwrap();
        let sql = match limit {
            Some(_) => {
                "SELECT ts, open, high, low, close, volume, close_raw FROM bars
                 WHERE symbol = ?1 ORDER BY day DESC LIMIT ?2"
            }
            None => {
                "SELECT ts, open, high, low, close, volume, close_raw FROM bars
                 WHERE symbol = ?1 ORDER BY day ASC"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        let map = |r: &rusqlite::Row| {
            let mut bar = Bar {
                ts: r.get(0)?,
                open: r.get(1)?,
                high: r.get(2)?,
                low: r.get(3)?,
                close: r.get(4)?,
                volume: r.get(5)?,
            };
            reconcile(&mut bar, r.get(6)?);
            Ok(bar)
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
    ///
    /// Unreconciled by design: this is a bookkeeping question about what has
    /// been fetched, not a price.
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

    /// Whether the whole-market snapshot for `day` has already been ingested,
    /// completely enough for what the cache now keeps.
    ///
    /// A day read by an older version counts as unread, so the rolling startup
    /// backfill repairs its own window without anyone asking — see
    /// [`SNAPSHOT_MARK`]. A holiday stays read: there is nothing to re-read.
    pub fn has_historical(&self, day: &str) -> Result<bool> {
        Ok(self
            .get_meta(&format!("historical:{day}"))?
            .is_some_and(|mark| mark == SNAPSHOT_MARK || mark == "empty"))
    }

    /// Whether a day is known to have traded, judged by the EOD series.
    ///
    /// This is the check that tells a public holiday apart from a throttled
    /// request: `/historical` answers both with the same empty table, but the
    /// EOD feed only ever carries a close for a session that actually happened.
    /// A day with bars is a trading day, full stop.
    pub fn day_traded(&self, day: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM bars WHERE day = ?1",
            params![day],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// How many of a symbol's bars carry true intraday high/low.
    ///
    /// Returns `(with_true_range, total)` over the whole cached series.
    /// Anything the `/historical` snapshot has not reached still has its
    /// high/low derived from open and close.
    pub fn hl_coverage(&self, symbol: &str) -> Result<(usize, usize)> {
        let conn = self.conn.lock().unwrap();
        let (known, total): (i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(hl_known), 0), COUNT(*) FROM bars WHERE symbol = ?1",
            params![symbol],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((known as usize, total as usize))
    }

    /// True-range coverage from a given timestamp's PSX trading day onward.
    /// Research with a long indicator warm-up can use this to judge only bars
    /// on which the strategy was actually eligible to trade.
    pub fn hl_coverage_since(&self, symbol: &str, from_ts: i64) -> Result<(usize, usize)> {
        let conn = self.conn.lock().unwrap();
        let from_day = trading_day(from_ts);
        let (known, total): (i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(hl_known), 0), COUNT(*) FROM bars
             WHERE symbol = ?1 AND day >= ?2",
            params![symbol, from_day],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((known as usize, total as usize))
    }

    /// Every day the EOD series says traded but that has no `/historical`
    /// snapshot yet, newest first.
    ///
    /// This drives the deep backfill, and it is derived from the bars already
    /// cached rather than from a calendar. That matters twice over: holidays
    /// never appear, so they are never requested and can never be mistaken for
    /// a throttled response — and the work is bounded by history actually held
    /// rather than by a date range guessed at.
    pub fn days_missing_true_range(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT b.day FROM bars b
             LEFT JOIN meta m ON m.key = 'historical:' || b.day
             WHERE m.key IS NULL OR m.value NOT IN (?1, 'empty')
             ORDER BY b.day DESC",
        )?;
        let days = stmt
            .query_map(params![SNAPSHOT_MARK], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(days)
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

    // --- external context ------------------------------------------------

    /// Store an external-context payload under `key` as a JSON blob.
    ///
    /// Generic because the three payloads — commodity series, headlines, policy
    /// rates — have nothing in common but their lifecycle, and a table per kind
    /// would be three schemas for one access pattern.
    pub fn put_external<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO external (key, json, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                json = excluded.json, fetched_at = excluded.fetched_at",
            params![key, serde_json::to_string(value)?, Utc::now().timestamp()],
        )?;
        Ok(())
    }

    /// Read an external-context payload. A blob that no longer deserialises —
    /// after a shape change — reads as absent rather than as an error.
    pub fn external<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let conn = self.conn.lock().unwrap();
        let json: Option<String> = conn
            .query_row(
                "SELECT json FROM external WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    /// When `key` was last written, as a Unix timestamp.
    pub fn external_fetched_at(&self, key: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT fetched_at FROM external WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Every cached company profile no older than `max_age`.
    ///
    /// Profiles are fetched one symbol at a time, on demand, so this returns
    /// whatever the user happens to have visited — a fraction of the market,
    /// not a survey of it. Callers that rank on the result are responsible for
    /// saying how much of the board it actually covers.
    ///
    /// Rows that fail to deserialise (a schema change against an old cache)
    /// are skipped rather than failing the whole read.
    pub fn companies(&self, max_age: Duration) -> Result<Vec<Company>> {
        let conn = self.conn.lock().unwrap();
        let cutoff = Utc::now().timestamp() - max_age.num_seconds();
        let mut stmt = conn.prepare("SELECT json FROM companies WHERE fetched_at >= ?1")?;
        let rows: Vec<String> = stmt
            .query_map(params![cutoff], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows
            .iter()
            .filter_map(|j| serde_json::from_str(j).ok())
            .collect())
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

    // -- reconciling the two feeds' price bases -----------------------------

    /// The real numbers that provoked this: STL on 2026-03-19, which PSX
    /// reported through `/historical` as 1267.00 / 1390.00 / 1150.00 / 1379.35
    /// and through the EOD feed as a close of 125.39 — the same session, before
    /// and after an eleven-for-one adjustment.
    #[test]
    fn a_snapshot_range_is_scaled_onto_the_adjusted_close() {
        let mut b = bar(0, 1267.0, 1390.0, 1150.0, 125.39, 2378.0);
        reconcile(&mut b, Some(1379.35));

        assert!(
            plausible(&b),
            "the reconciled bar must describe one session"
        );
        assert_eq!(b.close, 125.39, "the adjusted close is never touched");
        // The factor is 125.39 / 1379.35 ≈ 0.0909.
        assert!((b.open - 115.17).abs() < 0.01, "open: {}", b.open);
        assert!((b.high - 126.35).abs() < 0.01, "high: {}", b.high);
        assert!((b.low - 104.54).abs() < 0.01, "low: {}", b.low);
        // The shape of the session survives the move.
        assert!(
            ((b.high / b.low) - (1390.0 / 1150.0)).abs() < 1e-9,
            "the range kept its proportions"
        );
    }

    #[test]
    fn a_bar_on_one_basis_passes_through_untouched() {
        let before = bar(0, 41.71, 42.90, 41.30, 41.73, 209_549.0);
        let mut after = before;
        // Same close in both feeds: nothing was adjusted, factor is 1.
        reconcile(&mut after, Some(41.73));
        assert_eq!(after, before);

        let mut after = before;
        reconcile(&mut after, None);
        assert_eq!(after, before, "and with no snapshot read either");
    }

    /// The EOD feed alone gives an adjusted close beside an unadjusted open,
    /// and there is nothing in the row to compute a factor from. An eleven-fold
    /// candle is worse than no candle.
    #[test]
    fn an_impossible_bar_with_no_factor_keeps_only_its_close() {
        // open/high derived from the raw open, low/close from the adjusted one.
        let mut b = bar(0, 1267.0, 1267.0, 125.39, 125.39, 2378.0);
        reconcile(&mut b, None);

        assert_eq!(
            (b.open, b.high, b.low, b.close),
            (125.39, 125.39, 125.39, 125.39),
            "the session closed here; its range is unknown"
        );
        assert_eq!(b.volume, 2378.0, "volume is unaffected by any of this");
    }

    #[test]
    fn a_close_outside_its_own_range_is_not_plausible() {
        // The state the cache was left in by the two feeds overwriting one
        // another: a true range from the snapshot, an adjusted close from EOD.
        assert!(!plausible(&bar(0, 1278.0, 1280.0, 1250.99, 117.72, 10.0)));
        // A wide but possible session.
        assert!(plausible(&bar(0, 100.0, 110.0, 95.0, 108.0, 1.0)));
        // Wider than any circuit breaker allows: not one session's range.
        assert!(!plausible(&bar(0, 100.0, 300.0, 100.0, 300.0, 1.0)));
        // Zeroes and NaNs from suspended scrips.
        assert!(!plausible(&bar(0, 0.0, 0.0, 0.0, 0.0, 0.0)));
        assert!(!plausible(&bar(0, f64::NAN, 1.0, 1.0, 1.0, 0.0)));
    }

    /// Reconciliation must not run on a close there is no basis for.
    #[test]
    fn a_bar_with_no_usable_close_is_left_for_the_caller_to_reject() {
        let mut b = bar(0, 10.0, 12.0, 9.0, 0.0, 5.0);
        reconcile(&mut b, Some(11.0));
        assert_eq!(b.open, 10.0, "a zero close scales nothing to zero");
    }

    #[test]
    fn reconciliation_reaches_the_bars_a_caller_reads() {
        let s = Store::open_in_memory().unwrap();
        // The EOD feed lands first, with its adjusted close.
        s.put_eod_bars(
            "STL",
            &[bar(
                day_close_ts("2026-03-19"),
                1267.0,
                1267.0,
                125.39,
                125.39,
                2378.0,
            )],
        )
        .unwrap();
        // Then the snapshot, unadjusted throughout.
        s.put_historical(
            "2026-03-19",
            &[crate::psx::HistoricalRow {
                symbol: "STL".into(),
                ldcp: 1263.84,
                open: 1267.0,
                high: 1390.0,
                low: 1150.0,
                close: 1379.35,
                volume: 2378.0,
            }],
        )
        .unwrap();

        let bars = s.bars("STL", None).unwrap();
        assert_eq!(bars.len(), 1);
        let b = bars[0];
        assert_eq!(b.close, 125.39, "the snapshot must not restore a raw close");
        assert!(
            plausible(&b),
            "and the range comes back on that basis: {b:?}"
        );
        assert!((b.high - 126.35).abs() < 0.01, "high: {}", b.high);
    }

    // -- true-range coverage ------------------------------------------------

    #[test]
    fn day_traded_follows_the_eod_series_not_the_calendar() {
        // This is what tells a public holiday apart from a throttled request:
        // `/historical` answers both with an empty table, but the EOD feed
        // only carries a close for a session that happened.
        let s = Store::open_in_memory().unwrap();
        s.put_eod_bars(
            "OGDC",
            &[bar(day_close_ts("2024-03-12"), 1.0, 1.0, 1.0, 1.0, 10.0)],
        )
        .unwrap();

        assert!(s.day_traded("2024-03-12").unwrap());
        assert!(!s.day_traded("2024-03-13").unwrap());
    }

    #[test]
    fn hl_coverage_counts_only_snapshot_backed_bars() {
        let s = Store::open_in_memory().unwrap();
        s.put_eod_bars(
            "OGDC",
            &[
                bar(day_close_ts("2024-03-11"), 1.0, 1.0, 1.0, 1.0, 10.0),
                bar(day_close_ts("2024-03-12"), 1.0, 1.0, 1.0, 1.0, 10.0),
            ],
        )
        .unwrap();
        assert_eq!(s.hl_coverage("OGDC").unwrap(), (0, 2));

        // A snapshot upgrades one of them to a true intraday range.
        s.put_historical("2024-03-12", &[hist("OGDC", 1.0, 1.5, 0.5, 1.2, 10.0)])
            .unwrap();
        assert_eq!(s.hl_coverage("OGDC").unwrap(), (1, 2));
    }

    #[test]
    fn days_missing_true_range_lists_traded_days_without_a_snapshot() {
        let s = Store::open_in_memory().unwrap();
        s.put_eod_bars(
            "OGDC",
            &[
                bar(day_close_ts("2024-03-11"), 1.0, 1.0, 1.0, 1.0, 10.0),
                bar(day_close_ts("2024-03-12"), 1.0, 1.0, 1.0, 1.0, 10.0),
            ],
        )
        .unwrap();

        assert_eq!(
            s.days_missing_true_range().unwrap(),
            vec!["2024-03-12".to_string(), "2024-03-11".to_string()]
        );

        s.put_historical("2024-03-12", &[hist("OGDC", 1.0, 1.5, 0.5, 1.2, 10.0)])
            .unwrap();
        assert_eq!(
            s.days_missing_true_range().unwrap(),
            vec!["2024-03-11".to_string()]
        );
    }

    /// A snapshot read by an older version left no raw close behind, so the
    /// adjustment factor for that session is unrecoverable until it is read
    /// again. The marker's version is what says so.
    #[test]
    fn snapshots_read_before_raw_closes_were_kept_are_offered_again() {
        let s = Store::open_in_memory().unwrap();
        s.put_eod_bars(
            "OGDC",
            &[bar(day_close_ts("2024-03-12"), 1.0, 1.0, 1.0, 1.0, 10.0)],
        )
        .unwrap();
        s.put_historical("2024-03-12", &[hist("OGDC", 1.0, 1.5, 0.5, 1.2, 10.0)])
            .unwrap();
        assert!(s.days_missing_true_range().unwrap().is_empty());

        // Exactly what an older cache holds.
        s.set_meta("historical:2024-03-12", "1").unwrap();
        assert_eq!(
            s.days_missing_true_range().unwrap(),
            vec!["2024-03-12".to_string()],
            "an old mark must not pass for a complete snapshot"
        );

        // A holiday, though, has nothing to re-read however it was marked.
        s.mark_historical_empty("2024-03-12").unwrap();
        assert!(s.days_missing_true_range().unwrap().is_empty());
    }

    #[test]
    fn a_day_marked_empty_is_never_relisted() {
        // Holidays are recorded once and then stay out of the work queue.
        let s = Store::open_in_memory().unwrap();
        s.put_eod_bars(
            "OGDC",
            &[bar(day_close_ts("2024-03-11"), 1.0, 1.0, 1.0, 1.0, 10.0)],
        )
        .unwrap();
        s.mark_historical_empty("2024-03-11").unwrap();
        assert!(s.days_missing_true_range().unwrap().is_empty());
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
