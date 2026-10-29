//! Production plans: items to make from open sales orders, material requests (when the
//! Procurement app is installed) or typed in; a multi-level BOM explosion into sub-assemblies and
//! raw materials netted against projected stock; then work orders (one per item and
//! sub-assembly) and material requests — or, without the Procurement app, purchase order drafts
//! per default supplier. Quantities ordered and produced are kept current from the work orders.

use crate::{cached, rows_value, table_of};
use async_trait::async_trait;
use ferrum_domain::flow::created;
use ferrum_domain::manufacturing::default_bom;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{b, d, elevated, get, id, money, rows, s, set, sys, to_stock, unit_factor};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Op, Result};
use ferrum_meta::{EffEntity, FieldType};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

const MAX_DEPTH: i64 = 30;

/// Recomputes the work order quantities on a plan's rows, its produced total and its status.
pub(crate) async fn refresh_plan(hc: &mut HookCtx<'_>, plan: Uuid, except_wo: Option<Uuid>) -> Result<()> {
    let sctx = sys(hc.ctx);
    let doc = hc.kernel.get_tx(&sctx, hc.tx, "ProductionPlan", plan, false).await?;
    let made: HashMap<String, (Decimal, Decimal)> = sqlx::query_as::<_, (String, Decimal, Decimal)>(
        "select ext->>'mfo_production_plan_item', sum(qty), sum(coalesce(produced_qty, 0)) from e_work_order where tenant_id = $1 \
         and ext->>'mfo_production_plan' = $2 and docstatus < 2 and id <> $3 and ext->>'mfo_production_plan_item' is not null group by 1",
    )
    .bind(hc.ctx.tenant)
    .bind(plan.to_string())
    .bind(except_wo.unwrap_or_default())
    .fetch_all(&mut **hc.tx)
    .await?
    .into_iter()
    .map(|(k, q, p)| (k, (q, p)))
    .collect();
    let (mut any_ordered, mut complete, mut produced_total) = (false, true, Decimal::ZERO);
    for table in ["po_items", "sub_assemblies"] {
        for r in rows(&doc, table) {
            let (ordered, produced) = s(&r, "id").and_then(|k| made.get(k)).copied().unwrap_or_default();
            any_ordered |= ordered > Decimal::ZERO;
            if table == "po_items" {
                produced_total += produced;
                complete &= produced >= d(&r, "planned_qty");
            }
            if d(&r, "ordered_qty") != ordered || d(&r, "produced_qty") != produced || r.get("ordered_qty").is_none_or(Value::is_null) {
                let mut v = Doc::new();
                v.insert("ordered_qty".into(), money(ordered));
                v.insert("produced_qty".into(), money(produced));
                hc.kernel.set_child_values_tx(&sctx, hc.tx, "ProductionPlan", table, id(&r, "id").unwrap_or_default(), v).await?;
            }
        }
    }
    let mut v = Doc::new();
    v.insert("total_produced_qty".into(), money(produced_total));
    if d(&doc, "docstatus") == Decimal::ONE {
        let requested = rows(&doc, "raw_materials").iter().any(|r| s(r, "order_id").is_some());
        let status = if complete && !rows(&doc, "po_items").is_empty() {
            "Completed"
        } else if any_ordered || produced_total > Decimal::ZERO {
            "In Process"
        } else if requested {
            "Material Requested"
        } else {
            "Submitted"
        };
        v.insert("status".into(), json!(status));
    }
    hc.kernel.set_values_tx(&sctx, hc.tx, "ProductionPlan", plan, v).await
}

/// Quantity of a sales order or material request line already planned by other submitted plans.
async fn planned_elsewhere(hc: &mut HookCtx<'_>, plan: Uuid, line: &str) -> Result<Decimal> {
    let table = table_of(hc.ctx, "ProductionPlan", "po_items")?;
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select coalesce(sum(i.planned_qty), 0) from {table} i join e_production_plan p on p.tenant_id = i.tenant_id and p.id = i.parent_id \
         where i.tenant_id = $1 and p.docstatus = 1 and p.id <> $2 and (i.so_line = $3 or i.mr_line = $3)"
    )))
    .bind(hc.ctx.tenant)
    .bind(plan)
    .bind(line)
    .fetch_one(&mut **hc.tx)
    .await?)
}

