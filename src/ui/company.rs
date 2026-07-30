//! Company drill-down: profile, financials, ratios and filings.
//!
//! The financial statements PSX publishes are sector-specific — a bank reports
//! "Mark-up Earned" where a cement maker reports "Sales" — so nothing here
//! hardcodes a row name. Tables are built from the union of the row labels
//! actually present across the periods, with the periods as columns.

use ratatui::prelude::*;
use ratatui::widgets::{Cell, Paragraph, Row, Table, Tabs, Wrap};

use super::hit::{Target, Zone};
use super::{theme, widgets};
use crate::app::{App, CompanyTab};
use crate::model::{Announcement, Company, FinancialPeriod, RatioPeriod};

/// One statement column: a period label and its (row label, value) pairs.
type Column<'a> = (&'a str, &'a [(String, f64)]);

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let Some(company) = app.company.as_ref() else {
        let block = widgets::panel("Company");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height > 0 {
            let pad = (inner.height.saturating_sub(1) / 2) as usize;
            let mut lines: Vec<Line> = vec![Line::raw(""); pad];
            lines.push(widgets::placeholder("Loading company profile…"));
            f.render_widget(
                Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
                inner,
            );
        }
        return;
    };

    let [header, tabs, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    draw_header(f, header, company);
    draw_tabs(f, tabs, app);

    if body.height == 0 || body.width == 0 {
        return;
    }
    match app.company_tab {
        CompanyTab::Profile => draw_profile(f, body, company),
        CompanyTab::Financials => draw_financials(f, body, company),
        CompanyTab::Ratios => draw_ratios(f, body, company),
        CompanyTab::Announcements => {
            draw_announcements(f, body, company, app.announcement_cursor, app)
        }
    }
}

// --- chrome --------------------------------------------------------------

fn draw_header(f: &mut Frame, area: Rect, c: &Company) {
    if area.height == 0 {
        return;
    }
    // The sector only earns its place once the name has room of its own.
    let sector_w = if area.width >= 48 {
        (area.width / 3).min(30) as usize
    } else {
        0
    };
    let name_w =
        area.width
            .saturating_sub(c.symbol.chars().count() as u16 + 3 + sector_w as u16) as usize;

    let left = Line::from(vec![
        Span::styled(
            format!(" {} ", c.symbol),
            Style::new().fg(theme::ACCENT).bold(),
        ),
        Span::styled(theme::truncate(&c.name, name_w), theme::value_style()),
    ]);
    f.render_widget(Paragraph::new(left), area);

    if sector_w > 0 {
        let right = Line::from(Span::styled(
            format!(
                "{} ",
                theme::truncate(&c.sector, sector_w.saturating_sub(1))
            ),
            theme::header_style(),
        ))
        .right_aligned();
        f.render_widget(Paragraph::new(right), area);
    }
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let selected = CompanyTab::ALL
        .iter()
        .position(|t| *t == app.company_tab)
        .unwrap_or(0);
    let titles: Vec<Line> = CompanyTab::ALL
        .iter()
        .map(|t| Line::from(t.label()))
        .collect();

    let tabs = Tabs::new(titles)
        .select(selected)
        .style(Style::new().fg(theme::MUTED))
        .highlight_style(Style::new().fg(theme::ACCENT).bold())
        .divider(Span::styled("│", theme::border_style()));

    f.render_widget(tabs, area);

    // Mirror the widget's geometry: one space of padding either side of each
    // title, one column of divider between them.
    let mut x = area.x;
    for (i, t) in CompanyTab::ALL.iter().enumerate() {
        let w = t.label().chars().count() as u16 + 2;
        if x >= area.right() {
            break;
        }
        app.hits.borrow_mut().target(
            Rect {
                x,
                y: area.y,
                width: w.min(area.right() - x),
                height: 1,
            },
            Target::CompanyTab(i),
        );
        x = x.saturating_add(w).saturating_add(1);
    }
}

// --- profile -------------------------------------------------------------

fn draw_profile(f: &mut Frame, area: Rect, c: &Company) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).areas(area);

    let [desc_area, people_area] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(left);
    let [equity_area, market_area] =
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(right);

    draw_business(f, desc_area, c);
    draw_people(f, people_area, c);
    draw_equity(f, equity_area, c);
    draw_market(f, market_area, c);
}

