//! Colour palette and number formatting.
//!
//! Keeping both here means every screen renders a price, a percentage or a
//! volume the same way — the tables line up and the eye can scan a column
//! without re-reading units.
//!
//! Colours are read through functions rather than constants because the active
//! [`Palette`] can change while the app runs: `t` cycles themes and every
//! subsequent frame picks up the new one. A frame is drawn from one thread, so
//! the selection is a plain atomic index into [`THEMES`] — no lock on the hot
//! path.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use ratatui::prelude::*;
use ratatui::symbols::Marker;

// --- palette -------------------------------------------------------------

/// Every colour a screen can ask for.
///
/// One struct rather than a trait or a map: a palette is data, and a new theme
/// should be a literal someone can read top to bottom and compare against its
/// neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Lower-case key used by `PSXTUI_THEME` and stored in the cache.
    pub key: &'static str,
    /// How the theme is named in the status bar.
    pub name: &'static str,
    /// Whether the theme paints the whole screen itself.
    ///
    /// The default theme leaves the terminal's own background showing, which
    /// keeps transparency and blur working. Every other theme owns the
    /// background, because a Solarized foreground over someone else's
    /// background is not Solarized.
    pub opaque: bool,
    /// The background the theme assumes. Used to paint the screen when
    /// `opaque`, and always used as the ground for pixel-rendered charts,
    /// which have no notion of a "default" colour.
    pub bg: Color,
    pub fg: Color,
    pub dim: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub up: Color,
    pub down: Color,
    pub flat: Color,
    pub warn: Color,
    pub select_bg: Color,
    pub volume: Color,
    /// Extra hues, used where several series share one set of axes and the
    /// only thing telling them apart is colour. Chosen to stay separable from
    /// each other *and* from the four above, which come first.
    pub violet: Color,
    pub cyan: Color,
    pub pink: Color,
    pub sand: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// The original palette: GitHub-dark hues over whatever background the
/// terminal already had.
pub const TERMINAL: Palette = Palette {
    key: "terminal",
    name: "Terminal",
    opaque: false,
    bg: rgb(13, 17, 23),
    fg: rgb(220, 223, 228),
    dim: rgb(110, 118, 129),
    muted: rgb(139, 148, 158),
    border: rgb(48, 54, 61),
    accent: rgb(88, 166, 255),
    up: rgb(63, 185, 80),
    down: rgb(248, 81, 73),
    flat: rgb(139, 148, 158),
    warn: rgb(210, 153, 34),
    select_bg: rgb(33, 38, 45),
    volume: rgb(88, 110, 150),
    violet: rgb(188, 140, 255),
    cyan: rgb(57, 197, 207),
    pink: rgb(247, 120, 186),
    sand: rgb(219, 171, 121),
};

/// The same hues, but owning the background — for terminals whose default is
/// light, or a translucent surface that makes thin strokes hard to read.
pub const MIDNIGHT: Palette = Palette {
    key: "midnight",
    name: "Midnight",
    opaque: true,
    bg: rgb(16, 18, 28),
    fg: rgb(192, 202, 245),
    dim: rgb(86, 95, 137),
    muted: rgb(130, 139, 184),
    border: rgb(41, 46, 66),
    accent: rgb(122, 162, 247),
    up: rgb(158, 206, 106),
    down: rgb(247, 118, 142),
    flat: rgb(130, 139, 184),
    warn: rgb(224, 175, 104),
    select_bg: rgb(32, 37, 57),
    volume: rgb(86, 108, 168),
    violet: rgb(187, 154, 247),
    cyan: rgb(125, 207, 255),
    pink: rgb(255, 138, 197),
    sand: rgb(222, 189, 137),
};

pub const NORD: Palette = Palette {
    key: "nord",
    name: "Nord",
    opaque: true,
    bg: rgb(46, 52, 64),
    fg: rgb(216, 222, 233),
    dim: rgb(97, 110, 136),
    muted: rgb(143, 156, 178),
    border: rgb(59, 66, 82),
    accent: rgb(136, 192, 208),
    up: rgb(163, 190, 140),
    down: rgb(191, 97, 106),
    flat: rgb(143, 156, 178),
    warn: rgb(235, 203, 139),
    select_bg: rgb(59, 66, 82),
    volume: rgb(94, 129, 172),
    violet: rgb(180, 142, 173),
    cyan: rgb(143, 188, 187),
    pink: rgb(208, 135, 173),
    sand: rgb(222, 190, 145),
};

