//! Technical indicators over OHLCV bars and intraday ticks.
//!
//! Every function returns a vector the same length as its input, with `None`
//! for the warm-up region where the indicator has not yet accumulated enough
//! history to be meaningful. See the module docs on [`super`] for the full
//! contract.

use crate::model::{Bar, Tick};

/// Convenience: a fully undefined series of the requested length.
fn undefined(len: usize) -> Vec<Option<f64>> {
    vec![None; len]
}

/// `Some(x)` if `x` is finite, otherwise `None`.
///
/// Used as the last gate before a value escapes to the UI, so that a
/// pathological input (a zero-range bar, a corrupt price of 0.0) shows up as a
/// gap in the overlay rather than as a `NaN` that would poison the chart's
/// min/max scaling.
fn finite(x: f64) -> Option<f64> {
    if x.is_finite() { Some(x) } else { None }
}

/// Extract closing prices from a bar series.
///
/// Most price-series helpers (`sma`, `ema`, `rsi`, ...) take a bare `&[f64]`
/// so they can be reused for index levels, equity curves or the MACD line
/// itself; this is the usual adapter from bars.
pub fn closes(bars: &[Bar]) -> Vec<f64> {
    bars.iter().map(|b| b.close).collect()
}

/// Extract typical prices — `(high + low + close) / 3` — from a bar series.
pub fn typicals(bars: &[Bar]) -> Vec<f64> {
    bars.iter().map(|b| b.typical()).collect()
}

// ---------------------------------------------------------------------------
// Moving averages
// ---------------------------------------------------------------------------

/// Simple moving average — the unweighted mean of the last `period` values.
///
/// The classic trend filter: price above its SMA is conventionally read as an
/// uptrend, and crossings of a fast SMA through a slow one are the oldest
/// mechanical trade signal there is. Because every observation in the window
/// carries equal weight, the SMA reacts late to a shock but is also the least
/// noisy of the moving averages.
///
/// The first `period - 1` outputs are `None`. If `period == 0` or the input is
/// shorter than `period`, the whole output is `None`.
pub fn sma(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = values.len();
    if period == 0 || n < period {
        return undefined(n);
    }

    let mut out = undefined(n);
    let mut sum: f64 = values[..period].iter().sum();
    out[period - 1] = finite(sum / period as f64);

    for i in period..n {
        sum += values[i] - values[i - period];
        out[i] = finite(sum / period as f64);
    }
    out
}

/// Exponential moving average — a geometrically decaying weighted mean.
///
/// Weights recent prices more heavily than old ones (smoothing factor
/// `2 / (period + 1)`), so it turns faster than an SMA of the same length at
/// the cost of more whipsaw. Seeded with the SMA of the first `period` values,
/// which is the convention used by virtually every charting package and the
/// one the MACD definition assumes.
///
/// The first `period - 1` outputs are `None`.
pub fn ema(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = values.len();
    if period == 0 || n < period {
        return undefined(n);
    }

    let mut out = undefined(n);
    let seed = values[..period].iter().sum::<f64>() / period as f64;
    let k = 2.0 / (period as f64 + 1.0);

    let mut prev = seed;
    out[period - 1] = finite(seed);
    for i in period..n {
        prev = (values[i] - prev) * k + prev;
        out[i] = finite(prev);
    }
    out
}

// ---------------------------------------------------------------------------
// Momentum
// ---------------------------------------------------------------------------

