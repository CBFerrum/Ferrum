//! Customer billing: time-and-materials invoices from timesheets, milestone invoices for fixed
//! price projects, and the Sales Invoice hook that marks what an invoice bills. Marking happens when
//! the invoice is submitted (and is undone when it is cancelled), never when the draft is made: a
//! deleted draft leaves everything unbilled, and two drafts can never both bill the same time.

use crate::lock;
use crate::logic::{BillableLog, group_time};
use crate::project::{invoice_number, label, refresh_project, try_get};
use crate::timesheet::refresh_timesheet;
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use ferrum_domain::flow::created;
use ferrum_domain::util::{Company, b, company, d, elevated, get, id, money, round, rows, s};
use ferrum_kernel::{Action, Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

/// The invoice header for a project: its company, customer and cost center, in the company
/// currency the rates and the contract value are kept in.
async fn invoice_header(hc: &mut HookCtx<'_>, project: &Doc) -> Result<(Doc, Company)> {
    let cid = id(project, "company").ok_or_else(|| Error::invalid("the project has no company"))?;
    let customer = id(project, "customer").ok_or_else(|| Error::invalid(format!("set the customer of project {} before invoicing it", label(project))))?;
    let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
    let mut h = Doc::new();
    h.insert("company".into(), json!(cid.to_string()));
    h.insert("customer".into(), json!(customer.to_string()));
    h.insert("project".into(), project.get("id").cloned().unwrap_or(Value::Null));
    h.insert("currency".into(), json!(co.currency_id.to_string()));
    if let Some(cc) = id(project, "proj_cost_center") {
        h.insert("cost_center".into(), json!(cc.to_string()));
    }
    Ok((h, co))
}

/// The user must be allowed to create invoices; the draft is inserted with engine rights because
/// the line prices and billing links are fields only the engine sets.
async fn create_invoice(hc: &mut HookCtx<'_>, input: Doc) -> Result<Doc> {
    hc.ctx.require("SalesInvoice", Action::Create)?;
    hc.kernel.insert_tx(&elevated(hc.ctx), hc.tx, "SalesInvoice", input).await
}

fn param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

fn span_text(from: DateTime<Utc>, to: DateTime<Utc>) -> String {
    let (a, z) = (from.date_naive(), to.date_naive());
    if a == z { a.to_string() } else { format!("{a} to {z}") }
}

fn to_date(params: &Value) -> Result<Option<NaiveDate>> {
    param(params, "to_date").map(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").map_err(|_| Error::field("to_date", "use the format YYYY-MM-DD"))).transpose()
}

/// `make_sales_invoice` on a Time and Materials project: its billable, unbilled time, optionally
/// only time ending on or before `to_date`.
pub(crate) async fn invoice_project(hc: &mut HookCtx<'_>, project: &Doc, params: &Value) -> Result<Value> {
    let to_date = to_date(params)?;
    invoice_time(hc, project, None, to_date).await
}

/// `make_sales_invoice` on a submitted timesheet: the same for this timesheet's time on one project
/// (`project` chooses it when the timesheet has unbilled time on several).
pub(crate) async fn invoice_timesheet(hc: &mut HookCtx<'_>, ts: &Doc, params: &Value) -> Result<Value> {
    let tid = id(ts, "id").ok_or_else(|| Error::internal("timesheet without id"))?;
    let to_date = to_date(params)?;
    let mut candidates: Vec<Uuid> = rows(ts, "time_logs").iter().filter(|r| b(r, "is_billable") && !b(r, "billed")).filter_map(|r| id(r, "project")).collect();
    candidates.sort();
    candidates.dedup();
    let project_id: Uuid = match param(params, "project") {
        Some(p) => p.parse().map_err(|_| Error::field("project", "is not a valid id"))?,
        None => match candidates.as_slice() {
            [one] => *one,
            [] => return Err(Error::invalid("nothing to invoice: this timesheet has no unbilled billable time")),
            _ => return Err(Error::invalid("this timesheet has unbilled time on several projects; choose one with the project parameter")),
        },
    };
    let project = try_get(hc.kernel, hc.ctx, hc.tx, "Project", project_id).await?.ok_or_else(|| Error::field("project", "does not exist"))?;
    invoice_time(hc, &project, Some(tid), to_date).await
}

/// One draft invoice for a project's billable, unbilled, submitted time (of one timesheet, or all),
/// with a line per activity type and rate.
async fn invoice_time(hc: &mut HookCtx<'_>, project: &Doc, timesheet: Option<Uuid>, to_date: Option<NaiveDate>) -> Result<Value> {
    hc.ctx.require("SalesInvoice", Action::Create)?;
    let project_id = id(project, "id").ok_or_else(|| Error::internal("project without id"))?;
    if s(project, "proj_billing_method") != Some("Time and Materials") {
        return Err(Error::invalid(format!("project {} is not billed on time and materials", label(project))));
    }
    let (mut input, _) = invoice_header(hc, project).await?;
    let pending = pending_logs(hc, project_id).await?;
    let found: Vec<(Uuid, Option<Uuid>, Option<Decimal>, Option<Decimal>, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "select l.id, l.activity_type, l.billing_hours, l.billing_rate, l.from_time, l.to_time \
         from e_timesheet__time_logs l join e_timesheet t on t.tenant_id = l.tenant_id and t.id = l.parent_id \
         where l.tenant_id = $1 and l.project = $2 and t.docstatus = 1 and l.is_billable and not l.billed and l.billing_amount > 0 \
         and ($3::uuid is null or t.id = $3) and ($4::date is null or l.to_time < ($4::date + 1)::timestamptz) and l.id <> all($5) \
         order by l.from_time, l.id",
    )
    .bind(hc.ctx.tenant)
    .bind(project_id)
    .bind(timesheet)
    .bind(to_date)
    .bind(&pending)
    .fetch_all(&mut **hc.tx)
    .await?;
    let logs: Vec<BillableLog> = found
        .into_iter()
        .filter_map(|(id, a, h, r, f, t)| {
            Some(BillableLog { id, activity_type: a?, hours: h.unwrap_or_default(), rate: r.unwrap_or_default(), from: f?, to: t? })
        })
        .collect();
    if logs.is_empty() {
        let upto = to_date.map(|d| format!(" up to {d}")).unwrap_or_default();
        let hint = if pending.is_empty() { String::new() } else { format!("; {} log(s) are on a draft invoice already (submit or delete it)", pending.len()) };
        return Err(Error::invalid(format!("nothing to invoice: no submitted, billable and unbilled time{upto} on project {}{hint}", label(project))));
    }
    let mut items = Vec::new();
    for line in group_time(&logs) {
        let at = get(hc.kernel, hc.ctx, hc.tx, "ActivityType", line.activity_type).await?;
        let activity = s(&at, "activity_type").unwrap_or_default().to_string();
        let item = id(&at, "item").ok_or_else(|| Error::invalid(format!("set the service item of activity type {activity} to invoice its time")))?;
        items.push(json!({
            "item": item.to_string(),
            "qty": money(line.hours),
            "price_list_rate": money(line.rate),
            "discount_percent": "0",
            "description": format!("{activity}: {} h, {}", line.hours.normalize(), span_text(line.from, line.to)),
            "proj_time_logs": line.logs.iter().map(Uuid::to_string).collect::<Vec<_>>(),
        }));
    }
    input.insert("items".into(), Value::Array(items));
    input.insert("remarks".into(), json!(format!("Time on project {}{}", label(project), to_date.map(|d| format!(" up to {d}")).unwrap_or_default())));
    let inv = create_invoice(hc, input).await?;
    let mut out = created("SalesInvoice", &inv);
    out["logs"] = json!(logs.len());
    Ok(out)
}

/// Time logs already on a draft invoice of the project: they are not offered again.
async fn pending_logs(hc: &mut HookCtx<'_>, project: Uuid) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "select distinct x.log::uuid from e_sales_invoice s \
         join e_sales_invoice__items i on i.tenant_id = s.tenant_id and i.parent_id = s.id \
         cross join lateral jsonb_array_elements_text(case when jsonb_typeof(i.ext->'proj_time_logs') = 'array' then i.ext->'proj_time_logs' else '[]'::jsonb end) x(log) \
         where s.tenant_id = $1 and s.project = $2 and s.docstatus = 0 \
         and x.log ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'",
    )
    .bind(hc.ctx.tenant)
    .bind(project)
    .fetch_all(&mut **hc.tx)
    .await?)
}

