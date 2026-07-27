//! Business headlines from the Business Recorder and Dawn RSS feeds.
//!
//! Both feeds are plain RSS 2.0 over HTTP with no key. Two details of the real
//! markup drive the parser:
//!
//! - Items are emitted as `<item xmlns:default="...">`, i.e. **with attributes**,
//!   so a naive search for the literal `<item>` finds nothing at all.
//! - Titles are sometimes CDATA-wrapped and routinely carry HTML entities
//!   (`&amp;`, `&#8217;`), which must be decoded before they reach a terminal.
//!
//! A full XML parser would be overkill for four fields; what matters is that the
//! extraction is total — a malformed item is skipped, never fatal.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::client::ExtClient;

/// The feeds polled, newest-first after merging.
pub const FEEDS: &[(&str, &str)] = &[
    (
        "Business Recorder",
        "https://www.brecorder.com/feeds/markets",
    ),
    ("Dawn", "https://www.dawn.com/feeds/business"),
];

/// One headline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Headline {
    pub title: String,
    pub url: String,
    /// The raw RSS `pubDate`, kept verbatim so nothing is invented if it fails
    /// to parse. Use [`Headline::when`] for display.
    pub published: String,
    pub source: String,
}

impl Headline {
    /// Publication time as a Unix timestamp, if the `pubDate` parses.
    pub fn timestamp(&self) -> Option<i64> {
        chrono::DateTime::parse_from_rfc2822(self.published.trim())
            .ok()
            .map(|d| d.timestamp())
    }

    /// A compact local-time stamp for the news panel, e.g. `26 Jul 19:21`.
    /// Falls back to the raw string when the date is unparseable.
    pub fn when(&self) -> String {
        match chrono::DateTime::parse_from_rfc2822(self.published.trim()) {
            Ok(d) => d
                .with_timezone(&crate::cache::pkt())
                .format("%d %b %H:%M")
                .to_string(),
            Err(_) => self.published.trim().chars().take(12).collect(),
        }
    }
}

/// Fetch every feed in [`FEEDS`] and merge them, newest first.
///
/// A feed that fails is reported but does not suppress the others.
pub async fn fetch_headlines(client: &ExtClient) -> (Vec<Headline>, Vec<String>) {
    let mut all = Vec::new();
    let mut errors = Vec::new();
    for (source, url) in FEEDS {
        match client.get_text(url).await {
            Ok(body) => all.extend(parse_rss(&body, source)),
            Err(e) => errors.push(format!("{source}: {e}")),
        }
    }
    sort_newest_first(&mut all);
    (all, errors)
}

/// Convenience wrapper returning only the headlines.
pub async fn fetch(client: &ExtClient) -> Result<Vec<Headline>> {
    let (items, errors) = fetch_headlines(client).await;
    if items.is_empty() && !errors.is_empty() {
        anyhow::bail!("{}", errors.join("; "));
    }
    Ok(items)
}

