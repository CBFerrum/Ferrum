//! Maintenance contracts: submitting a schedule plans one visit per period of each line;
//! a maintenance visit carries out exactly one planned row; cancelling the schedule drops the
//! planned visits not made yet (the ones made stay as history).

use crate::{child_table, int};
use async_trait::async_trait;
use chrono::{Duration, Months, NaiveDate};
use ferrum_domain::flow::created;
use ferrum_domain::util::{date, id, rows, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use serde_json::{Value, json};
use uuid::Uuid;

/// Upper bound of planned visits on one schedule (a child table holds at most 5,000 rows).
const MAX_PLANNED: usize = 5000;

/// Visit dates of one schedule line: the start date, then one per period while not past the end.
/// Months are added to the start date (31 Jan → 28 Feb → 31 Mar), so they never drift.
pub fn visit_dates(start: NaiveDate, end: NaiveDate, periodicity: &str) -> Vec<NaiveDate> {
    let at = |k: u32| match periodicity {
        "Weekly" => start.checked_add_signed(Duration::weeks(k.into())),
        "Monthly" => start.checked_add_months(Months::new(k)),
        "Quarterly" => start.checked_add_months(Months::new(3 * k)),
        "Half-yearly" => start.checked_add_months(Months::new(6 * k)),
        "Yearly" => start.checked_add_months(Months::new(12 * k)),
        _ => None,
    };
    (0..).map_while(|k| at(k).filter(|d| *d <= end)).take(MAX_PLANNED + 1).collect()
}

fn line_dates(line: &Doc) -> Vec<NaiveDate> {
    match (date(line, "start_date"), date(line, "end_date"), s(line, "periodicity")) {
        (Ok(a), Ok(z), Some(p)) => visit_dates(a, z, p),
        _ => vec![],
    }
}

/// Draft / Active / Completed (every planned visit made) / Cancelled.
async fn refresh_status(hc: &mut HookCtx<'_>, schedule: Uuid) -> Result<()> {
    let sctx = sys(hc.ctx);
    let doc = hc.kernel.get_tx(&sctx, hc.tx, "MaintenanceSchedule", schedule, false).await?;
    let plan = rows(&doc, "schedules");
    let status = match int(&doc, "docstatus") {
        1 if !plan.is_empty() && plan.iter().all(|r| id(r, "visit").is_some()) => "Completed",
        1 => "Active",
        2 => "Cancelled",
        _ => "Draft",
    };
    let mut v = Doc::new();
    v.insert("status".into(), json!(status));
    hc.kernel.set_values_tx(&sctx, hc.tx, "MaintenanceSchedule", schedule, v).await
}

pub struct ScheduleHook;

#[async_trait]
impl DocHook for ScheduleHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let lines: Vec<Value> = rows(doc, "items")
            .into_iter()
            .map(|mut l| {
                l.insert("no_of_visits".into(), json!(line_dates(&l).len()));
                Value::Object(l)
            })
            .collect();
        set(doc, "items", Value::Array(lines));
        if int(doc, "docstatus") == 0 {
            set(doc, "status", "Draft");
        }
        Ok(())
    }

    async fn before_submit(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let mut plan: Vec<(NaiveDate, usize, Value)> = Vec::new();
        for (i, line) in rows(doc, "items").iter().enumerate() {
            for day in line_dates(line) {
                plan.push((
                    day,
                    i,
                    json!({"id": Uuid::now_v7().to_string(), "item": line.get("item"), "item_name": line.get("item_name"), "serial_no": line.get("serial_no"),
                           "scheduled_date": day.to_string(), "assigned_to": doc.get("assigned_to"), "completion_status": "Pending"}),
                ));
            }
        }
        if plan.len() > MAX_PLANNED {
            return Err(Error::invalid(format!("this schedule would plan more than {MAX_PLANNED} visits; shorten it or use a longer periodicity")));
        }
        plan.sort_by_key(|(day, line, _)| (*day, *line));
        let rows: Vec<Value> = plan
            .into_iter()
            .enumerate()
            .map(|(idx, (_, _, mut row))| {
                row["idx"] = json!(idx);
                row
            })
            .collect();
        set(doc, "schedules", Value::Array(rows));
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        refresh_status(hc, id(doc, "id").ok_or_else(|| Error::internal("schedule without id"))?).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let schedule = id(doc, "id").ok_or_else(|| Error::internal("schedule without id"))?;
        let plan = child_table(hc.ctx, "MaintenanceSchedule", "schedules")?;
        sqlx::query(sqlx::AssertSqlSafe(format!("delete from {plan} where tenant_id = $1 and parent_id = $2 and visit is null")))
            .bind(hc.ctx.tenant)
            .bind(schedule)
            .execute(&mut **hc.tx)
            .await?;
        refresh_status(hc, schedule).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "make_visit" {
            return Ok(None);
        }
        let pending: Vec<Doc> = rows(doc, "schedules").into_iter().filter(|r| id(r, "visit").is_none()).collect();
        let row = match params.get("schedule_detail").and_then(Value::as_str) {
            Some(wanted) => pending
                .iter()
                .find(|r| s(r, "id") == Some(wanted))
                .ok_or_else(|| Error::field("schedule_detail", "is not a pending planned visit of this schedule"))?,
            None => pending.first().ok_or_else(|| Error::invalid("every planned visit has been made"))?,
        };
        let mut input = Doc::new();
        input.insert("customer".into(), doc.get("customer").cloned().unwrap_or(Value::Null));
        input.insert("maintenance_schedule".into(), doc.get("id").cloned().unwrap_or(Value::Null));
        input.insert("schedule_detail".into(), row.get("id").cloned().unwrap_or(Value::Null));
        input.insert(
            "work_done".into(),
            json!([{"item": row.get("item"), "serial_no": row.get("serial_no"), "service_person": row.get("assigned_to"),
                    "work_done": format!("Planned maintenance due {}", s(row, "scheduled_date").unwrap_or_default())}]),
        );
        let visit = hc.kernel.insert_tx(hc.ctx, hc.tx, "MaintenanceVisit", input).await?;
        Ok(Some(created("MaintenanceVisit", &visit)))
    }
}

