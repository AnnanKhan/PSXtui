//! Pixel-resolution charts, via the terminal graphics protocol.
//!
//! A braille cell gives a chart 2x4 dots; a modern terminal will happily draw a
//! bitmap instead, at whatever resolution the font's cell size implies —
//! roughly 9x18 dots per cell, or forty times the detail. On a terminal that
//! supports the kitty graphics protocol every chart is rasterised here and sent
//! as an image; everywhere else the braille canvas is still what runs, so
//! nothing regresses on a terminal that cannot do this.
//!
//! ## How an image gets onto the screen
//!
//! ratatui owns the cell grid and knows nothing about images, so the two are
//! kept apart: during a frame a widget rasterises into an [`Image`] and calls
//! [`submit`] with the [`Rect`] it wants covered, which only records the
//! request. Once ratatui has drawn and flushed its own frame, the event loop
//! calls [`present`], which emits the escape sequences.
//!
//! Placements use `z=-1`, which the protocol defines as *below text but above
//! the cell background*. That is what lets a chart be an image and its axis
//! labels be ordinary terminal text drawn on top — the labels stay crisp at the
//! font's own hinting rather than being rasterised into the bitmap.
//!
//! ## What is not re-sent
//!
//! Retransmitting a megabyte of base64 for a redraw that only advanced the
//! spinner would make the whole app feel slow, so [`present`] keeps what it
//! sent last frame and skips any slot whose pixels and position are unchanged.
//! A kitty placement persists until deleted, so skipping is invisible.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

use ratatui::prelude::*;

/// The largest bitmap a chart is allowed to become, in pixels.
///
/// Every frame that changes ships its image as base64 over the pty, so
/// resolution is paid for in latency. Past this the picture is rendered smaller
/// and the terminal scales it back over the same cells: on a 4K display the
/// difference is a hair of softness, against several megabytes a keystroke.
const MAX_PIXELS: u32 = 1_400 * 800;

/// Assumed cell size when the terminal will not report one.
///
/// Only the *ratio* really matters — the placement pins the image to a known
/// number of cells either way — and 1:2 is close enough for every common
/// terminal font.
const FALLBACK_CELL: (u16, u16) = (9, 18);

/// Kitty's own limit on the payload of one escape sequence, in base64 chars.
const CHUNK: usize = 4096;

// --- capability ----------------------------------------------------------

/// Whether this terminal is drawn with images or with cell glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Cell glyphs — braille or half-blocks.
    Text,
    /// The kitty graphics protocol.
    Kitty,
}

/// What the terminal can do, resolved once.
pub fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let env = |k: &str| std::env::var(k).ok();
        detect(
            env("PSXTUI_GRAPHICS").as_deref(),
            &Env {
                term: env("TERM").unwrap_or_default(),
                term_program: env("TERM_PROGRAM").unwrap_or_default(),
                kitty_window_id: env("KITTY_WINDOW_ID").is_some(),
                ghostty: env("GHOSTTY_RESOURCES_DIR").is_some() || env("GHOSTTY_BIN_DIR").is_some(),
                wezterm: env("WEZTERM_PANE").is_some() || env("WEZTERM_EXECUTABLE").is_some(),
                konsole: env("KONSOLE_VERSION").is_some(),
                multiplexed: env("TMUX").is_some() || env("STY").is_some(),
            },
        )
    })
}

pub fn enabled() -> bool {
    mode() == Mode::Kitty
}

/// The environment signals capability detection reads.
///
/// Passed in rather than read inside [`detect`] so the rules can be tested;
/// process-wide environment mutation in a test would race every other test.
pub struct Env {
    pub term: String,
    pub term_program: String,
    pub kitty_window_id: bool,
    pub ghostty: bool,
    pub wezterm: bool,
    pub konsole: bool,
    pub multiplexed: bool,
}

