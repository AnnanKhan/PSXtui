//! Risk & return screen.
//!
//! Everything here is derived from `App::bars` (the selected scrip's daily
//! history) and `App::benchmark` (the KSE-100). The one piece of real logic is
//! [`align_closes`]: the two series routinely differ in length *and* in date
//! coverage — a scrip that was suspended for a week, a cache backfilled at a
//! different time — so beta and correlation are computed only over sessions both
//! series actually observed.

use std::collections::HashMap;

use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

use super::{theme, widgets};
use crate::analysis::stats::{self, TRADING_DAYS_PER_YEAR};
use crate::app::{App, BENCHMARK};
use crate::cache::trading_day;
use crate::model::Bar;

// The risk-free rate for Sharpe and Sortino is the live SBP policy rate,
// scraped by `crate::ext::macros` and read through `App::risk_free`.
//
// Pakistan's policy rate has run in double digits for years, so annualising
// against zero would flatter every scrip on the exchange — and a hardcoded
// constant silently ages into the same error. It is labelled on screen, with
// its source, so the ratios stay interpretable.

/// Below this many sessions the statistics are noise, not information.
const MIN_BARS: usize = 30;

/// The trailing windows shown in the returns panel. `None` means "everything".
const WINDOWS: [(&str, Option<usize>); 5] = [
    ("1M", Some(22)),
    ("3M", Some(65)),
    ("6M", Some(125)),
    ("1Y", Some(250)),
    ("ALL", None),
];

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if app.bars.len() < MIN_BARS {
        let msg = if app.selected.is_empty() {
            "No symbol selected — choose one on the Screener".to_string()
        } else if app.bars.is_empty() {
            format!("Loading daily history for {}…", app.selected)
        } else {
            format!(
                "Not enough history for {} — {} sessions cached, {MIN_BARS} needed",
                app.selected,
                app.bars.len()
            )
        };
        notice(f, area, "Risk & Return", &msg);
        return;
    }

    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
    let [returns_area, risk_area, bench_area] = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .areas(top);
    let [dd_area, rel_area] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(bottom);

    draw_returns(f, returns_area, app);
    draw_risk(f, risk_area, app);
    draw_benchmark(f, bench_area, app);
    draw_drawdown(f, dd_area, app);
    draw_relative(f, rel_area, app);
}

// --- panels --------------------------------------------------------------

