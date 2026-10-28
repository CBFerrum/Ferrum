//! Timesheets: derived spans, rates and amounts, per-user overlap checks, and the roll-ups into
//! tasks and projects when time is submitted or cancelled.

use crate::logic::{CLOSED_PROJECT, Span, add_hours, first_overlap, hours_between, percent, timesheet_status};
use crate::project::{label, refresh_project, refresh_task, try_get};
use crate::{is_manager, unknown_users};
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use ferrum_domain::util::{b, d, find, id, money, round, rows, s, set, sys};
use ferrum_kernel::value::parse_datetime;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, Filter, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

fn time(row: &Doc, field: &str) -> Option<DateTime<Utc>> {
    s(row, field).and_then(|v| parse_datetime(v).ok())
}

fn row_err(field: &str, message: impl Into<String>, row: usize) -> FieldError {
    FieldError::new(field, message).in_row("time_logs", row)
}

async fn cached(hc: &mut HookCtx<'_>, cache: &mut HashMap<Uuid, Option<Doc>>, entity: &str, record: Uuid) -> Result<Option<Doc>> {
    if let Some(v) = cache.get(&record) {
        return Ok(v.clone());
    }
    let v = try_get(hc.kernel, hc.ctx, hc.tx, entity, record).await?;
    cache.insert(record, v.clone());
    Ok(v)
}

pub struct TimesheetHook;

#[async_trait]
impl DocHook for TimesheetHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        derive(hc, doc).await
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        check(hc, doc).await
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        roll_up(hc, doc).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let billed: Vec<String> = rows(doc, "time_logs").iter().enumerate().filter(|(_, r)| b(r, "billed")).map(|(i, _)| (i + 1).to_string()).collect();
        if !billed.is_empty() {
            return Err(Error::invalid(format!("time log(s) {} are billed; cancel their sales invoice first", billed.join(", "))));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        roll_up(hc, doc).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "make_sales_invoice" {
            return Ok(None);
        }
        crate::billing::invoice_timesheet(hc, doc, params).await.map(Some)
    }
}

/// Amounts round to the company currency's precision.
async fn precision(hc: &mut HookCtx<'_>, company: Option<Uuid>) -> Result<u32> {
    let Some(c) = company else { return Ok(2) };
    let currency = try_get(hc.kernel, hc.ctx, hc.tx, "Company", c).await?.and_then(|co| id(&co, "default_currency"));
    match currency {
        Some(cur) => Ok(ferrum_domain::util::currency(hc.kernel, hc.ctx, hc.tx, cur).await?.1),
        None => Ok(2),
    }
}

/// `(billing, costing)` hourly rates of `user` for an activity: the user's activity cost where it
/// sets one, else the activity type's default.
async fn rates_for(hc: &mut HookCtx<'_>, cache: &mut HashMap<Uuid, (Decimal, Decimal)>, user: &str, activity: Uuid) -> Result<(Decimal, Decimal)> {
    if let Some(r) = cache.get(&activity) {
        return Ok(*r);
    }
    let Some(at) = try_get(hc.kernel, hc.ctx, hc.tx, "ActivityType", activity).await? else { return Ok((Decimal::ZERO, Decimal::ZERO)) };
    let cost = if user.is_empty() {
        vec![]
    } else {
        find(hc.kernel, hc.ctx, hc.tx, "ActivityCost", vec![Filter::eq("user", user), Filter::eq("activity_type", activity.to_string())], None, 1).await?
    };
    let pick = |f: &str| cost.first().map(|c| d(c, f)).filter(|v| *v > Decimal::ZERO).unwrap_or_else(|| d(&at, f));
    let r = (pick("billing_rate"), pick("costing_rate"));
    cache.insert(activity, r);
    Ok(r)
}

