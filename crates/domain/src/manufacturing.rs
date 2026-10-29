//! Manufacturing: bills of materials (costing, revisions, recursion checks), work orders that
//! produce through manufacture stock entries, and an MRP run that nets demand level by level.

use crate::flow::created;
use crate::stock::valuation_rate;
use crate::util::{b, company, d, elevated, get, id, money, round, rows, s, set, sys};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_kernel::ddl::q;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

const MAX_BOM_DEPTH: usize = 30;

fn child_table(hc: &HookCtx<'_>, entity: &str, field: &str) -> Result<String> {
    hc.ctx
        .schema
        .entity(entity)
        .and_then(|e| e.children.get(field).map(|c| c.table.clone()))
        .ok_or_else(|| Error::internal(format!("{entity}.{field} is not a table")))
}

/// The BOM used for an item: the default one, else the newest active submitted one.
pub async fn default_bom(hc: &mut HookCtx<'_>, company: Uuid, item: Uuid) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "select id from e_bom where tenant_id = $1 and company = $2 and item = $3 and docstatus = 1 and is_active \
         order by is_default desc, created_at desc limit 1",
    )
    .bind(hc.ctx.tenant)
    .bind(company)
    .bind(item)
    .fetch_optional(&mut **hc.tx)
    .await?)
}

/// Components per one unit of the BOM's item, scrap included.
async fn components(hc: &mut HookCtx<'_>, bom: Uuid) -> Result<(Decimal, Vec<(Uuid, Decimal)>)> {
    let qty: Decimal =
        sqlx::query_scalar("select quantity from e_bom where tenant_id = $1 and id = $2").bind(hc.ctx.tenant).bind(bom).fetch_one(&mut **hc.tx).await?;
    let table = child_table(hc, "Bom", "items")?;
    let lines: Vec<(Uuid, Decimal, Option<Decimal>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("select item, qty, scrap_percent from {} where tenant_id = $1 and parent_id = $2 order by idx", q(&table))))
            .bind(hc.ctx.tenant)
            .bind(bom)
            .fetch_all(&mut **hc.tx)
            .await?;
    let per = if qty.is_zero() { Decimal::ONE } else { qty };
    Ok((per, lines.into_iter().map(|(i, lq, scrap)| (i, lq / per * (Decimal::ONE + scrap.unwrap_or_default() / Decimal::ONE_HUNDRED))).collect()))
}

/// Unit cost used for BOM costing: stock valuation, else last purchase rate, else the item's own BOM.
async fn unit_cost(hc: &mut HookCtx<'_>, company: Uuid, item: Uuid, warehouse: Option<Uuid>, depth: usize) -> Result<Decimal> {
    let v = valuation_rate(hc.tx, hc.ctx.tenant, company, item, warehouse).await?;
    if v > Decimal::ZERO {
        return Ok(v);
    }
    let it = get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?;
    if d(&it, "last_purchase_rate") > Decimal::ZERO {
        return Ok(d(&it, "last_purchase_rate"));
    }
    if depth < MAX_BOM_DEPTH
        && let Some(sub) = default_bom(hc, company, item).await?
    {
        let total: Decimal = sqlx::query_scalar("select coalesce(total_cost, 0) / nullif(quantity, 0) from e_bom where tenant_id = $1 and id = $2")
            .bind(hc.ctx.tenant)
            .bind(sub)
            .fetch_one(&mut **hc.tx)
            .await
            .map(|x: Option<Decimal>| x.unwrap_or_default())?;
        return Ok(total);
    }
    Ok(Decimal::ZERO)
}

/// Fills line rates/amounts and the cost totals of a BOM document.
async fn cost_bom(hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
    let Some(cid) = id(doc, "company") else { return Ok(()) };
    let precision = company(hc.kernel, hc.ctx, hc.tx, cid).await?.precision;
    let mut lines = rows(doc, "items");
    let mut raw = Decimal::ZERO;
    for r in lines.iter_mut() {
        let Some(item) = id(r, "item") else { continue };
        let rate = unit_cost(hc, cid, item, id(r, "source_warehouse"), 0).await?;
        let factor = Decimal::ONE + d(r, "scrap_percent") / Decimal::ONE_HUNDRED;
        let amount = round(d(r, "qty") * rate * factor, precision);
        set(r, "rate", money(rate.round_dp(9)));
        set(r, "amount", money(amount));
        raw += amount;
    }
    set(doc, "items", Value::Array(lines.into_iter().map(Value::Object).collect()));
    set(doc, "raw_material_cost", money(raw));
    set(doc, "total_cost", money(raw + d(doc, "operating_cost")));
    Ok(())
}