/// The Procurement app's material request entity, when installed.
fn material_request(hc: &HookCtx<'_>) -> Result<std::sync::Arc<EffEntity>> {
    hc.ctx.schema.entity("MaterialRequest").cloned().ok_or_else(|| Error::invalid("material requests come with the Procurement app, which is not installed"))
}

/// The table field of material request lines (`item` + `qty`) and its row entity.
fn mr_lines(hc: &HookCtx<'_>, mr: &EffEntity) -> Result<(String, std::sync::Arc<EffEntity>)> {
    for f in mr.table_fields() {
        if let Some(child) = f.of.as_deref().and_then(|c| hc.ctx.schema.entity(c))
            && child.field("item").is_some_and(|x| x.ftype == FieldType::Link)
            && child.field("qty").is_some()
        {
            return Ok((f.name.clone(), child.clone()));
        }
    }
    Err(Error::invalid("material requests have no item lines this plan understands"))
}

/// The material request's type field and whether `value` is one of its options.
fn mr_type_field(mr: &EffEntity, value: &str) -> Option<String> {
    mr.def
        .fields
        .iter()
        .find(|f| f.ftype == FieldType::Select && (f.name.contains("type") || f.name == "purpose") && f.options.iter().any(|o| o == value))
        .map(|f| f.name.clone())
}

fn first_field<'a>(ent: &EffEntity, names: &[&'a str]) -> Option<&'a str> {
    names.iter().copied().find(|n| ent.field(n).is_some())
}

/// A BOM explosion: raw materials and sub-assemblies (item, BOM) with quantities and levels.
#[derive(Default)]
struct Explosion {
    raw: BTreeMap<Uuid, Decimal>,
    subs: BTreeMap<(Uuid, Uuid), (Decimal, i64)>,
}

/// Whether an item is made rather than bought (the platform MRP's rule).
fn is_made(item: &Doc) -> bool {
    b(item, "is_manufactured") || !item.get("is_purchased").and_then(Value::as_bool).unwrap_or(true)
}

/// Explodes BOMs level by level: components that are made and have a BOM are sub-assemblies
/// (exploded further), the rest are raw materials. Scrap is included.
async fn explode(hc: &mut HookCtx<'_>, company: Uuid, roots: &[(Uuid, Decimal)]) -> Result<Explosion> {
    let mut out = Explosion::default();
    let (mut boms, mut items) = (HashMap::new(), HashMap::new());
    let mut subs_of: HashMap<Uuid, Option<Uuid>> = HashMap::new();
    let mut stack: Vec<(Uuid, Decimal, i64)> = roots.iter().map(|(bom, q)| (*bom, *q, 1)).collect();
    while let Some((bom, qty, level)) = stack.pop() {
        if level > MAX_DEPTH {
            return Err(Error::invalid(format!("the bill of materials is deeper than {MAX_DEPTH} levels")));
        }
        let bdoc = cached(hc, &mut boms, "Bom", bom).await?;
        let per = if d(&bdoc, "quantity").is_zero() { Decimal::ONE } else { d(&bdoc, "quantity") };
        for c in rows(&bdoc, "items") {
            let Some(item) = id(&c, "item") else { continue };
            let need = (d(&c, "qty") / per * qty * (Decimal::ONE + d(&c, "scrap_percent") / Decimal::ONE_HUNDRED)).round_dp(9);
            if let std::collections::hash_map::Entry::Vacant(e) = subs_of.entry(item) {
                let it = cached(hc, &mut items, "Item", item).await?;
                e.insert(if is_made(&it) { default_bom(hc, company, item).await? } else { None });
            }
            match subs_of[&item] {
                Some(sub) => {
                    let e = out.subs.entry((item, sub)).or_insert((Decimal::ZERO, level));
                    e.0 += need;
                    e.1 = e.1.max(level);
                    stack.push((sub, need, level + 1));
                }
                None => *out.raw.entry(item).or_default() += need,
            }
        }
    }
    Ok(out)
}

