//! Dashboard — the at-a-glance market overview.
//!
//! Four blocks, top to bottom: market breadth (advancers / decliners /
//! unchanged plus total turnover), the leaderboards (top gainers, top losers,
//! most active by value) and a sector heatmap ordered by turnover.
//!
//! Everything degrades by width and height: columns are dropped in priority
//! order and whole blocks disappear rather than being rendered as a stump, so
//! the screen stays readable from 20x10 up to a full-screen terminal.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::{App, Board, DashFocus, DashLayout, SectorAgg};
use crate::model::Quote;

use super::theme;
use super::widgets;

// --- entry point ---------------------------------------------------------

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if app.quotes.is_empty() {
        let block = widgets::panel("Market");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height > 0 {
            f.render_widget(
                Paragraph::new(widgets::placeholder("Loading market data…")),
                inner,
            );
        }
        return;
    }

    // Too short for a bordered panel: fall back to a single dense status line.
    if area.height < 3 {
        let b = breadth(&app.quotes);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("▲{} ", b.up), Style::new().fg(theme::UP)),
                Span::styled(format!("▼{} ", b.down), Style::new().fg(theme::DOWN)),
                Span::styled(format!("={} ", b.flat), Style::new().fg(theme::FLAT)),
                Span::styled(theme::compact(b.turnover), theme::value_style()),
            ])),
            area,
        );
        return;
    }

    let [breadth_area, lists_area, sector_area] = split_rows(area);

    if breadth_area.height > 0 {
        draw_breadth(f, breadth_area, app);
    }
    if lists_area.height > 0 {
        draw_lists(f, lists_area, app);
    }
    if sector_area.height > 0 {
        draw_sectors(f, sector_area, app);
    }
}

/// Height budget: breadth first (it is the headline), then the leaderboards,
/// and the sector map only once there is genuinely room for it.
fn split_rows(area: Rect) -> [Rect; 3] {
    let h = area.height;
    let breadth_h: u16 = if h >= 9 {
        5
    } else if h >= 7 {
        4
    } else {
        h.min(3)
    };
    let rest = h.saturating_sub(breadth_h);
    let sector_h: u16 = if rest >= 16 {
        (rest / 3).clamp(6, 14)
    } else if rest >= 12 {
        5
    } else {
        0
    };
    let lists_h = rest.saturating_sub(sector_h);

    let [a, b, c] = Layout::vertical([
        Constraint::Length(breadth_h),
        Constraint::Length(lists_h),
        Constraint::Length(sector_h),
    ])
    .areas(area);
    [a, b, c]
}

// --- breadth -------------------------------------------------------------

/// Advancing / declining / unchanged counts plus session totals.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct Breadth {
    pub up: usize,
    pub down: usize,
    pub flat: usize,
    pub turnover: f64,
    pub volume: f64,
    pub traded: usize,
}

impl Breadth {
    fn total(&self) -> usize {
        self.up + self.down + self.flat
    }
}

fn breadth(quotes: &[Quote]) -> Breadth {
    let mut b = Breadth::default();
    for q in quotes {
        if q.change_pct > 0.0 {
            b.up += 1;
        } else if q.change_pct < 0.0 {
            b.down += 1;
        } else {
            b.flat += 1;
        }
        if q.volume > 0.0 {
            b.traded += 1;
            b.volume += q.volume;
            let t = q.turnover();
            if t.is_finite() {
                b.turnover += t;
            }
        }
    }
    b
}

/// Cell widths for the up / down / unchanged segments of the breadth bar.
///
/// Always sums to exactly `width` when there is anything to show, and to
/// `(0, 0, 0)` for an empty market so no divide-by-zero can occur.
fn breadth_segments(b: &Breadth, width: usize) -> (usize, usize, usize) {
    let total = b.total();
    if total == 0 || width == 0 {
        return (0, 0, 0);
    }
    let scale = |n: usize| ((n as f64 / total as f64) * width as f64).round() as usize;
    let up = scale(b.up).min(width);
    let down = scale(b.down).min(width - up);
    let mut seg = [up, down, width - up - down];

    // Rounding must never erase a bucket that has members: borrow a cell from
    // the widest segment instead. The total is preserved either way.
    let counts = [b.up, b.down, b.flat];
    for i in 0..3 {
        if counts[i] > 0
            && seg[i] == 0
            && let Some(j) = (0..3).filter(|&j| seg[j] >= 2).max_by_key(|&j| seg[j])
        {
            seg[j] -= 1;
            seg[i] += 1;
        }
    }
    (seg[0], seg[1], seg[2])
}

