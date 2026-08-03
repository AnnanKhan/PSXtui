//! Rendering. Every function here is a pure read of [`App`] — no mutation, no
//! I/O — so a frame can be drawn at any time without side effects.

pub mod analysis;
pub mod chart;
pub mod company;
pub mod compare;
pub mod dashboard;
pub mod hit;
pub mod intraday;
pub mod macro_;
pub mod screener;
pub mod seasonality;
pub mod theme;
pub mod widgets;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph, Tabs, Wrap};

use crate::app::{App, Screen};

pub fn draw(f: &mut Frame, app: &App) {
    let [ticker, tabs, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());

    // Everything the mouse can act on is registered fresh each frame, because
    // only the renderer knows where things ended up.
    app.hits.borrow_mut().clear();

    draw_ticker(f, ticker, app);
    draw_tabs(f, tabs, app);

    match app.screen {
        Screen::Dashboard => dashboard::draw(f, body, app),
        Screen::Screener => screener::draw(f, body, app),
        Screen::Chart => chart::draw(f, body, app),
        Screen::Analysis => analysis::draw(f, body, app),
        Screen::Company => company::draw(f, body, app),
        Screen::Intraday => intraday::draw(f, body, app),
        Screen::Compare => compare::draw(f, body, app),
        Screen::Seasonality => seasonality::draw(f, body, app),
        Screen::Macro => macro_::draw(f, body, app),
    }

    draw_status(f, status, app);

    // Nothing useful can be rendered until the board arrives, so explain the
    // wait rather than showing an empty dashboard.
    if app.quotes.is_empty() && !app.show_help {
        draw_startup(f, app);
    }

    if app.show_help {
        draw_help(f);
    }
}