/// Stock in, coming into and committed out of `warehouses` per item: actual + open purchase
/// orders + open work orders' output − open work orders' remaining material needs.
async fn projected(hc: &mut HookCtx<'_>, company: Uuid, warehouses: &[Uuid], items: &[Uuid]) -> Result<HashMap<Uuid, (Decimal, Decimal)>> {
    let tenant = hc.ctx.tenant;
    let actual: HashMap<Uuid, Decimal> = sqlx::query_as::<_, (Uuid, Decimal)>(
        "select item_id, sum(qty) from stock_balance where tenant_id = $1 and warehouse_id = any($2) and item_id = any($3) group by item_id",
    )
    .bind(tenant)
    .bind(warehouses)
    .bind(items)
    .fetch_all(&mut **hc.tx)
    .await?
    .into_iter()
    .collect();
    let po_lines = table_of(hc.ctx, "PurchaseOrder", "items")?;
    let ordered: Vec<(Uuid, Decimal)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select l.item, sum(l.qty - coalesce(l.received_qty, 0)) from e_purchase_order o join {po_lines} l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
         where o.tenant_id = $1 and o.company = $2 and o.docstatus = 1 and o.status not in ('Closed', 'Completed') and l.warehouse = any($3) \
         and l.item = any($4) and l.qty > coalesce(l.received_qty, 0) group by l.item"
    )))
    .bind(tenant)
    .bind(company)
    .bind(warehouses)
    .bind(items)
    .fetch_all(&mut **hc.tx)
    .await?;
    let planned: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "select item, sum(qty - coalesce(produced_qty, 0)) from e_work_order where tenant_id = $1 and company = $2 and docstatus = 1 \
         and status in ('Not Started', 'In Process') and target_warehouse = any($3) and item = any($4) group by item",
    )
    .bind(tenant)
    .bind(company)
    .bind(warehouses)
    .bind(items)
    .fetch_all(&mut **hc.tx)
    .await?;
    let open_wos: Vec<(Uuid, Decimal, Option<Uuid>)> = sqlx::query_as(
        "select bom, qty - coalesce(produced_qty, 0), source_warehouse from e_work_order where tenant_id = $1 and company = $2 and docstatus = 1 \
         and status in ('Not Started', 'In Process') and qty > coalesce(produced_qty, 0)",
    )
    .bind(tenant)
    .bind(company)
    .fetch_all(&mut **hc.tx)
    .await?;
    let wanted: HashSet<Uuid> = items.iter().copied().collect();
    let mut net: HashMap<Uuid, (Decimal, Decimal)> = items.iter().map(|i| (*i, (actual.get(i).copied().unwrap_or_default(), Decimal::ZERO))).collect();
    for (i, q) in ordered.into_iter().chain(planned) {
        if let Some(e) = net.get_mut(&i) {
            e.1 += q;
        }
    }
    let mut boms = HashMap::new();
    for (bom, remaining, source) in open_wos {
        let bdoc = cached(hc, &mut boms, "Bom", bom).await?;
        let per = if d(&bdoc, "quantity").is_zero() { Decimal::ONE } else { d(&bdoc, "quantity") };
        for c in rows(&bdoc, "items") {
            let (Some(item), Some(wh)) = (id(&c, "item"), id(&c, "source_warehouse").or(source)) else { continue };
            if wanted.contains(&item) && warehouses.contains(&wh) {
                let need = d(&c, "qty") / per * remaining * (Decimal::ONE + d(&c, "scrap_percent") / Decimal::ONE_HUNDRED);
                if let Some(e) = net.get_mut(&item) {
                    e.1 -= need.round_dp(9);
                }
            }
        }
    }
    // (actual, actual + incoming − committed)
    Ok(net.into_iter().map(|(i, (a, delta))| (i, (a, a + delta))).collect())
}

/// The first approved supplier of an item, by priority.
fn default_supplier(item: &Doc) -> Option<Uuid> {
    let mut list: Vec<Doc> = rows(item, "suppliers").into_iter().filter(|r| r.get("approved").and_then(Value::as_bool).unwrap_or(true)).collect();
    list.sort_by_key(|r| r.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX));
    list.iter().find_map(|r| id(r, "supplier"))
}

pub struct ProductionPlanHook;

impl ProductionPlanHook {
    async fn patch(&self, hc: &mut HookCtx<'_>, plan: Uuid, patch: Doc) -> Result<()> {
        hc.kernel.update_tx(&elevated(hc.ctx), hc.tx, "ProductionPlan", plan, patch, None).await.map(|_| ())
    }

