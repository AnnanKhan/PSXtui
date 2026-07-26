//! Company drill-down: everything on `/company/<SYM>` that isn't price data —
//! profile, equity structure, financials, ratios and announcements.

use anyhow::{Context, Result};
use scraper::{ElementRef, Html, Selector};

use super::client::PsxClient;
use super::parse::{parse_number, text_of};
use crate::model::{Announcement, AnnouncementKind, Company, FinancialPeriod, RatioPeriod};

pub async fn company(client: &PsxClient, symbol: &str) -> Result<Company> {
    let html = client
        .get_text(&format!("/company/{symbol}"))
        .await
        .with_context(|| format!("fetching company page for {symbol}"))?;
    Ok(parse_company(symbol, &html))
}

/// Parse a company page.
///
/// Every section is optional: PSX omits financials for newly listed scrips,
/// ratios for funds, and profile fields for debt instruments. Missing sections
/// yield empty collections rather than errors so a partial page still renders.
pub fn parse_company(symbol: &str, html: &str) -> Company {
    let doc = Html::parse_document(html);
    let mut c = Company {
        symbol: symbol.to_string(),
        ..Default::default()
    };

    // --- Header: company name and sector ---------------------------------
    if let Some(el) = select_one(&doc, ".quote__name") {
        c.name = text_of(el);
    }
    if let Some(el) = select_one(&doc, ".quote__sector") {
        c.sector = text_of(el);
    }

    // --- Quote stats -----------------------------------------------------
    // Scoped to the REG (regular market) tab: the DFC/CSF/ODL futures panels
    // reuse the same labels and would otherwise shadow the cash-market values.
    let reg = select_one(&doc, r#".tabs__panel[data-name="REG"]"#);
    if let Some(reg) = reg {
        c.pe_ratio = stat_value(reg, "P/E Ratio");
        c.change_1y_pct = stat_value(reg, "1-Year Change");
        c.change_ytd_pct = stat_value(reg, "YTD Change");

        if let Some((lo, hi)) = stat_range(reg, "52-WEEK RANGE") {
            c.week52_low = Some(lo);
            c.week52_high = Some(hi);
        }
        if let Some((lo, hi)) = stat_range(reg, "CIRCUIT BREAKER") {
            c.circuit_low = Some(lo);
            c.circuit_high = Some(hi);
        }
    }

    // --- Equity profile --------------------------------------------------
    if let Some(eq) = select_one(&doc, "#equity") {
        c.market_cap_000 = stat_value(eq, "Market Cap");
        c.shares = stat_value(eq, "Shares");

        // "Free Float" appears twice: absolute share count then percentage.
        let floats = stat_values(eq, "Free Float");
        c.free_float = floats.first().copied();
        c.free_float_pct = floats.get(1).copied();
    }

    // --- Company profile -------------------------------------------------
    if let Some(profile) = select_one(&doc, "#profile") {
        let fields = profile_fields(profile);
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        c.business_description = get("BUSINESS DESCRIPTION");
        c.address = get("ADDRESS");
        c.website = get("WEBSITE");
        c.registrar = get("REGISTRAR");
        c.auditor = get("AUDITOR");
        c.fiscal_year_end = get("Fiscal Year End");
        c.key_people = key_people(profile);
    }

    // --- Financials / ratios --------------------------------------------
    c.financials_annual = periods(&doc, "#financialTab", "Annual")
        .into_iter()
        .map(|(period, rows)| FinancialPeriod { period, rows })
        .collect();
    c.financials_quarterly = periods(&doc, "#financialTab", "Quarterly")
        .into_iter()
        .map(|(period, rows)| FinancialPeriod { period, rows })
        .collect();

    // The ratios section is a bare table with no tab wrapper.
    if let Some(section) = select_one(&doc, "#ratios") {
        c.ratios = table_periods(section)
            .into_iter()
            .map(|(period, rows)| RatioPeriod { period, rows })
            .collect();
    }

    // --- Announcements ---------------------------------------------------
    for (tab, kind) in [
        ("Financial Results", AnnouncementKind::FinancialResults),
        ("Board Meetings", AnnouncementKind::BoardMeeting),
        ("Others", AnnouncementKind::Other),
    ] {
        c.announcements.extend(announcements(&doc, tab, kind));
    }

    c
}

// --- selector helpers ----------------------------------------------------

fn select_one<'a>(doc: &'a Html, sel: &str) -> Option<ElementRef<'a>> {
    Selector::parse(sel)
        .ok()
        .and_then(|s| doc.select(&s).next())
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).unwrap()
}