/// A single-line index ticker across the top.
fn draw_ticker(f: &mut Frame, area: Rect, app: &App) {
    if app.indices.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(" Loading indices… ", theme::label_style())),
            area,
        );
        return;
    }

    let mut spans = Vec::new();
    for idx in &app.indices {
        let color = theme::change_color(idx.change);
        spans.push(Span::styled(
            format!(" {} ", idx.name),
            Style::new().fg(theme::MUTED).bold(),
        ));
        spans.push(Span::styled(
            theme::index_level(idx.value),
            Style::new().fg(theme::FG),
        ));
        spans.push(Span::styled(
            format!(" {} ", theme::pct(idx.change_pct)),
            Style::new().fg(color),
        ));
        spans.push(Span::styled("│", theme::border_style()));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let labels: Vec<String> = Screen::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{} {}", i + 1, s.title()))
        .collect();

    let titles: Vec<Line> = Screen::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| {
            Line::from(vec![
                Span::styled(format!("{} ", i + 1), Style::new().fg(theme::DIM)),
                Span::raw(s.title()),
            ])
        })
        .collect();

    let tabs = Tabs::new(titles)
        .select(app.screen.index())
        .style(Style::new().fg(theme::MUTED))
        .highlight_style(Style::new().fg(theme::ACCENT).bold())
        .divider(Span::styled("│", theme::border_style()));

    f.render_widget(tabs, area);

    // Mirror the widget's own geometry so a click lands on the tab under the
    // pointer: Tabs pads each title with one space either side and separates
    // them with a single-column divider.
    let mut x = area.x;
    for (i, label) in labels.iter().enumerate() {
        let w = label.chars().count() as u16 + 2;
        if x >= area.right() {
            break;
        }
        let width = w.min(area.right() - x);
        app.hits.borrow_mut().target(
            Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            hit::Target::Tab(i),
        );
        // Advance past this tab and its divider.
        x = x.saturating_add(w).saturating_add(1);
    }
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();

    // Search takes over the status line while it is open.
    if let Some(query) = &app.search {
        spans.push(Span::styled(" / ", Style::new().fg(theme::ACCENT).bold()));
        spans.push(Span::styled(query.clone(), Style::new().fg(theme::FG)));
        spans.push(Span::styled("▏", Style::new().fg(theme::ACCENT)));
        spans.push(Span::styled(
            "   Enter to open · Esc to cancel",
            theme::label_style(),
        ));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    if !app.selected.is_empty() {
        spans.push(Span::styled(
            format!(" {} ", app.selected),
            Style::new().fg(theme::ACCENT).bold(),
        ));
        if app.watchlist.contains(&app.selected) {
            spans.push(Span::styled("★ ", Style::new().fg(theme::WARN)));
        }
        if let Some(q) = app.selected_quote() {
            spans.push(Span::styled(theme::price(q.current), theme::value_style()));
            spans.push(Span::styled(
                format!(" {} ", theme::pct(q.change_pct)),
                Style::new().fg(theme::change_color(q.change_pct)),
            ));
        }
        spans.push(Span::styled("│ ", theme::border_style()));
    }

    // Errors outrank status; they persist until Esc.
    if let Some(err) = &app.error {
        spans.push(Span::styled(
            format!("⚠ {} ", theme::truncate(err, 60)),
            Style::new().fg(theme::DOWN),
        ));
        spans.push(Span::styled("(Esc) ", theme::label_style()));
    } else {
        spans.push(Span::styled(app.status.clone(), theme::label_style()));
    }

    // Name the work in flight rather than showing an anonymous busy dot: when
    // a load is slow, "what is it waiting on" is the only useful information.
    if !app.activities.is_empty() {
        spans.push(Span::styled(
            format!("  {} ", app.spinner_glyph()),
            Style::new().fg(theme::WARN),
        ));
        spans.push(Span::styled(
            app.activities.join(" · "),
            Style::new().fg(theme::WARN),
        ));
    }

    if let Some(b) = &app.backfill {
        spans.push(Span::styled(" │ ", theme::border_style()));
        spans.push(Span::styled("backfill ", theme::label_style()));
        spans.push(Span::styled(
            widgets::bar(b.ratio(), 12),
            Style::new().fg(theme::ACCENT),
        ));
        spans.push(Span::styled(
            format!(" {}/{} {}", b.done, b.total, b.day),
            theme::label_style(),
        ));
        if let Some(rows) = b.rows {
            spans.push(Span::styled(
                format!(" ({rows} symbols)"),
                Style::new().fg(theme::DIM),
            ));
        }
    }

    let left = Paragraph::new(Line::from(spans));
    const HINT: &str = "? help  q quit ";
    let right = Paragraph::new(Line::from(vec![Span::styled(HINT, theme::label_style())]))
        .alignment(Alignment::Right);

    f.render_widget(left, area);
    f.render_widget(right, area);

    // The hint reads as two buttons, so make it behave as two. Right-aligned,
    // so positions are measured back from the right edge.
    let hint_w = HINT.len() as u16;
    if area.width > hint_w {
        let start = area.right() - hint_w;
        let mut hits = app.hits.borrow_mut();
        hits.target(
            Rect {
                x: start,
                y: area.y,
                width: 7, // "? help "
                height: 1,
            },
            hit::Target::Help,
        );
        hits.target(
            Rect {
                x: start + 8,
                y: area.y,
                width: 7, // "q quit "
                height: 1,
            },
            hit::Target::Quit,
        );
    }
}

