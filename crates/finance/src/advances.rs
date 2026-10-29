//! Advance payments: an invoice action that settles it with the party's unallocated payment
//! entries (oldest first) through the ledger's allocation machinery.
//!
//! Each (invoice, advance) pairing is one allocation key of its own (`AdvanceAllocation`, a new
//! id): it allocates the invoice and the advance's open item by the same amount and, when the two
//! were booked at different exchange rates, posts the difference to the exchange gain/loss
//! account under the same key. Cancelling either document first undoes its pairings (append-only
//! reversals), so the other one is open again.

use crate::common::installed;
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::flow::sync_invoice_outstanding;
use ferrum_domain::ledger::{self, Entry, Line};
use ferrum_domain::util::{company, date, id, money, round, s};
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

pub const KEY: &str = "AdvanceAllocation";

pub struct AdvanceHook;

#[async_trait]
impl DocHook for AdvanceHook {
    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let Some(iid) = id(doc, "id") else { return Ok(()) };
        undo_pairings(hc, iid).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "allocate_advances" || !installed(hc.ctx) {
            return Ok(None);
        }
        let entity = hc.entity.def.name.clone();
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let invoice =
            ledger::load_open_item(hc.tx, hc.ctx.tenant, iid, true).await?.ok_or_else(|| Error::invalid("this invoice has no receivable or payable"))?;
        if invoice.status != "open" || invoice.outstanding <= Decimal::ZERO {
            return Err(Error::invalid("nothing is outstanding on this invoice"));
        }
        let inv_date = date(doc, "posting_date")?;
        let advances: Vec<(Uuid, Option<String>, NaiveDate, Decimal, Decimal)> = sqlx::query_as(
            "select id, number, posting_date, outstanding, exchange_rate from open_item where tenant_id = $1 and company_id = $2 and entity = 'PaymentEntry' \
             and status = 'open' and outstanding < 0 and customer_id is not distinct from $3 and supplier_id is not distinct from $4 \
             and trim(currency) = $5 and account_id = $6 order by posting_date, created_at, id for update",
        )
        .bind(hc.ctx.tenant)
        .bind(co.id)
        .bind(invoice.customer_id)
        .bind(invoice.supplier_id)
        .bind(&invoice.currency)
        .bind(invoice.account_id)
        .fetch_all(&mut **hc.tx)
        .await?;
        if advances.is_empty() {
            return Err(Error::invalid("the party has no unallocated payments in this currency and account"));
        }
        let mut left = invoice.outstanding;
        let mut used = Vec::new();
        for (pid, number, adv_date, outstanding, adv_rate) in advances {
            let take = left.min(-outstanding);
            if take <= Decimal::ZERO {
                break;
            }
            let key = Uuid::now_v7();
            // Effective once both exist; today when that day's period is already closed.
            let mut on = inv_date.max(adv_date);
            if !ledger::period_open(hc.tx, hc.ctx.tenant, co.id, on).await? {
                on = hc.ctx.today();
            }
            let inv_base = round(take * invoice.exchange_rate, co.precision);
            let adv_base = round(take * adv_rate, co.precision);
            ledger::allocate(hc.tx, hc.ctx, co.id, iid, KEY, key, on, take, inv_base).await?;
            ledger::allocate(hc.tx, hc.ctx, co.id, pid, KEY, key, on, -take, -adv_base).await?;
            let diff = inv_base - adv_base;
            if !diff.is_zero() {
                // The party account keeps the base difference between the two rates; clear it.
                let sign = if invoice.customer_id.is_some() { -Decimal::ONE } else { Decimal::ONE };
                let gl = co.require("exchange_gain_loss_account", "exchange gain/loss account")?;
                let mut entry = Entry::new(co.id, on, KEY, key, s(doc, "number").map(str::to_string));
                entry.memo =
                    Some(format!("Exchange difference on advance {} against {}", number.clone().unwrap_or_default(), s(doc, "number").unwrap_or_default()));
                entry.lines.push(
                    Line::new(invoice.account_id, sign * diff, &co.currency_code)
                        .txn(Decimal::ZERO, &invoice.currency, Decimal::ONE)
                        .customer(invoice.customer_id)
                        .supplier(invoice.supplier_id)
                        .against(&entity, iid),
                );
                entry.lines.push(Line::new(gl, -sign * diff, &co.currency_code).remarks("Exchange difference"));
                ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
            }
            left -= take;
            used.push(json!({"payment_entry": pid, "number": number, "allocated": money(take)}));
            if left.is_zero() {
                break;
            }
        }
        sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, &entity, iid).await?;
        Ok(Some(json!({"allocated": money(invoice.outstanding - left), "outstanding": money(left), "advances": used})))
    }
}

/// Cancelling an advance payment first undoes the invoices it settled through this action.
pub struct AdvancePaymentHook;

#[async_trait]
impl DocHook for AdvancePaymentHook {
    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let Some(pid) = id(doc, "id") else { return Ok(()) };
        undo_pairings(hc, pid).await
    }
}

/// Reverses every live advance pairing on an open item (the invoice's or the payment's), with its
/// exchange difference, and refreshes the invoices' outstanding amounts.
async fn undo_pairings(hc: &mut HookCtx<'_>, item: Uuid) -> Result<()> {
    let keys: Vec<Uuid> = sqlx::query_scalar(
        "select distinct a.payment_id from allocation a where a.tenant_id = $1 and a.open_item_id = $2 and a.payment_entity = $3 \
         and a.reversal_of is null and not exists (select 1 from allocation r where r.tenant_id = a.tenant_id and r.reversal_of = a.id)",
    )
    .bind(hc.ctx.tenant)
    .bind(item)
    .bind(KEY)
    .fetch_all(&mut **hc.tx)
    .await?;
    let today = hc.ctx.today();
    for key in keys {
        let touched = ledger::unallocate(hc.tx, hc.ctx, KEY, key, today).await?;
        let companies: Vec<Uuid> =
            sqlx::query_scalar("select distinct company_id from journal_entry where tenant_id = $1 and source_entity = $2 and source_id = $3")
                .bind(hc.ctx.tenant)
                .bind(KEY)
                .bind(key)
                .fetch_all(&mut **hc.tx)
                .await?;
        for cid in companies {
            let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
            ledger::reverse(hc.tx, hc.ctx, KEY, key, today, co.precision).await?;
        }
        for t in touched {
            if let Some(o) = ledger::load_open_item(hc.tx, hc.ctx.tenant, t, false).await? {
                sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, &o.entity, t).await?;
            }
        }
    }
    Ok(())
}
