//! The Macro screen — what the rest of the world is doing to the PSX book.
//!
//! Four panels:
//!
//! 1. **Commodities & FX** — level, day move, unit and a sparkline for each
//!    external series, each tagged with the PSX sectors it actually drives.
//!    Without that tag the panel is a decorative price board.
//! 2. **Correlation** — the selected scrip against each series, over
//!    *date-aligned* daily returns. PSX and the global exchanges keep different
//!    calendars (Eid, Ashura, Thanksgiving, Christmas, and a different weekend
//!    convention historically), so pairing the two series by position would
//!    silently offset them by a growing number of sessions and produce a number
//!    that means nothing. The overlap count is shown so a thin sample is
//!    visible rather than implied.
//! 3. **Policy rate** — the live SBP rate, its source, and the fact that it is
//!    the risk-free rate the Analysis screen measures excess return against.
//! 4. **News** — merged Business Recorder and Dawn headlines, with anything
//!    naming the selected scrip or its company highlighted.

use std::collections::BTreeMap;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use super::{theme, widgets};
use crate::analysis::stats;
use crate::app::App;
use crate::cache::trading_day;
use crate::ext::{Group, MacroSeries, psx_link};
use crate::model::Bar;

/// Below this width the two-column layout has no room for either column.
const TWO_COLUMN_MIN_WIDTH: u16 = 76;

/// Fewest overlapping sessions worth reporting a correlation over. Below this
/// the coefficient is dominated by whichever fortnight happened to overlap.
const MIN_OVERLAP: usize = 20;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    if area.width < 8 || area.height < 3 {
        return;
    }

    if area.width < TWO_COLUMN_MIN_WIDTH {
        draw_narrow(f, area, app);
        return;
    }

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(area);

    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(56), Constraint::Percentage(44)]).areas(left);
    draw_commodities(f, top, app);
    draw_correlation(f, bottom, app);

    let [rates, news] = Layout::vertical([Constraint::Length(7), Constraint::Min(3)]).areas(right);
    draw_rates(f, rates, app);
    draw_news(f, news, app);
}

/// On a narrow terminal the panels stack; correlation and news are dropped
/// first because they need the most horizontal room to say anything.
fn draw_narrow(f: &mut Frame, area: Rect, app: &App) {
    if area.height < 10 {
        draw_commodities(f, area, app);
        return;
    }
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
    draw_commodities(f, top, app);
    draw_news(f, bottom, app);
}

// --- commodities ---------------------------------------------------------

fn draw_commodities(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.macro_focus == crate::app::MacroFocus::Series;
    let title = if focused {
        "Commodities, metals, crypto  ·  j/k scroll · s to news".to_string()
    } else {
        format!(
            "Commodities, metals, crypto  ({} · s)",
            app.macro_series.len()
        )
    };
    let block = widgets::panel(&title).border_style(if focused {
        Style::new().fg(theme::ACCENT)
    } else {
        theme::border_style()
    });
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if app.macro_series.is_empty() {
        f.render_widget(
            Paragraph::new(widgets::placeholder("Loading world markets…")),
            inner,
        );
        return;
    }

    let w = inner.width as usize;
    let budget = inner.height as usize;

    // Build the full list first, then window it. Grouping and scrolling have to
    // agree on row numbering, and headings occupy rows too — slicing the series
    // before laying out would put the offset out of step with what is drawn.
    let mut all: Vec<Line> = Vec::with_capacity(app.macro_series.len() + Group::ALL.len());
    for group in Group::ALL {
        let rows: Vec<&MacroSeries> = app
            .macro_series
            .iter()
            .filter(|s| group_of(&s.key) == Some(group))
            .collect();
        if rows.is_empty() {
            continue;
        }
        all.push(Line::from(Span::styled(
            format!("{} ", group.label()),
            Style::new().fg(theme::MUTED).bold(),
        )));
        all.extend(rows.into_iter().map(|s| series_row(s, w)));
    }
    // Anything the catalogue gained without a group still gets shown.
    all.extend(
        app.macro_series
            .iter()
            .filter(|s| group_of(&s.key).is_none())
            .map(|s| series_row(s, w)),
    );

    let max_offset = all.len().saturating_sub(budget);
    let offset = app.series_offset.min(max_offset);
    let end = (offset + budget).min(all.len());
    let visible: Vec<Line> = all[offset..end].to_vec();

    f.render_widget(Paragraph::new(Text::from(visible)), inner);

    // Show that there is more above or below, so a cut-off crypto section
    // reads as scrollable rather than missing.
    if all.len() > budget && inner.width > 2 {
        let mut marks = Vec::new();
        if offset > 0 {
            marks.push("\u{25b2}");
        }
        if end < all.len() {
            marks.push("\u{25bc}");
        }
        if !marks.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    marks.join(" "),
                    Style::new().fg(theme::ACCENT),
                )))
                .alignment(Alignment::Right),
                Rect {
                    x: inner.x,
                    y: inner.y + inner.height.saturating_sub(1),
                    width: inner.width,
                    height: 1,
                },
            );
        }
    }
}

