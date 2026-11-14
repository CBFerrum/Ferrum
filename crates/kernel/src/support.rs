//! Support access a workspace gives, in layers, without anyone at Ferrum or a partner holding a way in.
//!
//! | Level | What it reaches |
//! |---|---|
//! | `health` | Versions, queues, installed packages: no records. |
//! | `diagnostics` | That, and what failed: job and event errors, without their payloads. |
//! | `assistance` | Named entities, for a limited time, read-only unless the customer allows writing. |
//! | `incident` | Emergency access to named entities, or all of them, for a day at most. Needs a reason. |
//!
//! A grant is an API key on a service account with its own role, so it goes through every check
//! anyone's request does: tenant isolation, permissions, the audit trail. It expires by itself, can be
//! revoked at any moment, and every request made with it is listed for the customer. Administrators
//! hear when one is made and when it is first used. Nothing here reaches `internal` data's protections
//! by another door, and this limits what the product gives support: it says nothing of what someone
//! with the database's credentials can do, which [docs/support-access.md] states plainly.

use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::{Kernel, audit, notify};
use chrono::Utc;
use ferrum_meta::EntityKind;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// How an app's own part of a workspace is doing, for the support report: counts, and problems in
/// words (`{"problems": ["…"], …counts}`), never a record, since a `health` key reads it. Registered
/// for a package ([`crate::HookRegistry::add_health`]) and asked only of workspaces that installed
/// it, in a transaction of their own.
#[async_trait::async_trait]
pub trait Health: Send + Sync {
    async fn health(&self, kernel: &Kernel, tx: &mut Tx, tenant: Uuid) -> Result<Value>;
}

/// An app's health as the report gives it: its counts, and its problems in words (20 at most, each
/// cut at 200 characters). Nothing else passes, so a health key never reads a record through it.
fn counts_and_problems(v: &Value) -> Value {
    let mut out: serde_json::Map<String, Value> =
        v.as_object().into_iter().flatten().filter(|(_, x)| x.is_number()).map(|(k, x)| (k.clone(), x.clone())).collect();
    let problems = v.get("problems").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    out.insert("problems".into(), problems.take(20).map(|p| json!(p.chars().take(200).collect::<String>())).collect());
    Value::Object(out)
}

/// The longest each level may last, in hours.
fn longest(level: &str) -> Option<i64> {
    match level {
        "health" => Some(30 * 24),
        "diagnostics" => Some(7 * 24),
        "assistance" => Some(3 * 24),
        "incident" => Some(24),
        _ => None,
    }
}

/// The domain of the accounts grants run as.
pub const SUPPORT_DOMAIN: &str = "@support.invalid";

pub fn reaches_records(level: &str) -> bool {
    matches!(level, "assistance" | "incident")
}

#[derive(Debug, Clone, Deserialize)]
pub struct GrantInput {
    pub level: String,
    /// Who it is for: a partner, a named person at support.
    pub grantee: String,
    /// `assistance`: the entities it reaches. `incident`: these, or empty for all.
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub can_write: bool,
    pub reason: String,
    pub hours: i64,
}

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin && !ctx.is_system { Ok(()) } else { Err(Error::Forbidden("support access is given by the workspace's administrators".into())) }
}

