//! The Backtest screen.
//!
//! Three columns: the strategies on disk, the parameters of the selected one,
//! and the results. The results pane cycles through the equity curve, the
//! trade list, a parameter sweep, walk-forward folds and a universe scan.
//!
//! The screen is opinionated about honesty. Buy-and-hold is drawn on the same
//! axes as the equity curve because a strategy that trails it has cost money
//! to run, and [`crate::backtest::Report::caveats`] is rendered directly under
//! the headline numbers rather than tucked away — a thin sample or an
//! implausible Sharpe should be as visible as the return that provoked it.

use ratatui::prelude::*;
use ratatui::symbols::Marker;
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::{theme, widgets};
use crate::app::App;
use crate::app::backtest_state::{Focus, View};
use crate::backtest::{Report, TradeExit};
use crate::cache::trading_day;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let [left, right] =
        Layout::horizontal([Constraint::Length(34), Constraint::Min(0)]).areas(area);
    let [list_area, params_area] =
        Layout::vertical([Constraint::Percentage(45), Constraint::Min(0)]).areas(left);

    draw_strategies(f, list_area, app);
    draw_params(f, params_area, app);
    draw_results(f, right, app);
}

// --- strategy list --------------------------------------------------------

fn draw_strategies(f: &mut Frame, area: Rect, app: &App) {
    let bt = &app.backtest;
    let block = widgets::panel("Strategies");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if bt.strategies.is_empty() {
        let msg = if bt.load_errors.is_empty() {
            "No strategies found.\n\nPress i to install the bundled ones."
        } else {
            "No strategies loaded — see the errors below."
        };
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(theme::MUTED))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let rows = inner.height as usize;
    let offset = bt.offset.min(bt.selected);
    let mut lines = Vec::new();

    for (i, s) in bt.strategies.iter().enumerate().skip(offset).take(rows) {
        let selected = i == bt.selected;
        let marker = if selected { "▸ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(theme::FG)
                .bg(theme::SELECT_BG)
                .add_modifier(Modifier::BOLD)
        } else if bt.focus == Focus::Strategies {
            Style::default().fg(theme::FG)
        } else {
            Style::default().fg(theme::MUTED)
        };
        lines.push(Line::from(vec![Span::styled(
            format!(
                "{marker}{}",
                theme::truncate(&s.name, (inner.width as usize).saturating_sub(2))
            ),
            style,
        )]));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

// --- parameters -----------------------------------------------------------

fn draw_params(f: &mut Frame, area: Rect, app: &App) {
    let bt = &app.backtest;
    let title = match bt.strategy() {
        Some(s) if bt.focus == Focus::Params => format!("Parameters — {}", s.name),
        _ => "Parameters".to_string(),
    };
    let block = widgets::panel(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Files that failed to parse are shown whether or not anything else
    // loaded. An import that silently does nothing is the worst outcome here:
    // the user edits a file, presses R, and has no idea why their strategy
    // never appeared.
    let mut errors: Vec<Line> = Vec::new();
    for e in &bt.load_errors {
        for (i, part) in wrap_text(e, inner.width as usize).into_iter().enumerate() {
            errors.push(Line::styled(
                if i == 0 {
                    format!("✗ {part}")
                } else {
                    format!("  {part}")
                },
                Style::default().fg(theme::DOWN),
            ));
        }
    }

    let Some(s) = bt.strategy() else {
        f.render_widget(Paragraph::new(errors).wrap(Wrap { trim: true }), inner);
        return;
    };

    let mut lines = Vec::new();

    // Above the description, not below it: the panel overflows on a short
    // terminal, and a rejected import is more urgent than prose about the
    // strategy that did load.
    if !errors.is_empty() {
        lines.extend(errors);
        lines.push(Line::raw(""));
    }

    if !s.about.is_empty() {
        for line in wrap_text(&s.about, inner.width as usize) {
            lines.push(Line::styled(line, Style::default().fg(theme::MUTED)));
        }
        lines.push(Line::raw(""));
    }

    if s.params.is_empty() {
        lines.push(Line::styled(
            "No tunable parameters.",
            Style::default().fg(theme::DIM),
        ));
    }

    for (i, (name, def)) in s.params.iter().enumerate() {
        let value = bt.params.get(name).copied().unwrap_or(def.default);
        let focused = bt.focus == Focus::Params && i == bt.param_cursor;
        let changed = (value - def.default).abs() > f64::EPSILON;

        let value_style = if changed {
            // A tweaked value must be obvious — it is the difference between
            // the published strategy and the user's variant of it.
            Style::default()
                .fg(theme::WARN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FG)
        };

        let mut spans = vec![
            Span::styled(
                if focused { "▸ " } else { "  " },
                Style::default().fg(theme::ACCENT),
            ),
            Span::styled(
                format!("{name:<12}"),
                Style::default().fg(if focused { theme::FG } else { theme::MUTED }),
            ),
            Span::styled(format!("{value:>8}", value = fmt_param(value)), value_style),
        ];
        if changed {
            spans.push(Span::styled(
                format!(" (was {})", fmt_param(def.default)),
                Style::default().fg(theme::DIM),
            ));
        }
        let mut line = Line::from(spans);
        if focused {
            line = line.style(Style::default().bg(theme::SELECT_BG));
        }
        lines.push(line);
    }

    // Costs are part of the result and easy to forget, so they are shown with
    // the parameters rather than buried in a config file.
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!(
            "costs {:.0}bp + {:.0}bp slippage",
            bt.config.costs.commission_bps, bt.config.costs.slippage_bps
        ),
        Style::default().fg(theme::DIM),
    ));
    lines.push(Line::styled(
        format!("rank by {}", bt.objective.label()),
        Style::default().fg(theme::DIM),
    ));

    if !s.source.is_empty() {
        lines.push(Line::raw(""));
        for line in wrap_text(&format!("Source: {}", s.source), inner.width as usize) {
            lines.push(Line::styled(line, Style::default().fg(theme::DIM)));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn fmt_param(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{:.0}", v)
    } else {
        format!("{v:.2}")
    }
}

// --- results --------------------------------------------------------------

fn draw_results(f: &mut Frame, area: Rect, app: &App) {
    let bt = &app.backtest;

    let tabs: Vec<String> = View::ALL
        .iter()
        .map(|v| {
            if *v == bt.view {
                format!("[{}]", v.title())
            } else {
                format!(" {} ", v.title())
            }
        })
        .collect();

    // The symbol the result actually came from, so a run left on screen after
    // the selection moves elsewhere is never mislabelled.
    let symbol = if bt.report_symbol.is_empty() {
        app.selected.as_str()
    } else {
        bt.report_symbol.as_str()
    };
    let title = format!("{symbol} — {}", tabs.join(" "));
    let block = widgets::panel(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(e) = &bt.error {
        f.render_widget(
            Paragraph::new(e.as_str())
                .style(Style::default().fg(theme::DOWN))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    match bt.view {
        View::Equity => draw_equity(f, inner, app),
        View::Trades => draw_trades(f, inner, app),
        View::Sweep => draw_sweep(f, inner, app),
        View::WalkForward => draw_walk_forward(f, inner, app),
        View::Scan => draw_scan(f, inner, app),
    }
}

fn hint(f: &mut Frame, area: Rect, msg: &str) {
    f.render_widget(
        Paragraph::new(msg)
            .style(Style::default().fg(theme::MUTED))
            .wrap(Wrap { trim: true }),
        area,
    );
}

// --- equity ---------------------------------------------------------------

fn draw_equity(f: &mut Frame, area: Rect, app: &App) {
    let bt = &app.backtest;
    let Some(r) = &bt.report else {
        hint(
            f,
            area,
            "Press Enter to run this strategy on the selected symbol.",
        );
        return;
    };

    let caveats = r.caveats();
    let caveat_height = if caveats.is_empty() {
        0
    } else {
        (caveats.len() as u16 + 1).min(6)
    };

    let [stats_area, chart_area, caveat_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(5),
        Constraint::Length(caveat_height),
    ])
    .areas(area);

    draw_stats(f, stats_area, r);
    draw_equity_curve(f, chart_area, app, r);

    if !caveats.is_empty() {
        let mut lines = Vec::new();
        for c in &caveats {
            // saturating: the "! " prefix costs two columns, and a terminal
            // can be narrower than that.
            for (i, part) in wrap_text(c, (caveat_area.width as usize).saturating_sub(2))
                .into_iter()
                .enumerate()
            {
                lines.push(Line::from(vec![
                    Span::styled(
                        if i == 0 { "! " } else { "  " },
                        Style::default().fg(theme::WARN),
                    ),
                    Span::styled(part, Style::default().fg(theme::WARN)),
                ]));
            }
        }
        f.render_widget(Paragraph::new(lines), caveat_area);
    }
}

fn draw_stats(f: &mut Frame, area: Rect, r: &Report) {
    // Four columns of paired label/value, which fits the panel at any width
    // the rest of the app supports.
    let col = |label: &'static str, value: String, color: Color| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{label:<10}"), Style::default().fg(theme::MUTED)),
            Span::styled(format!("{value:<10}"), Style::default().fg(color)),
        ]
    };

    let mut line1 = Vec::new();
    line1.extend(col(
        "return",
        theme::pct(r.total_return_pct),
        theme::change_color(r.total_return_pct),
    ));
    line1.extend(col(
        "buy+hold",
        theme::pct(r.buy_hold_return_pct),
        theme::change_color(r.buy_hold_return_pct),
    ));
    line1.extend(col(
        "CAGR",
        theme::pct(r.cagr_pct),
        theme::change_color(r.cagr_pct),
    ));
    line1.extend(col(
        "max DD",
        theme::pct_plain(r.max_drawdown_pct),
        theme::DOWN,
    ));

    let mut line2 = Vec::new();
    line2.extend(col("Sharpe", format!("{:.2}", r.sharpe), theme::FG));
    line2.extend(col("Sortino", format!("{:.2}", r.sortino), theme::FG));
    line2.extend(col("trades", r.trade_count.to_string(), theme::FG));
    line2.extend(col(
        "win rate",
        format!("{:.0}%", r.win_rate_pct),
        theme::FG,
    ));

    let mut line3 = Vec::new();
    line3.extend(col(
        "profit f.",
        if r.profit_factor > 0.0 {
            format!("{:.2}", r.profit_factor)
        } else {
            "—".into()
        },
        theme::FG,
    ));
    line3.extend(col(
        "exposure",
        format!("{:.0}%", r.exposure_pct),
        theme::FG,
    ));
    line3.extend(col(
        "avg hold",
        format!("{:.0}d", r.avg_bars_held),
        theme::FG,
    ));
    line3.extend(col("bars", r.bar_count.to_string(), theme::FG));

    f.render_widget(
        Paragraph::new(vec![
            Line::from(line1),
            Line::from(line2),
            Line::from(line3),
        ]),
        area,
    );
}

fn draw_equity_curve(f: &mut Frame, area: Rect, app: &App, r: &Report) {
    if r.equity.len() < 2 {
        hint(f, area, "Not enough bars to draw an equity curve.");
        return;
    }

    // Buy-and-hold rebased to the same starting capital, so the two curves are
    // directly comparable on one axis.
    let hold: Vec<f64> = match app.bars.first() {
        Some(first) if first.close > 0.0 => app
            .bars
            .iter()
            .map(|b| r.initial_equity * b.close / first.close)
            .collect(),
        _ => Vec::new(),
    };

    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for v in r.equity.iter().chain(hold.iter()) {
        if v.is_finite() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        hint(f, area, "The equity curve is not finite — this is a bug.");
        return;
    }
    if (hi - lo).abs() < f64::EPSILON {
        lo -= 1.0;
        hi += 1.0;
    }
    let pad = (hi - lo) * 0.05;
    let (lo, hi) = (lo - pad, hi + pad);

    let [chart_area, legend_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);

    let equity = r.equity.clone();
    let hold_curve = hold.clone();
    let initial = r.initial_equity;
    let n = equity.len();

    let canvas = Canvas::default()
        .block(Block::default())
        .marker(theme::marker(Marker::Braille))
        .x_bounds([0.0, n as f64])
        .y_bounds([lo, hi])
        .paint(move |ctx| {
            // The starting capital, so drawdown below the line is obvious.
            if initial >= lo && initial <= hi {
                ctx.draw(&CanvasLine {
                    x1: 0.0,
                    y1: initial,
                    x2: n as f64,
                    y2: initial,
                    color: theme::BORDER,
                });
            }

            // Buy-and-hold underneath: it is the benchmark, not the subject.
            for w in hold_curve.windows(2).enumerate() {
                let (i, pair) = w;
                ctx.draw(&CanvasLine {
                    x1: i as f64,
                    y1: pair[0],
                    x2: (i + 1) as f64,
                    y2: pair[1],
                    color: theme::DIM,
                });
            }

            for (i, pair) in equity.windows(2).enumerate() {
                let rising = pair[1] >= pair[0];
                ctx.draw(&CanvasLine {
                    x1: i as f64,
                    y1: pair[0],
                    x2: (i + 1) as f64,
                    y2: pair[1],
                    color: if rising { theme::UP } else { theme::DOWN },
                });
            }
        });

    f.render_widget(canvas, chart_area);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("── strategy  ", Style::default().fg(theme::UP)),
            Span::styled("── buy & hold  ", Style::default().fg(theme::DIM)),
            Span::styled(
                format!(
                    "{} → {}",
                    theme::compact(r.initial_equity),
                    theme::compact(r.final_equity)
                ),
                Style::default().fg(theme::MUTED),
            ),
        ])),
        legend_area,
    );
}

