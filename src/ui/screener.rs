//! Screener — the full sortable market board.
//!
//! The rows come from [`App::visible_quotes`] already filtered and sorted; this
//! module only decides which columns fit and how each cell is painted. Columns
//! are dropped in priority order as the terminal narrows, so the symbol, the
//! last price and the day's move survive all the way down to a 20-column pane.
//!
//! `f` swaps the price columns for a **valuation** view — P/E, EPS, growth,
//! margin, market cap and free float. Those come from company profiles, which
//! PSX only serves one symbol at a time and which are therefore cached for a
//! fraction of the board. That fraction is printed on the panel rather than
//! papered over: a ranking of the thirty scrips someone happened to open is not
//! a ranking of the market, and it must not be able to look like one.

use ratatui::prelude::*;
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use super::hit::{Target, Toggle, Zone};
use crate::app::{App, SortKey, Valuation};
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
    // --- valuation view ---
    Pe,
    Eps,
    EpsGrowth,
    NetMargin,
    MarketCap,
    FreeFloat,
}

/// Left-to-right order on screen, price view.
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

/// Left-to-right order on screen, valuation view. The last price and the day's
/// move stay: a multiple means nothing without the price it was struck from.
const DISPLAY_VALUATION: [Col; 10] = [
    Col::Symbol,
    Col::Name,
    Col::Current,
    Col::ChangePct,
    Col::MarketCap,
    Col::Pe,
    Col::Eps,
    Col::EpsGrowth,
    Col::NetMargin,
    Col::FreeFloat,
];

/// Priority for the valuation view — size and the earnings multiple first.
const PRIORITY_VALUATION: [Col; 10] = [
    Col::Symbol,
    Col::MarketCap,
    Col::Pe,
    Col::Eps,
    Col::EpsGrowth,
    Col::NetMargin,
    Col::Current,
    Col::FreeFloat,
    Col::ChangePct,
    Col::Name,
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
            Col::Pe => "P/E",
            Col::Eps => "EPS",
            Col::EpsGrowth => "EPSG%",
            Col::NetMargin => "MARGIN%",
            Col::MarketCap => "MKT CAP",
            Col::FreeFloat => "FLOAT%",
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
            Col::Pe => 7,
            Col::Eps | Col::EpsGrowth | Col::NetMargin | Col::FreeFloat => 8,
            Col::MarketCap => 10,
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
            Col::Pe => Some(SortKey::Pe),
            Col::Eps => Some(SortKey::Eps),
            Col::EpsGrowth => Some(SortKey::EpsGrowth),
            Col::NetMargin => Some(SortKey::NetMargin),
            Col::MarketCap => Some(SortKey::MarketCap),
            Col::FreeFloat => Some(SortKey::FreeFloat),
            _ => None,
        }
    }
}

/// Pick the columns that fit `avail` cells (one cell of spacing between each)
/// and hand any slack to the company name.
fn choose_columns(avail: u16, valuation: bool) -> Vec<(Col, u16)> {
    let (display, priority): (&[Col], &[Col]) = if valuation {
        (&DISPLAY_VALUATION, &PRIORITY_VALUATION)
    } else {
        (&DISPLAY, &PRIORITY)
    };
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
    for col in priority {
        let need = col.width() + if chosen.is_empty() { 0 } else { 1 };
        if used + need <= avail {
            used += need;
            chosen.push(*col);
        }
    }
    let mut out: Vec<(Col, u16)> = display
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

        // Fundamentals. A symbol with no cached profile — most of them, most
        // of the time — renders an em dash in every valuation cell. Nothing is
        // inferred from the quote to fill the gap.
        Col::Pe | Col::Eps | Col::EpsGrowth | Col::NetMargin | Col::MarketCap | Col::FreeFloat => {
            let v = app.fundamentals.get(&q.symbol);
            match col {
                Col::Pe => theme::opt(v.and_then(|v| v.pe), 2),
                Col::Eps => theme::opt(v.and_then(|v| v.eps), 2),
                Col::EpsGrowth => opt_pct(v.and_then(|v| v.eps_growth_pct)),
                Col::NetMargin => opt_pct(v.and_then(|v| v.net_margin_pct)),
                // `market_cap` is already in PKR: App converts from the
                // thousands PSX publishes exactly once, at the source.
                Col::MarketCap => match v.and_then(|v| v.market_cap) {
                    Some(m) => theme::compact(m),
                    None => "—".into(),
                },
                _ => match v.and_then(|v| v.free_float_pct) {
                    Some(p) => theme::pct_plain(p),
                    None => "—".into(),
                },
            }
        }
    }
}

