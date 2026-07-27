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

// ---------------------------------------------------------------------------
// Trend strength
// ---------------------------------------------------------------------------

/// Wilder's Directional Movement system — ADX plus the two directional lines.
#[derive(Debug, Clone, PartialEq)]
pub struct AdxOutput {
    /// Average Directional Index, 0–100. Trend *strength*, direction-blind.
    pub adx: Vec<Option<f64>>,
    /// +DI: the share of range travelled upward.
    pub plus_di: Vec<Option<f64>>,
    /// -DI: the share of range travelled downward.
    pub minus_di: Vec<Option<f64>>,
}

/// Average Directional Index (Wilder, 1978) — how *trending* a market is.
///
/// Directional movement is the part of each session's range that extends beyond
/// the previous one, up (`+DM`) or down (`-DM`). Each is Wilder-smoothed and
/// normalised by smoothed true range to give `+DI` and `-DI`; `DX` is the
/// normalised spread between them, and ADX is a smoothed `DX`. Above 25 the
/// market is conventionally read as trending — which line is on top says which
/// way. Below 20 it is ranging and trend-following signals are noise.
///
/// Warm-up costs two windows, not one: `+DI`/`-DI` are defined from index
/// `period` (one bar is consumed computing the first change), and ADX only from
/// index `2 * period - 1`, because its seed is the mean of the first `period`
/// `DX` readings. A session with zero true range — a limit-locked or untraded
/// PSX scrip — contributes no directional movement rather than dividing by
/// zero, and a window with no movement at all reports `DX = 0`.
pub fn adx(bars: &[Bar], period: usize) -> AdxOutput {
    let n = bars.len();
    let empty = AdxOutput {
        adx: undefined(n),
        plus_di: undefined(n),
        minus_di: undefined(n),
    };
    if period == 0 || n <= period {
        return empty;
    }

    // Per-bar true range and directional movement. Index 0 has no predecessor,
    // so it contributes nothing and is simply left at zero.
    let mut tr = vec![0.0; n];
    let mut plus_dm = vec![0.0; n];
    let mut minus_dm = vec![0.0; n];
    for i in 1..n {
        let raw = bars[i].true_range(Some(&bars[i - 1]));
        tr[i] = if raw.is_finite() { raw.max(0.0) } else { 0.0 };

        let up = bars[i].high - bars[i - 1].high;
        let down = bars[i - 1].low - bars[i].low;
        if up.is_finite() && down.is_finite() {
            // Only the larger of the two moves counts, and only if positive:
            // an inside day has no directional movement in either direction.
            if up > down && up > 0.0 {
                plus_dm[i] = up;
            }
            if down > up && down > 0.0 {
                minus_dm[i] = down;
            }
        }
    }

    // Wilder smoothing: seed with the sum over the first window, then decay the
    // running total by 1/period each bar rather than dropping a single term.
    let p = period as f64;
    let mut tr_s: f64 = tr[1..=period].iter().sum();
    let mut plus_s: f64 = plus_dm[1..=period].iter().sum();
    let mut minus_s: f64 = minus_dm[1..=period].iter().sum();

    let mut plus_di = undefined(n);
    let mut minus_di = undefined(n);
    let mut dx = undefined(n);

    let record = |i: usize,
                  tr_s: f64,
                  plus_s: f64,
                  minus_s: f64,
                  plus_di: &mut Vec<Option<f64>>,
                  minus_di: &mut Vec<Option<f64>>,
                  dx: &mut Vec<Option<f64>>| {
        // A flat window has no range to normalise against; both lines are zero.
        let (pdi, mdi) = if tr_s > 0.0 {
            (100.0 * plus_s / tr_s, 100.0 * minus_s / tr_s)
        } else {
            (0.0, 0.0)
        };
        let sum = pdi + mdi;
        let d = if sum > 0.0 {
            100.0 * (pdi - mdi).abs() / sum
        } else {
            0.0
        };
        plus_di[i] = finite(pdi);
        minus_di[i] = finite(mdi);
        dx[i] = finite(d);
    };

    record(
        period,
        tr_s,
        plus_s,
        minus_s,
        &mut plus_di,
        &mut minus_di,
        &mut dx,
    );
    for i in (period + 1)..n {
        tr_s = tr_s - tr_s / p + tr[i];
        plus_s = plus_s - plus_s / p + plus_dm[i];
        minus_s = minus_s - minus_s / p + minus_dm[i];
        record(
            i,
            tr_s,
            plus_s,
            minus_s,
            &mut plus_di,
            &mut minus_di,
            &mut dx,
        );
    }

    // ADX seeds with the arithmetic mean of the first `period` DX readings and
    // is Wilder-smoothed thereafter.
    let mut adx_line = undefined(n);
    let first = 2 * period - 1;
    if first < n {
        let mut value = dx[period..=first].iter().flatten().sum::<f64>() / p;
        adx_line[first] = finite(value);
        for (i, item) in dx.iter().enumerate().take(n).skip(first + 1) {
            let d = item.unwrap_or(0.0);
            value = (value * (p - 1.0) + d) / p;
            adx_line[i] = finite(value);
        }
    }

    AdxOutput {
        adx: adx_line,
        plus_di,
        minus_di,
    }
}