/// Collect every `.stats_item` under `root` as (label, value-element) pairs.
fn stat_items(root: ElementRef) -> Vec<(String, ElementRef)> {
    let item = sel(".stats_item");
    let label = sel(".stats_label");
    let value = sel(".stats_value");

    root.select(&item)
        .filter_map(|it| {
            let l = text_of(it.select(&label).next()?);
            let v = it.select(&value).next()?;
            Some((l, v))
        })
        .collect()
}

/// First numeric stat whose label starts with `label`.
///
/// Prefix matching absorbs PSX's footnote markers — "P/E Ratio (TTM) **",
/// "52-WEEK RANGE ^", "1-Year Change * ^".
fn stat_value(root: ElementRef, label: &str) -> Option<f64> {
    stat_values(root, label).into_iter().next()
}

fn stat_values(root: ElementRef, label: &str) -> Vec<f64> {
    stat_items(root)
        .into_iter()
        .filter(|(l, _)| starts_with_ci(l, label))
        .filter_map(|(_, v)| parse_number(&text_of(v)))
        .collect()
}

/// Read a `low — high` stat from its machine-readable `.numRange` attributes.
fn stat_range(root: ElementRef, label: &str) -> Option<(f64, f64)> {
    let range = sel(".numRange");
    stat_items(root)
        .into_iter()
        .filter(|(l, _)| starts_with_ci(l, label))
        .find_map(|(_, v)| {
            let r = v.select(&range).next()?;
            let lo = r.value().attr("data-low").and_then(parse_number)?;
            let hi = r.value().attr("data-high").and_then(parse_number)?;
            Some((lo, hi))
        })
}

fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

/// Flatten `.profile__item` blocks into (heading, text) pairs.
///
/// A single block can hold more than one heading — AUDITOR and Fiscal Year End
/// share one — so headings and paragraphs are zipped in document order rather
/// than assuming one pair per block.
fn profile_fields(profile: ElementRef) -> Vec<(String, String)> {
    let item = sel(".profile__item");
    let head = sel(".item__head");
    let para = sel("p");

    let mut out = Vec::new();
    for block in profile.select(&item) {
        let heads: Vec<_> = block.select(&head).map(text_of).collect();
        let paras: Vec<_> = block.select(&para).map(text_of).collect();
        for (h, p) in heads.into_iter().zip(paras) {
            out.push((h, p));
        }
    }
    out
}

/// Extract the KEY PEOPLE block as (name, role) pairs.
///
/// Rendered as a two-column table — name in a `<strong>`, role beside it —
/// inside the `.profile__item--people` block.
fn key_people(profile: ElementRef) -> Vec<(String, String)> {
    let block_sel = sel(".profile__item--people");
    let row = sel("tr");
    let cell = sel("td");

    let Some(block) = profile.select(&block_sel).next() else {
        return Vec::new();
    };

    block
        .select(&row)
        .filter_map(|r| {
            let cells: Vec<_> = r.select(&cell).collect();
            if cells.len() < 2 {
                return None;
            }
            let name = text_of(cells[0]);
            if name.is_empty() {
                return None;
            }
            Some((name, text_of(cells[1])))
        })
        .collect()
}

// --- table extraction ----------------------------------------------------