fn draw_business(f: &mut Frame, area: Rect, c: &Company) {
    let block = widgets::panel("Business");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    if c.business_description.trim().is_empty() {
        lines.push(widgets::placeholder("No description published"));
    } else {
        for para in c.business_description.split('\n') {
            lines.push(Line::from(Span::styled(
                para.trim().to_string(),
                theme::value_style(),
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(field("Address", &c.address, 10));
    lines.push(field("Website", &c.website, 10));

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_people(f: &mut Frame, area: Rect, c: &Company) {
    let block = widgets::panel("Key People");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if c.key_people.is_empty() {
        f.render_widget(
            Paragraph::new(Text::from(vec![widgets::placeholder("Not published")])),
            inner,
        );
        return;
    }

    let rows: Vec<Row> = c
        .key_people
        .iter()
        .map(|(name, role)| {
            Row::new(vec![
                Cell::from(Span::styled(name.clone(), theme::value_style())),
                Cell::from(Span::styled(role.clone(), theme::label_style())),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [Constraint::Percentage(52), Constraint::Percentage(48)],
    )
    .header(
        Row::new(vec![
            Cell::from(Span::styled("Name", theme::header_style())),
            Cell::from(Span::styled("Role", theme::header_style())),
        ])
        .bottom_margin(0),
    )
    .column_spacing(1);

    f.render_widget(table, inner);
}

fn draw_equity(f: &mut Frame, area: Rect, c: &Company) {
    let block = widgets::panel("Equity");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let w = 13;
    // `market_cap_000` is published in thousands of PKR.
    let cap = c.market_cap_000.map(|v| v * 1_000.0);
    let lines = vec![
        widgets::stat("Market Cap", opt_compact(cap), w),
        widgets::stat("Shares Out", opt_compact(c.shares), w),
        widgets::stat("Free Float", opt_compact(c.free_float), w),
        widgets::stat(
            "Free Float %",
            c.free_float_pct
                .map(theme::pct_plain)
                .unwrap_or_else(|| "—".into()),
            w,
        ),
        widgets::stat("Fiscal Year", dash(&c.fiscal_year_end), w),
    ];

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_market(f: &mut Frame, area: Rect, c: &Company) {
    let block = widgets::panel("Quote & Ratios");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let w = 13;
    let mut lines = vec![
        widgets::stat(
            "52w Range",
            format!(
                "{} – {}",
                theme::opt_price(c.week52_low),
                theme::opt_price(c.week52_high)
            ),
            w,
        ),
        widgets::stat(
            "Circuit",
            format!(
                "{} – {}",
                theme::opt_price(c.circuit_low),
                theme::opt_price(c.circuit_high)
            ),
            w,
        ),
        widgets::stat("P/E", theme::opt(c.pe_ratio, 2), w),
    ];

    match c.change_1y_pct {
        Some(v) => lines.push(widgets::stat_signed("1Y Change", v, theme::pct(v), w)),
        None => lines.push(widgets::stat("1Y Change", "—", w)),
    }
    match c.change_ytd_pct {
        Some(v) => lines.push(widgets::stat_signed("YTD Change", v, theme::pct(v), w)),
        None => lines.push(widgets::stat("YTD Change", "—", w)),
    }

    lines.push(Line::raw(""));
    lines.push(field("Registrar", &c.registrar, w));
    lines.push(field("Auditor", &c.auditor, w));

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        inner,
    );
}

// --- statements ----------------------------------------------------------

fn draw_financials(f: &mut Frame, area: Rect, c: &Company) {
    let annual = financial_columns(&c.financials_annual);
    let quarterly = financial_columns(&c.financials_quarterly);

    // Side by side once there is room for two readable tables.
    if area.width >= 100 {
        let [l, r] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(area);
        draw_grid(f, l, "Annual", &annual, Scale::Thousands);
        draw_grid(f, r, "Quarterly", &quarterly, Scale::Thousands);
    } else {
        let [t, b] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
        draw_grid(f, t, "Annual", &annual, Scale::Thousands);
        draw_grid(f, b, "Quarterly", &quarterly, Scale::Thousands);
    }
}

fn draw_ratios(f: &mut Frame, area: Rect, c: &Company) {
    let cols = ratio_columns(&c.ratios);
    draw_grid(f, area, "Ratios", &cols, Scale::AsPublished);
}

fn financial_columns(periods: &[FinancialPeriod]) -> Vec<Column<'_>> {
    periods
        .iter()
        .map(|p| (p.period.as_str(), p.rows.as_slice()))
        .collect()
}

fn ratio_columns(periods: &[RatioPeriod]) -> Vec<Column<'_>> {
    periods
        .iter()
        .map(|p| (p.period.as_str(), p.rows.as_slice()))
        .collect()
}

/// How a statement's numbers should be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scale {
    /// Currency lines are published in thousands of PKR; per-share lines are
    /// already in rupees.
    Thousands,
    /// Ratios and percentages, shown exactly as published.
    AsPublished,
}

/// Render a period-per-column statement table.
fn draw_grid(f: &mut Frame, area: Rect, title: &str, cols: &[Column<'_>], scale: Scale) {
    let block = widgets::panel(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let labels = row_label_union(cols);
    if labels.is_empty() || cols.is_empty() {
        f.render_widget(
            Paragraph::new(Text::from(vec![widgets::placeholder("Not published")])),
            inner,
        );
        return;
    }

    let [note_area, table_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let note = match scale {
        Scale::Thousands => " published in PKR '000 · shown in PKR · per-share lines in PKR",
        Scale::AsPublished => " one column per period · values as published",
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(note, theme::label_style()))),
        note_area,
    );
    if table_area.height == 0 {
        return;
    }

    // Fit as many periods as the panel can show without squashing the values.
    const VALUE_W: u16 = 11;
    let label_w = 20u16.min(table_area.width.saturating_sub(VALUE_W).max(6));
    let room = table_area.width.saturating_sub(label_w) / VALUE_W;
    let shown = (room as usize).clamp(1, cols.len());
    let cols = &cols[..shown];

    let header = Row::new(
        std::iter::once(Cell::from(Span::styled("", theme::header_style())))
            .chain(cols.iter().map(|(p, _)| {
                Cell::from(
                    Text::from(theme::truncate(p, VALUE_W as usize))
                        .right_aligned()
                        .style(theme::header_style()),
                )
            }))
            .collect::<Vec<_>>(),
    );

    let rows: Vec<Row> = labels
        .iter()
        .map(|label| {
            let per_share = is_per_share(label);
            let cells = std::iter::once(Cell::from(Span::styled(
                format!(
                    " {}",
                    theme::truncate(label, label_w.saturating_sub(1) as usize)
                ),
                theme::label_style(),
            )))
            .chain(cols.iter().map(|(_, rows)| {
                let v = lookup(rows, label);
                let text = match v {
                    None => "—".into(),
                    Some(v) if scale == Scale::AsPublished || per_share => theme::opt(Some(v), 2),
                    // Statement currency lines arrive in thousands of PKR.
                    Some(v) => theme::compact(v * 1_000.0),
                };
                let style = match v {
                    Some(v) if v < 0.0 => Style::new().fg(theme::DOWN),
                    Some(_) => theme::value_style(),
                    None => theme::label_style(),
                };
                Cell::from(Text::from(text).right_aligned().style(style))
            }))
            .collect::<Vec<_>>();
            Row::new(cells)
        })
        .collect();

    let widths: Vec<Constraint> = std::iter::once(Constraint::Length(label_w))
        .chain(std::iter::repeat_n(
            Constraint::Length(VALUE_W.saturating_sub(1)),
            cols.len(),
        ))
        .collect();

    f.render_widget(
        Table::new(rows, widths).header(header).column_spacing(1),
        table_area,
    );
}

// --- announcements -------------------------------------------------------

fn draw_announcements(f: &mut Frame, area: Rect, c: &Company, cursor: usize, app: &App) {
    let block = widgets::panel("Announcements");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if c.announcements.is_empty() {
        f.render_widget(
            Paragraph::new(Text::from(vec![widgets::placeholder(
                "No filings published",
            )])),
            inner,
        );
        return;
    }

    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(inner);

    let cursor = cursor.min(c.announcements.len() - 1);
    let visible = list_area.height.saturating_sub(1) as usize; // one line for the header
    let offset = scroll_offset(cursor, visible, c.announcements.len());

    let rows: Vec<Row> = c
        .announcements
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible.max(1))
        .map(|(i, a)| announcement_row(a, i == cursor))
        .collect();

    let header = Row::new(vec![
        Cell::from(Span::styled("Date", theme::header_style())),
        Cell::from(Span::styled("Category", theme::header_style())),
        Cell::from(Span::styled("Title", theme::header_style())),
        Cell::from(Span::styled("PDF", theme::header_style())),
    ]);

    let table = Table::new(
        rows,
        [
            // "Apr 30, 2026" is 12 columns — 11 clipped the year.
            Constraint::Length(12),
            Constraint::Length(17),
            Constraint::Min(10),
            Constraint::Length(4),
        ],
    )
    .header(header)
    .column_spacing(1);

    if list_area.height > 0 {
        f.render_widget(table, list_area);

        // The announcements table is unscrolled — it renders from the top —
        // so hit rows start at zero, past the header line.
        let header_rows = u16::from(list_area.height >= 2);
        let mut hits = app.hits.borrow_mut();
        hits.zone(list_area, Zone::Announcements);
        if list_area.height > header_rows {
            hits.rows(
                Rect {
                    x: list_area.x,
                    y: list_area.y + header_rows,
                    width: list_area.width,
                    height: list_area.height - header_rows,
                },
                0,
                c.announcements.len(),
                Target::Announcement,
            );
        }
    }

    if detail_area.height == 0 {
        return;
    }
    let selected = &c.announcements[cursor];
    let link = selected
        .pdf_url
        .as_deref()
        .unwrap_or("no attachment for this filing");
    let detail = Text::from(vec![
        Line::from(vec![
            Span::styled(" ", theme::label_style()),
            Span::styled(
                format!("{}/{}  ", cursor + 1, c.announcements.len()),
                theme::label_style(),
            ),
            Span::styled(
                theme::truncate(&selected.title, area.width.saturating_sub(12) as usize),
                theme::value_style(),
            ),
        ]),
        Line::from(vec![
            Span::styled(" PDF ", theme::label_style()),
            Span::styled(
                theme::truncate(link, area.width.saturating_sub(7) as usize),
                Style::new().fg(theme::ACCENT),
            ),
        ]),
    ]);
    f.render_widget(Paragraph::new(detail), detail_area);
}

fn announcement_row<'a>(a: &'a Announcement, selected: bool) -> Row<'a> {
    let base = if selected {
        Style::new().bg(theme::SELECT_BG)
    } else {
        Style::new()
    };
    let kind_color = match a.category {
        crate::model::AnnouncementKind::FinancialResults => theme::UP,
        crate::model::AnnouncementKind::BoardMeeting => theme::WARN,
        crate::model::AnnouncementKind::Other => theme::MUTED,
    };
    let (pdf, pdf_color) = match a.pdf_url {
        Some(_) => ("PDF", theme::ACCENT),
        None => ("—", theme::DIM),
    };

    Row::new(vec![
        Cell::from(Span::styled(a.date.clone(), theme::value_style())),
        Cell::from(Span::styled(
            a.category.label(),
            Style::new().fg(kind_color),
        )),
        Cell::from(Span::styled(a.title.clone(), theme::value_style())),
        Cell::from(Span::styled(pdf, Style::new().fg(pdf_color))),
    ])
    .style(base)
}

// --- helpers -------------------------------------------------------------

/// Row labels in first-seen order across every period.
///
/// PSX's statement rows are sector-specific and a period occasionally omits a
/// line entirely, so the table's row set is the union rather than any single
/// period's.
fn row_label_union(cols: &[Column<'_>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, rows) in cols {
        for (label, _) in rows.iter() {
            let label = label.trim();
            if label.is_empty() {
                continue;
            }
            if !out.iter().any(|k| k.eq_ignore_ascii_case(label)) {
                out.push(label.to_string());
            }
        }
    }
    out
}

fn lookup(rows: &[(String, f64)], label: &str) -> Option<f64> {
    rows.iter()
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(label))
        .map(|(_, v)| *v)
        .filter(|v| v.is_finite())
}

/// Per-share lines are reported in rupees, not thousands.
fn is_per_share(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    l.contains("eps") || l.contains("per share") || l.contains("dividend %")
}

/// Keep `cursor` inside a `visible`-row window over `len` rows.
fn scroll_offset(cursor: usize, visible: usize, len: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    let max = len - visible;
    cursor.saturating_sub(visible.saturating_sub(1)).min(max)
}

fn opt_compact(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() => theme::compact(v),
        _ => "—".into(),
    }
}

fn dash(s: &str) -> String {
    if s.trim().is_empty() {
        "—".into()
    } else {
        s.trim().to_string()
    }
}

fn field<'a>(label: &'a str, value: &str, width: usize) -> Line<'a> {
    widgets::stat(label, dash(value), width)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::app::detached_channel;
    use crate::cache::Store;
    use crate::model::AnnouncementKind;

    fn period(name: &str, rows: &[(&str, f64)]) -> FinancialPeriod {
        FinancialPeriod {
            period: name.into(),
            rows: rows.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
        }
    }

    // -- row-label union ---------------------------------------------------

    #[test]
    fn row_labels_are_the_union_across_periods_in_first_seen_order() {
        let periods = vec![
            period("2025", &[("Sales", 1.0), ("Profit After Tax", 2.0)]),
            // A later period drops a line and adds one of its own.
            period("2024", &[("Sales", 3.0), ("EPS", 4.0)]),
        ];
        let cols = financial_columns(&periods);
        assert_eq!(
            row_label_union(&cols),
            vec!["Sales", "Profit After Tax", "EPS"]
        );
    }

    #[test]
    fn sector_specific_row_names_are_not_hardcoded() {
        let bank = vec![period("2025", &[("Mark-up Earned", 10.0), ("EPS", 1.0)])];
        let industrial = vec![period("2025", &[("Sales", 10.0), ("EPS", 1.0)])];
        assert_eq!(
            row_label_union(&financial_columns(&bank)),
            vec!["Mark-up Earned", "EPS"]
        );
        assert_eq!(
            row_label_union(&financial_columns(&industrial)),
            vec!["Sales", "EPS"]
        );
    }

    #[test]
    fn row_label_union_deduplicates_case_and_whitespace() {
        let periods = vec![
            period("2025", &[(" Sales ", 1.0), ("sales", 2.0)]),
            period("2024", &[("SALES", 3.0), ("", 9.0)]),
        ];
        assert_eq!(row_label_union(&financial_columns(&periods)), vec!["Sales"]);
    }

    #[test]
    fn row_label_union_is_empty_without_data() {
        assert!(row_label_union(&[]).is_empty());
        let empty = vec![period("2025", &[])];
        assert!(row_label_union(&financial_columns(&empty)).is_empty());
    }

    #[test]
    fn lookup_is_case_insensitive_and_rejects_non_finite() {
        let rows = vec![
            ("Sales".to_string(), 12.0),
            ("Broken".to_string(), f64::NAN),
        ];
        assert_eq!(lookup(&rows, "sales"), Some(12.0));
        assert_eq!(lookup(&rows, "SALES"), Some(12.0));
        assert_eq!(lookup(&rows, "Broken"), None);
        assert_eq!(lookup(&rows, "Missing"), None);
    }

    #[test]
    fn per_share_rows_are_detected() {
        assert!(is_per_share("EPS"));
        assert!(is_per_share("eps (rs)"));
        assert!(is_per_share("Earnings Per Share"));
        assert!(!is_per_share("Sales"));
        assert!(!is_per_share("Mark-up Earned"));
    }

    // -- scrolling ---------------------------------------------------------

    #[test]
    fn scroll_offset_keeps_the_cursor_visible() {
        assert_eq!(scroll_offset(0, 5, 20), 0);
        assert_eq!(scroll_offset(4, 5, 20), 0);
        assert_eq!(scroll_offset(5, 5, 20), 1);
        assert_eq!(scroll_offset(19, 5, 20), 15);
        // Never scrolls past the end, and is a no-op when everything fits.
        assert_eq!(scroll_offset(19, 50, 20), 0);
        assert_eq!(scroll_offset(3, 0, 20), 0);
        assert_eq!(scroll_offset(0, 5, 0), 0);
    }

    // -- rendering smoke tests --------------------------------------------

    fn company() -> Company {
        Company {
            symbol: "HBL".into(),
            name: "Habib Bank Limited".into(),
            sector: "COMMERCIAL BANKS".into(),
            business_description: "Habib Bank Limited provides commercial banking \
                and related services in Pakistan and internationally."
                .repeat(3),
            key_people: vec![
                ("Muhammad Aurangzeb".into(), "President & CEO".into()),
                ("Sultan Ali Allana".into(), "Chairman".into()),
            ],
            address: "Habib Bank Plaza, I.I. Chundrigar Road, Karachi".into(),
            website: "www.hbl.com".into(),
            registrar: "CDC Share Registrar Services".into(),
            auditor: "KPMG Taseer Hadi & Co.".into(),
            fiscal_year_end: "December 31".into(),
            market_cap_000: Some(430_000_000.0),
            shares: Some(1_466_852_508.0),
            free_float: Some(500_000_000.0),
            free_float_pct: Some(34.09),
            week52_low: Some(180.0),
            week52_high: Some(350.0),
            circuit_low: Some(263.0),
            circuit_high: Some(321.0),
            pe_ratio: Some(5.21),
            change_1y_pct: Some(33.9),
            change_ytd_pct: Some(-4.2),
            financials_annual: vec![
                period("2025", &[("Mark-up Earned", 1_000_000.0), ("EPS", 42.5)]),
                period("2024", &[("Mark-up Earned", 900_000.0), ("EPS", -3.5)]),
                period("2023", &[("Mark-up Earned", 800_000.0)]),
            ],
            financials_quarterly: vec![period("Q1 2026", &[("Sales", 250_000.0), ("EPS", 10.0)])],
            ratios: vec![RatioPeriod {
                period: "2025".into(),
                rows: vec![("ROE".into(), 21.4), ("Debt/Equity".into(), 0.8)],
            }],
            announcements: (0..40)
                .map(|i| Announcement {
                    date: format!("2026-01-{:02}", (i % 28) + 1),
                    title: format!("Filing number {i} with a fairly long descriptive title"),
                    category: match i % 3 {
                        0 => AnnouncementKind::FinancialResults,
                        1 => AnnouncementKind::BoardMeeting,
                        _ => AnnouncementKind::Other,
                    },
                    pdf_url: (i % 2 == 0)
                        .then(|| format!("https://dps.psx.com.pk/download/document/{i}.pdf")),
                })
                .collect(),
        }
    }

    fn app_with(c: Option<Company>, tab: CompanyTab, cursor: usize) -> App {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _rx) = detached_channel();
        let mut a = App::new(store, tx);
        a.selected = "HBL".into();
        a.company = c;
        a.company_tab = tab;
        a.announcement_cursor = cursor;
        a
    }

    fn render(app: &App, w: u16, h: u16) -> Buffer {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, f.area(), app))
            .expect("company screen must render");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn eyeball_dump() {
        for tab in CompanyTab::ALL {
            let app = app_with(Some(company()), tab, 6);
            for (w, h) in [(140u16, 22u16), (20, 10)] {
                let buf = render(&app, w, h);
                println!("=== {} {w}x{h} ===", tab.label());
                for y in 0..h {
                    let row: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
                    println!("{row}");
                }
            }
        }
    }

    #[test]
    fn renders_every_tab_without_panicking_at_every_size() {
        for tab in CompanyTab::ALL {
            for cursor in [0usize, 39, 500] {
                let app = app_with(Some(company()), tab, cursor);
                for (w, h) in [(20u16, 10u16), (1, 1), (60, 20), (110, 30), (200, 60)] {
                    render(&app, w, h);
                }
            }
        }
    }

    #[test]
    fn renders_a_missing_or_empty_company_without_panicking() {
        let none = app_with(None, CompanyTab::Profile, 0);
        let bare = app_with(Some(Company::default()), CompanyTab::Financials, 0);
        for app in [&none, &bare] {
            for tab in CompanyTab::ALL {
                let mut app = app_with(app.company.clone(), tab, 0);
                app.company_tab = tab;
                for (w, h) in [(20u16, 10u16), (1, 1), (80, 24), (200, 60)] {
                    render(&app, w, h);
                }
            }
        }
    }
}
