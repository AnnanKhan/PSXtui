//! Intraday microstructure: the session's trade-by-trade feed.
//!
//! PSX publishes intraday ticks as a flat list of trades. Four views are built
//! from it: the price path with session VWAP overlaid, the session's summary
//! statistics, where the volume actually traded through the day, and the raw
//! tape.

use chrono::{DateTime, Utc};
use ratatui::prelude::*;
use ratatui::symbols::Marker;
use ratatui::widgets::{Axis, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table, Wrap};

use super::{theme, widgets};
use crate::analysis::indicators::vwap_session;
use crate::app::App;
use crate::cache::pkt;
use crate::model::Tick;

/// Width of a volume-histogram bucket. Fifteen minutes over a ~5-hour PSX
/// session gives around twenty buckets — enough to see the open and close
/// spikes without turning into noise.
const BUCKET_SECS: i64 = 15 * 60;

/// How many trades the tape shows.
const TAPE_ROWS: usize = 20;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if app.ticks.is_empty() {
        let msg = if app.selected.is_empty() {
            "No symbol selected — choose one on the Screener".to_string()
        } else {
            format!(
                "No trades for {} today — the market is closed or the scrip is untraded",
                app.selected
            )
        };
        notice(f, area, "Intraday", &msg);
        return;
    }

    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
    let [chart_area, stats_area] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(30)]).areas(top);
    let [volume_area, tape_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bottom);

    draw_price(f, chart_area, app);
    draw_stats(f, stats_area, app);
    draw_volume(f, volume_area, &app.ticks);
    draw_tape(f, tape_area, &app.ticks);
}

// --- price + VWAP --------------------------------------------------------

fn draw_price(f: &mut Frame, area: Rect, app: &App) {
    let title = if app.selected.is_empty() {
        "Session".to_string()
    } else {
        format!("{} · Session Price & VWAP", app.selected)
    };
    let block = widgets::panel(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let ticks = &app.ticks;
    let vwap = vwap_session(ticks);

    let price_pts: Vec<(f64, f64)> = ticks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.price.is_finite())
        .map(|(i, t)| (i as f64, t.price))
        .collect();
    let vwap_pts: Vec<(f64, f64)> = vwap
        .iter()
        .enumerate()
        .filter_map(|(i, v)| v.map(|v| (i as f64, v)))
        .collect();

    if price_pts.is_empty() {
        f.render_widget(
            Paragraph::new(Text::from(vec![widgets::placeholder("No priced trades")])),
            inner,
        );
        return;
    }

    // A real chart needs axis room; fall back to a sparkline in a cramped pane.
    if inner.width < 24 || inner.height < 6 {
        let closes: Vec<f64> = price_pts.iter().map(|(_, p)| *p).collect();
        f.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(widgets::sparkline_span(&closes, inner.width as usize)),
                Line::from(Span::styled(
                    theme::price(closes.last().copied().unwrap_or(f64::NAN)),
                    theme::value_style(),
                )),
            ])),
            inner,
        );
        return;
    }

    let (y_lo, y_hi) = price_bounds(&price_pts, &vwap_pts);
    let x_hi = (ticks.len().saturating_sub(1)).max(1) as f64;

    let first_ts = ticks.first().map(|t| t.ts).unwrap_or_default();
    let last_ts = ticks.last().map(|t| t.ts).unwrap_or_default();
    let mid_ts = first_ts + (last_ts - first_ts) / 2;

    let datasets = vec![
        Dataset::default()
            .name("Price")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(theme::ACCENT))
            .data(&price_pts),
        Dataset::default()
            .name("VWAP")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(theme::WARN))
            .data(&vwap_pts),
    ];

    let x_axis = Axis::default()
        .style(theme::border_style())
        .bounds([0.0, x_hi])
        .labels(vec![
            Span::styled(pkt_time(first_ts, "%H:%M"), theme::label_style()),
            Span::styled(pkt_time(mid_ts, "%H:%M"), theme::label_style()),
            Span::styled(pkt_time(last_ts, "%H:%M"), theme::label_style()),
        ]);
    let y_axis = Axis::default()
        .style(theme::border_style())
        .bounds([y_lo, y_hi])
        .labels(vec![
            Span::styled(theme::price(y_lo), theme::label_style()),
            Span::styled(theme::price((y_lo + y_hi) / 2.0), theme::label_style()),
            Span::styled(theme::price(y_hi), theme::label_style()),
        ]);

    f.render_widget(
        Chart::new(datasets)
            .x_axis(x_axis)
            .y_axis(y_axis)
            .style(Style::new().fg(theme::FG)),
        inner,
    );
}

