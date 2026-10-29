//! Dunning: reminders for a customer's overdue installments.
//!
//! A dunning type defines escalation levels: from how many days overdue, a fee, a yearly interest
//! rate and a letter. A dunning lists the customer's installments overdue on its date in one
//! currency (payments settle installments in due-date order), takes the level its most overdue
//! installment reached, and charges interest per installment by the actual/actual day count plus
//! the level's fee. Submitting books fee and interest on the customer's receivable account as an
//! open item, paid like an invoice; the letter can be emailed. The upkeep drafts dunnings once a
//! day for customers whose overdue installments reached a level not dunned yet, and marks
//! dunnings resolved once everything they listed is paid.

use crate::common::{claim_daily, fmt};
use crate::rules;
use crate::terms::{open_installments, schedules};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::ledger::{self, Entry, Line, OpenItem};
use ferrum_domain::util::{b, company, currency, d, date, exchange_rate, get, id, money, round, rows, s, set, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashSet;
use uuid::Uuid;

const DEFAULT_LETTER: &str = "Dear {{ customer_name }},

According to our records the following payments were overdue on {{ posting_date }}:
{% for r in overdue %}
- {{ r.invoice }}{% if r.installment %} ({{ r.installment }}){% endif %}: {{ r.outstanding }} {{ currency }}, due {{ r.due_date }}, {{ r.overdue_days }} days overdue{% endfor %}

Total overdue: {{ total_outstanding }} {{ currency }}{% if has_charges %}
Interest ({{ interest_rate }}% a year): {{ interest_amount }} {{ currency }}
Dunning fee: {{ dunning_fee }} {{ currency }}{% endif %}

Please pay {{ grand_total }} {{ currency }} at your earliest convenience. If you have paid in the meantime, please disregard this letter.

{{ company }}";

/// A level of a dunning type.
#[derive(Debug, Clone)]
struct Level {
    level: i64,
    days: i64,
    fee: Decimal,
    rate: Decimal,
    subject: Option<String>,
    letter: Option<String>,
}

fn levels(dunning_type: &Doc) -> Vec<Level> {
    let mut out: Vec<Level> = rows(dunning_type, "levels")
        .iter()
        .map(|r| Level {
            level: crate::common::int(r, "level"),
            days: crate::common::int(r, "days_overdue"),
            fee: d(r, "dunning_fee"),
            rate: d(r, "interest_rate"),
            subject: s(r, "subject").map(str::to_string),
            letter: s(r, "letter_text").map(str::to_string),
        })
        .collect();
    out.sort_by_key(|l| l.level);
    out
}

/// The level reached by installments overdue up to `max_days`.
fn level_for(levels: &[Level], max_days: i64) -> Option<&Level> {
    levels.iter().filter(|l| l.days <= max_days).max_by_key(|l| l.level)
}

pub struct DunningTypeHook;

#[async_trait]
impl DocHook for DunningTypeHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let all = levels(doc);
        let mut seen = HashSet::new();
        for l in &all {
            if !seen.insert(l.level) {
                errors.push(FieldError::new("levels", format!("level {} appears twice", l.level)));
            }
        }
        if all.windows(2).any(|w| w[1].days <= w[0].days) {
            errors.push(FieldError::new("levels", "each higher level must start at more days overdue than the one before"));
        }
        for field in ["income_account", "interest_income_account", "cost_center"] {
            if let Some(x) = id(doc, field) {
                let entity = if field == "cost_center" { "CostCenter" } else { "Account" };
                if id(&get(hc.kernel, hc.ctx, hc.tx, entity, x).await?, "company") != id(doc, "company") {
                    errors.push(FieldError::new(field, "belongs to another company"));
                }
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        if b(doc, "is_default") {
            sqlx::query("update e_dunning_type set is_default = false, updated_at = now() where tenant_id = $1 and company = $2 and id <> $3 and is_default")
                .bind(hc.ctx.tenant)
                .bind(id(doc, "company"))
                .bind(id(doc, "id"))
                .execute(&mut **hc.tx)
                .await?;
        }
        Ok(())
    }
}

/// One overdue installment.
#[derive(Debug, Clone)]
struct Overdue {
    invoice: Uuid,
    number: String,
    installment: String,
    due: NaiveDate,
    days: i64,
    outstanding: Decimal,
}

/// The customer's installments overdue on `on` in currency `code`, as of that day.
async fn overdue(tx: &mut Tx, tenant: Uuid, company: Uuid, customer: Uuid, code: &str, on: NaiveDate) -> Result<Vec<Overdue>> {
    let items: Vec<(Uuid, Option<String>, NaiveDate, Option<NaiveDate>, Decimal, Decimal)> = sqlx::query_as(
        "select o.id, o.number, o.posting_date, o.due_date, o.amount, \
         o.amount - coalesce((select sum(x.amount) from allocation x where x.tenant_id = o.tenant_id and x.open_item_id = o.id and x.posting_date <= $5), 0) \
         from open_item o where o.tenant_id = $1 and o.company_id = $2 and o.customer_id = $3 and trim(o.currency) = $4 and o.entity = 'SalesInvoice' \
         and o.status <> 'cancelled' and o.posting_date <= $5 and o.amount > 0 order by coalesce(o.due_date, o.posting_date), o.posting_date, o.id",
    )
    .bind(tenant)
    .bind(company)
    .bind(customer)
    .bind(code)
    .bind(on)
    .fetch_all(&mut **tx)
    .await?;
    let items: Vec<_> = items.into_iter().filter(|i| i.5 > Decimal::ZERO).collect();
    let ids: Vec<Uuid> = items.iter().map(|i| i.0).collect();
    let plans = schedules(tx, tenant, "e_sales_invoice", &ids).await?;
    let mut out = Vec::new();
    for (inv, number, posted, due, amount, outstanding) in items {
        for (inst, open) in open_installments(plans.get(&inv), amount, outstanding, due.unwrap_or(posted)) {
            if open > Decimal::ZERO && inst.due < on {
                out.push(Overdue {
                    invoice: inv,
                    number: number.clone().unwrap_or_default(),
                    installment: inst.description,
                    due: inst.due,
                    days: (on - inst.due).num_days(),
                    outstanding: open,
                });
            }
        }
    }
    Ok(out)
}

async fn default_type(tx: &mut Tx, k: &Kernel, ctx: &Ctx, company: Uuid, customer: Uuid) -> Result<Option<Uuid>> {
    let c = get(k, ctx, tx, "Customer", customer).await?;
    if let Some(t) = id(&c, "fin_dunning_type") {
        return Ok(Some(t));
    }
    Ok(sqlx::query_scalar("select id from e_dunning_type where tenant_id = $1 and company = $2 and is_default order by created_at limit 1")
        .bind(ctx.tenant)
        .bind(company)
        .fetch_optional(&mut **tx)
        .await?)
}

fn render(template: &str, vars: &Value) -> Result<String> {
    let mut env = minijinja::Environment::new();
    env.set_fuel(Some(500_000));
    env.add_template("letter", template).map_err(|e| Error::field("letter_text", format!("the letter template is invalid: {e}")))?;
    let t = env.get_template("letter").map_err(Error::internal)?;
    t.render(minijinja::Value::from_serialize(vars)).map_err(|e| Error::field("letter_text", format!("the letter could not be written: {e}")))
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub struct DunningHook;

impl DunningHook {
    /// Lists the overdue installments on the dunning's date and derives level, interest, fee,
    /// totals and the letter.
    async fn compute(hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let (Some(cid), Some(customer)) = (id(doc, "company"), id(doc, "customer")) else { return Ok(()) };
        let on = date(doc, "posting_date")?;
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        if id(doc, "dunning_type").is_none()
            && let Some(t) = default_type(hc.tx, hc.kernel, hc.ctx, cid, customer).await?
        {
            set(doc, "dunning_type", t.to_string());
        }
        if id(doc, "currency").is_none() {
            set(doc, "currency", co.currency_id.to_string());
        }
        let cur = id(doc, "currency").unwrap_or(co.currency_id);
        let (code, precision) = currency(hc.kernel, hc.ctx, hc.tx, cur).await?;
        let rate = exchange_rate(hc.kernel, hc.ctx, hc.tx, cur, co.currency_id, on).await?;
        let cust = get(hc.kernel, hc.ctx, hc.tx, "Customer", customer).await?;
        let debit_to = id(&cust, "receivable_account").or(co.account("default_receivable_account"));
        set(doc, "debit_to", debit_to.map(|a| json!(a.to_string())).unwrap_or(Value::Null));
        let found = overdue(hc.tx, hc.ctx.tenant, cid, customer, &code, on).await?;
        let dunning_type = match id(doc, "dunning_type") {
            Some(t) => Some(get(hc.kernel, hc.ctx, hc.tx, "DunningType", t).await?),
            None => None,
        };
        let all = dunning_type.as_ref().map(levels).unwrap_or_default();
        let max_days = found.iter().map(|o| o.days).max().unwrap_or(0);
        let level = if found.is_empty() { None } else { level_for(&all, max_days).cloned() };
        let (fee, interest_rate) = level.as_ref().map(|l| (round(l.fee, precision), l.rate)).unwrap_or_default();
        let mut total = Decimal::ZERO;
        let mut interest_total = Decimal::ZERO;
        let mut lines = Vec::new();
        let mut letter_rows = Vec::new();
        for o in &found {
            let interest = rules::interest(o.outstanding, interest_rate, o.due, on, precision);
            total += o.outstanding;
            interest_total += interest;
            lines.push(
                json!({"sales_invoice": o.invoice.to_string(), "invoice_number": o.number, "installment": o.installment,
                "due_date": o.due.to_string(), "overdue_days": o.days, "outstanding": money(o.outstanding), "interest": money(interest)})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            );
            letter_rows.push(json!({"invoice": o.number, "installment": o.installment, "due_date": o.due.to_string(), "overdue_days": o.days,
                "outstanding": fmt(o.outstanding, precision), "interest": fmt(interest, precision)}));
        }
        let charges = fee + interest_total;
        set(doc, "dunning_level", level.as_ref().map(|l| json!(l.level)).unwrap_or(Value::Null));
        set(doc, "conversion_rate", money(rate));
        set(doc, "overdue_payments", crate::common::child_rows(lines));
        set(doc, "total_outstanding", money(total));
        set(doc, "interest_rate", money(interest_rate));
        set(doc, "interest_amount", money(interest_total));
        set(doc, "dunning_fee", money(fee));
        set(doc, "dunning_amount", money(charges));
        set(doc, "grand_total", money(total + charges));
        set(doc, "base_dunning_amount", money(round(fee * rate, co.precision) + round(interest_total * rate, co.precision)));
        set(doc, "outstanding_amount", money(charges));
        set(doc, "status", json!("Draft"));
        let vars = json!({
            "customer_name": s(&cust, "customer_name").unwrap_or_default(), "company": co.name(), "posting_date": on.to_string(),
            "dunning_level": level.as_ref().map(|l| l.level), "currency": code, "total_outstanding": fmt(total, precision),
            "interest_rate": interest_rate.normalize().to_string(), "interest_amount": fmt(interest_total, precision), "dunning_fee": fmt(fee, precision),
            "dunning_amount": fmt(charges, precision), "grand_total": fmt(total + charges, precision), "has_charges": !charges.is_zero(),
            "overdue": letter_rows,
        });
        let template = level.as_ref().and_then(|l| l.letter.clone()).unwrap_or_else(|| DEFAULT_LETTER.to_string());
        set(doc, "letter", render(&template, &vars)?);
        let subject_template = level.as_ref().and_then(|l| l.subject.clone()).unwrap_or_else(|| "Payment reminder from {{ company }}".to_string());
        set(doc, "letter_subject", render(&subject_template, &vars)?.trim().to_string());
        Ok(())
    }
}

#[async_trait]
impl DocHook for DunningHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if d(doc, "docstatus").is_zero() {
            Self::compute(hc, doc).await?;
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let Some(t) = id(doc, "dunning_type") {
            let dt = get(hc.kernel, hc.ctx, hc.tx, "DunningType", t).await?;
            if id(&dt, "company") != id(doc, "company") {
                return Err(Error::field("dunning_type", "belongs to another company"));
            }
            let overdue = rows(doc, "overdue_payments");
            if !overdue.is_empty() && doc.get("dunning_level").is_none_or(Value::is_null) {
                let max = overdue.iter().map(|r| crate::common::int(r, "overdue_days")).max().unwrap_or(0);
                let first = levels(&dt).first().map(|l| l.days).unwrap_or(0);
                return Err(Error::field(
                    "dunning_type",
                    format!("no level applies yet: the oldest installment is {max} days overdue, the first level starts at {first}"),
                ));
            }
        }
        if id(doc, "debit_to").is_none() {
            return Err(Error::field("debit_to", "no receivable account for this customer or company"));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let did = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let charges = d(doc, "dunning_amount");
        if charges > Decimal::ZERO {
            let (code, _) = currency(hc.kernel, hc.ctx, hc.tx, id(doc, "currency").unwrap_or(co.currency_id)).await?;
            let rate = d(doc, "conversion_rate");
            let customer = id(doc, "customer");
            let receivable = id(doc, "debit_to").ok_or_else(|| Error::field("debit_to", "is required"))?;
            let dt = get(hc.kernel, hc.ctx, hc.tx, "DunningType", id(doc, "dunning_type").unwrap_or_default()).await?;
            let income = match id(&dt, "income_account") {
                Some(a) => a,
                None => co.require("default_income_account", "default income account")?,
            };
            let interest_account = id(&dt, "interest_income_account").unwrap_or(income);
            let (fee, interest) = (d(doc, "dunning_fee"), d(doc, "interest_amount"));
            let (base_fee, base_interest) = (round(fee * rate, co.precision), round(interest * rate, co.precision));
            let mut entry = Entry::new(co.id, on, "Dunning", did, s(doc, "number").map(str::to_string));
            entry.memo = Some(format!("Dunning level {} for {}", crate::common::int(doc, "dunning_level"), s(doc, "customer_name").unwrap_or_default()));
            entry
                .lines
                .push(Line::new(receivable, base_fee + base_interest, &co.currency_code).txn(charges, &code, rate).customer(customer).against("Dunning", did));
            for (account, base, txn, what) in
                [(income, base_fee, fee, "Dunning fee"), (interest_account, base_interest, interest, "Interest on overdue payments")]
            {
                if !txn.is_zero() {
                    entry.lines.push(Line::new(account, -base, &co.currency_code).txn(-txn, &code, rate).cost_center(id(&dt, "cost_center")).remarks(what));
                }
            }
            ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
            ledger::open_item(
                hc.tx,
                hc.ctx,
                &OpenItem {
                    id: did,
                    company: co.id,
                    entity: "Dunning".into(),
                    number: s(doc, "number").map(str::to_string),
                    customer,
                    supplier: None,
                    account: receivable,
                    posting_date: on,
                    due_date: Some(on),
                    currency: code,
                    exchange_rate: rate,
                    amount: charges,
                    base_amount: base_fee + base_interest,
                },
            )
            .await?;
        }
        let mut v = Doc::new();
        v.insert("status".into(), json!("Unresolved"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "Dunning", did, v).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let did = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        if ledger::load_open_item(hc.tx, hc.ctx.tenant, did, true).await?.is_some() {
            ledger::cancel_open_item(hc.tx, hc.ctx, did).await?;
        }
        let (on, _) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        ledger::reverse(hc.tx, hc.ctx, "Dunning", did, on, co.precision).await?;
        let mut v = Doc::new();
        v.insert("status".into(), json!("Cancelled"));
        v.insert("outstanding_amount".into(), money(Decimal::ZERO));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "Dunning", did, v).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "send_letter" {
            return Ok(None);
        }
        let mailer = hc.kernel.mailer.clone().ok_or_else(|| Error::Unavailable("no mailer is configured on this server".into()))?;
        let customer = get(hc.kernel, hc.ctx, hc.tx, "Customer", id(doc, "customer").unwrap_or_default()).await?;
        let to = params.get("to").and_then(Value::as_str).map(str::to_string).or_else(|| s(&customer, "email").map(str::to_string));
        let to = to.filter(|t| t.contains('@')).ok_or_else(|| Error::field("to", "the customer has no email address; give one"))?;
        let subject = s(doc, "letter_subject").unwrap_or("Payment reminder").to_string();
        let html = format!("<div style=\"font-family:sans-serif;white-space:pre-wrap\">{}</div>", html_escape(s(doc, "letter").unwrap_or_default()));
        mailer.send(&to, &subject, &html).await?;
        let mut v = Doc::new();
        v.insert("letter_sent_to".into(), json!(to));
        v.insert("letter_sent_at".into(), json!(chrono::Utc::now().to_rfc3339()));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "Dunning", id(doc, "id").unwrap_or_default(), v).await?;
        Ok(Some(json!({"sent_to": to})))
    }
}

/// Daily: resolve dunnings whose items are all paid, then draft new ones where a customer's
/// overdue installments reached a level not dunned yet (idempotent: an open draft is refreshed
/// to today instead of duplicated, and a submitted unresolved dunning at that level suffices).
pub async fn upkeep(k: &Kernel, ctx: &Ctx) -> Result<()> {
    let today = ctx.today();
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    if !claim_daily(&mut tx, ctx.tenant, "dunning", today).await? {
        tx.rollback().await?;
        return Ok(());
    }
    sqlx::query(
        "update e_dunning d set outstanding_amount = coalesce((select o.outstanding from open_item o where o.tenant_id = d.tenant_id and o.id = d.id \
         and o.status <> 'cancelled'), 0), updated_at = now() where d.tenant_id = $1 and d.docstatus = 1 and d.status = 'Unresolved'",
    )
    .bind(ctx.tenant)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "update e_dunning d set status = 'Resolved', updated_at = now() where d.tenant_id = $1 and d.docstatus = 1 and d.status = 'Unresolved' \
         and not exists (select 1 from open_item o where o.tenant_id = d.tenant_id and o.id = d.id and o.status = 'open' and o.outstanding <> 0) \
         and not exists (select 1 from e_dunning__overdue_payments r join open_item o on o.tenant_id = r.tenant_id and o.id = r.sales_invoice \
                         where r.tenant_id = d.tenant_id and r.parent_id = d.id and o.status = 'open' and o.outstanding > 0)",
    )
    .bind(ctx.tenant)
    .execute(&mut *tx)
    .await?;
    let candidates: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "select distinct o.company_id, o.customer_id, trim(o.currency) from open_item o where o.tenant_id = $1 and o.entity = 'SalesInvoice' \
         and o.status = 'open' and o.outstanding > 0 and o.posting_date < $2 and (o.due_date < $2 or exists (select 1 from \
         e_sales_invoice__fin_payment_schedule s where s.tenant_id = o.tenant_id and s.parent_id = o.id and s.due_date < $2))",
    )
    .bind(ctx.tenant)
    .bind(today)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for (cid, customer, code) in candidates {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        match draft(k, ctx, &mut tx, cid, customer, &code, today).await {
            Ok(()) => tx.commit().await?,
            Err(e) => {
                tx.rollback().await?;
                tracing::warn!(tenant = %ctx.tenant, %customer, "dunning not drafted: {e}");
            }
        }
    }
    Ok(())
}