    async fn get_sales_orders(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
        let lines = table_of(hc.ctx, "SalesOrder", "items")?;
        let found: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select o.id from e_sales_order o where o.tenant_id = $1 and o.company = $2 and o.docstatus = 1 and o.status not in ('Closed', 'Completed') \
             and ($3::uuid is null or o.customer = $3) and ($4::date is null or o.posting_date >= $4::date) and ($5::date is null or o.posting_date <= $5::date) \
             and exists (select 1 from {lines} l join e_bom m on m.tenant_id = l.tenant_id and m.company = o.company and m.item = l.item \
                         and m.docstatus = 1 and m.is_active where l.tenant_id = o.tenant_id and l.parent_id = o.id and l.qty > coalesce(l.delivered_qty, 0)) \
             order by o.posting_date, o.number"
        )))
        .bind(hc.ctx.tenant)
        .bind(id(doc, "company"))
        .bind(id(doc, "customer"))
        .bind(s(doc, "from_date").map(str::to_string))
        .bind(s(doc, "to_date").map(str::to_string))
        .fetch_all(&mut **hc.tx)
        .await?;
        let mut table = rows(doc, "sales_orders");
        let have: HashSet<Uuid> = table.iter().filter_map(|r| id(r, "sales_order")).collect();
        let added: Vec<Uuid> = found.into_iter().filter(|o| !have.contains(o)).collect();
        table.extend(added.iter().map(|o| Doc::from_iter([("sales_order".to_string(), json!(o.to_string()))])));
        let mut p = Doc::new();
        p.insert("sales_orders".into(), rows_value(table));
        self.patch(hc, id(doc, "id").unwrap_or_default(), p).await?;
        Ok(json!({"added": added.len()}))
    }

    async fn get_material_requests(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
        let mr = material_request(hc)?;
        let mut filters = vec![Filter { field: "docstatus".into(), op: Op::Eq, value: json!(1) }];
        if mr.field("company").is_some() {
            filters.push(Filter::eq("company", doc.get("company").cloned().unwrap_or(Value::Null)));
        }
        if let Some(t) = mr_type_field(&mr, "Manufacture") {
            filters.push(Filter::eq(&t, "Manufacture"));
        }
        if let Some(df) = first_field(&mr, &["transaction_date", "posting_date"]) {
            for (k, op) in [("from_date", Op::Gte), ("to_date", Op::Lte)] {
                if let Some(day) = s(doc, k) {
                    filters.push(Filter { field: df.into(), op, value: json!(day) });
                }
            }
        }
        let found = hc.kernel.find_tx(&sys(hc.ctx), hc.tx, "MaterialRequest", &filters, None, 500).await?;
        let mut table = rows(doc, "material_requests");
        let have: HashSet<String> = table.iter().filter_map(|r| s(r, "material_request").map(str::to_string)).collect();
        let mut added = 0;
        for m in found {
            let Some(mid) = s(&m, "id").map(str::to_string) else { continue };
            if !have.contains(&mid) {
                added += 1;
                table.push(Doc::from_iter([
                    ("material_request".to_string(), json!(mid)),
                    ("material_request_number".to_string(), m.get("number").cloned().unwrap_or(Value::Null)),
                ]));
            }
        }
        let mut p = Doc::new();
        p.insert("material_requests".into(), rows_value(table));
        self.patch(hc, id(doc, "id").unwrap_or_default(), p).await?;
        Ok(json!({"added": added}))
    }

    /// Items to make from the listed sales orders and material requests, for what is still open
    /// on them (not delivered or already planned elsewhere). Rows typed by hand stay.
    async fn get_items(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
        let plan = id(doc, "id").unwrap_or_default();
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        if rows(doc, "sales_orders").is_empty() && rows(doc, "material_requests").is_empty() {
            return Err(Error::invalid("list sales orders or material requests first (Get Sales Orders / Get Material Requests)"));
        }
        let mut items: Vec<Doc> = rows(doc, "po_items").into_iter().filter(|r| id(r, "sales_order").is_none() && s(r, "material_request").is_none()).collect();
        let fg = doc.get("fg_warehouse").cloned().unwrap_or(Value::Null);
        for so in rows(doc, "sales_orders").iter().filter_map(|r| id(r, "sales_order")) {
            let order = get(hc.kernel, hc.ctx, hc.tx, "SalesOrder", so).await?;
            if d(&order, "docstatus") != Decimal::ONE || id(&order, "company") != Some(cid) {
                return Err(Error::invalid(format!("sales order {} is not a submitted order of this company", s(&order, "number").unwrap_or_default())));
            }
            for l in rows(&order, "items") {
                let (Some(item), Some(line)) = (id(&l, "item"), s(&l, "id").map(str::to_string)) else { continue };
                let elsewhere = planned_elsewhere(hc, plan, &line).await?;
                // Planned in stock units, whatever unit the order line counts in.
                let f = unit_factor(&l);
                let pending = to_stock(d(&l, "qty"), f) - to_stock(d(&l, "delivered_qty"), f).max(elsewhere);
                if pending <= Decimal::ZERO {
                    continue;
                }
                let Some(bom) = default_bom(hc, cid, item).await? else { continue };
                items.push(Doc::from_iter([
                    ("item".to_string(), json!(item.to_string())),
                    ("bom".to_string(), json!(bom.to_string())),
                    ("planned_qty".to_string(), money(pending)),
                    ("pending_qty".to_string(), money(pending)),
                    ("warehouse".to_string(), fg.clone()),
                    ("sales_order".to_string(), json!(so.to_string())),
                    ("so_line".to_string(), json!(line)),
                ]));
            }
        }
        let mut requests = rows(doc, "material_requests");
        if !requests.is_empty() {
            let mr = material_request(hc)?;
            let (table, line_ent) = mr_lines(hc, &mr)?;
            let done_field = first_field(&line_ent, &["ordered_qty", "received_qty"]);
            for r in requests.iter_mut() {
                let mid: Uuid = s(r, "material_request")
                    .and_then(|x| x.parse().ok())
                    .ok_or_else(|| Error::field("material_requests", "a row has no valid material request id"))?;
                let m = get(hc.kernel, hc.ctx, hc.tx, "MaterialRequest", mid).await?;
                if d(&m, "docstatus") != Decimal::ONE || (mr.field("company").is_some() && id(&m, "company") != Some(cid)) {
                    return Err(Error::invalid(format!("material request {} is not a submitted request of this company", s(&m, "number").unwrap_or_default())));
                }
                set(r, "material_request_number", m.get("number").cloned().unwrap_or(Value::Null));
                for l in rows(&m, &table) {
                    let (Some(item), Some(line)) = (id(&l, "item"), s(&l, "id").map(str::to_string)) else { continue };
                    let done = done_field.map(|f| d(&l, f)).unwrap_or_default();
                    let elsewhere = planned_elsewhere(hc, plan, &line).await?;
                    let pending = d(&l, "qty") - done.max(elsewhere);
                    if pending <= Decimal::ZERO {
                        continue;
                    }
                    let Some(bom) = default_bom(hc, cid, item).await? else { continue };
                    let wh = first_field(&line_ent, &["warehouse", "target_warehouse"])
                        .and_then(|f| l.get(f))
                        .filter(|v| !v.is_null())
                        .cloned()
                        .unwrap_or(fg.clone());
                    items.push(Doc::from_iter([
                        ("item".to_string(), json!(item.to_string())),
                        ("bom".to_string(), json!(bom.to_string())),
                        ("planned_qty".to_string(), money(pending)),
                        ("pending_qty".to_string(), money(pending)),
                        ("warehouse".to_string(), wh),
                        ("material_request".to_string(), json!(mid.to_string())),
                        ("mr_line".to_string(), json!(line)),
                    ]));
                }
            }
        }
        let count = items.len();
        let mut p = Doc::new();
        p.insert("po_items".into(), rows_value(items));
        if !requests.is_empty() {
            p.insert("material_requests".into(), rows_value(requests));
        }
        self.patch(hc, plan, p).await?;
        Ok(json!({"items": count}))
    }

    /// Explodes the planned items' BOMs into sub-assemblies (to make) and raw materials netted
    /// against projected stock in the chosen warehouses: to order = required + safety stock − projected.
    async fn get_raw_materials(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let for_wh = id(doc, "for_warehouse").ok_or_else(|| Error::field("for_warehouse", "is required"))?;
        let mut roots = Vec::new();
        for (i, r) in rows(doc, "po_items").iter().enumerate() {
            let bom = match id(r, "bom") {
                Some(b) => b,
                None => default_bom(hc, cid, id(r, "item").unwrap_or_default())
                    .await?
                    .ok_or_else(|| Error::Validation(vec![FieldError::new("bom", "the item has no active BOM").in_row("po_items", i)]))?,
            };
            roots.push((bom, d(r, "planned_qty")));
        }
        if roots.is_empty() {
            return Err(Error::invalid("add the items to manufacture first"));
        }
        let ex = explode(hc, cid, &roots).await?;
        let mut warehouses: Vec<Uuid> = rows(doc, "warehouses").iter().filter_map(|r| id(r, "warehouse")).collect();
        if warehouses.is_empty() {
            warehouses.push(for_wh);
        }
        let raw_items: Vec<Uuid> = ex.raw.keys().copied().collect();
        let stock = projected(hc, cid, &warehouses, &raw_items).await?;
        let safety = b(doc, "include_safety_stock");
        let start = doc.get("posting_date").cloned().unwrap_or(Value::Null);
        let mut items = HashMap::new();
        let mut materials = Vec::new();
        for (item, required) in &ex.raw {
            let it = cached(hc, &mut items, "Item", *item).await?;
            let (actual, proj) = stock.get(item).copied().unwrap_or_default();
            let ss = if safety { d(&it, "safety_stock") } else { Decimal::ZERO };
            let to_order = (*required + ss - proj).max(Decimal::ZERO);
            materials.push((
                s(&it, "item_code").unwrap_or_default().to_string(),
                Doc::from_iter([
                    ("item".to_string(), json!(item.to_string())),
                    ("required_qty".to_string(), money(*required)),
                    ("actual_qty".to_string(), money(actual)),
                    ("projected_qty".to_string(), money(proj)),
                    ("safety_stock".to_string(), money(ss)),
                    ("qty_to_order".to_string(), money(to_order)),
                    ("supplier".to_string(), default_supplier(&it).map(|x| json!(x.to_string())).unwrap_or(Value::Null)),
                    ("required_by".to_string(), start.clone()),
                ]),
            ));
        }
        materials.sort_by(|a, b| a.0.cmp(&b.0));
        let mut subs: Vec<(i64, Uuid, Doc)> = ex
            .subs
            .iter()
            .map(|((item, bom), (qty, level))| {
                (
                    *level,
                    *item,
                    Doc::from_iter([
                        ("item".to_string(), json!(item.to_string())),
                        ("bom".to_string(), json!(bom.to_string())),
                        ("qty".to_string(), money(*qty)),
                        ("bom_level".to_string(), json!(level)),
                        ("warehouse".to_string(), json!(for_wh.to_string())),
                        ("planned_start_date".to_string(), start.clone()),
                    ]),
                )
            })
            .collect();
        subs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let (n_raw, n_sub) = (materials.len(), subs.len());
        let mut p = Doc::new();
        p.insert("raw_materials".into(), rows_value(materials.into_iter().map(|(_, r)| r).collect()));
        p.insert("sub_assemblies".into(), rows_value(subs.into_iter().map(|(_, _, r)| r).collect()));
        self.patch(hc, id(doc, "id").unwrap_or_default(), p).await?;
        Ok(json!({"raw_materials": n_raw, "sub_assemblies": n_sub}))
    }

    /// Draft work orders for whatever no work order covers yet: sub-assemblies (deepest first),
    /// then the planned items.
    async fn make_work_orders(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
        let plan = id(doc, "id").unwrap_or_default();
        let for_wh = doc.get("for_warehouse").cloned().unwrap_or(Value::Null);
        let start = doc.get("posting_date").cloned().unwrap_or(Value::Null);
        let mut made = Vec::new();
        let mut subs = rows(doc, "sub_assemblies");
        subs.sort_by_key(|r| -r.get("bom_level").and_then(Value::as_i64).unwrap_or(0));
        let mut jobs: Vec<(Doc, Decimal, Value)> = subs
            .into_iter()
            .map(|r| {
                let open = d(&r, "qty") - d(&r, "ordered_qty");
                let target = r.get("warehouse").filter(|v| !v.is_null()).cloned().unwrap_or(for_wh.clone());
                (r, open, target)
            })
            .collect();
        for (i, r) in rows(doc, "po_items").into_iter().enumerate() {
            let open = d(&r, "planned_qty") - d(&r, "ordered_qty");
            let target = r.get("warehouse").filter(|v| !v.is_null()).or(doc.get("fg_warehouse").filter(|v| !v.is_null())).cloned();
            if open > Decimal::ZERO && target.is_none() {
                return Err(Error::Validation(vec![
                    FieldError::new("warehouse", "set a finished goods warehouse on the row or the plan").in_row("po_items", i),
                ]));
            }
            jobs.push((r, open, target.unwrap_or(Value::Null)));
        }
        for (r, open, target) in jobs {
            if open <= Decimal::ZERO {
                continue;
            }
            let mut input = Doc::new();
            input.insert("company".into(), doc.get("company").cloned().unwrap_or(Value::Null));
            input.insert("item".into(), r.get("item").cloned().unwrap_or(Value::Null));
            if let Some(bom) = r.get("bom").filter(|v| !v.is_null()) {
                input.insert("bom".into(), bom.clone());
            }
            input.insert("qty".into(), money(open));
            input.insert("source_warehouse".into(), for_wh.clone());
            input.insert("target_warehouse".into(), target);
            input.insert("planned_start_date".into(), r.get("planned_start_date").filter(|v| !v.is_null()).cloned().unwrap_or(start.clone()));
            if let Some(so) = r.get("sales_order").filter(|v| !v.is_null()) {
                input.insert("sales_order".into(), so.clone());
            }
            input.insert("mfo_production_plan".into(), json!(plan.to_string()));
            input.insert("mfo_production_plan_item".into(), r.get("id").cloned().unwrap_or(Value::Null));
            let wo = create_for_user(hc, "WorkOrder", input).await?;
            made.push(created("WorkOrder", &wo));
        }
        if made.is_empty() {
            return Err(Error::invalid("work orders already cover every planned quantity"));
        }
        refresh_plan(hc, plan, None).await?;
        Ok(json!({"created": made}))
    }

    /// Requests the raw materials still to order: one material request when the Procurement app
    /// is installed, else a draft purchase order per supplier.
    async fn make_material_requests(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
        let plan = id(doc, "id").unwrap_or_default();
        let sctx = sys(hc.ctx);
        let mut todo = Vec::new();
        for r in rows(doc, "raw_materials") {
            if d(&r, "qty_to_order") <= Decimal::ZERO {
                continue;
            }
            // Already requested on a document that still stands?
            if let (Some(entity), Some(oid)) = (s(&r, "order_entity"), id(&r, "order_id"))
                && hc.ctx.schema.entity(entity).is_some()
                && let Ok(existing) = hc.kernel.get_tx(&sctx, hc.tx, entity, oid, false).await
                && d(&existing, "docstatus") < Decimal::TWO
            {
                continue;
            }
            todo.push(r);
        }
        if todo.is_empty() {
            return Err(Error::invalid("no raw material is left to request (run Get Raw Materials before submitting the plan)"));
        }
        let wh = doc.get("for_warehouse").cloned().unwrap_or(Value::Null);
        let mut made = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut marks: Vec<(Uuid, &'static str, Doc)> = Vec::new();
        if hc.ctx.schema.entity("MaterialRequest").is_some() {
            let mr = material_request(hc)?;
            let (table, line_ent) = mr_lines(hc, &mr)?;
            let mut input = Doc::new();
            if mr.field("company").is_some() {
                input.insert("company".into(), doc.get("company").cloned().unwrap_or(Value::Null));
            }
            if let Some(t) = mr_type_field(&mr, "Purchase") {
                input.insert(t, json!("Purchase"));
            }
            let first_by = todo.iter().filter_map(|r| s(r, "required_by")).min().map(|x| json!(x));
            if let (Some(f), Some(by)) = (first_field(&mr, &["schedule_date", "required_by"]), first_by) {
                input.insert(f.into(), by);
            }
            let wh_field = first_field(&line_ent, &["warehouse", "target_warehouse"]);
            let by_field = first_field(&line_ent, &["schedule_date", "required_by"]);
            let lines: Vec<Value> = todo
                .iter()
                .map(|r| {
                    let mut l =
                        Doc::from_iter([("item".to_string(), r.get("item").cloned().unwrap_or(Value::Null)), ("qty".to_string(), money(d(r, "qty_to_order")))]);
                    if let Some(f) = wh_field {
                        l.insert(f.into(), wh.clone());
                    }
                    if let Some(f) = by_field {
                        l.insert(f.into(), r.get("required_by").cloned().unwrap_or(Value::Null));
                    }
                    Value::Object(l)
                })
                .collect();
            input.insert(table, Value::Array(lines));
            let m = create_for_user(hc, "MaterialRequest", input).await?;
            for r in &todo {
                marks.push((id(r, "id").unwrap_or_default(), "MaterialRequest", m.clone()));
            }
            made.push(created("MaterialRequest", &m));
        } else {
            // The row's supplier, else the item's approved supplier now: a submitted plan cannot
            // be edited, so fixing the item is how a missing supplier gets resolved.
            let mut by_supplier: BTreeMap<Uuid, Vec<&Doc>> = BTreeMap::new();
            let mut item_docs = HashMap::new();
            for r in &todo {
                let it = cached(hc, &mut item_docs, "Item", id(r, "item").unwrap_or_default()).await?;
                match id(r, "supplier").or_else(|| default_supplier(&it)) {
                    Some(sup) => by_supplier.entry(sup).or_default().push(r),
                    None => skipped.push(s(&it, "item_code").unwrap_or("?").to_string()),
                }
            }
            if by_supplier.is_empty() {
                return Err(Error::invalid(format!("set an approved supplier on the items to order: {}", skipped.join(", "))));
            }
            for (supplier, list) in by_supplier {
                let lines: Vec<Value> = list
                    .iter()
                    .map(|r| json!({"item": r.get("item"), "qty": money(d(r, "qty_to_order")), "warehouse": wh, "schedule_date": r.get("required_by")}))
                    .collect();
                let mut input = Doc::new();
                input.insert("company".into(), doc.get("company").cloned().unwrap_or(Value::Null));
                input.insert("supplier".into(), json!(supplier.to_string()));
                input.insert("items".into(), Value::Array(lines));
                let po = create_for_user(hc, "PurchaseOrder", input).await?;
                for r in list {
                    marks.push((id(r, "id").unwrap_or_default(), "PurchaseOrder", po.clone()));
                }
                made.push(created("PurchaseOrder", &po));
            }
        }
        for (row, entity, target) in marks {
            let mut v = Doc::new();
            v.insert("order_entity".into(), json!(entity));
            v.insert("order_id".into(), target.get("id").cloned().unwrap_or(Value::Null));
            v.insert("order_number".into(), json!(format!("{entity} {}", s(&target, "number").or(s(&target, "id")).unwrap_or_default())));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "ProductionPlan", "raw_materials", row, v).await?;
        }
        refresh_plan(hc, plan, None).await?;
        Ok(json!({"created": made, "skipped_no_supplier": skipped}))
    }
}

