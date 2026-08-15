//! State for the Backtest screen.
//!
//! Kept out of `app.rs` because a backtest carries rather more state than the
//! other screens — the strategy list, the tweaked parameters, the last run and
//! whatever long-running analysis was asked for.
//!
//! Running is synchronous. A single run over five years of daily bars is
//! microseconds, and even a full parameter sweep is a few milliseconds, so
//! there is nothing here worth the complexity of a background task. The
//! universe scan is the one exception and is bounded rather than backgrounded
//! — see [`BacktestState::scan_limit`].

use std::collections::HashMap;

use crate::backtest::{Config, Objective, Report, ScanRow, ScanSummary, Strategy, WalkForward};

/// Which panel the keyboard drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// The strategy list.
    #[default]
    Strategies,
    /// The parameter tweak panel.
    Params,
}

/// What the results pane is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Equity curve plus summary statistics.
    #[default]
    Equity,
    /// The individual round trips.
    Trades,
    /// Parameter sweep, best first.
    Sweep,
    /// In-sample versus out-of-sample folds.
    WalkForward,
    /// The same strategy across many symbols.
    Scan,
}

impl View {
    pub const ALL: [View; 5] = [
        View::Equity,
        View::Trades,
        View::Sweep,
        View::WalkForward,
        View::Scan,
    ];

    pub fn title(&self) -> &'static str {
        match self {
            View::Equity => "Equity",
            View::Trades => "Trades",
            View::Sweep => "Sweep",
            View::WalkForward => "Walk-forward",
            View::Scan => "Scan",
        }
    }
}

/// Which universe the last scan ran over.
///
/// The same table answers two different questions, and the difference matters
/// enough to be on screen: "does this rule work anywhere" is a market-wide
/// result with survivorship bias baked in, while "how would it have done on the
/// eight scrips I am comparing" is a hand-picked basket and biased by whatever
/// made the user pick them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanScope {
    /// The most liquid symbols on the board, up to [`SCAN_LIMIT`].
    #[default]
    Market,
    /// Every symbol on the board, however thin.
    All,
    /// Exactly the symbols on the Compare screen.
    Compare,
}

impl ScanScope {
    pub fn label(&self) -> &'static str {
        match self {
            ScanScope::Market => "Market 150",
            ScanScope::All => "All symbols",
            ScanScope::Compare => "Compare set",
        }
    }
}

/// How many symbols a universe scan covers before it stops.
///
/// A scan is O(symbols x bars) and the cache holds ~1,100 symbols against five
/// years each. Capping it keeps the screen responsive; the ranking is over the
/// most liquid names, which is where a tradable edge would have to show up
/// anyway.
pub const SCAN_LIMIT: usize = 150;

#[derive(Debug, Default)]
pub struct BacktestState {
    /// Every strategy loaded from disk, sorted by name.
    pub strategies: Vec<Strategy>,
    /// Files that failed to load, so a typo is visible rather than silent.
    pub load_errors: Vec<String>,
    pub selected: usize,
    /// Row offset, for a list taller than the panel.
    pub offset: usize,

    pub focus: Focus,
    pub view: View,

    /// The tweaked parameter values for the selected strategy. Reset to the
    /// strategy's declared defaults whenever the selection changes.
    pub params: HashMap<String, f64>,
    /// Which parameter the tweak panel is editing.
    pub param_cursor: usize,

    pub config: Config,
    pub objective: Objective,

    /// The most recent run of the selected strategy on the selected symbol.
    pub report: Option<Report>,
    /// The symbol `report` was produced from, so a stale result is never shown
    /// against the wrong scrip.
    pub report_symbol: String,

    pub sweep: Vec<crate::backtest::SweepPoint>,
    pub walk_forward: Option<WalkForward>,
    pub scan: Vec<ScanRow>,
    pub scan_summary: ScanSummary,
    /// What the rows in `scan` were run over.
    pub scan_scope: ScanScope,
    /// How many symbols were asked for, which is not how many produced a row —
    /// a scrip with too little history to warm an indicator up cannot be run
    /// at all, and the difference is worth reporting rather than hiding.
    pub scan_asked: usize,

    /// Row offsets for the scrollable result views.
    pub trades_offset: usize,
    pub sweep_offset: usize,
    pub scan_offset: usize,

    /// Set when a run failed, e.g. an indicator period longer than the history.
    pub error: Option<String>,
}

impl BacktestState {
    pub fn strategy(&self) -> Option<&Strategy> {
        self.strategies.get(self.selected)
    }

    /// Reload the strategy directory, keeping the selection on the same
    /// strategy by name where possible.
    pub fn reload(&mut self) {
        let previous = self.strategy().map(|s| s.name.clone());

        let (strategies, errors) = crate::backtest::strategy::load_all();
        self.strategies = strategies;
        self.load_errors = errors;

        self.selected = previous
            .and_then(|name| self.strategies.iter().position(|s| s.name == name))
            .unwrap_or(0);
        if self.selected >= self.strategies.len() {
            self.selected = 0;
        }
        self.reset_params();
        self.invalidate();
    }