pub const GRUVBOX: Palette = Palette {
    key: "gruvbox",
    name: "Gruvbox",
    opaque: true,
    bg: rgb(29, 32, 33),
    fg: rgb(235, 219, 178),
    dim: rgb(124, 111, 100),
    muted: rgb(168, 153, 132),
    border: rgb(60, 56, 54),
    accent: rgb(131, 165, 152),
    up: rgb(184, 187, 38),
    down: rgb(251, 73, 52),
    flat: rgb(168, 153, 132),
    warn: rgb(250, 189, 47),
    select_bg: rgb(50, 48, 47),
    volume: rgb(104, 125, 118),
    violet: rgb(211, 134, 155),
    cyan: rgb(142, 192, 124),
    pink: rgb(211, 134, 155),
    sand: rgb(214, 153, 33),
};

pub const SOLARIZED: Palette = Palette {
    key: "solarized",
    name: "Solarized Dark",
    opaque: true,
    bg: rgb(0, 43, 54),
    fg: rgb(147, 161, 161),
    dim: rgb(88, 110, 117),
    muted: rgb(131, 148, 150),
    border: rgb(7, 54, 66),
    accent: rgb(38, 139, 210),
    up: rgb(133, 153, 0),
    down: rgb(220, 50, 47),
    flat: rgb(131, 148, 150),
    warn: rgb(181, 137, 0),
    select_bg: rgb(7, 54, 66),
    volume: rgb(42, 106, 128),
    violet: rgb(108, 113, 196),
    cyan: rgb(42, 161, 152),
    pink: rgb(211, 54, 130),
    sand: rgb(203, 75, 22),
};

/// A light theme, for a bright room or a projector. The up/down pair is darkened
/// well past the dark themes' — a mid green that reads fine on charcoal turns
/// into a smudge on paper.
pub const PAPER: Palette = Palette {
    key: "paper",
    name: "Paper",
    opaque: true,
    bg: rgb(253, 246, 227),
    fg: rgb(60, 66, 74),
    dim: rgb(147, 153, 142),
    muted: rgb(101, 123, 131),
    border: rgb(214, 206, 184),
    accent: rgb(24, 106, 173),
    up: rgb(28, 126, 61),
    down: rgb(190, 40, 38),
    flat: rgb(120, 130, 133),
    warn: rgb(160, 108, 0),
    select_bg: rgb(238, 230, 208),
    volume: rgb(140, 160, 180),
    violet: rgb(96, 84, 178),
    cyan: rgb(20, 132, 128),
    pink: rgb(184, 44, 116),
    sand: rgb(150, 92, 30),
};

/// A monochrome amber phosphor, for the terminal that wants to look like the
/// terminal. Up and down are separated by brightness rather than hue, so the
/// sign still reads.
pub const AMBER: Palette = Palette {
    key: "amber",
    name: "Amber",
    opaque: true,
    bg: rgb(18, 12, 4),
    fg: rgb(255, 176, 46),
    dim: rgb(112, 74, 20),
    muted: rgb(178, 122, 34),
    border: rgb(74, 50, 14),
    accent: rgb(255, 214, 122),
    up: rgb(255, 208, 96),
    down: rgb(198, 92, 22),
    flat: rgb(160, 112, 32),
    warn: rgb(255, 236, 170),
    select_bg: rgb(48, 32, 8),
    volume: rgb(126, 84, 24),
    violet: rgb(226, 160, 90),
    cyan: rgb(255, 232, 150),
    pink: rgb(214, 124, 48),
    sand: rgb(200, 148, 60),
};

/// Every theme, in cycle order. The first is the default.
pub const THEMES: &[Palette] = &[TERMINAL, MIDNIGHT, NORD, GRUVBOX, SOLARIZED, PAPER, AMBER];

