//! CEL expressions: compiled once when metadata loads, evaluated per document.
//!
//! Variables available to every expression: `doc`, `old` (previous version or null), `parent`
//! (parent document for child rows, or null), `user` (`{id, email, roles, companies}`), `today`
//! (`YYYY-MM-DD`, UTC) and `now` (RFC 3339). Dates are ISO strings, so `>=` compares them correctly.

use cel::{Context, Program};
use serde_json::Value;
use std::fmt;
use std::sync::Arc;

pub const MAX_EXPR_LEN: usize = 4096;
const MAX_NESTING: usize = 32;

#[derive(Clone)]
pub struct Expr {
    source: Arc<str>,
    program: Arc<Program>,
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
        let program = Program::compile(source).map_err(|e| e.to_string())?;
        Ok(Expr { source: source.into(), program: Arc::new(program) })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn eval(&self, scope: &Scope<'_>) -> Result<Value, String> {
        let mut ctx = Context::default();
        let bind = |ctx: &mut Context, name: &str, v: &Value| -> Result<(), String> {
            let cv = cel::to_value(v).map_err(|e| format!("cannot bind {name}: {e}"))?;
            ctx.add_variable_from_value(name.to_string(), cv);
            Ok(())
        };
        bind(&mut ctx, "doc", scope.doc)?;
        bind(&mut ctx, "old", scope.old.unwrap_or(&Value::Null))?;
        bind(&mut ctx, "parent", scope.parent.unwrap_or(&Value::Null))?;
        bind(&mut ctx, "user", scope.user)?;
        // "today" and "now" are local to the workspace: the kernel puts them in `user` (its time
        // zone); without them (no workspace context) UTC is used.
        let now = chrono::Utc::now();
        let local = |key: &str| scope.user.get(key).and_then(Value::as_str).map(str::to_string);
        ctx.add_variable_from_value("today".to_string(), local("today").unwrap_or_else(|| now.format("%Y-%m-%d").to_string()));
        ctx.add_variable_from_value("now".to_string(), local("now").unwrap_or_else(|| now.to_rfc3339()));
        let out = self.program.execute(&ctx).map_err(|e| e.to_string())?;
        Ok(cel_to_json(&out))
    }

    /// Evaluates to a boolean; `null` counts as `false`.
    pub fn eval_bool(&self, scope: &Scope<'_>) -> Result<bool, String> {
        match self.eval(scope)? {
            Value::Bool(b) => Ok(b),
            Value::Null => Ok(false),
            other => Err(format!("expected a boolean, got {other}")),
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
        assert!(t.eval_bool(&scope(&doc, &user)).unwrap());
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
    fn expr_is_shareable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Expr>();
    }
}
