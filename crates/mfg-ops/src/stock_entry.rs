//! What stock entries mean to this package, after the platform posted them:
//! * manufacture entries of work orders with operations absorb the operations' cost and may
//!   not finish more than the last operation completed;
//! * transfers for a job card count as its transferred materials;
//! * transfers for a subcontracting order count as materials sent to or returned from the supplier.

use crate::installed;
use crate::job_card::add_transferred;
use crate::production_plan::refresh_plan;
use crate::subcontracting::book_transfer;
use crate::work_order::{last_completed, operating_cost_for};
use async_trait::async_trait;
use ferrum_domain::stock_entry::MANUFACTURE;
use ferrum_domain::util::{company, d, get, id, money, rows, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use rust_decimal::Decimal;

/// Finished units of the work order's item an entry produces.
fn finished(entry: &Doc, wo: &Doc) -> Decimal {
    rows(entry, "items")
        .iter()
        .filter(|r| id(r, "target_warehouse").is_some() && id(r, "source_warehouse").is_none() && id(r, "item") == id(wo, "item"))
        .map(|r| d(r, "qty"))
        .sum()
}

fn over_production(wo: &Doc, last: Decimal, already: Decimal, producing: Decimal) -> Error {
    Error::invalid(format!(
        "the last operation of work order {} completed {} and {} were already produced, so {} cannot be finished (at most {})",
        s(wo, "number").unwrap_or_default(),
        last.normalize(),
        already.normalize(),
        producing.normalize(),
        (last - already).max(Decimal::ZERO).normalize()
    ))
}

pub struct StockEntryOpsHook;

#[async_trait]
impl DocHook for StockEntryOpsHook {
    /// Operations tracked by job cards replace the BOM's standard operating cost with the work
    /// order's own (actual where operations completed, planned otherwise).
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) || !d(doc, "docstatus").is_zero() || s(doc, "purpose") != Some(MANUFACTURE) {
            return Ok(());
        }
        let (Some(wo_id), Some(cid)) = (id(doc, "mfg_work_order"), id(doc, "company")) else { return Ok(()) };
        let wo = get(hc.kernel, hc.ctx, hc.tx, "WorkOrder", wo_id).await?;
        if rows(&wo, "mfo_operations").is_empty() {
            return Ok(());
        }
        let precision = company(hc.kernel, hc.ctx, hc.tx, cid).await?.precision;
        set(doc, "mfg_additional_cost", money(operating_cost_for(&wo, finished(doc, &wo), precision)));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) || s(doc, "purpose") != Some(MANUFACTURE) {
            return Ok(());
        }
        let Some(wo_id) = id(doc, "mfg_work_order") else { return Ok(()) };
        let wo = get(hc.kernel, hc.ctx, hc.tx, "WorkOrder", wo_id).await?;
        if let Some(last) = last_completed(&wo) {
            let producing = finished(doc, &wo);
            if d(&wo, "produced_qty") + producing > last {
                return Err(over_production(&wo, last, d(&wo, "produced_qty"), producing));
            }
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        if s(doc, "purpose") == Some(MANUFACTURE)
            && let Some(wo_id) = id(doc, "mfg_work_order")
        {
            // The platform has added this entry to the (locked) work order: check the total.
            let wo = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "WorkOrder", wo_id, true).await?;
            if let Some(last) = last_completed(&wo)
                && d(&wo, "produced_qty") > last
            {
                let producing = finished(doc, &wo);
                return Err(over_production(&wo, last, d(&wo, "produced_qty") - producing, producing));
            }
            if let Some(plan) = id(&wo, "mfo_production_plan") {
                refresh_plan(hc, plan, None).await?;
            }
        }
        if let Some(card) = id(doc, "mfo_job_card") {
            let jc = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "JobCard", card, true).await?;
            if !d(&jc, "docstatus").is_zero() {
                return Err(Error::field("mfo_job_card", "the job card is already completed or cancelled"));
            }
            add_transferred(hc, card, d(doc, "mfo_job_card_qty")).await?;
        }
        book_transfer(hc, doc, Decimal::ONE).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        if s(doc, "purpose") == Some(MANUFACTURE)
            && let Some(wo_id) = id(doc, "mfg_work_order")
            && let Some(plan) = id(&get(hc.kernel, hc.ctx, hc.tx, "WorkOrder", wo_id).await?, "mfo_production_plan")
        {
            refresh_plan(hc, plan, None).await?;
        }
        if let Some(card) = id(doc, "mfo_job_card") {
            add_transferred(hc, card, -d(doc, "mfo_job_card_qty")).await?;
        }
        book_transfer(hc, doc, -Decimal::ONE).await
    }
}
