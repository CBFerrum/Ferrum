//! Append-only audit trail with a hash chain per record, plus daily per-tenant anchors.
//!
//! Chaining per record (not per tenant) avoids a global hot row: the record is already locked
//! by the write that produces the event. The daily anchor chains all of a day's event hashes, so
//! editing or deleting history (which triggers also forbid) is detectable after the fact.

use crate::Doc;
use crate::ctx::Ctx;
use crate::db::{Db, Tx};
use crate::error::Result;
use crate::value::canonical_json;
use chrono::{NaiveDate, SecondsFormat, Utc};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn event_hash(prev: Option<&[u8]>, body: &Value) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(prev.unwrap_or(&[]));
    h.update(canonical_json(body));
    h.finalize().to_vec()
}

fn body(tenant: Uuid, at: &str, actor: Option<Uuid>, entity: &str, record: Uuid, action: &str, version: Option<i32>, diff: &Value) -> Value {
    json!({"tenant": tenant, "at": at, "actor": actor, "entity": entity, "record": record, "action": action, "version": version, "diff": diff})
}

/// Field-level diff: `{field: [old, new]}`; child tables report added/removed/changed row ids.
pub fn diff(old: Option<&Doc>, new: Option<&Doc>) -> Value {
    let empty = Map::new();
    let o = old.unwrap_or(&empty);
    let n = new.unwrap_or(&empty);
    let mut out = Map::new();
    let keys: std::collections::BTreeSet<&String> = o.keys().chain(n.keys()).collect();
    for k in keys {
        if k.starts_with('_') || ["version", "updated_at", "updated_by", "created_at"].contains(&k.as_str()) {
            continue;
        }
        let a = o.get(k).unwrap_or(&Value::Null);
        let b = n.get(k).unwrap_or(&Value::Null);
        if a == b {
            continue;
        }
        if let (Value::Array(ra), Value::Array(rb)) = (a, b) {
            let ids = |rows: &Vec<Value>| -> Vec<String> { rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string)).collect() };
            let (ia, ib) = (ids(ra), ids(rb));
            let added: Vec<&String> = ib.iter().filter(|i| !ia.contains(i)).collect();
            let removed: Vec<&String> = ia.iter().filter(|i| !ib.contains(i)).collect();
            let changed: Vec<&String> = ib
                .iter()
                .filter(|i| ia.contains(i))
                .filter(|i| {
                    let find = |rows: &Vec<Value>| rows.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(i.as_str())).cloned();
                    find(ra) != find(rb)
                })
                .collect();
            out.insert(k.clone(), json!({"added": added, "removed": removed, "changed": changed}));
        } else {
            out.insert(k.clone(), json!([a, b]));
        }
    }
    Value::Object(out)
}

