//! Row storage for metadata-defined entities.
//!
//! Writes go through `jsonb_populate_record(set)`, so one JSON parameter carries the whole row and
//! Postgres does the typing; decimals are read back as text, so money never touches a float.
//! Child rows are written diff-wise: unchanged rows are not rewritten.

use crate::Doc;
use crate::db::Tx;
use crate::ddl::q;
use crate::error::Result;
use crate::value::{decimal_value, parse_datetime, parse_decimal};
use chrono::SecondsFormat;
use ferrum_meta::{EffEntity, FieldType, Schema, Storage};
use serde_json::{Map, Value};
use sqlx::AssertSqlSafe;
use std::collections::HashMap;
use uuid::Uuid;

pub const SYSTEM_KEYS: &[&str] =
    &["id", "number", "docstatus", "version", "workflow_state", "amended_from", "created_at", "created_by", "updated_at", "updated_by"];

fn col_expr(ent: &EffEntity, name: &str, alias: &str) -> String {
    let decimal = ent.field(name).is_some_and(|f| f.ftype.is_decimal());
    if decimal { format!("{alias}.{c}::text as {c}", c = q(name)) } else { format!("{alias}.{}", q(name)) }
}

/// `select` list producing every stored value of an entity row.
pub fn select_list(ent: &EffEntity, alias: &str) -> String {
    let mut cols: Vec<String> = SYSTEM_KEYS.iter().map(|c| format!("{alias}.{c}")).collect();
    cols.push(format!("{alias}.ext"));
    if !ent.custom {
        for f in ent.value_fields() {
            if ent.storage_of(&f.name) == Some(Storage::Column) {
                cols.push(col_expr(ent, &f.name, alias));
            }
        }
    }
    cols.join(", ")
}

pub fn child_select_list(cent: &EffEntity, alias: &str) -> String {
    let mut cols = vec![format!("{alias}.id"), format!("{alias}.idx"), format!("{alias}.ext")];
    for f in cent.value_fields() {
        if cent.storage_of(&f.name) == Some(Storage::Column) {
            cols.push(col_expr(cent, &f.name, alias));
        }
    }
    cols.join(", ")
}

/// Extra predicate for entities stored in the shared `custom_record` table.
pub fn entity_predicate(ent: &EffEntity, alias: &str) -> String {
    if ent.custom { format!(" and {alias}.entity = '{}'", ent.def.name) } else { String::new() }
}

/// Column (or ext JSON path) expression for a field, typed for comparisons.
pub fn field_expr(ent: &EffEntity, name: &str, alias: &str) -> Option<String> {
    if SYSTEM_KEYS.contains(&name) {
        return Some(format!("{alias}.{name}"));
    }
    let f = ent.field(name)?;
    if f.ftype == FieldType::Table {
        return None;
    }
    Some(match ent.storage_of(name)? {
        Storage::Column => format!("{alias}.{}", q(name)),
        Storage::Ext => {
            let raw = format!("({alias}.ext->>'{name}')");
            match f.ftype {
                FieldType::Int => format!("{raw}::bigint"),
                FieldType::Decimal | FieldType::Currency | FieldType::Percent => format!("{raw}::numeric"),
                FieldType::Date => format!("{raw}::date"),
                FieldType::Datetime => format!("{raw}::timestamptz"),
                FieldType::Check => format!("coalesce({raw}::boolean, false)"),
                FieldType::Link => format!("nullif({raw}, '')::uuid"),
                FieldType::Json => format!("({alias}.ext->'{name}')"),
                _ => raw,
            }
        }
    })
}

fn normalize_out(ent: &EffEntity, schema: &Schema, m: &mut Map<String, Value>) {
    let ext = match m.remove("ext") {
        Some(Value::Object(e)) => e,
        _ => Map::new(),
    };
    m.remove("tenant_id");
    m.remove("parent_id");
    m.remove("entity");
    for f in &ent.def.fields {
        match f.ftype {
            FieldType::Table => {
                let child = f.of.as_deref().and_then(|c| schema.entity(c));
                if let (Some(Value::Array(rows)), Some(cent)) = (m.get_mut(&f.name), child) {
                    for r in rows.iter_mut() {
                        if let Value::Object(rm) = r {
                            normalize_out(cent, schema, rm);
                        }
                    }
                }
            }
            _ => {
                if ent.storage_of(&f.name) == Some(Storage::Ext) {
                    m.insert(f.name.clone(), ext.get(&f.name).cloned().unwrap_or(Value::Null));
                }
                let v = m.get(&f.name).cloned().unwrap_or(Value::Null);
                let fixed = match (f.ftype, &v) {
                    (t, Value::String(_) | Value::Number(_)) if t.is_decimal() => parse_decimal(&v).map(decimal_value).unwrap_or(v),
                    (FieldType::Datetime, Value::String(s)) => {
                        parse_datetime(s).map(|d| Value::String(d.to_rfc3339_opts(SecondsFormat::Millis, true))).unwrap_or(v)
                    }
                    _ => v,
                };
                m.insert(f.name.clone(), fixed);
            }
        }
    }
    for key in ["created_at", "updated_at"] {
        if let Some(Value::String(s)) = m.get(key)
            && let Ok(d) = parse_datetime(s)
        {
            m.insert(key.into(), Value::String(d.to_rfc3339_opts(SecondsFormat::Millis, true)));
        }
    }
}

