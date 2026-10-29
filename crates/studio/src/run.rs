//! Runs a compiled [`Plan`] in one read-only transaction (statement timeout, workspace isolation)
//! and shapes the answer the UI and agents consume:
//! `{columns: [{name, label, type, role, ...}], rows: [{name: value, _titles?}], totals, meta}`.
//! Decimals stay exact strings; links and users carry display titles in `_titles`.

use crate::query::{Dim, Meas, Mode, Plan, Ty, USERS, bad, compile};
use crate::spec::{MAX_LIMIT, Spec, buckets, previous};
use chrono::{NaiveDate, SecondsFormat};
use ferrum_kernel::ddl::q;
use ferrum_kernel::store::{entity_predicate, field_expr};
use ferrum_kernel::value::{decimal_value, parse_datetime};
use ferrum_kernel::{Ctx, Kernel, Result, Tx};
use ferrum_meta::EntityKind;
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, Postgres, QueryBuilder};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::Instant;
use uuid::Uuid;

/// Compiles and runs a spec for `ctx`.
pub async fn run(k: &Kernel, ctx: &Ctx, spec: &Spec) -> Result<Value> {
    let plan = compile(ctx, spec)?;
    let mut tx = ferrum_domain::reports::read_tx(k, ctx).await?;
    let out = execute(&mut tx, ctx, &plan).await;
    // Read-only: nothing to keep; a failed statement has already aborted it.
    let _ = tx.rollback().await;
    out
}

#[derive(Debug, Clone, Default)]
struct Row {
    cells: Map<String, Value>,
    /// Text key of the dimensions that identify the row across statements.
    key: Option<Value>,
}

struct Shaped {
    columns: Vec<Value>,
    /// Value columns and the value of an empty group (for filled buckets and missing cells).
    empties: Vec<(String, Value)>,
    rows: Vec<Row>,
    totals: Option<Map<String, Value>>,
    truncated: bool,
}

struct Timer(f64);

impl Timer {
    async fn fetch(&mut self, tx: &mut Tx, mut qb: QueryBuilder<Postgres>) -> Result<Vec<Map<String, Value>>> {
        let t = Instant::now();
        let rows: Vec<Value> = qb.build_query_scalar().fetch_all(&mut **tx).await?;
        self.0 += t.elapsed().as_secs_f64() * 1000.0;
        Ok(rows.into_iter().filter_map(|v| if let Value::Object(m) = v { Some(m) } else { None }).collect())
    }
}

/// A value as Postgres returned it (numbers as exact text) → its JSON form for `ty`.
pub(crate) fn decode(v: Value, ty: Ty) -> Value {
    match &v {
        Value::String(s) if ty.is_decimal() => Decimal::from_str(s).map(decimal_value).unwrap_or(v),
        Value::String(s) if ty.is_int() => s.parse::<i64>().map(Value::from).unwrap_or(v),
        Value::String(s) if ty.is_datetime() => parse_datetime(s).map(|d| json!(d.to_rfc3339_opts(SecondsFormat::Millis, true))).unwrap_or(v),
        _ => v,
    }
}

/// Measures of a result row; an empty sum is zero (SQL says null), like a missing group.
fn measures_of(plan: &Plan, src: &Map<String, Value>, name: impl Fn(&Meas) -> String, out: &mut Map<String, Value>) {
    for (i, m) in plan.measures.iter().enumerate() {
        let v = decode(src.get(&format!("m{i}")).cloned().unwrap_or(Value::Null), m.ty());
        out.insert(name(m), if v.is_null() { m.empty() } else { v });
    }
}

fn dims_of(plan: &Plan, src: &Map<String, Value>, out: &mut Map<String, Value>) {
    for (i, d) in plan.dims.iter().enumerate() {
        out.insert(d.name.clone(), decode(src.get(&format!("d{i}")).cloned().unwrap_or(Value::Null), d.ty()));
    }
}

