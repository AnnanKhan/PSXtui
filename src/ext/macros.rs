//! The State Bank of Pakistan policy rate.
//!
//! Every risk-adjusted statistic in the app — Sharpe, Sortino — is an excess
//! return over a risk-free rate, and in a market whose policy rate has spent
//! recent years in double digits, hardcoding that constant is the difference
//! between "this scrip beat cash" and "this scrip did not". SBP publishes the
//! current rate on its homepage, so it is scraped from there.
//!
//! The markup around it changes; the sentence does not. Tag-stripped, the page
//! reads `… SBP Policy Rate 11.50% p.a. …`, so the extraction works over the
//! plain text rather than a CSS selector, and falls back to a documented
//! constant when it finds nothing.

use serde::{Deserialize, Serialize};

use super::client::ExtClient;

pub const SBP_URL: &str = "https://www.sbp.org.pk/";

/// Used when the scrape fails. Kept close to the prevailing rate so a failed
/// fetch degrades to "slightly stale" rather than "wrong by a factor".
pub const FALLBACK_POLICY_RATE_PCT: f64 = 11.5;

/// Policy rates feeding the analysis screens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MacroRates {
    /// The SBP policy rate, in percent per annum (e.g. `11.5`).
    pub policy_rate_pct: f64,
    /// Whether the value came from a live scrape. `false` means
    /// [`FALLBACK_POLICY_RATE_PCT`], and the UI says so.
    pub fetched: bool,
}

impl Default for MacroRates {
    fn default() -> Self {
        Self {
            policy_rate_pct: FALLBACK_POLICY_RATE_PCT,
            fetched: false,
        }
    }
}

impl MacroRates {
    /// The rate as a fraction, ready for [`crate::analysis::stats::sharpe_ratio`].
    pub fn risk_free(&self) -> f64 {
        let r = self.policy_rate_pct / 100.0;
        if r.is_finite() && (0.0..1.0).contains(&r) {
            r
        } else {
            FALLBACK_POLICY_RATE_PCT / 100.0
        }
    }
}

/// Fetch the homepage and extract the policy rate.
///
/// Never fails: an unreachable SBP yields the fallback with `fetched: false`,
/// because a missing rate must not blank a screen that is otherwise useful.
pub async fn fetch_rates(client: &ExtClient) -> MacroRates {
    match client.get_text(SBP_URL).await {
        Ok(html) => match parse_policy_rate(&html) {
            Some(pct) => MacroRates {
                policy_rate_pct: pct,
                fetched: true,
            },
            None => MacroRates::default(),
        },
        Err(_) => MacroRates::default(),
    }
}

/// Pull the policy rate out of the SBP homepage.
///
/// The phrase appears twice: once inside the heading "SBP Policy Rate &
/// Interest Rate Corridor Facilities" and once as the label immediately before
/// the number. Both are matched, and the occurrence whose percentage is
/// *closest* wins — that is the label, not the heading.
pub fn parse_policy_rate(html: &str) -> Option<f64> {
    let text = strip_tags(html);
    let hay = text.to_ascii_lowercase();
    const NEEDLE: &str = "policy rate";
    /// Characters to look ahead for the number. Long enough to clear the
    /// heading's trailing words, short enough not to capture an unrelated
    /// percentage further down the page.
    const WINDOW: usize = 140;

    let mut best: Option<(usize, f64)> = None;
    let mut from = 0usize;

    while let Some(rel) = hay[from..].find(NEEDLE) {
        let start = from + rel + NEEDLE.len();
        from = start;
        let end = (start + WINDOW).min(text.len());
        // `text` is built by `strip_tags` from `html`; slice on a char boundary.
        let Some(slice) = text.get(start..end) else {
            continue;
        };
        if let Some((offset, value)) = first_percentage(slice)
            && best.is_none_or(|(d, _)| offset < d)
        {
            best = Some((offset, value));
        }
    }

    best.map(|(_, v)| v)
        .filter(|v| v.is_finite() && (0.0..100.0).contains(v))
}

/// The first `NN.NN%` in `s`, with its offset.
fn first_percentage(s: &str) -> Option<(usize, f64)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        let num = &s[start..i];
        // Allow "11.50 %" as well as "11.50%".
        let mut j = i;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        if bytes.get(j) == Some(&b'%')
            && let Ok(v) = num.trim_end_matches('.').parse::<f64>()
        {
            return Some((start, v));
        }
    }
    None
}

