//! Where a workspace's account stands, and what it has used. The operator decides these; nothing here
//! charges anyone or decides a price.
//!
//! A billing problem never touches data, and never takes away the customer's way out:
//!
//! | State | What it means |
//! |---|---|
//! | `active` | Everything works. |
//! | `grace` | Everything works; administrators have been told. |
//! | `restricted` | Read-only for everyone. Reading, reporting and exporting the workspace still work: export is a right, not a feature. |
//! | `closed` | Read-only as restricted, and kept until a date. After it an operator may delete it ([`Kernel::delete_tenant`]); nothing does by itself. |
//!
//! Usage is counted from what is really there, once a day (`usage_day`), so a bill can be explained
//! afterwards from the same numbers. Quotas are the operator's, per workspace (`tenant.quotas`).

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::{Kernel, Tx, audit};
use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

const METERS: &str = "select 'storage_bytes', coalesce(sum(size_bytes), 0)::bigint from file_object where tenant_id = $1 \
    union all select 'people', count(*) from app_user where tenant_id = $1 and is_active and kind = 'member' \
    union all select 'ai_tokens_month', coalesce(sum(coalesce((usage->'usage'->>'input_tokens')::bigint, 0) + coalesce((usage->'usage'->>'output_tokens')::bigint, 0)), 0)::bigint \
      from assistant_message where tenant_id = $1 and role = 'assistant' and created_at >= date_trunc('month', now()) \
    union all select 'jobs_month', count(*) from job where tenant_id = $1 and created_at >= date_trunc('month', now())";

/// What an app still holds of a workspace outside the database: databases at a provider, repositories on a
/// forge. A workspace is deleted only once every app says it holds nothing, so nothing is left running, or
/// billed, with no record of whose it was.
#[async_trait]
pub trait Leaving: Send + Sync {
    async fn held_outside(&self, tx: &mut Tx, tenant: Uuid) -> Result<Vec<String>>;
}

impl Kernel {
    /// What a workspace is using now, and its quotas, for its administrators.
    pub async fn usage(&self, ctx: &Ctx) -> Result<Value> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("this is for administrators".into()));
        }
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        let meters: Vec<(String, i64)> = sqlx::query_as(METERS).bind(ctx.tenant).fetch_all(&mut *tx).await?;
        let (state, note, quotas): (String, Option<String>, Value) =
            sqlx::query_as("select account_state, account_note, quotas from tenant where id = $1").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        let days: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('day', day, 'meter', meter, 'quantity', quantity) from usage_day where tenant_id = $1 and day > now() - interval '35 days' order by day desc, meter",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(
            json!({"now": meters.into_iter().map(|(m, q)| (m, json!(q))).collect::<serde_json::Map<_, _>>(), "state": state, "note": note, "quotas": quotas, "days": days}),
        )
    }

    /// Records each workspace's usage for today (`maintenance`): the last reading of a day stands.
    pub(crate) async fn record_usage(&self) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        let tenants: Vec<Uuid> = sqlx::query_scalar("select id from tenant").fetch_all(&mut *tx).await?;
        let today = Utc::now().date_naive();
        for tenant in tenants {
            let meters: Vec<(String, i64)> = sqlx::query_as(METERS).bind(tenant).fetch_all(&mut *tx).await?;
            for (meter, quantity) in meters {
                sqlx::query("insert into usage_day (tenant_id, day, meter, quantity) values ($1, $2, $3, $4) on conflict (tenant_id, day, meter) do update set quantity = excluded.quantity")
                    .bind(tenant)
                    .bind(today)
                    .bind(meter)
                    .bind(quantity)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// The operator moves a workspace between account states (`ferrum task account_state`).
    pub async fn set_account_state(&self, tenant: Uuid, state: &str, note: Option<&str>) -> Result<()> {
        if !matches!(state, "active" | "grace" | "restricted") {
            return Err(Error::field("state", "active, grace or restricted; a workspace is closed with close_account"));
        }
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let n = sqlx::query("update tenant set account_state = $2, account_note = $3, closed_at = null, retain_until = null where id = $1")
            .bind(tenant)
            .bind(state)
            .bind(note)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("workspace".into()));
        }
        let system = Ctx::system(tenant, self.registry.schema(tenant).await?);
        audit::record(&mut tx, &system, "Workspace", tenant, "account_state", None, json!({"state": state, "note": note})).await?;
        crate::auth::notify_users(&mut tx, tenant, None).await?;
        tx.commit().await?;
        self.invalidate_tenant_users(tenant);
        Ok(())
    }

    /// The operator closes a workspace: read-only for everyone, with export still open, and kept until `retain_until`.
    /// Opening it again is setting another state. Apps see it closed and wind down what they run.
    pub async fn close_account(&self, tenant: Uuid, retain_until: NaiveDate, note: &str) -> Result<()> {
        if note.trim().is_empty() {
            return Err(Error::field("note", "say why, for whoever reads this later"));
        }
        if retain_until <= Utc::now().date_naive() {
            return Err(Error::field("retain_until", "a date after today: a closed workspace is kept for a while"));
        }
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let n = sqlx::query(
            "update tenant set account_state = 'closed', account_note = $2, closed_at = coalesce(closed_at, now()), retain_until = $3 where id = $1",
        )
        .bind(tenant)
        .bind(note)
        .bind(retain_until)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("workspace".into()));
        }
        let system = Ctx::system(tenant, self.registry.schema(tenant).await?);
        audit::record(&mut tx, &system, "Workspace", tenant, "account_closed", None, json!({"retain_until": retain_until, "note": note})).await?;
        crate::auth::notify_users(&mut tx, tenant, None).await?;
        tx.commit().await?;
        self.invalidate_tenant_users(tenant);
        Ok(())
    }

    /// Refuses what a storage quota does not allow: `size` more bytes.
    pub async fn check_storage_quota(&self, tenant: Uuid, size: u64) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let (limit, used): (Option<i64>, i64) = sqlx::query_as(
            "select (select (quotas->>'storage_bytes')::bigint from tenant where id = $1), (select coalesce(sum(size_bytes), 0)::bigint from file_object where tenant_id = $1)",
        )
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        if limit.is_some_and(|l| used + size as i64 > l) {
            return Err(Error::Conflict("this workspace has reached its storage limit".into()));
        }
        Ok(())
    }
}
