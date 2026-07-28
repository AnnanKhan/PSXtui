# psxtui

A terminal client for **Pakistan Stock Exchange** market data — live quotes, a
full-market screener, candlestick charts with technical indicators, risk and
return analytics, company fundamentals, and intraday microstructure.

```
┌ PRL — Pakistan Refinery Limited ──────────────────────────────────────────────┐
│53.14  +0.60 (+1.14%)  O 52.50  H 54.65  L 51.51  C 53.14  Vol 24.6M           │
│ Range 1M 3M 6M 1Y 3Y MAX   Overlays SMA20 EMA50 BB20   125 sessions           │
└───────────────────────────────────────────────────────────────────────────────┘
```

## Screens

| # | Screen | What it shows |
|---|--------|---------------|
| 1 | **Dashboard** | Market breadth, top gainers/losers, most active by value, scrollable sector heatmap |
| 2 | **Screener** | Every listed scrip — sortable and filterable by symbol, name or sector, plus valuation columns |
| 3 | **Chart** | Candlesticks with SMA/EMA/Bollinger/Donchian/Ichimoku, and a Volume / RSI / MACD / ATR / Stochastic / ADX / CCI / Williams %R pane |
| 4 | **Analysis** | Returns by window, annualized return & volatility, Sharpe, Sortino, max drawdown, beta and correlation vs KSE100 |
| 5 | **Company** | Business profile, key people, equity structure, annual & quarterly financials, ratios, announcements |
| 6 | **Intraday** | Session price with VWAP, 15-minute volume distribution, live trade tape |
| 7 | **Compare** | 2-4 scrips side by side — rebased performance overlay, risk table, correlation matrix |
| 8 | **Seasonality** | Month-by-year return grid, day-of-week effects, return distribution, streaks |
| 9 | **Macro** | Oil, gold, cotton, USD/PKR, freight proxy, SBP policy rate and business news — with correlation to the selected scrip |

Timeframes: 5D, 1M, 3M, 6M, YTD, 1Y, 2Y, 3Y, 5Y and MAX.

## Install

```sh
cargo build --release
./target/release/psxtui
```

Requires a Rust toolchain (2024 edition) and a terminal with 256-colour and
Unicode support. No API key or account is needed.

## Keys

Press `?` in the app for the full list.

| Key | Action |
|-----|--------|
| `1`–`6`, `Tab` | Switch screen |
| `/` | Search by symbol, company or sector |
| `j`/`k`, `↑`/`↓` | Move cursor · `Enter` opens in the chart |
| `s` / `S` | Cycle sort column / reverse |
| `W` / `e` | Watchlist only / equities only |
| `w` | Add or remove the current symbol from the watchlist |
| `[` / `]` | Chart range · `i` cycles the indicator pane |
| `c` | Chart style: candles → line → dots → area |
| `m` / `e` / `b` | Toggle SMA / EMA / Bollinger overlays |
| `r` | Refresh · `q` quit |

## How it gets data

PSX publishes no API, so `psxtui` reads the public data portal at
`dps.psx.com.pk` — two JSON feeds plus scraped HTML:

| Source | Used for |
|--------|----------|
| `/symbols` | Master list of instruments, sector names, ETF/debt flags |
| `/market-watch` | Live board — OHLC, change, volume for every scrip |
| `/timeseries/eod/<SYM>` | Long-run daily history (also works for indices like `KSE100`) |
| `/timeseries/int/<SYM>` | Intraday trade ticks |
| `POST /historical` | Whole-market OHLC for one date — the only source of true daily high/low |
| `/company/<SYM>` | Profile, financials, ratios, announcements |

### External context (Macro screen)

All unauthenticated, no API keys:

| Source | Used for |
|--------|----------|
| Yahoo Finance chart API | Brent, WTI, gold, cotton, natural gas, USD/PKR, S&P 500 |
| Yahoo Finance (`BDRY`) | Dry-bulk freight — a **proxy** ETF, not the Baltic Dry Index, which isn't freely available |
| Business Recorder / Dawn RSS | Business and market headlines, matched to the selected scrip |
| `sbp.org.pk` | SBP policy rate, which feeds the risk-free rate in Sharpe and Sortino |

Correlations against these are computed on **date-aligned** returns — PSX and
global markets keep different holiday calendars, so the series are intersected
by trading day before anything is compared.

Port throughput and trade-flow volumes were investigated and dropped: neither
Karachi Port Trust nor Port Qasim publishes a machine-readable feed, and
inventing a number is worse than omitting one.

Two details worth knowing:

- **The EOD feed has no high or low.** It returns `[timestamp, close, volume, open]`
  only. Real intraday extremes come from the daily `/historical` snapshot, which
  covers every symbol in a single request. The cache merges the two and a
  derived range is never allowed to overwrite a true one — so ATR and
  candlestick wicks are honest.
- **Market-watch reports sector *codes*** (`0825`), not names. These are joined
  against `/symbols` so every screen can show `COMMERCIAL BANKS`.

### Caching

Everything fetched is persisted to SQLite (`~/.local/share/psxtui/psx.db`), so
the app opens instantly on cached data, analysis runs offline, and history
accumulates over time. On first run it backfills ~120 days of true OHLC in the
background — one request per trading day, marking holidays so they are never
refetched. Backfill runs on its own task and never blocks an interactive load.

Requests are deliberately paced (one at a time, ≥350 ms apart, bounded retries)
so the tool behaves like a single person browsing rather than a crawler.

## Architecture

```
src/
  psx/        HTTP client + JSON feeds + HTML scrapers (one module per source)
  cache/      SQLite store; merges EOD and /historical into one bar series
  analysis/   indicators.rs (SMA/EMA/RSI/MACD/Bollinger/ATR/OBV/VWAP/Stochastic)
              stats.rs      (returns, volatility, Sharpe, Sortino, drawdown, beta)
  data.rs     background worker — owns every network call and cache write
  app.rs      all application state and key handling
  ui/         one module per screen; rendering is a pure function of `App`
```

Two invariants the code depends on:

1. **Indicator alignment.** Every indicator returns a `Vec<Option<f64>>` the
   same length as its input, with `None` for the warm-up window — so overlays
   zip straight onto the price series with no offset bookkeeping.
2. **No poisoned floats.** PSX data is full of thin scrips, limit-locked
   sessions and zero-volume days. No analysis function panics or returns `NaN`
   or infinity; degenerate cases collapse to documented sentinels.

Charts aggregate bars into one candle per terminal column (first open, last
close, extreme high/low, summed volume) rather than dropping sessions, while
indicators stay computed on the *daily* series — so `SMA(20)` means twenty
sessions at every zoom level.

## Development

```sh
cargo test                     # unit tests, no network
cargo run --example live_smoke # exercises every parser against the live portal
```

`live_smoke` is the one that catches a PSX layout change — unit tests only prove
the parsers handle markup we wrote ourselves.

## Notes

Data is sourced from the PSX data portal for personal use. PSX's terms of use
restrict systematic retrieval; the client is rate-limited and caches
aggressively to stay well within the behaviour of an ordinary browser, but you
are responsible for how you use it. Nothing here is investment advice.
