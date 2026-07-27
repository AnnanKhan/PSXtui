//! Side-by-side comparison of two to four symbols.
//!
//! The screen answers one question — *which of these did better, and at what
//! risk* — so everything on it is derived from a single aligned window.
//!
//! The only real logic here is [`align_days`]. PSX scrips do not share a
//! calendar: a thin scrip misses sessions its peers traded, a recent listing
//! starts halfway through the history, and the local cache may have been
//! backfilled to different depths per symbol. Rebasing, correlating or
//! regressing unaligned series silently compares Tuesday against Thursday, so
//! every series is first intersected on its PSX trading day
//! ([`crate::cache::trading_day`]) and only the sessions *all* of them observed
//! survive. The aligned session count is shown on screen for that reason.
//!
//! History is read from the local cache only ([`crate::cache::Store::bars`]) —
//! a render must never reach the network.

use std::collections::BTreeMap;

use ratatui::prelude::*;
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, Paragraph};

use super::{theme, widgets};
use crate::analysis::stats::{self, TRADING_DAYS_PER_YEAR};
use crate::app::{App, BENCHMARK, MAX_COMPARE, Range};
use crate::cache::trading_day;
use crate::model::Bar;

/// Risk-free rate for Sharpe, matching the Analysis screen.
const RISK_FREE: f64 = 0.11;

/// Below this many aligned sessions the overlay and the statistics are noise.
const MIN_SESSIONS: usize = 5;

/// Distinct hues for the overlay, one per compared symbol.
///
/// Four is the cap, which is about as many series as one set of axes can carry
/// before the eye stops separating them.
pub fn series_color(i: usize) -> Color {
    const PALETTE: [Color; MAX_COMPARE] = [theme::ACCENT, theme::WARN, theme::UP, theme::DOWN];
    PALETTE[i % PALETTE.len()]
}

// --- alignment (the load-bearing logic) ----------------------------------

/// Intersect a set of bar series on their PSX trading days.
///
/// Returns the days present in *every* series, oldest first, and one close
/// vector per input series indexed in lock-step with those days. An empty
/// input, or a set with no common session, yields empty vectors rather than a
/// partial (and therefore wrong) alignment.
///
/// Non-positive and non-finite closes are treated as missing: a placeholder
/// zero from a suspended scrip must not become a session everyone else is
/// rebased against.
pub fn align_days(series: &[&[Bar]]) -> (Vec<String>, Vec<Vec<f64>>) {
    if series.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let maps: Vec<BTreeMap<String, f64>> = series
        .iter()
        .map(|bars| {
            let mut m = BTreeMap::new();
            for b in bars.iter() {
                if b.close.is_finite() && b.close > 0.0 {
                    m.insert(trading_day(b.ts), b.close);
                }
            }
            m
        })
        .collect();

    // BTreeMap keys come out sorted, so `days` is chronological by construction.
    let mut days: Vec<String> = maps[0].keys().cloned().collect();
    for m in &maps[1..] {
        days.retain(|d| m.contains_key(d));
    }

    let closes = maps
        .iter()
        .map(|m| days.iter().filter_map(|d| m.get(d).copied()).collect())
        .collect();

    (days, closes)
}

/// First index of `days` inside `range`.
///
/// Year-to-date is bounded by the calendar rather than a session count, for the
/// same reason [`crate::app::App::trim_range`] treats it separately.
pub fn window_start(days: &[String], range: Range) -> usize {
    if range == Range::Ytd {
        let cut = trading_day(crate::cache::year_start_ts());
        return days.partition_point(|d| *d < cut);
    }
    match range.sessions() {
        Some(n) if days.len() > n => days.len() - n,
        _ => 0,
    }
}

