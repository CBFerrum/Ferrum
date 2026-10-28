//! Support: tickets with SLAs on business hours, teams and assignment, knowledge base, warranty claims and maintenance.
//!
//! Hooks are registered per entity, so a tenant without the `support` package never runs them.

mod kb;
mod reports;
mod sla;
mod ticket;
mod visits;
mod warranty;

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{Ctx, Doc, Error, HookRegistry, Kernel, Result, Tx};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

/// Registers this package's document hooks.
pub fn register(hooks: &mut HookRegistry) {
    hooks
        .register("Ticket", Arc::new(ticket::TicketHook))
        .register("ServiceLevelAgreement", Arc::new(ticket::SlaHook))
        .register("SupportTeam", Arc::new(ticket::TeamHook))
        .register("TicketAssignmentRule", Arc::new(ticket::RuleHook))
        .register("KbArticle", Arc::new(kb::ArticleHook))
        .register("WarrantyClaim", Arc::new(warranty::ClaimHook))
        .register("MaintenanceSchedule", Arc::new(visits::ScheduleHook))
        .register("MaintenanceVisit", Arc::new(visits::VisitHook));
}

/// This package's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![Arc::new(reports::SupportReports)]
}

/// Periodic upkeep (every few minutes on every replica; must be idempotent and safe to run concurrently):
/// re-evaluates the SLA of open tickets and closes tickets resolved longer than the grace period.
/// A failing tenant is logged and does not stop the others; the first error is returned.
pub async fn maintenance(k: &Kernel) -> Result<()> {
    let mut tx = k.db.system_tx().await?;
    let tenants: Vec<Uuid> = sqlx::query_scalar("select id from tenant where status = 'active' order by id").fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let mut first_error = None;
    for tenant in tenants {
        let res = async {
            let schema = k.registry.schema(tenant).await?;
            if schema.entity("Ticket").is_none() {
                return Ok(());
            }
            let ctx = Ctx::system(tenant, schema);
            let refreshed = ticket::refresh_slas(k, &ctx).await?;
            let closed = ticket::auto_close(k, &ctx).await?;
            if refreshed + closed > 0 {
                tracing::info!(%tenant, refreshed, closed, "support maintenance");
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(%tenant, "support maintenance failed: {e}");
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// A whole-number field (missing or empty reads as 0).
fn int(doc: &Doc, key: &str) -> i64 {
    doc.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Quoted physical table of a child table field (`e_parent__field`, shortened when long).
fn child_table(ctx: &Ctx, entity: &str, field: &str) -> Result<String> {
    ctx.schema
        .entity(entity)
        .and_then(|e| e.children.get(field))
        .map(|c| ferrum_kernel::ddl::q(&c.table))
        .ok_or_else(|| Error::internal(format!("{entity}.{field} is not a table")))
}

/// A linked record, or `None` when it does not exist (link validation reports that to the user).
async fn linked(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid) -> Result<Option<Doc>> {
    match ferrum_domain::util::get(k, ctx, tx, entity, id).await {
        Ok(d) => Ok(Some(d)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}
