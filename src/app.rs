//! Application state and input handling.
//!
//! The UI is a pure function of [`App`]: the event loop mutates state here and
//! [`crate::ui`] renders it. Network work never happens on this path — screens
//! post [`DataRequest`]s to a background worker and receive [`DataEvent`]s
//! back, so the interface stays responsive while PSX is slow.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc::UnboundedSender;

use crate::cache::Store;
use crate::ext::{Headline, MacroRates, MacroSeries};
use crate::model::{Bar, Company, Index, Quote, SymbolInfo, Tick};
use crate::ui::hit::{HitMap, Target, Zone};

/// The benchmark every risk statistic is measured against.
pub const BENCHMARK: &str = "KSE100";

/// How long a symbol's live data stays fresh before revisiting the network.
///
/// Revisiting a symbol within this window is served entirely from the local
/// cache — matching the market board's own refresh cadence, so nothing on
/// screen is more stale than the quotes beside it.
const SYMBOL_TTL: Duration = Duration::from_secs(60);

/// How long the cursor must sit still before a symbol's data is fetched.
///
/// Scrolling a list selects every row it passes. Fetching each one would queue
/// three requests per row against a rate limiter, so a quick scroll leaves the
/// worker minutes behind the cursor. Cached data still paints on every move —
/// only the network call waits for the cursor to settle.
const LOAD_DEBOUNCE: Duration = Duration::from_millis(250);

/// How long a cached company profile is reused. Filings land a few times a
/// quarter, so a day is generous.
const COMPANY_TTL_DAYS: i64 = 1;

// --- messages ------------------------------------------------------------

/// Work the UI asks the background worker to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataRequest {
    /// Refresh the market-watch board and index ticker.
    RefreshMarket,
    /// Load price history (and intraday ticks) for a symbol.
    LoadSymbol(String),
    /// Load the company drill-down for a symbol.
    LoadCompany(String),
    /// Backfill true-OHLC daily snapshots for the last N calendar days.
    Backfill(i64),
    /// Refresh external market context: commodities/FX, headlines, policy rate.
    RefreshExternal,
}

/// Results flowing back from the background worker.
#[derive(Debug)]
pub enum DataEvent {
    Symbols(Vec<SymbolInfo>),
    Quotes(Vec<Quote>),
    Indices(Vec<Index>),
    Bars {
        symbol: String,
        bars: Vec<Bar>,
    },
    Ticks {
        symbol: String,
        ticks: Vec<Tick>,
    },
    Company(Box<Company>),
    Status(String),
    Error(String),
    /// A named unit of background work started — shown live in the status bar
    /// so the user can see *what* is loading, not merely that something is.
    Begin(String),
    /// That unit finished. Carries the same label so concurrent work (an
    /// interactive load racing the backfill) unwinds in any order.
    End(String),
    /// Backfill advanced. Reported separately from [`DataEvent::Begin`] because
    /// it is long-running and deserves a progress bar rather than a spinner.
    Backfill(BackfillProgress),
    /// Backfill finished, or had nothing to do.
    BackfillDone,
    /// Commodity, FX and global-index series for the Macro screen.
    MacroSeries(Vec<MacroSeries>),
    /// Business headlines, newest first.
    Headlines(Vec<Headline>),
    /// Policy rates — the risk-free rate the analysis screens measure against.
    Rates(MacroRates),
}

/// How far the whole-market OHLC backfill has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillProgress {
    pub done: usize,
    pub total: usize,
    /// The trading day currently being ingested.
    pub day: String,
    /// Symbols written for that day; `None` marks a market holiday.
    pub rows: Option<usize>,
}

impl BackfillProgress {
    pub fn ratio(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
    }
}

// --- screens -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Screener,
    Chart,
    Analysis,
    Company,
    Intraday,
    Compare,
    Seasonality,
    Macro,
}

impl Screen {
    pub const ALL: [Screen; 9] = [
        Screen::Dashboard,
        Screen::Screener,
        Screen::Chart,
        Screen::Analysis,
        Screen::Company,
        Screen::Intraday,
        Screen::Compare,
        Screen::Seasonality,
        Screen::Macro,
    ];

    pub fn title(&self) -> &'static str {
        match self {
            Screen::Dashboard => "Dashboard",
            Screen::Screener => "Screener",
            Screen::Chart => "Chart",
            Screen::Analysis => "Analysis",
            Screen::Company => "Company",
            Screen::Intraday => "Intraday",
            Screen::Compare => "Compare",
            Screen::Seasonality => "Seasonality",
            Screen::Macro => "Macro",
        }
    }

    pub fn index(&self) -> usize {
        Screen::ALL.iter().position(|s| s == self).unwrap_or(0)
    }
}

// --- per-screen state ----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Symbol,
    Price,
    Change,
    Volume,
    Turnover,
    // --- valuation view ---
    Pe,
    Eps,
    EpsGrowth,
    NetMargin,
    MarketCap,
    FreeFloat,
}

impl SortKey {
    /// Cycled by `s` on the price board.
    pub const ALL: [SortKey; 5] = [
        SortKey::Symbol,
        SortKey::Price,
        SortKey::Change,
        SortKey::Volume,
        SortKey::Turnover,
    ];

    /// Cycled by `s` in the valuation view — the fundamentals columns, plus
    /// the symbol so there is always an alphabetical fallback.
    pub const VALUATION: [SortKey; 7] = [
        SortKey::Symbol,
        SortKey::MarketCap,
        SortKey::Pe,
        SortKey::Eps,
        SortKey::EpsGrowth,
        SortKey::NetMargin,
        SortKey::FreeFloat,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            SortKey::Symbol => "Symbol",
            SortKey::Price => "Price",
            SortKey::Change => "Change %",
            SortKey::Volume => "Volume",
            SortKey::Turnover => "Turnover",
            SortKey::Pe => "P/E",
            SortKey::Eps => "EPS",
            SortKey::EpsGrowth => "EPS growth",
            SortKey::NetMargin => "Net margin",
            SortKey::MarketCap => "Market cap",
            SortKey::FreeFloat => "Free float",
        }
    }

    /// Whether this key ranks a fundamentals column rather than a price one.
    pub fn is_valuation(&self) -> bool {
        SortKey::VALUATION.contains(self) && *self != SortKey::Symbol
    }
}

/// One of the dashboard's leaderboards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    Gainers,
    Losers,
    Active,
}

impl Board {
    pub fn title(&self) -> &'static str {
        match self {
            Board::Gainers => "Top Gainers",
            Board::Losers => "Top Losers",
            Board::Active => "Most Active",
        }
    }
}

/// What the dashboard last drew.
///
/// Which boards fit, and how many rows each holds, are decisions the renderer
/// makes from the available area. Key handling needs the same answer to keep
/// the cursor on a row that is actually visible, so the renderer publishes it
/// here after each frame.
#[derive(Debug, Clone, Copy)]
pub struct DashLayout {
    pub boards: [Board; 3],
    /// How many of `boards` were drawn.
    pub count: usize,
    /// Rows available inside each board.
    pub rows: usize,
    /// Rows the sector heatmap can draw at once.
    pub sector_rows: usize,
}

impl Default for DashLayout {
    fn default() -> Self {
        Self {
            boards: [Board::Gainers, Board::Losers, Board::Active],
            count: 3,
            rows: 0,
            sector_rows: 0,
        }
    }
}

/// One sector's aggregate performance across the board.
#[derive(Debug, Clone, PartialEq)]
pub struct SectorAgg {
    pub name: String,
    /// Turnover-weighted average change %, falling back to a plain mean when
    /// nothing in the sector traded.
    pub avg_pct: f64,
    pub turnover: f64,
    pub count: usize,
}

/// How long two clicks on the same target may be apart and still count as a
/// double-click. Terminals do not report double-clicks, so it is timed here.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Rows moved per wheel notch.
const WHEEL_LINES: usize = 3;

/// Which Macro panel the keyboard is driving.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MacroFocus {
    #[default]
    Series,
    News,
}

/// Which half of the dashboard the keyboard is driving.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DashFocus {
    #[default]
    Boards,
    Sectors,
}

/// Cursor position on the dashboard: which board, and which row within it,
/// plus a separate cursor for the sector heatmap.
#[derive(Debug, Default)]
pub struct DashboardState {
    pub board: usize,
    pub cursor: usize,
    pub focus: DashFocus,
    /// Row of the sector heatmap under the cursor.
    pub sector: usize,
    /// First sector row drawn, for scrolling a list taller than the pane.
    pub sector_offset: usize,
}

#[derive(Debug)]
pub struct ScreenerState {
    pub sort: SortKey,
    /// Descending is the useful default for every column except the symbol.
    pub descending: bool,
    pub cursor: usize,
    pub offset: usize,
    /// Restrict the board to watchlist members.
    pub watchlist_only: bool,
    /// Hide debt instruments and ETFs, which dominate the raw symbol list.
    pub equities_only: bool,
    /// Swap the OHLC columns for fundamentals (P/E, EPS, margins, market cap).
    ///
    /// Company profiles are fetched on demand, so this view is only ever as
    /// complete as the local cache — see [`App::fundamentals`].
    pub valuation: bool,
}

impl Default for ScreenerState {
    fn default() -> Self {
        Self {
            sort: SortKey::Turnover,
            descending: true,
            cursor: 0,
            offset: 0,
            watchlist_only: false,
            equities_only: true,
            valuation: false,
        }
    }
}

/// One row of the screener's valuation view, reduced from a cached [`Company`].
///
/// Every field is optional and independently so: PSX publishes a P/E for some
/// scrips and not others, banks report "Mark-up Earned" where industrials
/// report "Sales", and a newly listed company has no prior year to grow from.
/// Nothing here is ever imputed — a missing number stays missing all the way to
/// the screen.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Valuation {
    /// Price / earnings, as published on the company quote tab.
    pub pe: Option<f64>,
    /// Earnings per share for the most recent *annual* period.
    pub eps: Option<f64>,
    /// Year-on-year EPS growth, in percent.
    pub eps_growth_pct: Option<f64>,
    /// Net profit margin, in percent.
    pub net_margin_pct: Option<f64>,
    /// Market capitalisation in **PKR**. PSX publishes it in thousands; the
    /// conversion happens once, here, so no renderer has to remember.
    pub market_cap: Option<f64>,
    /// Free float as a percentage of shares outstanding.
    pub free_float_pct: Option<f64>,
}

impl Valuation {
    /// Reduce a cached company profile to its valuation row.
    pub fn from_company(c: &Company) -> Self {
        // The financials and ratios tables are published newest column first.
        let latest = c.financials_annual.first();
        let prior = c.financials_annual.get(1);
        let ratios = c.ratios.first();

        // PSX already computes both of these; recomputing from the raw rows
        // would mean guessing at sector-specific labels, so prefer the
        // published ratio and only fall back where it is absent.
        let eps_growth_pct = ratio_row(ratios, "EPS Growth").or_else(|| {
            let (now, then) = (latest?.eps()?, prior?.eps()?);
            if then == 0.0 {
                // Growth from nothing is undefined, not infinite.
                return None;
            }
            Some((now - then) / then.abs() * 100.0)
        });

        // Free float is published as a percentage for most scrips and as a
        // share count for the rest.
        let free_float_pct = c.free_float_pct.or_else(|| {
            let (float, shares) = (c.free_float?, c.shares?);
            if shares > 0.0 {
                Some(float / shares * 100.0)
            } else {
                None
            }
        });

        Self {
            pe: finite_opt(c.pe_ratio),
            eps: finite_opt(latest.and_then(|p| p.eps())),
            eps_growth_pct: finite_opt(eps_growth_pct),
            net_margin_pct: finite_opt(ratio_row(ratios, "Net Profit Margin")),
            market_cap: finite_opt(c.market_cap_000.map(|v| v * 1_000.0)),
            free_float_pct: finite_opt(free_float_pct),
        }
    }

    /// The value a valuation [`SortKey`] ranks on, if this row has it.
    pub fn key(&self, sort: SortKey) -> Option<f64> {
        match sort {
            SortKey::Pe => self.pe,
            SortKey::Eps => self.eps,
            SortKey::EpsGrowth => self.eps_growth_pct,
            SortKey::NetMargin => self.net_margin_pct,
            SortKey::MarketCap => self.market_cap,
            SortKey::FreeFloat => self.free_float_pct,
            _ => None,
        }
    }
}

/// Look up a ratio row by label prefix.
///
/// PSX suffixes the unit onto the label ("Net Profit Margin (%)"), and the
/// exact wording drifts between sectors, so matching is by prefix rather than
/// equality.
fn ratio_row(period: Option<&crate::model::RatioPeriod>, prefix: &str) -> Option<f64> {
    period?
        .rows
        .iter()
        .find(|(k, _)| {
            k.trim()
                .to_ascii_lowercase()
                .starts_with(&prefix.to_ascii_lowercase())
        })
        .map(|(_, v)| *v)
}

/// Drop a value that is present but not a usable number.
fn finite_opt(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite())
}