pub struct BomHook;

#[async_trait]
impl DocHook for BomHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if d(doc, "docstatus").is_zero() {
            cost_bom(hc, doc).await?;
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let (Some(item), Some(cid)) = (id(doc, "item"), id(doc, "company")) else { return Ok(()) };
        let mut errors = Vec::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            if id(r, "item") == Some(item) {
                errors.push(FieldError::new("item", "a BOM cannot consume the item it produces").in_row("items", i));
            }
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        // Walk the default BOMs below every component; reaching our own item is a cycle.
        let mut stack: Vec<(Uuid, Vec<Uuid>)> = rows(doc, "items").iter().filter_map(|r| id(r, "item")).map(|c| (c, vec![item, c])).collect();
        let mut seen = HashSet::new();
        while let Some((node, path)) = stack.pop() {
            if path.len() > MAX_BOM_DEPTH {
                return Err(Error::invalid(format!("the BOM structure is deeper than {MAX_BOM_DEPTH} levels")));
            }
            if !seen.insert((node, path.len())) {
                continue;
            }
            let Some(sub) = default_bom(hc, cid, node).await? else { continue };
            let (_, comps) = components(hc, sub).await?;
            for (c, _) in comps {
                let mut p = path.clone();
                p.push(c);
                if c == item {
                    let mut names = Vec::new();
                    for x in &p {
                        names.push(s(&get(hc.kernel, hc.ctx, hc.tx, "Item", *x).await?, "item_code").unwrap_or("?").to_string());
                    }
                    return Err(Error::invalid(format!("circular bill of materials: {}", names.join(" → "))));
                }
                stack.push((c, p));
            }
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let (Some(bid), Some(item), Some(cid)) = (id(doc, "id"), id(doc, "item"), id(doc, "company")) else { return Ok(()) };
        let others: Vec<Uuid> =
            sqlx::query_scalar("select id from e_bom where tenant_id = $1 and company = $2 and item = $3 and docstatus = 1 and is_default and id <> $4")
                .bind(hc.ctx.tenant)
                .bind(cid)
                .bind(item)
                .bind(bid)
                .fetch_all(&mut **hc.tx)
                .await?;
        let sctx = sys(hc.ctx);
        if b(doc, "is_default") {
            for o in others {
                let mut v = Doc::new();
                v.insert("is_default".into(), json!(false));
                hc.kernel.set_values_tx(&sctx, hc.tx, "Bom", o, v).await?;
            }
        } else if others.is_empty() {
            let mut v = Doc::new();
            v.insert("is_default".into(), json!(true));
            hc.kernel.set_values_tx(&sctx, hc.tx, "Bom", bid, v).await?;
        }
        Ok(())
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let open: i64 = sqlx::query_scalar(
            "select count(*) from e_work_order where tenant_id = $1 and bom = $2 and docstatus = 1 and status in ('Not Started', 'In Process', 'Stopped')",
        )
        .bind(hc.ctx.tenant)
        .bind(id(doc, "id"))
        .fetch_one(&mut **hc.tx)
        .await?;
        if open > 0 {
            return Err(Error::invalid(format!("{open} open work order(s) use this BOM; complete or cancel them first")));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "update_cost" {
            return Ok(None);
        }
        let bid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let mut fresh = doc.clone();
        cost_bom(hc, &mut fresh).await?;
        let sctx = sys(hc.ctx);
        for r in rows(&fresh, "items") {
            let Some(line) = id(&r, "id") else { continue };
            let mut v = Doc::new();
            v.insert("rate".into(), r.get("rate").cloned().unwrap_or(Value::Null));
            v.insert("amount".into(), r.get("amount").cloned().unwrap_or(Value::Null));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "Bom", "items", line, v).await?;
        }
        let mut v = Doc::new();
        v.insert("raw_material_cost".into(), fresh.get("raw_material_cost").cloned().unwrap_or(Value::Null));
        v.insert("total_cost".into(), fresh.get("total_cost").cloned().unwrap_or(Value::Null));
        hc.kernel.set_values_tx(&sctx, hc.tx, "Bom", bid, v).await?;
        Ok(Some(json!({"raw_material_cost": fresh.get("raw_material_cost"), "total_cost": fresh.get("total_cost")})))
    }
}