fn draw_returns(f: &mut Frame, area: Rect, app: &App) {
    let block = widgets::panel("Return");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let closes = closes_of(&app.bars);
    let mut lines: Vec<Line> = Vec::new();
    for (label, sessions) in WINDOWS {
        match window_return(&closes, sessions) {
            Some(v) => lines.push(widgets::stat_signed(label, v, theme::pct(v), 6)),
            None => lines.push(widgets::stat(label, "—", 6)),
        }
    }

    lines.push(Line::raw(""));
    let spark_width = inner.width.saturating_sub(2) as usize;
    if spark_width > 0 {
        lines.push(Line::from(vec![
            Span::raw(" "),
            widgets::sparkline_span(&closes, spark_width),
        ]));
    }
    lines.push(Line::from(Span::styled(
        format!(" close · {} sessions", app.bars.len()),
        theme::label_style(),
    )));

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_risk(f: &mut Frame, area: Rect, app: &App) {
    let block = widgets::panel("Risk-Adjusted");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let closes = closes_of(&app.bars);
    let returns = stats::simple_returns(&closes);
    let ann_ret = stats::annualized_return(&returns, TRADING_DAYS_PER_YEAR) * 100.0;
    let ann_vol = stats::annualized_volatility(&returns, TRADING_DAYS_PER_YEAR) * 100.0;
    let risk_free = app.risk_free();
    let sharpe = stats::sharpe_ratio(&returns, risk_free, TRADING_DAYS_PER_YEAR);
    let sortino = stats::sortino_ratio(&returns, risk_free, TRADING_DAYS_PER_YEAR);

    let w = 12;
    let lines = vec![
        widgets::stat_signed("Ann. Return", ann_ret, theme::pct(ann_ret), w),
        widgets::stat("Ann. Vol", theme::pct_plain(ann_vol), w),
        Line::raw(""),
        widgets::stat_signed("Sharpe", sharpe, format!("{sharpe:.2}"), w),
        widgets::stat_signed("Sortino", sortino, format!("{sortino:.2}"), w),
        Line::raw(""),
        widgets::stat("Risk-free", theme::pct_plain(risk_free * 100.0), w),
        Line::from(Span::styled(
            format!("{:<w$} SBP policy rate p.a.", "", w = w),
            theme::label_style(),
        )),
        widgets::stat(
            "Basis",
            format!("{TRADING_DAYS_PER_YEAR:.0} sessions/yr"),
            w,
        ),
    ];

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_benchmark(f: &mut Frame, area: Rect, app: &App) {
    let title = format!("vs {BENCHMARK}");
    let block = widgets::panel(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let (sym, bench) = align_closes(&app.bars, &app.benchmark);
    if sym.len() < MIN_BARS {
        let msg = if app.benchmark.is_empty() {
            format!("Loading {BENCHMARK}…")
        } else {
            format!("Only {} overlapping sessions", sym.len())
        };
        f.render_widget(
            Paragraph::new(Text::from(vec![Line::raw(""), widgets::placeholder(&msg)]))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let sym_r = stats::simple_returns(&sym);
    let bench_r = stats::simple_returns(&bench);
    let beta = stats::beta(&sym_r, &bench_r);
    let corr = stats::correlation(&sym_r, &bench_r);
    let r2 = corr * corr;

    let w = 12;
    let lines = vec![
        widgets::stat("Beta", format!("{beta:.2}"), w),
        widgets::stat("Correlation", format!("{corr:.2}"), w),
        widgets::stat("R²", format!("{r2:.2}"), w),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                widgets::bar(r2, inner.width.saturating_sub(2) as usize),
                Style::new().fg(theme::ACCENT),
            ),
        ]),
        Line::raw(""),
        widgets::stat("Overlap", format!("{} sessions", sym.len()), w),
        Line::from(Span::styled(
            format!(" {}", beta_note(beta)),
            theme::label_style(),
        )),
    ];

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_drawdown(f: &mut Frame, area: Rect, app: &App) {
    let block = widgets::panel("Max Drawdown");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let closes = closes_of(&app.bars);
    let dd = stats::max_drawdown(&closes);
    let pct = dd.pct * 100.0;

    let peak = app.bars.get(dd.peak_idx);
    let trough = app.bars.get(dd.trough_idx);
    let day = |b: Option<&Bar>| b.map(|b| trading_day(b.ts)).unwrap_or_else(|| "—".into());
    let px = |b: Option<&Bar>| {
        b.map(|b| theme::price(b.close))
            .unwrap_or_else(|| "—".into())
    };

    let w = 8;
    let mut lines = vec![
        widgets::stat_signed("Depth", pct, theme::pct(pct), w),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                // A 50% fall saturates the bar: on PSX that is already a disaster.
                widgets::bar(dd.pct.abs() / 0.5, inner.width.saturating_sub(2) as usize),
                Style::new().fg(theme::DOWN),
            ),
        ]),
        widgets::stat("Peak", format!("{}  {}", day(peak), px(peak)), w),
        widgets::stat("Trough", format!("{}  {}", day(trough), px(trough)), w),
        widgets::stat(
            "Length",
            format!("{} sessions", dd.trough_idx.saturating_sub(dd.peak_idx)),
            w,
        ),
    ];

    // How far the scrip still has to climb to make the loss back.
    if dd.pct < 0.0 && dd.pct > -1.0 {
        let recover = (1.0 / (1.0 + dd.pct) - 1.0) * 100.0;
        lines.push(widgets::stat(
            "Recover",
            format!("{} to regain peak", theme::pct(recover)),
            w,
        ));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_relative(f: &mut Frame, area: Rect, app: &App) {
    let title = format!("Relative Performance vs {BENCHMARK}");
    let block = widgets::panel(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let (sym, bench) = align_closes(&app.bars, &app.benchmark);
    if sym.len() < 2 {
        f.render_widget(
            Paragraph::new(Text::from(vec![
                Line::raw(""),
                widgets::placeholder("No overlapping sessions with the benchmark"),
            ]))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let sym_cum = cumulative_pct(&sym);
    let bench_cum = cumulative_pct(&bench);
    let name = if app.selected.is_empty() {
        "SYMBOL"
    } else {
        app.selected.as_str()
    };
    let label_w = name.chars().count().max(BENCHMARK.len()).min(10);
    // " LABEL " + sparkline + " +xx.xx%"
    let spark_w = (inner.width as usize).saturating_sub(label_w + 12);

    let row = |label: &str, series: &[f64]| -> Line<'static> {
        let last = series.last().copied().unwrap_or(0.0);
        Line::from(vec![
            Span::styled(
                format!(" {:<label_w$} ", theme::truncate(label, label_w)),
                theme::header_style(),
            ),
            widgets::sparkline_span(series, spark_w),
            Span::styled(
                format!(" {:>8}", theme::pct(last)),
                Style::new().fg(theme::change_color(last)),
            ),
        ])
    };

    let sym_last = sym_cum.last().copied().unwrap_or(0.0);
    let bench_last = bench_cum.last().copied().unwrap_or(0.0);
    let excess = sym_last - bench_last;

    let mut lines = vec![
        row(name, &sym_cum),
        row(BENCHMARK, &bench_cum),
        Line::raw(""),
        widgets::stat_signed(
            "Excess",
            excess,
            format!("{} over {} aligned sessions", theme::pct(excess), sym.len()),
            9,
        ),
        widgets::stat(
            "Verdict",
            // Tolerance matches the two decimals actually shown, so a
            // "+0.00%" excess never reads as outperformance.
            if excess > 0.005 {
                "outperforming the index"
            } else if excess < -0.005 {
                "lagging the index"
            } else {
                "tracking the index"
            },
            9,
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled(format!(" {:<6}", "Window"), theme::header_style()),
            Span::styled(
                format!("{:>9}", theme::truncate(name, 9)),
                theme::header_style(),
            ),
            Span::styled(format!("{:>10}", BENCHMARK), theme::header_style()),
            Span::styled(format!("{:>10}", "Excess"), theme::header_style()),
        ]),
    ];

    for (label, sessions) in WINDOWS {
        let a = window_return(&sym, sessions);
        let b = window_return(&bench, sessions);
        if a.is_none() && b.is_none() {
            continue;
        }
        let diff = match (a, b) {
            (Some(a), Some(b)) => a - b,
            _ => f64::NAN,
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:<6}"), theme::label_style()),
            Span::styled(
                format!("{:>9}", a.map(theme::pct).unwrap_or_else(|| "—".into())),
                Style::new().fg(theme::change_color(a.unwrap_or(0.0))),
            ),
            Span::styled(
                format!("{:>10}", b.map(theme::pct).unwrap_or_else(|| "—".into())),
                Style::new().fg(theme::change_color(b.unwrap_or(0.0))),
            ),
            Span::styled(
                format!("{:>10}", theme::pct(diff)),
                Style::new().fg(theme::change_color(if diff.is_finite() {
                    diff
                } else {
                    0.0
                })),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

// --- helpers -------------------------------------------------------------

fn closes_of(bars: &[Bar]) -> Vec<f64> {
    bars.iter().map(|b| b.close).collect()
}

/// Pair the scrip's closes with the benchmark's, one entry per trading day both
/// series observed, in chronological order.
///
/// PSX history arrives from two different endpoints and is cached
/// independently, so the two vectors routinely have different lengths *and*
/// different coverage. Truncating to the shorter — which is what the pairwise
/// statistics do on their own — would silently regress Monday's scrip return on
/// Thursday's index return. Matching on the trading day fixes that.
fn align_closes(bars: &[Bar], benchmark: &[Bar]) -> (Vec<f64>, Vec<f64>) {
    if bars.is_empty() || benchmark.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut index: HashMap<String, f64> = HashMap::with_capacity(benchmark.len());
    for b in benchmark {
        // A duplicated day (an intraday snapshot followed by the EOD print)
        // resolves to the last observation.
        index.insert(trading_day(b.ts), b.close);
    }

    let mut sym = Vec::new();
    let mut bench = Vec::new();
    let mut last_day: Option<String> = None;
    for b in bars {
        let day = trading_day(b.ts);
        let Some(&close) = index.get(&day) else {
            continue;
        };
        if last_day.as_deref() == Some(day.as_str()) {
            // Same-day duplicate on the scrip side: keep the latest.
            if let Some(slot) = sym.last_mut() {
                *slot = b.close;
            }
            continue;
        }
        sym.push(b.close);
        bench.push(close);
        last_day = Some(day);
    }
    (sym, bench)
}

/// Close-to-close percentage change over the trailing `sessions` bars, or over
/// the whole series when `None`.
///
/// `None` is returned when the window is not fully covered, so a three-month-old
/// listing shows "—" for 1Y rather than a misleadingly short number.
fn window_return(closes: &[f64], sessions: Option<usize>) -> Option<f64> {
    if closes.len() < 2 {
        return None;
    }
    let start = match sessions {
        Some(n) => {
            if closes.len() < n + 1 {
                return None;
            }
            closes.len() - n - 1
        }
        None => 0,
    };
    let first = *closes.get(start)?;
    let last = *closes.last()?;
    if !first.is_finite() || first <= 0.0 || !last.is_finite() {
        return None;
    }
    let pct = (last / first - 1.0) * 100.0;
    pct.is_finite().then_some(pct)
}

/// Rebase a close series to a cumulative percentage return from its first
/// usable observation.
fn cumulative_pct(closes: &[f64]) -> Vec<f64> {
    let base = closes
        .iter()
        .copied()
        .find(|c| c.is_finite() && *c > 0.0)
        .unwrap_or(0.0);
    if base <= 0.0 {
        return vec![0.0; closes.len()];
    }
    closes
        .iter()
        .map(|c| {
            if c.is_finite() {
                (c / base - 1.0) * 100.0
            } else {
                0.0
            }
        })
        .collect()
}

fn beta_note(beta: f64) -> &'static str {
    if beta >= 1.2 {
        "amplifies index moves"
    } else if beta >= 0.8 {
        "moves with the index"
    } else if beta > 0.0 {
        "damps index moves"
    } else {
        "no reliable relationship"
    }
}

/// A bordered panel whose whole body is a single centred message.
fn notice(f: &mut Frame, area: Rect, title: &str, msg: &str) {
    let block = widgets::panel(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let pad = (inner.height.saturating_sub(1) / 2) as usize;
    let mut lines: Vec<Line> = vec![Line::raw(""); pad];
    lines.push(widgets::placeholder(msg));
    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::app::{DataEvent, detached_channel};
    use crate::cache::{Store, day_close_ts};

    fn bar(day: &str, close: f64) -> Bar {
        Bar {
            ts: day_close_ts(day),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
        }
    }

    // -- alignment ---------------------------------------------------------

    #[test]
    fn alignment_keeps_only_days_present_in_both_series() {
        let bars = vec![
            bar("2025-01-01", 10.0),
            bar("2025-01-02", 11.0),
            bar("2025-01-03", 12.0),
            bar("2025-01-06", 13.0),
        ];
        // The benchmark is missing the 2nd and carries a day the scrip never
        // traded on.
        let bench = vec![
            bar("2025-01-01", 100.0),
            bar("2025-01-03", 102.0),
            bar("2025-01-05", 103.0),
            bar("2025-01-06", 104.0),
        ];

        let (sym, mkt) = align_closes(&bars, &bench);
        assert_eq!(sym, vec![10.0, 12.0, 13.0]);
        assert_eq!(mkt, vec![100.0, 102.0, 104.0]);
    }

    #[test]
    fn alignment_survives_different_lengths_and_offsets() {
        // A newly listed scrip against a long index history.
        let bench: Vec<Bar> = (1..=28)
            .map(|d| bar(&format!("2025-02-{d:02}"), 100.0 + d as f64))
            .collect();
        let bars: Vec<Bar> = (20..=28)
            .map(|d| bar(&format!("2025-02-{d:02}"), 5.0 + d as f64))
            .collect();

        let (sym, mkt) = align_closes(&bars, &bench);
        assert_eq!(sym.len(), 9);
        assert_eq!(mkt.len(), 9);
        assert_eq!(sym[0], 25.0);
        assert_eq!(mkt[0], 120.0);
    }

    #[test]
    fn alignment_is_empty_when_coverage_does_not_overlap() {
        let bars = vec![bar("2024-01-02", 10.0), bar("2024-01-03", 11.0)];
        let bench = vec![bar("2025-01-02", 100.0), bar("2025-01-03", 101.0)];
        let (sym, mkt) = align_closes(&bars, &bench);
        assert!(sym.is_empty() && mkt.is_empty());
    }

    #[test]
    fn alignment_handles_empty_inputs() {
        assert_eq!(align_closes(&[], &[]), (vec![], vec![]));
        assert_eq!(
            align_closes(&[bar("2025-01-01", 1.0)], &[]),
            (vec![], vec![])
        );
        assert_eq!(
            align_closes(&[], &[bar("2025-01-01", 1.0)]),
            (vec![], vec![])
        );
    }

    #[test]
    fn duplicate_days_collapse_to_the_latest_observation() {
        let bars = vec![
            bar("2025-01-01", 10.0),
            bar("2025-01-01", 10.5),
            bar("2025-01-02", 11.0),
        ];
        let bench = vec![bar("2025-01-01", 100.0), bar("2025-01-02", 101.0)];
        let (sym, mkt) = align_closes(&bars, &bench);
        assert_eq!(sym, vec![10.5, 11.0]);
        assert_eq!(mkt, vec![100.0, 101.0]);
    }

    #[test]
    fn alignment_makes_beta_meaningful() {
        // The scrip moves exactly twice the index on every shared day; the
        // benchmark additionally carries days the scrip never traded, which a
        // naive length truncation would smear across the regression.
        let mut bench = Vec::new();
        let mut bars = Vec::new();
        for d in 1..=20 {
            let day = format!("2025-04-{d:02}");
            bench.push(bar(&day, 100.0 * 1.01f64.powi(d)));
            if d % 3 != 0 {
                bars.push(bar(&day, 50.0 * 1.02f64.powi(d)));
            }
        }
        let (s, m) = align_closes(&bars, &bench);
        assert_eq!(s.len(), m.len());
        let b = stats::beta(&stats::simple_returns(&s), &stats::simple_returns(&m));
        assert!(b.is_finite());
        assert!(b > 1.5 && b < 2.5, "beta should be ~2, got {b}");
    }

    // -- windows -----------------------------------------------------------

    #[test]
    fn window_returns_require_full_coverage() {
        let closes: Vec<f64> = (0..30).map(|i| 100.0 + i as f64).collect();
        assert!(window_return(&closes, Some(22)).is_some());
        assert!(window_return(&closes, Some(250)).is_none());
        assert!(window_return(&closes, None).is_some());
        assert!(window_return(&[], None).is_none());
        assert!(window_return(&[1.0], None).is_none());
    }

    #[test]
    fn window_return_is_hand_checkable() {
        let closes = [100.0, 110.0, 121.0];
        assert!((window_return(&closes, None).unwrap() - 21.0).abs() < 1e-9);
        assert!((window_return(&closes, Some(1)).unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn window_return_guards_non_positive_and_non_finite_prices() {
        assert!(window_return(&[0.0, 10.0], None).is_none());
        assert!(window_return(&[f64::NAN, 10.0], None).is_none());
        assert!(window_return(&[10.0, f64::INFINITY], None).is_none());
    }

    #[test]
    fn cumulative_pct_rebases_to_zero() {
        let c = cumulative_pct(&[50.0, 55.0, 45.0]);
        assert_eq!(c.len(), 3);
        assert!(c[0].abs() < 1e-9);
        assert!((c[1] - 10.0).abs() < 1e-9);
        assert!(c.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn cumulative_pct_handles_a_worthless_series() {
        assert_eq!(cumulative_pct(&[0.0, 0.0]), vec![0.0, 0.0]);
        assert!(cumulative_pct(&[]).is_empty());
    }

    // -- rendering smoke tests --------------------------------------------

    fn app_with(bars: Vec<Bar>, bench: Vec<Bar>) -> App {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _rx) = detached_channel();
        let mut a = App::new(store, tx);
        a.selected = "HBL".into();
        a.bars = bars;
        a.on_event(DataEvent::Bars {
            symbol: BENCHMARK.into(),
            bars: bench,
        });
        a
    }

    fn series(n: i64, base: f64) -> Vec<Bar> {
        (1..=n)
            .map(|i| Bar {
                ts: 1_700_000_000 + i * 86_400,
                open: base,
                high: base * 1.02,
                low: base * 0.98,
                close: base * (1.0 + 0.1 * (i as f64 / 7.0).sin()),
                volume: 1_000.0,
            })
            .collect()
    }

    fn render(app: &App, w: u16, h: u16) -> Buffer {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, f.area(), app))
            .expect("analysis screen must render");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn eyeball_dump() {
        let app = app_with(series(300, 100.0), series(300, 50_000.0));
        for (w, h) in [(140u16, 22u16), (20, 10)] {
            let buf = render(&app, w, h);
            println!("=== {w}x{h} ===");
            for y in 0..h {
                let row: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
                println!("{row}");
            }
        }
    }

    #[test]
    fn renders_without_panicking_at_every_size() {
        let full = app_with(series(300, 100.0), series(300, 50_000.0));
        let short = app_with(series(5, 100.0), series(300, 50_000.0));
        let empty = app_with(Vec::new(), Vec::new());
        let no_bench = app_with(series(300, 100.0), Vec::new());
        let flat = app_with(
            (1..=200)
                .map(|i| Bar {
                    ts: 1_700_000_000 + i * 86_400,
                    open: 10.0,
                    high: 10.0,
                    low: 10.0,
                    close: 10.0,
                    volume: 0.0,
                })
                .collect(),
            series(200, 50_000.0),
        );

        for app in [&full, &short, &empty, &no_bench, &flat] {
            for (w, h) in [(20u16, 10u16), (1, 1), (40, 12), (80, 24), (200, 60)] {
                render(app, w, h);
            }
        }
    }
}
