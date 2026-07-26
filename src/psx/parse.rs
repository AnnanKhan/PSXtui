//! Shared helpers for turning PSX's HTML into numbers.
//!
//! The portal formats numbers for humans — thousands separators, percent signs,
//! accounting-style parentheses for negatives, and en-dashes for "no data" —
//! so every numeric cell goes through [`parse_number`].

use scraper::ElementRef;

/// Parse a human-formatted numeric cell.
///
/// Handles `1,234.56`, `-4.26%`, `(0.15)` (negative, accounting style), and
/// returns `None` for placeholders like `-`, `—`, `N/A` or empty cells.
pub fn parse_number(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    // Accounting-style negatives: (0.15) means -0.15.
    //
    // PSX also parenthesises already-signed percentages in the index ticker —
    // "(-0.42%)" — so parentheses only imply negation when the inner value
    // carries no explicit sign of its own.
    let (s, negated) = match s.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        Some(inner) => {
            let inner = inner.trim();
            let signed = inner.starts_with('-') || inner.starts_with('+');
            (inner, !signed)
        }
        None => (s, false),
    };

    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
        .collect();

    if cleaned.is_empty() || cleaned == "-" || cleaned == "+" || cleaned == "." {
        return None;
    }

    let value: f64 = cleaned.parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    Some(if negated { -value } else { value })
}

/// Collapse an element's descendant text into a single whitespace-normalised
/// string. PSX markup is minified with stray `&nbsp;` runs, so naive
/// concatenation produces ragged output.
pub fn text_of(el: ElementRef) -> String {
    let joined: String = el.text().collect::<Vec<_>>().join(" ");
    normalize_ws(&joined)
}

/// Collapse all whitespace (including non-breaking spaces) to single spaces.
pub fn normalize_ws(s: &str) -> String {
    s.replace('\u{a0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read a numeric cell, preferring the machine-readable attribute PSX attaches
/// to sortable table cells over the formatted display text.
///
/// Market-watch uses `data-order`; the historical table uses `data-value`.
/// Falling back to the visible text keeps the parser working if either
/// attribute disappears.
pub fn cell_number(el: ElementRef) -> Option<f64> {
    el.value()
        .attr("data-order")
        .or_else(|| el.value().attr("data-value"))
        .and_then(parse_number)
        .or_else(|| parse_number(&text_of(el)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_thousands_separators() {
        assert_eq!(parse_number("59,327,773"), Some(59_327_773.0));
        assert_eq!(parse_number("1,234.56"), Some(1234.56));
    }

    #[test]
    fn parses_signed_and_percent_values() {
        assert_eq!(parse_number("-4.26%"), Some(-4.26));
        assert_eq!(parse_number("1.88%"), Some(1.88));
        assert_eq!(parse_number("0.00"), Some(0.0));
    }

    #[test]
    fn parses_accounting_negatives() {
        assert_eq!(parse_number("(0.15)"), Some(-0.15));
        assert_eq!(parse_number("(29.12)"), Some(-29.12));
    }

    #[test]
    fn parenthesised_signed_percentages_keep_their_sign() {
        // The index ticker wraps an already-signed value in parentheses;
        // treating that as accounting notation would flip it positive.
        assert_eq!(parse_number("(-0.42%)"), Some(-0.42));
        assert_eq!(parse_number("(0.00%)"), Some(0.0));
        assert_eq!(parse_number("(+1.25%)"), Some(1.25));
    }

    #[test]
    fn rejects_placeholders() {
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("-"), None);
        assert_eq!(parse_number("—"), None);
        assert_eq!(parse_number("N/A"), None);
    }

    #[test]
    fn normalizes_nbsp_and_runs() {
        assert_eq!(normalize_ws("a\u{a0}\u{a0}b   c\n d"), "a b c d");
    }
}
