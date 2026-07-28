//! Rendering. Every function here is a pure read of [`App`] — no mutation, no
//! I/O — so a frame can be drawn at any time without side effects.

pub mod analysis;
pub mod chart;
pub mod company;
pub mod compare;
pub mod dashboard;
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
    let right = Paragraph::new(Line::from(vec![Span::styled(
        "? help  q quit ",
        theme::label_style(),
    )]))
    .alignment(Alignment::Right);

    f.render_widget(left, area);
    f.render_widget(right, area);
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
            Span::styled(format!("    {k:<14}"), Style::new().fg(theme::FG)),
            Span::styled(d.to_string(), theme::label_style()),
        ])
    };

    let text = Text::from(vec![
        Line::raw(""),
        section("Global"),
        bind("1 – 6", "jump to screen"),
        bind("Tab / S-Tab", "cycle screens"),
        bind("/", "search symbol, company or sector"),
        bind("r", "refresh market data"),
        bind("w", "toggle watchlist for current symbol"),
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
