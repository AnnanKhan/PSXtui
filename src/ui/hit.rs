//! Mouse hit-testing.
//!
//! Rendering decides where everything lands — which boards fit, how wide a
//! column is, where a row ends up after scrolling — so only the renderer can
//! say what is under a given cell. Each frame it registers the regions it drew
//! here, and [`crate::app::App`] reads them back when a mouse event arrives.
//! This is the same arrangement as `DashLayout`, generalised.
//!
//! Two layers, because clicking and scrolling want different granularity:
//!
//! - **Targets** are precise things you can click: one table row, one tab, one
//!   range button.
//! - **Zones** are the whole panel a target sits in. Scrolling anywhere over a
//!   list should move it, including the blank area below the last row, where no
//!   target exists.

use ratatui::layout::Rect;

/// Something clickable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Screen tab by index into `Screen::ALL`.
    Tab(usize),
    /// A screener row, as an index into the currently visible quotes.
    ScreenerRow(usize),
    /// A dashboard leaderboard row: which board, and which row within it.
    BoardRow { board: usize, row: usize },
    /// A sector heatmap row, indexed into the full sector list.
    SectorRow(usize),
    /// A company drill-down tab by index into `CompanyTab::ALL`.
    CompanyTab(usize),
    /// An announcement row.
    Announcement(usize),
    /// A chart range button by index into `Range::ALL`.
    ChartRange(usize),
    /// A chart overlay toggle, by index into the header's overlay list.
    ChartOverlay(usize),
    /// The screener's sort-column header.
    SortColumn(usize),
    /// The status bar's help affordance.
    Help,
    /// The status bar's quit affordance.
    Quit,
    /// The chart's style indicator — clicking cycles it, like `c`.
    ChartStyle,
    /// The chart's indicator pane title — clicking cycles it, like `i`.
    ChartPane,
    /// A macro series row.
    MacroRow(usize),
    /// A headline row.
    NewsRow(usize),
    /// A comparison range button by index into `Range::ALL`.
    CompareRange(usize),
    /// A symbol chip in the comparison header, by index into the compared set.
    CompareSymbol(usize),
    /// One of the screener's filter or sort affordances.
    ScreenerToggle(Toggle),
}

/// A screener switch that the footer or the panel title draws.
///
/// Each one has a key already; the mouse targets name the same switches so the
/// two paths stay in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Price columns versus the valuation view — the `f` key.
    Valuation,
    /// Ascending versus descending — the `S` key.
    SortDirection,
    /// Watchlist members only — the `W` key.
    Watchlist,
    /// Equities only — the `e` key.
    Equities,
}

/// A scrollable or focusable panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Screener,
    Board(usize),
    Sectors,
    Announcements,
    MacroSeries,
    MacroNews,
    Chart,
    /// The comparison plot, where the wheel changes the range as on the chart.
    Compare,
}

/// What the renderer drew, and where.
#[derive(Debug, Default)]
pub struct HitMap {
    targets: Vec<(Rect, Target)>,
    zones: Vec<(Rect, Zone)>,
}

impl HitMap {
    /// Drop the previous frame's regions. Called once at the top of a draw.
    pub fn clear(&mut self) {
        self.targets.clear();
        self.zones.clear();
    }

    pub fn target(&mut self, rect: Rect, target: Target) {
        // A zero-sized rect can never be hit and would only slow the scan.
        if rect.width > 0 && rect.height > 0 {
            self.targets.push((rect, target));
        }
    }

    pub fn zone(&mut self, rect: Rect, zone: Zone) {
        if rect.width > 0 && rect.height > 0 {
            self.zones.push((rect, zone));
        }
    }

    /// The clickable target at a cell, if any.
    ///
    /// Searched newest-first so that a target drawn over an earlier one — an
    /// overlay above the screen beneath it — wins, matching what is visible.
    pub fn target_at(&self, x: u16, y: u16) -> Option<Target> {
        self.targets
            .iter()
            .rev()
            .find(|(r, _)| contains(*r, x, y))
            .map(|(_, t)| *t)
    }