// --- trades ---------------------------------------------------------------

fn draw_trades(f: &mut Frame, area: Rect, app: &App) {
    let bt = &app.backtest;
    let Some(r) = &bt.report else {
        hint(f, area, "Press Enter to run this strategy first.");
        return;
    };
    if r.trades.is_empty() {
        hint(
            f,
            area,
            "This strategy never opened a position on this symbol.",
        );
        return;
    }

    let mut lines = vec![Line::styled(
        format!(
            "{:<12} {:<12} {:>9} {:>9} {:>8} {:>6} {:>7}",
            "entry", "exit", "in", "out", "return", "days", "why"
        ),
        theme::header_style(),
    )];

    let rows = area.height.saturating_sub(1) as usize;
    for t in r.trades.iter().skip(bt.trades_offset).take(rows) {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<12} ", trading_day(t.entry_ts)),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(
                format!("{:<12} ", trading_day(t.exit_ts)),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(
                format!("{:>9} ", theme::price(t.entry_price)),
                Style::default().fg(theme::FG),
            ),
            Span::styled(
                format!("{:>9} ", theme::price(t.exit_price)),
                Style::default().fg(theme::FG),
            ),
            Span::styled(
                format!("{:>8} ", theme::pct(t.return_pct)),
                Style::default().fg(theme::change_color(t.return_pct)),
            ),
            Span::styled(
                format!("{:>6} ", t.bars_held),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(
                format!("{:>7}", t.exit_reason.label()),
                Style::default().fg(match t.exit_reason {
                    TradeExit::StopLoss => theme::DOWN,
                    TradeExit::TakeProfit => theme::UP,
                    _ => theme::DIM,
                }),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines), area);
}

