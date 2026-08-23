//! The bar-by-bar simulation.
//!
//! # The look-ahead guarantee
//!
//! Look-ahead bias is the failure mode that matters here: a signal computed
//! from a bar's *close*, filled at that same bar's *open*, is trading on
//! information that did not exist yet. It does not announce itself — the
//! equity curve simply comes out beautiful — which is why this engine makes it
//! structurally impossible rather than leaving it to whoever writes a strategy
//! file:
//!
//! - Signals for bar `i` are read only from columns computed over `bars[0..=i]`.
//! - Any order they raise fills at `bars[i + 1].open`.
//! - A signal on the final bar has no bar to fill in and is dropped.
//!
//! There is no code path that fills at the signal bar's own price, so no
//! strategy file can ask for one.
//!
//! # Intrabar fills
//!
//! Stops and targets are checked **at bar close only**, never against the
//! bar's high or low. PSX's long-run EOD feed carries no true high or low —
//! outside the recent snapshot window those columns are derived from open and
//! close — so triggering a stop intrabar would fill at a price that never
//! traded. [`Report::intrabar_warning`] carries this to the UI whenever a
//! strategy's outcome depends on it.

use std::collections::HashMap;

use anyhow::Result;

use super::report::{Report, Trade, TradeExit};
use super::strategy::{Direction, Strategy};
use crate::model::Bar;

/// Trading frictions, in basis points of the traded value.
///
/// Defaults are deliberately non-zero. A frictionless backtest flatters every
/// strategy, and mean-reversion strategies that trade often are flattered
/// most — so the honest default is to charge something.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Costs {
    /// Broker commission per side.
    pub commission_bps: f64,
    /// Slippage per side: the gap between the modelled price and a real fill.
    pub slippage_bps: f64,
}

impl Default for Costs {
    fn default() -> Self {
        // ~15bp a side is a realistic retail round trip on the PSX for a
        // liquid scrip once commission and the bid/ask are both counted.
        Self {
            commission_bps: 10.0,
            slippage_bps: 5.0,
        }
    }
}

impl Costs {
    fn per_side(&self) -> f64 {
        (self.commission_bps + self.slippage_bps) / 10_000.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub initial_equity: f64,
    pub costs: Costs,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            initial_equity: 1_000_000.0,
            costs: Costs::default(),
        }
    }
}

/// Run a strategy over a bar series.
///
/// `bars` must be oldest-first, which is what [`crate::cache::Store::bars`]
/// returns.
pub fn run(
    strategy: &Strategy,
    bars: &[Bar],
    params: &HashMap<String, f64>,
    config: &Config,
) -> Result<Report> {
    run_with_benchmark(strategy, bars, None, params, config)
}

/// Run with a market benchmark available to the strategy as
/// `benchmark_close`. The plain [`run`] entry point remains source-compatible
/// for strategies that do not use relative strength.
pub fn run_with_benchmark(
    strategy: &Strategy,
    bars: &[Bar],
    benchmark: Option<&[Bar]>,
    params: &HashMap<String, f64>,
    config: &Config,
) -> Result<Report> {
    run_with_benchmark_from(strategy, bars, benchmark, params, config, None)
}

