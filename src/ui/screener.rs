//! Screener — the full sortable market board.
//!
//! The rows come from [`App::visible_quotes`] already filtered and sorted; this
//! module only decides which columns fit and how each cell is painted. Columns
//! are dropped in priority order as the terminal narrows, so the symbol, the
//! last price and the day's move survive all the way down to a 20-column pane.

use ratatui::prelude::*;
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use crate::app::{App, SortKey};
use crate::model::Quote;

use super::theme;
use super::widgets;

// --- columns -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Symbol,
    Name,
    Sector,
    Ldcp,
    Open,
    High,
    Low,
    Current,
    Change,
    ChangePct,
    Volume,
    Turnover,
}

/// Left-to-right order on screen.
const DISPLAY: [Col; 12] = [
    Col::Symbol,
    Col::Name,
    Col::Sector,
    Col::Ldcp,
    Col::Open,
    Col::High,
    Col::Low,
    Col::Current,
    Col::Change,
    Col::ChangePct,
    Col::Volume,
    Col::Turnover,
];

/// Order in which columns earn their place as the pane widens. The identity,
/// the move and the liquidity come first; the intraday OHLC detail last.
const PRIORITY: [Col; 12] = [
    Col::Symbol,
    Col::ChangePct,
    Col::Current,
    Col::Volume,
    Col::Turnover,
    Col::Change,
    Col::Name,
    Col::High,
    Col::Low,
    Col::Open,
    Col::Ldcp,
    Col::Sector,
];

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Symbol => "SYMBOL",
            Col::Name => "COMPANY",
            Col::Sector => "SECTOR",
            Col::Ldcp => "LDCP",
            Col::Open => "OPEN",
            Col::High => "HIGH",
            Col::Low => "LOW",
            Col::Current => "CURRENT",
            Col::Change => "CHANGE",
            Col::ChangePct => "CHG%",
            Col::Volume => "VOLUME",
            Col::Turnover => "TURNOVER",
        }
    }

    fn width(self) -> u16 {
        match self {
            // Two extra cells carry the watchlist star.
            Col::Symbol => 11,
            Col::Name => 22,
            Col::Sector => 18,
            Col::Ldcp | Col::Open | Col::High | Col::Low => 9,
            Col::Current => 10,
            Col::Change | Col::ChangePct => 8,
            Col::Volume | Col::Turnover => 10,
        }
    }

    fn numeric(self) -> bool {
        !matches!(self, Col::Symbol | Col::Name | Col::Sector)
    }

    /// The sort key this column represents, if any.
    fn sort_key(self) -> Option<SortKey> {
        match self {
            Col::Symbol => Some(SortKey::Symbol),
            Col::Current => Some(SortKey::Price),
            Col::ChangePct => Some(SortKey::Change),
            Col::Volume => Some(SortKey::Volume),
            Col::Turnover => Some(SortKey::Turnover),
            _ => None,
        }
    }
}

/// Pick the columns that fit `avail` cells (one cell of spacing between each)
/// and hand any slack to the company name.
fn choose_columns(avail: u16) -> Vec<(Col, u16)> {
    if avail == 0 {
        return Vec::new();
    }
    // Below the width of a symbol plus a percentage there is nothing to
    // negotiate: squeeze the two most important columns in by hand.
    if avail < Col::Symbol.width() + 1 + Col::ChangePct.width() {
        if avail >= 5 + 1 + Col::ChangePct.width() {
            return vec![
                (Col::Symbol, avail - 1 - Col::ChangePct.width()),
                (Col::ChangePct, Col::ChangePct.width()),
            ];
        }
        return vec![(Col::Symbol, avail)];
    }

    let mut chosen: Vec<Col> = Vec::new();
    let mut used: u16 = 0;
    for col in PRIORITY {
        let need = col.width() + if chosen.is_empty() { 0 } else { 1 };
        if used + need <= avail {
            used += need;
            chosen.push(col);
        }
    }
    let mut out: Vec<(Col, u16)> = DISPLAY
        .iter()
        .filter(|c| chosen.contains(c))
        .map(|c| (*c, c.width()))
        .collect();

    let slack = avail - used;
    if slack > 0 {
        let flex = out
            .iter()
            .position(|(c, _)| *c == Col::Name)
            .or_else(|| out.iter().position(|(c, _)| *c == Col::Sector))
            .unwrap_or(0);
        out[flex].1 += slack;
    }
    out
}