/// A signed percentage that may be missing.
fn opt_pct(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() => theme::pct(v),
        _ => "—".into(),
    }
}

fn cell_style(col: Col, q: &Quote, val: Option<&Valuation>) -> Style {
    match col {
        Col::Symbol => Style::new().fg(theme::FG).bold(),
        Col::Name | Col::Sector => Style::new().fg(theme::MUTED),
        Col::Ldcp | Col::Open => Style::new().fg(theme::DIM),
        Col::High => Style::new().fg(theme::UP),
        Col::Low => Style::new().fg(theme::DOWN),
        Col::Current => Style::new().fg(theme::FG),
        Col::Change | Col::ChangePct => Style::new().fg(theme::change_color(q.change_pct)),
        Col::Volume | Col::Turnover => Style::new().fg(theme::VOLUME),

        // Growth and margin are signed, so they carry the up/down hue; the
        // rest are plain. A missing value stays dim so the eye reads the gap
        // as absence rather than as a number worth comparing.
        Col::EpsGrowth => signed_style(val.and_then(|v| v.eps_growth_pct)),
        Col::NetMargin => signed_style(val.and_then(|v| v.net_margin_pct)),
        Col::Pe | Col::Eps | Col::MarketCap | Col::FreeFloat => {
            if val.is_some() {
                Style::new().fg(theme::FG)
            } else {
                Style::new().fg(theme::DIM)
            }
        }
    }
}

fn signed_style(v: Option<f64>) -> Style {
    match v {
        Some(v) if v.is_finite() => Style::new().fg(theme::change_color(v)),
        _ => Style::new().fg(theme::DIM),
    }
}

fn to_cell(text: String, style: Style, numeric: bool) -> Cell<'static> {
    let line = Line::styled(text, style);
    Cell::from(if numeric { line.right_aligned() } else { line })
}

// --- summary lines -------------------------------------------------------

fn title(shown: usize, total: usize, valuation: bool) -> String {
    if valuation {
        format!("Valuation  {shown}/{total}")
    } else {
        format!("Screener  {shown}/{total}")
    }
}

/// How the valuation view states its own incompleteness.
///
/// This is the whole point of the readout: `34/483` says plainly that the
/// ranking below covers 34 of the 483 visible symbols, because company
/// profiles are fetched on demand and most have never been opened.
fn coverage(have: usize, shown: usize) -> String {
    format!("fundamentals: {have}/{shown} cached")
}

