//! Decision tables (package MASTER_PLAN §6, BUILDER_UX §6): typed inputs, rows with stable ids, a hit policy
//! and an explicit default.
//!
//! * **unique**: exactly one row may match; two matching is an error, found where it can be found before
//!   running (overlapping ranges) and refused when it happens anyway;
//! * **first**: rows are tried in order and the first that matches wins; a row that earlier rows always
//!   cover is reported unreachable;
//! * **collect**: every matching row's outcome, reduced by a named reducer (`list`, `count`, `sum`, `min`,
//!   `max`).
//!
//! A row is written either as ranges and sets per input (`match`), which can be analysed for overlaps and
//! gaps, or as an expression (`when`), which cannot: its coverage is said to be unproven, never assumed.
//! When an input is unknown and that could change which row wins, the decision is unknown. It never falls
//! through to the default as if the input had not matched.

use crate::expr::{self, Env, Facts, Ty, Val};
use crate::refusal::{Code, Refusal};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reducer {
    List,
    Count,
    Sum,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitPolicy {
    Unique,
    First,
    Collect(Reducer),
}

/// A range or set an input must fall in, for a row written as `match`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cond {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<Value>,
    #[serde(default, rename = "eq", skip_serializing_if = "Option::is_none")]
    pub equals: Option<Value>,
    #[serde(default, rename = "in", skip_serializing_if = "Vec::is_empty")]
    pub one_of: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub id: String,
    /// An expression over the inputs. Not analysable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Ranges and sets per input. Analysable.
    #[serde(default, rename = "match", skip_serializing_if = "BTreeMap::is_empty")]
    pub matches: BTreeMap<String, Cond>,
    pub outcome: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: String,
    /// What the input is, as an expression over the decision's scope (`snapshot.total`).
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub id: String,
    pub hit_policy: HitPolicy,
    pub inputs: Vec<Input>,
    pub rows: Vec<Row>,
    /// What it decides when no row matches. Required unless the policy collects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// What a decision came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// One outcome, from the rows that matched or the default.
    Decided { outcome: Value, rows: Vec<String> },
    /// It could not be decided, and why: an input unknown where it mattered, or two rows of a unique table.
    Unknown(String),
}

/// Something the builder shows about a table: a fault that stops publishing, or a note it must show.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<String>,
    /// A fault refuses publication; a note does not.
    pub fault: bool,
}

/// A test the builder suggests at a boundary: a suggestion, not proof, until someone gives its result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggested {
    pub input: String,
    pub value: Value,
    pub why: String,
}

/// A table checked against its scope, ready to evaluate.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub table: Table,
    inputs: Vec<(String, expr::Compiled)>,
    rows: Vec<(String, RowTest)>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone)]
enum RowTest {
    When(expr::Compiled),
    Match(BTreeMap<String, Cond>),
}

fn fault(code: &'static str, message: impl Into<String>, row: Option<&str>) -> Finding {
    Finding { code, message: message.into(), row: row.map(str::to_string), fault: true }
}

fn note(code: &'static str, message: impl Into<String>, row: Option<&str>) -> Finding {
    Finding { code, message: message.into(), row: row.map(str::to_string), fault: false }
}

/// A bound as a number, when it is one.
fn number(v: &Value) -> Option<Decimal> {
    match v {
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        Value::String(s) => Decimal::from_str(s).ok(),
        Value::Object(m) => m.get("amount").or_else(|| m.get("value")).and_then(number),
        _ => None,
    }
}

/// A numeric interval: (low, low included, high, high included). `None` is unbounded.
type Interval = (Option<Decimal>, bool, Option<Decimal>, bool);

