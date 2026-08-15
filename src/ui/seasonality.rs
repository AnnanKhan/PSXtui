//! Seasonality and return distribution for the selected symbol.
//!
//! Everything here is derived from `App::bars` — the full cached daily history
//! — and asks the questions a chart cannot answer: *when* does this scrip make
//! its money, how fat are its tails, and how long do its runs last.
//!
//! Calendar buckets use Pakistan Standard Time ([`crate::cache::pkt`]) rather
//! than UTC. The EOD feed stamps a bar at 16:00 PKT, which is still the
//! previous day in UTC for part of the year — bucketing on the raw timestamp
//! would push January sessions into December.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Utc, Weekday};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use super::{theme, widgets};
use crate::app::App;
use crate::cache::pkt;
use crate::model::Bar;

/// Below this many sessions the monthly grid is mostly holes and the moments of
/// the distribution are dominated by a handful of prints.
const MIN_BARS: usize = 60;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// PSX trades Monday to Friday; there is no weekend session to report on.
const WEEKDAYS: [&str; 5] = ["Mon", "Tue", "Wed", "Thu", "Fri"];

// --- calendar bucketing ---------------------------------------------------

/// The PKT calendar year and month (1-12) a bar belongs to.
fn year_month(ts: i64) -> (i32, u32) {
    let dt = DateTime::<Utc>::from_timestamp(ts, 0)
        .unwrap_or_default()
        .with_timezone(&pkt());
    (dt.year(), dt.month())
}

/// Weekday index, Monday = 0. `None` for a weekend print.
fn weekday_index(ts: i64) -> Option<usize> {
    let dt = DateTime::<Utc>::from_timestamp(ts, 0)
        .unwrap_or_default()
        .with_timezone(&pkt());
    match dt.weekday() {
        Weekday::Mon => Some(0),
        Weekday::Tue => Some(1),
        Weekday::Wed => Some(2),
        Weekday::Thu => Some(3),
        Weekday::Fri => Some(4),
        _ => None,
    }
}

/// One calendar month's return, in percent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonthCell {
    pub year: i32,
    /// 1-12.
    pub month: u32,
    pub pct: f64,
}

/// Month-by-month returns, oldest first.
///
/// A month's return is measured close-to-close from the previous month the
/// scrip actually traded in, so the gap over a month-end (which is where a good
/// deal of the move often is) is not thrown away. The very first month in the
/// history has no predecessor and is measured from its own first close — which
/// is why a symbol listed mid-year reports a partial first month rather than
/// nothing at all.
pub fn month_returns(bars: &[Bar]) -> Vec<MonthCell> {
    struct Acc {
        first_ts: i64,
        first: f64,
        last_ts: i64,
        last: f64,
    }

    // A BTreeMap keyed by (year, month) both groups and sorts, so an unsorted
    // or duplicated history still buckets correctly.
    let mut buckets: BTreeMap<(i32, u32), Acc> = BTreeMap::new();
    for b in bars {
        if !b.close.is_finite() || b.close <= 0.0 {
            continue;
        }
        let key = year_month(b.ts);
        buckets
            .entry(key)
            .and_modify(|a| {
                if b.ts < a.first_ts {
                    a.first_ts = b.ts;
                    a.first = b.close;
                }
                if b.ts >= a.last_ts {
                    a.last_ts = b.ts;
                    a.last = b.close;
                }
            })
            .or_insert(Acc {
                first_ts: b.ts,
                first: b.close,
                last_ts: b.ts,
                last: b.close,
            });
    }

    let mut out = Vec::with_capacity(buckets.len());
    let mut prev_close: Option<f64> = None;
    for ((year, month), acc) in buckets {
        let base = prev_close.unwrap_or(acc.first);
        let pct = if base > 0.0 && base.is_finite() {
            (acc.last / base - 1.0) * 100.0
        } else {
            0.0
        };
        out.push(MonthCell {
            year,
            month,
            pct: if pct.is_finite() { pct } else { 0.0 },
        });
        prev_close = Some(acc.last);
    }
    out
}

