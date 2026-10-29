//! Work order operations: copied from the BOM (or its routing), timed and costed for the order
//! quantity, forward-scheduled over the workstations' working hours and capacity on submit, and
//! tracked through job cards.

use crate::bom::{per_batch, workstation};
use crate::calendar::{find_slot, op_minutes, secs_of};
use crate::masters::{calendar_of, capacity_of, op_defaults};
use crate::production_plan::refresh_plan;
use crate::{installed, rows_value, table_of, xact_lock};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{b, company, d, date, elevated, get, id, money, round, rows, s, set, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

pub(crate) fn tz(ctx: &Ctx) -> Tz {
    ctx.schema.timezone.parse().unwrap_or(chrono_tz::UTC)
}

/// A local time as stored (RFC 3339 UTC). In a DST gap the next valid instant is used.
pub(crate) fn to_utc(zone: Tz, t: NaiveDateTime) -> DateTime<Utc> {
    let local = zone.from_local_datetime(&t).earliest().or_else(|| zone.from_local_datetime(&(t + chrono::Duration::hours(1))).earliest());
    local.map(|l| l.with_timezone(&Utc)).unwrap_or_else(|| t.and_utc())
}

pub(crate) fn stamp(t: DateTime<Utc>) -> Value {
    json!(t.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Operation rows for a work order from its BOM: the BOM's operations, else its routing's.
async fn bom_operations(hc: &mut HookCtx<'_>, bom: Uuid) -> Result<Vec<Doc>> {
    let bdoc = get(hc.kernel, hc.ctx, hc.tx, "Bom", bom).await?;
    let mut src = rows(&bdoc, "mfo_operations");
    if src.is_empty()
        && let Some(r) = id(&bdoc, "mfo_routing")
    {
        src = rows(&get(hc.kernel, hc.ctx, hc.tx, "Routing", r).await?, "operations");
    }
    Ok(src
        .iter()
        .map(|r| {
            let mut row = Doc::new();
            for (from, to) in [
                ("operation", "operation"),
                ("workstation", "workstation"),
                ("time_basis", "time_basis"),
                ("time_in_mins", "unit_time_in_mins"),
                ("batch_size", "batch_size"),
                ("setup_time", "setup_time"),
            ] {
                if let Some(v) = r.get(from).filter(|v| !v.is_null()) {
                    row.insert(to.into(), v.clone());
                }
            }
            row
        })
        .collect())
}

/// Busy intervals (local time) of other submitted, open work orders' operations on a workstation.
async fn busy_on(hc: &mut HookCtx<'_>, zone: Tz, ws: Uuid, wo: Option<Uuid>, after: DateTime<Utc>) -> Result<Vec<(NaiveDateTime, NaiveDateTime)>> {
    let table = table_of(hc.ctx, "WorkOrder", "mfo_operations")?;
    let rows: Vec<(DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select o.planned_start_time, o.planned_end_time from {table} o join e_work_order w on w.tenant_id = o.tenant_id and w.id = o.parent_id \
         where o.tenant_id = $1 and o.workstation = $2 and w.docstatus = 1 and w.status in ('Not Started', 'In Process') and w.id <> $3 \
         and coalesce(o.status, 'Pending') <> 'Completed' and o.planned_start_time is not null and o.planned_end_time > $4"
    )))
    .bind(hc.ctx.tenant)
    .bind(ws)
    .bind(wo.unwrap_or_default())
    .bind(after)
    .fetch_all(&mut **hc.tx)
    .await?;
    Ok(rows.into_iter().map(|(a, b)| (a.with_timezone(&zone).naive_local(), b.with_timezone(&zone).naive_local())).collect())
}

/// Forward scheduling: each operation starts when the previous one ends (the first at the
/// planned start date), at the earliest moment its workstation has a free lane, and runs over
/// the workstation's working hours; the order's expected end date becomes the last operation's
/// end. Callers serialize per company so lanes are not double-booked.
async fn schedule(hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
    let zone = tz(hc.ctx);
    let start = date(doc, "planned_start_date")?.and_hms_opt(0, 0, 0).unwrap_or_default();
    let after = to_utc(zone, start);
    let wo = id(doc, "id");
    let mut ops = rows(doc, "mfo_operations");
    let mut ws_cache = HashMap::new();
    let mut busy: HashMap<Uuid, Vec<(NaiveDateTime, NaiveDateTime)>> = HashMap::new();
    let mut cursor = start;
    for (i, r) in ops.iter_mut().enumerate() {
        let ws = id(r, "workstation").ok_or_else(|| Error::Validation(vec![FieldError::new("workstation", "is required").in_row("mfo_operations", i)]))?;
        let w = workstation(hc, &mut ws_cache, ws).await?;
        if let std::collections::hash_map::Entry::Vacant(e) = busy.entry(ws) {
            e.insert(busy_on(hc, zone, ws, wo, after).await?);
        }
        let lanes = busy.get_mut(&ws).expect("loaded");
        let (from, to) = find_slot(&calendar_of(&w), lanes, capacity_of(&w), cursor, secs_of(d(r, "time_in_mins"))).ok_or_else(|| {
            Error::invalid(format!("workstation {} has no free working time within three years", s(&w, "workstation_name").unwrap_or_default()))
        })?;
        lanes.push((from, to));
        set(r, "planned_start_time", stamp(to_utc(zone, from)));
        set(r, "planned_end_time", stamp(to_utc(zone, to)));
        cursor = to;
    }
    if !ops.is_empty() {
        set(doc, "expected_end_date", cursor.date().to_string());
    }
    set(doc, "mfo_operations", rows_value(ops));
    Ok(())
}

/// Quantity of a work order operation neither completed nor on a draft job card.
pub(crate) async fn open_qty(hc: &mut HookCtx<'_>, wo: &Doc, op_row: &Doc, except_card: Option<Uuid>) -> Result<Decimal> {
    let drafts: Decimal = sqlx::query_scalar(
        "select coalesce(sum(for_quantity), 0) from e_job_card where tenant_id = $1 and work_order = $2 and wo_operation = $3 and docstatus = 0 and id <> $4",
    )
    .bind(hc.ctx.tenant)
    .bind(id(wo, "id"))
    .bind(s(op_row, "id"))
    .bind(except_card.unwrap_or_default())
    .fetch_one(&mut **hc.tx)
    .await?;
    Ok((d(wo, "qty") - d(op_row, "completed_qty") - drafts).max(Decimal::ZERO))
}

/// Draft job cards for every operation with an open quantity.
async fn make_job_cards(hc: &mut HookCtx<'_>, wo: &Doc, as_user: bool) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for r in rows(wo, "mfo_operations") {
        let open = open_qty(hc, wo, &r, None).await?;
        if open <= Decimal::ZERO {
            continue;
        }
        let mut input = Doc::new();
        input.insert("company".into(), wo.get("company").cloned().unwrap_or(Value::Null));
        input.insert("work_order".into(), wo.get("id").cloned().unwrap_or(Value::Null));
        input.insert("operation".into(), r.get("operation").cloned().unwrap_or(Value::Null));
        input.insert("wo_operation".into(), r.get("id").cloned().unwrap_or(Value::Null));
        input.insert("workstation".into(), r.get("workstation").cloned().unwrap_or(Value::Null));
        input.insert("for_quantity".into(), money(open));
        let jc = if as_user { create_for_user(hc, "JobCard", input).await? } else { hc.kernel.insert_tx(&elevated(hc.ctx), hc.tx, "JobCard", input).await? };
        out.push(created("JobCard", &jc));
    }
    Ok(out)
}