/// Decide how to draw, from an explicit override and the environment.
///
/// Detection is by environment rather than by querying the terminal and waiting
/// for a reply: the query has to happen before the input reader starts, and a
/// terminal that ignores it leaves the app either blocked or guessing after a
/// timeout. Guessing from `TERM` costs nothing and is wrong only in the
/// direction of "kept the braille charts", which is a working app.
///
/// `PSXTUI_GRAPHICS` overrides both ways, which is the escape hatch for a
/// terminal this does not know about (`=kitty`) and for one that claims support
/// it does not have (`=off`).
pub fn detect(override_var: Option<&str>, env: &Env) -> Mode {
    match override_var
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("off" | "none" | "0" | "text") => return Mode::Text,
        Some("kitty" | "on" | "1") => return Mode::Kitty,
        _ => {}
    }

    // Inside tmux or screen the escape would have to be wrapped in passthrough
    // and re-emitted on every pane change, and the multiplexer's own idea of
    // what is on screen would still not include the image. Not worth a corrupt
    // display: the braille charts work there.
    if env.multiplexed {
        return Mode::Text;
    }

    let term = env.term.to_ascii_lowercase();
    let program = env.term_program.to_ascii_lowercase();
    let kitty = env.kitty_window_id
        || env.ghostty
        || env.wezterm
        || env.konsole
        || term.contains("kitty")
        || term.contains("ghostty")
        || matches!(program.as_str(), "ghostty" | "wezterm");

    if kitty { Mode::Kitty } else { Mode::Text }
}

/// The terminal's cell size in pixels, resolved once per process.
///
/// A resize changes the grid, not the cell, so this does not need to follow the
/// window — the number of cells does, and that comes from the layout.
pub fn cell_size() -> (u16, u16) {
    static CELL: OnceLock<(u16, u16)> = OnceLock::new();
    *CELL.get_or_init(|| {
        if let Ok(spec) = std::env::var("PSXTUI_CELL")
            && let Some(size) = parse_cell(&spec)
        {
            return size;
        }
        match crossterm::terminal::window_size() {
            // A terminal that does not implement the pixel fields reports
            // zeroes rather than failing, so they have to be checked.
            Ok(w) if w.width > 0 && w.height > 0 && w.columns > 0 && w.rows > 0 => {
                (w.width / w.columns, w.height / w.rows)
            }
            _ => FALLBACK_CELL,
        }
    })
}

/// Parse a `PSXTUI_CELL=9x18` override.
fn parse_cell(spec: &str) -> Option<(u16, u16)> {
    let (w, h) = spec.trim().split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

// --- images --------------------------------------------------------------

/// An RGBA bitmap, drawn into with anti-aliased primitives.
///
/// Coordinates are `f32` pixels with the origin at the top left, and every
/// primitive blends by coverage — a wick half a pixel wide still renders, at
/// half intensity, instead of snapping to a column or vanishing.
///
/// Alpha is not decoration. A chart on a theme that defers to the terminal is
/// drawn on a *transparent* ground, so the user's own background — colour
/// scheme, transparency, whatever is behind the window — shows through the plot
/// exactly as it does through the rest of the UI. Compositing then happens in
/// the terminal, against the real backdrop, rather than here against a guess.
#[derive(Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Four bytes per pixel, row-major, straight (non-premultiplied) alpha.
    /// Kitty's `f=32`, so it goes onto the wire with no conversion.
    px: Vec<u8>,
}

impl Image {
    /// A new image of `width` x `height`, flooded with `bg` — or left fully
    /// transparent when the theme defers to the terminal's own background.
    pub fn new(width: u32, height: u32, bg: Option<Color>) -> Self {
        let ground = match bg {
            Some(c) => {
                let (r, g, b) = rgb(c);
                [r, g, b, 255]
            }
            None => [0, 0, 0, 0],
        };
        let px = ground.repeat((width as usize) * (height as usize));
        Self { width, height, px }
    }