    /// The panel at a cell, if any.
    pub fn zone_at(&self, x: u16, y: u16) -> Option<Zone> {
        self.zones
            .iter()
            .rev()
            .find(|(r, _)| contains(*r, x, y))
            .map(|(_, z)| *z)
    }

    /// Register one row per line of `area`, numbering from `first`.
    ///
    /// The common case: a list drawn top to bottom from a scroll offset.
    pub fn rows(&mut self, area: Rect, first: usize, count: usize, f: impl Fn(usize) -> Target) {
        for i in 0..count.min(area.height as usize) {
            self.target(
                Rect {
                    x: area.x,
                    y: area.y + i as u16,
                    width: area.width,
                    height: 1,
                },
                f(first + i),
            );
        }
    }
}

/// Whether a cell falls inside a rect.
///
/// `Rect::contains` takes a `Position`; this keeps the call sites in terms of
/// the raw column and row the mouse event carries.
fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn finds_a_target_inside_its_rect() {
        let mut m = HitMap::default();
        m.target(rect(2, 3, 10, 1), Target::Tab(4));

        assert_eq!(m.target_at(2, 3), Some(Target::Tab(4)));
        assert_eq!(m.target_at(11, 3), Some(Target::Tab(4)));
        // Just outside on every edge.
        assert_eq!(m.target_at(1, 3), None);
        assert_eq!(m.target_at(12, 3), None);
        assert_eq!(m.target_at(2, 2), None);
        assert_eq!(m.target_at(2, 4), None);
    }

    #[test]
    fn later_targets_win_so_overlays_are_clickable() {
        let mut m = HitMap::default();
        m.target(rect(0, 0, 20, 5), Target::ScreenerRow(1));
        m.target(rect(2, 2, 4, 1), Target::ChartStyle);

        assert_eq!(m.target_at(3, 2), Some(Target::ChartStyle));
        assert_eq!(m.target_at(10, 2), Some(Target::ScreenerRow(1)));
    }

    #[test]
    fn zones_are_searched_independently_of_targets() {
        let mut m = HitMap::default();
        m.zone(rect(0, 0, 20, 10), Zone::Screener);
        m.target(rect(0, 1, 20, 1), Target::ScreenerRow(0));

        // The blank area below the last row still scrolls the list.
        assert_eq!(m.zone_at(5, 8), Some(Zone::Screener));
        assert_eq!(m.target_at(5, 8), None);
        assert_eq!(m.target_at(5, 1), Some(Target::ScreenerRow(0)));
    }

    #[test]
    fn rows_are_numbered_from_the_scroll_offset() {
        let mut m = HitMap::default();
        m.rows(rect(0, 5, 10, 3), 20, 3, Target::ScreenerRow);

        assert_eq!(m.target_at(0, 5), Some(Target::ScreenerRow(20)));
        assert_eq!(m.target_at(0, 6), Some(Target::ScreenerRow(21)));
        assert_eq!(m.target_at(0, 7), Some(Target::ScreenerRow(22)));
        assert_eq!(m.target_at(0, 8), None);
    }

    #[test]
    fn rows_never_run_past_the_area() {
        let mut m = HitMap::default();
        // More rows offered than the area can hold.
        m.rows(rect(0, 0, 10, 2), 0, 99, Target::ScreenerRow);
        assert_eq!(m.target_at(0, 1), Some(Target::ScreenerRow(1)));
        assert_eq!(m.target_at(0, 2), None);
    }

    #[test]
    fn clearing_drops_the_previous_frame() {
        let mut m = HitMap::default();
        m.target(rect(0, 0, 5, 1), Target::Tab(0));
        m.zone(rect(0, 0, 5, 5), Zone::Chart);
        m.clear();
        assert_eq!(m.target_at(0, 0), None);
        assert_eq!(m.zone_at(0, 0), None);
    }

    #[test]
    fn degenerate_rects_are_never_registered() {
        let mut m = HitMap::default();
        m.target(rect(0, 0, 0, 1), Target::Tab(0));
        m.target(rect(0, 0, 5, 0), Target::Tab(1));
        m.zone(rect(0, 0, 0, 0), Zone::Chart);
        assert_eq!(m.target_at(0, 0), None);
        assert_eq!(m.zone_at(0, 0), None);
    }
}
