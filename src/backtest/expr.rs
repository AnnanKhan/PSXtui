//! The tiny expression language strategy files are written in.
//!
//! A strategy declares named series (`f = "sma(close, fast)"`) and boolean
//! rules over them (`entry = "cross_above(f, s)"`). Both are parsed by this
//! module; the difference is only what the caller does with the result.
//!
//! Everything is **series-valued**. `sma(close, 20)` is a series, `close` is a
//! series, `f > s` is a series of booleans, and a scalar like `30` broadcasts
//! against whichever series it meets. Evaluating a whole strategy therefore
//! produces one aligned column per rule, and the engine reads bar `i` out of
//! it — it never asks the expression layer a question about a single bar,
//! which is what keeps the look-ahead guarantee in one place (the engine)
//! instead of spread across this file.
//!
//! Why hand-rolled rather than a scripting engine: the semantics that matter
//! here are the series ones — alignment, `None` during warm-up, and
//! `cross_above` needing bar `i-1`. A general-purpose evaluator gives none of
//! those and would need all of them rebuilt on top anyway.

use std::collections::HashMap;
use std::fmt;

use crate::analysis::indicators;
use crate::model::Bar;

/// A column of values aligned 1:1 with the bar series.
///
/// `None` marks "not defined here" — the warm-up window of an indicator, or a
/// comparison against one. It propagates through every operator, so a rule
/// that depends on a 200-day average is simply false for the first 199 bars
/// rather than accidentally true.
pub type Series = Vec<Option<f64>>;

// Booleans travel as 1.0/0.0 inside a `Series` so that one representation
// covers both, and `and`/`or`/`>` compose with arithmetic without a second
// value type and the conversions that come with it.
const TRUE: f64 = 1.0;
const FALSE: f64 = 0.0;

fn truthy(v: f64) -> bool {
    v != 0.0
}

fn flag(b: bool) -> Option<f64> {
    Some(if b { TRUE } else { FALSE })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Ident(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl BinOp {
    fn precedence(self) -> u8 {
        match self {
            BinOp::Or => 1,
            BinOp::And => 2,
            BinOp::Eq | BinOp::Ne => 3,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 4,
            BinOp::Add | BinOp::Sub => 5,
            BinOp::Mul | BinOp::Div => 6,
        }
    }
}

// --- errors ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ExprError {
    pub message: String,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExprError {}

fn err<T>(message: impl Into<String>) -> Result<T, ExprError> {
    Err(ExprError {
        message: message.into(),
    })
}

// --- lexing ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(BinOp),
    Not,
    LParen,
    RParen,
    Comma,
}

fn lex(src: &str) -> Result<Vec<Tok>, ExprError> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        if c.is_ascii_digit() || (c == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
        {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            match text.parse::<f64>() {
                Ok(n) => toks.push(Tok::Num(n)),
                Err(_) => return err(format!("`{text}` is not a number")),
            }
            continue;
        }

        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // Word-shaped operators, so `a and b` reads the way the rest of a
            // strategy file does. `&&` is accepted below for people arriving
            // from Pine or C-like syntax.
            match word.as_str() {
                "and" => toks.push(Tok::Op(BinOp::And)),
                "or" => toks.push(Tok::Op(BinOp::Or)),
                "not" => toks.push(Tok::Not),
                _ => toks.push(Tok::Ident(word)),
            }
            continue;
        }

        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        let matched = match two.as_str() {
            ">=" => Some(Tok::Op(BinOp::Ge)),
            "<=" => Some(Tok::Op(BinOp::Le)),
            "==" => Some(Tok::Op(BinOp::Eq)),
            "!=" => Some(Tok::Op(BinOp::Ne)),
            "&&" => Some(Tok::Op(BinOp::And)),
            "||" => Some(Tok::Op(BinOp::Or)),
            _ => None,
        };
        if let Some(t) = matched {
            toks.push(t);
            i += 2;
            continue;
        }

        let one = match c {
            '+' => Tok::Op(BinOp::Add),
            '-' => Tok::Op(BinOp::Sub),
            '*' => Tok::Op(BinOp::Mul),
            '/' => Tok::Op(BinOp::Div),
            '>' => Tok::Op(BinOp::Gt),
            '<' => Tok::Op(BinOp::Lt),
            '=' => Tok::Op(BinOp::Eq),
            '!' => Tok::Not,
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            ',' => Tok::Comma,
            _ => return err(format!("unexpected character `{c}`")),
        };
        toks.push(one);
        i += 1;
    }

    Ok(toks)
}

