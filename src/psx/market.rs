//! Market-wide scrapes: the live market-watch board, end-of-day OHLC
//! snapshots, and the index ticker.

use anyhow::{Context, Result};
use scraper::{Html, Selector};

use super::client::PsxClient;
use super::parse::{cell_number, parse_number, text_of};
use crate::model::{Bar, Index, Quote};

/// One row of the daily `POST /historical` snapshot: full OHLC for a symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalRow {
    pub symbol: String,
    pub ldcp: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

impl HistoricalRow {
    pub fn to_bar(&self, ts: i64) -> Bar {
        Bar {
            ts,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
        }
    }
}

/// Fetch the live market-watch board — every listed scrip's current session.
pub async fn market_watch(client: &PsxClient) -> Result<Vec<Quote>> {
    let html = client
        .get_text("/market-watch")
        .await
        .context("fetching market watch")?;
    parse_market_watch(&html)
}

/// Fetch full OHLC for every symbol on a given trading date (`YYYY-MM-DD`).
///
/// PSX returns an empty table for weekends and holidays; that surfaces as an
/// empty `Vec` rather than an error so callers can skip non-trading days.
pub async fn historical(client: &PsxClient, date: &str) -> Result<Vec<HistoricalRow>> {
    let html = client
        .post_form_text("/historical", &[("date", date)])
        .await
        .with_context(|| format!("fetching historical snapshot for {date}"))?;
    parse_historical(&html)
}

/// Fetch the index ticker (KSE100, ALLSHR, KMI30, sector indices, ...).
///
/// `/market-watch` returns a bare table fragment with no ticker, so this reads
/// the full `/indices` page, which carries the same ticker markup.
pub async fn indices(client: &PsxClient) -> Result<Vec<Index>> {
    let html = client
        .get_text("/indices")
        .await
        .context("fetching indices")?;
    Ok(parse_indices(&html))
}

pub fn parse_market_watch(html: &str) -> Result<Vec<Quote>> {
    let doc = Html::parse_document(html);
    let row_sel = Selector::parse("tbody.tbl__body tr").unwrap();
    let cell_sel = Selector::parse("td").unwrap();
    let sym_sel = Selector::parse("a.tbl__symbol").unwrap();

    let mut quotes = Vec::new();
    for row in doc.select(&row_sel) {
        let cells: Vec<_> = row.select(&cell_sel).collect();
        // symbol, sector, listed-in, ldcp, open, high, low, current, change, %, volume
        if cells.len() < 11 {
            continue;
        }

        let symbol = match row.select(&sym_sel).next() {
            Some(a) => text_of(a),
            // Fall back to the cell's data-search attribute for rows that
            // aren't linked to a company page.
            None => cells[0]
                .value()
                .attr("data-search")
                .map(str::to_string)
                .unwrap_or_else(|| text_of(cells[0])),
        };
        if symbol.is_empty() {
            continue;
        }

        let indices = text_of(cells[2])
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        quotes.push(Quote {
            symbol,
            sector: text_of(cells[1]),
            indices,
            ldcp: cell_number(cells[3]).unwrap_or(0.0),
            open: cell_number(cells[4]).unwrap_or(0.0),
            high: cell_number(cells[5]).unwrap_or(0.0),
            low: cell_number(cells[6]).unwrap_or(0.0),
            current: cell_number(cells[7]).unwrap_or(0.0),
            change: cell_number(cells[8]).unwrap_or(0.0),
            change_pct: cell_number(cells[9]).unwrap_or(0.0),
            volume: cell_number(cells[10]).unwrap_or(0.0),
        });
    }

    Ok(quotes)
}

pub fn parse_historical(html: &str) -> Result<Vec<HistoricalRow>> {
    let doc = Html::parse_fragment(html);
    let row_sel = Selector::parse("tbody tr").unwrap();
    let cell_sel = Selector::parse("td").unwrap();

    let mut rows = Vec::new();
    for row in doc.select(&row_sel) {
        let cells: Vec<_> = row.select(&cell_sel).collect();
        // symbol, ldcp, open, high, low, close, change, %change, volume
        if cells.len() < 9 {
            continue;
        }

        let symbol = cells[0]
            .value()
            .attr("data-value")
            .map(str::to_string)
            .unwrap_or_else(|| text_of(cells[0]));
        if symbol.is_empty() {
            continue;
        }

        let (Some(open), Some(high), Some(low), Some(close)) = (
            cell_number(cells[2]),
            cell_number(cells[3]),
            cell_number(cells[4]),
            cell_number(cells[5]),
        ) else {
            continue;
        };

        // Suspended or never-traded scrips report a full row of zeros; they
        // would otherwise poison ranges and indicator warm-ups.
        if close <= 0.0 {
            continue;
        }

        rows.push(HistoricalRow {
            symbol,
            ldcp: cell_number(cells[1]).unwrap_or(0.0),
            open,
            high,
            low,
            close,
            volume: cell_number(cells[8]).unwrap_or(0.0),
        });
    }

    Ok(rows)
}