/// Rebase a close series to 100 at its first observation.
///
/// This is what makes two scrips at PKR 12 and PKR 1,400 comparable on one
/// axis. A series with no usable base (all zeros, an empty window) renders as a
/// flat line at 100 rather than as `NaN`.
pub fn rebase(closes: &[f64]) -> Vec<f64> {
    let base = closes
        .iter()
        .copied()
        .find(|c| c.is_finite() && *c > 0.0)
        .unwrap_or(0.0);
    if base <= 0.0 {
        return vec![100.0; closes.len()];
    }
    closes
        .iter()
        .map(|c| {
            if c.is_finite() {
                c / base * 100.0
            } else {
                100.0
            }
        })
        .collect()
}

// --- per-symbol statistics ------------------------------------------------

/// One line of the comparison table.
struct Row {
    symbol: String,
    color: Color,
    /// False when the local cache holds no history for this symbol.
    cached: bool,
    last: f64,
    change_pct: f64,
    cagr_pct: f64,
    vol_pct: f64,
    sharpe: f64,
    mdd_pct: f64,
    beta: f64,
}

/// Everything the screen draws, computed once per frame.
struct View {
    rows: Vec<Row>,
    /// Rebased series for the symbols that had cached history, in row order.
    curves: Vec<(String, Color, Vec<f64>)>,
    corr: Vec<Vec<f64>>,
    corr_labels: Vec<String>,
    days: Vec<String>,
    sessions: usize,
}

fn build(app: &App) -> View {
    let symbols = app.compare_symbols();

    // Cache-only reads. The selected symbol's history is already in memory.
    let histories: Vec<Vec<Bar>> = symbols
        .iter()
        .map(|s| {
            if *s == app.selected && !app.bars.is_empty() {
                app.bars.clone()
            } else {
                app.store.bars(s, None).unwrap_or_default()
            }
        })
        .collect();

    let refs: Vec<&[Bar]> = histories
        .iter()
        .filter(|h| !h.is_empty())
        .map(|h| h.as_slice())
        .collect();
    let (all_days, all_closes) = align_days(&refs);
    let start = window_start(&all_days, app.compare.range);
    let days: Vec<String> = all_days.get(start..).unwrap_or_default().to_vec();

    let mut rows = Vec::new();
    let mut curves = Vec::new();
    let mut corr_series: Vec<(String, Vec<f64>)> = Vec::new();
    let mut corr_labels = Vec::new();

    let mut cached_i = 0usize;
    for (i, symbol) in symbols.iter().enumerate() {
        let color = series_color(i);
        if histories[i].is_empty() {
            rows.push(Row {
                symbol: symbol.clone(),
                color,
                cached: false,
                last: f64::NAN,
                change_pct: f64::NAN,
                cagr_pct: f64::NAN,
                vol_pct: f64::NAN,
                sharpe: f64::NAN,
                mdd_pct: f64::NAN,
                beta: f64::NAN,
            });
            continue;
        }

        let window: Vec<f64> = all_closes
            .get(cached_i)
            .and_then(|c| c.get(start..))
            .unwrap_or_default()
            .to_vec();
        cached_i += 1;

        let returns = stats::simple_returns(&window);
        let first = window.first().copied().unwrap_or(f64::NAN);
        let last = window.last().copied().unwrap_or(f64::NAN);
        let change_pct = if first.is_finite() && first > 0.0 {
            (last / first - 1.0) * 100.0
        } else {
            f64::NAN
        };

        rows.push(Row {
            symbol: symbol.clone(),
            color,
            cached: true,
            last,
            change_pct,
            cagr_pct: stats::annualized_return(&returns, TRADING_DAYS_PER_YEAR) * 100.0,
            vol_pct: stats::annualized_volatility(&returns, TRADING_DAYS_PER_YEAR) * 100.0,
            sharpe: stats::sharpe_ratio(&returns, RISK_FREE, TRADING_DAYS_PER_YEAR),
            mdd_pct: stats::max_drawdown(&window).pct * 100.0,
            beta: beta_vs_benchmark(&histories[i], &app.benchmark, app.compare.range),
        });

        curves.push((symbol.clone(), color, rebase(&window)));
        corr_series.push((symbol.clone(), returns));
        corr_labels.push(symbol.clone());
    }

    View {
        corr: stats::correlation_matrix(&corr_series),
        corr_labels,
        sessions: days.len(),
        days,
        rows,
        curves,
    }
}