static ACTIVE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// The palette every colour accessor reads.
///
/// The first call resolves `PSXTUI_THEME`, so a theme set in the environment
/// applies even before the app has loaded its saved choice.
pub fn palette() -> &'static Palette {
    let mut i = ACTIVE.load(Ordering::Relaxed);
    if i == usize::MAX {
        i = std::env::var("PSXTUI_THEME")
            .ok()
            .and_then(|v| theme_index(&v))
            .unwrap_or(0);
        ACTIVE.store(i, Ordering::Relaxed);
    }
    &THEMES[i.min(THEMES.len() - 1)]
}

/// The index of the theme named `key`, matched case-insensitively against both
/// the key and the display name.
pub fn theme_index(key: &str) -> Option<usize> {
    let key = key.trim().to_ascii_lowercase();
    THEMES
        .iter()
        .position(|p| p.key == key || p.name.to_ascii_lowercase() == key)
}

/// Switch to a theme by index. Out-of-range indices wrap, so callers can just
/// add one.
pub fn set_theme(i: usize) {
    ACTIVE.store(i % THEMES.len(), Ordering::Relaxed);
}

/// The active theme's index.
pub fn current() -> usize {
    // Through `palette` so the environment default is resolved first.
    let p = palette();
    THEMES.iter().position(|t| t.key == p.key).unwrap_or(0)
}

/// Advance to the next theme and return it.
pub fn next_theme() -> &'static Palette {
    set_theme(current() + 1);
    palette()
}

pub fn bg() -> Color {
    palette().bg
}
pub fn fg() -> Color {
    palette().fg
}
pub fn dim() -> Color {
    palette().dim
}
pub fn muted() -> Color {
    palette().muted
}
pub fn border() -> Color {
    palette().border
}
pub fn accent() -> Color {
    palette().accent
}
pub fn up() -> Color {
    palette().up
}
pub fn down() -> Color {
    palette().down
}
pub fn flat() -> Color {
    palette().flat
}
pub fn warn() -> Color {
    palette().warn
}
pub fn select_bg() -> Color {
    palette().select_bg
}
pub fn volume() -> Color {
    palette().volume
}
pub fn violet() -> Color {
    palette().violet
}
pub fn cyan() -> Color {
    palette().cyan
}
pub fn pink() -> Color {
    palette().pink
}
pub fn sand() -> Color {
    palette().sand
}

/// The background this theme paints, or `None` when it defers to the
/// terminal's own.
///
/// The default theme defers, and that is the point of it: whatever the terminal
/// was configured with — a colour scheme, transparency, a blurred desktop
/// behind it — shows through, and the app sits in it rather than on top of it.
/// Every other theme owns the background, because a Solarized foreground over
/// someone else's background is not Solarized.
///
/// Charts respect this too: where a theme defers, their bitmaps are rendered on
/// a transparent ground (see [`super::gfx`]) so the terminal shows through
/// those as well.
pub fn ground() -> Option<Color> {
    let p = palette();
    p.opaque.then_some(p.bg)
}

/// The style the whole screen sits on.
pub fn screen_style() -> Style {
    Style::new().fg(fg()).bg(ground().unwrap_or(Color::Reset))
}

/// Colour for a signed change: green up, red down, grey unchanged.
pub fn change_color(v: f64) -> Color {
    if v > 0.0 {
        up()
    } else if v < 0.0 {
        down()
    } else {
        flat()
    }
}

pub fn header_style() -> Style {
    Style::new().fg(muted()).bold()
}

pub fn title_style() -> Style {
    Style::new().fg(accent()).bold()
}

pub fn border_style() -> Style {
    Style::new().fg(border())
}

pub fn label_style() -> Style {
    Style::new().fg(dim())
}

