//! Backtesting: run a declared strategy over cached history and report what
//! it would have done.
//!
//! Layering mirrors the rest of the crate — pure, synchronous, allocation-light
//! so a run can happen between two frames without a background task:
//!
//! - [`expr`] parses and evaluates the small series language strategy files
//!   are written in
//! - [`strategy`] is the TOML schema, its validation and loading from disk
//! - [`engine`] steps through bars and produces fills
//! - [`report`] turns a run into metrics
//! - [`optimize`] sweeps parameters, walks forward and scans the universe
//!
//! # What this cannot tell you
//!
//! Three limits are structural rather than bugs to be fixed later, and the UI
//! states each one where the affected number appears:
//!
//! 1. **Synthetic intrabar range.** PSX's long-run EOD feed carries close,
//!    volume and open — no high or low. Outside the recent snapshot window
//!    those columns are derived from open/close, so intrabar fills are not
//!    modelled at all and stops are evaluated at the close.
//! 2. **Survivorship.** The symbol list holds currently-listed scrips, so
//!    anything delisted is absent and every universe-wide aggregate is biased
//!    upward.
//! 3. **Fitting.** A tweak panel is a curve-fitting machine. [`optimize`]
//!    provides walk-forward validation because an in-sample equity curve is
//!    not evidence, and [`report::Report::caveats`] flags results too thin or
//!    too good to believe.

pub mod engine;
pub mod expr;
pub mod optimize;
pub mod report;
pub mod strategy;

pub use engine::{Config, Costs};
pub use optimize::{Objective, ScanRow, ScanSummary, SweepPoint, WalkForward};
pub use report::{Report, Trade, TradeExit};
pub use strategy::{Direction, Param, Strategy};

/// The strategies shipped with the binary.
///
/// Bundled rather than downloaded so a fresh install has something to run
/// immediately, and so the format has worked examples that are guaranteed to
/// parse — every one of these is checked by a test.
pub const BUILTIN: &[(&str, &str)] = &[
    (
        "golden-cross.toml",
        include_str!("../../strategies/golden-cross.toml"),
    ),
    ("rsi-2.toml", include_str!("../../strategies/rsi-2.toml")),
    (
        "turtle-donchian.toml",
        include_str!("../../strategies/turtle-donchian.toml"),
    ),
    (
        "macd-cross.toml",
        include_str!("../../strategies/macd-cross.toml"),
    ),
    (
        "bollinger-reversion.toml",
        include_str!("../../strategies/bollinger-reversion.toml"),
    ),
    (
        "dual-momentum.toml",
        include_str!("../../strategies/dual-momentum.toml"),
    ),
    (
        "adx-trend.toml",
        include_str!("../../strategies/adx-trend.toml"),
    ),
    (
        "triple-ma.toml",
        include_str!("../../strategies/triple-ma.toml"),
    ),
];

/// Write the bundled strategies into the user's strategy directory.
///
/// Existing files are never overwritten: once a strategy is on disk it is the
/// user's to edit, and an upgrade quietly reverting their tweaks would be
/// worse than shipping nothing.
pub fn install_builtins() -> anyhow::Result<usize> {
    let dir = strategy::strategy_dir()?;
    std::fs::create_dir_all(&dir)?;

    let mut written = 0;
    for (name, body) in BUILTIN {
        let path = dir.join(name);
        if path.exists() {
            continue;
        }
        std::fs::write(&path, body)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_strategy_parses() {
        for (name, body) in BUILTIN {
            if let Err(e) = Strategy::parse(body) {
                panic!("bundled strategy {name} does not parse: {e:#}");
            }
        }
    }

    #[test]
    fn bundled_strategies_have_distinct_names() {
        let mut names: Vec<String> = BUILTIN
            .iter()
            .map(|(_, b)| Strategy::parse(b).unwrap().name)
            .collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two bundled strategies share a name");
    }

    #[test]
    fn bundled_strategies_credit_their_source() {
        // These are all published, well-known systems; the file should say
        // where each came from rather than presenting it as ours.
        for (name, body) in BUILTIN {
            let s = Strategy::parse(body).unwrap();
            assert!(
                !s.source.trim().is_empty(),
                "{name} does not name its source"
            );
            assert!(!s.about.trim().is_empty(), "{name} has no description");
        }
    }

    #[test]
    fn bundled_strategies_actually_trade_on_a_realistic_series() {
        // A strategy that parses but never fires is worse than useless as a
        // worked example.
        let bars: Vec<crate::model::Bar> = (0..900)
            .map(|i| {
                let t = i as f64;
                let c = 100.0
                    + t * 0.04
                    + (t / 9.0).sin() * 7.0
                    + (t / 31.0).cos() * 12.0
                    + (t / 5.0).sin() * 2.0;
                crate::model::Bar {
                    ts: 1_600_000_000 + i as i64 * 86_400,
                    open: c,
                    high: c * 1.01,
                    low: c * 0.99,
                    close: c,
                    volume: 100_000.0,
                }
            })
            .collect();

        for (name, body) in BUILTIN {
            let s = Strategy::parse(body).unwrap();
            let r = engine::run(&s, &bars, &s.defaults(), &Config::default())
                .unwrap_or_else(|e| panic!("{name} failed to run: {e:#}"));
            assert!(
                r.trade_count > 0,
                "{name} never opened a position on a trending, oscillating series"
            );
            assert!(
                r.total_return_pct.is_finite(),
                "{name} produced a non-finite return"
            );
        }
    }

    #[test]
    fn bundled_strategy_params_are_sweepable() {
        // Bounds are what make the tweak panel and the optimiser work, so a
        // bundled file without them is a bad example to copy.
        for (name, body) in BUILTIN {
            let s = Strategy::parse(body).unwrap();
            for (p, def) in &s.params {
                assert!(
                    def.min.is_some() && def.max.is_some(),
                    "{name}: param `{p}` has no min/max, so it cannot be swept"
                );
            }
        }
    }
}