/// The planned row a visit carries out. Refuses unsubmitted schedules, other customers' schedules,
/// unknown rows and rows another visit already carried out.
fn planned_row(schedule: &Doc, visit: &Doc) -> Result<Doc> {
    if int(schedule, "docstatus") != 1 {
        return Err(Error::field("maintenance_schedule", "is not an active (submitted) maintenance schedule"));
    }
    if id(schedule, "customer") != id(visit, "customer") {
        return Err(Error::field("customer", "differs from the customer of the maintenance schedule"));
    }
    let wanted = s(visit, "schedule_detail").ok_or_else(|| Error::field("schedule_detail", "choose the planned visit this visit carries out"))?;
    let row = rows(schedule, "schedules")
        .into_iter()
        .find(|r| s(r, "id") == Some(wanted))
        .ok_or_else(|| Error::field("schedule_detail", "is not a planned visit of this schedule"))?;
    if id(&row, "visit").is_some_and(|v| Some(v) != id(visit, "id")) {
        return Err(Error::field("schedule_detail", "this planned visit was already carried out by another maintenance visit"));
    }
    Ok(row)
}

pub struct VisitHook;

#[async_trait]
impl DocHook for VisitHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if id(doc, "maintenance_schedule").is_some() {
            set(doc, "maintenance_type", "Scheduled");
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let Some(schedule) = id(doc, "maintenance_schedule") {
            let sched = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "MaintenanceSchedule", schedule, false).await?;
            planned_row(&sched, doc)?;
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let Some(schedule) = id(doc, "maintenance_schedule") else { return Ok(()) };
        // Locked: of two visits submitted at once for the same row, the second sees the first.
        let sched = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "MaintenanceSchedule", schedule, true).await?;
        let row = id(&planned_row(&sched, doc)?, "id").ok_or_else(|| Error::internal("planned visit without id"))?;
        let mut v = Doc::new();
        v.insert("visit".into(), doc.get("id").cloned().unwrap_or(Value::Null));
        v.insert("actual_date".into(), doc.get("visit_date").cloned().unwrap_or(Value::Null));
        v.insert("completion_status".into(), doc.get("completion_status").cloned().unwrap_or(Value::Null));
        hc.kernel.set_child_values_tx(&sys(hc.ctx), hc.tx, "MaintenanceSchedule", "schedules", row, v).await?;
        refresh_status(hc, schedule).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let (Some(schedule), Some(row)) = (id(doc, "maintenance_schedule"), id(doc, "schedule_detail")) else { return Ok(()) };
        let sched = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "MaintenanceSchedule", schedule, true).await?;
        if !rows(&sched, "schedules").iter().any(|r| id(r, "id") == Some(row) && id(r, "visit") == id(doc, "id")) {
            return Ok(());
        }
        if int(&sched, "docstatus") == 2 {
            // The contract is cancelled: an unmade visit is no longer planned.
            let plan = child_table(hc.ctx, "MaintenanceSchedule", "schedules")?;
            sqlx::query(sqlx::AssertSqlSafe(format!("delete from {plan} where tenant_id = $1 and id = $2")))
                .bind(hc.ctx.tenant)
                .bind(row)
                .execute(&mut **hc.tx)
                .await?;
        } else {
            let mut v = Doc::new();
            v.insert("visit".into(), Value::Null);
            v.insert("actual_date".into(), Value::Null);
            v.insert("completion_status".into(), json!("Pending"));
            hc.kernel.set_child_values_tx(&sys(hc.ctx), hc.tx, "MaintenanceSchedule", "schedules", row, v).await?;
        }
        refresh_status(hc, schedule).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn dates(start: &str, end: &str, p: &str) -> Vec<String> {
        visit_dates(day(start), day(end), p).iter().map(|d| d.to_string()).collect()
    }

    #[test]
    fn visit_plans() {
        assert_eq!(dates("2026-01-31", "2026-04-30", "Monthly"), ["2026-01-31", "2026-02-28", "2026-03-31", "2026-04-30"], "month ends do not drift");
        assert_eq!(dates("2026-01-01", "2026-12-31", "Monthly").len(), 12);
        assert_eq!(dates("2026-01-01", "2026-12-31", "Quarterly"), ["2026-01-01", "2026-04-01", "2026-07-01", "2026-10-01"]);
        assert_eq!(dates("2026-01-01", "2027-12-31", "Half-yearly").len(), 4);
        assert_eq!(dates("2024-02-29", "2027-03-01", "Yearly"), ["2024-02-29", "2025-02-28", "2026-02-28", "2027-02-28"]);
        assert_eq!(dates("2026-11-02", "2026-11-30", "Weekly"), ["2026-11-02", "2026-11-09", "2026-11-16", "2026-11-23", "2026-11-30"]);
        assert_eq!(dates("2026-05-01", "2026-05-01", "Yearly"), ["2026-05-01"], "a one-day contract still has its visit");
        assert!(dates("2026-05-02", "2026-05-01", "Monthly").is_empty());
        assert!(dates("2026-05-01", "2026-12-01", "Daily").is_empty());
        assert_eq!(visit_dates(day("2000-01-01"), day("2199-01-01"), "Weekly").len(), MAX_PLANNED + 1, "capped, so the caller can refuse");
    }
}