// --- parsing --------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// Precedence climbing: parse a prefix, then absorb every operator at or
    /// above `min_prec` into it.
    fn expr(&mut self, min_prec: u8) -> Result<Expr, ExprError> {
        let mut lhs = self.prefix()?;

        while let Some(Tok::Op(op)) = self.peek().cloned() {
            let prec = op.precedence();
            if prec < min_prec {
                break;
            }
            self.pos += 1;
            // All operators here are left-associative, so the right side binds
            // only tighter-than-this.
            let rhs = self.expr(prec + 1)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }

        Ok(lhs)
    }

    fn prefix(&mut self) -> Result<Expr, ExprError> {
        match self.next() {
            Some(Tok::Num(n)) => Ok(Expr::Num(n)),
            Some(Tok::Not) => Ok(Expr::Unary(UnOp::Not, Box::new(self.prefix()?))),
            Some(Tok::Op(BinOp::Sub)) => Ok(Expr::Unary(UnOp::Neg, Box::new(self.prefix()?))),
            Some(Tok::LParen) => {
                let inner = self.expr(0)?;
                match self.next() {
                    Some(Tok::RParen) => Ok(inner),
                    _ => err("missing `)`"),
                }
            }
            Some(Tok::Ident(name)) => {
                if self.peek() == Some(&Tok::LParen) {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if self.peek() == Some(&Tok::RParen) {
                        self.pos += 1;
                        return Ok(Expr::Call(name, args));
                    }
                    loop {
                        args.push(self.expr(0)?);
                        match self.next() {
                            Some(Tok::Comma) => continue,
                            Some(Tok::RParen) => break,
                            _ => return err(format!("missing `)` after `{name}(`")),
                        }
                    }
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Ident(name))
                }
            }
            Some(t) => err(format!("unexpected {t:?}")),
            None => err("unexpected end of expression"),
        }
    }
}

/// Parse an expression. The result is reusable across bars and parameter
/// sweeps — parsing happens once, evaluation many times.
pub fn parse(src: &str) -> Result<Expr, ExprError> {
    if src.trim().is_empty() {
        return err("empty expression");
    }
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0 };
    let e = p.expr(0)?;
    if p.pos != p.toks.len() {
        return err("trailing input after a complete expression");
    }
    Ok(e)
}

// --- evaluation -----------------------------------------------------------

/// Everything an expression can refer to by name.
pub struct Context<'a> {
    pub bars: &'a [Bar],
    /// Series already computed — earlier `[indicators]` entries, in the order
    /// the file declared them.
    pub series: HashMap<String, Series>,
    /// Scalar `[params]` values, broadcast to every bar when referenced.
    pub params: HashMap<String, f64>,
}

