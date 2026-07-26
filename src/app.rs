//! Application state and input handling.
//!
//! The UI is a pure function of [`App`]: the event loop mutates state here and
//! [`crate::ui`] renders it. Network work never happens on this path — screens
//! post [`DataRequest`]s to a background worker and receive [`DataEvent`]s
//! back, so the interface stays responsive while PSX is slow.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;

use crate::cache::Store;
use crate::model::{Bar, Company, Index, Quote, SymbolInfo, Tick};

/// The benchmark every risk statistic is measured against.
pub const BENCHMARK: &str = "KSE100";

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
    /// A unit of background work finished (used to drive the busy indicator).
    Done,
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
}

impl Screen {
    pub const ALL: [Screen; 6] = [
        Screen::Dashboard,
        Screen::Screener,
        Screen::Chart,
        Screen::Analysis,
        Screen::Company,
        Screen::Intraday,
    ];

    pub fn title(&self) -> &'static str {
        match self {
            Screen::Dashboard => "Dashboard",
            Screen::Screener => "Screener",
            Screen::Chart => "Chart",
            Screen::Analysis => "Analysis",
            Screen::Company => "Company",
            Screen::Intraday => "Intraday",
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
}

impl SortKey {
    pub const ALL: [SortKey; 5] = [
        SortKey::Symbol,
        SortKey::Price,
        SortKey::Change,
        SortKey::Volume,
        SortKey::Turnover,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            SortKey::Symbol => "Symbol",
            SortKey::Price => "Price",
            SortKey::Change => "Change %",
            SortKey::Volume => "Volume",
            SortKey::Turnover => "Turnover",
        }
    }
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
        }
    }
}

/// How much history the chart shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    M1,
    M3,
    M6,
    Y1,
    Y3,
    Max,
}