/// Commodity Channel Index (Lambert, 1980) — deviation from the mean in units
/// of mean absolute deviation.
///
/// `CCI = (typical - SMA(typical)) / (0.015 * MAD)`, where the 0.015 constant
/// is chosen so that roughly 70–80% of readings fall inside ±100. Despite the
/// name it is used on equities as an overbought/oversold and breakout gauge:
/// crossing above +100 marks an unusually strong push, below -100 an unusually
/// weak one.
///
/// The first `period - 1` outputs are `None`. A window whose typical prices are
/// all identical has zero mean absolute deviation; rather than dividing by zero
/// that is reported as exactly `0.0` — the price *is* the mean.
pub fn cci(bars: &[Bar], period: usize) -> Vec<Option<f64>> {
    let n = bars.len();
    if period == 0 || n < period {
        return undefined(n);
    }

    let tp = typicals(bars);
    let mean = sma(&tp, period);
    let mut out = undefined(n);

    for i in (period - 1)..n {
        let Some(m) = mean[i] else { continue };
        let window = &tp[i + 1 - period..=i];
        let mad = window.iter().map(|v| (v - m).abs()).sum::<f64>() / period as f64;
        let value = if mad > 0.0 {
            (tp[i] - m) / (0.015 * mad)
        } else {
            0.0
        };
        out[i] = finite(value);
    }
    out
}

/// Williams %R (Williams, 1973) — the close's position in its recent range, on
/// an inverted -100..0 scale.
///
/// `%R = -100 * (highest high - close) / (highest high - lowest low)`. It is the
/// stochastic %K flipped: 0 means the close is at the very top of the window,
/// -100 at the very bottom. Above -20 is conventionally overbought and below
/// -80 oversold.
///
/// The first `period - 1` outputs are `None`. A zero-range window — the flat,
/// limit-locked session PSX produces regularly — is reported as the neutral
/// `-50.0` rather than dividing by zero. The result is always within -100..0.
pub fn williams_r(bars: &[Bar], period: usize) -> Vec<Option<f64>> {
    let n = bars.len();
    if period == 0 || n < period {
        return undefined(n);
    }

    let mut out = undefined(n);
    for i in (period - 1)..n {
        let window = &bars[i + 1 - period..=i];
        let highest = window.iter().fold(f64::NEG_INFINITY, |m, b| m.max(b.high));
        let lowest = window.iter().fold(f64::INFINITY, |m, b| m.min(b.low));
        let range = highest - lowest;
        let value = if !range.is_finite() || range <= 0.0 {
            -50.0
        } else {
            (-100.0 * (highest - bars[i].close) / range).clamp(-100.0, 0.0)
        };
        out[i] = finite(value);
    }
    out
}

/// The three Donchian channel lines, aligned to the input.
#[derive(Debug, Clone, PartialEq)]
pub struct DonchianOutput {
    /// Highest high over the window.
    pub upper: Vec<Option<f64>>,
    /// Midpoint of the channel — `(upper + lower) / 2`.
    pub middle: Vec<Option<f64>>,
    /// Lowest low over the window.
    pub lower: Vec<Option<f64>>,
}

/// Donchian channels (Donchian, ~1960) — the rolling high/low envelope.
///
/// The original turtle-trading breakout system: buy a close above the upper
/// channel, sell below the lower one, with the midline as a trailing exit. It
/// is the most literal possible picture of support and resistance — every point
/// on the upper line is a price that actually traded.
///
/// The window is **inclusive of the current bar**, so a new high pushes the
/// upper band up on the same session it prints. (Some packages exclude the
/// current bar to make breakouts self-evident; this does not.) The first
/// `period - 1` outputs are `None`.
pub fn donchian(bars: &[Bar], period: usize) -> DonchianOutput {
    let n = bars.len();
    if period == 0 || n < period {
        return DonchianOutput {
            upper: undefined(n),
            middle: undefined(n),
            lower: undefined(n),
        };
    }

    let mut upper = undefined(n);
    let mut middle = undefined(n);
    let mut lower = undefined(n);

    for i in (period - 1)..n {
        let window = &bars[i + 1 - period..=i];
        let hi = window.iter().fold(f64::NEG_INFINITY, |m, b| m.max(b.high));
        let lo = window.iter().fold(f64::INFINITY, |m, b| m.min(b.low));
        upper[i] = finite(hi);
        lower[i] = finite(lo);
        middle[i] = finite((hi + lo) / 2.0);
    }

    DonchianOutput {
        upper,
        middle,
        lower,
    }
}