/// Read a column-per-period table into `(period, rows)` tuples.
///
/// The financials markup nests `<tbody>` inside `<thead>`, which different
/// parsers normalise differently, so rows are read positionally from the whole
/// table: the first row supplies period headings, the rest supply labels.
fn table_periods(root: ElementRef) -> Vec<(String, Vec<(String, f64)>)> {
    let table = sel("table");
    let Some(table) = root.select(&table).next() else {
        return Vec::new();
    };

    let tr = sel("tr");
    let cell = sel("th, td");
    let rows: Vec<Vec<String>> = table
        .select(&tr)
        .map(|r| r.select(&cell).map(text_of).collect())
        .collect();

    let Some(header) = rows.first() else {
        return Vec::new();
    };
    // Column 0 is the row-label column and has an empty heading.
    let periods: Vec<String> = header.iter().skip(1).cloned().collect();
    if periods.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<(String, Vec<(String, f64)>)> =
        periods.into_iter().map(|p| (p, Vec::new())).collect();

    for row in rows.iter().skip(1) {
        let Some(label) = row.first() else { continue };
        if label.is_empty() {
            continue;
        }
        for (i, raw) in row.iter().skip(1).enumerate() {
            if let (Some(slot), Some(v)) = (out.get_mut(i), parse_number(raw)) {
                slot.1.push((label.clone(), v));
            }
        }
    }

    out
}