async fn execute(tx: &mut Tx, ctx: &Ctx, plan: &Plan) -> Result<Value> {
    let mut timer = Timer(0.0);
    let mut meta = Map::new();
    meta.insert("entity".into(), json!(plan.base.def.name));
    if let Some(g) = plan.grain() {
        meta.insert("grain".into(), json!(g));
    }
    if let Some(r) = &plan.range {
        meta.insert("date_range".into(), json!({"field": r.res.path, "from": r.from, "to": r.to}));
    }
    meta.insert("timezone".into(), json!(plan.timezone));
    let mut shaped = match plan.mode {
        Mode::Raw => raw(tx, plan, &mut timer).await?,
        Mode::Grouped if plan.pivot.is_some() => pivot(tx, ctx, plan, &mut timer, &mut meta).await?,
        Mode::Grouped => grouped(tx, plan, &mut timer).await?,
    };
    if plan.time_series.as_ref().is_some_and(|t| t.fill_zero) {
        fill(plan, &mut shaped)?;
    }
    if let Some(kind) = plan.compare {
        compare(tx, plan, kind, &mut timer, &mut shaped, &mut meta).await?;
    }
    let titled: Vec<(String, String)> = plan.dims.iter().chain(plan.raw.iter()).filter_map(|d| d.titled().map(|t| (d.name.clone(), t))).collect();
    let mut rows: Vec<Map<String, Value>> = shaped.rows.into_iter().map(|r| r.cells).collect();
    add_titles(tx, ctx, &titled, &mut rows).await?;
    meta.insert("sql_ms".into(), json!((timer.0 * 10.0).round() / 10.0));
    meta.insert("truncated".into(), json!(shaped.truncated));
    meta.insert("row_count".into(), json!(rows.len()));
    Ok(json!({"columns": shaped.columns, "rows": rows, "totals": shaped.totals, "meta": meta}))
}

async fn raw(tx: &mut Tx, plan: &Plan, timer: &mut Timer) -> Result<Shaped> {
    let mut data = timer.fetch(tx, plan.rows()?).await?;
    let truncated = data.len() > plan.limit as usize;
    data.truncate(plan.limit as usize);
    let rows = data
        .into_iter()
        .map(|src| {
            let mut cells = Map::new();
            cells.insert("_id".into(), src.get("_id").cloned().unwrap_or(Value::Null));
            for (i, d) in plan.raw.iter().enumerate() {
                cells.insert(d.name.clone(), decode(src.get(&format!("r{i}")).cloned().unwrap_or(Value::Null), d.ty()));
            }
            Row { cells, key: None }
        })
        .collect();
    let totals = match plan.raw_totals()? {
        Some(qb) => timer.fetch(tx, qb).await?.into_iter().next().map(|src| {
            let mut t = Map::new();
            for (i, d) in plan.raw.iter().enumerate() {
                if let Some(v) = src.get(&format!("r{i}")) {
                    t.insert(d.name.clone(), decode(v.clone(), d.ty()));
                }
            }
            t
        }),
        None => None,
    };
    Ok(Shaped { columns: plan.raw.iter().map(|d| d.column("field")).collect(), empties: vec![], rows, totals, truncated })
}

/// Dimensions that identify a row across periods: all of them, or all but the time bucket.
fn key_dims(plan: &Plan) -> Vec<usize> {
    let skip = usize::from(plan.time_series.is_some());
    (skip..plan.dims.len()).collect()
}

async fn grouped(tx: &mut Tx, plan: &Plan, timer: &mut Timer) -> Result<Shaped> {
    let keyed = if plan.compare.is_some() { key_dims(plan) } else { vec![] };
    let mut data = timer.fetch(tx, plan.grouped(None, &keyed, None, plan.limit)?).await?;
    let truncated = data.len() > plan.limit as usize;
    data.truncate(plan.limit as usize);
    let rows = data
        .into_iter()
        .map(|src| {
            let mut cells = Map::new();
            dims_of(plan, &src, &mut cells);
            measures_of(plan, &src, |m| m.name.clone(), &mut cells);
            Row { cells, key: src.get("_k").cloned() }
        })
        .collect();
    let totals = timer.fetch(tx, plan.totals(None)?).await?.into_iter().next().map(|src| {
        let mut t = Map::new();
        measures_of(plan, &src, |m| m.name.clone(), &mut t);
        t
    });
    let mut columns: Vec<Value> = plan.dims.iter().map(|d| d.column("dimension")).collect();
    columns.extend(plan.measures.iter().map(Meas::column));
    Ok(Shaped { columns, empties: plan.measures.iter().map(|m| (m.name.clone(), m.empty())).collect(), rows, totals, truncated })
}

