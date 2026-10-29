//! Stock operations reports: reconciliation variance, landed cost allocation, pick list status,
//! quality inspection summary, variant stock matrix and stock ageing.

use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::reports::{Params, ReportCompany, ReportSource, col, param_def, period, read_tx, resolve_company, today};
use ferrum_domain::util::money;
use ferrum_kernel::ddl::q;
use ferrum_kernel::{Ctx, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use uuid::Uuid;

const MAX_ROWS: i64 = 10_000;

struct Def {
    name: &'static str,
    label: &'static str,
    entity: &'static str,
    params: &'static [&'static str],
}

const DEFS: &[Def] = &[
    Def {
        name: "stock_reconciliation_variance",
        label: "Stock Reconciliation Variance",
        entity: "StockReconciliation",
        params: &["company", "from_date", "to_date", "warehouse", "item"],
    },
    Def {
        name: "landed_cost_allocation",
        label: "Landed Cost Allocation",
        entity: "LandedCostVoucher",
        params: &["company", "from_date", "to_date", "item", "supplier"],
    },
    Def { name: "pick_list_status", label: "Pick List Status", entity: "PickList", params: &["company", "from_date", "to_date", "customer", "status"] },
    Def {
        name: "quality_inspection_summary",
        label: "Quality Inspection Summary",
        entity: "QualityInspection",
        params: &["company", "from_date", "to_date", "item", "inspection_type"],
    },
    Def { name: "variant_stock_matrix", label: "Variant Stock Matrix", entity: "Item", params: &["company", "template", "warehouse"] },
    Def { name: "stock_ageing", label: "Stock Ageing", entity: "StockEntry", params: &["company", "as_of", "warehouse", "item", "range1", "range2", "range3"] },
];

fn param(name: &str) -> Value {
    match name {
        "status" => json!({"name": name, "label": "Status", "type": "select", "options": ["Draft", "Open", "Completed", "Cancelled"]}),
        "inspection_type" => json!({"name": name, "label": "Inspection Type", "type": "select", "options": ["Incoming", "Outgoing", "In Process"]}),
        "template" => json!({"name": name, "label": "Template Item", "type": "link", "to": "Item", "required": true}),
        "range1" => json!({"name": name, "label": "Ageing Range 1 (days)", "type": "int", "default": 30}),
        "range2" => json!({"name": name, "label": "Ageing Range 2 (days)", "type": "int", "default": 60}),
        "range3" => json!({"name": name, "label": "Ageing Range 3 (days)", "type": "int", "default": 90}),
        other => param_def(other),
    }
}

pub struct StockOpsReports;

