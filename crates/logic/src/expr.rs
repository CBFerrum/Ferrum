//! Typed expressions (docs/logic/decisions.md L-02): CEL's syntax, Ferrum's types and evaluation.
//!
//! The `cel` crate parses; this module checks every name, field and operator against declared types before
//! anything runs, and evaluates the checked tree itself, so that:
//!
//! * money, quantities and decimals are exact: `money("100000.00", "INR")`, `qty("2.5", "kg")`, `dec("0.1")`.
//!   A fractional literal such as `2.5` is refused (a double cannot hold most decimals); rounding is always
//!   asked for (`round(x, 2)`), never implied;
//! * a money amount is not compared with a temperature, nor rupees with dollars, without a conversion;
//! * an unknown or missing value is *unknown*, not false: `unknown && false` is false, `unknown && true` is
//!   unknown, and a comparison with a missing value is unknown. A caller treats unknown as "not proven";
//! * nothing reads the clock, the network or randomness: `now` and `today` are facts handed in, like any other;
//! * every evaluation has a step budget, and lists and texts handed in are bounded.

use crate::refusal::{Code, Refusal};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use cel::common::ast::{Expr, IdedExpr, LiteralValue};

pub const MAX_SOURCE: usize = 4096;
const MAX_NESTING: usize = 32;
/// Operators one expression may hold. A chain of operators nests as deep as it is long, parentheses or
/// not, and the parser and checker recurse that deep: a long enough chain would end the process.
pub const MAX_OPERATORS: usize = 64;
/// Nodes one evaluation may visit.
pub const STEP_BUDGET: u32 = 10_000;
pub const MAX_LIST: usize = 1_000;
pub const MAX_TEXT: usize = 64 * 1024;

/// A static type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Bool,
    Int,
    Decimal,
    Text,
    Date,
    Instant,
    /// Money, with its currency when it is known before running.
    Money(Option<String>),
    /// A quantity, with its unit when it is known before running.
    Quantity(Option<String>),
    List(Box<Ty>),
    Record(BTreeMap<String, Ty>),
    /// Untyped data (a JSON value whose shape is not declared): fields read from it are untyped too.
    Any,
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Bool => f.write_str("yes/no"),
            Ty::Int => f.write_str("whole number"),
            Ty::Decimal => f.write_str("decimal"),
            Ty::Text => f.write_str("text"),
            Ty::Date => f.write_str("date"),
            Ty::Instant => f.write_str("moment"),
            Ty::Money(Some(c)) => write!(f, "money in {c}"),
            Ty::Money(None) => f.write_str("money"),
            Ty::Quantity(Some(u)) => write!(f, "quantity in {u}"),
            Ty::Quantity(None) => f.write_str("quantity"),
            Ty::List(t) => write!(f, "list of {t}"),
            Ty::Record(_) => f.write_str("record"),
            Ty::Any => f.write_str("any value"),
        }
    }
}

impl Ty {
    fn numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Decimal | Ty::Any)
    }
}

/// A value as an expression sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Dec(Decimal),
    Text(String),
    Date(NaiveDate),
    Instant(DateTime<Utc>),
    Money(Decimal, String),
    Qty(Decimal, String),
    List(Vec<Val>),
    Record(BTreeMap<String, Val>),
    /// Not known, and why: a missing value, a type found wrong at run time, the budget spent.
    Unknown(String),
}

impl Val {
    pub fn is_unknown(&self) -> bool {
        matches!(self, Val::Unknown(_))
    }

    /// As JSON: decimals and money as exact strings.
    pub fn to_json(&self) -> Value {
        match self {
            Val::Null | Val::Unknown(_) => Value::Null,
            Val::Bool(b) => Value::Bool(*b),
            Val::Int(i) => Value::from(*i),
            Val::Dec(d) => Value::String(d.normalize().to_string()),
            Val::Text(s) => Value::String(s.clone()),
            Val::Date(d) => Value::String(d.format("%Y-%m-%d").to_string()),
            Val::Instant(t) => Value::String(t.to_rfc3339()),
            Val::Money(a, c) => serde_json::json!({"amount": a.to_string(), "currency": c}),
            Val::Qty(v, u) => serde_json::json!({"value": v.to_string(), "unit": u}),
            Val::List(items) => Value::Array(items.iter().map(Val::to_json).collect()),
            Val::Record(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
        }
    }

    /// A JSON value read as `ty` declares it. Shapes that do not fit become unknown, not a guess.
    pub fn from_json(v: &Value, ty: &Ty) -> Val {
        let unknown = |why: &str| Val::Unknown(format!("{why}: {v}"));
        match (ty, v) {
            (_, Value::Null) => Val::Null,
            (Ty::Bool, Value::Bool(b)) => Val::Bool(*b),
            (Ty::Int, Value::Number(n)) => n.as_i64().map(Val::Int).unwrap_or_else(|| unknown("not a whole number")),
            (Ty::Int, Value::String(s)) => s.parse().map(Val::Int).unwrap_or_else(|_| unknown("not a whole number")),
            (Ty::Decimal, Value::Number(n)) => Decimal::from_str(&n.to_string()).map(Val::Dec).unwrap_or_else(|_| unknown("not a decimal")),
            (Ty::Decimal, Value::String(s)) => Decimal::from_str(s).map(Val::Dec).unwrap_or_else(|_| unknown("not a decimal")),
            (Ty::Text, Value::String(s)) if s.len() <= MAX_TEXT => Val::Text(s.clone()),
            (Ty::Text, Value::String(_)) => unknown("text too long"),
            (Ty::Date, Value::String(s)) => {
                NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d").map(Val::Date).unwrap_or_else(|_| unknown("not a date"))
            }
            (Ty::Instant, Value::String(s)) => {
                DateTime::parse_from_rfc3339(s).map(|t| Val::Instant(t.with_timezone(&Utc))).unwrap_or_else(|_| unknown("not a moment"))
            }
            (Ty::Money(want), Value::Object(m)) => {
                let amount = m.get("amount").and_then(|a| match a {
                    Value::String(s) => Decimal::from_str(s).ok(),
                    Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
                    _ => None,
                });
                match (amount, m.get("currency").and_then(Value::as_str)) {
                    (Some(a), Some(c)) if want.as_deref().is_none_or(|w| w == c) => Val::Money(a, c.to_string()),
                    (Some(_), Some(c)) => Val::Unknown(format!("money in {c} where {} was declared", want.as_deref().unwrap_or("?"))),
                    (_, None) => unknown("money without a currency"),
                    _ => unknown("not money"),
                }
            }
            (Ty::Quantity(want), Value::Object(m)) => {
                let value = m
                    .get("value")
                    .and_then(|a| a.as_str().map(str::to_string).or_else(|| a.as_number().map(|n| n.to_string())))
                    .and_then(|s| Decimal::from_str(&s).ok());
                match (value, m.get("unit").and_then(Value::as_str)) {
                    (Some(q), Some(u)) if want.as_deref().is_none_or(|w| w == u) => Val::Qty(q, u.to_string()),
                    (Some(_), Some(u)) => Val::Unknown(format!("a quantity in {u} where {} was declared", want.as_deref().unwrap_or("?"))),
                    _ => unknown("a quantity needs a value and a unit"),
                }
            }
            (Ty::List(item), Value::Array(items)) if items.len() <= MAX_LIST => Val::List(items.iter().map(|i| Val::from_json(i, item)).collect()),
            (Ty::List(_), Value::Array(_)) => unknown("list too long"),
            (Ty::Record(fields), Value::Object(m)) => {
                Val::Record(fields.iter().map(|(k, t)| (k.clone(), m.get(k).map_or(Val::Null, |v| Val::from_json(v, t)))).collect())
            }
            (Ty::Any, _) => Val::any(v),
            _ => unknown(&format!("not {ty}")),
        }
    }

    /// Untyped JSON, read as faithfully as it allows: numbers with fractions stay exact decimals.
    pub fn any(v: &Value) -> Val {
        match v {
            Value::Null => Val::Null,
            Value::Bool(b) => Val::Bool(*b),
            Value::Number(n) => {
                n.as_i64().map(Val::Int).unwrap_or_else(|| Decimal::from_str(&n.to_string()).map(Val::Dec).unwrap_or(Val::Unknown(format!("number {n}"))))
            }
            Value::String(s) if s.len() <= MAX_TEXT => Val::Text(s.clone()),
            Value::String(_) => Val::Unknown("text too long".into()),
            Value::Array(items) if items.len() <= MAX_LIST => Val::List(items.iter().map(Val::any).collect()),
            Value::Array(_) => Val::Unknown("list too long".into()),
            Value::Object(m) => Val::Record(m.iter().map(|(k, v)| (k.clone(), Val::any(v))).collect()),
        }
    }
}

/// Names an expression may use, with their types.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub vars: BTreeMap<String, Ty>,
}

