//! Small reusable rendering pieces shared across screens.

use ratatui::prelude::*;
use ratatui::widgets::Block;

use super::theme;

/// A centred rectangle sized as a percentage of `area`, for modal overlays.
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [_, mid, _] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(area);

    let [_, center, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(mid);

    center
}

/// The standard bordered panel used by every screen.
pub fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_style(theme::border_style())
        .title(Span::styled(format!(" {title} "), theme::title_style()))
}

/// A dim-label / bright-value line, padded so values align in a column.
pub fn stat<'a>(label: &'a str, value: impl Into<String>, width: usize) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!(" {label:<width$}", width = width),
            theme::label_style(),
        ),
        Span::styled(value.into(), theme::value_style()),
    ])
}

/// A stat whose value is coloured by sign.
pub fn stat_signed<'a>(label: &'a str, value: f64, text: String, width: usize) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!(" {label:<width$}", width = width),
            theme::label_style(),
        ),
        Span::styled(text, Style::new().fg(theme::change_color(value))),
    ])
}

/// Eight block glyphs, tallest last, for inline bar/sparkline rendering.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Render `values` as a one-line block sparkline of exactly `width` cells.
///
/// Values are bucketed when there are more points than columns so the shape of
/// a long series survives being squeezed into a narrow panel.
pub fn sparkline(values: &[f64], width: usize) -> String {
    if values.is_empty() || width == 0 {
        return String::new();
    }

    let buckets = bucket(values, width);
    let (min, max) = buckets
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));

    // A flat series has no range to scale against; draw it as a mid-height line.
    let span = max - min;
    buckets
        .iter()
        .map(|v| {
            if span <= 0.0 {
                BLOCKS[3]
            } else {
                let t = ((v - min) / span * (BLOCKS.len() - 1) as f64).round();
                BLOCKS[t.clamp(0.0, (BLOCKS.len() - 1) as f64) as usize]
            }
        })
        .collect()
}

/// A sparkline coloured green or red by whether the series ended up or down.
pub fn sparkline_span(values: &[f64], width: usize) -> Span<'static> {
    let color = match (values.first(), values.last()) {
        (Some(a), Some(b)) => theme::change_color(b - a),
        _ => theme::flat(),
    };
    Span::styled(sparkline(values, width), Style::new().fg(color))
}

/// Average `values` down to at most `width` points, preserving overall shape.
fn bucket(values: &[f64], width: usize) -> Vec<f64> {
    if values.len() <= width {
        return values.to_vec();
    }
    let per = values.len() as f64 / width as f64;
    (0..width)
        .map(|i| {
            let start = (i as f64 * per).floor() as usize;
            let end = (((i + 1) as f64 * per).ceil() as usize).min(values.len());
            let slice = &values[start..end.max(start + 1)];
            slice.iter().sum::<f64>() / slice.len() as f64
        })
        .collect()
}

/// A proportional bar of `width` cells filled to `ratio` (0.0–1.0).
pub fn bar(ratio: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let r = ratio.clamp(0.0, 1.0);
    let filled = (r * width as f64 * 8.0).round() as usize;
    let full = filled / 8;
    let rem = filled % 8;

    let mut s: String = "█".repeat(full.min(width));
    if full < width && rem > 0 {
        s.push(BLOCKS[rem - 1]);
    }
    let used = s.chars().count();
    s.push_str(&" ".repeat(width.saturating_sub(used)));
    s
}

/// Background colour for a heatmap cell, scaled by how extreme `pct` is
/// relative to `max_abs`. PSX's daily circuit breaker is ±10%, which makes a
/// natural default saturation point.
pub fn heat_color(pct: f64, max_abs: f64) -> Color {
    if !pct.is_finite() || max_abs <= 0.0 {
        return theme::flat();
    }
    let t = (pct.abs() / max_abs).clamp(0.0, 1.0);
    // Blend from the theme's neutral panel tone toward its up/down hue, so the
    // heatmap belongs to whichever palette is active rather than to the one it
    // was written against.
    let (r0, g0, b0) = rgb(theme::border());
    let (r1, g1, b1) = rgb(if pct >= 0.0 {
        theme::up()
    } else {
        theme::down()
    });
    let mix = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
    Color::Rgb(mix(r0, r1), mix(g0, g1), mix(b0, b1))
}

/// A palette colour's components. Every palette entry is `Color::Rgb`; anything
/// else could only arrive from a caller outside the theme, and grey is a
/// harmless answer for it.
fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (128, 128, 128),
    }
}

/// Centred placeholder for a panel with nothing to show yet.
pub fn placeholder(msg: &str) -> Line<'_> {
    Line::from(Span::styled(msg, theme::label_style())).centered()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_is_exactly_the_requested_width() {
        assert_eq!(sparkline(&[1.0, 2.0, 3.0], 3).chars().count(), 3);
        assert_eq!(
            sparkline(&(0..500).map(|i| i as f64).collect::<Vec<_>>(), 20)
                .chars()
                .count(),
            20
        );
    }

    #[test]
    fn sparkline_rises_with_the_series() {
        let s = sparkline(&[1.0, 2.0, 3.0, 4.0], 4);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars[0], '▁');
        assert_eq!(chars[3], '█');
    }

    #[test]
    fn flat_series_renders_without_dividing_by_zero() {
        let s = sparkline(&[5.0; 6], 6);
        assert_eq!(s.chars().count(), 6);
        assert!(s.chars().all(|c| c == '▄'));
    }

    #[test]
    fn empty_input_is_safe() {
        assert_eq!(sparkline(&[], 10), "");
        assert_eq!(sparkline(&[1.0], 0), "");
        assert_eq!(bar(0.5, 0), "");
    }

    #[test]
    fn bar_fills_proportionally_and_pads_to_width() {
        assert_eq!(bar(1.0, 4).chars().count(), 4);
        assert_eq!(bar(0.0, 4), "    ");
        assert_eq!(bar(1.0, 4), "████");
        // Out-of-range ratios clamp rather than overflow the cell budget.
        assert_eq!(bar(5.0, 4), "████");
        assert_eq!(bar(-1.0, 4).chars().count(), 4);
    }

    #[test]
    fn bucketing_preserves_endpoints_of_a_trend() {
        let values: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let b = bucket(&values, 10);
        assert_eq!(b.len(), 10);
        assert!(b[0] < b[9], "trend direction must survive bucketing");
    }

    #[test]
    fn heat_colour_saturates_toward_the_sign_hue() {
        // Full saturation lands exactly on the theme's own up/down hues, and
        // no change stays at its panel tone — whichever theme is active.
        assert_eq!(heat_color(10.0, 10.0), theme::up());
        assert_eq!(heat_color(-10.0, 10.0), theme::down());
        assert_eq!(heat_color(0.0, 10.0), theme::border());
        assert_eq!(heat_color(f64::NAN, 10.0), theme::flat());
    }
}
