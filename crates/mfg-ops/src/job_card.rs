//! Job cards: time logged on one work order operation at a workstation. Logs may not put more
//! jobs on a workstation than its capacity; submitting rolls the completed quantity, time and cost
//! into the work order operation, in sequence (an operation cannot complete more than the one
//! before it). Materials consumed at the operation can be transferred to the shop floor first.

use crate::calendar::peak_overlap;
use crate::masters::capacity_of;
use crate::work_order::open_qty;
use crate::{installed, param_dec, param_uuid, rows_value, submit_param, table_of, xact_lock};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{b, company, d, elevated, get, id, money, round, rows, s, set, sys};
use ferrum_kernel::value::parse_datetime;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

fn status(doc: &Doc) -> &'static str {
    match d(doc, "docstatus") {
        x if x == Decimal::TWO => "Cancelled",
        x if x == Decimal::ONE => "Completed",
        _ if !rows(doc, "time_logs").is_empty() => "Work In Progress",
        _ if d(doc, "for_quantity") > Decimal::ZERO && d(doc, "transferred_qty") >= d(doc, "for_quantity") => "Material Transferred",
        _ => "Open",
    }
}

/// Recomputes a draft job card's status after its transferred quantity changed.
pub(crate) async fn add_transferred(hc: &mut HookCtx<'_>, card: Uuid, delta: Decimal) -> Result<()> {
    let sctx = sys(hc.ctx);
    let mut jc = hc.kernel.get_tx(&sctx, hc.tx, "JobCard", card, true).await?;
    let transferred = d(&jc, "transferred_qty") + delta;
    if transferred < Decimal::ZERO {
        return Err(Error::internal("transferred quantity would become negative"));
    }
    set(&mut jc, "transferred_qty", money(transferred));
    let mut v = Doc::new();
    v.insert("transferred_qty".into(), money(transferred));
    v.insert("status".into(), json!(status(&jc)));
    hc.kernel.set_values_tx(&sctx, hc.tx, "JobCard", card, v).await
}

fn when(r: &Doc, k: &str) -> Option<DateTime<Utc>> {
    s(r, k).and_then(|x| parse_datetime(x).ok())
}

fn minutes(secs: i64) -> Decimal {
    (Decimal::from(secs) / Decimal::from(60)).round_dp(2)
}

/// An operator: free text, or with the HR app installed an employee of the company (by id,
/// number or full name). Returns the stored value and the employee's name.
async fn resolve_employee(
    hc: &mut HookCtx<'_>,
    company: Option<Uuid>,
    text: &str,
    cache: &mut HashMap<String, (String, Option<String>)>,
) -> Result<(String, Option<String>)> {
    let text = text.trim().to_string();
    if hc.ctx.schema.entity("Employee").is_none() {
        return Ok((text, None));
    }
    if let Some(hit) = cache.get(&text) {
        return Ok(hit.clone());
    }
    let sctx = sys(hc.ctx);
    let mut found = Vec::new();
    if let Ok(eid) = text.parse::<Uuid>()
        && let Ok(e) = hc.kernel.get_tx(&sctx, hc.tx, "Employee", eid, false).await
    {
        found.push(e);
    }
    for field in ["number", "employee_name"] {
        if found.is_empty() {
            found = hc.kernel.find_tx(&sctx, hc.tx, "Employee", &[Filter::eq(field, text.clone())], None, 5).await?;
        }
    }
    found.retain(|e| company.is_none() || id(e, "company") == company);
    found.retain(|e| s(e, "status") != Some("Left"));
    let hit = match found.as_slice() {
        [] => return Err(Error::field("employee", format!("no current employee '{text}' in this company"))),
        [e] => (s(e, "number").or(s(e, "id")).unwrap_or_default().to_string(), s(e, "employee_name").map(str::to_string)),
        _ => return Err(Error::field("employee", format!("several employees match '{text}'; use the employee number"))),
    };
    cache.insert(text, hit.clone());
    Ok(hit)
}