/// The header text for a column, carrying the sort arrow when it is active.
fn header_text(col: Col, sort: SortKey, descending: bool, width: u16) -> String {
    let active = col.sort_key() == Some(sort);
    let label = if active {
        format!("{}{}", col.label(), if descending { "▼" } else { "▲" })
    } else {
        col.label().to_string()
    };
    theme::truncate(&label, width as usize)
}

/// The rendered value for one cell.
fn cell_text(col: Col, q: &Quote, app: &App, width: u16) -> String {
    let w = width as usize;
    match col {
        Col::Symbol => {
            let star = if app.watchlist.contains(&q.symbol) {
                "★ "
            } else {
                ""
            };
            format!(
                "{star}{}",
                theme::truncate(&q.symbol, w.saturating_sub(star.chars().count()))
            )
        }
        Col::Name => theme::truncate(&app.company_name(&q.symbol), w),
        Col::Sector => theme::truncate(&q.sector, w),
        Col::Ldcp => theme::price(q.ldcp),
        Col::Open => theme::price(q.open),
        Col::High => theme::price(q.high),
        Col::Low => theme::price(q.low),
        Col::Current => theme::price(q.current),
        Col::Change => theme::signed(q.change),
        Col::ChangePct => theme::pct(q.change_pct),
        Col::Volume => theme::compact(q.volume),
        Col::Turnover => theme::compact(q.turnover()),
    }
}

fn cell_style(col: Col, q: &Quote) -> Style {
    match col {
        Col::Symbol => Style::new().fg(theme::FG).bold(),
        Col::Name | Col::Sector => Style::new().fg(theme::MUTED),
        Col::Ldcp | Col::Open => Style::new().fg(theme::DIM),
        Col::High => Style::new().fg(theme::UP),
        Col::Low => Style::new().fg(theme::DOWN),
        Col::Current => Style::new().fg(theme::FG),
        Col::Change | Col::ChangePct => Style::new().fg(theme::change_color(q.change_pct)),
        Col::Volume | Col::Turnover => Style::new().fg(theme::VOLUME),
    }
}

fn to_cell(text: String, style: Style, numeric: bool) -> Cell<'static> {
    let line = Line::styled(text, style);
    Cell::from(if numeric { line.right_aligned() } else { line })
}

// --- summary lines -------------------------------------------------------

fn title(shown: usize, total: usize) -> String {
    format!("Screener  {shown}/{total}")
}