/// The group a cached series belongs to, looked up by key.
///
/// Cached series are deserialised from JSON written by an earlier run, so a key
/// that has since been dropped from the catalogue resolves to `None` rather
/// than being assumed into the wrong section.
fn group_of(key: &str) -> Option<Group> {
    crate::ext::CATALOG
        .iter()
        .find(|spec| spec.key == key)
        .map(|spec| spec.group)
}

/// How wide the name column may be at a given panel width.
///
/// The freight row's label is "Dry bulk freight (BDRY ETF, proxy)" — 34
/// columns — and the "proxy" is the part that must not be the first thing
/// truncated away, so the column grows to fit it whenever there is room.
fn name_width(width: usize) -> usize {
    if width >= 104 {
        36
    } else if width >= 88 {
        28
    } else {
        20
    }
}

/// One series as a row, shedding columns as the panel narrows.
fn series_row(s: &MacroSeries, width: usize) -> Line<'static> {
    let name = name_width(width);
    const LAST: usize = 11;
    const CHG: usize = 9;
    const UNIT: usize = 10;
    const SPARK: usize = 12;

    let mut spans = vec![
        Span::styled(
            format!("{:<w$} ", theme::truncate(&s.name, name), w = name),
            theme::value_style(),
        ),
        Span::styled(
            format!("{:>LAST$}", theme::price(s.last)),
            theme::value_style(),
        ),
        Span::styled(
            format!("{:>CHG$}", theme::pct(s.change_pct)),
            Style::new().fg(theme::change_color(s.change_pct)),
        ),
    ];
    let mut used = name + 1 + LAST + CHG;

    if width >= used + UNIT {
        spans.push(Span::styled(
            format!(" {:<w$}", theme::truncate(&s.unit, UNIT - 1), w = UNIT - 1),
            theme::label_style(),
        ));
        used += UNIT;
    }
    if width > used + SPARK {
        spans.push(Span::raw(" "));
        spans.push(widgets::sparkline_span(&s.closes(), SPARK));
        used += SPARK + 1;
    }

    let link = psx_link(&s.key);
    if !link.is_empty() && width > used + 4 {
        let room = width - used - 2;
        spans.push(Span::styled(
            format!("  {}", theme::truncate(link, room)),
            Style::new().fg(theme::DIM),
        ));
    }

    Line::from(spans)
}

// --- correlation ---------------------------------------------------------

/// Daily returns of two bar series restricted to the trading days both saw.
///
/// PSX bars are stamped at 16:00 PKT and the Yahoo series at their own
/// exchange's close, so the join key is the PKT calendar day rather than the
/// raw timestamp. Returns are computed *after* the intersection: a return
/// spanning a gap in one calendar is a genuine multi-day move, and pairing it
/// with a single-day move on the other side is what makes an unaligned
/// correlation meaningless.
///
/// Returns `(psx_returns, other_returns, overlapping_days)`.
pub fn aligned_returns(psx: &[Bar], other: &[Bar]) -> (Vec<f64>, Vec<f64>, usize) {
    if psx.is_empty() || other.is_empty() {
        return (Vec::new(), Vec::new(), 0);
    }

    // BTreeMap keeps the intersection in chronological order and collapses a
    // duplicated day (a re-fetch landing twice) to its last print.
    let index = |bars: &[Bar]| -> BTreeMap<String, f64> {
        bars.iter()
            .filter(|b| b.close.is_finite())
            .map(|b| (trading_day(b.ts), b.close))
            .collect()
    };
    let (a, b) = (index(psx), index(other));

    let mut left = Vec::new();
    let mut right = Vec::new();
    for (day, close) in &a {
        if let Some(other_close) = b.get(day) {
            left.push(*close);
            right.push(*other_close);
        }
    }

    let overlap = left.len();
    (
        stats::simple_returns(&left),
        stats::simple_returns(&right),
        overlap,
    )
}

