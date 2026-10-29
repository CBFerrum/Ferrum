//! Payment terms: templates of installments (portion, due-date rule, early payment discount) and
//! the payment schedule they produce on sales and purchase invoices and orders.
//!
//! The schedule is recomputed on every save from the template, the document date and its total
//! (the last installment absorbs the rounding), or kept as entered when there is no template and
//! must then add up to the total. An invoice's due date becomes its last installment's, so the
//! receivable's due date follows the terms. Payments are not tied to installments: they settle
//! them in due-date order, which is how reports and dunning read what is overdue.

use crate::common::{installed, is_new, opt_date};
use crate::rules;
use async_trait::async_trait;
use chrono::{Duration, NaiveDate};
use ferrum_domain::util::{b, company, currency, d, date, get, id, money, round, rows, s, set};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

pub struct TemplateHook;

#[async_trait]
impl DocHook for TemplateHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let total: Decimal = rows(doc, "terms").iter().map(|t| d(t, "invoice_portion")).sum();
        if total != Decimal::ONE_HUNDRED {
            return Err(Error::field("terms", format!("the installments' portions add up to {}%, not 100%", total.normalize())));
        }
        Ok(())
    }
}

/// The party field and entity of a trade document.
fn party(doc_has: impl Fn(&str) -> bool) -> Option<(&'static str, &'static str)> {
    if doc_has("customer") {
        Some(("customer", "Customer"))
    } else if doc_has("supplier") {
        Some(("supplier", "Supplier"))
    } else {
        None
    }
}

pub struct ScheduleHook;

