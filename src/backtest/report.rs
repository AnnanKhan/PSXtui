//! What a run produced: the equity curve, the trades, and the summary
//! statistics computed from them.
//!
//! The risk statistics come from [`crate::analysis::stats`] rather than being
//! reimplemented, so a Sharpe ratio means the same thing on this screen as it
//! does on the Analysis screen.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::engine::Config;
use super::strategy::Strategy;
use crate::analysis::stats;
use crate::model::Bar;

/// Why a position was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradeExit {
    /// The strategy's exit rule fired.
    Signal,
    StopLoss,
    TakeProfit,
    /// Still open when the data ran out; closed at the last close so the
    /// numbers reconcile.
    EndOfData,
}

impl TradeExit {
    pub fn label(&self) -> &'static str {
        match self {
            TradeExit::Signal => "signal",
            TradeExit::StopLoss => "stop",
            TradeExit::TakeProfit => "target",
            TradeExit::EndOfData => "open",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub entry_ts: i64,
    pub exit_ts: i64,
    pub entry_price: f64,
    pub exit_price: f64,
    /// Signed: negative is a short.
    pub qty: f64,
    pub pnl: f64,
    /// Direction-adjusted, so a profitable short is positive.
    pub return_pct: f64,
    pub bars_held: usize,
    pub exit_reason: TradeExit,
}

impl Trade {
    pub fn is_win(&self) -> bool {
        self.pnl > 0.0
    }
}

/// The outcome of one backtest run.
#[derive(Debug, Clone)]
pub struct Report {
    pub strategy_name: String,
    pub params: HashMap<String, f64>,

    /// Equity after each bar, one point per bar.
    pub equity: Vec<f64>,
    /// Timestamps matching `equity`, for the x axis.
    pub timestamps: Vec<i64>,
    pub trades: Vec<Trade>,

    pub initial_equity: f64,
    pub final_equity: f64,
    pub total_return_pct: f64,
    /// Buy-and-hold over the same window, which is the honest benchmark: a
    /// strategy that trails it has cost the user money to run.
    pub buy_hold_return_pct: f64,
    pub cagr_pct: f64,
    pub sharpe: f64,
    pub sortino: f64,
    /// Negative, matching [`crate::analysis::stats::Drawdown`] and the
    /// Analysis screen: `-23.4` is a 23.4% peak-to-trough loss.
    pub max_drawdown_pct: f64,
    pub volatility_pct: f64,

    pub trade_count: usize,
    pub win_rate_pct: f64,
    pub profit_factor: f64,
    pub avg_win_pct: f64,
    pub avg_loss_pct: f64,
    pub best_trade_pct: f64,
    pub worst_trade_pct: f64,
    pub avg_bars_held: f64,
    /// Share of bars spent holding a position.
    pub exposure_pct: f64,