pub fn value_style() -> Style {
    Style::new().fg(fg())
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

// --- canvas markers ------------------------------------------------------

/// The marker a canvas should actually draw with.
///
/// Braille packs 2x4 dots into one cell, which is the resolution every chart
/// here is drawn against — but a font without U+2800–U+28FF renders the lot as
/// boxes, which is the state some Windows console fonts ship in. Setting
/// `PSXTUI_MARKER=block` falls back to half-blocks: half the vertical
/// resolution, and present in any font that can draw the rest of the UI.
///
/// Passing a non-braille marker through is deliberate — the chart's line and
/// area styles already use half-blocks, and the override has nothing to say
/// about them.
///
/// None of this applies when the terminal draws charts as images: see
/// [`super::gfx`], which bypasses cell markers entirely.
pub fn marker(preferred: Marker) -> Marker {
    static BLOCK: OnceLock<bool> = OnceLock::new();
    let block = *BLOCK.get_or_init(|| {
        std::env::var("PSXTUI_MARKER")
            .map(|v| block_requested(&v))
            .unwrap_or(false)
    });

    resolve(block, preferred)
}

/// The mapping itself, split out so it can be tested without the process-wide
/// environment read that decides `block`.
fn resolve(block: bool, preferred: Marker) -> Marker {
    match (block, preferred) {
        (true, Marker::Braille) => Marker::HalfBlock,
        _ => preferred,
    }
}

/// Whether a `PSXTUI_MARKER` value asks for the block fallback.
///
/// Anything unrecognised means braille: the variable exists to rescue a broken
/// display, so a typo must not silently degrade a working one.
fn block_requested(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "block" | "blocks"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_block_asks_for_the_fallback() {
        assert!(block_requested("block"));
        assert!(block_requested("BLOCK"));
        assert!(block_requested(" blocks "));

        assert!(!block_requested("braille"));
        assert!(!block_requested(""));
        assert!(
            !block_requested("blocky"),
            "a typo must not degrade a good display"
        );
    }

    #[test]
    fn the_override_only_touches_braille() {
        assert_eq!(resolve(true, Marker::Braille), Marker::HalfBlock);
        assert_eq!(resolve(false, Marker::Braille), Marker::Braille);
        // A style that already asked for half-blocks is unaffected either way.
        assert_eq!(resolve(true, Marker::HalfBlock), Marker::HalfBlock);
        assert_eq!(resolve(false, Marker::HalfBlock), Marker::HalfBlock);
    }

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
        assert_eq!(change_color(1.0), up());
        assert_eq!(change_color(-1.0), down());
        assert_eq!(change_color(0.0), flat());
    }

    #[test]
    fn truncates_with_an_ellipsis() {
        assert_eq!(truncate("Habib Bank Limited", 8), "Habib B…");
        assert_eq!(truncate("HBL", 8), "HBL");
    }

    #[test]
    fn themes_are_addressable_by_key_and_name() {
        assert_eq!(theme_index("nord"), Some(2));
        assert_eq!(theme_index(" Solarized Dark "), Some(4));
        assert_eq!(theme_index("NORD"), Some(2));
        assert_eq!(theme_index("no-such-theme"), None);
    }

    #[test]
    fn cycling_wraps_and_lands_on_every_theme() {
        let start = current();
        let mut seen = Vec::new();
        for _ in 0..THEMES.len() {
            seen.push(next_theme().key);
        }
        assert_eq!(seen.len(), THEMES.len(), "the cycle visits every theme");
        assert_eq!(current(), start, "and returns to where it began");
    }

    /// Every theme must separate up from down, and both from the background —
    /// a palette where a gain and a loss look the same is not usable.
    #[test]
    fn every_theme_distinguishes_its_key_colours() {
        for p in THEMES {
            assert_ne!(p.up, p.down, "{} up == down", p.key);
            assert_ne!(p.fg, p.bg, "{} fg == bg", p.key);
            assert_ne!(p.accent, p.bg, "{} accent == bg", p.key);
            assert_ne!(p.select_bg, p.fg, "{} selection hides text", p.key);
        }
    }

    #[test]
    fn theme_keys_are_unique_and_lower_case() {
        for (i, p) in THEMES.iter().enumerate() {
            assert_eq!(p.key, p.key.to_ascii_lowercase(), "{} is not lower case", i);
            assert_eq!(theme_index(p.key), Some(i), "{} is not addressable", p.key);
        }
    }
}