fn label_of(v: &Value, d: &Dim, titles: &HashMap<Uuid, String>) -> String {
    match v {
        Value::Null => "(not set)".into(),
        Value::Bool(b) => if *b { "Yes" } else { "No" }.into(),
        Value::String(s) => s.parse::<Uuid>().ok().filter(|_| d.titled().is_some()).and_then(|id| titles.get(&id).cloned()).unwrap_or_else(|| s.clone()),
        other => other.to_string(),
    }
}

async fn pivot(tx: &mut Tx, ctx: &Ctx, plan: &Plan, timer: &mut Timer, meta: &mut Map<String, Value>) -> Result<Shaped> {
    let p = plan.pivot.as_ref().ok_or_else(|| ferrum_kernel::Error::internal("not a pivot"))?;
    let mut cols = timer.fetch(tx, plan.pivot_columns()?).await?;
    let cap = plan.pivot_cap as usize;
    meta.insert("pivot_truncated".into(), json!(cols.len() > cap));
    cols.truncate(cap);
    meta.insert("pivot_columns".into(), json!(cols.len()));
    let values: Vec<Value> = cols.iter().map(|c| decode(c.get("p").cloned().unwrap_or(Value::Null), p.ty())).collect();
    let titles = match p.titled() {
        Some(target) => {
            let ids: Vec<Uuid> = values.iter().filter_map(|v| v.as_str()?.parse().ok()).collect();
            title_map(tx, ctx, &target, &ids).await?
        }
        None => HashMap::new(),
    };
    let all: Vec<usize> = (0..plan.dims.len()).collect();
    let mut data = timer.fetch(tx, plan.grouped(None, &all, None, plan.limit)?).await?;
    let truncated = data.len() > plan.limit as usize;
    data.truncate(plan.limit as usize);
    // Without row dimensions there is one row, keyed by the empty key.
    let key_of = |r: &Map<String, Value>| r.get("_k").cloned().unwrap_or_else(|| json!([]));
    let texts: Vec<String> = cols.iter().filter_map(|c| c.get("_p").and_then(Value::as_str).map(str::to_string)).collect();
    let has_null = cols.iter().any(|c| c.get("_p").is_none_or(Value::is_null));
    let mut cells: HashMap<(String, Option<String>), Map<String, Value>> = HashMap::new();
    if !data.is_empty() && !cols.is_empty() {
        let keys = Value::Array(data.iter().map(key_of).collect());
        for c in timer.fetch(tx, plan.pivot_cells(&keys, &texts, has_null)?).await? {
            let k = c.get("_k").map(Value::to_string).unwrap_or_default();
            let pv = c.get("_p").and_then(Value::as_str).map(str::to_string);
            cells.insert((k, pv), c);
        }
    }
    let mut columns: Vec<Value> = plan.dims.iter().map(|d| d.column("dimension")).collect();
    let mut empties = Vec::new();
    for (j, v) in values.iter().enumerate() {
        let plabel = label_of(v, p, &titles);
        for m in &plan.measures {
            let name = format!("{}@{j}", m.name);
            let label = if plan.measures.len() == 1 { plabel.clone() } else { format!("{plabel} · {}", m.label) };
            columns.push(json!({"name": name, "label": label, "type": m.ty().name(), "role": "pivot", "measure": m.name,
                "pivot_value": v, "pivot_label": plabel}));
            empties.push((name, m.empty()));
        }
    }
    for m in &plan.measures {
        let mut c = m.column();
        c["label"] = json!(format!("Total · {}", m.label));
        columns.push(c);
        empties.push((m.name.clone(), m.empty()));
    }
    let rows = data
        .into_iter()
        .map(|src| {
            let mut out = Map::new();
            dims_of(plan, &src, &mut out);
            let key = key_of(&src).to_string();
            for (j, c) in cols.iter().enumerate() {
                let pv = c.get("_p").and_then(Value::as_str).map(str::to_string);
                let cell = cells.get(&(key.clone(), pv));
                for (i, m) in plan.measures.iter().enumerate() {
                    let v = cell.and_then(|x| x.get(&format!("m{i}")).cloned()).map(|v| decode(v, m.ty())).unwrap_or_else(|| m.empty());
                    out.insert(format!("{}@{j}", m.name), v);
                }
            }
            measures_of(plan, &src, |m| m.name.clone(), &mut out);
            Row { cells: out, key: Some(key_of(&src)) }
        })
        .collect();
    let mut totals = Map::new();
    for (j, c) in cols.iter().enumerate() {
        for (i, m) in plan.measures.iter().enumerate() {
            totals.insert(format!("{}@{j}", m.name), decode(c.get(&format!("m{i}")).cloned().unwrap_or(Value::Null), m.ty()));
        }
    }
    if let Some(src) = timer.fetch(tx, plan.totals(None)?).await?.into_iter().next() {
        measures_of(plan, &src, |m| m.name.clone(), &mut totals);
    }
    Ok(Shaped { columns, empties, rows, totals: Some(totals), truncated })
}