/// Fills each log's project (from its task, else the timesheet's), span, rates and amounts, then
/// the totals. Only time on Time and Materials projects is billable.
async fn derive(hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
    let precision = precision(hc, id(doc, "company")).await?;
    let user = s(doc, "user").unwrap_or_default().to_lowercase();
    let old_rows: HashMap<String, Doc> = match id(doc, "id") {
        Some(tid) => try_get(hc.kernel, hc.ctx, hc.tx, "Timesheet", tid)
            .await?
            .map(|o| rows(&o, "time_logs"))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| Some((s(&r, "id")?.to_string(), r)))
            .collect(),
        None => HashMap::new(),
    };
    let header_project = id(doc, "project");
    let mut projects = HashMap::new();
    let mut tasks = HashMap::new();
    let mut rates = HashMap::new();
    let mut logs = rows(doc, "time_logs");
    for r in logs.iter_mut() {
        if id(r, "project").is_none() {
            let from_task = match id(r, "task") {
                Some(t) => cached(hc, &mut tasks, "Task", t).await?.and_then(|t| id(&t, "project")),
                None => None,
            };
            if let Some(p) = from_task.or(header_project) {
                set(r, "project", p.to_string());
            }
        }
        // The hours follow the span; without an end time, the end follows the hours.
        let from = time(r, "from_time");
        let mut to = time(r, "to_time");
        if let (Some(f), None) = (from, to)
            && d(r, "hours") > Decimal::ZERO
        {
            to = add_hours(f, d(r, "hours"));
            if let Some(t) = to {
                set(r, "to_time", t.to_rfc3339_opts(SecondsFormat::Millis, true));
            }
        }
        let hours = match (from, to) {
            (Some(f), Some(t)) => hours_between(f, t),
            _ => d(r, "hours"),
        };
        set(r, "hours", money(hours));
        let (default_billing, costing_rate) = match id(r, "activity_type") {
            Some(a) => rates_for(hc, &mut rates, &user, a).await?,
            None => (Decimal::ZERO, Decimal::ZERO),
        };
        let time_and_materials = match id(r, "project") {
            Some(p) => cached(hc, &mut projects, "Project", p).await?.is_some_and(|p| s(&p, "proj_billing_method") == Some("Time and Materials")),
            None => false,
        };
        let billable = b(r, "is_billable") && time_and_materials;
        set(r, "is_billable", billable);
        if billable {
            // Billing hours follow the hours, and the rate follows the activity, until the user
            // changes them (the client-side rule of other ERPs, enforced on the server).
            let old = s(r, "id").and_then(|i| old_rows.get(i));
            let unset = |r: &Doc, f: &str| r.get(f).is_none_or(Value::is_null);
            let hours_followed = old.is_some_and(|o| r.get("billing_hours") == o.get("billing_hours") && d(o, "billing_hours") == d(o, "hours"));
            if unset(r, "billing_hours") || hours_followed {
                set(r, "billing_hours", money(hours));
            }
            let activity_changed = old.is_some_and(|o| id(o, "activity_type") != id(r, "activity_type") && r.get("billing_rate") == o.get("billing_rate"));
            if unset(r, "billing_rate") || activity_changed {
                set(r, "billing_rate", money(default_billing));
            }
        } else {
            set(r, "billing_hours", Value::Null);
            set(r, "billing_rate", Value::Null);
        }
        set(r, "billing_amount", money(round(d(r, "billing_hours") * d(r, "billing_rate"), precision)));
        set(r, "costing_rate", money(costing_rate));
        set(r, "costing_amount", money(round(hours * costing_rate, precision)));
    }
    // hours, billable hours, billed hours, billable amount, billed amount, cost
    let mut totals = [Decimal::ZERO; 6];
    for r in &logs {
        totals[0] += d(r, "hours");
        if b(r, "is_billable") {
            totals[1] += d(r, "billing_hours");
            totals[3] += d(r, "billing_amount");
        }
        if b(r, "billed") {
            totals[2] += d(r, "billing_hours");
            totals[4] += d(r, "billing_amount");
        }
        totals[5] += d(r, "costing_amount");
    }
    let names = ["total_hours", "total_billable_hours", "total_billed_hours", "total_billable_amount", "total_billed_amount", "total_costing_amount"];
    for (f, v) in names.into_iter().zip(totals) {
        set(doc, f, money(v));
    }
    set(doc, "per_billed", money(percent(totals[4], totals[3])));
    let day = |t: Option<DateTime<Utc>>| t.map(|t| json!(t.date_naive().to_string())).unwrap_or(Value::Null);
    set(doc, "start_date", day(logs.iter().filter_map(|r| time(r, "from_time")).min()));
    set(doc, "end_date", day(logs.iter().filter_map(|r| time(r, "to_time").or_else(|| time(r, "from_time"))).max()));
    if d(doc, "docstatus").is_zero() {
        set(doc, "status", "Draft");
    }
    set(doc, "time_logs", Value::Array(logs.into_iter().map(Value::Object).collect()));
    Ok(())
}