// --- sweep ----------------------------------------------------------------

fn draw_sweep(f: &mut Frame, area: Rect, app: &App) {
    let bt = &app.backtest;
    if bt.sweep.is_empty() {
        hint(
            f,
            area,
            "Press s to sweep every parameter combination.\n\nRanked by the current objective (o cycles it). Remember that the best row here is the most curve-fitted one — check it on the Walk-forward tab before believing it.",
        );
        return;
    }

    let names: Vec<String> = bt
        .strategy()
        .map(|s| s.params.keys().cloned().collect())
        .unwrap_or_default();

    let mut header = format!("{:>8} ", "score");
    for n in &names {
        header.push_str(&format!("{:>9} ", theme::truncate(n, 9)));
    }
    header.push_str(&format!(
        "{:>9} {:>7} {:>8} {:>7}",
        "return", "Sharpe", "maxDD", "trades"
    ));

    let mut lines = vec![Line::styled(header, theme::header_style())];

    let rows = area.height.saturating_sub(1) as usize;
    for (i, p) in bt.sweep.iter().enumerate().skip(bt.sweep_offset).take(rows) {
        let mut spans = vec![Span::styled(
            format!("{:>8} ", format!("{:.3}", p.score)),
            Style::default().fg(if i == 0 { theme::ACCENT } else { theme::MUTED }),
        )];
        for n in &names {
            spans.push(Span::styled(
                format!("{:>9} ", fmt_param(p.params.get(n).copied().unwrap_or(0.0))),
                Style::default().fg(theme::FG),
            ));
        }
        spans.push(Span::styled(
            format!("{:>9} ", theme::pct(p.total_return_pct)),
            Style::default().fg(theme::change_color(p.total_return_pct)),
        ));
        spans.push(Span::styled(
            format!("{:>7} ", format!("{:.2}", p.sharpe)),
            Style::default().fg(theme::FG),
        ));
        spans.push(Span::styled(
            format!("{:>8} ", theme::pct_plain(p.max_drawdown_pct)),
            Style::default().fg(theme::DOWN),
        ));
        spans.push(Span::styled(
            format!("{:>7}", p.trade_count),
            Style::default().fg(theme::MUTED),
        ));
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), area);
}