fn interval(c: &Cond) -> Option<Interval> {
    if c.equals.is_some() || !c.one_of.is_empty() {
        return c.equals.as_ref().and_then(number).map(|v| (Some(v), true, Some(v), true));
    }
    let low = c.gte.as_ref().map(|v| (number(v), true)).or_else(|| c.gt.as_ref().map(|v| (number(v), false)));
    let high = c.lte.as_ref().map(|v| (number(v), true)).or_else(|| c.lt.as_ref().map(|v| (number(v), false)));
    let (lo, lo_in) = match low {
        Some((Some(v), inc)) => (Some(v), inc),
        Some((None, _)) => return None,
        None => (None, true),
    };
    let (hi, hi_in) = match high {
        Some((Some(v), inc)) => (Some(v), inc),
        Some((None, _)) => return None,
        None => (None, true),
    };
    Some((lo, lo_in, hi, hi_in))
}

fn overlaps(a: &Interval, b: &Interval) -> bool {
    // a's low is not above b's high, and b's low is not above a's high.
    let below = |lo: Option<Decimal>, lo_in: bool, hi: Option<Decimal>, hi_in: bool| match (lo, hi) {
        (Some(l), Some(h)) => l < h || (l == h && lo_in && hi_in),
        _ => true,
    };
    below(a.0, a.1, b.2, b.3) && below(b.0, b.1, a.2, a.3)
}

/// Whether two conditions on one input can both hold.
fn cond_overlap(a: &Cond, b: &Cond) -> bool {
    let set = |c: &Cond| -> Option<Vec<Value>> { if !c.one_of.is_empty() { Some(c.one_of.clone()) } else { c.equals.clone().map(|v| vec![v]) } };
    match (set(a), set(b), interval(a), interval(b)) {
        (Some(x), Some(y), _, _) => x.iter().any(|v| y.contains(v)),
        (Some(x), None, _, Some(ib)) => {
            x.iter().filter_map(number).any(|v| overlaps(&(Some(v), true, Some(v), true), &ib)) || x.iter().all(|v| number(v).is_none())
        }
        (None, Some(y), Some(ia), _) => {
            y.iter().filter_map(number).any(|v| overlaps(&ia, &(Some(v), true, Some(v), true))) || y.iter().all(|v| number(v).is_none())
        }
        (None, None, Some(ia), Some(ib)) => overlaps(&ia, &ib),
        _ => true,
    }
}

/// Whether condition `inner` always holds where `outer`... is implied by it (every value of inner is in outer).
fn covered_by(inner: &Cond, outer: &Cond) -> bool {
    match (interval(inner), interval(outer)) {
        (Some(i), Some(o)) => {
            let low_ok = match (o.0, i.0) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(ol), Some(il)) => ol < il || (ol == il && (o.1 || !i.1)),
            };
            let high_ok = match (o.2, i.2) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(oh), Some(ih)) => oh > ih || (oh == ih && (o.3 || !i.3)),
            };
            low_ok && high_ok
        }
        _ => !inner.one_of.is_empty() && inner.one_of.iter().all(|v| outer.one_of.contains(v)),
    }
}

/// Compiles a table in a scope: its inputs typed, its rows checked, its coverage analysed.
/// The most rows and inputs a table may have: its overlap and gap analysis compares rows pairwise.
pub const MAX_ROWS: usize = 256;
pub const MAX_INPUTS: usize = 16;