pub fn row_to_doc(ent: &EffEntity, schema: &Schema, row: Value) -> Doc {
    let mut m = match row {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    normalize_out(ent, schema, &mut m);
    m
}

fn load_sql(ent: &EffEntity, schema: &Schema, lock: bool) -> String {
    let mut children = Vec::new();
    for (field, info) in &ent.children {
        if let Some(cent) = schema.entity(&info.entity) {
            children.push(format!(
                "'{field}', (select coalesce(jsonb_agg(to_jsonb(c) order by c.idx), '[]'::jsonb) from \
                 (select {} from {} ci where ci.tenant_id = x.tenant_id and ci.parent_id = x.id) c)",
                child_select_list(cent, "ci"),
                q(&info.table)
            ));
        }
    }
    let build = if children.is_empty() { String::new() } else { format!(" || jsonb_build_object({})", children.join(", ")) };
    format!(
        "select to_jsonb(x){build} from (select {}, a.tenant_id from {} a where a.tenant_id = $1 and a.id = $2{}{}) x",
        select_list(ent, "a"),
        q(&ent.table),
        entity_predicate(ent, "a"),
        if lock { " for update" } else { "" }
    )
}

/// Loads one record with its child rows; `lock` takes a row lock for the rest of the transaction.
pub async fn load(tx: &mut Tx, schema: &Schema, ent: &EffEntity, tenant: Uuid, id: Uuid, lock: bool) -> Result<Option<Doc>> {
    let row: Option<Value> = sqlx::query_scalar(AssertSqlSafe(load_sql(ent, schema, lock))).bind(tenant).bind(id).fetch_optional(&mut **tx).await?;
    Ok(row.map(|r| row_to_doc(ent, schema, r)))
}

/// Loads several records (without child rows).
pub async fn load_many(tx: &mut Tx, schema: &Schema, ent: &EffEntity, tenant: Uuid, ids: &[Uuid]) -> Result<Vec<Doc>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let sql = format!(
        "select to_jsonb(x) from (select {} from {} a where a.tenant_id = $1 and a.id = any($2){}) x",
        select_list(ent, "a"),
        q(&ent.table),
        entity_predicate(ent, "a")
    );
    let rows: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(sql)).bind(tenant).bind(ids).fetch_all(&mut **tx).await?;
    Ok(rows.into_iter().map(|r| row_to_doc(ent, schema, r)).collect())
}

async fn existing_ext(tx: &mut Tx, table: &str, tenant: Uuid, ids: &[Uuid]) -> Result<HashMap<Uuid, Value>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!("select id, ext from {} where tenant_id = $1 and id = any($2)", q(table));
    let rows: Vec<(Uuid, Value)> = sqlx::query_as(AssertSqlSafe(sql)).bind(tenant).bind(ids).fetch_all(&mut **tx).await?;
    Ok(rows.into_iter().collect())
}