/// Collapse HTML to its visible text.
///
/// `<script>` and `<style>` bodies are dropped outright — the SBP page embeds
/// JSON blobs that contain percentages of their own.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;

    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        out.push(' ');
        let after = &rest[at..];

        let lower_start: String = after
            .chars()
            .take(8)
            .collect::<String>()
            .to_ascii_lowercase();
        let skip_to = if lower_start.starts_with("<script") {
            Some("</script")
        } else if lower_start.starts_with("<style") {
            Some("</style")
        } else {
            None
        };

        if let Some(close) = skip_to {
            match after.to_ascii_lowercase().find(close) {
                Some(end) => {
                    rest = &after[end..];
                    continue;
                }
                None => return finish(out),
            }
        }

        match after.find('>') {
            Some(end) => rest = &after[end + 1..],
            None => return finish(out),
        }
    }
    out.push_str(rest);
    finish(out)
}

fn finish(s: String) -> String {
    let decoded = super::news::decode_entities(&s);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from the live homepage: the phrase appears in a heading first,
    /// then again as the label beside the value.
    const SBP: &str = r#"<!DOCTYPE html><html><head>
        <style>.a{width:99.9%}</style>
        <script>var conf={"share":"45.00%"};</script>
        </head><body>
        <div class="swiper-slide">
          <div class="box d-flex flex-column">
            <h4 class="eco-data-title primary-color mb-0">SBP Policy Rate &amp; Interest Rate Corridor Facilities</h4>
            <hr>
            <div class="d-flex align-items-center gap-2 mt-auto">
              <p class="text-black-custom mb-0 fw-medium">SBP Policy Rate</p>
              <span class="bright-green ms-auto">11.50%</span>
              <p class="text-black-custom mb-0 fw-medium">p.a.</p>
            </div>
          </div>
        </div>
        <div>Inflation (YoY) 4.10%</div>
        </body></html>"#;

    #[test]
    fn extracts_the_policy_rate_from_the_homepage() {
        assert_eq!(parse_policy_rate(SBP), Some(11.5));
    }

    #[test]
    fn tag_stripping_produces_the_documented_sentence() {
        let text = strip_tags(SBP);
        assert!(text.contains("SBP Policy Rate 11.50% p.a."), "got: {text}");
        // Script and style bodies must not leak their own percentages.
        assert!(!text.contains("45.00%"));
        assert!(!text.contains("99.9%"));
    }

    #[test]
    fn ignores_unrelated_percentages() {
        // A page where the only nearby number is the right one.
        let html = "<p>Inflation 4.10%</p><p>SBP Policy Rate</p><span>12.25%</span>";
        assert_eq!(parse_policy_rate(html), Some(12.25));
    }

    #[test]
    fn tolerates_a_space_before_the_percent_sign() {
        assert_eq!(
            parse_policy_rate("<p>SBP Policy Rate</p><b>10.5 %</b> p.a."),
            Some(10.5)
        );
    }

    #[test]
    fn a_page_without_the_rate_yields_none() {
        assert_eq!(
            parse_policy_rate("<html><body>Maintenance</body></html>"),
            None
        );
        assert_eq!(parse_policy_rate(""), None);
        // Present but nonsensical values are rejected rather than displayed.
        assert_eq!(parse_policy_rate("SBP Policy Rate 4300.00%"), None);
    }

    #[test]
    fn unterminated_markup_does_not_hang_or_panic() {
        assert!(parse_policy_rate("<div class=\"x").is_none());
        assert!(parse_policy_rate("<script>forever").is_none());
        assert!(strip_tags("<<<>>>").len() < 20);
    }

    #[test]
    fn the_fallback_is_used_when_nothing_is_scraped() {
        let r = MacroRates::default();
        assert!(!r.fetched);
        assert_eq!(r.policy_rate_pct, FALLBACK_POLICY_RATE_PCT);
        assert!((r.risk_free() - 0.115).abs() < 1e-12);
    }

    #[test]
    fn risk_free_rejects_impossible_rates() {
        let bad = MacroRates {
            policy_rate_pct: f64::NAN,
            fetched: true,
        };
        assert!(bad.risk_free().is_finite());
        let silly = MacroRates {
            policy_rate_pct: 1_000.0,
            fetched: true,
        };
        assert!((silly.risk_free() - 0.115).abs() < 1e-12);
    }
}