/// Sort in place, newest first. Items with an unparseable date sink to the
/// bottom rather than jumping to the top on a zero timestamp.
pub fn sort_newest_first(items: &mut [Headline]) {
    items.sort_by(|a, b| match (a.timestamp(), b.timestamp()) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
}

/// Parse an RSS body into headlines, newest first.
pub fn parse_rss(xml: &str, source: &str) -> Vec<Headline> {
    let mut out = Vec::new();

    for raw in items(xml) {
        let title = field(raw, "title").unwrap_or_default();
        let link = field(raw, "link").unwrap_or_default();
        if title.is_empty() && link.is_empty() {
            continue;
        }
        out.push(Headline {
            title,
            url: link,
            published: field(raw, "pubDate").unwrap_or_default(),
            source: source.to_string(),
        });
    }

    sort_newest_first(&mut out);
    out
}

/// Slice out each `<item …>…</item>` body.
///
/// The opening tag is matched as `<item` followed by whitespace or `>`, so both
/// the bare form and the attribute-carrying form both feeds actually emit are
/// found — and `<itemDescription>` is not mistaken for one.
fn items(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = xml;

    while let Some(at) = rest.find("<item") {
        let after = &rest[at + 5..];
        let Some(next) = after.chars().next() else {
            break;
        };
        if !(next.is_whitespace() || next == '>') {
            // e.g. <itemDescription> — not an item.
            rest = after;
            continue;
        }
        let Some(open_end) = after.find('>') else {
            break;
        };
        let body = &after[open_end + 1..];
        match body.find("</item>") {
            Some(close) => {
                out.push(&body[..close]);
                rest = &body[close + 7..];
            }
            None => break,
        }
    }
    out
}

/// Read one child element's text, unwrapping CDATA and decoding entities.
fn field(item: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");

    let mut rest = item;
    loop {
        let at = rest.find(&open)?;
        let after = &rest[at + open.len()..];
        let next = after.chars().next()?;
        if !(next.is_whitespace() || next == '>' || next == '/') {
            // <linkAlternate> is not <link>.
            rest = after;
            continue;
        }
        let open_end = after.find('>')?;
        // A self-closing <link/> carries no text.
        if after[..open_end].ends_with('/') {
            rest = &after[open_end + 1..];
            continue;
        }
        let body = &after[open_end + 1..];
        let end = body.find(&close)?;
        return Some(clean(&body[..end]));
    }
}

/// Strip CDATA wrapping, decode entities, and collapse whitespace.
fn clean(raw: &str) -> String {
    let s = raw.trim();
    let s = match s
        .strip_prefix("<![CDATA[")
        .and_then(|r| r.strip_suffix("]]>"))
    {
        Some(inner) => inner,
        None => s,
    };
    let decoded = decode_entities(s);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decode the XML/HTML entities these feeds actually emit.
///
/// Named entities beyond the XML five are rare here; numeric references are
/// not, because both publishers use curly quotes and dashes.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != '&' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // An entity is short; anything longer is a stray ampersand.
        let end = (i + 1..bytes.len().min(i + 12)).find(|&j| bytes[j] == ';');
        let Some(end) = end else {
            out.push('&');
            i += 1;
            continue;
        };
        let name: String = bytes[i + 1..end].iter().collect();
        let replacement = match name.as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            other => other
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match replacement {
            Some(c) => {
                out.push(c);
                i = end + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape: attributes on `<item>`, entity-laden titles, RFC 2822
    /// dates with a +0500 offset.
    const BR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss xmlns:media="http://search.yahoo.com/mrss/" version="2.0">
  <channel>
    <title>Business Recorder - Markets</title>
    <link>https://www.brecorder.com/</link>
    <pubDate>Sun, 26 Jul 2026 19:21:13 +0500</pubDate>
    <item xmlns:default="http://purl.org/rss/1.0/modules/content/">
      <title>Oil &amp; gas sector leads gains as Brent tops $96</title>
      <link>https://www.brecorder.com/news/40431871/oil-gas-sector-leads-gains</link>
      <description>&lt;p&gt;KARACHI: markets rallied.&lt;/p&gt;</description>
      <pubDate>Sun, 26 Jul 2026 18:02:00 +0500</pubDate>
    </item>
    <item xmlns:default="http://purl.org/rss/1.0/modules/content/">
      <title><![CDATA[Rupee holds at 277.59 against the dollar]]></title>
      <link>https://www.brecorder.com/news/40431870/rupee-holds</link>
      <pubDate>Sat, 25 Jul 2026 11:30:00 +0500</pubDate>
    </item>
  </channel>
</rss>"#;

    const DAWN: &str = r#"<rss version="2.0"><channel>
    <item xmlns:default="http://purl.org/rss/1.0/modules/content/">
      <title>Uncertainty in Gulf likely to keep policy rate unchanged
</title>
      <link>https://www.dawn.com/news/2018392/uncertainty-in-gulf</link>
      <pubDate>Sun, 26 Jul 2026 20:15:00 +0500</pubDate>
    </item>
</channel></rss>"#;

    #[test]
    fn parses_items_that_carry_attributes() {
        let items = parse_rss(BR, "Business Recorder");
        assert_eq!(items.len(), 2, "`<item ...>` must match, not just `<item>`");
        assert_eq!(items[0].source, "Business Recorder");
        assert!(items[0].url.starts_with("https://www.brecorder.com/news/"));
    }

    #[test]
    fn decodes_entities_and_unwraps_cdata_in_titles() {
        let items = parse_rss(BR, "Business Recorder");
        assert_eq!(
            items[0].title,
            "Oil & gas sector leads gains as Brent tops $96"
        );
        assert_eq!(items[1].title, "Rupee holds at 277.59 against the dollar");
        assert!(items.iter().all(|h| !h.title.contains("CDATA")));
    }

    #[test]
    fn newest_first_across_a_merged_set() {
        let mut all = parse_rss(BR, "Business Recorder");
        all.extend(parse_rss(DAWN, "Dawn"));
        sort_newest_first(&mut all);

        assert_eq!(all[0].source, "Dawn", "20:15 beats 18:02");
        assert_eq!(all[2].title, "Rupee holds at 277.59 against the dollar");
        let stamps: Vec<i64> = all.iter().filter_map(|h| h.timestamp()).collect();
        assert!(stamps.windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn multiline_titles_are_collapsed() {
        let items = parse_rss(DAWN, "Dawn");
        assert_eq!(
            items[0].title,
            "Uncertainty in Gulf likely to keep policy rate unchanged"
        );
        assert!(!items[0].title.contains('\n'));
    }

    #[test]
    fn formats_a_display_timestamp() {
        let items = parse_rss(DAWN, "Dawn");
        assert_eq!(items[0].when(), "26 Jul 20:15");
    }

    #[test]
    fn an_unparseable_date_degrades_instead_of_lying() {
        let h = Headline {
            title: "t".into(),
            url: "u".into(),
            published: "yesterday-ish".into(),
            source: "s".into(),
        };
        assert!(h.timestamp().is_none());
        assert_eq!(h.when(), "yesterday-is");
    }

    #[test]
    fn empty_and_malformed_feeds_are_survivable() {
        assert!(parse_rss("", "x").is_empty());
        assert!(parse_rss("<rss></rss>", "x").is_empty());
        // Unterminated item: nothing to extract, and no infinite loop.
        assert!(parse_rss("<item ><title>a</title>", "x").is_empty());
        // An item with neither title nor link is dropped.
        assert!(parse_rss("<item><guid>1</guid></item>", "x").is_empty());
    }

    #[test]
    fn similarly_named_tags_are_not_mistaken_for_items() {
        let xml = "<itemDescription>no</itemDescription>\
                   <item><title>yes</title><link>u</link></item>";
        let items = parse_rss(xml, "x");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "yes");
    }

    #[test]
    fn decodes_numeric_character_references() {
        assert_eq!(decode_entities("don&#8217;t"), "don\u{2019}t");
        assert_eq!(decode_entities("a&#x27;b"), "a'b");
        assert_eq!(decode_entities("Q&A"), "Q&A", "a stray & is left alone");
        assert_eq!(decode_entities("&amp;lt;"), "&lt;", "decoded exactly once");
        assert_eq!(decode_entities("&notanentity;"), "&notanentity;");
    }
}