fn value_columns(ent: &EffEntity, doc: &Doc, base_ext: Option<&Value>, obj: &mut Map<String, Value>) {
    let mut ext = match base_ext {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    for f in ent.value_fields() {
        let mut v = doc.get(&f.name).cloned().unwrap_or(Value::Null);
        if f.ftype == FieldType::Check && v.is_null() {
            v = Value::Bool(false);
        }
        match ent.storage_of(&f.name) {
            Some(Storage::Column) => {
                obj.insert(f.name.clone(), v);
            }
            Some(Storage::Ext) => {
                if v.is_null() {
                    ext.remove(&f.name);
                } else {
                    ext.insert(f.name.clone(), v);
                }
            }
            None => {}
        }
    }
    // Keys of disabled customizations stay in `ext`, so re-enabling them restores the data.
    obj.insert("ext".into(), Value::Object(ext));
}

fn row_json(ent: &EffEntity, doc: &Doc, tenant: Uuid, base_ext: Option<&Value>) -> Value {
    let mut obj = Map::new();
    obj.insert("tenant_id".into(), Value::String(tenant.to_string()));
    for k in ["id", "number", "docstatus", "version", "workflow_state", "amended_from", "created_by", "updated_by"] {
        obj.insert(k.into(), doc.get(k).cloned().unwrap_or(Value::Null));
    }
    if ent.custom {
        obj.insert("entity".into(), Value::String(ent.def.name.clone()));
    }
    value_columns(ent, doc, base_ext, &mut obj);
    Value::Object(obj)
}

fn update_columns(ent: &EffEntity) -> Vec<String> {
    let mut cols: Vec<String> =
        ["number", "docstatus", "version", "workflow_state", "amended_from", "updated_by", "ext"].iter().map(|s| s.to_string()).collect();
    if !ent.custom {
        cols.extend(ent.value_fields().filter(|f| ent.storage_of(&f.name) == Some(Storage::Column)).map(|f| f.name.clone()));
    }
    cols
}

pub async fn insert_row(tx: &mut Tx, schema: &Schema, ent: &EffEntity, tenant: Uuid, doc: &Doc) -> Result<()> {
    let row = row_json(ent, doc, tenant, None);
    let sql = format!(
        "insert into {t} select * from jsonb_populate_record(null::{t}, $1::jsonb || jsonb_build_object('created_at', now(), 'updated_at', now()))",
        t = q(&ent.table)
    );
    sqlx::query(AssertSqlSafe(sql)).bind(row).execute(&mut **tx).await?;
    let id = doc_id(doc)?;
    for (field, info) in &ent.children {
        let Some(cent) = schema.entity(&info.entity) else { continue };
        let rows = child_rows_json(cent, doc.get(field), tenant, id, &HashMap::new());
        if !rows.is_empty() {
            upsert_children(tx, cent, &info.table, rows).await?;
        }
    }
    Ok(())
}

pub async fn update_row(tx: &mut Tx, schema: &Schema, ent: &EffEntity, tenant: Uuid, doc: &Doc, old: &Doc) -> Result<()> {
    let id = doc_id(doc)?;
    let ext = existing_ext(tx, &ent.table, tenant, &[id]).await?;
    let row = row_json(ent, doc, tenant, ext.get(&id));
    let cols = update_columns(ent);
    let collist = cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "update {t} a set ({collist}) = (select {collist} from jsonb_populate_record(null::{t}, $1::jsonb)), updated_at = now() \
         where a.tenant_id = $2 and a.id = $3{}",
        entity_predicate(ent, "a"),
        t = q(&ent.table)
    );
    sqlx::query(AssertSqlSafe(sql)).bind(row).bind(tenant).bind(id).execute(&mut **tx).await?;

    for (field, info) in &ent.children {
        let Some(cent) = schema.entity(&info.entity) else { continue };
        let new_rows = doc.get(field).and_then(Value::as_array).cloned().unwrap_or_default();
        let old_rows = old.get(field).and_then(Value::as_array).cloned().unwrap_or_default();
        let keep: Vec<Uuid> = new_rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str)).filter_map(|s| s.parse().ok()).collect();
        let sql = format!("delete from {} where tenant_id = $1 and parent_id = $2 and not (id = any($3))", q(&info.table));
        sqlx::query(AssertSqlSafe(sql)).bind(tenant).bind(id).bind(&keep).execute(&mut **tx).await?;
        let old_by_id: HashMap<&str, &Value> = old_rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(|i| (i, r))).collect();
        let changed: Vec<Value> = new_rows
            .iter()
            .filter(|r| {
                let rid = r.get("id").and_then(Value::as_str).unwrap_or_default();
                old_by_id.get(rid).is_none_or(|o| !rows_equal(cent, o, r))
            })
            .cloned()
            .collect();
        if changed.is_empty() {
            continue;
        }
        let changed_ids: Vec<Uuid> = changed.iter().filter_map(|r| r.get("id").and_then(Value::as_str)).filter_map(|s| s.parse().ok()).collect();
        let ext = existing_ext(tx, &info.table, tenant, &changed_ids).await?;
        let rows = child_rows_json(cent, Some(&Value::Array(changed)), tenant, id, &ext);
        upsert_children(tx, cent, &info.table, rows).await?;
    }
    Ok(())
}

fn rows_equal(cent: &EffEntity, a: &Value, b: &Value) -> bool {
    if a.get("idx") != b.get("idx") {
        return false;
    }
    cent.value_fields().all(|f| {
        let x = a.get(&f.name).unwrap_or(&Value::Null);
        let y = b.get(&f.name).unwrap_or(&Value::Null);
        x == y
    })
}

