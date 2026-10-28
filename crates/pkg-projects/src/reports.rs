//! Project reports, each computed in one read-only, tenant-isolated transaction: profitability
//! (billed revenue against timesheet and ledger expense costs), timesheet summary and task overview.

use crate::logic::{percent, task_bucket};
use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use ferrum_domain::reports::{Params, ReportCompany, ReportSource, col, param_def, period, read_tx, resolve_company};
use ferrum_domain::util::money;
use ferrum_kernel::{Access, Action, Ctx, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

const MAX_ROWS: i64 = 10_000;

pub struct Reports;

fn link(name: &str, label: &str, to: &str) -> Value {
    json!({"name": name, "label": label, "type": "link", "to": to})
}

fn text(name: &str, label: &str) -> Value {
    json!({"name": name, "label": label, "type": "data"})
}

#[async_trait]
impl ReportSource for Reports {
    fn catalog(&self) -> Vec<Value> {
        vec![
            json!({"name": "project_profitability", "label": "Project Profitability", "module": "projects", "entity": "Project",
                "params": [param_def("company"), {"name": "from_date", "label": "From Date", "type": "date"}, {"name": "to_date", "label": "To Date", "type": "date"},
                    link("project", "Project", "Project"), param_def("customer"),
                    {"name": "status", "label": "Status", "type": "select", "options": ["Open", "On Hold", "Completed", "Cancelled"]}]}),
            json!({"name": "timesheet_summary", "label": "Timesheet Summary", "module": "projects", "entity": "Timesheet",
                "params": [param_def("company"), param_def("from_date"), param_def("to_date"), text("user", "User (email)"),
                    link("project", "Project", "Project"), link("activity_type", "Activity Type", "ActivityType")]}),
            json!({"name": "task_overview", "label": "Task Overview", "module": "projects", "entity": "Task",
                "params": [param_def("company"), link("project", "Project", "Project"), text("user", "Assignee (email)")]}),
        ]
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let mut out = match name {
            "project_profitability" => profitability(&mut tx, ctx, &co, &p).await?,
            "timesheet_summary" => timesheet_summary(&mut tx, ctx, &co, &p).await?,
            "task_overview" => task_overview(&mut tx, ctx, &co, &p).await?,
            _ => return Err(Error::NotFound(format!("report {name}"))),
        };
        tx.rollback().await?;
        if let Some(meta) = out.get_mut("meta").and_then(Value::as_object_mut) {
            meta.insert("company".into(), json!(co.name));
            meta.insert("currency".into(), json!(co.currency));
        }
        Ok(out)
    }
}

/// The user, when their role reads only its own records of `entity` (the creator or anyone named
/// in its owner fields), so a report shows just those.
fn own_only(ctx: &Ctx, entity: &str) -> Option<Uuid> {
    (!ctx.is_admin && !ctx.is_system && ctx.perms.access(entity, Action::Read) == Access::Own).then_some(ctx.user)
}

/// Costs show only to roles that may read timesheet costing (the `costing` field group).
fn can_see_costs(ctx: &Ctx) -> bool {
    ctx.schema.entity("TimesheetDetail").and_then(|e| e.field("costing_amount")).is_some_and(|f| ctx.perms.can_read_field("TimesheetDetail", f))
}

fn project_label(number: Option<String>, name: Option<String>) -> String {
    format!("{} {}", number.unwrap_or_default(), name.unwrap_or_default()).trim().to_string()
}

async fn profitability(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    if !can_see_costs(ctx) {
        return Err(Error::Forbidden("project profitability shows costs, which your roles may not see".into()));
    }
    let (from, to) = (p.date("from_date")?, p.date("to_date")?);
    if let (Some(f), Some(t)) = (from, to)
        && f > t
    {
        return Err(Error::field("from_date", "must not be after the to date"));
    }
    #[allow(clippy::type_complexity)]
    let projects: Vec<(Uuid, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<Decimal>)> = sqlx::query_as(
        "select p.id, p.number, p.project_name, p.status, c.customer_name, p.ext->>'proj_billing_method', nullif(p.ext->>'proj_contract_value', '')::numeric \
         from e_project p left join e_customer c on c.tenant_id = p.tenant_id and c.id = p.customer \
         where p.tenant_id = $1 and p.company = $2 and ($3::uuid is null or p.id = $3) and ($4::uuid is null or p.customer = $4) \
         and ($5::text is null or p.status = $5) order by p.number nulls last, p.project_name limit $6",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(p.uuid("project")?)
    .bind(p.uuid("customer")?)
    .bind(p.str("status"))
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = projects.len() as i64 > MAX_ROWS;
    let ids: Vec<Uuid> = projects.iter().take(MAX_ROWS as usize).map(|r| r.0).collect();
    // Revenue: submitted invoices (credit notes net it down), net of taxes, in company currency.
    let revenue: HashMap<Uuid, Decimal> = sqlx::query_as::<_, (Uuid, Decimal)>(
        "select project, coalesce(sum(base_net_total), 0) from e_sales_invoice where tenant_id = $1 and company = $2 and docstatus = 1 \
         and project = any($3) and ($4::date is null or posting_date >= $4) and ($5::date is null or posting_date <= $5) group by project",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(&ids)
    .bind(from)
    .bind(to)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    let time: HashMap<Uuid, (Decimal, Decimal)> = sqlx::query_as::<_, (Uuid, Decimal, Decimal)>(
        "select l.project, coalesce(sum(l.costing_amount), 0), coalesce(sum(l.billing_amount) filter (where l.is_billable and not l.billed), 0) \
         from e_timesheet__time_logs l join e_timesheet t on t.tenant_id = l.tenant_id and t.id = l.parent_id \
         where l.tenant_id = $1 and t.docstatus = 1 and l.project = any($2) \
         and ($3::date is null or l.from_time >= $3::date) and ($4::date is null or l.from_time < $4::date + 1) group by l.project",
    )
    .bind(ctx.tenant)
    .bind(&ids)
    .bind(from)
    .bind(to)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|(p, cost, unbilled)| (p, (cost, unbilled)))
    .collect();
    // Purchases and other expenses booked to the project: expense-account journal lines carrying
    // it (cancellations post reversals, which net out here).
    let expense: HashMap<Uuid, Decimal> = sqlx::query_as::<_, (Uuid, Decimal)>(
        "select l.project_id, coalesce(sum(l.debit - l.credit), 0) from journal_line l \
         join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
         where l.tenant_id = $1 and l.company_id = $2 and l.project_id = any($3) and a.root_type = 'Expense' \
         and ($4::date is null or l.posting_date >= $4) and ($5::date is null or l.posting_date <= $5) group by l.project_id",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(&ids)
    .bind(from)
    .bind(to)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    let r = |v: Decimal| money(v.round_dp(co.precision));
    let names = ["contract_value", "billed_revenue", "timesheet_cost", "expense_cost", "total_cost", "margin", "unbilled_time"];
    let mut totals = [Decimal::ZERO; 7];
    let mut rows = Vec::new();
    for (pid, number, name, status, customer, method, contract) in projects.into_iter().take(MAX_ROWS as usize) {
        let billed = revenue.get(&pid).copied().unwrap_or_default();
        let (time_cost, unbilled) = time.get(&pid).copied().unwrap_or_default();
        let expense_cost = expense.get(&pid).copied().unwrap_or_default();
        let cost = time_cost + expense_cost;
        let values = [contract.unwrap_or_default(), billed, time_cost, expense_cost, cost, billed - cost, unbilled];
        let mut row = json!({"project_id": pid, "project": project_label(number, name), "customer": customer, "status": status, "billing_method": method,
            "margin_pct": if billed.is_zero() { Value::Null } else { money(percent(billed - cost, billed)) }});
        for (i, v) in values.into_iter().enumerate() {
            totals[i] += v;
            row[names[i]] = r(v);
        }
        rows.push(row);
    }
    let mut total_obj: serde_json::Map<String, Value> = names.iter().zip(totals).map(|(n, v)| (n.to_string(), r(v))).collect();
    total_obj.insert("margin_pct".into(), if totals[1].is_zero() { Value::Null } else { money(percent(totals[5], totals[1])) });
    Ok(json!({
        "columns": [col("project", "Project", "data"), col("customer", "Customer", "data"), col("status", "Status", "data"), col("billing_method", "Billing", "data"),
            col("contract_value", "Contract Value", "currency"), col("billed_revenue", "Billed Revenue", "currency"), col("timesheet_cost", "Timesheet Cost", "currency"),
            col("expense_cost", "Expense Cost", "currency"), col("total_cost", "Total Cost", "currency"), col("margin", "Margin", "currency"),
            col("margin_pct", "Margin %", "decimal"), col("unbilled_time", "Unbilled Time", "currency")],
        "rows": rows,
        "totals": total_obj,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

async fn timesheet_summary(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    // Roles that read only their own timesheets (created by them or recording their time) summarise only those.
    let own = own_only(ctx, "Timesheet");
    let costs = can_see_costs(ctx);
    #[allow(clippy::type_complexity)]
    let data: Vec<(Option<String>, Option<String>, Option<String>, Option<String>, Decimal, Decimal, Decimal, Decimal, Decimal, Decimal)> = sqlx::query_as(
        "select t.\"user\", p.number, p.project_name, a.activity_type, coalesce(sum(l.hours), 0), \
         coalesce(sum(l.billing_hours) filter (where l.is_billable), 0), coalesce(sum(l.billing_hours) filter (where l.billed), 0), \
         coalesce(sum(l.costing_amount), 0), coalesce(sum(l.billing_amount) filter (where l.is_billable), 0), \
         coalesce(sum(l.billing_amount) filter (where l.billed), 0) \
         from e_timesheet__time_logs l join e_timesheet t on t.tenant_id = l.tenant_id and t.id = l.parent_id \
         left join e_project p on p.tenant_id = l.tenant_id and p.id = l.project \
         left join e_activity_type a on a.tenant_id = l.tenant_id and a.id = l.activity_type \
         where l.tenant_id = $1 and t.company = $2 and t.docstatus = 1 and l.from_time >= $3::date and l.from_time < $4::date + 1 \
         and ($5::text is null or t.\"user\" = $5) and ($6::uuid is null or l.project = $6) and ($7::uuid is null or l.activity_type = $7) \
         and ($8::uuid is null or t.created_by = $8 or lower(t.\"user\") = $9) \
         group by t.\"user\", l.project, p.number, p.project_name, a.activity_type \
         order by t.\"user\", p.number nulls first, a.activity_type limit $10",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(p.str("user").map(str::to_lowercase))
    .bind(p.uuid("project")?)
    .bind(p.uuid("activity_type")?)
    .bind(own)
    .bind(ctx.email.to_lowercase())
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let names = ["hours", "billable_hours", "billed_hours", "cost", "billable_amount", "billed_amount"];
    let mut totals = [Decimal::ZERO; 6];
    let mut rows = Vec::new();
    for (user, number, name, activity, hours, billable_h, billed_h, cost, billable, billed) in data.into_iter().take(MAX_ROWS as usize) {
        let project = if number.is_none() && name.is_none() { "(no project)".to_string() } else { project_label(number, name) };
        let mut row = json!({"user": user, "project": project, "activity_type": activity});
        for (i, v) in [hours, billable_h, billed_h, cost, billable, billed].into_iter().enumerate() {
            if names[i] == "cost" && !costs {
                continue;
            }
            totals[i] += v;
            row[names[i]] = money(v);
        }
        rows.push(row);
    }
    let mut columns =
        vec![col("user", "User", "data"), col("project", "Project", "data"), col("activity_type", "Activity Type", "data"), col("hours", "Hours", "decimal")];
    columns.extend([col("billable_hours", "Billable Hours", "decimal"), col("billed_hours", "Billed Hours", "decimal")]);
    if costs {
        columns.push(col("cost", "Cost", "currency"));
    }
    columns.extend([col("billable_amount", "Billable Amount", "currency"), col("billed_amount", "Billed Amount", "currency")]);
    let total_obj: serde_json::Map<String, Value> =
        names.iter().zip(totals).filter(|(n, _)| costs || **n != "cost").map(|(n, v)| (n.to_string(), money(v))).collect();
    Ok(json!({
        "columns": columns,
        "rows": rows,
        "totals": total_obj,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated, "own_time_only": own.is_some()},
    }))
}

#[derive(Default)]
struct TaskAgg {
    tasks: i64,
    open: i64,
    overdue: i64,
    completed: i64,
    planned: Decimal,
    actual: Decimal,
}

impl TaskAgg {
    fn add(&mut self, (open, overdue, completed): (bool, bool, bool), planned: Decimal, actual: Decimal) {
        self.tasks += 1;
        self.open += open as i64;
        self.overdue += overdue as i64;
        self.completed += completed as i64;
        self.planned += planned;
        self.actual += actual;
    }

    fn json(&self) -> Value {
        json!({"tasks": self.tasks, "open": self.open, "overdue": self.overdue, "completed": self.completed,
            "planned_hours": money(self.planned), "actual_hours": money(self.actual), "variance_hours": money(self.actual - self.planned)})
    }
}

async fn task_overview(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let today = Utc::now().date_naive();
    #[allow(clippy::type_complexity)]
    let data: Vec<(Uuid, Option<String>, Option<String>, Option<String>, Option<String>, Option<NaiveDate>, Option<Decimal>, Option<Decimal>)> =
        sqlx::query_as(
            "select t.id, p.number, p.project_name, a.\"user\", t.status, t.exp_end_date, t.expected_hours, t.actual_hours \
         from e_task t join e_project p on p.tenant_id = t.tenant_id and p.id = t.project \
         left join e_task__assigned_to a on a.tenant_id = t.tenant_id and a.parent_id = t.id \
         where t.tenant_id = $1 and p.company = $2 and t.status <> 'Cancelled' and ($3::uuid is null or t.project = $3) \
         and ($4::text is null or a.\"user\" = $4) \
         and ($5::uuid is null or t.created_by = $5 or exists (select 1 from e_task__assigned_to o \
              where o.tenant_id = t.tenant_id and o.parent_id = t.id and lower(o.\"user\") = $6)) \
         order by p.number, a.\"user\" nulls last limit $7",
        )
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(p.uuid("project")?)
        .bind(p.str("user").map(str::to_lowercase))
        .bind(own_only(ctx, "Task"))
        .bind(ctx.email.to_lowercase())
        .bind(MAX_ROWS + 1)
        .fetch_all(&mut **tx)
        .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let mut groups: BTreeMap<(String, String), TaskAgg> = BTreeMap::new();
    // A task with several assignees appears on each of their rows but counts once in the totals.
    let mut total = TaskAgg::default();
    let mut seen = HashSet::new();
    for (task, number, name, assignee, status, end, planned, actual) in data.into_iter().take(MAX_ROWS as usize) {
        let bucket = task_bucket(status.as_deref().unwrap_or("Open"), end, today);
        let (planned, actual) = (planned.unwrap_or_default(), actual.unwrap_or_default());
        let key = (project_label(number, name), assignee.unwrap_or_else(|| "(unassigned)".into()));
        groups.entry(key).or_default().add(bucket, planned, actual);
        if seen.insert(task) {
            total.add(bucket, planned, actual);
        }
    }
    let rows: Vec<Value> = groups
        .into_iter()
        .map(|((project, assignee), agg)| {
            let mut row = agg.json();
            row["project"] = json!(project);
            row["assignee"] = json!(assignee);
            row
        })
        .collect();
    Ok(json!({
        "columns": [col("project", "Project", "data"), col("assignee", "Assignee", "data"), col("tasks", "Tasks", "int"), col("open", "Open", "int"),
            col("overdue", "Overdue", "int"), col("completed", "Completed", "int"), col("planned_hours", "Planned Hours", "decimal"),
            col("actual_hours", "Actual Hours", "decimal"), col("variance_hours", "Variance (h)", "decimal")],
        "rows": rows,
        "totals": total.json(),
        "meta": {"as_of": today, "truncated": truncated},
    }))
}
