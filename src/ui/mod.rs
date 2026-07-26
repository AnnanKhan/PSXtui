//! Rendering. Every function here is a pure read of [`App`] — no mutation, no
//! I/O — so a frame can be drawn at any time without side effects.

pub mod analysis;
pub mod chart;
pub mod company;
pub mod dashboard;
pub mod intraday;
pub mod screener;
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
    }

    draw_status(f, status, app);

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

    if app.inflight > 0 {
        spans.push(Span::styled(" ●", Style::new().fg(theme::WARN)));
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
        section("Lists (Dashboard, Screener)"),
        bind("j / k, ↑ ↓", "move cursor"),
        bind("g / G", "first / last row"),
        bind("PgUp/PgDn", "page"),
        bind("Enter", "open in chart"),
        bind("s / S", "cycle sort column / reverse"),
        bind("W", "watchlist only"),
        bind("e", "equities only (hide debt & ETFs)"),
        Line::raw(""),
        section("Chart"),
        bind("[ / ]", "shrink / extend range"),
        bind("i", "cycle lower indicator pane"),
        bind("c", "candles or line"),
        bind("m / e / b", "toggle SMA / EMA / Bollinger"),
        Line::raw(""),
        section("Company"),
        bind("h / l, ← →", "switch tab"),
        bind("j / k", "scroll announcements"),
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
