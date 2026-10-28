//! CRM: leads, opportunities and pipeline, prospects, contacts and addresses, campaigns, activities and quotations.
//!
//! Quotations are priced by the selling engine (`ferrum_domain::pricing`) and turn into sales
//! orders; the orders' quantities flow back onto the quotation lines and win the opportunity
//! behind them. Leads, opportunities, activities and quotations are also owned by the user named
//! in `assigned_to` (`owner_fields`), so own-records permissions cover what is assigned to a rep.

mod activity;
mod contact;
mod lead;
mod opportunity;
mod quotation;
mod reports;

use chrono::NaiveDate;
use ferrum_domain::reports::ReportSource;
use ferrum_domain::util::{find, id, s, sys};
use ferrum_kernel::{Doc, Error, Filter, HookCtx, HookRegistry, Kernel, Result};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

/// Registers this package's document hooks. The `SalesOrder` hook runs after the selling hooks.
pub fn register(hooks: &mut HookRegistry) {
    hooks
        .register("Lead", Arc::new(lead::LeadHook))
        .register("Opportunity", Arc::new(opportunity::OpportunityHook))
        .register("Quotation", Arc::new(quotation::QuotationHook))
        .register("SalesOrder", Arc::new(quotation::SalesOrderHook))
        .register("Activity", Arc::new(activity::ActivityHook))
        .register("Contact", Arc::new(contact::ContactHook))
        .register("Address", Arc::new(contact::AddressHook));
}

/// This package's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![Arc::new(reports::CrmReports)]
}

/// Periodic upkeep (every few minutes on every replica; must be idempotent and safe to run concurrently).
pub async fn maintenance(k: &Kernel) -> Result<()> {
    quotation::expire_all(k).await
}

pub(crate) fn today() -> NaiveDate {
    chrono::Utc::now().date_naive()
}

/// "First Last" from the parts that are present.
pub(crate) fn person_name(first: Option<&str>, last: Option<&str>) -> String {
    [first, last].into_iter().flatten().map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

/// A record by id, read with engine rights; `None` when absent (link validation reports bad ids).
pub(crate) async fn lookup(hc: &mut HookCtx<'_>, entity: &str, record: Option<Uuid>) -> Result<Option<Doc>> {
    let Some(record) = record else { return Ok(None) };
    match hc.kernel.get_tx(&sys(hc.ctx), hc.tx, entity, record, false).await {
        Ok(doc) => Ok(Some(doc)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The saved version of the document being validated (`None` while it is being created).
pub(crate) async fn stored(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Option<Doc>> {
    let entity = hc.entity.def.name.clone();
    lookup(hc, &entity, id(doc, "id")).await
}

/// The assignee must be an active user: an unknown address would leave the record owned by
/// nobody. Checked when it is set or changed, so a later deactivation does not block edits.
pub(crate) async fn check_assignee(hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
    let email = s(doc, "assigned_to");
    // Engine contexts (automations, imports) carry a placeholder address of their own.
    if hc.ctx.is_system || old.is_some_and(|o| s(o, "assigned_to") == email) {
        return Ok(());
    }
    let Some(email) = email else { return Ok(()) };
    let active: bool = sqlx::query_scalar("select exists(select 1 from app_user where tenant_id = $1 and email = $2::citext and is_active)")
        .bind(hc.ctx.tenant)
        .bind(email)
        .fetch_one(&mut **hc.tx)
        .await?;
    if active { Ok(()) } else { Err(Error::field("assigned_to", format!("{email} is not an active user"))) }
}

/// The `lost_reason` action parameter (a Lost Reason id or its name), if given.
pub(crate) async fn lost_reason(hc: &mut HookCtx<'_>, params: &Value) -> Result<Option<Uuid>> {
    let Some(raw) = params.get("lost_reason").and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()) else { return Ok(None) };
    let filter = match raw.parse::<Uuid>() {
        Ok(u) => Filter::eq("id", u.to_string()),
        Err(_) => Filter::eq("reason", raw),
    };
    let found = find(hc.kernel, hc.ctx, hc.tx, "LostReason", vec![filter], None, 1).await?;
    found.first().and_then(|r| id(r, "id")).map(Some).ok_or_else(|| Error::field("lost_reason", format!("'{raw}' is not a lost reason")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn person_names() {
        assert_eq!(person_name(Some(" Jane "), Some("Doe")), "Jane Doe");
        assert_eq!(person_name(None, Some("Doe")), "Doe");
        assert_eq!(person_name(Some(""), None), "");
    }
}
