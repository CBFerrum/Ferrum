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
use ferrum_meta::{EffEntity, EntityKind, FieldType, Reference, Schema, Storage};
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

/// A title field as text. A record headed by a link (an appointment by its patient) is named by
/// the linked record's own title, a step or two on, when that entity names itself to anyone
/// (`reference: open`); otherwise it has no title here, and callers fall back to its number.
pub fn title_text(schema: &Schema, ent: &EffEntity, name: &str, alias: &str) -> Option<String> {
    title_at(schema, ent, name, alias, 2)
}

fn title_at(schema: &Schema, ent: &EffEntity, name: &str, alias: &str, depth: u8) -> Option<String> {
    let e = field_expr(ent, name, alias)?;
    let Some(to) = ent.field(name).filter(|f| f.ftype == FieldType::Link).and_then(|f| schema.entity(f.to.as_deref()?)) else {
        return Some(format!("{e}::text"));
    };
    if depth == 0 || to.table.is_empty() || to.def.kind == EntityKind::Ledger || to.def.reference != Reference::Open {
        return None;
    }
    let b = format!("{alias}_t");
    let own = to.def.title_field.as_deref().and_then(|t| title_at(schema, to, t, &b, depth - 1)).map(|t| format!("nullif({t}, ''), ")).unwrap_or_default();
    Some(format!(
        "(select coalesce({own}{b}.number, {b}.id::text) from {} {b} where {b}.tenant_id = {alias}.tenant_id and {b}.id = {e}{})",
        q(&to.table),
        entity_predicate(to, &b)
    ))
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

/// ` || jsonb_build_object(…)`: each of `ent`'s tables as its rows in order, under the row aliased `parent`; a row of
/// a custom entity's lines carries its own lines the same way (lines within lines, two levels at most: BD-06). One
/// statement, whatever the number of rows.
fn tables_sql(ent: &EffEntity, schema: &Schema, parent: &str, depth: usize) -> String {
    let mut children = Vec::new();
    for (field, info) in &ent.children {
        if let Some(cent) = schema.entity(&info.entity) {
            let (ci, c) = (format!("ci{depth}"), format!("c{depth}"));
            children.push(format!(
                "'{field}', (select coalesce(jsonb_agg(to_jsonb({c}){} order by {c}.idx), '[]'::jsonb) from \
                 (select {} from {} {ci} where {ci}.tenant_id = x.tenant_id and {ci}.parent_id = {parent}.id{}) {c})",
                tables_sql(cent, schema, &c, depth + 1),
                child_select_list(cent, &ci),
                q(&info.table),
                info.predicate(&ci)
            ));
        }
    }
    if children.is_empty() { String::new() } else { format!(" || jsonb_build_object({})", children.join(", ")) }
}

fn load_sql(ent: &EffEntity, schema: &Schema) -> String {
    format!(
        "select to_jsonb(x){} from (select {}, a.tenant_id from {} a where a.tenant_id = $1 and a.id = $2{}) x",
        tables_sql(ent, schema, "x", 0),
        select_list(ent, "a"),
        q(&ent.table),
        entity_predicate(ent, "a")
    )
}

/// Loads one record with its child rows; `lock` takes a row lock for the rest of the transaction.
pub async fn load(tx: &mut Tx, schema: &Schema, ent: &EffEntity, tenant: Uuid, id: Uuid, lock: bool) -> Result<Option<Doc>> {
    if lock {
        // Lock first, read in a new statement: under read committed, a statement that had to wait
        // for the lock still sees child rows from its own (older) snapshot, and writing them back
        // would undo the other writer's changes.
        let sql = format!("select a.id from {} a where a.tenant_id = $1 and a.id = $2{} for update", q(&ent.table), entity_predicate(ent, "a"));
        let locked: Option<Uuid> = sqlx::query_scalar(AssertSqlSafe(sql)).bind(tenant).bind(id).fetch_optional(&mut **tx).await?;
        if locked.is_none() {
            return Ok(None);
        }
    }
    let row: Option<Value> = sqlx::query_scalar(AssertSqlSafe(load_sql(ent, schema))).bind(tenant).bind(id).fetch_optional(&mut **tx).await?;
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

/// The columns a row object sets, quoted: only those of this workspace's schema, so the columns
/// other packages add to a shared table (§13 of docs/plan/apps-ui.md) keep their defaults.
fn row_columns(row: &Value, extra: &[&str]) -> String {
    let keys = row.as_object().into_iter().flat_map(|o| o.keys().map(String::as_str));
    keys.chain(extra.iter().copied()).map(q).collect::<Vec<_>>().join(", ")
}

pub async fn insert_row(tx: &mut Tx, schema: &Schema, ent: &EffEntity, tenant: Uuid, doc: &Doc) -> Result<()> {
    let row = row_json(ent, doc, tenant, None);
    let cols = row_columns(&row, &["created_at", "updated_at"]);
    let sql = format!(
        "insert into {t} ({cols}) select {cols} from jsonb_populate_record(null::{t}, $1::jsonb || jsonb_build_object('created_at', now(), 'updated_at', now()))",
        t = q(&ent.table)
    );
    sqlx::query(AssertSqlSafe(sql)).bind(row).execute(&mut **tx).await?;
    let id = doc_id(doc)?;
    write_tables(tx, schema, ent, tenant, &[(id, doc, None)]).await?;
    keep_keys(tx, ent, tenant, id).await
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
    write_tables(tx, schema, ent, tenant, &[(id, doc, Some(old))]).await?;
    keep_keys(tx, ent, tenant, id).await
}

/// One parent's rows as saved (`new`) and, for one that was stored, as they were (`old`).
type Parent<'a> = (Uuid, &'a Map<String, Value>, Option<&'a Map<String, Value>>);

/// Writes `ent`'s tables under `parents`, a table at a time across all of them: rows no longer there are deleted
/// (their own rows go with them, by the foreign key), new and changed rows written in one statement, unchanged ones
/// left alone; then, the same way, the tables of those rows (lines within lines: a custom entity's lines may hold
/// lines of their own, two levels at most, BD-06). At most three statements a table, whatever the number of rows.
async fn write_tables(tx: &mut Tx, schema: &Schema, ent: &EffEntity, tenant: Uuid, parents: &[Parent<'_>]) -> Result<()> {
    let rows_of = |d: &'_ Map<String, Value>, field: &str| -> Vec<Map<String, Value>> {
        d.get(field).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object).cloned().collect()
    };
    for (field, info) in &ent.children {
        let Some(cent) = schema.entity(&info.entity) else { continue };
        let stored: Vec<Uuid> = parents.iter().filter(|p| p.2.is_some()).map(|p| p.0).collect();
        let new_rows: Vec<(Uuid, Map<String, Value>)> = parents.iter().flat_map(|p| rows_of(p.1, field).into_iter().map(move |r| (p.0, r))).collect();
        let old_rows: Vec<Map<String, Value>> = parents.iter().filter_map(|p| p.2).flat_map(|o| rows_of(o, field)).collect();
        if !stored.is_empty() {
            let keep: Vec<Uuid> = new_rows.iter().filter_map(|(_, r)| r.get("id").and_then(Value::as_str)).filter_map(|s| s.parse().ok()).collect();
            let sql =
                format!("delete from {} c where c.tenant_id = $1 and c.parent_id = any($2) and not (c.id = any($3)){}", q(&info.table), info.predicate("c"));
            sqlx::query(AssertSqlSafe(sql)).bind(tenant).bind(&stored).bind(&keep).execute(&mut **tx).await?;
        }
        let old_by_id: HashMap<&str, &Map<String, Value>> = old_rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(|i| (i, r))).collect();
        let changed: Vec<(Uuid, &Map<String, Value>)> = new_rows
            .iter()
            .filter(|(_, r)| old_by_id.get(r.get("id").and_then(Value::as_str).unwrap_or_default()).is_none_or(|o| !rows_equal(cent, o, r)))
            .map(|(p, r)| (*p, r))
            .collect();
        if !changed.is_empty() {
            let ids: Vec<Uuid> = changed.iter().filter_map(|(_, r)| r.get("id").and_then(Value::as_str)).filter_map(|s| s.parse().ok()).collect();
            let ext = if old_rows.is_empty() { HashMap::new() } else { existing_ext(tx, &info.table, tenant, &ids).await? };
            upsert_children(tx, cent, &info.table, child_rows_json(cent, &changed, tenant, &ext)).await?;
        }
        if !cent.children.is_empty() {
            let next: Vec<Parent<'_>> = new_rows
                .iter()
                .filter_map(|(_, r)| {
                    let id = r.get("id").and_then(Value::as_str)?;
                    Some((id.parse().ok()?, r, old_by_id.get(id).copied()))
                })
                .collect();
            Box::pin(write_tables(tx, schema, cent, tenant, &next)).await?;
        }
    }
    Ok(())
}