// --- walk-forward ---------------------------------------------------------

fn draw_walk_forward(f: &mut Frame, area: Rect, app: &App) {
    let bt = &app.backtest;
    let Some(wf) = &bt.walk_forward else {
        hint(
            f,
            area,
            "Press w to walk this strategy forward.\n\nParameters are optimised on each training window, then scored on the untouched window that follows. The out-of-sample number is the only one here that was never fitted — it is the closest thing to an honest answer this screen can give.",
        );
        return;
    };

    let [summary_area, folds_area] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(area);

    let efficiency_color = if wf.efficiency >= 0.6 {
        theme::UP
    } else if wf.efficiency >= 0.3 {
        theme::WARN
    } else {
        theme::DOWN
    };

    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("in-sample  ", Style::default().fg(theme::MUTED)),
                Span::styled(
                    format!("{:<12}", theme::pct(wf.in_sample_return_pct)),
                    Style::default().fg(theme::DIM),
                ),
                Span::styled("out-of-sample  ", Style::default().fg(theme::MUTED)),
                Span::styled(
                    format!("{:<12}", theme::pct(wf.out_of_sample_return_pct)),
                    Style::default().fg(theme::change_color(wf.out_of_sample_return_pct)),
                ),
                Span::styled("efficiency  ", Style::default().fg(theme::MUTED)),
                Span::styled(
                    format!("{:.2}", wf.efficiency),
                    Style::default()
                        .fg(efficiency_color)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::styled(wf.verdict(), Style::default().fg(efficiency_color)),
            Line::raw(""),
        ]),
        summary_area,
    );

    let names: Vec<String> = bt
        .strategy()
        .map(|s| s.params.keys().cloned().collect())
        .unwrap_or_default();

    let mut header = format!("{:<24} ", "test window");
    for n in &names {
        header.push_str(&format!("{:>9} ", theme::truncate(n, 9)));
    }
    header.push_str(&format!(
        "{:>10} {:>10} {:>7}",
        "in-sample", "out", "trades"
    ));

    let mut lines = vec![Line::styled(header, theme::header_style())];

    for fold in &wf.folds {
        let mut spans = vec![Span::styled(
            format!(
                "{:<24} ",
                format!(
                    "{} → {}",
                    trading_day(fold.test_start_ts),
                    trading_day(fold.test_end_ts)
                )
            ),
            Style::default().fg(theme::MUTED),
        )];
        for n in &names {
            spans.push(Span::styled(
                format!(
                    "{:>9} ",
                    fmt_param(fold.params.get(n).copied().unwrap_or(0.0))
                ),
                Style::default().fg(theme::FG),
            ));
        }
        spans.push(Span::styled(
            format!("{:>10} ", theme::pct(fold.in_sample_return_pct)),
            Style::default().fg(theme::DIM),
        ));
        spans.push(Span::styled(
            format!("{:>10} ", theme::pct(fold.out_of_sample_return_pct)),
            Style::default().fg(theme::change_color(fold.out_of_sample_return_pct)),
        ));
        spans.push(Span::styled(
            format!("{:>7}", fold.trade_count),
            Style::default().fg(theme::MUTED),
        ));
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), folds_area);
}

