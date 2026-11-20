//! Receipts for writes a client may send again (FEO-003).
//!
//! A request carrying an `Idempotency-Key` claims that key, with a digest of what it does, in the same
//! transaction as its write, and the answer is kept with the key. Sent again, because its response was
//! lost after the write committed, from a second tab, or by a device back online, it gets that same answer
//! and nothing is done twice. A key belongs to the person who claimed it: anyone else using it is refused,
//! and so is the same key for a different request. A request still in hand when the same key comes again
//! is answered as retryable. Receipts are kept for [`RECEIPT_DAYS`], since a device's work may wait offline
//! for days.

use crate::ctx::Ctx;
use crate::db::{Tx, with_retry};
use crate::error::{Error, Result};
use crate::value::canonical_json;
use crate::{Doc, Kernel};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// How long a receipt answers a request sent again.
pub const RECEIPT_DAYS: i32 = 30;

/// The digest a key is claimed with, bound to what the request is done to (`target`, such as
/// `PATCH Customer/<id>`): the client's own digest of the operation when it sends one, for a request whose
/// body is rebuilt on each try (an offline change made again on the record as it stands), and otherwise
/// one of the request's body.
pub fn digest(target: &str, client: Option<&str>, body: &Value) -> String {
    let mut h = Sha256::new();
    h.update(target.as_bytes());
    h.update(b"\n");
    match client.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => {
            h.update(b"client:");
            h.update(c.as_bytes());
        }
        None => h.update(canonical_json(body)),
    }
    hex::encode(h.finalize())
}

fn checked(key: &str) -> Result<()> {
    if key.len() < 8 || key.len() > 200 {
        return Err(Error::invalid("Idempotency-Key must be 8-200 characters"));
    }
    Ok(())
}

/// Claims `key` for this request in `tx`: `None` to go ahead (the answer is then kept with
/// [`keep`] before `tx` commits), or the answer given the first time.
pub async fn claim(tx: &mut Tx, ctx: &Ctx, key: &str, digest: &str) -> Result<Option<Value>> {
    checked(key)?;
    let actor = (!ctx.user.is_nil()).then_some(ctx.user);
    let inserted = sqlx::query("insert into idempotency_key (tenant_id, key, request_hash, actor) values ($1, $2, $3, $4) on conflict do nothing")
        .bind(ctx.tenant)
        .bind(key)
        .bind(digest)
        .bind(actor)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    if inserted == 1 {
        return Ok(None);
    }
    let (hash, by, answer): (String, Option<Uuid>, Option<Value>) =
        sqlx::query_as("select request_hash, actor, response from idempotency_key where tenant_id = $1 and key = $2")
            .bind(ctx.tenant)
            .bind(key)
            .fetch_one(&mut **tx)
            .await?;
    if by != actor {
        // Whose it is stays theirs: the same refusal as a different request.
        return Err(Error::Conflict("this Idempotency-Key was already used for a different request".into()));
    }
    if hash != digest {
        return Err(Error::Conflict("this Idempotency-Key was already used for a different request".into()));
    }
    match answer {
        Some(answer) => Ok(Some(answer)),
        None => Err(Error::Retryable("a request with this Idempotency-Key is still being processed".into())),
    }
}