impl<'a> Context<'a> {
    pub fn new(bars: &'a [Bar]) -> Self {
        Self {
            bars,
            series: HashMap::new(),
            params: HashMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.bars.len()
    }

    fn constant(&self, v: f64) -> Series {
        vec![Some(v); self.len()]
    }

    fn price(&self, field: fn(&Bar) -> f64) -> Series {
        self.bars.iter().map(|b| Some(field(b))).collect()
    }
}

/// A parameter used where a whole number is required (an indicator period).
///
/// Periods come from `[params]` as `f64`, so this is where a swept value lands
/// back on solid ground. A period of zero or less would make every indicator
/// return nothing at all, silently; better to say so.
fn period(s: &Series, name: &str) -> Result<usize, ExprError> {
    let v = s.iter().find_map(|x| *x).ok_or_else(|| ExprError {
        message: format!("`{name}` needs a period, but got an empty series"),
    })?;
    if !v.is_finite() || v < 1.0 {
        return err(format!("`{name}` needs a period of at least 1, got {v}"));
    }
    Ok(v.round() as usize)
}

fn scalar(s: &Series, name: &str) -> Result<f64, ExprError> {
    s.iter().find_map(|x| *x).ok_or_else(|| ExprError {
        message: format!("`{name}` expects a constant argument"),
    })
}

/// Widen an indicator output that is shorter than the bar series.
///
/// Every indicator in `analysis` already returns a full-length column, so this
/// is belt and braces — but a length mismatch here would silently shift a
/// signal onto the wrong bar, which is precisely the class of bug this whole
/// module exists to make impossible.
fn aligned(mut s: Series, n: usize) -> Series {
    if s.len() < n {
        let mut padded = vec![None; n - s.len()];
        padded.append(&mut s);
        return padded;
    }
    s.truncate(n);
    s
}

pub fn eval(expr: &Expr, ctx: &Context) -> Result<Series, ExprError> {
    let n = ctx.len();

    match expr {
        Expr::Num(v) => Ok(ctx.constant(*v)),

        Expr::Ident(name) => {
            // Params shadow nothing: a strategy that names a param `close`
            // would be too confusing to allow, so built-ins win and the
            // collision is reported when the strategy is validated.
            match name.as_str() {
                "close" => Ok(ctx.price(|b| b.close)),
                "open" => Ok(ctx.price(|b| b.open)),
                "high" => Ok(ctx.price(|b| b.high)),
                "low" => Ok(ctx.price(|b| b.low)),
                "volume" => Ok(ctx.price(|b| b.volume)),
                "typical" => Ok(ctx.price(|b| b.typical())),
                "true" => Ok(ctx.constant(TRUE)),
                "false" => Ok(ctx.constant(FALSE)),
                _ => {
                    if let Some(s) = ctx.series.get(name) {
                        Ok(aligned(s.clone(), n))
                    } else if let Some(v) = ctx.params.get(name) {
                        Ok(ctx.constant(*v))
                    } else {
                        err(format!("unknown name `{name}`"))
                    }
                }
            }
        }

        Expr::Unary(op, inner) => {
            let s = eval(inner, ctx)?;
            Ok(s.into_iter()
                .map(|v| match (op, v) {
                    (_, None) => None,
                    (UnOp::Neg, Some(x)) => Some(-x),
                    (UnOp::Not, Some(x)) => flag(!truthy(x)),
                })
                .collect())
        }

        Expr::Binary(op, l, r) => {
            let a = eval(l, ctx)?;
            let b = eval(r, ctx)?;
            Ok(binary(*op, &a, &b))
        }

        Expr::Call(name, args) => call(name, args, ctx),
    }
}

fn binary(op: BinOp, a: &Series, b: &Series) -> Series {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let (Some(x), Some(y)) = (x, y) else {
                return None;
            };
            let (x, y) = (*x, *y);
            match op {
                BinOp::Add => Some(x + y),
                BinOp::Sub => Some(x - y),
                BinOp::Mul => Some(x * y),
                // A zero divisor is routine in market data (a flat range, a
                // zero-volume day). `None` keeps it out of the signal instead
                // of seeding an infinity that compares true against anything.
                BinOp::Div => {
                    if y == 0.0 {
                        None
                    } else {
                        Some(x / y)
                    }
                }
                BinOp::Lt => flag(x < y),
                BinOp::Le => flag(x <= y),
                BinOp::Gt => flag(x > y),
                BinOp::Ge => flag(x >= y),
                BinOp::Eq => flag(x == y),
                BinOp::Ne => flag(x != y),
                BinOp::And => flag(truthy(x) && truthy(y)),
                BinOp::Or => flag(truthy(x) || truthy(y)),
            }
        })
        .collect()
}

/// The functions a strategy file may call.
///
/// Kept deliberately small and total: every one of these maps onto something
/// already in `analysis::indicators`, so a strategy cannot reach a code path
/// the rest of the app does not already exercise.
pub const FUNCTIONS: &[&str] = &[
    "sma",
    "ema",
    "rsi",
    "atr",
    "obv",
    "cci",
    "williams_r",
    "macd",
    "macd_signal",
    "macd_hist",
    "bb_upper",
    "bb_mid",
    "bb_lower",
    "stoch_k",
    "stoch_d",
    "adx",
    "di_plus",
    "di_minus",
    "donchian_upper",
    "donchian_lower",
    "donchian_mid",
    "highest",
    "lowest",
    "change",
    "pct_change",
    "prev",
    "cross_above",
    "cross_below",
    "abs",
    "min",
    "max",
    "hammer",
    "bullish_engulfing",
    "morning_star",
];

