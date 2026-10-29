//! Subscriptions: customers billed once per period for their plans.
//!
//! Periods follow the billing anchor (the billing start when there is none): after the trial the
//! first period runs to the next anchor boundary, prorated by the day when the subscription
//! prorates, then whole periods follow, the last one cut at the end or cancellation date. Each
//! period's invoice is made `generate_invoice_days_before` days before the period starts and is
//! dated that day. The period is claimed in `fin_subscription_period` in the same transaction as
//! its invoice, so retries, catch-up after downtime and concurrent runs never bill it twice.
//!
//! Once invoicing has started, plans change through the Change Plans action: the periods already
//! invoiced from the effective date on get a supplementary invoice (more) or a credit note (less)
//! for their remaining days. Cancelling at the period end stops billing after the current period;
//! cancelling immediately also credits the unused days.

use crate::common::{claim_daily, installed, notify_user, opt_date};
use crate::rules::{Interval, Period, bracket};
use async_trait::async_trait;
use chrono::{Duration, NaiveDate};
use ferrum_domain::util::{b, d, elevated, get, id, money, rows, s, sys};
use ferrum_kernel::{Action, Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

const MAX_CATCH_UP: usize = 400;

/// A plan line of a subscription with its plan's terms.
#[derive(Debug, Clone)]
pub struct PlanLine {
    pub plan: Uuid,
    pub item: Uuid,
    pub name: String,
    pub price: Decimal,
    pub qty: Decimal,
    pub interval: Interval,
    pub count: u32,
    pub currency: Uuid,
}

/// A subscription as billing sees it.
#[derive(Debug, Clone)]
pub struct Sub {
    pub id: Uuid,
    pub number: String,
    pub company: Uuid,
    pub customer: Uuid,
    pub currency: Uuid,
    pub interval: Interval,
    pub count: u32,
    pub start: NaiveDate,
    pub trial_end: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub billing_start: NaiveDate,
    pub anchor: NaiveDate,
    pub cancellation: Option<NaiveDate>,
    pub at_period_end: bool,
    pub days_before: i64,
    pub days_until_due: i64,
    pub submit: bool,
    pub prorate: bool,
    pub tax_template: Option<Uuid>,
    pub cost_center: Option<Uuid>,
    pub created_by: Option<Uuid>,
    pub plans: Vec<PlanLine>,
}

impl Sub {
    /// The last day billed: the end or cancellation date, whichever comes first.
    pub fn stop(&self) -> Option<NaiveDate> {
        match (self.end_date, self.cancellation) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn period_from(&self, start: NaiveDate) -> Period {
        Period::from(self.anchor, self.interval, self.count, start)
    }
}

pub async fn plan_lines(k: &Kernel, ctx: &Ctx, tx: &mut Tx, lines: &[Doc]) -> Result<Vec<PlanLine>> {
    let mut out = Vec::new();
    for r in lines {
        let Some(pid) = id(r, "plan") else { continue };
        let p = get(k, ctx, tx, "SubscriptionPlan", pid).await?;
        out.push(PlanLine {
            plan: pid,
            item: id(&p, "item").ok_or_else(|| Error::invalid("the subscription plan has no item"))?,
            name: s(&p, "plan_name").unwrap_or_default().to_string(),
            price: d(&p, "price"),
            qty: d(r, "qty"),
            interval: s(&p, "billing_interval").and_then(Interval::parse).unwrap_or(Interval::Month),
            count: crate::common::int(&p, "interval_count").clamp(1, 1000) as u32,
            currency: id(&p, "currency").ok_or_else(|| Error::invalid("the subscription plan has no currency"))?,
        });
    }
    Ok(out)
}

pub async fn load(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<Sub> {
    let plans = plan_lines(k, ctx, tx, &rows(doc, "plans")).await?;
    let first = plans.first().ok_or_else(|| Error::field("plans", "add at least one plan"))?;
    let start = opt_date(doc, "start_date").ok_or_else(|| Error::field("start_date", "is required"))?;
    let trial_end = opt_date(doc, "trial_end");
    let billing_start = trial_end.map(|t| t + Duration::days(1)).unwrap_or(start).max(start);
    Ok(Sub {
        id: id(doc, "id").ok_or_else(|| Error::internal("no id"))?,
        number: s(doc, "number").unwrap_or_default().to_string(),
        company: id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?,
        customer: id(doc, "customer").ok_or_else(|| Error::field("customer", "is required"))?,
        currency: first.currency,
        interval: first.interval,
        count: first.count,
        start,
        trial_end,
        end_date: opt_date(doc, "end_date"),
        billing_start,
        anchor: opt_date(doc, "billing_anchor").unwrap_or(billing_start),
        cancellation: opt_date(doc, "cancellation_date"),
        at_period_end: b(doc, "cancel_at_period_end"),
        days_before: crate::common::int(doc, "generate_invoice_days_before").max(0),
        days_until_due: crate::common::int(doc, "days_until_due").max(0),
        submit: b(doc, "submit_invoices"),
        prorate: b(doc, "prorate"),
        tax_template: id(doc, "tax_template"),
        cost_center: id(doc, "cost_center"),
        created_by: id(doc, "created_by"),
        plans,
    })
}

/// Billed periods of a subscription: (first day, last day billed, invoice).
pub async fn logged(tx: &mut Tx, tenant: Uuid, sub: Uuid) -> Result<Vec<(NaiveDate, NaiveDate, Option<Uuid>)>> {
    Ok(sqlx::query_as(
        "select period_start, period_end, invoice_id from fin_subscription_period where tenant_id = $1 and subscription_id = $2 order by period_start",
    )
    .bind(tenant)
    .bind(sub)
    .fetch_all(&mut **tx)
    .await?)
}

/// The first day not billed yet.
pub async fn next_start(tx: &mut Tx, tenant: Uuid, s: &Sub) -> Result<NaiveDate> {
    let last: Option<NaiveDate> = sqlx::query_scalar("select max(period_end) from fin_subscription_period where tenant_id = $1 and subscription_id = $2")
        .bind(tenant)
        .bind(s.id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(last.map(|l| l + Duration::days(1)).unwrap_or(s.billing_start))
}

/// The billed part of the period starting at `start` (cut at the stop date) and its share of a
/// full period: 1 for whole periods and without proration.
pub fn billed(s: &Sub, p: &Period) -> (NaiveDate, Decimal) {
    let end = s.stop().map_or(p.end, |x| x.min(p.end));
    let days = p.days_from(p.start, s.stop());
    let factor = if s.prorate { Decimal::from(days) / Decimal::from(p.full_days()) } else { Decimal::ONE };
    (end, factor)
}

/// Unit rate of a plan for `factor` of a period (to 9 decimals, like any stored rate).
fn rate(price: Decimal, factor: Decimal) -> Decimal {
    if factor == Decimal::ONE { price } else { (price * factor).round_dp(9) }
}

/// One invoice line: (plan, quantity, unit rate, note).
type Bill = (PlanLine, Decimal, Decimal, String);

/// Makes a subscription invoice (or credit note, with `return_against`) dated `posting` for
/// `(from, to)`, submitted when the subscription submits its invoices. A submission that fails
/// leaves the draft for the user and tells the subscription's creator.
#[allow(clippy::too_many_arguments)]
async fn invoice(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    s: &Sub,
    lines: Vec<Bill>,
    posting: NaiveDate,
    (from, to): (NaiveDate, NaiveDate),
    return_against: Option<Uuid>,
    submit: bool,
) -> Result<Option<Uuid>> {
    let items: Vec<Value> = lines
        .into_iter()
        .filter(|(_, qty, rate, _)| !qty.is_zero() && *rate > Decimal::ZERO)
        .map(|(p, qty, rate, note)| {
            json!({"item": p.item.to_string(), "qty": money(qty), "price_list_rate": money(rate), "discount_percent": "0", "description": note,
                   "cost_center": s.cost_center.map(|c| c.to_string())})
        })
        .collect();
    if items.is_empty() {
        return Ok(None);
    }
    let mut input = Doc::new();
    input.insert("company".into(), json!(s.company.to_string()));
    input.insert("customer".into(), json!(s.customer.to_string()));
    input.insert("posting_date".into(), json!(posting.to_string()));
    input.insert("currency".into(), json!(s.currency.to_string()));
    input.insert("items".into(), Value::Array(items));
    if let Some(t) = s.tax_template {
        input.insert("tax_template".into(), json!(t.to_string()));
    }
    if let Some(c) = s.cost_center {
        input.insert("cost_center".into(), json!(c.to_string()));
    }
    match return_against {
        Some(orig) => {
            input.insert("is_return".into(), json!(true));
            input.insert("return_against".into(), json!(orig.to_string()));
        }
        None => {
            input.insert("due_date".into(), json!((posting + Duration::days(s.days_until_due)).to_string()));
        }
    }
    input.insert("fin_subscription".into(), json!(s.id.to_string()));
    input.insert("fin_subscription_period_start".into(), json!(from.to_string()));
    input.insert("fin_subscription_period_end".into(), json!(to.to_string()));
    let engine = elevated(ctx);
    let created = k.insert_tx(&engine, tx, "SalesInvoice", input).await?;
    let iid = id(&created, "id").ok_or_else(|| Error::internal("no id"))?;
    if submit {
        sqlx::query("savepoint fin_subscription_submit").execute(&mut **tx).await?;
        match k.submit_tx(&engine, tx, "SalesInvoice", iid, None).await {
            Ok(_) => {
                sqlx::query("release savepoint fin_subscription_submit").execute(&mut **tx).await?;
            }
            Err(e) => {
                sqlx::query("rollback to savepoint fin_subscription_submit").execute(&mut **tx).await?;
                let what = if return_against.is_some() { "credit note" } else { "invoice" };
                let body = format!("The {what} for {from} to {to} of subscription {} was left as a draft: {e}", s.number);
                notify_user(tx, ctx.tenant, None, s.created_by, &format!("Subscription {} {what} not submitted", s.number), &body, "SalesInvoice", Some(iid))
                    .await?;
            }
        }
    }
    Ok(Some(iid))
}

/// Bills every period whose invoice date has come by `as_of`, oldest first. Returns the invoices made.
pub async fn generate(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc, as_of: NaiveDate) -> Result<Vec<Uuid>> {
    let s = load(k, ctx, tx, doc).await?;
    let mut start = next_start(tx, ctx.tenant, &s).await?;
    let mut made = Vec::new();
    for _ in 0..MAX_CATCH_UP {
        if s.stop().is_some_and(|stop| start > stop) {
            break;
        }
        let p = s.period_from(start);
        let invoice_date = p.start - Duration::days(s.days_before);
        if invoice_date > as_of {
            break;
        }
        let (end, factor) = billed(&s, &p);
        // Claim the period first: a concurrent or repeated run finds it taken and bills nothing.
        let claimed: Option<NaiveDate> = sqlx::query_scalar(
            "insert into fin_subscription_period (tenant_id, subscription_id, period_start, period_end) values ($1, $2, $3, $4) \
             on conflict do nothing returning period_start",
        )
        .bind(ctx.tenant)
        .bind(s.id)
        .bind(p.start)
        .bind(end)
        .fetch_optional(&mut **tx)
        .await?;
        if claimed.is_none() {
            break;
        }
        let note = |pl: &PlanLine| {
            let mut n = format!("{}: {} to {end}", pl.name, p.start);
            if factor != Decimal::ONE {
                n.push_str(&format!(" (prorated {} of {} days)", p.days_from(p.start, s.stop()), p.full_days()));
            }
            n
        };
        let lines: Vec<Bill> = s.plans.iter().map(|pl| (pl.clone(), pl.qty, rate(pl.price, factor), note(pl))).collect();
        let inv = invoice(k, ctx, tx, &s, lines, invoice_date, (p.start, end), None, s.submit).await?;
        sqlx::query("update fin_subscription_period set invoice_id = $4 where tenant_id = $1 and subscription_id = $2 and period_start = $3")
            .bind(ctx.tenant)
            .bind(s.id)
            .bind(p.start)
            .bind(inv)
            .execute(&mut **tx)
            .await?;
        made.extend(inv);
        start = end + Duration::days(1);
    }
    Ok(made)
}

/// Bills or credits the periods already invoiced from `from` on for quantity changes per plan
/// (`deltas`: the plan with the change in quantity): a supplementary invoice for increases, a
/// credit note against the period's invoice for decreases, each for the days left in the period.
/// Without proration only periods starting on or after `from` are adjusted.
async fn adjust(k: &Kernel, ctx: &Ctx, tx: &mut Tx, s: &Sub, from: NaiveDate, deltas: &[(PlanLine, Decimal)]) -> Result<Vec<Uuid>> {
    let mut made = Vec::new();
    for (ps, pe, inv) in logged(tx, ctx.tenant, s.id).await? {
        if pe < from || (!s.prorate && ps < from) {
            continue;
        }
        let first = ps.max(from);
        let full = s.period_from(ps).full_days();
        let days = (pe - first).num_days() + 1;
        let factor = Decimal::from(days) / Decimal::from(full);
        let note = |pl: &PlanLine| {
            let mut n = format!("{}: {first} to {pe}", pl.name);
            if days != full {
                n.push_str(&format!(" (prorated {days} of {full} days)"));
            }
            n
        };
        // Dated when the change takes effect (never ahead of today, for periods billed in advance).
        let posting = from.max(ps).min(ctx.today().max(from));
        let ups: Vec<Bill> = deltas.iter().filter(|(_, q)| *q > Decimal::ZERO).map(|(pl, q)| (pl.clone(), *q, rate(pl.price, factor), note(pl))).collect();
        let downs: Vec<Bill> = deltas.iter().filter(|(_, q)| *q < Decimal::ZERO).map(|(pl, q)| (pl.clone(), *q, rate(pl.price, factor), note(pl))).collect();
        let submitted = match inv {
            Some(i) => {
                sqlx::query_scalar::<_, i16>("select docstatus from e_sales_invoice where tenant_id = $1 and id = $2")
                    .bind(ctx.tenant)
                    .bind(i)
                    .fetch_optional(&mut **tx)
                    .await?
                    == Some(1)
            }
            None => false,
        };
        if !ups.is_empty() {
            made.extend(invoice(k, ctx, tx, s, ups, posting, (first, pe), None, s.submit).await?);
        }
        if let Some(orig) = inv
            && !downs.is_empty()
        {
            made.extend(invoice(k, ctx, tx, s, downs, posting, (first, pe), Some(orig), s.submit && submitted).await?);
        }
    }
    Ok(made)
}

/// Status, current period and next invoice date as of `today`.
pub async fn derive(tx: &mut Tx, tenant: Uuid, s: &Sub, today: NaiveDate) -> Result<Doc> {
    let past_due: bool = sqlx::query_scalar(
        "select exists(select 1 from e_sales_invoice where tenant_id = $1 and (ext->>'fin_subscription') = $2 and ext ? 'fin_subscription' \
         and docstatus = 1 and not coalesce(is_return, false) and outstanding_amount > 0 and due_date < $3)",
    )
    .bind(tenant)
    .bind(s.id.to_string())
    .bind(today)
    .fetch_one(&mut **tx)
    .await?;
    let status = match (s.cancellation, s.end_date, s.trial_end) {
        (Some(c), _, _) if !s.at_period_end || today > c => "Cancelled",
        (_, Some(e), _) if today > e => "Completed",
        (_, _, Some(t)) if today <= t => "Trialling",
        _ if past_due => "Past Due",
        _ => "Active",
    };
    let current = if today < s.billing_start || s.stop().is_some_and(|x| today > x) {
        None
    } else {
        let (b, next) = bracket(s.anchor, s.interval, s.count, today);
        let end = next - Duration::days(1);
        Some((b.max(s.billing_start), s.stop().map_or(end, |x| x.min(end))))
    };
    let start = next_start(tx, tenant, s).await?;
    let next = if s.stop().is_some_and(|x| start > x) { None } else { Some(s.period_from(start).start - Duration::days(s.days_before)) };
    let mut v = Doc::new();
    v.insert("status".into(), json!(status));
    v.insert("current_period_start".into(), current.map(|c| json!(c.0.to_string())).unwrap_or(Value::Null));
    v.insert("current_period_end".into(), current.map(|c| json!(c.1.to_string())).unwrap_or(Value::Null));
    v.insert("next_invoice_date".into(), next.map(|n| json!(n.to_string())).unwrap_or(Value::Null));
    Ok(v)
}

async fn refresh(k: &Kernel, ctx: &Ctx, tx: &mut Tx, sid: Uuid) -> Result<Doc> {
    let doc = k.get_tx(&sys(ctx), tx, "Subscription", sid, true).await?;
    let s = load(k, ctx, tx, &doc).await?;
    let v = derive(tx, ctx.tenant, &s, ctx.today()).await?;
    k.set_values_tx(&sys(ctx), tx, "Subscription", sid, v).await?;
    k.get_tx(&sys(ctx), tx, "Subscription", sid, false).await
}

pub struct PlanHook;

#[async_trait]
impl DocHook for PlanHook {
    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        let Some(old) = old else { return Ok(()) };
        let changed = ["billing_interval", "interval_count", "currency"].iter().any(|f| doc.get(*f) != old.get(*f));
        if changed {
            let used: bool = sqlx::query_scalar(
                "select exists(select 1 from e_subscription__plans p join e_subscription s on s.tenant_id = p.tenant_id and s.id = p.parent_id \
                 where p.tenant_id = $1 and p.plan = $2 and s.status not in ('Cancelled', 'Completed'))",
            )
            .bind(hc.ctx.tenant)
            .bind(id(doc, "id"))
            .fetch_one(&mut **hc.tx)
            .await?;
            if used {
                return Err(Error::invalid("subscriptions bill this plan: its interval and currency cannot change (create a new plan instead)"));
            }
        }
        Ok(())
    }
}

pub struct SubscriptionHook;

#[async_trait]
impl DocHook for SubscriptionHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let plans = plan_lines(hc.kernel, hc.ctx, hc.tx, &rows(doc, "plans")).await?;
        if let Some(first) = plans.first() {
            doc.insert("currency".into(), json!(first.currency.to_string()));
        }
        if plans.is_empty() || opt_date(doc, "start_date").is_none() || id(doc, "id").is_none() {
            return Ok(());
        }
        let s = load(hc.kernel, hc.ctx, hc.tx, doc).await?;
        for (k, v) in derive(hc.tx, hc.ctx.tenant, &s, hc.ctx.today()).await? {
            doc.insert(k, v);
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let lines = rows(doc, "plans");
        let plans = plan_lines(hc.kernel, hc.ctx, hc.tx, &lines).await?;
        let mut errors = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (i, (p, r)) in plans.iter().zip(&lines).enumerate() {
            if !seen.insert(p.plan) {
                errors.push(FieldError::new("plan", "appears twice").in_row("plans", i));
            }
            let first = &plans[0];
            if (p.interval, p.count, p.currency) != (first.interval, first.count, first.currency) {
                errors.push(FieldError::new("plan", "all plans of a subscription bill on the same interval and in the same currency").in_row("plans", i));
            }
            let plan = get(hc.kernel, hc.ctx, hc.tx, "SubscriptionPlan", p.plan).await?;
            if b(&plan, "disabled") && d(r, "qty") > Decimal::ZERO {
                errors.push(FieldError::new("plan", "this plan is disabled").in_row("plans", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        let (Some(old), Some(sid)) = (old, id(doc, "id")) else { return Ok(()) };
        if logged(hc.tx, hc.ctx.tenant, sid).await?.is_empty() {
            return Ok(());
        }
        for f in ["start_date", "trial_end", "billing_anchor", "customer", "company"] {
            if doc.get(f) != old.get(f) {
                return Err(Error::field(f, "cannot change once the subscription has been invoiced"));
            }
        }
        let plans = |x: &Doc| {
            let mut v: Vec<(String, String)> =
                rows(x, "plans").iter().map(|r| (s(r, "plan").unwrap_or_default().to_string(), d(r, "qty").normalize().to_string())).collect();
            v.sort();
            v
        };
        if !hc.ctx.is_system && plans(doc) != plans(old) {
            return Err(Error::field("plans", "plans of an invoiced subscription change through the Change Plans action, which prorates the period"));
        }
        Ok(())
    }

    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !logged(hc.tx, hc.ctx.tenant, id(doc, "id").unwrap_or_default()).await?.is_empty() {
            return Err(Error::invalid("this subscription has been invoiced; cancel it instead of deleting it"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if !matches!(name, "generate_invoices" | "change_plans" | "cancel_subscription") || !installed(hc.ctx) {
            return Ok(None);
        }
        hc.ctx.require("SalesInvoice", Action::Create)?;
        let sid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let today = hc.ctx.today();
        let date_param = |k: &str| -> Result<Option<NaiveDate>> {
            match params.get(k).and_then(Value::as_str) {
                Some(v) => ferrum_kernel::value::parse_date(v).map(Some).map_err(|m| Error::field(k, m)),
                None => Ok(None),
            }
        };
        let made = match name {
            "generate_invoices" => {
                let as_of = date_param("as_of")?.unwrap_or(today);
                if as_of > today {
                    return Err(Error::field("as_of", "invoices are not made ahead of their date"));
                }
                generate(hc.kernel, hc.ctx, hc.tx, doc, as_of).await?
            }
            "change_plans" => {
                let wanted = params.get("plans").and_then(Value::as_array).ok_or_else(|| Error::field("plans", "give the new plans and quantities"))?;
                let from = date_param("effective_date")?.unwrap_or(today);
                if from > today {
                    return Err(Error::field("effective_date", "plan changes take effect today or on an earlier date"));
                }
                let before = load(hc.kernel, hc.ctx, hc.tx, doc).await?;
                if from < before.start {
                    return Err(Error::field("effective_date", "is before the subscription starts"));
                }
                let mut patch = Doc::new();
                patch.insert("plans".into(), Value::Array(wanted.clone()));
                let updated = hc.kernel.update_tx(&elevated(hc.ctx), hc.tx, "Subscription", sid, patch, None).await?;
                let after = load(hc.kernel, hc.ctx, hc.tx, &updated).await?;
                let mut deltas: BTreeMap<Uuid, (PlanLine, Decimal)> = BTreeMap::new();
                for p in &before.plans {
                    deltas.insert(p.plan, (p.clone(), -p.qty));
                }
                for p in &after.plans {
                    let e = deltas.entry(p.plan).or_insert((p.clone(), Decimal::ZERO));
                    e.1 += p.qty;
                    e.0 = p.clone();
                }
                let deltas: Vec<(PlanLine, Decimal)> = deltas.into_values().filter(|(_, q)| !q.is_zero()).collect();
                adjust(hc.kernel, hc.ctx, hc.tx, &after, from, &deltas).await?
            }
            _ => {
                let s = load(hc.kernel, hc.ctx, hc.tx, doc).await?;
                if s.cancellation.is_some() {
                    return Err(Error::invalid("this subscription is already cancelled"));
                }
                let at_period_end = params.get("at_period_end").and_then(Value::as_bool).unwrap_or(false);
                let on = if at_period_end {
                    if today < s.billing_start {
                        s.billing_start - Duration::days(1)
                    } else {
                        let (_, next) = bracket(s.anchor, s.interval, s.count, today);
                        next - Duration::days(1)
                    }
                } else {
                    date_param("date")?.unwrap_or(today)
                };
                if on > today && !at_period_end {
                    return Err(Error::field("date", "an immediate cancellation cannot be in the future (cancel at the period end instead)"));
                }
                let on = on.max(s.start - Duration::days(1));
                let mut v = Doc::new();
                v.insert("cancellation_date".into(), json!(on.to_string()));
                v.insert("cancel_at_period_end".into(), json!(at_period_end));
                hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "Subscription", sid, v).await?;
                let deltas: Vec<(PlanLine, Decimal)> = s.plans.iter().map(|p| (p.clone(), -p.qty)).collect();
                adjust(hc.kernel, hc.ctx, hc.tx, &s, on + Duration::days(1), &deltas).await?
            }
        };
        let refreshed = refresh(hc.kernel, hc.ctx, hc.tx, sid).await?;
        Ok(Some(json!({"invoices": made, "status": refreshed.get("status"), "next_invoice_date": refreshed.get("next_invoice_date")})))
    }
}

/// Bills what is due, then (once a day) refreshes statuses: trial ends, completion, cancellation
/// at the period end, invoices past due.
pub async fn upkeep(k: &Kernel, ctx: &Ctx) -> Result<()> {
    let today = ctx.today();
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let due: Vec<Uuid> =
        sqlx::query_scalar("select id from e_subscription where tenant_id = $1 and next_invoice_date <= $2 order by next_invoice_date, id limit 200")
            .bind(ctx.tenant)
            .bind(today)
            .fetch_all(&mut *tx)
            .await?;
    let daily = claim_daily(&mut tx, ctx.tenant, "subscriptions", today).await?;
    tx.commit().await?;
    for sid in due {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let run = async {
            let doc = k.get_tx(&sys(ctx), &mut tx, "Subscription", sid, true).await?;
            generate(k, ctx, &mut tx, &doc, today).await?;
            refresh(k, ctx, &mut tx, sid).await
        }
        .await;
        match run {
            Ok(_) => tx.commit().await?,
            Err(e) => {
                tx.rollback().await?;
                tracing::warn!(tenant = %ctx.tenant, subscription = %sid, "subscription billing failed: {e}");
            }
        }
    }
    if daily {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let live: Vec<Uuid> = sqlx::query_scalar("select id from e_subscription where tenant_id = $1 and status not in ('Cancelled', 'Completed')")
            .bind(ctx.tenant)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        for sid in live {
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            match refresh(k, ctx, &mut tx, sid).await {
                Ok(_) => tx.commit().await?,
                Err(e) => {
                    tx.rollback().await?;
                    tracing::warn!(tenant = %ctx.tenant, subscription = %sid, "subscription status refresh failed: {e}");
                }
            }
        }
    }
    Ok(())
}