/// Average return per calendar month across every year observed.
pub fn month_averages(cells: &[MonthCell]) -> [Option<f64>; 12] {
    let mut sums = [0.0f64; 12];
    let mut counts = [0usize; 12];
    for c in cells {
        if let Some(i) = (c.month as usize).checked_sub(1)
            && i < 12
            && c.pct.is_finite()
        {
            sums[i] += c.pct;
            counts[i] += 1;
        }
    }
    std::array::from_fn(|i| (counts[i] > 0).then(|| sums[i] / counts[i] as f64))
}

// --- daily returns --------------------------------------------------------

/// One session's return, in percent, tagged with its trading day.
#[derive(Debug, Clone, PartialEq)]
pub struct DailyReturn {
    pub ts: i64,
    pub pct: f64,
}

/// Close-to-close percentage returns, attributed to the *later* session.
pub fn daily_returns(bars: &[Bar]) -> Vec<DailyReturn> {
    bars.windows(2)
        .filter_map(|w| {
            let (a, b) = (w[0].close, w[1].close);
            if !a.is_finite() || !b.is_finite() || a <= 0.0 {
                return None;
            }
            let pct = (b / a - 1.0) * 100.0;
            pct.is_finite().then_some(DailyReturn { ts: w[1].ts, pct })
        })
        .collect()
}

/// Average return and hit rate for one weekday.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DowStat {
    pub avg_pct: f64,
    /// Share of sessions that closed up, in percent.
    pub win_rate: f64,
    pub count: usize,
}

/// Monday-to-Friday averages. Weekend prints, which PSX does not produce, are
/// ignored rather than folded into Monday.
pub fn day_of_week(returns: &[DailyReturn]) -> [DowStat; 5] {
    let mut sums = [0.0f64; 5];
    let mut wins = [0usize; 5];
    let mut counts = [0usize; 5];

    for r in returns {
        let Some(i) = weekday_index(r.ts) else {
            continue;
        };
        sums[i] += r.pct;
        counts[i] += 1;
        if r.pct > 0.0 {
            wins[i] += 1;
        }
    }

    std::array::from_fn(|i| {
        if counts[i] == 0 {
            DowStat::default()
        } else {
            DowStat {
                avg_pct: sums[i] / counts[i] as f64,
                win_rate: wins[i] as f64 / counts[i] as f64 * 100.0,
                count: counts[i],
            }
        }
    })
}

// --- distribution ---------------------------------------------------------

/// Shape of the daily return distribution.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Dist {
    pub n: usize,
    pub mean: f64,
    pub median: f64,
    pub sd: f64,
    /// Fisher skewness. Negative means the left tail is the long one.
    pub skew: f64,
    /// Excess kurtosis: 0 is Gaussian, positive means fat tails.
    pub kurtosis: f64,
    pub up: usize,
    pub down: usize,
    pub flat: usize,
}

/// Moments and up/down counts of a return series.
///
/// Skew and kurtosis are undefined without dispersion, so a scrip that never
/// moved reports zero for both rather than a division by zero.
pub fn distribution(values: &[f64]) -> Dist {
    let clean: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if clean.is_empty() {
        return Dist::default();
    }

    let n = clean.len();
    let mean = clean.iter().sum::<f64>() / n as f64;

    let mut sorted = clean.clone();
    sorted.sort_by(f64::total_cmp);
    let median = if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    };

    let sd = crate::analysis::stats::std_dev(&clean);
    let (skew, kurtosis) = if sd > 0.0 {
        let m3: f64 = clean.iter().map(|v| ((v - mean) / sd).powi(3)).sum::<f64>() / n as f64;
        let m4: f64 = clean.iter().map(|v| ((v - mean) / sd).powi(4)).sum::<f64>() / n as f64;
        (m3, m4 - 3.0)
    } else {
        (0.0, 0.0)
    };

    Dist {
        n,
        mean,
        median,
        sd,
        skew: if skew.is_finite() { skew } else { 0.0 },
        kurtosis: if kurtosis.is_finite() { kurtosis } else { 0.0 },
        up: clean.iter().filter(|v| **v > 0.0).count(),
        down: clean.iter().filter(|v| **v < 0.0).count(),
        flat: clean.iter().filter(|v| **v == 0.0).count(),
    }
}

/// One histogram bucket: `[lo, hi)` and how many sessions landed in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bin {
    pub lo: f64,
    pub hi: f64,
    pub count: usize,
}