    /// Blend `color` into one pixel at `coverage` (0..=1), source-over.
    ///
    /// Straight alpha throughout: over an opaque ground this is the plain
    /// lerp it always was, and over a transparent one it accumulates coverage
    /// so an anti-aliased edge stays translucent instead of being flattened
    /// against a background that is not there.
    #[inline]
    fn blend(&mut self, x: i32, y: i32, color: (u8, u8, u8), coverage: f32) {
        if coverage <= 0.0 || x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let sa = coverage.min(1.0);
        let i = ((y as usize) * (self.width as usize) + x as usize) * 4;
        let da = self.px[i + 3] as f32 / 255.0;
        let out_a = sa + da * (1.0 - sa);
        if out_a <= 0.0 {
            return;
        }
        for (k, c) in [color.0, color.1, color.2].into_iter().enumerate() {
            let dst = self.px[i + k] as f32;
            let out = (c as f32 * sa + dst * da * (1.0 - sa)) / out_a;
            self.px[i + k] = out.round().clamp(0.0, 255.0) as u8;
        }
        self.px[i + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    /// Fill an axis-aligned rectangle, anti-aliasing the edges.
    ///
    /// Fractional edges matter more here than they look: a candle body is
    /// column-width divided by the number of sessions, which is rarely a whole
    /// number of pixels, and rounding each one independently makes a row of
    /// identical candles come out visibly uneven.
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let color = rgb(color);
        let (x0, x1) = (x, x + w);
        let (y0, y1) = (y, y + h);
        let px0 = x0.floor() as i32;
        let px1 = x1.ceil() as i32;
        let py0 = y0.floor() as i32;
        let py1 = y1.ceil() as i32;

        for py in py0..py1 {
            let cy = (py as f32 + 1.0).min(y1) - (py as f32).max(y0);
            if cy <= 0.0 {
                continue;
            }
            for px in px0..px1 {
                let cx = (px as f32 + 1.0).min(x1) - (px as f32).max(x0);
                if cx > 0.0 {
                    self.blend(px, py, color, cx * cy);
                }
            }
        }
    }

    /// Stroke a line of `weight` pixels, anti-aliased.
    ///
    /// Coverage comes from the distance to the segment rather than from a
    /// Bresenham walk, so a diagonal stroke has the same apparent weight as a
    /// vertical one and joins between segments do not leave notches.
    pub fn stroke_line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, weight: f32, color: Color) {
        let color = rgb(color);
        let half = (weight.max(0.5)) / 2.0;
        let (dx, dy) = (x2 - x1, y2 - y1);
        let len_sq = dx * dx + dy * dy;

        let pad = half + 1.0;
        let x0 = (x1.min(x2) - pad).floor().max(0.0) as i32;
        let xe = (x1.max(x2) + pad).ceil().min(self.width as f32) as i32;
        let y0 = (y1.min(y2) - pad).floor().max(0.0) as i32;
        let ye = (y1.max(y2) + pad).ceil().min(self.height as f32) as i32;

        for py in y0..ye {
            for px in x0..xe {
                // Pixel centre.
                let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                let t = if len_sq > 0.0 {
                    (((cx - x1) * dx + (cy - y1) * dy) / len_sq).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (nx, ny) = (x1 + t * dx, y1 + t * dy);
                let dist = ((cx - nx).powi(2) + (cy - ny).powi(2)).sqrt();
                // One pixel of feathering either side of the edge.
                let coverage = (half + 0.5 - dist).clamp(0.0, 1.0);
                self.blend(px, py, color, coverage);
            }
        }
    }

    /// A filled circle — the marker for a series with a single point.
    pub fn dot(&mut self, x: f32, y: f32, radius: f32, color: Color) {
        let color = rgb(color);
        let r = radius.max(0.5);
        let x0 = (x - r - 1.0).floor().max(0.0) as i32;
        let xe = (x + r + 1.0).ceil().min(self.width as f32) as i32;
        let y0 = (y - r - 1.0).floor().max(0.0) as i32;
        let ye = (y + r + 1.0).ceil().min(self.height as f32) as i32;
        for py in y0..ye {
            for px in x0..xe {
                let d = ((px as f32 + 0.5 - x).powi(2) + (py as f32 + 0.5 - y).powi(2)).sqrt();
                self.blend(px, py, color, (r + 0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    /// The colour and alpha of one pixel. For tests, and for nothing else —
    /// drawing is write-only.
    #[cfg(test)]
    pub fn sample(&self, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y as usize) * (self.width as usize) + x as usize) * 4;
        (self.px[i], self.px[i + 1], self.px[i + 2], self.px[i + 3])
    }

    fn digest(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.width.hash(&mut h);
        self.height.hash(&mut h);
        self.px.hash(&mut h);
        h.finish()
    }
}

/// A ratatui colour as three bytes.
///
/// Every palette colour is already RGB; the ANSI cases exist because a caller
/// may pass a themed style through, and are approximated rather than refused —
/// a slightly-off green beats a hole in the chart.
fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (204, 204, 204),
        Color::DarkGray => (102, 102, 102),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::White => (229, 229, 229),
        Color::Indexed(i) => indexed(i),
        // `Reset` has no colour of its own; the theme's background is the
        // nearest honest answer, and it is what the image sits on anyway.
        Color::Reset => rgb(super::theme::palette().bg),
    }
}

/// The xterm 256-colour cube and greyscale ramp.
fn indexed(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => {
            const BASE: [(u8, u8, u8); 16] = [
                (0, 0, 0),
                (128, 0, 0),
                (0, 128, 0),
                (128, 128, 0),
                (0, 0, 128),
                (128, 0, 128),
                (0, 128, 128),
                (192, 192, 192),
                (128, 128, 128),
                (255, 0, 0),
                (0, 255, 0),
                (255, 255, 0),
                (0, 0, 255),
                (255, 0, 255),
                (0, 255, 255),
                (255, 255, 255),
            ];
            BASE[i as usize]
        }
        16..=231 => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

// --- plotting ------------------------------------------------------------

/// An [`Image`] plus the data-space bounds it represents.
///
/// Callers draw in data coordinates — prices and session indices — exactly as
/// they would against a ratatui canvas, and the mapping to pixels happens here.
/// y is flipped on the way in, because a chart's y grows upward and an image's
/// grows downward.
pub struct Plot {
    pub image: Image,
    /// Stroke weight in pixels, scaled to the plot so a line looks the same
    /// thickness on a laptop and on a 4K panel.
    pub weight: f32,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
}

impl Plot {
    pub fn new(image: Image, x_bounds: [f64; 2], y_bounds: [f64; 2]) -> Self {
        // A degenerate axis would divide by zero and put every point in the
        // same place; widening it draws a flat line through the middle, which
        // is the truth about a series that never moved.
        let (x0, x1) = spread(x_bounds[0], x_bounds[1]);
        let (y0, y1) = spread(y_bounds[0], y_bounds[1]);
        // Thin enough that overlays do not smother the candles, never so thin
        // that anti-aliasing turns a line into a grey suggestion.
        let weight = (image.height as f32 / 300.0).clamp(1.2, 3.0);
        Self {
            image,
            weight,
            x0,
            x1,
            y0,
            y1,
        }
    }

    /// Data x to pixel x.
    pub fn px(&self, x: f64) -> f32 {
        (((x - self.x0) / (self.x1 - self.x0)) * self.image.width as f64) as f32
    }

    /// Data y to pixel y, flipped.
    pub fn py(&self, y: f64) -> f32 {
        ((1.0 - (y - self.y0) / (self.y1 - self.y0)) * self.image.height as f64) as f32
    }

    /// One data-space x unit, in pixels — the width of a session's column.
    pub fn x_scale(&self) -> f32 {
        (self.image.width as f64 / (self.x1 - self.x0)) as f32
    }
}

/// Widen a zero-width range around its own value.
fn spread(a: f64, b: f64) -> (f64, f64) {
    if !a.is_finite() || !b.is_finite() {
        return (0.0, 1.0);
    }
    if (b - a).abs() < f64::EPSILON {
        let pad = if a.abs() > 0.0 { a.abs() * 0.01 } else { 0.5 };
        return (a - pad, b + pad);
    }
    (a, b)
}

/// The pixel size to rasterise `area` at, or `None` if it is too small to be
/// worth drawing.
///
/// The image is capped at [`MAX_PIXELS`] and the terminal scales it back to the
/// same cells, so a huge window costs sharpness rather than latency.
pub fn image_size(area: Rect) -> Option<(u32, u32)> {
    if area.width < 2 || area.height < 2 {
        return None;
    }
    let (cw, ch) = cell_size();
    let w = area.width as u32 * cw.max(1) as u32;
    let h = area.height as u32 * ch.max(1) as u32;
    let total = w * h;
    if total <= MAX_PIXELS {
        return Some((w, h));
    }
    let scale = (MAX_PIXELS as f64 / total as f64).sqrt();
    Some((
        ((w as f64 * scale) as u32).max(2),
        ((h as f64 * scale) as u32).max(2),
    ))
}

// --- the present queue ---------------------------------------------------

/// One image waiting to be drawn, as handed over by a widget.
struct Pending {
    slot: &'static str,
    area: Rect,
    image: Image,
}

/// What a slot looked like on the last presented frame.
#[derive(PartialEq)]
struct Placed {
    id: u32,
    area: Rect,
    digest: u64,
}

#[derive(Default)]
struct State {
    queue: Vec<Pending>,
    placed: HashMap<&'static str, Placed>,
    next_id: u32,
    /// Set for frames that must not use images at all — see [`begin_frame`].
    suppressed: bool,
}

/// Open a frame, declaring whether images may be used in it.
///
/// Called once per frame, before anything is drawn.
pub fn begin_frame(suppressed: bool) {
    state().lock().unwrap().suppressed = suppressed;
}

/// Whether this frame may draw images: the terminal can, and nothing this
/// frame draws would be spoiled by one.
pub fn available() -> bool {
    enabled() && !state().lock().unwrap().suppressed
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            // Kitty image ids are global to the terminal, so starting well
            // clear of 1 keeps this out of the way of anything else that drew
            // into the same window.
            next_id: 0x7053_0000,
            ..State::default()
        })
    })
}