/// The work order operation row this card works on, and its index: the row it names while that
/// row is still of its work order and operation, else the first row of its operation with work
/// left (an operation can appear twice in a routing).
fn op_row(wo: &Doc, doc: &Doc) -> Result<(Doc, usize)> {
    let ops = rows(wo, "mfo_operations");
    if ops.is_empty() {
        return Err(Error::field("work_order", "has no operations to track"));
    }
    let op = id(doc, "operation");
    if let Some(i) = s(doc, "wo_operation").and_then(|row| ops.iter().position(|r| s(r, "id") == Some(row)))
        && (op.is_none() || id(&ops[i], "operation") == op)
    {
        return Ok((ops[i].clone(), i));
    }
    let op = op.ok_or_else(|| Error::field("operation", "is required"))?;
    let matching: Vec<usize> = (0..ops.len()).filter(|i| id(&ops[*i], "operation") == Some(op)).collect();
    let open = matching.iter().copied().find(|i| d(&ops[*i], "completed_qty") < d(wo, "qty"));
    let i = open.or(matching.first().copied()).ok_or_else(|| Error::field("operation", "is not an operation of this work order"))?;
    Ok((ops[i].clone(), i))
}

/// No moment of a time log may have more jobs on the workstation than it can run: other job
/// cards' logs (drafts included) and this card's other logs each hold one lane.
async fn check_capacity(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let Some(ws) = id(doc, "workstation") else { return Ok(()) };
    let logs: Vec<(DateTime<Utc>, DateTime<Utc>, usize)> = rows(doc, "time_logs")
        .iter()
        .enumerate()
        .filter_map(|(i, r)| Some((when(r, "from_time")?, when(r, "to_time")?, i)))
        .filter(|(f, t, _)| f < t)
        .collect();
    if logs.is_empty() {
        return Ok(());
    }
    // Serializes concurrent saves on this workstation, so two cards cannot both take the last lane.
    xact_lock(hc.tx, &format!("mfo:workstation:{ws}")).await?;
    let w = get(hc.kernel, hc.ctx, hc.tx, "Workstation", ws).await?;
    let cap = capacity_of(&w);
    let lo = logs.iter().map(|l| l.0).min().unwrap_or_default();
    let hi = logs.iter().map(|l| l.1).max().unwrap_or_default();
    let table = table_of(hc.ctx, "JobCard", "time_logs")?;
    let others: Vec<(DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select l.from_time, l.to_time from {table} l join e_job_card j on j.tenant_id = l.tenant_id and j.id = l.parent_id \
         where l.tenant_id = $1 and j.workstation = $2 and j.docstatus < 2 and j.id <> $3 and l.from_time < $5 and l.to_time > $4"
    )))
    .bind(hc.ctx.tenant)
    .bind(ws)
    .bind(id(doc, "id").unwrap_or_default())
    .bind(lo)
    .bind(hi)
    .fetch_all(&mut **hc.tx)
    .await?;
    let mut errors = Vec::new();
    for (k, (from, to, row)) in logs.iter().enumerate() {
        let mut busy = others.clone();
        busy.extend(logs.iter().enumerate().filter(|(j, _)| *j != k).map(|(_, l)| (l.0, l.1)));
        if peak_overlap(&busy, *from, *to) >= cap {
            errors.push(
                FieldError::new(
                    "from_time",
                    format!("{} runs {cap} job(s) at a time and is fully booked during this time", s(&w, "workstation_name").unwrap_or("the workstation")),
                )
                .in_row("time_logs", *row),
            );
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
}

pub struct JobCardHook;

#[async_trait]
impl DocHook for JobCardHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let Some(wo_id) = id(doc, "work_order") else { return Ok(()) };
        let wo = get(hc.kernel, hc.ctx, hc.tx, "WorkOrder", wo_id).await?;
        for (k, from) in [("company", "company"), ("item", "item"), ("item_name", "item_name")] {
            set(doc, k, wo.get(from).cloned().unwrap_or(Value::Null));
        }
        let (row, i) = op_row(&wo, doc)?;
        set(doc, "wo_operation", row.get("id").cloned().unwrap_or(Value::Null));
        set(doc, "operation", row.get("operation").cloned().unwrap_or(Value::Null));
        set(doc, "sequence", json!(i + 1));
        if id(doc, "workstation").is_none() {
            set(doc, "workstation", row.get("workstation").cloned().unwrap_or(Value::Null));
        }
        if d(doc, "for_quantity") <= Decimal::ZERO {
            let open = open_qty(hc, &wo, &row, id(doc, "id")).await?;
            set(doc, "for_quantity", if open > Decimal::ZERO { money(open) } else { Value::Null });
        }
        let cid = id(doc, "company");
        let mut people = HashMap::new();
        let (operator, operator_name) = match s(doc, "employee").map(str::to_string) {
            Some(e) => {
                let (v, n) = resolve_employee(hc, cid, &e, &mut people).await?;
                (Some(v), n)
            }
            None => (None, None),
        };
        set(doc, "employee", operator.clone().map(Value::String).unwrap_or(Value::Null));
        set(doc, "employee_name", operator_name.map(Value::String).unwrap_or(Value::Null));
        let mut logs = rows(doc, "time_logs");
        let (mut secs, mut done) = (0i64, Decimal::ZERO);
        for r in logs.iter_mut() {
            match s(r, "employee").map(str::to_string) {
                Some(e) => {
                    let (v, _) = resolve_employee(hc, cid, &e, &mut people).await?;
                    set(r, "employee", v);
                }
                None => set(r, "employee", operator.clone().map(Value::String).unwrap_or(Value::Null)),
            }
            let span = match (when(r, "from_time"), when(r, "to_time")) {
                (Some(f), Some(t)) if t > f => (t - f).num_seconds(),
                _ => 0,
            };
            set(r, "time_in_mins", money(minutes(span)));
            secs += span;
            done += d(r, "completed_qty");
        }
        set(doc, "time_logs", rows_value(logs));
        set(doc, "total_time_in_mins", money(minutes(secs)));
        set(doc, "total_completed_qty", money(done));
        let rate = match id(doc, "workstation") {
            Some(ws) => d(&get(hc.kernel, hc.ctx, hc.tx, "Workstation", ws).await?, "hour_rate"),
            None => Decimal::ZERO,
        };
        let precision = match cid {
            Some(c) => company(hc.kernel, hc.ctx, hc.tx, c).await?.precision,
            None => 2,
        };
        set(doc, "hour_rate", money(rate));
        set(doc, "operating_cost", money(round(Decimal::from(secs) * rate / Decimal::from(3600), precision)));
        set(doc, "status", status(doc));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let Some(wo_id) = id(doc, "work_order") else { return Ok(()) };
        let wo = get(hc.kernel, hc.ctx, hc.tx, "WorkOrder", wo_id).await?;
        if d(&wo, "docstatus") != Decimal::ONE {
            return Err(Error::field("work_order", "must be submitted"));
        }
        if let Some(st @ ("Stopped" | "Completed" | "Cancelled")) = s(&wo, "status") {
            return Err(Error::field("work_order", format!("is {}", st.to_lowercase())));
        }
        let (row, _) = op_row(&wo, doc)?;
        let mut errors = Vec::new();
        match id(doc, "workstation") {
            None => errors.push(FieldError::new("workstation", "is required")),
            Some(ws) => {
                let w = get(hc.kernel, hc.ctx, hc.tx, "Workstation", ws).await?;
                if id(&w, "company") != id(doc, "company") {
                    errors.push(FieldError::new("workstation", "belongs to another company"));
                } else if b(&w, "disabled") {
                    errors.push(FieldError::new("workstation", "is disabled"));
                }
            }
        }
        let open = d(&wo, "qty") - d(&row, "completed_qty");
        let qty = d(doc, "for_quantity");
        if qty <= Decimal::ZERO {
            errors.push(FieldError::new("for_quantity", "nothing is left to do on this operation"));
        } else if qty > open {
            errors.push(FieldError::new("for_quantity", format!("only {} of this operation is left to complete", open.normalize())));
        }
        for (i, r) in rows(doc, "time_logs").iter().enumerate() {
            if let (Some(f), Some(t)) = (when(r, "from_time"), when(r, "to_time"))
                && t <= f
            {
                errors.push(FieldError::new("to_time", "must be after the from time").in_row("time_logs", i));
            }
        }
        if d(doc, "total_completed_qty") + d(doc, "scrap_qty") > qty && qty > Decimal::ZERO {
            errors.push(FieldError::new("total_completed_qty", "completed and lost quantities exceed the quantity to manufacture"));
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        check_capacity(hc, doc).await
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if rows(doc, "time_logs").is_empty() {
            return Err(Error::field("time_logs", "log the time spent before completing the job"));
        }
        let done = d(doc, "total_completed_qty");
        if done <= Decimal::ZERO {
            return Err(Error::field("total_completed_qty", "must be more than zero"));
        }
        let wo_id = id(doc, "work_order").ok_or_else(|| Error::field("work_order", "is required"))?;
        // Locked: job cards of one work order complete one at a time.
        let wo = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "WorkOrder", wo_id, true).await?;
        let (row, i) = op_row(&wo, doc)?;
        let total = d(&row, "completed_qty") + done;
        if total > d(&wo, "qty") {
            return Err(Error::invalid(format!("this would complete {} of the {} ordered", total.normalize(), d(&wo, "qty").normalize())));
        }
        if i > 0 {
            let prev = &rows(&wo, "mfo_operations")[i - 1];
            if total > d(prev, "completed_qty") {
                let name = get(hc.kernel, hc.ctx, hc.tx, "Operation", id(prev, "operation").unwrap_or_default()).await?;
                return Err(Error::invalid(format!(
                    "the previous operation {} has completed only {}; this operation cannot complete {}",
                    s(&name, "operation_name").unwrap_or_default(),
                    d(prev, "completed_qty").normalize(),
                    total.normalize()
                )));
            }
        }
        set(doc, "status", "Completed");
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        roll_up(hc, doc, Decimal::ONE).await
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let wo_id = id(doc, "work_order").ok_or_else(|| Error::field("work_order", "is required"))?;
        let wo = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "WorkOrder", wo_id, true).await?;
        let (row, i) = op_row(&wo, doc)?;
        let left = d(&row, "completed_qty") - d(doc, "total_completed_qty");
        let ops = rows(&wo, "mfo_operations");
        if let Some(next) = ops.get(i + 1)
            && d(next, "completed_qty") > left
        {
            return Err(Error::invalid("the next operation already completed this quantity; cancel its job cards first"));
        }
        if i + 1 == ops.len() && d(&wo, "produced_qty") > left {
            return Err(Error::invalid(format!(
                "{} were already produced from this operation's output; cancel those manufacture entries first",
                d(&wo, "produced_qty").normalize()
            )));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        roll_up(hc, doc, -Decimal::ONE).await?;
        let mut v = Doc::new();
        v.insert("status".into(), json!("Cancelled"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "JobCard", id(doc, "id").unwrap_or_default(), v).await
    }

    /// Stock entries point at their job card from an extension field (no foreign key).
    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let entries: i64 = sqlx::query_scalar("select count(*) from e_stock_entry where tenant_id = $1 and ext->>'mfo_job_card' = $2 and docstatus < 2")
            .bind(hc.ctx.tenant)
            .bind(s(doc, "id"))
            .fetch_one(&mut **hc.tx)
            .await?;
        if entries > 0 {
            return Err(Error::invalid("materials were transferred for this job card; cancel or delete those stock entries first"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "make_material_transfer" {
            return Ok(None);
        }
        let jc = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let wo = get(hc.kernel, hc.ctx, hc.tx, "WorkOrder", id(doc, "work_order").unwrap_or_default()).await?;
        let bom = get(hc.kernel, hc.ctx, hc.tx, "Bom", id(&wo, "bom").unwrap_or_default()).await?;
        let qty = match param_dec(params, "qty")? {
            Some(q) => q,
            None => d(doc, "for_quantity") - d(doc, "transferred_qty"),
        };
        if qty <= Decimal::ZERO {
            return Err(Error::field("qty", "the materials of this job card were already transferred"));
        }
        let from = param_uuid(params, "from_warehouse")?.ok_or_else(|| Error::field("from_warehouse", "is required (where the materials are now)"))?;
        let to = id(&wo, "source_warehouse").ok_or_else(|| Error::invalid("the work order has no raw material warehouse"))?;
        if from == to {
            return Err(Error::field("from_warehouse", "the materials are already in the work order's raw material warehouse"));
        }
        let op = id(doc, "operation");
        let first = d(doc, "sequence") <= Decimal::ONE;
        let per = if d(&bom, "quantity").is_zero() { Decimal::ONE } else { d(&bom, "quantity") };
        let items: Vec<Value> = rows(&bom, "items")
            .iter()
            .filter(|r| match id(r, "mfo_operation") {
                Some(o) => Some(o) == op,
                None => first,
            })
            .map(|r| {
                let need = (d(r, "qty") / per * qty * (Decimal::ONE + d(r, "scrap_percent") / Decimal::ONE_HUNDRED)).round_dp(9);
                json!({"item": r.get("item"), "qty": money(need), "source_warehouse": from.to_string(), "target_warehouse": to.to_string()})
            })
            .collect();
        if items.is_empty() {
            return Err(Error::invalid("no materials are consumed at this operation"));
        }
        let mut input = Doc::new();
        input.insert("company".into(), doc.get("company").cloned().unwrap_or(Value::Null));
        input.insert("purpose".into(), json!("Material Transfer"));
        input.insert("mfo_job_card".into(), json!(jc.to_string()));
        input.insert("mfo_job_card_qty".into(), money(qty));
        input.insert("items".into(), Value::Array(items));
        input.insert("remarks".into(), json!(format!("Materials for job card {}", s(doc, "number").unwrap_or_default())));
        let entry = create_for_user(hc, "StockEntry", input).await?;
        let entry = if submit_param(params) {
            hc.kernel.submit_tx(&elevated(hc.ctx), hc.tx, "StockEntry", id(&entry, "id").unwrap_or_default(), None).await?
        } else {
            entry
        };
        Ok(Some(created("StockEntry", &entry)))
    }
}

/// Adds (`sign` 1) or removes (-1) a submitted card's quantity, time and cost on its work order
/// operation, and the work order's actual operating cost.
async fn roll_up(hc: &mut HookCtx<'_>, doc: &Doc, sign: Decimal) -> Result<()> {
    if !installed(hc.ctx) {
        return Ok(());
    }
    let sctx = sys(hc.ctx);
    let wo_id = id(doc, "work_order").ok_or_else(|| Error::field("work_order", "is required"))?;
    let wo = hc.kernel.get_tx(&sctx, hc.tx, "WorkOrder", wo_id, true).await?;
    let (row, i) = op_row(&wo, doc)?;
    let done = d(&row, "completed_qty") + sign * d(doc, "total_completed_qty");
    let cost = d(&row, "actual_operating_cost") + sign * d(doc, "operating_cost");
    let mut v = Doc::new();
    v.insert("completed_qty".into(), money(done));
    v.insert("actual_time_in_mins".into(), money(d(&row, "actual_time_in_mins") + sign * d(doc, "total_time_in_mins")));
    v.insert("actual_operating_cost".into(), money(cost));
    let state = if done >= d(&wo, "qty") {
        "Completed"
    } else if done > Decimal::ZERO {
        "Work in Progress"
    } else {
        "Pending"
    };
    v.insert("status".into(), json!(state));
    hc.kernel.set_child_values_tx(&sctx, hc.tx, "WorkOrder", "mfo_operations", id(&row, "id").unwrap_or_default(), v).await?;
    let total: Decimal = rows(&wo, "mfo_operations").iter().enumerate().map(|(j, r)| if j == i { cost } else { d(r, "actual_operating_cost") }).sum();
    let mut h = Doc::new();
    h.insert("mfo_actual_operating_cost".into(), money(total));
    hc.kernel.set_values_tx(&sctx, hc.tx, "WorkOrder", wo_id, h).await
}