async fn check(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let tid = id(doc, "id").ok_or_else(|| Error::internal("timesheet without id"))?;
    let tenant = hc.ctx.tenant;
    let user = s(doc, "user").unwrap_or_default().to_lowercase();
    if !is_manager(hc.ctx) && user != hc.ctx.email.to_lowercase() {
        return Err(Error::Forbidden("you can only record your own time".into()));
    }
    if !unknown_users(hc.tx, tenant, std::slice::from_ref(&user)).await?.is_empty() {
        return Err(Error::field("user", "no user has this email"));
    }
    let company = id(doc, "company");
    let mut errors = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut at: Vec<usize> = Vec::new();
    let mut projects = HashMap::new();
    let mut tasks = HashMap::new();
    for (i, r) in rows(doc, "time_logs").iter().enumerate() {
        let (Some(from), Some(to)) = (time(r, "from_time"), time(r, "to_time")) else {
            errors.push(row_err("to_time", "set the end time or the hours", i));
            continue;
        };
        if to <= from {
            errors.push(row_err("to_time", "must be after the start time", i));
            continue;
        }
        let project = id(r, "project");
        if let Some(t) = id(r, "task") {
            match cached(hc, &mut tasks, "Task", t).await? {
                None => errors.push(row_err("task", "does not exist", i)),
                Some(task) if id(&task, "project") != project => errors.push(row_err("task", format!("{} belongs to another project", label(&task)), i)),
                Some(task) if s(&task, "status") == Some("Cancelled") => errors.push(row_err("task", format!("{} is cancelled", label(&task)), i)),
                _ => {}
            }
        }
        if let Some(p) = project {
            match cached(hc, &mut projects, "Project", p).await? {
                None => errors.push(row_err("project", "does not exist", i)),
                Some(pd) => {
                    if let Some(st) = s(&pd, "status").filter(|st| CLOSED_PROJECT.contains(st)) {
                        errors.push(row_err("project", format!("project {} is {}; no more time can be booked on it", label(&pd), st.to_lowercase()), i));
                    } else if id(&pd, "company") != company {
                        errors.push(row_err("project", "belongs to another company", i));
                    }
                }
            }
        }
        spans.push((from, to));
        at.push(i);
    }
    if errors.is_empty()
        && let Some((a, z)) = first_overlap(&spans)
    {
        errors.push(row_err("from_time", format!("overlaps time log {}", at[a] + 1), at[z]));
    }
    if !errors.is_empty() {
        return Err(Error::Validation(errors));
    }
    // Against the user's submitted time. The lock makes one user's saves take turns, so two
    // timesheets submitted at the same moment cannot both pass this check.
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("ferrum:timesheet:{tenant}:{user}")).execute(&mut **hc.tx).await?;
    let (froms, tos): (Vec<DateTime<Utc>>, Vec<DateTime<Utc>>) = spans.iter().copied().unzip();
    let clashes: Vec<(i64, Option<String>, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
        "select r.i, t.number, l.from_time, l.to_time \
         from unnest($3::timestamptz[], $4::timestamptz[]) with ordinality as r(f, e, i) \
         join e_timesheet t on t.tenant_id = $1 and t.\"user\" = $2 and t.docstatus = 1 and t.id <> $5 \
         join e_timesheet__time_logs l on l.tenant_id = t.tenant_id and l.parent_id = t.id and l.from_time < r.e and r.f < l.to_time \
         order by r.i, l.from_time limit 20",
    )
    .bind(tenant)
    .bind(&user)
    .bind(&froms)
    .bind(&tos)
    .bind(tid)
    .fetch_all(&mut **hc.tx)
    .await?;
    for (i, number, f, t) in clashes {
        let message =
            format!("overlaps time already submitted on {} ({} – {})", number.unwrap_or_default(), f.format("%Y-%m-%d %H:%M"), t.format("%Y-%m-%d %H:%M"));
        errors.push(row_err("from_time", message, at[(i - 1) as usize]));
    }
    if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
}

/// Submitting or cancelling time re-derives every task and project it touches.
async fn roll_up(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let tid = id(doc, "id").ok_or_else(|| Error::internal("timesheet without id"))?;
    refresh_timesheet(hc.kernel, hc.ctx, hc.tx, tid).await?;
    let logs = rows(doc, "time_logs");
    let mut tasks: Vec<Uuid> = logs.iter().filter_map(|r| id(r, "task")).collect();
    let mut projects: Vec<Uuid> = logs.iter().filter_map(|r| id(r, "project")).collect();
    tasks.sort();
    tasks.dedup();
    projects.sort();
    projects.dedup();
    for t in tasks {
        refresh_task(hc.kernel, hc.ctx, hc.tx, t).await?;
    }
    for p in projects {
        refresh_project(hc.kernel, hc.ctx, hc.tx, p, None).await?;
    }
    Ok(())
}

/// Billing totals and status of a timesheet, from its logs.
pub(crate) async fn refresh_timesheet(k: &Kernel, ctx: &Ctx, tx: &mut Tx, timesheet: Uuid) -> Result<()> {
    let (docstatus, billed_hours, billable, billed): (i16, Decimal, Decimal, Decimal) = sqlx::query_as(
        "select t.docstatus, coalesce(sum(l.billing_hours) filter (where l.billed), 0), \
         coalesce(sum(l.billing_amount) filter (where l.is_billable), 0), coalesce(sum(l.billing_amount) filter (where l.billed), 0) \
         from e_timesheet t left join e_timesheet__time_logs l on l.tenant_id = t.tenant_id and l.parent_id = t.id \
         where t.tenant_id = $1 and t.id = $2 group by t.docstatus",
    )
    .bind(ctx.tenant)
    .bind(timesheet)
    .fetch_one(&mut **tx)
    .await?;
    let mut v = Doc::new();
    v.insert("total_billed_hours".into(), money(billed_hours));
    v.insert("total_billed_amount".into(), money(billed));
    v.insert("per_billed".into(), money(percent(billed, billable)));
    v.insert("status".into(), json!(timesheet_status(docstatus, billable, billed)));
    k.set_values_tx(&sys(ctx), tx, "Timesheet", timesheet, v).await
}

/// Rates are per user: the user must exist.
pub struct ActivityCostHook;

#[async_trait]
impl DocHook for ActivityCostHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let Some(u) = s(doc, "user")
            && !unknown_users(hc.tx, hc.ctx.tenant, &[u.to_string()]).await?.is_empty()
        {
            return Err(Error::field("user", "no user has this email"));
        }
        Ok(())
    }
}