/// As [`run_with_benchmark`], but bars before `trade_from_ts` are indicator
/// warm-up only: entries are blocked and the report starts at that timestamp.
pub fn run_with_benchmark_from(
    strategy: &Strategy,
    bars: &[Bar],
    benchmark: Option<&[Bar]>,
    params: &HashMap<String, f64>,
    config: &Config,
    trade_from_ts: Option<i64>,
) -> Result<Report> {
    let signals = strategy.signals_with_benchmark(bars, benchmark, params)?;
    let report_from = trade_from_ts
        .and_then(|ts| bars.iter().position(|b| b.ts >= ts))
        .unwrap_or(0);

    if strategy.entry_size_pct < 100.0 || strategy.add.is_some() || strategy.stop.is_some() {
        return run_staged(
            strategy,
            bars,
            params,
            config,
            signals,
            trade_from_ts,
            report_from,
        );
    }

    let n = bars.len();
    let mut equity: Vec<f64> = Vec::with_capacity(n);
    let mut trades: Vec<Trade> = Vec::new();

    let mut cash = config.initial_equity;
    let mut open: Option<OpenTrade> = None;
    let cost = config.costs.per_side();
    let long = strategy.direction == Direction::Long;

    // A pending order raised on bar `i`, to be filled on bar `i + 1`. This is
    // the whole of the look-ahead defence: an order can only ever be created
    // in one iteration and executed in the next.
    let mut pending: Option<Order> = None;

    for i in 0..n {
        let bar = &bars[i];

        // 1. Fill whatever the previous bar decided, at this bar's open.
        if let Some(order) = pending.take() {
            let price = bar.open;
            match order {
                Order::Enter => {
                    // All-in sizing: the equity curve then reads as the
                    // strategy's own return, uncontaminated by a position-sizing
                    // scheme the user did not choose.
                    //
                    // The entry fee comes out of cash here, so `equity_at_entry`
                    // is already net of it and the exit only charges its own
                    // side.
                    let fee = cash * cost;
                    let spendable = cash - fee;
                    if price > 0.0 && spendable > 0.0 {
                        let qty = spendable / price;
                        cash = spendable;
                        open = Some(OpenTrade {
                            entry_index: i,
                            entry_ts: bar.ts,
                            entry_price: price,
                            qty: if long { qty } else { -qty },
                            equity_at_entry: cash,
                        });
                    }
                }
                Order::Exit(reason) => {
                    if let Some(t) = open.take() {
                        let fee = t.qty.abs() * price * cost;
                        // Realised P&L on a signed position: a short profits
                        // when the price falls.
                        let pnl = t.qty * (price - t.entry_price) - fee;
                        cash = t.equity_at_entry + pnl;
                        trades.push(Trade {
                            entry_ts: t.entry_ts,
                            exit_ts: bar.ts,
                            entry_price: t.entry_price,
                            exit_price: price,
                            qty: t.qty,
                            pnl,
                            return_pct: if t.entry_price > 0.0 {
                                let raw = (price - t.entry_price) / t.entry_price * 100.0;
                                if long { raw } else { -raw }
                            } else {
                                0.0
                            },
                            bars_held: i - t.entry_index,
                            adds: 0,
                            exit_reason: reason,
                        });
                    }
                }
            }
        }

        // 2. Mark to market at this bar's close.
        let marked = if let Some(t) = &open {
            t.equity_at_entry + t.qty * (bar.close - t.entry_price)
        } else {
            cash
        };
        equity.push(marked);

        // 3. Decide what to do on the *next* bar, using only what is known now.
        //    The last bar gets no decision because nothing could fill it.
        if i + 1 >= n {
            continue;
        }

        // A rule column is "true at bar i" only if it is defined there; the
        // warm-up `None` must never read as a signal.
        let fired = |s: &Option<super::expr::Series>| -> bool {
            s.as_ref().and_then(|c| c[i]).is_some_and(|v| v != 0.0)
        };

        if let Some(t) = &open {
            let held = i - t.entry_index;
            let mut reason = None;

            // Stops are close-based on purpose; see the module docs.
            if held >= strategy.min_hold_bars {
                let move_pct = signed_return_pct(t, bar.close, long);
                if let Some(sl) = strategy.stop_loss_pct
                    && move_pct <= -sl
                {
                    reason = Some(TradeExit::StopLoss);
                } else if let Some(tp) = strategy.take_profit_pct
                    && move_pct >= tp
                {
                    reason = Some(TradeExit::TakeProfit);
                } else if fired(&signals.exit) {
                    reason = Some(TradeExit::Signal);
                }
            }

            if let Some(r) = reason {
                pending = Some(Order::Exit(r));
            }
        } else {
            let entry = signals.entry[i].is_some_and(|v| v != 0.0);
            let allowed = signals.filter.is_none() || fired(&signals.filter);
            let active = trade_from_ts.is_none_or(|ts| bar.ts >= ts);
            if entry && allowed && active {
                pending = Some(Order::Enter);
            }
        }
    }

    // A position still open at the end is closed at the last close, so the
    // equity curve and the trade list agree on the final number.
    if let Some(t) = open.take()
        && let Some(last) = bars.last()
    {
        let fee = t.qty.abs() * last.close * cost;
        let pnl = t.qty * (last.close - t.entry_price) - fee;
        cash = t.equity_at_entry + pnl;
        trades.push(Trade {
            entry_ts: t.entry_ts,
            exit_ts: last.ts,
            entry_price: t.entry_price,
            exit_price: last.close,
            qty: t.qty,
            pnl,
            return_pct: {
                let raw = (last.close - t.entry_price) / t.entry_price * 100.0;
                if long { raw } else { -raw }
            },
            bars_held: n.saturating_sub(1) - t.entry_index,
            adds: 0,
            exit_reason: TradeExit::EndOfData,
        });
        if let Some(e) = equity.last_mut() {
            *e = cash;
        }
    }

    Ok(Report::build(
        strategy,
        &bars[report_from..],
        equity[report_from..].to_vec(),
        trades,
        params.clone(),
        config,
    ))
}