/// Relative Strength Index (Wilder, 1978) on a 0–100 scale.
///
/// Compares the average magnitude of up-closes to that of down-closes over the
/// lookback, using Wilder's recursive smoothing (an EMA with factor
/// `1 / period`). Readings above 70 are traditionally called overbought and
/// below 30 oversold, though on a strongly trending PSX scrip the indicator can
/// sit pinned at an extreme for weeks.
///
/// The first `period` outputs are `None` — `period` price *changes* are needed,
/// which costs one extra bar. A window with no down-closes at all yields
/// exactly `100.0` rather than a division by zero.
pub fn rsi(closes: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = closes.len();
    if period == 0 || n <= period {
        return undefined(n);
    }

    let mut out = undefined(n);

    // Seed: arithmetic mean of the first `period` gains and losses.
    let (mut avg_gain, mut avg_loss) = (0.0, 0.0);
    for i in 1..=period {
        let delta = closes[i] - closes[i - 1];
        if delta >= 0.0 {
            avg_gain += delta;
        } else {
            avg_loss -= delta;
        }
    }
    avg_gain /= period as f64;
    avg_loss /= period as f64;
    out[period] = finite(rsi_from(avg_gain, avg_loss));

    // Wilder smoothing for the rest of the series.
    let p = period as f64;
    for i in (period + 1)..n {
        let delta = closes[i] - closes[i - 1];
        let (gain, loss) = if delta >= 0.0 {
            (delta, 0.0)
        } else {
            (0.0, -delta)
        };
        avg_gain = (avg_gain * (p - 1.0) + gain) / p;
        avg_loss = (avg_loss * (p - 1.0) + loss) / p;
        out[i] = finite(rsi_from(avg_gain, avg_loss));
    }
    out
}

/// RSI from smoothed average gain/loss, with the zero-loss branch pinned to 100.
fn rsi_from(avg_gain: f64, avg_loss: f64) -> f64 {
    if avg_loss <= 0.0 {
        // No downside in the window: maximally strong (and a flat series, where
        // both averages are zero, is treated the same way by convention).
        return 100.0;
    }
    let rs = avg_gain / avg_loss;
    100.0 - 100.0 / (1.0 + rs)
}

/// Moving Average Convergence/Divergence — the three aligned MACD series.
#[derive(Debug, Clone, PartialEq)]
pub struct MacdOutput {
    /// Fast EMA minus slow EMA. Positive means short-term momentum leads.
    pub macd: Vec<Option<f64>>,
    /// EMA of the MACD line — the trigger for crossover signals.
    pub signal: Vec<Option<f64>>,
    /// `macd - signal`. Sign flips mark crossovers; the bar height shows how
    /// fast momentum is changing.
    pub histogram: Vec<Option<f64>>,
}

/// MACD (Appel) — momentum as the spread between two EMAs of price.
///
/// The MACD line is `ema(fast) - ema(slow)`; the signal line is an EMA of that
/// spread and the histogram is their difference. A MACD line crossing above its
/// signal is a bullish trigger, below it a bearish one, while the histogram
/// leads both by showing momentum decelerating before the cross happens.
/// Conventional parameters are 12 / 26 / 9.
///
/// The signal EMA is computed only over the region where the MACD line exists
/// and is then re-expanded to full length, so all three vectors stay aligned to
/// the input bars.
pub fn macd(closes: &[f64], fast: usize, slow: usize, signal: usize) -> MacdOutput {
    let n = closes.len();
    let empty = MacdOutput {
        macd: undefined(n),
        signal: undefined(n),
        histogram: undefined(n),
    };
    if fast == 0 || slow == 0 || signal == 0 || n == 0 {
        return empty;
    }

    let fast_ema = ema(closes, fast);
    let slow_ema = ema(closes, slow);

    let mut macd_line = undefined(n);
    for i in 0..n {
        if let (Some(f), Some(s)) = (fast_ema[i], slow_ema[i]) {
            macd_line[i] = finite(f - s);
        }
    }

    // Compact the defined region, EMA it, then scatter back to full length.
    let first_defined = macd_line.iter().position(|v| v.is_some());
    let mut signal_line = undefined(n);
    if let Some(start) = first_defined {
        let dense: Vec<f64> = macd_line[start..].iter().filter_map(|v| *v).collect();
        for (offset, value) in ema(&dense, signal).into_iter().enumerate() {
            signal_line[start + offset] = value;
        }
    }

    let mut histogram = undefined(n);
    for i in 0..n {
        if let (Some(m), Some(s)) = (macd_line[i], signal_line[i]) {
            histogram[i] = finite(m - s);
        }
    }

    MacdOutput {
        macd: macd_line,
        signal: signal_line,
        histogram,
    }
}

/// The three Bollinger bands, aligned to the input.
#[derive(Debug, Clone, PartialEq)]
pub struct BollingerOutput {
    /// Middle band plus `std_devs` standard deviations.
    pub upper: Vec<Option<f64>>,
    /// The simple moving average at the centre of the channel.
    pub middle: Vec<Option<f64>>,
    /// Middle band minus `std_devs` standard deviations.
    pub lower: Vec<Option<f64>>,
}