/// Read the table inside a named tab panel under `container`.
fn periods(doc: &Html, container: &str, tab: &str) -> Vec<(String, Vec<(String, f64)>)> {
    let selector = format!(r#"{container} .tabs__panel[data-name="{tab}"]"#);
    select_one(doc, &selector)
        .map(table_periods)
        .unwrap_or_default()
}

fn announcements(doc: &Html, tab: &str, kind: AnnouncementKind) -> Vec<Announcement> {
    let selector = format!(r#"#announcementsTab .tabs__panel[data-name="{tab}"]"#);
    let Some(panel) = select_one(doc, &selector) else {
        return Vec::new();
    };

    let tr = sel("tbody tr");
    let td = sel("td");
    let link = sel("a");

    panel
        .select(&tr)
        .filter_map(|row| {
            let cells: Vec<_> = row.select(&td).collect();
            if cells.len() < 2 {
                return None;
            }
            let date = text_of(cells[0]);
            let title = text_of(cells[1]);
            if title.is_empty() {
                return None;
            }

            // The document cell holds a "View" (inline image) link and a "PDF"
            // link; only the PDF has a resolvable href.
            let pdf_url = cells.get(2).and_then(|c| {
                c.select(&link)
                    .filter_map(|a| a.value().attr("href"))
                    .find(|h| h.ends_with(".pdf"))
                    .map(|h| format!("{}{}", super::client::BASE, h))
            });

            Some(Announcement {
                date,
                title,
                category: kind,
                pdf_url,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"
    <html><body>
      <div class="quote__name">Habib Bank Limited</div>
      <div class="quote__sector">COMMERCIAL BANKS</div>

      <div class="tabs__panel" data-name="REG">
        <div class="stats_item"><div class="stats_label">Open</div><div class="stats_value">294.70</div></div>
        <div class="stats_item"><div class="stats_label">P/E Ratio (TTM) **</div><div class="stats_value">6.82</div></div>
        <div class="stats_item"><div class="stats_label">1-Year Change * ^</div><div class="stats_value">28.06%</div></div>
        <div class="stats_item"><div class="stats_label">YTD Change * ^</div><div class="stats_value">-9.71%</div></div>
        <div class="stats_item"><div class="stats_label">52-WEEK RANGE ^</div>
          <div class="stats_value">214.00 — 369.99<div class="numRange" data-low="214" data-high="369.99"></div></div></div>
        <div class="stats_item"><div class="stats_label">CIRCUIT BREAKER</div>
          <div class="stats_value">265.94 — 325.04<div class="numRange" data-low="265.94" data-high="325.04"></div></div></div>
      </div>
      <div class="tabs__panel" data-name="FUT">
        <div class="stats_item"><div class="stats_label">Open</div><div class="stats_value">999.99</div></div>
        <div class="stats_item"><div class="stats_label">P/E Ratio</div><div class="stats_value">111.11</div></div>
      </div>

      <div id="equity">
        <div class="stats_item"><div class="stats_label">Market Cap (000's)</div><div class="stats_value">428,320,932.34</div></div>
        <div class="stats_item"><div class="stats_label">Shares</div><div class="stats_value">1,466,852,508</div></div>
        <div class="stats_item"><div class="stats_label">Free Float</div><div class="stats_value">586,741,003</div></div>
        <div class="stats_item"><div class="stats_label">Free Float</div><div class="stats_value">40.00%</div></div>
      </div>

      <div id="profile">
        <div class="profile__item"><div class="item__head">BUSINESS DESCRIPTION</div><p>Habib Bank Limited is a commercial bank.</p></div>
        <div class="profile__item profile__item--people"><div class="item__head">KEY PEOPLE</div>
          <table class="tbl"><tbody class="tbl__body">
            <tr><td><strong>Muhammad Nassir Salim</strong></td><td>CEO</td></tr>
            <tr><td><strong>Sultan Ali Allana</strong></td><td>Chairperson</td></tr>
          </tbody></table></div>
        <div class="profile__item"><div class="item__head">WEBSITE</div><p><a href="http://www.hbl.com">www.hbl.com</a></p></div>
        <div class="profile__item">
          <div class="item__head">AUDITOR</div><p>A.F. Ferguson &amp; Co.</p>
          <div class="item__head">Fiscal Year End</div><p>December</p>
        </div>
      </div>

      <div id="financialTab">
        <div class="tabs__panel" data-name="Annual"><table>
          <tr><th></th><th>2025</th><th>2024</th></tr>
          <tr><td>Profit after Taxation</td><td>62,492,417</td><td>56,765,819</td></tr>
          <tr><td>EPS</td><td>42.60</td><td>38.70</td></tr>
        </table></div>
        <div class="tabs__panel" data-name="Quarterly"><table>
          <tr><th></th><th>Q1 2026</th></tr>
          <tr><td>EPS</td><td>10.51</td></tr>
        </table></div>
      </div>

      <div id="ratios"><table>
        <tr><th></th><th>2025</th><th>2024</th></tr>
        <tr><td>Net Profit Margin (%)</td><td>9.84</td><td>7.39</td></tr>
        <tr><td>EPS Growth (%)</td><td>10.08</td><td>(0.15)</td></tr>
      </table></div>

      <div id="announcementsTab">
        <div class="tabs__panel" data-name="Financial Results"><table><tbody>
          <tr><td>Apr 17, 2026</td><td>Announcement of Financial Results Q1 2026</td>
              <td><a href="javascript:" data-images="274357-1.gif">View</a>
                  <a href="/download/document/274357.pdf">PDF</a></td></tr>
        </tbody></table></div>
        <div class="tabs__panel" data-name="Board Meetings"><table><tbody>
          <tr><td>Jul 23, 2026</td><td>Board Meeting for H1 2026 Results</td><td></td></tr>
        </tbody></table></div>
      </div>
    </body></html>"##;

    fn parsed() -> Company {
        parse_company("HBL", PAGE)
    }

    #[test]
    fn reads_header_and_quote_stats() {
        let c = parsed();
        assert_eq!(c.name, "Habib Bank Limited");
        assert_eq!(c.sector, "COMMERCIAL BANKS");
        assert_eq!(c.pe_ratio, Some(6.82));
        assert_eq!(c.change_1y_pct, Some(28.06));
        assert_eq!(c.change_ytd_pct, Some(-9.71));
    }

    #[test]
    fn quote_stats_ignore_the_futures_panel() {
        // The FUT panel repeats "P/E Ratio" with a different value; the cash
        // market figure must win.
        assert_eq!(parsed().pe_ratio, Some(6.82));
    }

    #[test]
    fn reads_ranges_from_data_attributes() {
        let c = parsed();
        assert_eq!(c.week52_low, Some(214.0));
        assert_eq!(c.week52_high, Some(369.99));
        assert_eq!(c.circuit_low, Some(265.94));
        assert_eq!(c.circuit_high, Some(325.04));
    }

    #[test]
    fn distinguishes_free_float_count_from_percentage() {
        let c = parsed();
        assert_eq!(c.market_cap_000, Some(428_320_932.34));
        assert_eq!(c.shares, Some(1_466_852_508.0));
        assert_eq!(c.free_float, Some(586_741_003.0));
        assert_eq!(c.free_float_pct, Some(40.0));
    }

    #[test]
    fn reads_profile_including_two_fields_in_one_block() {
        let c = parsed();
        assert!(c.business_description.starts_with("Habib Bank Limited"));
        assert_eq!(c.website, "www.hbl.com");
        assert_eq!(c.auditor, "A.F. Ferguson & Co.");
        assert_eq!(c.fiscal_year_end, "December");
    }

    #[test]
    fn reads_key_people_from_the_profile_table() {
        let c = parsed();
        assert_eq!(
            c.key_people,
            vec![
                ("Muhammad Nassir Salim".to_string(), "CEO".to_string()),
                ("Sultan Ali Allana".to_string(), "Chairperson".to_string()),
            ]
        );
    }

    #[test]
    fn website_link_text_is_unwrapped_from_the_anchor() {
        assert_eq!(parsed().website, "www.hbl.com");
    }

    #[test]
    fn reads_financials_by_period_column() {
        let c = parsed();
        assert_eq!(c.financials_annual.len(), 2);
        let y2025 = &c.financials_annual[0];
        assert_eq!(y2025.period, "2025");
        assert_eq!(y2025.eps(), Some(42.60));
        assert_eq!(y2025.get("Profit after Taxation"), Some(62_492_417.0));

        assert_eq!(c.financials_annual[1].period, "2024");
        assert_eq!(c.financials_annual[1].eps(), Some(38.70));

        assert_eq!(c.financials_quarterly.len(), 1);
        assert_eq!(c.financials_quarterly[0].period, "Q1 2026");
    }

    #[test]
    fn reads_ratios_with_accounting_negatives() {
        let c = parsed();
        assert_eq!(c.ratios.len(), 2);
        let y2024 = c.ratios.iter().find(|r| r.period == "2024").unwrap();
        let growth = y2024
            .rows
            .iter()
            .find(|(k, _)| k.starts_with("EPS Growth"))
            .unwrap();
        assert_eq!(growth.1, -0.15, "(0.15) must parse as negative");
    }

    #[test]
    fn reads_announcements_with_pdf_links() {
        let c = parsed();
        assert_eq!(c.announcements.len(), 2);

        let fin = &c.announcements[0];
        assert_eq!(fin.category, AnnouncementKind::FinancialResults);
        assert_eq!(fin.date, "Apr 17, 2026");
        assert_eq!(
            fin.pdf_url.as_deref(),
            Some("https://dps.psx.com.pk/download/document/274357.pdf"),
            "should pick the PDF link, not the javascript: View link"
        );

        let board = &c.announcements[1];
        assert_eq!(board.category, AnnouncementKind::BoardMeeting);
        assert_eq!(board.pdf_url, None);
    }

    #[test]
    fn missing_sections_do_not_panic() {
        let c = parse_company("XYZ", "<html><body>nothing here</body></html>");
        assert_eq!(c.symbol, "XYZ");
        assert!(c.financials_annual.is_empty());
        assert!(c.announcements.is_empty());
        assert_eq!(c.pe_ratio, None);
    }
}
