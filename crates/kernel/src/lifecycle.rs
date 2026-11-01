//! The end of someone's access, whoever ends it: an administrator, or an identity provider through
//! SCIM (docs/identity-lifecycle.md). Sessions, API keys and OAuth grants go at once, on every
//! replica, and their open work moves to whoever takes it over. Their row stays, so their name
//! stays on what they did. Apps hear of it through `ferrum.user.deprovisioned` and hand over what
//! they keep themselves.

use crate::auth::notify_users;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::{Kernel, audit, notify, outbox};
use serde_json::{Value, json};
use uuid::Uuid;

/// Published once someone's access has ended: `{user, email, reassign_to, reason}`.
pub const DEPROVISIONED_TOPIC: &str = "ferrum.user.deprovisioned";

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
            moved = sqlx::query(
                "update record_assignment a set user_id = $3 where a.tenant_id = $1 and a.user_id = $2 and a.status = 'Open' and not exists \
                 (select 1 from record_assignment b where b.tenant_id = a.tenant_id and b.entity = a.entity and b.record_id = a.record_id \
                  and b.user_id = $3 and b.status = 'Open')",
            )
            .bind(ctx.tenant)
            .bind(user)
            .bind(to)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            // What is left, the successor holds already.
            sqlx::query(
                "update record_assignment set status = 'Cancelled', closed_at = now(), closed_by = $3 where tenant_id = $1 and user_id = $2 and status = 'Open'",
            )
            .bind(ctx.tenant)
            .bind(user)
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?;
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
}