/// Beta against the KSE-100, over the days the scrip and the index share.
///
/// Aligned pairwise rather than against the whole comparison set: one illiquid
/// peer must not shrink everybody else's regression window.
fn beta_vs_benchmark(bars: &[Bar], bench: &[Bar], range: Range) -> f64 {
    if bars.is_empty() || bench.is_empty() {
        return f64::NAN;
    }
    let (days, closes) = align_days(&[bars, bench]);
    if days.len() < 3 || closes.len() < 2 {
        return f64::NAN;
    }
    let start = window_start(&days, range);
    let a = stats::simple_returns(closes[0].get(start..).unwrap_or_default());
    let b = stats::simple_returns(closes[1].get(start..).unwrap_or_default());
    if a.len() < 2 || b.len() < 2 {
        return f64::NAN;
    }
    stats::beta(&a, &b)
}

// --- rendering ------------------------------------------------------------

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let view = build(app);

    let header_h = if area.height >= 8 { 4 } else { 0 };
    let [header, main] =
        Layout::vertical([Constraint::Length(header_h), Constraint::Min(0)]).areas(area);
    if header_h > 0 {
        draw_header(f, header, app, &view);
    }
    if main.height == 0 {
        return;
    }

    // The table and the matrix only earn their rows once the overlay still has
    // room left to say something.
    let table_h = (view.rows.len() as u16).saturating_add(4);
    let bottom_h = if main.height >= table_h + 8 {
        table_h
    } else {
        0
    };
    let [plot, bottom] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(bottom_h)]).areas(main);

    draw_overlay(f, plot, &view);

    if bottom_h > 0 {
        // The matrix needs a label gutter plus one narrow cell per symbol.
        let matrix_w = (view.corr_labels.len() as u16 * 6).saturating_add(11);
        let [table_area, matrix_area] = if bottom.width > matrix_w + 44 {
            Layout::horizontal([Constraint::Min(0), Constraint::Length(matrix_w)]).areas(bottom)
        } else {
            Layout::horizontal([Constraint::Min(0), Constraint::Length(0)]).areas(bottom)
        };
        draw_table(f, table_area, &view);
        if matrix_area.width > 0 {
            draw_matrix(f, matrix_area, &view);
        }
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App, view: &View) {
    let block = widgets::panel("Compare");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut names = vec![Span::styled(" Symbols ", theme::label_style())];
    for row in &view.rows {
        names.push(Span::styled(
            format!("{} ", row.symbol),
            if row.cached {
                Style::new().fg(row.color).bold()
            } else {
                Style::new().fg(theme::DIM)
            },
        ));
    }
    names.push(Span::styled(
        format!("· {} aligned sessions ", view.sessions),
        theme::label_style(),
    ));
    names.push(Span::styled(
        format!("· β vs {BENCHMARK} "),
        theme::label_style(),
    ));
    if view.rows.iter().any(|r| !r.cached) {
        names.push(Span::styled(
            "· missing history ",
            Style::new().fg(theme::WARN),
        ));
    }

    let mut ranges = vec![Span::styled(" Range ", theme::label_style())];
    for r in Range::ALL {
        ranges.push(Span::styled(
            format!("{} ", r.label()),
            if r == app.compare.range {
                Style::new().fg(theme::ACCENT).bold()
            } else {
                Style::new().fg(theme::DIM)
            },
        ));
    }
    ranges.push(Span::styled(
        "  [ ] range · a add/remove selected · c reset",
        theme::label_style(),
    ));

    f.render_widget(
        Paragraph::new(Text::from(vec![Line::from(names), Line::from(ranges)])),
        inner,
    );
}

