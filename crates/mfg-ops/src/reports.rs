//! Shop floor and subcontracting reports: job card time, cost and efficiency; workstation load
//! against capacity per day; production plan progress; work order operation progress; and
//! materials and goods per subcontracting order.

use crate::masters::{calendar_of, capacity_of};
use crate::table_of;
use crate::work_order::tz;
use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use ferrum_domain::reports::{Params, ReportCompany, ReportSource, col, param_def, period, read_tx, resolve_company, today};
use ferrum_domain::util::{money, round, sys};
use ferrum_kernel::{Ctx, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

const MAX_ROWS: i64 = 10_000;
const MAX_DAYS: i64 = 366;

pub struct MfgOpsReports;

struct Def {
    name: &'static str,
    label: &'static str,
    module: &'static str,
    entity: &'static str,
    params: &'static [&'static str],
}

const DEFS: &[Def] = &[
    Def {
        name: "job_card_summary",
        label: "Job Card Summary",
        module: "shop_floor",
        entity: "JobCard",
        params: &["company", "from_date", "to_date", "group_by", "workstation"],
    },
    Def {
        name: "capacity_planning",
        label: "Capacity Planning",
        module: "shop_floor",
        entity: "Workstation",
        params: &["company", "start_date", "end_date", "workstation"],
    },
    Def {
        name: "work_order_operations",
        label: "Work Order Operations Progress",
        module: "shop_floor",
        entity: "WorkOrder",
        params: &["company", "work_order", "workstation", "wo_status"],
    },
    Def {
        name: "production_plan_status",
        label: "Production Plan Status",
        module: "shop_floor",
        entity: "ProductionPlan",
        params: &["company", "from_date", "to_date", "production_plan"],
    },
    Def {
        name: "subcontracting_status",
        label: "Subcontracting Status",
        module: "subcontracting",
        entity: "SubcontractingOrder",
        params: &["company", "from_date", "to_date", "supplier", "subcontracting_order"],
    },
];

fn param(name: &str) -> Value {
    let link = |label: &str, to: &str| json!({"name": name, "label": label, "type": "link", "to": to});
    match name {
        "workstation" => link("Workstation", "Workstation"),
        "work_order" => link("Work Order", "WorkOrder"),
        "production_plan" => link("Production Plan", "ProductionPlan"),
        "subcontracting_order" => link("Subcontracting Order", "SubcontractingOrder"),
        "start_date" => json!({"name": name, "label": "From Date", "type": "date", "default": "today"}),
        "end_date" => json!({"name": name, "label": "To Date", "type": "date"}),
        "group_by" => {
            json!({"name": name, "label": "Group By", "type": "select", "options": ["Workstation", "Operator", "Operation"], "default": "Workstation"})
        }
        "wo_status" => json!({"name": name, "label": "Work Order Status", "type": "select", "options": ["Not Started", "In Process", "Completed", "Stopped"]}),
        other => param_def(other),
    }
}

fn pct(part: Decimal, whole: Decimal) -> Decimal {
    if whole.is_zero() { Decimal::ZERO } else { (part / whole * Decimal::ONE_HUNDRED).round_dp(2) }
}

fn mins(secs: i64) -> Decimal {
    (Decimal::from(secs) / Decimal::from(60)).round_dp(2)
}

#[async_trait]
impl ReportSource for MfgOpsReports {
    fn catalog(&self) -> Vec<Value> {
        DEFS.iter()
            .map(|d| json!({"name": d.name, "label": d.label, "module": d.module, "entity": d.entity, "params": d.params.iter().map(|p| param(p)).collect::<Vec<_>>()}))
            .collect()
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let mut out = match name {
            "job_card_summary" => job_card_summary(&mut tx, ctx, &co, &p).await?,
            "capacity_planning" => capacity_planning(k, &mut tx, ctx, &co, &p).await?,
            "work_order_operations" => work_order_operations(&mut tx, ctx, &co, &p).await?,
            "production_plan_status" => production_plan_status(&mut tx, ctx, &co, &p).await?,
            "subcontracting_status" => subcontracting_status(&mut tx, ctx, &co, &p).await?,
            _ => return Err(Error::NotFound(format!("report {name}"))),
        };
        tx.rollback().await?;
        if let Value::Object(m) = &mut out {
            let meta = m.entry("meta").or_insert_with(|| json!({}));
            meta["company"] = json!(co.name);
            meta["currency"] = json!(co.currency);
        }
        Ok(out)
    }
}