// --- universe scan --------------------------------------------------------

fn draw_scan(f: &mut Frame, area: Rect, app: &App) {
    let bt = &app.backtest;
    if bt.scan.is_empty() {
        hint(
            f,
            area,
            "Press u to run this strategy across the market.\n\nOne symbol proves nothing — an edge that only works on the scrip you happened to be looking at is a coincidence. Note that delisted scrips are absent from PSX's symbol list, so these results are biased upward by survivorship.",
        );
        return;
    }

    let [summary_area, rows_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);

    let s = &bt.scan_summary;
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("symbols ", Style::default().fg(theme::MUTED)),
                Span::styled(format!("{:<8}", s.symbols), Style::default().fg(theme::FG)),
                Span::styled("median ", Style::default().fg(theme::MUTED)),
                Span::styled(
                    format!("{:<10}", theme::pct(s.median_return_pct)),
                    Style::default().fg(theme::change_color(s.median_return_pct)),
                ),
                Span::styled("profitable ", Style::default().fg(theme::MUTED)),
                Span::styled(
                    format!("{}/{}   ", s.profitable, s.symbols),
                    Style::default().fg(theme::FG),
                ),
                Span::styled("beat buy+hold ", Style::default().fg(theme::MUTED)),
                Span::styled(
                    format!("{}/{}", s.beat_buy_hold, s.symbols),
                    Style::default().fg(theme::FG),
                ),
            ]),
            Line::styled(
                "Survivorship: delisted scrips are not in PSX's symbol list, so this is biased upward.",
                Style::default().fg(theme::WARN),
            ),
        ]),
        summary_area,
    );

    let mut lines = vec![Line::styled(
        format!(
            "{:<12} {:>10} {:>10} {:>8} {:>9} {:>7} {:>6}",
            "symbol", "return", "buy+hold", "Sharpe", "maxDD", "trades", "win%"
        ),
        theme::header_style(),
    )];

    let rows = rows_area.height.saturating_sub(1) as usize;
    for row in bt.scan.iter().skip(bt.scan_offset).take(rows) {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<12} ", theme::truncate(&row.symbol, 12)),
                Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:>10} ", theme::pct(row.total_return_pct)),
                Style::default().fg(theme::change_color(row.total_return_pct)),
            ),
            Span::styled(
                format!("{:>10} ", theme::pct(row.buy_hold_return_pct)),
                Style::default().fg(theme::DIM),
            ),
            Span::styled(
                format!("{:>8} ", format!("{:.2}", row.sharpe)),
                Style::default().fg(theme::FG),
            ),
            Span::styled(
                format!("{:>9} ", theme::pct_plain(row.max_drawdown_pct)),
                Style::default().fg(theme::DOWN),
            ),
            Span::styled(
                format!("{:>7} ", row.trade_count),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(
                format!("{:>6.0}", row.win_rate_pct),
                Style::default().fg(theme::MUTED),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines), rows_area);
}