/// Keeps the answer to the request that claimed `key`, in its transaction.
pub async fn keep(tx: &mut Tx, ctx: &Ctx, key: &str, status: i32, answer: &Value) -> Result<()> {
    sqlx::query("update idempotency_key set status_code = $3, response = $4 where tenant_id = $1 and key = $2")
        .bind(ctx.tenant)
        .bind(key)
        .bind(status)
        .bind(answer)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl Kernel {
    /// [`Kernel::update`] under a receipt: done once whatever how often it is sent. `(doc, replayed)`.
    pub async fn update_idempotent(
        &self,
        ctx: &Ctx,
        entity: &str,
        id: Uuid,
        patch: Doc,
        expected_version: Option<i64>,
        key: &str,
        digest: &str,
    ) -> Result<(Doc, bool)> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            if let Some(answer) = claim(&mut tx, ctx, key, digest).await? {
                tx.rollback().await?;
                return match answer {
                    Value::Object(doc) => Ok((doc, true)),
                    _ => Err(Error::Conflict("this Idempotency-Key was already used for a different request".into())),
                };
            }
            let doc = self.update_tx(&ctx.keyed(key), &mut tx, entity, id, patch.clone(), expected_version).await?;
            keep(&mut tx, ctx, key, 200, &Value::Object(doc.clone())).await?;
            tx.commit().await?;
            Ok((doc, false))
        })
        .await
    }

    /// [`Kernel::run_action`] under a receipt: done once whatever how often it is sent. `(answer, replayed)`.
    pub async fn run_action_idempotent(
        &self,
        ctx: &Ctx,
        entity: &str,
        id: Uuid,
        action: &str,
        params: Value,
        key: &str,
        digest: &str,
    ) -> Result<(Value, bool)> {
        let def = action_def(ctx, entity, action);
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            if let Some(mut answer) = claim(&mut tx, ctx, key, digest).await? {
                tx.rollback().await?;
                if let (Some(once), Some(o)) = (def.and_then(|d| d.answer_once().map(|a| a.told(&d.label))), answer.as_object_mut()) {
                    o.insert("answered_once".into(), Value::String(once));
                }
                return Ok((answer, true));
            }
            let out = self.run_action_tx(&ctx.keyed(key), &mut tx, entity, id, action, &params).await?;
            // What it gives once goes to this caller and is not kept for a repeat.
            keep(&mut tx, ctx, key, 200, &def.map_or_else(|| out.clone(), |d| d.kept_result(&out))).await?;
            tx.commit().await?;
            Ok((out, false))
        })
        .await
    }

    /// What a receipt's digest is taken over for a record's `values` (a create's or an update's, at the gate or the records
    /// door): each `secret` field's value, a child row's too, as a fingerprint keyed with the workspace's key for receipts,
    /// as [`Kernel::digestible_params`] does for an action's, so that a digest kept for weeks cannot be used to try values.
    /// Values with no secret field come back as they are.
    pub async fn digestible_record(&self, ctx: &Ctx, entity: &str, values: &Value) -> Result<Value> {
        let mut out = values.clone();
        let Some(ent) = ctx.schema.entity(entity) else { return Ok(out) };
        let any = std::cell::Cell::new(false);
        each_secret(ctx, ent, &mut out, &|_| any.set(true));
        if !any.get() {
            return Ok(out);
        }
        let (_, key) = self.making_key(crate::vault::Scope::Workspace(ctx.tenant), crate::vault::Keyed::Receipt).await?;
        each_secret(ctx, ent, &mut out, &|v| {
            let text = v.as_str().map_or_else(|| v.to_string(), str::to_string);
            *v = Value::String(key.fingerprint("record field", &format!("{}|{text}", ctx.tenant)));
        });
        Ok(out)
    }

    /// What a receipt's digest is taken over for action `action` of `entity`: `params` with each secret parameter
    /// as a fingerprint keyed with the workspace's key for receipts (crate::vault::keyed), so that the digest cannot be
    /// used to try values.
    pub async fn digestible_params(&self, ctx: &Ctx, entity: &str, action: &str, params: &Value) -> Result<Value> {
        let mut out = params.clone();
        let (Some(def), Some(o)) = (action_def(ctx, entity, action), out.as_object_mut()) else { return Ok(out) };
        for p in def.params.iter().filter(|p| p.secret) {
            if let Some(v) = o.get_mut(&p.name).filter(|v| !v.is_null()) {
                let (_, key) = self.making_key(crate::vault::Scope::Workspace(ctx.tenant), crate::vault::Keyed::Receipt).await?;
                let text = v.as_str().map_or_else(|| v.to_string(), str::to_string);
                *v = Value::String(key.fingerprint("action parameter", &format!("{}|{text}", ctx.tenant)));
            }
        }
        Ok(out)
    }
}

/// `f` on each set value of a `secret` field of `doc`, a record of `ent`, and of its child rows.
fn each_secret(ctx: &Ctx, ent: &ferrum_meta::EffEntity, doc: &mut Value, f: &dyn Fn(&mut Value)) {
    let Some(o) = doc.as_object_mut() else { return };
    for field in &ent.def.fields {
        match field.ftype {
            ferrum_meta::FieldType::Secret => {
                if let Some(v) = o.get_mut(&field.name).filter(|v| !v.is_null()) {
                    f(v);
                }
            }
            ferrum_meta::FieldType::Table => {
                let child = field.of.as_deref().and_then(|c| ctx.schema.entity(c));
                if let (Some(rows), Some(child)) = (o.get_mut(&field.name).and_then(Value::as_array_mut), child) {
                    for row in rows {
                        each_secret(ctx, child, row, f);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Action `action` of `entity`, as `ctx`'s workspace has it.
pub(crate) fn action_def<'a>(ctx: &'a Ctx, entity: &str, action: &str) -> Option<&'a ferrum_meta::ActionDef> {
    ctx.schema.entity(entity)?.def.actions.iter().find(|a| a.name == action)
}