/// Cash-account simulation for partial entries and add-ons. Existing
/// all-in strategies stay on the original path above so their historical
/// results and arithmetic remain byte-for-byte stable.
fn run_staged(
    strategy: &Strategy,
    bars: &[Bar],
    params: &HashMap<String, f64>,
    config: &Config,
    signals: super::strategy::Signals,
    trade_from_ts: Option<i64>,
    report_from: usize,
) -> Result<Report> {
    let n = bars.len();
    let mut equity = Vec::with_capacity(n);
    let mut trades = Vec::new();
    let mut cash = config.initial_equity;
    let mut open: Option<LayeredTrade> = None;
    let mut pending: Option<LayeredOrder> = None;
    let cost = config.costs.per_side();
    let long = strategy.direction == Direction::Long;

    for i in 0..n {
        let bar = &bars[i];

        if let Some(order) = pending.take() {
            match order {
                LayeredOrder::Enter => {
                    let before = cash;
                    if let Some((qty, notional, fee)) =
                        allocation(cash, 0.0, bar.open, strategy.entry_size_pct, cost, long)
                    {
                        cash += if long {
                            -notional - fee
                        } else {
                            notional - fee
                        };
                        open = Some(LayeredTrade {
                            entry_index: i,
                            entry_ts: bar.ts,
                            entry_price: bar.open,
                            qty,
                            equity_at_entry: before,
                            adds: 0,
                            stop_level: None,
                        });
                    }
                }
                LayeredOrder::Add => {
                    if let Some(t) = open.as_mut() {
                        let marked = cash + t.qty * bar.open;
                        if let Some((qty, notional, fee)) =
                            allocation(marked, cash, bar.open, strategy.add_size_pct, cost, long)
                        {
                            let old_abs = t.qty.abs();
                            let add_abs = qty.abs();
                            t.entry_price = (t.entry_price * old_abs + bar.open * add_abs)
                                / (old_abs + add_abs);
                            t.qty += qty;
                            t.adds += 1;
                            cash += if long {
                                -notional - fee
                            } else {
                                notional - fee
                            };
                        }
                    }
                }
                LayeredOrder::Exit(reason) => {
                    if let Some(t) = open.take() {
                        let notional = t.qty.abs() * bar.open;
                        let fee = notional * cost;
                        cash += if long {
                            notional - fee
                        } else {
                            -notional - fee
                        };
                        let pnl = cash - t.equity_at_entry;
                        trades.push(layered_trade(&t, bar.ts, bar.open, i, pnl, reason, long));
                    }
                }
            }
        }

        let marked = open
            .as_ref()
            .map(|t| cash + t.qty * bar.close)
            .unwrap_or(cash);
        equity.push(marked);

        if i + 1 >= n {
            continue;
        }

        let fired = |s: &Option<super::expr::Series>| -> bool {
            s.as_ref().and_then(|c| c[i]).is_some_and(|v| v != 0.0)
        };
        let allowed = signals.filter.is_none() || fired(&signals.filter);

        if let Some(t) = open.as_mut() {
            if let Some(level) = signals.stop.as_ref().and_then(|s| s[i])
                && level.is_finite()
                && level > 0.0
            {
                t.stop_level = Some(match t.stop_level {
                    None => level,
                    Some(old) if long => old.max(level),
                    Some(old) => old.min(level),
                });
            }

            let held = i - t.entry_index;
            let mut reason = None;
            if held >= strategy.min_hold_bars {
                let move_pct = signed_return_pct_layered(t, bar.close, long);
                let dynamic_stop = t
                    .stop_level
                    .is_some_and(|s| if long { bar.close <= s } else { bar.close >= s });
                if dynamic_stop {
                    reason = Some(TradeExit::StopLoss);
                } else if let Some(sl) = strategy.stop_loss_pct
                    && move_pct <= -sl
                {
                    reason = Some(TradeExit::StopLoss);
                } else if let Some(tp) = strategy.take_profit_pct
                    && move_pct >= tp
                {
                    reason = Some(TradeExit::TakeProfit);
                } else if fired(&signals.exit) {
                    reason = Some(TradeExit::Signal);
                }
            }

            if let Some(reason) = reason {
                pending = Some(LayeredOrder::Exit(reason));
            } else if t.adds < strategy.max_adds && allowed && fired(&signals.add) {
                pending = Some(LayeredOrder::Add);
            }
        } else if signals.entry[i].is_some_and(|v| v != 0.0)
            && allowed
            && trade_from_ts.is_none_or(|ts| bar.ts >= ts)
        {
            pending = Some(LayeredOrder::Enter);
        }
    }

    if let Some(t) = open.take()
        && let Some(last) = bars.last()
    {
        let notional = t.qty.abs() * last.close;
        let fee = notional * cost;
        cash += if long {
            notional - fee
        } else {
            -notional - fee
        };
        let pnl = cash - t.equity_at_entry;
        trades.push(layered_trade(
            &t,
            last.ts,
            last.close,
            n.saturating_sub(1),
            pnl,
            TradeExit::EndOfData,
            long,
        ));
        if let Some(last_equity) = equity.last_mut() {
            *last_equity = cash;
        }
    }

    Ok(Report::build(
        strategy,
        &bars[report_from..],
        equity[report_from..].to_vec(),
        trades,
        params.clone(),
        config,
    ))
}