fn draw_correlation(f: &mut Frame, area: Rect, app: &App) {
    let title = if app.selected.is_empty() {
        "Correlation".to_string()
    } else {
        format!("{} vs world — daily returns", app.selected)
    };
    let block = widgets::panel(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if app.bars.len() < 2 || app.macro_series.is_empty() {
        let msg = if app.macro_series.is_empty() {
            "Waiting for world markets…"
        } else {
            "Select a symbol with price history"
        };
        f.render_widget(Paragraph::new(widgets::placeholder(msg)), inner);
        return;
    }

    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for s in &app.macro_series {
        if lines.len() + 1 >= inner.height as usize {
            break;
        }
        let (a, b, overlap) = aligned_returns(&app.bars, &s.bars);
        let thin = overlap < MIN_OVERLAP;
        let corr = if thin {
            0.0
        } else {
            stats::correlation(&a, &b)
        };

        let name = if w >= 72 { 34 } else { 18 };
        let mut spans = vec![Span::styled(
            format!(" {:<name$} ", theme::truncate(&s.name, name)),
            theme::value_style(),
        )];
        if thin {
            spans.push(Span::styled(
                format!("{:>7}", "—"),
                Style::new().fg(theme::DIM),
            ));
        } else {
            spans.push(Span::styled(
                format!("{corr:>+7.2}"),
                Style::new().fg(theme::change_color(corr)),
            ));
        }
        if w >= 40 {
            spans.push(Span::styled(
                format!("  {overlap:>4} days"),
                Style::new().fg(theme::DIM),
            ));
        }
        if w >= 56 {
            spans.push(Span::styled(
                format!("  {}", strength(corr, thin)),
                theme::label_style(),
            ));
        }
        lines.push(Line::from(spans));
    }

    // One honest footnote rather than a caveat per row.
    if (lines.len() as u16) < inner.height {
        lines.push(Line::from(Span::styled(
            format!(" Aligned on shared trading days; <{MIN_OVERLAP} shown as \u{2014}"),
            Style::new().fg(theme::DIM),
        )));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Plain-language reading of a correlation coefficient.
fn strength(c: f64, thin: bool) -> &'static str {
    if thin || !c.is_finite() {
        return "too little overlap";
    }
    let a = c.abs();
    let direction = if c >= 0.0 { "with" } else { "against" };
    match (a, direction) {
        (a, _) if a < 0.15 => "no relationship",
        (a, "with") if a < 0.4 => "drifts with it",
        (_, "with") => "moves with it",
        (a, _) if a < 0.4 => "drifts against it",
        _ => "moves against it",
    }
}

// --- rates ---------------------------------------------------------------

fn draw_rates(f: &mut Frame, area: Rect, app: &App) {
    let block = widgets::panel("Policy rate");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let Some(rates) = app.rates else {
        f.render_widget(
            Paragraph::new(widgets::placeholder("Fetching SBP policy rate…")),
            inner,
        );
        return;
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(" SBP policy rate  ", theme::label_style()),
        Span::styled(
            theme::pct_plain(rates.policy_rate_pct),
            Style::new().fg(theme::ACCENT).bold(),
        ),
        Span::styled(" p.a.", theme::label_style()),
    ])];

    lines.push(Line::from(Span::styled(
        if rates.fetched {
            " Source  sbp.org.pk (live)"
        } else {
            " Source  fallback — SBP scrape unavailable"
        },
        Style::new().fg(if rates.fetched {
            theme::DIM
        } else {
            theme::WARN
        }),
    )));
    lines.push(Line::from(Span::styled(
        " Feeds the risk-free rate used by",
        Style::new().fg(theme::DIM),
    )));
    lines.push(Line::from(Span::styled(
        " Sharpe and Sortino on Analysis.",
        Style::new().fg(theme::DIM),
    )));

    lines.truncate(inner.height as usize);
    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

// --- news ----------------------------------------------------------------

fn draw_news(f: &mut Frame, area: Rect, app: &App) {
    let total = app.headlines.len();
    let title = if total > 0 {
        format!("News  {}/{}", (app.news_offset + 1).min(total), total)
    } else {
        "News".to_string()
    };
    let focused = app.macro_focus == crate::app::MacroFocus::News;
    let block = widgets::panel(&title).border_style(if focused {
        Style::new().fg(theme::ACCENT)
    } else {
        theme::border_style()
    });
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if app.headlines.is_empty() {
        f.render_widget(
            Paragraph::new(widgets::placeholder("Loading headlines…")),
            inner,
        );
        return;
    }

    let company = app.company_name(&app.selected);
    let rows = inner.height as usize;
    let offset = app.news_offset.min(total.saturating_sub(1));
    let w = inner.width as usize;

    let lines: Vec<Line> = app
        .headlines
        .iter()
        .skip(offset)
        .take(rows)
        .map(|h| {
            let hit = mentions(&h.title, &app.selected, &company);
            let mut spans = Vec::new();
            let mut used = 0usize;

            if w >= 44 {
                let stamp = format!(" {:<12} ", theme::truncate(&h.when(), 12));
                used += stamp.chars().count();
                spans.push(Span::styled(stamp, Style::new().fg(theme::DIM)));
            }
            if w >= 60 {
                let src = format!("{:<4} ", theme::truncate(&h.source, 3));
                used += src.chars().count();
                spans.push(Span::styled(src, Style::new().fg(theme::MUTED)));
            }
            if used == 0 {
                spans.push(Span::raw(" "));
                used = 1;
            }

            let style = if hit {
                Style::new().fg(theme::ACCENT).bold()
            } else {
                theme::value_style()
            };
            spans.push(Span::styled(
                theme::truncate(&h.title, w.saturating_sub(used).max(1)),
                style,
            ));
            Line::from(spans)
        })
        .collect();

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Whether a headline names the selected scrip or its company.
///
/// The symbol is matched as a whole word — "PSO" must not light up on
/// "disposal" — and the company name by its first distinctive word, since
/// publishers write "Habib Bank" where PSX lists "Habib Bank Limited".
pub fn mentions(title: &str, symbol: &str, company: &str) -> bool {
    if symbol.len() >= 2 && contains_word(title, symbol) {
        return true;
    }
    company
        .split_whitespace()
        .filter(|w| w.len() >= 5 && !is_boilerplate(w))
        .any(|w| contains_word(title, w))
}

/// Corporate-form words that would match half the market.
fn is_boilerplate(word: &str) -> bool {
    const NOISE: [&str; 8] = [
        "limited",
        "company",
        "pakistan",
        "corporation",
        "industries",
        "holdings",
        "international",
        "mills",
    ];
    let w = word
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    NOISE.contains(&w.as_str())
}

/// Case-insensitive whole-word containment.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return false;
    }
    let hay = haystack.to_lowercase();
    let need = needle.to_lowercase();
    let bytes = hay.as_bytes();

    let mut from = 0usize;
    while let Some(rel) = hay[from..].find(&need) {
        let start = from + rel;
        let end = start + need.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        if from >= hay.len() {
            break;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::{DataEvent, detached_channel};
    use crate::cache::{Store, day_close_ts};
    use crate::ext::{Headline, MacroRates};
    use crate::model::{Quote, SymbolInfo};

    fn app() -> App {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _rx) = detached_channel();
        App::new(store, tx)
    }

    /// A bar on a given calendar day with a given close.
    fn day_bar(day: &str, close: f64) -> Bar {
        Bar {
            ts: day_close_ts(day),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
        }
    }

    fn series(key: &str, name: &str, bars: Vec<Bar>) -> MacroSeries {
        MacroSeries {
            key: key.into(),
            name: name.into(),
            symbol: "BZ=F".into(),
            unit: "USD/bbl".into(),
            last: bars.last().map(|b| b.close).unwrap_or(0.0),
            change_pct: 1.25,
            bars,
        }
    }

    // -- date alignment ---------------------------------------------------

    #[test]
    fn alignment_intersects_two_different_trading_calendars() {
        // PSX closed on the 8th (a local holiday); the world closed on the 6th.
        let psx = vec![
            day_bar("2026-07-06", 100.0),
            day_bar("2026-07-07", 110.0),
            day_bar("2026-07-09", 121.0),
        ];
        let world = vec![
            day_bar("2026-07-07", 50.0),
            day_bar("2026-07-08", 55.0),
            day_bar("2026-07-09", 60.5),
        ];

        let (a, b, overlap) = aligned_returns(&psx, &world);
        assert_eq!(overlap, 2, "only the 7th and 9th are shared");
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert!((a[0] - 0.10).abs() < 1e-12, "110 -> 121");
        assert!((b[0] - 0.21).abs() < 1e-12, "50 -> 60.5");
    }

    #[test]
    fn alignment_is_not_positional() {
        // Same closes, offset by one calendar day. Pairing by position would
        // report a perfect correlation; pairing by date finds no overlap.
        let psx: Vec<Bar> = (1..=10)
            .map(|d| day_bar(&format!("2026-03-{d:02}"), 100.0 + d as f64))
            .collect();
        let world: Vec<Bar> = (11..=20)
            .map(|d| day_bar(&format!("2026-03-{d:02}"), 100.0 + (d - 10) as f64))
            .collect();

        let (a, b, overlap) = aligned_returns(&psx, &world);
        assert_eq!(overlap, 0);
        assert!(a.is_empty() && b.is_empty());
        assert_eq!(stats::correlation(&a, &b), 0.0);
    }

    #[test]
    fn a_perfectly_shared_calendar_correlates_at_one() {
        let psx: Vec<Bar> = (1..=25)
            .map(|d| day_bar(&format!("2026-04-{d:02}"), 100.0 + d as f64))
            .collect();
        // Same shape at a different scale: identical returns, so a correctly
        // aligned pair must correlate at exactly 1.
        let world: Vec<Bar> = (1..=25)
            .map(|d| day_bar(&format!("2026-04-{d:02}"), (100.0 + d as f64) * 0.37))
            .collect();

        let (a, b, overlap) = aligned_returns(&psx, &world);
        assert_eq!(overlap, 25);
        assert!(stats::correlation(&a, &b) > 0.999);
    }

    #[test]
    fn alignment_survives_empty_and_duplicated_input() {
        assert_eq!(aligned_returns(&[], &[]).2, 0);
        assert_eq!(aligned_returns(&[day_bar("2026-01-05", 1.0)], &[]).2, 0);

        // The same day fetched twice collapses to one observation.
        let dup = vec![day_bar("2026-01-05", 1.0), day_bar("2026-01-05", 2.0)];
        let other = vec![day_bar("2026-01-05", 9.0)];
        assert_eq!(aligned_returns(&dup, &other).2, 1);

        // A non-finite close is dropped rather than poisoning the returns.
        let nan = vec![
            day_bar("2026-01-05", f64::NAN),
            day_bar("2026-01-06", 10.0),
            day_bar("2026-01-07", 11.0),
        ];
        let world = vec![
            day_bar("2026-01-05", 1.0),
            day_bar("2026-01-06", 2.0),
            day_bar("2026-01-07", 3.0),
        ];
        let (a, b, overlap) = aligned_returns(&nan, &world);
        assert_eq!(overlap, 2);
        assert!(a.iter().chain(b.iter()).all(|v| v.is_finite()));
    }

    // -- highlighting ------------------------------------------------------

    #[test]
    fn headlines_naming_the_symbol_or_company_are_flagged() {
        assert!(mentions(
            "PSO posts record profit",
            "PSO",
            "Pakistan State Oil"
        ));
        assert!(mentions(
            "Habib Bank leads the rally",
            "HBL",
            "Habib Bank Limited"
        ));
        assert!(!mentions(
            "Cotton arrivals slump",
            "HBL",
            "Habib Bank Limited"
        ));
    }

    #[test]
    fn highlighting_matches_whole_words_only() {
        assert!(!mentions("Asset disposal completed", "PSO", ""));
        assert!(!mentions("Shbl trading halted", "HBL", ""));
        assert!(mentions("Shares of HBL rose 2%", "HBL", ""));
        // Corporate boilerplate must not light up every headline.
        assert!(!mentions(
            "Pakistan inflation eases",
            "HBL",
            "Habib Bank Limited Pakistan"
        ));
        assert!(!mentions("anything at all", "", ""));
    }

    // -- rendering ---------------------------------------------------------

    fn populated() -> App {
        let mut a = app();
        a.on_event(DataEvent::Symbols(vec![SymbolInfo {
            symbol: "HBL".into(),
            name: "Habib Bank Limited".into(),
            sector_name: "COMMERCIAL BANKS".into(),
            is_etf: false,
            is_debt: false,
        }]));
        a.on_event(DataEvent::Quotes(vec![Quote {
            symbol: "HBL".into(),
            sector: "BANKS".into(),
            indices: vec![],
            ldcp: 292.0,
            open: 292.0,
            high: 295.0,
            low: 290.0,
            current: 293.0,
            change: 1.0,
            change_pct: 0.34,
            volume: 1000.0,
        }]));
        a.bars = (1..=28)
            .map(|d| day_bar(&format!("2026-05-{d:02}"), 290.0 + d as f64))
            .collect();
        a.on_event(DataEvent::MacroSeries(vec![
            series(
                "brent",
                "Brent crude",
                (1..=28)
                    .map(|d| day_bar(&format!("2026-05-{d:02}"), 90.0 + d as f64 * 0.2))
                    .collect(),
            ),
            series("freight", "Dry bulk freight (BDRY ETF, proxy)", vec![]),
            series("usdpkr", "USD / PKR", vec![day_bar("2026-05-01", 277.59)]),
        ]));
        a.on_event(DataEvent::Headlines(vec![
            Headline {
                title: "Habib Bank leads the KSE-100 higher".into(),
                url: "https://example.test/1".into(),
                published: "Sun, 26 Jul 2026 18:02:00 +0500".into(),
                source: "Business Recorder".into(),
            },
            Headline {
                title: "Brent tops $96 on Gulf tension".into(),
                url: "https://example.test/2".into(),
                published: "Sun, 26 Jul 2026 17:00:00 +0500".into(),
                source: "Dawn".into(),
            },
        ]));
        a.on_event(DataEvent::Rates(MacroRates {
            policy_rate_pct: 11.5,
            fetched: true,
        }));
        a
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
    fn renders_at_a_tiny_terminal_without_panicking() {
        let app = populated();
        for (w, h) in [(20u16, 10u16), (20, 3), (20, 1), (1, 1), (8, 4), (40, 6)] {
            let _ = render(&app, w, h);
        }
    }

    #[test]
    fn renders_at_a_large_terminal() {
        let app = populated();
        let buf = render(&app, 200, 60);
        let text = text_of(&buf);
        assert!(text.contains("Commodities"));
        assert!(text.contains("Brent crude"));
        assert!(text.contains("11.50%"));
        assert!(text.contains("Habib Bank"));
        // The PSX association must be on screen, not just the price.
        assert!(text.contains("refineries"));
        // No formatter may leak a non-finite value.
        assert!(!text.contains("NaN") && !text.contains("inf"));
    }

    #[test]
    fn the_freight_row_is_labelled_as_a_proxy() {
        let app = populated();
        let text = text_of(&render(&app, 200, 60));
        assert!(text.contains("proxy"), "BDRY must never pose as the BDI");
        assert!(!text.contains("Baltic"));
    }

    #[test]
    fn empty_state_shows_placeholders_rather_than_blank_panels() {
        let app = app();
        let text = text_of(&render(&app, 120, 40));
        assert!(text.contains("Loading world markets"));
        assert!(text.contains("Loading headlines"));
        assert!(text.contains("SBP policy rate") || text.contains("Fetching SBP"));
    }

    #[test]
    fn correlation_panel_reports_the_overlap_and_withholds_thin_samples() {
        let app = populated();
        let text = text_of(&render(&app, 200, 60));
        assert!(text.contains("days"), "overlap count must be visible");
        // USD/PKR has a single shared session — far too few to report.
        assert!(text.contains("too little overlap"));
    }

    #[test]
    fn news_scrolling_stays_in_range() {
        let mut app = populated();
        app.news_offset = 999;
        let _ = render(&app, 120, 40);

        app.on_event(DataEvent::Headlines(vec![]));
        assert_eq!(app.news_offset, 0);
        let _ = render(&app, 120, 40);
    }

    #[test]
    fn a_series_with_no_bars_still_renders_a_row() {
        let mut app = app();
        app.on_event(DataEvent::MacroSeries(vec![series(
            "gold",
            "Gold",
            Vec::new(),
        )]));
        let text = text_of(&render(&app, 120, 40));
        assert!(text.contains("Gold"));
    }

    #[test]
    fn non_finite_levels_render_as_a_dash() {
        let mut app = app();
        let mut s = series("gold", "Gold", Vec::new());
        s.last = f64::NAN;
        s.change_pct = f64::INFINITY;
        app.on_event(DataEvent::MacroSeries(vec![s]));
        let text = text_of(&render(&app, 120, 40));
        assert!(!text.contains("NaN") && !text.contains("inf"));
        assert!(text.contains('\u{2014}'));
    }
}