#[async_trait]
impl DocHook for ProductionPlanHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let cid = id(doc, "company");
        let mut items = rows(doc, "po_items");
        for r in items.iter_mut() {
            if id(r, "bom").is_none()
                && let (Some(c), Some(item)) = (cid, id(r, "item"))
                && let Some(bom) = default_bom(hc, c, item).await?
            {
                set(r, "bom", bom.to_string());
            }
            if id(r, "warehouse").is_none() {
                set(r, "warehouse", doc.get("fg_warehouse").cloned().unwrap_or(Value::Null));
            }
        }
        let total: Decimal = items.iter().map(|r| d(r, "planned_qty")).sum();
        set(doc, "po_items", rows_value(items));
        set(doc, "total_planned_qty", money(total));
        set(doc, "status", "Draft");
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let mut boms = HashMap::new();
        for (i, r) in rows(doc, "po_items").iter().enumerate() {
            let Some(bom) = id(r, "bom") else {
                errors.push(FieldError::new("bom", "the item has no active BOM").in_row("po_items", i));
                continue;
            };
            let bd = cached(hc, &mut boms, "Bom", bom).await?;
            if id(&bd, "item") != id(r, "item") {
                errors.push(FieldError::new("bom", "is for another item").in_row("po_items", i));
            } else if id(&bd, "company") != id(doc, "company") || d(&bd, "docstatus") != Decimal::ONE || !b(&bd, "is_active") {
                errors.push(FieldError::new("bom", "must be a submitted, active BOM of this company").in_row("po_items", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        refresh_plan(hc, id(doc, "id").unwrap_or_default(), None).await
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let open: i64 = sqlx::query_scalar("select count(*) from e_work_order where tenant_id = $1 and ext->>'mfo_production_plan' = $2 and docstatus < 2")
            .bind(hc.ctx.tenant)
            .bind(s(doc, "id"))
            .fetch_one(&mut **hc.tx)
            .await?;
        if open > 0 {
            return Err(Error::invalid(format!("{open} work order(s) were made from this plan; cancel or delete them first")));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Cancelled"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "ProductionPlan", id(doc, "id").unwrap_or_default(), v).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        Ok(Some(match name {
            "get_sales_orders" => self.get_sales_orders(hc, doc).await?,
            "get_material_requests" => self.get_material_requests(hc, doc).await?,
            "get_items" => self.get_items(hc, doc).await?,
            "get_raw_materials" => self.get_raw_materials(hc, doc).await?,
            "make_work_orders" => self.make_work_orders(hc, doc).await?,
            "make_material_requests" => self.make_material_requests(hc, doc).await?,
            _ => return Ok(None),
        }))
    }
}
