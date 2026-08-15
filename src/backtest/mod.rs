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
    (
        "swing-checklist.toml",
        include_str!("../../strategies/swing-checklist.toml"),
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
        //
        // The bars carry real bodies, real shadows and a varying volume rather
        // than `open == close` at a flat 100k. A strategy reading candle shape
        // or a volume average cannot say anything at all about a series that
        // has neither, so a degenerate fixture would quietly exempt exactly the
        // strategies this test exists to check.
        // Bar shape is drawn from a seeded generator rather than from more
        // sine terms. Sines would stay phase-locked to the price cycle, so a
        // rule needing a particular candle at a particular point in a pullback
        // either aligns in the first cycle or never aligns at all, however long
        // the series runs. A fixed seed keeps the test deterministic.
        let mut seed: u64 = 0x5eed_1234_9abc_def1;
        let mut noise = move || {
            // xorshift64*: a few lines, good enough to decorrelate shape from
            // phase, and reproducible across platforms.
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };

        let close_at = |t: f64| {
            100.0
                + t * 0.04
                + (t / 9.0).sin() * 7.0
                + (t / 31.0).cos() * 12.0
                + (t / 5.0).sin() * 2.0
        };
        let mut bars: Vec<crate::model::Bar> = Vec::with_capacity(1_500);
        for i in 0..1_500i64 {
            let t = i as f64;
            let close = close_at(t);
            // Opening away from the previous close is what gives the bar a
            // body, and what lets one bar engulf another.
            let open = if i == 0 {
                close
            } else {
                close_at(t - 1.0) + (noise() - 0.5) * 2.4
            };
            // Independent shadows above and below, so the lopsided shapes
            // (hammer, star) occur at a plausible rate instead of never.
            let (hi, lo) = (close.max(open), close.min(open));
            bars.push(crate::model::Bar {
                ts: 1_600_000_000 + i * 86_400,
                open,
                high: hi + noise().powi(2) * 3.0,
                low: lo - noise().powi(2) * 3.0,
                close,
                volume: 100_000.0 * (0.5 + noise() * 1.5),
            });
        }

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