#[derive(sqlx::FromRow)]
struct LogRow {
    card: Uuid,
    workstation: Option<String>,
    operation: Option<String>,
    operator: String,
    minutes: Decimal,
    completed: Decimal,
    hour_rate: Decimal,
    planned_per_unit: Decimal,
}

#[derive(Default)]
struct Group {
    cards: std::collections::BTreeSet<Uuid>,
    completed: Decimal,
    actual: Decimal,
    planned: Decimal,
    cost: Decimal,
}

/// Time, quantity, cost and efficiency (standard minutes for the quantity completed over the
/// minutes logged) of completed job cards, per workstation, operator or operation.
async fn job_card_summary(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let by = p.str("group_by").unwrap_or("Workstation");
    let logs = table_of(ctx, "JobCard", "time_logs")?;
    let ops = table_of(ctx, "WorkOrder", "mfo_operations")?;
    let data: Vec<LogRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select j.id as card, ws.workstation_name as workstation, op.operation_name as operation, \
                coalesce(nullif(l.employee, ''), nullif(j.employee, ''), '(no operator)') as operator, \
                coalesce(l.time_in_mins, 0) as minutes, coalesce(l.completed_qty, 0) as completed, coalesce(j.hour_rate, 0) as hour_rate, \
                coalesce(wop.time_in_mins / nullif(wo.qty, 0), 0) as planned_per_unit \
         from {logs} l join e_job_card j on j.tenant_id = l.tenant_id and j.id = l.parent_id \
         left join e_workstation ws on ws.tenant_id = j.tenant_id and ws.id = j.workstation \
         left join e_operation op on op.tenant_id = j.tenant_id and op.id = j.operation \
         left join e_work_order wo on wo.tenant_id = j.tenant_id and wo.id = j.work_order \
         left join {ops} wop on wop.tenant_id = j.tenant_id and wop.id::text = j.wo_operation \
         where l.tenant_id = $1 and j.company = $2 and j.docstatus = 1 and j.posting_date between $3 and $4 \
         and ($5::uuid is null or j.workstation = $5) limit $6"
    )))
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(p.uuid("workstation")?)
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for r in data.iter().take(MAX_ROWS as usize) {
        let key = match by {
            "Operator" => r.operator.clone(),
            "Operation" => r.operation.clone().unwrap_or_default(),
            _ => r.workstation.clone().unwrap_or_default(),
        };
        let g = groups.entry(key).or_default();
        g.cards.insert(r.card);
        g.completed += r.completed;
        g.actual += r.minutes;
        g.planned += r.completed * r.planned_per_unit;
        g.cost += r.minutes * r.hour_rate / Decimal::from(60);
    }
    let mut total = Group::default();
    let rows: Vec<Value> = groups
        .into_iter()
        .map(|(k, g)| {
            total.cards.extend(g.cards.iter().copied());
            total.completed += g.completed;
            total.actual += g.actual;
            total.planned += g.planned;
            total.cost += g.cost;
            json!({"group": k, "job_cards": g.cards.len(), "completed_qty": money(g.completed), "actual_mins": money(g.actual.round_dp(2)),
                   "planned_mins": money(g.planned.round_dp(2)), "efficiency": money(pct(g.planned, g.actual)), "operating_cost": money(round(g.cost, co.precision))})
        })
        .collect();
    Ok(json!({
        "columns": [col("group", by, "data"), col("job_cards", "Job Cards", "int"), col("completed_qty", "Completed Qty", "decimal"),
            col("actual_mins", "Actual Minutes", "decimal"), col("planned_mins", "Standard Minutes", "decimal"), col("efficiency", "Efficiency %", "percent"),
            col("operating_cost", "Operating Cost", "currency")],
        "rows": rows,
        "totals": {"job_cards": total.cards.len(), "completed_qty": money(total.completed), "actual_mins": money(total.actual.round_dp(2)),
                   "planned_mins": money(total.planned.round_dp(2)), "efficiency": money(pct(total.planned, total.actual)), "operating_cost": money(round(total.cost, co.precision))},
        "meta": {"from_date": from, "to_date": to, "group_by": by, "truncated": truncated},
    }))
}

