//! Pick lists: where to pick the stock for sales orders (or for a transfer), by warehouse, batch
//! (earliest expiry first, then oldest receipt, skipping expired and disabled batches) and serial
//! (oldest receipt first). A submitted list holds what it picked: other lists see it as taken until
//! it is delivered, and nothing can be picked beyond the stock on hand or the quantity still to
//! deliver on the order. Delivery notes and transfers made from the list report back what they
//! delivered, which completes it.

use crate::bundle::{PACKED, bundles};
use crate::common::{ItemInfo, child_table, finish_rows, installed, items, rows_value, serials, validation};
use async_trait::async_trait;
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{d, elevated, get, id, money, rows, s, set, stock_qty, sys, to_stock, unit_factor};
use ferrum_kernel::ddl::q;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

pub const ENTITY: &str = "PickList";

/// Something to pick: an item for an order line (or a bundle component of it), or a free quantity.
#[derive(Debug, Clone)]
struct Need {
    item: Uuid,
    qty: Decimal,
    sales_order: Option<Uuid>,
    so_line: Option<String>,
    bundle: Option<Uuid>,
    per_bundle: Option<Decimal>,
    preferred: Option<Uuid>,
}

/// Stock available at one place: a warehouse, maybe a batch, maybe serials (oldest first).
#[derive(Debug, Clone)]
struct Spot {
    warehouse: Uuid,
    batch: Option<String>,
    serials: Vec<String>,
    qty: Decimal,
}

/// Stock held by other submitted, not yet completed pick lists.
#[derive(Default)]
struct Held {
    qty: HashMap<(Uuid, Uuid), Decimal>,
    batch: HashMap<(Uuid, Uuid, String), Decimal>,
    serials: HashSet<(Uuid, String)>,
    /// `(order line, item)` → quantity picked but not delivered.
    order: HashMap<(String, Uuid), Decimal>,
}

async fn held(hc: &mut HookCtx<'_>, exclude: Option<Uuid>, item_ids: &[Uuid]) -> Result<Held> {
    let lines = child_table(hc.ctx, ENTITY, "locations")?;
    let parent = hc.ctx.schema.entity(ENTITY).map(|e| e.table.clone()).unwrap_or_default();
    let found: Vec<(Uuid, Option<Uuid>, Option<String>, Option<String>, Decimal, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select r.item, r.warehouse, r.batch_no, r.serial_no, coalesce(r.picked_qty, r.qty) - coalesce(r.delivered_qty, 0), r.so_line \
         from {} r join {} h on h.tenant_id = r.tenant_id and h.id = r.parent_id \
         where r.tenant_id = $1 and h.docstatus = 1 and h.status = 'Open' and ($2::uuid is null or h.id <> $2) and r.item = any($3)",
        q(&lines),
        q(&parent)
    )))
    .bind(hc.ctx.tenant)
    .bind(exclude)
    .bind(item_ids)
    .fetch_all(&mut **hc.tx)
    .await?;
    let mut out = Held::default();
    for (item, wh, batch, serial_text, open, so_line) in found {
        if open <= Decimal::ZERO {
            continue;
        }
        if let Some(w) = wh {
            *out.qty.entry((item, w)).or_default() += open;
            if let Some(b) = batch.filter(|b| !b.is_empty()) {
                *out.batch.entry((item, w, b)).or_default() += open;
            }
        }
        for sn in serials(serial_text.as_deref()) {
            out.serials.insert((item, sn));
        }
        if let Some(l) = so_line {
            *out.order.entry((l, item)).or_default() += open;
        }
    }
    Ok(out)
}

/// Leaf warehouses of the company under `parent` (all of them without one), by name.
async fn warehouses(hc: &mut HookCtx<'_>, company: Uuid, parent: Option<Uuid>) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "with recursive tree(id, depth) as (select id, 0 from e_warehouse where tenant_id = $1 and id = $3 \
           union all select w.id, t.depth + 1 from e_warehouse w join tree t on w.parent_warehouse = t.id where w.tenant_id = $1 and t.depth < 50) \
         select w.id from e_warehouse w where w.tenant_id = $1 and w.company = $2 and not coalesce(w.is_group, false) and not coalesce(w.disabled, false) \
         and ($3::uuid is null or w.id in (select id from tree)) order by w.warehouse_name",
    )
    .bind(hc.ctx.tenant)
    .bind(company)
    .bind(parent)
    .fetch_all(&mut **hc.tx)
    .await?)
}