#[allow(clippy::too_many_arguments)]
pub async fn record(tx: &mut Tx, ctx: &Ctx, entity: &str, record_id: Uuid, action: &str, version: Option<i32>, diff: Value) -> Result<()> {
    let prev: Option<Vec<u8>> =
        sqlx::query_scalar("select hash from audit_event where tenant_id = $1 and entity = $2 and record_id = $3 order by id desc limit 1")
            .bind(ctx.tenant)
            .bind(entity)
            .bind(record_id)
            .fetch_optional(&mut **tx)
            .await?;
    let at = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let actor = (!ctx.user.is_nil()).then_some(ctx.user);
    let hash = event_hash(prev.as_deref(), &body(ctx.tenant, &at, actor, entity, record_id, action, version, &diff));
    sqlx::query(
        "insert into audit_event (tenant_id, at, actor, entity, record_id, action, version, diff, request_id, prev_hash, hash) \
         values ($1, $2::timestamptz, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(ctx.tenant)
    .bind(&at)
    .bind(actor)
    .bind(entity)
    .bind(record_id)
    .bind(action)
    .bind(version)
    .bind(&diff)
    .bind(&ctx.request_id)
    .bind(prev)
    .bind(hash)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Row {
    at: String,
    actor: Option<Uuid>,
    action: String,
    version: Option<i32>,
    diff: Option<Value>,
    prev_hash: Option<Vec<u8>>,
    hash: Vec<u8>,
}

/// Recomputes a record's chain. Returns the index of the first broken event, if any.
pub async fn verify_record(tx: &mut Tx, tenant: Uuid, entity: &str, record_id: Uuid) -> Result<Option<usize>> {
    let rows: Vec<Row> = sqlx::query_as(
        "select to_char(at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') as at, actor, action, version, diff, prev_hash, hash \
         from audit_event where tenant_id = $1 and entity = $2 and record_id = $3 order by id",
    )
    .bind(tenant)
    .bind(entity)
    .bind(record_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut prev: Option<Vec<u8>> = None;
    for (i, r) in rows.iter().enumerate() {
        let expected =
            event_hash(prev.as_deref(), &body(tenant, &r.at, r.actor, entity, record_id, &r.action, r.version, r.diff.as_ref().unwrap_or(&Value::Null)));
        if r.prev_hash != prev || r.hash != expected {
            return Ok(Some(i));
        }
        prev = Some(r.hash.clone());
    }
    Ok(None)
}

/// Writes (idempotently) the anchor for `day` for every tenant with events that day.
pub async fn anchor_day(db: &Db, day: NaiveDate) -> Result<usize> {
    let mut tx = db.system_tx().await?;
    let tenants: Vec<Uuid> = sqlx::query_scalar(
        "select distinct tenant_id from audit_event where at >= $1::date and at < $1::date + 1 \
         and not exists (select 1 from audit_anchor x where x.tenant_id = audit_event.tenant_id and x.day = $1::date)",
    )
    .bind(day)
    .fetch_all(&mut *tx)
    .await?;
    for t in &tenants {
        let hashes: Vec<Vec<u8>> = sqlx::query_scalar("select hash from audit_event where tenant_id = $1 and at >= $2::date and at < $2::date + 1 order by id")
            .bind(t)
            .bind(day)
            .fetch_all(&mut *tx)
            .await?;
        let prev: Option<Vec<u8>> = sqlx::query_scalar("select root_hash from audit_anchor where tenant_id = $1 and day < $2 order by day desc limit 1")
            .bind(t)
            .bind(day)
            .fetch_optional(&mut *tx)
            .await?;
        let mut h = Sha256::new();
        h.update(prev.as_deref().unwrap_or(&[]));
        for x in &hashes {
            h.update(x);
        }
        sqlx::query("insert into audit_anchor (tenant_id, day, events, root_hash, prev_root) values ($1, $2, $3, $4, $5) on conflict do nothing")
            .bind(t)
            .bind(day)
            .bind(hashes.len() as i64)
            .bind(h.finalize().to_vec())
            .bind(prev)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(tenants.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_fields_and_rows() {
        let old: Doc = serde_json::from_value(json!({"a": 1, "b": "x", "items": [{"id": "r1", "q": 1}, {"id": "r2", "q": 2}]})).unwrap();
        let new: Doc = serde_json::from_value(json!({"a": 2, "b": "x", "items": [{"id": "r1", "q": 5}, {"id": "r3", "q": 1}]})).unwrap();
        let d = diff(Some(&old), Some(&new));
        assert_eq!(d["a"], json!([1, 2]));
        assert!(d.get("b").is_none());
        assert_eq!(d["items"], json!({"added": ["r3"], "removed": ["r2"], "changed": ["r1"]}));
    }

    #[test]
    fn hash_chains() {
        let b = json!({"x": 1});
        let h1 = event_hash(None, &b);
        let h2 = event_hash(Some(&h1), &b);
        assert_ne!(h1, h2);
        assert_eq!(h1, event_hash(None, &json!({"x": 1})));
    }
}

impl crate::Kernel {
    /// A record's audit trail with the result of verifying its hash chain. Requires read access.
    pub async fn audit_trail(&self, ctx: &crate::Ctx, entity: &str, id: Uuid) -> Result<Value> {
        // Reading the record proves read permission (and tenant ownership) before showing its history.
        self.get(ctx, entity, id, false).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(i64, chrono::DateTime<chrono::Utc>, Option<Uuid>, String, Option<i32>, Option<Value>, Option<String>)> = sqlx::query_as(
            "select e.id, e.at, e.actor, e.action, e.version, e.diff, u.full_name from audit_event e \
             left join app_user u on u.tenant_id = e.tenant_id and u.id = e.actor \
             where e.tenant_id = $1 and e.entity = $2 and e.record_id = $3 order by e.id",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        let broken_at = verify_record(&mut tx, ctx.tenant, entity, id).await?;
        tx.commit().await?;
        let events: Vec<Value> = rows
            .into_iter()
            .map(|(eid, at, actor, action, version, diff, name)| {
                serde_json::json!({"id": eid, "at": at, "actor": actor, "actor_name": name, "action": action, "version": version, "diff": diff})
            })
            .collect();
        Ok(serde_json::json!({"events": events, "chain_valid": broken_at.is_none(), "broken_at": broken_at}))
    }
}