    /// Take the selected strategy's declared defaults.
    pub fn reset_params(&mut self) {
        self.params = self.strategy().map(|s| s.defaults()).unwrap_or_default();
        self.param_cursor = 0;
    }

    /// Drop every derived result. Called whenever an input changes, so the
    /// screen can never show a sweep computed from different parameters than
    /// the equity curve beside it.
    pub fn invalidate(&mut self) {
        self.report = None;
        self.sweep.clear();
        self.walk_forward = None;
        self.scan.clear();
        self.scan_summary = ScanSummary::default();
        self.scan_asked = 0;
        self.trades_offset = 0;
        self.sweep_offset = 0;
        self.scan_offset = 0;
        self.error = None;
    }

    /// The parameter the tweak panel is on.
    pub fn current_param(&self) -> Option<(String, crate::backtest::Param)> {
        let s = self.strategy()?;
        s.params
            .iter()
            .nth(self.param_cursor)
            .map(|(k, v)| (k.clone(), v.clone()))
    }

    /// Nudge the selected parameter by `steps` of its declared step size.
    pub fn nudge_param(&mut self, steps: f64) {
        let Some((name, def)) = self.current_param() else {
            return;
        };
        let current = self.params.get(&name).copied().unwrap_or(def.default);
        let next = def.clamp(current + def.effective_step() * steps);
        self.params.insert(name, next);
        self.invalidate();
    }

    pub fn select(&mut self, index: usize) {
        if index < self.strategies.len() && index != self.selected {
            self.selected = index;
            self.reset_params();
            self.invalidate();
        }
    }

    pub fn cycle_view(&mut self, delta: isize) {
        let n = View::ALL.len() as isize;
        let i = (View::ALL.iter().position(|v| *v == self.view).unwrap_or(0) as isize + delta)
            .rem_euclid(n);
        self.view = View::ALL[i as usize];
    }

    pub fn cycle_objective(&mut self) {
        let n = Objective::ALL.len();
        let i = Objective::ALL
            .iter()
            .position(|o| *o == self.objective)
            .unwrap_or(0);
        self.objective = Objective::ALL[(i + 1) % n];
        // The sweep and walk-forward were ranked by the old objective.
        self.sweep.clear();
        self.walk_forward = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(strategies: Vec<Strategy>) -> BacktestState {
        BacktestState {
            strategies,
            ..Default::default()
        }
    }

    fn strategy(name: &str) -> Strategy {
        Strategy::parse(&format!(
            r#"
name = "{name}"
[params]
n = {{ default = 10, min = 2, max = 50, step = 2 }}
[rules]
entry = "close > n"
exit = "close < n"
"#
        ))
        .unwrap()
    }

    #[test]
    fn selecting_a_strategy_resets_its_params_to_the_declared_defaults() {
        let mut s = state_with(vec![strategy("A"), strategy("B")]);
        s.reset_params();
        s.params.insert("n".into(), 44.0);

        s.select(1);
        assert_eq!(s.params["n"], 10.0, "tweaks leaked across strategies");
    }

    #[test]
    fn nudging_respects_the_declared_step_and_bounds() {
        let mut s = state_with(vec![strategy("A")]);
        s.reset_params();

        s.nudge_param(1.0);
        assert_eq!(s.params["n"], 12.0);

        // Far past the maximum: must clamp, not run away.
        s.nudge_param(1000.0);
        assert_eq!(s.params["n"], 50.0);

        s.nudge_param(-1000.0);
        assert_eq!(s.params["n"], 2.0);
    }

    #[test]
    fn changing_a_param_invalidates_every_derived_result() {
        // The screen shows an equity curve and a sweep side by side; if a
        // tweak did not clear both, they would describe different parameters.
        let mut s = state_with(vec![strategy("A")]);
        s.reset_params();
        s.sweep.push(crate::backtest::SweepPoint {
            params: HashMap::new(),
            score: 1.0,
            total_return_pct: 1.0,
            sharpe: 1.0,
            max_drawdown_pct: 0.0,
            trade_count: 1,
        });

        s.nudge_param(1.0);
        assert!(s.sweep.is_empty());
        assert!(s.report.is_none());
    }

    #[test]
    fn cycling_views_wraps_in_both_directions() {
        let mut s = BacktestState::default();
        assert_eq!(s.view, View::Equity);
        s.cycle_view(-1);
        assert_eq!(s.view, View::Scan);
        s.cycle_view(1);
        assert_eq!(s.view, View::Equity);
    }

    #[test]
    fn changing_the_objective_drops_results_that_were_ranked_by_the_old_one() {
        let mut s = BacktestState::default();
        s.sweep.push(crate::backtest::SweepPoint {
            params: HashMap::new(),
            score: 1.0,
            total_return_pct: 1.0,
            sharpe: 1.0,
            max_drawdown_pct: 0.0,
            trade_count: 1,
        });
        s.cycle_objective();
        assert!(s.sweep.is_empty());
    }

    #[test]
    fn selecting_out_of_range_is_ignored() {
        let mut s = state_with(vec![strategy("A")]);
        s.select(99);
        assert_eq!(s.selected, 0);
    }
}