impl Kernel {
    /// Gives support access: answers the grant and its key, shown this once.
    pub async fn grant_support(&self, ctx: &Ctx, input: GrantInput) -> Result<(Uuid, String)> {
        admin_only(ctx)?;
        let longest = longest(&input.level).ok_or_else(|| Error::field("level", "health, diagnostics, assistance or incident"))?;
        if !(1..=longest).contains(&input.hours) {
            return Err(Error::field("hours", format!("{} access lasts 1 to {longest} hours", input.level)));
        }
        let (grantee, reason) = (input.grantee.trim(), input.reason.trim());
        if grantee.is_empty() || grantee.chars().count() > 200 {
            return Err(Error::field("grantee", "say who it is for"));
        }
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why, in at most 500 characters"));
        }
        let records = reaches_records(&input.level);
        if !records && (!input.entities.is_empty() || input.can_write) {
            return Err(Error::field("entities", format!("{} access reaches no records", input.level)));
        }
        if input.level == "assistance" && input.entities.is_empty() {
            return Err(Error::field("entities", "name what it reaches"));
        }
        let entities: Vec<String> = if input.level == "incident" && input.entities.is_empty() {
            ctx.schema
                .entities
                .values()
                .filter(|e| !matches!(e.def.kind, EntityKind::Child | EntityKind::Ledger) && !e.table.is_empty())
                .map(|e| e.def.name.clone())
                .collect()
        } else {
            input.entities.clone()
        };
        for e in &entities {
            self.entity(ctx, e)?;
        }
        let (id, user) = (Uuid::now_v7(), Uuid::now_v7());
        let role = format!("support:{id}");
        let expires_at = Utc::now() + chrono::Duration::hours(input.hours);
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into app_user (tenant_id, id, email, full_name, kind) values ($1, $2, $3, $4, 'support')")
            .bind(ctx.tenant)
            .bind(user)
            .bind(format!("support-{id}{SUPPORT_DOMAIN}"))
            .bind(format!("Support: {}", grantee.chars().take(180).collect::<String>()))
            .execute(&mut *tx)
            .await?;
        sqlx::query("insert into role (tenant_id, name, description) values ($1, $2, $3)").bind(ctx.tenant).bind(&role).bind(reason).execute(&mut *tx).await?;
        sqlx::query("insert into user_role (tenant_id, user_id, role) values ($1, $2, $3)").bind(ctx.tenant).bind(user).bind(&role).execute(&mut *tx).await?;
        for e in &entities {
            sqlx::query("insert into role_permission (tenant_id, role, entity, can_read, can_write) values ($1, $2, $3, true, $4)")
                .bind(ctx.tenant)
                .bind(&role)
                .bind(e)
                .bind(input.can_write)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("insert into support_grant (tenant_id, id, level, grantee, entities, can_write, reason, user_id, created_by, expires_at) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(&input.level)
            .bind(grantee)
            .bind(&entities)
            .bind(input.can_write)
            .bind(reason)
            .bind(user)
            .bind(ctx.user)
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;
        audit::record_admin(
            &mut tx,
            ctx,
            "SupportGrant",
            id,
            "grant",
            None,
            json!({"level": input.level, "grantee": grantee, "entities": entities, "can_write": input.can_write, "reason": reason, "expires_at": expires_at}),
        )
        .await?;
        self.tell_admins(&mut tx, ctx.tenant, &format!("Support access given to {grantee}"), &format!("{}: {reason}", input.level)).await?;
        tx.commit().await?;
        let (_, key) = self.create_api_key(ctx, user, &format!("Support {id}"), Some(expires_at), Some("support")).await?;
        Ok((id, key))
    }

    /// A notification to each active administrator of the workspace.
    pub(crate) async fn tell_admins(&self, tx: &mut crate::Tx, tenant: Uuid, title: &str, body: &str) -> Result<()> {
        let admins: Vec<Uuid> = sqlx::query_scalar("select id from app_user where tenant_id = $1 and is_admin and is_active and kind = 'member'")
            .bind(tenant)
            .fetch_all(&mut **tx)
            .await?;
        for admin in admins {
            notify::send(tx, tenant, admin, title, Some(body), None, None).await?;
        }
        Ok(())
    }

    /// Ends a grant now, on every replica.
    pub async fn revoke_support(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        let user: Option<Uuid> = sqlx::query_scalar(
            "update support_grant set revoked_at = now(), revoked_by = $3 where tenant_id = $1 and id = $2 and revoked_at is null returning user_id",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(ctx.user)
        .fetch_optional(&mut *tx)
        .await?;
        let user = user.ok_or_else(|| Error::NotFound("support grant".into()))?;
        self.close_grant(&mut tx, ctx.tenant, user).await?;
        audit::record_admin(&mut tx, ctx, "SupportGrant", id, "revoke", None, json!({})).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, user);
        Ok(())
    }

    async fn close_grant(&self, tx: &mut crate::Tx, tenant: Uuid, user: Uuid) -> Result<()> {
        sqlx::query("update api_key set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
            .bind(tenant)
            .bind(user)
            .execute(&mut **tx)
            .await?;
        sqlx::query("update app_user set is_active = false, token_version = token_version + 1 where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(user)
            .execute(&mut **tx)
            .await?;
        crate::auth::notify_users(tx, tenant, Some(user)).await
    }

    /// Closes what has expired: a key has stopped working by itself, and this tidies what is left.
    pub(crate) async fn expire_support(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let due: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "select g.tenant_id, g.user_id from support_grant g join app_user u on u.tenant_id = g.tenant_id and u.id = g.user_id \
             where g.revoked_at is null and g.expires_at < now() and u.is_active and not ferrum_workspace_held(g.tenant_id)",
        )
        .fetch_all(&mut *tx)
        .await?;
        for (tenant, user) in &due {
            self.close_grant(&mut tx, *tenant, *user).await?;
        }
        tx.commit().await?;
        Ok(due.len() as u64)
    }

    /// The workspace's grants, past and present, with how much each was used.
    pub async fn support_grants(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', g.id, 'level', g.level, 'grantee', g.grantee, 'entities', g.entities, 'can_write', g.can_write, 'reason', g.reason, \
             'created_at', g.created_at, 'expires_at', g.expires_at, 'revoked_at', g.revoked_at, 'active', g.revoked_at is null and g.expires_at > now(), \
             'uses', (select count(*) from support_access a where a.tenant_id = g.tenant_id and a.grant_id = g.id), \
             'last_used_at', (select max(a.at) from support_access a where a.tenant_id = g.tenant_id and a.grant_id = g.id)) \
             from support_grant g where g.tenant_id = $1 order by g.created_at desc",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Every request made with a grant, newest first.
    pub async fn support_activity(&self, ctx: &Ctx, id: Uuid) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar("select jsonb_build_object('at', at, 'method', method, 'path', path, 'refused', refused) from support_access where tenant_id = $1 and grant_id = $2 order by seq desc limit 1000").bind(ctx.tenant).bind(id).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// The grant behind a support key, if it is live, with its level.
    pub async fn support_level(&self, ctx: &Ctx) -> Result<Option<(Uuid, String)>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row = sqlx::query_as("select id, level from support_grant where tenant_id = $1 and user_id = $2 and revoked_at is null and expires_at > now()")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Records a request made with a grant; the first tells the administrators.
    pub async fn support_used(&self, ctx: &Ctx, grant: Uuid, method: &str, path: &str) -> Result<()> {
        self.support_tried(ctx, grant, method, path, false).await
    }

    /// Records a request made with a grant, `refused` when it was turned away before reaching anything.
    pub async fn support_tried(&self, ctx: &Ctx, grant: Uuid, method: &str, path: &str, refused: bool) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        let first: bool = sqlx::query_scalar("select not exists (select 1 from support_access where tenant_id = $1 and grant_id = $2)")
            .bind(ctx.tenant)
            .bind(grant)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("insert into support_access (tenant_id, grant_id, method, path, refused) values ($1, $2, $3, $4, $5)")
            .bind(ctx.tenant)
            .bind(grant)
            .bind(method)
            .bind(path.chars().take(500).collect::<String>())
            .bind(refused)
            .execute(&mut *tx)
            .await?;
        if first {
            self.tell_admins(&mut tx, ctx.tenant, "Support access was used", &ctx.full_name).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// What `health` and `diagnostics` see: how the workspace is doing, and no record of it.
    pub async fn support_report(&self, ctx: &Ctx, level: &str) -> Result<Value> {
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        let (pending, dead): (i64, i64) =
            sqlx::query_as("select count(*) filter (where processed_at is null and not dead), count(*) filter (where dead) from outbox where tenant_id = $1")
                .bind(ctx.tenant)
                .fetch_one(&mut *tx)
                .await?;
        let (queued, failed): (i64, i64) = sqlx::query_as(
            "select count(*) filter (where status in ('queued', 'running')), count(*) filter (where status = 'failed') from job where tenant_id = $1",
        )
        .bind(ctx.tenant)
        .fetch_one(&mut *tx)
        .await?;
        // What it uses, as counts and nothing else: its active people and the bytes its files take
        // (what an operator charges for beyond a plan).
        let meters: Vec<(String, i64)> = sqlx::query_as(crate::account::METERS).bind(ctx.tenant).fetch_all(&mut *tx).await?;
        let meter = |m: &str| meters.iter().find(|(name, _)| name == m).map_or(0, |(_, q)| *q);
        let (mut failures, mut events): (Vec<Value>, Vec<Value>) = (Vec::new(), Vec::new());
        // Runs stuck for a person, as counts for health and, from diagnostics up, which ones and at what step,
        // never what they carry: no trigger, material, state or evidence (FLC-SEC-007).
        let stuck: i64 = sqlx::query_scalar("select count(*) from logic_run where tenant_id = $1 and status = 'needs_attention'")
            .bind(ctx.tenant)
            .fetch_one(&mut *tx)
            .await?;
        let (mut runs, mut items): (Vec<Value>, Vec<Value>) = (Vec::new(), Vec::new());
        if level != "health" {
            runs = sqlx::query_scalar(
                "select jsonb_build_object('run', id, 'solution', solution, 'process', process, 'status', status, 'since', updated_at) \
                 from logic_run where tenant_id = $1 and status = 'needs_attention' order by updated_at desc limit 50",
            )
            .bind(ctx.tenant)
            .fetch_all(&mut *tx)
            .await?;
            items = sqlx::query_scalar(
                "select jsonb_build_object('run', run_id, 'kind', kind, 'step', activation, 'opened_at', opened_at) \
                 from logic_task where tenant_id = $1 and status = 'open' and kind in ('recovery', 'attention') order by opened_at desc limit 50",
            )
            .bind(ctx.tenant)
            .fetch_all(&mut *tx)
            .await?;
            failures = sqlx::query_scalar("select jsonb_build_object('kind', kind, 'attempts', attempts, 'error', last_error, 'at', finished_at) from job where tenant_id = $1 and status = 'failed' order by finished_at desc limit 50").bind(ctx.tenant).fetch_all(&mut *tx).await?;
            events = sqlx::query_scalar("select jsonb_build_object('topic', topic, 'attempts', attempts, 'error', last_error, 'at', created_at) from outbox where tenant_id = $1 and dead order by id desc limit 50").bind(ctx.tenant).fetch_all(&mut *tx).await?;
        }
        tx.commit().await?;
        // Each installed app's own health, one transaction each, so one that fails spoils no other.
        let mut apps = serde_json::Map::new();
        for (package, source) in self.hooks.health().iter().filter(|(p, _)| ctx.schema.packages.contains(p)) {
            let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
            let found = match source.health(self, &mut tx, ctx.tenant).await {
                Ok(v) => {
                    tx.commit().await?;
                    counts_and_problems(&v)
                }
                Err(e) => {
                    tracing::warn!(package = %package, error = %e, "an app's health could not be read");
                    let _ = tx.rollback().await;
                    json!({"problems": ["its health could not be read"]})
                }
            };
            apps.insert(package.clone(), found);
        }
        let warnings: Vec<String> = ctx.schema.warnings.iter().map(|w| format!("{}: {}", w.entity, w.message)).collect();
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"), "packages": ctx.schema.packages,
            "events": {"pending": pending, "dead": dead}, "jobs": {"active": queued, "failed": failed},
            "usage": {"people": meter("people"), "storage_bytes": meter("storage_bytes")}, "apps": apps,
            "customization_warnings": if level == "health" { json!(warnings.len()) } else { json!(warnings) },
            "failed_jobs": failures, "dead_events": events,
            "runs": {"needing_attention": stuck, "stuck": runs, "open_items": items},
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::counts_and_problems;
    use serde_json::json;

    #[test]
    fn an_app_says_counts_and_problems_and_nothing_else() {
        let said = json!({"gateways": 5, "silent": 1, "name": "Press hall", "list": [1], "problems": ["1 gateway silent", 7, "x".repeat(300)]});
        let kept = counts_and_problems(&said);
        assert_eq!(kept["gateways"], json!(5));
        assert_eq!((kept.get("name"), kept.get("list")), (None, None), "no words but problems, no lists");
        assert_eq!(kept["problems"].as_array().unwrap().len(), 2);
        assert_eq!(kept["problems"][1].as_str().unwrap().len(), 200);
        assert_eq!(counts_and_problems(&json!("anything")), json!({"problems": []}));
        assert_eq!(counts_and_problems(&json!({"problems": vec!["p"; 30]}))["problems"].as_array().unwrap().len(), 20);
    }
}