// --- helpers --------------------------------------------------------------

/// Word-wrap to a width, for the descriptive text the strategy files carry.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width < 8 {
        return vec![];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if line.is_empty() {
            line = word.to_string();
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_breaks_on_word_boundaries() {
        let out = wrap_text("the quick brown fox jumps", 11);
        assert!(out.iter().all(|l| l.chars().count() <= 11), "{out:?}");
        assert_eq!(out.join(" "), "the quick brown fox jumps");
    }

    #[test]
    fn wrapping_a_too_narrow_panel_yields_nothing_rather_than_panicking() {
        assert!(wrap_text("hello world", 3).is_empty());
    }

    #[test]
    fn params_render_whole_numbers_without_decimals() {
        assert_eq!(fmt_param(50.0), "50");
        assert_eq!(fmt_param(2.5), "2.50");
    }

    // -- rendering ---------------------------------------------------------

    fn render(app: &App, w: u16, h: u16) {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, f.area(), app))
            .expect("backtest screen must render");
    }

    fn bars(n: usize) -> Vec<crate::model::Bar> {
        (0..n)
            .map(|i| {
                let t = i as f64;
                let c = 100.0 + t * 0.05 + (t / 8.0).sin() * 6.0 + (t / 29.0).cos() * 10.0;
                crate::model::Bar {
                    ts: 1_600_000_000 + i as i64 * 86_400,
                    open: c,
                    high: c * 1.01,
                    low: c * 0.99,
                    close: c,
                    volume: 50_000.0,
                }
            })
            .collect()
    }

    fn app_with_bars(n: usize) -> App {
        use std::sync::Arc;
        let store = Arc::new(crate::cache::Store::open_in_memory().unwrap());
        let (tx, _rx) = crate::app::detached_channel();
        let mut app = App::new(store, tx);
        app.selected = "TEST".into();
        app.bars = bars(n);
        app.backtest.strategies = crate::backtest::BUILTIN
            .iter()
            .map(|(_, b)| crate::backtest::Strategy::parse(b).unwrap())
            .collect();
        app.backtest.reset_params();
        app
    }

    /// Every view, populated, at sizes from absurd to generous.
    ///
    /// The populated path is the one that indexes into equity curves, trade
    /// lists and fold tables, so an empty-state render proves very little
    /// about it.
    #[test]
    fn renders_every_view_without_panicking_at_every_size() {
        let config = crate::backtest::Config::default();

        let mut full = app_with_bars(600);
        let strategy = full.backtest.strategy().unwrap().clone();
        let params = full.backtest.params.clone();
        full.backtest.report =
            Some(crate::backtest::engine::run(&strategy, &full.bars, &params, &config).unwrap());
        full.backtest.report_symbol = "TEST".into();
        full.backtest.sweep =
            crate::backtest::optimize::sweep(&strategy, &full.bars, &config, Default::default())
                .unwrap();
        full.backtest.walk_forward = Some(
            crate::backtest::optimize::walk_forward(
                &strategy,
                &full.bars,
                &config,
                Default::default(),
                3,
            )
            .unwrap(),
        );
        full.backtest.scan = crate::backtest::optimize::scan(
            &strategy,
            &params,
            &["AAA".to_string(), "BBB".to_string()],
            &config,
            |_| Some(bars(600)),
        );
        full.backtest.scan_summary = crate::backtest::optimize::summarize(&full.backtest.scan);

        // Nothing run yet — every view shows its explanatory hint instead.
        let fresh = app_with_bars(600);

        // No strategies at all, plus a file that failed to parse.
        let mut broken = app_with_bars(600);
        broken.backtest.strategies.clear();
        broken.backtest.load_errors =
            vec!["bad.toml: unknown function `smaa` — available: sma, ema, rsi".to_string()];

        // A run over a series too short to draw a curve from.
        let mut tiny = app_with_bars(3);
        tiny.backtest.report =
            Some(crate::backtest::engine::run(&strategy, &tiny.bars, &params, &config).unwrap());

        // An error from a failed run.
        let mut errored = app_with_bars(600);
        errored.backtest.error = Some("period longer than the available history".into());

        for app in [&full, &fresh, &broken, &tiny, &errored] {
            for view in View::ALL {
                let mut a = app_with_bars(0);
                // Cheap clone of just what the renderer reads.
                a.selected = app.selected.clone();
                a.bars = app.bars.clone();
                a.backtest.strategies = app.backtest.strategies.clone();
                a.backtest.load_errors = app.backtest.load_errors.clone();
                a.backtest.params = app.backtest.params.clone();
                a.backtest.report = app.backtest.report.clone();
                a.backtest.report_symbol = app.backtest.report_symbol.clone();
                a.backtest.sweep = app.backtest.sweep.clone();
                a.backtest.walk_forward = app.backtest.walk_forward.clone();
                a.backtest.scan = app.backtest.scan.clone();
                a.backtest.scan_summary = app.backtest.scan_summary;
                a.backtest.error = app.backtest.error.clone();
                a.backtest.view = view;

                for (w, h) in [(1u16, 1u16), (20, 10), (40, 12), (80, 24), (200, 60)] {
                    render(&a, w, h);
                }

                // And with focus on the parameter panel, which changes styling
                // and adds the "(was …)" suffix.
                a.backtest.focus = Focus::Params;
                a.backtest.nudge_param(3.0);
                render(&a, 80, 24);
            }
        }
    }
}