async fn draft(k: &Kernel, ctx: &Ctx, tx: &mut Tx, cid: Uuid, customer: Uuid, code: &str, today: NaiveDate) -> Result<()> {
    let Some(dtype) = default_type(tx, k, ctx, cid, customer).await? else { return Ok(()) };
    let Some(cur) = sqlx::query_scalar::<_, Uuid>("select id from e_currency where tenant_id = $1 and code = $2")
        .bind(ctx.tenant)
        .bind(code)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(());
    };
    let existing: Vec<(Uuid, i16, Option<i64>, NaiveDate)> = sqlx::query_as(
        "select id, docstatus, dunning_level, posting_date from e_dunning where tenant_id = $1 and company = $2 and customer = $3 and currency = $4 \
         and dunning_type = $5 and (docstatus = 0 or (docstatus = 1 and status = 'Unresolved'))",
    )
    .bind(ctx.tenant)
    .bind(cid)
    .bind(customer)
    .bind(cur)
    .bind(dtype)
    .fetch_all(&mut **tx)
    .await?;
    if let Some((open_draft, _, _, day)) = existing.iter().find(|e| e.1 == 0) {
        // Refresh the waiting draft to today: its rows, level and charges follow.
        if *day < today {
            let mut patch = Doc::new();
            patch.insert("posting_date".into(), json!(today.to_string()));
            k.update_tx(ctx, tx, "Dunning", *open_draft, patch, None).await?;
        }
        return Ok(());
    }
    let dt = get(k, ctx, tx, "DunningType", dtype).await?;
    let found = overdue(tx, ctx.tenant, cid, customer, code, today).await?;
    let max_days = found.iter().map(|o| o.days).max().unwrap_or(0);
    let Some(level) = level_for(&levels(&dt), max_days).map(|l| l.level) else { return Ok(()) };
    if existing.iter().any(|e| e.2.unwrap_or(0) >= level) {
        return Ok(());
    }
    let mut input = Doc::new();
    input.insert("company".into(), json!(cid.to_string()));
    input.insert("customer".into(), json!(customer.to_string()));
    input.insert("currency".into(), json!(cur.to_string()));
    input.insert("dunning_type".into(), json!(dtype.to_string()));
    input.insert("posting_date".into(), json!(today.to_string()));
    k.insert_tx(ctx, tx, "Dunning", input).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_escalate_by_days_overdue() {
        let mk = |level, days| Level { level, days, fee: Decimal::ZERO, rate: Decimal::ZERO, subject: None, letter: None };
        let all = vec![mk(1, 0), mk(2, 30), mk(3, 60)];
        assert_eq!(level_for(&all, 5).map(|l| l.level), Some(1));
        assert_eq!(level_for(&all, 30).map(|l| l.level), Some(2));
        assert_eq!(level_for(&all, 365).map(|l| l.level), Some(3));
        assert!(level_for(&[mk(1, 10)], 9).is_none());
    }

    #[test]
    fn the_default_letter_renders() {
        let vars = json!({"customer_name": "Acme", "company": "Us", "posting_date": "2026-03-01", "currency": "EUR", "total_outstanding": "100.00",
            "interest_rate": "8", "interest_amount": "1.20", "dunning_fee": "5.00", "dunning_amount": "6.20", "grand_total": "106.20", "has_charges": true,
            "overdue": [{"invoice": "SINV-1", "installment": "", "due_date": "2026-01-31", "overdue_days": 29, "outstanding": "100.00"}]});
        let text = render(DEFAULT_LETTER, &vars).unwrap();
        assert!(
            text.contains("Dear Acme,") && text.contains("- SINV-1: 100.00 EUR, due 2026-01-31, 29 days overdue") && text.contains("Please pay 106.20 EUR"),
            "{text}"
        );
        assert!(render("{{ broken", &vars).is_err());
        assert_eq!(html_escape("<b>&"), "&lt;b&gt;&amp;");
    }
}