/// `make_milestone_invoice` on a milestone of a fixed price project: a draft invoice for the
/// milestone's share of the contract value. Each milestone is invoiced once.
pub(crate) async fn invoice_milestone(hc: &mut HookCtx<'_>, task: &Doc) -> Result<Value> {
    hc.ctx.require("SalesInvoice", Action::Create)?;
    let tid = id(task, "id").ok_or_else(|| Error::internal("task without id"))?;
    if !b(task, "is_milestone") {
        return Err(Error::invalid("only milestones are invoiced"));
    }
    if let Some(inv) = id(task, "sales_invoice") {
        return Err(Error::invalid(format!("this milestone is already invoiced on {}", invoice_number(hc, inv).await?)));
    }
    if s(task, "status") == Some("Cancelled") {
        return Err(Error::invalid("this milestone is cancelled"));
    }
    let pct = d(task, "billing_percent");
    if pct <= Decimal::ZERO {
        return Err(Error::invalid("set the milestone's billing percentage first"));
    }
    let draft: Option<Uuid> = sqlx::query_scalar(
        "select s.id from e_sales_invoice s join e_sales_invoice__items i on i.tenant_id = s.tenant_id and i.parent_id = s.id \
         where s.tenant_id = $1 and s.docstatus = 0 and i.ext->>'proj_milestone' = $2 limit 1",
    )
    .bind(hc.ctx.tenant)
    .bind(tid.to_string())
    .fetch_optional(&mut **hc.tx)
    .await?;
    if let Some(draft) = draft {
        return Err(Error::invalid(format!("draft sales invoice {draft} already bills this milestone; submit or delete it")));
    }
    let project = get(hc.kernel, hc.ctx, hc.tx, "Project", id(task, "project").ok_or_else(|| Error::invalid("the milestone has no project"))?).await?;
    if s(&project, "proj_billing_method") != Some("Fixed Price") {
        return Err(Error::invalid(format!("project {} is not a fixed price project", label(&project))));
    }
    let value = d(&project, "proj_contract_value");
    if value <= Decimal::ZERO {
        return Err(Error::invalid(format!("set the contract value of project {}", label(&project))));
    }
    let item = id(&project, "proj_billing_item").ok_or_else(|| Error::invalid(format!("set the milestone billing item of project {}", label(&project))))?;
    let (mut input, co) = invoice_header(hc, &project).await?;
    let amount = round(value * pct / Decimal::ONE_HUNDRED, co.precision);
    input.insert(
        "items".into(),
        json!([{
            "item": item.to_string(),
            "qty": "1",
            "price_list_rate": money(amount),
            "discount_percent": "0",
            "description": format!("Milestone {}: {}% of the contract value {}", label(task), pct.normalize(), value.normalize()),
            "proj_milestone": tid.to_string(),
        }]),
    );
    input.insert("remarks".into(), json!(format!("Milestone {} of project {}", label(task), label(&project))));
    let inv = create_invoice(hc, input).await?;
    Ok(created("SalesInvoice", &inv))
}

