//! Colour palette and number formatting.
//!
//! Keeping both here means every screen renders a price, a percentage or a
//! volume the same way — the tables line up and the eye can scan a column
//! without re-reading units.

use ratatui::prelude::*;

// --- palette -------------------------------------------------------------

pub const FG: Color = Color::Rgb(220, 223, 228);
pub const DIM: Color = Color::Rgb(110, 118, 129);
pub const MUTED: Color = Color::Rgb(139, 148, 158);
pub const BORDER: Color = Color::Rgb(48, 54, 61);
pub const ACCENT: Color = Color::Rgb(88, 166, 255);
pub const UP: Color = Color::Rgb(63, 185, 80);
pub const DOWN: Color = Color::Rgb(248, 81, 73);
pub const FLAT: Color = Color::Rgb(139, 148, 158);
pub const WARN: Color = Color::Rgb(210, 153, 34);
pub const SELECT_BG: Color = Color::Rgb(33, 38, 45);
pub const VOLUME: Color = Color::Rgb(88, 110, 150);

/// Colour for a signed change: green up, red down, grey unchanged.
pub fn change_color(v: f64) -> Color {
    if v > 0.0 {
        UP
    } else if v < 0.0 {
        DOWN
    } else {
        FLAT
    }
}

pub fn header_style() -> Style {
    Style::new().fg(MUTED).bold()
}

pub fn title_style() -> Style {
    Style::new().fg(ACCENT).bold()
}

pub fn border_style() -> Style {
    Style::new().fg(BORDER)
}

pub fn label_style() -> Style {
    Style::new().fg(DIM)
}

pub fn value_style() -> Style {
    Style::new().fg(FG)
}

// --- formatting ----------------------------------------------------------

/// A price, always two decimals with thousands separators: `1,105.08`.
pub fn price(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    group_thousands(&format!("{v:.2}"))
}

/// A signed percentage: `+1.88%`, `-10.00%`.
pub fn pct(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    format!("{v:+.2}%")
}

/// An unsigned percentage: `40.00%`.
pub fn pct_plain(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    format!("{v:.2}%")
}

/// A signed absolute change: `+0.19`, `-17.14`.
pub fn signed(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    format!("{v:+.2}")
}

/// A share count or volume in compact form: `59.3M`, `1.02B`, `12,345`.
pub fn compact(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let a = v.abs();
    let sign = if v < 0.0 { "-" } else { "" };
    if a >= 1e12 {
        format!("{sign}{:.2}T", a / 1e12)
    } else if a >= 1e9 {
        format!("{sign}{:.2}B", a / 1e9)
    } else if a >= 1e6 {
        format!("{sign}{:.1}M", a / 1e6)
    } else if a >= 1e3 {
        format!("{sign}{:.1}K", a / 1e3)
    } else {
        format!("{sign}{a:.0}")
    }
}

/// An index level: `171,021.20`.
pub fn index_level(v: f64) -> String {
    group_thousands(&format!("{v:.2}"))
}

/// A ratio that may be missing, e.g. a P/E of `None` → `—`.
pub fn opt(v: Option<f64>, decimals: usize) -> String {
    match v {
        Some(v) if v.is_finite() => format!("{v:.*}", decimals),
        _ => "—".into(),
    }
}

pub fn opt_price(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() => price(v),
        _ => "—".into(),
    }
}

/// Insert thousands separators into an already-formatted decimal string.
fn group_thousands(s: &str) -> String {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", s),
    };
    let (int_part, frac) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };

    let mut grouped = String::new();
    for (i, c) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }

    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

/// Truncate to `width` display columns, ellipsising when it doesn't fit.
pub fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".into();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands_in_prices() {
        assert_eq!(price(1105.08), "1,105.08");
        assert_eq!(price(292.0), "292.00");
        assert_eq!(price(1_466_852_508.0), "1,466,852,508.00");
        assert_eq!(price(-1234.5), "-1,234.50");
        assert_eq!(price(0.0), "0.00");
    }

    #[test]
    fn formats_signed_values() {
        assert_eq!(pct(1.88), "+1.88%");
        assert_eq!(pct(-10.0), "-10.00%");
        assert_eq!(pct(0.0), "+0.00%");
        assert_eq!(signed(-17.14), "-17.14");
    }

    #[test]
    fn compacts_large_magnitudes() {
        assert_eq!(compact(59_327_773.0), "59.3M");
        assert_eq!(compact(1_466_852_508.0), "1.47B");
        assert_eq!(compact(12_345.0), "12.3K");
        assert_eq!(compact(676.0), "676");
        assert_eq!(compact(0.0), "0");
    }

    #[test]
    fn non_finite_values_render_as_a_dash() {
        assert_eq!(price(f64::NAN), "—");
        assert_eq!(pct(f64::INFINITY), "—");
        assert_eq!(compact(f64::NAN), "—");
        assert_eq!(opt(None, 2), "—");
    }

    #[test]
    fn change_colour_follows_sign() {
        assert_eq!(change_color(1.0), UP);
        assert_eq!(change_color(-1.0), DOWN);
        assert_eq!(change_color(0.0), FLAT);
    }

    #[test]
    fn truncates_with_an_ellipsis() {
        assert_eq!(truncate("Habib Bank Limited", 8), "Habib B…");
        assert_eq!(truncate("HBL", 8), "HBL");
    }
}