fn draw_breadth(f: &mut Frame, area: Rect, app: &App) {
    let b = breadth(&app.quotes);
    let block = widgets::panel("Market Breadth");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let w = inner.width as usize;

    let total = b.total().max(1);
    // Below this the prose labels no longer fit, so drop to bare glyphs.
    let narrow = w < 54;
    let mut lines: Vec<Line> = Vec::new();

    lines.push(if narrow {
        Line::from(vec![
            Span::styled(format!("▲{:<5}", b.up), Style::new().fg(theme::UP).bold()),
            Span::styled(
                format!("▼{:<5}", b.down),
                Style::new().fg(theme::DOWN).bold(),
            ),
            Span::styled(format!("─{}", b.flat), Style::new().fg(theme::FLAT).bold()),
        ])
    } else {
        Line::from(vec![
            Span::styled("▲ ", Style::new().fg(theme::UP)),
            Span::styled(format!("{:<5}", b.up), Style::new().fg(theme::UP).bold()),
            Span::styled("advancing   ", theme::label_style()),
            Span::styled("▼ ", Style::new().fg(theme::DOWN)),
            Span::styled(
                format!("{:<5}", b.down),
                Style::new().fg(theme::DOWN).bold(),
            ),
            Span::styled("declining   ", theme::label_style()),
            Span::styled("─ ", Style::new().fg(theme::FLAT)),
            Span::styled(
                format!("{:<5}", b.flat),
                Style::new().fg(theme::FLAT).bold(),
            ),
            Span::styled("unchanged", theme::label_style()),
        ])
    });

    if inner.height >= 2 {
        let (uw, dw, fw) = breadth_segments(&b, w);
        lines.push(Line::from(vec![
            Span::styled(widgets::bar(1.0, uw), Style::new().fg(theme::UP)),
            Span::styled(widgets::bar(1.0, dw), Style::new().fg(theme::DOWN)),
            Span::styled("░".repeat(fw), Style::new().fg(theme::BORDER)),
        ]));
    }

    if inner.height >= 3 {
        let adv_pct = b.up as f64 / total as f64 * 100.0;
        let ad_style = Style::new().fg(theme::change_color(adv_pct - 50.0));
        // Stat segments in descending order of importance; whatever does not
        // fit is dropped rather than clipped mid-number.
        let stats: Vec<(String, String, Style)> = if narrow {
            vec![
                (
                    "PKR ".into(),
                    theme::compact(b.turnover),
                    theme::value_style(),
                ),
                ("  A/D ".into(), theme::pct_plain(adv_pct), ad_style),
            ]
        } else {
            vec![
                (
                    "Turnover ".into(),
                    format!("PKR {}", theme::compact(b.turnover)),
                    theme::value_style(),
                ),
                (
                    "   Volume ".into(),
                    theme::compact(b.volume),
                    theme::value_style(),
                ),
                (
                    "   Traded ".into(),
                    format!("{}/{}", b.traded, b.total()),
                    theme::value_style(),
                ),
                ("   A/D ".into(), theme::pct_plain(adv_pct), ad_style),
            ]
        };

        let mut spans = Vec::new();
        let mut used = 0usize;
        for (label, value, style) in stats {
            let need = label.chars().count() + value.chars().count();
            if used + need > w {
                break;
            }
            used += need;
            spans.push(Span::styled(label, theme::label_style()));
            spans.push(Span::styled(value, style));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

// --- leaderboards --------------------------------------------------------

const PRICE_W: usize = 9;
const CHG_W: usize = 8;
const VAL_W: usize = 8;
const SYM_MIN: usize = 6;

/// Which optional columns fit in `width`, and the residual symbol field.
///
/// Returns `(symbol_width, price, change, value)`.
fn list_columns(width: usize) -> (usize, bool, bool, bool) {
    let show_chg = width >= SYM_MIN + 1 + CHG_W;
    let show_price = show_chg && width >= SYM_MIN + 1 + PRICE_W + 1 + CHG_W;
    let show_val = show_price && width >= SYM_MIN + 1 + PRICE_W + 1 + CHG_W + 1 + VAL_W;

    let mut fixed = 0;
    if show_price {
        fixed += PRICE_W + 1;
    }
    if show_chg {
        fixed += CHG_W + 1;
    }
    if show_val {
        fixed += VAL_W + 1;
    }
    (width.saturating_sub(fixed), show_price, show_chg, show_val)
}

fn list_header(width: usize, value_label: &str) -> Line<'static> {
    let (sym_w, price, chg, val) = list_columns(width);
    let mut s = format!(
        "{:<sym_w$}",
        theme::truncate("SYMBOL", sym_w),
        sym_w = sym_w
    );
    if price {
        s.push_str(&format!(" {:>PRICE_W$}", "PRICE"));
    }
    if chg {
        s.push_str(&format!(" {:>CHG_W$}", "CHG%"));
    }
    if val {
        s.push_str(&format!(" {:>VAL_W$}", value_label));
    }
    Line::from(Span::styled(
        theme::truncate(&s, width),
        theme::header_style(),
    ))
}

fn quote_line(q: &Quote, width: usize, by_value: bool, selected: bool) -> Line<'static> {
    let (sym_w, price, chg, val) = list_columns(width);
    let mut spans = vec![Span::styled(
        format!(
            "{:<sym_w$}",
            theme::truncate(&q.symbol, sym_w),
            sym_w = sym_w
        ),
        Style::new()
            .fg(if selected { theme::ACCENT } else { theme::FG })
            .bold(),
    )];
    if price {
        spans.push(Span::styled(
            format!(" {:>PRICE_W$}", theme::price(q.current)),
            theme::value_style(),
        ));
    }
    if chg {
        spans.push(Span::styled(
            format!(" {:>CHG_W$}", theme::pct(q.change_pct)),
            Style::new().fg(theme::change_color(q.change_pct)),
        ));
    }
    if val {
        let v = if by_value { q.turnover() } else { q.volume };
        spans.push(Span::styled(
            format!(" {:>VAL_W$}", theme::compact(v)),
            Style::new().fg(theme::VOLUME),
        ));
    }

    let line = Line::from(spans);
    if selected {
        line.style(Style::new().bg(theme::SELECT_BG))
    } else {
        line
    }
}

/// Draw one leaderboard. `cursor` is `Some(row)` only for the focused board.
fn draw_quote_list(
    f: &mut Frame,
    area: Rect,
    title: &str,
    rows: &[&Quote],
    by_value: bool,
    cursor: Option<usize>,
) {
    let block = widgets::panel(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let w = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    let mut budget = inner.height as usize;
    if budget >= 3 {
        lines.push(list_header(w, if by_value { "VALUE" } else { "VOL" }));
        budget -= 1;
    }

    if rows.is_empty() {
        lines.push(widgets::placeholder("no data"));
    } else {
        for (i, q) in rows.iter().take(budget).enumerate() {
            lines.push(quote_line(q, w, by_value, cursor == Some(i)));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}

/// Rank the tradable board. `n` is capped by the caller's row budget.
fn draw_lists(f: &mut Frame, area: Rect, app: &App) {
    // One header line plus borders eat three rows; never ask for more.
    let cap = (area.height as usize).saturating_sub(3).max(1);

    // Which boards fit is a layout decision, so it is made here and published
    // for key handling — otherwise the cursor could address a board or row
    // that was never drawn.
    let (boards, areas) = if area.width >= 96 {
        let cols = Layout::horizontal([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(area);
        (
            vec![Board::Gainers, Board::Losers, Board::Active],
            vec![cols[0], cols[1], cols[2]],
        )
    } else if area.width >= 60 {
        let cols =
            Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(area);
        (vec![Board::Gainers, Board::Losers], vec![cols[0], cols[1]])
    } else if area.height >= 10 {
        let rows = Layout::vertical([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(area);
        (vec![Board::Gainers, Board::Losers], vec![rows[0], rows[1]])
    } else {
        (vec![Board::Active], vec![area])
    };

    let mut published = DashLayout {
        boards: [Board::Gainers, Board::Losers, Board::Active],
        count: boards.len(),
        rows: cap,
        sector_rows: app.dash_layout.get().sector_rows,
    };
    for (i, b) in boards.iter().enumerate() {
        published.boards[i] = *b;
    }
    app.dash_layout.set(published);

    // The focused board, clamped the same way key handling clamps it.
    let focused = app.dashboard.board.min(boards.len().saturating_sub(1));

    for (i, (board, rect)) in boards.iter().zip(areas).enumerate() {
        let rows = app.leaderboard(*board, cap);
        // Highlight by position, not by symbol: a scrip can top both the
        // gainers and the most-active board, and matching on the symbol lit
        // it up in both places at once.
        let selected = (i == focused).then_some(app.dashboard.cursor);
        draw_quote_list(
            f,
            rect,
            board.title(),
            &rows,
            *board == Board::Active,
            selected,
        );
    }
}

// --- sector heatmap ------------------------------------------------------

fn sector_line(s: &SectorAgg, width: usize, selected: bool) -> Line<'static> {
    let bg = widgets::heat_color(s.avg_pct, 5.0);
    // Above roughly half saturation the background is bright enough that white
    // text reads better than the muted foreground.
    let fg = if s.avg_pct.abs() >= 2.5 {
        theme::FG
    } else {
        theme::MUTED
    };

    // The cursor row is marked with a rule in its own column, so the marker
    // never overwrites the sector name. The content is laid out one column
    // narrower to pay for it, keeping every row exactly `width` wide.
    let marker_w = usize::from(selected);
    let content_w = width.saturating_sub(marker_w);

    let pct_w = 8usize;
    let cnt_w = 4usize;
    let text = if content_w <= pct_w + 2 {
        format!(
            "{:<width$}",
            theme::truncate(&s.name, content_w),
            width = content_w
        )
    } else {
        let show_count = content_w >= pct_w + cnt_w + 8;
        let name_w = content_w - pct_w - if show_count { cnt_w } else { 0 };
        let mut t = format!(
            "{:<name_w$}{:>pct_w$}",
            theme::truncate(&s.name, name_w),
            theme::pct(s.avg_pct),
            name_w = name_w,
            pct_w = pct_w
        );
        if show_count {
            t.push_str(&format!("{:>cnt_w$}", s.count, cnt_w = cnt_w));
        }
        t
    };

    if selected {
        return Line::from(vec![
            Span::styled("▌", Style::new().fg(theme::ACCENT).bg(bg)),
            Span::styled(text, Style::new().bg(bg).fg(theme::FG).bold()),
        ]);
    }
    Line::from(Span::styled(text, Style::new().bg(bg).fg(fg).bold()))
}

fn draw_sectors(f: &mut Frame, area: Rect, app: &App) {
    let sectors = app.sectors();
    let focused = app.dashboard.focus == DashFocus::Sectors;

    // Signal focus in the title, and say how to get there when it isn't.
    let title = if focused {
        format!(
            "Sectors — turnover-weighted change  [{}/{}]  Enter filters · s back",
            (app.dashboard.sector + 1).min(sectors.len().max(1)),
            sectors.len()
        )
    } else {
        format!(
            "Sectors — turnover-weighted change  ({} · s to scroll)",
            sectors.len()
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

    if sectors.is_empty() {
        app.publish_sector_rows(0);
        f.render_widget(Paragraph::new(widgets::placeholder("no sectors")), inner);
        return;
    }

    // Two columns once there is room, so more of the list is visible at once.
    let cols = if inner.width >= 72 { 2 } else { 1 };
    let col_areas: Vec<Rect> = if cols == 2 {
        Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .spacing(1)
            .split(inner)
            .to_vec()
    } else {
        vec![inner]
    };

    let per_col = inner.height as usize;
    let visible = per_col * cols;
    // Publish the window size so key handling scrolls by the right amount and
    // can keep the cursor on a row that is actually drawn.
    app.publish_sector_rows(visible);

    // Clamp the offset here too: the pane can shrink on a resize after the
    // offset was set against a taller one.
    let max_offset = sectors.len().saturating_sub(visible);
    let offset = app.dashboard.sector_offset.min(max_offset);

    for (i, col) in col_areas.iter().enumerate() {
        if col.width == 0 || col.height == 0 {
            continue;
        }
        let start = offset + i * per_col;
        if start >= sectors.len() {
            break;
        }
        let end = (start + per_col).min(sectors.len());
        let lines: Vec<Line> = sectors[start..end]
            .iter()
            .enumerate()
            .map(|(row, s)| {
                let selected = focused && start + row == app.dashboard.sector;
                sector_line(s, col.width as usize, selected)
            })
            .collect();
        f.render_widget(Paragraph::new(lines), *col);
    }

    // Scroll affordance: show that there is more above or below.
    if sectors.len() > visible && inner.width > 2 {
        let mut marks = Vec::new();
        if offset > 0 {
            marks.push("▲");
        }
        if offset + visible < sectors.len() {
            marks.push("▼");
        }
        if !marks.is_empty() {
            let y = inner.y + inner.height.saturating_sub(1);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    marks.join(" "),
                    Style::new().fg(theme::ACCENT),
                )))
                .alignment(Alignment::Right),
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::{DataEvent, detached_channel};
    use crate::cache::Store;

    fn quote(symbol: &str, sector: &str, price: f64, pct: f64, volume: f64) -> Quote {
        Quote {
            symbol: symbol.into(),
            sector: sector.into(),
            indices: vec![],
            ldcp: price,
            open: price,
            high: price,
            low: price,
            current: price,
            change: 0.0,
            change_pct: pct,
            volume,
        }
    }

    fn app_with(quotes: Vec<Quote>) -> App {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _rx) = detached_channel();
        let mut a = App::new(store, tx);
        a.on_event(DataEvent::Quotes(quotes));
        a
    }

    #[test]
    fn breadth_counts_split_by_sign() {
        let b = breadth(&[
            quote("A", "S", 10.0, 1.0, 100.0),
            quote("B", "S", 10.0, -1.0, 100.0),
            quote("C", "S", 10.0, 0.0, 0.0),
            quote("D", "S", 10.0, 2.0, 50.0),
        ]);
        assert_eq!((b.up, b.down, b.flat), (2, 1, 1));
        assert_eq!(b.total(), 4);
        assert_eq!(b.traded, 3, "zero-volume rows are not counted as traded");
        assert_eq!(b.turnover, 100.0 * 10.0 + 100.0 * 10.0 + 50.0 * 10.0);
    }

    #[test]
    fn breadth_of_an_empty_market_is_zero_everywhere() {
        let b = breadth(&[]);
        assert_eq!(b.total(), 0);
        assert_eq!(breadth_segments(&b, 20), (0, 0, 0));
    }

    #[test]
    fn breadth_segments_fill_exactly_the_available_width() {
        let b = Breadth {
            up: 3,
            down: 1,
            flat: 0,
            ..Breadth::default()
        };
        let (u, d, fl) = breadth_segments(&b, 20);
        assert_eq!(u + d + fl, 20);
        assert_eq!(u, 15);
        assert_eq!(d, 5);

        // Zero width must not panic or overflow.
        assert_eq!(breadth_segments(&b, 0), (0, 0, 0));
    }

    #[test]
    fn a_tiny_minority_still_gets_one_cell() {
        let b = Breadth {
            up: 1,
            down: 999,
            flat: 0,
            ..Breadth::default()
        };
        let (u, d, fl) = breadth_segments(&b, 10);
        assert_eq!(u, 1, "a non-empty bucket must not round away to nothing");
        assert_eq!(u + d + fl, 10);
    }

    #[test]
    fn all_unchanged_market_does_not_divide_by_zero() {
        let b = breadth(&[
            quote("A", "S", 10.0, 0.0, 0.0),
            quote("B", "S", 10.0, 0.0, 0.0),
        ]);
        assert_eq!((b.up, b.down, b.flat), (0, 0, 2));
        let (u, d, fl) = breadth_segments(&b, 8);
        assert_eq!((u, d, fl), (0, 0, 8));
        assert_eq!(b.turnover, 0.0);
    }

    #[test]
    fn sectors_are_turnover_weighted_and_ranked_by_value() {
        let s = app_with(vec![
            // BANKS: a big +10% name and a tiny -10% name.
            quote("BIG", "BANKS", 100.0, 10.0, 1000.0),
            quote("TINY", "BANKS", 1.0, -10.0, 1.0),
            quote("OIL1", "OIL", 50.0, -2.0, 10.0),
        ])
        .sectors();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "BANKS", "biggest sector by turnover reads first");
        assert_eq!(s[0].count, 2);
        assert!(
            (s[0].avg_pct - 9.98).abs() < 0.05,
            "weighting must follow value, got {}",
            s[0].avg_pct
        );
        assert_eq!(s[1].name, "OIL");
    }

    #[test]
    fn untraded_sector_falls_back_to_a_plain_mean() {
        let s = app_with(vec![
            quote("A", "CEMENT", 10.0, 4.0, 0.0),
            quote("B", "CEMENT", 10.0, -2.0, 0.0),
        ])
        .sectors();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].turnover, 0.0);
        assert!((s[0].avg_pct - 1.0).abs() < 1e-9);
    }

    #[test]
    fn blank_sectors_are_bucketed_as_unclassified() {
        let s = app_with(vec![quote("A", "  ", 10.0, 1.0, 5.0)]).sectors();
        assert_eq!(s[0].name, "UNCLASSIFIED");
    }

    #[test]
    fn empty_quotes_produce_no_sectors() {
        assert!(app_with(vec![]).sectors().is_empty());
    }

    #[test]
    fn list_columns_drop_in_priority_order() {
        // Symbol only.
        let (sym, p, c, v) = list_columns(10);
        assert_eq!((p, c, v), (false, false, false));
        assert_eq!(sym, 10);

        // Change is the first column earned back.
        let (_, p, c, v) = list_columns(16);
        assert_eq!((p, c, v), (false, true, false));

        // Then price, then the value column.
        let (_, p, c, v) = list_columns(26);
        assert_eq!((p, c, v), (true, true, false));
        let (sym, p, c, v) = list_columns(40);
        assert_eq!((p, c, v), (true, true, true));
        assert_eq!(sym + (PRICE_W + 1) + (CHG_W + 1) + (VAL_W + 1), 40);
    }

    #[test]
    fn quote_line_is_exactly_the_requested_width() {
        let q = quote("HBL", "BANKS", 292.0, -1.25, 1_234_567.0);
        for w in [4usize, 10, 17, 30, 40, 80] {
            let line = quote_line(&q, w, true, false);
            assert_eq!(line.width(), w, "width {w} must be filled exactly");
        }
    }

    #[test]
    fn sector_line_is_exactly_the_requested_width() {
        let s = SectorAgg {
            name: "COMMERCIAL BANKS".into(),
            avg_pct: -3.5,
            turnover: 1e9,
            count: 21,
        };
        for w in [2usize, 8, 12, 24, 40, 90] {
            assert_eq!(sector_line(&s, w, false).width(), w);
            assert_eq!(
                sector_line(&s, w, true).width(),
                w,
                "the selected row must keep the same width"
            );
            // The cursor marker gets its own column and must not eat the name.
            let lit: String = sector_line(&s, w, true)
                .spans
                .iter()
                .map(|sp| sp.content.as_ref())
                .collect();
            let plain: String = sector_line(&s, w, false)
                .spans
                .iter()
                .map(|sp| sp.content.as_ref())
                .collect();
            if w >= 12 {
                // The marker costs one column, so the name may truncate one
                // char earlier — but it must never *lose its first letters*,
                // which is what happened when the marker overwrote them.
                // Compare by chars: a truncated name ends in a multi-byte
                // ellipsis that byte-slicing would split.
                let head =
                    |s: &str| -> String { s.trim_start_matches('▌').chars().take(2).collect() };
                assert_eq!(
                    head(&lit),
                    head(&plain),
                    "selected row lost the start of the name: {lit:?} vs {plain:?}"
                );
            }
        }
    }

    #[test]
    fn leaders_rank_by_change_and_skip_untraded() {
        let mut app = app_with(vec![
            quote("A", "S", 10.0, 5.0, 100.0),
            quote("B", "S", 10.0, 9.0, 0.0),
            quote("C", "S", 10.0, -7.0, 100.0),
        ]);
        app.screener.equities_only = false;

        let g = app.leaderboard(Board::Gainers, 5);
        assert_eq!(g.len(), 2, "the untraded limit-up name is excluded");
        assert_eq!(g[0].symbol, "A");

        let l = app.leaderboard(Board::Losers, 5);
        assert_eq!(l[0].symbol, "C");

        assert_eq!(app.leaderboard(Board::Active, 1)[0].symbol, "A");
        assert!(app_with(vec![]).leaderboard(Board::Gainers, 5).is_empty());
    }

    fn render(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, f.area(), app)).unwrap();
        term.backend().buffer().clone()
    }

    fn non_blank(buf: &ratatui::buffer::Buffer) -> usize {
        buf.content().iter().filter(|c| c.symbol() != " ").count()
    }

    #[test]
    fn renders_at_a_tiny_terminal_without_panicking() {
        let app = app_with(vec![
            quote("HBL", "BANKS", 292.0, 1.5, 1000.0),
            quote("OGDC", "OIL", 200.0, -2.0, 5000.0),
        ]);
        for (w, h) in [(20u16, 10u16), (20, 3), (20, 1), (1, 1), (40, 6), (10, 40)] {
            let buf = render(&app, w, h);
            assert_eq!(buf.area.width, w);
        }
        assert!(non_blank(&render(&app, 20, 10)) > 0);
    }

    #[test]
    fn renders_a_full_board_at_a_large_terminal() {
        let quotes: Vec<Quote> = (0..300)
            .map(|i| {
                quote(
                    &format!("SYM{i:03}"),
                    ["BANKS", "OIL", "CEMENT", "TECH", "POWER"][i % 5],
                    10.0 + i as f64,
                    (i as f64 % 21.0) - 10.0,
                    (i as f64) * 1000.0,
                )
            })
            .collect();
        let app = app_with(quotes);
        let buf = render(&app, 200, 60);
        assert!(non_blank(&buf) > 500, "a wide dashboard should be dense");
    }

    #[test]
    fn empty_market_renders_the_placeholder() {
        let app = app_with(vec![]);
        let buf = render(&app, 60, 20);
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Loading"), "empty state must be explained");
    }

    /// Rows carrying the selection background, as `(symbol, y)`.
    fn highlighted(buf: &ratatui::buffer::Buffer) -> Vec<String> {
        let mut out = Vec::new();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            let lit = (0..buf.area.width).any(|x| buf[(x, y)].bg == theme::SELECT_BG);
            if lit && !row.trim().is_empty() {
                out.push(row.trim().to_string());
            }
        }
        out
    }

    #[test]
    fn the_cursor_row_is_highlighted() {
        let mut app = app_with(vec![
            quote("AAA", "BANKS", 10.0, 5.0, 100.0),
            quote("BBB", "BANKS", 10.0, -5.0, 100.0),
        ]);
        app.dashboard.board = 0; // Top Gainers
        app.dashboard.cursor = 0;

        let buf = render(&app, 80, 24);
        let lit = highlighted(&buf);
        assert_eq!(lit.len(), 1, "exactly one row may be highlighted");
        assert!(lit[0].contains("AAA"), "got {lit:?}");
    }

    #[test]
    fn only_the_focused_board_highlights_a_shared_symbol() {
        // One scrip tops both the gainers and the most-active board. Matching
        // by symbol used to light it up on both at once.
        let app = app_with(vec![
            quote("HOT", "BANKS", 100.0, 9.0, 1_000_000.0),
            quote("MEH", "BANKS", 10.0, -5.0, 10.0),
        ]);
        assert_eq!(app.leaderboard(Board::Gainers, 10)[0].symbol, "HOT");
        assert_eq!(app.leaderboard(Board::Active, 10)[0].symbol, "HOT");

        let buf = render(&app, 120, 24);
        assert_eq!(
            highlighted(&buf).len(),
            1,
            "a symbol on two boards must only highlight on the focused one"
        );
    }

    #[test]
    fn rendering_publishes_the_layout_for_key_handling() {
        let app = app_with(vec![quote("AAA", "BANKS", 10.0, 5.0, 100.0)]);

        render(&app, 120, 30);
        let wide = app.dash_layout.get();
        assert_eq!(wide.count, 3, "all three boards fit at 120 columns");
        assert!(wide.rows > 0);

        render(&app, 70, 30);
        assert_eq!(app.dash_layout.get().count, 2, "only two boards fit at 70");
    }
}