/// Marks what a Sales Invoice bills when it is submitted and releases it when it is cancelled.
pub struct InvoiceHook;

fn time_logs(line: &Doc) -> Vec<Uuid> {
    line.get("proj_time_logs").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str()?.parse().ok()).collect()).unwrap_or_default()
}

#[async_trait]
impl DocHook for InvoiceHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut seen = HashSet::new();
        let mut errors = Vec::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            let bad = match r.get("proj_time_logs") {
                None | Some(Value::Null) => None,
                Some(Value::Array(a)) => a.iter().find_map(|v| match v.as_str().and_then(|x| x.parse::<Uuid>().ok()) {
                    None => Some("must list time log ids"),
                    Some(u) if !seen.insert(u) => Some("bills a time log twice"),
                    _ => None,
                }),
                Some(_) => Some("must list time log ids"),
            };
            if let Some(m) = bad {
                errors.push(FieldError::new("proj_time_logs", m).in_row("items", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.ctx.schema.entity("Timesheet").is_none() {
            return Ok(());
        }
        mark_billed(hc, doc).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.ctx.schema.entity("Timesheet").is_none() {
            return Ok(());
        }
        unmark_billed(hc, doc).await
    }
}

async fn mark_billed(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
    let lines = rows(doc, "items");
    let logs: Vec<Uuid> = lines.iter().flat_map(time_logs).collect();
    let mut milestones: BTreeMap<Uuid, Decimal> = BTreeMap::new();
    for r in &lines {
        if let Some(t) = id(r, "proj_milestone") {
            *milestones.entry(t).or_default() += d(r, "base_net_amount");
        }
    }
    if logs.is_empty() && milestones.is_empty() {
        return Ok(());
    }
    if b(doc, "is_return") {
        return Err(Error::invalid("a credit note cannot bill project time or milestones"));
    }
    let project = id(doc, "project").ok_or_else(|| Error::field("project", "is required on an invoice that bills project time or milestones"))?;
    let tenant = hc.ctx.tenant;
    let mut problems: Vec<String> = Vec::new();
    let mut timesheets: Vec<Uuid> = Vec::new();
    if !logs.is_empty() {
        // The timesheets first, as their cancellation locks them, then the logs themselves: a
        // timesheet cannot be cancelled while its time is being billed, nor billed once cancelled.
        timesheets = sqlx::query_scalar(
            "select t.id from e_timesheet t where t.tenant_id = $1 \
             and t.id in (select parent_id from e_timesheet__time_logs where tenant_id = $1 and id = any($2)) order by t.id for update",
        )
        .bind(tenant)
        .bind(&logs)
        .fetch_all(&mut **hc.tx)
        .await?;
        let found: Vec<(Uuid, Option<Uuid>, bool, bool, Option<String>, i16, Option<String>)> = sqlx::query_as(
            "select l.id, l.project, l.is_billable, l.billed, si.number, t.docstatus, t.number from e_timesheet__time_logs l \
             join e_timesheet t on t.tenant_id = l.tenant_id and t.id = l.parent_id \
             left join e_sales_invoice si on si.tenant_id = l.tenant_id and si.id = l.sales_invoice \
             where l.tenant_id = $1 and l.id = any($2) for update of l",
        )
        .bind(tenant)
        .bind(&logs)
        .fetch_all(&mut **hc.tx)
        .await?;
        for log in &logs {
            let problem = match found.iter().find(|f| f.0 == *log) {
                None => Some(format!("time log {log} no longer exists")),
                Some((_, p, billable, billed, by, docstatus, ts)) => {
                    let ts = ts.clone().unwrap_or_default();
                    if *docstatus != 1 {
                        Some(format!("timesheet {ts} is not submitted"))
                    } else if *billed {
                        Some(format!("time on {ts} is already billed on {}", by.clone().unwrap_or_default()))
                    } else if !billable {
                        Some(format!("time on {ts} is not billable"))
                    } else if *p != Some(project) {
                        Some(format!("time on {ts} belongs to another project"))
                    } else {
                        None
                    }
                }
            };
            if let Some(p) = problem.filter(|p| !problems.contains(p)) {
                problems.push(p);
            }
        }
    }
    let tasks: Vec<(Uuid, Decimal)> = milestones.into_iter().collect();
    if !tasks.is_empty() {
        lock(hc.tx, "e_task", tenant, &tasks.iter().map(|t| t.0).collect::<Vec<_>>()).await?;
        for (t, _) in &tasks {
            let Some(task) = try_get(hc.kernel, hc.ctx, hc.tx, "Task", *t).await? else {
                problems.push("a billed milestone no longer exists".into());
                continue;
            };
            let name = label(&task);
            if id(&task, "project") != Some(project) {
                problems.push(format!("milestone {name} belongs to another project"));
            } else if !b(&task, "is_milestone") {
                problems.push(format!("{name} is not a milestone"));
            } else if let Some(other) = id(&task, "sales_invoice") {
                problems.push(format!("milestone {name} is already invoiced on {}", invoice_number(hc, other).await?));
            } else if s(&task, "status") == Some("Cancelled") {
                problems.push(format!("milestone {name} is cancelled"));
            }
        }
    }
    if !problems.is_empty() {
        return Err(Error::invalid(format!("this invoice cannot bill its project items: {}", problems.join("; "))));
    }
    if !logs.is_empty() {
        sqlx::query("update e_timesheet__time_logs set billed = true, sales_invoice = $3, updated_at = now() where tenant_id = $1 and id = any($2)")
            .bind(tenant)
            .bind(&logs)
            .bind(inv)
            .execute(&mut **hc.tx)
            .await?;
    }
    for (t, amount) in &tasks {
        sqlx::query("update e_task set sales_invoice = $3, billed_amount = $4, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(t)
            .bind(inv)
            .bind(amount)
            .execute(&mut **hc.tx)
            .await?;
    }
    for ts in timesheets {
        refresh_timesheet(hc.kernel, hc.ctx, hc.tx, ts).await?;
    }
    refresh_project(hc.kernel, hc.ctx, hc.tx, project, None).await
}

async fn unmark_billed(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
    let tenant = hc.ctx.tenant;
    let timesheets: Vec<Uuid> = sqlx::query_scalar(
        "select t.id from e_timesheet t where t.tenant_id = $1 \
         and t.id in (select parent_id from e_timesheet__time_logs where tenant_id = $1 and sales_invoice = $2) order by t.id for update",
    )
    .bind(tenant)
    .bind(inv)
    .fetch_all(&mut **hc.tx)
    .await?;
    let mut projects: Vec<Option<Uuid>> =
        sqlx::query_scalar("update e_timesheet__time_logs set billed = false, sales_invoice = null, updated_at = now() where tenant_id = $1 and sales_invoice = $2 returning project")
            .bind(tenant)
            .bind(inv)
            .fetch_all(&mut **hc.tx)
            .await?;
    let milestone_projects: Vec<Option<Uuid>> = sqlx::query_scalar(
        "update e_task set sales_invoice = null, billed_amount = null, updated_at = now() where tenant_id = $1 and sales_invoice = $2 returning project",
    )
    .bind(tenant)
    .bind(inv)
    .fetch_all(&mut **hc.tx)
    .await?;
    projects.extend(milestone_projects);
    let mut projects: Vec<Uuid> = projects.into_iter().flatten().collect();
    projects.sort();
    projects.dedup();
    for ts in timesheets {
        refresh_timesheet(hc.kernel, hc.ctx, hc.tx, ts).await?;
    }
    for p in projects {
        refresh_project(hc.kernel, hc.ctx, hc.tx, p, None).await?;
    }
    Ok(())
}