/// Queue an image to cover `area`, replacing whatever that slot held before.
///
/// `slot` names a place in the UI ("chart.price"), not a piece of data: the
/// same slot showing a different symbol is still one image, and giving it a
/// stable name is what lets an unchanged frame skip the wire entirely.
pub fn submit(slot: &'static str, area: Rect, image: Image) {
    let mut st = state().lock().unwrap();
    st.queue.retain(|p| p.slot != slot);
    st.queue.push(Pending { slot, area, image });
}

/// Emit everything queued this frame, and delete anything left over from the
/// last one. Call after ratatui has flushed its own output.
///
/// Nothing is submitted on a terminal that draws with cells — [`crate::ui::paint::begin`]
/// refuses there — so this needs no capability check of its own: with an empty
/// queue and nothing placed, it writes nothing.
pub fn present(out: &mut impl Write) -> io::Result<()> {
    flush(&mut state().lock().unwrap(), out)
}

/// [`present`] over an explicit state, so the protocol can be tested without
/// the process-wide queue every screen shares.
fn flush(st: &mut State, out: &mut impl Write) -> io::Result<()> {
    if st.queue.is_empty() && st.placed.is_empty() {
        return Ok(());
    }
    let queue = std::mem::take(&mut st.queue);

    // Anything the frame did not draw is gone from the screen — a chart the
    // user navigated away from — and its placement has to go with it, or it
    // would sit on top of whatever replaced it.
    let live: Vec<&'static str> = queue.iter().map(|p| p.slot).collect();
    let stale: Vec<&'static str> = st
        .placed
        .keys()
        .filter(|s| !live.contains(s))
        .copied()
        .collect();
    for slot in stale {
        if let Some(p) = st.placed.remove(slot) {
            write!(out, "\x1b_Ga=d,d=i,i={},q=2\x1b\\", p.id)?;
        }
    }

    for pending in queue {
        let digest = pending.image.digest();
        let id = match st.placed.get(pending.slot) {
            Some(prev) if prev.area == pending.area && prev.digest == digest => continue,
            Some(prev) => prev.id,
            None => {
                st.next_id += 1;
                st.next_id
            }
        };

        // A retransmission under the same id replaces the image but leaves the
        // old placement behind, so it is deleted first.
        write!(out, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\")?;
        transmit(out, id, &pending.image, pending.area)?;

        st.placed.insert(
            pending.slot,
            Placed {
                id,
                area: pending.area,
                digest,
            },
        );
    }

    out.flush()
}

/// Drop every image this process placed. Called on the way out, so a quit does
/// not leave a chart painted over the user's shell.
pub fn clear_all(out: &mut impl Write) -> io::Result<()> {
    let mut st = state().lock().unwrap();
    if st.queue.is_empty() && st.placed.is_empty() {
        return Ok(());
    }
    st.queue.clear();
    for (_, p) in st.placed.drain() {
        write!(out, "\x1b_Ga=d,d=i,i={},q=2\x1b\\", p.id)?;
    }
    out.flush()
}

/// Transmit and place one image at `area`.
fn transmit(out: &mut impl Write, id: u32, img: &Image, area: Rect) -> io::Result<()> {
    // The placement is anchored at the cursor, so the cursor goes first. The
    // sequence is 1-based; ratatui's Rect is not.
    write!(out, "\x1b[{};{}H", area.y + 1, area.x + 1)?;

    let payload = base64(&deflate(&img.px));
    let mut chunks = payload.as_bytes().chunks(CHUNK).peekable();

    // `c`/`r` pin the image to exactly the cells the layout gave it, whatever
    // its pixel size — which is what makes the resolution cap safe.
    // `f=32` is RGBA: the alpha is what lets a chart sit on the terminal's own
    // background rather than on one of ours. `o=z` marks the payload
    // zlib-compressed; `s`/`v` still describe the image, not the bytes on the
    // wire.
    // `z=-1` puts it under the text, `C=1` stops it moving the cursor, and
    // `q=2` suppresses the terminal's replies, which would otherwise arrive in
    // the middle of the key stream.
    let head = format!(
        "a=T,f=32,o=z,s={},v={},c={},r={},i={id},z=-1,C=1,q=2",
        img.width, img.height, area.width, area.height
    );

    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        out.write_all(b"\x1b_G")?;
        if first {
            write!(out, "{head},")?;
            first = false;
        }
        write!(out, "m={more};")?;
        out.write_all(chunk)?;
        out.write_all(b"\x1b\\")?;
    }
    Ok(())
}