fn draw_overlay(f: &mut Frame, area: Rect, view: &View) {
    let block = widgets::panel("Rebased to 100");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if view.sessions < MIN_SESSIONS || view.curves.len() < 2 {
        let msg = if view.curves.len() < 2 {
            "Need two symbols with cached history — press a to add the selected one".to_string()
        } else {
            format!(
                "Only {} sessions are shared by all {} symbols — widen the range with ]",
                view.sessions,
                view.curves.len()
            )
        };
        let msg = theme::truncate(&msg, inner.width as usize);
        let pad = (inner.height.saturating_sub(1) / 2) as usize;
        let mut lines: Vec<Line> = vec![Line::raw(""); pad];
        lines.push(widgets::placeholder(&msg));
        f.render_widget(Paragraph::new(Text::from(lines)), inner);
        return;
    }

    let legend_h = u16::from(inner.height >= 4);
    let [plot, legend] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(legend_h)]).areas(inner);

    let (lo, hi) = bounds(view);
    let n = view.sessions.max(2) as f64;
    let curves: Vec<(Color, Vec<f64>)> = view
        .curves
        .iter()
        .map(|(_, c, v)| (*c, v.clone()))
        .collect();

    let canvas = Canvas::default()
        .block(Block::default())
        .marker(symbols::Marker::Braille)
        .x_bounds([0.0, n - 1.0])
        .y_bounds([lo, hi])
        .paint(move |ctx| {
            // The 100 line is the whole point of rebasing: above it is profit.
            ctx.draw(&CanvasLine {
                x1: 0.0,
                y1: 100.0,
                x2: n - 1.0,
                y2: 100.0,
                color: theme::BORDER,
            });
            for (color, values) in &curves {
                for (i, w) in values.windows(2).enumerate() {
                    ctx.draw(&CanvasLine {
                        x1: i as f64,
                        y1: w[0],
                        x2: i as f64 + 1.0,
                        y2: w[1],
                        color: *color,
                    });
                }
            }
        });
    f.render_widget(canvas, plot);

    if legend_h > 0 {
        let mut spans = Vec::new();
        for (symbol, color, values) in &view.curves {
            let last = values.last().copied().unwrap_or(100.0);
            spans.push(Span::styled(" ── ", Style::new().fg(*color)));
            spans.push(Span::styled(symbol.clone(), Style::new().fg(*color).bold()));
            spans.push(Span::styled(
                format!(" {} ", theme::pct(last - 100.0)),
                Style::new().fg(theme::change_color(last - 100.0)),
            ));
        }
        if let (Some(first), Some(last)) = (view.days.first(), view.days.last()) {
            spans.push(Span::styled(
                format!("│ {first} → {last}"),
                theme::label_style(),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), legend);
    }
}

