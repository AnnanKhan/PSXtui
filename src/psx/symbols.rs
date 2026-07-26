//! The `/symbols` feed — the master list of every listed instrument.

use anyhow::{Context, Result};

use super::client::PsxClient;
use crate::model::SymbolInfo;

/// Fetch every listed instrument: equities, ETFs, and debt (TFCs, bonds).
pub async fn symbols(client: &PsxClient) -> Result<Vec<SymbolInfo>> {
    let body = client
        .get_text("/symbols")
        .await
        .context("fetching symbol list")?;
    parse_symbols(&body)
}

pub fn parse_symbols(body: &str) -> Result<Vec<SymbolInfo>> {
    serde_json::from_str(body).context("decoding symbol list JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_symbol_list() {
        let body = r#"[
          {"symbol":"AKBLTFC6","name":"Askari Bank(TFC6)","sectorName":"BILLS AND BONDS","isETF":false,"isDebt":true},
          {"symbol":"HBL","name":"Habib Bank Limited","sectorName":"COMMERCIAL BANKS","isETF":false,"isDebt":false}
        ]"#;
        let syms = parse_symbols(body).unwrap();
        assert_eq!(syms.len(), 2);
        assert!(syms[0].is_debt);
        assert_eq!(syms[1].symbol, "HBL");
        assert_eq!(syms[1].sector_name, "COMMERCIAL BANKS");
        assert!(!syms[1].is_etf);
    }
}