/// Bollinger Bands — a volatility envelope around a moving average.
///
/// The channel widens when realised volatility rises and pinches shut when it
/// collapses; a "squeeze" (unusually narrow bands) often precedes a directional
/// move, and touches of the outer bands mark statistically stretched prices
/// rather than automatic reversals. Typical settings are 20 periods and 2
/// standard deviations.
///
/// Uses the *population* standard deviation over the window, matching the
/// original definition. On a perfectly flat window the bands collapse onto the
/// middle line (deviation zero) rather than producing a degenerate value.
pub fn bollinger(closes: &[f64], period: usize, std_devs: f64) -> BollingerOutput {
    let n = closes.len();
    if period == 0 || n < period || !std_devs.is_finite() {
        return BollingerOutput {
            upper: undefined(n),
            middle: undefined(n),
            lower: undefined(n),
        };
    }

    let middle = sma(closes, period);
    let mut upper = undefined(n);
    let mut lower = undefined(n);

    for i in (period - 1)..n {
        let Some(mean) = middle[i] else { continue };
        let window = &closes[i + 1 - period..=i];
        let variance = window.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / period as f64;
        let sd = variance.max(0.0).sqrt();
        upper[i] = finite(mean + std_devs * sd);
        lower[i] = finite(mean - std_devs * sd);
    }

    BollingerOutput {
        upper,
        middle,
        lower,
    }
}

// ---------------------------------------------------------------------------
// Volatility and volume
// ---------------------------------------------------------------------------

/// Average True Range (Wilder) — realised volatility in price units.
///
/// True range is the day's span including any overnight gap, so ATR measures
/// how far a scrip actually travels per session. It is directionless: it sizes
/// stops and positions rather than predicting direction. On PSX it also flags
/// circuit-breaker days, which show up as an abrupt ATR expansion.
///
/// Seeded with the arithmetic mean of the first `period` true ranges and then
/// Wilder-smoothed; the first `period - 1` outputs are `None`.
pub fn atr(bars: &[Bar], period: usize) -> Vec<Option<f64>> {
    let n = bars.len();
    if period == 0 || n < period {
        return undefined(n);
    }

    let tr: Vec<f64> = bars
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let raw = b.true_range(if i == 0 { None } else { Some(&bars[i - 1]) });
            if raw.is_finite() { raw.max(0.0) } else { 0.0 }
        })
        .collect();

    let mut out = undefined(n);
    let mut value = tr[..period].iter().sum::<f64>() / period as f64;
    out[period - 1] = finite(value);

    let p = period as f64;
    for i in period..n {
        value = (value * (p - 1.0) + tr[i]) / p;
        out[i] = finite(value);
    }
    out
}

/// On-Balance Volume — a cumulative, signed volume tally.
///
/// Adds the session's volume when the close rises and subtracts it when the
/// close falls, on the theory that volume precedes price. The absolute level is
/// meaningless (it depends on where the series starts); what matters is the
/// slope, and above all divergence — OBV making lower highs while price makes
/// higher highs suggests the advance is not being funded.
///
/// The series is defined from the first bar, which is anchored at `0.0`.
pub fn obv(bars: &[Bar]) -> Vec<Option<f64>> {
    let n = bars.len();
    if n == 0 {
        return Vec::new();
    }

    let mut out = undefined(n);
    let mut total = 0.0;
    out[0] = Some(0.0);

    for i in 1..n {
        let volume = if bars[i].volume.is_finite() {
            bars[i].volume
        } else {
            0.0
        };
        if bars[i].close > bars[i - 1].close {
            total += volume;
        } else if bars[i].close < bars[i - 1].close {
            total -= volume;
        }
        out[i] = finite(total);
    }
    out
}