/// The five Ichimoku lines, **all aligned to the input bar that produced them**.
///
/// See [`ichimoku`] for the plotting shifts the chart layer must apply; they
/// are deliberately *not* baked in here.
#[derive(Debug, Clone, PartialEq)]
pub struct IchimokuOutput {
    /// Tenkan-sen (conversion line): midpoint of the last `conversion` bars.
    /// Plotted at the same index it is computed at — no shift.
    pub conversion: Vec<Option<f64>>,
    /// Kijun-sen (base line): midpoint of the last `base` bars. No shift.
    pub base: Vec<Option<f64>>,
    /// Senkou Span A: `(conversion + base) / 2`, computed from bar `i`.
    /// **Conventionally plotted `base` bars to the right**, i.e. the value at
    /// index `i` belongs on the x-position of bar `i + base`.
    pub span_a: Vec<Option<f64>>,
    /// Senkou Span B: midpoint of the last `span_b` bars, computed from bar
    /// `i`. **Also plotted `base` bars to the right**, same as span A. The area
    /// between the two shifted spans is the cloud (kumo).
    pub span_b: Vec<Option<f64>>,
    /// Chikou Span: simply `close[i]`. **Conventionally plotted `base` bars to
    /// the left**, i.e. the value at index `i` belongs on the x-position of bar
    /// `i - base`.
    pub lagging: Vec<Option<f64>>,
}

/// Ichimoku Kinko Hyo (Hosoda) — a whole trend system in five lines.
///
/// Standard parameters are 9 / 26 / 52. The conversion and base lines are
/// midpoints of their windows (Ichimoku uses `(high + low) / 2`, not closes);
/// their crossover is the fast signal. The two Senkou spans, projected forward,
/// bound the *cloud*: price above the cloud is an uptrend, below it a
/// downtrend, inside it a market with no opinion, and cloud thickness measures
/// how much work a reversal would take.
///
/// **Alignment contract.** Every returned vector is input-aligned in the usual
/// sense — index `i` is the value *derived from* `bars[i]`, with `None` during
/// warm-up. The two time shifts Ichimoku is famous for are **not** applied:
///
/// * `span_a` / `span_b` must be drawn shifted **forward** by `base` columns.
/// * `lagging` must be drawn shifted **backward** by `base` columns.
///
/// They are left unshifted because a forward shift has nowhere to put the last
/// `base` values (they project past the final bar) and applying it here would
/// silently discard them. The chart layer owns that decision.
pub fn ichimoku(bars: &[Bar], conversion: usize, base: usize, span_b: usize) -> IchimokuOutput {
    let n = bars.len();
    if conversion == 0 || base == 0 || span_b == 0 || n == 0 {
        return IchimokuOutput {
            conversion: undefined(n),
            base: undefined(n),
            span_a: undefined(n),
            span_b: undefined(n),
            lagging: undefined(n),
        };
    }

    let conversion_line = midpoints(bars, conversion);
    let base_line = midpoints(bars, base);
    let span_b_line = midpoints(bars, span_b);

    let mut span_a = undefined(n);
    for i in 0..n {
        if let (Some(c), Some(b)) = (conversion_line[i], base_line[i]) {
            span_a[i] = finite((c + b) / 2.0);
        }
    }

    let lagging: Vec<Option<f64>> = bars.iter().map(|b| finite(b.close)).collect();

    IchimokuOutput {
        conversion: conversion_line,
        base: base_line,
        span_a,
        span_b: span_b_line,
        lagging,
    }
}

/// Rolling `(highest high + lowest low) / 2` — the Ichimoku building block.
fn midpoints(bars: &[Bar], period: usize) -> Vec<Option<f64>> {
    let n = bars.len();
    if period == 0 || n < period {
        return undefined(n);
    }
    let mut out = undefined(n);
    for i in (period - 1)..n {
        let window = &bars[i + 1 - period..=i];
        let hi = window.iter().fold(f64::NEG_INFINITY, |m, b| m.max(b.high));
        let lo = window.iter().fold(f64::INFINITY, |m, b| m.min(b.low));
        out[i] = finite((hi + lo) / 2.0);
    }
    out
}

// ---------------------------------------------------------------------------
// Price structure
// ---------------------------------------------------------------------------

/// A horizontal price level the market has repeatedly reacted to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    /// The clustered price — the mean of the pivots that formed the level.
    pub price: f64,
    /// How many swing pivots landed in the cluster. More touches, more weight.
    pub touches: usize,
    /// Whether the level sits *below* the latest close (support) or above it
    /// (resistance). A level is not intrinsically one or the other — the same
    /// price flips role once price trades through it, which is exactly why it
    /// is derived from where the market is now rather than from how the pivots
    /// were formed.
    pub is_support: bool,
}

/// Half-width of the fractal pivot test: a swing high must be the highest bar
/// within two sessions either side.
const PIVOT_WING: usize = 2;