impl Env {
    pub fn with(mut self, name: &str, ty: Ty) -> Env {
        self.vars.insert(name.to_string(), ty);
        self
    }
}

/// The values those names have for one evaluation.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub vars: BTreeMap<String, Val>,
}

impl Facts {
    pub fn with(mut self, name: &str, v: Val) -> Facts {
        self.vars.insert(name.to_string(), v);
        self
    }
}

/// A checked expression, ready to evaluate as often as needed.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub source: String,
    pub ty: Ty,
    ast: IdedExpr,
}

fn bad(code: Code, msg: impl Into<String>) -> Refusal {
    Refusal::new(code, msg)
}

/// Parses and checks `source` against `env`.
pub fn compile(source: &str, env: &Env) -> Result<Compiled, Refusal> {
    if source.trim().is_empty() {
        return Err(bad(Code::SchemaInvalid, "the expression is empty"));
    }
    if source.len() > MAX_SOURCE {
        return Err(bad(Code::SchemaInvalid, format!("the expression is longer than {MAX_SOURCE} characters")));
    }
    let mut depth = 0usize;
    for c in source.chars() {
        match c {
            '(' | '[' | '{' => {
                depth += 1;
                if depth > MAX_NESTING {
                    return Err(bad(Code::SchemaInvalid, format!("the expression is nested deeper than {MAX_NESTING} levels")));
                }
            }
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let operators = source.chars().filter(|c| "+-*/%<>=!&|?".contains(*c)).count();
    if operators > MAX_OPERATORS {
        return Err(bad(Code::SchemaInvalid, format!("the expression holds more than {MAX_OPERATORS} operators: split it into steps")));
    }
    let ast = cel::parser::Parser::new().parse(source).map_err(|e| bad(Code::SchemaInvalid, format!("'{source}' does not parse: {e}")))?;
    let ty = check(&ast, env)?;
    Ok(Compiled { source: source.to_string(), ty, ast })
}

/// Compiles and also requires a type: a condition must be yes/no, a value must fit where it goes.
pub fn compile_as(source: &str, env: &Env, want: &Ty) -> Result<Compiled, Refusal> {
    let c = compile(source, env)?;
    if !fits(&c.ty, want) {
        return Err(bad(Code::TypeMismatch, format!("'{source}' is {}, where {want} is needed", c.ty)));
    }
    Ok(c)
}

/// Whether a value of type `have` may go where `want` is needed.
pub fn fits(have: &Ty, want: &Ty) -> bool {
    match (have, want) {
        (Ty::Any, _) | (_, Ty::Any) => true,
        (Ty::Int, Ty::Decimal) => true,
        (Ty::Money(a), Ty::Money(b)) => a.is_none() || b.is_none() || a == b,
        (Ty::Quantity(a), Ty::Quantity(b)) => a.is_none() || b.is_none() || a == b,
        (Ty::List(a), Ty::List(b)) => fits(a, b),
        (a, b) => a == b,
    }
}

/// One type for two branches or list items, or why there is none.
fn unify(a: &Ty, b: &Ty) -> Option<Ty> {
    Some(match (a, b) {
        (Ty::Any, t) | (t, Ty::Any) => t.clone(),
        (Ty::Int, Ty::Decimal) | (Ty::Decimal, Ty::Int) => Ty::Decimal,
        (Ty::Money(x), Ty::Money(y)) => match (x, y) {
            (Some(x), Some(y)) if x != y => return None,
            _ => Ty::Money(x.clone().or_else(|| y.clone())),
        },
        (Ty::Quantity(x), Ty::Quantity(y)) => match (x, y) {
            (Some(x), Some(y)) if x != y => return None,
            _ => Ty::Quantity(x.clone().or_else(|| y.clone())),
        },
        (a, b) if a == b => a.clone(),
        _ => return None,
    })
}

fn literal_text(e: &IdedExpr) -> Option<&str> {
    match &e.expr {
        Expr::Literal(LiteralValue::String(s)) => Some(s.inner()),
        _ => None,
    }
}

fn check(e: &IdedExpr, env: &Env) -> Result<Ty, Refusal> {
    match &e.expr {
        Expr::Literal(l) => match l {
            LiteralValue::Boolean(_) => Ok(Ty::Bool),
            LiteralValue::Int(_) | LiteralValue::UInt(_) => Ok(Ty::Int),
            LiteralValue::String(_) => Ok(Ty::Text),
            LiteralValue::Null => Ok(Ty::Any),
            LiteralValue::Double(d) => Err(bad(Code::TypeMismatch, format!("{} is not exact: write dec(\"{}\")", d.inner(), d.inner()))),
            LiteralValue::Bytes(_) => Err(bad(Code::TypeMismatch, "bytes are not a business value")),
        },
        Expr::Ident(name) => env.vars.get(name).cloned().ok_or_else(|| bad(Code::SchemaInvalid, format!("unknown name '{name}'"))),
        Expr::Select(s) => {
            let operand = check(&s.operand, env)?;
            if s.test {
                return match operand {
                    Ty::Record(_) | Ty::Any => Ok(Ty::Bool),
                    other => Err(bad(Code::TypeMismatch, format!("has() asks a record, not {other}"))),
                };
            }
            field(&operand, &s.field)
        }
        Expr::List(l) => {
            if l.elements.len() > MAX_LIST {
                return Err(bad(Code::SchemaInvalid, "the list is too long"));
            }
            let mut item = Ty::Any;
            for el in &l.elements {
                let t = check(el, env)?;
                item = unify(&item, &t).ok_or_else(|| bad(Code::TypeMismatch, format!("a list mixes {item} and {t}")))?;
            }
            Ok(Ty::List(Box::new(item)))
        }
        Expr::Call(c) => call(&c.func_name, c.target.as_deref(), &c.args, env),
        Expr::Comprehension(_) => Err(bad(Code::UnsupportedProfile, "loops over a list are not an expression's: a process does them as a for-each")),
        Expr::Map(_) | Expr::Struct(_) => Err(bad(Code::UnsupportedProfile, "maps and structures are not written in expressions")),
        Expr::Unspecified => Err(bad(Code::SchemaInvalid, "an empty expression")),
    }
}

fn field(operand: &Ty, name: &str) -> Result<Ty, Refusal> {
    match operand {
        Ty::Record(fields) => fields.get(name).cloned().ok_or_else(|| bad(Code::SchemaInvalid, format!("there is no field '{name}'"))),
        Ty::Any => Ok(Ty::Any),
        Ty::Money(_) if name == "amount" => Ok(Ty::Decimal),
        Ty::Money(_) if name == "currency" => Ok(Ty::Text),
        Ty::Quantity(_) if name == "value" => Ok(Ty::Decimal),
        Ty::Quantity(_) if name == "unit" => Ok(Ty::Text),
        other => Err(bad(Code::TypeMismatch, format!("{other} has no field '{name}'"))),
    }
}

fn comparable(a: &Ty, b: &Ty, op: &str) -> Result<(), Refusal> {
    let ok = match (a, b) {
        (Ty::Any, _) | (_, Ty::Any) => true,
        (x, y) if x.numeric() && y.numeric() => true,
        (Ty::Money(x), Ty::Money(y)) => {
            if let (Some(x), Some(y)) = (x, y)
                && x != y
            {
                return Err(bad(Code::TypeMismatch, format!("money in {x} and in {y} cannot be compared without a conversion")));
            }
            true
        }
        (Ty::Quantity(x), Ty::Quantity(y)) => {
            if let (Some(x), Some(y)) = (x, y)
                && x != y
            {
                return Err(bad(Code::TypeMismatch, format!("quantities in {x} and in {y} cannot be compared without a conversion")));
            }
            true
        }
        // Lists and records are equal or not; they have no order.
        (x, y) => x == y && (matches!(op, "==" | "!=") || !matches!(x, Ty::List(_) | Ty::Record(_))),
    };
    if ok { Ok(()) } else { Err(bad(Code::TypeMismatch, format!("{a} and {b} cannot be compared ({op})"))) }
}

const FUNCTIONS: &[&str] = &[
    "size",
    "startsWith",
    "endsWith",
    "contains",
    "dec",
    "money",
    "qty",
    "date",
    "days_between",
    "add_days",
    "coalesce",
    "round",
    "round_even",
    "abs",
    "min",
    "max",
    "lower",
    "upper",
];

fn call(name: &str, target: Option<&IdedExpr>, args: &[IdedExpr], env: &Env) -> Result<Ty, Refusal> {
    let arg = |i: usize| -> Result<Ty, Refusal> { check(&args[i], env) };
    let n = args.len();
    let arity = |want: usize| -> Result<(), Refusal> {
        if n == want { Ok(()) } else { Err(bad(Code::SchemaInvalid, format!("{name} takes {want} argument(s), not {n}"))) }
    };
    match name {
        "_&&_" | "_||_" => {
            for i in 0..2 {
                let t = arg(i)?;
                if !fits(&t, &Ty::Bool) {
                    return Err(bad(Code::TypeMismatch, format!("&& and || take yes/no, not {t}")));
                }
            }
            Ok(Ty::Bool)
        }
        "!_" => {
            let t = arg(0)?;
            if fits(&t, &Ty::Bool) { Ok(Ty::Bool) } else { Err(bad(Code::TypeMismatch, format!("! takes yes/no, not {t}"))) }
        }
        "-_" => match arg(0)? {
            t @ (Ty::Int | Ty::Decimal | Ty::Money(_) | Ty::Quantity(_) | Ty::Any) => Ok(t),
            t => Err(bad(Code::TypeMismatch, format!("- cannot negate {t}"))),
        },
        "_+_" | "_-_" => {
            let (a, b) = (arg(0)?, arg(1)?);
            match (&a, &b) {
                (Ty::Text, Ty::Text) if name == "_+_" => Ok(Ty::Text),
                (x, y) if x.numeric() && y.numeric() => Ok(unify(x, y).unwrap_or(Ty::Decimal)),
                (Ty::Money(_), Ty::Money(_)) | (Ty::Quantity(_), Ty::Quantity(_)) => {
                    unify(&a, &b).ok_or_else(|| bad(Code::TypeMismatch, format!("{a} and {b} cannot be added without a conversion")))
                }
                (Ty::Date, Ty::Date) if name == "_-_" => Err(bad(Code::TypeMismatch, "subtract dates with days_between(a, b)")),
                _ => Err(bad(Code::TypeMismatch, format!("{a} and {b} cannot be added or subtracted"))),
            }
        }
        "_*_" => {
            let (a, b) = (arg(0)?, arg(1)?);
            match (&a, &b) {
                (x, y) if x.numeric() && y.numeric() => Ok(unify(x, y).unwrap_or(Ty::Decimal)),
                (m @ (Ty::Money(_) | Ty::Quantity(_)), x) | (x, m @ (Ty::Money(_) | Ty::Quantity(_))) if x.numeric() => Ok(m.clone()),
                _ => Err(bad(Code::TypeMismatch, format!("{a} and {b} cannot be multiplied"))),
            }
        }
        "_/_" => {
            let (a, b) = (arg(0)?, arg(1)?);
            match (&a, &b) {
                (x, y) if x.numeric() && y.numeric() => Ok(Ty::Decimal),
                (Ty::Money(_), Ty::Money(_)) => {
                    unify(&a, &b).map(|_| Ty::Decimal).ok_or_else(|| bad(Code::TypeMismatch, format!("{a} and {b} cannot be divided without a conversion")))
                }
                (Ty::Money(_), _) => Err(bad(Code::TypeMismatch, "money is divided with its rounding said: round(m * dec(\"0.5\"), 2)")),
                _ => Err(bad(Code::TypeMismatch, format!("{a} and {b} cannot be divided"))),
            }
        }
        "_%_" => match (arg(0)?, arg(1)?) {
            (Ty::Int | Ty::Any, Ty::Int | Ty::Any) => Ok(Ty::Int),
            (a, b) => Err(bad(Code::TypeMismatch, format!("% takes whole numbers, not {a} and {b}"))),
        },
        "_<_" | "_<=_" | "_>_" | "_>=_" => {
            let (a, b) = (arg(0)?, arg(1)?);
            comparable(&a, &b, name.trim_matches('_'))?;
            if matches!(a, Ty::Bool) {
                return Err(bad(Code::TypeMismatch, "yes/no values are not ordered"));
            }
            Ok(Ty::Bool)
        }
        "_==_" | "_!=_" => {
            let (a, b) = (arg(0)?, arg(1)?);
            comparable(&a, &b, "==")?;
            Ok(Ty::Bool)
        }
        "_?_:_" => {
            let cond = arg(0)?;
            if !fits(&cond, &Ty::Bool) {
                return Err(bad(Code::TypeMismatch, format!("a condition is yes/no, not {cond}")));
            }
            let (a, b) = (arg(1)?, arg(2)?);
            unify(&a, &b).ok_or_else(|| bad(Code::TypeMismatch, format!("the two branches are {a} and {b}")))
        }
        "@in" => {
            let (item, list) = (arg(0)?, arg(1)?);
            match list {
                Ty::List(inner) => {
                    comparable(&item, &inner, "in")?;
                    Ok(Ty::Bool)
                }
                Ty::Any => Ok(Ty::Bool),
                other => Err(bad(Code::TypeMismatch, format!("in asks a list, not {other}"))),
            }
        }
        "_[_]" => match (arg(0)?, arg(1)?) {
            (Ty::List(inner), Ty::Int | Ty::Any) => Ok(*inner),
            (Ty::Any, _) => Ok(Ty::Any),
            (Ty::Record(fields), Ty::Text) => match literal_text(&args[1]) {
                Some(k) => field(&Ty::Record(fields), k),
                None => Ok(Ty::Any),
            },
            (a, b) => Err(bad(Code::TypeMismatch, format!("{a} cannot be indexed by {b}"))),
        },
        _ if !FUNCTIONS.contains(&name) => Err(bad(Code::CapabilityUnavailable, format!("there is no function '{name}'"))),
        // Functions, called as f(x) or as x.f() for the text ones.
        "startsWith" | "endsWith" | "contains" => {
            let on = target.ok_or_else(|| bad(Code::SchemaInvalid, format!("{name} is called on a text: x.{name}(\"…\")")))?;
            let (t, a) = (check(on, env)?, arg(0)?);
            if fits(&t, &Ty::Text) && fits(&a, &Ty::Text) {
                Ok(Ty::Bool)
            } else {
                Err(bad(Code::TypeMismatch, format!("{name} compares texts, not {t} and {a}")))
            }
        }
        _ if target.is_some() => Err(bad(Code::SchemaInvalid, format!("{name} is called as {name}(…)"))),
        "size" => {
            arity(1)?;
            match arg(0)? {
                Ty::Text | Ty::List(_) | Ty::Any => Ok(Ty::Int),
                t => Err(bad(Code::TypeMismatch, format!("size counts a text or a list, not {t}"))),
            }
        }
        "dec" => {
            arity(1)?;
            if let Some(s) = literal_text(&args[0])
                && Decimal::from_str(s).is_err()
            {
                return Err(bad(Code::TypeMismatch, format!("\"{s}\" is not a decimal")));
            }
            text_arg(arg(0)?, "dec").map(|_| Ty::Decimal)
        }
        "money" | "qty" => {
            arity(2)?;
            if let Some(s) = literal_text(&args[0])
                && Decimal::from_str(s).is_err()
            {
                return Err(bad(Code::TypeMismatch, format!("\"{s}\" is not an amount")));
            }
            let a = arg(0)?;
            if !(fits(&a, &Ty::Text) || a.numeric()) {
                return Err(bad(Code::TypeMismatch, format!("{name} takes an amount as text or a decimal, not {a}")));
            }
            text_arg(arg(1)?, name)?;
            let unit = literal_text(&args[1]).map(str::to_string);
            Ok(if name == "money" { Ty::Money(unit) } else { Ty::Quantity(unit) })
        }
        "date" => {
            arity(1)?;
            if let Some(s) = literal_text(&args[0])
                && NaiveDate::parse_from_str(s, "%Y-%m-%d").is_err()
            {
                return Err(bad(Code::TypeMismatch, format!("\"{s}\" is not a date (YYYY-MM-DD)")));
            }
            text_arg(arg(0)?, "date").map(|_| Ty::Date)
        }
        "days_between" => {
            arity(2)?;
            for i in 0..2 {
                let t = arg(i)?;
                if !fits(&t, &Ty::Date) {
                    return Err(bad(Code::TypeMismatch, format!("days_between takes dates, not {t}")));
                }
            }
            Ok(Ty::Int)
        }
        "add_days" => {
            arity(2)?;
            let (d, k) = (arg(0)?, arg(1)?);
            if fits(&d, &Ty::Date) && fits(&k, &Ty::Int) {
                Ok(Ty::Date)
            } else {
                Err(bad(Code::TypeMismatch, format!("add_days takes a date and a whole number, not {d} and {k}")))
            }
        }
        "coalesce" => {
            arity(2)?;
            let (a, b) = (arg(0)?, arg(1)?);
            unify(&a, &b).ok_or_else(|| bad(Code::TypeMismatch, format!("coalesce takes two of one kind, not {a} and {b}")))
        }
        "round" | "round_even" => {
            arity(2)?;
            let (v, places) = (arg(0)?, arg(1)?);
            if !fits(&places, &Ty::Int) {
                return Err(bad(Code::TypeMismatch, format!("{name}'s places are a whole number, not {places}")));
            }
            match v {
                t @ (Ty::Decimal | Ty::Money(_) | Ty::Quantity(_) | Ty::Any) => Ok(t),
                Ty::Int => Ok(Ty::Int),
                t => Err(bad(Code::TypeMismatch, format!("{name} rounds a number, not {t}"))),
            }
        }
        "abs" => {
            arity(1)?;
            match arg(0)? {
                t @ (Ty::Int | Ty::Decimal | Ty::Money(_) | Ty::Quantity(_) | Ty::Any) => Ok(t),
                t => Err(bad(Code::TypeMismatch, format!("abs takes a number, not {t}"))),
            }
        }
        "min" | "max" => {
            arity(2)?;
            let (a, b) = (arg(0)?, arg(1)?);
            comparable(&a, &b, name)?;
            unify(&a, &b).ok_or_else(|| bad(Code::TypeMismatch, format!("{name} takes two of one kind, not {a} and {b}")))
        }
        "lower" | "upper" => {
            arity(1)?;
            text_arg(arg(0)?, name).map(|_| Ty::Text)
        }
        other => Err(bad(Code::CapabilityUnavailable, format!("there is no function '{other}'"))),
    }
}

fn text_arg(t: Ty, f: &str) -> Result<Ty, Refusal> {
    if fits(&t, &Ty::Text) { Ok(t) } else { Err(bad(Code::TypeMismatch, format!("{f} takes text here, not {t}"))) }
}

// ----- Evaluation -----

struct Run<'a> {
    facts: &'a Facts,
    steps: u32,
}

fn unk(why: impl Into<String>) -> Val {
    Val::Unknown(why.into())
}

fn num(v: &Val) -> Option<Decimal> {
    match v {
        Val::Int(i) => Some(Decimal::from(*i)),
        Val::Dec(d) => Some(*d),
        _ => None,
    }
}

impl Compiled {
    /// What the expression reads, as dotted paths from a name (`doc.x_risk`, `parameters.review_limit`); a
    /// name read whole is the name alone. `has(doc.x)` reads `doc.x`.
    pub fn references(&self) -> BTreeSet<String> {
        fn path(e: &IdedExpr) -> Option<String> {
            match &e.expr {
                Expr::Ident(n) => Some(n.clone()),
                Expr::Select(s) => path(&s.operand).map(|p| format!("{p}.{}", s.field)),
                _ => None,
            }
        }
        fn walk(e: &IdedExpr, out: &mut BTreeSet<String>) {
            if let Some(p) = path(e) {
                out.insert(p);
                return;
            }
            match &e.expr {
                Expr::Select(s) => walk(&s.operand, out),
                Expr::Call(c) => {
                    if let Some(t) = &c.target {
                        walk(t, out);
                    }
                    for a in &c.args {
                        walk(a, out);
                    }
                }
                Expr::List(l) => l.elements.iter().for_each(|el| walk(el, out)),
                _ => {}
            }
        }
        let mut out = BTreeSet::new();
        walk(&self.ast, &mut out);
        out
    }

    /// Evaluates with `facts`. Never panics and never reads anything but `facts`.
    pub fn eval(&self, facts: &Facts) -> Val {
        let mut run = Run { facts, steps: 0 };
        run.eval(&self.ast)
    }

    /// Evaluates a condition: `Some(true)`, `Some(false)`, or `None` when it is not known.
    pub fn decide(&self, facts: &Facts) -> Result<bool, String> {
        match self.eval(facts) {
            Val::Bool(b) => Ok(b),
            Val::Unknown(why) => Err(why),
            Val::Null => Err("the condition's value is missing".into()),
            other => Err(format!("the condition gave {}", other.to_json())),
        }
    }
}

impl Run<'_> {
    fn eval(&mut self, e: &IdedExpr) -> Val {
        self.steps += 1;
        if self.steps > STEP_BUDGET {
            return unk("the expression ran past its step budget");
        }
        match &e.expr {
            Expr::Literal(l) => match l {
                LiteralValue::Boolean(b) => Val::Bool(*b.inner()),
                LiteralValue::Int(i) => Val::Int(*i.inner()),
                LiteralValue::UInt(u) => i64::try_from(*u.inner()).map(Val::Int).unwrap_or_else(|_| unk("a number too large")),
                LiteralValue::String(s) => Val::Text(s.inner().to_string()),
                LiteralValue::Null => Val::Null,
                _ => unk("an inexact literal"),
            },
            Expr::Ident(name) => self.facts.vars.get(name).cloned().unwrap_or_else(|| unk(format!("{name} was not given"))),
            Expr::Select(s) => {
                let operand = self.eval(&s.operand);
                if s.test {
                    return match operand {
                        Val::Record(m) => Val::Bool(m.get(&s.field).is_some_and(|v| !matches!(v, Val::Null | Val::Unknown(_)))),
                        Val::Null => Val::Bool(false),
                        Val::Unknown(why) => Val::Unknown(why),
                        _ => Val::Bool(false),
                    };
                }
                match operand {
                    Val::Record(m) => m.get(&s.field).cloned().unwrap_or(Val::Null),
                    Val::Money(a, c) => match s.field.as_str() {
                        "amount" => Val::Dec(a),
                        "currency" => Val::Text(c),
                        _ => Val::Null,
                    },
                    Val::Qty(v, u) => match s.field.as_str() {
                        "value" => Val::Dec(v),
                        "unit" => Val::Text(u),
                        _ => Val::Null,
                    },
                    Val::Null => unk(format!("{} of a missing value", s.field)),
                    Val::Unknown(why) => Val::Unknown(why),
                    other => unk(format!("{} of {}", s.field, other.to_json())),
                }
            }
            Expr::List(l) => Val::List(l.elements.iter().map(|el| self.eval(el)).collect()),
            Expr::Call(c) => self.call(&c.func_name, c.target.as_deref(), &c.args),
            _ => unk("not an expression this runs"),
        }
    }

    fn call(&mut self, name: &str, target: Option<&IdedExpr>, args: &[IdedExpr]) -> Val {
        match name {
            "_&&_" => {
                let a = self.eval(&args[0]);
                if a == Val::Bool(false) {
                    return a;
                }
                let b = self.eval(&args[1]);
                match (a, b) {
                    (_, Val::Bool(false)) => Val::Bool(false),
                    (Val::Bool(true), Val::Bool(true)) => Val::Bool(true),
                    (Val::Unknown(w), _) | (_, Val::Unknown(w)) => Val::Unknown(w),
                    _ => unk("&& of a missing value"),
                }
            }
            "_||_" => {
                let a = self.eval(&args[0]);
                if a == Val::Bool(true) {
                    return a;
                }
                let b = self.eval(&args[1]);
                match (a, b) {
                    (_, Val::Bool(true)) => Val::Bool(true),
                    (Val::Bool(false), Val::Bool(false)) => Val::Bool(false),
                    (Val::Unknown(w), _) | (_, Val::Unknown(w)) => Val::Unknown(w),
                    _ => unk("|| of a missing value"),
                }
            }
            "!_" => match self.eval(&args[0]) {
                Val::Bool(b) => Val::Bool(!b),
                Val::Unknown(w) => Val::Unknown(w),
                _ => unk("! of a missing value"),
            },
            "_?_:_" => match self.eval(&args[0]) {
                Val::Bool(true) => self.eval(&args[1]),
                Val::Bool(false) => self.eval(&args[2]),
                Val::Unknown(w) => Val::Unknown(w),
                _ => unk("a missing condition"),
            },
            "startsWith" | "endsWith" | "contains" => {
                let on = target.map(|t| self.eval(t)).unwrap_or(Val::Null);
                let a = self.eval(&args[0]);
                match (on, a) {
                    (Val::Text(s), Val::Text(p)) => Val::Bool(match name {
                        "startsWith" => s.starts_with(&p),
                        "endsWith" => s.ends_with(&p),
                        _ => s.contains(&p),
                    }),
                    (Val::Unknown(w), _) | (_, Val::Unknown(w)) => Val::Unknown(w),
                    _ => unk(format!("{name} of a missing value")),
                }
            }
            "coalesce" => match self.eval(&args[0]) {
                Val::Null => self.eval(&args[1]),
                v => v,
            },
            _ => {
                let vals: Vec<Val> = args.iter().map(|a| self.eval(a)).collect();
                if let Some(Val::Unknown(w)) = vals.iter().find(|v| v.is_unknown()) {
                    return Val::Unknown(w.clone());
                }
                apply(name, &vals)
            }
        }
    }
}