/// Where `info` can be picked from, in picking order, net of what other lists hold.
async fn supply(hc: &mut HookCtx<'_>, info: &ItemInfo, whs: &[Uuid], held: &Held) -> Result<Vec<Spot>> {
    let today = hc.ctx.today();
    let mut out = Vec::new();
    if info.serial {
        // Serials oldest receipt first, grouped by warehouse and batch in that order.
        let found: Vec<(String, Uuid, Option<String>, Option<chrono::NaiveDate>, Option<bool>)> = sqlx::query_as(
            "select s.serial, s.warehouse_id, m.batch_no, b.expiry_date, b.disabled from serial_no s \
             left join stock_move m on m.tenant_id = s.tenant_id and m.id = s.last_move_id \
             left join e_batch b on b.tenant_id = s.tenant_id and b.item = s.item_id and b.batch_id = m.batch_no \
             where s.tenant_id = $1 and s.item_id = $2 and s.status = 'in_stock' and s.warehouse_id = any($3) \
             order by b.expiry_date nulls last, m.posting_ts, m.seq, s.serial",
        )
        .bind(hc.ctx.tenant)
        .bind(info.id)
        .bind(whs)
        .fetch_all(&mut **hc.tx)
        .await?;
        for (serial, wh, batch, expiry, disabled) in found {
            if held.serials.contains(&(info.id, serial.clone())) || disabled.unwrap_or(false) || expiry.is_some_and(|e| e < today) {
                continue;
            }
            let batch = if info.batch { batch } else { None };
            match out.iter_mut().find(|x: &&mut Spot| x.warehouse == wh && x.batch == batch) {
                Some(spot) => {
                    spot.qty += Decimal::ONE;
                    spot.serials.push(serial);
                }
                None => out.push(Spot { warehouse: wh, batch, serials: vec![serial], qty: Decimal::ONE }),
            }
        }
    } else if info.batch {
        let found: Vec<(Uuid, String, Decimal, Option<chrono::NaiveDate>, Option<bool>)> = sqlx::query_as(
            "select m.warehouse_id, m.batch_no, sum(m.qty), b.expiry_date, b.disabled from stock_move m \
             left join e_batch b on b.tenant_id = m.tenant_id and b.item = m.item_id and b.batch_id = m.batch_no \
             where m.tenant_id = $1 and m.item_id = $2 and m.warehouse_id = any($3) and m.batch_no is not null \
             group by m.warehouse_id, m.batch_no, b.expiry_date, b.disabled having sum(m.qty) > 0 \
             order by b.expiry_date nulls last, min(m.posting_ts), m.batch_no",
        )
        .bind(hc.ctx.tenant)
        .bind(info.id)
        .bind(whs)
        .fetch_all(&mut **hc.tx)
        .await?;
        for (wh, batch, qty, expiry, disabled) in found {
            if disabled.unwrap_or(false) || expiry.is_some_and(|e| e < today) {
                continue;
            }
            let free = qty - held.batch.get(&(info.id, wh, batch.clone())).copied().unwrap_or_default();
            if free > Decimal::ZERO {
                out.push(Spot { warehouse: wh, batch: Some(batch), serials: vec![], qty: free });
            }
        }
    } else {
        let found: Vec<(Uuid, Decimal)> =
            sqlx::query_as("select warehouse_id, qty from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = any($3) and qty > 0")
                .bind(hc.ctx.tenant)
                .bind(info.id)
                .bind(whs)
                .fetch_all(&mut **hc.tx)
                .await?;
        for wh in whs {
            let Some((_, qty)) = found.iter().find(|(w, _)| w == wh) else { continue };
            let free = *qty - held.qty.get(&(info.id, *wh)).copied().unwrap_or_default();
            if free > Decimal::ZERO {
                out.push(Spot { warehouse: *wh, batch: None, serials: vec![], qty: free });
            }
        }
    }
    Ok(out)
}

/// Splits the needs over the available stock (never from `target`, where a transfer goes).
/// Returns the location rows and what could not be found.
async fn locate(
    hc: &mut HookCtx<'_>,
    company: Uuid,
    parent: Option<Uuid>,
    target: Option<Uuid>,
    exclude: Option<Uuid>,
    needs: Vec<Need>,
) -> Result<(Vec<Doc>, Vec<String>)> {
    let mut whs = warehouses(hc, company, parent).await?;
    whs.retain(|w| Some(*w) != target);
    let mut ids: Vec<Uuid> = Vec::new();
    for n in &needs {
        if !ids.contains(&n.item) {
            ids.push(n.item);
        }
    }
    let infos = items(hc.tx, hc.ctx.tenant, &ids).await?;
    let taken = held(hc, exclude, &ids).await?;
    let mut stock: HashMap<Uuid, Vec<Spot>> = HashMap::new();
    for item in &ids {
        let info = infos.get(item).ok_or_else(|| Error::NotFound("Item".into()))?;
        stock.insert(*item, supply(hc, info, &whs, &taken).await?);
    }
    let mut rows_out = Vec::new();
    let mut short = Vec::new();
    for n in needs {
        let info = &infos[&n.item];
        let spots = stock.get_mut(&n.item).expect("supply listed");
        // The order line's own warehouse first, then the picking order.
        let mut order: Vec<usize> = (0..spots.len()).collect();
        order.sort_by_key(|&i| if Some(spots[i].warehouse) == n.preferred { 0 } else { 1 });
        let wanted = if info.serial { n.qty.floor() } else { n.qty };
        let mut left = wanted;
        let row = |wh: Option<Uuid>, batch: Option<String>, sn: Vec<String>, qty: Decimal, picked: Decimal| {
            let mut r = Doc::new();
            r.insert("item".into(), json!(n.item.to_string()));
            r.insert("item_name".into(), json!(info.name));
            r.insert("uom".into(), info.stock_uom.map(|u| json!(u.to_string())).unwrap_or(Value::Null));
            r.insert("warehouse".into(), wh.map(|w| json!(w.to_string())).unwrap_or(Value::Null));
            r.insert("qty".into(), money(qty));
            r.insert("picked_qty".into(), money(picked));
            r.insert("batch_no".into(), batch.map(Value::String).unwrap_or(Value::Null));
            r.insert("serial_no".into(), if sn.is_empty() { Value::Null } else { json!(sn.join("\n")) });
            r.insert("sales_order".into(), n.sales_order.map(|o| json!(o.to_string())).unwrap_or(Value::Null));
            r.insert("so_line".into(), n.so_line.clone().map(Value::String).unwrap_or(Value::Null));
            r.insert("product_bundle_item".into(), n.bundle.map(|b| json!(b.to_string())).unwrap_or(Value::Null));
            r.insert("qty_per_bundle".into(), n.per_bundle.map(money).unwrap_or(Value::Null));
            r
        };
        for i in order {
            if left <= Decimal::ZERO {
                break;
            }
            let spot = &mut spots[i];
            let take = left.min(spot.qty);
            if take <= Decimal::ZERO {
                continue;
            }
            let sn: Vec<String> = if info.serial {
                let k = take.trunc().to_usize().unwrap_or(0).min(spot.serials.len());
                spot.serials.drain(..k).collect()
            } else {
                vec![]
            };
            spot.qty -= take;
            left -= take;
            rows_out.push(row(Some(spot.warehouse), spot.batch.clone(), sn, take, take));
        }
        let missing = n.qty - wanted + left;
        if missing > Decimal::ZERO {
            short.push(format!("{} short of {}", missing.normalize(), info.code));
            rows_out.push(row(None, None, vec![], missing, Decimal::ZERO));
        }
    }
    finish_rows(&mut rows_out);
    Ok((rows_out, short))
}