fn date_of(v: Option<&Value>) -> Option<NaiveDate> {
    v.and_then(Value::as_str).and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

/// Every bucket of the time series appears, for every combination of the other dimensions seen,
/// with zero counts and sums (and unknown averages, minimums and maximums).
fn fill(plan: &Plan, shaped: &mut Shaped) -> Result<()> {
    let (Some(ts), Some(time)) = (&plan.time_series, plan.dims.first()) else { return Ok(()) };
    let rows = std::mem::take(&mut shaped.rows);
    let dated: Vec<NaiveDate> = rows.iter().filter_map(|r| date_of(r.cells.get(&time.name))).collect();
    let range = plan.range.as_ref();
    let (Some(lo), Some(hi)) = (range.and_then(|r| r.from).or(dated.iter().min().copied()), range.and_then(|r| r.to).or(dated.iter().max().copied())) else {
        shaped.rows = rows;
        return Ok(());
    };
    let all = buckets(ts.bucket, lo, hi, plan.fy_month, MAX_LIMIT as usize)
        .ok_or_else(|| bad("time_series", "more than 10000 buckets: narrow the range or use a larger bucket"))?;
    let others: Vec<&str> = plan.dims[1..].iter().map(|d| d.name.as_str()).collect();
    let combo = |r: &Row| Value::Array(others.iter().map(|n| r.cells.get(*n).cloned().unwrap_or(Value::Null)).collect()).to_string();
    let mut combos: Vec<(String, Row)> = Vec::new();
    for r in &rows {
        let c = combo(r);
        if !combos.iter().any(|(k, _)| *k == c) {
            combos.push((c, r.clone()));
        }
    }
    if combos.is_empty() {
        combos.push((String::new(), Row { cells: Map::new(), key: Some(json!([])) }));
    }
    let mut by: HashMap<(NaiveDate, String), Row> = HashMap::new();
    let mut undated = Vec::new();
    for r in rows {
        match date_of(r.cells.get(&time.name)) {
            Some(d) => {
                by.insert((d, combo(&r)), r);
            }
            None => undated.push(r),
        }
    }
    let cap = plan.limit as usize;
    let mut out = Vec::with_capacity((all.len() * combos.len()).min(cap + 1));
    // The grid can be far larger than the result may be: stop one row past the limit.
    'grid: for b in all {
        for (c, template) in &combos {
            if out.len() > cap {
                break 'grid;
            }
            out.push(by.remove(&(b, c.clone())).unwrap_or_else(|| {
                let mut cells = Map::new();
                cells.insert(time.name.clone(), json!(b.to_string()));
                for n in &others {
                    cells.insert(n.to_string(), template.cells.get(*n).cloned().unwrap_or(Value::Null));
                }
                for (name, empty) in &shaped.empties {
                    cells.insert(name.clone(), empty.clone());
                }
                Row { cells, key: template.key.clone() }
            }));
        }
    }
    out.extend(undated);
    if out.len() > cap {
        out.truncate(cap);
        shaped.truncated = true;
    }
    shaped.rows = out;
    Ok(())
}