/// Session VWAP — the running volume-weighted average price over intraday ticks.
///
/// The benchmark institutional desks are measured against: trading below VWAP
/// is a better-than-average buy for the session. Because it resets each day, it
/// is only meaningful on the intraday screen, and the caller is responsible for
/// passing ticks from a single session.
///
/// Zero- and non-positive-volume ticks (PSX publishes a few, typically crossed
/// or corrected trades) contribute nothing to the accumulator, but still emit an
/// aligned output element carrying the VWAP as of that instant. Leading ticks
/// before any volume has traded are `None`.
pub fn vwap_session(ticks: &[Tick]) -> Vec<Option<f64>> {
    let n = ticks.len();
    let mut out = undefined(n);

    let mut cumulative_pv = 0.0;
    let mut cumulative_volume = 0.0;

    for (i, tick) in ticks.iter().enumerate() {
        if tick.volume > 0.0 && tick.volume.is_finite() && tick.price.is_finite() {
            cumulative_pv += tick.price * tick.volume;
            cumulative_volume += tick.volume;
        }
        if cumulative_volume > 0.0 {
            out[i] = finite(cumulative_pv / cumulative_volume);
        }
    }
    out
}

/// The stochastic oscillator's two lines, aligned to the input.
#[derive(Debug, Clone, PartialEq)]
pub struct StochasticOutput {
    /// Fast %K — where the close sits within the recent high/low range, 0–100.
    pub k: Vec<Option<f64>>,
    /// Slow %D — the moving average of %K used as the signal line.
    pub d: Vec<Option<f64>>,
}