/// Whether a row's own values are as stored (its rows' rows are compared on their own).
fn rows_equal(cent: &EffEntity, a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    if a.get("idx") != b.get("idx") {
        return false;
    }
    cent.value_fields().all(|f| {
        let x = a.get(&f.name).unwrap_or(&Value::Null);
        let y = b.get(&f.name).unwrap_or(&Value::Null);
        x == y
    })
}

fn child_rows_json(cent: &EffEntity, rows: &[(Uuid, &Map<String, Value>)], tenant: Uuid, ext: &HashMap<Uuid, Value>) -> Vec<Value> {
    rows.iter()
        .map(|(parent, r)| {
            let id: Option<Uuid> = r.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok());
            let mut obj = Map::new();
            obj.insert("tenant_id".into(), Value::String(tenant.to_string()));
            obj.insert("id".into(), r.get("id").cloned().unwrap_or(Value::Null));
            obj.insert("parent_id".into(), Value::String(parent.to_string()));
            if cent.custom {
                obj.insert("entity".into(), Value::String(cent.def.name.clone()));
            }
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
    if rows.is_empty() {
        return Ok(());
    }
    let mut cols = vec!["idx".to_string(), "updated_at".to_string(), "ext".to_string()];
    cols.extend(cent.value_fields().filter(|f| cent.storage_of(&f.name) == Some(Storage::Column)).map(|f| f.name.clone()));
    let set = cols.iter().map(|c| format!("{c} = excluded.{c}", c = q(c))).collect::<Vec<_>>().join(", ");
    let list = row_columns(rows.first().unwrap_or(&Value::Null), &[]);
    let sql = format!(
        "insert into {t} ({list}) select {list} from jsonb_populate_recordset(null::{t}, $1::jsonb) \
         on conflict (tenant_id, id) do update set {set} where {t}.parent_id = excluded.parent_id{same}",
        t = q(table),
        // Rows of a custom entity's tables share `custom_record`: a row id from another table stays there.
        same = if cent.custom { format!(" and {}.entity = excluded.entity", q(table)) } else { String::new() }
    );
    sqlx::query(AssertSqlSafe(sql)).bind(Value::Array(rows)).execute(&mut **tx).await?;
    Ok(())
}

/// Replaces the rows of one child table of `parent` with `rows` (ids and idx set): engine-kept rows.
pub async fn set_child_rows(tx: &mut Tx, table: &str, cent: &EffEntity, tenant: Uuid, parent: Uuid, rows: Vec<Value>) -> Result<()> {
    let keep: Vec<Uuid> = rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str)).filter_map(|s| s.parse().ok()).collect();
    let sql = format!("delete from {} c where c.tenant_id = $1 and c.parent_id = $2 and not (c.id = any($3)){}", q(table), entity_predicate(cent, "c"));
    sqlx::query(AssertSqlSafe(sql)).bind(tenant).bind(parent).bind(&keep).execute(&mut **tx).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let ext = existing_ext(tx, table, tenant, &keep).await?;
    let rows: Vec<(Uuid, &Map<String, Value>)> = rows.iter().filter_map(Value::as_object).map(|r| (parent, r)).collect();
    upsert_children(tx, cent, table, child_rows_json(cent, &rows, tenant, &ext)).await
}