/// Adds `previous(<measure>)` to every row and to the totals: the same query over the previous
/// period, matched by the row's dimensions (a time bucket by its position in the period).
async fn compare(tx: &mut Tx, plan: &Plan, kind: crate::spec::Compare, timer: &mut Timer, shaped: &mut Shaped, meta: &mut Map<String, Value>) -> Result<()> {
    let range = plan.range.as_ref().ok_or_else(|| bad("compare", "needs a date range"))?;
    let (Some(from), Some(to)) = (range.from, range.to) else { return Err(bad("compare", "needs a date range with from and to")) };
    let (pf, pt) = previous(kind, from, to);
    meta.insert("compare".into(), json!({"kind": kind, "from": from, "to": to, "previous_from": pf, "previous_to": pt}));
    let name = |m: &Meas| format!("previous({})", m.name);
    let keyed = key_dims(plan);
    let mut found: HashMap<(Option<usize>, String), Map<String, Value>> = HashMap::new();
    if !plan.dims.is_empty() && !shaped.rows.is_empty() {
        let keys = if keyed.is_empty() {
            None
        } else {
            let mut seen = HashSet::new();
            Some(Value::Array(shaped.rows.iter().filter_map(|r| r.key.clone()).filter(|k| seen.insert(k.to_string())).collect()))
        };
        let prev_buckets = plan.time_series.as_ref().map(|t| buckets(t.bucket, pf, pt, plan.fy_month, MAX_LIMIT as usize).unwrap_or_default());
        for src in timer.fetch(tx, plan.grouped(Some((pf, pt)), &keyed, keys.as_ref(), MAX_LIMIT)?).await? {
            let pos = prev_buckets.as_ref().and_then(|b| date_of(src.get("d0")).and_then(|d| b.iter().position(|x| *x == d)));
            if prev_buckets.is_some() && pos.is_none() {
                continue;
            }
            found.insert((pos, src.get("_k").map(Value::to_string).unwrap_or_else(|| "[]".into())), src);
        }
    }
    let cur_buckets = plan.time_series.as_ref().map(|t| buckets(t.bucket, from, to, plan.fy_month, MAX_LIMIT as usize).unwrap_or_default());
    let time = plan.dims.first().map(|d| d.name.clone()).unwrap_or_default();
    for r in shaped.rows.iter_mut() {
        let pos = cur_buckets.as_ref().and_then(|b| date_of(r.cells.get(&time)).and_then(|d| b.iter().position(|x| *x == d)));
        let key = r.key.as_ref().map(Value::to_string).unwrap_or_else(|| "[]".into());
        let prev = if cur_buckets.is_some() && pos.is_none() { None } else { found.get(&(pos, key)) };
        for (i, m) in plan.measures.iter().enumerate() {
            let v = prev.and_then(|p| p.get(&format!("m{i}")).cloned()).map(|v| decode(v, m.ty())).unwrap_or_else(|| m.empty());
            r.cells.insert(name(m), v);
        }
    }
    if let Some(src) = timer.fetch(tx, plan.totals(Some((pf, pt)))?).await?.into_iter().next() {
        let t = shaped.totals.get_or_insert_with(Map::new);
        measures_of(plan, &src, name, t);
    }
    for m in &plan.measures {
        let mut c = m.column();
        c["name"] = json!(name(m));
        c["label"] = json!(format!("{} (previous)", m.label));
        c["role"] = json!("previous");
        c["measure"] = json!(m.name);
        shaped.columns.push(c);
        shaped.empties.push((name(m), m.empty()));
    }
    Ok(())
}

