//! HTTP access to PSX's data portal (`dps.psx.com.pk`).
//!
//! The portal has no public API and no documented rate limits, so this client
//! deliberately behaves like a single human browsing: one request at a time,
//! a minimum gap between requests, a real User-Agent, and bounded retries.
//! Everything it fetches is cached upstream by [`crate::cache`] so a running
//! session issues far fewer requests than screens rendered.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio::time::Instant;

pub const BASE: &str = "https://dps.psx.com.pk";

const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/124.0.0.0 Safari/537.36 psxtui/",
    env!("CARGO_PKG_VERSION"),
);

/// Minimum spacing between outbound requests.
const MIN_REQUEST_GAP: Duration = Duration::from_millis(350);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETRIES: u32 = 3;

pub struct PsxClient {
    http: reqwest::Client,
    /// Timestamp of the last request, used to enforce [`MIN_REQUEST_GAP`].
    last_request: Mutex<Option<Instant>>,
}

impl PsxClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .gzip(true)
            .build()
            .context("building HTTP client")?;

        Ok(Self {
            http,
            last_request: Mutex::new(None),
        })
    }

    /// Sleep as needed so consecutive requests are at least [`MIN_REQUEST_GAP`]
    /// apart. Held across the wait so concurrent callers queue rather than all
    /// firing at once.
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

    /// GET `path` (relative to [`BASE`]) and return the body as text.
    pub async fn get_text(&self, path: &str) -> Result<String> {
        self.request_text(path, None).await
    }

    /// POST `path` with a form body and return the response as text.
    pub async fn post_form_text(&self, path: &str, form: &[(&str, &str)]) -> Result<String> {
        let encoded = serde_urlencoded_lite(form);
        self.request_text(path, Some(encoded)).await
    }

    async fn request_text(&self, path: &str, form_body: Option<String>) -> Result<String> {
        let url = format!("{BASE}{path}");
        let mut last_err = None;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                // Exponential backoff: 500ms, 1s.
                tokio::time::sleep(Duration::from_millis(500 * (1 << (attempt - 1)))).await;
            }
            self.throttle().await;

            let mut req = match &form_body {
                Some(body) => self
                    .http
                    .post(&url)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body.clone()),
                None => self.http.get(&url),
            };
            // The portal serves different markup to XHR callers for some routes
            // and is happier when it sees a same-origin referer.
            req = req
                .header("X-Requested-With", "XMLHttpRequest")
                .header("Referer", BASE);

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.text().await.context("reading response body");
                    }
                    // 4xx other than 429 won't improve with a retry.
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

/// Minimal `application/x-www-form-urlencoded` encoder.
///
/// The only form values sent are ISO dates and ticker symbols, so this covers
/// the needed cases without pulling in another dependency.
fn serde_urlencoded_lite(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_form_pairs() {
        assert_eq!(
            serde_urlencoded_lite(&[("date", "2026-07-24"), ("sym", "HBL")]),
            "date=2026-07-24&sym=HBL"
        );
    }

    #[test]
    fn escapes_reserved_characters() {
        assert_eq!(percent_encode("a b&c=d"), "a+b%26c%3Dd");
    }
}