    /// Set when the strategy reads high/low, which are synthetic on most of
    /// the history. The UI must show this next to the numbers.
    pub intrabar_warning: bool,
    /// Bars the run covered — small samples make every other number noise.
    pub bar_count: usize,
}

impl Report {
    pub(super) fn build(
        strategy: &Strategy,
        bars: &[Bar],
        equity: Vec<f64>,
        trades: Vec<Trade>,
        params: HashMap<String, f64>,
        config: &Config,
    ) -> Self {
        let initial = config.initial_equity;
        let final_equity = equity.last().copied().unwrap_or(initial);
        let total_return_pct = pct_change(initial, final_equity);

        let returns = stats::simple_returns(&equity);
        let sharpe = stats::sharpe_ratio(&returns, 0.0, stats::TRADING_DAYS_PER_YEAR);
        let sortino = stats::sortino_ratio(&returns, 0.0, stats::TRADING_DAYS_PER_YEAR);
        let volatility_pct =
            stats::annualized_volatility(&returns, stats::TRADING_DAYS_PER_YEAR) * 100.0;
        let dd = stats::max_drawdown(&equity);

        // Years from the actual span rather than the bar count, so a gap in
        // the data does not inflate the annualised figure.
        let years = match (bars.first(), bars.last()) {
            (Some(a), Some(b)) if b.ts > a.ts => (b.ts - a.ts) as f64 / 31_557_600.0,
            _ => 0.0,
        };
        let cagr_pct = if years > 0.0 && initial > 0.0 && final_equity > 0.0 {
            ((final_equity / initial).powf(1.0 / years) - 1.0) * 100.0
        } else {
            0.0
        };

        let buy_hold_return_pct = match (bars.first(), bars.last()) {
            (Some(a), Some(b)) => pct_change(a.close, b.close),
            _ => 0.0,
        };

        let wins: Vec<&Trade> = trades.iter().filter(|t| t.is_win()).collect();
        let losses: Vec<&Trade> = trades.iter().filter(|t| !t.is_win()).collect();

        let gross_profit: f64 = wins.iter().map(|t| t.pnl).sum();
        let gross_loss: f64 = losses.iter().map(|t| t.pnl.abs()).sum();

        let held_bars: usize = trades.iter().map(|t| t.bars_held).sum();

        Self {
            strategy_name: strategy.name.clone(),
            params,
            timestamps: bars.iter().map(|b| b.ts).collect(),
            equity,
            initial_equity: initial,
            final_equity,
            total_return_pct,
            buy_hold_return_pct,
            cagr_pct,
            sharpe,
            sortino,
            max_drawdown_pct: dd.pct * 100.0,
            volatility_pct,
            trade_count: trades.len(),
            win_rate_pct: if trades.is_empty() {
                0.0
            } else {
                wins.len() as f64 / trades.len() as f64 * 100.0
            },
            // A run with no losses has an undefined profit factor rather than
            // an infinite one; report it as 0 and let the trade count show why.
            profit_factor: if gross_loss > 0.0 {
                gross_profit / gross_loss
            } else {
                0.0
            },
            avg_win_pct: mean_of(wins.iter().map(|t| t.return_pct)),
            avg_loss_pct: mean_of(losses.iter().map(|t| t.return_pct)),
            best_trade_pct: extremum(&trades, f64::max),
            worst_trade_pct: extremum(&trades, f64::min),
            avg_bars_held: if trades.is_empty() {
                0.0
            } else {
                held_bars as f64 / trades.len() as f64
            },
            exposure_pct: if bars.is_empty() {
                0.0
            } else {
                held_bars as f64 / bars.len() as f64 * 100.0
            },
            intrabar_warning: strategy.uses_intrabar_range(),
            bar_count: bars.len(),
            trades,
        }
    }

