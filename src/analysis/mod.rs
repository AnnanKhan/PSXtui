//! Analysis engine: technical indicators and return statistics.
//!
//! Everything in here is pure, synchronous and allocation-light so it can be
//! recomputed on every redraw of the TUI without a background task.
//!
//! Two conventions hold throughout and the UI depends on both:
//!
//! 1. **Alignment.** Every indicator returns a `Vec<Option<f64>>` whose length
//!    equals the length of its input. Index `i` of the output describes
//!    `bars[i]`; `None` marks the warm-up window where the indicator is not yet
//!    defined. The chart layer can therefore zip an overlay straight onto the
//!    price series without any offset bookkeeping.
//! 2. **No poisoned floats.** Pakistan Stock Exchange data is full of thin
//!    scrips, limit-locked sessions and zero-volume days, so degenerate inputs
//!    (flat prices, zero variance, empty slices) are the norm rather than the
//!    exception. No public function here ever panics, and none ever returns
//!    `NaN` or an infinity — degenerate cases collapse to a documented
//!    sentinel instead.

// The binary's other layers (ui/, psx/, cache/) are authored separately and do
// not yet consume the whole surface of this engine.
#![allow(dead_code)]

pub mod indicators;
pub mod stats;

pub use indicators::{
    AdxOutput, BollingerOutput, DonchianOutput, IchimokuOutput, Level, MacdOutput,
    StochasticOutput, adx, atr, bollinger, cci, donchian, ema, ichimoku, macd, obv, rsi, sma,
    stochastic, support_resistance, vwap_session, week52_position, williams_r,
};
pub use stats::{
    Drawdown, TRADING_DAYS_PER_YEAR, annualized_return, annualized_volatility, beta, correlation,
    correlation_matrix, covariance, log_returns, max_drawdown, mean, sharpe_ratio, simple_returns,
    sortino_ratio, std_dev,
};