/// How much history the chart shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    D5,
    M1,
    M3,
    M6,
    Ytd,
    Y1,
    Y2,
    Y3,
    Y5,
    Max,
}

impl Range {
    pub const ALL: [Range; 10] = [
        Range::D5,
        Range::M1,
        Range::M3,
        Range::M6,
        Range::Ytd,
        Range::Y1,
        Range::Y2,
        Range::Y3,
        Range::Y5,
        Range::Max,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Range::D5 => "5D",
            Range::M1 => "1M",
            Range::M3 => "3M",
            Range::M6 => "6M",
            Range::Ytd => "YTD",
            Range::Y1 => "1Y",
            Range::Y2 => "2Y",
            Range::Y3 => "3Y",
            Range::Y5 => "5Y",
            Range::Max => "MAX",
        }
    }

    /// Trading sessions to display.
    ///
    /// `None` means the window is not a fixed count — `Max` shows everything
    /// and `Ytd` is bounded by the calendar, not a session count.
    pub fn sessions(&self) -> Option<usize> {
        match self {
            Range::D5 => Some(5),
            Range::M1 => Some(22),
            Range::M3 => Some(65),
            Range::M6 => Some(125),
            Range::Y1 => Some(250),
            Range::Y2 => Some(500),
            Range::Y3 => Some(750),
            Range::Y5 => Some(1250),
            Range::Ytd | Range::Max => None,
        }
    }
}

/// The indicator shown in the lower pane of the chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Volume,
    Rsi,
    Macd,
    Atr,
    Stochastic,
    Adx,
    Cci,
    WilliamsR,
}

impl Pane {
    pub const ALL: [Pane; 8] = [
        Pane::Volume,
        Pane::Rsi,
        Pane::Macd,
        Pane::Atr,
        Pane::Stochastic,
        Pane::Adx,
        Pane::Cci,
        Pane::WilliamsR,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Pane::Volume => "Volume",
            Pane::Rsi => "RSI(14)",
            Pane::Macd => "MACD(12,26,9)",
            Pane::Atr => "ATR(14)",
            Pane::Stochastic => "Stoch(14,3)",
            Pane::Adx => "ADX(14) +DI/-DI",
            Pane::Cci => "CCI(20)",
            Pane::WilliamsR => "Williams %R(14)",
        }
    }
}

#[derive(Debug)]
pub struct ChartState {
    pub range: Range,
    pub pane: Pane,
    pub show_sma: bool,
    pub show_ema: bool,
    pub show_bollinger: bool,
    /// Donchian channel — the rolling 20-session high/low envelope.
    pub show_donchian: bool,
    /// Ichimoku cloud plus its conversion and base lines.
    pub show_ichimoku: bool,
    /// Horizontal support/resistance levels clustered from swing pivots.
    pub show_levels: bool,
    /// How the price series itself is drawn.
    pub style: ChartStyle,
}

impl Default for ChartState {
    fn default() -> Self {
        Self {
            range: Range::M6,
            pane: Pane::Volume,
            show_sma: true,
            show_ema: false,
            show_bollinger: false,
            show_donchian: false,
            show_ichimoku: false,
            show_levels: false,
            style: ChartStyle::Candles,
        }
    }
}

/// How the price series is drawn.
///
/// The distinction between [`ChartStyle::Line`] and [`ChartStyle::Dots`] is the
/// canvas marker, not the geometry. Braille packs 2x4 sub-cells into every
/// character, which is what makes candle wicks precise — but a thin diagonal
/// line drawn that way lights isolated sub-cells and reads as a dotted trail.
/// Half-blocks give a solid, continuous stroke at the cost of horizontal
/// resolution, which a close-only line does not need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartStyle {
    Candles,
    /// Solid continuous line through the closes.
    Line,
    /// Fine braille line — higher resolution, but reads as dots.
    Dots,
    /// Solid line with the area beneath it filled.
    Area,
}