fn allocation(
    equity: f64,
    cash: f64,
    price: f64,
    pct: f64,
    cost: f64,
    long: bool,
) -> Option<(f64, f64, f64)> {
    if price <= 0.0 || equity <= 0.0 || pct <= 0.0 {
        return None;
    }
    let mut notional = equity * pct / 100.0;
    if long {
        let available = if cash > 0.0 {
            cash / (1.0 + cost)
        } else {
            equity / (1.0 + cost)
        };
        notional = notional.min(available);
    }
    if notional <= 0.0 {
        return None;
    }
    let abs_qty = notional / price;
    Some((
        if long { abs_qty } else { -abs_qty },
        notional,
        notional * cost,
    ))
}

fn layered_trade(
    t: &LayeredTrade,
    exit_ts: i64,
    exit_price: f64,
    exit_index: usize,
    pnl: f64,
    reason: TradeExit,
    long: bool,
) -> Trade {
    let raw = if t.entry_price > 0.0 {
        (exit_price - t.entry_price) / t.entry_price * 100.0
    } else {
        0.0
    };
    Trade {
        entry_ts: t.entry_ts,
        exit_ts,
        entry_price: t.entry_price,
        exit_price,
        qty: t.qty,
        pnl,
        return_pct: if long { raw } else { -raw },
        bars_held: exit_index.saturating_sub(t.entry_index),
        adds: t.adds,
        exit_reason: reason,
    }
}

fn signed_return_pct_layered(t: &LayeredTrade, price: f64, long: bool) -> f64 {
    if t.entry_price <= 0.0 {
        return 0.0;
    }
    let raw = (price - t.entry_price) / t.entry_price * 100.0;
    if long { raw } else { -raw }
}