/// What is left to pick on a sales order, bundles expanded into their components.
async fn order_needs(hc: &mut HookCtx<'_>, so: &Doc, exclude: Option<Uuid>) -> Result<Vec<Need>> {
    let sid = id(so, "id");
    let lines = rows(so, "items");
    let ids: Vec<Uuid> = lines.iter().filter_map(|r| id(r, "item")).collect();
    let kits = bundles(hc.tx, hc.ctx, &ids).await?;
    let mut all_items = ids.clone();
    all_items.extend(kits.values().flatten().map(|(c, _)| *c));
    let infos = items(hc.tx, hc.ctx.tenant, &all_items).await?;
    let taken = held(hc, exclude, &all_items).await?;
    let mut out = Vec::new();
    for r in &lines {
        let (Some(item), Some(line)) = (id(r, "item"), s(r, "id")) else { continue };
        // Picked in stock units, whatever unit the order line counts in.
        let f = unit_factor(r);
        let open = to_stock(d(r, "qty"), f) - to_stock(d(r, "delivered_qty"), f);
        if open <= Decimal::ZERO {
            continue;
        }
        let preferred = id(r, "warehouse").or(id(so, "set_warehouse"));
        let parts: Vec<(Uuid, Decimal, bool)> = match kits.get(&item) {
            Some(comps) => comps.iter().map(|(c, per)| (*c, *per, true)).collect(),
            None => vec![(item, Decimal::ONE, false)],
        };
        for (comp, per, is_kit) in parts {
            if !infos.get(&comp).is_some_and(|i| i.is_stock) {
                continue;
            }
            let want = (open * per).round_dp(9) - taken.order.get(&(line.to_string(), comp)).copied().unwrap_or_default();
            if want > Decimal::ZERO {
                out.push(Need {
                    item: comp,
                    qty: want,
                    sales_order: sid,
                    so_line: Some(line.to_string()),
                    bundle: is_kit.then_some(item),
                    per_bundle: is_kit.then_some(per),
                    preferred,
                });
            }
        }
    }
    Ok(out)
}

/// Rows still to deliver: `(row, open qty)`.
fn open_rows(doc: &Doc) -> Vec<(Doc, Decimal)> {
    rows(doc, "locations")
        .into_iter()
        .filter_map(|r| {
            let open = d(&r, "picked_qty") - d(&r, "delivered_qty");
            (open > Decimal::ZERO && id(&r, "warehouse").is_some()).then_some((r, open))
        })
        .collect()
}

/// The picked serials of a row that are still in its warehouse (a partial delivery took the others).
async fn serials_left(hc: &mut HookCtx<'_>, r: &Doc) -> Result<Option<String>> {
    let listed = serials(s(r, "serial_no"));
    if listed.is_empty() {
        return Ok(None);
    }
    let here: Vec<String> = sqlx::query_scalar(
        "select serial from serial_no where tenant_id = $1 and item_id = $2 and warehouse_id = $3 and status = 'in_stock' and serial = any($4)",
    )
    .bind(hc.ctx.tenant)
    .bind(id(r, "item"))
    .bind(id(r, "warehouse"))
    .bind(&listed)
    .fetch_all(&mut **hc.tx)
    .await?;
    Ok(Some(listed.into_iter().filter(|x| here.contains(x)).collect::<Vec<_>>().join("\n")))
}

/// Recomputes how much of the list was delivered and whether it is complete.
async fn refresh(hc: &mut HookCtx<'_>, pick: Uuid) -> Result<()> {
    let sctx = sys(hc.ctx);
    let doc = hc.kernel.get_tx(&sctx, hc.tx, ENTITY, pick, false).await?;
    let (mut picked, mut done) = (Decimal::ZERO, Decimal::ZERO);
    for r in rows(&doc, "locations") {
        picked += d(&r, "picked_qty");
        done += d(&r, "delivered_qty").min(d(&r, "picked_qty"));
    }
    let pct = if picked.is_zero() { Decimal::ZERO } else { (done / picked * Decimal::ONE_HUNDRED).round_dp(2) };
    let status = match d(&doc, "docstatus") {
        x if x == Decimal::TWO => "Cancelled",
        x if x.is_zero() => "Draft",
        _ if !picked.is_zero() && done >= picked => "Completed",
        _ => "Open",
    };
    let mut v = Doc::new();
    v.insert("per_delivered".into(), money(pct));
    v.insert("status".into(), json!(status));
    hc.kernel.set_values_tx(&sctx, hc.tx, ENTITY, pick, v).await
}