impl Range {
    pub const ALL: [Range; 6] = [
        Range::M1,
        Range::M3,
        Range::M6,
        Range::Y1,
        Range::Y3,
        Range::Max,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Range::M1 => "1M",
            Range::M3 => "3M",
            Range::M6 => "6M",
            Range::Y1 => "1Y",
            Range::Y3 => "3Y",
            Range::Max => "MAX",
        }
    }

    /// Number of trading sessions to display, or `None` for everything.
    pub fn sessions(&self) -> Option<usize> {
        match self {
            Range::M1 => Some(22),
            Range::M3 => Some(65),
            Range::M6 => Some(125),
            Range::Y1 => Some(250),
            Range::Y3 => Some(750),
            Range::Max => None,
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
}

impl Pane {
    pub const ALL: [Pane; 5] = [
        Pane::Volume,
        Pane::Rsi,
        Pane::Macd,
        Pane::Atr,
        Pane::Stochastic,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Pane::Volume => "Volume",
            Pane::Rsi => "RSI(14)",
            Pane::Macd => "MACD(12,26,9)",
            Pane::Atr => "ATR(14)",
            Pane::Stochastic => "Stoch(14,3)",
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
    /// Draw candles rather than a close line.
    pub candles: bool,
}

impl Default for ChartState {
    fn default() -> Self {
        Self {
            range: Range::M6,
            pane: Pane::Volume,
            show_sma: true,
            show_ema: false,
            show_bollinger: false,
            candles: true,
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
    pub chart: ChartState,
    pub company_tab: CompanyTab,
    pub announcement_cursor: usize,

    pub watchlist: BTreeSet<String>,
    /// `Some` while the search prompt is open.
    pub search: Option<String>,
    pub show_help: bool,
    pub status: String,
    pub error: Option<String>,
    /// Outstanding background requests, for the activity indicator.
    pub inflight: usize,
}

impl App {
    pub fn new(store: Arc<Store>, tx: UnboundedSender<DataRequest>) -> Self {
        let watchlist = load_watchlist(&store);
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
            chart: ChartState::default(),
            company_tab: CompanyTab::Profile,
            announcement_cursor: 0,
            watchlist,
            search: None,
            show_help: false,
            status: "Loading market data…".into(),
            error: None,
            inflight: 0,
        }
    }

    pub fn request(&mut self, req: DataRequest) {
        self.inflight += 1;
        let _ = self.tx.send(req);
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
                if c.symbol == self.selected {
                    self.announcement_cursor = 0;
                    self.company = Some(*c);
                }
            }
            DataEvent::Status(s) => self.status = s,
            DataEvent::Error(e) => self.error = Some(e),
            DataEvent::Done => self.inflight = self.inflight.saturating_sub(1),
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
        self.selected = symbol.clone();
        // Clear stale series so a slow fetch never renders under a new ticker.
        self.bars.clear();
        self.ticks.clear();
        self.company = None;
        self.announcement_cursor = 0;
        self.request(DataRequest::LoadSymbol(symbol.clone()));
        self.request(DataRequest::LoadCompany(symbol));
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

    /// Bars trimmed to the chart's selected range.
    pub fn ranged_bars(&self) -> &[Bar] {
        match self.chart.range.sessions() {
            Some(n) if self.bars.len() > n => &self.bars[self.bars.len() - n..],
            _ => &self.bars,
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
        rows.sort_by(|a, b| {
            let ord = match self.screener.sort {
                SortKey::Symbol => a.symbol.cmp(&b.symbol),
                SortKey::Price => a.current.total_cmp(&b.current),
                SortKey::Change => a.change_pct.total_cmp(&b.change_pct),
                SortKey::Volume => a.volume.total_cmp(&b.volume),
                SortKey::Turnover => a.turnover().total_cmp(&b.turnover()),
            };
            if desc { ord.reverse() } else { ord }
        });
        rows
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
            KeyCode::Esc => self.error = None,

            KeyCode::Char('/') => {
                self.search = Some(String::new());
                self.screen = Screen::Screener;
            }
            KeyCode::Char('r') => {
                self.status = "Refreshing…".into();
                self.request(DataRequest::RefreshMarket);
                if !self.selected.is_empty() {
                    let sym = self.selected.clone();
                    self.request(DataRequest::LoadSymbol(sym));
                }
            }

            KeyCode::Tab => self.cycle_screen(1),
            KeyCode::BackTab => self.cycle_screen(-1),
            KeyCode::Char(c @ '1'..='6') => {
                let idx = c as usize - '1' as usize;
                self.screen = Screen::ALL[idx];
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

    fn cycle_screen(&mut self, delta: isize) {
        let n = Screen::ALL.len() as isize;
        let i = (self.screen.index() as isize + delta).rem_euclid(n);
        self.screen = Screen::ALL[i as usize];
    }

    fn on_screen_key(&mut self, key: KeyEvent) {
        match self.screen {
            Screen::Screener | Screen::Dashboard => self.on_list_key(key),
            Screen::Chart => self.on_chart_key(key),
            Screen::Company => self.on_company_key(key),
            Screen::Analysis | Screen::Intraday => {}
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
                // Cycle the sort column.
                let i = SortKey::ALL
                    .iter()
                    .position(|k| *k == self.screener.sort)
                    .unwrap_or(0);
                self.screener.sort = SortKey::ALL[(i + 1) % SortKey::ALL.len()];
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
            KeyCode::Char('c') => self.chart.candles = !self.chart.candles,
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

// --- watchlist persistence ----------------------------------------------

const WATCHLIST_KEY: &str = "watchlist";

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
    fn selecting_a_symbol_clears_the_previous_series() {
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
        a.select("OGDC".into());
        assert!(
            a.bars.is_empty(),
            "stale bars must not render under a new ticker"
        );
        assert!(a.company.is_none());
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
        let mut a = app();
        a.screen = Screen::Intraday;
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.screen, Screen::Dashboard);

        a.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(a.screen, Screen::Intraday);
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
    fn help_overlay_is_dismissed_by_any_key() {
        let mut a = app();
        a.on_key(key('?'));
        assert!(a.show_help);
        a.on_key(key('j'));
        assert!(!a.show_help);
    }
}
