//! One drawing vocabulary, two renderers.
//!
//! A chart is described once — "stroke this line, fill this candle" — and drawn
//! either onto a ratatui canvas of braille cells or into a bitmap for the
//! terminal graphics protocol. Without this the two paths would be two copies
//! of the same geometry, and every indicator added to one of them would quietly
//! be missing from the other.
//!
//! Coordinates are always data-space: a session index across, a price up.

use ratatui::prelude::*;
use ratatui::widgets::canvas::{Context, Line as CanvasLine, Rectangle};

use super::gfx::{self, Image, Plot};
use super::theme;

/// What a chart can ask for.
pub trait Paint {
    /// A stroke between two points.
    fn stroke(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, color: Color);

    /// A filled box, anchored at its bottom-left corner — the same convention
    /// as a candle body or a volume bar.
    fn fill(&mut self, x: f64, y: f64, width: f64, height: f64, color: Color);

    /// A single marked point, for a series too short to have a segment.
    fn point(&mut self, x: f64, y: f64, color: Color);

    /// Whether this renderer draws real pixels.
    ///
    /// Charts use it sparingly — to fill an area that the cell renderer can
    /// only approximate with a comb of strokes, not to draw different content.
    fn is_pixel(&self) -> bool {
        false
    }
}

impl Paint for Context<'_> {
    fn stroke(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, color: Color) {
        self.draw(&CanvasLine {
            x1,
            y1,
            x2,
            y2,
            color,
        });
    }

    fn fill(&mut self, x: f64, y: f64, width: f64, height: f64, color: Color) {
        // A canvas rectangle is an outline, but at the width of a candle on a
        // terminal grid the outline *is* the body — its two sides land on
        // adjacent cells with nothing between them to leave hollow.
        self.draw(&Rectangle {
            x,
            y,
            width,
            height,
            color,
        });
    }

    fn point(&mut self, x: f64, y: f64, color: Color) {
        self.print(x, y, Span::styled("•", Style::new().fg(color)));
    }
}

impl Paint for Plot {
    fn stroke(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, color: Color) {
        let (px1, py1) = (self.px(x1), self.py(y1));
        let (px2, py2) = (self.px(x2), self.py(y2));
        let weight = self.weight;
        self.image.stroke_line(px1, py1, px2, py2, weight, color);
    }

    fn fill(&mut self, x: f64, y: f64, width: f64, height: f64, color: Color) {
        let x0 = self.px(x);
        let x1 = self.px(x + width);
        // y is flipped, so the top of the box in data space is the smaller
        // pixel row.
        let y0 = self.py(y + height);
        let y1 = self.py(y);
        self.image
            .fill_rect(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0), color);
    }

    fn point(&mut self, x: f64, y: f64, color: Color) {
        let (px, py) = (self.px(x), self.py(y));
        let r = self.weight * 1.6;
        self.image.dot(px, py, r, color);
    }

    fn is_pixel(&self) -> bool {
        true
    }
}

/// Start a pixel-rendered chart over `area`, or `None` when this terminal draws
/// with cells — in which case the caller falls back to its canvas.
pub fn begin(area: Rect, x_bounds: [f64; 2], y_bounds: [f64; 2]) -> Option<Plot> {
    if !gfx::available() {
        return None;
    }
    let (w, h) = gfx::image_size(area)?;
    Some(Plot::new(
        Image::new(w, h, theme::palette().bg),
        x_bounds,
        y_bounds,
    ))
}

/// Hand a finished chart to the terminal.
///
/// The cells underneath are blanked in the same pass: the image is placed below
/// the text layer, so anything ratatui left there would show through it.
pub fn finish(f: &mut Frame, slot: &'static str, area: Rect, plot: Plot) {
    f.render_widget(
        ratatui::widgets::Block::default().style(Style::new().bg(theme::palette().bg)),
        area,
    );
    gfx::submit(slot, area, plot.image);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plot(w: u32, h: u32) -> Plot {
        Plot::new(
            Image::new(w, h, Color::Rgb(0, 0, 0)),
            [0.0, 10.0],
            [0.0, 100.0],
        )
    }

    #[test]
    fn a_filled_box_is_anchored_at_its_bottom_left() {
        let mut p = plot(100, 100);
        // Two units wide from x=1, fifty high from y=25 — the top half-ish of
        // the plot's lower half.
        p.fill(1.0, 25.0, 2.0, 50.0, Color::Rgb(255, 255, 255));
        let at = |x: usize, y: usize| p.image.sample(x as u32, y as u32).0;

        assert_eq!(at(15, 50), 255, "inside the box");
        assert_eq!(at(15, 20), 0, "above its top edge");
        assert_eq!(at(15, 80), 0, "below its bottom edge");
        assert_eq!(at(5, 50), 0, "left of it");
        assert_eq!(at(40, 50), 0, "right of it");
    }

    #[test]
    fn a_stroke_runs_between_its_data_points() {
        let mut p = plot(100, 100);
        p.stroke(0.0, 50.0, 10.0, 50.0, Color::Rgb(255, 255, 255));
        // y = 50 of 0..100 is the middle row.
        assert!(p.image.sample(50, 50).0 > 100, "the line is drawn mid-plot");
        assert_eq!(p.image.sample(50, 10).0, 0, "and nowhere near the top");
    }

    #[test]
    fn a_point_marks_its_position() {
        let mut p = plot(100, 100);
        p.point(5.0, 50.0, Color::Rgb(255, 255, 255));
        assert!(p.image.sample(50, 50).0 > 100);
        assert_eq!(p.image.sample(90, 10).0, 0);
    }
}