/// Adds `qty` delivered to pick list rows, refusing to deliver more than was picked.
async fn deliver(hc: &mut HookCtx<'_>, links: &[(Uuid, Uuid, Decimal)], sign: Decimal) -> Result<()> {
    let mut lists: Vec<Uuid> = links.iter().map(|l| l.0).collect();
    lists.sort();
    lists.dedup();
    let sctx = sys(hc.ctx);
    for pick in lists {
        // The lock serializes deliveries from the same list.
        let doc = hc.kernel.get_tx(&sctx, hc.tx, ENTITY, pick, true).await?;
        if d(&doc, "docstatus") != Decimal::ONE {
            return Err(Error::invalid(format!("pick list {} is not submitted", s(&doc, "number").unwrap_or_default())));
        }
        let locations = rows(&doc, "locations");
        let mut sums: BTreeMap<Uuid, Decimal> = BTreeMap::new();
        for (p, row, q) in links {
            if *p == pick {
                *sums.entry(*row).or_default() += *q * sign;
            }
        }
        for (row, delta) in sums {
            let r = locations.iter().find(|r| id(r, "id") == Some(row)).ok_or_else(|| Error::invalid("the pick list line no longer exists"))?;
            let new = d(r, "delivered_qty") + delta;
            if new > d(r, "picked_qty") {
                return Err(Error::invalid(format!(
                    "this delivers more {} than pick list {} picked ({} of {})",
                    s(r, "item_name").unwrap_or("stock"),
                    s(&doc, "number").unwrap_or_default(),
                    new.normalize(),
                    d(r, "picked_qty").normalize()
                )));
            }
            let mut v = Doc::new();
            v.insert("delivered_qty".into(), money(new.max(Decimal::ZERO)));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, ENTITY, "locations", row, v).await?;
        }
        refresh(hc, pick).await?;
    }
    Ok(())
}

pub struct PickListHook;

