//! The end of someone's access, whoever ends it: an administrator, or an identity provider through
//! SCIM (docs/identity-lifecycle.md). Sessions, API keys and OAuth grants go at once, on every
//! replica, and their open work moves to whoever takes it over. Their row stays, so their name
//! stays on what they did.
//!
//! The work moves after the commit, from `ferrum.user.deprovisioned`, in batches: a person can be
//! named on a great many records, and access must end whatever that takes. Packages declare what
//! moves (`handover`); an app whose records need more than a changed name also listens to the event.

use crate::auth::notify_users;
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::ddl::q;
use crate::error::{Error, Result};
use crate::outbox::Event;
use crate::store::{entity_predicate, field_expr};
use crate::{Doc, Kernel, audit, notify, outbox};
use ferrum_meta::{EffEntity, HandoverDef};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// Published once someone's access has ended: `{user, email, reassign_to, reason}`.
pub const DEPROVISIONED_TOPIC: &str = "ferrum.user.deprovisioned";

/// Records moved in one transaction.
const HANDOVER_BATCH: i64 = 200;

/// Moves open assignments to `to`. Ones `to` holds already on the same record are closed.
async fn move_assignments(tx: &mut Tx, ctx: &Ctx, from: Uuid, to: Uuid) -> Result<u64> {
    let moved = sqlx::query(
        "update record_assignment a set user_id = $3 where a.tenant_id = $1 and a.user_id = $2 and a.status = 'Open' and not exists \
         (select 1 from record_assignment b where b.tenant_id = a.tenant_id and b.entity = a.entity and b.record_id = a.record_id \
          and b.user_id = $3 and b.status = 'Open')",
    )
    .bind(ctx.tenant)
    .bind(from)
    .bind(to)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    // What is left, the successor holds already.
    sqlx::query(
        "update record_assignment set status = 'Cancelled', closed_at = now(), closed_by = $3 where tenant_id = $1 and user_id = $2 and status = 'Open'",
    )
    .bind(ctx.tenant)
    .bind(from)
    .bind(ctx.user)
    .execute(&mut **tx)
    .await?;
    Ok(moved)
}

/// The next batch of a declaration's records naming `from`.
async fn named_on(tx: &mut Tx, tenant: Uuid, ent: &EffEntity, schema: &ferrum_meta::Schema, def: &HandoverDef, from: &str) -> Result<Vec<Uuid>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!("select a.id from {} a where a.tenant_id = ", q(&ent.table)));
    qb.push_bind(tenant);
    match def.field.split_once('.') {
        None => {
            let Some(expr) = field_expr(ent, &def.field, "a") else { return Ok(vec![]) };
            qb.push(format!(" and lower(({expr})::text) = ")).push_bind(from.to_string());
        }
        Some((table, field)) => {
            let Some(info) = ent.children.get(table) else { return Ok(vec![]) };
            let Some(expr) = schema.entity(&info.entity).and_then(|cent| field_expr(cent, field, "c")) else { return Ok(vec![]) };
            qb.push(format!(
                " and exists (select 1 from {} c where c.tenant_id = a.tenant_id and c.parent_id = a.id and lower(({expr})::text) = ",
                q(&info.table)
            ));
            qb.push_bind(from.to_string()).push(")");
        }
    }
    for (field, wanted) in
        (ferrum_meta::PermissionDef { entity: def.entity.clone(), condition: def.condition.clone(), ..Default::default() }).condition_values()
    {
        let Some(expr) = field_expr(ent, &field, "a") else { return Ok(vec![]) };
        qb.push(format!(" and ({expr})::text = any(")).push_bind(wanted).push(")");
    }
    qb.push(entity_predicate(ent, "a")).push(" order by a.id limit ").push_bind(HANDOVER_BATCH);
    Ok(qb.build_query_scalar().fetch_all(&mut **tx).await?)
}