/// Per workstation and day: working minutes × parallel jobs (capacity) against the working
/// minutes scheduled operations of open work orders occupy.
async fn capacity_planning(k: &Kernel, tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let from = p.date("start_date")?.unwrap_or_else(today);
    let to = p.date("end_date")?.unwrap_or(from + Duration::days(13));
    if to < from {
        return Err(Error::field("end_date", "must not be before the from date"));
    }
    if (to - from).num_days() >= MAX_DAYS {
        return Err(Error::field("end_date", format!("show at most {MAX_DAYS} days")));
    }
    let zone = tz(ctx);
    let (start, end) = (from.and_hms_opt(0, 0, 0).unwrap_or_default(), (to + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap_or_default());
    let stations: Vec<Uuid> = sqlx::query_scalar(
        "select id from e_workstation where tenant_id = $1 and company = $2 and not coalesce(disabled, false) and ($3::uuid is null or id = $3) \
         order by workstation_name",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(p.uuid("workstation")?)
    .fetch_all(&mut **tx)
    .await?;
    let ops = table_of(ctx, "WorkOrder", "mfo_operations")?;
    let jobs: Vec<(Uuid, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select o.workstation, o.planned_start_time, o.planned_end_time from {ops} o join e_work_order w on w.tenant_id = o.tenant_id and w.id = o.parent_id \
         where o.tenant_id = $1 and w.company = $2 and w.docstatus = 1 and w.status in ('Not Started', 'In Process') \
         and coalesce(o.status, 'Pending') <> 'Completed' and o.planned_start_time < $4 and o.planned_end_time > $3"
    )))
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(crate::work_order::to_utc(zone, start))
    .bind(crate::work_order::to_utc(zone, end))
    .fetch_all(&mut **tx)
    .await?;
    let mut by_station: HashMap<Uuid, Vec<(NaiveDateTime, NaiveDateTime)>> = HashMap::new();
    for (ws, a, b) in jobs {
        by_station.entry(ws).or_default().push((a.with_timezone(&zone).naive_local(), b.with_timezone(&zone).naive_local()));
    }
    let sctx = sys(ctx);
    let mut rows = Vec::new();
    let (mut tc, mut tl) = (0i64, 0i64);
    for ws in stations {
        let w = k.get_tx(&sctx, tx, "Workstation", ws, false).await?;
        let cal = calendar_of(&w);
        let lanes = capacity_of(&w) as i64;
        let name = w.get("workstation_name").cloned().unwrap_or(Value::Null);
        let jobs = by_station.remove(&ws).unwrap_or_default();
        let mut day: NaiveDate = from;
        while day <= to {
            let (d0, d1) = (day.and_hms_opt(0, 0, 0).unwrap_or_default(), (day + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap_or_default());
            let capacity = cal.working_secs(d0, d1) * lanes;
            let load: i64 = jobs.iter().map(|(a, b)| cal.working_secs((*a).max(d0), (*b).min(d1))).sum();
            if capacity > 0 || load > 0 {
                tc += capacity;
                tl += load;
                rows.push(json!({"workstation": name, "date": day, "capacity_mins": money(mins(capacity)), "load_mins": money(mins(load)),
                                 "free_mins": money(mins(capacity - load)), "utilization": money(pct(Decimal::from(load), Decimal::from(capacity)))}));
            }
            day += Duration::days(1);
        }
    }
    Ok(json!({
        "columns": [col("workstation", "Workstation", "data"), col("date", "Date", "date"), col("capacity_mins", "Capacity (Minutes)", "decimal"),
            col("load_mins", "Planned Load (Minutes)", "decimal"), col("free_mins", "Free (Minutes)", "decimal"), col("utilization", "Utilization %", "percent")],
        "rows": rows,
        "totals": {"capacity_mins": money(mins(tc)), "load_mins": money(mins(tl)), "free_mins": money(mins(tc - tl)),
                   "utilization": money(pct(Decimal::from(tl), Decimal::from(tc)))},
        "meta": {"from_date": from, "to_date": to},
    }))
}

#[derive(sqlx::FromRow)]
struct OpRow {
    number: Option<String>,
    item_name: Option<String>,
    wo_status: Option<String>,
    qty: Decimal,
    operation: Option<String>,
    workstation: Option<String>,
    planned_mins: Option<Decimal>,
    actual_mins: Option<Decimal>,
    completed: Option<Decimal>,
    planned_start: Option<DateTime<Utc>>,
    planned_end: Option<DateTime<Utc>>,
    status: Option<String>,
    planned_cost: Option<Decimal>,
    actual_cost: Option<Decimal>,
}

async fn work_order_operations(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let ops = table_of(ctx, "WorkOrder", "mfo_operations")?;
    let data: Vec<OpRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select w.number, w.item_name, w.status as wo_status, w.qty, op.operation_name as operation, ws.workstation_name as workstation, \
                o.time_in_mins as planned_mins, o.actual_time_in_mins as actual_mins, o.completed_qty as completed, \
                o.planned_start_time as planned_start, o.planned_end_time as planned_end, o.status, \
                o.planned_operating_cost as planned_cost, o.actual_operating_cost as actual_cost \
         from {ops} o join e_work_order w on w.tenant_id = o.tenant_id and w.id = o.parent_id \
         left join e_operation op on op.tenant_id = o.tenant_id and op.id = o.operation \
         left join e_workstation ws on ws.tenant_id = o.tenant_id and ws.id = o.workstation \
         where o.tenant_id = $1 and w.company = $2 and w.docstatus = 1 and ($3::uuid is null or w.id = $3) \
         and ($4::uuid is null or o.workstation = $4) and ($5::text is null or w.status = $5) \
         order by w.planned_start_date, w.number, o.idx limit $6"
    )))
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(p.uuid("work_order")?)
    .bind(p.uuid("workstation")?)
    .bind(p.str("wo_status"))
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let (mut tp, mut ta, mut tpc, mut tac) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|r| {
            let done = r.completed.unwrap_or_default();
            tp += r.planned_mins.unwrap_or_default();
            ta += r.actual_mins.unwrap_or_default();
            tpc += r.planned_cost.unwrap_or_default();
            tac += r.actual_cost.unwrap_or_default();
            json!({"work_order": r.number, "item_name": r.item_name, "wo_status": r.wo_status, "operation": r.operation, "workstation": r.workstation,
                   "qty": money(r.qty), "completed_qty": money(done), "progress": money(pct(done, r.qty)),
                   "planned_mins": money(r.planned_mins.unwrap_or_default()), "actual_mins": money(r.actual_mins.unwrap_or_default()),
                   "planned_start": r.planned_start, "planned_end": r.planned_end, "status": r.status,
                   "planned_cost": money(r.planned_cost.unwrap_or_default()), "actual_cost": money(r.actual_cost.unwrap_or_default())})
        })
        .collect();
    Ok(json!({
        "columns": [col("work_order", "Work Order", "data"), col("item_name", "Item", "data"), col("wo_status", "Order Status", "data"),
            col("operation", "Operation", "data"), col("workstation", "Workstation", "data"), col("qty", "Qty", "decimal"),
            col("completed_qty", "Completed", "decimal"), col("progress", "Progress %", "percent"), col("planned_mins", "Planned Minutes", "decimal"),
            col("actual_mins", "Actual Minutes", "decimal"), col("planned_start", "Planned Start", "datetime"), col("planned_end", "Planned End", "datetime"),
            col("status", "Status", "data"), col("planned_cost", "Planned Cost", "currency"), col("actual_cost", "Actual Cost", "currency")],
        "rows": rows,
        "totals": {"planned_mins": money(tp), "actual_mins": money(ta), "planned_cost": money(tpc), "actual_cost": money(tac)},
        "meta": {"truncated": truncated},
    }))
}