#[async_trait]
impl DocHook for PickListHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        set(doc, "status", "Draft");
        let mut lines = rows(doc, "locations");
        for r in lines.iter_mut() {
            if r.get("picked_qty").is_none_or(Value::is_null) {
                let q = r.get("qty").cloned().unwrap_or(Value::Null);
                r.insert("picked_qty".into(), q);
            }
        }
        if s(doc, "purpose") == Some("Delivery")
            && id(doc, "customer").is_none()
            && let Some(so) = lines.iter().find_map(|r| id(r, "sales_order"))
        {
            let order = get(hc.kernel, hc.ctx, hc.tx, "SalesOrder", so).await?;
            set(doc, "customer", order.get("customer").cloned().unwrap_or(Value::Null));
        }
        set(doc, "locations", rows_value(lines));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let transfer = s(doc, "purpose") == Some("Material Transfer");
        let target = id(doc, "target_warehouse");
        let mut orders: HashMap<Uuid, Doc> = HashMap::new();
        for (i, r) in rows(doc, "locations").iter().enumerate() {
            if d(r, "picked_qty") > d(r, "qty") {
                errors.push(
                    FieldError::new("picked_qty", format!("over-picking: {} picked of {} to pick", d(r, "picked_qty").normalize(), d(r, "qty").normalize()))
                        .in_row("locations", i),
                );
            }
            if transfer && id(r, "sales_order").is_some() {
                errors.push(FieldError::new("sales_order", "a transfer is not picked for sales orders").in_row("locations", i));
            }
            if transfer && target.is_some() && id(r, "warehouse") == target {
                errors.push(FieldError::new("warehouse", "is the transfer's target").in_row("locations", i));
            }
            if let Some(so) = id(r, "sales_order") {
                if let std::collections::hash_map::Entry::Vacant(e) = orders.entry(so) {
                    e.insert(get(hc.kernel, hc.ctx, hc.tx, "SalesOrder", so).await?);
                }
                let o = &orders[&so];
                if id(o, "company") != id(doc, "company") || d(o, "docstatus") != Decimal::ONE {
                    errors.push(FieldError::new("sales_order", "is not a submitted order of this company").in_row("locations", i));
                } else if s(o, "status") == Some("Closed") {
                    errors.push(FieldError::new("sales_order", "is closed").in_row("locations", i));
                }
                if id(doc, "customer").is_some() && id(o, "customer") != id(doc, "customer") {
                    errors.push(FieldError::new("sales_order", "belongs to another customer").in_row("locations", i));
                }
            }
        }
        if let Some(w) = target {
            let wd = get(hc.kernel, hc.ctx, hc.tx, "Warehouse", w).await?;
            if id(&wd, "company") != id(doc, "company") {
                errors.push(FieldError::new("target_warehouse", "belongs to another company"));
            }
        }
        validation(errors)
    }

    /// Holds the picked stock: refuses picking more than is on hand (net of other open lists) or
    /// more than the orders still have to deliver. Balances are locked, so two lists submitted at
    /// once cannot both take the last units.
    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let pid = id(doc, "id");
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let lines = rows(doc, "locations");
        let mut errors = Vec::new();
        let picked: Vec<(usize, &Doc)> = lines.iter().enumerate().filter(|(_, r)| d(r, "picked_qty") > Decimal::ZERO).collect();
        if picked.is_empty() {
            return Err(Error::field("locations", "nothing is picked"));
        }
        let ids: Vec<Uuid> = picked.iter().filter_map(|(_, r)| id(r, "item")).collect();
        let infos = items(hc.tx, hc.ctx.tenant, &ids).await?;
        let keys: Vec<(Uuid, Uuid)> = picked.iter().filter_map(|(_, r)| Some((id(r, "item")?, id(r, "warehouse")?))).collect();
        let bal = ferrum_domain::stock::lock_balances(hc.tx, hc.ctx.tenant, cid, &keys).await?;
        // Lists for the same orders but other warehouses serialize on the orders (in id order).
        let mut so_ids: Vec<Uuid> = picked.iter().filter_map(|(_, r)| id(r, "sales_order")).collect();
        so_ids.sort();
        so_ids.dedup();
        let mut orders: HashMap<Uuid, Doc> = HashMap::new();
        for so in so_ids {
            let o = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "SalesOrder", so, true).await?;
            orders.insert(so, o);
        }
        let wh_company = crate::common::warehouse_companies(hc.tx, hc.ctx.tenant, &keys.iter().map(|k| k.1).collect::<Vec<_>>()).await?;
        let taken = held(hc, pid, &ids).await?;
        let mut per_key: BTreeMap<(Uuid, Uuid), Decimal> = BTreeMap::new();
        let mut per_batch: BTreeMap<(Uuid, Uuid, String), Decimal> = BTreeMap::new();
        let mut per_order: BTreeMap<(String, Uuid), (Decimal, Option<Uuid>, Decimal)> = BTreeMap::new();
        let mut serial_seen: HashSet<(Uuid, String)> = HashSet::new();
        for (i, r) in &picked {
            let mut err = |f: &str, m: String| errors.push(FieldError::new(f, m).in_row("locations", *i));
            let (Some(item), qty) = (id(r, "item"), d(r, "picked_qty")) else { continue };
            let Some(info) = infos.get(&item) else { continue };
            let Some(wh) = id(r, "warehouse") else {
                err("warehouse", "say where the picked stock is".into());
                continue;
            };
            if wh_company.get(&wh).map(|w| w.0) != Some(cid) {
                err("warehouse", "belongs to another company".into());
            }
            if !info.is_stock {
                err("item", format!("{} is not a stock item", info.code));
            }
            *per_key.entry((item, wh)).or_default() += qty;
            let batch = s(r, "batch_no").map(str::to_string);
            match (&batch, info.batch) {
                (None, true) => err("batch_no", format!("{} is batch-tracked: give the batch", info.code)),
                (Some(b), true) => *per_batch.entry((item, wh, b.clone())).or_default() += qty,
                _ => {}
            }
            if info.serial {
                let listed = serials(s(r, "serial_no"));
                if Decimal::from(listed.len()) != qty {
                    err("serial_no", format!("{} is serialized: list the {} picked serial number(s)", info.code, qty.normalize()));
                }
                let here: Vec<String> = sqlx::query_scalar(
                    "select s.serial from serial_no s left join stock_move m on m.tenant_id = s.tenant_id and m.id = s.last_move_id \
                     where s.tenant_id = $1 and s.item_id = $2 and s.warehouse_id = $3 and s.status = 'in_stock' and s.serial = any($4) \
                     and ($5::text is null or m.batch_no = $5)",
                )
                .bind(hc.ctx.tenant)
                .bind(item)
                .bind(wh)
                .bind(&listed)
                .bind(&batch)
                .fetch_all(&mut **hc.tx)
                .await?;
                for sn in &listed {
                    if !here.contains(sn) {
                        err("serial_no", format!("serial {sn} is not in stock there"));
                    } else if taken.serials.contains(&(item, sn.clone())) {
                        err("serial_no", format!("serial {sn} is already picked on another pick list"));
                    } else if !serial_seen.insert((item, sn.clone())) {
                        err("serial_no", format!("serial {sn} is picked twice"));
                    }
                }
            }
            if let Some(line) = s(r, "so_line") {
                let e = per_order.entry((line.to_string(), item)).or_insert((Decimal::ZERO, id(r, "sales_order"), Decimal::ONE));
                e.0 += qty;
                if let Some(per) = crate::common::opt_dec(r, "qty_per_bundle") {
                    e.2 = per;
                }
            }
        }
        for ((item, wh), qty) in &per_key {
            let on_hand = bal.get(&(*item, *wh)).map(|b| b.0).unwrap_or_default();
            let free = on_hand - taken.qty.get(&(*item, *wh)).copied().unwrap_or_default();
            if *qty > free {
                let code = infos.get(item).map(|i| i.code.clone()).unwrap_or_default();
                let name = wh_company.get(wh).map(|w| w.2.clone()).unwrap_or_default();
                errors.push(FieldError::new(
                    "locations",
                    format!("not enough {code} in {name}: {} available after other pick lists, {} picked", free.normalize(), qty.normalize()),
                ));
            }
        }
        for ((item, wh, batch), qty) in &per_batch {
            let free = crate::common::batch_qty(hc.tx, hc.ctx.tenant, *item, *wh, batch).await?
                - taken.batch.get(&(*item, *wh, batch.clone())).copied().unwrap_or_default();
            if *qty > free {
                errors.push(FieldError::new("locations", format!("not enough in batch {batch}: {} available, {} picked", free.normalize(), qty.normalize())));
            }
        }
        for ((line, item), (qty, so, per)) in &per_order {
            let Some(o) = so.and_then(|so| orders.get(&so)) else { continue };
            let Some(ol) = rows(o, "items").into_iter().find(|r| s(r, "id") == Some(line.as_str())) else {
                errors.push(FieldError::new("locations", "a sales order line no longer exists"));
                continue;
            };
            let f = unit_factor(&ol);
            let open = ((to_stock(d(&ol, "qty"), f) - to_stock(d(&ol, "delivered_qty"), f)) * per).round_dp(9);
            let elsewhere = taken.order.get(&(line.clone(), *item)).copied().unwrap_or_default();
            if *qty + elsewhere > open {
                let code = infos.get(item).map(|i| i.code.clone()).unwrap_or_default();
                errors.push(FieldError::new(
                    "locations",
                    format!(
                        "over-picking: {} of {code} picked for {} but only {} is left to deliver ({} already picked elsewhere)",
                        qty.normalize(),
                        s(o, "number").unwrap_or("the order"),
                        open.normalize(),
                        elsewhere.normalize()
                    ),
                ));
            }
        }
        validation(errors)
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        refresh(hc, id(doc, "id").ok_or_else(|| Error::internal("no id"))?).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if rows(doc, "locations").iter().any(|r| d(r, "delivered_qty") > Decimal::ZERO) {
            return Err(Error::invalid("stock was delivered from this pick list; cancel those delivery notes or stock entries first"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        refresh(hc, id(doc, "id").ok_or_else(|| Error::internal("no id"))?).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        match name {
            "get_item_locations" => {
                let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
                // Requirements as entered (or as found before), then placed again.
                let mut needs: Vec<Need> = Vec::new();
                for r in rows(doc, "locations") {
                    let Some(item) = id(&r, "item") else { continue };
                    let key = (item, id(&r, "sales_order"), s(&r, "so_line").map(str::to_string));
                    match needs.iter_mut().find(|n| (n.item, n.sales_order, n.so_line.clone()) == key) {
                        Some(n) => n.qty += d(&r, "qty"),
                        None => needs.push(Need {
                            item,
                            qty: d(&r, "qty"),
                            sales_order: key.1,
                            so_line: key.2,
                            bundle: id(&r, "product_bundle_item"),
                            per_bundle: crate::common::opt_dec(&r, "qty_per_bundle"),
                            preferred: id(&r, "warehouse"),
                        }),
                    }
                }
                let target = if s(doc, "purpose") == Some("Material Transfer") { id(doc, "target_warehouse") } else { None };
                let (locations, short) = locate(hc, cid, id(doc, "parent_warehouse"), target, Some(pid), needs).await?;
                let mut patch = Doc::new();
                patch.insert("locations".into(), rows_value(locations));
                hc.kernel.update_tx(&elevated(hc.ctx), hc.tx, ENTITY, pid, patch, None).await?;
                Ok(Some(json!({"shortages": short})))
            }
            "make_delivery_note" => make_delivery_notes(hc, doc).await.map(Some),
            "make_stock_entry" => {
                let mut lines = Vec::new();
                for (r, open) in open_rows(doc) {
                    lines.push(json!({
                        "item": r.get("item"), "qty": open.normalize().to_string(), "source_warehouse": r.get("warehouse"),
                        "target_warehouse": doc.get("target_warehouse"), "batch_no": r.get("batch_no"), "serial_no": serials_left(hc, &r).await?,
                        "stk_pick_list": pid.to_string(), "stk_pick_list_item": r.get("id"),
                    }));
                }
                if lines.is_empty() {
                    return Err(Error::invalid("everything picked was already moved"));
                }
                let mut input = Doc::new();
                input.insert("company".into(), doc.get("company").cloned().unwrap_or(Value::Null));
                input.insert("purpose".into(), json!("Material Transfer"));
                input.insert("items".into(), Value::Array(lines));
                input.insert("remarks".into(), json!(format!("Transfer for pick list {}", s(doc, "number").unwrap_or_default())));
                let se = create_for_user(hc, "StockEntry", input).await?;
                Ok(Some(created("StockEntry", &se)))
            }
            _ => Ok(None),
        }
    }
}

/// One delivery note per sales order (or one for the list's customer) with the picked stock.
async fn make_delivery_notes(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
    let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let open = open_rows(doc);
    if open.is_empty() {
        return Err(Error::invalid("everything picked was already delivered"));
    }
    let mut groups: Vec<(Option<Uuid>, Vec<(Doc, Decimal)>)> = Vec::new();
    for (r, q) in open {
        let so = id(&r, "sales_order");
        match groups.iter_mut().find(|g| g.0 == so) {
            Some(g) => g.1.push((r, q)),
            None => groups.push((so, vec![(r, q)])),
        }
    }
    let mut made = Vec::new();
    for (so, picked) in groups {
        let order = match so {
            Some(o) => Some(get(hc.kernel, hc.ctx, hc.tx, "SalesOrder", o).await?),
            None => None,
        };
        let mut input = Doc::new();
        input.insert("company".into(), doc.get("company").cloned().unwrap_or(Value::Null));
        let customer = order.as_ref().and_then(|o| o.get("customer").cloned()).or_else(|| doc.get("customer").cloned()).unwrap_or(Value::Null);
        if customer.is_null() {
            return Err(Error::field("customer", "is required to deliver"));
        }
        input.insert("customer".into(), customer);
        if let Some(o) = &order {
            for k in ["currency", "conversion_rate", "price_list", "tax_template", "discount_percent", "project", "po_no"] {
                if let Some(v) = o.get(k).filter(|v| !v.is_null()) {
                    input.insert(k.into(), v.clone());
                }
            }
            let taxes: Vec<Value> = rows(o, "taxes")
                .into_iter()
                .map(|mut t| {
                    for k in ["id", "tax_amount", "total", "base_tax_amount", "base_total"] {
                        t.remove(k);
                    }
                    Value::Object(t)
                })
                .collect();
            if !taxes.is_empty() {
                input.insert("taxes".into(), Value::Array(taxes));
            }
        }
        let order_lines = order.as_ref().map(|o| rows(o, "items")).unwrap_or_default();
        let priced = |line: Option<&str>| order_lines.iter().find(|l| s(l, "id") == line).cloned();
        let mut lines: Vec<Value> = Vec::new();
        // Bundle components picked for an order line: `so_line` → rows.
        let mut kits: Vec<(String, Uuid, Vec<(Doc, Decimal)>)> = Vec::new();
        for (r, q) in picked {
            if let Some(bundle) = id(&r, "product_bundle_item") {
                let line = s(&r, "so_line").unwrap_or_default().to_string();
                match kits.iter_mut().find(|k| k.0 == line) {
                    Some(k) => k.2.push((r, q)),
                    None => kits.push((line, bundle, vec![(r, q)])),
                }
                continue;
            }
            // Delivered in the order line's unit, at its price.
            let src = priced(s(&r, "so_line"));
            let f = src.as_ref().map(unit_factor).unwrap_or(Decimal::ONE);
            lines.push(json!({
                "item": r.get("item"), "uom": src.as_ref().and_then(|l| l.get("uom").cloned()), "qty": (q / f).round_dp(9).normalize().to_string(),
                "warehouse": r.get("warehouse"), "batch_no": r.get("batch_no"),
                "serial_no": serials_left(hc, &r).await?, "sales_order": r.get("sales_order"), "so_line": r.get("so_line"),
                "price_list_rate": src.as_ref().and_then(|l| l.get("price_list_rate").cloned()), "discount_percent": src.as_ref().and_then(|l| l.get("discount_percent").cloned()),
                "stk_pick_list": pid.to_string(), "stk_pick_list_item": r.get("id"),
            }));
        }
        // A kit ships as many whole bundles as every component allows.
        let mut kit_rows: Vec<(usize, Vec<(Doc, Decimal)>, Vec<(Uuid, Decimal)>)> = Vec::new();
        for (line, bundle, comps) in kits {
            let comps_def = bundles(hc.tx, hc.ctx, &[bundle]).await?.remove(&bundle).unwrap_or_default();
            let mut count: Option<Decimal> = None;
            for (comp, per) in &comps_def {
                let have: Decimal = comps.iter().filter(|(r, _)| id(r, "item") == Some(*comp)).map(|(_, q)| *q).sum();
                let n = if per.is_zero() { Decimal::ZERO } else { (have / per).floor() };
                count = Some(count.map_or(n, |c| c.min(n)));
            }
            let kits_n = count.unwrap_or_default();
            if kits_n <= Decimal::ZERO {
                continue;
            }
            let src = priced(Some(line.as_str()));
            let f = src.as_ref().map(unit_factor).unwrap_or(Decimal::ONE);
            let first_wh = comps.first().and_then(|(r, _)| r.get("warehouse").cloned()).unwrap_or(Value::Null);
            lines.push(json!({
                "item": bundle.to_string(), "uom": src.as_ref().and_then(|l| l.get("uom").cloned()), "qty": (kits_n / f).round_dp(9).normalize().to_string(),
                "warehouse": first_wh, "sales_order": so.map(|o| o.to_string()),
                "so_line": line, "price_list_rate": src.as_ref().and_then(|l| l.get("price_list_rate").cloned()),
                "discount_percent": src.as_ref().and_then(|l| l.get("discount_percent").cloned()), "stk_pick_list": pid.to_string(),
            }));
            kit_rows.push((lines.len() - 1, comps, comps_def.iter().map(|(c, per)| (*c, kits_n * per)).collect()));
        }
        if lines.is_empty() {
            continue;
        }
        input.insert("items".into(), Value::Array(lines));
        let dn = create_for_user(hc, "DeliveryNote", input).await?;
        let dn_id = id(&dn, "id").ok_or_else(|| Error::internal("delivery note without id"))?;
        if !kit_rows.is_empty() {
            // The components go out from where they were picked.
            let dn_lines = rows(&dn, "items");
            let current = rows(&dn, PACKED);
            let mut packed: Vec<Doc> = current
                .iter()
                .filter(|p| !kit_rows.iter().any(|(i, _, _)| s(p, "parent_line") == dn_lines.get(*i).and_then(|l| s(l, "id"))))
                .cloned()
                .collect();
            for (i, comps, needs) in &kit_rows {
                let Some(line_id) = dn_lines.get(*i).and_then(|l| s(l, "id")) else { continue };
                for (comp, need) in needs {
                    let mut left = *need;
                    let mut first = true;
                    for (r, q) in comps.iter().filter(|(r, _)| id(r, "item") == Some(*comp)) {
                        if left <= Decimal::ZERO {
                            break;
                        }
                        let take = left.min(*q);
                        left -= take;
                        let mut p = Doc::new();
                        if first && let Some(existing) = current.iter().find(|x| s(x, "parent_line") == Some(line_id) && id(x, "item") == Some(*comp)) {
                            p.insert("id".into(), existing.get("id").cloned().unwrap_or(Value::Null));
                        }
                        first = false;
                        let sn: Vec<String> = serials(serials_left(hc, r).await?.as_deref()).into_iter().take(take.floor().to_usize().unwrap_or(0)).collect();
                        p.insert("parent_line".into(), json!(line_id));
                        p.insert("item".into(), json!(comp.to_string()));
                        p.insert("qty".into(), money(take));
                        p.insert("warehouse".into(), r.get("warehouse").cloned().unwrap_or(Value::Null));
                        p.insert("batch_no".into(), r.get("batch_no").cloned().unwrap_or(Value::Null));
                        p.insert("serial_no".into(), if sn.is_empty() { Value::Null } else { json!(sn.join("\n")) });
                        p.insert("pick_list_item".into(), r.get("id").cloned().unwrap_or(Value::Null));
                        packed.push(p);
                    }
                }
            }
            let mut patch = Doc::new();
            patch.insert(PACKED.into(), rows_value(packed));
            hc.kernel.update_tx(&elevated(hc.ctx), hc.tx, "DeliveryNote", dn_id, patch, None).await?;
        }
        made.push(created("DeliveryNote", &dn));
    }
    if made.is_empty() {
        return Err(Error::invalid("the picked quantities make no whole bundle to deliver"));
    }
    let mut out = made[0].clone();
    out["documents"] = Value::Array(made);
    Ok(out)
}

/// `make_pick_list` on sales orders.
pub struct OrderPicking;

#[async_trait]
impl DocHook for OrderPicking {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "make_pick_list" || !installed(hc.ctx) {
            return Ok(None);
        }
        let needs = order_needs(hc, doc, None).await?;
        if needs.is_empty() {
            return Err(Error::invalid("nothing is left to pick on this order"));
        }
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let (locations, short) = locate(hc, cid, None, None, None, needs).await?;
        let mut input = Doc::new();
        input.insert("company".into(), json!(cid.to_string()));
        input.insert("purpose".into(), json!("Delivery"));
        input.insert("customer".into(), doc.get("customer").cloned().unwrap_or(Value::Null));
        input.insert("locations".into(), rows_value(locations));
        let pick = create_for_user(hc, ENTITY, input).await?;
        let mut out = created(ENTITY, &pick);
        out["shortages"] = json!(short);
        Ok(Some(out))
    }
}

