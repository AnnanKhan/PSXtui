//! Strategy files: the TOML schema, its validation, and loading from disk.
//!
//! The shape follows the one every mainstream platform converged on —
//! declaration, typed inputs, indicator calculations, entry and exit rules.
//! The typed inputs are the load-bearing part: because a param declares its
//! bounds and step, the tweak panel, the parameter sweep and the walk-forward
//! optimiser can all be generated from the same declaration instead of each
//! needing its own hand-written list.
//!
//! ```toml
//! name = "Golden Cross"
//!
//! [params]
//! fast = { default = 50, min = 5, max = 100, step = 5 }
//! slow = { default = 200, min = 20, max = 300, step = 10 }
//!
//! [indicators]
//! f = "sma(close, fast)"
//! s = "sma(close, slow)"
//!
//! [rules]
//! entry = "cross_above(f, s)"
//! exit  = "cross_below(f, s)"
//! ```

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::expr::{self, Expr, Series};
use crate::model::Bar;

/// Which way a strategy trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Long,
    Short,
}

/// A tunable input, with the bounds that make it sweepable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub default: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    #[serde(default)]
    pub about: String,
}

impl Param {
    /// The step used when nudging or sweeping. Defaults to something sane for
    /// the parameter's magnitude rather than a fixed 1, so a `0.5` threshold
    /// and a `200` period both behave when the user holds a key down.
    pub fn effective_step(&self) -> f64 {
        if let Some(s) = self.step
            && s > 0.0
        {
            return s;
        }
        let span = match (self.min, self.max) {
            (Some(lo), Some(hi)) if hi > lo => hi - lo,
            _ => self.default.abs().max(1.0),
        };
        if span <= 5.0 { 0.1 } else { 1.0 }
    }

    pub fn clamp(&self, v: f64) -> f64 {
        let mut v = v;
        if let Some(lo) = self.min {
            v = v.max(lo);
        }
        if let Some(hi) = self.max {
            v = v.min(hi);
        }
        v
    }