pub fn compile(table: &Table, scope: &Env) -> Result<Compiled, Refusal> {
    if table.rows.len() > MAX_ROWS || table.inputs.len() > MAX_INPUTS {
        return Err(Refusal::new(Code::SchemaInvalid, format!("{}: a table has at most {MAX_ROWS} rows and {MAX_INPUTS} inputs", table.id)));
    }
    let mut findings = vec![];
    let mut seen = std::collections::BTreeSet::new();
    let mut inputs = vec![];
    let mut env = Env::default();
    for i in &table.inputs {
        if !seen.insert(i.name.clone()) {
            return Err(Refusal::new(Code::SchemaInvalid, format!("{}: the input {} is named twice", table.id, i.name)));
        }
        let c = expr::compile(&i.value, scope).map_err(|e| Refusal::new(e.code, format!("{} input {}: {}", table.id, i.name, e.message)))?;
        env = env.with(&i.name, c.ty.clone());
        inputs.push((i.name.clone(), c));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut rows = vec![];
    if table.rows.is_empty() {
        return Err(Refusal::new(Code::SchemaInvalid, format!("{} has no rows", table.id)));
    }
    for r in &table.rows {
        if !ids.insert(r.id.clone()) {
            return Err(Refusal::new(Code::SchemaInvalid, format!("{}: the row {} is there twice", table.id, r.id)));
        }
        let test = match (&r.when, r.matches.is_empty()) {
            (Some(w), true) => {
                RowTest::When(expr::compile_as(w, &env, &Ty::Bool).map_err(|e| Refusal::new(e.code, format!("{} row {}: {}", table.id, r.id, e.message)))?)
            }
            (None, false) => {
                for name in r.matches.keys() {
                    if !seen.contains(name) {
                        return Err(Refusal::new(Code::SchemaInvalid, format!("{} row {} matches {name}, which is not an input", table.id, r.id)));
                    }
                }
                RowTest::Match(r.matches.clone())
            }
            (None, true) => RowTest::Match(BTreeMap::new()),
            (Some(_), false) => return Err(Refusal::new(Code::SchemaInvalid, format!("{} row {} has both when and match: use one", table.id, r.id))),
        };
        rows.push((r.id.clone(), test));
    }
    if table.default.is_none() && !matches!(table.hit_policy, HitPolicy::Collect(_)) {
        findings.push(fault("missing_default", format!("{} says nothing for the cases no row matches: give it a default", table.id), None));
    }
    analyse(table, &rows, &mut findings);
    Ok(Compiled { table: table.clone(), inputs, rows, findings })
}

fn analyse(table: &Table, rows: &[(String, RowTest)], findings: &mut Vec<Finding>) {
    let structured: Vec<(&String, &BTreeMap<String, Cond>)> = rows
        .iter()
        .filter_map(|(id, t)| match t {
            RowTest::Match(m) => Some((id, m)),
            RowTest::When(_) => None,
        })
        .collect();
    if structured.len() < rows.len() {
        findings.push(note("coverage_unproven", format!("{}: rows written as expressions cannot be checked for overlaps or gaps; test them", table.id), None));
    }
    // Two rows overlap when every input either is unconstrained in one of them or has overlapping conditions.
    let both = |a: &BTreeMap<String, Cond>, b: &BTreeMap<String, Cond>| a.iter().all(|(k, ca)| b.get(k).is_none_or(|cb| cond_overlap(ca, cb)));
    for (i, (ida, a)) in structured.iter().enumerate() {
        for (idb, b) in structured.iter().skip(i + 1) {
            if !both(a, b) {
                continue;
            }
            match table.hit_policy {
                HitPolicy::Unique => {
                    findings.push(fault("rows_overlap", format!("{}: rows {ida} and {idb} can both match, and the table takes one", table.id), Some(idb)))
                }
                HitPolicy::First => {
                    // b is unreachable when every one of a's conditions covers b's for that input.
                    if a.iter().all(|(k, ca)| b.get(k).is_some_and(|cb| covered_by(cb, ca))) {
                        findings.push(fault(
                            "row_unreachable",
                            format!("{}: row {idb} can never win: row {ida} before it always matches first", table.id),
                            Some(idb),
                        ));
                    }
                }
                HitPolicy::Collect(_) => {}
            }
        }
    }
    // Gaps over one numeric input that every structured row constrains, when there is no default.
    if table.default.is_none() && structured.len() == rows.len() && table.inputs.len() == 1 {
        let name = &table.inputs[0].name;
        let mut ranges: Vec<Interval> = structured.iter().filter_map(|(_, m)| m.get(name).and_then(interval)).collect();
        if ranges.len() == structured.len() && !ranges.is_empty() {
            // From minus infinity upwards: unbounded lows first, then by low.
            ranges.sort_by(|a, b| match (a.0, b.0) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, _) => std::cmp::Ordering::Less,
                (_, None) => std::cmp::Ordering::Greater,
                (Some(x), Some(y)) => x.cmp(&y),
            });
            // How far up the rows cover so far: `None` before any, `Some(None)` to plus infinity.
            let mut reach: Option<Option<(Decimal, bool)>> = None;
            for r in &ranges {
                let gap = match (reach, r.0) {
                    (None, Some(l)) => Some(format!("below {l}")),
                    (Some(Some((u, inc))), Some(l)) if l > u || (l == u && !inc && !r.1) => Some(format!("between {u} and {l}")),
                    _ => None,
                };
                if let Some(where_) = gap {
                    findings.push(fault("gap", format!("{}: no row matches {name} {where_}", table.id), None));
                    return;
                }
                reach = Some(match (reach.flatten(), r.2) {
                    (_, None) => None,
                    (Some((u, inc)), Some(h)) if u > h || (u == h && inc) => Some((u, inc)),
                    (_, Some(h)) => Some((h, r.3)),
                });
                if reach == Some(None) {
                    return;
                }
            }
            if let Some(Some((u, _))) = reach {
                findings.push(fault("gap", format!("{}: no row matches {name} above {u}", table.id), None));
            }
        }
    }
}