fn child_rows_json(cent: &EffEntity, rows: Option<&Value>, tenant: Uuid, parent: Uuid, ext: &HashMap<Uuid, Value>) -> Vec<Value> {
    let Some(Value::Array(rows)) = rows else { return vec![] };
    rows.iter()
        .filter_map(|r| r.as_object())
        .map(|r| {
            let id: Option<Uuid> = r.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok());
            let mut obj = Map::new();
            obj.insert("tenant_id".into(), Value::String(tenant.to_string()));
            obj.insert("id".into(), r.get("id").cloned().unwrap_or(Value::Null));
            obj.insert("parent_id".into(), Value::String(parent.to_string()));
            obj.insert("idx".into(), r.get("idx").cloned().unwrap_or(Value::from(0)));
            let now = chrono::Utc::now().to_rfc3339();
            obj.insert("created_at".into(), Value::String(now.clone()));
            obj.insert("updated_at".into(), Value::String(now));
            value_columns(cent, r, id.and_then(|i| ext.get(&i)), &mut obj);
            Value::Object(obj)
        })
        .collect()
}

async fn upsert_children(tx: &mut Tx, cent: &EffEntity, table: &str, rows: Vec<Value>) -> Result<()> {
    let mut cols = vec!["idx".to_string(), "updated_at".to_string(), "ext".to_string()];
    cols.extend(cent.value_fields().filter(|f| cent.storage_of(&f.name) == Some(Storage::Column)).map(|f| f.name.clone()));
    let set = cols.iter().map(|c| format!("{c} = excluded.{c}", c = q(c))).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "insert into {t} select * from jsonb_populate_recordset(null::{t}, $1::jsonb) \
         on conflict (tenant_id, id) do update set {set} where {t}.parent_id = excluded.parent_id",
        t = q(table)
    );
    sqlx::query(AssertSqlSafe(sql)).bind(Value::Array(rows)).execute(&mut **tx).await?;
    Ok(())
}

pub async fn delete_row(tx: &mut Tx, ent: &EffEntity, tenant: Uuid, id: Uuid) -> Result<u64> {
    let sql = format!("delete from {} a where a.tenant_id = $1 and a.id = $2{}", q(&ent.table), entity_predicate(ent, "a"));
    Ok(sqlx::query(AssertSqlSafe(sql)).bind(tenant).bind(id).execute(&mut **tx).await?.rows_affected())
}

/// Engine-level update of derived fields (no validation, no hooks). For status/progress counters.
pub async fn set_values(tx: &mut Tx, ent: &EffEntity, tenant: Uuid, id: Uuid, values: &Doc) -> Result<()> {
    set_values_in(tx, &ent.table, ent, tenant, id, values).await
}

/// Like `set_values`, for a row of an explicit table (child tables live in per-parent tables).
pub async fn set_values_in(tx: &mut Tx, table: &str, ent: &EffEntity, tenant: Uuid, id: Uuid, values: &Doc) -> Result<()> {
    let mut sets = Vec::new();
    let mut ext_sets = Map::new();
    for (k, v) in values {
        match ent.storage_of(k) {
            Some(Storage::Column) => sets.push(k.clone()),
            Some(Storage::Ext) => {
                ext_sets.insert(k.clone(), v.clone());
            }
            None => return Err(crate::Error::Internal(format!("set_values: unknown field {k} on {}", ent.def.name))),
        }
    }
    let mut assigns = vec!["updated_at = now()".to_string()];
    let mut param = 3;
    if !sets.is_empty() {
        let collist = sets.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
        assigns.push(format!("({collist}) = (select {collist} from jsonb_populate_record(null::{t}, ${param}::jsonb))", t = q(table)));
        param += 1;
    }
    assigns.push(format!("ext = a.ext || ${param}::jsonb"));
    let sql = format!(
        "update {} a set {} where a.tenant_id = $1 and a.id = $2{}",
        q(table),
        assigns.join(", "),
        if table == ent.table { entity_predicate(ent, "a") } else { String::new() }
    );
    let mut query = sqlx::query(AssertSqlSafe(sql)).bind(tenant).bind(id);
    if !sets.is_empty() {
        query = query.bind(Value::Object(values.clone()));
    }
    let n = query.bind(Value::Object(ext_sets)).execute(&mut **tx).await?.rows_affected();
    if n == 0 {
        return Err(crate::Error::NotFound(ent.def.name.clone()));
    }
    Ok(())
}

pub fn doc_id(doc: &Doc) -> Result<Uuid> {
    doc.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).ok_or_else(|| crate::Error::Internal("document has no id".into()))
}