/// Y-bounds covering every drawn curve, padded so nothing hugs the frame.
fn bounds(view: &View) -> (f64, f64) {
    let mut lo = 100.0f64;
    let mut hi = 100.0f64;
    for (_, _, values) in &view.curves {
        for v in values {
            if v.is_finite() {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
        }
    }
    if !lo.is_finite() || !hi.is_finite() || hi - lo < 1e-9 {
        // A perfectly flat comparison still needs an axis to draw on.
        return (lo - 1.0, hi + 1.0);
    }
    let pad = (hi - lo) * 0.06;
    (lo - pad, hi + pad)
}

/// Table columns, widest-first in importance: they are dropped from the right
/// as the pane narrows, so the symbol and its return always survive.
const COLUMNS: [(&str, usize); 8] = [
    ("Symbol", 9),
    ("Last", 11),
    ("Chg%", 9),
    ("CAGR", 9),
    ("Vol", 9),
    ("Sharpe", 8),
    ("MaxDD", 9),
    ("β", 7),
];

fn draw_table(f: &mut Frame, area: Rect, view: &View) {
    let block = widgets::panel("Performance & Risk");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let width = inner.width as usize;
    let mut header = Vec::new();
    let mut used = 0usize;
    let mut shown = 0usize;
    for (label, w) in COLUMNS {
        if used + w > width {
            break;
        }
        used += w;
        shown += 1;
        header.push(Span::styled(
            if shown == 1 {
                format!(" {label:<w$}", w = w - 1)
            } else {
                format!("{label:>w$}")
            },
            theme::header_style(),
        ));
    }
    if shown == 0 {
        return;
    }

    let mut lines = vec![Line::from(header)];
    for row in &view.rows {
        let mut spans = vec![Span::styled(
            format!(
                " {:<w$}",
                theme::truncate(&row.symbol, COLUMNS[0].1 - 1),
                w = COLUMNS[0].1 - 1
            ),
            Style::new().fg(row.color).bold(),
        )];

        if !row.cached {
            let rest: usize = COLUMNS[1..shown].iter().map(|c| c.1).sum();
            if rest > 0 {
                spans.push(Span::styled(
                    format!("{:>rest$}", theme::truncate("no cached history", rest)),
                    Style::new().fg(theme::WARN),
                ));
            }
            lines.push(Line::from(spans));
            continue;
        }

        let cells: [(String, Color); 7] = [
            (theme::price(row.last), theme::FG),
            (
                theme::pct(row.change_pct),
                theme::change_color(row.change_pct),
            ),
            (theme::pct(row.cagr_pct), theme::change_color(row.cagr_pct)),
            (theme::pct_plain(row.vol_pct), theme::MUTED),
            (
                theme::opt(finite(row.sharpe), 2),
                theme::change_color(row.sharpe),
            ),
            (theme::pct(row.mdd_pct), theme::DOWN),
            (theme::opt(finite(row.beta), 2), theme::FG),
        ];
        for (i, (text, color)) in cells.iter().enumerate() {
            let col = i + 1;
            if col >= shown {
                break;
            }
            let w = COLUMNS[col].1;
            spans.push(Span::styled(
                format!("{:>w$}", theme::truncate(text, w)),
                Style::new().fg(*color),
            ));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// `None` for anything that must never reach the screen as a number.
fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

fn draw_matrix(f: &mut Frame, area: Rect, view: &View) {
    let block = widgets::panel("Correlation");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    const LABEL_W: usize = 7;
    const CELL_W: usize = 6;
    let n = view.corr_labels.len();
    if n == 0 {
        f.render_widget(Paragraph::new(widgets::placeholder("—")), inner);
        return;
    }

    let mut header = vec![Span::styled(
        format!("{:<LABEL_W$}", ""),
        theme::header_style(),
    )];
    for label in &view.corr_labels {
        header.push(Span::styled(
            format!("{:>CELL_W$}", theme::truncate(label, CELL_W)),
            theme::header_style(),
        ));
    }

    let mut lines = vec![Line::from(header)];
    for (i, label) in view.corr_labels.iter().enumerate() {
        let mut spans = vec![Span::styled(
            format!("{:<LABEL_W$}", theme::truncate(label, LABEL_W)),
            Style::new().fg(series_color(i)),
        )];
        for j in 0..n {
            let c = view
                .corr
                .get(i)
                .and_then(|r| r.get(j))
                .copied()
                .unwrap_or(0.0);
            // heat_color speaks in percent, so a ±1.0 correlation is mapped
            // onto the full ±10 saturation range it expects.
            spans.push(Span::styled(
                format!("{:>CELL_W$}", theme::opt(finite(c), 2)),
                Style::new()
                    .bg(widgets::heat_color(c * 10.0, 10.0))
                    .fg(theme::FG),
            ));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::app::detached_channel;
    use crate::cache::{Store, day_close_ts};

    fn bar(day: &str, close: f64) -> Bar {
        Bar {
            ts: day_close_ts(day),
            open: close,
            high: close,
            low: close,
            close,
            volume: 100.0,
        }
    }

    // -- alignment ---------------------------------------------------------

    #[test]
    fn alignment_keeps_only_days_every_series_traded() {
        // A thin scrip that missed the 3rd, against one that missed the 2nd.
        let a = vec![
            bar("2025-01-01", 10.0),
            bar("2025-01-02", 11.0),
            bar("2025-01-06", 12.0),
        ];
        let b = vec![
            bar("2025-01-01", 100.0),
            bar("2025-01-03", 101.0),
            bar("2025-01-06", 102.0),
        ];
        let c = vec![
            bar("2025-01-01", 5.0),
            bar("2025-01-02", 5.5),
            bar("2025-01-03", 5.6),
            bar("2025-01-06", 6.0),
        ];

        let (days, closes) = align_days(&[&a, &b, &c]);
        assert_eq!(days, vec!["2025-01-01", "2025-01-06"]);
        assert_eq!(closes[0], vec![10.0, 12.0]);
        assert_eq!(closes[1], vec![100.0, 102.0]);
        assert_eq!(closes[2], vec![5.0, 6.0]);
    }

    #[test]
    fn alignment_handles_a_recent_listing_against_a_long_history() {
        let long: Vec<Bar> = (1..=28)
            .map(|d| bar(&format!("2025-02-{d:02}"), 100.0 + d as f64))
            .collect();
        let new: Vec<Bar> = (20..=28)
            .map(|d| bar(&format!("2025-02-{d:02}"), 5.0 + d as f64))
            .collect();

        let (days, closes) = align_days(&[&long, &new]);
        assert_eq!(days.len(), 9);
        assert_eq!(closes[0].len(), 9);
        assert_eq!(closes[1].len(), 9);
        assert_eq!(closes[0][0], 120.0);
        assert_eq!(closes[1][0], 25.0);
    }

    #[test]
    fn alignment_is_empty_when_calendars_do_not_overlap() {
        let a = vec![bar("2024-01-02", 10.0)];
        let b = vec![bar("2025-01-02", 10.0)];
        let (days, closes) = align_days(&[&a, &b]);
        assert!(days.is_empty());
        assert!(closes.iter().all(|c| c.is_empty()));
    }

    #[test]
    fn alignment_guards_empty_and_degenerate_input() {
        assert_eq!(align_days(&[]), (Vec::new(), Vec::new()));

        let (days, closes) = align_days(&[&[], &[bar("2025-01-01", 1.0)][..]]);
        assert!(days.is_empty());
        assert_eq!(closes.len(), 2);

        // A single series is trivially aligned with itself.
        let a = vec![bar("2025-01-01", 1.0), bar("2025-01-02", 2.0)];
        let (days, closes) = align_days(&[&a]);
        assert_eq!(days.len(), 2);
        assert_eq!(closes[0], vec![1.0, 2.0]);
    }

    #[test]
    fn alignment_ignores_non_positive_closes() {
        let a = vec![bar("2025-01-01", 0.0), bar("2025-01-02", 10.0)];
        let b = vec![bar("2025-01-01", 5.0), bar("2025-01-02", 6.0)];
        let (days, _) = align_days(&[&a, &b]);
        assert_eq!(days, vec!["2025-01-02"], "a zero print is not a session");
    }

    // -- rebasing ----------------------------------------------------------

    #[test]
    fn rebasing_starts_at_100_and_tracks_relative_performance() {
        let r = rebase(&[50.0, 55.0, 45.0]);
        assert_eq!(r[0], 100.0);
        assert!((r[1] - 110.0).abs() < 1e-9);
        assert!((r[2] - 90.0).abs() < 1e-9);

        // A 1,400-rupee scrip and a 12-rupee one land on the same scale.
        let cheap = rebase(&[12.0, 13.2]);
        let dear = rebase(&[1400.0, 1540.0]);
        assert!((cheap[1] - dear[1]).abs() < 1e-9);
    }

    #[test]
    fn rebasing_survives_degenerate_series() {
        assert!(rebase(&[]).is_empty());
        assert_eq!(rebase(&[0.0, 0.0]), vec![100.0, 100.0]);
        assert_eq!(rebase(&[7.0]), vec![100.0]);
        assert!(rebase(&[10.0, f64::NAN]).iter().all(|v| v.is_finite()));
    }

    // -- windowing ---------------------------------------------------------

    #[test]
    fn window_start_trims_to_the_range() {
        let days: Vec<String> = (1..=28).map(|d| format!("2025-02-{d:02}")).collect();
        assert_eq!(window_start(&days, Range::Max), 0);
        assert_eq!(window_start(&days, Range::D5), 23);
        // A window longer than the history keeps everything.
        assert_eq!(window_start(&days, Range::Y5), 0);
        assert_eq!(window_start(&[], Range::D5), 0);
    }

    // -- view --------------------------------------------------------------

    fn series(days: usize, base: f64, drift: f64) -> Vec<Bar> {
        (0..days)
            .map(|i| Bar {
                ts: day_close_ts("2025-01-01") + i as i64 * 86_400,
                open: base,
                high: base,
                low: base,
                close: base * (1.0 + drift * i as f64) + (i as f64 / 9.0).sin() * base * 0.05,
                volume: 1_000.0,
            })
            .collect()
    }

    fn app_with(entries: &[(&str, Vec<Bar>)]) -> App {
        let store = Arc::new(Store::open_in_memory().unwrap());
        for (sym, bars) in entries {
            store.put_eod_bars(sym, bars).unwrap();
        }
        let (tx, _rx) = detached_channel();
        let mut app = App::new(store, tx);
        if let Some((sym, bars)) = entries.first() {
            app.selected = (*sym).to_string();
            app.bars = bars.clone();
        }
        for (sym, _) in entries {
            app.watchlist.insert((*sym).to_string());
        }
        app.benchmark = series(300, 50_000.0, 0.0005);
        app
    }

    fn render(app: &App, w: u16, h: u16) {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, f.area(), app))
            .expect("compare screen must render");
    }

    #[test]
    fn renders_without_panicking_at_every_size() {
        let full = app_with(&[
            ("HBL", series(300, 100.0, 0.001)),
            ("OGDC", series(300, 220.0, -0.0005)),
            ("PPL", series(180, 90.0, 0.0002)),
        ]);
        let mut missing = app_with(&[("HBL", series(300, 100.0, 0.001))]);
        missing.compare.symbols = vec!["HBL".into(), "GHOST".into()];

        let single = app_with(&[("HBL", series(1, 100.0, 0.0))]);
        let flat = app_with(&[
            ("AAA", series(80, 10.0, 0.0)),
            ("BBB", series(80, 10.0, 0.0)),
        ]);
        let empty = {
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (tx, _rx) = detached_channel();
            App::new(store, tx)
        };

        for app in [&full, &missing, &single, &flat, &empty] {
            for (w, h) in [(20u16, 10u16), (1, 1), (40, 12), (80, 24), (200, 60)] {
                render(app, w, h);
            }
        }
    }

    #[test]
    fn a_symbol_with_no_cached_history_is_reported_not_dropped() {
        let mut app = app_with(&[("HBL", series(120, 100.0, 0.001))]);
        app.compare.symbols = vec!["HBL".into(), "GHOST".into()];

        let view = build(&app);
        assert_eq!(view.rows.len(), 2);
        assert!(!view.rows[1].cached);
        assert_eq!(view.curves.len(), 1, "only cached series are plotted");
    }

    #[test]
    fn comparison_is_computed_over_the_intersected_calendar() {
        // OGDC skips every third session; the alignment must shrink to match.
        let hbl = series(120, 100.0, 0.001);
        let ogdc: Vec<Bar> = series(120, 50.0, 0.0005)
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !i.is_multiple_of(3))
            .map(|(_, b)| b)
            .collect();

        let mut app = app_with(&[("HBL", hbl), ("OGDC", ogdc.clone())]);
        app.compare.symbols = vec!["HBL".into(), "OGDC".into()];
        app.compare.range = Range::Max;

        let view = build(&app);
        assert_eq!(view.sessions, ogdc.len());
        for (_, _, curve) in &view.curves {
            assert_eq!(curve.len(), view.sessions);
            assert!(curve.iter().all(|v| v.is_finite()));
            assert_eq!(curve[0], 100.0);
        }
        assert_eq!(view.corr.len(), 2);
        assert!(view.corr[0][1].is_finite());
    }
}
