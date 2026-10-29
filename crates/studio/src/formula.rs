//! Calculated columns: arithmetic over a result row's other columns, `[Revenue] - [Cost]` or
//! `round([sum(margin)] / [sum(grand_total)] * 100, 1)`, in exact decimals. A reference is a
//! column's name or label in brackets; `+ - * /`, parentheses and `round(x[, places])`, `abs`,
//! `min`, `max` and `coalesce` are understood. Anything missing or a division by zero is empty.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Map, Value};
use std::str::FromStr;

/// Formulas a query may have; their text may be this long.
pub const MAX_FORMULAS: usize = 10;
const MAX_TEXT: usize = 500;
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum Formula {
    Num(Decimal),
    /// The column's name.
    Col(String),
    Neg(Box<Formula>),
    Op(char, Box<Formula>, Box<Formula>),
    Call(Func, Vec<Formula>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Func {
    Round,
    Abs,
    Min,
    Max,
    Coalesce,
}

impl Formula {
    /// Parses `text`; `column` turns a bracketed reference into a column name (or says why not).
    pub fn parse(text: &str, column: &dyn Fn(&str) -> Result<String, String>) -> Result<Formula, String> {
        if text.len() > MAX_TEXT {
            return Err(format!("a formula is at most {MAX_TEXT} characters"));
        }
        let mut p = Parser { s: text.as_bytes(), at: 0, depth: 0, column };
        let f = p.expr()?;
        p.space();
        if p.at < p.s.len() {
            return Err(format!("unexpected '{}' at {}", p.s[p.at] as char, p.at + 1));
        }
        Ok(f)
    }

    /// The value for one row (`None`: empty).
    pub fn eval(&self, row: &Map<String, Value>) -> Option<Decimal> {
        match self {
            Formula::Num(n) => Some(*n),
            Formula::Col(c) => match row.get(c)? {
                Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
                Value::String(s) => Decimal::from_str(s).ok(),
                _ => None,
            },
            Formula::Neg(x) => x.eval(row).map(|v| -v),
            Formula::Op(op, a, b) => {
                let (a, b) = (a.eval(row)?, b.eval(row)?);
                match op {
                    '+' => a.checked_add(b),
                    '-' => a.checked_sub(b),
                    '*' => a.checked_mul(b),
                    _ => a.checked_div(b),
                }
            }
            Formula::Call(f, args) => match f {
                Func::Coalesce => args.iter().find_map(|a| a.eval(row)),
                Func::Abs => args[0].eval(row).map(|v| v.abs()),
                Func::Min | Func::Max => {
                    let vals: Option<Vec<Decimal>> = args.iter().map(|a| a.eval(row)).collect();
                    let vals = vals?;
                    if *f == Func::Min { vals.into_iter().min() } else { vals.into_iter().max() }
                }
                Func::Round => {
                    let places = match args.get(1) {
                        Some(p) => p.eval(row)?.to_u32().filter(|p| *p <= 12)?,
                        None => 0,
                    };
                    args[0].eval(row).map(|v| v.round_dp_with_strategy(places, rust_decimal::RoundingStrategy::MidpointAwayFromZero))
                }
            },
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
    depth: usize,
    column: &'a dyn Fn(&str) -> Result<String, String>,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.s.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.space();
        self.s.get(self.at).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at {}", c as char, self.at + 1))
        }
    }

    fn expr(&mut self) -> Result<Formula, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("the formula is nested too deeply".into());
        }
        let mut left = self.term()?;
        while let Some(op @ (b'+' | b'-')) = self.peek() {
            self.at += 1;
            left = Formula::Op(op as char, Box::new(left), Box::new(self.term()?));
        }
        self.depth -= 1;
        Ok(left)
    }

    fn term(&mut self) -> Result<Formula, String> {
        let mut left = self.unary()?;
        while let Some(op @ (b'*' | b'/')) = self.peek() {
            self.at += 1;
            left = Formula::Op(op as char, Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Formula, String> {
        if self.peek() == Some(b'-') {
            self.at += 1;
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                return Err("the formula is nested too deeply".into());
            }
            let x = self.unary()?;
            self.depth -= 1;
            return Ok(Formula::Neg(Box::new(x)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Formula, String> {
        match self.peek() {
            Some(b'(') => {
                self.at += 1;
                let x = self.expr()?;
                self.expect(b')')?;
                Ok(x)
            }
            Some(b'[') => {
                let start = self.at + 1;
                let end = self.s[start..].iter().position(|c| *c == b']').ok_or("a '[' has no closing ']'")? + start;
                self.at = end + 1;
                let name = std::str::from_utf8(&self.s[start..end]).map_err(|_| "a column reference is not text")?.trim();
                Ok(Formula::Col((self.column)(name)?))
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => {
                let start = self.at;
                while self.s.get(self.at).is_some_and(|c| c.is_ascii_digit() || *c == b'.') {
                    self.at += 1;
                }
                let text = std::str::from_utf8(&self.s[start..self.at]).unwrap_or_default();
                Decimal::from_str(text).map(Formula::Num).map_err(|_| format!("'{text}' is not a number"))
            }
            Some(c) if c.is_ascii_alphabetic() => {
                let start = self.at;
                while self.s.get(self.at).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
                    self.at += 1;
                }
                let name = std::str::from_utf8(&self.s[start..self.at]).unwrap_or_default().to_ascii_lowercase();
                let (func, arity) = match name.as_str() {
                    "round" => (Func::Round, 1..=2),
                    "abs" => (Func::Abs, 1..=1),
                    "min" => (Func::Min, 2..=10),
                    "max" => (Func::Max, 2..=10),
                    "coalesce" => (Func::Coalesce, 1..=10),
                    _ => return Err(format!("unknown function {name} (round, abs, min, max and coalesce are known; columns go in [brackets])")),
                };
                self.expect(b'(')?;
                let mut args = vec![self.expr()?];
                while self.peek() == Some(b',') {
                    self.at += 1;
                    args.push(self.expr()?);
                }
                self.expect(b')')?;
                if !arity.contains(&args.len()) {
                    return Err(format!("{name} takes {} to {} values", arity.start(), arity.end()));
                }
                Ok(Formula::Call(func, args))
            }
            Some(c) => Err(format!("unexpected '{}' at {}", c as char, self.at + 1)),
            None => Err("the formula ends too soon".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cols(name: &str) -> Result<String, String> {
        match name.to_lowercase().as_str() {
            "revenue" | "sum(grand_total)" => Ok("sum(grand_total)".into()),
            "cost" => Ok("sum(cost)".into()),
            "count" => Ok("count".into()),
            other => Err(format!("no column {other}")),
        }
    }

    fn eval(text: &str, row: Value) -> Option<String> {
        let f = Formula::parse(text, &cols).unwrap();
        f.eval(row.as_object().unwrap()).map(|d| d.normalize().to_string())
    }

    #[test]
    fn arithmetic_over_columns_is_exact() {
        let row = json!({"sum(grand_total)": "1000.10", "sum(cost)": "600.05", "count": 3});
        assert_eq!(eval("[Revenue] - [Cost]", row.clone()).as_deref(), Some("400.05"));
        assert_eq!(eval("round(([revenue] - [cost]) / [Revenue] * 100, 2)", row.clone()).as_deref(), Some("40"));
        assert_eq!(eval("[sum(grand_total)] / [count]", row.clone()).as_deref(), Some("333.36666666666666666666666667"));
        assert_eq!(eval("-[cost] + 2 * 3", row.clone()).as_deref(), Some("-594.05"));
        assert_eq!(eval("max([cost], 700)", row.clone()).as_deref(), Some("700"));
        assert_eq!(eval("abs(0.1 - 0.3)", row.clone()).as_deref(), Some("0.2"), "no floating point");
    }

    #[test]
    fn empty_and_zero_give_empty() {
        let row = json!({"sum(grand_total)": "10", "sum(cost)": null, "count": 0});
        assert_eq!(eval("[revenue] - [cost]", row.clone()), None);
        assert_eq!(eval("[revenue] / [count]", row.clone()), None, "division by zero");
        assert_eq!(eval("coalesce([cost], 0) + 1", row).as_deref(), Some("1"));
    }

    #[test]
    fn mistakes_are_named() {
        let err = |t: &str| Formula::parse(t, &cols).unwrap_err();
        assert!(err("[profit] * 2").contains("no column profit"));
        assert!(err("[revenue] +").contains("ends too soon"));
        assert!(err("sqrt([revenue])").contains("unknown function sqrt"));
        assert!(err("round([revenue], 1, 2)").contains("round takes 1 to 2"));
        assert!(err("([revenue]").contains("expected ')'"));
        assert!(err(&"(".repeat(40)).contains("nested too deeply"));
        assert!(err("[revenue] ; drop").contains("unexpected ';'"));
    }
}