/// The compact filter + sort readout hung off the bottom border.
///
/// Groups are appended only while they fit in `max` cells, so the readout never
/// eats into the border on a narrow pane.
///
/// Returns the line together with where each switch ended up inside it, as
/// (offset in cells from the start of the line, width, switch). Only the caller
/// knows where a right-aligned title lands, so it does the final placement;
/// building the offsets here keeps them tied to the spans they describe, which
/// a dropped group would otherwise shift.
fn footer(
    app: &App,
    coverage_of: Option<(usize, usize)>,
    max: usize,
) -> (Line<'static>, Vec<(u16, u16, Toggle)>) {
    let flag = |on: bool| {
        if on {
            Style::new().fg(theme::WARN).bold()
        } else {
            theme::label_style()
        }
    };

    // (text, style, switch) groups in descending order of importance. A span
    // carrying a switch is the part of the group a click acts on; the
    // separators around it carry none.
    let mut groups: Vec<Vec<(String, Style, Option<Toggle>)>> = Vec::new();

    // Coverage leads, ahead of even the sort readout: on a pane too narrow for
    // everything, the caveat is the last thing that should be dropped.
    if let Some((have, shown)) = coverage_of {
        groups.push(vec![(
            format!(" {} ", coverage(have, shown)),
            if have < shown {
                Style::new().fg(theme::WARN).bold()
            } else {
                Style::new().fg(theme::ACCENT)
            },
            None,
        )]);
    }

    groups.push(vec![
        (" sort ".into(), theme::label_style(), None),
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
            Some(Toggle::SortDirection),
        ),
    ]);

    if let Some(q) = app.search.as_ref().filter(|q| !q.is_empty()) {
        groups.push(vec![
            (" · ".into(), theme::border_style(), None),
            (
                format!("/{}", theme::truncate(q, 16)),
                Style::new().fg(theme::ACCENT),
                None,
            ),
        ]);
    }
    if app.screener.watchlist_only {
        groups.push(vec![
            (" · ".into(), theme::border_style(), None),
            ("watchlist".into(), flag(true), Some(Toggle::Watchlist)),
        ]);
    }
    groups.push(vec![
        (" · ".into(), theme::border_style(), None),
        (
            "equities".into(),
            flag(app.screener.equities_only),
            Some(Toggle::Equities),
        ),
    ]);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut switches: Vec<(u16, u16, Toggle)> = Vec::new();
    let mut used = 1usize; // the trailing pad
    let mut offset = 0usize; // cells consumed by the spans pushed so far
    for group in groups {
        let need: usize = group.iter().map(|(t, _, _)| t.chars().count()).sum();
        if used + need > max {
            break;
        }
        used += need;
        for (text, style, toggle) in group {
            let w = text.chars().count();
            if let Some(toggle) = toggle {
                switches.push((offset as u16, w as u16, toggle));
            }
            offset += w;
            spans.push(Span::styled(text, style));
        }
    }
    if spans.is_empty() {
        return (Line::default(), Vec::new());
    }
    spans.push(Span::raw(" "));
    (Line::from(spans), switches)
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
    let valuation = app.screener.valuation;
    let heading = title(rows.len(), app.quotes.len(), valuation);
    // Only the valuation view makes a claim it has to qualify.
    let coverage_of = valuation.then(|| app.fundamentals_coverage(&rows));
    let (footer_line, switches) = footer(app, coverage_of, area.width.saturating_sub(2) as usize);
    let footer_width = footer_line.width() as u16;
    let block = widgets::panel(&heading).title_bottom(footer_line.right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);

    // The heading doubles as the price/valuation switch, like the `f` key.
    if area.width > 2 && area.height > 0 {
        let w = heading.chars().count() as u16;
        app.hits.borrow_mut().target(
            Rect {
                x: area.x + 1,
                y: area.y,
                width: w.min(area.width - 2),
                height: 1,
            },
            Target::ScreenerToggle(Toggle::Valuation),
        );
    }

    // The footer sits on the bottom border, right-aligned and ending one cell
    // short of the corner. Offsets were measured from the start of the line, so
    // they only need the line's own origin added.
    if area.height > 1 && footer_width > 0 {
        let start = area.right().saturating_sub(1 + footer_width);
        let y = area.bottom() - 1;
        let mut hits = app.hits.borrow_mut();
        for (offset, width, toggle) in switches {
            let x = start.saturating_add(offset);
            if x < area.right() {
                hits.target(
                    Rect {
                        x,
                        y,
                        width: width.min(area.right() - x),
                        height: 1,
                    },
                    Target::ScreenerToggle(toggle),
                );
            }
        }
    }
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

    let cols = choose_columns(inner.width, valuation);
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
            let val = app.fundamentals.get(&q.symbol);
            Row::new(
                cols.iter()
                    .map(|(c, w)| {
                        let style = if *c == Col::Symbol && app.watchlist.contains(&q.symbol) {
                            Style::new().fg(theme::WARN).bold()
                        } else {
                            cell_style(*c, q, val)
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

    // Read the offset back rather than assuming ours was used: the table
    // scrolls itself to keep the selection visible, so only it knows which
    // rows ended up on screen. Registering from a guessed offset would put
    // clicks a few rows out exactly when the list had scrolled.
    let offset = state.offset();
    let header_rows = u16::from(inner.height >= 2);
    let mut hits = app.hits.borrow_mut();
    hits.zone(area, Zone::Screener);

    // Sortable headers are clickable. Columns are fixed-width with one column
    // of spacing, so their positions follow directly from the widths chosen
    // above — the same list the table was built from, so the two cannot drift.
    if header_rows == 1 {
        let keys = app.sort_keys();
        let mut hx = inner.x;
        for (col, w) in &cols {
            if hx >= inner.right() {
                break;
            }
            if let Some(key) = col.sort_key()
                && let Some(i) = keys.iter().position(|k| *k == key)
            {
                hits.target(
                    Rect {
                        x: hx,
                        y: inner.y,
                        width: (*w).min(inner.right() - hx),
                        height: 1,
                    },
                    Target::SortColumn(i),
                );
            }
            hx = hx.saturating_add(*w).saturating_add(1);
        }
    }
    if inner.height > header_rows {
        hits.rows(
            Rect {
                x: inner.x,
                y: inner.y + header_rows,
                width: inner.width,
                height: inner.height - header_rows,
            },
            offset,
            rows.len().saturating_sub(offset),
            Target::ScreenerRow,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::{DataEvent, detached_channel};
    use crate::cache::Store;
    use crate::model::{Company, FinancialPeriod, RatioPeriod, SymbolInfo};

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
        let cols = choose_columns(24, false);
        let names: Vec<Col> = cols.iter().map(|(c, _)| *c).collect();
        assert!(names.contains(&Col::Symbol));
        assert!(names.contains(&Col::ChangePct));
        assert!(!names.contains(&Col::Sector));
    }

    #[test]
    fn columns_never_exceed_the_available_width() {
        for avail in 1u16..250 {
            let cols = choose_columns(avail, false);
            let total: u16 = cols.iter().map(|(_, w)| *w).sum::<u16>() + (cols.len() as u16 - 1);
            assert!(total <= avail, "avail {avail} overflowed to {total}");
        }
        assert!(choose_columns(0, false).is_empty());
    }

    #[test]
    fn slack_is_given_to_the_company_name() {
        let cols = choose_columns(200, false);
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
        let cols = choose_columns(200, false);
        assert_eq!(cols.len(), DISPLAY.len());
        let order: Vec<Col> = cols.iter().map(|(c, _)| *c).collect();
        assert_eq!(order, DISPLAY.to_vec(), "display order must be preserved");
    }

    #[test]
    fn a_single_cell_pane_still_yields_a_symbol_column() {
        assert_eq!(choose_columns(3, false), vec![(Col::Symbol, 3)]);
        assert!(choose_columns(0, false).is_empty());
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
        footer(app, None, max)
            .0
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
        assert_eq!(title(12, 500, false), "Screener  12/500");
        assert_eq!(title(12, 500, true), "Valuation  12/500");
    }

    // --- valuation view ---------------------------------------------------

    /// A company with the fundamentals the valuation view reads.
    fn company(symbol: &str) -> Company {
        Company {
            symbol: symbol.into(),
            name: format!("{symbol} Limited"),
            // 1.5 million *thousands* of PKR — 1.5 billion rupees.
            market_cap_000: Some(1_500_000.0),
            shares: Some(1_000_000.0),
            free_float: Some(400_000.0),
            pe_ratio: Some(6.25),
            financials_annual: vec![
                FinancialPeriod {
                    period: "2025".into(),
                    rows: vec![("EPS".into(), 42.60)],
                },
                FinancialPeriod {
                    period: "2024".into(),
                    rows: vec![("EPS".into(), 38.70)],
                },
            ],
            ratios: vec![RatioPeriod {
                period: "2025".into(),
                rows: vec![
                    ("Net Profit Margin (%)".into(), 9.84),
                    ("EPS Growth (%)".into(), 10.08),
                ],
            }],
            ..Default::default()
        }
    }

    fn valuation_app() -> App {
        let mut a = app_with(vec![
            quote("HBL", 292.0, 1.0, 1000.0),
            quote("OGDC", 200.0, -1.0, 500.0),
        ]);
        a.screener.valuation = true;
        a.selected = "HBL".into();
        a.on_event(DataEvent::Company(Box::new(company("HBL"))));
        a
    }

    #[test]
    fn the_valuation_view_swaps_in_the_fundamentals_columns() {
        let cols: Vec<Col> = choose_columns(200, true).iter().map(|(c, _)| *c).collect();
        assert_eq!(cols, DISPLAY_VALUATION.to_vec());
        assert!(!cols.contains(&Col::Turnover), "liquidity is not valuation");

        // And the width negotiation still holds.
        for avail in 1u16..250 {
            let cols = choose_columns(avail, true);
            let total: u16 = cols.iter().map(|(_, w)| *w).sum::<u16>() + (cols.len() as u16 - 1);
            assert!(total <= avail, "avail {avail} overflowed to {total}");
        }
    }

    #[test]
    fn valuation_cells_render_the_cached_fundamentals() {
        let app = valuation_app();
        let q = app.quotes.iter().find(|q| q.symbol == "HBL").unwrap();
        assert_eq!(cell_text(Col::Pe, q, &app, 7), "6.25");
        assert_eq!(cell_text(Col::Eps, q, &app, 8), "42.60");
        assert_eq!(cell_text(Col::EpsGrowth, q, &app, 8), "+10.08%");
        assert_eq!(cell_text(Col::NetMargin, q, &app, 8), "+9.84%");
        // 1,500,000 thousands of PKR is 1.5 billion rupees, not 1.5 million.
        assert_eq!(cell_text(Col::MarketCap, q, &app, 10), "1.50B");
        assert_eq!(cell_text(Col::FreeFloat, q, &app, 8), "40.00%");
    }

    #[test]
    fn symbols_without_a_cached_profile_show_a_dash_not_a_zero() {
        let app = valuation_app();
        let q = app.quotes.iter().find(|q| q.symbol == "OGDC").unwrap();
        for col in [
            Col::Pe,
            Col::Eps,
            Col::EpsGrowth,
            Col::NetMargin,
            Col::MarketCap,
            Col::FreeFloat,
        ] {
            assert_eq!(
                cell_text(col, q, &app, 10),
                "—",
                "{col:?} must not invent a value"
            );
        }
    }

    #[test]
    fn the_footer_admits_how_little_of_the_board_is_covered() {
        let app = valuation_app();
        let rows = app.visible_quotes();
        let (have, shown) = app.fundamentals_coverage(&rows);
        assert_eq!((have, shown), (1, 2));

        let text = footer_text_with(&app, Some((have, shown)), 80);
        assert!(
            text.contains("fundamentals: 1/2 cached"),
            "the coverage caveat must be on screen: {text:?}"
        );

        // On a pane too narrow for everything, the caveat is what survives.
        let narrow = footer_text_with(&app, Some((have, shown)), 28);
        assert!(narrow.contains("fundamentals: 1/2"), "got {narrow:?}");
        assert!(narrow.chars().count() <= 28);
    }

    fn footer_text_with(app: &App, coverage_of: Option<(usize, usize)>, max: usize) -> String {
        footer(app, coverage_of, max)
            .0
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn rows_without_fundamentals_sort_to_the_bottom_either_way() {
        let mut app = valuation_app();
        app.screener.sort = SortKey::Pe;
        for descending in [true, false] {
            app.screener.descending = descending;
            let rows = app.visible_quotes();
            assert_eq!(
                rows[0].symbol, "HBL",
                "an absent P/E must never outrank a known one (desc={descending})"
            );
        }
    }

    #[test]
    fn the_valuation_view_renders_at_tiny_sizes() {
        let app = valuation_app();
        for (w, h) in [(20u16, 10u16), (20, 2), (3, 3), (1, 1), (5, 40), (80, 1)] {
            let buf = render(&app, w, h);
            assert_eq!(buf.area.height, h);
        }
        let text = text_of(&render(&app, 120, 20));
        assert!(text.contains("MKT CAP"));
        assert!(text.contains("P/E"));
        assert!(text.contains("fundamentals: 1/2 cached"));
        assert!(
            !text.to_lowercase().contains("nan"),
            "no poisoned float may reach the screen"
        );
    }
}