impl ChartStyle {
    pub const ALL: [ChartStyle; 4] = [
        ChartStyle::Candles,
        ChartStyle::Line,
        ChartStyle::Dots,
        ChartStyle::Area,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            ChartStyle::Candles => "Candles",
            ChartStyle::Line => "Line",
            ChartStyle::Dots => "Dots",
            ChartStyle::Area => "Area",
        }
    }

    /// Candles need braille's sub-cell precision for wicks; the solid styles
    /// deliberately trade that away for a continuous stroke.
    pub fn marker(&self) -> ratatui::symbols::Marker {
        use ratatui::symbols::Marker;
        match self {
            ChartStyle::Candles | ChartStyle::Dots => Marker::Braille,
            ChartStyle::Line | ChartStyle::Area => Marker::HalfBlock,
        }
    }

    pub fn next(&self) -> ChartStyle {
        let i = Self::ALL.iter().position(|s| s == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

/// The most symbols the Compare screen will overlay at once.
///
/// Four distinguishable colours is about as much as one set of axes carries
/// before the overlay stops being readable.
pub const MAX_COMPARE: usize = 4;

/// The Compare screen's symbol set and window.
///
/// `symbols` empty means "not chosen yet" — the screen then seeds itself from
/// the watchlist (see [`App::compare_symbols`]) so it is useful before the user
/// touches it.
#[derive(Debug)]
pub struct CompareState {
    pub symbols: Vec<String>,
    pub range: Range,
}

impl Default for CompareState {
    fn default() -> Self {
        Self {
            symbols: Vec::new(),
            range: Range::Y1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanyTab {
    Profile,
    Financials,
    Ratios,
    Announcements,
}

impl CompanyTab {
    pub const ALL: [CompanyTab; 4] = [
        CompanyTab::Profile,
        CompanyTab::Financials,
        CompanyTab::Ratios,
        CompanyTab::Announcements,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            CompanyTab::Profile => "Profile",
            CompanyTab::Financials => "Financials",
            CompanyTab::Ratios => "Ratios",
            CompanyTab::Announcements => "Announcements",
        }
    }
}

// --- app -----------------------------------------------------------------

pub struct App {
    pub store: Arc<Store>,
    pub tx: UnboundedSender<DataRequest>,
    pub should_quit: bool,

    pub screen: Screen,
    pub quotes: Vec<Quote>,
    pub indices: Vec<Index>,
    pub symbols: BTreeMap<String, SymbolInfo>,

    /// The symbol every symbol-scoped screen renders.
    pub selected: String,
    pub bars: Vec<Bar>,
    pub ticks: Vec<Tick>,
    pub company: Option<Company>,
    /// Benchmark history for beta and relative performance.
    pub benchmark: Vec<Bar>,

    pub screener: ScreenerState,
    pub dashboard: DashboardState,
    /// Published by the dashboard renderer each frame; read by key handling.
    pub dash_layout: Cell<DashLayout>,
    /// What the renderer drew and where, for mouse hit-testing.
    pub hits: RefCell<HitMap>,
    /// The last click, for detecting a double-click.
    last_click: Option<(Target, Instant)>,
    /// Whether the terminal is reporting mouse events. Toggled with `M` so the
    /// terminal's own text selection can be used.
    pub mouse_enabled: bool,
    pub chart: ChartState,
    pub compare: CompareState,
    pub company_tab: CompanyTab,
    pub announcement_cursor: usize,

    /// Commodity, FX and global-index context for the Macro screen.
    pub macro_series: Vec<MacroSeries>,
    /// Business headlines, newest first.
    pub headlines: Vec<Headline>,
    /// Live policy rates. `None` until the first fetch or cache read lands.
    pub rates: Option<MacroRates>,
    /// First headline drawn, so a long list can be scrolled on the Macro screen.
    pub news_offset: usize,
    /// First row drawn in the Macro screen's series list, for scrolling a
    /// catalogue taller than the panel.
    pub series_offset: usize,
    /// Which Macro panel the keyboard drives.
    pub macro_focus: MacroFocus,

    /// Valuation rows for the symbols whose company profile is cached locally.
    ///
    /// Deliberately sparse: profiles are fetched per symbol on demand, so this
    /// covers only what the user has visited plus whatever earlier sessions
    /// left behind. The screener reports the coverage rather than presenting a
    /// partial ranking as a complete one.
    pub fundamentals: HashMap<String, Valuation>,

    pub watchlist: BTreeSet<String>,
    /// `Some` while the search prompt is open.
    pub search: Option<String>,
    /// Restrict the screener to one sector, set by drilling through the
    /// dashboard heatmap. Cleared with Esc.
    pub sector_filter: Option<String>,
    pub show_help: bool,
    pub status: String,
    pub error: Option<String>,
    /// Labels of the background work currently in flight, oldest first, so the
    /// status bar can name what it is waiting on.
    pub activities: Vec<String>,
    /// Progress of the long-running OHLC backfill, if it is running.
    pub backfill: Option<BackfillProgress>,
    /// Animation frame for the busy spinner.
    pub spinner: usize,
    /// When each symbol last went to the network, so revisits are served from
    /// the cache instead of refetching.
    refreshed: HashMap<String, Instant>,
    /// A fetch waiting for the cursor to settle, and when it was queued.
    pending_load: Option<(String, Instant)>,
    /// The selection changed and its cached data has not been read yet.
    needs_cache_load: bool,
}

impl App {
    pub fn new(store: Arc<Store>, tx: UnboundedSender<DataRequest>) -> Self {
        let watchlist = load_watchlist(&store);
        let fundamentals = load_fundamentals(&store);
        Self {
            store,
            tx,
            should_quit: false,
            screen: Screen::Dashboard,
            quotes: Vec::new(),
            indices: Vec::new(),
            symbols: BTreeMap::new(),
            selected: String::new(),
            bars: Vec::new(),
            ticks: Vec::new(),
            company: None,
            benchmark: Vec::new(),
            screener: ScreenerState::default(),
            dashboard: DashboardState::default(),
            dash_layout: Cell::new(DashLayout::default()),
            hits: RefCell::new(HitMap::default()),
            last_click: None,
            mouse_enabled: true,
            chart: ChartState::default(),
            compare: CompareState::default(),
            company_tab: CompanyTab::Profile,
            announcement_cursor: 0,
            macro_series: Vec::new(),
            headlines: Vec::new(),
            rates: None,
            news_offset: 0,
            series_offset: 0,
            macro_focus: MacroFocus::default(),
            fundamentals,
            watchlist,
            search: None,
            sector_filter: None,
            show_help: false,
            status: "Loading market data…".into(),
            error: None,
            activities: Vec::new(),
            backfill: None,
            spinner: 0,
            refreshed: HashMap::new(),
            pending_load: None,
            needs_cache_load: false,
        }
    }

    pub fn request(&mut self, req: DataRequest) {
        let _ = self.tx.send(req);
    }

    /// Whether anything is loading right now.
    pub fn is_busy(&self) -> bool {
        !self.activities.is_empty() || self.backfill.is_some()
    }

    /// The current spinner glyph.
    pub fn spinner_glyph(&self) -> char {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        FRAMES[self.spinner % FRAMES.len()]
    }

    /// Advance the spinner. Called on a timer while work is in flight.
    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    // --- data events -----------------------------------------------------

    pub fn on_event(&mut self, ev: DataEvent) {
        match ev {
            DataEvent::Symbols(syms) => {
                self.symbols = syms.into_iter().map(|s| (s.symbol.clone(), s)).collect();
                self.resolve_sectors();
            }
            DataEvent::Quotes(q) => {
                self.quotes = q;
                self.resolve_sectors();
                if self.selected.is_empty() {
                    // Open on the most active scrip so the app is useful
                    // before the user picks anything.
                    if let Some(top) = self
                        .quotes
                        .iter()
                        .max_by(|a, b| a.turnover().total_cmp(&b.turnover()))
                    {
                        let sym = top.symbol.clone();
                        self.select(sym);
                    }
                }
                self.status = format!("{} symbols quoted", self.quotes.len());
            }
            DataEvent::Indices(i) => self.indices = i,
            DataEvent::Bars { symbol, bars } => {
                if symbol == BENCHMARK {
                    self.benchmark = bars;
                } else if symbol == self.selected {
                    self.bars = bars;
                }
            }
            DataEvent::Ticks { symbol, ticks } => {
                if symbol == self.selected {
                    self.ticks = ticks;
                }
            }
            DataEvent::Company(c) => {
                // Every profile that lands widens the valuation view's
                // coverage, whether or not it is the symbol on screen.
                if !c.symbol.is_empty() {
                    self.fundamentals
                        .insert(c.symbol.clone(), Valuation::from_company(&c));
                }
                if c.symbol == self.selected {
                    self.announcement_cursor = 0;
                    self.company = Some(*c);
                }
            }
            DataEvent::Status(s) => self.status = s,
            DataEvent::Error(e) => self.error = Some(e),
            DataEvent::Begin(label) => self.activities.push(label),
            DataEvent::End(label) => {
                // Remove one matching entry: the same label can legitimately be
                // in flight twice (a re-selected symbol), and dropping all of
                // them would clear the indicator while work is still running.
                if let Some(i) = self.activities.iter().position(|a| *a == label) {
                    self.activities.remove(i);
                }
            }
            DataEvent::Backfill(p) => self.backfill = Some(p),
            DataEvent::BackfillDone => self.backfill = None,
            DataEvent::MacroSeries(series) => self.macro_series = series,
            DataEvent::Headlines(items) => {
                // A shorter list must not strand the viewport past its end.
                self.headlines = items;
                self.news_offset = self.news_offset.min(self.headlines.len().saturating_sub(1));
            }
            DataEvent::Rates(r) => self.rates = Some(r),
        }
    }

    /// Market-watch publishes numeric sector codes; `/symbols` has the human
    /// names. Join them so every screen can show "COMMERCIAL BANKS".
    fn resolve_sectors(&mut self) {
        if self.symbols.is_empty() {
            return;
        }
        for q in &mut self.quotes {
            if let Some(info) = self.symbols.get(&q.symbol)
                && !info.sector_name.is_empty()
            {
                q.sector = info.sector_name.clone();
            }
        }
    }

    // --- selection -------------------------------------------------------

    pub fn select(&mut self, symbol: String) {
        if symbol.is_empty() || symbol == self.selected {
            return;
        }
        self.selected = symbol;
        // Intraday ticks aren't persisted, so they always start empty.
        self.ticks.clear();
        self.bars.clear();
        self.company = None;
        self.announcement_cursor = 0;
        self.needs_cache_load = true;
    }

    /// Load the selected symbol's cached data. Call once per frame, before
    /// rendering.
    ///
    /// The read itself is fast, but scrolling a list selects every row it
    /// passes, and a held arrow key delivers a burst of moves before a single
    /// frame is drawn. Doing the read inside [`App::select`] meant a full
    /// history query per row skipped over — work whose result was overwritten
    /// before anything was displayed. Deferring to frame granularity means one
    /// read per burst, for the row the cursor actually landed on.
    pub fn settle_selection(&mut self) {
        if !self.needs_cache_load {
            return;
        }
        self.needs_cache_load = false;

        let symbol = self.selected.clone();
        if symbol.is_empty() {
            return;
        }

        // The worker also reads the cache, but at the back of a serial queue
        // behind the OHLC backfill and any in-flight refresh. Waiting for that
        // turned revisiting a symbol into a fresh "Loading…" every time, even
        // though the data was already on disk.
        self.bars = self.store.bars(&symbol, None).unwrap_or_default();
        self.company = self
            .store
            .company(&symbol, chrono::Duration::days(COMPANY_TTL_DAYS))
            .ok()
            .flatten();

        // Only go back to the network once the cached copy has aged out, and
        // even then not until the cursor settles — see [`LOAD_DEBOUNCE`].
        if self.is_stale(&symbol) {
            self.pending_load = Some((symbol, Instant::now()));
        } else {
            self.pending_load = None;
            self.status = format!("{symbol} — from cache");
        }
    }

    /// Issue a deferred fetch once the cursor has stopped moving.
    ///
    /// Driven by the UI tick. Returns whether a request was sent.
    pub fn poll_pending_load(&mut self) -> bool {
        let Some((symbol, since)) = &self.pending_load else {
            return false;
        };
        if since.elapsed() < LOAD_DEBOUNCE {
            return false;
        }

        let symbol = symbol.clone();
        self.pending_load = None;

        // The cursor may have moved on, or a refresh may have landed already.
        if symbol != self.selected || !self.is_stale(&symbol) {
            return false;
        }

        self.refreshed.insert(symbol.clone(), Instant::now());
        self.request(DataRequest::LoadSymbol(symbol.clone()));
        if self.company.is_none() {
            self.request(DataRequest::LoadCompany(symbol));
        }
        true
    }

    /// Whether `symbol`'s live data is due a network refresh.
    fn is_stale(&self, symbol: &str) -> bool {
        // Never fetched this session, or fetched longer ago than the TTL.
        // An empty cache is always stale — there is nothing to show otherwise.
        if self.bars.is_empty() {
            return true;
        }
        self.refreshed
            .get(symbol)
            .is_none_or(|t| t.elapsed() >= SYMBOL_TTL)
    }

    pub fn quote(&self, symbol: &str) -> Option<&Quote> {
        self.quotes.iter().find(|q| q.symbol == symbol)
    }

    pub fn selected_quote(&self) -> Option<&Quote> {
        self.quote(&self.selected)
    }

    pub fn company_name(&self, symbol: &str) -> String {
        self.symbols
            .get(symbol)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }

    /// The risk-free rate the risk-adjusted statistics measure against, as a
    /// fraction.
    ///
    /// The live SBP policy rate when it has been fetched, otherwise the
    /// documented fallback — never a hardcoded guess that silently ages.
    pub fn risk_free(&self) -> f64 {
        self.rates.unwrap_or_default().risk_free()
    }

    /// Bars trimmed to the chart's selected range.
    pub fn ranged_bars(&self) -> &[Bar] {
        Self::trim_range(&self.bars, self.chart.range)
    }

    /// Trim a bar series to `range`.
    ///
    /// Year-to-date is bounded by the calendar rather than a session count,
    /// because the number of sessions so far depends on where in the year we
    /// are — and on PSX's holiday calendar, which is not fixed.
    pub fn trim_range(bars: &[Bar], range: Range) -> &[Bar] {
        if range == Range::Ytd {
            let year_start = crate::cache::year_start_ts();
            let from = bars.partition_point(|b| b.ts < year_start);
            return &bars[from..];
        }
        match range.sessions() {
            Some(n) if bars.len() > n => &bars[bars.len() - n..],
            _ => bars,
        }
    }

    // --- screener --------------------------------------------------------

    /// The board as currently filtered and sorted.
    pub fn visible_quotes(&self) -> Vec<&Quote> {
        let needle = self
            .search
            .as_deref()
            .map(str::to_ascii_uppercase)
            .unwrap_or_default();

        let mut rows: Vec<&Quote> = self
            .quotes
            .iter()
            .filter(|q| {
                if self.screener.watchlist_only && !self.watchlist.contains(&q.symbol) {
                    return false;
                }
                if let Some(sector) = &self.sector_filter
                    && q.sector.trim() != sector
                {
                    return false;
                }
                if self.screener.equities_only
                    && let Some(info) = self.symbols.get(&q.symbol)
                    && (info.is_debt || info.is_etf)
                {
                    return false;
                }
                if !needle.is_empty() {
                    let name = self.company_name(&q.symbol).to_ascii_uppercase();
                    if !q.symbol.contains(&needle)
                        && !name.contains(&needle)
                        && !q.sector.to_ascii_uppercase().contains(&needle)
                    {
                        return false;
                    }
                }
                true
            })
            .collect();

        let desc = self.screener.descending;
        let sort = self.screener.sort;
        rows.sort_by(|a, b| {
            // Fundamentals are cached per symbol, so most rows have no value
            // at all for a valuation column. Those rows sink to the bottom in
            // *both* directions: an absent P/E is not a low P/E, and letting
            // it sort as one would put the least-known scrips at the top of an
            // ascending ranking.
            if sort.is_valuation() {
                let av = self.fundamentals.get(&a.symbol).and_then(|v| v.key(sort));
                let bv = self.fundamentals.get(&b.symbol).and_then(|v| v.key(sort));
                return match (av, bv) {
                    (Some(x), Some(y)) => {
                        if desc {
                            y.total_cmp(&x)
                        } else {
                            x.total_cmp(&y)
                        }
                    }
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => a.symbol.cmp(&b.symbol),
                };
            }

            let ord = match sort {
                SortKey::Price => a.current.total_cmp(&b.current),
                SortKey::Change => a.change_pct.total_cmp(&b.change_pct),
                SortKey::Volume => a.volume.total_cmp(&b.volume),
                SortKey::Turnover => a.turnover().total_cmp(&b.turnover()),
                // Symbol, and any valuation key handled above.
                _ => a.symbol.cmp(&b.symbol),
            };
            if desc { ord.reverse() } else { ord }
        });
        rows
    }

    /// How many of the currently visible rows have cached fundamentals.
    ///
    /// The screener prints this beside the valuation columns so a ranking over
    /// a handful of cached profiles is never mistaken for a ranking of the
    /// whole market.
    pub fn fundamentals_coverage(&self, rows: &[&Quote]) -> (usize, usize) {
        let have = rows
            .iter()
            .filter(|q| self.fundamentals.contains_key(&q.symbol))
            .count();
        (have, rows.len())
    }

    /// Re-read every cached company profile into [`App::fundamentals`].
    ///
    /// Called when the valuation view is toggled: profiles land in the
    /// background as symbols are visited, so a view opened later in the
    /// session should see everything that has arrived since start-up.
    pub fn reload_fundamentals(&mut self) {
        self.fundamentals = load_fundamentals(&self.store);
    }

    /// The sort columns the screener is currently offering, which depends on
    /// whether the valuation view is showing.
    pub fn sort_keys(&self) -> &'static [SortKey] {
        if self.screener.valuation {
            &SortKey::VALUATION
        } else {
            &SortKey::ALL
        }
    }

    // --- dashboard -------------------------------------------------------

    /// Rows of one leaderboard, capped at `n`.
    ///
    /// Shared by the renderer and by key handling so the cursor can never
    /// address a row the dashboard didn't draw. Untraded scrips are excluded:
    /// a limit-up print on zero volume isn't a real mover.
    pub fn leaderboard(&self, board: Board, n: usize) -> Vec<&Quote> {
        let mut v: Vec<&Quote> = self
            .visible_quotes()
            .into_iter()
            .filter(|q| q.volume > 0.0)
            .collect();

        match board {
            Board::Gainers => v.sort_by(|a, b| b.change_pct.total_cmp(&a.change_pct)),
            Board::Losers => v.sort_by(|a, b| a.change_pct.total_cmp(&b.change_pct)),
            Board::Active => v.sort_by(|a, b| b.turnover().total_cmp(&a.turnover())),
        }
        v.truncate(n);
        v
    }

    /// Sectors ranked by traded value, with a turnover-weighted average change.
    ///
    /// Weighting by value stops a thin scrip printing ±10% on a handful of
    /// shares from dominating a sector that is really flat.
    pub fn sectors(&self) -> Vec<SectorAgg> {
        struct Acc {
            weighted: f64,
            weight: f64,
            sum: f64,
            count: usize,
        }
        let mut map: BTreeMap<&str, Acc> = BTreeMap::new();

        for q in &self.quotes {
            let name = if q.sector.trim().is_empty() {
                "UNCLASSIFIED"
            } else {
                q.sector.trim()
            };
            let acc = map.entry(name).or_insert(Acc {
                weighted: 0.0,
                weight: 0.0,
                sum: 0.0,
                count: 0,
            });
            acc.count += 1;
            if q.change_pct.is_finite() {
                acc.sum += q.change_pct;
                let t = q.turnover();
                if t.is_finite() && t > 0.0 {
                    acc.weighted += q.change_pct * t;
                    acc.weight += t;
                }
            }
        }

        let mut out: Vec<SectorAgg> = map
            .into_iter()
            .map(|(name, a)| SectorAgg {
                name: name.to_string(),
                avg_pct: if a.weight > 0.0 {
                    a.weighted / a.weight
                } else if a.count > 0 {
                    a.sum / a.count as f64
                } else {
                    0.0
                },
                turnover: a.weight,
                count: a.count,
            })
            .collect();

        out.sort_by(|a, b| {
            b.turnover
                .total_cmp(&a.turnover)
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// Record how many heatmap rows the renderer drew, so scrolling moves by
    /// the visible window rather than a guess.
    pub fn publish_sector_rows(&self, rows: usize) {
        let mut l = self.dash_layout.get();
        l.sector_rows = rows;
        self.dash_layout.set(l);
    }

    /// The sector under the heatmap cursor.
    pub fn sector_at_cursor(&self) -> Option<SectorAgg> {
        self.sectors().into_iter().nth(self.dashboard.sector)
    }

    /// The board the dashboard cursor is on.
    pub fn dash_board(&self) -> Board {
        let l = self.dash_layout.get();
        let i = self.dashboard.board.min(l.count.saturating_sub(1));
        l.boards[i.min(2)]
    }

    /// The symbol under the dashboard cursor, if any.
    pub fn dash_symbol(&self) -> Option<String> {
        let rows = self.dash_layout.get().rows;
        self.leaderboard(self.dash_board(), rows)
            .get(self.dashboard.cursor)
            .map(|q| q.symbol.clone())
    }

    pub fn watchlist_toggle(&mut self, symbol: &str) {
        if self.watchlist.contains(symbol) {
            self.watchlist.remove(symbol);
            self.status = format!("{symbol} removed from watchlist");
        } else {
            self.watchlist.insert(symbol.to_string());
            self.status = format!("{symbol} added to watchlist");
        }
        save_watchlist(&self.store, &self.watchlist);
    }

    // --- input -----------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        // The search prompt swallows most keys while it is open.
        if let Some(query) = self.search.as_mut() {
            match key.code {
                KeyCode::Esc => self.search = None,
                KeyCode::Enter => {
                    // Commit: jump to the first match.
                    if let Some(q) = self.visible_quotes().first() {
                        let sym = q.symbol.clone();
                        self.search = None;
                        self.select(sym);
                        self.screen = Screen::Chart;
                    } else {
                        self.search = None;
                    }
                }
                KeyCode::Backspace => {
                    query.pop();
                }
                KeyCode::Char(c) => query.push(c),
                _ => {}
            }
            self.screener.cursor = 0;
            return;
        }

        if self.show_help {
            // Any key dismisses help.
            self.show_help = false;
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('?') => self.show_help = true,
            // Mouse reporting swallows the terminal's own text selection, so
            // it can be turned off when you want to copy something out.
            KeyCode::Char('M') => {
                self.mouse_enabled = !self.mouse_enabled;
                self.status = if self.mouse_enabled {
                    "Mouse on".into()
                } else {
                    "Mouse off — terminal text selection restored".into()
                };
            }
            KeyCode::Esc => {
                // Clear the most specific thing first, so one key backs out of
                // a drill-through without also dismissing an unrelated error.
                if self.sector_filter.take().is_some() {
                    self.screener.cursor = 0;
                    self.status = "Sector filter cleared".into();
                } else {
                    self.error = None;
                }
            }

            KeyCode::Char('/') => {
                self.search = Some(String::new());
                self.screen = Screen::Screener;
            }
            KeyCode::Char('r') => {
                self.status = "Refreshing…".into();
                self.request(DataRequest::RefreshMarket);
                if !self.selected.is_empty() {
                    let sym = self.selected.clone();
                    // An explicit refresh overrides the freshness window.
                    self.refreshed.insert(sym.clone(), Instant::now());
                    self.request(DataRequest::LoadSymbol(sym));
                }
            }

            KeyCode::Tab => self.cycle_screen(1),
            KeyCode::BackTab => self.cycle_screen(-1),
            // Bounds-checked against Screen::ALL rather than a hardcoded
            // range, so adding a screen can't leave its number key dead.
            KeyCode::Char(c @ '1'..='9') => {
                let idx = c as usize - '1' as usize;
                if let Some(screen) = Screen::ALL.get(idx) {
                    self.screen = *screen;
                }
            }

            KeyCode::Char('w') => {
                if !self.selected.is_empty() {
                    let sym = self.selected.clone();
                    self.watchlist_toggle(&sym);
                }
            }

            _ => self.on_screen_key(key),
        }
    }

    // --- mouse -----------------------------------------------------------

    /// Handle a mouse event against the regions the last frame registered.
    pub fn on_mouse(&mut self, ev: MouseEvent) {
        // The help overlay covers everything beneath it; a click there should
        // dismiss it rather than reach a control it is hiding.
        if self.show_help {
            if matches!(ev.kind, MouseEventKind::Down(_)) {
                self.show_help = false;
            }
            return;
        }

        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll_at(ev.column, ev.row, -(WHEEL_LINES as isize)),
            MouseEventKind::ScrollDown => self.scroll_at(ev.column, ev.row, WHEEL_LINES as isize),
            MouseEventKind::Down(MouseButton::Left) => self.click_at(ev.column, ev.row),
            _ => {}
        }
    }

    /// Act on a left click at a cell.
    fn click_at(&mut self, x: u16, y: u16) {
        // Resolve against the hit map and drop the borrow before mutating:
        // handling a click can trigger a redraw path that rewrites the map.
        let hit = {
            let hits = self.hits.borrow();
            (hits.target_at(x, y), hits.zone_at(x, y))
        };
        let Some(target) = hit.0 else {
            // Clicking a panel's empty space still moves focus there, which is
            // what makes the subsequent wheel and arrow keys go where expected.
            if let Some(zone) = hit.1 {
                self.focus_zone(zone);
            }
            return;
        };

        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(prev, at)| prev == target && at.elapsed() < DOUBLE_CLICK);
        self.last_click = Some((target, now));

        match target {
            Target::Tab(i) => {
                if let Some(screen) = Screen::ALL.get(i) {
                    self.screen = *screen;
                }
            }

            Target::ScreenerRow(i) => {
                let symbol = self.visible_quotes().get(i).map(|q| q.symbol.clone());
                if let Some(symbol) = symbol {
                    self.screener.cursor = i;
                    self.select(symbol);
                    // Click to inspect, double-click to open — the same
                    // convention as a file manager.
                    if double {
                        self.screen = Screen::Chart;
                    }
                }
            }

            Target::BoardRow { board, row } => {
                self.dashboard.focus = DashFocus::Boards;
                self.dashboard.board = board;
                let rows = self.dash_layout.get().rows;
                let b = self.dash_board();
                if let Some(q) = self.leaderboard(b, rows).get(row) {
                    let symbol = q.symbol.clone();
                    self.dashboard.cursor = row;
                    self.select(symbol);
                    if double {
                        self.screen = Screen::Chart;
                    }
                }
            }

            Target::SectorRow(i) => {
                self.dashboard.focus = DashFocus::Sectors;
                if i < self.sectors().len() {
                    self.dashboard.sector = i;
                    if double && let Some(s) = self.sector_at_cursor() {
                        self.sector_filter = Some(s.name.clone());
                        self.screener.cursor = 0;
                        self.screen = Screen::Screener;
                        self.status = format!("Filtered to {}", s.name);
                    }
                }
            }

            Target::CompanyTab(i) => {
                if let Some(tab) = CompanyTab::ALL.get(i) {
                    self.company_tab = *tab;
                    self.announcement_cursor = 0;
                }
            }

            Target::Announcement(i) => {
                let len = self
                    .company
                    .as_ref()
                    .map(|c| c.announcements.len())
                    .unwrap_or(0);
                if i < len {
                    self.announcement_cursor = i;
                }
            }

            Target::ChartRange(i) => {
                if let Some(r) = Range::ALL.get(i) {
                    self.chart.range = *r;
                }
            }
            // Same order as the header lays them out.
            Target::ChartOverlay(i) => {
                let flag = match i {
                    0 => &mut self.chart.show_sma,
                    1 => &mut self.chart.show_ema,
                    2 => &mut self.chart.show_bollinger,
                    3 => &mut self.chart.show_donchian,
                    4 => &mut self.chart.show_ichimoku,
                    5 => &mut self.chart.show_levels,
                    _ => return,
                };
                *flag = !*flag;
            }

            Target::SortColumn(i) => {
                // Clicking the active column reverses it, as a spreadsheet
                // does; clicking a different one sorts by that column.
                if let Some(key) = self.sort_keys().get(i).copied() {
                    if self.screener.sort == key {
                        self.screener.descending = !self.screener.descending;
                    } else {
                        self.screener.sort = key;
                        self.screener.descending = key != SortKey::Symbol;
                    }
                    self.screener.cursor = 0;
                }
            }

            Target::Help => self.show_help = true,
            Target::Quit => self.should_quit = true,

            Target::ChartStyle => self.chart.style = self.chart.style.next(),
            Target::ChartPane => {
                let i = Pane::ALL
                    .iter()
                    .position(|p| *p == self.chart.pane)
                    .unwrap_or(0);
                self.chart.pane = Pane::ALL[(i + 1) % Pane::ALL.len()];
            }

            Target::MacroRow(_) => self.macro_focus = MacroFocus::Series,
            Target::NewsRow(i) => {
                self.macro_focus = MacroFocus::News;
                if i < self.headlines.len() {
                    self.news_offset = i;
                }
            }
        }
    }

    /// Move keyboard focus to whichever panel was clicked, so the wheel and the
    /// arrow keys agree about what they are driving.
    fn focus_zone(&mut self, zone: Zone) {
        match zone {
            Zone::Board(i) => {
                self.dashboard.focus = DashFocus::Boards;
                self.dashboard.board = i;
            }
            Zone::Sectors => self.dashboard.focus = DashFocus::Sectors,
            Zone::MacroSeries => self.macro_focus = MacroFocus::Series,
            Zone::MacroNews => self.macro_focus = MacroFocus::News,
            Zone::Screener | Zone::Announcements | Zone::Chart => {}
        }
    }

    /// Scroll whichever panel the pointer is over.
    ///
    /// Deliberately keyed on position rather than focus: a wheel acts on what
    /// is under the pointer, which is not necessarily what the keyboard drives.
    fn scroll_at(&mut self, x: u16, y: u16, delta: isize) {
        let zone = self.hits.borrow().zone_at(x, y);
        let Some(zone) = zone else {
            return;
        };

        match zone {
            Zone::Screener => {
                let len = self.visible_quotes().len();
                self.screener.cursor = step(self.screener.cursor, delta, len);
                if let Some(q) = self.visible_quotes().get(self.screener.cursor) {
                    let symbol = q.symbol.clone();
                    self.select(symbol);
                }
            }
            Zone::Board(i) => {
                self.dashboard.focus = DashFocus::Boards;
                self.dashboard.board = i;
                let rows = self.dash_layout.get().rows;
                let b = self.dash_board();
                let len = self.leaderboard(b, rows).len();
                self.dashboard.cursor = step(self.dashboard.cursor, delta, len);
                if let Some(symbol) = self.dash_symbol() {
                    self.select(symbol);
                }
            }
            Zone::Sectors => {
                self.dashboard.focus = DashFocus::Sectors;
                let len = self.sectors().len();
                self.dashboard.sector = step(self.dashboard.sector, delta, len);
                let window = self.dash_layout.get().sector_rows.max(1);
                self.dashboard.sector_offset =
                    keep_visible(self.dashboard.sector, self.dashboard.sector_offset, window);
            }
            Zone::Announcements => {
                let len = self
                    .company
                    .as_ref()
                    .map(|c| c.announcements.len())
                    .unwrap_or(0);
                self.announcement_cursor = step(self.announcement_cursor, delta, len);
            }
            Zone::MacroSeries => {
                let len = self.macro_series.len();
                self.series_offset = step(self.series_offset, delta, len);
            }
            Zone::MacroNews => {
                let len = self.headlines.len();
                self.news_offset = step(self.news_offset, delta, len);
            }
            // Over the chart the wheel changes timeframe, which is what a
            // charting tool does with a wheel.
            Zone::Chart => {
                let i = Range::ALL
                    .iter()
                    .position(|r| *r == self.chart.range)
                    .unwrap_or(0) as isize;
                let n = Range::ALL.len() as isize;
                // Scrolling up zooms in, so it walks toward the shorter window.
                let next = (i + delta.signum()).clamp(0, n - 1);
                self.chart.range = Range::ALL[next as usize];
            }
        }
    }

    fn cycle_screen(&mut self, delta: isize) {
        let n = Screen::ALL.len() as isize;
        let i = (self.screen.index() as isize + delta).rem_euclid(n);
        self.screen = Screen::ALL[i as usize];
    }

    fn on_screen_key(&mut self, key: KeyEvent) {
        match self.screen {
            Screen::Dashboard => self.on_dashboard_key(key),
            Screen::Screener => self.on_list_key(key),
            Screen::Chart => self.on_chart_key(key),
            Screen::Company => self.on_company_key(key),
            Screen::Macro => self.on_macro_key(key),
            Screen::Compare => self.on_compare_key(key),
            Screen::Analysis | Screen::Intraday | Screen::Seasonality => {}
        }
    }

    /// Scroll the Macro screen's news list.
    ///
    /// Self-contained: the only state it touches is [`App::news_offset`], and
    /// it clamps against the list length so a shrinking feed cannot leave the
    /// viewport past the end.
    fn on_macro_key(&mut self, key: KeyEvent) {
        // `s` moves between the series list and the headlines, mirroring the
        // dashboard's boards/heatmap toggle.
        if key.code == KeyCode::Char('s') {
            self.macro_focus = match self.macro_focus {
                MacroFocus::Series => MacroFocus::News,
                MacroFocus::News => MacroFocus::Series,
            };
            return;
        }

        // The catalogue outgrew a single panel once metals, agriculture and
        // crypto were added, so the series list scrolls rather than silently
        // dropping whatever sits past the last row.
        let (offset, len) = match self.macro_focus {
            MacroFocus::Series => (&mut self.series_offset, self.macro_series.len()),
            MacroFocus::News => (&mut self.news_offset, self.headlines.len()),
        };
        if len == 0 {
            return;
        }
        let last = len - 1;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => *offset = (*offset + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => *offset = offset.saturating_sub(1),
            KeyCode::PageDown => *offset = (*offset + 10).min(last),
            KeyCode::PageUp => *offset = offset.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => *offset = 0,
            KeyCode::End | KeyCode::Char('G') => *offset = last,
            _ => {}
        }
    }

    /// Dashboard navigation moves within the focused leaderboard.
    ///
    /// The cursor is deliberately *not* an index into the full quote list:
    /// the dashboard only draws a dozen-odd rows per board, so indexing the
    /// whole market walked the cursor off-screen and the highlight vanished.
    fn on_dashboard_key(&mut self, key: KeyEvent) {
        // `s` moves the keyboard between the leaderboards and the heatmap.
        if key.code == KeyCode::Char('s') {
            self.dashboard.focus = match self.dashboard.focus {
                DashFocus::Boards => DashFocus::Sectors,
                DashFocus::Sectors => DashFocus::Boards,
            };
            return;
        }
        if self.dashboard.focus == DashFocus::Sectors {
            self.on_sector_key(key);
            return;
        }

        let layout = self.dash_layout.get();
        let boards = layout.count.max(1);
        let len = self.leaderboard(self.dash_board(), layout.rows).len();
        if len == 0 {
            return;
        }
        let last = len - 1;

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.dashboard.cursor = (self.dashboard.cursor + 1).min(last)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.dashboard.cursor = self.dashboard.cursor.saturating_sub(1)
            }
            KeyCode::PageDown => self.dashboard.cursor = (self.dashboard.cursor + 10).min(last),
            KeyCode::PageUp => self.dashboard.cursor = self.dashboard.cursor.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => self.dashboard.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.dashboard.cursor = last,

            // Move between boards, keeping the row where it still exists.
            // Tab is not used here — it switches screens globally.
            KeyCode::Right | KeyCode::Char('l') => {
                self.dashboard.board = (self.dashboard.board + 1) % boards;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.dashboard.board = (self.dashboard.board + boards - 1) % boards;
            }

            KeyCode::Enter => {
                if let Some(sym) = self.dash_symbol() {
                    self.select(sym);
                    self.screen = Screen::Chart;
                }
                return;
            }
            _ => return,
        }

        // Clamp after a board change: boards can differ in length.
        let len = self.leaderboard(self.dash_board(), layout.rows).len();
        self.dashboard.cursor = self.dashboard.cursor.min(len.saturating_sub(1));

        // Track the highlighted row so the detail screens follow it.
        if let Some(sym) = self.dash_symbol() {
            self.select(sym);
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) {
        let len = self.visible_quotes().len();
        if len == 0 {
            return;
        }
        let cursor = &mut self.screener.cursor;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => *cursor = (*cursor + 1).min(len - 1),
            KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
            KeyCode::PageDown => *cursor = (*cursor + 20).min(len - 1),
            KeyCode::PageUp => *cursor = cursor.saturating_sub(20),
            KeyCode::Home | KeyCode::Char('g') => *cursor = 0,
            KeyCode::End | KeyCode::Char('G') => *cursor = len - 1,
            KeyCode::Enter => {
                if let Some(q) = self.visible_quotes().get(self.screener.cursor) {
                    let sym = q.symbol.clone();
                    self.select(sym);
                    self.screen = Screen::Chart;
                }
                return;
            }
            KeyCode::Char('s') => {
                // Cycle the sort column within whichever set of columns the
                // current view actually draws.
                let set: &[SortKey] = if self.screener.valuation {
                    &SortKey::VALUATION
                } else {
                    &SortKey::ALL
                };
                let i = set
                    .iter()
                    .position(|k| *k == self.screener.sort)
                    .unwrap_or(0);
                self.screener.sort = set[(i + 1) % set.len()];
                self.screener.cursor = 0;
                return;
            }
            KeyCode::Char('f') => {
                self.screener.valuation = !self.screener.valuation;
                // Pick up any profile that has landed since start-up.
                self.reload_fundamentals();
                // Leave the sort alone unless it names a column the new view
                // does not have.
                if self.screener.valuation {
                    if !SortKey::VALUATION.contains(&self.screener.sort) {
                        self.screener.sort = SortKey::MarketCap;
                    }
                } else if !SortKey::ALL.contains(&self.screener.sort) {
                    self.screener.sort = SortKey::Turnover;
                }
                self.screener.cursor = 0;
                return;
            }
            KeyCode::Char('S') => {
                self.screener.descending = !self.screener.descending;
                self.screener.cursor = 0;
                return;
            }
            KeyCode::Char('W') => {
                self.screener.watchlist_only = !self.screener.watchlist_only;
                self.screener.cursor = 0;
                return;
            }
            KeyCode::Char('e') => {
                self.screener.equities_only = !self.screener.equities_only;
                self.screener.cursor = 0;
                return;
            }
            _ => return,
        }

        // Follow the cursor so the detail screens track the highlighted row.
        if let Some(q) = self.visible_quotes().get(self.screener.cursor) {
            let sym = q.symbol.clone();
            self.select(sym);
        }
    }

    /// Scroll the sector heatmap.
    ///
    /// The list is longer than the pane on any realistic terminal — PSX has
    /// ~35 sectors — so the visible window follows the cursor.
    fn on_sector_key(&mut self, key: KeyEvent) {
        let len = self.sectors().len();
        if len == 0 {
            return;
        }
        let last = len - 1;
        let page = self.dash_layout.get().sector_rows.max(1);

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.dashboard.sector = (self.dashboard.sector + 1).min(last)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.dashboard.sector = self.dashboard.sector.saturating_sub(1)
            }
            KeyCode::PageDown => self.dashboard.sector = (self.dashboard.sector + page).min(last),
            KeyCode::PageUp => self.dashboard.sector = self.dashboard.sector.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => self.dashboard.sector = 0,
            KeyCode::End | KeyCode::Char('G') => self.dashboard.sector = last,
            KeyCode::Enter => {
                // Drill through: open the screener filtered to this sector.
                if let Some(s) = self.sector_at_cursor() {
                    self.search = None;
                    self.sector_filter = Some(s.name.clone());
                    self.screener.cursor = 0;
                    self.screen = Screen::Screener;
                    self.status = format!("Filtered to {}", s.name);
                }
                return;
            }
            _ => return,
        }

        // Keep the cursor inside the drawn window.
        let rows = self.dash_layout.get().sector_rows.max(1);
        if self.dashboard.sector < self.dashboard.sector_offset {
            self.dashboard.sector_offset = self.dashboard.sector;
        } else if self.dashboard.sector >= self.dashboard.sector_offset + rows {
            self.dashboard.sector_offset = self.dashboard.sector + 1 - rows;
        }
    }

    fn on_chart_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(']') | KeyCode::Right => {
                let i = Range::ALL
                    .iter()
                    .position(|r| *r == self.chart.range)
                    .unwrap_or(0);
                self.chart.range = Range::ALL[(i + 1) % Range::ALL.len()];
            }
            KeyCode::Char('[') | KeyCode::Left => {
                let i = Range::ALL
                    .iter()
                    .position(|r| *r == self.chart.range)
                    .unwrap_or(0);
                self.chart.range = Range::ALL[(i + Range::ALL.len() - 1) % Range::ALL.len()];
            }
            KeyCode::Char('i') => {
                let i = Pane::ALL
                    .iter()
                    .position(|p| *p == self.chart.pane)
                    .unwrap_or(0);
                self.chart.pane = Pane::ALL[(i + 1) % Pane::ALL.len()];
            }
            KeyCode::Char('m') => self.chart.show_sma = !self.chart.show_sma,
            KeyCode::Char('e') => self.chart.show_ema = !self.chart.show_ema,
            KeyCode::Char('b') => self.chart.show_bollinger = !self.chart.show_bollinger,
            KeyCode::Char('d') => self.chart.show_donchian = !self.chart.show_donchian,
            // `k` for kumo — the Ichimoku cloud.
            KeyCode::Char('k') => self.chart.show_ichimoku = !self.chart.show_ichimoku,
            KeyCode::Char('v') => self.chart.show_levels = !self.chart.show_levels,
            KeyCode::Char('c') => self.chart.style = self.chart.style.next(),
            _ => {}
        }
    }

    /// The symbols the Compare screen overlays.
    ///
    /// Until the user curates a set the screen seeds itself: the selected
    /// symbol first, then the watchlist. With neither — a fresh install — it
    /// falls back to the day's most-traded names, so the screen has something
    /// meaningful on it the first time it is opened.
    pub fn compare_symbols(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push = |out: &mut Vec<String>, s: &str| {
            if !s.is_empty() && out.len() < MAX_COMPARE && !out.iter().any(|x| x == s) {
                out.push(s.to_string());
            }
        };

        if !self.compare.symbols.is_empty() {
            for s in &self.compare.symbols {
                push(&mut out, s);
            }
            return out;
        }

        push(&mut out, &self.selected);
        for s in &self.watchlist {
            push(&mut out, s);
        }
        if out.len() < 2 {
            let mut by_value: Vec<&Quote> = self.quotes.iter().collect();
            by_value.sort_by(|a, b| b.turnover().total_cmp(&a.turnover()));
            for q in by_value {
                push(&mut out, &q.symbol);
            }
        }
        out
    }

    /// Compare screen: `[`/`]` cycle the window, `a` adds or removes the
    /// selected symbol, `c` clears the set back to the watchlist seed.
    fn on_compare_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(']') | KeyCode::Right => {
                let i = Range::ALL
                    .iter()
                    .position(|r| *r == self.compare.range)
                    .unwrap_or(0);
                self.compare.range = Range::ALL[(i + 1) % Range::ALL.len()];
            }
            KeyCode::Char('[') | KeyCode::Left => {
                let n = Range::ALL.len();
                let i = Range::ALL
                    .iter()
                    .position(|r| *r == self.compare.range)
                    .unwrap_or(0);
                self.compare.range = Range::ALL[(i + n - 1) % n];
            }
            KeyCode::Char('a') => {
                if self.selected.is_empty() {
                    return;
                }
                // Editing starts from whatever is on screen, seed included, so
                // the first keypress does not silently discard the view.
                let mut set = self.compare_symbols();
                let selected = self.selected.clone();
                match set.iter().position(|s| *s == selected) {
                    Some(i) => {
                        set.remove(i);
                        self.status = format!("{selected} removed from comparison");
                    }
                    None if set.len() < MAX_COMPARE => {
                        set.push(selected.clone());
                        self.status = format!("{selected} added to comparison");
                    }
                    None => {
                        self.status =
                            format!("Comparison holds {MAX_COMPARE} symbols — remove one first");
                        return;
                    }
                }
                self.compare.symbols = set;
            }
            KeyCode::Char('c') => {
                self.compare.symbols.clear();
                self.status = "Comparison reset to the watchlist".into();
            }
            _ => {}
        }
    }

    fn on_company_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => {
                let i = CompanyTab::ALL
                    .iter()
                    .position(|t| *t == self.company_tab)
                    .unwrap_or(0);
                self.company_tab = CompanyTab::ALL[(i + 1) % CompanyTab::ALL.len()];
                self.announcement_cursor = 0;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let n = CompanyTab::ALL.len();
                let i = CompanyTab::ALL
                    .iter()
                    .position(|t| *t == self.company_tab)
                    .unwrap_or(0);
                self.company_tab = CompanyTab::ALL[(i + n - 1) % n];
                self.announcement_cursor = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = self
                    .company
                    .as_ref()
                    .map(|c| c.announcements.len())
                    .unwrap_or(0);
                if len > 0 {
                    self.announcement_cursor = (self.announcement_cursor + 1).min(len - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.announcement_cursor = self.announcement_cursor.saturating_sub(1);
            }
            _ => {}
        }
    }
}