/// Adds (sign = 1) or removes (sign = -1) produced output on a work order.
pub async fn record_production(hc: &mut HookCtx<'_>, wo: Uuid, produced: &[(Uuid, Decimal, Decimal)], sign: Decimal) -> Result<()> {
    let sctx = sys(hc.ctx);
    let w = hc.kernel.get_tx(&sctx, hc.tx, "WorkOrder", wo, true).await?;
    if d(&w, "docstatus") != Decimal::ONE {
        return Err(Error::invalid("the work order is not submitted"));
    }
    let item = id(&w, "item");
    let qty: Decimal = produced.iter().filter(|(i, _, _)| Some(*i) == item).map(|(_, q, _)| *q).sum();
    let value: Decimal = produced.iter().map(|(_, _, v)| *v).sum();
    let new_qty = d(&w, "produced_qty") + sign * qty;
    if new_qty > d(&w, "qty") {
        return Err(Error::invalid(format!(
            "this would produce {} of {} ordered on work order {}",
            new_qty.normalize(),
            d(&w, "qty").normalize(),
            s(&w, "number").unwrap_or_default()
        )));
    }
    if new_qty < Decimal::ZERO {
        return Err(Error::internal("produced quantity would become negative"));
    }
    let status = if s(&w, "status") == Some("Stopped") && sign < Decimal::ZERO {
        "Stopped"
    } else if new_qty >= d(&w, "qty") {
        "Completed"
    } else if new_qty > Decimal::ZERO {
        "In Process"
    } else {
        "Not Started"
    };
    let mut v = Doc::new();
    v.insert("produced_qty".into(), money(new_qty));
    v.insert("actual_cost".into(), money(d(&w, "actual_cost") + sign * value));
    v.insert("status".into(), json!(status));
    hc.kernel.set_values_tx(&sctx, hc.tx, "WorkOrder", wo, v).await
}

pub struct WorkOrderHook;

