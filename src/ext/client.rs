//! Rate-limited HTTP client for the external context feeds.
//!
//! Mirrors [`crate::psx::client::PsxClient`]: one request at a time, a minimum
//! gap between them, a real browser User-Agent, a hard timeout, and bounded
//! retries with exponential backoff. The difference is that this client talks
//! to several hosts rather than one, so it takes absolute URLs.
//!
//! Yahoo and Cloudflare-fronted news sites both reject the default reqwest
//! agent, so the User-Agent here is a plain Chrome string with no suffix.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio::time::Instant;

/// A browser User-Agent. Yahoo's chart API returns 4xx without one.
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/124.0.0.0 Safari/537.36",
);

/// Minimum spacing between outbound requests.
const MIN_REQUEST_GAP: Duration = Duration::from_millis(400);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETRIES: u32 = 3;

pub struct ExtClient {
    http: reqwest::Client,
    /// Timestamp of the last request, used to enforce [`MIN_REQUEST_GAP`].
    last_request: Mutex<Option<Instant>>,
}

impl ExtClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .gzip(true)
            .build()
            .context("building external HTTP client")?;

        Ok(Self {
            http,
            last_request: Mutex::new(None),
        })
    }

    /// Sleep as needed so consecutive requests are at least [`MIN_REQUEST_GAP`]
    /// apart. The lock is held across the wait so concurrent callers queue
    /// rather than all firing at once.
    async fn throttle(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_REQUEST_GAP {
                tokio::time::sleep(MIN_REQUEST_GAP - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// GET an absolute `url` and return the body as text.
    pub async fn get_text(&self, url: &str) -> Result<String> {
        let mut last_err = None;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                // Exponential backoff: 500ms, 1s.
                tokio::time::sleep(Duration::from_millis(500 * (1 << (attempt - 1)))).await;
            }
            self.throttle().await;

            let req = self
                .http
                .get(url)
                .header("Accept", "application/json, text/xml, text/html, */*")
                .header("Accept-Language", "en-US,en;q=0.9");

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.text().await.context("reading response body");
                    }
                    // 404 on an unknown ticker, 403 on a blocked agent: neither
                    // improves with a retry. 429 does, once the gap has passed.
                    if status.is_client_error() && status.as_u16() != 429 {
                        anyhow::bail!("{url} returned {status}");
                    }
                    last_err = Some(anyhow::anyhow!("{url} returned {status}"));
                }
                Err(e) => {
                    last_err = Some(anyhow::Error::new(e).context(format!("requesting {url}")))
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("{url}: exhausted retries")))
    }
}

/// Percent-encode a path segment.
///
/// Yahoo tickers carry characters that are otherwise meaningful in a URL —
/// `^GSPC` for an index, `BZ=F` for a futures contract — so they must be
/// escaped rather than interpolated raw.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_yahoo_ticker_symbols() {
        assert_eq!(encode_segment("^GSPC"), "%5EGSPC");
        assert_eq!(encode_segment("BZ=F"), "BZ%3DF");
        assert_eq!(encode_segment("PKR=X"), "PKR%3DX");
        assert_eq!(encode_segment("BDRY"), "BDRY");
    }

    #[test]
    fn user_agent_looks_like_a_browser() {
        assert!(USER_AGENT.starts_with("Mozilla/5.0"));
    }

    #[test]
    fn client_builds() {
        assert!(ExtClient::new().is_ok());
    }
}