/// Move a cursor by `delta`, clamped to `len`. Returns 0 for an empty list.
fn step(cursor: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let last = (len - 1) as isize;
    (cursor as isize + delta).clamp(0, last) as usize
}

/// Slide `offset` the least amount that brings `cursor` inside a `window`-row
/// view.
fn keep_visible(cursor: usize, offset: usize, window: usize) -> usize {
    if cursor < offset {
        cursor
    } else if cursor >= offset + window {
        cursor + 1 - window
    } else {
        offset
    }
}

// --- watchlist persistence ----------------------------------------------

const WATCHLIST_KEY: &str = "watchlist";

/// How stale a cached profile may be and still feed the valuation view.
///
/// Far more generous than [`COMPANY_TTL_DAYS`], and deliberately so: the
/// company screen wants today's filings, whereas a market-wide P/E ranking is
/// better served by a month-old number than by an empty column. Fundamentals
/// only move when results are announced, a few times a year.
const FUNDAMENTALS_TTL_DAYS: i64 = 30;

/// Read every cached company profile and reduce it to a valuation row.
fn load_fundamentals(store: &Store) -> HashMap<String, Valuation> {
    store
        .companies(chrono::Duration::days(FUNDAMENTALS_TTL_DAYS))
        .unwrap_or_default()
        .iter()
        .filter(|c| !c.symbol.is_empty())
        .map(|c| (c.symbol.clone(), Valuation::from_company(c)))
        .collect()
}