#[async_trait]
impl ReportSource for StockOpsReports {
    fn catalog(&self) -> Vec<Value> {
        DEFS.iter()
            .map(|d| json!({"name": d.name, "label": d.label, "module": "stock_ops", "entity": d.entity, "params": d.params.iter().map(|p| param(p)).collect::<Vec<_>>()}))
            .collect()
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let mut out = match name {
            "stock_reconciliation_variance" => variance(&mut tx, ctx, &co, &p).await?,
            "landed_cost_allocation" => landed(&mut tx, ctx, &co, &p).await?,
            "pick_list_status" => picking(&mut tx, ctx, &co, &p).await?,
            "quality_inspection_summary" => quality(&mut tx, ctx, &co, &p).await?,
            "variant_stock_matrix" => matrix(&mut tx, ctx, &co, &p).await?,
            "stock_ageing" => ageing(&mut tx, ctx, &co, &p).await?,
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

fn table(ctx: &Ctx, entity: &str) -> Result<String> {
    ctx.schema.entity(entity).map(|e| q(&e.table)).ok_or_else(|| Error::NotFound(format!("entity {entity}")))
}

fn child(ctx: &Ctx, entity: &str, field: &str) -> Result<String> {
    crate::common::child_table(ctx, entity, field).map(|t| q(&t))
}

async fn variance(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let sql = format!(
        "select p.id, p.posting_date, p.number, p.purpose, i.item_code, i.item_name, w.warehouse_name, r.batch_no, \
         coalesce(r.current_qty, 0), r.qty, coalesce(r.quantity_difference, 0), coalesce(r.current_valuation_rate, 0), \
         coalesce(r.valuation_rate, r.current_valuation_rate, 0), coalesce(r.current_amount, 0), coalesce(r.amount, 0), coalesce(r.amount_difference, 0) \
         from {} r join {} p on p.tenant_id = r.tenant_id and p.id = r.parent_id \
         join e_item i on i.tenant_id = r.tenant_id and i.id = r.item join e_warehouse w on w.tenant_id = r.tenant_id and w.id = r.warehouse \
         where r.tenant_id = $1 and p.company = $2 and p.docstatus = 1 and p.posting_date between $3 and $4 \
         and ($5::uuid is null or r.warehouse = $5) and ($6::uuid is null or r.item = $6) order by p.posting_date, p.number, r.idx limit $7",
        child(ctx, "StockReconciliation", "items")?,
        table(ctx, "StockReconciliation")?
    );
    #[allow(clippy::type_complexity)]
    let data: Vec<(
        Uuid,
        NaiveDate,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        String,
        Option<String>,
        Decimal,
        Decimal,
        Decimal,
        Decimal,
        Decimal,
        Decimal,
        Decimal,
        Decimal,
    )> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(from)
        .bind(to)
        .bind(p.uuid("warehouse")?)
        .bind(p.uuid("item")?)
        .bind(MAX_ROWS + 1)
        .fetch_all(&mut **tx)
        .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let (mut gain, mut loss) = (Decimal::ZERO, Decimal::ZERO);
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(vid, day, number, purpose, code, name, wh, batch, cq, qty, dq, cr, nr, ca, amount, diff)| {
            if diff > Decimal::ZERO {
                gain += diff;
            } else {
                loss += diff;
            }
            json!({"voucher_id": vid, "posting_date": day, "voucher_no": number, "purpose": purpose, "item_code": code, "item_name": name,
                "warehouse": wh, "batch_no": batch, "current_qty": money(cq), "counted_qty": money(qty), "qty_difference": money(dq),
                "current_rate": money(cr.round_dp(9)), "new_rate": money(nr.round_dp(9)), "current_value": money(ca), "new_value": money(amount),
                "value_difference": money(diff)})
        })
        .collect();
    Ok(json!({
        "columns": [col("posting_date", "Date", "date"), col("voucher_no", "Reconciliation", "data"), col("purpose", "Purpose", "data"),
            col("item_code", "Item", "data"), col("warehouse", "Warehouse", "data"), col("batch_no", "Batch", "data"),
            col("current_qty", "Book Qty", "decimal"), col("counted_qty", "Counted Qty", "decimal"), col("qty_difference", "Qty Difference", "decimal"),
            col("current_rate", "Book Rate", "currency"), col("new_rate", "New Rate", "currency"), col("current_value", "Book Value", "currency"),
            col("new_value", "New Value", "currency"), col("value_difference", "Value Difference", "currency")],
        "rows": rows,
        "totals": {"gain": money(gain), "loss": money(loss), "value_difference": money(gain + loss)},
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

async fn landed(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let sql = format!(
        "select p.id, p.posting_date, p.number, r.receipt_type, coalesce(pr.number, pi.number), coalesce(pr.supplier_name, pi.supplier_name), \
         i.item_code, i.item_name, w.warehouse_name, coalesce(r.qty, 0), coalesce(r.amount, 0), coalesce(r.applicable_charges, 0), \
         coalesce(r.capitalized_amount, 0), coalesce(r.consumed_amount, 0) \
         from {} r join {} p on p.tenant_id = r.tenant_id and p.id = r.parent_id \
         left join e_purchase_receipt pr on pr.tenant_id = r.tenant_id and pr.id = r.purchase_receipt \
         left join e_purchase_invoice pi on pi.tenant_id = r.tenant_id and pi.id = r.purchase_invoice \
         join e_item i on i.tenant_id = r.tenant_id and i.id = r.item join e_warehouse w on w.tenant_id = r.tenant_id and w.id = r.warehouse \
         where r.tenant_id = $1 and p.company = $2 and p.docstatus = 1 and p.posting_date between $3 and $4 \
         and ($5::uuid is null or r.item = $5) and ($6::uuid is null or coalesce(pr.supplier, pi.supplier) = $6) \
         order by p.posting_date, p.number, r.idx limit $7",
        child(ctx, "LandedCostVoucher", "items")?,
        table(ctx, "LandedCostVoucher")?
    );
    #[allow(clippy::type_complexity)]
    let data: Vec<(
        Uuid,
        NaiveDate,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        String,
        Decimal,
        Decimal,
        Decimal,
        Decimal,
        Decimal,
    )> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(from)
        .bind(to)
        .bind(p.uuid("item")?)
        .bind(p.uuid("supplier")?)
        .bind(MAX_ROWS + 1)
        .fetch_all(&mut **tx)
        .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let mut totals = [Decimal::ZERO; 4];
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(vid, day, number, kind, receipt, supplier, code, name, wh, qty, amount, charges, capitalized, consumed)| {
            for (t, v) in totals.iter_mut().zip([amount, charges, capitalized, consumed]) {
                *t += v;
            }
            let landed = if qty.is_zero() { Decimal::ZERO } else { ((amount + charges) / qty).round_dp(co.precision + 2) };
            json!({"voucher_id": vid, "posting_date": day, "voucher_no": number, "receipt_type": kind, "receipt_no": receipt, "supplier": supplier,
                "item_code": code, "item_name": name, "warehouse": wh, "qty": money(qty), "receipt_value": money(amount),
                "applicable_charges": money(charges), "capitalized": money(capitalized), "consumed": money(consumed), "landed_rate": money(landed)})
        })
        .collect();
    Ok(json!({
        "columns": [col("posting_date", "Date", "date"), col("voucher_no", "Voucher", "data"), col("receipt_no", "Receipt", "data"),
            col("supplier", "Supplier", "data"), col("item_code", "Item", "data"), col("warehouse", "Warehouse", "data"), col("qty", "Qty", "decimal"),
            col("receipt_value", "Receipt Value", "currency"), col("applicable_charges", "Landed Charges", "currency"),
            col("capitalized", "Added to Stock", "currency"), col("consumed", "Booked as Consumed", "currency"), col("landed_rate", "Landed Rate", "currency")],
        "rows": rows,
        "totals": {"receipt_value": money(totals[0]), "applicable_charges": money(totals[1]), "capitalized": money(totals[2]), "consumed": money(totals[3])},
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

async fn picking(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let sql = format!(
        "select p.id, p.number, p.posting_date, p.purpose, c.customer_name, p.status, coalesce(sum(r.qty), 0), coalesce(sum(r.picked_qty), 0), \
         coalesce(sum(r.delivered_qty), 0), count(r.id) \
         from {} p left join {} r on r.tenant_id = p.tenant_id and r.parent_id = p.id left join e_customer c on c.tenant_id = p.tenant_id and c.id = p.customer \
         where p.tenant_id = $1 and p.company = $2 and p.posting_date between $3 and $4 and ($5::uuid is null or p.customer = $5) \
         and ($6::text is null or p.status = $6) \
         group by p.id, p.number, p.posting_date, p.purpose, c.customer_name, p.status order by p.posting_date, p.number limit $7",
        table(ctx, "PickList")?,
        child(ctx, "PickList", "locations")?
    );
    #[allow(clippy::type_complexity)]
    let data: Vec<(Uuid, Option<String>, NaiveDate, Option<String>, Option<String>, Option<String>, Decimal, Decimal, Decimal, i64)> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(ctx.tenant)
            .bind(co.id)
            .bind(from)
            .bind(to)
            .bind(p.uuid("customer")?)
            .bind(p.str("status"))
            .bind(MAX_ROWS + 1)
            .fetch_all(&mut **tx)
            .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let mut by_status: BTreeMap<String, i64> = BTreeMap::new();
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(pid, number, day, purpose, customer, status, qty, picked, delivered, lines)| {
            let status = status.unwrap_or_else(|| "Draft".into());
            *by_status.entry(status.clone()).or_default() += 1;
            let pct = if picked.is_zero() { Decimal::ZERO } else { (delivered.min(picked) / picked * Decimal::ONE_HUNDRED).round_dp(2) };
            json!({"pick_list_id": pid, "pick_list": number, "posting_date": day, "purpose": purpose, "customer": customer, "status": status,
                "lines": lines, "qty": money(qty), "picked_qty": money(picked), "delivered_qty": money(delivered),
                "to_deliver": money((picked - delivered).max(Decimal::ZERO)), "per_delivered": money(pct)})
        })
        .collect();
    Ok(json!({
        "columns": [col("posting_date", "Date", "date"), col("pick_list", "Pick List", "data"), col("purpose", "Purpose", "data"),
            col("customer", "Customer", "data"), col("status", "Status", "data"), col("lines", "Lines", "int"), col("qty", "To Pick", "decimal"),
            col("picked_qty", "Picked", "decimal"), col("delivered_qty", "Delivered", "decimal"), col("to_deliver", "Still to Deliver", "decimal"),
            col("per_delivered", "% Delivered", "percent")],
        "rows": rows,
        "totals": {"by_status": by_status},
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

async fn quality(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let qi = table(ctx, "QualityInspection")?;
    let filter = "q.tenant_id = $1 and q.company = $2 and q.docstatus = 1 and q.report_date between $3 and $4 \
                  and ($5::uuid is null or q.item = $5) and ($6::text is null or q.inspection_type = $6)";
    #[allow(clippy::type_complexity)]
    let data: Vec<(Uuid, String, Option<String>, Option<String>, i64, i64, i64, Decimal)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select i.id, i.item_code, i.item_name, q.inspection_type, count(*), count(*) filter (where q.status = 'Accepted'), \
         count(*) filter (where q.status = 'Rejected'), coalesce(sum(q.sample_size), 0) \
         from {qi} q join e_item i on i.tenant_id = q.tenant_id and i.id = q.item where {filter} \
         group by i.id, i.item_code, i.item_name, q.inspection_type order by i.item_code, q.inspection_type limit {}",
        MAX_ROWS + 1
    )))
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(p.uuid("item")?)
    .bind(p.str("inspection_type"))
    .fetch_all(&mut **tx)
    .await?;
    let failing: Vec<(Uuid, Option<String>, String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select q.item, q.inspection_type, r.parameter, count(*) from {} r join {qi} q on q.tenant_id = r.tenant_id and q.id = r.parent_id \
         where {filter} and r.status = 'Rejected' group by q.item, q.inspection_type, r.parameter order by count(*) desc, r.parameter",
        child(ctx, "QualityInspection", "readings")?
    )))
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(p.uuid("item")?)
    .bind(p.str("inspection_type"))
    .fetch_all(&mut **tx)
    .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let (mut total, mut accepted, mut rejected) = (0i64, 0i64, 0i64);
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(item, code, name, kind, n, ok, bad, sample)| {
            total += n;
            accepted += ok;
            rejected += bad;
            let params: Vec<String> = failing.iter().filter(|f| f.0 == item && f.1 == kind).map(|f| format!("{} ({})", f.2, f.3)).collect();
            let rate = if n == 0 { Decimal::ZERO } else { (Decimal::from(ok) * Decimal::ONE_HUNDRED / Decimal::from(n)).round_dp(2) };
            json!({"item_code": code, "item_name": name, "inspection_type": kind, "inspections": n, "accepted": ok, "rejected": bad,
                "acceptance_rate": money(rate), "sample_size": money(sample), "rejected_parameters": params.join(", ")})
        })
        .collect();
    Ok(json!({
        "columns": [col("item_code", "Item", "data"), col("item_name", "Item Name", "data"), col("inspection_type", "Type", "data"),
            col("inspections", "Inspections", "int"), col("accepted", "Accepted", "int"), col("rejected", "Rejected", "int"),
            col("acceptance_rate", "Acceptance %", "percent"), col("sample_size", "Units Sampled", "decimal"),
            col("rejected_parameters", "Failing Parameters", "data")],
        "rows": rows,
        "totals": {"inspections": total, "accepted": accepted, "rejected": rejected},
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

async fn matrix(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let template = p.uuid("template")?.ok_or_else(|| Error::field("template", "choose the template item"))?;
    let attrs_table = child(ctx, "Item", "stk_attributes")?;
    let attrs: Vec<(Uuid, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select a.attribute, coalesce(t.attribute_name, '') from {attrs_table} a left join e_item_attribute t on t.tenant_id = a.tenant_id and t.id = a.attribute \
         where a.tenant_id = $1 and a.parent_id = $2 order by a.idx"
    )))
    .bind(ctx.tenant)
    .bind(template)
    .fetch_all(&mut **tx)
    .await?;
    let variants: Vec<(Uuid, String, Option<String>)> =
        sqlx::query_as("select id, item_code, item_name from e_item where tenant_id = $1 and ext->>'stk_variant_of' = $2 order by item_code")
            .bind(ctx.tenant)
            .bind(template.to_string())
            .fetch_all(&mut **tx)
            .await?;
    let ids: Vec<Uuid> = variants.iter().map(|v| v.0).collect();
    let values: Vec<(Uuid, Uuid, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select parent_id, attribute, attribute_value from {attrs_table} where tenant_id = $1 and parent_id = any($2)"
    )))
    .bind(ctx.tenant)
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?;
    let stock: Vec<(Uuid, Uuid, String, Decimal, Decimal)> = sqlx::query_as(
        "select b.item_id, b.warehouse_id, w.warehouse_name, b.qty, b.value from stock_balance b join e_warehouse w on w.tenant_id = b.tenant_id and w.id = b.warehouse_id \
         where b.tenant_id = $1 and b.company_id = $2 and b.item_id = any($3) and ($4::uuid is null or b.warehouse_id = $4) and b.qty <> 0 order by w.warehouse_name",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(&ids)
    .bind(p.uuid("warehouse")?)
    .fetch_all(&mut **tx)
    .await?;
    let mut whs: Vec<(Uuid, String)> = Vec::new();
    for (_, w, name, _, _) in &stock {
        if !whs.iter().any(|x| x.0 == *w) {
            whs.push((*w, name.clone()));
        }
    }
    let mut columns = vec![col("item_code", "Variant", "data"), col("item_name", "Item Name", "data")];
    for (i, (_, name)) in attrs.iter().enumerate() {
        columns.push(col(&format!("attribute_{i}"), name, "data"));
    }
    for (i, (_, name)) in whs.iter().enumerate() {
        columns.push(col(&format!("warehouse_{i}"), name, "decimal"));
    }
    columns.push(col("total_qty", "Total Qty", "decimal"));
    columns.push(col("total_value", "Total Value", "currency"));
    let mut wh_totals = vec![Decimal::ZERO; whs.len()];
    let (mut all_qty, mut all_value) = (Decimal::ZERO, Decimal::ZERO);
    let mut rows = Vec::new();
    for (vid, code, name) in &variants {
        let mut r = json!({"item_id": vid, "item_code": code, "item_name": name});
        for (i, (aid, _)) in attrs.iter().enumerate() {
            let v = values.iter().find(|(p, a, _)| p == vid && a == aid).and_then(|x| x.2.clone());
            r[format!("attribute_{i}")] = json!(v);
        }
        let (mut tq, mut tv) = (Decimal::ZERO, Decimal::ZERO);
        for (i, (w, _)) in whs.iter().enumerate() {
            let (q, v) = stock.iter().filter(|s| s.0 == *vid && s.1 == *w).fold((Decimal::ZERO, Decimal::ZERO), |a, s| (a.0 + s.3, a.1 + s.4));
            r[format!("warehouse_{i}")] = money(q);
            wh_totals[i] += q;
            tq += q;
            tv += v;
        }
        r["total_qty"] = money(tq);
        r["total_value"] = money(tv);
        all_qty += tq;
        all_value += tv;
        rows.push(r);
    }
    let mut totals = json!({"total_qty": money(all_qty), "total_value": money(all_value)});
    for (i, t) in wh_totals.iter().enumerate() {
        totals[format!("warehouse_{i}")] = money(*t);
    }
    Ok(json!({"columns": columns, "rows": rows, "totals": totals, "meta": {"template": template, "variants": variants.len()}}))
}

/// One received quantity and the day it arrived.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Slot {
    pub qty: Decimal,
    pub day: NaiveDate,
}

/// Takes `qty` from the oldest slots; returns what was taken (with its dates).
pub(crate) fn take(queue: &mut VecDeque<Slot>, mut qty: Decimal) -> Vec<Slot> {
    let mut out = Vec::new();
    while qty > Decimal::ZERO {
        let Some(front) = queue.front_mut() else { break };
        let t = qty.min(front.qty);
        out.push(Slot { qty: t, day: front.day });
        front.qty -= t;
        qty -= t;
        if front.qty <= Decimal::ZERO {
            queue.pop_front();
        }
    }
    out
}

/// Days since receipt bucketed by the three range limits: `[0..=r1, ..=r2, ..=r3, older]`.
pub(crate) fn buckets(queue: &VecDeque<Slot>, as_of: NaiveDate, limits: [i64; 3]) -> [Decimal; 4] {
    let mut out = [Decimal::ZERO; 4];
    for s in queue {
        let age = (as_of - s.day).num_days();
        let i = limits.iter().position(|l| age <= *l).unwrap_or(3);
        out[i] += s.qty;
    }
    out
}

#[derive(sqlx::FromRow)]
struct AgeMove {
    item_id: Uuid,
    warehouse_id: Uuid,
    posting_date: NaiveDate,
    qty: Decimal,
    value: Decimal,
    source_entity: String,
    source_id: Uuid,
    source_line_id: Option<Uuid>,
    /// Part of a cancelled document (the move or its reversal): counts for the value, not the age.
    cancelled: bool,
}

/// Ages the stock on hand by when it was received, first in first out per item and warehouse.
/// Cancelled documents do not count, stock moved between warehouses keeps its receipt date,
/// reconciliations count by their net change and landed-cost revaluations not at all.
async fn ageing(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let as_of = p.date("as_of")?.unwrap_or_else(today);
    let limit = |k: &str, d: i64| -> Result<i64> {
        match p.str(k) {
            None => Ok(d),
            Some(v) => v.parse::<i64>().ok().filter(|x| *x > 0).ok_or_else(|| Error::field(k, "must be a positive number of days")),
        }
    };
    let limits = [limit("range1", 30)?, limit("range2", 60)?, limit("range3", 90)?];
    if !(limits[0] < limits[1] && limits[1] < limits[2]) {
        return Err(Error::field("range2", "the ranges must increase"));
    }
    let (wh_filter, item_filter) = (p.uuid("warehouse")?, p.uuid("item")?);
    let moves: Vec<AgeMove> = sqlx::query_as(
        "select m.item_id, m.warehouse_id, m.posting_date, m.qty, m.value, m.source_entity, m.source_id, m.source_line_id, \
         (m.reversal_of is not null or exists (select 1 from stock_move r where r.tenant_id = m.tenant_id and r.reversal_of = m.id and r.posting_date <= $3)) as cancelled \
         from stock_move m where m.tenant_id = $1 and m.company_id = $2 and m.posting_date <= $3 and ($4::uuid is null or m.item_id = $4) \
         order by m.posting_ts, m.seq",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(as_of)
    .bind(item_filter)
    .fetch_all(&mut **tx)
    .await?;
    // Reconciliations and landed-cost revaluations count once, by their net change per line.
    let netted = |m: &AgeMove| matches!(m.source_entity.as_str(), "StockReconciliation" | "LandedCostVoucher");
    let group_key = |m: &AgeMove| {
        let line = if m.source_entity == "StockReconciliation" { m.source_line_id } else { None };
        (m.source_id, m.item_id, m.warehouse_id, line)
    };
    let mut net: HashMap<(Uuid, Uuid, Uuid, Option<Uuid>), Decimal> = HashMap::new();
    for m in moves.iter().filter(|m| netted(m) && !m.cancelled) {
        *net.entry(group_key(m)).or_default() += m.qty;
    }
    let mut queues: BTreeMap<(Uuid, Uuid), VecDeque<Slot>> = BTreeMap::new();
    let mut values: HashMap<(Uuid, Uuid), (Decimal, Decimal)> = HashMap::new();
    let mut in_transit: HashMap<(String, Uuid, Option<Uuid>, Uuid), Vec<Slot>> = HashMap::new();
    for m in &moves {
        let key = (m.item_id, m.warehouse_id);
        let v = values.entry(key).or_default();
        v.0 += m.qty;
        v.1 += m.value;
        if m.cancelled {
            continue;
        }
        let qty = if netted(m) {
            match net.remove(&group_key(m)) {
                Some(n) => n,
                None => continue,
            }
        } else {
            m.qty
        };
        let queue = queues.entry(key).or_default();
        let transfer = (m.source_entity.clone(), m.source_id, m.source_line_id, m.item_id);
        if qty < Decimal::ZERO {
            let taken = take(queue, -qty);
            if m.source_entity == "StockEntry" {
                in_transit.insert(transfer, taken);
            }
        } else if qty > Decimal::ZERO {
            match in_transit.remove(&transfer) {
                Some(slots) if slots.iter().map(|s| s.qty).sum::<Decimal>() == qty => queue.extend(slots),
                _ => queue.push_back(Slot { qty, day: m.posting_date }),
            }
        }
    }
    let names: HashMap<Uuid, (String, Option<String>)> =
        sqlx::query_as::<_, (Uuid, String, Option<String>)>("select id, item_code, item_name from e_item where tenant_id = $1 and id = any($2)")
            .bind(ctx.tenant)
            .bind(queues.keys().map(|k| k.0).collect::<Vec<_>>())
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .map(|(i, c, n)| (i, (c, n)))
            .collect();
    let whs: HashMap<Uuid, String> = sqlx::query_as::<_, (Uuid, String)>("select id, warehouse_name from e_warehouse where tenant_id = $1 and company = $2")
        .bind(ctx.tenant)
        .bind(co.id)
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .collect();
    let mut rows = Vec::new();
    let mut totals = [Decimal::ZERO; 4];
    let (mut total_qty, mut total_value) = (Decimal::ZERO, Decimal::ZERO);
    for ((item, wh), queue) in &queues {
        if wh_filter.is_some_and(|w| w != *wh) {
            continue;
        }
        let (qty, value) = values.get(&(*item, *wh)).copied().unwrap_or_default();
        let held: Decimal = queue.iter().map(|s| s.qty).sum();
        if held.is_zero() && qty.is_zero() {
            continue;
        }
        let b = buckets(queue, as_of, limits);
        for (t, x) in totals.iter_mut().zip(b) {
            *t += x;
        }
        total_qty += qty;
        total_value += value;
        let days: Decimal = queue.iter().map(|s| s.qty * Decimal::from((as_of - s.day).num_days())).sum();
        let avg = if held.is_zero() { Decimal::ZERO } else { (days / held).round_dp(1) };
        let (code, name) = names.get(item).cloned().unwrap_or_default();
        rows.push(json!({
            "item_code": code, "item_name": name, "warehouse": whs.get(wh), "qty": money(qty), "value": money(value), "average_age": money(avg),
            "oldest": queue.front().map(|s| s.day), "range1": money(b[0]), "range2": money(b[1]), "range3": money(b[2]), "older": money(b[3]),
        }));
    }
    rows.sort_by(|a, b| (a["item_code"].as_str(), a["warehouse"].as_str()).cmp(&(b["item_code"].as_str(), b["warehouse"].as_str())));
    let label = |a: i64, b: i64| format!("{a}-{b} days");
    Ok(json!({
        "columns": [col("item_code", "Item", "data"), col("item_name", "Item Name", "data"), col("warehouse", "Warehouse", "data"),
            col("qty", "Qty", "decimal"), col("value", "Value", "currency"), col("average_age", "Average Age (days)", "decimal"),
            col("oldest", "Oldest Receipt", "date"), col("range1", &label(0, limits[0]), "decimal"), col("range2", &label(limits[0] + 1, limits[1]), "decimal"),
            col("range3", &label(limits[1] + 1, limits[2]), "decimal"), col("older", &format!("Over {} days", limits[2]), "decimal")],
        "rows": rows,
        "totals": {"qty": money(total_qty), "value": money(total_value), "range1": money(totals[0]), "range2": money(totals[1]),
            "range3": money(totals[2]), "older": money(totals[3])},
        "meta": {"as_of": as_of, "ranges": limits},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, d).unwrap()
    }

    #[test]
    fn fifo_slots_age_the_remaining_stock() {
        let mut q: VecDeque<Slot> = VecDeque::new();
        q.push_back(Slot { qty: n("5"), day: day(1) });
        q.push_back(Slot { qty: n("5"), day: day(20) });
        let taken = take(&mut q, n("7"));
        assert_eq!(taken, vec![Slot { qty: n("5"), day: day(1) }, Slot { qty: n("2"), day: day(20) }]);
        assert_eq!(q.front().map(|s| s.qty), Some(n("3")));
        assert!(take(&mut q, n("10")).iter().map(|s| s.qty).sum::<Decimal>() == n("3"), "never more than on hand");
        let mut q: VecDeque<Slot> = VecDeque::new();
        q.push_back(Slot { qty: n("2"), day: day(1) });
        q.push_back(Slot { qty: n("3"), day: day(31) });
        let as_of = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        assert_eq!(buckets(&q, as_of, [30, 45, 90]), [n("3"), Decimal::ZERO, n("2"), Decimal::ZERO]);
    }
}