impl Compiled {
    /// Faults that refuse publication.
    pub fn faults(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.fault)
    }

    /// Decides for `facts`.
    pub fn decide(&self, facts: &Facts) -> Outcome {
        let mut env = Facts::default();
        for (name, c) in &self.inputs {
            env = env.with(name, c.eval(facts));
        }
        let mut matched: Vec<(String, Value)> = vec![];
        let mut unknown: Option<String> = None;
        for ((id, test), row) in self.rows.iter().zip(&self.table.rows) {
            let hit = match test {
                RowTest::When(c) => c.decide(&env),
                RowTest::Match(m) => match_row(m, &env),
            };
            match hit {
                Ok(true) => {
                    if self.table.hit_policy == HitPolicy::First {
                        // An unknown row before the first match could have been the first match.
                        return match unknown {
                            Some(why) => Outcome::Unknown(why),
                            None => Outcome::Decided { outcome: row.outcome.clone(), rows: vec![id.clone()] },
                        };
                    }
                    matched.push((id.clone(), row.outcome.clone()));
                }
                Ok(false) => {}
                Err(why) => {
                    unknown.get_or_insert(format!("row {id}: {why}"));
                }
            }
        }
        if let Some(why) = unknown {
            return Outcome::Unknown(why);
        }
        match &self.table.hit_policy {
            HitPolicy::Unique if matched.len() > 1 => Outcome::Unknown(format!(
                "rows {} all match, and the table takes one",
                matched.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", ")
            )),
            HitPolicy::Unique | HitPolicy::First => match matched.pop() {
                Some((id, outcome)) => Outcome::Decided { outcome, rows: vec![id] },
                None => match &self.table.default {
                    Some(d) => Outcome::Decided { outcome: d.clone(), rows: vec![] },
                    None => Outcome::Unknown("no row matches and there is no default".into()),
                },
            },
            HitPolicy::Collect(reducer) => {
                let rows: Vec<String> = matched.iter().map(|(id, _)| id.clone()).collect();
                let outcomes: Vec<Value> = matched.into_iter().map(|(_, o)| o).collect();
                let numbers = || outcomes.iter().filter_map(number).collect::<Vec<_>>();
                let outcome = match reducer {
                    Reducer::List => Value::Array(outcomes.clone()),
                    Reducer::Count => Value::from(outcomes.len()),
                    Reducer::Sum => Value::String(numbers().into_iter().sum::<Decimal>().to_string()),
                    Reducer::Min => numbers().into_iter().min().map(|d| Value::String(d.to_string())).unwrap_or(Value::Null),
                    Reducer::Max => numbers().into_iter().max().map(|d| Value::String(d.to_string())).unwrap_or(Value::Null),
                };
                if outcomes.is_empty()
                    && let Some(d) = &self.table.default
                {
                    return Outcome::Decided { outcome: d.clone(), rows };
                }
                Outcome::Decided { outcome, rows }
            }
        }
    }

    /// Tests the builder suggests: just below, at and just above each bound, and a missing value.
    pub fn suggest(&self) -> Vec<Suggested> {
        let mut out = vec![];
        for r in &self.table.rows {
            for (input, c) in &r.matches {
                for bound in [&c.gte, &c.gt, &c.lte, &c.lt, &c.equals].into_iter().flatten() {
                    if let Some(v) = number(bound) {
                        let step = Decimal::new(1, v.scale());
                        for (value, why) in [(v - step, "just below"), (v, "exactly at"), (v + step, "just above")] {
                            let s = Suggested { input: input.clone(), value: with_number(bound, value), why: format!("{why} the bound of row {}", r.id) };
                            if !out.contains(&s) {
                                out.push(s);
                            }
                        }
                    }
                }
            }
        }
        for i in &self.table.inputs {
            out.push(Suggested { input: i.name.clone(), value: Value::Null, why: "missing: the decision must say it does not know".into() });
        }
        out
    }
}

