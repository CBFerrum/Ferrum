//! Plans (FERRUM-CLOUD-002): named sets of quotas the operator defines (`FERRUM_PLANS`), so that a
//! workspace is put on one by its name instead of quota by quota. Putting a workspace on a plan
//! writes the plan's quotas to it (`tenant.quotas`, which the storage and people checks read) and
//! records its name; nothing here charges anyone. A plan's quotas are whole numbers, each held where
//! the thing is made: `people` (active members, a database trigger), `storage_bytes` (files),
//! `ai_tokens_month` (the assistant), `devices` and `measurements_per_day` (Operations). One left
//! out has no limit.

use crate::error::{Error, Result};
use crate::{Ctx, Kernel, audit};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// The quotas a plan may set.
pub const QUOTAS: &[&str] = &["people", "storage_bytes", "ai_tokens_month", "devices", "measurements_per_day"];

/// Checks the operator's plans: each a set of whole-number quotas this knows.
pub fn validate(plans: &Map<String, Value>) -> std::result::Result<(), String> {
    for (name, plan) in plans {
        if name.is_empty() || name.len() > 40 || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(format!("plan name {name:?}: lower-case letters, digits and hyphens"));
        }
        let Some(quotas) = plan.as_object() else { return Err(format!("plan {name}: an object of quotas")) };
        for (k, v) in quotas {
            if !QUOTAS.contains(&k.as_str()) {
                return Err(format!("plan {name}: {k} is not a quota ({})", QUOTAS.join(", ")));
            }
            if v.as_i64().is_none_or(|n| n < 0) {
                return Err(format!("plan {name}: {k} is a whole number"));
            }
        }
    }
    Ok(())
}

impl Kernel {
    /// Puts a workspace on one of the operator's plans: its quotas become the plan's, on record.
    pub async fn set_plan(&self, tenant: Uuid, plan: &str) -> Result<()> {
        let quotas = self.settings.plans.get(plan).cloned().ok_or_else(|| {
            let known: Vec<&String> = self.settings.plans.keys().collect();
            Error::field("plan", format!("is not one of this server's plans ({known:?})"))
        })?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let n = sqlx::query("update tenant set quotas = $2, settings = settings || jsonb_build_object('plan', $3::text) where id = $1")
            .bind(tenant)
            .bind(&quotas)
            .bind(plan)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("workspace".into()));
        }
        let system = Ctx::system(tenant, self.registry.schema(tenant).await?);
        audit::record(&mut tx, &system, "Workspace", tenant, "plan", None, json!({"plan": plan, "quotas": quotas})).await?;
        // What was read of its quotas (the assistant's allowance) is stale on every replica.
        crate::registry::Registry::bump_version(&mut tx, tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(tenant);
        Ok(())
    }

    /// The plan a workspace is on, by name (none when it is on quotas set one by one, or none).
    pub async fn plan_of(&self, tenant: Uuid) -> Result<Option<String>> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let plan: Option<String> = sqlx::query_scalar("select settings->>'plan' from tenant where id = $1").bind(tenant).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(plan)
    }
}