#[derive(Debug, Clone, Copy)]
enum LayeredOrder {
    Enter,
    Add,
    Exit(TradeExit),
}

#[derive(Debug, Clone, Copy)]
struct LayeredTrade {
    entry_index: usize,
    entry_ts: i64,
    entry_price: f64,
    qty: f64,
    equity_at_entry: f64,
    adds: usize,
    stop_level: Option<f64>,
}

fn signed_return_pct(t: &OpenTrade, price: f64, long: bool) -> f64 {
    if t.entry_price <= 0.0 {
        return 0.0;
    }
    let raw = (price - t.entry_price) / t.entry_price * 100.0;
    if long { raw } else { -raw }
}

#[derive(Debug, Clone, Copy)]
enum Order {
    Enter,
    Exit(TradeExit),
}

#[derive(Debug, Clone, Copy)]
struct OpenTrade {
    entry_index: usize,
    entry_ts: i64,
    entry_price: f64,
    qty: f64,
    equity_at_entry: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::strategy::Strategy;

    fn series(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, c)| Bar {
                ts: 1_600_000_000 + i as i64 * 86_400,
                // Open equals the previous close, which is what makes the
                // next-bar-open fill testable to an exact number.
                open: if i == 0 { *c } else { closes[i - 1] },
                high: c.max(if i == 0 { *c } else { closes[i - 1] }),
                low: c.min(if i == 0 { *c } else { closes[i - 1] }),
                close: *c,
                volume: 10_000.0,
            })
            .collect()
    }

    fn frictionless() -> Config {
        Config {
            initial_equity: 100_000.0,
            costs: Costs {
                commission_bps: 0.0,
                slippage_bps: 0.0,
            },
        }
    }

    /// Enters when close crosses above 100, exits when it crosses below.
    fn threshold_strategy() -> Strategy {
        Strategy::parse(
            r#"
name = "Threshold"
[rules]
entry = "cross_above(close, 100)"
exit = "cross_below(close, 100)"
"#,
        )
        .unwrap()
    }

    #[test]
    fn entry_fills_at_the_next_bars_open_not_the_signal_bars_close() {
        // Signal fires on index 1 (99 -> 101 crosses 100). The fill must be at
        // bar 2's open, which is 101 — NOT bar 1's close, also 101 but for the
        // wrong reason, so bar 2's open is set apart to tell them apart.
        let mut bars = series(&[99.0, 101.0, 105.0, 110.0]);
        bars[2].open = 102.0; // distinct from bar 1's close of 101

        let s = threshold_strategy();
        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();

        assert_eq!(r.trades.len(), 1, "expected one round trip");
        assert_eq!(
            r.trades[0].entry_price, 102.0,
            "filled at the signal bar's close instead of the next bar's open"
        );
    }

    #[test]
    fn a_signal_on_the_final_bar_never_trades() {
        // The cross happens on the last bar, so there is no bar left to fill
        // in. Taking the trade anyway would be free money from the future.
        let bars = series(&[98.0, 99.0, 101.0]);
        let s = threshold_strategy();
        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert!(r.trades.is_empty(), "traded on a bar that had no successor");
    }

    #[test]
    fn equity_is_flat_while_out_of_the_market() {
        let bars = series(&[90.0, 91.0, 92.0, 93.0]);
        let s = threshold_strategy();
        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert!(r.trades.is_empty());
        assert!(
            r.equity.iter().all(|e| (*e - 100_000.0).abs() < 1e-6),
            "equity moved without a position: {:?}",
            r.equity
        );
    }

    #[test]
    fn a_long_trade_earns_the_price_move() {
        // Enter at bar 2 open (100), exit at bar 5 open (120): +20%.
        let mut bars = series(&[99.0, 101.0, 0.0, 0.0, 90.0, 0.0]);
        bars[2].open = 100.0;
        bars[2].close = 110.0;
        bars[3].close = 115.0;
        bars[4].close = 90.0; // crosses back below 100 -> exit signal
        bars[5].open = 120.0;
        bars[5].close = 120.0;

        let s = threshold_strategy();
        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();

        assert_eq!(r.trades.len(), 1);
        let t = &r.trades[0];
        assert_eq!(t.entry_price, 100.0);
        assert_eq!(t.exit_price, 120.0);
        assert!((t.return_pct - 20.0).abs() < 1e-9, "{}", t.return_pct);
        assert!((r.equity.last().unwrap() - 120_000.0).abs() < 1.0);
    }

    #[test]
    fn costs_reduce_the_return() {
        let mut bars = series(&[99.0, 101.0, 0.0, 0.0, 90.0, 0.0]);
        bars[2].open = 100.0;
        bars[2].close = 110.0;
        bars[3].close = 115.0;
        bars[4].close = 90.0;
        bars[5].open = 120.0;
        bars[5].close = 120.0;

        let s = threshold_strategy();
        let free = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        let charged = run(&s, &bars, &s.defaults(), &Config::default()).unwrap();

        assert!(
            charged.total_return_pct < free.total_return_pct,
            "costs did not bite: {} vs {}",
            charged.total_return_pct,
            free.total_return_pct
        );
    }

    #[test]
    fn a_stop_loss_closes_the_position() {
        let s = Strategy::parse(
            r#"
name = "Stopped"
stop_loss_pct = 5
[rules]
entry = "cross_above(close, 100)"
"#,
        )
        .unwrap();

        // Enter around 100, then fall hard.
        let mut bars = series(&[99.0, 101.0, 0.0, 0.0, 0.0]);
        bars[2].open = 100.0;
        bars[2].close = 100.0;
        bars[3].close = 90.0; // -10%, past the 5% stop
        bars[4].open = 89.0;
        bars[4].close = 88.0;

        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].exit_reason, TradeExit::StopLoss);
        // Filled at the next open after the breach, never intrabar.
        assert_eq!(r.trades[0].exit_price, 89.0);
    }

    #[test]
    fn a_stop_never_fills_at_the_bars_low() {
        // The bar dips to 50 intrabar but closes at 99. With true intrabar
        // stops this would fill near 95; close-based it must not fire at all,
        // because that 50 is synthetic data on most of our history.
        let s = Strategy::parse(
            r#"
name = "Stopped"
stop_loss_pct = 5
[rules]
entry = "cross_above(close, 100)"
"#,
        )
        .unwrap();

        let mut bars = series(&[99.0, 101.0, 0.0, 0.0]);
        bars[2].open = 100.0;
        bars[2].close = 100.0;
        bars[3].low = 50.0;
        bars[3].close = 99.0;

        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert!(
            r.trades.is_empty() || r.trades[0].exit_reason != TradeExit::StopLoss,
            "a stop fired against an intrabar low"
        );
    }

    #[test]
    fn an_open_position_is_closed_at_the_end_of_the_data() {
        let mut bars = series(&[99.0, 101.0, 0.0, 0.0]);
        bars[2].open = 100.0;
        bars[2].close = 105.0;
        bars[3].close = 110.0;

        let s = threshold_strategy();
        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].exit_reason, TradeExit::EndOfData);
        // Equity's last point must agree with the closed trade.
        assert!((r.equity.last().unwrap() - (100_000.0 * 1.10)).abs() < 1.0);
    }

    #[test]
    fn a_filter_can_block_entries() {
        let with_filter = Strategy::parse(
            r#"
name = "Filtered"
[rules]
entry = "cross_above(close, 100)"
exit = "cross_below(close, 100)"
filter = "false"
"#,
        )
        .unwrap();

        let mut bars = series(&[99.0, 101.0, 0.0, 0.0]);
        bars[2].open = 100.0;
        bars[2].close = 105.0;
        bars[3].close = 110.0;

        let r = run(
            &with_filter,
            &bars,
            &with_filter.defaults(),
            &frictionless(),
        )
        .unwrap();
        assert!(r.trades.is_empty(), "filter did not block the entry");
    }

    #[test]
    fn a_short_strategy_profits_when_the_price_falls() {
        let s = Strategy::parse(
            r#"
name = "Short it"
direction = "short"
[rules]
entry = "cross_below(close, 100)"
exit = "cross_above(close, 100)"
"#,
        )
        .unwrap();

        let mut bars = series(&[101.0, 99.0, 0.0, 0.0, 105.0, 0.0]);
        bars[2].open = 100.0;
        bars[2].close = 95.0;
        bars[3].close = 90.0;
        bars[4].close = 105.0; // crosses back above -> exit
        bars[5].open = 80.0;
        bars[5].close = 80.0;

        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert_eq!(r.trades.len(), 1);
        // Entered short at 100, covered at 80: +20%.
        assert!(r.trades[0].return_pct > 0.0, "{:?}", r.trades[0]);
        assert!(*r.equity.last().unwrap() > 100_000.0);
    }

    #[test]
    fn equity_has_one_point_per_bar() {
        let bars = series(&[99.0, 101.0, 105.0, 98.0, 103.0]);
        let s = threshold_strategy();
        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert_eq!(r.equity.len(), bars.len());
    }

    #[test]
    fn an_empty_series_produces_an_empty_report_rather_than_panicking() {
        let s = threshold_strategy();
        let r = run(&s, &[], &s.defaults(), &frictionless()).unwrap();
        assert!(r.trades.is_empty());
        assert!(r.equity.is_empty());
    }

    #[test]
    fn a_flat_price_series_trades_nothing_and_stays_finite() {
        // Limit-locked scrips are common on the PSX; nothing here may produce
        // a NaN.
        let bars = series(&[100.0; 50]);
        let s = threshold_strategy();
        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert!(r.equity.iter().all(|e| e.is_finite()));
        assert!(r.total_return_pct.is_finite());
    }

    #[test]
    fn min_hold_bars_delays_the_exit() {
        let s = Strategy::parse(
            r#"
name = "Patient"
min_hold_bars = 3
[rules]
entry = "cross_above(close, 100)"
exit = "close > 0"
"#,
        )
        .unwrap();

        let mut bars = series(&[99.0, 101.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        bars[2].open = 100.0;
        for b in bars.iter_mut().skip(2) {
            b.close = 100.0;
        }

        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert_eq!(r.trades.len(), 1);
        assert!(
            r.trades[0].bars_held >= 3,
            "exited after {} bars despite min_hold_bars = 3",
            r.trades[0].bars_held
        );
    }

    #[test]
    fn staged_entry_commits_probe_then_confirmation_capital() {
        let s = Strategy::parse(
            r#"
name = "Staged"
entry_size_pct = 50
add_size_pct = 50
max_adds = 1
[rules]
entry = "cross_above(close, 100)"
add = "close > 104"
exit = "close < 0"
"#,
        )
        .unwrap();
        let mut bars = series(&[99.0, 101.0, 105.0, 110.0, 110.0]);
        bars[2].open = 100.0;
        bars[3].open = 105.0;

        let r = run(&s, &bars, &s.defaults(), &frictionless()).unwrap();
        assert_eq!(r.trades.len(), 1);
        assert_eq!(r.trades[0].adds, 1);
        let final_equity = *r.equity.last().unwrap();
        assert!(final_equity > 105_000.0 && final_equity < 110_000.0);
    }

    #[test]
    fn warmup_bars_compute_indicators_but_cannot_trade() {
        let s = Strategy::parse(
            r#"
name = "Warm"
[indicators]
trend = "sma(close, 200)"
[rules]
entry = "close > trend"
exit = "false"
"#,
        )
        .unwrap();
        let closes: Vec<f64> = (0..250).map(|i| 100.0 + i as f64 * 0.1).collect();
        let bars = series(&closes);
        let from = bars[220].ts;
        let r =
            run_with_benchmark_from(&s, &bars, None, &s.defaults(), &frictionless(), Some(from))
                .unwrap();
        assert_eq!(r.bar_count, 30);
        assert_eq!(r.trades.len(), 1);
        assert!(r.trades[0].entry_ts >= from);
    }
}