fn load_watchlist(store: &Store) -> BTreeSet<String> {
    store
        .get_meta(WATCHLIST_KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

fn save_watchlist(store: &Store, list: &BTreeSet<String>) {
    if let Ok(json) = serde_json::to_string(list) {
        let _ = store.set_meta(WATCHLIST_KEY, &json);
    }
}

/// Convenience for tests and callers that want a store-backed app without a
/// live worker attached.
pub fn detached_channel() -> (
    UnboundedSender<DataRequest>,
    tokio::sync::mpsc::UnboundedReceiver<DataRequest>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(symbol: &str, price: f64, change_pct: f64, volume: f64) -> Quote {
        Quote {
            symbol: symbol.into(),
            sector: "0825".into(),
            indices: vec![],
            ldcp: price,
            open: price,
            high: price,
            low: price,
            current: price,
            change: 0.0,
            change_pct,
            volume,
        }
    }

    fn app() -> App {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _rx) = detached_channel();
        App::new(store, tx)
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn quotes_event_auto_selects_the_most_active_symbol() {
        let mut a = app();
        a.on_event(DataEvent::Quotes(vec![
            quote("SMALL", 10.0, 1.0, 100.0),
            quote("BIG", 100.0, 1.0, 10_000.0),
        ]));
        assert_eq!(a.selected, "BIG");
    }

    #[test]
    fn sector_codes_are_replaced_with_names_from_the_symbol_list() {
        let mut a = app();
        a.on_event(DataEvent::Quotes(vec![quote("HBL", 292.0, -1.0, 1000.0)]));
        assert_eq!(a.quotes[0].sector, "0825");

        a.on_event(DataEvent::Symbols(vec![SymbolInfo {
            symbol: "HBL".into(),
            name: "Habib Bank Limited".into(),
            sector_name: "COMMERCIAL BANKS".into(),
            is_etf: false,
            is_debt: false,
        }]));
        assert_eq!(a.quotes[0].sector, "COMMERCIAL BANKS");
    }

    #[test]
    fn selecting_a_symbol_drops_the_previous_series() {
        let mut a = app();
        a.select("HBL".into());
        a.bars = vec![Bar {
            ts: 1,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        }];
        // Nothing is cached for OGDC, so the previous symbol's bars must go
        // rather than render under the new ticker.
        a.select("OGDC".into());
        assert!(
            a.bars.is_empty(),
            "stale bars must not render under a new ticker"
        );
        assert!(a.company.is_none());
        assert!(a.ticks.is_empty());
    }

    /// Bars for a symbol, spaced one trading day apart.
    fn cached_bars(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| Bar {
                ts: crate::cache::day_close_ts("2026-01-05") + i as i64 * 86_400,
                open: 10.0,
                high: 11.0,
                low: 9.0,
                close: 10.0 + i as f64,
                volume: 100.0,
            })
            .collect()
    }

    #[test]
    fn selecting_paints_cached_bars_without_waiting_for_the_worker() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.put_eod_bars("HBL", &cached_bars(30)).unwrap();

        let (tx, _rx) = detached_channel();
        let mut a = App::new(store, tx);
        a.select("HBL".into());
        a.settle_selection();

        // Painted from the cache before any DataEvent has been delivered.
        assert_eq!(a.bars.len(), 30, "cached history must paint immediately");
    }

    #[test]
    fn revisiting_a_fresh_symbol_serves_cache_and_skips_the_network() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.put_eod_bars("HBL", &cached_bars(30)).unwrap();
        store.put_eod_bars("OGDC", &cached_bars(30)).unwrap();

        let (tx, mut rx) = detached_channel();
        let mut a = App::new(store, tx);

        a.select("HBL".into());
        settle(&mut a);
        while rx.try_recv().is_ok() {} // drain the first, legitimate fetch

        a.select("OGDC".into());
        settle(&mut a);
        while rx.try_recv().is_ok() {}

        // Back to HBL within the freshness window.
        a.select("HBL".into());
        a.settle_selection();
        assert_eq!(a.bars.len(), 30, "revisit must still show the history");
        assert!(
            rx.try_recv().is_err(),
            "a revisit inside the TTL must not hit the network again"
        );
    }

    /// Advance a frame: resolve the deferred cache read, then pretend the
    /// cursor has been still for longer than the debounce.
    fn settle(a: &mut App) -> bool {
        a.settle_selection();
        if let Some((sym, _)) = a.pending_load.take() {
            a.pending_load = Some((sym, Instant::now() - LOAD_DEBOUNCE));
        }
        a.poll_pending_load()
    }

    #[test]
    fn a_symbol_with_no_cached_history_fetches_once_settled() {
        let (tx, mut rx) = detached_channel();
        let mut a = App::new(Arc::new(Store::open_in_memory().unwrap()), tx);

        a.select("HBL".into());
        assert!(
            rx.try_recv().is_err(),
            "the fetch waits for the cursor to settle"
        );

        assert!(settle(&mut a));
        assert_eq!(
            rx.try_recv(),
            Ok(DataRequest::LoadSymbol("HBL".into())),
            "an empty cache leaves nothing to show, so it must fetch"
        );
    }

    #[test]
    fn scrolling_past_rows_queues_one_fetch_not_one_per_row() {
        // The bug this guards: every arrow keypress selected a row and fired
        // three requests, so a quick scroll left the worker minutes behind.
        let (tx, mut rx) = detached_channel();
        let mut a = App::new(Arc::new(Store::open_in_memory().unwrap()), tx);
        a.on_event(DataEvent::Quotes(
            (0..60)
                .map(|i| {
                    quote(
                        &format!("S{i:02}"),
                        10.0 + i as f64,
                        i as f64 - 30.0,
                        1000.0,
                    )
                })
                .collect(),
        ));
        a.screener.equities_only = false;
        a.screen = Screen::Dashboard;
        a.dash_layout.set(DashLayout {
            boards: [Board::Gainers, Board::Losers, Board::Active],
            count: 3,
            rows: 12,
            sector_rows: 10,
        });
        while rx.try_recv().is_ok() {}

        for _ in 0..20 {
            a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert!(
            rx.try_recv().is_err(),
            "scrolling must not fetch a row it is merely passing over"
        );

        let landed = a.selected.clone();
        assert!(!landed.is_empty());
        assert!(settle(&mut a));

        let requests: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(!requests.is_empty(), "the settled row must be fetched");
        assert!(
            requests.iter().all(|r| matches!(
                r,
                DataRequest::LoadSymbol(s) | DataRequest::LoadCompany(s) if *s == landed
            )),
            "only the settled symbol may be fetched, got {requests:?}"
        );
    }

    #[test]
    fn a_burst_of_moves_costs_one_cache_read_not_one_per_row() {
        // Scrolling selects every row it passes. Loading each one meant a full
        // history read per row skipped over, whose result was overwritten
        // before it was ever drawn — the app fell behind the keyboard and kept
        // scrolling after the key was released.
        let mut a = busy_market();

        for _ in 0..20 {
            a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert!(
                a.needs_cache_load,
                "the read must stay deferred while keys are still arriving"
            );
            assert!(a.bars.is_empty(), "no read may happen mid-burst");
        }

        // One frame resolves the burst, for the row the cursor landed on.
        a.settle_selection();
        assert!(!a.needs_cache_load);
    }

    #[test]
    fn a_pending_fetch_is_dropped_if_the_cursor_moves_on() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, mut rx) = detached_channel();
        let mut a = App::new(store, tx);

        a.select("HBL".into());
        a.select("OGDC".into());
        assert!(settle(&mut a));

        let requests: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !requests.contains(&DataRequest::LoadSymbol("HBL".into())),
            "the abandoned symbol must not be fetched"
        );
        assert!(requests.contains(&DataRequest::LoadSymbol("OGDC".into())));
    }

    #[test]
    fn explicit_refresh_requests_the_network_even_when_fresh() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.put_eod_bars("HBL", &cached_bars(30)).unwrap();

        let (tx, mut rx) = detached_channel();
        let mut a = App::new(store, tx);
        a.select("HBL".into());
        settle(&mut a);
        while rx.try_recv().is_ok() {}

        a.on_key(key('r'));
        let mut requests = Vec::new();
        while let Ok(r) = rx.try_recv() {
            requests.push(r);
        }
        assert!(requests.contains(&DataRequest::RefreshMarket));
        assert!(
            requests.contains(&DataRequest::LoadSymbol("HBL".into())),
            "r must override the freshness window"
        );
    }

    #[test]
    fn bars_for_a_stale_symbol_are_ignored() {
        let mut a = app();
        a.select("HBL".into());
        a.on_event(DataEvent::Bars {
            symbol: "OGDC".into(),
            bars: vec![Bar {
                ts: 1,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
            }],
        });
        assert!(
            a.bars.is_empty(),
            "late response for a deselected symbol must be dropped"
        );
    }

    #[test]
    fn benchmark_bars_land_in_their_own_slot() {
        let mut a = app();
        a.select("HBL".into());
        a.on_event(DataEvent::Bars {
            symbol: BENCHMARK.into(),
            bars: vec![Bar {
                ts: 1,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
            }],
        });
        assert_eq!(a.benchmark.len(), 1);
        assert!(a.bars.is_empty());
    }

    #[test]
    fn screener_sorts_and_reverses() {
        let mut a = app();
        a.on_event(DataEvent::Quotes(vec![
            quote("AAA", 10.0, 5.0, 100.0),
            quote("BBB", 20.0, -5.0, 200.0),
        ]));
        a.screener.sort = SortKey::Change;
        a.screener.descending = true;
        assert_eq!(a.visible_quotes()[0].symbol, "AAA");

        a.screener.descending = false;
        assert_eq!(a.visible_quotes()[0].symbol, "BBB");
    }

    #[test]
    fn search_filters_by_symbol_name_and_sector() {
        let mut a = app();
        a.on_event(DataEvent::Symbols(vec![SymbolInfo {
            symbol: "HBL".into(),
            name: "Habib Bank Limited".into(),
            sector_name: "COMMERCIAL BANKS".into(),
            is_etf: false,
            is_debt: false,
        }]));
        a.on_event(DataEvent::Quotes(vec![
            quote("HBL", 292.0, -1.0, 1000.0),
            quote("OGDC", 200.0, 1.0, 5000.0),
        ]));

        a.search = Some("habib".into());
        assert_eq!(a.visible_quotes().len(), 1, "should match on company name");

        a.search = Some("COMMERCIAL".into());
        assert_eq!(a.visible_quotes().len(), 1, "should match on sector");

        a.search = Some("OGD".into());
        assert_eq!(a.visible_quotes()[0].symbol, "OGDC");
    }

    #[test]
    fn equities_only_filter_hides_debt_and_etfs() {
        let mut a = app();
        a.on_event(DataEvent::Symbols(vec![
            SymbolInfo {
                symbol: "HBL".into(),
                name: "Habib Bank".into(),
                sector_name: "BANKS".into(),
                is_etf: false,
                is_debt: false,
            },
            SymbolInfo {
                symbol: "AKBLTFC6".into(),
                name: "Askari TFC".into(),
                sector_name: "BILLS AND BONDS".into(),
                is_etf: false,
                is_debt: true,
            },
        ]));
        a.on_event(DataEvent::Quotes(vec![
            quote("HBL", 292.0, -1.0, 1000.0),
            quote("AKBLTFC6", 100.0, 0.0, 10.0),
        ]));

        a.screener.equities_only = true;
        assert_eq!(a.visible_quotes().len(), 1);

        a.screener.equities_only = false;
        assert_eq!(a.visible_quotes().len(), 2);
    }

    #[test]
    fn watchlist_round_trips_through_the_store() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _rx) = detached_channel();
        let mut a = App::new(store.clone(), tx.clone());
        a.watchlist_toggle("HBL");
        assert!(a.watchlist.contains("HBL"));

        let reopened = App::new(store, tx);
        assert!(reopened.watchlist.contains("HBL"), "watchlist must persist");
    }

    #[test]
    fn ranged_bars_trim_to_the_selected_window() {
        let mut a = app();
        a.bars = (0..300)
            .map(|i| Bar {
                ts: i,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
            })
            .collect();

        a.chart.range = Range::M1;
        assert_eq!(a.ranged_bars().len(), 22);

        a.chart.range = Range::Max;
        assert_eq!(a.ranged_bars().len(), 300);
    }

    #[test]
    fn every_screen_has_a_working_number_key() {
        // Guards the case where Screen::ALL grew but the key range didn't.
        let mut a = app();
        for (i, expected) in Screen::ALL.iter().enumerate() {
            let c = char::from_digit(i as u32 + 1, 10).unwrap();
            a.screen = Screen::Dashboard;
            a.on_key(key(c));
            assert_eq!(a.screen, *expected, "key '{c}' should open {expected:?}");
        }
    }

    #[test]
    fn number_keys_and_tab_switch_screens() {
        let mut a = app();
        a.on_key(key('3'));
        assert_eq!(a.screen, Screen::Chart);

        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.screen, Screen::Analysis);

        a.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(a.screen, Screen::Chart);
    }

    #[test]
    fn tab_cycling_wraps_in_both_directions() {
        // Derived from Screen::ALL so adding a screen can't silently break the
        // wrap-around assertion.
        let last = *Screen::ALL.last().unwrap();
        let mut a = app();
        a.screen = last;
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.screen, Screen::Dashboard);

        a.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(a.screen, last);
    }

    #[test]
    fn search_prompt_captures_typing_and_escape_cancels() {
        let mut a = app();
        a.on_key(key('/'));
        assert_eq!(a.screen, Screen::Screener);

        a.on_key(key('h'));
        a.on_key(key('b'));
        assert_eq!(a.search.as_deref(), Some("hb"));

        a.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(a.search.as_deref(), Some("h"));

        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(a.search, None);
    }

    #[test]
    fn quit_keys_are_not_swallowed_by_the_search_prompt() {
        let mut a = app();
        a.on_key(key('/'));
        a.on_key(key('q'));
        assert!(!a.should_quit, "typing q into search must not quit");
        assert_eq!(a.search.as_deref(), Some("q"));

        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        a.on_key(key('q'));
        assert!(a.should_quit);
    }

    #[test]
    fn chart_keys_cycle_ranges_and_toggle_overlays() {
        let mut a = app();
        a.screen = Screen::Chart;
        a.chart.range = Range::M1;
        a.on_key(key(']'));
        assert_eq!(a.chart.range, Range::M3);
        a.on_key(key('['));
        assert_eq!(a.chart.range, Range::M1);

        let sma = a.chart.show_sma;
        a.on_key(key('m'));
        assert_eq!(a.chart.show_sma, !sma);

        a.on_key(key('i'));
        assert_eq!(a.chart.pane, Pane::Rsi);
    }

    #[test]
    fn list_navigation_is_clamped_to_the_visible_rows() {
        let mut a = app();
        a.screen = Screen::Screener;
        a.on_event(DataEvent::Quotes(vec![
            quote("AAA", 1.0, 0.0, 1.0),
            quote("BBB", 2.0, 0.0, 2.0),
        ]));

        a.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(a.screener.cursor, 1);

        a.on_key(key('j'));
        assert_eq!(
            a.screener.cursor, 1,
            "cursor must not run past the last row"
        );

        a.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(a.screener.cursor, 0);
        a.on_key(key('k'));
        assert_eq!(a.screener.cursor, 0);
    }

    #[test]
    fn activities_are_named_and_cleared_in_any_order() {
        let mut a = app();
        assert!(!a.is_busy());

        a.on_event(DataEvent::Begin("HBL history".into()));
        a.on_event(DataEvent::Begin("market board".into()));
        assert!(a.is_busy());
        assert_eq!(a.activities, vec!["HBL history", "market board"]);

        // Concurrent work can finish out of order.
        a.on_event(DataEvent::End("market board".into()));
        assert_eq!(a.activities, vec!["HBL history"]);

        a.on_event(DataEvent::End("HBL history".into()));
        assert!(!a.is_busy());
    }

    #[test]
    fn duplicate_activity_labels_unwind_one_at_a_time() {
        // Re-selecting a symbol can put the same label in flight twice;
        // clearing all of them would hide work that is still running.
        let mut a = app();
        a.on_event(DataEvent::Begin("HBL history".into()));
        a.on_event(DataEvent::Begin("HBL history".into()));

        a.on_event(DataEvent::End("HBL history".into()));
        assert_eq!(a.activities.len(), 1, "one End must clear only one Begin");
        assert!(a.is_busy());

        a.on_event(DataEvent::End("HBL history".into()));
        assert!(!a.is_busy());
    }

    #[test]
    fn an_unmatched_end_does_not_underflow() {
        let mut a = app();
        a.on_event(DataEvent::End("never started".into()));
        assert!(a.activities.is_empty());
        assert!(!a.is_busy());
    }

    #[test]
    fn backfill_progress_is_tracked_and_cleared() {
        let mut a = app();
        a.on_event(DataEvent::Backfill(BackfillProgress {
            done: 12,
            total: 48,
            day: "2026-05-06".into(),
            rows: Some(629),
        }));
        let b = a.backfill.as_ref().unwrap();
        assert_eq!(b.done, 12);
        assert!((b.ratio() - 0.25).abs() < 1e-9);
        assert!(a.is_busy(), "a running backfill counts as busy");

        a.on_event(DataEvent::BackfillDone);
        assert!(a.backfill.is_none());
        assert!(!a.is_busy());
    }

    #[test]
    fn backfill_ratio_is_bounded_and_safe_when_empty() {
        let p = |done, total| BackfillProgress {
            done,
            total,
            day: "2026-01-01".into(),
            rows: None,
        };
        assert_eq!(p(0, 0).ratio(), 1.0, "no work to do reads as complete");
        assert_eq!(p(0, 10).ratio(), 0.0);
        assert_eq!(p(99, 10).ratio(), 1.0, "must clamp, never exceed 1.0");
    }

    #[test]
    fn spinner_advances_and_wraps_without_overflow() {
        let mut a = app();
        let first = a.spinner_glyph();
        a.tick();
        assert_ne!(a.spinner_glyph(), first, "the spinner must animate");

        // Must not panic after a long-running session.
        a.spinner = usize::MAX;
        a.tick();
        assert!(a.spinner_glyph().is_alphanumeric() || !a.spinner_glyph().is_control());
    }

    /// A market with more symbols than any board can display.
    fn busy_market() -> App {
        let mut a = app();
        let quotes: Vec<Quote> = (0..60)
            .map(|i| {
                let mut q = quote(
                    &format!("S{i:02}"),
                    10.0 + i as f64,
                    i as f64 - 30.0,
                    1000.0,
                );
                q.volume = 1000.0 + i as f64;
                q
            })
            .collect();
        a.on_event(DataEvent::Quotes(quotes));
        a.screener.equities_only = false;
        a.screen = Screen::Dashboard;
        // Stand in for a render: 3 boards of 12 rows each.
        a.dash_layout.set(DashLayout {
            boards: [Board::Gainers, Board::Losers, Board::Active],
            count: 3,
            rows: 12,
            sector_rows: 10,
        });
        a
    }

    #[test]
    fn dashboard_cursor_cannot_run_past_the_rows_on_screen() {
        // The bug: the cursor indexed the whole market (60 symbols) while the
        // board only drew 12 rows, so it walked off-screen and the highlight
        // vanished.
        let mut a = busy_market();
        for _ in 0..50 {
            a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(a.dashboard.cursor, 11, "must stop at the last drawn row");
        assert!(
            a.dash_symbol().is_some(),
            "the cursor must always sit on a visible row"
        );
    }

    #[test]
    fn dashboard_end_key_lands_on_the_last_visible_row() {
        let mut a = busy_market();
        a.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(a.dashboard.cursor, 11);

        let expected = a.leaderboard(Board::Gainers, 12)[11].symbol.clone();
        assert_eq!(a.dash_symbol().as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn dashboard_arrows_switch_boards_and_track_the_right_symbol() {
        let mut a = busy_market();
        assert_eq!(a.dash_board(), Board::Gainers);
        let top_gainer = a.dash_symbol().unwrap();

        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(a.dash_board(), Board::Losers);
        let top_loser = a.dash_symbol().unwrap();
        assert_ne!(top_gainer, top_loser);
        assert_eq!(a.selected, top_loser, "selection follows the focused board");

        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(a.dash_board(), Board::Active);

        // Wraps back around.
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(a.dash_board(), Board::Gainers);
    }

    #[test]
    fn dashboard_cursor_is_clamped_when_a_shorter_board_is_focused() {
        let mut a = app();
        // Two gainers, but only one name is down on the day.
        let mut down = quote("DOWN", 10.0, -5.0, 100.0);
        down.volume = 100.0;
        a.on_event(DataEvent::Quotes(vec![
            quote("UP1", 10.0, 5.0, 100.0),
            quote("UP2", 10.0, 4.0, 100.0),
            down,
        ]));
        a.screener.equities_only = false;
        a.screen = Screen::Dashboard;
        a.dash_layout.set(DashLayout {
            boards: [Board::Gainers, Board::Losers, Board::Active],
            count: 3,
            rows: 12,
            sector_rows: 10,
        });

        a.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(a.dashboard.cursor, 2);

        // Losers holds a single row; the cursor must come back into range.
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(
            a.dashboard.cursor < a.leaderboard(Board::Losers, 12).len(),
            "cursor must be clamped to the shorter board"
        );
        assert!(a.dash_symbol().is_some());
    }

    #[test]
    fn dashboard_navigation_is_safe_before_the_first_render() {
        // dash_layout starts with rows = 0; keys must not panic or select.
        let mut a = app();
        a.screen = Screen::Dashboard;
        a.on_event(DataEvent::Quotes(vec![quote("AAA", 10.0, 1.0, 100.0)]));
        a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.dashboard.cursor, 0);
    }

    #[test]
    fn dashboard_enter_opens_the_focused_row_in_the_chart() {
        let mut a = busy_market();
        a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let expected = a.dash_symbol().unwrap();

        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.screen, Screen::Chart);
        assert_eq!(a.selected, expected);
    }

    /// A market spread across more sectors than the heatmap can draw.
    fn many_sectors(visible: usize) -> App {
        let mut a = app();
        let quotes: Vec<Quote> = (0..30)
            .map(|i| {
                let mut q = quote(&format!("S{i:02}"), 10.0, i as f64 - 15.0, 1000.0);
                q.sector = format!("SECTOR{i:02}");
                q.volume = 1000.0 + i as f64;
                q
            })
            .collect();
        a.on_event(DataEvent::Quotes(quotes));
        a.screener.equities_only = false;
        a.screen = Screen::Dashboard;
        a.dashboard.focus = DashFocus::Sectors;
        a.publish_sector_rows(visible);
        a
    }

    #[test]
    fn s_toggles_focus_between_boards_and_the_heatmap() {
        let mut a = app();
        a.screen = Screen::Dashboard;
        assert_eq!(a.dashboard.focus, DashFocus::Boards);

        a.on_key(key('s'));
        assert_eq!(a.dashboard.focus, DashFocus::Sectors);
        a.on_key(key('s'));
        assert_eq!(a.dashboard.focus, DashFocus::Boards);
    }

    #[test]
    fn heatmap_scrolls_and_clamps_to_the_sector_count() {
        let mut a = many_sectors(10);
        assert_eq!(a.sectors().len(), 30);

        for _ in 0..100 {
            a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(a.dashboard.sector, 29, "must stop at the last sector");
        assert!(a.sector_at_cursor().is_some());

        for _ in 0..100 {
            a.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        assert_eq!(a.dashboard.sector, 0);
        assert_eq!(a.dashboard.sector_offset, 0);
    }

    #[test]
    fn heatmap_offset_follows_the_cursor_out_of_the_window() {
        let mut a = many_sectors(10);
        // Within the first window, nothing scrolls.
        for _ in 0..9 {
            a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(a.dashboard.sector, 9);
        assert_eq!(a.dashboard.sector_offset, 0, "row 9 is still on screen");

        // Stepping past the window edge scrolls by exactly one row.
        a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(a.dashboard.sector, 10);
        assert_eq!(a.dashboard.sector_offset, 1);

        // And scrolls back when the cursor returns above the window.
        for _ in 0..10 {
            a.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        assert_eq!(a.dashboard.sector, 0);
        assert_eq!(a.dashboard.sector_offset, 0);
    }

    #[test]
    fn heatmap_end_and_page_keys_keep_the_cursor_visible() {
        let mut a = many_sectors(10);
        a.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(a.dashboard.sector, 29);
        let off = a.dashboard.sector_offset;
        assert!(
            a.dashboard.sector >= off && a.dashboard.sector < off + 10,
            "cursor {} outside window starting {off}",
            a.dashboard.sector
        );

        a.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(a.dashboard.sector, 19);
        let off = a.dashboard.sector_offset;
        assert!(a.dashboard.sector >= off && a.dashboard.sector < off + 10);
    }

    #[test]
    fn heatmap_enter_filters_the_screener_to_that_sector() {
        let mut a = many_sectors(10);
        a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let target = a.sector_at_cursor().unwrap().name;

        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.screen, Screen::Screener);
        assert_eq!(a.sector_filter.as_deref(), Some(target.as_str()));

        let rows = a.visible_quotes();
        assert!(!rows.is_empty());
        assert!(
            rows.iter().all(|q| q.sector == target),
            "the screener must show only that sector"
        );

        // Esc backs out of the drill-through.
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(a.sector_filter, None);
        assert_eq!(a.visible_quotes().len(), 30);
    }

    #[test]
    fn heatmap_keys_are_safe_with_no_sectors() {
        let mut a = app();
        a.screen = Screen::Dashboard;
        a.dashboard.focus = DashFocus::Sectors;
        a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.dashboard.sector, 0);
        assert_eq!(a.screen, Screen::Dashboard);
    }

    #[test]
    fn ytd_range_is_bounded_by_the_calendar_not_a_session_count() {
        let year_start = crate::cache::year_start_ts();
        let bars: Vec<Bar> = (0..400)
            .map(|i| Bar {
                ts: year_start - 200 * 86_400 + i * 86_400,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
            })
            .collect();

        let ytd = App::trim_range(&bars, Range::Ytd);
        assert!(!ytd.is_empty());
        assert!(
            ytd.iter().all(|b| b.ts >= year_start),
            "YTD must exclude last year's sessions"
        );
        assert_eq!(App::trim_range(&bars, Range::Max).len(), 400);
        assert_eq!(App::trim_range(&bars, Range::D5).len(), 5);
    }

    #[test]
    fn c_cycles_the_chart_style_and_wraps() {
        let mut a = app();
        a.screen = Screen::Chart;
        assert_eq!(a.chart.style, ChartStyle::Candles);

        for expected in [
            ChartStyle::Line,
            ChartStyle::Dots,
            ChartStyle::Area,
            ChartStyle::Candles,
        ] {
            a.on_key(key('c'));
            assert_eq!(a.chart.style, expected);
        }
    }

    #[test]
    fn solid_styles_use_a_block_marker_and_fine_styles_use_braille() {
        use ratatui::symbols::Marker;
        // This is the whole point of the option: braille renders a thin
        // diagonal as a dotted trail, half-blocks render it solid.
        assert_eq!(ChartStyle::Line.marker(), Marker::HalfBlock);
        assert_eq!(ChartStyle::Area.marker(), Marker::HalfBlock);
        assert_eq!(ChartStyle::Dots.marker(), Marker::Braille);
        // Candles keep braille — wicks need the sub-cell precision.
        assert_eq!(ChartStyle::Candles.marker(), Marker::Braille);
    }

    #[test]
    fn every_chart_style_is_reachable_by_cycling() {
        let mut seen = vec![ChartStyle::Candles];
        let mut s = ChartStyle::Candles;
        for _ in 0..ChartStyle::ALL.len() {
            s = s.next();
            seen.push(s);
        }
        for style in ChartStyle::ALL {
            assert!(seen.contains(&style), "{style:?} is unreachable");
        }
    }

    fn click(a: &mut App, x: u16, y: u16) {
        a.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    }

    fn wheel(a: &mut App, x: u16, y: u16, down: bool) {
        a.on_mouse(MouseEvent {
            kind: if down {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            },
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    }

    /// Register a single full-width row target plus its zone.
    fn put(a: &App, y: u16, target: Target, zone: Zone) {
        let r = ratatui::layout::Rect {
            x: 0,
            y,
            width: 40,
            height: 1,
        };
        let mut hits = a.hits.borrow_mut();
        hits.zone(
            ratatui::layout::Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 40,
            },
            zone,
        );
        hits.target(r, target);
    }

    #[test]
    fn clicking_a_tab_switches_screen() {
        let mut a = app();
        put(&a, 1, Target::Tab(4), Zone::Screener);
        click(&mut a, 3, 1);
        assert_eq!(a.screen, Screen::ALL[4]);
    }

    #[test]
    fn clicking_a_tab_index_past_the_end_is_ignored() {
        // Guards against a stale hit map from a build with more screens.
        let mut a = app();
        put(&a, 1, Target::Tab(99), Zone::Screener);
        click(&mut a, 3, 1);
        assert_eq!(a.screen, Screen::Dashboard);
    }

    #[test]
    fn clicking_a_screener_row_selects_it_and_double_click_opens_the_chart() {
        let mut a = app();
        a.on_event(DataEvent::Quotes(vec![
            quote("AAA", 10.0, 1.0, 100.0),
            quote("BBB", 20.0, 2.0, 200.0),
        ]));
        a.screener.equities_only = false;
        put(&a, 5, Target::ScreenerRow(1), Zone::Screener);

        let expected = a.visible_quotes()[1].symbol.clone();
        click(&mut a, 3, 5);
        assert_eq!(a.screener.cursor, 1);
        assert_eq!(a.selected, expected);
        assert_eq!(a.screen, Screen::Dashboard, "one click must not navigate");

        click(&mut a, 3, 5);
        assert_eq!(a.screen, Screen::Chart, "double click opens the chart");
    }

    #[test]
    fn two_clicks_on_different_rows_are_not_a_double_click() {
        let mut a = app();
        a.on_event(DataEvent::Quotes(vec![
            quote("AAA", 10.0, 1.0, 100.0),
            quote("BBB", 20.0, 2.0, 200.0),
        ]));
        a.screener.equities_only = false;

        put(&a, 5, Target::ScreenerRow(0), Zone::Screener);
        click(&mut a, 3, 5);
        a.hits.borrow_mut().clear();
        put(&a, 6, Target::ScreenerRow(1), Zone::Screener);
        click(&mut a, 3, 6);

        assert_eq!(a.screen, Screen::Dashboard, "different rows must not open");
    }

    #[test]
    fn the_wheel_scrolls_whatever_is_under_the_pointer() {
        let mut a = app();
        a.on_event(DataEvent::Quotes(
            (0..40)
                .map(|i| quote(&format!("S{i:02}"), 10.0, i as f64, 100.0))
                .collect(),
        ));
        a.screener.equities_only = false;
        put(&a, 5, Target::ScreenerRow(0), Zone::Screener);

        wheel(&mut a, 3, 5, true);
        assert_eq!(a.screener.cursor, WHEEL_LINES);
        wheel(&mut a, 3, 5, false);
        assert_eq!(a.screener.cursor, 0);
        // Cannot be driven past either end.
        wheel(&mut a, 3, 5, false);
        assert_eq!(a.screener.cursor, 0);
    }

    #[test]
    fn the_wheel_over_the_chart_changes_timeframe() {
        let mut a = app();
        a.chart.range = Range::M3;
        put(&a, 5, Target::ChartStyle, Zone::Chart);

        wheel(&mut a, 3, 5, true);
        assert_eq!(a.chart.range, Range::M6, "down walks toward longer windows");
        wheel(&mut a, 3, 5, false);
        assert_eq!(a.chart.range, Range::M3);
    }

    #[test]
    fn the_wheel_over_nothing_does_nothing() {
        let mut a = app();
        a.on_event(DataEvent::Quotes(vec![quote("AAA", 10.0, 1.0, 100.0)]));
        wheel(&mut a, 99, 99, true);
        assert_eq!(a.screener.cursor, 0);
    }

    #[test]
    fn clicking_a_board_row_focuses_that_board() {
        let mut a = busy_market();
        a.dashboard.focus = DashFocus::Sectors;
        put(&a, 4, Target::BoardRow { board: 1, row: 2 }, Zone::Board(1));

        click(&mut a, 3, 4);
        assert_eq!(a.dashboard.focus, DashFocus::Boards);
        assert_eq!(a.dashboard.board, 1);
        assert_eq!(a.dashboard.cursor, 2);
        assert!(!a.selected.is_empty());
    }

    #[test]
    fn double_clicking_a_sector_filters_the_screener() {
        let mut a = many_sectors(10);
        put(&a, 3, Target::SectorRow(2), Zone::Sectors);

        click(&mut a, 3, 3);
        assert_eq!(a.dashboard.sector, 2);
        assert_eq!(a.screen, Screen::Dashboard);

        let target = a.sector_at_cursor().unwrap().name;
        click(&mut a, 3, 3);
        assert_eq!(a.screen, Screen::Screener);
        assert_eq!(a.sector_filter.as_deref(), Some(target.as_str()));
    }

    #[test]
    fn clicking_chart_controls_matches_the_keys() {
        let mut a = app();
        a.chart.range = Range::D5;
        put(&a, 2, Target::ChartRange(5), Zone::Chart);
        click(&mut a, 3, 2);
        assert_eq!(a.chart.range, Range::ALL[5]);

        a.hits.borrow_mut().clear();
        put(&a, 2, Target::ChartStyle, Zone::Chart);
        let before = a.chart.style;
        click(&mut a, 3, 2);
        assert_eq!(a.chart.style, before.next());
    }

    #[test]
    fn clicking_empty_panel_space_still_moves_focus() {
        let mut a = app();
        a.hits.borrow_mut().zone(
            ratatui::layout::Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 10,
            },
            Zone::Sectors,
        );
        // No target here, only the zone.
        click(&mut a, 5, 5);
        assert_eq!(a.dashboard.focus, DashFocus::Sectors);
    }

    #[test]
    fn a_click_dismisses_the_help_overlay_without_reaching_beneath_it() {
        let mut a = app();
        put(&a, 1, Target::Tab(3), Zone::Screener);
        a.show_help = true;

        click(&mut a, 3, 1);
        assert!(!a.show_help);
        assert_eq!(
            a.screen,
            Screen::Dashboard,
            "the click must not fall through to the tab underneath"
        );
    }

    #[test]
    fn clicking_overlay_labels_toggles_them() {
        let mut a = app();
        let before = (
            a.chart.show_sma,
            a.chart.show_ema,
            a.chart.show_bollinger,
            a.chart.show_donchian,
            a.chart.show_ichimoku,
            a.chart.show_levels,
        );

        for i in 0..6 {
            a.hits.borrow_mut().clear();
            put(&a, 2, Target::ChartOverlay(i), Zone::Chart);
            click(&mut a, 3, 2);
        }

        assert_eq!(
            (
                a.chart.show_sma,
                a.chart.show_ema,
                a.chart.show_bollinger,
                a.chart.show_donchian,
                a.chart.show_ichimoku,
                a.chart.show_levels,
            ),
            (
                !before.0, !before.1, !before.2, !before.3, !before.4, !before.5
            ),
            "each overlay label must toggle its own flag"
        );
    }

    #[test]
    fn an_out_of_range_overlay_index_is_ignored() {
        let mut a = app();
        let before = a.chart.show_sma;
        put(&a, 2, Target::ChartOverlay(99), Zone::Chart);
        click(&mut a, 3, 2);
        assert_eq!(a.chart.show_sma, before);
    }

    #[test]
    fn clicking_a_sort_header_sorts_then_reverses() {
        let mut a = app();
        a.on_event(DataEvent::Quotes(vec![
            quote("AAA", 10.0, 5.0, 100.0),
            quote("BBB", 20.0, -5.0, 200.0),
        ]));
        a.screener.equities_only = false;
        a.screener.sort = SortKey::Turnover;

        let change = a
            .sort_keys()
            .iter()
            .position(|k| *k == SortKey::Change)
            .unwrap();

        put(&a, 1, Target::SortColumn(change), Zone::Screener);
        click(&mut a, 3, 1);
        assert_eq!(a.screener.sort, SortKey::Change);
        assert!(a.screener.descending, "a fresh column sorts high-to-low");
        assert_eq!(a.visible_quotes()[0].symbol, "AAA");

        // Clicking the active column reverses it.
        click(&mut a, 3, 1);
        assert_eq!(a.screener.sort, SortKey::Change);
        assert!(!a.screener.descending);
        assert_eq!(a.visible_quotes()[0].symbol, "BBB");
    }

    #[test]
    fn clicking_the_symbol_header_sorts_ascending_first() {
        // Alphabetical is the one column where high-to-low is the wrong
        // default.
        let mut a = app();
        a.on_event(DataEvent::Quotes(vec![quote("AAA", 10.0, 1.0, 100.0)]));
        let sym = a
            .sort_keys()
            .iter()
            .position(|k| *k == SortKey::Symbol)
            .unwrap();
        put(&a, 1, Target::SortColumn(sym), Zone::Screener);
        click(&mut a, 3, 1);
        assert_eq!(a.screener.sort, SortKey::Symbol);
        assert!(!a.screener.descending);
    }

    #[test]
    fn clicking_help_and_quit_works() {
        let mut a = app();
        put(&a, 9, Target::Help, Zone::Screener);
        click(&mut a, 3, 9);
        assert!(a.show_help);

        // The overlay swallows the next click, so dismiss it first.
        a.show_help = false;
        a.hits.borrow_mut().clear();
        put(&a, 9, Target::Quit, Zone::Screener);
        click(&mut a, 3, 9);
        assert!(a.should_quit);
    }

    #[test]
    fn capital_m_toggles_mouse_reporting() {
        let mut a = app();
        assert!(a.mouse_enabled);
        a.on_key(key('M'));
        assert!(!a.mouse_enabled);
        assert!(a.status.contains("selection"));
        a.on_key(key('M'));
        assert!(a.mouse_enabled);
    }

    #[test]
    fn step_and_keep_visible_are_bounded() {
        assert_eq!(step(0, -5, 10), 0);
        assert_eq!(step(9, 5, 10), 9);
        assert_eq!(step(0, 0, 0), 0, "an empty list has no cursor");
        assert_eq!(step(5, -2, 10), 3);

        // The window slides the least amount that reveals the cursor.
        assert_eq!(keep_visible(0, 0, 10), 0);
        assert_eq!(keep_visible(12, 0, 10), 3);
        assert_eq!(keep_visible(2, 5, 10), 2);
        assert_eq!(keep_visible(7, 5, 10), 5, "already visible, do not move");
    }

    #[test]
    fn help_overlay_is_dismissed_by_any_key() {
        let mut a = app();
        a.on_key(key('?'));
        assert!(a.show_help);
        a.on_key(key('j'));
        assert!(!a.show_help);
    }
}