fn call(name: &str, args: &[Expr], ctx: &Context) -> Result<Series, ExprError> {
    let n = ctx.len();
    let evaluated: Vec<Series> = args
        .iter()
        .map(|a| eval(a, ctx))
        .collect::<Result<_, _>>()?;

    let arity = |want: usize| -> Result<(), ExprError> {
        if evaluated.len() == want {
            Ok(())
        } else {
            err(format!(
                "`{name}` takes {want} argument(s), got {}",
                evaluated.len()
            ))
        }
    };

    // Indicators that need the full OHLC bar rather than one column.
    match name {
        "atr" => {
            arity(1)?;
            let p = period(&evaluated[0], name)?;
            return Ok(aligned(indicators::atr(ctx.bars, p), n));
        }
        "obv" => {
            arity(0)?;
            return Ok(aligned(indicators::obv(ctx.bars), n));
        }
        "cci" => {
            arity(1)?;
            let p = period(&evaluated[0], name)?;
            return Ok(aligned(indicators::cci(ctx.bars, p), n));
        }
        "williams_r" => {
            arity(1)?;
            let p = period(&evaluated[0], name)?;
            return Ok(aligned(indicators::williams_r(ctx.bars, p), n));
        }
        "stoch_k" | "stoch_d" => {
            arity(2)?;
            let k = period(&evaluated[0], name)?;
            let d = period(&evaluated[1], name)?;
            let out = indicators::stochastic(ctx.bars, k, d);
            let s = if name == "stoch_k" { out.k } else { out.d };
            return Ok(aligned(s, n));
        }
        "adx" | "di_plus" | "di_minus" => {
            arity(1)?;
            let p = period(&evaluated[0], name)?;
            let out = indicators::adx(ctx.bars, p);
            let s = match name {
                "adx" => out.adx,
                "di_plus" => out.plus_di,
                _ => out.minus_di,
            };
            return Ok(aligned(s, n));
        }
        // Candlestick patterns are shape statements about whole bars, so like
        // `obv` they take no source column.
        "hammer" | "bullish_engulfing" | "morning_star" => {
            arity(0)?;
            let s = match name {
                "hammer" => indicators::hammer(ctx.bars),
                "bullish_engulfing" => indicators::bullish_engulfing(ctx.bars),
                _ => indicators::morning_star(ctx.bars),
            };
            return Ok(aligned(s, n));
        }
        "donchian_upper" | "donchian_lower" | "donchian_mid" => {
            arity(1)?;
            let p = period(&evaluated[0], name)?;
            let out = indicators::donchian(ctx.bars, p);
            let s = match name {
                "donchian_upper" => out.upper,
                "donchian_lower" => out.lower,
                _ => out.middle,
            };
            return Ok(aligned(s, n));
        }
        _ => {}
    }

    // Everything else works on a source column.
    let src = |i: usize| -> Vec<f64> {
        // A source series is a price column in practice, so `None` only shows
        // up if someone feeds one indicator into another during its warm-up.
        // Carrying the last known value keeps the downstream indicator's own
        // warm-up honest rather than truncating the series.
        let mut last = 0.0;
        evaluated[i]
            .iter()
            .map(|v| {
                if let Some(x) = *v {
                    last = x;
                }
                last
            })
            .collect()
    };

    match name {
        "sma" | "ema" | "rsi" => {
            arity(2)?;
            let p = period(&evaluated[1], name)?;
            let values = src(0);
            let out = match name {
                "sma" => indicators::sma(&values, p),
                "ema" => indicators::ema(&values, p),
                _ => indicators::rsi(&values, p),
            };
            Ok(aligned(out, n))
        }

        "macd" | "macd_signal" | "macd_hist" => {
            arity(4)?;
            let fast = period(&evaluated[1], name)?;
            let slow = period(&evaluated[2], name)?;
            let signal = period(&evaluated[3], name)?;
            let out = indicators::macd(&src(0), fast, slow, signal);
            let s = match name {
                "macd" => out.macd,
                "macd_signal" => out.signal,
                _ => out.histogram,
            };
            Ok(aligned(s, n))
        }

        "bb_upper" | "bb_mid" | "bb_lower" => {
            arity(3)?;
            let p = period(&evaluated[1], name)?;
            let sd = scalar(&evaluated[2], name)?;
            let out = indicators::bollinger(&src(0), p, sd);
            let s = match name {
                "bb_upper" => out.upper,
                "bb_mid" => out.middle,
                _ => out.lower,
            };
            Ok(aligned(s, n))
        }

        "highest" | "lowest" => {
            arity(2)?;
            let p = period(&evaluated[1], name)?;
            let values = &evaluated[0];
            Ok((0..n)
                .map(|i| {
                    if i + 1 < p {
                        return None;
                    }
                    let window = &values[i + 1 - p..=i];
                    let mut acc: Option<f64> = None;
                    for v in window.iter().flatten() {
                        acc = Some(match acc {
                            None => *v,
                            Some(a) if name == "highest" => a.max(*v),
                            Some(a) => a.min(*v),
                        });
                    }
                    acc
                })
                .collect())
        }

        "prev" => {
            arity(2)?;
            let back = period(&evaluated[1], name)?;
            let values = &evaluated[0];
            Ok((0..n)
                .map(|i| if i >= back { values[i - back] } else { None })
                .collect())
        }

        "change" | "pct_change" => {
            arity(2)?;
            let back = period(&evaluated[1], name)?;
            let values = &evaluated[0];
            Ok((0..n)
                .map(|i| {
                    if i < back {
                        return None;
                    }
                    let (now, then) = (values[i]?, values[i - back]?);
                    if name == "change" {
                        Some(now - then)
                    } else if then == 0.0 {
                        None
                    } else {
                        Some((now - then) / then * 100.0)
                    }
                })
                .collect())
        }

        // The reason this module exists rather than a generic evaluator: a
        // cross is a statement about two adjacent bars, and it has to be
        // computed where the alignment is known to be right.
        "cross_above" | "cross_below" => {
            arity(2)?;
            let (a, b) = (&evaluated[0], &evaluated[1]);
            Ok((0..n)
                .map(|i| {
                    if i == 0 {
                        return flag(false);
                    }
                    let (a1, b1) = (a[i]?, b[i]?);
                    let (a0, b0) = (a[i - 1]?, b[i - 1]?);
                    flag(if name == "cross_above" {
                        a0 <= b0 && a1 > b1
                    } else {
                        a0 >= b0 && a1 < b1
                    })
                })
                .collect())
        }

        "abs" => {
            arity(1)?;
            Ok(evaluated[0].iter().map(|v| v.map(f64::abs)).collect())
        }

        "min" | "max" => {
            arity(2)?;
            let (a, b) = (&evaluated[0], &evaluated[1]);
            Ok(a.iter()
                .zip(b.iter())
                .map(|(x, y)| match (x, y) {
                    (Some(x), Some(y)) => Some(if name == "min" { x.min(*y) } else { x.max(*y) }),
                    _ => None,
                })
                .collect())
        }

        _ => err(format!("unknown function `{name}`")),
    }
}