/// Stochastic oscillator (Lane) — the close's position in its recent range.
///
/// `%K = 100 * (close - lowest low) / (highest high - lowest low)` over
/// `k_period` bars, with `%D` the `d_period` SMA of `%K`. Near 100 the scrip is
/// closing at the top of its range (strength, or exhaustion), near 0 at the
/// bottom. %K crossing %D out of an extreme is the standard signal.
///
/// A completely flat range — a limit-locked or untraded PSX session where high
/// equals low — would divide by zero, so it is reported as the neutral `50.0`.
pub fn stochastic(bars: &[Bar], k_period: usize, d_period: usize) -> StochasticOutput {
    let n = bars.len();
    if k_period == 0 || d_period == 0 || n < k_period {
        return StochasticOutput {
            k: undefined(n),
            d: undefined(n),
        };
    }

    let mut k = undefined(n);
    for i in (k_period - 1)..n {
        let window = &bars[i + 1 - k_period..=i];
        let highest = window.iter().fold(f64::NEG_INFINITY, |m, b| m.max(b.high));
        let lowest = window.iter().fold(f64::INFINITY, |m, b| m.min(b.low));
        let range = highest - lowest;
        let value = if !range.is_finite() || range <= 0.0 {
            50.0
        } else {
            100.0 * (bars[i].close - lowest) / range
        };
        k[i] = finite(value);
    }

    // %D is an SMA of the defined region of %K, re-expanded to full length.
    let mut d = undefined(n);
    let start = k_period - 1;
    let dense: Vec<f64> = k[start..].iter().filter_map(|v| *v).collect();
    if dense.len() == n - start {
        for (offset, value) in sma(&dense, d_period).into_iter().enumerate() {
            d[start + offset] = value;
        }
    }

    StochasticOutput { k, d }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a bar with the given OHLCV; `ts` is the index, which is all the
    /// indicators need (they are position-based, not calendar-based).
    fn bar(i: i64, open: f64, high: f64, low: f64, close: f64, volume: f64) -> Bar {
        Bar {
            ts: i,
            open,
            high,
            low,
            close,
            volume,
        }
    }

    /// A bar whose whole range is a single price — the flat/limit-locked case.
    fn flat_bar(i: i64, price: f64, volume: f64) -> Bar {
        bar(i, price, price, price, price, volume)
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    fn assert_all_finite(series: &[Option<f64>]) {
        for (i, v) in series.iter().enumerate() {
            if let Some(x) = v {
                assert!(x.is_finite(), "index {i} leaked a non-finite value: {x}");
            }
        }
    }

    // -- sma ---------------------------------------------------------------

    #[test]
    fn sma_matches_hand_computed_values() {
        let values = [1.0, 2.0, 3.0, 4.0, 5.0];
        let out = sma(&values, 3);
        assert_eq!(out.len(), values.len());
        assert_eq!(out[0], None);
        assert_eq!(out[1], None);
        approx(out[2].unwrap(), 2.0); // (1+2+3)/3
        approx(out[3].unwrap(), 3.0); // (2+3+4)/3
        approx(out[4].unwrap(), 4.0); // (3+4+5)/3
    }

    #[test]
    fn sma_period_one_is_the_identity() {
        let values = [7.0, -3.0, 0.0];
        let out = sma(&values, 1);
        assert_eq!(out, vec![Some(7.0), Some(-3.0), Some(0.0)]);
    }

    #[test]
    fn sma_rolling_sum_does_not_drift() {
        let values: Vec<f64> = (0..500).map(|i| (i as f64) * 0.1).collect();
        let out = sma(&values, 20);
        let last = out.last().unwrap().unwrap();
        let expected = values[480..].iter().sum::<f64>() / 20.0;
        assert!((last - expected).abs() < 1e-9);
    }

    #[test]
    fn sma_guards_degenerate_input() {
        assert!(sma(&[], 5).is_empty());
        assert_eq!(sma(&[1.0, 2.0], 5), vec![None, None]);
        assert_eq!(sma(&[1.0, 2.0], 0), vec![None, None]);
    }

    // -- ema ---------------------------------------------------------------

    #[test]
    fn ema_seeds_with_sma_then_smooths() {
        let values = [1.0, 2.0, 3.0, 4.0, 5.0];
        let out = ema(&values, 3);
        assert_eq!(out.len(), 5);
        assert_eq!((out[0], out[1]), (None, None));
        approx(out[2].unwrap(), 2.0); // seed = SMA(1,2,3)
        // k = 2/4 = 0.5 -> 2 + 0.5*(4-2) = 3
        approx(out[3].unwrap(), 3.0);
        // 3 + 0.5*(5-3) = 4
        approx(out[4].unwrap(), 4.0);
    }

    #[test]
    fn ema_of_a_constant_series_is_that_constant() {
        let values = vec![42.0; 30];
        for v in ema(&values, 10).into_iter().flatten() {
            approx(v, 42.0);
        }
    }

    #[test]
    fn ema_guards_degenerate_input() {
        assert!(ema(&[], 5).is_empty());
        assert_eq!(ema(&[1.0], 5), vec![None]);
        assert_eq!(ema(&[1.0], 0), vec![None]);
    }

    // -- rsi ---------------------------------------------------------------

    #[test]
    fn rsi_warmup_is_period_long() {
        let closes: Vec<f64> = (1..=30).map(|i| i as f64).collect();
        let out = rsi(&closes, 14);
        assert_eq!(out.len(), closes.len());
        assert!(out[..14].iter().all(|v| v.is_none()));
        assert!(out[14].is_some());
    }

    #[test]
    fn rsi_is_100_when_there_are_no_losses() {
        let closes: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        let out = rsi(&closes, 14);
        for v in out.into_iter().flatten() {
            approx(v, 100.0);
        }
    }

    #[test]
    fn rsi_is_zero_on_a_pure_downtrend() {
        let closes: Vec<f64> = (1..=20).rev().map(|i| i as f64).collect();
        let out = rsi(&closes, 14);
        for v in out.into_iter().flatten() {
            approx(v, 0.0);
        }
    }

    #[test]
    fn rsi_matches_hand_computed_value() {
        // Three periods: gains of 1,1 and a loss of 2 -> avg gain 2/3,
        // avg loss 2/3, RS = 1, RSI = 50.
        let closes = [10.0, 11.0, 12.0, 10.0];
        let out = rsi(&closes, 3);
        approx(out[3].unwrap(), 50.0);
        assert!(out[..3].iter().all(|v| v.is_none()));
    }

    #[test]
    fn rsi_on_a_flat_series_is_finite() {
        let closes = vec![100.0; 20];
        let out = rsi(&closes, 14);
        assert_all_finite(&out);
        approx(out[14].unwrap(), 100.0);
    }

    #[test]
    fn rsi_guards_degenerate_input() {
        assert!(rsi(&[], 14).is_empty());
        // Needs period + 1 closes; exactly `period` is still all-None.
        assert_eq!(rsi(&[1.0, 2.0, 3.0], 3), vec![None, None, None]);
        assert_eq!(rsi(&[1.0, 2.0], 0), vec![None, None]);
    }

    // -- macd --------------------------------------------------------------

    #[test]
    fn macd_series_are_aligned_and_warm_up_correctly() {
        let closes: Vec<f64> = (1..=60).map(|i| i as f64).collect();
        let out = macd(&closes, 12, 26, 9);
        assert_eq!(out.macd.len(), closes.len());
        assert_eq!(out.signal.len(), closes.len());
        assert_eq!(out.histogram.len(), closes.len());

        // MACD is defined once the slow EMA is, i.e. from index 25.
        assert!(out.macd[..25].iter().all(|v| v.is_none()));
        assert!(out.macd[25].is_some());
        // Signal needs 9 more MACD points: index 25 + 8 = 33.
        assert!(out.signal[..33].iter().all(|v| v.is_none()));
        assert!(out.signal[33].is_some());
        assert!(out.histogram[32].is_none());
        assert!(out.histogram[33].is_some());

        approx(
            out.histogram[40].unwrap(),
            out.macd[40].unwrap() - out.signal[40].unwrap(),
        );
        assert_all_finite(&out.macd);
        assert_all_finite(&out.signal);
        assert_all_finite(&out.histogram);
    }

    #[test]
    fn macd_of_a_constant_series_is_zero() {
        let closes = vec![50.0; 60];
        let out = macd(&closes, 12, 26, 9);
        for v in out.macd.into_iter().flatten() {
            approx(v, 0.0);
        }
        for v in out.histogram.into_iter().flatten() {
            approx(v, 0.0);
        }
    }

    #[test]
    fn macd_guards_degenerate_input() {
        let out = macd(&[], 12, 26, 9);
        assert!(out.macd.is_empty() && out.signal.is_empty() && out.histogram.is_empty());

        let closes = vec![1.0; 5];
        let out = macd(&closes, 12, 26, 9);
        assert_eq!(out.macd, vec![None; 5]);
        assert_eq!(out.signal, vec![None; 5]);

        let out = macd(&closes, 0, 26, 9);
        assert_eq!(out.macd, vec![None; 5]);
    }

    // -- bollinger ---------------------------------------------------------

    #[test]
    fn bollinger_matches_hand_computed_values() {
        // Window [2,4,4,4,5,5,7,9] has mean 5 and population sd 2.
        let closes = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let out = bollinger(&closes, 8, 2.0);
        approx(out.middle[7].unwrap(), 5.0);
        approx(out.upper[7].unwrap(), 9.0);
        approx(out.lower[7].unwrap(), 1.0);
        assert!(out.middle[..7].iter().all(|v| v.is_none()));
    }

    #[test]
    fn bollinger_bands_collapse_on_a_flat_series() {
        let closes = vec![10.0; 25];
        let out = bollinger(&closes, 20, 2.0);
        approx(out.upper[24].unwrap(), 10.0);
        approx(out.lower[24].unwrap(), 10.0);
        assert_all_finite(&out.upper);
        assert_all_finite(&out.lower);
    }

    #[test]
    fn bollinger_guards_degenerate_input() {
        let out = bollinger(&[], 20, 2.0);
        assert!(out.upper.is_empty());
        let out = bollinger(&[1.0, 2.0], 20, 2.0);
        assert_eq!(out.upper, vec![None, None]);
        let out = bollinger(&[1.0, 2.0], 0, 2.0);
        assert_eq!(out.middle, vec![None, None]);
        let out = bollinger(&[1.0, 2.0, 3.0], 2, f64::NAN);
        assert_eq!(out.upper, vec![None; 3]);
    }

    // -- atr ---------------------------------------------------------------

    #[test]
    fn atr_matches_hand_computed_values() {
        // Ranges: 2 (first bar, no prev), then 2, 2 with no gaps.
        let bars = vec![
            bar(0, 10.0, 11.0, 9.0, 10.0, 100.0),
            bar(1, 10.0, 11.0, 9.0, 10.0, 100.0),
            bar(2, 10.0, 11.0, 9.0, 10.0, 100.0),
        ];
        let out = atr(&bars, 3);
        assert_eq!(out.len(), 3);
        assert_eq!((out[0], out[1]), (None, None));
        approx(out[2].unwrap(), 2.0);
    }

    #[test]
    fn atr_accounts_for_overnight_gaps() {
        let bars = vec![
            bar(0, 10.0, 10.0, 10.0, 10.0, 1.0),
            // Gaps up to 20 with a zero intraday range: TR = |20 - 10| = 10.
            bar(1, 20.0, 20.0, 20.0, 20.0, 1.0),
        ];
        let out = atr(&bars, 2);
        approx(out[1].unwrap(), 5.0); // mean of 0 and 10
    }

    #[test]
    fn atr_is_zero_and_finite_on_flat_bars() {
        let bars: Vec<Bar> = (0..20).map(|i| flat_bar(i, 5.0, 0.0)).collect();
        let out = atr(&bars, 14);
        assert_all_finite(&out);
        approx(out[19].unwrap(), 0.0);
    }

    #[test]
    fn atr_guards_degenerate_input() {
        assert!(atr(&[], 14).is_empty());
        let bars = vec![flat_bar(0, 1.0, 1.0)];
        assert_eq!(atr(&bars, 14), vec![None]);
        assert_eq!(atr(&bars, 0), vec![None]);
    }

    // -- obv ---------------------------------------------------------------

    #[test]
    fn obv_signs_volume_by_close_direction() {
        let bars = vec![
            bar(0, 10.0, 10.0, 10.0, 10.0, 100.0),
            bar(1, 10.0, 12.0, 10.0, 11.0, 200.0), // up   -> +200
            bar(2, 11.0, 11.0, 9.0, 10.0, 300.0),  // down -> -300
            bar(3, 10.0, 10.0, 10.0, 10.0, 400.0), // flat -> unchanged
        ];
        let out = obv(&bars);
        assert_eq!(
            out,
            vec![Some(0.0), Some(200.0), Some(-100.0), Some(-100.0)]
        );
    }

    #[test]
    fn obv_tolerates_zero_volume_sessions() {
        let bars: Vec<Bar> = (0..5).map(|i| flat_bar(i, 3.0, 0.0)).collect();
        let out = obv(&bars);
        assert_eq!(out.len(), 5);
        assert_all_finite(&out);
    }

    #[test]
    fn obv_on_empty_input_is_empty() {
        assert!(obv(&[]).is_empty());
    }

    // -- vwap --------------------------------------------------------------

    #[test]
    fn vwap_is_the_running_volume_weighted_mean() {
        let ticks = vec![
            Tick {
                ts: 0,
                price: 10.0,
                volume: 100.0,
            },
            Tick {
                ts: 1,
                price: 20.0,
                volume: 100.0,
            },
            Tick {
                ts: 2,
                price: 30.0,
                volume: 200.0,
            },
        ];
        let out = vwap_session(&ticks);
        approx(out[0].unwrap(), 10.0);
        approx(out[1].unwrap(), 15.0);
        approx(out[2].unwrap(), 22.5); // (1000+2000+6000)/400
    }

    #[test]
    fn vwap_skips_zero_volume_ticks_but_stays_aligned() {
        let ticks = vec![
            Tick {
                ts: 0,
                price: 99.0,
                volume: 0.0,
            },
            Tick {
                ts: 1,
                price: 10.0,
                volume: 100.0,
            },
            Tick {
                ts: 2,
                price: 99.0,
                volume: 0.0,
            },
        ];
        let out = vwap_session(&ticks);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], None); // nothing has traded yet
        approx(out[1].unwrap(), 10.0);
        approx(out[2].unwrap(), 10.0); // unchanged by the zero-volume print
    }

    #[test]
    fn vwap_on_empty_input_is_empty() {
        assert!(vwap_session(&[]).is_empty());
    }

    // -- stochastic --------------------------------------------------------

    #[test]
    fn stochastic_matches_hand_computed_values() {
        let bars = vec![
            bar(0, 10.0, 12.0, 8.0, 9.0, 1.0),
            bar(1, 9.0, 14.0, 9.0, 13.0, 1.0),
            bar(2, 13.0, 16.0, 12.0, 16.0, 1.0),
        ];
        let out = stochastic(&bars, 3, 1);
        assert_eq!(out.k.len(), 3);
        assert_eq!((out.k[0], out.k[1]), (None, None));
        // Range over the three bars is 8..16; close 16 -> 100%.
        approx(out.k[2].unwrap(), 100.0);
        // %D with d_period 1 equals %K.
        approx(out.d[2].unwrap(), 100.0);
    }

    #[test]
    fn stochastic_d_line_lags_k_by_d_period() {
        let bars: Vec<Bar> = (0..20)
            .map(|i| {
                bar(
                    i,
                    10.0,
                    12.0 + i as f64,
                    8.0 + i as f64,
                    10.0 + i as f64,
                    1.0,
                )
            })
            .collect();
        let out = stochastic(&bars, 5, 3);
        assert!(out.k[..4].iter().all(|v| v.is_none()));
        assert!(out.k[4].is_some());
        assert!(out.d[..6].iter().all(|v| v.is_none()));
        assert!(out.d[6].is_some());
        assert_all_finite(&out.k);
        assert_all_finite(&out.d);
    }

    #[test]
    fn stochastic_is_neutral_on_a_zero_range_window() {
        let bars: Vec<Bar> = (0..10).map(|i| flat_bar(i, 7.0, 0.0)).collect();
        let out = stochastic(&bars, 5, 3);
        approx(out.k[9].unwrap(), 50.0);
        approx(out.d[9].unwrap(), 50.0);
        assert_all_finite(&out.k);
    }

    #[test]
    fn stochastic_guards_degenerate_input() {
        let out = stochastic(&[], 14, 3);
        assert!(out.k.is_empty() && out.d.is_empty());

        let bars: Vec<Bar> = (0..3).map(|i| flat_bar(i, 1.0, 1.0)).collect();
        let out = stochastic(&bars, 14, 3);
        assert_eq!(out.k, vec![None; 3]);
        let out = stochastic(&bars, 0, 3);
        assert_eq!(out.k, vec![None; 3]);
        let out = stochastic(&bars, 2, 0);
        assert_eq!(out.d, vec![None; 3]);
    }

    #[test]
    fn stochastic_d_is_all_none_when_window_is_too_short() {
        let bars: Vec<Bar> = (0..5).map(|i| bar(i, 1.0, 2.0, 0.0, 1.0, 1.0)).collect();
        let out = stochastic(&bars, 5, 3);
        assert!(out.k[4].is_some());
        assert_eq!(out.d, vec![None; 5]); // only one %K point, needs three
    }

    // -- shared invariants -------------------------------------------------

    #[test]
    fn every_indicator_is_length_preserving() {
        let bars: Vec<Bar> = (0..40)
            .map(|i| {
                let p = 100.0 + (i as f64 * 0.7).sin() * 5.0;
                bar(i, p, p + 1.0, p - 1.0, p + 0.5, 1_000.0 + i as f64)
            })
            .collect();
        let c = closes(&bars);

        assert_eq!(sma(&c, 10).len(), bars.len());
        assert_eq!(ema(&c, 10).len(), bars.len());
        assert_eq!(rsi(&c, 14).len(), bars.len());
        assert_eq!(bollinger(&c, 20, 2.0).upper.len(), bars.len());
        assert_eq!(atr(&bars, 14).len(), bars.len());
        assert_eq!(obv(&bars).len(), bars.len());
        assert_eq!(stochastic(&bars, 14, 3).k.len(), bars.len());
        assert_eq!(typicals(&bars).len(), bars.len());

        let m = macd(&c, 12, 26, 9);
        assert_eq!(m.macd.len(), bars.len());
        assert_eq!(m.signal.len(), bars.len());
        assert_eq!(m.histogram.len(), bars.len());
    }

    #[test]
    fn no_indicator_panics_on_empty_bars() {
        let bars: [Bar; 0] = [];
        assert!(atr(&bars, 14).is_empty());
        assert!(obv(&bars).is_empty());
        assert!(stochastic(&bars, 14, 3).k.is_empty());
        assert!(closes(&bars).is_empty());
        assert!(typicals(&bars).is_empty());
    }
}
