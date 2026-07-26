//! Return statistics and risk metrics.
//!
//! These are the numbers shown on the performance panel: how much a scrip
//! returned, how violently it got there, and how much of that return survives a
//! risk adjustment.
//!
//! Every function is total. Degenerate inputs — an empty series, a scrip that
//! never moved, an index with two overlapping observations — return `0.0` (or
//! the documented sentinel) instead of `NaN` or an infinity. This matters
//! because on PSX a fair number of listed scrips genuinely do not trade for
//! days at a time.

/// Trading sessions in a Pakistan Stock Exchange year.
///
/// PSX runs Monday–Friday minus roughly a dozen public holidays; ~250 sessions
/// is the conventional annualisation factor and the default used across the UI.
pub const TRADING_DAYS_PER_YEAR: f64 = 250.0;

/// Collapse a non-finite result to `0.0`.
///
/// The last line of defence before a statistic reaches the UI: no `NaN` or
/// infinity is ever allowed to escape this module.
fn finite_or_zero(x: f64) -> f64 {
    if x.is_finite() { x } else { 0.0 }
}

/// Truncate two series to their common length.
///
/// Return series from two symbols routinely differ in length — a newly listed
/// scrip, a suspended one, a gap in the cached history — so the pairwise
/// statistics compare only the overlapping tail-aligned prefix.
fn paired<'a>(a: &'a [f64], b: &'a [f64]) -> (&'a [f64], &'a [f64]) {
    let n = a.len().min(b.len());
    (&a[..n], &b[..n])
}

// ---------------------------------------------------------------------------
// Descriptive helpers
// ---------------------------------------------------------------------------

/// Arithmetic mean. Empty input yields `0.0`.
pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    finite_or_zero(values.iter().sum::<f64>() / values.len() as f64)
}

/// Relative tolerance below which a dispersion is treated as exactly zero.
///
/// Summing a "constant" series in floating point leaves residue on the order of
/// `1e-16` of its magnitude, which would otherwise turn a flat, untraded scrip
/// into an astronomically high Sharpe ratio. Anything this small relative to
/// the data's own scale is numerical noise, never a signal.
const DISPERSION_EPSILON: f64 = 1e-12;

/// Sample standard deviation (Bessel-corrected, `n - 1` denominator).
///
/// The sample form is used throughout because a price history is a sample of
/// the return-generating process, not the whole population. Fewer than two
/// observations, or a flat series, gives exactly `0.0` — see
/// [`DISPERSION_EPSILON`] for why "flat" is judged relative to scale.
pub fn std_dev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let m = mean(values);
    let variance =
        values.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (values.len() - 1) as f64;
    let sd = finite_or_zero(variance.max(0.0).sqrt());

    let scale = values.iter().fold(
        m.abs(),
        |acc, v| if v.is_finite() { acc.max(v.abs()) } else { acc },
    );
    if sd <= scale * DISPERSION_EPSILON {
        return 0.0;
    }
    sd
}

/// Sample covariance of two series, truncated to their common length.
///
/// The raw ingredient of beta and correlation: positive when the two move
/// together, negative when they hedge each other. Returns `0.0` when fewer than
/// two paired observations exist.
pub fn covariance(a: &[f64], b: &[f64]) -> f64 {
    let (a, b) = paired(a, b);
    if a.len() < 2 {
        return 0.0;
    }
    let (ma, mb) = (mean(a), mean(b));
    let sum: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (x - ma) * (y - mb))
        .sum();
    finite_or_zero(sum / (a.len() - 1) as f64)
}

// ---------------------------------------------------------------------------
// Return series
// ---------------------------------------------------------------------------

