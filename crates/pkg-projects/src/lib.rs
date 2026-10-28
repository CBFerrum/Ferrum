//! Projects: projects, tasks with dependencies, timesheets with costing and billing, project profitability.
//!
//! `Project` is core's master, extended with `proj_` overlay fields; tasks, timesheets and activity
//! rates are this package's entities. Derived figures (hours, costs, billed amounts, % complete)
//! are always recomputed from their sources under a row lock instead of being incremented, so
//! cancellations, amendments and concurrent postings cannot make them drift.
//!
//! Project gets `make_sales_invoice` and `complete` through `add_action` overlays; a timesheet and a
//! milestone task can also be invoiced on their own. Staff own the tasks assigned to them and the
//! timesheets recording their time through `owner_fields`.

mod billing;
mod logic;
mod project;
mod reports;
mod timesheet;

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{Ctx, HookRegistry, Kernel, Result, Tx};
use std::sync::Arc;
use uuid::Uuid;

/// Registers this package's document hooks.
pub fn register(hooks: &mut HookRegistry) {
    hooks
        .register("Project", Arc::new(project::ProjectHook))
        .register("Task", Arc::new(project::TaskHook))
        .register("Timesheet", Arc::new(timesheet::TimesheetHook))
        .register("ActivityCost", Arc::new(timesheet::ActivityCostHook))
        .register("SalesInvoice", Arc::new(billing::InvoiceHook));
}

/// This package's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![Arc::new(reports::Reports)]
}

/// Periodic upkeep (every few minutes on every replica; must be idempotent and safe to run concurrently):
/// open tasks past their expected end become Overdue.
pub async fn maintenance(k: &Kernel) -> Result<()> {
    let mut tx = k.db.system_tx().await?;
    let tenants: Vec<Uuid> = sqlx::query_scalar("select id from tenant where status = 'active'").fetch_all(&mut *tx).await?;
    tx.commit().await?;
    for tenant in tenants {
        match mark_overdue(k, tenant).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(%tenant, tasks = n, "tasks marked overdue"),
            Err(e) => tracing::warn!(%tenant, "marking overdue tasks failed: {e}"),
        }
    }
    Ok(())
}

/// Only tasks still `Open` change, in one statement that re-checks each row under its lock, so
/// replicas running at once (or a user starting the task meanwhile) never conflict.
async fn mark_overdue(k: &Kernel, tenant: Uuid) -> Result<u64> {
    let ctx = Ctx::system(tenant, k.registry.schema(tenant).await?);
    if ctx.schema.entity("Task").is_none() {
        return Ok(0);
    }
    let mut tx = k.db.tenant_tx(tenant).await?;
    let n = sqlx::query(
        "update e_task t set status = 'Overdue', updated_at = now() \
         where t.tenant_id = $1 and t.status = 'Open' and t.exp_end_date < $2 and not exists \
         (select 1 from e_project p where p.tenant_id = t.tenant_id and p.id = t.project and p.status in ('Completed', 'Cancelled'))",
    )
    .bind(tenant)
    .bind(chrono::Utc::now().date_naive())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(n)
}

/// Projects Managers (and administrators and the engine) act on everyone's tasks and time.
pub(crate) fn is_manager(ctx: &Ctx) -> bool {
    ctx.is_system || ctx.has_role("Projects Manager")
}

/// Row locks taken in id order, so transactions touching the same rows queue instead of deadlocking.
pub(crate) async fn lock(tx: &mut Tx, table: &str, tenant: Uuid, ids: &[Uuid]) -> Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!("select 1 from {table} where tenant_id = $1 and id = any($2) order by id for update")))
        .bind(tenant)
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The emails (lower-cased) that belong to no user of the tenant.
pub(crate) async fn unknown_users(tx: &mut Tx, tenant: Uuid, emails: &[String]) -> Result<Vec<String>> {
    if emails.is_empty() {
        return Ok(vec![]);
    }
    let wanted: Vec<String> = emails.iter().map(|e| e.trim().to_lowercase()).collect();
    let known: Vec<String> = sqlx::query_scalar("select lower(email::text) from app_user where tenant_id = $1 and lower(email::text) = any($2)")
        .bind(tenant)
        .bind(&wanted)
        .fetch_all(&mut **tx)
        .await?;
    Ok(wanted.into_iter().filter(|e| !known.contains(e)).collect())
}