/// `bound` with its number replaced, keeping a money's currency or a quantity's unit.
fn with_number(bound: &Value, v: Decimal) -> Value {
    match bound {
        Value::Object(m) if m.contains_key("amount") => {
            let mut m = m.clone();
            m.insert("amount".into(), Value::String(v.to_string()));
            Value::Object(m)
        }
        Value::Object(m) if m.contains_key("value") => {
            let mut m = m.clone();
            m.insert("value".into(), Value::String(v.to_string()));
            Value::Object(m)
        }
        _ => Value::String(v.to_string()),
    }
}

fn match_row(m: &BTreeMap<String, Cond>, env: &Facts) -> Result<bool, String> {
    for (name, c) in m {
        let v = env.vars.get(name).cloned().unwrap_or(Val::Null);
        if let Val::Unknown(why) = &v {
            return Err(format!("{name} is not known: {why}"));
        }
        if matches!(v, Val::Null) {
            return Err(format!("{name} is missing"));
        }
        if !cond_holds(c, &v)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn cond_holds(c: &Cond, v: &Val) -> Result<bool, String> {
    let expected_unit = |bound: &Value| -> Option<String> { bound.get("currency").or_else(|| bound.get("unit")).and_then(Value::as_str).map(str::to_string) };
    let (value, unit) = match v {
        Val::Int(i) => (Some(Decimal::from(*i)), None),
        Val::Dec(d) => (Some(*d), None),
        Val::Money(a, c) | Val::Qty(a, c) => (Some(*a), Some(c.clone())),
        _ => (None, None),
    };
    let numeric = |bound: &Value| -> Result<Decimal, String> {
        if let (Some(want), Some(have)) = (expected_unit(bound), &unit)
            && &want != have
        {
            return Err(format!("{have} against a bound in {want}: convert first"));
        }
        let b = number(bound).ok_or_else(|| format!("{bound} is not a number"))?;
        value.ok_or_else(|| "a number was needed".to_string()).map(|_| b)
    };
    if let Some(b) = &c.gte {
        let nb = numeric(b)?;
        if value.is_some_and(|x| x < nb) {
            return Ok(false);
        }
    }
    if let Some(b) = &c.gt {
        let nb = numeric(b)?;
        if value.is_some_and(|x| x <= nb) {
            return Ok(false);
        }
    }
    if let Some(b) = &c.lte {
        let nb = numeric(b)?;
        if value.is_some_and(|x| x > nb) {
            return Ok(false);
        }
    }
    if let Some(b) = &c.lt {
        let nb = numeric(b)?;
        if value.is_some_and(|x| x >= nb) {
            return Ok(false);
        }
    }
    let same = |b: &Value| -> bool {
        match (number(b), value) {
            (Some(n), Some(x)) => n == x,
            _ => b == &v.to_json(),
        }
    };
    if let Some(b) = &c.equals
        && !same(b)
    {
        return Ok(false);
    }
    if !c.one_of.is_empty() && !c.one_of.iter().any(same) {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> Env {
        Env::default().with(
            "snapshot",
            Ty::Record(BTreeMap::from([("total".into(), Ty::Money(Some("INR".into()))), ("category".into(), Ty::Text), ("qty".into(), Ty::Int)])),
        )
    }

    fn facts(snapshot: Value) -> Facts {
        let ty = scope().vars["snapshot"].clone();
        Facts::default().with("snapshot", Val::from_json(&snapshot, &ty))
    }

    fn inr(a: &str) -> Value {
        json!({"amount": a, "currency": "INR"})
    }

    /// The package's purchase policy: above a money threshold, or a restricted supplier category, needs review.
    fn review_table(policy: HitPolicy) -> Table {
        serde_json::from_value(json!({
            "id": "requires_review",
            "hit_policy": serde_json::to_value(&policy).unwrap(),
            "inputs": [{"name": "total", "value": "snapshot.total"}, {"name": "category", "value": "snapshot.category"}],
            "rows": [
                {"id": "high_amount", "match": {"total": {"gt": inr("100000.00")}}, "outcome": "review"},
                {"id": "restricted", "match": {"category": {"in": ["restricted"]}, "total": {"lte": inr("100000.00")}}, "outcome": "review"}
            ],
            "default": "proceed"
        }))
        .unwrap()
    }

    #[test]
    fn the_threshold_decides_exactly() {
        let t = compile(&review_table(HitPolicy::Unique), &scope()).unwrap();
        assert!(t.faults().next().is_none(), "{:?}", t.findings);
        let decide = |total: &str, category: &str| t.decide(&facts(json!({"total": inr(total), "category": category})));
        assert_eq!(decide("100000.00", "approved"), Outcome::Decided { outcome: json!("proceed"), rows: vec![] }, "exactly the limit proceeds");
        assert_eq!(decide("100000.01", "approved"), Outcome::Decided { outcome: json!("review"), rows: vec!["high_amount".into()] });
        assert_eq!(decide("10.00", "restricted"), Outcome::Decided { outcome: json!("review"), rows: vec!["restricted".into()] });
    }

    #[test]
    fn a_missing_currency_or_value_is_unknown_not_the_default() {
        let t = compile(&review_table(HitPolicy::Unique), &scope()).unwrap();
        assert!(matches!(t.decide(&facts(json!({"total": {"amount": "5"}, "category": "approved"}))), Outcome::Unknown(_)), "no currency: not decided");
        assert!(matches!(t.decide(&facts(json!({"category": "approved"}))), Outcome::Unknown(_)), "no total: not 'proceed'");
        assert!(
            matches!(t.decide(&facts(json!({"total": {"amount": "5", "currency": "USD"}, "category": "approved"}))), Outcome::Unknown(_)),
            "dollars against a rupee bound"
        );
    }

    #[test]
    fn overlapping_rows_are_found_before_publishing() {
        let mut table = review_table(HitPolicy::Unique);
        table.rows[1].matches.remove("total");
        let t = compile(&table, &scope()).unwrap();
        let f: Vec<&str> = t.faults().map(|f| f.code).collect();
        assert_eq!(f, ["rows_overlap"], "a restricted supplier above the limit matches both");
        // And if it ran anyway, two matches is not a decision.
        assert!(matches!(t.decide(&facts(json!({"total": inr("200000.00"), "category": "restricted"}))), Outcome::Unknown(why) if why.contains("high_amount")));
    }

    #[test]
    fn a_row_earlier_rows_cover_is_unreachable() {
        let table: Table = serde_json::from_value(json!({
            "id": "band", "hit_policy": "first", "inputs": [{"name": "qty", "value": "snapshot.qty"}],
            "rows": [
                {"id": "any_positive", "match": {"qty": {"gt": 0}}, "outcome": "small"},
                {"id": "large", "match": {"qty": {"gte": 100}}, "outcome": "large"}
            ],
            "default": "none"
        }))
        .unwrap();
        let t = compile(&table, &scope()).unwrap();
        assert_eq!(t.faults().map(|f| (f.code, f.row.clone())).collect::<Vec<_>>(), [("row_unreachable", Some("large".into()))]);
    }

    #[test]
    fn gaps_and_a_missing_default_are_faults() {
        let table: Table = serde_json::from_value(json!({
            "id": "bands", "hit_policy": "unique", "inputs": [{"name": "qty", "value": "snapshot.qty"}],
            "rows": [
                {"id": "low", "match": {"qty": {"gte": 0, "lt": 10}}, "outcome": "low"},
                {"id": "high", "match": {"qty": {"gte": 20}}, "outcome": "high"}
            ]
        }))
        .unwrap();
        let t = compile(&table, &scope()).unwrap();
        let codes: Vec<&str> = t.faults().map(|f| f.code).collect();
        assert!(codes.contains(&"missing_default"), "{codes:?}");
        assert!(codes.contains(&"gap"), "between 10 and 20, and below 0: {codes:?}");
        let mut covered = table.clone();
        covered.default = Some(json!("other"));
        assert!(compile(&covered, &scope()).unwrap().faults().next().is_none(), "a default covers the gaps");
    }

    #[test]
    fn expression_rows_are_never_called_covered() {
        let table: Table = serde_json::from_value(json!({
            "id": "e", "hit_policy": "first", "inputs": [{"name": "qty", "value": "snapshot.qty"}],
            "rows": [{"id": "big", "when": "qty > 10", "outcome": "big"}], "default": "small"
        }))
        .unwrap();
        let t = compile(&table, &scope()).unwrap();
        assert!(t.findings.iter().any(|f| f.code == "coverage_unproven" && !f.fault));
        assert_eq!(t.decide(&facts(json!({"qty": 11}))), Outcome::Decided { outcome: json!("big"), rows: vec!["big".into()] });
        assert!(matches!(t.decide(&facts(json!({}))), Outcome::Unknown(_)), "a missing qty is not small");
    }

    #[test]
    fn first_waits_on_an_unknown_row_before_it() {
        let table: Table = serde_json::from_value(json!({
            "id": "f", "hit_policy": "first", "inputs": [{"name": "qty", "value": "snapshot.qty"}, {"name": "category", "value": "snapshot.category"}],
            "rows": [
                {"id": "by_qty", "match": {"qty": {"gt": 5}}, "outcome": "a"},
                {"id": "by_category", "match": {"category": {"eq": "x"}}, "outcome": "b"}
            ],
            "default": "c"
        }))
        .unwrap();
        let t = compile(&table, &scope()).unwrap();
        assert!(matches!(t.decide(&facts(json!({"category": "x"}))), Outcome::Unknown(_)), "the first row might have matched");
    }

    #[test]
    fn collect_reduces_by_name() {
        let table: Table = serde_json::from_value(json!({
            "id": "fees", "hit_policy": {"collect": "sum"}, "inputs": [{"name": "qty", "value": "snapshot.qty"}],
            "rows": [
                {"id": "base", "match": {"qty": {"gte": 0}}, "outcome": "10.50"},
                {"id": "bulk", "match": {"qty": {"gte": 100}}, "outcome": "2.25"}
            ]
        }))
        .unwrap();
        let t = compile(&table, &scope()).unwrap();
        assert_eq!(t.decide(&facts(json!({"qty": 150}))), Outcome::Decided { outcome: json!("12.75"), rows: vec!["base".into(), "bulk".into()] });
    }

    #[test]
    fn boundary_tests_are_suggested_with_their_currency() {
        let t = compile(&review_table(HitPolicy::Unique), &scope()).unwrap();
        let s = t.suggest();
        for a in ["99999.99", "100000.00", "100000.01"] {
            assert!(s.iter().any(|x| x.input == "total" && x.value == inr(a)), "{a}: {s:?}");
        }
        assert!(s.iter().any(|x| x.input == "total" && x.value.is_null()), "and a missing total");
    }

    #[test]
    fn a_table_refers_only_to_what_it_has() {
        let mut table = review_table(HitPolicy::Unique);
        table.rows[0].matches.insert("supplier".into(), Cond::default());
        assert_eq!(compile(&table, &scope()).unwrap_err().code, Code::SchemaInvalid);
        let mut dup = review_table(HitPolicy::Unique);
        dup.rows[1].id = "high_amount".into();
        assert_eq!(compile(&dup, &scope()).unwrap_err().code, Code::SchemaInvalid);
    }
}