fn round(d: Decimal, places: &Val, even: bool) -> Option<Decimal> {
    let p = match places {
        Val::Int(p) if (0..=12).contains(p) => *p as u32,
        _ => return None,
    };
    let strategy = if even { rust_decimal::RoundingStrategy::MidpointNearestEven } else { rust_decimal::RoundingStrategy::MidpointAwayFromZero };
    Some(d.round_dp_with_strategy(p, strategy))
}

fn compare(a: &Val, b: &Val) -> Result<std::cmp::Ordering, String> {
    use Val::*;
    match (a, b) {
        (x, y) if num(x).is_some() && num(y).is_some() => Ok(num(x).cmp(&num(y))),
        (Money(x, c), Money(y, d)) | (Qty(x, c), Qty(y, d)) => {
            if c == d {
                Ok(x.cmp(y))
            } else {
                Err(format!("{c} and {d} cannot be compared without a conversion"))
            }
        }
        (Text(x), Text(y)) => Ok(x.cmp(y)),
        (Date(x), Date(y)) => Ok(x.cmp(y)),
        (Instant(x), Instant(y)) => Ok(x.cmp(y)),
        (Null, _) | (_, Null) => Err("a comparison with a missing value".into()),
        (x, y) => Err(format!("{} and {} are not comparable", x.to_json(), y.to_json())),
    }
}