/// zlib-compress the pixels.
///
/// A chart is a flat background with a few thin strokes across it, which is
/// close to the best case for deflate — two orders of magnitude is typical.
/// Speed over ratio: the compression happens between a keystroke and the frame
/// it redraws, and the difference between fast and best here is a few percent
/// of a payload that is already small.
fn deflate(px: &[u8]) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    let mut enc = ZlibEncoder::new(Vec::with_capacity(px.len() / 8), Compression::fast());
    // Writing to a `Vec` cannot fail, and neither can finishing one.
    let _ = enc.write_all(px);
    enc.finish().unwrap_or_default()
}

/// Standard base64, no line breaks.
///
/// Written out rather than pulled in: it is fifteen lines, and the alternative
/// is a dependency on the app's hottest path for a hundred bytes of table.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let b = [
            group[0],
            *group.get(1).unwrap_or(&0),
            *group.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            term: "xterm-256color".into(),
            term_program: String::new(),
            kitty_window_id: false,
            ghostty: false,
            wezterm: false,
            konsole: false,
            multiplexed: false,
        }
    }

    #[test]
    fn a_plain_xterm_keeps_the_braille_charts() {
        assert_eq!(detect(None, &env()), Mode::Text);
    }

    #[test]
    fn known_terminals_are_recognised() {
        let mut e = env();
        e.term = "xterm-kitty".into();
        assert_eq!(detect(None, &e), Mode::Kitty);

        let mut e = env();
        e.term_program = "ghostty".into();
        assert_eq!(detect(None, &e), Mode::Kitty);

        let mut e = env();
        e.wezterm = true;
        assert_eq!(detect(None, &e), Mode::Kitty);

        let mut e = env();
        e.konsole = true;
        assert_eq!(detect(None, &e), Mode::Kitty);
    }

    #[test]
    fn a_multiplexer_falls_back_however_capable_the_terminal_is() {
        let mut e = env();
        e.term = "xterm-kitty".into();
        e.kitty_window_id = true;
        e.multiplexed = true;
        assert_eq!(detect(None, &e), Mode::Text);
    }

    #[test]
    fn the_override_wins_in_both_directions() {
        let mut kitty = env();
        kitty.kitty_window_id = true;
        assert_eq!(detect(Some("off"), &kitty), Mode::Text);
        assert_eq!(detect(Some(" OFF "), &kitty), Mode::Text);
        assert_eq!(detect(Some("kitty"), &env()), Mode::Kitty);
        // An unrecognised value must not disable a working terminal.
        assert_eq!(detect(Some("weird"), &kitty), Mode::Kitty);
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0x00, 0xff]), "/wD/");
    }

    #[test]
    fn cell_overrides_parse_and_reject_nonsense() {
        assert_eq!(parse_cell("9x18"), Some((9, 18)));
        assert_eq!(parse_cell(" 10X20 "), Some((10, 20)));
        assert_eq!(parse_cell("0x18"), None);
        assert_eq!(parse_cell("wide"), None);
        assert_eq!(parse_cell(""), None);
    }

    #[test]
    fn an_image_starts_flooded_with_its_background() {
        let img = Image::new(3, 2, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(img.px.len(), 3 * 2 * 4);
        assert!(
            img.px.chunks(4).all(|p| p == [1, 2, 3, 255]),
            "opaque ground"
        );

        let clear = Image::new(3, 2, None);
        assert!(
            clear.px.chunks(4).all(|p| p[3] == 0),
            "a deferring theme leaves the terminal showing through"
        );
    }

    /// The whole point of the alpha channel: on a theme that defers to the
    /// terminal, the plot must not lay a rectangle of "background" over the
    /// user's own — transparency, blur and all — and the strokes drawn into it
    /// must still be solid.
    #[test]
    fn a_transparent_ground_stays_transparent_where_nothing_is_drawn() {
        let mut img = Image::new(8, 8, None);
        img.fill_rect(2.0, 2.0, 2.0, 2.0, Color::Rgb(255, 255, 255));

        assert_eq!(img.sample(0, 0).3, 0, "untouched pixels stay clear");
        assert_eq!(
            img.sample(2, 2),
            (255, 255, 255, 255),
            "and what is drawn is fully opaque"
        );
    }

    /// An anti-aliased edge over a transparent ground carries partial alpha
    /// rather than being flattened against a background that is not there —
    /// that is what lets the terminal do the compositing against the real
    /// backdrop.
    #[test]
    fn a_soft_edge_over_nothing_keeps_its_coverage() {
        let mut img = Image::new(4, 4, None);
        img.fill_rect(1.0, 1.0, 0.5, 1.0, Color::Rgb(255, 255, 255));

        let (r, g, b, a) = img.sample(1, 1);
        assert!((0..255).contains(&a) && a > 0, "partial coverage: {a}");
        assert_eq!(
            (r, g, b),
            (255, 255, 255),
            "the colour is the stroke's, not a blend with a fake background"
        );
    }

    #[test]
    fn a_filled_rectangle_lands_where_it_was_asked_to() {
        let mut img = Image::new(8, 8, Some(Color::Rgb(0, 0, 0)));
        img.fill_rect(2.0, 3.0, 2.0, 2.0, Color::Rgb(255, 255, 255));
        let at = |x: usize, y: usize| img.px[(y * 8 + x) * 4];
        assert_eq!(at(2, 3), 255);
        assert_eq!(at(3, 4), 255);
        assert_eq!(at(1, 3), 0, "nothing spills to the left");
        assert_eq!(at(4, 3), 0, "nor to the right");
        assert_eq!(at(2, 5), 0, "nor below");
    }

    /// A sub-pixel body — a doji, or a candle on a five-year chart — must still
    /// leave a mark rather than rounding away to nothing.
    #[test]
    fn a_sub_pixel_rectangle_still_renders() {
        let mut img = Image::new(4, 4, Some(Color::Rgb(0, 0, 0)));
        img.fill_rect(1.25, 1.0, 0.5, 1.0, Color::Rgb(255, 255, 255));
        let at = |x: usize, y: usize| img.px[(y * 4 + x) * 4];
        assert!(at(1, 1) > 0, "a half-pixel wide body must be visible");
        assert!(at(1, 1) < 255, "and drawn at partial coverage");
    }

    #[test]
    fn a_stroke_covers_its_endpoints_and_stays_inside_the_image() {
        let mut img = Image::new(16, 16, Some(Color::Rgb(0, 0, 0)));
        img.stroke_line(1.5, 1.5, 14.5, 14.5, 1.5, Color::Rgb(255, 255, 255));
        let at = |x: usize, y: usize| img.px[(y * 16 + x) * 4];
        assert!(at(1, 1) > 100, "the start of the line is drawn");
        assert!(at(14, 14) > 100, "and so is the end");
        assert!(at(8, 8) > 100, "and the middle of the diagonal");
        assert_eq!(at(1, 14), 0, "but not the opposite corner");
    }

    #[test]
    fn drawing_outside_the_image_is_clipped_not_panicked() {
        let mut img = Image::new(4, 4, Some(Color::Rgb(0, 0, 0)));
        img.stroke_line(-50.0, -50.0, 100.0, 100.0, 3.0, Color::Rgb(255, 0, 0));
        img.fill_rect(-10.0, -10.0, 100.0, 100.0, Color::Rgb(0, 255, 0));
        img.dot(-5.0, -5.0, 20.0, Color::Rgb(0, 0, 255));
        assert_eq!(img.px.len(), 4 * 4 * 4);
    }

    #[test]
    fn a_plot_maps_data_to_pixels_with_y_flipped() {
        let plot = Plot::new(
            Image::new(100, 50, Some(Color::Reset)),
            [0.0, 10.0],
            [0.0, 100.0],
        );
        assert_eq!(plot.px(0.0), 0.0);
        assert_eq!(plot.px(10.0), 100.0);
        assert_eq!(plot.px(5.0), 50.0);
        assert_eq!(plot.py(100.0), 0.0, "the highest price is the top row");
        assert_eq!(plot.py(0.0), 50.0, "and the lowest is the bottom");
        assert_eq!(plot.x_scale(), 10.0);
    }

    #[test]
    fn a_flat_series_still_gets_an_axis() {
        let plot = Plot::new(
            Image::new(10, 10, Some(Color::Reset)),
            [0.0, 1.0],
            [5.0, 5.0],
        );
        let y = plot.py(5.0);
        assert!(y.is_finite(), "a flat range must not divide by zero");
        assert!((y - 5.0).abs() < 1.0, "and should sit mid-plot");
    }

    #[test]
    fn image_size_scales_a_huge_area_down_instead_of_out() {
        // Smaller than a cell in either direction: nothing worth drawing.
        assert_eq!(image_size(Rect::new(0, 0, 1, 40)), None);

        let (w, h) = image_size(Rect::new(0, 0, 80, 24)).unwrap();
        assert!(w > 80 && h > 24, "an image is drawn above cell resolution");

        // Four times as wide as it is tall, in cells and therefore in pixels.
        let (w, h) = image_size(Rect::new(0, 0, 800, 100)).unwrap();
        assert!(
            w * h <= MAX_PIXELS,
            "a wall-sized terminal must stay under the cap: got {w}x{h}"
        );
        let (cw, ch) = cell_size();
        let aspect = (800 * cw as u32) as f64 / (100 * ch as u32) as f64;
        assert!(
            ((w as f64 / h as f64) - aspect).abs() < 0.1,
            "the cap must scale the image, not crop it: {w}x{h}"
        );
    }

    /// The whole placement lifecycle, over a state of this test's own — the
    /// real queue belongs to whatever the app is drawing.
    #[test]
    fn a_slot_is_transmitted_once_then_skipped_until_it_changes() {
        let area = Rect::new(4, 2, 10, 6);
        let mut out = Vec::new();
        let mut st = State::default();
        let submit = |st: &mut State, image: Image| {
            st.queue.push(Pending {
                slot: "test.slot",
                area,
                image,
            });
        };

        submit(&mut st, Image::new(4, 4, Some(Color::Rgb(1, 2, 3))));
        flush(&mut st, &mut out).unwrap();
        let first = String::from_utf8(out.clone()).unwrap();
        assert!(
            first.contains("\x1b[3;5H"),
            "the cursor moves to the area's top-left cell first: {first:?}"
        );
        assert!(first.contains("a=T,f=32,o=z,s=4,v=4,c=10,r=6"));
        assert!(
            first.contains("z=-1") && first.contains("C=1") && first.contains("q=2"),
            "under the text, without moving the cursor, and without a reply"
        );
        assert!(first.contains("m=0;"), "and ends the transmission");

        // An unchanged frame must not put the image back on the wire.
        out.clear();
        submit(&mut st, Image::new(4, 4, Some(Color::Rgb(1, 2, 3))));
        flush(&mut st, &mut out).unwrap();
        assert!(out.is_empty(), "an unchanged slot is not re-sent");

        // Different pixels, same slot: re-sent, and the stale placement is
        // dropped first so the two do not stack.
        out.clear();
        submit(&mut st, Image::new(4, 4, Some(Color::Rgb(9, 9, 9))));
        flush(&mut st, &mut out).unwrap();
        let redrawn = String::from_utf8(out.clone()).unwrap();
        assert!(
            redrawn.starts_with("\x1b_Ga=d,d=i,i="),
            "deleted, then sent"
        );
        assert!(redrawn.contains("a=T,f=32"));

        // A frame that draws no chart takes the placement away with it.
        out.clear();
        flush(&mut st, &mut out).unwrap();
        let gone = String::from_utf8(out.clone()).unwrap();
        assert!(gone.contains("a=d,d=i,i="), "the placement is deleted");
        assert!(!gone.contains("a=T"), "and nothing is drawn in its place");

        out.clear();
        flush(&mut st, &mut out).unwrap();
        assert!(out.is_empty(), "there is nothing left to delete");
    }

    #[test]
    fn a_large_image_is_split_into_continuation_chunks() {
        let mut out = Vec::new();
        // Noise, because a flat image compresses down to a single chunk — the
        // point of the test is the chunking, not the payload.
        let mut img = Image::new(120, 120, Some(Color::Rgb(0, 0, 0)));
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for b in img.px.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = seed as u8;
        }
        transmit(&mut out, 7, &img, Rect::new(0, 0, 8, 4)).unwrap();
        let s = String::from_utf8(out).unwrap();

        let escapes = s.matches("\x1b_G").count();
        assert!(escapes >= 3, "several chunks: {escapes}");
        assert_eq!(
            s.matches("m=1;").count(),
            escapes - 1,
            "every chunk but the last says more is coming"
        );
        assert_eq!(s.matches("m=0;").count(), 1, "exactly one final chunk");
        assert_eq!(
            s.matches("a=T").count(),
            1,
            "the keys are sent once, on the first chunk"
        );
        assert!(s.ends_with("\x1b\\"));
    }

    #[test]
    fn indexed_colours_cover_the_whole_range() {
        assert_eq!(indexed(0), (0, 0, 0));
        assert_eq!(indexed(15), (255, 255, 255));
        assert_eq!(indexed(16), (0, 0, 0), "the base of the colour cube");
        assert_eq!(indexed(231), (255, 255, 255), "and its far corner");
        assert_eq!(indexed(232), (8, 8, 8), "the greyscale ramp");
        assert_eq!(indexed(255), (238, 238, 238));
    }
}