/// Period-over-period simple (arithmetic) returns, length `n - 1`.
///
/// `r_t = P_t / P_{t-1} - 1` — the return an investor actually realises over
/// one period. These aggregate correctly across a portfolio at a point in time,
/// which is why they are used for beta, correlation and the Sharpe ratio.
///
/// A non-positive or non-finite previous price (a bad print, a placeholder
/// zero) yields `0.0` for that period rather than an infinity.
pub fn simple_returns(closes: &[f64]) -> Vec<f64> {
    if closes.len() < 2 {
        return Vec::new();
    }
    closes
        .windows(2)
        .map(|w| {
            if w[0] > 0.0 && w[0].is_finite() && w[1].is_finite() {
                finite_or_zero(w[1] / w[0] - 1.0)
            } else {
                0.0
            }
        })
        .collect()
}

/// Period-over-period log (continuously compounded) returns, length `n - 1`.
///
/// `r_t = ln(P_t / P_{t-1})`. Log returns add across time, which makes them the
/// natural input for volatility estimates and for anything compounded over many
/// periods.
///
/// Any period with a non-positive price — where the logarithm is undefined —
/// contributes `0.0`, keeping the series aligned with `simple_returns`.
pub fn log_returns(closes: &[f64]) -> Vec<f64> {
    if closes.len() < 2 {
        return Vec::new();
    }
    closes
        .windows(2)
        .map(|w| {
            if w[0] > 0.0 && w[1] > 0.0 && w[0].is_finite() && w[1].is_finite() {
                finite_or_zero((w[1] / w[0]).ln())
            } else {
                0.0
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Performance
// ---------------------------------------------------------------------------

/// Geometric (compound) annualised return — the CAGR of the return series.
///
/// Chains the periodic returns into a total growth factor and re-expresses it
/// per year: `(prod(1 + r))^(periods_per_year / n) - 1`. Unlike a naive average
/// this accounts for compounding, so a +50% followed by a -50% correctly
/// reports a loss.
///
/// Returns `0.0` for an empty series and `-1.0` (total loss) if the compounded
/// equity curve reaches zero or below.
pub fn annualized_return(returns: &[f64], periods_per_year: f64) -> f64 {
    if returns.is_empty() || periods_per_year <= 0.0 || !periods_per_year.is_finite() {
        return 0.0;
    }

    let mut growth = 1.0f64;
    for r in returns {
        if !r.is_finite() {
            continue;
        }
        growth *= 1.0 + r;
        if growth <= 0.0 {
            return -1.0;
        }
    }

    let exponent = periods_per_year / returns.len() as f64;
    finite_or_zero(growth.powf(exponent) - 1.0)
}

/// Annualised volatility — the sample standard deviation of returns scaled by
/// `sqrt(periods_per_year)`.
///
/// The standard risk measure: roughly, the one-standard-deviation range a
/// year's return is expected to fall in. The square-root-of-time scaling
/// assumes returns are serially independent, which is the usual working
/// approximation.
///
/// A flat series, or fewer than two observations, gives `0.0`.
pub fn annualized_volatility(returns: &[f64], periods_per_year: f64) -> f64 {
    if returns.len() < 2 || periods_per_year <= 0.0 || !periods_per_year.is_finite() {
        return 0.0;
    }
    finite_or_zero(std_dev(returns) * periods_per_year.sqrt())
}

/// Convert an annual rate (e.g. a T-bill yield) to its per-period equivalent.
///
/// Uses geometric de-annualisation, `(1 + annual)^(1/ppy) - 1`, so that
/// compounding the result over a year reproduces the input exactly.
fn per_period_rate(annual: f64, periods_per_year: f64) -> f64 {
    if !annual.is_finite() || periods_per_year <= 0.0 || !periods_per_year.is_finite() {
        return 0.0;
    }
    if 1.0 + annual <= 0.0 {
        return 0.0;
    }
    finite_or_zero((1.0 + annual).powf(1.0 / periods_per_year) - 1.0)
}

/// Sharpe ratio — annualised excess return per unit of total volatility.
///
/// `(annualised mean excess return) / (annualised volatility)`, where excess is
/// measured against `risk_free_annual` (for PSX, typically the prevailing
/// 6-month T-bill yield, which has often been in double digits — using zero
/// here would flatter every scrip badly).
///
/// Above 1 is good, above 2 is unusual. Returns `0.0` when volatility is zero,
/// since "infinite Sharpe" is an artefact of a non-trading scrip rather than a
/// real result.
pub fn sharpe_ratio(returns: &[f64], risk_free_annual: f64, periods_per_year: f64) -> f64 {
    if returns.len() < 2 || periods_per_year <= 0.0 || !periods_per_year.is_finite() {
        return 0.0;
    }

    let rf = per_period_rate(risk_free_annual, periods_per_year);
    let excess: Vec<f64> = returns.iter().map(|r| r - rf).collect();

    let sd = std_dev(&excess);
    if sd <= 0.0 {
        return 0.0;
    }
    finite_or_zero((mean(&excess) * periods_per_year) / (sd * periods_per_year.sqrt()))
}

/// Sortino ratio — excess return per unit of *downside* volatility.
///
/// Identical to Sharpe except that the denominator counts only returns below
/// the risk-free target, on the argument that investors do not need to be
/// compensated for upside surprises. It is the fairer measure for strategies
/// with deliberately asymmetric payoffs.
///
/// Downside deviation is computed over the full sample (shortfalls squared,
/// zeros for periods that beat the target). Returns `0.0` when there is no
/// downside at all — a genuinely undefined ratio, not an infinite one.
pub fn sortino_ratio(returns: &[f64], risk_free_annual: f64, periods_per_year: f64) -> f64 {
    if returns.len() < 2 || periods_per_year <= 0.0 || !periods_per_year.is_finite() {
        return 0.0;
    }

    let rf = per_period_rate(risk_free_annual, periods_per_year);
    let excess: Vec<f64> = returns.iter().map(|r| r - rf).collect();

    let downside_sq: f64 = excess
        .iter()
        .map(|e| if *e < 0.0 { e * e } else { 0.0 })
        .sum::<f64>()
        / excess.len() as f64;
    let downside = downside_sq.max(0.0).sqrt();
    if downside <= 0.0 {
        return 0.0;
    }
    finite_or_zero((mean(&excess) * periods_per_year) / (downside * periods_per_year.sqrt()))
}

/// The worst peak-to-trough decline in an equity or price curve.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Drawdown {
    /// The decline as a negative fraction, e.g. `-0.23` for a 23% drawdown.
    /// Zero when the series never declined.
    pub pct: f64,
    /// Index of the peak the decline started from.
    pub peak_idx: usize,
    /// Index of the trough. Equals `peak_idx` when there was no drawdown.
    pub trough_idx: usize,
}

/// Maximum drawdown — the deepest peak-to-trough loss over the whole series.
///
/// The most intuitive risk statistic there is: the worst loss an investor would
/// have suffered buying at exactly the wrong moment and selling at exactly the
/// wrong moment. Reported as a negative fraction together with the indices of
/// the peak and trough so the chart can shade the episode.
///
/// A never-declining or empty series yields `pct = 0.0`.
pub fn max_drawdown(closes: &[f64]) -> Drawdown {
    let mut result = Drawdown::default();
    if closes.is_empty() {
        return result;
    }

    let mut peak = f64::NEG_INFINITY;
    let mut peak_idx = 0usize;

    for (i, &price) in closes.iter().enumerate() {
        if !price.is_finite() {
            continue;
        }
        if price > peak {
            peak = price;
            peak_idx = i;
        }
        if peak > 0.0 {
            let dd = price / peak - 1.0;
            if dd.is_finite() && dd < result.pct {
                result = Drawdown {
                    pct: dd,
                    peak_idx,
                    trough_idx: i,
                };
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Relationships between series
// ---------------------------------------------------------------------------

/// Beta — sensitivity of a scrip's returns to the market's.
///
/// `cov(asset, market) / var(market)`, normally measured against the KSE-100.
/// A beta of 1 moves with the index, above 1 amplifies it (cyclicals, leveraged
/// balance sheets), below 1 damps it (utilities, fertilisers). Negative betas
/// are rare and usually a data artefact on illiquid scrips.
///
/// Mismatched lengths are truncated to the shorter. Returns `0.0` when the
/// market series has no variance or there are fewer than two paired points.
pub fn beta(asset_returns: &[f64], market_returns: &[f64]) -> f64 {
    let (asset, market) = paired(asset_returns, market_returns);
    if asset.len() < 2 {
        return 0.0;
    }
    // Guard on the standard deviation rather than the raw variance so the
    // flat-market case is caught by the same scale-aware test everywhere.
    let market_sd = std_dev(market);
    if market_sd <= 0.0 {
        return 0.0;
    }
    finite_or_zero(covariance(asset, market) / (market_sd * market_sd))
}

/// Pearson correlation coefficient, in `[-1, 1]`.
///
/// Beta's scale-free cousin: it measures how tightly two series move together
/// without saying anything about magnitude. Used for the diversification view —
/// a basket of highly correlated scrips (as most PSX banks are) carries far more
/// concentrated risk than its position count suggests.
///
/// Mismatched lengths are truncated to the shorter. Returns `0.0` if either
/// series is constant (undefined correlation) or fewer than two paired points
/// exist. The result is clamped to `[-1, 1]` to absorb floating-point overshoot.
pub fn correlation(a: &[f64], b: &[f64]) -> f64 {
    let (a, b) = paired(a, b);
    if a.len() < 2 {
        return 0.0;
    }
    let (sa, sb) = (std_dev(a), std_dev(b));
    if sa <= 0.0 || sb <= 0.0 {
        return 0.0;
    }
    finite_or_zero(covariance(a, b) / (sa * sb)).clamp(-1.0, 1.0)
}

/// Pairwise correlation matrix over a set of named return series.
///
/// Row `i`, column `j` holds `correlation(series[i], series[j])`. The matrix is
/// symmetric with `1.0` on the diagonal — including for a constant series,
/// where a series is trivially perfectly correlated with itself even though its
/// correlation with anything else is reported as `0.0`.
///
/// Series of differing lengths are handled pairwise by truncation, so a newly
/// listed scrip can be compared against a long history without pre-alignment.
pub fn correlation_matrix(series: &[(String, Vec<f64>)]) -> Vec<Vec<f64>> {
    let n = series.len();
    let mut matrix = vec![vec![0.0; n]; n];

    for i in 0..n {
        matrix[i][i] = 1.0;
        for j in (i + 1)..n {
            let c = correlation(&series[i].1, &series[j].1);
            matrix[i][j] = c;
            matrix[j][i] = c;
        }
    }
    matrix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    fn approx_eps(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "expected {b}, got {a}");
    }

    fn named(name: &str, values: &[f64]) -> (String, Vec<f64>) {
        (name.to_string(), values.to_vec())
    }

    // -- return series -----------------------------------------------------

    #[test]
    fn simple_returns_are_hand_checkable() {
        let closes = [100.0, 110.0, 99.0];
        let r = simple_returns(&closes);
        assert_eq!(r.len(), 2);
        approx(r[0], 0.10);
        approx(r[1], -0.10);
    }

    #[test]
    fn log_returns_are_hand_checkable() {
        let closes = [100.0, 100.0 * std::f64::consts::E];
        let r = log_returns(&closes);
        assert_eq!(r.len(), 1);
        approx(r[0], 1.0);
    }

    #[test]
    fn returns_are_empty_for_short_input() {
        assert!(simple_returns(&[]).is_empty());
        assert!(simple_returns(&[1.0]).is_empty());
        assert!(log_returns(&[]).is_empty());
        assert!(log_returns(&[1.0]).is_empty());
    }

    #[test]
    fn returns_guard_non_positive_prices() {
        let closes = [0.0, 10.0, -5.0, 10.0];
        for r in simple_returns(&closes) {
            assert!(r.is_finite());
        }
        for r in log_returns(&closes) {
            assert!(r.is_finite());
        }
        // A zero previous price cannot produce a return: reported as flat.
        approx(simple_returns(&closes)[0], 0.0);
        approx(log_returns(&closes)[0], 0.0);
    }

    // -- descriptive -------------------------------------------------------

    #[test]
    fn mean_and_std_dev_are_hand_checkable() {
        let values = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        approx(mean(&values), 5.0);
        // Sample sd: sum of squares 32 / 7 -> sqrt(32/7).
        approx(std_dev(&values), (32.0f64 / 7.0).sqrt());
    }

    #[test]
    fn descriptive_helpers_guard_degenerate_input() {
        approx(mean(&[]), 0.0);
        approx(std_dev(&[]), 0.0);
        approx(std_dev(&[1.0]), 0.0);
        approx(std_dev(&[3.0; 10]), 0.0);
        approx(covariance(&[], &[]), 0.0);
        approx(covariance(&[1.0], &[2.0]), 0.0);
    }

    #[test]
    fn covariance_truncates_to_the_shorter_series() {
        let a = [1.0, 2.0, 3.0, 99.0, 99.0];
        let b = [2.0, 4.0, 6.0];
        approx(covariance(&a, &b), 2.0); // cov([1,2,3],[2,4,6]) = 2
    }

    // -- performance -------------------------------------------------------

    #[test]
    fn annualized_return_compounds_geometrically() {
        // +50% then -50% is a 25% loss over two periods.
        let returns = [0.5, -0.5];
        approx(annualized_return(&returns, 2.0), -0.25);
    }

    #[test]
    fn annualized_return_scales_to_a_year() {
        // 1% per period over 250 periods, annualised at 250/yr.
        let returns = vec![0.01; 250];
        approx_eps(
            annualized_return(&returns, TRADING_DAYS_PER_YEAR),
            1.01f64.powi(250) - 1.0,
            1e-9,
        );
    }

    #[test]
    fn annualized_return_reports_total_loss_on_wipeout() {
        approx(annualized_return(&[-1.0, 0.5], TRADING_DAYS_PER_YEAR), -1.0);
        approx(annualized_return(&[-2.0], TRADING_DAYS_PER_YEAR), -1.0);
    }

    #[test]
    fn annualized_return_guards_degenerate_input() {
        approx(annualized_return(&[], 250.0), 0.0);
        approx(annualized_return(&[0.01], 0.0), 0.0);
        approx(annualized_return(&[0.01], f64::NAN), 0.0);
    }

    #[test]
    fn annualized_volatility_scales_by_sqrt_time() {
        let returns = [0.01, -0.01, 0.01, -0.01];
        let expected = std_dev(&returns) * TRADING_DAYS_PER_YEAR.sqrt();
        approx(
            annualized_volatility(&returns, TRADING_DAYS_PER_YEAR),
            expected,
        );
    }

    #[test]
    fn annualized_volatility_is_zero_for_a_flat_scrip() {
        approx(
            annualized_volatility(&[0.0; 30], TRADING_DAYS_PER_YEAR),
            0.0,
        );
        approx(annualized_volatility(&[], TRADING_DAYS_PER_YEAR), 0.0);
        approx(annualized_volatility(&[0.01], TRADING_DAYS_PER_YEAR), 0.0);
    }

    #[test]
    fn sharpe_ratio_is_hand_checkable_with_zero_risk_free() {
        let returns = [0.01, -0.01, 0.01, -0.01, 0.02];
        let expected = (mean(&returns) * TRADING_DAYS_PER_YEAR)
            / (std_dev(&returns) * TRADING_DAYS_PER_YEAR.sqrt());
        approx(sharpe_ratio(&returns, 0.0, TRADING_DAYS_PER_YEAR), expected);
    }

    #[test]
    fn sharpe_ratio_falls_when_the_risk_free_rate_rises() {
        let returns = vec![0.001; 100];
        // Constant returns have zero volatility -> undefined, reported as zero.
        approx(sharpe_ratio(&returns, 0.0, TRADING_DAYS_PER_YEAR), 0.0);

        let noisy: Vec<f64> = (0..100)
            .map(|i| 0.001 + if i % 2 == 0 { 0.002 } else { -0.002 })
            .collect();
        let low_rf = sharpe_ratio(&noisy, 0.0, TRADING_DAYS_PER_YEAR);
        let high_rf = sharpe_ratio(&noisy, 0.15, TRADING_DAYS_PER_YEAR);
        assert!(high_rf < low_rf);
        assert!(low_rf.is_finite() && high_rf.is_finite());
    }

    #[test]
    fn sharpe_ratio_guards_degenerate_input() {
        approx(sharpe_ratio(&[], 0.05, 250.0), 0.0);
        approx(sharpe_ratio(&[0.01], 0.05, 250.0), 0.0);
        approx(sharpe_ratio(&[0.01, 0.02], 0.05, 0.0), 0.0);
        approx(sharpe_ratio(&[0.0; 50], 0.05, 250.0), 0.0);
    }

    #[test]
    fn sortino_penalises_only_downside() {
        // No period is below the (zero) target -> no downside deviation.
        let all_up = [0.01, 0.02, 0.03];
        approx(sortino_ratio(&all_up, 0.0, TRADING_DAYS_PER_YEAR), 0.0);

        let mixed = [0.02, -0.01, 0.03, -0.02];
        let s = sortino_ratio(&mixed, 0.0, TRADING_DAYS_PER_YEAR);
        assert!(s.is_finite());
        // Downside dev uses only the two negative periods but divides by 4.
        let downside = ((0.0001 + 0.0004) / 4.0f64).sqrt();
        let expected =
            (mean(&mixed) * TRADING_DAYS_PER_YEAR) / (downside * TRADING_DAYS_PER_YEAR.sqrt());
        approx(s, expected);
        // Ignoring upside noise makes Sortino the more generous measure here.
        assert!(s > sharpe_ratio(&mixed, 0.0, TRADING_DAYS_PER_YEAR));
    }

    #[test]
    fn sortino_ratio_guards_degenerate_input() {
        approx(sortino_ratio(&[], 0.05, 250.0), 0.0);
        approx(sortino_ratio(&[0.01], 0.05, 250.0), 0.0);
        approx(sortino_ratio(&[0.01, 0.02], 0.05, -1.0), 0.0);
        approx(sortino_ratio(&[0.0; 50], 0.0, 250.0), 0.0);
    }

    // -- drawdown ----------------------------------------------------------

    #[test]
    fn max_drawdown_finds_the_deepest_episode() {
        //             0     1      2     3      4      5
        let closes = [100.0, 120.0, 90.0, 130.0, 100.0, 140.0];
        let dd = max_drawdown(&closes);
        approx(dd.pct, 90.0 / 120.0 - 1.0); // -25%
        assert_eq!(dd.peak_idx, 1);
        assert_eq!(dd.trough_idx, 2);
    }

    #[test]
    fn max_drawdown_is_zero_for_a_monotonic_rise() {
        let closes = [1.0, 2.0, 3.0, 4.0];
        let dd = max_drawdown(&closes);
        approx(dd.pct, 0.0);
        assert_eq!(dd.peak_idx, 0);
        assert_eq!(dd.trough_idx, 0);
    }

    #[test]
    fn max_drawdown_guards_degenerate_input() {
        let dd = max_drawdown(&[]);
        approx(dd.pct, 0.0);
        assert_eq!((dd.peak_idx, dd.trough_idx), (0, 0));

        let dd = max_drawdown(&[5.0; 20]);
        approx(dd.pct, 0.0);

        let dd = max_drawdown(&[0.0, 0.0, 0.0]);
        assert!(dd.pct.is_finite());
        approx(dd.pct, 0.0);
    }

    // -- relationships -----------------------------------------------------

    #[test]
    fn correlation_is_one_for_a_perfect_linear_relationship() {
        let a = [1.0, 2.0, 3.0, 4.0];
        let b = [10.0, 20.0, 30.0, 40.0];
        approx(correlation(&a, &b), 1.0);

        let c = [40.0, 30.0, 20.0, 10.0];
        approx(correlation(&a, &c), -1.0);
    }

    #[test]
    fn correlation_and_beta_are_zero_on_zero_variance() {
        let flat = [3.0; 10];
        let moving: Vec<f64> = (0..10).map(|i| i as f64).collect();

        approx(correlation(&flat, &moving), 0.0);
        approx(correlation(&moving, &flat), 0.0);
        approx(correlation(&flat, &flat), 0.0);

        // A market that never moves has no variance to regress against.
        approx(beta(&moving, &flat), 0.0);
        approx(beta(&flat, &flat), 0.0);
    }

    #[test]
    fn correlation_and_beta_truncate_mismatched_lengths() {
        let a = [1.0, 2.0, 3.0];
        let b = [2.0, 4.0, 6.0, 100.0, -100.0];
        approx(correlation(&a, &b), 1.0);
        approx(beta(&a, &b), 0.5); // asset moves half as much as the "market"
    }

    #[test]
    fn beta_is_hand_checkable() {
        let market = [0.01, -0.01, 0.02, -0.02];
        let asset: Vec<f64> = market.iter().map(|r| r * 2.0).collect();
        approx(beta(&asset, &market), 2.0);
    }

    #[test]
    fn correlation_and_beta_guard_short_input() {
        approx(correlation(&[], &[]), 0.0);
        approx(correlation(&[1.0], &[2.0]), 0.0);
        approx(beta(&[], &[]), 0.0);
        approx(beta(&[1.0], &[2.0]), 0.0);
    }

    #[test]
    fn correlation_matrix_is_symmetric_with_unit_diagonal() {
        let series = vec![
            named("OGDC", &[0.01, -0.02, 0.03, 0.00]),
            named("PPL", &[0.02, -0.01, 0.04, 0.01]),
            named("HBL", &[0.0; 4]), // a scrip that did not trade
        ];
        let m = correlation_matrix(&series);

        assert_eq!(m.len(), 3);
        for row in &m {
            assert_eq!(row.len(), 3);
        }
        for (i, row) in m.iter().enumerate() {
            approx(row[i], 1.0);
            for (j, value) in row.iter().enumerate() {
                approx(*value, m[j][i]);
                assert!(value.is_finite());
                assert!((-1.0..=1.0).contains(value));
            }
        }
        // The flat scrip correlates with nothing.
        approx(m[2][0], 0.0);
        approx(m[2][1], 0.0);
    }

    #[test]
    fn correlation_matrix_handles_empty_and_single_inputs() {
        assert!(correlation_matrix(&[]).is_empty());
        let m = correlation_matrix(&[named("SYS", &[0.01, 0.02])]);
        assert_eq!(m, vec![vec![1.0]]);
    }

    // -- shared invariant --------------------------------------------------

    #[test]
    fn no_statistic_leaks_nan_on_pathological_input() {
        let cases: Vec<Vec<f64>> = vec![
            vec![],
            vec![0.0],
            vec![0.0; 10],
            vec![-1.0, -1.0],
            vec![1e308, -1e308],
        ];
        for case in &cases {
            let stats = [
                annualized_return(case, TRADING_DAYS_PER_YEAR),
                annualized_volatility(case, TRADING_DAYS_PER_YEAR),
                sharpe_ratio(case, 0.12, TRADING_DAYS_PER_YEAR),
                sortino_ratio(case, 0.12, TRADING_DAYS_PER_YEAR),
                mean(case),
                std_dev(case),
                beta(case, case),
                correlation(case, case),
            ];
            for (i, s) in stats.iter().enumerate() {
                assert!(s.is_finite(), "statistic {i} on {case:?} was {s}");
            }
            assert!(max_drawdown(case).pct.is_finite());
        }
    }
}