/// Support and resistance levels from clustered swing pivots.
///
/// Two steps. First, *fractal pivots*: a bar is a swing high if no bar within
/// [`PIVOT_WING`] sessions either side traded higher, and a swing low if none
/// traded lower. Second, *clustering*: pivots within `sensitivity` (a fraction,
/// so `0.01` means 1%) of a running cluster mean are merged into one level, and
/// levels are ranked by how many pivots they absorbed. A price the market has
/// turned at five times matters more than one it grazed once.
///
/// `lookback` bounds the history considered — `0`, or anything at least as long
/// as the series, means all of it. At most six levels are returned, strongest
/// first; a series with fewer than `2 * PIVOT_WING + 1` bars yields none.
/// `sensitivity` is clamped to `0.0..=0.5`, and a non-finite one is treated as
/// `0.0` (cluster only exactly equal pivots) rather than poisoning the output.
pub fn support_resistance(bars: &[Bar], lookback: usize, sensitivity: f64) -> Vec<Level> {
    const MAX_LEVELS: usize = 6;
    let span = 2 * PIVOT_WING + 1;
    if bars.len() < span {
        return Vec::new();
    }

    let window = if lookback == 0 || lookback >= bars.len() {
        bars
    } else {
        &bars[bars.len() - lookback..]
    };
    if window.len() < span {
        return Vec::new();
    }

    let tol = if sensitivity.is_finite() {
        sensitivity.clamp(0.0, 0.5)
    } else {
        0.0
    };

    // --- pivots ---------------------------------------------------------
    let mut pivots: Vec<f64> = Vec::new();
    for i in PIVOT_WING..window.len() - PIVOT_WING {
        let neighbours = || i - PIVOT_WING..=i + PIVOT_WING;
        let h = window[i].high;
        if h.is_finite() && h > 0.0 && neighbours().all(|j| j == i || window[j].high <= h) {
            pivots.push(h);
        }
        let l = window[i].low;
        if l.is_finite() && l > 0.0 && neighbours().all(|j| j == i || window[j].low >= l) {
            pivots.push(l);
        }
    }
    if pivots.is_empty() {
        return Vec::new();
    }

    // --- clustering -----------------------------------------------------
    pivots.sort_by(f64::total_cmp);

    let mut levels: Vec<Level> = Vec::new();
    let mut sum = pivots[0];
    let mut count = 1usize;
    let last_close = bars.last().map(|b| b.close).unwrap_or(f64::NAN);

    let flush = |sum: f64, count: usize, levels: &mut Vec<Level>| {
        let price = sum / count as f64;
        if price.is_finite() {
            levels.push(Level {
                price,
                touches: count,
                is_support: last_close.is_finite() && price <= last_close,
            });
        }
    };

    for &p in &pivots[1..] {
        let mean = sum / count as f64;
        // Tolerance is proportional so a 500-rupee scrip and a 5-rupee one
        // cluster on the same *relative* nearness.
        if (p - mean).abs() <= mean.abs() * tol {
            sum += p;
            count += 1;
        } else {
            flush(sum, count, &mut levels);
            sum = p;
            count = 1;
        }
    }
    flush(sum, count, &mut levels);

    // Strongest first; ties broken by price so the order is deterministic.
    levels.sort_by(|a, b| {
        b.touches
            .cmp(&a.touches)
            .then_with(|| a.price.total_cmp(&b.price))
    });
    levels.truncate(MAX_LEVELS);
    levels
}

