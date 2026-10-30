//! Period closing: at a fiscal year's end every income and expense balance (per cost center)
//! moves to the closing account in one balanced entry dated the year's last day, and the year's
//! accounting periods are closed through the ledger's period lock, so the database refuses any
//! later posting into the year. Cancelling reopens what the closing closed and reverses its entry
//! on the same date. Years close in order: never before the previous year, never under a later one.

use crate::common::{FiscalYear, fiscal_year};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::ledger::{self, Entry, Line};
use ferrum_domain::util::{company, elevated, get, id, money, s, set, sys};
use ferrum_kernel::store::field_expr;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

const ENTITY: &str = "PeriodClosingVoucher";

/// Documents that post to the ledger when submitted (besides any entity found in the year's journal).
const POSTING: &[&str] = &[
    "JournalEntry",
    "PaymentEntry",
    "SalesInvoice",
    "PurchaseInvoice",
    "DeliveryNote",
    "PurchaseReceipt",
    "StockEntry",
    "Dunning",
    "PosInvoice",
    "PosClosingEntry",
    "LandedCostVoucher",
    "ExpenseClaim",
    "PayrollEntry",
    "SalarySlip",
];

async fn year(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<FiscalYear> {
    let fy = id(doc, "fiscal_year").ok_or_else(|| Error::field("fiscal_year", "is required"))?;
    let (owner, fy) = fiscal_year(hc.tx, hc.ctx.tenant, fy).await?.ok_or_else(|| Error::field("fiscal_year", "does not exist"))?;
    if Some(owner) != id(doc, "company") {
        return Err(Error::field("fiscal_year", "belongs to another company"));
    }
    Ok(fy)
}

/// The company's accounting periods within the year: (first day, last day, closed).
async fn periods(hc: &mut HookCtx<'_>, company: Uuid, fy: &FiscalYear) -> Result<Vec<(NaiveDate, NaiveDate, bool)>> {
    Ok(sqlx::query_as(
        "select period_start, period_end, status = 'closed' from accounting_period where tenant_id = $1 and company_id = $2 \
         and period_start >= $3 and period_end <= $4 order by period_start",
    )
    .bind(hc.ctx.tenant)
    .bind(company)
    .bind(fy.start)
    .bind(fy.end)
    .fetch_all(&mut **hc.tx)
    .await?)
}

/// Closes or reopens one month through the fiscal year's own action (the platform's period lock),
/// with engine rights: submitting or cancelling the closing is the authorization.
async fn set_period(hc: &mut HookCtx<'_>, fy: Uuid, start: NaiveDate, close: bool) -> Result<()> {
    let action = if close { "close_period" } else { "reopen_period" };
    hc.kernel.run_action_tx(&elevated(hc.ctx), hc.tx, "FiscalYear", fy, action, &json!({"period_start": start.to_string()})).await?;
    Ok(())
}

/// Submitted closings of the company: (number, year end).
async fn closings(hc: &mut HookCtx<'_>, company: Uuid, except: Option<Uuid>) -> Result<Vec<(Option<String>, NaiveDate, Option<Uuid>)>> {
    Ok(sqlx::query_as(
        "select number, posting_date, fiscal_year from e_period_closing_voucher where tenant_id = $1 and company = $2 and docstatus = 1 \
         and ($3::uuid is null or id <> $3)",
    )
    .bind(hc.ctx.tenant)
    .bind(company)
    .bind(except)
    .fetch_all(&mut **hc.tx)
    .await?)
}

pub struct ClosingHook;

#[async_trait]
impl DocHook for ClosingHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if let Some(fy) = id(doc, "fiscal_year")
            && let Some((_, fy)) = fiscal_year(hc.tx, hc.ctx.tenant, fy).await?
        {
            set(doc, "posting_date", fy.end.to_string());
        }
        if id(doc, "closing_account").is_none()
            && let Some(cid) = id(doc, "company")
        {
            let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
            if let Some(re) = co.account("retained_earnings_account") {
                set(doc, "closing_account", re.to_string());
            }
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        year(hc, doc).await?;
        if let Some(acc) = id(doc, "closing_account") {
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
            if id(&a, "company") != id(doc, "company") {
                return Err(Error::field("closing_account", "belongs to another company"));
            }
            if !matches!(s(&a, "root_type"), Some("Equity" | "Liability")) {
                return Err(Error::field("closing_account", "close the year into an equity (retained earnings) or liability account"));
            }
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let fy = year(hc, doc).await?;
        let others = closings(hc, cid, id(doc, "id")).await?;
        if let Some((n, _, _)) = others.iter().find(|(_, _, y)| *y == Some(fy.id)) {
            return Err(Error::invalid(format!("fiscal year {} is already closed by {}", fy.name, n.clone().unwrap_or_default())));
        }
        if let Some((n, end, _)) = others.iter().filter(|(_, end, _)| *end > fy.end).max_by_key(|(_, end, _)| *end) {
            return Err(Error::invalid(format!("a later year is already closed ({} on {end}); cancel that closing first", n.clone().unwrap_or_default())));
        }
        // The previous year must be closed first when it had any postings.
        let day_before = fy.start.pred_opt().unwrap_or(fy.start);
        if let Some(prev) = crate::common::fiscal_year_of(hc.tx, hc.ctx.tenant, cid, day_before).await? {
            let active: bool =
                sqlx::query_scalar("select exists(select 1 from journal_entry where tenant_id = $1 and company_id = $2 and posting_date between $3 and $4)")
                    .bind(hc.ctx.tenant)
                    .bind(cid)
                    .bind(prev.start)
                    .bind(prev.end)
                    .fetch_one(&mut **hc.tx)
                    .await?;
            if active && !others.iter().any(|(_, _, y)| *y == Some(prev.id)) {
                return Err(Error::invalid(format!("close the previous fiscal year {} first", prev.name)));
            }
        }
        // Nothing may still be waiting to post into the year.
        let mut entities: Vec<String> = POSTING.iter().map(|e| e.to_string()).collect();
        let found: Vec<String> =
            sqlx::query_scalar("select distinct source_entity from journal_entry where tenant_id = $1 and company_id = $2 and posting_date between $3 and $4")
                .bind(hc.ctx.tenant)
                .bind(cid)
                .bind(fy.start)
                .bind(fy.end)
                .fetch_all(&mut **hc.tx)
                .await?;
        entities.extend(found);
        entities.sort();
        entities.dedup();
        let mut drafts = Vec::new();
        for name in entities.iter().filter(|e| e.as_str() != ENTITY) {
            let Some(ent) = hc.ctx.schema.entity(name).cloned() else { continue };
            let (Some(c), Some(p)) = (field_expr(&ent, "company", "a"), field_expr(&ent, "posting_date", "a")) else { continue };
            if !ent.def.submittable || ent.custom {
                continue;
            }
            let sql = format!(
                "select count(*) from {} a where a.tenant_id = $1 and {c} = $2 and a.docstatus = 0 and {p} between $3 and $4",
                ferrum_kernel::ddl::q(&ent.table)
            );
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(hc.ctx.tenant).bind(cid).bind(fy.start).bind(fy.end).fetch_one(&mut **hc.tx).await?;
            if n > 0 {
                drafts.push(format!("{n} {}", if n == 1 { ent.def.label.clone() } else { ent.def.plural_label() }));
            }
        }
        if !drafts.is_empty() {
            return Err(Error::Validation(vec![FieldError::new(
                "fiscal_year",
                format!("{} has unposted drafts: {}; submit or delete them first", fy.name, drafts.join(", ")),
            )]));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let fy = year(hc, doc).await?;
        let closing = id(doc, "closing_account").ok_or_else(|| Error::field("closing_account", "is required"))?;
        let months = periods(hc, co.id, &fy).await?;
        // The entry is dated the year's last day; open that month for it if it was closed already.
        let last = months.iter().find(|(a, b, _)| *a <= fy.end && fy.end <= *b).copied();
        if let Some((start, _, true)) = last {
            set_period(hc, fy.id, start, false).await?;
        }
        let balances: Vec<(Uuid, Option<Uuid>, Decimal)> = sqlx::query_as(
            "select l.account_id, l.cost_center_id, sum(l.debit - l.credit) from journal_line l \
             join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
             where l.tenant_id = $1 and l.company_id = $2 and l.posting_date between $3 and $4 and a.root_type in ('Income', 'Expense') \
             group by 1, 2 having sum(l.debit - l.credit) <> 0",
        )
        .bind(hc.ctx.tenant)
        .bind(co.id)
        .bind(fy.start)
        .bind(fy.end)
        .fetch_all(&mut **hc.tx)
        .await?;
        let mut entry = Entry::new(co.id, fy.end, ENTITY, pid, s(doc, "number").map(str::to_string));
        entry.memo = Some(format!("Closing of fiscal year {}", fy.name));
        let mut per_cost_center: BTreeMap<Option<Uuid>, Decimal> = BTreeMap::new();
        for (account, cc, balance) in balances {
            entry.lines.push(Line::new(account, -balance, &co.currency_code).cost_center(cc).remarks("Period closing"));
            *per_cost_center.entry(cc).or_default() += balance;
        }
        let net_loss: Decimal = per_cost_center.values().copied().sum();
        for (cc, total) in per_cost_center {
            entry.lines.push(Line::new(closing, total, &co.currency_code).cost_center(cc).remarks("Period closing"));
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        // Lock the year: every month that was open is closed now (and reopened on cancel).
        let mut closed = Vec::new();
        for (start, _, was_closed) in months {
            if !was_closed {
                closed.push(json!(start.to_string()));
            }
            set_period(hc, fy.id, start, true).await?;
        }
        let mut v = Doc::new();
        v.insert("closed_periods".into(), Value::Array(closed));
        v.insert("net_profit".into(), money(-net_loss));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, ENTITY, pid, v).await
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let fy = year(hc, doc).await?;
        let later = closings(hc, cid, id(doc, "id")).await?.into_iter().filter(|(_, end, _)| *end > fy.end).map(|(n, _, _)| n.unwrap_or_default()).next();
        if let Some(n) = later {
            return Err(Error::invalid(format!("the later closing {n} depends on this year; cancel it first")));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let fy = year(hc, doc).await?;
        let reopen: Vec<NaiveDate> = doc
            .get("closed_periods")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str()).filter_map(|v| ferrum_kernel::value::parse_date(v).ok()).collect())
            .unwrap_or_default();
        for start in &reopen {
            set_period(hc, fy.id, *start, false).await?;
        }
        // The reversal belongs on the year's last day too, even when that month stays closed.
        let last = periods(hc, co.id, &fy).await?.into_iter().find(|(a, b, _)| *a <= fy.end && fy.end <= *b);
        let relock = matches!(last, Some((_, _, true)));
        if let Some((start, _, true)) = last {
            set_period(hc, fy.id, start, false).await?;
        }
        ledger::reverse(hc.tx, hc.ctx, ENTITY, pid, fy.end, co.precision).await?;
        if relock && let Some((start, _, _)) = last {
            set_period(hc, fy.id, start, true).await?;
        }
        Ok(())
    }
}