pub async fn delete_row(tx: &mut Tx, ent: &EffEntity, tenant: Uuid, id: Uuid) -> Result<u64> {
    let sql = format!("delete from {} a where a.tenant_id = $1 and a.id = $2{}", q(&ent.table), entity_predicate(ent, "a"));
    let n = sqlx::query(AssertSqlSafe(sql)).bind(tenant).bind(id).execute(&mut **tx).await?.rows_affected();
    // A package's record takes the lines a customization added to it (custom records under its id, BD-06), also
    // while that customization is off; a custom record's go by the trigger that cascades in `custom_record`.
    if !ent.custom {
        sqlx::query("delete from custom_record where tenant_id = $1 and parent_id = $2").bind(tenant).bind(id).execute(&mut **tx).await?;
    }
    Ok(n)
}

/// Engine-level update of derived fields (no validation, no hooks). For status/progress counters.
pub async fn set_values(tx: &mut Tx, ent: &EffEntity, tenant: Uuid, id: Uuid, values: &Doc) -> Result<()> {
    set_values_in(tx, &ent.table, ent, tenant, id, values).await
}

/// Like `set_values`, for a row of an explicit table (child tables live in per-parent tables).
pub async fn set_values_in(tx: &mut Tx, table: &str, ent: &EffEntity, tenant: Uuid, id: Uuid, values: &Doc) -> Result<()> {
    // A custom entity's unique keys are kept by saves (`keep_keys`); derived values never touch them.
    if ent.custom
        && let Some(k) = values.keys().find(|k| ent.unique_keys().iter().flatten().any(|f| f == *k))
    {
        return Err(crate::Error::Internal(format!("set_values: {k} is part of a unique key of {}; save the record instead", ent.def.name)));
    }
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
    let sql = format!("update {} a set {} where a.tenant_id = $1 and a.id = $2{}", q(table), assigns.join(", "), entity_predicate(ent, "a"));
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

/// One unique key's value in a custom record's row (`a`): its fields' values as a JSON array, or
/// null when any is empty, as nulls take no part in a SQL unique index. Field names are identifiers.
pub fn key_value_sql(fields: &[String], alias: &str) -> String {
    let present = fields.iter().map(|f| format!("coalesce({alias}.ext->>'{f}', '') <> ''")).collect::<Vec<_>>().join(" and ");
    let values = fields.iter().map(|f| format!("{alias}.ext->'{f}'")).collect::<Vec<_>>().join(", ");
    format!("case when {present} then jsonb_build_array({values})::text end")
}

/// The unique keys a custom record holds: each key's value, unless it is cancelled (as unique
/// indexes leave cancelled documents out). `$1` tenant, `$2` record.
fn held_keys_sql(keys: &[Vec<String>]) -> String {
    let values = keys.iter().map(|k| format!("('{}', {})", k.join(","), key_value_sql(k, "a"))).collect::<Vec<_>>().join(", ");
    format!(
        "select v.key, v.value from custom_record a cross join lateral (values {values}) v(key, value) \
         where a.tenant_id = $1 and a.id = $2 and a.docstatus <> 2 and v.value is not null"
    )
}

/// Keeps a custom record's unique keys in `custom_key`, in the record's own transaction: drops the
/// values it no longer holds, writes the ones it does, and refuses when another record holds one.
/// The primary key decides (a concurrent save waits for the other to finish); nothing is read first.
async fn keep_keys(tx: &mut Tx, ent: &EffEntity, tenant: Uuid, id: Uuid) -> Result<()> {
    let keys = ent.unique_keys();
    if !ent.custom || keys.is_empty() {
        return Ok(());
    }
    let held = held_keys_sql(&keys);
    let drop = format!("delete from custom_key k where k.tenant_id = $1 and k.record_id = $2 and (k.key, k.value) not in (select key, value from ({held}) h)");
    sqlx::query(AssertSqlSafe(drop)).bind(tenant).bind(id).execute(&mut **tx).await?;
    let write =
        format!("insert into custom_key (tenant_id, entity, key, value, record_id) select $1, $3, h.key, h.value, $2 from ({held}) h on conflict do nothing");
    sqlx::query(AssertSqlSafe(write)).bind(tenant).bind(id).bind(&ent.def.name).execute(&mut **tx).await?;
    let taken = format!(
        "select h.key from ({held}) h where not exists (select 1 from custom_key k where k.tenant_id = $1 and k.entity = $3 \
         and k.key = h.key and k.value = h.value and k.record_id = $2) order by h.key"
    );
    let taken: Vec<String> = sqlx::query_scalar(AssertSqlSafe(taken)).bind(tenant).bind(id).bind(&ent.def.name).fetch_all(&mut **tx).await?;
    if let Some(key) = taken.first() {
        let labels: Vec<String> = key.split(',').map(|f| ent.field(f).map(|f| f.label()).unwrap_or_else(|| f.to_string())).collect();
        return Err(crate::Error::Conflict(format!("another {} already has this {}", ent.def.label, labels.join(" and "))));
    }
    Ok(())
}

pub fn doc_id(doc: &Doc) -> Result<Uuid> {
    doc.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).ok_or_else(|| crate::Error::Internal("document has no id".into()))
}