pub fn parse_indices(html: &str) -> Vec<Index> {
    let doc = Html::parse_document(html);
    let item_sel = Selector::parse(".topIndices__item").unwrap();
    let name_sel = Selector::parse(".topIndices__item__name").unwrap();
    let val_sel = Selector::parse(".topIndices__item__val").unwrap();
    let chg_sel = Selector::parse(".topIndices__item__change").unwrap();
    let chgp_sel = Selector::parse(".topIndices__item__changep").unwrap();

    doc.select(&item_sel)
        .filter_map(|item| {
            let name = text_of(item.select(&name_sel).next()?);
            let value = parse_number(&text_of(item.select(&val_sel).next()?))?;
            // Stale indices render a date where the change would be; those
            // parse to None and are reported as unchanged rather than dropped.
            let change = item
                .select(&chg_sel)
                .next()
                .map(text_of)
                .and_then(|t| parse_number(&t))
                .unwrap_or(0.0);
            let change_pct = item
                .select(&chgp_sel)
                .next()
                .map(text_of)
                .and_then(|t| parse_number(&t))
                .unwrap_or(0.0);

            Some(Index {
                name,
                value,
                change,
                change_pct,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKET_ROW: &str = r#"
    <table><tbody class="tbl__body">
    <tr>
      <td data-search="CNERGY" data-order="CNERGY"><a class="tbl__symbol" href="/company/CNERGY" data-title="Cnergyico PK Limited"><strong>CNERGY</strong></a></td>
      <td>0825</td>
      <td>ALLSHR,KMIALLSHR,KSE100</td>
      <td class="right" data-order="10.1">10.10</td>
      <td class="right" data-order="10.11">10.11</td>
      <td class="right" data-order="10.59">10.59</td>
      <td class="right" data-order="10.05">10.05</td>
      <td class="right" data-order="10.29">10.29</td>
      <td class="right" data-order="0.19"><i class="icon-up-dir"></i> 0.19</td>
      <td class="right" data-order="1.881"><i class="icon-up-dir"></i> 1.88%</td>
      <td class="right" data-order="59327773">59,327,773</td>
    </tr>
    </tbody></table>"#;

    #[test]
    fn parses_market_watch_row() {
        let quotes = parse_market_watch(MARKET_ROW).unwrap();
        assert_eq!(quotes.len(), 1);
        let q = &quotes[0];
        assert_eq!(q.symbol, "CNERGY");
        assert_eq!(q.sector, "0825");
        assert_eq!(q.indices, vec!["ALLSHR", "KMIALLSHR", "KSE100"]);
        assert_eq!(q.ldcp, 10.10);
        assert_eq!(q.high, 10.59);
        assert_eq!(q.current, 10.29);
        assert_eq!(q.change_pct, 1.881);
        assert_eq!(q.volume, 59_327_773.0);
    }

    const HISTORICAL_ROWS: &str = r#"
    <table id="historicalTable"><tbody>
    <tr data-type="equity">
      <td data-value="AABS"><strong>AABS</strong></td>
      <td class="right" data-value="917.96">917.96</td>
      <td class="right" data-value="889.02">889.02</td>
      <td class="right" data-value="920">920.00</td>
      <td class="right" data-value="880">880.00</td>
      <td class="right" data-value="900.82">900.82</td>
      <td class="right" data-value="-17.14">-17.14</td>
      <td class="right" data-value="-1.867">-1.87%</td>
      <td class="right" data-value="12345">12,345</td>
    </tr>
    <tr data-type="equity">
      <td data-value="DEAD"><strong>DEAD</strong></td>
      <td class="right" data-value="0">0.00</td>
      <td class="right" data-value="0">0.00</td>
      <td class="right" data-value="0">0.00</td>
      <td class="right" data-value="0">0.00</td>
      <td class="right" data-value="0">0.00</td>
      <td class="right" data-value="0">0.00</td>
      <td class="right" data-value="0">0.00%</td>
      <td class="right" data-value="0">0</td>
    </tr>
    </tbody></table>"#;

    #[test]
    fn parses_historical_ohlc_and_drops_untraded() {
        let rows = parse_historical(HISTORICAL_ROWS).unwrap();
        assert_eq!(rows.len(), 1, "zero-close row should be dropped");
        let r = &rows[0];
        assert_eq!(r.symbol, "AABS");
        assert_eq!(r.open, 889.02);
        assert_eq!(r.high, 920.0);
        assert_eq!(r.low, 880.0);
        assert_eq!(r.close, 900.82);
        assert_eq!(r.volume, 12_345.0);
    }

    const INDEX_ITEMS: &str = r#"
    <div>
      <div class="topIndices__item">
        <div><div class="topIndices__item__name">KSE100</div>
        <div class="topIndices__item__val">171,021.20</div></div>
        <div class="change__text--neg">
          <div class="topIndices__item__change"><i class="icon-down-dir"></i> -718.24</div>
          <div class="topIndices__item__changep">(-0.42%)</div>
        </div>
      </div>
      <div class="topIndices__item">
        <div><div class="topIndices__item__name">HBLTTI</div>
        <div class="topIndices__item__val">18,765.90</div></div>
        <div><div class="topIndices__item__change">23-07-2026</div>
        <div class="topIndices__item__changep">(0.00%)</div></div>
      </div>
    </div>"#;

    #[test]
    fn parses_index_ticker() {
        let idx = parse_indices(INDEX_ITEMS);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[0].name, "KSE100");
        assert_eq!(idx[0].value, 171_021.20);
        assert_eq!(idx[0].change, -718.24);
        assert_eq!(idx[0].change_pct, -0.42);
    }

    #[test]
    fn stale_index_with_date_instead_of_change_is_kept() {
        let idx = parse_indices(INDEX_ITEMS);
        let hbltti = idx.iter().find(|i| i.name == "HBLTTI").unwrap();
        assert_eq!(hbltti.value, 18_765.90);
        assert_eq!(
            hbltti.change, 0.0,
            "unparseable date must not drop the index"
        );
    }
}