    /// Whether the result is too thin to draw a conclusion from.
    ///
    /// Research on backtest validity is blunt about this: a handful of trades
    /// tells you nothing, and a Sharpe above ~3 is far more likely to be
    /// overfitting or a data artefact than a real edge. Both are surfaced as
    /// caveats rather than left for the user to know unprompted.
    pub fn caveats(&self) -> Vec<String> {
        let mut out = Vec::new();

        // First, because it is a statement about the data rather than about
        // the result — it holds even when the strategy never fired.
        if self.intrabar_warning {
            out.push(
                "Uses high/low, which PSX's long-run feed does not carry — those are derived from open/close outside the recent snapshot window.".into(),
            );
        }

        if self.trade_count == 0 {
            out.push("No trades — the entry rule never fired on this symbol.".into());
            return out;
        }
        if self.trade_count < 10 {
            out.push(format!(
                "Only {} trades: too few to distinguish skill from luck.",
                self.trade_count
            ));
        }
        if self.sharpe > 3.0 {
            out.push(format!(
                "A Sharpe of {:.2} is implausibly high — suspect overfitting before celebrating.",
                self.sharpe
            ));
        }
        if self.total_return_pct < self.buy_hold_return_pct {
            out.push(format!(
                "Buy-and-hold returned {:.1}% over the same window; this strategy did not beat it.",
                self.buy_hold_return_pct
            ));
        }
        out
    }
}

fn pct_change(from: f64, to: f64) -> f64 {
    if from.abs() < f64::EPSILON {
        0.0
    } else {
        (to - from) / from * 100.0
    }
}

/// Best or worst trade return. Folding an empty list would give an infinity,
/// which then propagates into the UI as `inf%`; no trades means zero here.
fn extremum(trades: &[Trade], pick: fn(f64, f64) -> f64) -> f64 {
    trades
        .iter()
        .map(|t| t.return_pct)
        .reduce(pick)
        .unwrap_or(0.0)
}

fn mean_of(it: impl Iterator<Item = f64>) -> f64 {
    let v: Vec<f64> = it.collect();
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::engine::{self, Costs};
    use crate::backtest::strategy::Strategy;

    fn bars_from(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, c)| Bar {
                ts: 1_600_000_000 + i as i64 * 86_400,
                open: if i == 0 { *c } else { closes[i - 1] },
                high: *c,
                low: *c,
                close: *c,
                volume: 1000.0,
            })
            .collect()
    }

    fn always_in() -> Strategy {
        Strategy::parse(
            r#"
name = "Always in"
[rules]
entry = "close > 0"
exit = "false"
"#,
        )
        .unwrap()
    }

    fn cfg() -> Config {
        Config {
            initial_equity: 100_000.0,
            costs: Costs {
                commission_bps: 0.0,
                slippage_bps: 0.0,
            },
        }
    }

    #[test]
    fn buy_and_hold_is_reported_as_the_benchmark() {
        let bars = bars_from(&[100.0, 110.0, 120.0]);
        let s = always_in();
        let r = engine::run(&s, &bars, &s.defaults(), &cfg()).unwrap();
        assert!((r.buy_hold_return_pct - 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_run_with_no_trades_says_so_rather_than_reporting_zeros() {
        let s = Strategy::parse(
            r#"
name = "Never"
[rules]
entry = "false"
exit = "false"
"#,
        )
        .unwrap();
        let bars = bars_from(&[100.0, 110.0, 120.0]);
        let r = engine::run(&s, &bars, &s.defaults(), &cfg()).unwrap();
        assert_eq!(r.trade_count, 0);
        assert!(r.caveats().iter().any(|c| c.contains("No trades")));
    }

    #[test]
    fn a_thin_sample_is_flagged() {
        let bars = bars_from(&[100.0, 105.0, 110.0]);
        let s = always_in();
        let r = engine::run(&s, &bars, &s.defaults(), &cfg()).unwrap();
        assert!(r.caveats().iter().any(|c| c.contains("too few")));
    }

    #[test]
    fn failing_to_beat_buy_and_hold_is_called_out() {
        // Enters late, so it captures less than the full move.
        let s = Strategy::parse(
            r#"
name = "Late"
[rules]
entry = "close > 115"
exit = "false"
"#,
        )
        .unwrap();
        let bars = bars_from(&[100.0, 110.0, 120.0, 130.0]);
        let r = engine::run(&s, &bars, &s.defaults(), &cfg()).unwrap();
        assert!(
            r.caveats().iter().any(|c| c.contains("Buy-and-hold")),
            "{:?}",
            r.caveats()
        );
    }

    #[test]
    fn intrabar_dependence_reaches_the_caveats() {
        let s = Strategy::parse(
            r#"
name = "Ranger"
[rules]
entry = "close > low"
exit = "close < high"
"#,
        )
        .unwrap();
        let bars = bars_from(&[100.0, 110.0, 120.0]);
        let r = engine::run(&s, &bars, &s.defaults(), &cfg()).unwrap();
        assert!(r.intrabar_warning);
        assert!(r.caveats().iter().any(|c| c.contains("high/low")));
    }

    #[test]
    fn metrics_stay_finite_on_a_flat_series() {
        let bars = bars_from(&[100.0; 40]);
        let s = always_in();
        let r = engine::run(&s, &bars, &s.defaults(), &cfg()).unwrap();
        for v in [
            r.total_return_pct,
            r.cagr_pct,
            r.sharpe,
            r.sortino,
            r.max_drawdown_pct,
            r.volatility_pct,
            r.profit_factor,
            r.win_rate_pct,
        ] {
            assert!(v.is_finite(), "non-finite metric in a flat market: {v}");
        }
    }

    #[test]
    fn win_rate_and_profit_factor_agree_with_the_trade_list() {
        let s = Strategy::parse(
            r#"
name = "Chop"
[rules]
entry = "cross_above(close, 100)"
exit = "cross_below(close, 100)"
"#,
        )
        .unwrap();
        // Up through 100, back down, repeatedly.
        let bars = bars_from(&[
            98.0, 102.0, 105.0, 99.0, 97.0, 103.0, 108.0, 95.0, 99.0, 101.0, 110.0, 90.0,
        ]);
        let r = engine::run(&s, &bars, &s.defaults(), &cfg()).unwrap();

        let wins = r.trades.iter().filter(|t| t.is_win()).count();
        let expected = wins as f64 / r.trades.len() as f64 * 100.0;
        assert!((r.win_rate_pct - expected).abs() < 1e-9);
        assert!(r.profit_factor.is_finite());
    }

    #[test]
    fn exposure_reflects_time_in_the_market() {
        let bars = bars_from(&[100.0, 101.0, 102.0, 103.0, 104.0]);
        let s = always_in();
        let r = engine::run(&s, &bars, &s.defaults(), &cfg()).unwrap();
        assert!(r.exposure_pct > 0.0 && r.exposure_pct <= 100.0);
    }
}
