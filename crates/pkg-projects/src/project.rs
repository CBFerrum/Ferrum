//! Project and Task hooks, and the roll-ups that keep project and task figures in step with
//! timesheets, invoices and task progress.

use crate::logic::{CLOSED_PROJECT, STARTED, dependency_cycle, weighted_progress};
use crate::{lock, unknown_users};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use ferrum_domain::util::{b, d, elevated, get, id, money, rows, s, set, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

/// A record read with engine rights; `None` when it does not exist (links stored in columns are
/// only checked by their foreign key at write time, after the hooks).
pub(crate) async fn try_get(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, record: Uuid) -> Result<Option<Doc>> {
    match get(k, ctx, tx, entity, record).await {
        Ok(doc) => Ok(Some(doc)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `TASK-00012 Design` / `PROJ-0003 Website` for messages.
pub(crate) fn label(doc: &Doc) -> String {
    let title = s(doc, "subject").or_else(|| s(doc, "project_name")).unwrap_or_default();
    match s(doc, "number") {
        Some(n) => format!("{n} {title}").trim().to_string(),
        None => title.to_string(),
    }
}

fn task_label(number: Option<String>, subject: Option<String>) -> String {
    format!("{} {}", number.unwrap_or_default(), subject.unwrap_or_default()).trim().to_string()
}

// ───────────────────────────── projects ─────────────────────────────

pub struct ProjectHook;

/// The project's tasks that are neither completed nor cancelled, as `(id, label)`.
async fn open_tasks(tx: &mut Tx, tenant: Uuid, project: Uuid) -> Result<Vec<(Uuid, String)>> {
    let open: Vec<(Uuid, Option<String>, Option<String>)> = sqlx::query_as(
        "select id, number, subject from e_task where tenant_id = $1 and project = $2 and status not in ('Completed', 'Cancelled') order by number",
    )
    .bind(tenant)
    .bind(project)
    .fetch_all(&mut **tx)
    .await?;
    Ok(open.into_iter().map(|(i, n, subj)| (i, task_label(n, subj))).collect())
}

fn open_tasks_message(open: &[(Uuid, String)]) -> String {
    let names: Vec<&str> = open.iter().take(5).map(|t| t.1.as_str()).collect();
    format!(
        "{} task(s) are still open ({}{}); complete or cancel them, or run Complete Project with force to cancel them",
        open.len(),
        names.join(", "),
        if open.len() > 5 { ", …" } else { "" }
    )
}

#[async_trait]
impl DocHook for ProjectHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let Some(pid) = id(doc, "id") else { return Ok(()) };
        if s(doc, "status") == Some("Completed") {
            let open = open_tasks(hc.tx, hc.ctx.tenant, pid).await?;
            if !open.is_empty() {
                return Err(Error::field("status", open_tasks_message(&open)));
            }
        }
        let derived = project_derived(hc.tx, hc.ctx.tenant, pid, doc, None).await?;
        doc.extend(derived);
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        match name {
            "make_sales_invoice" => crate::billing::invoice_project(hc, doc, params).await.map(Some),
            "complete" => complete(hc, doc, params).await.map(Some),
            _ => Ok(None),
        }
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        if let Some(pm) = s(doc, "proj_project_manager")
            && !unknown_users(hc.tx, hc.ctx.tenant, &[pm.to_string()]).await?.is_empty()
        {
            errors.push(FieldError::new("proj_project_manager", "no user has this email"));
        }
        if let Some(cc) = id(doc, "proj_cost_center") {
            let center = get(hc.kernel, hc.ctx, hc.tx, "CostCenter", cc).await?;
            if id(&center, "company") != id(doc, "company") {
                errors.push(FieldError::new("proj_cost_center", "belongs to another company"));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

/// `complete`: refused while tasks are open unless `force` is set, which cancels them first. Every
/// change goes through the task and project pipelines, so it is validated, audited and published.
async fn complete(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    let pid = id(doc, "id").ok_or_else(|| Error::internal("project without id"))?;
    let force = matches!(params.get("force"), Some(Value::Bool(true))) || matches!(params.get("force").and_then(Value::as_str), Some("true" | "1"));
    let open = open_tasks(hc.tx, hc.ctx.tenant, pid).await?;
    if !open.is_empty() && !force {
        return Err(Error::invalid(open_tasks_message(&open)));
    }
    let actor = elevated(hc.ctx);
    let status = |s: &str| {
        let mut patch = Doc::new();
        patch.insert("status".into(), json!(s));
        patch
    };
    for (task, _) in &open {
        hc.kernel.update_tx(&actor, hc.tx, "Task", *task, status("Cancelled"), None).await?;
    }
    hc.kernel.update_tx(&actor, hc.tx, "Project", pid, status("Completed"), None).await?;
    Ok(json!({"status": "Completed", "cancelled_tasks": open.len()}))
}

/// Figures derived from submitted time logs, billed milestones and (unless % complete is manual)
/// task progress, recomputed from their sources every time so they cannot drift.
pub(crate) async fn project_derived(tx: &mut Tx, tenant: Uuid, project: Uuid, doc: &Doc, without_task: Option<Uuid>) -> Result<Doc> {
    let (hours, cost, billable, billed, first, last): (Decimal, Decimal, Decimal, Decimal, Option<DateTime<Utc>>, Option<DateTime<Utc>>) = sqlx::query_as(
        "select coalesce(sum(l.hours), 0), coalesce(sum(l.costing_amount), 0), \
         coalesce(sum(l.billing_amount) filter (where l.is_billable), 0), coalesce(sum(l.billing_amount) filter (where l.billed), 0), \
         min(l.from_time), max(l.to_time) \
         from e_timesheet__time_logs l join e_timesheet t on t.tenant_id = l.tenant_id and t.id = l.parent_id \
         where l.tenant_id = $1 and l.project = $2 and t.docstatus = 1",
    )
    .bind(tenant)
    .bind(project)
    .fetch_one(&mut **tx)
    .await?;
    let tasks: Vec<(Option<String>, Option<Decimal>, Option<Decimal>, Option<Uuid>, Option<Decimal>)> = sqlx::query_as(
        "select status, progress, expected_hours, sales_invoice, billed_amount from e_task \
         where tenant_id = $1 and project = $2 and ($3::uuid is null or id <> $3)",
    )
    .bind(tenant)
    .bind(project)
    .bind(without_task)
    .fetch_all(&mut **tx)
    .await?;
    let milestones: Decimal = tasks.iter().filter(|t| t.3.is_some()).map(|t| t.4.unwrap_or_default()).sum();
    let date = |t: Option<DateTime<Utc>>| t.map(|t| json!(t.date_naive().to_string())).unwrap_or(Value::Null);
    let mut v = Doc::new();
    v.insert("proj_actual_hours".into(), money(hours));
    v.insert("proj_costing_amount".into(), money(cost));
    v.insert("proj_billable_amount".into(), money(billable));
    v.insert("proj_billed_amount".into(), money(billed + milestones));
    v.insert("proj_actual_start_date".into(), date(first));
    v.insert("proj_actual_end_date".into(), date(last));
    if s(doc, "proj_percent_complete_method") != Some("Manual") {
        let progress: Vec<(String, Decimal, Decimal)> =
            tasks.into_iter().map(|(st, p, h, _, _)| (st.unwrap_or_default(), p.unwrap_or_default(), h.unwrap_or_default())).collect();
        v.insert("proj_percent_complete".into(), money(weighted_progress(&progress)));
    }
    Ok(v)
}

/// Recomputes a project's derived figures under its row lock, so concurrent roll-ups queue and
/// each sees the other's committed time.
pub(crate) async fn refresh_project(k: &Kernel, ctx: &Ctx, tx: &mut Tx, project: Uuid, without_task: Option<Uuid>) -> Result<()> {
    let sctx = sys(ctx);
    let doc = k.get_tx(&sctx, tx, "Project", project, true).await?;
    let v = project_derived(tx, ctx.tenant, project, &doc, without_task).await?;
    k.set_values_tx(&sctx, tx, "Project", project, v).await
}

/// Recomputes a task's actual hours from submitted time, under its row lock.
pub(crate) async fn refresh_task(k: &Kernel, ctx: &Ctx, tx: &mut Tx, task: Uuid) -> Result<()> {
    lock(tx, "e_task", ctx.tenant, &[task]).await?;
    let hours: Decimal = sqlx::query_scalar(
        "select coalesce(sum(l.hours), 0) from e_timesheet__time_logs l join e_timesheet t on t.tenant_id = l.tenant_id and t.id = l.parent_id \
         where l.tenant_id = $1 and l.task = $2 and t.docstatus = 1",
    )
    .bind(ctx.tenant)
    .bind(task)
    .fetch_one(&mut **tx)
    .await?;
    let mut v = Doc::new();
    v.insert("actual_hours".into(), money(hours));
    k.set_values_tx(&sys(ctx), tx, "Task", task, v).await
}

// ───────────────────────────── tasks ─────────────────────────────

pub struct TaskHook;

#[async_trait]
impl DocHook for TaskHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let today = Utc::now().date_naive();
        let past_due = s(doc, "exp_end_date").and_then(|e| NaiveDate::parse_from_str(e, "%Y-%m-%d").ok()).is_some_and(|e| e < today);
        // Overdue means "should be done and has not started"; it follows the dates both ways.
        let status = match s(doc, "status").unwrap_or("Open") {
            "Open" if past_due => "Overdue",
            "Overdue" if !past_due => "Open",
            other => other,
        }
        .to_string();
        if status == "Completed" {
            set(doc, "progress", money(Decimal::ONE_HUNDRED));
            if s(doc, "completed_on").is_none() {
                set(doc, "completed_on", today.to_string());
            }
        } else {
            set(doc, "completed_on", Value::Null);
        }
        set(doc, "status", status);
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        check_task(hc, doc).await
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        let mut projects: Vec<Uuid> = [id(doc, "project"), old.and_then(|o| id(o, "project"))].into_iter().flatten().collect();
        projects.sort();
        projects.dedup();
        for p in projects {
            refresh_project(hc.kernel, hc.ctx, hc.tx, p, None).await?;
        }
        Ok(())
    }

    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let Some(inv) = id(doc, "sales_invoice") {
            return Err(Error::invalid(format!("this milestone is invoiced on {}; cancel that invoice first", invoice_number(hc, inv).await?)));
        }
        if let Some(p) = id(doc, "project") {
            refresh_project(hc.kernel, hc.ctx, hc.tx, p, id(doc, "id")).await?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "make_milestone_invoice" {
            return Ok(None);
        }
        crate::billing::invoice_milestone(hc, doc).await.map(Some)
    }
}

pub(crate) async fn invoice_number(hc: &mut HookCtx<'_>, invoice: Uuid) -> Result<String> {
    let n: Option<Option<String>> = sqlx::query_scalar("select number from e_sales_invoice where tenant_id = $1 and id = $2")
        .bind(hc.ctx.tenant)
        .bind(invoice)
        .fetch_optional(&mut **hc.tx)
        .await?;
    Ok(n.flatten().unwrap_or_else(|| invoice.to_string()))
}

/// `id → (label, status)` of tasks.
async fn task_states(tx: &mut Tx, tenant: Uuid, ids: &[Uuid]) -> Result<HashMap<Uuid, (String, String)>> {
    let found: Vec<(Uuid, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as("select id, number, subject, status from e_task where tenant_id = $1 and id = any($2)")
            .bind(tenant)
            .bind(ids)
            .fetch_all(&mut **tx)
            .await?;
    Ok(found.into_iter().map(|(i, n, subj, st)| (i, (task_label(n, subj), st.unwrap_or_default()))).collect())
}

/// The dependency edges reachable from `deps` (excluding `task`'s own, which are being replaced),
/// checked for a path back to `task`.
async fn find_cycle(tx: &mut Tx, tenant: Uuid, task: Uuid, deps: &[Uuid]) -> Result<Option<Vec<Uuid>>> {
    let edges: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "with recursive reach(id) as ( \
           select unnest($2::uuid[]) \
           union select d.task from e_task__depends_on d join reach r on d.parent_id = r.id \
           where d.tenant_id = $1 and r.id <> $3 and d.task is not null) \
         select d.parent_id, d.task from e_task__depends_on d \
         where d.tenant_id = $1 and d.parent_id <> $3 and d.task is not null and d.parent_id in (select id from reach)",
    )
    .bind(tenant)
    .bind(deps)
    .bind(task)
    .fetch_all(&mut **tx)
    .await?;
    let mut graph: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (from, to) in edges {
        graph.entry(from).or_default().push(to);
    }
    Ok(dependency_cycle(task, deps, &graph))
}

async fn check_task(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let tid = id(doc, "id").ok_or_else(|| Error::internal("task without id"))?;
    let tenant = hc.ctx.tenant;
    // Who may change the task (creator, assignee, manager) is the kernel's `owner_fields` check.
    let old = try_get(hc.kernel, hc.ctx, hc.tx, "Task", tid).await?;
    let assignees = |d: &Doc| -> Vec<String> { rows(d, "assigned_to").iter().filter_map(|r| s(r, "user").map(str::to_lowercase)).collect() };
    let project_id = id(doc, "project").ok_or_else(|| Error::field("project", "is required"))?;
    let project = try_get(hc.kernel, hc.ctx, hc.tx, "Project", project_id).await?.ok_or_else(|| Error::field("project", "does not exist"))?;
    let moved_from = old.as_ref().and_then(|o| id(o, "project")).filter(|p| *p != project_id);
    let mut projects = vec![project.clone()];
    if let Some(p) = moved_from {
        projects.push(get(hc.kernel, hc.ctx, hc.tx, "Project", p).await?);
    }
    for p in &projects {
        if let Some(st) = s(p, "status").filter(|st| CLOSED_PROJECT.contains(st)) {
            return Err(Error::field("project", format!("project {} is {}; reopen it to change its tasks", label(p), st.to_lowercase())));
        }
    }
    let status = s(doc, "status").unwrap_or("Open");
    let was = old.as_ref().and_then(|o| s(o, "status"));
    let mut errors = Vec::new();

    if let Some(parent) = id(doc, "parent_task") {
        match try_get(hc.kernel, hc.ctx, hc.tx, "Task", parent).await? {
            None => errors.push(FieldError::new("parent_task", "does not exist")),
            Some(p) if id(&p, "project") != Some(project_id) => errors.push(FieldError::new("parent_task", "belongs to another project")),
            _ => {}
        }
    }

    // Dependencies: no self-reference, no duplicates, no cycle anywhere in the graph, and no
    // start before every dependency is completed.
    let deps: Vec<Uuid> = rows(doc, "depends_on").iter().filter_map(|r| id(r, "task")).collect();
    for (i, dep) in deps.iter().enumerate() {
        if *dep == tid {
            errors.push(FieldError::new("task", "a task cannot depend on itself").in_row("depends_on", i));
        } else if deps[..i].contains(dep) {
            errors.push(FieldError::new("task", "is listed twice").in_row("depends_on", i));
        }
    }
    if errors.is_empty() && !deps.is_empty() {
        let states = task_states(hc.tx, tenant, &deps).await?;
        for (i, dep) in deps.iter().enumerate() {
            if !states.contains_key(dep) {
                errors.push(FieldError::new("task", "does not exist").in_row("depends_on", i));
            }
        }
        if errors.is_empty() {
            // Two concurrent edits could each add one half of a cycle: serialize them per tenant.
            sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("ferrum:task_deps:{tenant}")).execute(&mut **hc.tx).await?;
            if let Some(cycle) = find_cycle(hc.tx, tenant, tid, &deps).await? {
                let names = task_states(hc.tx, tenant, &cycle).await?;
                let path: Vec<String> =
                    cycle.iter().map(|t| if *t == tid { label(doc) } else { names.get(t).map(|n| n.0.clone()).unwrap_or_else(|| t.to_string()) }).collect();
                errors.push(FieldError::new("depends_on", format!("this would create a dependency cycle: {}", path.join(" → "))));
            }
            if STARTED.contains(&status) && was != Some(status) {
                let pending: Vec<String> =
                    deps.iter().filter_map(|d| states.get(d)).filter(|(_, st)| st != "Completed").map(|(l, st)| format!("{l} ({st})")).collect();
                if !pending.is_empty() {
                    errors.push(FieldError::new("status", format!("cannot be {status} until these tasks are completed: {}", pending.join(", "))));
                }
            }
        }
    }

    // A parent task closes after its sub-tasks.
    if status == "Completed" && was != Some("Completed") {
        let open: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
            "select number, subject from e_task where tenant_id = $1 and parent_task = $2 and status not in ('Completed', 'Cancelled') order by number",
        )
        .bind(tenant)
        .bind(tid)
        .fetch_all(&mut **hc.tx)
        .await?;
        if !open.is_empty() {
            let names: Vec<String> = open.into_iter().map(|(n, subj)| task_label(n, subj)).collect();
            errors.push(FieldError::new("status", format!("complete or cancel its sub-tasks first: {}", names.join(", "))));
        }
    }

    // Milestone billing: a share of the contract value, at most 100 % per project, frozen once invoiced.
    let pct = d(doc, "billing_percent");
    if let Some(o) = &old
        && let Some(inv) = id(o, "sales_invoice")
    {
        let number = invoice_number(hc, inv).await?;
        if d(o, "billing_percent") != pct || !b(doc, "is_milestone") || moved_from.is_some() {
            errors.push(FieldError::new("billing_percent", format!("this milestone is invoiced on {number}; cancel that invoice before changing its billing")));
        }
        if status == "Cancelled" {
            errors.push(FieldError::new("status", format!("this milestone is invoiced on {number}; cancel that invoice first")));
        }
    }
    if pct > Decimal::ZERO {
        if !b(doc, "is_milestone") {
            errors.push(FieldError::new("billing_percent", "only milestones carry a billing percentage"));
        } else if s(&project, "proj_billing_method") != Some("Fixed Price") {
            errors.push(FieldError::new("billing_percent", "milestone billing applies to Fixed Price projects"));
        } else {
            // Under the project's lock, so two milestones saved at once cannot together pass 100 %.
            lock(hc.tx, "e_project", tenant, &[project_id]).await?;
            let others: Decimal = sqlx::query_scalar(
                "select coalesce(sum(billing_percent), 0) from e_task where tenant_id = $1 and project = $2 and id <> $3 and is_milestone and status <> 'Cancelled'",
            )
            .bind(tenant)
            .bind(project_id)
            .bind(tid)
            .fetch_one(&mut **hc.tx)
            .await?;
            if others + pct > Decimal::ONE_HUNDRED {
                errors.push(FieldError::new(
                    "billing_percent",
                    format!("the project's milestones would bill {}% of the contract value; at most 100% can be billed", (others + pct).normalize()),
                ));
            }
        }
    }

    // A task carries its booked time and sub-tasks with it, so it stays in its project once used.
    if let Some(o) = &old
        && moved_from.is_some()
    {
        let children: i64 =
            sqlx::query_scalar("select count(*) from e_task where tenant_id = $1 and parent_task = $2").bind(tenant).bind(tid).fetch_one(&mut **hc.tx).await?;
        if d(o, "actual_hours") > Decimal::ZERO || children > 0 {
            errors.push(FieldError::new("project", "time or sub-tasks are booked on this task, so it cannot move to another project"));
        }
    }

    // Assignees: each once, and (when newly added) a user of this tenant.
    let emails = assignees(doc);
    let before = old.as_ref().map(assignees).unwrap_or_default();
    let added: Vec<String> = emails.iter().filter(|e| !before.contains(e)).cloned().collect();
    let unknown = unknown_users(hc.tx, tenant, &added).await?;
    for (i, e) in emails.iter().enumerate() {
        if emails[..i].contains(e) {
            errors.push(FieldError::new("user", "is assigned twice").in_row("assigned_to", i));
        } else if unknown.contains(e) {
            errors.push(FieldError::new("user", "no user has this email").in_row("assigned_to", i));
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
}