impl ScheduleHook {
    /// The template a new document starts with: its order's (for invoices made from one), else
    /// the party's. Disabled templates are skipped.
    async fn default_template(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Option<Uuid>> {
        let lines = rows(doc, "items");
        for (link, entity) in [("sales_order", "SalesOrder"), ("purchase_order", "PurchaseOrder")] {
            if let Some(order) = lines.iter().find_map(|r| id(r, link)) {
                let o = get(hc.kernel, hc.ctx, hc.tx, entity, order).await?;
                if let Some(t) = id(&o, "fin_payment_terms_template") {
                    return Ok(Some(t));
                }
            }
        }
        let Some((field, entity)) = party(|f| hc.entity.field(f).is_some()) else { return Ok(None) };
        let Some(pid) = id(doc, field) else { return Ok(None) };
        let p = get(hc.kernel, hc.ctx, hc.tx, entity, pid).await?;
        let Some(t) = id(&p, "fin_payment_terms_template") else { return Ok(None) };
        let template = get(hc.kernel, hc.ctx, hc.tx, "PaymentTermsTemplate", t).await?;
        Ok((!b(&template, "disabled")).then_some(t))
    }
}

#[async_trait]
impl DocHook for ScheduleHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) || hc.entity.field("fin_payment_schedule").is_none() {
            return Ok(());
        }
        if b(doc, "is_return") {
            set(doc, "fin_payment_schedule", Value::Array(vec![]));
            return Ok(());
        }
        if id(doc, "fin_payment_terms_template").is_none()
            && rows(doc, "fin_payment_schedule").is_empty()
            && is_new(hc, doc).await?
            && let Some(t) = Self::default_template(hc, doc).await?
        {
            set(doc, "fin_payment_terms_template", t.to_string());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let (_, precision) = currency(hc.kernel, hc.ctx, hc.tx, id(doc, "currency").unwrap_or(co.currency_id)).await?;
        let rate = Some(d(doc, "conversion_rate")).filter(|r| *r > Decimal::ZERO).unwrap_or(Decimal::ONE);
        let total = if d(doc, "rounded_total").is_zero() { d(doc, "grand_total") } else { d(doc, "rounded_total") };
        let on = date(doc, "posting_date")?;
        let existing = rows(doc, "fin_payment_schedule");
        let mut schedule: Vec<Doc> = match id(doc, "fin_payment_terms_template") {
            Some(t) => {
                let template = get(hc.kernel, hc.ctx, hc.tx, "PaymentTermsTemplate", t).await?;
                let terms = rows(&template, "terms");
                let portions: Vec<Decimal> = terms.iter().map(|t| d(t, "invoice_portion")).collect();
                let amounts = rules::split(total, &portions, precision);
                terms
                    .iter()
                    .zip(amounts)
                    .enumerate()
                    .map(|(i, (t, amount))| {
                        let rule = s(t, "due_date_based_on").unwrap_or("Day(s) after invoice date");
                        let due = rules::due_date(
                            rule,
                            on,
                            crate::common::int(t, "credit_days"),
                            crate::common::int(t, "credit_months"),
                            crate::common::int(t, "day_of_month").clamp(0, 31) as u32,
                        );
                        let mut r = Doc::new();
                        // Keep row ids stable across saves so unchanged installments are not rewritten.
                        if let Some(old) = existing.get(i).and_then(|o| o.get("id")) {
                            r.insert("id".into(), old.clone());
                        }
                        r.insert("description".into(), json!(s(t, "description").unwrap_or(rule)));
                        r.insert("due_date".into(), json!(due.to_string()));
                        r.insert("invoice_portion".into(), money(d(t, "invoice_portion")));
                        r.insert("payment_amount".into(), money(amount));
                        let disc = d(t, "discount_percent");
                        r.insert("discount_percent".into(), money(disc));
                        r.insert(
                            "discount_date".into(),
                            if disc > Decimal::ZERO { json!((on + Duration::days(crate::common::int(t, "discount_days"))).to_string()) } else { Value::Null },
                        );
                        r
                    })
                    .collect()
            }
            None => {
                let mut manual = existing;
                let all_portions = !manual.is_empty() && manual.iter().all(|r| d(r, "payment_amount").is_zero() && d(r, "invoice_portion") > Decimal::ZERO);
                if all_portions {
                    let portions: Vec<Decimal> = manual.iter().map(|r| d(r, "invoice_portion")).collect();
                    for (r, a) in manual.iter_mut().zip(rules::split(total, &portions, precision)) {
                        set(r, "payment_amount", money(a));
                    }
                } else if !total.is_zero() {
                    for r in manual.iter_mut() {
                        set(r, "invoice_portion", money((d(r, "payment_amount") / total * Decimal::ONE_HUNDRED).round_dp(6)));
                    }
                }
                manual
            }
        };
        for r in schedule.iter_mut() {
            let amount = d(r, "payment_amount");
            set(r, "base_payment_amount", money(round(amount * rate, co.precision)));
            set(r, "discount_amount", money(round(amount * d(r, "discount_percent") / Decimal::ONE_HUNDRED, precision)));
        }
        let last_due = schedule.iter().filter_map(|r| opt_date(r, "due_date")).max();
        set(doc, "fin_payment_schedule", crate::common::child_rows(schedule));
        if hc.entity.field("due_date").is_some()
            && let Some(due) = last_due
        {
            set(doc, "due_date", due.to_string());
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) || hc.entity.field("fin_payment_schedule").is_none() {
            return Ok(());
        }
        let schedule = rows(doc, "fin_payment_schedule");
        if schedule.is_empty() {
            return Ok(());
        }
        if let Some(t) = id(doc, "fin_payment_terms_template") {
            let template = get(hc.kernel, hc.ctx, hc.tx, "PaymentTermsTemplate", t).await?;
            if b(&template, "disabled") {
                return Err(Error::field("fin_payment_terms_template", "this payment terms template is disabled"));
            }
        }
        let on = date(doc, "posting_date")?;
        let mut errors = Vec::new();
        for (i, r) in schedule.iter().enumerate() {
            if d(r, "payment_amount") <= Decimal::ZERO {
                errors.push(FieldError::new("payment_amount", "must be greater than zero").in_row("fin_payment_schedule", i));
            }
            if opt_date(r, "due_date").is_some_and(|due| due < on) {
                errors.push(FieldError::new("due_date", "cannot be before the document date").in_row("fin_payment_schedule", i));
            }
        }
        let total = if d(doc, "rounded_total").is_zero() { d(doc, "grand_total") } else { d(doc, "rounded_total") };
        let scheduled: Decimal = schedule.iter().map(|r| d(r, "payment_amount")).sum();
        if scheduled != total {
            errors.push(FieldError::new(
                "fin_payment_schedule",
                format!("the payment schedule adds up to {}, the document total is {}", scheduled.normalize(), total.normalize()),
            ));
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

/// One installment of an invoice or order.
#[derive(Debug, Clone)]
pub struct Installment {
    pub description: String,
    pub due: NaiveDate,
    pub amount: Decimal,
}

/// Payment schedules of documents (by parent id), installments in due-date order. `table` is the
/// document's table (`e_sales_invoice`, ...).
pub async fn schedules(tx: &mut Tx, tenant: Uuid, table: &str, ids: &[Uuid]) -> Result<HashMap<Uuid, Vec<Installment>>> {
    let mut out: HashMap<Uuid, Vec<Installment>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "select parent_id, coalesce(description, ''), due_date, payment_amount from {table}__fin_payment_schedule \
         where tenant_id = $1 and parent_id = any($2) and due_date is not null and payment_amount is not null order by parent_id, due_date, idx"
    );
    let rows: Vec<(Uuid, String, NaiveDate, Decimal)> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(tenant).bind(ids).fetch_all(&mut **tx).await?;
    for (parent, description, due, amount) in rows {
        out.entry(parent).or_default().push(Installment { description, due, amount });
    }
    Ok(out)
}

/// The installments of a receivable/payable of `amount` with `outstanding` left: its schedule when
/// it adds up to the amount, else one installment on the due date; each with what is still open
/// after earlier installments took the payments.
pub fn open_installments(schedule: Option<&Vec<Installment>>, amount: Decimal, outstanding: Decimal, due: NaiveDate) -> Vec<(Installment, Decimal)> {
    let parts = match schedule {
        Some(s) if !s.is_empty() && s.iter().map(|i| i.amount).sum::<Decimal>() == amount => s.clone(),
        _ => vec![Installment { description: String::new(), due, amount }],
    };
    let amounts: Vec<Decimal> = parts.iter().map(|p| p.amount).collect();
    let open = rules::installments_open(&amounts, amount - outstanding);
    parts.into_iter().zip(open).collect()
}