/// Where the last close sits in the 52-week range, as `0.0..=1.0`.
///
/// `0.0` is the year's low, `1.0` the year's high. A scrip printing new highs
/// pins at 1.0; one grinding to new lows pins at 0.0. The window is bounded by
/// the *calendar* — 364 days back from the last bar — rather than by a session
/// count, because PSX's holiday calendar makes the number of sessions in a year
/// vary. If the timestamps do not span a usable window the whole series is used.
///
/// Returns `None` for an empty series. A year with no range at all (a suspended
/// scrip that never moved) is reported as the neutral `0.5` rather than
/// dividing by zero.
pub fn week52_position(bars: &[Bar]) -> Option<f64> {
    /// 52 weeks, in seconds.
    const YEAR_SECS: i64 = 364 * 86_400;

    let last = bars.last()?;
    if !last.close.is_finite() {
        return None;
    }

    let cutoff = last.ts.saturating_sub(YEAR_SECS);
    // `partition_point` is only meaningful on a sorted series; clamping keeps a
    // corrupt timestamp from producing an empty window rather than a panic.
    let from = bars.partition_point(|b| b.ts < cutoff).min(bars.len() - 1);
    let window = &bars[from..];

    let mut hi = f64::NEG_INFINITY;
    let mut lo = f64::INFINITY;
    for b in window {
        if b.high.is_finite() {
            hi = hi.max(b.high);
        }
        if b.low.is_finite() {
            lo = lo.min(b.low);
        }
    }
    if !hi.is_finite() || !lo.is_finite() {
        return None;
    }

    let range = hi - lo;
    if range <= 0.0 {
        return Some(0.5);
    }
    finite(((last.close - lo) / range).clamp(0.0, 1.0))
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

    // -- adx ---------------------------------------------------------------

    /// A clean staircase up: every bar's high and low are one above the last.
    fn uptrend(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                let f = i as f64;
                bar(i as i64, 10.0 + f, 11.0 + f, 9.0 + f, 10.5 + f, 100.0)
            })
            .collect()
    }

    #[test]
    fn adx_warms_up_in_two_windows() {
        let bars = uptrend(60);
        let out = adx(&bars, 14);
        assert_eq!(out.adx.len(), 60);
        assert_eq!(out.plus_di.len(), 60);
        assert_eq!(out.minus_di.len(), 60);

        // DI needs `period` changes, so it starts at index `period`.
        assert!(out.plus_di[..14].iter().all(|v| v.is_none()));
        assert!(out.plus_di[14].is_some());
        // ADX averages the first `period` DX values: index 2*14 - 1 = 27.
        assert!(out.adx[..27].iter().all(|v| v.is_none()));
        assert!(out.adx[27].is_some());

        assert_all_finite(&out.adx);
        assert_all_finite(&out.plus_di);
        assert_all_finite(&out.minus_di);
    }

    #[test]
    fn adx_reads_a_pure_uptrend_as_fully_directional() {
        let out = adx(&uptrend(60), 14);
        // Every bar shifts up by 1 against a true range of 2, so +DM/TR is a
        // steady 0.5 -> +DI 50. There is no downward movement at all, so -DI
        // is pinned at zero and DX — hence ADX — saturates at 100.
        approx(out.minus_di[30].unwrap(), 0.0);
        approx(out.plus_di[30].unwrap(), 50.0);
        approx(out.adx[40].unwrap(), 100.0);
    }

    #[test]
    fn adx_reads_a_pure_downtrend_the_other_way() {
        let bars: Vec<Bar> = (0..60)
            .map(|i| {
                let f = -(i as f64);
                bar(i as i64, 100.0 + f, 101.0 + f, 99.0 + f, 99.5 + f, 100.0)
            })
            .collect();
        let out = adx(&bars, 14);
        approx(out.plus_di[30].unwrap(), 0.0);
        approx(out.minus_di[30].unwrap(), 50.0);
        approx(out.adx[40].unwrap(), 100.0);
    }

    #[test]
    fn adx_is_zero_and_finite_on_flat_bars() {
        // No range at all: no true range to normalise by, no directional
        // movement to report. Everything must collapse to zero, not NaN.
        let bars: Vec<Bar> = (0..60).map(|i| flat_bar(i, 42.0, 0.0)).collect();
        let out = adx(&bars, 14);
        approx(out.plus_di[20].unwrap(), 0.0);
        approx(out.minus_di[20].unwrap(), 0.0);
        approx(out.adx[40].unwrap(), 0.0);
        assert_all_finite(&out.adx);
    }

    #[test]
    fn adx_guards_degenerate_input() {
        let out = adx(&[], 14);
        assert!(out.adx.is_empty() && out.plus_di.is_empty());

        let bars = uptrend(10);
        let out = adx(&bars, 14);
        assert_eq!(out.adx, vec![None; 10]);
        assert_eq!(out.plus_di, vec![None; 10]);

        let out = adx(&bars, 0);
        assert_eq!(out.adx, vec![None; 10]);

        // Enough bars for DI but not yet for ADX, whose seed needs a second
        // window: the vector must stay full length with the tail undefined.
        let out = adx(&uptrend(6), 4);
        assert_eq!(out.adx.len(), 6);
        assert!(out.plus_di[4].is_some());
        assert!(out.adx.iter().all(|v| v.is_none()));

        let out = adx(&uptrend(10), 4);
        assert!(out.adx[6].is_none());
        assert!(out.adx[7].is_some(), "2*4 - 1 = 7 is the first ADX index");
    }

    // -- cci ---------------------------------------------------------------

    #[test]
    fn cci_matches_a_hand_computed_value() {
        // Typical prices 1..=5 over a 5-bar window: mean 3, MAD
        // (2+1+0+1+2)/5 = 1.2, last typical 5 -> 2 / (0.015 * 1.2).
        let bars: Vec<Bar> = (1..=5)
            .map(|i| {
                let p = i as f64;
                bar(i, p, p, p, p, 1.0)
            })
            .collect();
        let out = cci(&bars, 5);
        assert_eq!(out.len(), 5);
        assert!(out[..4].iter().all(|v| v.is_none()));
        approx(out[4].unwrap(), 2.0 / (0.015 * 1.2));
    }

    #[test]
    fn cci_is_zero_when_the_window_has_no_deviation() {
        let bars: Vec<Bar> = (0..30).map(|i| flat_bar(i, 7.0, 1.0)).collect();
        let out = cci(&bars, 20);
        approx(out[29].unwrap(), 0.0);
        assert_all_finite(&out);
    }

    #[test]
    fn cci_guards_degenerate_input() {
        assert!(cci(&[], 20).is_empty());
        assert_eq!(cci(&uptrend(3), 20), vec![None; 3]);
        assert_eq!(cci(&uptrend(3), 0), vec![None; 3]);
        assert_eq!(cci(&uptrend(1), 1).len(), 1);
    }

    // -- williams %r -------------------------------------------------------

    #[test]
    fn williams_r_pins_at_the_extremes_of_the_range() {
        let bars = vec![
            bar(0, 10.0, 12.0, 8.0, 9.0, 1.0),
            bar(1, 9.0, 14.0, 9.0, 13.0, 1.0),
            bar(2, 13.0, 16.0, 12.0, 16.0, 1.0),
        ];
        let out = williams_r(&bars, 3);
        assert_eq!((out[0], out[1]), (None, None));
        // Range 8..16, close at the high -> 0.
        approx(out[2].unwrap(), 0.0);

        // Same window, close at the low -> -100.
        let mut low = bars.clone();
        low[2].close = 8.0;
        approx(williams_r(&low, 3)[2].unwrap(), -100.0);
    }

    #[test]
    fn williams_r_stays_within_its_scale() {
        let bars: Vec<Bar> = (0..40)
            .map(|i| {
                let p = 100.0 + (i as f64 * 0.9).sin() * 8.0;
                bar(i, p, p + 2.0, p - 2.0, p + 1.0, 1.0)
            })
            .collect();
        for v in williams_r(&bars, 14).into_iter().flatten() {
            assert!((-100.0..=0.0).contains(&v), "%R escaped its scale: {v}");
        }
    }

    #[test]
    fn williams_r_is_neutral_on_a_zero_range_window() {
        let bars: Vec<Bar> = (0..20).map(|i| flat_bar(i, 3.0, 0.0)).collect();
        let out = williams_r(&bars, 14);
        approx(out[19].unwrap(), -50.0);
        assert_all_finite(&out);
    }

    #[test]
    fn williams_r_guards_degenerate_input() {
        assert!(williams_r(&[], 14).is_empty());
        assert_eq!(williams_r(&uptrend(3), 14), vec![None; 3]);
        assert_eq!(williams_r(&uptrend(3), 0), vec![None; 3]);
    }

    // -- donchian ----------------------------------------------------------

    #[test]
    fn donchian_tracks_the_rolling_extremes() {
        let bars = vec![
            bar(0, 10.0, 12.0, 8.0, 10.0, 1.0),
            bar(1, 10.0, 15.0, 9.0, 14.0, 1.0),
            bar(2, 14.0, 16.0, 7.0, 11.0, 1.0),
        ];
        let out = donchian(&bars, 3);
        assert_eq!((out.upper[0], out.upper[1]), (None, None));
        approx(out.upper[2].unwrap(), 16.0);
        approx(out.lower[2].unwrap(), 7.0);
        approx(out.middle[2].unwrap(), 11.5);
    }

    #[test]
    fn donchian_collapses_onto_the_price_when_flat() {
        let bars: Vec<Bar> = (0..30).map(|i| flat_bar(i, 5.0, 0.0)).collect();
        let out = donchian(&bars, 20);
        approx(out.upper[29].unwrap(), 5.0);
        approx(out.lower[29].unwrap(), 5.0);
        approx(out.middle[29].unwrap(), 5.0);
        assert_all_finite(&out.middle);
    }

    #[test]
    fn donchian_guards_degenerate_input() {
        let out = donchian(&[], 20);
        assert!(out.upper.is_empty());
        let out = donchian(&uptrend(3), 20);
        assert_eq!(out.upper, vec![None; 3]);
        let out = donchian(&uptrend(3), 0);
        assert_eq!(out.middle, vec![None; 3]);
        // A single bar with period 1 is its own channel.
        let out = donchian(&uptrend(1), 1);
        approx(out.upper[0].unwrap(), 11.0);
        approx(out.lower[0].unwrap(), 9.0);
    }

    // -- ichimoku ----------------------------------------------------------

    #[test]
    fn ichimoku_lines_are_input_aligned_and_unshifted() {
        let bars = uptrend(80);
        let out = ichimoku(&bars, 9, 26, 52);
        for s in [
            &out.conversion,
            &out.base,
            &out.span_a,
            &out.span_b,
            &out.lagging,
        ] {
            assert_eq!(s.len(), 80);
            assert_all_finite(s);
        }

        assert!(out.conversion[..8].iter().all(|v| v.is_none()));
        assert!(out.conversion[8].is_some());
        assert!(out.base[..25].iter().all(|v| v.is_none()));
        assert!(out.base[25].is_some());
        assert!(out.span_b[..51].iter().all(|v| v.is_none()));
        assert!(out.span_b[51].is_some());
        // Span A needs both of its inputs, so it starts with the slower one.
        assert!(out.span_a[24].is_none());
        assert!(out.span_a[25].is_some());

        // The lagging span is the close itself — the *shift* is the chart's job.
        assert_eq!(out.lagging[40], Some(bars[40].close));
    }

    #[test]
    fn ichimoku_midpoints_are_high_low_means_not_close_means() {
        // Bar 0 spans 10..11 with a close of 10.5; bar 1 spans 11..12.
        let bars = uptrend(2);
        let out = ichimoku(&bars, 2, 2, 2);
        // Window high 12, low 9 -> midpoint 10.5. All three lines agree.
        approx(out.conversion[1].unwrap(), 10.5);
        approx(out.base[1].unwrap(), 10.5);
        approx(out.span_a[1].unwrap(), 10.5);
        approx(out.span_b[1].unwrap(), 10.5);
    }

    #[test]
    fn ichimoku_is_flat_and_finite_on_flat_bars() {
        let bars: Vec<Bar> = (0..80).map(|i| flat_bar(i, 9.0, 0.0)).collect();
        let out = ichimoku(&bars, 9, 26, 52);
        approx(out.span_a[79].unwrap(), 9.0);
        approx(out.span_b[79].unwrap(), 9.0);
        assert_all_finite(&out.span_a);
    }

    #[test]
    fn ichimoku_guards_degenerate_input() {
        let out = ichimoku(&[], 9, 26, 52);
        assert!(out.conversion.is_empty() && out.lagging.is_empty());

        let out = ichimoku(&uptrend(5), 9, 26, 52);
        assert_eq!(out.conversion, vec![None; 5]);
        assert_eq!(out.span_b, vec![None; 5]);
        // The lagging span is defined from the first bar regardless.
        assert!(out.lagging[0].is_some());

        let out = ichimoku(&uptrend(5), 0, 26, 52);
        assert_eq!(out.lagging, vec![None; 5]);
    }

    // -- support / resistance ----------------------------------------------

    #[test]
    fn support_resistance_clusters_repeated_swing_pivots() {
        // A saw-tooth between 100 and 110, so every peak is 110 and every
        // trough is 100 — two levels, each touched several times.
        let mut bars = Vec::new();
        for i in 0..40 {
            let up = i % 10 < 5;
            let p = if up {
                100.0 + (i % 10) as f64 * 2.0
            } else {
                110.0 - ((i % 10) - 5) as f64 * 2.0
            };
            bars.push(bar(i as i64, p, p + 0.1, p - 0.1, p, 1.0));
        }
        let levels = support_resistance(&bars, 0, 0.01);
        assert!(!levels.is_empty(), "a saw-tooth must produce levels");
        assert!(levels.len() <= 6);
        for l in &levels {
            assert!(l.price.is_finite() && l.touches > 0);
        }
        // Strongest first.
        for w in levels.windows(2) {
            assert!(w[0].touches >= w[1].touches);
        }
        // Something near each turning point should have been found.
        assert!(levels.iter().any(|l| (l.price - 110.0).abs() < 1.0));
        assert!(levels.iter().any(|l| (l.price - 100.0).abs() < 1.0));
    }

    #[test]
    fn support_is_below_the_last_close_and_resistance_above() {
        let mut bars: Vec<Bar> = (0..40)
            .map(|i| {
                let p = 100.0 + ((i % 8) as f64 - 4.0) * 3.0;
                bar(i, p, p + 1.0, p - 1.0, p, 1.0)
            })
            .collect();
        let last = bars.last().unwrap().close;
        bars.last_mut().unwrap().close = last;

        for l in support_resistance(&bars, 0, 0.02) {
            assert_eq!(l.is_support, l.price <= bars.last().unwrap().close);
        }
    }

    #[test]
    fn support_resistance_respects_the_lookback_window() {
        // Old, wild history followed by a quiet recent stretch.
        let mut bars: Vec<Bar> = (0..30)
            .map(|i| bar(i, 500.0, 520.0, 480.0, 500.0, 1.0))
            .collect();
        bars.extend((30..60).map(|i| {
            let p = 100.0 + ((i % 6) as f64 - 3.0);
            bar(i, p, p + 0.5, p - 0.5, p, 1.0)
        }));

        let recent = support_resistance(&bars, 30, 0.01);
        assert!(
            recent.iter().all(|l| l.price < 200.0),
            "a bounded lookback must not resurrect old levels: {recent:?}"
        );
    }

    #[test]
    fn support_resistance_survives_degenerate_input() {
        assert!(support_resistance(&[], 0, 0.01).is_empty());
        assert!(support_resistance(&uptrend(3), 0, 0.01).is_empty());

        // A perfectly flat series makes every bar a pivot; they must collapse
        // into a single level rather than blowing the cap or dividing by zero.
        let flat: Vec<Bar> = (0..40).map(|i| flat_bar(i, 50.0, 0.0)).collect();
        let levels = support_resistance(&flat, 0, 0.01);
        assert_eq!(levels.len(), 1);
        approx(levels[0].price, 50.0);
        assert!(
            levels[0].is_support,
            "a level at the close counts as support"
        );

        // Pathological sensitivities must not panic or leak NaN.
        for s in [f64::NAN, f64::INFINITY, -1.0, 0.0, 5.0] {
            for l in support_resistance(&uptrend(40), 0, s) {
                assert!(l.price.is_finite(), "sensitivity {s} leaked {l:?}");
            }
        }
    }

    // -- 52-week position --------------------------------------------------

    #[test]
    fn week52_position_locates_the_close_in_the_range() {
        // Prices 0..=99 in a straight line, one session per day.
        let bars: Vec<Bar> = (0..100)
            .map(|i| {
                let p = i as f64;
                bar(i as i64 * 86_400, p, p, p, p, 1.0)
            })
            .collect();
        approx(week52_position(&bars).unwrap(), 1.0);

        let mut mid = bars.clone();
        mid.last_mut().unwrap().close = 49.5;
        // Range 0..99 with a close at 49.5 -> exactly halfway.
        approx(week52_position(&mid).unwrap(), 0.5);
    }

    #[test]
    fn week52_position_ignores_history_older_than_a_year() {
        // A spike two years ago must not stretch this year's range.
        let mut bars = vec![bar(0, 900.0, 1000.0, 900.0, 950.0, 1.0)];
        bars.extend((0..300).map(|i| {
            let ts = 700 * 86_400 + i as i64 * 86_400;
            bar(ts, 10.0, 20.0, 10.0, 20.0, 1.0)
        }));
        // Everything inside the window spans 10..20 and the close is 20.
        approx(week52_position(&bars).unwrap(), 1.0);
    }

    #[test]
    fn week52_position_guards_degenerate_input() {
        assert_eq!(week52_position(&[]), None);

        let flat: Vec<Bar> = (0..300)
            .map(|i| flat_bar(i as i64 * 86_400, 12.0, 0.0))
            .collect();
        approx(week52_position(&flat).unwrap(), 0.5);

        // A single bar is its own 52-week range.
        let one = vec![bar(0, 5.0, 6.0, 4.0, 5.0, 1.0)];
        approx(week52_position(&one).unwrap(), 0.5);
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
        assert_eq!(cci(&bars, 20).len(), bars.len());
        assert_eq!(williams_r(&bars, 14).len(), bars.len());

        let m = macd(&c, 12, 26, 9);
        assert_eq!(m.macd.len(), bars.len());
        assert_eq!(m.signal.len(), bars.len());
        assert_eq!(m.histogram.len(), bars.len());

        let a = adx(&bars, 14);
        assert_eq!(a.adx.len(), bars.len());
        assert_eq!(a.plus_di.len(), bars.len());
        assert_eq!(a.minus_di.len(), bars.len());

        let d = donchian(&bars, 20);
        assert_eq!(d.upper.len(), bars.len());
        assert_eq!(d.middle.len(), bars.len());
        assert_eq!(d.lower.len(), bars.len());

        let ich = ichimoku(&bars, 9, 26, 52);
        for s in [
            &ich.conversion,
            &ich.base,
            &ich.span_a,
            &ich.span_b,
            &ich.lagging,
        ] {
            assert_eq!(s.len(), bars.len());
        }
    }

    #[test]
    fn no_indicator_panics_on_empty_bars() {
        let bars: [Bar; 0] = [];
        assert!(atr(&bars, 14).is_empty());
        assert!(obv(&bars).is_empty());
        assert!(stochastic(&bars, 14, 3).k.is_empty());
        assert!(closes(&bars).is_empty());
        assert!(typicals(&bars).is_empty());
        assert!(adx(&bars, 14).adx.is_empty());
        assert!(cci(&bars, 20).is_empty());
        assert!(williams_r(&bars, 14).is_empty());
        assert!(donchian(&bars, 20).upper.is_empty());
        assert!(ichimoku(&bars, 9, 26, 52).span_a.is_empty());
        assert!(support_resistance(&bars, 0, 0.01).is_empty());
        assert_eq!(week52_position(&bars), None);
    }

    /// A single bar is the hardest degenerate case: every window is longer
    /// than the series and every range is a point.
    #[test]
    fn a_single_bar_never_panics_or_leaks_a_non_number() {
        let one = vec![bar(0, 10.0, 10.0, 10.0, 10.0, 0.0)];
        assert_all_finite(&cci(&one, 14));
        assert_all_finite(&williams_r(&one, 14));
        assert_all_finite(&adx(&one, 14).adx);
        assert_all_finite(&donchian(&one, 14).upper);
        assert_all_finite(&ichimoku(&one, 9, 26, 52).span_a);
        assert!(support_resistance(&one, 0, 0.01).is_empty());
        assert!(week52_position(&one).unwrap().is_finite());

        // And with period 1, where the window *is* the bar.
        assert_all_finite(&cci(&one, 1));
        assert_all_finite(&williams_r(&one, 1));
        assert_all_finite(&donchian(&one, 1).middle);
        assert_all_finite(&ichimoku(&one, 1, 1, 1).span_a);
    }
}