    /// The values a sweep should try, capped so an unbounded param cannot
    /// generate a combinatorial explosion.
    pub fn sweep_values(&self, max_points: usize) -> Vec<f64> {
        let (Some(lo), Some(hi)) = (self.min, self.max) else {
            return vec![self.default];
        };
        if hi <= lo {
            return vec![self.default];
        }
        let step = self.effective_step();
        let count = ((hi - lo) / step).floor() as usize + 1;
        if count <= max_points {
            return (0..count).map(|i| lo + step * i as f64).collect();
        }
        // Too fine a grid: thin it out so a sweep stays interactive no matter
        // how the file was written.
        //
        // Values stay snapped to the declared step rather than spread at an
        // arbitrary stride. An indicator period is rounded to a whole number
        // when it is used, so an unsnapped 158.7 would be reported in the
        // results table as a parameter that was never actually run.
        let stride = ((hi - lo) / (max_points - 1) as f64 / step).ceil().max(1.0) * step;
        let mut out: Vec<f64> = Vec::new();
        let mut v = lo;
        while v <= hi + step / 2.0 {
            out.push(v.min(hi));
            v += stride;
        }
        // The top of the range is worth trying even when the stride overshoots it.
        if out
            .last()
            .is_some_and(|last| (last - hi).abs() > f64::EPSILON)
        {
            out.push(hi);
        }
        out.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
        out
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawRules {
    entry: String,
    exit: Option<String>,
    /// Blocks entry while false — a regime or liquidity gate.
    filter: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawStrategy {
    name: String,
    #[serde(default)]
    about: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    direction: Direction,
    /// Symbol the file was written for. Advisory only — the UI runs whatever
    /// symbol is selected.
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    stop_loss_pct: Option<f64>,
    #[serde(default)]
    take_profit_pct: Option<f64>,
    /// Bars to hold before an exit rule is allowed to fire.
    #[serde(default)]
    min_hold_bars: Option<usize>,
    #[serde(default)]
    params: BTreeMap<String, Param>,
    #[serde(default)]
    indicators: BTreeMap<String, String>,
    rules: RawRules,
}

/// A validated, parsed strategy, ready to run.
#[derive(Debug, Clone)]
pub struct Strategy {
    pub name: String,
    pub about: String,
    pub source: String,
    pub direction: Direction,
    pub symbol: String,
    pub stop_loss_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
    pub min_hold_bars: usize,
    pub params: BTreeMap<String, Param>,
    /// Declared series in the order they must be computed. TOML tables do not
    /// preserve order, so this is topologically sorted at load time: an
    /// indicator may reference one declared before it.
    pub indicators: Vec<(String, Expr)>,
    pub entry: Expr,
    pub exit: Option<Expr>,
    pub filter: Option<Expr>,
    /// Where the file came from, for the UI to show and for reloading.
    pub path: Option<PathBuf>,
}

/// The names the language provides, which an indicator may therefore use
/// without declaring.
const BUILTIN_COLUMNS: &[&str] = &[
    "close", "open", "high", "low", "volume", "typical", "true", "false",
];

impl Strategy {
    /// Parse and validate a strategy from TOML source.
    ///
    /// Validation is deliberately thorough and happens here rather than at
    /// run time: a strategy file is user input, and the difference between a
    /// clear "unknown function `smaa`" at import and a silent no-trade result
    /// three screens later is most of the usability of the feature.
    pub fn parse(src: &str) -> Result<Self> {
        let raw: RawStrategy = toml::from_str(src).context("parsing strategy TOML")?;

        if raw.name.trim().is_empty() {
            bail!("a strategy needs a non-empty `name`");
        }

        for name in raw.params.keys() {
            if BUILTIN_COLUMNS.contains(&name.as_str()) {
                bail!("`{name}` is a built-in price column and cannot be a param name");
            }
            if raw.indicators.contains_key(name) {
                bail!("`{name}` is declared as both a param and an indicator");
            }
        }
        for (name, p) in &raw.params {
            if let (Some(lo), Some(hi)) = (p.min, p.max)
                && lo > hi
            {
                bail!("param `{name}` has min {lo} above max {hi}");
            }
            if !p.default.is_finite() {
                bail!("param `{name}` has a non-finite default");
            }
        }

        for name in raw.indicators.keys() {
            if BUILTIN_COLUMNS.contains(&name.as_str()) {
                bail!("`{name}` is a built-in price column and cannot be redefined");
            }
        }

        // Parse every expression up front so a typo is reported at import.
        let mut parsed: HashMap<String, Expr> = HashMap::new();
        for (name, src) in &raw.indicators {
            let e = expr::parse(src).map_err(|e| anyhow!("indicator `{name}` (`{src}`): {e}"))?;
            parsed.insert(name.clone(), e);
        }

        let entry = expr::parse(&raw.rules.entry)
            .map_err(|e| anyhow!("entry rule (`{}`): {e}", raw.rules.entry))?;
        let exit = raw
            .rules
            .exit
            .as_ref()
            .map(|s| expr::parse(s).map_err(|e| anyhow!("exit rule (`{s}`): {e}")))
            .transpose()?;
        let filter = raw
            .rules
            .filter
            .as_ref()
            .map(|s| expr::parse(s).map_err(|e| anyhow!("filter rule (`{s}`): {e}")))
            .transpose()?;

        // Function names and argument counts, before anything is evaluated.
        // Arity matters as much as the name: `prev(close)` is one argument
        // short, and left to evaluation time it would surface as a strategy
        // that loads and then never fires, because the sweep and the scan skip
        // anything that errors.
        let mut fns = Vec::new();
        for e in parsed
            .values()
            .chain([&entry])
            .chain(exit.iter())
            .chain(filter.iter())
        {
            expr::calls(e, &mut fns);
        }
        for (f, argc) in &fns {
            match expr::arity_of(f) {
                None => bail!(
                    "unknown function `{f}` — available: {}",
                    expr::function_names().join(", ")
                ),
                Some(want) if want != *argc => bail!(
                    "`{f}` takes {want} argument{} but was given {argc}",
                    if want == 1 { "" } else { "s" }
                ),
                Some(_) => {}
            }
        }

        let indicators = order_indicators(&parsed, &raw.params)?;

        // Rules may only reference built-ins, params and declared indicators.
        let known: Vec<String> = BUILTIN_COLUMNS
            .iter()
            .map(|s| s.to_string())
            .chain(raw.params.keys().cloned())
            .chain(raw.indicators.keys().cloned())
            .collect();
        for (label, e) in [
            ("entry", Some(&entry)),
            ("exit", exit.as_ref()),
            ("filter", filter.as_ref()),
        ] {
            let Some(e) = e else { continue };
            let mut names = Vec::new();
            expr::referenced_names(e, &mut names);
            for n in names {
                if !known.contains(&n) {
                    bail!("{label} rule references unknown name `{n}`");
                }
            }
        }

        if let Some(sl) = raw.stop_loss_pct
            && (!sl.is_finite() || sl <= 0.0)
        {
            bail!("stop_loss_pct must be a positive percentage");
        }
        if let Some(tp) = raw.take_profit_pct
            && (!tp.is_finite() || tp <= 0.0)
        {
            bail!("take_profit_pct must be a positive percentage");
        }

        if raw.rules.exit.is_none() && raw.stop_loss_pct.is_none() && raw.take_profit_pct.is_none()
        {
            bail!(
                "a strategy needs an exit rule, a stop loss or a take profit — otherwise it never closes a position"
            );
        }

        Ok(Strategy {
            name: raw.name,
            about: raw.about,
            source: raw.source,
            direction: raw.direction,
            symbol: raw.symbol,
            stop_loss_pct: raw.stop_loss_pct,
            take_profit_pct: raw.take_profit_pct,
            min_hold_bars: raw.min_hold_bars.unwrap_or(0),
            params: raw.params,
            indicators,
            entry,
            exit,
            filter,
            path: None,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let src =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut s = Self::parse(&src).with_context(|| format!("in {}", path.display()))?;
        s.path = Some(path.to_path_buf());
        Ok(s)
    }

    /// The parameter set a fresh run starts from.
    pub fn defaults(&self) -> HashMap<String, f64> {
        self.params
            .iter()
            .map(|(k, v)| (k.clone(), v.default))
            .collect()
    }

    /// Compute every declared series for these bars and params.
    ///
    /// Returns the series map the engine evaluates rules against.
    pub fn evaluate(
        &self,
        bars: &[Bar],
        params: &HashMap<String, f64>,
    ) -> Result<HashMap<String, Series>> {
        let mut ctx = expr::Context::new(bars);
        ctx.params = params.clone();

        for (name, e) in &self.indicators {
            let s = expr::eval(e, &ctx).map_err(|err| anyhow!("indicator `{name}`: {err}"))?;
            ctx.series.insert(name.clone(), s);
        }
        Ok(ctx.series)
    }

    /// Evaluate the three rule columns.
    pub fn signals(&self, bars: &[Bar], params: &HashMap<String, f64>) -> Result<Signals> {
        let series = self.evaluate(bars, params)?;
        let mut ctx = expr::Context::new(bars);
        ctx.params = params.clone();
        ctx.series = series;

        let entry = expr::eval(&self.entry, &ctx).map_err(|e| anyhow!("entry rule: {e}"))?;
        let exit = self
            .exit
            .as_ref()
            .map(|e| expr::eval(e, &ctx).map_err(|err| anyhow!("exit rule: {err}")))
            .transpose()?;
        let filter = self
            .filter
            .as_ref()
            .map(|e| expr::eval(e, &ctx).map_err(|err| anyhow!("filter rule: {err}")))
            .transpose()?;

        Ok(Signals {
            entry,
            exit,
            filter,
            series: ctx.series,
        })
    }

    /// Whether this strategy's result depends on intrabar high/low.
    ///
    /// PSX's long-run feed carries no high or low, so those columns are
    /// synthetic outside the recent true-OHLC window. Anything that reads them
    /// is reporting fills against prices that never traded, and the UI says so
    /// rather than letting the number stand unqualified.
    pub fn uses_intrabar_range(&self) -> bool {
        let mut names = Vec::new();
        let mut fns = Vec::new();
        for e in self
            .indicators
            .iter()
            .map(|(_, e)| e)
            .chain([&self.entry])
            .chain(self.exit.iter())
            .chain(self.filter.iter())
        {
            expr::referenced_names(e, &mut names);
            expr::called_functions(e, &mut fns);
        }
        names
            .iter()
            .any(|n| n == "high" || n == "low" || n == "typical")
            || fns.iter().any(|f| {
                matches!(
                    f.as_str(),
                    "atr"
                        | "cci"
                        | "williams_r"
                        | "stoch_k"
                        | "stoch_d"
                        | "adx"
                        | "di_plus"
                        | "di_minus"
                        | "donchian_upper"
                        | "donchian_lower"
                        | "donchian_mid"
                )
            })
    }
}

/// Rule columns plus the series behind them, so the chart can overlay what the
/// strategy was actually looking at.
pub struct Signals {
    pub entry: Series,
    pub exit: Option<Series>,
    pub filter: Option<Series>,
    pub series: HashMap<String, Series>,
}

/// Order indicator declarations so each is computed after anything it uses.
///
/// TOML tables are unordered, so a file that declares `spread = "f - s"` above
/// `f` and `s` must still work. A cycle is a user error and is named as one.
fn order_indicators(
    parsed: &HashMap<String, Expr>,
    params: &BTreeMap<String, Param>,
) -> Result<Vec<(String, Expr)>> {
    let mut ordered: Vec<(String, Expr)> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    let mut remaining: Vec<&String> = parsed.keys().collect();
    remaining.sort();

    while !remaining.is_empty() {
        let before = remaining.len();

        remaining.retain(|name| {
            let e = &parsed[*name];
            let mut names = Vec::new();
            expr::referenced_names(e, &mut names);

            let ready = names.iter().all(|n| {
                BUILTIN_COLUMNS.contains(&n.as_str())
                    || params.contains_key(n)
                    || done.contains(n)
                    || !parsed.contains_key(n)
            });

            if ready {
                ordered.push(((*name).clone(), e.clone()));
                done.push((*name).clone());
            }
            !ready
        });

        if remaining.len() == before {
            let stuck: Vec<&str> = remaining.iter().map(|s| s.as_str()).collect();
            bail!(
                "indicators reference each other in a cycle: {}",
                stuck.join(", ")
            );
        }
    }

    // Any name that is neither builtin, param nor declared is a typo; catch it
    // now rather than on the first bar.
    for (name, e) in &ordered {
        let mut names = Vec::new();
        expr::referenced_names(e, &mut names);
        for n in names {
            if !BUILTIN_COLUMNS.contains(&n.as_str())
                && !params.contains_key(&n)
                && !parsed.contains_key(&n)
            {
                bail!("indicator `{name}` references unknown name `{n}`");
            }
        }
    }

    Ok(ordered)
}

/// Where imported strategies live.
///
/// Beside the cache rather than in a config directory: these are data the user
/// accumulates, and keeping them next to the database means one path to
/// mention and one directory to back up.
pub fn strategy_dir() -> Result<PathBuf> {
    let db = crate::cache::default_db_path()?;
    let dir = db
        .parent()
        .ok_or_else(|| anyhow!("the data directory has no parent"))?
        .join("strategies");
    Ok(dir)
}

/// Load every `.toml` in the strategy directory.
///
/// A broken file is reported and skipped rather than failing the whole load —
/// one bad import should not hide the strategies that do work.
pub fn load_all() -> (Vec<Strategy>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();

    let dir = match strategy_dir() {
        Ok(d) => d,
        Err(e) => return (found, vec![e.to_string()]),
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // A missing directory is the normal first-run state, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (found, errors),
        Err(e) => return (found, vec![format!("reading {}: {e}", dir.display())]),
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();

    for p in paths {
        match Strategy::load(&p) {
            Ok(s) => found.push(s),
            Err(e) => errors.push(format!(
                "{}: {e:#}",
                p.file_name().unwrap_or_default().to_string_lossy()
            )),
        }
    }

    found.sort_by_key(|s| s.name.to_lowercase());
    (found, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = r#"
name = "Golden Cross"
about = "Classic trend following"

[params]
fast = { default = 50, min = 5, max = 100, step = 5 }
slow = { default = 200, min = 20, max = 300, step = 10 }

[indicators]
f = "sma(close, fast)"
s = "sma(close, slow)"

[rules]
entry = "cross_above(f, s)"
exit = "cross_below(f, s)"
"#;

    /// A call with the wrong number of arguments must be rejected on load.
    /// Left to evaluation it becomes a strategy that never fires, because the
    /// sweep and the scan skip anything that errors — the failure mode that
    /// looks most like a result.
    #[test]
    fn a_call_with_the_wrong_argument_count_is_rejected() {
        let toml = r#"
name = "Bad"
[rules]
entry = "close > prev(close)"
exit = "close < sma(close, 5)"
"#;
        let err = Strategy::parse(toml).unwrap_err().to_string();
        assert!(err.contains("prev"), "{err}");
        assert!(err.contains("2 arguments"), "{err}");

        // The same file with the count corrected loads.
        assert!(Strategy::parse(&toml.replace("prev(close)", "prev(close, 1)")).is_ok());
    }

    #[test]
    fn a_well_formed_strategy_parses() {
        let s = Strategy::parse(GOLDEN).expect("should parse");
        assert_eq!(s.name, "Golden Cross");
        assert_eq!(s.params.len(), 2);
        assert_eq!(s.indicators.len(), 2);
        assert_eq!(s.direction, Direction::Long);
    }

    #[test]
    fn defaults_come_from_the_param_table() {
        let s = Strategy::parse(GOLDEN).unwrap();
        let d = s.defaults();
        assert_eq!(d["fast"], 50.0);
        assert_eq!(d["slow"], 200.0);
    }

    #[test]
    fn indicators_are_ordered_by_dependency_not_by_file_order() {
        // `spread` is declared alphabetically before `f` and `s` but depends on
        // both, so it must still be computed last.
        let src = r#"
name = "Ordered"
[indicators]
spread = "f - s"
f = "sma(close, 5)"
s = "sma(close, 10)"
[rules]
entry = "spread > 0"
exit = "spread < 0"
"#;
        let s = Strategy::parse(src).unwrap();
        let order: Vec<&str> = s.indicators.iter().map(|(n, _)| n.as_str()).collect();
        let spread_at = order.iter().position(|n| *n == "spread").unwrap();
        let f_at = order.iter().position(|n| *n == "f").unwrap();
        let s_at = order.iter().position(|n| *n == "s").unwrap();
        assert!(spread_at > f_at && spread_at > s_at, "got {order:?}");
    }

    #[test]
    fn a_dependency_cycle_is_reported() {
        let src = r#"
name = "Cyclic"
[indicators]
a = "b + 1"
b = "a + 1"
[rules]
entry = "a > 0"
exit = "a < 0"
"#;
        let e = Strategy::parse(src).unwrap_err().to_string();
        assert!(e.contains("cycle"), "{e}");
    }

    #[test]
    fn a_strategy_with_no_way_out_is_rejected() {
        let src = r#"
name = "Roach Motel"
[rules]
entry = "close > 0"
"#;
        let e = Strategy::parse(src).unwrap_err().to_string();
        assert!(e.contains("never closes"), "{e}");
    }

    #[test]
    fn a_stop_loss_alone_is_a_valid_exit() {
        let src = r#"
name = "Stopped"
stop_loss_pct = 5
[rules]
entry = "close > 0"
"#;
        assert!(Strategy::parse(src).is_ok());
    }

    #[test]
    fn typos_in_function_names_are_caught_at_import() {
        let src = r#"
name = "Typo"
[indicators]
f = "smaa(close, 5)"
[rules]
entry = "f > 0"
exit = "f < 0"
"#;
        let e = Strategy::parse(src).unwrap_err().to_string();
        assert!(e.contains("smaa"), "{e}");
    }

    #[test]
    fn rules_referencing_undeclared_names_are_caught_at_import() {
        let src = r#"
name = "Ghost"
[indicators]
f = "sma(close, 5)"
[rules]
entry = "g > 0"
exit = "f < 0"
"#;
        let e = Strategy::parse(src).unwrap_err().to_string();
        assert!(e.contains('g'), "{e}");
    }

    #[test]
    fn a_param_cannot_shadow_a_price_column() {
        let src = r#"
name = "Shadow"
[params]
close = { default = 5 }
[rules]
entry = "close > 0"
exit = "close < 0"
"#;
        let e = Strategy::parse(src).unwrap_err().to_string();
        assert!(e.contains("built-in"), "{e}");
    }

    #[test]
    fn inverted_param_bounds_are_rejected() {
        let src = r#"
name = "Inverted"
[params]
n = { default = 5, min = 100, max = 10 }
[rules]
entry = "close > n"
exit = "close < n"
"#;
        let e = Strategy::parse(src).unwrap_err().to_string();
        assert!(e.contains("min"), "{e}");
    }

    #[test]
    fn a_negative_stop_loss_is_rejected() {
        let src = r#"
name = "Backwards"
stop_loss_pct = -5
[rules]
entry = "close > 0"
"#;
        assert!(Strategy::parse(src).is_err());
    }

    #[test]
    fn sweep_values_walk_the_declared_range() {
        let p = Param {
            default: 10.0,
            min: Some(10.0),
            max: Some(14.0),
            step: Some(2.0),
            about: String::new(),
        };
        assert_eq!(p.sweep_values(50), vec![10.0, 12.0, 14.0]);
    }

    #[test]
    fn sweep_values_stay_bounded_on_a_fine_grid() {
        // A 1-step sweep from 1 to 10_000 must not try ten thousand runs.
        let p = Param {
            default: 1.0,
            min: Some(1.0),
            max: Some(10_000.0),
            step: Some(1.0),
            about: String::new(),
        };
        let v = p.sweep_values(12);
        assert!(v.len() <= 13, "{} points", v.len());
        assert_eq!(v[0], 1.0);
        assert_eq!(*v.last().unwrap(), 10_000.0);
    }

    #[test]
    fn a_thinned_sweep_stays_on_the_declared_step() {
        // 50..300 by 10 is 26 points; thinned to ~12 it must still yield whole
        // multiples of 10, because a period of 158.7 is reported in the results
        // table but rounded to 159 when actually run.
        let p = Param {
            default: 200.0,
            min: Some(50.0),
            max: Some(300.0),
            step: Some(10.0),
            about: String::new(),
        };
        for v in p.sweep_values(12) {
            assert!(
                ((v - 50.0) / 10.0).fract().abs() < 1e-9,
                "{v} is not a multiple of the declared step"
            );
        }
    }

    #[test]
    fn a_thinned_sweep_still_reaches_both_ends() {
        let p = Param {
            default: 10.0,
            min: Some(3.0),
            max: Some(97.0),
            step: Some(2.0),
            about: String::new(),
        };
        let v = p.sweep_values(8);
        assert_eq!(v[0], 3.0);
        assert_eq!(*v.last().unwrap(), 97.0);
    }

    #[test]
    fn an_unbounded_param_is_not_swept() {
        let p = Param {
            default: 7.0,
            min: None,
            max: None,
            step: None,
            about: String::new(),
        };
        assert_eq!(p.sweep_values(20), vec![7.0]);
    }

    #[test]
    fn params_clamp_to_their_bounds() {
        let p = Param {
            default: 10.0,
            min: Some(5.0),
            max: Some(20.0),
            step: None,
            about: String::new(),
        };
        assert_eq!(p.clamp(1.0), 5.0);
        assert_eq!(p.clamp(99.0), 20.0);
        assert_eq!(p.clamp(12.0), 12.0);
    }

    #[test]
    fn intrabar_dependence_is_detected_through_indicators() {
        let src = r#"
name = "ATR user"
[indicators]
a = "atr(14)"
[rules]
entry = "close > a"
exit = "close < a"
"#;
        assert!(Strategy::parse(src).unwrap().uses_intrabar_range());
        // Whereas a purely close-based one is safe across the full history.
        assert!(!Strategy::parse(GOLDEN).unwrap().uses_intrabar_range());
    }

    #[test]
    fn direct_high_low_references_count_as_intrabar() {
        let src = r#"
name = "Range"
[rules]
entry = "close > high"
exit = "close < low"
"#;
        assert!(Strategy::parse(src).unwrap().uses_intrabar_range());
    }

    #[test]
    fn signals_align_with_the_bar_series() {
        use crate::model::Bar;
        let bars: Vec<Bar> = (0..300)
            .map(|i| {
                let c = 100.0 + (i as f64 / 10.0).sin() * 10.0;
                Bar {
                    ts: i as i64 * 86_400,
                    open: c,
                    high: c,
                    low: c,
                    close: c,
                    volume: 1000.0,
                }
            })
            .collect();
        let s = Strategy::parse(GOLDEN).unwrap();
        let sig = s.signals(&bars, &s.defaults()).unwrap();
        assert_eq!(sig.entry.len(), bars.len());
        assert_eq!(sig.exit.as_ref().unwrap().len(), bars.len());
    }
}
