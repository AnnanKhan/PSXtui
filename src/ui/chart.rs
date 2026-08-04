//! Price chart: candlesticks with moving-average / Bollinger overlays and a
//! switchable indicator pane beneath.
//!
//! A terminal is far narrower than a year of trading sessions, so bars are
//! aggregated into one candle per column ([`aggregate`]) rather than being
//! dropped or truncated — the visible shape stays faithful at every range.
//! Indicators are still computed on the *daily* series and then sampled at
//! bucket boundaries, so "SMA(20)" always means twenty sessions regardless of
//! how many pixels a session gets.

use ratatui::prelude::*;
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine, Rectangle};
use ratatui::widgets::{Block, Paragraph};

use super::hit::{Target, Zone};
use super::{theme, widgets};
use crate::analysis::indicators;
use crate::app::{App, ChartStyle, Pane, Range};
use crate::cache::trading_day;
use crate::model::Bar;

/// Width of the price-axis gutter, wide enough for `1,466,852.00`.
const AXIS_WIDTH: u16 = 11;
/// Rows reserved for the lower indicator pane.
const PANE_HEIGHT: u16 = 9;

/// Donchian channel lookback, in sessions.
const DONCHIAN_PERIOD: usize = 20;
/// Ichimoku conversion / base / span-B windows — the standard 9, 26, 52.
const ICHIMOKU: (usize, usize, usize) = (9, 26, 52);
/// How near two swing pivots must be to count as the same level: 1%.
const LEVEL_SENSITIVITY: f64 = 0.01;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let bars = app.ranged_bars();

    let [header, main] = Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(area);
    draw_header(f, header, app, bars);

    if bars.is_empty() {
        let block = widgets::panel("Price");
        let inner = block.inner(main);
        f.render_widget(block, main);
        if inner.height > 0 {
            // Distinguish "still fetching" from "PSX has nothing" — otherwise
            // an untraded scrip looks identical to a hung request.
            let msg = if app.selected.is_empty() {
                "Select a symbol to chart it".to_string()
            } else if app.is_busy() {
                format!(
                    "{} Fetching {} daily history from PSX…",
                    app.spinner_glyph(),
                    app.selected
                )
            } else {
                format!("No price history published for {}", app.selected)
            };
            let pad = (inner.height.saturating_sub(1) / 2) as usize;
            let mut lines: Vec<Line> = vec![Line::raw(""); pad];
            lines.push(widgets::placeholder(&msg));
            f.render_widget(Paragraph::new(Text::from(lines)), inner);
        }
        return;
    }

    // Give the indicator pane its rows only when there is room to spare.
    let pane_height = if main.height > PANE_HEIGHT + 6 {
        PANE_HEIGHT
    } else {
        0
    };
    let [price_area, pane_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(pane_height)]).areas(main);

    draw_price(f, price_area, app, bars);
    if pane_height > 0 {
        draw_pane(f, pane_area, app, bars);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App, bars: &[Bar]) {
    let title = format!(
        "{} — {}",
        app.selected,
        theme::truncate(&app.company_name(&app.selected), 40)
    );
    let block = widgets::panel(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let last = bars.last();
    let mut top = Vec::new();

    if let Some(q) = app.selected_quote() {
        top.push(Span::styled(
            theme::price(q.current),
            Style::new().fg(theme::FG).bold(),
        ));
        top.push(Span::styled(
            format!(
                "  {} ({})  ",
                theme::signed(q.change),
                theme::pct(q.change_pct)
            ),
            Style::new().fg(theme::change_color(q.change_pct)),
        ));
    } else if let Some(b) = last {
        top.push(Span::styled(
            theme::price(b.close),
            Style::new().fg(theme::FG).bold(),
        ));
        top.push(Span::raw("  "));
    }

    if let Some(b) = last {
        for (label, value) in [("O", b.open), ("H", b.high), ("L", b.low), ("C", b.close)] {
            top.push(Span::styled(format!("{label} "), theme::label_style()));
            top.push(Span::styled(theme::price(value), theme::value_style()));
            top.push(Span::raw("  "));
        }
        top.push(Span::styled("Vol ", theme::label_style()));
        top.push(Span::styled(theme::compact(b.volume), theme::value_style()));
    }

    // Range selector, active entry highlighted. The x position of each label
    // is tracked as it is laid out so the buttons can be clicked — the header
    // is a hand-built line, so nothing else knows where they landed.
    // Row 1 of the panel's interior holds this line.
    let button_y = inner.y + 1;
    let mut x = inner.x + " Range ".len() as u16;
    let mut ranges = vec![Span::styled(" Range ", theme::label_style())];
    for (i, r) in Range::ALL.iter().enumerate() {
        let style = if *r == app.chart.range {
            Style::new().fg(theme::ACCENT).bold()
        } else {
            Style::new().fg(theme::DIM)
        };
        let text = format!("{} ", r.label());
        let w = text.chars().count() as u16;
        if button_y < inner.bottom() && x < inner.right() {
            app.hits.borrow_mut().target(
                Rect {
                    x,
                    y: button_y,
                    width: w.min(inner.right() - x),
                    height: 1,
                },
                Target::ChartRange(i),
            );
        }
        x = x.saturating_add(w);
        ranges.push(Span::styled(text, style));
    }
    ranges.push(Span::styled("  Overlays ", theme::label_style()));
    x = x.saturating_add("  Overlays ".len() as u16);
    for (i, (on, label)) in [
        (app.chart.show_sma, "SMA20"),
        (app.chart.show_ema, "EMA50"),
        (app.chart.show_bollinger, "BB20"),
        (app.chart.show_donchian, "DC20"),
        (app.chart.show_ichimoku, "ICHI"),
        (app.chart.show_levels, "S/R"),
    ]
    .into_iter()
    .enumerate()
    {
        let text = format!("{label} ");
        let w = text.chars().count() as u16;
        if button_y < inner.bottom() && x < inner.right() {
            app.hits.borrow_mut().target(
                Rect {
                    x,
                    y: button_y,
                    width: w.min(inner.right() - x),
                    height: 1,
                },
                Target::ChartOverlay(i),
            );
        }
        x = x.saturating_add(w);
        ranges.push(Span::styled(
            text,
            if on {
                Style::new().fg(theme::ACCENT)
            } else {
                Style::new().fg(theme::BORDER)
            },
        ));
    }
    ranges.push(Span::styled("  Style ", theme::label_style()));
    x = x.saturating_add("  Style ".len() as u16);
    let style_label = app.chart.style.label();
    let style_w = style_label.chars().count() as u16 + 4; // label + " (c) "
    if button_y < inner.bottom() && x < inner.right() {
        app.hits.borrow_mut().target(
            Rect {
                x,
                y: button_y,
                width: style_w.min(inner.right() - x),
                height: 1,
            },
            Target::ChartStyle,
        );
    }
    ranges.push(Span::styled(
        style_label,
        Style::new().fg(theme::ACCENT).bold(),
    ));
    ranges.push(Span::styled(" (c) ", Style::new().fg(theme::DIM)));
    ranges.push(Span::styled(
        format!(" {} sessions", bars.len()),
        theme::label_style(),
    ));

    f.render_widget(
        Paragraph::new(Text::from(vec![Line::from(top), Line::from(ranges)])),
        inner,
    );
}

fn draw_price(f: &mut Frame, area: Rect, app: &App, bars: &[Bar]) {
    let block = widgets::panel("Price");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width <= AXIS_WIDTH {
        return;
    }

    let [axis_area, plot_area] =
        Layout::horizontal([Constraint::Length(AXIS_WIDTH), Constraint::Min(0)]).areas(inner);

    // One candle per column keeps every session represented.
    let columns = plot_area.width as usize;
    let (candles, buckets) = aggregate(bars, columns);
    if candles.is_empty() {
        return;
    }

    // Overlays are computed on daily closes, then sampled per bucket.
    let closes = indicators::closes(bars);
    let sma = app
        .chart
        .show_sma
        .then(|| sample(&indicators::sma(&closes, 20), &buckets));
    let ema = app
        .chart
        .show_ema
        .then(|| sample(&indicators::ema(&closes, 50), &buckets));
    let bb = app.chart.show_bollinger.then(|| {
        let b = indicators::bollinger(&closes, 20, 2.0);
        (sample(&b.upper, &buckets), sample(&b.lower, &buckets))
    });
    let donchian = app.chart.show_donchian.then(|| {
        let d = indicators::donchian(bars, DONCHIAN_PERIOD);
        (
            sample(&d.upper, &buckets),
            sample(&d.middle, &buckets),
            sample(&d.lower, &buckets),
        )
    });
    // The Ichimoku spans are returned unshifted (see `indicators::ichimoku`);
    // the conventional 26-session forward projection is applied here, on the
    // *daily* series, before sampling — so the cloud lands on the right session
    // at every zoom level. The last 26 values project past the final bar and
    // have nowhere to go on an axis that ends there, so they are dropped.
    let ichimoku = app.chart.show_ichimoku.then(|| {
        let (conv, base, span_b) = ICHIMOKU;
        let i = indicators::ichimoku(bars, conv, base, span_b);
        IchimokuPlot {
            conversion: sample(&i.conversion, &buckets),
            base: sample(&i.base, &buckets),
            span_a: sample(&shift_forward(&i.span_a, base), &buckets),
            span_b: sample(&shift_forward(&i.span_b, base), &buckets),
        }
    });
    // Levels come from the visible window, so they answer "what has this scrip
    // reacted to over the range I am looking at" rather than over all history.
    let levels = if app.chart.show_levels {
        indicators::support_resistance(bars, bars.len(), LEVEL_SENSITIVITY)
    } else {
        Vec::new()
    };

    // Bound the y-axis by everything actually drawn, not just the candles.
    let mut lo = f64::MAX;
    let mut hi = f64::MIN;
    for c in &candles {
        lo = lo.min(c.low);
        hi = hi.max(c.high);
    }
    for series in [sma.as_ref(), ema.as_ref()].into_iter().flatten() {
        for v in series.iter().flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    if let Some((upper, lower)) = &bb {
        for v in upper.iter().chain(lower.iter()).flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    if let Some((upper, _, lower)) = &donchian {
        for v in upper.iter().chain(lower.iter()).flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    if let Some(i) = &ichimoku {
        for v in i.span_a.iter().chain(i.span_b.iter()).flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    let (lo, hi) = pad_bounds(lo, hi);

    draw_price_axis(f, axis_area, lo, hi);

    let candles_ref = candles.clone();
    let style = app.chart.style;
    let canvas = Canvas::default()
        .block(Block::default())
        // Braille for candles (sub-cell wick precision), half-blocks for the
        // line styles (a continuous stroke instead of a dotted trail).
        .marker(theme::marker(style.marker()))
        .x_bounds([0.0, candles_ref.len().max(1) as f64])
        .y_bounds([lo, hi])
        .paint(move |ctx| {
            // Levels first, so the candles paint over them: the price action
            // is the subject, the level is the backdrop.
            for level in &levels {
                if level.price < lo || level.price > hi {
                    continue;
                }
                let color = if level.is_support {
                    theme::UP
                } else {
                    theme::DOWN
                };
                ctx.draw(&CanvasLine {
                    x1: 0.0,
                    y1: level.price,
                    x2: candles_ref.len() as f64,
                    y2: level.price,
                    color,
                });
            }

            match style {
                ChartStyle::Candles => {
                    for (i, c) in candles_ref.iter().enumerate() {
                        let x = i as f64 + 0.5;
                        let color = if c.close >= c.open {
                            theme::UP
                        } else {
                            theme::DOWN
                        };
                        // Wick first so the body paints over it.
                        ctx.draw(&CanvasLine {
                            x1: x,
                            y1: c.low,
                            x2: x,
                            y2: c.high,
                            color,
                        });
                        let body_lo = c.open.min(c.close);
                        let body_hi = c.open.max(c.close);
                        ctx.draw(&Rectangle {
                            x: i as f64 + 0.15,
                            y: body_lo,
                            width: 0.7,
                            // A doji has zero height and would vanish entirely.
                            height: (body_hi - body_lo).max((hi - lo) * 0.002),
                            color,
                        });
                    }
                }
                ChartStyle::Area => {
                    // Fill from the floor up to each close, then stroke the
                    // top edge so the boundary stays crisp.
                    //
                    // Drawn as vertical strokes rather than rectangles:
                    // canvas rectangles are outlines, not filled shapes, so a
                    // row of them leaves a comb of gaps where the sides don't
                    // land on adjacent cells.
                    for (i, c) in candles_ref.iter().enumerate() {
                        let top = c.close.max(lo);
                        for step in 0..3 {
                            let x = i as f64 + 0.17 + step as f64 * 0.33;
                            ctx.draw(&CanvasLine {
                                x1: x,
                                y1: lo,
                                x2: x,
                                y2: top,
                                color: theme::VOLUME,
                            });
                        }
                    }
                    for (i, w) in candles_ref.windows(2).enumerate() {
                        ctx.draw(&CanvasLine {
                            x1: i as f64 + 0.5,
                            y1: w[0].close,
                            x2: i as f64 + 1.5,
                            y2: w[1].close,
                            color: theme::ACCENT,
                        });
                    }
                }
                ChartStyle::Line | ChartStyle::Dots => {
                    for (i, w) in candles_ref.windows(2).enumerate() {
                        ctx.draw(&CanvasLine {
                            x1: i as f64 + 0.5,
                            y1: w[0].close,
                            x2: i as f64 + 1.5,
                            y2: w[1].close,
                            color: theme::ACCENT,
                        });
                    }
                    // A single session has no segment to draw, so mark the
                    // point rather than rendering an empty chart.
                    if candles_ref.len() == 1 {
                        ctx.print(
                            0.5,
                            candles_ref[0].close,
                            Span::styled("•", Style::new().fg(theme::ACCENT)),
                        );
                    }
                }
            }

            for (series, color) in [(sma.as_ref(), theme::ACCENT), (ema.as_ref(), theme::WARN)] {
                if let Some(s) = series {
                    draw_overlay(ctx, s, color);
                }
            }
            if let Some((upper, lower)) = &bb {
                draw_overlay(ctx, upper, theme::VOLUME);
                draw_overlay(ctx, lower, theme::VOLUME);
            }
            if let Some((upper, middle, lower)) = &donchian {
                draw_overlay(ctx, upper, theme::UP);
                draw_overlay(ctx, lower, theme::DOWN);
                draw_overlay(ctx, middle, theme::DIM);
            }
            if let Some(i) = &ichimoku {
                // A terminal canvas cannot shade the cloud, so its two edges
                // carry the colour instead: span A above span B is a bullish
                // cloud, below it a bearish one.
                draw_overlay(ctx, &i.span_a, theme::UP);
                draw_overlay(ctx, &i.span_b, theme::DOWN);
                draw_overlay(ctx, &i.conversion, theme::ACCENT);
                draw_overlay(ctx, &i.base, theme::WARN);
            }

            // Level labels last: a price written under a candle is unreadable,
            // and the label is what makes the line actionable.
            for level in &levels {
                if level.price < lo || level.price > hi {
                    continue;
                }
                let color = if level.is_support {
                    theme::UP
                } else {
                    theme::DOWN
                };
                ctx.print(
                    0.0,
                    level.price,
                    Line::styled(theme::price(level.price), Style::new().fg(color)),
                );
            }
        });

    app.hits.borrow_mut().zone(area, Zone::Chart);
    f.render_widget(canvas, plot_area);
    draw_date_labels(f, plot_area, bars, &buckets);
}

/// The four Ichimoku lines the price canvas draws, already sampled per column
/// and with the spans' forward shift applied.
struct IchimokuPlot {
    conversion: Vec<Option<f64>>,
    base: Vec<Option<f64>>,
    span_a: Vec<Option<f64>>,
    span_b: Vec<Option<f64>>,
}

/// Delay a daily series by `n` sessions: `out[i] = series[i - n]`.
///
/// Used for the Ichimoku spans, which are computed from bar `i` but belong on
/// the x-position of bar `i + n`. Values that would land past the end of the
/// series are dropped — they describe sessions that have not happened yet, and
/// the chart's x-axis stops at the last one that has.
fn shift_forward(series: &[Option<f64>], n: usize) -> Vec<Option<f64>> {
    let len = series.len();
    let mut out = vec![None; len];
    if n < len {
        out[n..].copy_from_slice(&series[..len - n]);
    }
    out
}

/// Connect consecutive defined points of an overlay series.
fn draw_overlay(ctx: &mut ratatui::widgets::canvas::Context, series: &[Option<f64>], color: Color) {
    for i in 1..series.len() {
        if let (Some(a), Some(b)) = (series[i - 1], series[i]) {
            ctx.draw(&CanvasLine {
                x1: i as f64 - 0.5,
                y1: a,
                x2: i as f64 + 0.5,
                y2: b,
                color,
            });
        }
    }
}

/// Price gridline labels down the left gutter.
///
/// Labelling every row turns the gutter into a wall of numbers, so ticks are
/// spaced to land roughly every four rows — enough to read a level off the
/// chart, sparse enough to stay quiet.
fn draw_price_axis(f: &mut Frame, area: Rect, lo: f64, hi: f64) {
    let rows = area.height;
    if rows == 0 {
        return;
    }
    let step = ((rows as usize / 6).max(1)).min(rows as usize);

    let lines: Vec<Line> = (0..rows)
        .map(|r| {
            // Always label the top and bottom so the full range is explicit.
            let is_tick = (r as usize).is_multiple_of(step) || r == rows - 1;
            if !is_tick {
                return Line::raw("");
            }
            // Row 0 is the top of the plot, which is the highest price.
            let t = if rows > 1 {
                1.0 - r as f64 / (rows - 1) as f64
            } else {
                1.0
            };
            let value = lo + (hi - lo) * t;
            Line::from(Span::styled(
                format!(
                    "{:>width$} ",
                    theme::price(value),
                    width = AXIS_WIDTH as usize - 1
                ),
                theme::label_style(),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Date labels along the bottom of the plot, spaced to avoid collisions.
fn draw_date_labels(f: &mut Frame, area: Rect, bars: &[Bar], buckets: &[Bucket]) {
    if area.height < 2 || buckets.is_empty() {
        return;
    }
    let row = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };

    // "YYYY-MM-DD" plus breathing room, so labels never abut.
    const LABEL_WIDTH: usize = 10;
    const GAP: usize = 3;

    let width = area.width as usize;
    let slots = (width / (LABEL_WIDTH + GAP)).max(1);
    let mut spans = Vec::new();
    let mut used = 0usize;

    for slot in 0..slots {
        let bucket_idx = slot * buckets.len() / slots;
        let Some(b) = buckets.get(bucket_idx) else {
            break;
        };
        let Some(bar) = bars.get(b.end.saturating_sub(1)) else {
            break;
        };

        // Buckets are stretched across the plot, so a bucket index is only a
        // screen column after scaling by the plot width.
        let column = bucket_idx * width / buckets.len().max(1);
        if column > used {
            spans.push(Span::raw(" ".repeat(column - used)));
            used = column;
        }

        let text = trading_day(bar.ts);
        if used + text.len() > width {
            break;
        }
        used += text.len();
        spans.push(Span::styled(text, theme::label_style()));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), row);
}

fn draw_pane(f: &mut Frame, area: Rect, app: &App, bars: &[Bar]) {
    let block = widgets::panel(app.chart.pane.label());
    let inner = block.inner(area);
    f.render_widget(block, area);

    // The panel title doubles as the pane selector, like the `i` key.
    if area.width > 2 {
        let w = app.chart.pane.label().chars().count() as u16 + 2;
        app.hits.borrow_mut().target(
            Rect {
                x: area.x + 1,
                y: area.y,
                width: w.min(area.width - 1),
                height: 1,
            },
            Target::ChartPane,
        );
    }
    if inner.height == 0 || inner.width <= AXIS_WIDTH {
        return;
    }

    let [axis_area, plot_area] =
        Layout::horizontal([Constraint::Length(AXIS_WIDTH), Constraint::Min(0)]).areas(inner);

    let columns = plot_area.width as usize;
    let (_, buckets) = aggregate(bars, columns);
    if buckets.is_empty() {
        return;
    }
    let closes = indicators::closes(bars);

    // Each pane resolves to a set of series plus explicit y-bounds.
    let (series, bounds, kind) = match app.chart.pane {
        Pane::Volume => {
            let vols: Vec<Option<f64>> = buckets
                .iter()
                .map(|b| Some(bars[b.start..b.end].iter().map(|x| x.volume).sum::<f64>()))
                .collect();
            let max = vols.iter().flatten().cloned().fold(0.0, f64::max);
            (
                vec![(vols, theme::VOLUME)],
                (0.0, if max > 0.0 { max } else { 1.0 }),
                PaneKind::Bars,
            )
        }
        Pane::Rsi => (
            vec![(
                sample(&indicators::rsi(&closes, 14), &buckets),
                theme::ACCENT,
            )],
            (0.0, 100.0),
            PaneKind::Lines(vec![30.0, 70.0]),
        ),
        Pane::Macd => {
            let m = indicators::macd(&closes, 12, 26, 9);
            let macd_s = sample(&m.macd, &buckets);
            let signal_s = sample(&m.signal, &buckets);
            let hist_s = sample(&m.histogram, &buckets);
            let bounds = symmetric_bounds(
                macd_s
                    .iter()
                    .chain(signal_s.iter())
                    .chain(hist_s.iter())
                    .flatten()
                    .cloned(),
            );
            (
                vec![
                    (hist_s, theme::VOLUME),
                    (macd_s, theme::ACCENT),
                    (signal_s, theme::WARN),
                ],
                bounds,
                PaneKind::Lines(vec![0.0]),
            )
        }
        Pane::Atr => {
            let a = sample(&indicators::atr(bars, 14), &buckets);
            let max = a.iter().flatten().cloned().fold(0.0, f64::max);
            (
                vec![(a, theme::WARN)],
                (0.0, if max > 0.0 { max } else { 1.0 }),
                PaneKind::Line,
            )
        }
        Pane::Stochastic => {
            let s = indicators::stochastic(bars, 14, 3);
            (
                vec![
                    (sample(&s.k, &buckets), theme::ACCENT),
                    (sample(&s.d, &buckets), theme::WARN),
                ],
                (0.0, 100.0),
                PaneKind::Lines(vec![20.0, 80.0]),
            )
        }
        Pane::Adx => {
            let a = indicators::adx(bars, 14);
            (
                vec![
                    (sample(&a.adx, &buckets), theme::ACCENT),
                    (sample(&a.plus_di, &buckets), theme::UP),
                    (sample(&a.minus_di, &buckets), theme::DOWN),
                ],
                (0.0, 100.0),
                // 25 is the conventional line between trending and ranging.
                PaneKind::Lines(vec![25.0]),
            )
        }
        Pane::Cci => {
            let c = sample(&indicators::cci(bars, 20), &buckets);
            // CCI is unbounded, so the scale follows the data — but never
            // tighter than ±150, or the ±100 reference lines would sit on the
            // edge of the pane and stop reading as thresholds.
            let (_, mut hi) = symmetric_bounds(c.iter().flatten().cloned());
            hi = hi.max(150.0);
            (
                vec![(c, theme::ACCENT)],
                (-hi, hi),
                PaneKind::Lines(vec![-100.0, 0.0, 100.0]),
            )
        }
        Pane::WilliamsR => (
            vec![(
                sample(&indicators::williams_r(bars, 14), &buckets),
                theme::ACCENT,
            )],
            // %R is inverted: 0 is the top of the range, -100 the bottom.
            (-100.0, 0.0),
            PaneKind::Lines(vec![-20.0, -80.0]),
        ),
    };

    draw_price_axis(f, axis_area, bounds.0, bounds.1);

    let n = buckets.len().max(1);
    let canvas = Canvas::default()
        .marker(theme::marker(symbols::Marker::Braille))
        .x_bounds([0.0, n as f64])
        .y_bounds([bounds.0, bounds.1])
        .paint(move |ctx| {
            if let PaneKind::Lines(levels) = &kind {
                for level in levels {
                    ctx.draw(&CanvasLine {
                        x1: 0.0,
                        y1: *level,
                        x2: n as f64,
                        y2: *level,
                        color: theme::BORDER,
                    });
                }
            }

            for (s, color) in &series {
                match kind {
                    PaneKind::Bars => {
                        for (i, v) in s.iter().enumerate() {
                            if let Some(v) = v {
                                ctx.draw(&Rectangle {
                                    x: i as f64 + 0.15,
                                    y: 0.0,
                                    width: 0.7,
                                    height: *v,
                                    color: *color,
                                });
                            }
                        }
                    }
                    _ => draw_overlay(ctx, s, *color),
                }
            }
        });

    f.render_widget(canvas, plot_area);
}

enum PaneKind {
    Bars,
    Line,
    /// Line series plus horizontal reference levels (e.g. RSI 30/70).
    Lines(Vec<f64>),
}

/// Which daily bars collapsed into one drawn column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub start: usize,
    pub end: usize,
}

/// Aggregate `bars` into at most `columns` OHLC candles.
///
/// Open is the first open, close the last close, high/low the extremes and
/// volume the sum — the standard way to roll bars up to a coarser interval, so
/// a zoomed-out chart still tells the truth about the range.
pub fn aggregate(bars: &[Bar], columns: usize) -> (Vec<Bar>, Vec<Bucket>) {
    if bars.is_empty() || columns == 0 {
        return (Vec::new(), Vec::new());
    }
    if bars.len() <= columns {
        let buckets = (0..bars.len())
            .map(|i| Bucket {
                start: i,
                end: i + 1,
            })
            .collect();
        return (bars.to_vec(), buckets);
    }

    let per = bars.len() as f64 / columns as f64;
    let mut candles = Vec::with_capacity(columns);
    let mut buckets = Vec::with_capacity(columns);

    for i in 0..columns {
        let start = (i as f64 * per).floor() as usize;
        let end = ((((i + 1) as f64) * per).ceil() as usize)
            .min(bars.len())
            .max(start + 1);
        let slice = &bars[start..end];

        let high = slice.iter().map(|b| b.high).fold(f64::MIN, f64::max);
        let low = slice.iter().map(|b| b.low).fold(f64::MAX, f64::min);
        candles.push(Bar {
            ts: slice[slice.len() - 1].ts,
            open: slice[0].open,
            high,
            low,
            close: slice[slice.len() - 1].close,
            volume: slice.iter().map(|b| b.volume).sum(),
        });
        buckets.push(Bucket { start, end });
    }

    (candles, buckets)
}

/// Sample a daily-resolution indicator at each bucket's last session, so the
/// overlay lines up with the candle drawn for that column.
pub fn sample(series: &[Option<f64>], buckets: &[Bucket]) -> Vec<Option<f64>> {
    buckets
        .iter()
        .map(|b| series.get(b.end.saturating_sub(1)).copied().flatten())
        .collect()
}

/// Widen a degenerate range so a flat series still renders.
fn pad_bounds(lo: f64, hi: f64) -> (f64, f64) {
    if !lo.is_finite() || !hi.is_finite() {
        return (0.0, 1.0);
    }
    if (hi - lo).abs() < f64::EPSILON {
        let pad = if lo.abs() > 0.0 { lo.abs() * 0.01 } else { 1.0 };
        return (lo - pad, hi + pad);
    }
    let pad = (hi - lo) * 0.05;
    (lo - pad, hi + pad)
}

/// Bounds centred on zero, for oscillators that swing both ways.
fn symmetric_bounds(values: impl Iterator<Item = f64>) -> (f64, f64) {
    let max = values.fold(0.0f64, |acc, v| acc.max(v.abs()));
    if max <= 0.0 { (-1.0, 1.0) } else { (-max, max) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(ts: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> Bar {
        Bar {
            ts,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    fn series(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                let f = i as f64;
                bar(i as i64 * 86_400, f, f + 2.0, f - 1.0, f + 1.0, 100.0)
            })
            .collect()
    }

    #[test]
    fn short_series_maps_one_bar_per_column() {
        let bars = series(5);
        let (candles, buckets) = aggregate(&bars, 20);
        assert_eq!(candles.len(), 5);
        assert_eq!(buckets[0], Bucket { start: 0, end: 1 });
        assert_eq!(candles[4].close, bars[4].close);
    }

    #[test]
    fn aggregation_preserves_ohlc_semantics() {
        let bars = series(100);
        let (candles, buckets) = aggregate(&bars, 10);
        assert_eq!(candles.len(), 10);

        // First candle must span the first bucket faithfully.
        let b = buckets[0];
        let slice = &bars[b.start..b.end];
        assert_eq!(candles[0].open, slice[0].open, "open = first open");
        assert_eq!(
            candles[0].close,
            slice[slice.len() - 1].close,
            "close = last close"
        );
        assert_eq!(
            candles[0].high,
            slice.iter().map(|x| x.high).fold(f64::MIN, f64::max),
            "high = max high"
        );
        assert_eq!(
            candles[0].low,
            slice.iter().map(|x| x.low).fold(f64::MAX, f64::min),
            "low = min low"
        );
        assert_eq!(
            candles[0].volume,
            slice.iter().map(|x| x.volume).sum::<f64>(),
            "volume = sum"
        );
    }

    #[test]
    fn aggregation_covers_every_bar_without_gaps_or_overlap() {
        let bars = series(250);
        let (_, buckets) = aggregate(&bars, 37);
        assert_eq!(buckets[0].start, 0);
        assert_eq!(
            buckets.last().unwrap().end,
            250,
            "last bar must be included"
        );
        for w in buckets.windows(2) {
            assert!(w[0].end <= w[1].end);
            assert!(w[0].start < w[1].start, "buckets must advance");
        }
    }

    #[test]
    fn aggregation_never_produces_an_empty_bucket() {
        for columns in [1, 3, 7, 60, 199] {
            let (candles, buckets) = aggregate(&series(200), columns);
            assert_eq!(candles.len(), buckets.len());
            for b in &buckets {
                assert!(b.end > b.start, "empty bucket at columns={columns}");
            }
        }
    }

    #[test]
    fn empty_input_is_safe() {
        assert_eq!(aggregate(&[], 10).0.len(), 0);
        assert_eq!(aggregate(&series(10), 0).0.len(), 0);
    }

    #[test]
    fn sampling_takes_the_last_session_in_each_bucket() {
        let s: Vec<Option<f64>> = (0..10).map(|i| Some(i as f64)).collect();
        let buckets = vec![Bucket { start: 0, end: 5 }, Bucket { start: 5, end: 10 }];
        assert_eq!(sample(&s, &buckets), vec![Some(4.0), Some(9.0)]);
    }

    #[test]
    fn sampling_propagates_warm_up_gaps() {
        // Indicator undefined for the first three sessions.
        let s = vec![None, None, None, Some(1.0), Some(2.0)];
        let buckets = vec![Bucket { start: 0, end: 2 }, Bucket { start: 2, end: 5 }];
        assert_eq!(sample(&s, &buckets), vec![None, Some(2.0)]);
    }

    #[test]
    fn sampling_out_of_range_buckets_yields_none() {
        let s = vec![Some(1.0)];
        let buckets = vec![Bucket { start: 0, end: 99 }];
        assert_eq!(sample(&s, &buckets), vec![None]);
    }

    #[test]
    fn flat_series_gets_a_renderable_range() {
        let (lo, hi) = pad_bounds(100.0, 100.0);
        assert!(hi > lo, "a flat series must still have height");

        let (lo, hi) = pad_bounds(f64::NAN, f64::NAN);
        assert!(lo.is_finite() && hi.is_finite());
    }

    #[test]
    fn symmetric_bounds_centre_on_zero() {
        assert_eq!(symmetric_bounds([-3.0, 1.0].into_iter()), (-3.0, 3.0));
        assert_eq!(symmetric_bounds([].into_iter()), (-1.0, 1.0));
    }

    #[test]
    fn forward_shift_delays_a_series_and_drops_what_runs_off_the_end() {
        let s: Vec<Option<f64>> = (0..5).map(|i| Some(i as f64)).collect();
        assert_eq!(
            shift_forward(&s, 2),
            vec![None, None, Some(0.0), Some(1.0), Some(2.0)],
            "the last two values project past the axis and are dropped"
        );
        assert_eq!(shift_forward(&s, 0), s, "a zero shift is the identity");
        assert_eq!(
            shift_forward(&s, 9),
            vec![None; 5],
            "a shift longer than the series leaves nothing to draw"
        );
        assert!(shift_forward(&[], 3).is_empty());
    }

    /// Every pane must draw at every terminal size with every overlay on.
    #[test]
    fn every_pane_and_overlay_renders_without_panicking() {
        use crate::app::{App, DataEvent};
        use crate::cache::Store;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::sync::Arc;

        let (tx, _rx) = crate::app::detached_channel();
        let mut app = App::new(Arc::new(Store::open_in_memory().unwrap()), tx);
        app.selected = "HBL".into();
        app.chart.show_donchian = true;
        app.chart.show_ichimoku = true;
        app.chart.show_levels = true;

        // Enough history for Ichimoku's 52-session span, and a flat series
        // too — a limit-locked scrip is the case that divides by zero.
        for bars in [
            series(400),
            (0..80)
                .map(|i| bar(i as i64 * 86_400, 5.0, 5.0, 5.0, 5.0, 0.0))
                .collect(),
            series(3),
        ] {
            app.bars.clear();
            app.on_event(DataEvent::Bars {
                symbol: "HBL".into(),
                bars,
            });
            for pane in crate::app::Pane::ALL {
                app.chart.pane = pane;
                for (w, h) in [(20u16, 10u16), (80, 24), (200, 60), (5, 3)] {
                    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                    term.draw(|f| draw(f, f.area(), &app)).unwrap();
                }
            }
        }
    }

    #[test]
    fn the_header_lists_the_new_overlays() {
        use crate::app::{App, DataEvent};
        use crate::cache::Store;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::sync::Arc;

        let (tx, _rx) = crate::app::detached_channel();
        let mut app = App::new(Arc::new(Store::open_in_memory().unwrap()), tx);
        app.selected = "HBL".into();
        app.on_event(DataEvent::Bars {
            symbol: "HBL".into(),
            bars: series(200),
        });

        let mut term = Terminal::new(TestBackend::new(160, 30)).unwrap();
        term.draw(|f| draw(f, f.area(), &app)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        for label in ["DC20", "ICHI", "S/R"] {
            assert!(text.contains(label), "{label} missing from the header");
        }
    }

    #[test]
    fn renders_without_panicking_at_extreme_sizes() {
        use crate::app::{App, DataEvent};
        use crate::cache::Store;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::sync::Arc;

        let (tx, _rx) = crate::app::detached_channel();
        let mut app = App::new(Arc::new(Store::open_in_memory().unwrap()), tx);
        app.selected = "HBL".into();
        app.on_event(DataEvent::Bars {
            symbol: "HBL".into(),
            bars: series(400),
        });

        for (w, h) in [(20u16, 10u16), (80, 24), (200, 60), (5, 3)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| draw(f, f.area(), &app)).unwrap();
        }
    }

    #[test]
    fn renders_with_no_data_without_panicking() {
        use crate::app::App;
        use crate::cache::Store;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::sync::Arc;

        let (tx, _rx) = crate::app::detached_channel();
        let app = App::new(Arc::new(Store::open_in_memory().unwrap()), tx);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, f.area(), &app)).unwrap();
    }
}
