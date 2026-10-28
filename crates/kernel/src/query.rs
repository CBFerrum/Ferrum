//! List queries: a JSON filter DSL compiled to parameterized SQL with permission predicates.
//!
//! Filters: `[["field", "op", value], ...]` or `{"field": value}`. Paths may go one level deep:
//! `items.item` (any child row matches) or `customer.customer_group` (linked record matches).
//! Ops: `=`, `!=`, `>`, `>=`, `<`, `<=`, `in`, `not in`, `like`, `not like`, `between`, `is` (`set`/`not set`).

use crate::ctx::RowScope;
use crate::ddl::q;
use crate::error::{Error, Result};
use crate::store::{SYSTEM_KEYS, entity_predicate, field_expr};
use crate::value::{normalize, parse_datetime};
use base64::Engine as _;
use ferrum_meta::{EffEntity, FieldDef, FieldType, Schema};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

pub const MAX_LIMIT: u32 = 500;
pub const MAX_OFFSET: u32 = 100_000;
const MAX_FILTERS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    NotIn,
    Like,
    NotLike,
    Between,
    Is,
}

impl Op {
    fn parse(s: &str) -> Result<Op> {
        Ok(match s.trim().to_lowercase().as_str() {
            "=" | "==" | "eq" => Op::Eq,
            "!=" | "<>" | "ne" => Op::Ne,
            ">" | "gt" => Op::Gt,
            ">=" | "gte" => Op::Gte,
            "<" | "lt" => Op::Lt,
            "<=" | "lte" => Op::Lte,
            "in" => Op::In,
            "not in" | "not_in" => Op::NotIn,
            "like" => Op::Like,
            "not like" | "not_like" => Op::NotLike,
            "between" => Op::Between,
            "is" => Op::Is,
            other => return Err(Error::invalid(format!("unknown filter operator '{other}'"))),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Filter {
    pub field: String,
    pub op: Op,
    pub value: Value,
}

impl Filter {
    pub fn eq(field: &str, value: impl Into<Value>) -> Filter {
        Filter { field: field.into(), op: Op::Eq, value: value.into() }
    }
}

pub fn parse_filters(v: &Value) -> Result<Vec<Filter>> {
    let mut out = Vec::new();
    match v {
        Value::Null => {}
        Value::Object(m) => {
            for (k, val) in m {
                match val {
                    Value::Array(a) if a.len() == 2 && a[0].is_string() => {
                        out.push(Filter { field: k.clone(), op: Op::parse(a[0].as_str().unwrap_or_default())?, value: a[1].clone() })
                    }
                    _ => out.push(Filter { field: k.clone(), op: Op::Eq, value: val.clone() }),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                let a = item.as_array().ok_or_else(|| Error::invalid("each filter must be [field, op, value]"))?;
                let field = a.first().and_then(Value::as_str).ok_or_else(|| Error::invalid("filter field must be a string"))?;
                match a.len() {
                    2 => out.push(Filter { field: field.into(), op: Op::Eq, value: a[1].clone() }),
                    3 => out.push(Filter {
                        field: field.into(),
                        op: Op::parse(a[1].as_str().ok_or_else(|| Error::invalid("filter operator must be a string"))?)?,
                        value: a[2].clone(),
                    }),
                    _ => return Err(Error::invalid("each filter must be [field, op, value]")),
                }
            }
        }
        _ => return Err(Error::invalid("filters must be a list or an object")),
    }
    if out.len() > MAX_FILTERS {
        return Err(Error::invalid(format!("at most {MAX_FILTERS} filters")));
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub filters: Vec<Filter>,
    pub fields: Option<Vec<String>>,
    pub sort: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub cursor: Option<String>,
    pub search: Option<String>,
    pub with_titles: bool,
    pub with_count: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Page {
    pub rows: Vec<crate::Doc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
}

#[derive(Clone, Copy)]
enum Kind {
    Field(FieldType),
    Uuid,
    Text,
    SmallInt,
    Int,
    Timestamp,
}

fn system_kind(name: &str) -> Option<Kind> {
    Some(match name {
        "id" | "created_by" | "updated_by" | "amended_from" => Kind::Uuid,
        "number" | "workflow_state" => Kind::Text,
        "docstatus" => Kind::SmallInt,
        "version" => Kind::Int,
        "created_at" | "updated_at" => Kind::Timestamp,
        _ => return None,
    })
}

fn kind_of(ent: &EffEntity, name: &str) -> Option<Kind> {
    system_kind(name).or_else(|| ent.field(name).filter(|f| f.ftype != FieldType::Table).map(|f| Kind::Field(f.ftype)))
}

fn bind_scalar(qb: &mut QueryBuilder<Postgres>, kind: Kind, field: Option<&FieldDef>, v: &Value) -> Result<()> {
    let bad = |m: String| Error::invalid(format!("filter value: {m}"));
    match kind {
        Kind::Uuid => {
            let s = v.as_str().ok_or_else(|| bad("expected a record id".into()))?;
            qb.push_bind(Uuid::parse_str(s).map_err(|_| bad(format!("'{s}' is not a record id")))?);
        }
        Kind::Text => {
            qb.push_bind(match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
        }
        Kind::SmallInt | Kind::Int => {
            let i = v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).ok_or_else(|| bad("expected a whole number".into()))?;
            qb.push_bind(i);
        }
        Kind::Timestamp => {
            let s = v.as_str().ok_or_else(|| bad("expected a timestamp".into()))?;
            let t = parse_datetime(s).or_else(|_| crate::value::parse_date(s).map(|d| d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc())).map_err(bad)?;
            qb.push_bind(t);
        }
        Kind::Field(ft) => {
            let f = field.ok_or_else(|| Error::Internal("field def missing".into()))?;
            let mut lenient = f.clone();
            lenient.min = None;
            lenient.max = None;
            lenient.options = Vec::new();
            if ft == FieldType::Select {
                lenient.ftype = FieldType::Data;
            }
            let n = normalize(&lenient, v).map_err(bad)?;
            match ft {
                FieldType::Int => {
                    qb.push_bind(n.as_i64().unwrap_or_default());
                }
                FieldType::Decimal | FieldType::Currency | FieldType::Percent => {
                    qb.push_bind(n.as_str().unwrap_or("0").to_string()).push("::numeric");
                }
                FieldType::Date => {
                    qb.push_bind(n.as_str().unwrap_or_default().to_string()).push("::date");
                }
                FieldType::Datetime => {
                    qb.push_bind(n.as_str().unwrap_or_default().to_string()).push("::timestamptz");
                }
                FieldType::Check => {
                    qb.push_bind(n.as_bool().unwrap_or(false));
                }
                FieldType::Link => {
                    qb.push_bind(Uuid::parse_str(n.as_str().unwrap_or_default()).map_err(|_| bad("bad id".into()))?);
                }
                FieldType::Json => {
                    qb.push_bind(n).push("::jsonb");
                }
                _ => {
                    qb.push_bind(n.as_str().map(str::to_string).unwrap_or_else(|| n.to_string()));
                }
            }
        }
    }
    Ok(())
}

fn push_condition(qb: &mut QueryBuilder<Postgres>, expr: &str, kind: Kind, field: Option<&FieldDef>, f: &Filter) -> Result<()> {
    let is_text = matches!(kind, Kind::Text) || matches!(kind, Kind::Field(t) if t.is_text());
    match f.op {
        Op::Is => {
            let set = match f.value.as_str().map(|s| s.to_lowercase()) {
                Some(s) if s == "set" => true,
                Some(s) if s == "not set" => false,
                _ => return Err(Error::invalid("'is' expects 'set' or 'not set'")),
            };
            match (set, is_text) {
                (true, true) => qb.push(format!("({expr} is not null and {expr} <> '')")),
                (false, true) => qb.push(format!("({expr} is null or {expr} = '')")),
                (true, false) => qb.push(format!("{expr} is not null")),
                (false, false) => qb.push(format!("{expr} is null")),
            };
        }
        Op::Like | Op::NotLike => {
            let s = f.value.as_str().ok_or_else(|| Error::invalid("like expects text"))?;
            if s.len() > 200 {
                return Err(Error::invalid("like pattern too long"));
            }
            let pat = if s.contains('%') { s.to_string() } else { format!("%{s}%") };
            qb.push(format!("{expr}::text {} ", if f.op == Op::Like { "ilike" } else { "not ilike" })).push_bind(pat);
        }
        Op::In | Op::NotIn => {
            let items = f.value.as_array().ok_or_else(|| Error::invalid("'in' expects a list"))?;
            if items.is_empty() {
                qb.push(if f.op == Op::In { "false" } else { "true" });
                return Ok(());
            }
            if items.len() > 1000 {
                return Err(Error::invalid("'in' lists are limited to 1000 values"));
            }
            qb.push(format!("{expr} {} (", if f.op == Op::In { "in" } else { "not in" }));
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    qb.push(", ");
                }
                bind_scalar(qb, kind, field, item)?;
            }
            qb.push(")");
        }
        Op::Between => {
            let a = f.value.as_array().filter(|a| a.len() == 2).ok_or_else(|| Error::invalid("'between' expects [from, to]"))?;
            qb.push(format!("{expr} between "));
            bind_scalar(qb, kind, field, &a[0])?;
            qb.push(" and ");
            bind_scalar(qb, kind, field, &a[1])?;
        }
        op => {
            if f.value.is_null() {
                qb.push(format!("{expr} is {}null", if op == Op::Ne { "not " } else { "" }));
                return Ok(());
            }
            let sym = match op {
                Op::Eq => "=",
                Op::Ne => "<>",
                Op::Gt => ">",
                Op::Gte => ">=",
                Op::Lt => "<",
                _ => "<=",
            };
            if op == Op::Ne {
                qb.push(format!("{expr} is distinct from "));
            } else {
                qb.push(format!("{expr} {sym} "));
            }
            bind_scalar(qb, kind, field, &f.value)?;
        }
    }
    Ok(())
}

/// Appends ` and <condition>` for one filter on table alias `a`.
pub fn push_filter(qb: &mut QueryBuilder<Postgres>, schema: &Schema, ent: &EffEntity, alias: &str, f: &Filter) -> Result<()> {
    let unknown = || Error::invalid(format!("cannot filter on '{}'", f.field));
    // Records carrying a tag (collaboration): `["_tag", "=", "urgent"]`, `["_tag", "in", [...]]`.
    if f.field == "_tag" {
        qb.push(format!(" and exists (select 1 from record_tag t where t.tenant_id = {alias}.tenant_id and t.record_id = {alias}.id and t.entity = "));
        qb.push_bind(ent.def.name.clone());
        qb.push(" and ");
        push_condition(qb, "t.tag", Kind::Text, None, f)?;
        qb.push(")");
        return Ok(());
    }
    if let Some((head, tail)) = f.field.split_once('.') {
        let hf = ent.field(head).ok_or_else(unknown)?;
        match hf.ftype {
            FieldType::Table => {
                let info = ent.children.get(head).ok_or_else(unknown)?;
                let cent = schema.entity(&info.entity).ok_or_else(unknown)?;
                let kind = kind_of(cent, tail).ok_or_else(unknown)?;
                let expr = field_expr(cent, tail, "c").ok_or_else(unknown)?;
                qb.push(format!(" and exists (select 1 from {} c where c.tenant_id = {alias}.tenant_id and c.parent_id = {alias}.id and ", q(&info.table)));
                push_condition(qb, &expr, kind, cent.field(tail), f)?;
                qb.push(")");
            }
            FieldType::Link => {
                let target = hf.to.as_deref().and_then(|t| schema.entity(t)).ok_or_else(unknown)?;
                if target.table.is_empty() {
                    return Err(unknown());
                }
                let kind = kind_of(target, tail).ok_or_else(unknown)?;
                let expr = field_expr(target, tail, "l").ok_or_else(unknown)?;
                let link = field_expr(ent, head, alias).ok_or_else(unknown)?;
                qb.push(format!(
                    " and {link} in (select l.id from {} l where l.tenant_id = {alias}.tenant_id{} and ",
                    q(&target.table),
                    entity_predicate(target, "l")
                ));
                push_condition(qb, &expr, kind, target.field(tail), f)?;
                qb.push(")");
            }
            _ => return Err(unknown()),
        }
        return Ok(());
    }
    let kind = kind_of(ent, &f.field).ok_or_else(unknown)?;
    let expr = field_expr(ent, &f.field, alias).ok_or_else(unknown)?;
    qb.push(" and ");
    push_condition(qb, &expr, kind, ent.field(&f.field), f)
}

pub fn parse_sort(ent: &EffEntity, sort: Option<&str>) -> Result<(String, bool)> {
    let s = sort.map(str::to_string).or_else(|| ent.def.list.sort.clone()).unwrap_or_else(|| "created_at desc".into());
    let mut parts = s.split_whitespace();
    let field = parts.next().unwrap_or("created_at").to_string();
    let desc = match parts.next().map(|d| d.to_lowercase()) {
        None => false,
        Some(d) if d == "asc" => false,
        Some(d) if d == "desc" => true,
        Some(d) => return Err(Error::invalid(format!("invalid sort direction '{d}'"))),
    };
    if field_expr(ent, &field, "a").is_none() {
        return Err(Error::invalid(format!("cannot sort by '{field}'")));
    }
    Ok((field, desc))
}

pub fn encode_cursor(ts: &str, id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::json!([ts, id]).to_string())
}

pub fn decode_cursor(c: &str) -> Result<(String, Uuid)> {
    let bad = || Error::invalid("invalid cursor");
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(c).map_err(|_| bad())?;
    let v: Value = serde_json::from_slice(&raw).map_err(|_| bad())?;
    let ts = v.get(0).and_then(Value::as_str).ok_or_else(bad)?.to_string();
    let id = v.get(1).and_then(Value::as_str).and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    Ok((ts, id))
}

/// Builds `from <table> a where tenant and entity and scope and filters and search`.
pub fn push_from_where(
    qb: &mut QueryBuilder<Postgres>,
    schema: &Schema,
    ent: &EffEntity,
    tenant: Uuid,
    scope: &RowScope,
    filters: &[Filter],
    search: Option<&str>,
) -> Result<()> {
    qb.push(format!(" from {} a where a.tenant_id = ", q(&ent.table))).push_bind(tenant);
    qb.push(entity_predicate(ent, "a"));
    scope.push_sql(qb, "a");
    for f in filters {
        push_filter(qb, schema, ent, "a", f)?;
    }
    if let Some(s) = search.map(str::trim).filter(|s| !s.is_empty()) {
        if s.len() > 100 {
            return Err(Error::invalid("search text too long"));
        }
        let mut cols: Vec<String> = vec!["a.number".into()];
        for name in ent.def.title_field.iter().chain(ent.def.search_fields.iter()) {
            if let Some(e) = field_expr(ent, name, "a") {
                cols.push(format!("{e}::text"));
            }
        }
        cols.dedup();
        qb.push(" and (");
        for (i, c) in cols.iter().enumerate() {
            if i > 0 {
                qb.push(" or ");
            }
            qb.push(format!("{c} ilike ")).push_bind(format!("%{}%", s.replace('%', "\\%").replace('_', "\\_")));
        }
        qb.push(")");
    }
    Ok(())
}

pub fn system_keys() -> &'static [&'static str] {
    SYSTEM_KEYS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_filter_shapes() {
        let f = parse_filters(&json!([["status", "=", "Paid"], ["grand_total", ">", 10], ["customer", "x"]])).unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!(f[1].op, Op::Gt);
        let o = parse_filters(&json!({"status": "Paid", "qty": [">=", 2]})).unwrap();
        assert_eq!(o.len(), 2);
        assert!(parse_filters(&json!([["a", "~", 1]])).is_err());
        assert!(parse_filters(&json!("x")).is_err());
    }

    #[test]
    fn cursor_roundtrip() {
        let id = Uuid::now_v7();
        let c = encode_cursor("2026-01-01T00:00:00.000Z", &id.to_string());
        assert_eq!(decode_cursor(&c).unwrap(), ("2026-01-01T00:00:00.000Z".to_string(), id));
        assert!(decode_cursor("!!").is_err());
    }
}