fn equal(a: &Val, b: &Val) -> Result<bool, String> {
    match (a, b) {
        (Val::Null, Val::Null) => Ok(true),
        (Val::Null, _) | (_, Val::Null) => Ok(false),
        (Val::Bool(x), Val::Bool(y)) => Ok(x == y),
        (Val::List(x), Val::List(y)) => {
            if x.len() != y.len() {
                return Ok(false);
            }
            for (p, q) in x.iter().zip(y) {
                if !equal(p, q)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Val::Record(x), Val::Record(y)) => Ok(x == y),
        _ => compare(a, b).map(|o| o == std::cmp::Ordering::Equal),
    }
}

fn arith(op: &str, a: &Val, b: &Val) -> Val {
    use Val::*;
    let f = |x: Decimal, y: Decimal| -> Option<Decimal> {
        match op {
            "_+_" => x.checked_add(y),
            "_-_" => x.checked_sub(y),
            "_*_" => x.checked_mul(y),
            "_/_" => x.checked_div(y),
            _ => None,
        }
    };
    let over = || unk(format!("{} {op} {} overflows or divides by zero", a.to_json(), b.to_json()));
    match (a, b) {
        (Int(x), Int(y)) if op != "_/_" => match op {
            "_+_" => x.checked_add(*y).map(Int).unwrap_or_else(over),
            "_-_" => x.checked_sub(*y).map(Int).unwrap_or_else(over),
            _ => x.checked_mul(*y).map(Int).unwrap_or_else(over),
        },
        (x, y) if num(x).is_some() && num(y).is_some() => f(num(x).unwrap_or_default(), num(y).unwrap_or_default()).map(Dec).unwrap_or_else(over),
        (Text(x), Text(y)) if op == "_+_" => {
            if x.len() + y.len() > MAX_TEXT {
                unk("text too long")
            } else {
                Text(format!("{x}{y}"))
            }
        }
        (Money(x, c), Money(y, d)) if op == "_/_" => {
            if c == d {
                x.checked_div(*y).map(Dec).unwrap_or_else(over)
            } else {
                unk(format!("{c} and {d} cannot be divided without a conversion"))
            }
        }
        (Money(x, c), Money(y, d)) | (Qty(x, c), Qty(y, d)) if op == "_+_" || op == "_-_" => {
            if c != d {
                return unk(format!("{c} and {d} cannot be added without a conversion"));
            }
            match (f(*x, *y), a) {
                (Some(v), Money(..)) => Money(v, c.clone()),
                (Some(v), _) => Qty(v, c.clone()),
                (None, _) => over(),
            }
        }
        (Money(x, c), y) | (y, Money(x, c)) if op == "_*_" && num(y).is_some() => {
            f(*x, num(y).unwrap_or_default()).map(|v| Money(v, c.clone())).unwrap_or_else(over)
        }
        (Qty(x, u), y) | (y, Qty(x, u)) if op == "_*_" && num(y).is_some() => f(*x, num(y).unwrap_or_default()).map(|v| Qty(v, u.clone())).unwrap_or_else(over),
        (Null, _) | (_, Null) => unk("arithmetic on a missing value"),
        _ => unk(format!("{} {op} {} does not compute", a.to_json(), b.to_json())),
    }
}

fn apply(name: &str, v: &[Val]) -> Val {
    use Val::*;
    let ordered = |test: fn(std::cmp::Ordering) -> bool| match compare(&v[0], &v[1]) {
        Ok(o) => Bool(test(o)),
        Err(why) => unk(why),
    };
    match name {
        "_<_" => ordered(|o| o.is_lt()),
        "_<=_" => ordered(|o| o.is_le()),
        "_>_" => ordered(|o| o.is_gt()),
        "_>=_" => ordered(|o| o.is_ge()),
        "_==_" => equal(&v[0], &v[1]).map(Bool).unwrap_or_else(unk),
        "_!=_" => equal(&v[0], &v[1]).map(|e| Bool(!e)).unwrap_or_else(unk),
        "_+_" | "_-_" | "_*_" | "_/_" => arith(name, &v[0], &v[1]),
        "_%_" => match (&v[0], &v[1]) {
            (Int(a), Int(b)) if *b != 0 => Int(a.rem_euclid(*b)),
            _ => unk("% needs whole numbers and a divisor other than 0"),
        },
        "-_" => match &v[0] {
            Int(i) => i.checked_neg().map(Int).unwrap_or_else(|| unk("overflow")),
            Dec(d) => Dec(-d),
            Money(a, c) => Money(-a, c.clone()),
            Qty(a, u) => Qty(-a, u.clone()),
            _ => unk("negation of a missing value"),
        },
        "@in" => match &v[1] {
            List(items) => {
                let mut unknown = None;
                for i in items {
                    match equal(&v[0], i) {
                        Ok(true) => return Bool(true),
                        Ok(false) => {}
                        Err(w) => unknown = Some(w),
                    }
                }
                unknown.map(unk).unwrap_or(Bool(false))
            }
            _ => unk("in a missing list"),
        },
        "_[_]" => match (&v[0], &v[1]) {
            (List(items), Int(i)) => usize::try_from(*i).ok().and_then(|i| items.get(i)).cloned().unwrap_or(Null),
            (Record(m), Text(k)) => m.get(k).cloned().unwrap_or(Null),
            _ => unk("indexing a missing value"),
        },
        "size" => match &v[0] {
            Text(s) => Int(s.chars().count() as i64),
            List(l) => Int(l.len() as i64),
            _ => unk("size of a missing value"),
        },
        "dec" => match &v[0] {
            Text(s) => Decimal::from_str(s.trim()).map(Dec).unwrap_or_else(|_| unk(format!("\"{s}\" is not a decimal"))),
            _ => unk("dec of a missing value"),
        },
        "money" | "qty" => {
            let amount = match &v[0] {
                Text(s) => Decimal::from_str(s.trim()).ok(),
                other => num(other),
            };
            match (amount, &v[1]) {
                (Some(a), Text(c)) if !c.trim().is_empty() => {
                    if name == "money" {
                        Money(a, c.trim().to_string())
                    } else {
                        Qty(a, c.trim().to_string())
                    }
                }
                (None, _) => unk(format!("{name} without an amount")),
                _ => unk(format!("{name} without a {}", if name == "money" { "currency" } else { "unit" })),
            }
        }
        "date" => match &v[0] {
            Text(s) => NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d").map(Date).unwrap_or_else(|_| unk(format!("\"{s}\" is not a date"))),
            _ => unk("date of a missing value"),
        },
        "days_between" => match (&v[0], &v[1]) {
            (Date(a), Date(b)) => Int((*b - *a).num_days()),
            _ => unk("days_between of a missing date"),
        },
        "add_days" => match (&v[0], &v[1]) {
            (Date(d), Int(k)) => d.checked_add_signed(chrono::Duration::days(*k)).map(Date).unwrap_or_else(|| unk("a date out of range")),
            _ => unk("add_days of a missing value"),
        },
        "round" | "round_even" => {
            let even = name == "round_even";
            match &v[0] {
                Int(i) => Int(*i),
                Dec(d) => round(*d, &v[1], even).map(Dec).unwrap_or_else(|| unk("round takes 0 to 12 places")),
                Money(a, c) => round(*a, &v[1], even).map(|r| Money(r, c.clone())).unwrap_or_else(|| unk("round takes 0 to 12 places")),
                Qty(a, u) => round(*a, &v[1], even).map(|r| Qty(r, u.clone())).unwrap_or_else(|| unk("round takes 0 to 12 places")),
                _ => unk("round of a missing value"),
            }
        }
        "abs" => match &v[0] {
            Int(i) => i.checked_abs().map(Int).unwrap_or_else(|| unk("overflow")),
            Dec(d) => Dec(d.abs()),
            Money(a, c) => Money(a.abs(), c.clone()),
            Qty(a, u) => Qty(a.abs(), u.clone()),
            _ => unk("abs of a missing value"),
        },
        "min" | "max" => match compare(&v[0], &v[1]) {
            Ok(o) => {
                let first = if name == "min" { o.is_le() } else { o.is_ge() };
                if first { v[0].clone() } else { v[1].clone() }
            }
            Err(w) => unk(w),
        },
        "lower" | "upper" => match &v[0] {
            Text(s) => Text(if name == "lower" { s.to_lowercase() } else { s.to_uppercase() }),
            _ => unk(format!("{name} of a missing value")),
        },
        other => unk(format!("no function {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> Ty {
        Ty::Record(BTreeMap::from([
            ("total".into(), Ty::Money(None)),
            ("weight".into(), Ty::Quantity(Some("kg".into()))),
            ("temperature".into(), Ty::Quantity(Some("degC".into()))),
            ("supplier_category".into(), Ty::Text),
            ("qty".into(), Ty::Int),
            ("needed_by".into(), Ty::Date),
        ]))
    }

    fn env() -> Env {
        Env::default().with("doc", request()).with("limit", Ty::Money(Some("INR".into()))).with("today", Ty::Date)
    }

    fn facts(doc: Value) -> Facts {
        Facts::default()
            .with("doc", Val::from_json(&doc, &request()))
            .with("limit", Val::Money(Decimal::from_str("100000.00").unwrap(), "INR".into()))
            .with("today", Val::Date(NaiveDate::from_ymd_opt(2026, 11, 8).unwrap()))
    }

    fn run(src: &str, doc: Value) -> Val {
        compile(src, &env()).unwrap_or_else(|e| panic!("{src}: {e}")).eval(&facts(doc))
    }

    #[test]
    fn money_compares_exactly_at_the_boundary() {
        let over = "doc.total > limit";
        let at = json!({"total": {"amount": "100000.00", "currency": "INR"}});
        let above = json!({"total": {"amount": "100000.01", "currency": "INR"}});
        assert_eq!(run(over, at), Val::Bool(false), "exactly the limit is not over it");
        assert_eq!(run(over, above), Val::Bool(true), "a paisa over is");
        assert_eq!(run("dec(\"0.1\") + dec(\"0.2\") == dec(\"0.3\")", json!({})), Val::Bool(true), "decimals are exact");
    }

    #[test]
    fn the_wrong_kind_of_quantity_is_refused_before_running() {
        let e = compile("doc.temperature > doc.weight", &env()).unwrap_err();
        assert_eq!(e.code, Code::TypeMismatch, "{e}");
        assert!(e.message.contains("degC") && e.message.contains("kg"), "{e}");
        assert_eq!(compile("doc.total > doc.weight", &env()).unwrap_err().code, Code::TypeMismatch, "money is not a weight");
        assert_eq!(compile("doc.total > 100", &env()).unwrap_err().code, Code::TypeMismatch, "money is not a bare number");
        assert_eq!(compile("limit > money(\"5\", \"USD\")", &env()).unwrap_err().code, Code::TypeMismatch, "rupees are not dollars");
    }

    #[test]
    fn missing_currency_is_unknown_not_zero() {
        let v = run("doc.total > limit", json!({"total": {"amount": "5"}}));
        assert!(matches!(&v, Val::Unknown(w) if w.contains("currency")), "{v:?}");
        let other = run("doc.total > limit", json!({"total": {"amount": "5", "currency": "USD"}}));
        assert!(other.is_unknown(), "dollars against a rupee limit at run time: {other:?}");
    }

    #[test]
    fn unknown_is_not_false() {
        let missing = json!({"supplier_category": "restricted"});
        assert!(run("doc.total > limit", missing.clone()).is_unknown(), "no total: not known to be under the limit");
        assert_eq!(run("doc.total > limit || doc.supplier_category == \"restricted\"", missing.clone()), Val::Bool(true), "known true wins an or");
        assert_eq!(run("doc.total > limit && doc.supplier_category == \"approved\"", missing.clone()), Val::Bool(false), "known false wins an and");
        assert!(run("!(doc.total > limit)", missing).is_unknown(), "not unknown is unknown");
        let c = compile("doc.total > limit", &env()).unwrap();
        assert!(c.decide(&facts(json!({}))).is_err(), "a caller is told it is not known");
    }

    #[test]
    fn fractions_are_written_exactly_or_not_at_all() {
        assert_eq!(compile("doc.qty * 2.5", &env()).unwrap_err().code, Code::TypeMismatch);
        assert_eq!(
            run("round(doc.total * dec(\"1.18\"), 2)", json!({"total": {"amount": "99.99", "currency": "INR"}})),
            Val::Money(Decimal::from_str("117.99").unwrap(), "INR".into())
        );
        assert_eq!(compile("doc.total / 3", &env()).unwrap_err().code, Code::TypeMismatch, "money is divided with its rounding said");
        assert_eq!(run("round(dec(\"2.5\"), 0) == dec(\"3\") && round_even(dec(\"2.5\"), 0) == dec(\"2\")", json!({})), Val::Bool(true));
    }

    #[test]
    fn names_fields_and_functions_are_checked() {
        assert_eq!(compile("doc.supplier", &env()).unwrap_err().code, Code::SchemaInvalid, "a misspelt field");
        assert_eq!(compile("salary > 0", &env()).unwrap_err().code, Code::SchemaInvalid, "a name nobody gave");
        assert_eq!(compile("http_get(\"x\")", &env()).unwrap_err().code, Code::CapabilityUnavailable, "no function reaches out");
        assert_eq!(compile("[1, 2].map(x, x * 2)", &env()).unwrap_err().code, Code::UnsupportedProfile, "no loops");
        assert_eq!(compile_as("doc.qty + 1", &env(), &Ty::Bool).unwrap_err().code, Code::TypeMismatch, "a condition must be yes/no");
    }

    #[test]
    fn dates_and_texts() {
        assert_eq!(run("days_between(today, doc.needed_by) < 3", json!({"needed_by": "2026-11-10"})), Val::Bool(true));
        assert_eq!(run("add_days(today, 30) == date(\"2026-12-08\")", json!({})), Val::Bool(true));
        assert_eq!(
            run(
                "doc.supplier_category.startsWith(\"restr\") && doc.supplier_category in [\"restricted\", \"watch\"]",
                json!({"supplier_category": "restricted"})
            ),
            Val::Bool(true)
        );
        assert_eq!(run("coalesce(doc.supplier_category, \"none\")", json!({})), Val::Text("none".into()));
    }

    #[test]
    fn what_an_expression_reads() {
        let c = compile("has(doc.total) && doc.total > limit && size(doc.items) > 0", &Env::default().with("doc", Ty::Any).with("limit", Ty::Any)).unwrap();
        assert_eq!(c.references().into_iter().collect::<Vec<_>>(), vec!["doc.items", "doc.total", "limit"]);
    }

    #[test]
    fn lists_are_equal_or_not_and_have_no_order() {
        assert_eq!(run("[1, 2] != [1, 3]", json!({})), Val::Bool(true));
        assert_eq!(run("[1, 2] == [1, 2]", json!({})), Val::Bool(true));
        assert_eq!(compile("[1] < [2]", &env()).unwrap_err().code, Code::TypeMismatch);
    }

    #[test]
    fn the_clock_is_a_fact_not_a_reading() {
        let c = compile("today == date(\"2026-11-08\")", &env()).unwrap();
        assert_eq!(c.eval(&facts(json!({}))), Val::Bool(true));
        assert!(c.eval(&Facts::default()).is_unknown(), "without today handed in, today is not known");
    }

    #[test]
    fn work_is_bounded() {
        let long = format!("{}1", "1 + ".repeat(1000));
        assert_eq!(compile(&long, &env()).unwrap_err().code, Code::SchemaInvalid, "a chain that would nest a thousand deep is refused, not run");
        let at_limit = format!("{}1", "1 + ".repeat(MAX_OPERATORS));
        assert_eq!(
            compile(&at_limit, &env()).unwrap().eval(&facts(json!({}))),
            Val::Int(MAX_OPERATORS as i64 + 1),
            "the longest allowed runs, on a test thread's small stack"
        );
        assert!(compile(&"(".repeat(40), &env()).is_err(), "nesting is bounded");
        assert!(compile(&"x".repeat(MAX_SOURCE + 1), &env()).is_err(), "length is bounded");
    }

    #[test]
    fn values_go_back_to_json_exactly() {
        let v = run("doc.total + money(\"0.10\", \"INR\")", json!({"total": {"amount": "0.20", "currency": "INR"}}));
        assert_eq!(v.to_json(), json!({"amount": "0.30", "currency": "INR"}));
    }
}