// --- session statistics --------------------------------------------------

fn draw_stats(f: &mut Frame, area: Rect, app: &App) {
    let block = widgets::panel("Session");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let s = summarize(&app.ticks);
    let w = 10;
    let vs_vwap = s.last - s.vwap;
    let vs_vwap_pct = if s.vwap > 0.0 && s.vwap.is_finite() {
        vs_vwap / s.vwap * 100.0
    } else {
        f64::NAN
    };

    let mut lines = vec![
        widgets::stat("Open", theme::price(s.open), w),
        widgets::stat("High", theme::price(s.high), w),
        widgets::stat("Low", theme::price(s.low), w),
        widgets::stat("Last", theme::price(s.last), w),
        Line::raw(""),
        widgets::stat("VWAP", theme::price(s.vwap), w),
        widgets::stat_signed(
            "vs VWAP",
            vs_vwap,
            format!(
                "{} ({})",
                theme::signed(vs_vwap),
                if vs_vwap_pct.is_finite() {
                    theme::pct(vs_vwap_pct)
                } else {
                    "—".into()
                }
            ),
            w,
        ),
        Line::from(Span::styled(
            format!(
                " {:<w$}{}",
                "",
                if !s.last.is_finite() || !s.vwap.is_finite() {
                    "—"
                } else if s.last > s.vwap {
                    "above VWAP"
                } else if s.last < s.vwap {
                    "below VWAP"
                } else {
                    "at VWAP"
                },
                w = w
            ),
            theme::label_style(),
        )),
        Line::raw(""),
        widgets::stat("Volume", theme::compact(s.volume), w),
        widgets::stat("Value", format!("PKR {}", theme::compact(s.value)), w),
        widgets::stat("Trades", theme::compact(s.count as f64), w),
    ];

    if let (Some(a), Some(b)) = (app.ticks.first(), app.ticks.last()) {
        lines.push(widgets::stat(
            "Window",
            format!("{}–{}", pkt_time(a.ts, "%H:%M"), pkt_time(b.ts, "%H:%M")),
            w,
        ));
    }
    if let Some(q) = app.selected_quote() {
        let chg = s.last - q.ldcp;
        let pct = if q.ldcp > 0.0 {
            chg / q.ldcp * 100.0
        } else {
            f64::NAN
        };
        lines.push(widgets::stat("LDCP", theme::price(q.ldcp), w));
        lines.push(widgets::stat_signed("Change", chg, theme::pct(pct), w));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

// --- volume by time of day ----------------------------------------------

fn draw_volume(f: &mut Frame, area: Rect, ticks: &[Tick]) {
    // Widen the slice rather than truncate the day: a histogram that stops at
    // lunchtime is worse than a coarser one that shows the whole session.
    let rows = area.height.saturating_sub(2) as usize;
    let secs = fit_bucket_secs(ticks, rows);
    let title = format!("Volume by Time ({}, PKT)", span_label(secs));
    let block = widgets::panel(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let buckets = bucket_volume(ticks, secs);
    if buckets.is_empty() {
        f.render_widget(
            Paragraph::new(Text::from(vec![widgets::placeholder("No volume")])),
            inner,
        );
        return;
    }

    let peak = buckets.iter().fold(0.0f64, |m, b| m.max(b.volume));
    // " HH:MM " + bar + " 12.3M"
    let bar_w = (inner.width as usize).saturating_sub(16);

    let lines: Vec<Line> = buckets
        .iter()
        .take(inner.height as usize)
        .map(|b| {
            let ratio = if peak > 0.0 { b.volume / peak } else { 0.0 };
            Line::from(vec![
                Span::styled(
                    format!(" {} ", pkt_time(b.start, "%H:%M")),
                    theme::label_style(),
                ),
                Span::styled(
                    widgets::bar(ratio, bar_w),
                    Style::new().fg(if b.volume > 0.0 {
                        theme::VOLUME
                    } else {
                        theme::BORDER
                    }),
                ),
                Span::styled(
                    format!("{:>8}", theme::compact(b.volume)),
                    theme::value_style(),
                ),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

// --- the tape ------------------------------------------------------------

fn draw_tape(f: &mut Frame, area: Rect, ticks: &[Tick]) {
    let block = widgets::panel("Tape · latest trades");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let visible = (inner.height.saturating_sub(1) as usize).clamp(1, TAPE_ROWS);
    let start = ticks.len().saturating_sub(visible);

    // Newest first, coloured by the direction of the last price change.
    let rows: Vec<Row> = (start..ticks.len())
        .rev()
        .filter_map(|i| {
            let t = ticks.get(i)?;
            let prev = i.checked_sub(1).and_then(|j| ticks.get(j));
            let dir = match prev {
                Some(p) if t.price > p.price => 1.0,
                Some(p) if t.price < p.price => -1.0,
                _ => 0.0,
            };
            let color = theme::change_color(dir);
            let arrow = match dir {
                d if d > 0.0 => "▲",
                d if d < 0.0 => "▼",
                _ => "·",
            };
            Some(Row::new(vec![
                Cell::from(Span::styled(
                    pkt_time(t.ts, "%H:%M:%S"),
                    theme::label_style(),
                )),
                Cell::from(Span::styled(arrow, Style::new().fg(color))),
                Cell::from(
                    Text::from(theme::price(t.price))
                        .right_aligned()
                        .style(Style::new().fg(color)),
                ),
                Cell::from(
                    Text::from(theme::compact(t.volume))
                        .right_aligned()
                        .style(theme::value_style()),
                ),
            ]))
        })
        .collect();

    let header = Row::new(vec![
        Cell::from(Span::styled("Time", theme::header_style())),
        Cell::from(Span::styled(" ", theme::header_style())),
        Cell::from(
            Text::from("Price")
                .right_aligned()
                .style(theme::header_style()),
        ),
        Cell::from(
            Text::from("Volume")
                .right_aligned()
                .style(theme::header_style()),
        ),
    ]);

    f.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Length(1),
                Constraint::Min(8),
                Constraint::Length(9),
            ],
        )
        .header(header)
        .column_spacing(1),
        inner,
    );
}

// --- pure helpers --------------------------------------------------------

/// One 15-minute slice of the session.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bucket {
    /// Unix timestamp of the bucket's start.
    start: i64,
    volume: f64,
    trades: usize,
}

/// Bucket ticks into fixed-width slices of the trading day.
///
/// Buckets are emitted contiguously between the first and last trade so a
/// lunchtime lull shows as a gap rather than being silently closed up. The
/// boundary is computed in UTC, which is exact for PKT because the +05:00 offset
/// is a whole number of 15-minute slices.
fn bucket_volume(ticks: &[Tick], bucket_secs: i64) -> Vec<Bucket> {
    if ticks.is_empty() || bucket_secs <= 0 {
        return Vec::new();
    }

    let floor = |ts: i64| ts.div_euclid(bucket_secs) * bucket_secs;
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    for t in ticks {
        lo = lo.min(t.ts);
        hi = hi.max(t.ts);
    }
    let (start, end) = (floor(lo), floor(hi));

    // A corrupt timestamp must not make us allocate the heat death of the
    // universe: cap the span at one day's worth of buckets.
    let max_buckets = (86_400 / bucket_secs).max(1) as usize;
    let count = (((end - start) / bucket_secs) as usize + 1).min(max_buckets);

    let mut out: Vec<Bucket> = (0..count)
        .map(|i| Bucket {
            start: start + i as i64 * bucket_secs,
            volume: 0.0,
            trades: 0,
        })
        .collect();

    for t in ticks {
        let idx = ((floor(t.ts) - start) / bucket_secs) as usize;
        if let Some(b) = out.get_mut(idx) {
            if t.volume.is_finite() && t.volume > 0.0 {
                b.volume += t.volume;
            }
            b.trades += 1;
        }
    }
    out
}

/// The smallest bucket width, starting from 15 minutes and doubling, whose
/// histogram fits in `rows` lines. Caps at four hours, beyond which the whole
/// session is a single bar anyway.
fn fit_bucket_secs(ticks: &[Tick], rows: usize) -> i64 {
    let mut secs = BUCKET_SECS;
    if rows == 0 || ticks.is_empty() {
        return secs;
    }
    while secs < 4 * 3600 && bucket_volume(ticks, secs).len() > rows {
        secs *= 2;
    }
    secs
}

/// A bucket width as a short human label: `15m`, `1h`, `2h`.
fn span_label(secs: i64) -> String {
    if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", secs / 60)
    }
}

/// Session summary numbers, all guaranteed finite or `NaN` (which the theme
/// renders as an em dash).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Summary {
    open: f64,
    high: f64,
    low: f64,
    last: f64,
    vwap: f64,
    volume: f64,
    value: f64,
    count: usize,
}

fn summarize(ticks: &[Tick]) -> Summary {
    let mut s = Summary {
        open: f64::NAN,
        high: f64::NAN,
        low: f64::NAN,
        last: f64::NAN,
        vwap: f64::NAN,
        volume: 0.0,
        value: 0.0,
        count: 0,
    };

    let (mut hi, mut lo) = (f64::NEG_INFINITY, f64::INFINITY);
    for t in ticks {
        s.count += 1;
        if !t.price.is_finite() {
            continue;
        }
        if !s.open.is_finite() {
            s.open = t.price;
        }
        s.last = t.price;
        hi = hi.max(t.price);
        lo = lo.min(t.price);
        if t.volume.is_finite() && t.volume > 0.0 {
            s.volume += t.volume;
            s.value += t.price * t.volume;
        }
    }

    s.high = if hi.is_finite() { hi } else { f64::NAN };
    s.low = if lo.is_finite() { lo } else { f64::NAN };
    s.vwap = if s.volume > 0.0 {
        s.value / s.volume
    } else {
        f64::NAN
    };
    s
}

/// Y-axis bounds wide enough for both series, never degenerate.
fn price_bounds(price: &[(f64, f64)], vwap: &[(f64, f64)]) -> (f64, f64) {
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for (_, v) in price.iter().chain(vwap.iter()) {
        if v.is_finite() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        return (0.0, 1.0);
    }
    // An untraded scrip prints the same price all session; give the line
    // somewhere to sit instead of collapsing the axis.
    if (hi - lo).abs() < f64::EPSILON {
        let pad = (hi.abs() * 0.01).max(0.5);
        return (lo - pad, hi + pad);
    }
    let pad = (hi - lo) * 0.08;
    (lo - pad, hi + pad)
}

/// Format a Unix timestamp in Pakistan Standard Time.
fn pkt_time(ts: i64, fmt: &str) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&pkt()).format(fmt).to_string())
        .unwrap_or_else(|| "--:--".into())
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
    use crate::app::detached_channel;
    use crate::cache::{Store, day_close_ts};

    /// 09:30 PKT on a fixed day, as a Unix timestamp.
    fn session_open() -> i64 {
        // `day_close_ts` is 16:00 PKT; wind back to 09:30.
        day_close_ts("2026-01-15") - 6 * 3600 - 30 * 60
    }

    fn tick(offset_secs: i64, price: f64, volume: f64) -> Tick {
        Tick {
            ts: session_open() + offset_secs,
            price,
            volume,
        }
    }

    // -- bucketing ---------------------------------------------------------

    #[test]
    fn ticks_fall_into_fifteen_minute_buckets() {
        let ticks = vec![
            tick(0, 100.0, 10.0),
            tick(60, 101.0, 5.0),
            // 20 minutes in: the next bucket.
            tick(20 * 60, 102.0, 7.0),
        ];
        let b = bucket_volume(&ticks, BUCKET_SECS);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].volume, 15.0);
        assert_eq!(b[0].trades, 2);
        assert_eq!(b[1].volume, 7.0);
        assert_eq!(b[1].trades, 1);
    }

    #[test]
    fn bucket_boundaries_land_on_the_quarter_hour_in_pkt() {
        let ticks = vec![tick(0, 100.0, 1.0), tick(3 * 3600, 100.0, 1.0)];
        for b in bucket_volume(&ticks, BUCKET_SECS) {
            let label = pkt_time(b.start, "%M");
            assert!(
                ["00", "15", "30", "45"].contains(&label.as_str()),
                "bucket started at :{label}"
            );
        }
    }

    #[test]
    fn empty_buckets_are_emitted_for_gaps_in_trading() {
        // One trade at the open, one an hour later: four buckets, two empty.
        let ticks = vec![tick(0, 100.0, 10.0), tick(60 * 60, 105.0, 20.0)];
        let b = bucket_volume(&ticks, BUCKET_SECS);
        assert_eq!(b.len(), 5);
        assert_eq!(b[1].volume, 0.0);
        assert_eq!(b[2].trades, 0);
        assert_eq!(b[4].volume, 20.0);
    }

    #[test]
    fn bucketing_guards_degenerate_input() {
        assert!(bucket_volume(&[], BUCKET_SECS).is_empty());
        assert!(bucket_volume(&[tick(0, 1.0, 1.0)], 0).is_empty());
        assert_eq!(bucket_volume(&[tick(0, 1.0, 1.0)], BUCKET_SECS).len(), 1);
        // Non-positive and non-finite volumes still count as trades.
        let b = bucket_volume(&[tick(0, 1.0, -5.0), tick(1, 1.0, f64::NAN)], BUCKET_SECS);
        assert_eq!(b[0].volume, 0.0);
        assert_eq!(b[0].trades, 2);
    }

    #[test]
    fn bucket_width_widens_until_the_session_fits_the_pane() {
        // A five-hour session is twenty 15-minute buckets.
        let ticks: Vec<Tick> = (0..300).map(|i| tick(i * 60, 100.0, 10.0)).collect();
        assert_eq!(fit_bucket_secs(&ticks, 25), BUCKET_SECS);
        assert_eq!(fit_bucket_secs(&ticks, 12), 2 * BUCKET_SECS);
        assert_eq!(fit_bucket_secs(&ticks, 4), 8 * BUCKET_SECS);
        // Degenerate panes and empty sessions fall back to the default.
        assert_eq!(fit_bucket_secs(&ticks, 0), BUCKET_SECS);
        assert_eq!(fit_bucket_secs(&[], 10), BUCKET_SECS);
        // The widening is bounded.
        assert!(fit_bucket_secs(&ticks, 1) <= 4 * 3600);
    }

    #[test]
    fn span_labels_are_short() {
        assert_eq!(span_label(900), "15m");
        assert_eq!(span_label(1800), "30m");
        assert_eq!(span_label(3600), "1h");
        assert_eq!(span_label(7200), "2h");
    }

    #[test]
    fn a_wild_timestamp_cannot_explode_the_bucket_count() {
        let ticks = vec![tick(0, 1.0, 1.0), tick(3_000 * 86_400, 1.0, 1.0)];
        let b = bucket_volume(&ticks, BUCKET_SECS);
        assert!(b.len() <= 96, "got {} buckets", b.len());
    }

    // -- summary -----------------------------------------------------------

    #[test]
    fn summary_is_hand_checkable() {
        let ticks = vec![
            tick(0, 100.0, 10.0),
            tick(60, 110.0, 10.0),
            tick(120, 90.0, 0.0),
        ];
        let s = summarize(&ticks);
        assert_eq!(s.open, 100.0);
        assert_eq!(s.high, 110.0);
        assert_eq!(s.low, 90.0);
        assert_eq!(s.last, 90.0);
        assert_eq!(s.volume, 20.0);
        assert_eq!(s.value, 2100.0);
        assert_eq!(s.count, 3);
        assert!((s.vwap - 105.0).abs() < 1e-9);
    }

    #[test]
    fn summary_of_an_empty_or_broken_session_never_leaks_infinity() {
        let s = summarize(&[]);
        assert!(s.open.is_nan() && s.vwap.is_nan() && s.high.is_nan());
        assert_eq!(s.volume, 0.0);

        let s = summarize(&[tick(0, f64::NAN, 5.0)]);
        assert!(s.high.is_nan() && s.low.is_nan() && s.vwap.is_nan());
        assert_eq!(s.count, 1);
    }

    // -- axis bounds -------------------------------------------------------

    #[test]
    fn price_bounds_never_collapse() {
        let (lo, hi) = price_bounds(&[(0.0, 10.0), (1.0, 10.0)], &[]);
        assert!(hi > lo, "a flat session still needs a visible axis");

        let (lo, hi) = price_bounds(&[], &[]);
        assert!(hi > lo);

        let (lo, hi) = price_bounds(&[(0.0, 10.0), (1.0, 20.0)], &[(0.0, 5.0)]);
        assert!(lo < 5.0 && hi > 20.0);
    }

    #[test]
    fn pkt_time_formats_in_pakistan_time() {
        // 09:30 PKT by construction.
        assert_eq!(pkt_time(session_open(), "%H:%M"), "09:30");
        assert_eq!(pkt_time(i64::MAX, "%H:%M"), "--:--");
    }

    // -- rendering smoke tests --------------------------------------------

    fn app_with(ticks: Vec<Tick>) -> App {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _rx) = detached_channel();
        let mut a = App::new(store, tx);
        a.selected = "HBL".into();
        a.ticks = ticks;
        a
    }

    fn render(app: &App, w: u16, h: u16) -> Buffer {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, f.area(), app))
            .expect("intraday screen must render");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn eyeball_dump() {
        let app = app_with(
            (0..400)
                .map(|i| {
                    tick(
                        i * 45,
                        250.0 + 5.0 * (i as f64 / 40.0).sin(),
                        (i % 17) as f64 * 1_000.0,
                    )
                })
                .collect(),
        );
        for (w, h) in [(140u16, 26u16), (20, 10)] {
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
        let busy = app_with(
            (0..600)
                .map(|i| {
                    tick(
                        i * 30,
                        250.0 + 5.0 * (i as f64 / 30.0).sin(),
                        (i % 17) as f64 * 100.0,
                    )
                })
                .collect(),
        );
        let single = app_with(vec![tick(0, 250.0, 1_000.0)]);
        let flat = app_with((0..50).map(|i| tick(i * 60, 10.0, 100.0)).collect());
        let zero_volume = app_with((0..50).map(|i| tick(i * 60, 10.0, 0.0)).collect());
        let broken = app_with(vec![
            tick(0, f64::NAN, f64::NAN),
            tick(60, 1.0, f64::INFINITY),
        ]);
        let empty = app_with(Vec::new());

        for app in [&busy, &single, &flat, &zero_volume, &broken, &empty] {
            for (w, h) in [(20u16, 10u16), (1, 1), (40, 12), (80, 24), (200, 60)] {
                render(app, w, h);
            }
        }
    }
}