/// Operating cost to absorb for `fg_qty` finished units: per operation its actual cost per
/// completed unit, or while nothing is completed its planned cost per ordered unit.
pub(crate) fn operating_cost_for(wo: &Doc, fg_qty: Decimal, precision: u32) -> Decimal {
    let ordered = d(wo, "qty");
    let per_unit: Decimal = rows(wo, "mfo_operations")
        .iter()
        .map(|r| {
            let done = d(r, "completed_qty");
            if done > Decimal::ZERO {
                d(r, "actual_operating_cost") / done
            } else if ordered > Decimal::ZERO {
                d(r, "planned_operating_cost") / ordered
            } else {
                Decimal::ZERO
            }
        })
        .sum();
    round(per_unit * fg_qty, precision)
}

/// With operations tracked, finished goods can only be made from what the last operation completed.
pub(crate) fn last_completed(wo: &Doc) -> Option<Decimal> {
    rows(wo, "mfo_operations").last().map(|r| d(r, "completed_qty"))
}

pub struct WorkOrderOperationsHook;

#[async_trait]
impl DocHook for WorkOrderOperationsHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) || !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let mut ops = rows(doc, "mfo_operations");
        if let Some(bom) = id(doc, "bom") {
            // Copy on creation and when the BOM changes; operations typed by hand stay.
            let copied_from = s(doc, "mfo_operations_bom").map(str::to_string);
            if ops.is_empty() || copied_from.is_some_and(|c| c != bom.to_string()) {
                ops = bom_operations(hc, bom).await?;
                set(doc, "mfo_operations_bom", bom.to_string());
            }
        }
        op_defaults(hc, &mut ops, &mut HashMap::new()).await?;
        let precision = match id(doc, "company") {
            Some(c) => company(hc.kernel, hc.ctx, hc.tx, c).await?.precision,
            None => 2,
        };
        let qty = d(doc, "qty");
        let mut cache = HashMap::new();
        let mut planned = Decimal::ZERO;
        for r in ops.iter_mut() {
            let rate = match id(r, "workstation") {
                Some(ws) => d(&workstation(hc, &mut cache, ws).await?, "hour_rate"),
                None => Decimal::ZERO,
            };
            let minutes = op_minutes(qty, per_batch(r), d(r, "unit_time_in_mins"), d(r, "batch_size"), d(r, "setup_time")).round_dp(2);
            let cost = round(minutes * rate / Decimal::from(60), precision);
            set(r, "time_in_mins", money(minutes));
            set(r, "hour_rate", money(rate));
            set(r, "planned_operating_cost", money(cost));
            set(r, "status", "Pending");
            planned += cost;
        }
        set(doc, "mfo_planned_operating_cost", if ops.is_empty() { Value::Null } else { money(planned) });
        set(doc, "mfo_operations", rows_value(ops));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) || !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let mut errors = Vec::new();
        let mut cache = HashMap::new();
        for (i, r) in rows(doc, "mfo_operations").iter().enumerate() {
            let Some(ws) = id(r, "workstation") else {
                errors.push(FieldError::new("workstation", "is required (the operation has no default workstation)").in_row("mfo_operations", i));
                continue;
            };
            let w = workstation(hc, &mut cache, ws).await?;
            if id(&w, "company") != id(doc, "company") {
                errors.push(FieldError::new("workstation", "belongs to another company").in_row("mfo_operations", i));
            } else if b(&w, "disabled") {
                errors.push(FieldError::new("workstation", "is disabled").in_row("mfo_operations", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) || rows(doc, "mfo_operations").is_empty() {
            return Ok(());
        }
        xact_lock(hc.tx, &format!("mfo:schedule:{}", s(doc, "company").unwrap_or_default())).await?;
        schedule(hc, doc).await
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        if !rows(doc, "mfo_operations").is_empty() {
            make_job_cards(hc, doc, false).await?;
        }
        if let Some(plan) = id(doc, "mfo_production_plan") {
            refresh_plan(hc, plan, None).await?;
        }
        Ok(())
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let submitted: i64 = sqlx::query_scalar("select count(*) from e_job_card where tenant_id = $1 and work_order = $2 and docstatus = 1")
            .bind(hc.ctx.tenant)
            .bind(id(doc, "id"))
            .fetch_one(&mut **hc.tx)
            .await?;
        if submitted > 0 {
            return Err(Error::invalid(format!("{submitted} job card(s) were completed against this work order; cancel them first")));
        }
        Ok(())
    }

    /// Unused draft job cards go with the work order (ones holding transferred materials stay).
    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let drafts: Vec<Uuid> =
            sqlx::query_scalar("select id from e_job_card where tenant_id = $1 and work_order = $2 and docstatus = 0 and coalesce(transferred_qty, 0) = 0")
                .bind(hc.ctx.tenant)
                .bind(id(doc, "id"))
                .fetch_all(&mut **hc.tx)
                .await?;
        let sctx = sys(hc.ctx);
        for jc in drafts {
            hc.kernel.delete_tx(&sctx, hc.tx, "JobCard", jc, None).await?;
        }
        if let Some(plan) = id(doc, "mfo_production_plan") {
            refresh_plan(hc, plan, None).await?;
        }
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        if installed(hc.ctx)
            && let Some(plan) = id(doc, "mfo_production_plan")
        {
            refresh_plan(hc, plan, None).await?;
        }
        Ok(())
    }

    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if installed(hc.ctx)
            && let Some(plan) = id(doc, "mfo_production_plan")
        {
            refresh_plan(hc, plan, id(doc, "id")).await?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "mfo_make_job_cards" {
            return Ok(None);
        }
        if !matches!(s(doc, "status"), Some("Not Started" | "In Process")) {
            return Err(Error::invalid("job cards can only be made for a work order that is not started or in process"));
        }
        let made = make_job_cards(hc, doc, true).await?;
        if made.is_empty() {
            return Err(Error::invalid("every operation is already completed or on a draft job card"));
        }
        Ok(Some(json!({"created": made})))
    }
}