/// Delivery notes and transfers made from pick lists report what they delivered.
pub struct PickProgress;

impl PickProgress {
    /// `(pick list, pick list row, qty)` delivered by the document.
    fn links(entity: &str, doc: &Doc) -> Vec<(Uuid, Uuid, Decimal)> {
        let mut out = Vec::new();
        let lines = rows(doc, "items");
        for r in &lines {
            if let (Some(p), Some(row)) = (id(r, "stk_pick_list"), id(r, "stk_pick_list_item")) {
                out.push((p, row, stock_qty(r)));
            }
        }
        if entity == "DeliveryNote" {
            for p in rows(doc, PACKED) {
                let Some(row) = id(&p, "pick_list_item") else { continue };
                let parent = lines.iter().find(|l| s(l, "id") == s(&p, "parent_line"));
                if let Some(list) = parent.and_then(|l| id(l, "stk_pick_list")) {
                    out.push((list, row, d(&p, "qty")));
                }
            }
        }
        out
    }

    fn counts(entity: &str, doc: &Doc) -> bool {
        match entity {
            "DeliveryNote" => !ferrum_domain::util::b(doc, "is_return"),
            _ => s(doc, "purpose") == Some("Material Transfer"),
        }
    }
}

#[async_trait]
impl DocHook for PickProgress {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let entity = hc.entity.def.name.clone();
        if !installed(hc.ctx) || !Self::counts(&entity, doc) || !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let links = Self::links(&entity, doc);
        let mut sums: BTreeMap<(Uuid, Uuid), Decimal> = BTreeMap::new();
        for (p, row, q) in links {
            *sums.entry((p, row)).or_default() += q;
        }
        let mut cache: HashMap<Uuid, Doc> = HashMap::new();
        for ((p, row), q) in sums {
            if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(p) {
                e.insert(get(hc.kernel, hc.ctx, hc.tx, ENTITY, p).await?);
            }
            let pl = &cache[&p];
            let Some(r) = rows(pl, "locations").into_iter().find(|r| id(r, "id") == Some(row)) else { continue };
            let open = d(&r, "picked_qty") - d(&r, "delivered_qty");
            if q > open {
                return Err(Error::invalid(format!(
                    "{} {}: more than pick list {} has left to deliver ({})",
                    q.normalize(),
                    s(&r, "item_name").unwrap_or("stock"),
                    s(pl, "number").unwrap_or_default(),
                    open.normalize()
                )));
            }
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let entity = hc.entity.def.name.clone();
        if !installed(hc.ctx) || !Self::counts(&entity, doc) {
            return Ok(());
        }
        let links = Self::links(&entity, doc);
        if links.is_empty() {
            return Ok(());
        }
        deliver(hc, &links, Decimal::ONE).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let entity = hc.entity.def.name.clone();
        if !installed(hc.ctx) || !Self::counts(&entity, doc) {
            return Ok(());
        }
        let links = Self::links(&entity, doc);
        if links.is_empty() {
            return Ok(());
        }
        deliver(hc, &links, -Decimal::ONE).await
    }
}