/// Bucket returns into `bins` equal-width intervals spanning the observed range.
///
/// A degenerate range — every session identical, or a single observation —
/// collapses to one bin holding everything, which is the honest picture.
pub fn histogram(values: &[f64], bins: usize) -> Vec<Bin> {
    let clean: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if clean.is_empty() || bins == 0 {
        return Vec::new();
    }

    let lo = clean.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = clean.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !lo.is_finite() || !hi.is_finite() || hi - lo <= f64::EPSILON {
        return vec![Bin {
            lo,
            hi,
            count: clean.len(),
        }];
    }

    let width = (hi - lo) / bins as f64;
    let mut out: Vec<Bin> = (0..bins)
        .map(|i| Bin {
            lo: lo + width * i as f64,
            hi: lo + width * (i + 1) as f64,
            count: 0,
        })
        .collect();

    for v in clean {
        let idx = (((v - lo) / width).floor() as isize).clamp(0, bins as isize - 1) as usize;
        out[idx].count += 1;
    }
    out
}

// --- streaks --------------------------------------------------------------

/// Longest and current runs of consecutive up / down sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Streaks {
    pub longest_up: usize,
    pub longest_down: usize,
    /// The run in progress: positive for up sessions, negative for down, zero
    /// when the last session was unchanged.
    pub current: i32,
}

/// Count runs of same-signed sessions. Unchanged sessions break a run without
/// starting one — a limit-locked, untraded day is not a direction.
pub fn streaks(values: &[f64]) -> Streaks {
    let mut s = Streaks::default();
    let mut run_up = 0usize;
    let mut run_down = 0usize;

    for v in values {
        if !v.is_finite() || *v == 0.0 {
            run_up = 0;
            run_down = 0;
            s.current = 0;
            continue;
        }
        if *v > 0.0 {
            run_up += 1;
            run_down = 0;
            s.longest_up = s.longest_up.max(run_up);
            s.current = run_up as i32;
        } else {
            run_down += 1;
            run_up = 0;
            s.longest_down = s.longest_down.max(run_down);
            s.current = -(run_down as i32);
        }
    }
    s
}

// --- rendering ------------------------------------------------------------

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
        notice(f, area, "Seasonality", &msg);
        return;
    }

    let returns = daily_returns(&app.bars);
    let values: Vec<f64> = returns.iter().map(|r| r.pct).collect();
    let cells = month_returns(&app.bars);

    // The grid takes exactly the rows its years need — a header, one row per
    // year, the average row and the border — and the distribution panels get
    // everything left over rather than staring at a half-empty table.
    let years = cells
        .iter()
        .map(|c| c.year)
        .collect::<std::collections::BTreeSet<_>>()
        .len() as u16;
    // `clamp` would panic on a terminal shorter than the minimum, so the floor
    // and the ceiling are applied in order instead.
    // The grid needs one row per year plus header, average row and borders —
    // and no more. Giving it the leftover space instead left a block of dead
    // rows under the table on a tall terminal; the distribution histogram
    // below puts that height to better use.
    let grid_h = (years + 4).max(5).min(area.height);
    let show_lower = area.height.saturating_sub(grid_h) >= 8;

    let [grid_area, lower] = Layout::vertical([
        Constraint::Length(if show_lower { grid_h } else { area.height }),
        Constraint::Min(0),
    ])
    .areas(area);

    draw_grid(f, grid_area, app, &cells);

    if !show_lower || lower.height == 0 {
        return;
    }
    let [dow_area, dist_area, tail_area] = Layout::horizontal([
        Constraint::Percentage(26),
        Constraint::Percentage(42),
        Constraint::Percentage(32),
    ])
    .areas(lower);

    draw_day_of_week(f, dow_area, &returns);
    draw_distribution(f, dist_area, &values);
    draw_tails(f, tail_area, &returns, &values);
}