impl Kernel {
    /// Ends `user`'s access now. Their open assignments move to `reassign_to` (by default the
    /// workspace's `deprovision_reassign_to` setting, a person's id), or stay open and are counted
    /// when nobody takes them. An administrator's own account and the last administrator of a
    /// workspace are refused, so that no workspace can lock itself out. Safe to repeat.
    pub async fn deprovision_user(&self, ctx: &Ctx, user: Uuid, reassign_to: Option<Uuid>, reason: &str) -> Result<Value> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        if user == ctx.user {
            return Err(Error::invalid("you cannot end your own access"));
        }
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why, in at most 500 characters"));
        }
        // OAuth's tables are the server's, outside any workspace; every statement names the tenant.
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        // Administrators' rows locked, so that two people removing the last two both cannot pass.
        let people: Vec<(Uuid, bool, bool, String, String)> = sqlx::query_as(
            "select id, is_admin, is_active, kind, email::text from app_user where tenant_id = $1 and (is_admin or id = $2) order by id for update",
        )
        .bind(ctx.tenant)
        .bind(user)
        .fetch_all(&mut *tx)
        .await?;
        let Some((_, is_admin, _, kind, email)) = people.iter().find(|p| p.0 == user).cloned() else {
            return Err(Error::NotFound("user".into()));
        };
        if kind == "service" {
            return Err(Error::invalid("this is a plugin's service account; remove the plugin instead"));
        }
        if is_admin && !people.iter().any(|p| p.0 != user && p.1 && p.2 && p.3 == "member") {
            return Err(Error::invalid("this is the workspace's last administrator; make someone else one first"));
        }
        let setting: Option<String> =
            sqlx::query_scalar("select settings->>'deprovision_reassign_to' from tenant where id = $1").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        let successor = reassign_to.or(setting.and_then(|s| s.parse().ok()));
        if let Some(to) = successor {
            let fit: Option<bool> = sqlx::query_scalar("select is_active and kind = 'member' from app_user where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(to)
                .fetch_optional(&mut *tx)
                .await?;
            if to == user || fit != Some(true) {
                return Err(Error::field("reassign_to", "must be another active member of the workspace"));
            }
        }

        sqlx::query("update app_user set is_active = false, token_version = token_version + 1, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        let sessions = sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
            .bind(ctx.tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let keys = sqlx::query("update api_key set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
            .bind(ctx.tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let tokens = sqlx::query("update oauth_refresh set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
            .bind(ctx.tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        sqlx::query("update oauth_code set used_at = now() where tenant_id = $1 and user_id = $2 and used_at is null")
            .bind(ctx.tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        sqlx::query("update sign_in_code set used_at = now() where tenant_id = $1 and user_id = $2 and used_at is null")
            .bind(ctx.tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from push_subscription where tenant_id = $1 and user_id = $2").bind(ctx.tenant).bind(user).execute(&mut *tx).await?;

        let mut moved = 0;
        if let Some(to) = successor {
            moved = move_assignments(&mut tx, ctx, user, to).await?;
            if moved > 0 {
                let title = format!("{moved} assignments moved to you from {email}");
                notify::send(&mut tx, ctx.tenant, to, &title, None, None, None).await?;
            }
        }
        let left_open: i64 = sqlx::query_scalar("select count(*) from record_assignment where tenant_id = $1 and user_id = $2 and status = 'Open'")
            .bind(ctx.tenant)
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;

        let done = json!({
            "user": user, "sessions_ended": sessions, "api_keys_revoked": keys, "oauth_tokens_revoked": tokens,
            "assignments_moved": moved, "assignments_left_open": left_open, "reassign_to": successor,
        });
        // The audit trail outlives an erasure of the person, so it names them by id only.
        audit::record(&mut tx, ctx, "User", user, "deprovision", None, json!({"reason": reason, "outcome": done})).await?;
        let event = json!({"user": user, "email": email, "reassign_to": successor, "reason": reason});
        outbox::enqueue(&mut tx, ctx.tenant, DEPROVISIONED_TOPIC, Some("User"), Some(user), event).await?;
        notify_users(&mut tx, ctx.tenant, Some(user)).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, user);
        Ok(done)
    }

    /// Hands what someone is named on to `to`, later than their leaving: when nobody was named then,
    /// or someone else takes over now (administrators only). Open assignments move here and now;
    /// what packages declare moves after the commit, like a deprovisioning's.
    pub async fn hand_over_work(&self, ctx: &Ctx, user: Uuid, to: Uuid, reason: &str) -> Result<Value> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why, in at most 500 characters"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let from: Option<String> = sqlx::query_scalar("select email::text from app_user where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?;
        let email = from.ok_or_else(|| Error::NotFound("user".into()))?;
        let fit: Option<bool> = sqlx::query_scalar("select is_active and kind = 'member' from app_user where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(to)
            .fetch_optional(&mut *tx)
            .await?;
        if to == user || fit != Some(true) {
            return Err(Error::field("to", "must be another active member of the workspace"));
        }
        let moved = move_assignments(&mut tx, ctx, user, to).await?;
        audit::record(&mut tx, ctx, "User", user, "hand_over", None, json!({"to": to, "reason": reason, "assignments_moved": moved})).await?;
        outbox::enqueue(
            &mut tx,
            ctx.tenant,
            DEPROVISIONED_TOPIC,
            Some("User"),
            Some(user),
            json!({"user": user, "email": email, "reassign_to": to, "reason": reason}),
        )
        .await?;
        tx.commit().await?;
        Ok(json!({"user": user, "to": to, "assignments_moved": moved}))
    }

    /// Moves what the installed packages declare (`handover`) from the person to their successor,
    /// [`DEPROVISIONED_TOPIC`]'s handler. Runs in batches, each its own transaction, so a retry
    /// carries on where a failure stopped; without a successor nothing moves, and an administrator
    /// can name one later ([`Kernel::hand_over_work`]).
    pub(crate) async fn hand_over(&self, ev: &Event) -> Result<()> {
        let tenant = ev.tenant_id;
        let Some(to) = ev.payload.get("reassign_to").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
        let from = ev.payload.get("email").and_then(Value::as_str).unwrap_or_default().trim().to_lowercase();
        if from.is_empty() {
            return Ok(());
        }
        let schema = self.registry.schema(tenant).await?;
        let ctx = Ctx::system(tenant, schema.clone());
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let successor: Option<String> =
            sqlx::query_scalar("select email::text from app_user where tenant_id = $1 and id = $2").bind(tenant).bind(to).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let Some(successor) = successor else { return Ok(()) };
        let mut total = 0usize;
        for def in &schema.handover {
            let Ok(ent) = self.entity(&ctx, &def.entity) else { continue };
            loop {
                let mut tx = self.db.system_tenant_tx(tenant).await?;
                let ids = named_on(&mut tx, tenant, &ent, &schema, def, &from).await?;
                for id in &ids {
                    match def.field.split_once('.') {
                        None => {
                            let mut values = Doc::new();
                            values.insert(def.field.clone(), json!(successor));
                            self.set_values_tx(&ctx, &mut tx, &def.entity, *id, values).await?;
                        }
                        Some((table, field)) => {
                            let Some(info) = ent.children.get(table) else { continue };
                            // A row the successor is on already goes; the rest change hands.
                            let col = q(field);
                            let doubled = format!(
                                "delete from {t} c where c.tenant_id = $1 and c.parent_id = $2 and lower(c.{col}::text) = $3 \
                                 and exists (select 1 from {t} o where o.tenant_id = c.tenant_id and o.parent_id = c.parent_id and lower(o.{col}::text) = lower($4))",
                                t = q(&info.table)
                            );
                            sqlx::query(sqlx::AssertSqlSafe(doubled)).bind(tenant).bind(id).bind(&from).bind(&successor).execute(&mut *tx).await?;
                            let update =
                                format!("update {} set {col} = $4 where tenant_id = $1 and parent_id = $2 and lower({col}::text) = $3", q(&info.table));
                            sqlx::query(sqlx::AssertSqlSafe(update)).bind(tenant).bind(id).bind(&from).bind(&successor).execute(&mut *tx).await?;
                        }
                    }
                    audit::record(&mut tx, &ctx, &def.entity, *id, "hand_over", None, json!({"field": def.field, "to": to})).await?;
                }
                tx.commit().await?;
                total += ids.len();
                if (ids.len() as i64) < HANDOVER_BATCH {
                    break;
                }
            }
        }
        if total > 0 {
            let mut tx = self.db.system_tenant_tx(tenant).await?;
            notify::send(&mut tx, tenant, to, &format!("{total} records moved to you from {from}"), None, None, None).await?;
            tx.commit().await?;
        }
        Ok(())
    }
}