#[async_trait]
impl DocHook for WorkOrderHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if let (None, Some(item), Some(cid)) = (id(doc, "bom"), id(doc, "item"), id(doc, "company"))
            && let Some(bom) = default_bom(hc, cid, item).await?
        {
            set(doc, "bom", bom.to_string());
        }
        if let (None, Some(bom)) = (id(doc, "item"), id(doc, "bom")) {
            let bdoc = get(hc.kernel, hc.ctx, hc.tx, "Bom", bom).await?;
            set(doc, "item", bdoc.get("item").cloned().unwrap_or(Value::Null));
        }
        if d(doc, "docstatus").is_zero() {
            set(doc, "status", "Draft");
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let Some(bom) = id(doc, "bom") else { return Err(Error::field("bom", "no active BOM exists for this item")) };
        let bdoc = get(hc.kernel, hc.ctx, hc.tx, "Bom", bom).await?;
        if d(&bdoc, "docstatus") != Decimal::ONE || !b(&bdoc, "is_active") {
            return Err(Error::field("bom", "must be a submitted, active BOM"));
        }
        if id(&bdoc, "item") != id(doc, "item") {
            return Err(Error::field("bom", "is for a different item"));
        }
        if id(&bdoc, "company") != id(doc, "company") {
            return Err(Error::field("bom", "belongs to another company"));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Not Started"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "WorkOrder", id(doc, "id").unwrap_or_default(), v).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if d(doc, "produced_qty") > Decimal::ZERO {
            return Err(Error::invalid("goods were produced on this work order; cancel its manufacture entries first, or stop it instead"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Cancelled"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "WorkOrder", id(doc, "id").unwrap_or_default(), v).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let wo = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let sctx = sys(hc.ctx);
        match name {
            "stop" | "resume" => {
                let status = if name == "stop" {
                    if s(doc, "status") == Some("Completed") {
                        return Err(Error::invalid("the work order is already completed"));
                    }
                    "Stopped"
                } else if d(doc, "produced_qty") > Decimal::ZERO {
                    "In Process"
                } else {
                    "Not Started"
                };
                let mut v = Doc::new();
                v.insert("status".into(), json!(status));
                hc.kernel.set_values_tx(&sctx, hc.tx, "WorkOrder", wo, v).await?;
                Ok(Some(json!({"status": status})))
            }
            "manufacture" => {
                if s(doc, "status") == Some("Stopped") {
                    return Err(Error::invalid("the work order is stopped; resume it first"));
                }
                let remaining = d(doc, "qty") - d(doc, "produced_qty");
                let qty = match params.get("qty") {
                    Some(v) if !v.is_null() => v
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                        .parse::<Decimal>()
                        .map_err(|_| Error::field("qty", "is not a number"))?,
                    _ => remaining,
                };
                if qty <= Decimal::ZERO || qty > remaining {
                    return Err(Error::field("qty", format!("must be between 0 and the remaining {}", remaining.normalize())));
                }
                let bom = id(doc, "bom").ok_or_else(|| Error::field("bom", "is required"))?;
                let bdoc = get(hc.kernel, hc.ctx, hc.tx, "Bom", bom).await?;
                let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
                let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
                let per = if d(&bdoc, "quantity").is_zero() { Decimal::ONE } else { d(&bdoc, "quantity") };
                let mut items = Vec::new();
                for r in rows(&bdoc, "items") {
                    let need = (d(&r, "qty") / per * qty * (Decimal::ONE + d(&r, "scrap_percent") / Decimal::ONE_HUNDRED)).round_dp(9);
                    items.push(json!({
                        "item": r.get("item"),
                        "qty": need.normalize().to_string(),
                        "source_warehouse": r.get("source_warehouse").filter(|v| !v.is_null()).or(doc.get("source_warehouse")),
                    }));
                }
                items.push(json!({"item": doc.get("item"), "qty": qty.normalize().to_string(), "target_warehouse": doc.get("target_warehouse")}));
                let additional = round(d(&bdoc, "operating_cost") / per * qty, co.precision);
                let mut input = Doc::new();
                input.insert("company".into(), json!(cid.to_string()));
                input.insert("purpose".into(), json!("Manufacture"));
                input.insert("mfg_work_order".into(), json!(wo.to_string()));
                input.insert("mfg_additional_cost".into(), money(additional));
                input.insert("items".into(), Value::Array(items));
                if let Some(day) = params.get("posting_date").and_then(Value::as_str) {
                    input.insert("posting_date".into(), json!(day));
                }
                input.insert("remarks".into(), json!(format!("Production for work order {}", s(doc, "number").unwrap_or_default())));
                let ectx = elevated(hc.ctx);
                let entry = hc.kernel.insert_tx(&ectx, hc.tx, "StockEntry", input).await?;
                let eid = id(&entry, "id").ok_or_else(|| Error::internal("stock entry without id"))?;
                // `submit: false` leaves a draft to complete (e.g. batch or serial numbers) before posting.
                let entry = if params.get("submit").and_then(Value::as_bool).unwrap_or(true) {
                    hc.kernel.submit_tx(&ectx, hc.tx, "StockEntry", eid, None).await?
                } else {
                    entry
                };
                Ok(Some(created("StockEntry", &entry)))
            }
            _ => Ok(None),
        }
    }
}

// ───────────────────────────── MRP ─────────────────────────────

#[derive(sqlx::FromRow)]
struct ItemPlan {
    id: Uuid,
    item_name: Option<String>,
    safety_stock: Option<Decimal>,
    lead_time_days: Option<i64>,
    min_order_qty: Option<Decimal>,
    is_purchased: Option<bool>,
    is_manufactured: Option<bool>,
}

pub struct MrpRunHook;

impl MrpRunHook {
    async fn run(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
        let tenant = hc.ctx.tenant;
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let wh = id(doc, "warehouse").ok_or_else(|| Error::field("warehouse", "is required"))?;
        let today = hc.ctx.today();
        let mut gross: HashMap<Uuid, Decimal> = HashMap::new();
        let mut need_by: HashMap<Uuid, NaiveDate> = HashMap::new();
        let add = |gross: &mut HashMap<Uuid, Decimal>, need_by: &mut HashMap<Uuid, NaiveDate>, item: Uuid, qty: Decimal, by: NaiveDate| {
            *gross.entry(item).or_default() += qty;
            need_by.entry(item).and_modify(|d| *d = (*d).min(by)).or_insert(by);
        };

        if b(doc, "include_sales_orders") {
            let lines = child_table(hc, "SalesOrder", "items")?;
            let until = s(doc, "demand_until").map(str::to_string);
            let demand: Vec<(Uuid, Decimal, NaiveDate)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "select l.item, sum((l.qty - coalesce(l.delivered_qty, 0)) * coalesce(nullif(l.conversion_factor, 0), 1)), \
                 min(coalesce(l.delivery_date, o.delivery_date, o.posting_date)) \
                 from e_sales_order o join {} l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
                 where o.tenant_id = $1 and o.company = $2 and o.docstatus = 1 and o.status not in ('Closed', 'Completed') \
                 and l.qty > coalesce(l.delivered_qty, 0) and ($3::date is null or coalesce(l.delivery_date, o.delivery_date, o.posting_date) <= $3::date) \
                 group by l.item",
                q(&lines)
            )))
            .bind(tenant)
            .bind(cid)
            .bind(until)
            .fetch_all(&mut **hc.tx)
            .await?;
            for (item, qty, by) in demand {
                add(&mut gross, &mut need_by, item, qty, by);
            }
        }
        // Materials still to be consumed by open work orders are demand too.
        let open_wos: Vec<(Uuid, Decimal, NaiveDate)> = sqlx::query_as(
            "select bom, qty - coalesce(produced_qty, 0), planned_start_date from e_work_order where tenant_id = $1 and company = $2 \
             and docstatus = 1 and status in ('Not Started', 'In Process') and qty > coalesce(produced_qty, 0)",
        )
        .bind(tenant)
        .bind(cid)
        .fetch_all(&mut **hc.tx)
        .await?;
        for (bom, remaining, by) in open_wos {
            let (_, comps) = components(hc, bom).await?;
            for (c, per_unit) in comps {
                add(&mut gross, &mut need_by, c, per_unit * remaining, by);
            }
        }

        // Low-level codes: an item is planned only after every parent that can demand it.
        let mut bom_of: HashMap<Uuid, Option<(Uuid, Vec<(Uuid, Decimal)>)>> = HashMap::new();
        let mut level: HashMap<Uuid, usize> = gross.keys().map(|i| (*i, 0)).collect();
        let mut stack: Vec<(Uuid, usize)> = level.iter().map(|(i, l)| (*i, *l)).collect();
        while let Some((item, l)) = stack.pop() {
            if l > MAX_BOM_DEPTH {
                return Err(Error::invalid("the BOM structure is too deep or circular"));
            }
            if let std::collections::hash_map::Entry::Vacant(e) = bom_of.entry(item) {
                let entry = match default_bom(hc, cid, item).await? {
                    Some(bom) => Some((bom, components(hc, bom).await?.1)),
                    None => None,
                };
                e.insert(entry);
            }
            for (c, _) in bom_of[&item].as_ref().map(|(_, cs)| cs.clone()).unwrap_or_default() {
                if level.get(&c).is_none_or(|cl| *cl < l + 1) {
                    level.insert(c, l + 1);
                    stack.push((c, l + 1));
                }
            }
        }
        let all: Vec<Uuid> = level.keys().copied().collect();
        let plans: HashMap<Uuid, ItemPlan> = sqlx::query_as::<_, ItemPlan>(
            "select id, item_name, safety_stock, lead_time_days::int8 as lead_time_days, min_order_qty, is_purchased, is_manufactured \
             from e_item where tenant_id = $1 and id = any($2)",
        )
        .bind(tenant)
        .bind(&all)
        .fetch_all(&mut **hc.tx)
        .await?
        .into_iter()
        .map(|p| (p.id, p))
        .collect();
        let available: HashMap<Uuid, Decimal> =
            sqlx::query_as::<_, (Uuid, Decimal)>("select item_id, qty from stock_balance where tenant_id = $1 and warehouse_id = $2 and item_id = any($3)")
                .bind(tenant)
                .bind(wh)
                .bind(&all)
                .fetch_all(&mut **hc.tx)
                .await?
                .into_iter()
                .collect();
        let po_lines = child_table(hc, "PurchaseOrder", "items")?;
        let mut on_order: HashMap<Uuid, Decimal> = sqlx::query_as::<_, (Uuid, Decimal)>(sqlx::AssertSqlSafe(format!(
            "select l.item, sum(l.qty - coalesce(l.received_qty, 0)) from e_purchase_order o join {} l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
             where o.tenant_id = $1 and o.company = $2 and o.docstatus = 1 and o.status not in ('Closed', 'Completed') \
             and l.qty > coalesce(l.received_qty, 0) and l.item = any($3) group by l.item",
            q(&po_lines)
        )))
        .bind(tenant)
        .bind(cid)
        .bind(&all)
        .fetch_all(&mut **hc.tx)
        .await?
        .into_iter()
        .collect();
        let wo_supply: Vec<(Uuid, Decimal)> = sqlx::query_as(
            "select item, sum(qty - coalesce(produced_qty, 0)) from e_work_order where tenant_id = $1 and company = $2 and docstatus = 1 \
             and status in ('Not Started', 'In Process') and item = any($3) group by item",
        )
        .bind(tenant)
        .bind(cid)
        .bind(&all)
        .fetch_all(&mut **hc.tx)
        .await?;
        for (i, qty) in wo_supply {
            *on_order.entry(i).or_default() += qty;
        }
        let suppliers_table = child_table(hc, "Item", "suppliers")?;
        let suppliers: HashMap<Uuid, Uuid> = sqlx::query_as::<_, (Uuid, Uuid)>(sqlx::AssertSqlSafe(format!(
            "select distinct on (parent_id) parent_id, supplier from {} where tenant_id = $1 and parent_id = any($2) and supplier is not null \
             and coalesce(approved, true) order by parent_id, priority nulls last, idx",
            q(&suppliers_table)
        )))
        .bind(tenant)
        .bind(&all)
        .fetch_all(&mut **hc.tx)
        .await?
        .into_iter()
        .collect();

        let mut by_level: BTreeMap<usize, Vec<Uuid>> = BTreeMap::new();
        for (i, l) in &level {
            by_level.entry(*l).or_default().push(*i);
        }
        let mut lines = Vec::new();
        for (_, mut items) in by_level {
            items.sort();
            for item in items {
                let g = gross.get(&item).copied().unwrap_or_default();
                let plan = plans.get(&item);
                let safety = plan.and_then(|p| p.safety_stock).unwrap_or_default();
                let avail = available.get(&item).copied().unwrap_or_default().max(Decimal::ZERO);
                let incoming = on_order.get(&item).copied().unwrap_or_default();
                let net = g + safety - avail - incoming;
                if net <= Decimal::ZERO {
                    continue;
                }
                let suggested = net.max(plan.and_then(|p| p.min_order_qty).unwrap_or_default()).round_dp(9);
                let lead = plan.and_then(|p| p.lead_time_days).unwrap_or(0).max(0);
                let due = need_by.get(&item).copied().unwrap_or(today);
                let start = (due - chrono::Duration::days(lead)).max(today);
                let bom = bom_of.get(&item).and_then(|b| b.clone());
                let manufacture = bom.is_some() && plan.is_some_and(|p| p.is_manufactured.unwrap_or(false) || !p.is_purchased.unwrap_or(true));
                if manufacture && let Some((_, comps)) = &bom {
                    for (c, per_unit) in comps {
                        add(&mut gross, &mut need_by, *c, per_unit * suggested, start);
                    }
                }
                lines.push(json!({
                    "item": item.to_string(),
                    "item_name": plan.and_then(|p| p.item_name.clone()),
                    "action": if manufacture { "Manufacture" } else { "Purchase" },
                    "gross_requirement": g.normalize().to_string(),
                    "available": avail.normalize().to_string(),
                    "on_order": incoming.normalize().to_string(),
                    "safety_stock": safety.normalize().to_string(),
                    "net_requirement": net.normalize().to_string(),
                    "suggested_qty": suggested.normalize().to_string(),
                    "supplier": (!manufacture).then(|| suppliers.get(&item).map(|s| s.to_string())).flatten(),
                    "warehouse": wh.to_string(),
                    "required_by": start.to_string(),
                }));
            }
        }
        let count = lines.len();
        let mut patch = Doc::new();
        patch.insert("lines".into(), Value::Array(lines));
        patch.insert("status".into(), json!("Planned"));
        hc.kernel.update_tx(&elevated(hc.ctx), hc.tx, "MrpRun", id(doc, "id").unwrap_or_default(), patch, None).await?;
        Ok(json!({"lines": count}))
    }

    async fn create_orders(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
        let mid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let lines: Vec<Doc> = rows(doc, "lines").into_iter().filter(|l| d(l, "suggested_qty") > Decimal::ZERO && s(l, "created_document").is_none()).collect();
        if lines.is_empty() {
            return Err(Error::invalid("there is nothing left to order; run MRP again"));
        }
        let missing: Vec<String> = lines
            .iter()
            .filter(|l| s(l, "action") == Some("Purchase") && id(l, "supplier").is_none())
            .map(|l| s(l, "item_name").unwrap_or("?").to_string())
            .collect();
        if !missing.is_empty() {
            return Err(Error::invalid(format!("set a supplier on the MRP lines for: {}", missing.join(", "))));
        }
        let sctx = sys(hc.ctx);
        let mut created_docs = Vec::new();
        let mut by_supplier: BTreeMap<Uuid, Vec<&Doc>> = BTreeMap::new();
        for l in lines.iter().filter(|l| s(l, "action") == Some("Purchase")) {
            by_supplier.entry(id(l, "supplier").unwrap_or_default()).or_default().push(l);
        }
        for (supplier, ls) in by_supplier {
            let items: Vec<Value> = ls
                .iter()
                .map(|l| json!({"item": l.get("item"), "qty": l.get("suggested_qty"), "warehouse": l.get("warehouse"), "schedule_date": l.get("required_by")}))
                .collect();
            let mut input = Doc::new();
            input.insert("company".into(), json!(cid.to_string()));
            input.insert("supplier".into(), json!(supplier.to_string()));
            input.insert("items".into(), Value::Array(items));
            let po = hc.kernel.insert_tx(hc.ctx, hc.tx, "PurchaseOrder", input).await?;
            let label = format!("PurchaseOrder {}", s(&po, "number").or(s(&po, "id")).unwrap_or_default());
            for l in ls {
                let mut v = Doc::new();
                v.insert("created_document".into(), json!(label));
                hc.kernel.set_child_values_tx(&sctx, hc.tx, "MrpRun", "lines", id(l, "id").unwrap_or_default(), v).await?;
            }
            created_docs.push(created("PurchaseOrder", &po));
        }
        for l in lines.iter().filter(|l| s(l, "action") == Some("Manufacture")) {
            let mut input = Doc::new();
            input.insert("company".into(), json!(cid.to_string()));
            input.insert("item".into(), l.get("item").cloned().unwrap_or(Value::Null));
            input.insert("qty".into(), l.get("suggested_qty").cloned().unwrap_or(Value::Null));
            input.insert("source_warehouse".into(), l.get("warehouse").cloned().unwrap_or(Value::Null));
            input.insert("target_warehouse".into(), l.get("warehouse").cloned().unwrap_or(Value::Null));
            if let Some(day) = s(l, "required_by") {
                input.insert("planned_start_date".into(), json!(day));
            }
            let wo = hc.kernel.insert_tx(hc.ctx, hc.tx, "WorkOrder", input).await?;
            let mut v = Doc::new();
            v.insert("created_document".into(), json!(format!("WorkOrder {}", s(&wo, "number").or(s(&wo, "id")).unwrap_or_default())));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "MrpRun", "lines", id(l, "id").unwrap_or_default(), v).await?;
            created_docs.push(created("WorkOrder", &wo));
        }
        let mut v = Doc::new();
        v.insert("status".into(), json!("Orders Created"));
        hc.kernel.set_values_tx(&sctx, hc.tx, "MrpRun", mid, v).await?;
        Ok(json!({"created": created_docs}))
    }
}

#[async_trait]
impl DocHook for MrpRunHook {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        match name {
            "run" => Ok(Some(self.run(hc, doc).await?)),
            "create_orders" => Ok(Some(self.create_orders(hc, doc).await?)),
            _ => Ok(None),
        }
    }
}
