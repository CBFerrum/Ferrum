//! CEL expressions: compiled once when metadata loads, evaluated per document.
//!
//! Variables available to every expression: `doc`, `old` (previous version or null), `parent`
//! (parent document for child rows, or null), `user` (`{id, email, roles, companies}`), `today`
//! (`YYYY-MM-DD`), `now` (RFC 3339) and `time`. Time is a fact the caller supplies in `user` (the kernel: the
//! workspace's local time, read once per evaluation), never read from a clock here: without it they are null
//! (FLC-COMP-003). Dates are ISO strings, so `>=` compares them correctly.

use crate::expr_fns::{self, ENV};
use cel::{Context, Program};
use serde_json::Value;
use std::fmt;
use std::sync::Arc;

pub const MAX_EXPR_LEN: usize = 4096;

/// The key under which `EffEntity::cel_view` lists the fields it read as 0 or false because they were left
/// empty, so that `isEmpty(doc.qty)` and `coalesce(doc.qty, 1)` can tell them from a 0 entered. No field can
/// have this name, and expressions never see it.
pub const EMPTY_FIELDS: &str = "\u{0}empty";
const MAX_NESTING: usize = 32;
/// Operator characters one expression may hold. A chain of operators nests as deep as it is long,
/// parentheses or not, and evaluation recurses that deep: a thousand-term chain overflowed a 2 MiB
/// stack (8 November 2026). The longest shipped expression has about twenty.
const MAX_OPERATORS: usize = 512;

/// Different linked records one expression may read through `links` (docs/builder/decisions.md BD-14).
pub const MAX_LINKS: usize = 5;

#[derive(Clone)]
pub struct Expr {
    source: Arc<str>,
    program: Arc<Program>,
    /// The link fields it reads as `links.<field>`, in the order written.
    links: Arc<[String]>,
    /// Whether it reads `old` and `parent`: one it does not read is bound as null, so a line's rule does not
    /// convert its whole record (lines within lines make that large) for nothing.
    reads_old: bool,
    reads_parent: bool,
    /// The functions of ours it calls (`expr_fns::NAMES`), bound when it runs.
    uses: Arc<[&'static str]>,
}

impl fmt::Debug for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Expr({})", self.source)
    }
}

/// Values bound to an evaluation.
pub struct Scope<'a> {
    pub doc: &'a Value,
    pub old: Option<&'a Value>,
    pub parent: Option<&'a Value>,
    pub user: &'a Value,
}