fn notice(f: &mut Frame, area: Rect, title: &str, msg: &str) {
    let block = widgets::panel(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let msg = theme::truncate(msg, inner.width as usize);
    let pad = (inner.height.saturating_sub(1) / 2) as usize;
    let mut lines: Vec<Line> = vec![Line::raw(""); pad];
    lines.push(widgets::placeholder(&msg));
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_grid(f: &mut Frame, area: Rect, app: &App, cells: &[MonthCell]) {
    let title = format!("{} — Monthly Returns", app.selected);
    let block = widgets::panel(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if cells.is_empty() {
        f.render_widget(
            Paragraph::new(widgets::placeholder("No monthly data")),
            inner,
        );
        return;
    }

    const YEAR_W: usize = 5;
    let width = inner.width as usize;
    // Fall back to a narrow, one-line-per-year summary before dropping months.
    let cell = if width >= YEAR_W + 12 * 6 {
        6
    } else if width >= YEAR_W + 12 * 4 {
        4
    } else {
        0
    };

    // Saturate the heat at the largest move on screen, floored so a quiet
    // history is not painted as if every month were dramatic.
    let scale = cells
        .iter()
        .map(|c| c.pct.abs())
        .fold(0.0f64, f64::max)
        .clamp(5.0, 30.0);

    if cell == 0 {
        draw_grid_narrow(f, inner, cells, scale);
        return;
    }

    let mut header = vec![Span::styled(
        format!("{:<YEAR_W$}", "Year"),
        theme::header_style(),
    )];
    for m in MONTHS {
        header.push(Span::styled(
            format!("{m:>w$}", w = cell),
            theme::header_style(),
        ));
    }
    let mut lines = vec![Line::from(header)];

    let mut by_year: BTreeMap<i32, [Option<f64>; 12]> = BTreeMap::new();
    for c in cells {
        let row = by_year.entry(c.year).or_insert([None; 12]);
        if (1..=12).contains(&c.month) {
            row[c.month as usize - 1] = Some(c.pct);
        }
    }

    // Newest year first: the recent past is what gets read.
    for (year, row) in by_year.iter().rev() {
        let mut spans = vec![Span::styled(
            format!("{year:<YEAR_W$}"),
            theme::value_style(),
        )];
        for value in row.iter() {
            spans.push(heat_cell(*value, cell, scale));
        }
        lines.push(Line::from(spans));
    }

    let avg = month_averages(cells);
    let mut spans = vec![Span::styled(
        format!("{:<YEAR_W$}", "Avg"),
        theme::header_style(),
    )];
    for value in avg.iter() {
        spans.push(heat_cell(*value, cell, scale));
    }
    lines.push(Line::from(spans));

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// One heat-shaded month cell, right-aligned in `width` columns.
fn heat_cell(value: Option<f64>, width: usize, scale: f64) -> Span<'static> {
    let Some(v) = value.filter(|v| v.is_finite()) else {
        return Span::styled(format!("{:>width$}", "·"), theme::label_style());
    };
    let text = if width >= 6 {
        format!("{v:+.1}")
    } else {
        format!("{v:+.0}")
    };
    Span::styled(
        format!(
            "{:>width$}",
            theme::truncate(&text, width.saturating_sub(1))
        ),
        Style::new()
            .bg(widgets::heat_color(v, scale))
            .fg(theme::fg()),
    )
}

/// Year-by-year summary for panes too narrow for twelve columns.
fn draw_grid_narrow(f: &mut Frame, area: Rect, cells: &[MonthCell], scale: f64) {
    let mut by_year: BTreeMap<i32, f64> = BTreeMap::new();
    for c in cells {
        // Compounding the months gives the year's actual return.
        let e = by_year.entry(c.year).or_insert(1.0);
        *e *= 1.0 + c.pct / 100.0;
    }

    let lines: Vec<Line> = by_year
        .iter()
        .rev()
        .map(|(year, growth)| {
            let pct = (growth - 1.0) * 100.0;
            Line::from(vec![
                Span::styled(format!("{year} "), theme::label_style()),
                Span::styled(
                    format!("{:>8}", theme::pct(pct)),
                    Style::new()
                        .bg(widgets::heat_color(pct, scale * 3.0))
                        .fg(theme::fg()),
                ),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn draw_day_of_week(f: &mut Frame, area: Rect, returns: &[DailyReturn]) {
    let block = widgets::panel("Day of Week");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let stats = day_of_week(returns);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!(" {:<4}", "Day"), theme::header_style()),
        Span::styled(format!("{:>8}", "Avg"), theme::header_style()),
        Span::styled(format!("{:>7}", "Win"), theme::header_style()),
        Span::styled(format!("{:>6}", "N"), theme::header_style()),
    ])];

    for (i, name) in WEEKDAYS.iter().enumerate() {
        let s = stats[i];
        if s.count == 0 {
            lines.push(Line::from(vec![
                Span::styled(format!(" {name:<4}"), theme::label_style()),
                Span::styled(format!("{:>8}", "—"), theme::label_style()),
            ]));
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(format!(" {name:<4}"), theme::label_style()),
            Span::styled(
                format!("{:>8}", theme::pct(s.avg_pct)),
                Style::new().fg(theme::change_color(s.avg_pct)),
            ),
            Span::styled(
                format!("{:>7}", theme::pct_plain(s.win_rate)),
                theme::value_style(),
            ),
            Span::styled(format!("{:>6}", s.count), theme::label_style()),
        ]));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_distribution(f: &mut Frame, area: Rect, values: &[f64]) {
    let block = widgets::panel("Daily Return Distribution");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let d = distribution(values);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(" mean ", theme::label_style()),
            Span::styled(
                theme::pct(d.mean),
                Style::new().fg(theme::change_color(d.mean)),
            ),
            Span::styled("  med ", theme::label_style()),
            Span::styled(
                theme::pct(d.median),
                Style::new().fg(theme::change_color(d.median)),
            ),
            Span::styled("  sd ", theme::label_style()),
            Span::styled(theme::pct_plain(d.sd), theme::value_style()),
        ]),
        Line::from(vec![
            Span::styled(" skew ", theme::label_style()),
            Span::styled(theme::opt(Some(d.skew), 2), theme::value_style()),
            Span::styled("  kurt ", theme::label_style()),
            Span::styled(theme::opt(Some(d.kurtosis), 2), theme::value_style()),
            Span::styled("  up ", theme::label_style()),
            Span::styled(format!("{}", d.up), Style::new().fg(theme::up())),
            Span::styled(" / dn ", theme::label_style()),
            Span::styled(format!("{}", d.down), Style::new().fg(theme::down())),
        ]),
    ];

    // Whatever rows are left after the two stat lines become the histogram.
    let rows = inner.height.saturating_sub(lines.len() as u16) as usize;
    if rows >= 3 {
        let bins = histogram(values, rows.min(11));
        let peak = bins.iter().map(|b| b.count).max().unwrap_or(0);
        let bar_w = (inner.width as usize).saturating_sub(20);
        for b in &bins {
            let ratio = if peak > 0 {
                b.count as f64 / peak as f64
            } else {
                0.0
            };
            let mid = (b.lo + b.hi) / 2.0;
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {:>7}", theme::pct(b.lo)),
                    Style::new().fg(theme::change_color(mid)),
                ),
                Span::raw(" "),
                Span::styled(
                    widgets::bar(ratio, bar_w),
                    Style::new().fg(theme::change_color(mid)),
                ),
                Span::styled(format!(" {:>4}", b.count), theme::label_style()),
            ]));
        }
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_tails(f: &mut Frame, area: Rect, returns: &[DailyReturn], values: &[f64]) {
    let block = widgets::panel("Streaks & Extremes");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let s = streaks(values);
    let current = if s.current > 0 {
        format!("{} up", s.current)
    } else if s.current < 0 {
        format!("{} down", -s.current)
    } else {
        "flat".to_string()
    };

    let best = returns
        .iter()
        .max_by(|a, b| a.pct.total_cmp(&b.pct))
        .cloned();
    let worst = returns
        .iter()
        .min_by(|a, b| a.pct.total_cmp(&b.pct))
        .cloned();

    let mut lines = vec![
        widgets::stat("longest up", format!("{} sessions", s.longest_up), 14),
        widgets::stat("longest down", format!("{} sessions", s.longest_down), 14),
        widgets::stat_signed("current", s.current as f64, current, 14),
        Line::raw(""),
    ];

    for (label, entry) in [("best", best), ("worst", worst)] {
        match entry {
            Some(r) => lines.push(Line::from(vec![
                Span::styled(format!(" {label:<14}"), theme::label_style()),
                Span::styled(
                    format!("{:>8}", theme::pct(r.pct)),
                    Style::new().fg(theme::change_color(r.pct)),
                ),
                Span::styled(
                    format!("  {}", crate::cache::trading_day(r.ts)),
                    theme::value_style(),
                ),
            ])),
            None => lines.push(widgets::stat(label, "—", 14)),
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        format!(" {} sessions analysed", values.len()),
        theme::label_style(),
    )));

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

    // -- month bucketing ---------------------------------------------------

    #[test]
    fn months_are_bucketed_by_pkt_calendar_and_chained() {
        let bars = vec![
            bar("2025-01-02", 100.0),
            bar("2025-01-31", 110.0),
            bar("2025-02-03", 111.0),
            bar("2025-02-28", 121.0),
        ];
        let cells = month_returns(&bars);
        assert_eq!(cells.len(), 2);

        // The first month has no predecessor: measured from its own open close.
        assert_eq!((cells[0].year, cells[0].month), (2025, 1));
        assert!((cells[0].pct - 10.0).abs() < 1e-9);

        // February is measured from January's *close*, gap included.
        assert_eq!((cells[1].year, cells[1].month), (2025, 2));
        assert!((cells[1].pct - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_history_starting_mid_year_only_reports_the_months_it_traded() {
        let bars: Vec<Bar> = (1..=30)
            .map(|d| bar(&format!("2025-09-{d:02}"), 50.0 + d as f64))
            .chain((1..=31).map(|d| bar(&format!("2025-10-{d:02}"), 80.0 + d as f64)))
            .collect();

        let cells = month_returns(&bars);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].month, 9);
        assert_eq!(cells[1].month, 10);

        let avg = month_averages(&cells);
        assert!(avg[8].is_some(), "September must be populated");
        assert!(avg[9].is_some());
        assert!(avg[0].is_none(), "January never traded");
    }

    #[test]
    fn month_bucketing_spans_a_year_boundary() {
        let bars = vec![
            bar("2024-12-30", 100.0),
            bar("2024-12-31", 100.0),
            bar("2025-01-02", 90.0),
        ];
        let cells = month_returns(&bars);
        assert_eq!(cells.len(), 2);
        assert_eq!((cells[0].year, cells[0].month), (2024, 12));
        assert_eq!((cells[1].year, cells[1].month), (2025, 1));
        assert!((cells[1].pct + 10.0).abs() < 1e-9);
    }

    #[test]
    fn month_bucketing_guards_empty_and_bad_prices() {
        assert!(month_returns(&[]).is_empty());
        assert!(month_returns(&[bar("2025-01-02", 0.0)]).is_empty());
        let one = month_returns(&[bar("2025-01-02", 10.0)]);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].pct, 0.0, "a single close has no move to report");
        assert!(month_averages(&[]).iter().all(|v| v.is_none()));
    }

    // -- day of week -------------------------------------------------------

    #[test]
    fn day_of_week_attributes_returns_to_the_later_session() {
        // 2025-01-06 is a Monday.
        let bars = vec![
            bar("2025-01-06", 100.0),
            bar("2025-01-07", 110.0), // Tuesday +10%
            bar("2025-01-08", 99.0),  // Wednesday -10%
        ];
        let stats = day_of_week(&daily_returns(&bars));
        assert_eq!(stats[0].count, 0, "Monday has no predecessor here");
        assert_eq!(stats[1].count, 1);
        assert!((stats[1].avg_pct - 10.0).abs() < 1e-9);
        assert!((stats[1].win_rate - 100.0).abs() < 1e-9);
        assert_eq!(stats[2].count, 1);
        assert!((stats[2].win_rate - 0.0).abs() < 1e-9);
    }

    #[test]
    fn day_of_week_is_safe_with_no_returns() {
        let stats = day_of_week(&[]);
        assert!(stats.iter().all(|s| s.count == 0 && s.avg_pct == 0.0));
    }

    // -- distribution ------------------------------------------------------

    #[test]
    fn distribution_moments_are_hand_checkable() {
        let d = distribution(&[-2.0, -1.0, 0.0, 1.0, 2.0]);
        assert_eq!(d.n, 5);
        assert!((d.mean - 0.0).abs() < 1e-9);
        assert!((d.median - 0.0).abs() < 1e-9);
        assert!((d.sd - (10.0f64 / 4.0).sqrt()).abs() < 1e-9);
        assert!(d.skew.abs() < 1e-9, "a symmetric sample has no skew");
        assert!(d.kurtosis < 0.0, "a flat-topped sample is platykurtic");
        assert_eq!((d.up, d.down, d.flat), (2, 2, 1));
    }

    #[test]
    fn distribution_median_handles_an_even_sample() {
        let d = distribution(&[1.0, 3.0, 2.0, 4.0]);
        assert!((d.median - 2.5).abs() < 1e-9);
    }

    #[test]
    fn distribution_guards_degenerate_input() {
        let empty = distribution(&[]);
        assert_eq!(empty.n, 0);

        // No dispersion: skew and kurtosis are undefined, reported as zero.
        let flat = distribution(&[0.0; 20]);
        assert_eq!(flat.sd, 0.0);
        assert_eq!(flat.skew, 0.0);
        assert_eq!(flat.kurtosis, 0.0);
        assert_eq!(flat.flat, 20);

        let dirty = distribution(&[f64::NAN, 1.0, f64::INFINITY, 3.0]);
        assert_eq!(dirty.n, 2);
        for v in [
            dirty.mean,
            dirty.median,
            dirty.sd,
            dirty.skew,
            dirty.kurtosis,
        ] {
            assert!(v.is_finite());
        }
    }

    #[test]
    fn histogram_buckets_cover_every_observation() {
        let values: Vec<f64> = (0..100).map(|i| i as f64 / 10.0 - 5.0).collect();
        let bins = histogram(&values, 10);
        assert_eq!(bins.len(), 10);
        assert_eq!(bins.iter().map(|b| b.count).sum::<usize>(), values.len());
    }

    #[test]
    fn histogram_guards_degenerate_input() {
        assert!(histogram(&[], 10).is_empty());
        assert!(histogram(&[1.0, 2.0], 0).is_empty());

        // Every session identical: one bin, no division by a zero width.
        let one = histogram(&[2.0; 5], 8);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].count, 5);

        let single = histogram(&[1.5], 4);
        assert_eq!(single.iter().map(|b| b.count).sum::<usize>(), 1);
    }

    // -- streaks -----------------------------------------------------------

    #[test]
    fn streaks_count_the_longest_and_current_runs() {
        let s = streaks(&[1.0, 1.0, 1.0, -1.0, -1.0, 1.0, -1.0, -1.0, -1.0, -1.0]);
        assert_eq!(s.longest_up, 3);
        assert_eq!(s.longest_down, 4);
        assert_eq!(s.current, -4);

        let up = streaks(&[-1.0, 1.0, 1.0]);
        assert_eq!(up.current, 2);
        assert_eq!(up.longest_down, 1);
    }

    #[test]
    fn an_unchanged_session_breaks_a_streak() {
        let s = streaks(&[1.0, 1.0, 0.0, 1.0]);
        assert_eq!(s.longest_up, 2);
        assert_eq!(s.current, 1);

        assert_eq!(streaks(&[]), Streaks::default());
        assert_eq!(streaks(&[0.0; 5]).current, 0);
        assert_eq!(streaks(&[f64::NAN, 1.0]).current, 1);
    }

    // -- rendering ---------------------------------------------------------

    fn history(days: usize) -> Vec<Bar> {
        (0..days)
            .map(|i| {
                let close = 100.0 * (1.0 + 0.001 * i as f64) + (i as f64 / 5.0).sin() * 4.0;
                Bar {
                    ts: day_close_ts("2023-01-02") + i as i64 * 86_400,
                    open: close,
                    high: close * 1.01,
                    low: close * 0.99,
                    close,
                    volume: 1_000.0,
                }
            })
            .collect()
    }

    fn app_with(bars: Vec<Bar>) -> App {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _rx) = detached_channel();
        let mut app = App::new(store, tx);
        app.selected = "HBL".into();
        app.bars = bars;
        app
    }

    fn render(app: &App, w: u16, h: u16) {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, f.area(), app))
            .expect("seasonality screen must render");
    }

    #[test]
    fn renders_without_panicking_at_every_size() {
        let long = app_with(history(900));
        let short = app_with(history(10));
        let empty = app_with(Vec::new());
        let single = app_with(history(1));
        let flat = app_with(
            (0..400)
                .map(|i| Bar {
                    ts: day_close_ts("2024-01-01") + i * 86_400,
                    open: 10.0,
                    high: 10.0,
                    low: 10.0,
                    close: 10.0,
                    volume: 0.0,
                })
                .collect(),
        );

        for app in [&long, &short, &empty, &single, &flat] {
            for (w, h) in [(20u16, 10u16), (1, 1), (40, 12), (80, 24), (200, 60)] {
                render(app, w, h);
            }
        }
    }
}