/// Every name an expression refers to, for validating a strategy before it is
/// ever run against bars.
pub fn referenced_names(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Num(_) => {}
        Expr::Ident(n) => out.push(n.clone()),
        Expr::Unary(_, e) => referenced_names(e, out),
        Expr::Binary(_, l, r) => {
            referenced_names(l, out);
            referenced_names(r, out);
        }
        Expr::Call(_, args) => args.iter().for_each(|a| referenced_names(a, out)),
    }
}

/// Every function an expression calls, for the same reason.
pub fn called_functions(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Num(_) | Expr::Ident(_) => {}
        Expr::Unary(_, e) => called_functions(e, out),
        Expr::Binary(_, l, r) => {
            called_functions(l, out);
            called_functions(r, out);
        }
        Expr::Call(name, args) => {
            out.push(name.clone());
            args.iter().for_each(|a| called_functions(a, out));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bars(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, c)| Bar {
                ts: i as i64 * 86_400,
                open: *c,
                high: *c,
                low: *c,
                close: *c,
                volume: 1_000.0,
            })
            .collect()
    }

    fn run(src: &str, closes: &[f64]) -> Series {
        let b = bars(closes);
        let ctx = Context::new(&b);
        eval(&parse(src).expect("parse"), &ctx).expect("eval")
    }

    #[test]
    fn arithmetic_precedence_matches_the_usual_rules() {
        assert_eq!(run("2 + 3 * 4", &[1.0])[0], Some(14.0));
        assert_eq!(run("(2 + 3) * 4", &[1.0])[0], Some(20.0));
        assert_eq!(run("10 - 2 - 3", &[1.0])[0], Some(5.0));
    }

    #[test]
    fn comparison_binds_tighter_than_and() {
        // Without correct precedence this parses as `close > (5 and close) < 9`
        // and quietly produces nonsense rather than an error.
        assert_eq!(run("close > 5 and close < 9", &[7.0])[0], Some(TRUE));
        assert_eq!(run("close > 5 and close < 9", &[12.0])[0], Some(FALSE));
    }

    #[test]
    fn unary_not_and_negation() {
        assert_eq!(run("not (close > 5)", &[3.0])[0], Some(TRUE));
        assert_eq!(run("-close", &[3.0])[0], Some(-3.0));
    }

    #[test]
    fn price_columns_resolve() {
        let b = bars(&[10.0, 11.0]);
        let ctx = Context::new(&b);
        let out = eval(&parse("close").unwrap(), &ctx).unwrap();
        assert_eq!(out, vec![Some(10.0), Some(11.0)]);
    }

    #[test]
    fn warmup_propagates_as_none_not_as_false_signal() {
        // A 3-period average is undefined for the first two bars; a rule built
        // on it must be undefined too, never accidentally true.
        let out = run("sma(close, 3) > 0", &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(out[0], None);
        assert_eq!(out[1], None);
        assert_eq!(out[2], Some(TRUE));
    }

    #[test]
    fn division_by_zero_is_none_not_infinity() {
        let out = run("close / (close - close)", &[5.0]);
        assert_eq!(out[0], None);
    }

    #[test]
    fn cross_above_fires_only_on_the_crossing_bar() {
        // Two series that meet once: 1,2,3,4 against a constant 2.5.
        let out = run("cross_above(close, 2.5)", &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(out[0], Some(FALSE)); // no previous bar to cross from
        assert_eq!(out[1], Some(FALSE));
        assert_eq!(out[2], Some(TRUE)); // 2 -> 3 crosses 2.5
        assert_eq!(out[3], Some(FALSE)); // already above; not a fresh cross
    }

    #[test]
    fn cross_below_is_the_mirror() {
        let out = run("cross_below(close, 2.5)", &[4.0, 3.0, 2.0, 1.0]);
        assert_eq!(out[2], Some(TRUE));
        assert_eq!(out[3], Some(FALSE));
    }

    #[test]
    fn touching_without_passing_is_not_a_cross() {
        // 2.0 -> 2.5 -> 2.0 never gets strictly above, so nothing fires.
        let out = run("cross_above(close, 2.5)", &[2.0, 2.5, 2.0]);
        assert!(out.iter().all(|v| *v == Some(FALSE)));
    }

    #[test]
    fn params_broadcast_and_can_be_swept() {
        let b = bars(&[1.0, 2.0, 3.0, 4.0]);
        let mut ctx = Context::new(&b);
        ctx.params.insert("n".into(), 2.0);
        let out = eval(&parse("sma(close, n)").unwrap(), &ctx).unwrap();
        assert_eq!(out[1], Some(1.5));
    }

    #[test]
    fn declared_series_are_referenceable_by_name() {
        let b = bars(&[1.0, 2.0, 3.0, 4.0]);
        let mut ctx = Context::new(&b);
        ctx.series
            .insert("f".into(), vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)]);
        let out = eval(&parse("f > 2").unwrap(), &ctx).unwrap();
        assert_eq!(out[2], Some(TRUE));
    }

    #[test]
    fn highest_and_lowest_respect_the_window() {
        let out = run("highest(close, 3)", &[1.0, 5.0, 2.0, 3.0]);
        assert_eq!(out[0], None);
        assert_eq!(out[1], None);
        assert_eq!(out[2], Some(5.0));
        assert_eq!(out[3], Some(5.0));
    }

    #[test]
    fn prev_looks_backward_only() {
        let out = run("prev(close, 1)", &[1.0, 2.0, 3.0]);
        assert_eq!(out[0], None);
        assert_eq!(out[1], Some(1.0));
        assert_eq!(out[2], Some(2.0));
    }

    #[test]
    fn pct_change_is_a_percentage() {
        let out = run("pct_change(close, 1)", &[100.0, 110.0]);
        assert_eq!(out[1], Some(10.0));
    }

    #[test]
    fn unknown_name_is_an_error_not_a_zero() {
        let b = bars(&[1.0]);
        let ctx = Context::new(&b);
        let e = eval(&parse("wibble > 1").unwrap(), &ctx).unwrap_err();
        assert!(e.message.contains("wibble"), "{}", e.message);
    }

    #[test]
    fn unknown_function_is_an_error() {
        let b = bars(&[1.0]);
        let ctx = Context::new(&b);
        let e = eval(&parse("frobnicate(close, 2)").unwrap(), &ctx).unwrap_err();
        assert!(e.message.contains("frobnicate"), "{}", e.message);
    }

    #[test]
    fn wrong_arity_is_reported_with_the_expected_count() {
        let b = bars(&[1.0, 2.0]);
        let ctx = Context::new(&b);
        let e = eval(&parse("sma(close)").unwrap(), &ctx).unwrap_err();
        assert!(e.message.contains("2 argument"), "{}", e.message);
    }

    #[test]
    fn a_zero_period_is_rejected_rather_than_yielding_an_empty_column() {
        let b = bars(&[1.0, 2.0]);
        let ctx = Context::new(&b);
        let e = eval(&parse("sma(close, 0)").unwrap(), &ctx).unwrap_err();
        assert!(e.message.contains("at least 1"), "{}", e.message);
    }

    #[test]
    fn syntax_errors_are_caught_at_parse_time() {
        assert!(parse("close >").is_err());
        assert!(parse("(close > 2").is_err());
        assert!(parse("close 2").is_err());
        assert!(parse("").is_err());
        assert!(parse("close @ 2").is_err());
    }

    #[test]
    fn c_style_operators_are_accepted_too() {
        assert_eq!(run("close > 1 && close < 3", &[2.0])[0], Some(TRUE));
        assert_eq!(run("close < 1 || close > 1", &[2.0])[0], Some(TRUE));
    }

    #[test]
    fn every_advertised_function_parses_and_evaluates() {
        // Guards against FUNCTIONS drifting out of step with `call`, which
        // would otherwise only surface as a runtime error in a user's file.
        let closes: Vec<f64> = (1..=60)
            .map(|i| 100.0 + (i as f64 * 0.7).sin() * 5.0)
            .collect();
        let b = bars(&closes);
        let ctx = Context::new(&b);
        for f in FUNCTIONS {
            let src = match *f {
                "obv" | "hammer" | "bullish_engulfing" | "morning_star" => format!("{f}()"),
                "macd" | "macd_signal" | "macd_hist" => format!("{f}(close, 12, 26, 9)"),
                "bb_upper" | "bb_mid" | "bb_lower" => format!("{f}(close, 20, 2)"),
                "stoch_k" | "stoch_d" => format!("{f}(14, 3)"),
                "atr" | "cci" | "williams_r" | "adx" | "di_plus" | "di_minus"
                | "donchian_upper" | "donchian_lower" | "donchian_mid" => format!("{f}(14)"),
                "cross_above" | "cross_below" | "min" | "max" => format!("{f}(close, 100)"),
                "abs" => "abs(close)".to_string(),
                _ => format!("{f}(close, 5)"),
            };
            let e = parse(&src).unwrap_or_else(|e| panic!("{f}: parse failed: {e}"));
            let out = eval(&e, &ctx).unwrap_or_else(|e| panic!("{f}: eval failed: {e}"));
            assert_eq!(out.len(), b.len(), "{f} returned a misaligned column");
        }
    }

    #[test]
    fn referenced_names_finds_everything() {
        let e = parse("cross_above(f, s) and close > x").unwrap();
        let mut names = Vec::new();
        referenced_names(&e, &mut names);
        assert!(names.contains(&"f".to_string()));
        assert!(names.contains(&"s".to_string()));
        assert!(names.contains(&"x".to_string()));
    }
}