impl Expr {
    pub fn compile(source: &str) -> Result<Expr, String> {
        if source.trim().is_empty() {
            return Err("expression is empty".into());
        }
        if source.len() > MAX_EXPR_LEN {
            return Err(format!("expression longer than {MAX_EXPR_LEN} characters"));
        }
        let mut depth = 0usize;
        for c in source.chars() {
            match c {
                '(' | '[' | '{' => {
                    depth += 1;
                    if depth > MAX_NESTING {
                        return Err(format!("expression nested deeper than {MAX_NESTING} levels"));
                    }
                }
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if source.chars().filter(|c| "+-*/%<>=!&|?".contains(*c)).count() > MAX_OPERATORS {
            return Err(format!("expression holds more than {MAX_OPERATORS} operators: split it"));
        }
        let program = ENV.compile(source).map_err(|e| e.to_string())?;
        let mut links = Vec::new();
        link_names(program.expression(), &mut links)?;
        if links.len() > MAX_LINKS {
            return Err(format!("an expression reads at most {MAX_LINKS} linked records (links.<field>); this one reads {}", links.len()));
        }
        let refs = program.references();
        let (reads_old, reads_parent) = (refs.has_variable("old"), refs.has_variable("parent"));
        let uses = expr_fns::NAMES.iter().copied().filter(|n| refs.has_function(n)).collect();
        Ok(Expr { source: source.into(), program: Arc::new(program), links: links.into(), reads_old, reads_parent, uses })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// The link fields it reads as `links.<field>`: the caller binds `links` with those records.
    pub fn links(&self) -> &[String] {
        &self.links
    }

    pub fn eval(&self, scope: &Scope<'_>) -> Result<Value, String> {
        self.eval_with(scope, &[])
    }

    /// [`Expr::eval`] with more variables bound by name (a configured action's `params`, the `linked` record an
    /// effect writes).
    pub fn eval_with(&self, scope: &Scope<'_>, extra: &[(&str, &Value)]) -> Result<Value, String> {
        let mut ctx = Context::with_env(ENV.clone());
        let mut bound: Vec<(&str, &Value)> = vec![
            ("doc", scope.doc),
            ("old", scope.old.filter(|_| self.reads_old).unwrap_or(&Value::Null)),
            ("parent", scope.parent.filter(|_| self.reads_parent).unwrap_or(&Value::Null)),
            ("user", scope.user),
        ];
        // Time is the caller's fact, as the workspace sees it: never a clock read here (FLC-COMP-003).
        for key in ["today", "now", "time"] {
            bound.push((key, scope.user.get(key).unwrap_or(&Value::Null)));
        }
        bound.extend(extra.iter().copied());
        for name in self.uses.iter() {
            expr_fns::bind(&mut ctx, name, &bound);
        }
        for (name, v) in bound {
            ctx.add_variable_from_value(name.to_string(), json_to_cel(v));
        }
        let out = self.program.execute(&ctx).map_err(|e| e.to_string())?;
        Ok(cel_to_json(&out))
    }

    /// Evaluates to a boolean; `null` counts as `false`.
    pub fn eval_bool(&self, scope: &Scope<'_>) -> Result<bool, String> {
        self.eval_bool_with(scope, &[])
    }

    /// [`Expr::eval_bool`] with more variables bound by name.
    pub fn eval_bool_with(&self, scope: &Scope<'_>, extra: &[(&str, &Value)]) -> Result<bool, String> {
        match self.eval_with(scope, extra)? {
            Value::Bool(b) => Ok(b),
            Value::Null => Ok(false),
            other => Err(format!("expected a boolean, got {other}")),
        }
    }
}

/// The fields named as `links.<field>` (`has(links.<field>)` too), each once; `links` read any other way (indexed,
/// passed whole) is refused, so that what an expression reads is known before it runs.
fn link_names(e: &cel::IdedExpr, out: &mut Vec<String>) -> Result<(), String> {
    use cel::common::ast::{EntryExpr, Expr as E, IdedEntryExpr};
    let entries = |entries: &[IdedEntryExpr], out: &mut Vec<String>| -> Result<(), String> {
        for en in entries {
            match &en.expr {
                EntryExpr::StructField(f) => link_names(&f.value, out)?,
                EntryExpr::MapEntry(m) => {
                    link_names(&m.key, out)?;
                    link_names(&m.value, out)?;
                }
            }
        }
        Ok(())
    };
    match &e.expr {
        E::Select(s) if matches!(&s.operand.expr, E::Ident(n) if n == "links") => {
            if !out.contains(&s.field) {
                out.push(s.field.clone());
            }
        }
        E::Ident(n) if n == "links" => return Err("links is read as links.<link field>, naming the field".into()),
        E::Select(s) => link_names(&s.operand, out)?,
        E::Call(c) => {
            for x in c.target.as_deref().into_iter().chain(&c.args) {
                link_names(x, out)?;
            }
        }
        E::Comprehension(c) => {
            for x in [&c.iter_range, &c.accu_init, &c.loop_cond, &c.loop_step, &c.result] {
                link_names(x, out)?;
            }
        }
        E::List(l) => {
            for x in &l.elements {
                link_names(x, out)?;
            }
        }
        E::Map(m) => entries(&m.entries, out)?,
        E::Struct(s) => entries(&s.entries, out)?,
        E::Ident(_) | E::Literal(_) | E::Unspecified => {}
    }
    Ok(())
}

/// A JSON value as CEL sees it. A whole number is a signed integer, as CEL's own literals are, so that
/// `doc.days * 24` and `doc.qty > -1` work (`cel::to_value` made a positive one unsigned, and CEL does not mix
/// the two); only one beyond `i64` stays unsigned. Fractions are floats, as before.
fn json_to_cel(v: &Value) -> cel::Value {
    use cel::Value as C;
    match v {
        Value::Null => C::Null,
        Value::Bool(b) => C::Bool(*b),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => C::Int(i),
            (None, Some(u)) => C::UInt(u),
            _ => C::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => C::String(Arc::new(s.clone())),
        Value::Array(items) => C::List(Arc::new(items.iter().map(json_to_cel).collect())),
        Value::Object(m) => {
            // What `EffEntity::cel_view` notes as left empty is for `isEmpty` and `coalesce` alone: CEL never sees it.
            let map: std::collections::HashMap<cel::objects::Key, C> =
                m.iter().filter(|(k, _)| *k != EMPTY_FIELDS).map(|(k, v)| (k.clone().into(), json_to_cel(v))).collect();
            C::Map(map.into())
        }
    }
}

fn cel_to_json(v: &cel::Value) -> Value {
    use cel::Value as C;
    match v {
        C::Null => Value::Null,
        C::Bool(b) => Value::Bool(*b),
        C::Int(i) => Value::from(*i),
        C::UInt(u) => Value::from(*u),
        C::Float(f) => serde_json::Number::from_f64(*f).map(Value::Number).unwrap_or(Value::Null),
        C::String(s) => Value::String(s.to_string()),
        C::List(items) => Value::Array(items.iter().map(cel_to_json).collect()),
        C::Map(m) => {
            let mut out = serde_json::Map::new();
            for (k, v) in m.map.iter() {
                out.insert(format!("{k}").trim_matches('"').to_string(), cel_to_json(v));
            }
            Value::Object(out)
        }
        C::Timestamp(t) => Value::String(t.to_rfc3339()),
        C::Duration(d) => Value::String(format!("{}s", d.num_seconds())),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_too_long_to_evaluate_safely_is_refused() {
        let long = format!("{}1", "1 + ".repeat(1000));
        assert!(Expr::compile(&long).unwrap_err().contains("operators"));
        let ordinary = "doc.qty > 0 && (doc.status == 'Open' || doc.status == 'Draft') && doc.rate * doc.qty < 100000";
        assert!(Expr::compile(ordinary).is_ok());
    }
    use serde_json::json;

    fn scope<'a>(doc: &'a Value, user: &'a Value) -> Scope<'a> {
        Scope { doc, old: None, parent: None, user }
    }

    #[test]
    fn evaluates_conditions() {
        let doc = json!({"grand_total": 1500000.0, "po_no": "", "posting_date": "2026-01-02", "due_date": "2026-01-01"});
        let user = json!({"roles": ["Accounts Manager"]});
        let e = Expr::compile("doc.grand_total < 1000000.0 || doc.po_no != ''").unwrap();
        assert!(!e.eval_bool(&scope(&doc, &user)).unwrap());
        let d = Expr::compile("doc.due_date >= doc.posting_date").unwrap();
        assert!(!d.eval_bool(&scope(&doc, &user)).unwrap());
        let r = Expr::compile("'Accounts Manager' in user.roles").unwrap();
        assert!(r.eval_bool(&scope(&doc, &user)).unwrap());
        let t = Expr::compile("today.size() == 10").unwrap();
        let dated = json!({"roles": ["Accounts Manager"], "today": "2026-11-09"});
        assert!(t.eval_bool(&scope(&doc, &dated)).unwrap());
    }

    #[test]
    fn time_is_supplied_never_read_from_a_clock() {
        let doc = json!({"due_date": "2026-11-10"});
        let late = Expr::compile("doc.due_date < today").unwrap();
        assert!(late.eval_bool(&scope(&doc, &json!({"today": "2026-11-11"}))).unwrap());
        assert!(!late.eval_bool(&scope(&doc, &json!({"today": "2026-11-09"}))).unwrap(), "the same input, the same answer");
        let unsupplied = Expr::compile("today == null && now == null && time == null").unwrap();
        assert!(unsupplied.eval_bool(&scope(&doc, &json!({}))).unwrap(), "without the caller's time there is none");
    }

    /// A record's whole numbers are signed, as literals are (found rebuilding Governance and Risk as a pack).
    #[test]
    fn whole_numbers_are_signed() {
        let doc = json!({"days": 90, "debt": -5, "rate": 1.5, "big": u64::MAX});
        let user = json!({});
        let eval = |s: &str| Expr::compile(s).unwrap().eval(&scope(&doc, &user)).unwrap();
        assert_eq!(eval("doc.days * 24"), json!(2160));
        assert_eq!(eval("doc.days > 89 && doc.days >= doc.debt && doc.debt < 0"), json!(true));
        assert_eq!(eval("doc.debt * 2"), json!(-10));
        assert_eq!(eval("doc.days - 100"), json!(-10));
        assert_eq!(eval("doc.rate * 2.0"), json!(3.0), "fractions are floats as before");
        assert_eq!(eval("doc.big"), json!(u64::MAX), "beyond a signed integer, unsigned");
        assert_eq!(eval("string(doc.days * 24) + 'h'"), json!("2160h"));
    }

    #[test]
    fn rejects_bad_expressions() {
        assert!(Expr::compile("").is_err());
        assert!(Expr::compile("doc.a +").is_err());
        assert!(Expr::compile(&"(".repeat(40)).is_err());
        let doc = json!({});
        let user = json!({});
        assert!(Expr::compile("doc.missing > 1").unwrap().eval_bool(&scope(&doc, &user)).is_err());
    }

    #[test]
    fn the_linked_records_an_expression_reads_are_found() {
        let e = Expr::compile("links.type.documents.all(d, doc.documents.exists(r, r.document == d.document)) && has(links.office.name) && links.type != null")
            .unwrap();
        assert_eq!(e.links(), ["type", "office"]);
        assert!(Expr::compile("doc.qty > 0").unwrap().links().is_empty());
        assert_eq!(Expr::compile("[links.a, {'k': links.b}].size() > 0 && doc.x.map(r, links.c).size() > 0").unwrap().links(), ["a", "b", "c"]);
        for indexed in ["links['type'] != null", "links != null", "size(links) > 0", "'type' in links"] {
            assert!(Expr::compile(indexed).unwrap_err().contains("links.<link field>"), "{indexed}");
        }
        let six = "links.a == null && links.b == null && links.c == null && links.d == null && links.e == null && links.f == null";
        assert!(Expr::compile(six).unwrap_err().contains("at most 5"));
        let user = json!({});
        let doc = json!({});
        let linked = json!({"type": {"documents": [{"document": "ID"}]}});
        let reads = Expr::compile("links.type.documents.size() == 1").unwrap();
        assert!(reads.eval_bool_with(&scope(&doc, &user), &[("links", &linked)]).unwrap());
    }

    /// Dates (BL-19): on date and date-time fields, an empty date answering empty, months kept within the month.
    #[test]
    fn dates() {
        let doc = json!({"on": "2024-01-31", "at": "2026-03-15T09:30:00Z", "local": "2026-03-15 09:30:00", "none": null, "blank": "", "n": 13});
        let user = json!({});
        let eval = |s: &str| Expr::compile(s).unwrap().eval(&scope(&doc, &user)).unwrap();
        assert_eq!(eval("addMonths(doc.on, 1)"), json!("2024-02-29"), "31 January + 1 month, a leap year");
        assert_eq!(eval("addMonths('2025-01-31', 1)"), json!("2025-02-28"));
        assert_eq!(eval("addMonths(doc.on, -2)"), json!("2023-11-30"));
        assert_eq!(eval("addMonths(doc.on, doc.n)"), json!("2025-02-28"));
        assert_eq!(eval("addYears('2024-02-29', 1)"), json!("2025-02-28"));
        assert_eq!(eval("addDays(doc.on, 1)"), json!("2024-02-01"));
        assert_eq!(eval("doc.on.addDays(-31)"), json!("2023-12-31"), "as a method too");
        assert_eq!(eval("addDays(doc.at, 20)"), json!("2026-04-04T09:30:00Z"), "a date-time keeps its time and form");
        assert_eq!(eval("addMonths(doc.local, 1)"), json!("2026-04-15T09:30:00"));
        assert_eq!(eval("addDays(timestamp(doc.at), 1) == timestamp('2026-03-16T09:30:00Z')"), json!(true));
        assert_eq!(eval("[startOfMonth(doc.at), endOfMonth(doc.on), endOfMonth('2026-12-05')]"), json!(["2026-03-01", "2024-01-31", "2026-12-31"]));
        assert_eq!(eval("[weekday('2026-11-12'), weekday('2026-11-15')]"), json!([4, 7]), "Monday is 1, Sunday 7");
        assert_eq!(eval("[year(doc.at), month(doc.at), day(doc.on)]"), json!([2026, 3, 31]));
        assert_eq!(eval("daysBetween(doc.on, '2024-03-01')"), json!(30));
        assert_eq!(eval("daysBetween('2024-03-01', doc.on)"), json!(-30));
        assert_eq!(eval("[monthsBetween(doc.on, '2024-02-29'), monthsBetween(doc.on, '2024-02-28'), monthsBetween('2024-03-31', doc.on)]"), json!([1, 0, -2]));
        assert_eq!(eval("formatDate(doc.on, '%d %B %Y')"), json!("31 January 2024"));
        assert_eq!(eval("formatDate(doc.at, '%d/%m/%Y %H:%M')"), json!("15/03/2026 09:30"));
        for empty in [
            "addMonths(doc.none, 1)",
            "addDays(doc.blank, 1)",
            "addDays(doc.on, null)",
            "endOfMonth(doc.none)",
            "year(doc.blank)",
            "daysBetween(doc.none, doc.on)",
            "formatDate(doc.none, '%Y')",
        ] {
            assert_eq!(eval(empty), Value::Null, "{empty}: an empty date is empty, not an error");
        }
        let fails = |s: &str| Expr::compile(s).unwrap().eval(&scope(&doc, &user)).unwrap_err();
        assert!(fails("addDays('31/01/2024', 1)").contains("is not a date"));
        assert!(fails("addDays(doc.on, 1.5)").contains("whole number"));
        assert!(fails("formatDate(doc.on, '%H')").contains("pattern"));
        assert!(fails("addDays(doc.on)").contains("2 arguments"));
    }

    /// The long form the Government pack wrote before `addMonths` (2,479 characters for a permit's expiry) gives
    /// the same date, and still works.
    #[test]
    fn add_months_agrees_with_the_long_form() {
        let t = "timestamp(doc.d + 'T00:00:00Z')";
        let m = format!("({t}.getMonth() + int(doc.n))");
        let y = format!("({t}.getFullYear() + {m} / 12)");
        let mo = format!("({m} % 12 + 1)");
        let last = format!(
            "({mo} == 2 ? ({y} % 4 == 0 && ({y} % 100 != 0 || {y} % 400 == 0) ? 29 : 28) : ({mo} == 4 || {mo} == 6 || {mo} == 9 || {mo} == 11 ? 30 : 31))"
        );
        let long = format!(
            "string({y}) + '-' + ({mo} < 10 ? '0' : '') + string({mo}) + '-' + ({t}.getDate() < 10 ? '0' : '') + string({t}.getDate() < {last} ? {t}.getDate() : {last})"
        );
        let (long, short) = (Expr::compile(&long).unwrap(), Expr::compile("addMonths(doc.d, doc.n)").unwrap());
        let user = json!({});
        for d in ["2024-01-31", "2023-01-29", "2100-01-30", "2000-02-29", "2026-08-31", "2026-12-15", "2026-03-01"] {
            for n in [0, 1, 2, 6, 11, 12, 13, 25, 48] {
                let doc = json!({"d": d, "n": n});
                assert_eq!(long.eval(&scope(&doc, &user)).unwrap(), short.eval(&scope(&doc, &user)).unwrap(), "{d} + {n}");
            }
        }
    }

    /// Text (BL-20): CEL's own and its strings extension, with what they lack.
    #[test]
    fn text() {
        let doc = json!({"name": "  Ünal Şahin ", "code": "7", "docs": ["Site plan", "Proof"], "qty": 3, "none": null});
        let user = json!({});
        let eval = |s: &str| Expr::compile(s).unwrap().eval(&scope(&doc, &user)).unwrap();
        assert_eq!(eval("[lower(doc.name.trim()), doc.name.trim().upper(), lower(doc.none)]"), json!(["ünal şahin", "ÜNAL ŞAHIN", null]));
        assert_eq!(eval("'Check'.lowerAscii() == 'check' && 'a-b'.replace('-', '+') == 'a+b'"), json!(true));
        assert_eq!(eval("[size(doc.code), doc.name.contains('Şah'), 'APP-1'.startsWith('APP'), 'x.pdf'.endsWith('.pdf')]"), json!([1, true, true, true]));
        assert_eq!(eval("['a,b'.split(','), 'permit'.substring(0, 3), doc.docs.join(', ')]"), json!([["a", "b"], "per", "Site plan, Proof"]));
        assert_eq!(
            eval("[padStart(doc.code, 5, '0'), doc.code.padEnd(3), padStart('abcdef', 3, '0'), padStart(doc.none, 3)]"),
            json!(["00007", "7  ", "abcdef", null])
        );
        assert_eq!(eval("concat('No. ', doc.code, ' of ', doc.qty, doc.none, ': ', 1.5, ' ', true)"), json!("No. 7 of 3: 1.5 true"));
        assert_eq!(eval("'%s needs %d'.format([doc.code, doc.qty])"), json!("7 needs 3"));
    }

    #[test]
    fn expr_is_shareable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Expr>();
    }
}