/// Display titles of linked records (their title field, else number) and of users (their name).
pub(crate) async fn title_map(tx: &mut Tx, ctx: &Ctx, target: &str, ids: &[Uuid]) -> Result<HashMap<Uuid, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = if target == USERS {
        "select id, coalesce(nullif(full_name, ''), email::text) from app_user where tenant_id = $1 and id = any($2)".to_string()
    } else {
        let Some(ent) = ctx.schema.entity(target) else { return Ok(HashMap::new()) };
        if ent.table.is_empty() || ent.def.kind == EntityKind::Ledger {
            return Ok(HashMap::new());
        }
        let title = ent.def.title_field.as_deref().and_then(|f| field_expr(ent, f, "a")).map(|e| format!("{e}::text")).unwrap_or_else(|| "null".into());
        format!(
            "select a.id, coalesce(nullif({title}, ''), a.number, a.id::text) from {} a where a.tenant_id = $1 and a.id = any($2){}",
            q(&ent.table),
            entity_predicate(ent, "a")
        )
    };
    let rows: Vec<(Uuid, String)> = sqlx::query_as(AssertSqlSafe(sql)).bind(ctx.tenant).bind(ids).fetch_all(&mut **tx).await?;
    Ok(rows.into_iter().collect())
}

/// Adds `_titles: {column: title}` for link and user columns (`(column, target entity)`).
pub(crate) async fn add_titles(tx: &mut Tx, ctx: &Ctx, titled: &[(String, String)], rows: &mut [Map<String, Value>]) -> Result<()> {
    if titled.is_empty() || rows.is_empty() {
        return Ok(());
    }
    let mut wanted: HashMap<&str, HashSet<Uuid>> = HashMap::new();
    for (col, target) in titled {
        for r in rows.iter() {
            if let Some(id) = r.get(col).and_then(Value::as_str).and_then(|s| s.parse().ok()) {
                wanted.entry(target.as_str()).or_default().insert(id);
            }
        }
    }
    let mut titles: HashMap<&str, HashMap<Uuid, String>> = HashMap::new();
    for (target, ids) in wanted {
        let ids: Vec<Uuid> = ids.into_iter().collect();
        titles.insert(target, title_map(tx, ctx, target, &ids).await?);
    }
    for r in rows.iter_mut() {
        let mut t = Map::new();
        for (col, target) in titled {
            let id: Option<Uuid> = r.get(col).and_then(Value::as_str).and_then(|s| s.parse().ok());
            if let Some(title) = id.and_then(|id| titles.get(target.as_str())?.get(&id)) {
                t.insert(col.clone(), json!(title));
            }
        }
        if !t.is_empty() {
            r.insert("_titles".into(), Value::Object(t));
        }
    }
    Ok(())
}