/// First-run overlay.
///
/// With an empty cache there is genuinely nothing to render for a minute or
/// so, and a blank dashboard reads as a hang. This spells out each step, what
/// it is for, and what has already completed.
fn draw_startup(f: &mut Frame, app: &App) {
    let area = widgets::centered_rect(58, 46, f.area());
    f.render_widget(Clear, area);

    let block = Block::bordered()
        .border_style(Style::new().fg(theme::ACCENT))
        .title(Span::styled(" Connecting to PSX ", theme::title_style()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let step = |done: bool, active: bool, label: &str, detail: &str| {
        let (mark, style) = if done {
            ("✓", Style::new().fg(theme::UP))
        } else if active {
            ("→", Style::new().fg(theme::WARN))
        } else {
            ("·", Style::new().fg(theme::DIM))
        };
        Line::from(vec![
            Span::styled(format!("  {mark} "), style),
            Span::styled(
                format!("{label:<18}"),
                if done || active {
                    theme::value_style()
                } else {
                    theme::label_style()
                },
            ),
            Span::styled(detail.to_string(), Style::new().fg(theme::DIM)),
        ])
    };

    let waiting_on = |needle: &str| app.activities.iter().any(|a| a.contains(needle));

    let mut lines = vec![
        Line::raw(""),
        step(
            !app.symbols.is_empty(),
            waiting_on("symbol list"),
            "Symbol list",
            "every listed instrument",
        ),
        step(
            !app.quotes.is_empty(),
            waiting_on("market board"),
            "Market board",
            "live prices for the session",
        ),
        step(
            !app.indices.is_empty(),
            waiting_on("indices"),
            "Indices",
            "KSE100 and sector indices",
        ),
        step(
            !app.benchmark.is_empty(),
            waiting_on("KSE100"),
            "Benchmark history",
            "for beta and correlation",
        ),
        Line::raw(""),
    ];

    if let Some(b) = &app.backfill {
        lines.push(Line::from(vec![
            Span::styled("  → ", Style::new().fg(theme::WARN)),
            Span::styled(format!("{:<18}", "OHLC backfill"), theme::value_style()),
            Span::styled(
                format!("{}/{} sessions", b.done, b.total),
                Style::new().fg(theme::DIM),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("      "),
            Span::styled(widgets::bar(b.ratio(), 28), Style::new().fg(theme::ACCENT)),
            Span::styled(format!(" {}", b.day), Style::new().fg(theme::DIM)),
        ]));
        lines.push(Line::from(Span::styled(
            "      true daily high/low — one request per session",
            Style::new().fg(theme::DIM),
        )));
        lines.push(Line::raw(""));
    }

    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} ", app.spinner_glyph()),
            Style::new().fg(theme::WARN),
        ),
        Span::styled(app.status.clone(), theme::label_style()),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  Requests are paced to stay gentle on PSX. This runs",
        Style::new().fg(theme::DIM),
    )));
    lines.push(Line::from(Span::styled(
        "  once — later launches open straight from the cache.",
        Style::new().fg(theme::DIM),
    )));

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_help(f: &mut Frame) {
    let area = widgets::centered_rect(66, 80, f.area());
    f.render_widget(Clear, area);

    let section = |t: &str| {
        Line::from(Span::styled(
            format!("  {t}"),
            Style::new().fg(theme::ACCENT).bold(),
        ))
    };
    let bind = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!("    {k:<16}"), Style::new().fg(theme::FG)),
            Span::styled(d.to_string(), theme::label_style()),
        ])
    };

    let text = Text::from(vec![
        Line::raw(""),
        section("Mouse"),
        bind("click", "tab to switch screen · row to select"),
        bind("double-click", "open a row in the chart"),
        bind("wheel", "scroll the list under the pointer"),
        bind("wheel on chart", "change timeframe"),
        bind("panel title", "screener: price / valuation view"),
        bind(
            "footer switch",
            "screener: sort order · watchlist · equities",
        ),
        Line::raw(""),
        section("Global"),
        bind("1 – 6", "jump to screen"),
        bind("Tab / S-Tab", "cycle screens"),
        bind("/", "search symbol, company or sector"),
        bind("r", "refresh market data"),
        bind("w", "toggle watchlist for current symbol"),
        bind("M", "mouse on/off (off restores text selection)"),
        bind("?", "this help"),
        bind("q / Ctrl-C", "quit"),
        Line::raw(""),
        section("Dashboard"),
        bind("j / k, ↑ ↓", "move within the focused board"),
        bind("h / l, ← →", "switch board (gainers/losers/active)"),
        bind("s", "move focus to / from the sector heatmap"),
        bind("g / G", "first / last row"),
        bind("Enter", "open in chart — on a sector, filter the screener"),
        bind("Esc", "clear a sector filter"),
        Line::raw(""),
        section("Screener"),
        bind("j / k, ↑ ↓", "move cursor"),
        bind("g / G", "first / last row"),
        bind("PgUp/PgDn", "page"),
        bind("Enter", "open in chart"),
        bind("s / S", "cycle sort column / reverse"),
        bind("W", "watchlist only"),
        bind("e", "equities only (hide debt & ETFs)"),
        Line::raw(""),
        bind("f", "fundamentals view (P/E, EPS, margin)"),
        Line::raw(""),
        section("Chart"),
        bind("[ / ]", "range: 5D 1M 3M 6M YTD 1Y 2Y 3Y 5Y MAX"),
        bind("i", "cycle pane: volume RSI MACD ATR stoch ADX CCI %R"),
        bind("c", "style: candles → line → dots → area"),
        bind("m / e / b", "toggle SMA / EMA / Bollinger"),
        bind("d / k / v", "toggle Donchian / Ichimoku / S-R levels"),
        Line::raw(""),
        section("Company"),
        bind("h / l, ← →", "switch tab"),
        bind("j / k", "scroll announcements"),
        Line::raw(""),
        section("Compare"),
        bind("a", "add / remove the selected symbol"),
        bind("c", "reset the comparison set"),
        bind("[ / ]", "change range"),
        bind("click", "a symbol to select it, again to remove it"),
        Line::raw(""),
        section("Macro"),
        bind("s", "move focus between series and headlines"),
        bind("j / k, g / G", "scroll the focused panel"),
        Line::raw(""),
        Line::from(Span::styled(
            "    Data: Pakistan Stock Exchange (dps.psx.com.pk)",
            Style::new().fg(theme::DIM),
        )),
    ]);

    let block = Block::bordered()
        .border_style(Style::new().fg(theme::ACCENT))
        .title(Span::styled(" Keys ", theme::title_style()));

    f.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DataEvent, Screen, detached_channel};
    use crate::cache::Store;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;

    fn app() -> App {
        let (tx, _rx) = detached_channel();
        App::new(Arc::new(Store::open_in_memory().unwrap()), tx)
    }

    fn render(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        term.backend().buffer().clone()
    }

    /// The tab row as plain text.
    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn every_tab_is_clickable_where_it_is_drawn() {
        // The hit rects mirror the Tabs widget's own padding and dividers by
        // hand, so this checks them against what was actually rendered rather
        // than against the arithmetic that produced them.
        let app = app();
        let buf = render(&app, 160, 24);
        let text = row_text(&buf, 1);

        for (i, screen) in Screen::ALL.iter().enumerate() {
            let title = screen.title();
            // `find` gives a byte offset, and the divider glyph is three
            // bytes, so byte offsets drift from screen columns. Convert.
            let byte = text
                .find(title)
                .unwrap_or_else(|| panic!("{title} was not drawn: {text:?}"));
            let col = text[..byte].chars().count() as u16;
            // Probe the middle of the label, away from padding and dividers.
            let probe = col + (title.len() as u16) / 2;
            assert_eq!(
                app.hits.borrow().target_at(probe, 1),
                Some(hit::Target::Tab(i)),
                "clicking {title} at column {probe} should select tab {i}"
            );
        }
    }

    #[test]
    fn the_hit_map_is_rebuilt_each_frame() {
        let app = app();
        render(&app, 160, 24);
        let first = app.hits.borrow().target_at(3, 1);
        assert!(first.is_some());

        // Rendering again must not accumulate duplicates or stale entries.
        render(&app, 160, 24);
        assert_eq!(app.hits.borrow().target_at(3, 1), first);

        // A narrower frame drops the tabs that no longer fit.
        render(&app, 20, 10);
        assert_eq!(app.hits.borrow().target_at(150, 1), None);
    }

    #[test]
    fn screener_rows_are_clickable_where_they_are_drawn() {
        let mut app = app();
        app.on_event(DataEvent::Quotes(
            (0..30)
                .map(|i| crate::model::Quote {
                    symbol: format!("S{i:02}"),
                    sector: "BANKS".into(),
                    indices: vec![],
                    ldcp: 10.0,
                    open: 10.0,
                    high: 10.0,
                    low: 10.0,
                    current: 10.0 + i as f64,
                    change: 0.0,
                    change_pct: i as f64,
                    volume: 100.0,
                })
                .collect(),
        ));
        app.screener.equities_only = false;
        app.screen = Screen::Screener;

        let buf = render(&app, 160, 30);
        // Find a row by its symbol and confirm the hit map agrees.
        let target = app.visible_quotes()[0].symbol.clone();
        let y = (0..buf.area.height)
            .find(|y| row_text(&buf, *y).contains(&target))
            .expect("first row should be drawn");

        assert_eq!(
            app.hits.borrow().target_at(4, y),
            Some(hit::Target::ScreenerRow(0)),
            "row 0 is drawn at y={y} but is not clickable there"
        );
    }

    #[test]
    fn rendering_every_screen_registers_hits_without_panicking() {
        let mut app = app();
        for screen in Screen::ALL {
            app.screen = screen;
            for (w, h) in [(20u16, 10u16), (80, 24), (200, 60)] {
                render(&app, w, h);
            }
        }
    }

    #[test]
    fn sort_headers_are_clickable_where_they_are_drawn() {
        let mut app = app();
        app.on_event(DataEvent::Quotes(
            (0..5)
                .map(|i| crate::model::Quote {
                    symbol: format!("S{i:02}"),
                    sector: "BANKS".into(),
                    indices: vec![],
                    ldcp: 10.0,
                    open: 10.0,
                    high: 10.0,
                    low: 10.0,
                    current: 10.0,
                    change: 0.0,
                    change_pct: i as f64,
                    volume: 100.0,
                })
                .collect(),
        ));
        app.screener.equities_only = false;
        app.screen = Screen::Screener;

        let buf = render(&app, 160, 20);
        let header_y = (0..buf.area.height)
            .find(|y| row_text(&buf, *y).contains("SYMBOL"))
            .expect("header should be drawn");

        // The VOLUME header must map to the Volume sort key.
        let text = row_text(&buf, header_y);
        let byte = text.find("VOLUME").expect("VOLUME column");
        let col = text[..byte].chars().count() as u16;

        let want = app
            .sort_keys()
            .iter()
            .position(|k| *k == crate::app::SortKey::Volume)
            .unwrap();
        assert_eq!(
            app.hits.borrow().target_at(col + 2, header_y),
            Some(hit::Target::SortColumn(want)),
            "VOLUME is drawn at column {col} but is not clickable there"
        );
    }

    #[test]
    fn chart_overlay_labels_are_clickable_where_they_are_drawn() {
        let mut app = app();
        app.screen = Screen::Chart;
        app.selected = "HBL".into();
        app.bars = (0..60)
            .map(|i| crate::model::Bar {
                ts: i as i64 * 86_400,
                open: 10.0,
                high: 11.0,
                low: 9.0,
                close: 10.0 + i as f64,
                volume: 100.0,
            })
            .collect();

        let buf = render(&app, 160, 30);
        let y = (0..buf.area.height)
            .find(|y| row_text(&buf, *y).contains("Overlays"))
            .expect("overlay row should be drawn");

        let text = row_text(&buf, y);
        for (i, label) in ["SMA20", "EMA50", "BB20", "DC20", "ICHI", "S/R"]
            .iter()
            .enumerate()
        {
            let byte = text
                .find(label)
                .unwrap_or_else(|| panic!("{label} missing"));
            let col = text[..byte].chars().count() as u16;
            assert_eq!(
                app.hits.borrow().target_at(col + 1, y),
                Some(hit::Target::ChartOverlay(i)),
                "{label} drawn at column {col} is not clickable there"
            );
        }
    }

    /// A board of `n` synthetic quotes, enough for the screens to draw.
    fn quotes(n: usize) -> Vec<crate::model::Quote> {
        (0..n)
            .map(|i| crate::model::Quote {
                symbol: format!("S{i:02}"),
                sector: "BANKS".into(),
                indices: vec![],
                ldcp: 10.0,
                open: 10.0,
                high: 10.0,
                low: 10.0,
                current: 10.0,
                change: 0.0,
                change_pct: i as f64,
                volume: 100.0,
            })
            .collect()
    }

    #[test]
    fn screener_switches_are_clickable_where_they_are_drawn() {
        let mut app = app();
        app.on_event(DataEvent::Quotes(quotes(5)));
        app.screener.equities_only = false;
        app.screener.watchlist_only = true;
        app.screen = Screen::Screener;

        let buf = render(&app, 160, 20);
        // The footer hangs off the bottom border of the screener panel, which
        // is the last row the panel occupies — one above the status bar.
        let y = buf.area.height - 2;
        let text = row_text(&buf, y);

        for (needle, want) in [
            ("watchlist", hit::Toggle::Watchlist),
            ("equities", hit::Toggle::Equities),
        ] {
            let byte = text
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} missing from footer: {text:?}"));
            let col = text[..byte].chars().count() as u16;
            assert_eq!(
                app.hits.borrow().target_at(col + 1, y),
                Some(hit::Target::ScreenerToggle(want)),
                "{needle} drawn at column {col} is not clickable there"
            );
        }

        // The sort readout reverses the order, as clicking the active column
        // header does.
        let byte = text.find("sort ").expect("sort readout");
        let col = text[..byte].chars().count() as u16 + "sort ".len() as u16;
        assert_eq!(
            app.hits.borrow().target_at(col, y),
            Some(hit::Target::ScreenerToggle(hit::Toggle::SortDirection)),
        );

        // And the heading switches to the valuation view.
        let top = (0..buf.area.height)
            .find(|y| row_text(&buf, *y).contains("Screener  "))
            .expect("heading should be drawn");
        assert_eq!(
            app.hits.borrow().target_at(3, top),
            Some(hit::Target::ScreenerToggle(hit::Toggle::Valuation)),
        );
    }

    #[test]
    fn compare_ranges_and_symbols_are_clickable_where_they_are_drawn() {
        let mut app = app();
        app.on_event(DataEvent::Quotes(quotes(4)));
        app.screener.equities_only = false;
        app.selected = "S00".into();
        app.screen = Screen::Compare;

        let buf = render(&app, 160, 40);

        let sym_y = (0..buf.area.height)
            .find(|y| row_text(&buf, *y).contains(" Symbols "))
            .expect("symbol row should be drawn");
        let text = row_text(&buf, sym_y);
        for (i, symbol) in app.compare_symbols().iter().enumerate() {
            let byte = text
                .find(symbol.as_str())
                .unwrap_or_else(|| panic!("{symbol} missing: {text:?}"));
            let col = text[..byte].chars().count() as u16;
            assert_eq!(
                app.hits.borrow().target_at(col, sym_y),
                Some(hit::Target::CompareSymbol(i)),
                "{symbol} drawn at column {col} is not clickable there"
            );
        }

        let range_y = (0..buf.area.height)
            .find(|y| row_text(&buf, *y).contains(" Range "))
            .expect("range row should be drawn");
        let text = row_text(&buf, range_y);
        for (i, r) in crate::app::Range::ALL.iter().enumerate() {
            let byte = text
                .find(r.label())
                .unwrap_or_else(|| panic!("{} missing: {text:?}", r.label()));
            let col = text[..byte].chars().count() as u16;
            assert_eq!(
                app.hits.borrow().target_at(col, range_y),
                Some(hit::Target::CompareRange(i)),
                "range {} drawn at column {col} is not clickable there",
                r.label()
            );
        }

        // The wheel over the plot changes the range, so the plot is a zone.
        let plot_y = (0..buf.area.height)
            .find(|y| row_text(&buf, *y).contains("Rebased to 100"))
            .expect("plot should be drawn");
        assert_eq!(
            app.hits.borrow().zone_at(4, plot_y + 2),
            Some(hit::Zone::Compare),
        );
    }

    #[test]
    fn help_and_quit_are_clickable_where_they_are_drawn() {
        let app = app();
        let buf = render(&app, 160, 24);
        let y = buf.area.height - 1;
        let text = row_text(&buf, y);

        for (needle, want) in [("? help", hit::Target::Help), ("q quit", hit::Target::Quit)] {
            let byte = text
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} missing"));
            let col = text[..byte].chars().count() as u16;
            assert_eq!(
                app.hits.borrow().target_at(col + 1, y),
                Some(want),
                "{needle} drawn at column {col} is not clickable there"
            );
        }
    }
}