/// The compact filter + sort readout hung off the bottom border.
///
/// Groups are appended only while they fit in `max` cells, so the readout never
/// eats into the border on a narrow pane.
fn footer(app: &App, max: usize) -> Line<'static> {
    let flag = |on: bool| {
        if on {
            Style::new().fg(theme::WARN).bold()
        } else {
            theme::label_style()
        }
    };

    // (text, style) groups in descending order of importance.
    let mut groups: Vec<Vec<(String, Style)>> = vec![vec![
        (" sort ".into(), theme::label_style()),
        (
            format!(
                "{} {}",
                app.screener.sort.label(),
                if app.screener.descending {
                    "▼"
                } else {
                    "▲"
                }
            ),
            Style::new().fg(theme::ACCENT),
        ),
    ]];

    if let Some(q) = app.search.as_ref().filter(|q| !q.is_empty()) {
        groups.push(vec![
            (" · ".into(), theme::border_style()),
            (
                format!("/{}", theme::truncate(q, 16)),
                Style::new().fg(theme::ACCENT),
            ),
        ]);
    }
    if app.screener.watchlist_only {
        groups.push(vec![
            (" · ".into(), theme::border_style()),
            ("watchlist".into(), flag(true)),
        ]);
    }
    groups.push(vec![
        (" · ".into(), theme::border_style()),
        ("equities".into(), flag(app.screener.equities_only)),
    ]);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 1usize; // the trailing pad
    for group in groups {
        let need: usize = group.iter().map(|(t, _)| t.chars().count()).sum();
        if used + need > max {
            break;
        }
        used += need;
        spans.extend(group.into_iter().map(|(t, s)| Span::styled(t, s)));
    }
    if spans.is_empty() {
        return Line::default();
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

// --- entry point ---------------------------------------------------------

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if app.quotes.is_empty() {
        let block = widgets::panel("Screener");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height > 0 {
            f.render_widget(
                Paragraph::new(widgets::placeholder("Loading market data…")),
                inner,
            );
        }
        return;
    }

    let rows = app.visible_quotes();
    let heading = title(rows.len(), app.quotes.len());
    let block = widgets::panel(&heading)
        .title_bottom(footer(app, area.width.saturating_sub(2) as usize).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(widgets::placeholder("No rows match the current filters")),
            inner,
        );
        return;
    }

    let cols = choose_columns(inner.width);
    if cols.is_empty() {
        return;
    }

    let header = Row::new(
        cols.iter()
            .map(|(c, w)| {
                to_cell(
                    header_text(*c, app.screener.sort, app.screener.descending, *w),
                    if c.sort_key() == Some(app.screener.sort) {
                        Style::new().fg(theme::ACCENT).bold()
                    } else {
                        theme::header_style()
                    },
                    c.numeric(),
                )
            })
            .collect::<Vec<_>>(),
    )
    .height(1);

    let body: Vec<Row> = rows
        .iter()
        .map(|q| {
            Row::new(
                cols.iter()
                    .map(|(c, w)| {
                        let style = if *c == Col::Symbol && app.watchlist.contains(&q.symbol) {
                            Style::new().fg(theme::WARN).bold()
                        } else {
                            cell_style(*c, q)
                        };
                        to_cell(cell_text(*c, q, app, *w), style, c.numeric())
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();

    let widths: Vec<Constraint> = cols.iter().map(|(_, w)| Constraint::Length(*w)).collect();

    let table = Table::new(body, widths)
        .header(header)
        .column_spacing(1)
        .style(theme::value_style())
        .row_highlight_style(Style::new().bg(theme::SELECT_BG).bold());

    // Clamp defensively: the cursor is App's, and a filter change could in
    // principle leave it past the end of the freshly filtered board.
    let selected = app.screener.cursor.min(rows.len().saturating_sub(1));
    let mut state = TableState::new()
        .with_offset(app.screener.offset.min(selected))
        .with_selected(Some(selected));

    f.render_stateful_widget(table, inner, &mut state);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::{DataEvent, detached_channel};
    use crate::cache::Store;
    use crate::model::SymbolInfo;

    fn quote(symbol: &str, price: f64, pct: f64, volume: f64) -> Quote {
        Quote {
            symbol: symbol.into(),
            sector: "COMMERCIAL BANKS".into(),
            indices: vec![],
            ldcp: price,
            open: price,
            high: price * 1.01,
            low: price * 0.99,
            current: price,
            change: price * pct / 100.0,
            change_pct: pct,
            volume,
        }
    }

    fn app_with(quotes: Vec<Quote>) -> App {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _rx) = detached_channel();
        let mut a = App::new(store, tx);
        a.on_event(DataEvent::Quotes(quotes));
        a
    }

    #[test]
    fn narrow_panes_keep_the_identity_and_the_move() {
        let cols = choose_columns(24);
        let names: Vec<Col> = cols.iter().map(|(c, _)| *c).collect();
        assert!(names.contains(&Col::Symbol));
        assert!(names.contains(&Col::ChangePct));
        assert!(!names.contains(&Col::Sector));
    }

    #[test]
    fn columns_never_exceed_the_available_width() {
        for avail in 1u16..250 {
            let cols = choose_columns(avail);
            let total: u16 = cols.iter().map(|(_, w)| *w).sum::<u16>() + (cols.len() as u16 - 1);
            assert!(total <= avail, "avail {avail} overflowed to {total}");
        }
        assert!(choose_columns(0).is_empty());
    }

    #[test]
    fn slack_is_given_to_the_company_name() {
        let cols = choose_columns(200);
        let (_, name_w) = cols.iter().find(|(c, _)| *c == Col::Name).unwrap();
        assert!(
            *name_w > Col::Name.width(),
            "leftover space should widen the name"
        );
        let total: u16 = cols.iter().map(|(_, w)| *w).sum::<u16>() + cols.len() as u16 - 1;
        assert_eq!(total, 200, "a wide pane should be filled exactly");
    }

    #[test]
    fn every_column_is_present_on_a_wide_pane() {
        let cols = choose_columns(200);
        assert_eq!(cols.len(), DISPLAY.len());
        let order: Vec<Col> = cols.iter().map(|(c, _)| *c).collect();
        assert_eq!(order, DISPLAY.to_vec(), "display order must be preserved");
    }

    #[test]
    fn a_single_cell_pane_still_yields_a_symbol_column() {
        assert_eq!(choose_columns(3), vec![(Col::Symbol, 3)]);
        assert!(choose_columns(0).is_empty());
    }

    #[test]
    fn the_active_sort_column_carries_an_arrow() {
        assert_eq!(
            header_text(Col::Turnover, SortKey::Turnover, true, 20),
            "TURNOVER▼"
        );
        assert_eq!(
            header_text(Col::Turnover, SortKey::Turnover, false, 20),
            "TURNOVER▲"
        );
        assert_eq!(
            header_text(Col::Volume, SortKey::Turnover, true, 20),
            "VOLUME",
            "inactive columns stay plain"
        );
        // A header never overflows its column.
        assert_eq!(
            header_text(Col::Turnover, SortKey::Turnover, true, 4)
                .chars()
                .count(),
            4
        );
    }

    #[test]
    fn cells_use_the_theme_formatters() {
        let app = app_with(vec![quote("HBL", 292.0, -1.25, 1_234_567.0)]);
        let q = &app.quotes[0];
        assert_eq!(cell_text(Col::Current, q, &app, 10), "292.00");
        assert_eq!(cell_text(Col::ChangePct, q, &app, 8), "-1.25%");
        assert_eq!(cell_text(Col::Volume, q, &app, 10), "1.2M");
        assert_eq!(cell_text(Col::Turnover, q, &app, 10), "360.5M");
        assert_eq!(cell_text(Col::Sector, q, &app, 6), "COMME…");
    }

    #[test]
    fn watchlist_members_are_starred() {
        let mut app = app_with(vec![quote("HBL", 292.0, 1.0, 10.0)]);
        let q = app.quotes[0].clone();
        assert_eq!(cell_text(Col::Symbol, &q, &app, 11), "HBL");
        app.watchlist_toggle("HBL");
        assert_eq!(cell_text(Col::Symbol, &q, &app, 11), "★ HBL");
    }

    #[test]
    fn company_names_come_from_the_symbol_list_and_are_truncated() {
        let mut app = app_with(vec![quote("HBL", 292.0, 1.0, 10.0)]);
        app.on_event(DataEvent::Symbols(vec![SymbolInfo {
            symbol: "HBL".into(),
            name: "Habib Bank Limited".into(),
            sector_name: "COMMERCIAL BANKS".into(),
            is_etf: false,
            is_debt: false,
        }]));
        let q = app.quotes[0].clone();
        assert_eq!(cell_text(Col::Name, &q, &app, 8), "Habib B…");
        assert_eq!(cell_text(Col::Name, &q, &app, 40), "Habib Bank Limited");
    }

    #[test]
    fn missing_company_names_render_empty_rather_than_panicking() {
        let app = app_with(vec![quote("XYZ", 1.0, 0.0, 0.0)]);
        let q = &app.quotes[0];
        assert_eq!(cell_text(Col::Name, q, &app, 20), "");
        assert_eq!(cell_text(Col::Turnover, q, &app, 10), "0");
    }

    fn render(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, f.area(), app)).unwrap();
        term.backend().buffer().clone()
    }

    fn text_of(buf: &ratatui::buffer::Buffer) -> String {
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn renders_at_tiny_sizes_without_panicking() {
        let app = app_with(vec![
            quote("HBL", 292.0, 1.0, 100.0),
            quote("OGDC", 200.0, -1.0, 5.0),
        ]);
        for (w, h) in [(20u16, 10u16), (20, 2), (3, 3), (1, 1), (5, 40), (80, 1)] {
            let buf = render(&app, w, h);
            assert_eq!(buf.area.height, h);
        }
        assert!(text_of(&render(&app, 20, 10)).contains("HBL"));
    }

    #[test]
    fn renders_a_large_board_and_scrolls_to_the_cursor() {
        let quotes: Vec<Quote> = (0..400)
            .map(|i| {
                quote(
                    &format!("SYM{i:03}"),
                    10.0 + i as f64,
                    (i % 7) as f64 - 3.0,
                    i as f64,
                )
            })
            .collect();
        let mut app = app_with(quotes);
        app.screener.sort = SortKey::Symbol;
        app.screener.descending = false;
        app.screener.cursor = 350;

        let text = text_of(&render(&app, 200, 60));
        assert!(
            text.contains("SYM350"),
            "the table must scroll to the cursor"
        );
        assert!(text.contains("SYMBOL"), "the header must be drawn");
        assert!(text.contains("TURNOVER"), "a wide pane shows every column");
    }

    #[test]
    fn an_out_of_range_cursor_is_clamped() {
        let mut app = app_with(vec![quote("AAA", 1.0, 0.0, 1.0)]);
        app.screener.cursor = 999;
        assert!(text_of(&render(&app, 80, 20)).contains("AAA"));
    }

    #[test]
    fn empty_market_shows_the_loading_placeholder() {
        let app = app_with(vec![]);
        assert!(text_of(&render(&app, 60, 20)).contains("Loading market data"));
    }

    #[test]
    fn a_filtered_out_board_explains_itself() {
        let mut app = app_with(vec![quote("AAA", 1.0, 0.0, 1.0)]);
        app.screener.watchlist_only = true;
        assert!(text_of(&render(&app, 60, 20)).contains("No rows match"));
    }

    fn footer_text(app: &App, max: usize) -> String {
        footer(app, max)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn the_footer_reports_filters_and_sort() {
        let mut app = app_with(vec![quote("AAA", 1.0, 0.0, 1.0)]);
        app.screener.sort = SortKey::Volume;
        app.screener.descending = false;
        let text = footer_text(&app, 60);
        assert!(text.contains("Volume ▲"));
        assert!(text.contains("equities"));

        app.search = Some("hbl".into());
        assert!(footer_text(&app, 60).contains("/hbl"));

        // A narrow pane keeps the sort and drops the rest rather than
        // overflowing the border.
        let narrow = footer_text(&app, 16);
        assert!(
            narrow.chars().count() <= 16,
            "footer overflowed: {narrow:?}"
        );
        assert!(narrow.contains("sort"));
        assert_eq!(footer_text(&app, 4), "", "nothing fits, nothing is drawn");
    }

    #[test]
    fn the_title_counts_shown_versus_total() {
        assert_eq!(title(12, 500), "Screener  12/500");
    }
}