#[derive(sqlx::FromRow)]
struct PlanRow {
    number: Option<String>,
    status: Option<String>,
    posting_date: NaiveDate,
    kind: String,
    item_code: Option<String>,
    item_name: Option<String>,
    planned: Option<Decimal>,
    ordered: Option<Decimal>,
    produced: Option<Decimal>,
}

async fn production_plan_status(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let items = table_of(ctx, "ProductionPlan", "po_items")?;
    let subs = table_of(ctx, "ProductionPlan", "sub_assemblies")?;
    let data: Vec<PlanRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select * from ( \
           select p.number, p.status, p.posting_date, 'Item' as kind, it.item_code, it.item_name, i.planned_qty as planned, i.ordered_qty as ordered, \
                  i.produced_qty as produced, i.idx from {items} i join e_production_plan p on p.tenant_id = i.tenant_id and p.id = i.parent_id \
           join e_item it on it.tenant_id = i.tenant_id and it.id = i.item \
           where i.tenant_id = $1 and p.company = $2 and p.docstatus = 1 and p.posting_date between $3 and $4 and ($5::uuid is null or p.id = $5) \
           union all \
           select p.number, p.status, p.posting_date, 'Sub-assembly', it.item_code, it.item_name, i.qty, i.ordered_qty, i.produced_qty, i.idx \
           from {subs} i join e_production_plan p on p.tenant_id = i.tenant_id and p.id = i.parent_id join e_item it on it.tenant_id = i.tenant_id and it.id = i.item \
           where i.tenant_id = $1 and p.company = $2 and p.docstatus = 1 and p.posting_date between $3 and $4 and ($5::uuid is null or p.id = $5) \
         ) x order by posting_date, number, kind, idx limit $6"
    )))
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(p.uuid("production_plan")?)
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|r| {
            let (planned, produced) = (r.planned.unwrap_or_default(), r.produced.unwrap_or_default());
            json!({"production_plan": r.number, "status": r.status, "posting_date": r.posting_date, "kind": r.kind, "item_code": r.item_code,
                   "item_name": r.item_name, "planned_qty": money(planned), "ordered_qty": money(r.ordered.unwrap_or_default()),
                   "produced_qty": money(produced), "pending_qty": money((planned - produced).max(Decimal::ZERO)), "progress": money(pct(produced, planned).min(Decimal::ONE_HUNDRED))})
        })
        .collect();
    Ok(json!({
        "columns": [col("production_plan", "Production Plan", "data"), col("status", "Status", "data"), col("posting_date", "Date", "date"),
            col("kind", "Row", "data"), col("item_code", "Item", "data"), col("item_name", "Item Name", "data"), col("planned_qty", "Planned", "decimal"),
            col("ordered_qty", "In Work Orders", "decimal"), col("produced_qty", "Produced", "decimal"), col("pending_qty", "Pending", "decimal"),
            col("progress", "Progress %", "percent")],
        "rows": rows,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

#[derive(sqlx::FromRow)]
struct ScRow {
    number: Option<String>,
    supplier_name: Option<String>,
    status: Option<String>,
    finished_good: Option<String>,
    ordered: Option<Decimal>,
    received: Option<Decimal>,
    material: Option<String>,
    required: Option<Decimal>,
    supplied: Option<Decimal>,
    consumed: Option<Decimal>,
    returned: Option<Decimal>,
}

/// Per order material: required, sent, consumed by receipts, returned and still at the
/// supplier, next to the goods ordered and received.
async fn subcontracting_status(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let mats = table_of(ctx, "SubcontractingOrder", "supplied_items")?;
    let lines = table_of(ctx, "SubcontractingOrder", "items")?;
    let data: Vec<ScRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select o.number, o.supplier_name, o.status, fg.item_code as finished_good, l.qty as ordered, l.received_qty as received, \
                rm.item_code as material, m.required_qty as required, m.supplied_qty as supplied, m.consumed_qty as consumed, m.returned_qty as returned \
         from {mats} m join e_subcontracting_order o on o.tenant_id = m.tenant_id and o.id = m.parent_id \
         left join {lines} l on l.tenant_id = m.tenant_id and l.id::text = m.sco_line \
         left join e_item fg on fg.tenant_id = m.tenant_id and fg.id = m.main_item left join e_item rm on rm.tenant_id = m.tenant_id and rm.id = m.rm_item \
         where m.tenant_id = $1 and o.company = $2 and o.docstatus = 1 and o.posting_date between $3 and $4 \
         and ($5::uuid is null or o.supplier = $5) and ($6::uuid is null or o.id = $6) \
         order by o.posting_date, o.number, l.idx, m.idx limit $7"
    )))
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(p.uuid("supplier")?)
    .bind(p.uuid("subcontracting_order")?)
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let z = |x: Option<Decimal>| x.unwrap_or_default();
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|r| {
            let held = z(r.supplied) - z(r.returned) - z(r.consumed);
            json!({"subcontracting_order": r.number, "supplier": r.supplier_name, "status": r.status, "finished_good": r.finished_good,
                   "ordered_qty": money(z(r.ordered)), "received_qty": money(z(r.received)), "material": r.material,
                   "required_qty": money(z(r.required)), "supplied_qty": money(z(r.supplied)), "consumed_qty": money(z(r.consumed)),
                   "returned_qty": money(z(r.returned)), "at_supplier": money(held)})
        })
        .collect();
    Ok(json!({
        "columns": [col("subcontracting_order", "Subcontracting Order", "data"), col("supplier", "Supplier", "data"), col("status", "Status", "data"),
            col("finished_good", "Finished Good", "data"), col("ordered_qty", "Ordered", "decimal"), col("received_qty", "Received", "decimal"),
            col("material", "Raw Material", "data"), col("required_qty", "Required", "decimal"), col("supplied_qty", "Sent", "decimal"),
            col("consumed_qty", "Consumed", "decimal"), col("returned_qty", "Returned", "decimal"), col("at_supplier", "At Supplier", "decimal")],
        "rows": rows,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}