/// A result as CSV (UTF-8 with BOM, CRLF): the given columns in order, titles instead of ids,
/// exact values, a totals line, and cells that a spreadsheet would run as formulas defused.
pub fn csv(result: &Value) -> String {
    let cell = |v: &Value| -> String {
        let mut s = match v {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            Value::Bool(b) => if *b { "Yes" } else { "No" }.into(),
            other => other.to_string(),
        };
        let numeric = Decimal::from_str(s.trim()).is_ok();
        if !numeric && s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
            s.insert(0, '\'');
        }
        if s.contains(['"', ',', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
    };
    let cols: Vec<&Value> = result["columns"].as_array().map(|c| c.iter().collect()).unwrap_or_default();
    let mut lines = vec![cols.iter().map(|c| cell(c.get("label").unwrap_or(&c["name"]))).collect::<Vec<_>>().join(",")];
    let value = |row: &Value, name: &str| -> Value {
        row.get("_titles").and_then(|t| t.get(name)).cloned().unwrap_or_else(|| row.get(name).cloned().unwrap_or(Value::Null))
    };
    for r in result["rows"].as_array().into_iter().flatten() {
        lines.push(cols.iter().map(|c| cell(&value(r, c["name"].as_str().unwrap_or_default()))).collect::<Vec<_>>().join(","));
    }
    if let Some(t) = result["totals"].as_object().filter(|t| cols.iter().any(|c| t.contains_key(c["name"].as_str().unwrap_or_default()))) {
        let line: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| match t.get(c["name"].as_str().unwrap_or_default()) {
                Some(v) => cell(v),
                None if i == 0 => "Total".into(),
                None => String::new(),
            })
            .collect();
        lines.push(line.join(","));
    }
    format!("\u{feff}{}\r\n", lines.join("\r\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_decode_exactly() {
        let dec = Ty::Field(ferrum_meta::FieldType::Currency);
        assert_eq!(decode(json!("1361.000000000"), dec), json!("1361"));
        assert_eq!(decode(json!("0.300000000"), dec), json!("0.3"));
        assert_eq!(decode(json!("12345678901234567.123456789"), dec), json!("12345678901234567.123456789"));
        assert_eq!(decode(json!("42"), Ty::Field(ferrum_meta::FieldType::Int)), json!(42));
        assert_eq!(decode(json!("2026-09-30T20:00:00+00:00"), Ty::Timestamp), json!("2026-09-30T20:00:00.000Z"));
        assert_eq!(decode(Value::Null, dec), Value::Null);
    }

    #[test]
    fn csv_is_exact_titled_and_safe() {
        let result = json!({
            "columns": [{"name": "customer", "label": "Customer"}, {"name": "note", "label": "Note, quoted"}, {"name": "sum(x)", "label": "Sum"}],
            "rows": [{"customer": "id-1", "note": "=HYPERLINK(\"x\")", "sum(x)": "-0.3", "_titles": {"customer": "Alpha \"A\""}},
                     {"customer": null, "note": "line\nbreak", "sum(x)": "10"}],
            "totals": {"sum(x)": "9.7"},
        });
        let out = csv(&result);
        assert!(out.starts_with('\u{feff}'));
        let lines: Vec<&str> = out.trim_start_matches('\u{feff}').split("\r\n").collect();
        assert_eq!(lines[0], "Customer,\"Note, quoted\",Sum");
        assert_eq!(lines[1], "\"Alpha \"\"A\"\"\",\"'=HYPERLINK(\"\"x\"\")\",-0.3");
        assert_eq!(lines[2], ",\"line\nbreak\",10");
        assert_eq!(lines[3], "Total,,9.7");
    }

    fn row(cells: Value) -> Row {
        Row { cells: cells.as_object().cloned().unwrap(), key: Some(json!([])) }
    }

    #[test]
    fn time_series_fill_keeps_every_bucket() {
        let s = crate::query::tests::schema();
        let ctx = crate::query::tests::sales(s);
        let spec = Spec::parse(&json!({"entity": "SalesInvoice", "time_series": {"field": "posting_date", "bucket": "month", "from": "2026-07-01", "to": "2026-10-31", "fill_zero": true},
            "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total"}, {"fn": "avg", "field": "grand_total"}]}))
        .unwrap();
        let plan = compile(&ctx, &spec).unwrap();
        let mut shaped = Shaped {
            columns: vec![],
            empties: plan.measures.iter().map(|m| (m.name.clone(), m.empty())).collect(),
            rows: vec![row(json!({"month(posting_date)": "2026-08-01", "count": 2, "sum(grand_total)": "450.3", "avg(grand_total)": "225.15"}))],
            totals: None,
            truncated: false,
        };
        fill(&plan, &mut shaped).unwrap();
        let months: Vec<&str> = shaped.rows.iter().map(|r| r.cells["month(posting_date)"].as_str().unwrap()).collect();
        assert_eq!(months, ["2026-07-01", "2026-08-01", "2026-09-01", "2026-10-01"]);
        assert_eq!(shaped.rows[0].cells["count"], json!(0));
        assert_eq!(shaped.rows[0].cells["sum(grand_total)"], json!("0"));
        assert_eq!(shaped.rows[0].cells["avg(grand_total)"], Value::Null, "an empty average is unknown, not zero");
        assert_eq!(shaped.rows[1].cells["count"], json!(2));
    }
}
