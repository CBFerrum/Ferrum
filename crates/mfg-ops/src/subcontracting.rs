//! Subcontracting: a supplier makes goods from materials we send to a warehouse of ours at its
//! premises. The order lists the goods and the materials to supply (from each good's BOM) and
//! buys the service on a linked purchase order. Materials move by stock transfers to and from
//! the supplier warehouse; the receipt consumes them there and receives the goods valued at the
//! materials consumed + the service + additional costs, exactly.

use crate::{cached, rows_value, submit_param};
use async_trait::async_trait;
use ferrum_domain::flow::created;
use ferrum_domain::ledger::{self, Entry, Line};
use ferrum_domain::stock::{self, MoveIn, parse_serials};
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{Company, b, company, d, date, elevated, get, id, money, posting_ts, round, rows, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

const TRANSFER: &str = "Material Transfer";

fn pct(done: Decimal, total: Decimal) -> Decimal {
    if total.is_zero() { Decimal::ZERO } else { (done / total * Decimal::ONE_HUNDRED).round_dp(2).min(Decimal::ONE_HUNDRED) }
}

/// What is held at the supplier for one order material: sent − returned − consumed.
fn at_supplier(r: &Doc) -> Decimal {
    d(r, "supplied_qty") - d(r, "returned_qty") - d(r, "consumed_qty")
}

// ───────────────────────────── subcontracting BOM ─────────────────────────────

pub struct SubcontractingBomHook;

#[async_trait]
impl DocHook for SubcontractingBomHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        if let Some(bom) = id(doc, "finished_good_bom") {
            let bd = get(hc.kernel, hc.ctx, hc.tx, "Bom", bom).await?;
            if id(&bd, "item") != id(doc, "finished_good") {
                errors.push(FieldError::new("finished_good_bom", "is a BOM of another item"));
            } else if d(&bd, "docstatus") != Decimal::ONE || !b(&bd, "is_active") {
                errors.push(FieldError::new("finished_good_bom", "must be a submitted, active BOM"));
            }
        }
        if let Some(fg) = id(doc, "finished_good")
            && !b(&get(hc.kernel, hc.ctx, hc.tx, "Item", fg).await?, "is_stock_item")
        {
            errors.push(FieldError::new("finished_good", "must be a stock item"));
        }
        if let Some(svc) = id(doc, "service_item")
            && b(&get(hc.kernel, hc.ctx, hc.tx, "Item", svc).await?, "is_stock_item")
        {
            errors.push(FieldError::new("service_item", "must be a non-stock (service) item"));
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        if b(doc, "is_active") {
            let clash: i64 = sqlx::query_scalar(
                "select count(*) from e_subcontracting_bom where tenant_id = $1 and finished_good = $2 and is_active and id <> $3 and supplier is not distinct from $4",
            )
            .bind(hc.ctx.tenant)
            .bind(id(doc, "finished_good"))
            .bind(id(doc, "id").unwrap_or_default())
            .bind(id(doc, "supplier"))
            .fetch_one(&mut **hc.tx)
            .await?;
            if clash > 0 {
                return Err(Error::field("is_active", "another active subcontracting BOM exists for this finished good and supplier"));
            }
        }
        Ok(())
    }
}

/// The active subcontracting BOM for an item: the supplier's own, else one for any supplier.
async fn active_sc_bom(hc: &mut HookCtx<'_>, item: Uuid, supplier: Option<Uuid>) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "select id from e_subcontracting_bom where tenant_id = $1 and finished_good = $2 and is_active and (supplier = $3 or supplier is null) \
         order by supplier nulls last, created_at desc limit 1",
    )
    .bind(hc.ctx.tenant)
    .bind(item)
    .bind(supplier)
    .fetch_optional(&mut **hc.tx)
    .await?)
}

// ───────────────────────────── order ─────────────────────────────

/// Recomputes an order's received percentage and status from its lines and materials.
async fn refresh_order(hc: &mut HookCtx<'_>, sco: Uuid) -> Result<()> {
    let sctx = sys(hc.ctx);
    let doc = hc.kernel.get_tx(&sctx, hc.tx, "SubcontractingOrder", sco, false).await?;
    let items = rows(&doc, "items");
    let total: Decimal = items.iter().map(|r| d(r, "qty")).sum();
    let received: Decimal = items.iter().map(|r| d(r, "received_qty").min(d(r, "qty"))).sum();
    let mats = rows(&doc, "supplied_items");
    let all_sent = !mats.is_empty() && mats.iter().all(|r| d(r, "supplied_qty") - d(r, "returned_qty") >= d(r, "required_qty"));
    let any_sent = mats.iter().any(|r| d(r, "supplied_qty") > Decimal::ZERO);
    let per = pct(received, total);
    let status = match d(&doc, "docstatus") {
        x if x == Decimal::TWO => "Cancelled",
        x if x.is_zero() => "Draft",
        _ if per >= Decimal::ONE_HUNDRED => "Completed",
        _ if received > Decimal::ZERO => "Partially Received",
        _ if all_sent => "Material Transferred",
        _ if any_sent => "Partial Material Transferred",
        _ => "Open",
    };
    let mut v = Doc::new();
    v.insert("per_received".into(), money(per));
    v.insert("status".into(), json!(status));
    hc.kernel.set_values_tx(&sctx, hc.tx, "SubcontractingOrder", sco, v).await
}

/// A draft purchase order for the order's service items, at the agreed charges (in company
/// currency, like the order, whatever the supplier's usual currency).
async fn service_purchase_order(hc: &mut HookCtx<'_>, doc: &Doc, as_user: bool) -> Result<Doc> {
    let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
    let mut lines = Vec::new();
    for r in rows(doc, "items") {
        let qty = d(&r, "service_qty");
        if id(&r, "service_item").is_none() || qty <= Decimal::ZERO {
            continue;
        }
        lines.push(json!({
            "item": r.get("service_item"),
            "qty": money(qty),
            "price_list_rate": money((d(&r, "service_amount") / qty).round_dp(9)),
            "schedule_date": r.get("schedule_date").filter(|v| !v.is_null()).or(doc.get("schedule_date")),
        }));
    }
    if lines.is_empty() {
        return Err(Error::invalid("the order has no service to buy"));
    }
    let mut input = Doc::new();
    for k in ["company", "supplier", "schedule_date"] {
        input.insert(k.into(), doc.get(k).cloned().unwrap_or(Value::Null));
    }
    input.insert("currency".into(), json!(co.currency_id.to_string()));
    input.insert("items".into(), Value::Array(lines));
    input.insert("terms".into(), json!(format!("Subcontracting service for {}", s(doc, "number").unwrap_or_default())));
    if as_user { create_for_user(hc, "PurchaseOrder", input).await } else { hc.kernel.insert_tx(&elevated(hc.ctx), hc.tx, "PurchaseOrder", input).await }
}

/// A material transfer for an order (`mfo_supplied_item` ties each line to an order material).
async fn transfer(hc: &mut HookCtx<'_>, doc: &Doc, lines: Vec<Value>, remarks: String, params: &Value) -> Result<Value> {
    let mut input = Doc::new();
    input.insert("company".into(), doc.get("company").cloned().unwrap_or(Value::Null));
    input.insert("purpose".into(), json!(TRANSFER));
    input.insert("mfo_subcontracting_order".into(), doc.get("id").cloned().unwrap_or(Value::Null));
    input.insert("items".into(), Value::Array(lines));
    input.insert("remarks".into(), json!(remarks));
    let entry = create_for_user(hc, "StockEntry", input).await?;
    let entry = if submit_param(params) {
        hc.kernel.submit_tx(&elevated(hc.ctx), hc.tx, "StockEntry", id(&entry, "id").unwrap_or_default(), None).await?
    } else {
        entry
    };
    Ok(created("StockEntry", &entry))
}

pub struct SubcontractingOrderHook;

#[async_trait]
impl DocHook for SubcontractingOrderHook {
    /// Lines take their subcontracting BOM, service item and quantity; the materials to supply
    /// are regenerated from the BOMs (keeping the warehouse chosen for each).
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let precision = match id(doc, "company") {
            Some(c) => company(hc.kernel, hc.ctx, hc.tx, c).await?.precision,
            None => 2,
        };
        let supplier = id(doc, "supplier");
        let mut items = rows(doc, "items");
        let (mut total_qty, mut service_total) = (Decimal::ZERO, Decimal::ZERO);
        let mut sc_boms = HashMap::new();
        for r in items.iter_mut() {
            if id(r, "subcontracting_bom").is_none()
                && let Some(item) = id(r, "item")
                && let Some(sb) = active_sc_bom(hc, item, supplier).await?
            {
                set(r, "subcontracting_bom", sb.to_string());
            }
            if let Some(sb) = id(r, "subcontracting_bom") {
                let sbd = cached(hc, &mut sc_boms, "SubcontractingBom", sb).await?;
                let per = if d(&sbd, "finished_good_qty").is_zero() { Decimal::ONE } else { d(&sbd, "finished_good_qty") };
                set(r, "bom", sbd.get("finished_good_bom").cloned().unwrap_or(Value::Null));
                set(r, "service_item", sbd.get("service_item").cloned().unwrap_or(Value::Null));
                set(r, "service_qty", money((d(r, "qty") * d(&sbd, "service_item_qty") / per).round_dp(9)));
            }
            if id(r, "warehouse").is_none() {
                set(r, "warehouse", doc.get("set_warehouse").cloned().unwrap_or(Value::Null));
            }
            let amount = round(d(r, "qty") * d(r, "service_rate"), precision);
            set(r, "service_amount", money(amount));
            total_qty += d(r, "qty");
            service_total += amount;
        }
        let kept: HashMap<(String, String), Doc> =
            rows(doc, "supplied_items").into_iter().filter_map(|r| Some(((s(&r, "sco_line")?.to_string(), s(&r, "rm_item")?.to_string()), r))).collect();
        let mut needs: BTreeMap<(usize, String), (Doc, Decimal)> = BTreeMap::new();
        let mut boms = HashMap::new();
        for (i, r) in items.iter().enumerate() {
            let (Some(bom), Some(line)) = (id(r, "bom"), s(r, "id")) else { continue };
            let bd = cached(hc, &mut boms, "Bom", bom).await?;
            let per = if d(&bd, "quantity").is_zero() { Decimal::ONE } else { d(&bd, "quantity") };
            for c in rows(&bd, "items") {
                let Some(rm) = s(&c, "item").map(str::to_string) else { continue };
                let need = d(&c, "qty") / per * d(r, "qty") * (Decimal::ONE + d(&c, "scrap_percent") / Decimal::ONE_HUNDRED);
                let e = needs.entry((i, rm.clone())).or_insert_with(|| {
                    let mut row = kept.get(&(line.to_string(), rm.clone())).cloned().unwrap_or_default();
                    set(&mut row, "sco_line", line);
                    set(&mut row, "main_item", r.get("item").cloned().unwrap_or(Value::Null));
                    set(&mut row, "rm_item", rm.clone());
                    set(&mut row, "rm_item_name", c.get("item_name").cloned().unwrap_or(Value::Null));
                    if id(&row, "source_warehouse").is_none() {
                        let from = c.get("source_warehouse").filter(|v| !v.is_null()).or(doc.get("set_reserve_warehouse"));
                        set(&mut row, "source_warehouse", from.cloned().unwrap_or(Value::Null));
                    }
                    (row, Decimal::ZERO)
                });
                e.1 += need;
            }
        }
        let supplied: Vec<Doc> = needs
            .into_values()
            .map(|(mut row, need)| {
                set(&mut row, "required_qty", money(need.round_dp(9)));
                row
            })
            .collect();
        set(doc, "items", rows_value(items));
        set(doc, "supplied_items", rows_value(supplied));
        set(doc, "total_qty", money(total_qty));
        set(doc, "service_total", money(service_total));
        set(doc, "status", "Draft");
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company");
        let mut errors = Vec::new();
        let mut warehouses = HashMap::new();
        for (k, wh) in [
            ("supplier_warehouse", id(doc, "supplier_warehouse")),
            ("set_warehouse", id(doc, "set_warehouse")),
            ("set_reserve_warehouse", id(doc, "set_reserve_warehouse")),
        ] {
            if let Some(w) = wh
                && id(&cached(hc, &mut warehouses, "Warehouse", w).await?, "company") != cid
            {
                errors.push(FieldError::new(k, "belongs to another company"));
            }
        }
        let mut sc_boms = HashMap::new();
        let mut boms = HashMap::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            let Some(sb) = id(r, "subcontracting_bom") else {
                errors.push(FieldError::new("subcontracting_bom", "no active subcontracting BOM exists for this item").in_row("items", i));
                continue;
            };
            let sbd = cached(hc, &mut sc_boms, "SubcontractingBom", sb).await?;
            if id(&sbd, "finished_good") != id(r, "item") {
                errors.push(FieldError::new("subcontracting_bom", "is for another item").in_row("items", i));
            } else if !b(&sbd, "is_active") {
                errors.push(FieldError::new("subcontracting_bom", "is not active").in_row("items", i));
            } else if id(&sbd, "supplier").is_some_and(|x| Some(x) != id(doc, "supplier")) {
                errors.push(FieldError::new("subcontracting_bom", "belongs to another supplier").in_row("items", i));
            } else if let Some(bom) = id(r, "bom")
                && id(&cached(hc, &mut boms, "Bom", bom).await?, "company") != cid
            {
                errors.push(FieldError::new("subcontracting_bom", "its BOM belongs to another company").in_row("items", i));
            }
            match id(r, "warehouse") {
                None => errors.push(FieldError::new("warehouse", "is required (or set Receive Into on the order)").in_row("items", i)),
                Some(w) => {
                    if id(&cached(hc, &mut warehouses, "Warehouse", w).await?, "company") != cid {
                        errors.push(FieldError::new("warehouse", "belongs to another company").in_row("items", i));
                    }
                }
            }
        }
        if let Some(po) = id(doc, "purchase_order") {
            let p = get(hc.kernel, hc.ctx, hc.tx, "PurchaseOrder", po).await?;
            if id(&p, "supplier") != id(doc, "supplier") || id(&p, "company") != cid || d(&p, "docstatus") == Decimal::TWO {
                errors.push(FieldError::new("purchase_order", "must be a live purchase order of the same supplier and company"));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let sco = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        if id(doc, "purchase_order").is_none() && rows(doc, "items").iter().any(|r| d(r, "service_qty") > Decimal::ZERO) {
            let po = service_purchase_order(hc, doc, false).await?;
            let mut v = Doc::new();
            v.insert("purchase_order".into(), po.get("id").cloned().unwrap_or(Value::Null));
            hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SubcontractingOrder", sco, v).await?;
        }
        refresh_order(hc, sco).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let held = rows(doc, "supplied_items").iter().any(|r| d(r, "supplied_qty") > d(r, "returned_qty"));
        let received = rows(doc, "items").iter().any(|r| d(r, "received_qty") > Decimal::ZERO);
        if held || received {
            return Err(Error::invalid(
                "materials were sent or goods received on this order; cancel the receipts and transfers (or return the materials) first",
            ));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        refresh_order(hc, id(doc, "id").unwrap_or_default()).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let sco = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let supplier_wh = doc.get("supplier_warehouse").cloned().unwrap_or(Value::Null);
        let reserve = doc.get("set_reserve_warehouse").filter(|v| !v.is_null()).cloned();
        match name {
            "make_purchase_order" => {
                if let Some(po) = id(doc, "purchase_order")
                    && d(&get(hc.kernel, hc.ctx, hc.tx, "PurchaseOrder", po).await?, "docstatus") < Decimal::TWO
                {
                    return Err(Error::invalid("a live purchase order already covers the service"));
                }
                let po = service_purchase_order(hc, doc, true).await?;
                let mut v = Doc::new();
                v.insert("purchase_order".into(), po.get("id").cloned().unwrap_or(Value::Null));
                hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SubcontractingOrder", sco, v).await?;
                Ok(Some(created("PurchaseOrder", &po)))
            }
            "make_material_transfer" => {
                let mut lines = Vec::new();
                let mut missing = Vec::new();
                for r in rows(doc, "supplied_items") {
                    let pending = d(&r, "required_qty") - (d(&r, "supplied_qty") - d(&r, "returned_qty"));
                    if pending <= Decimal::ZERO {
                        continue;
                    }
                    match r.get("source_warehouse").filter(|v| !v.is_null()).cloned().or(reserve.clone()) {
                        Some(from) => lines.push(json!({"item": r.get("rm_item"), "qty": money(pending), "source_warehouse": from,
                                                        "target_warehouse": supplier_wh, "mfo_supplied_item": r.get("id")})),
                        None => missing.push(s(&r, "rm_item_name").unwrap_or("?").to_string()),
                    }
                }
                if !missing.is_empty() {
                    return Err(Error::invalid(format!("set the warehouse to send from (Send Materials From) for: {}", missing.join(", "))));
                }
                if lines.is_empty() {
                    return Err(Error::invalid("every material was already sent to the supplier"));
                }
                Ok(Some(transfer(hc, doc, lines, format!("Materials for subcontracting order {}", s(doc, "number").unwrap_or_default()), params).await?))
            }
            "return_excess_materials" => {
                // Held beyond what the goods still to come need, per material.
                let lines_by_id: HashMap<String, Doc> = rows(doc, "items").into_iter().filter_map(|r| Some((s(&r, "id")?.to_string(), r))).collect();
                let mut lines = Vec::new();
                for r in rows(doc, "supplied_items") {
                    let held = at_supplier(&r);
                    let still = match s(&r, "sco_line").and_then(|l| lines_by_id.get(l)) {
                        Some(line) if d(line, "qty") > Decimal::ZERO => {
                            (d(&r, "required_qty") / d(line, "qty") * (d(line, "qty") - d(line, "received_qty")).max(Decimal::ZERO)).round_dp(9)
                        }
                        _ => Decimal::ZERO,
                    };
                    let excess = held - still;
                    if excess <= Decimal::ZERO {
                        continue;
                    }
                    let to = r.get("source_warehouse").filter(|v| !v.is_null()).cloned().or(reserve.clone()).ok_or_else(|| {
                        Error::invalid(format!("set the warehouse to return {} to (Send Materials From)", s(&r, "rm_item_name").unwrap_or("the material")))
                    })?;
                    lines.push(json!({"item": r.get("rm_item"), "qty": money(excess), "source_warehouse": supplier_wh, "target_warehouse": to,
                                      "mfo_supplied_item": r.get("id")}));
                }
                if lines.is_empty() {
                    return Err(Error::invalid("the supplier holds nothing beyond what the goods still to come need"));
                }
                Ok(Some(
                    transfer(hc, doc, lines, format!("Materials returned from subcontracting order {}", s(doc, "number").unwrap_or_default()), params).await?,
                ))
            }
            "make_subcontracting_receipt" => {
                let lines: Vec<Value> = rows(doc, "items")
                    .iter()
                    .filter(|r| d(r, "qty") > d(r, "received_qty"))
                    .map(|r| {
                        json!({"item": r.get("item"), "qty": money(d(r, "qty") - d(r, "received_qty")), "sco_line": r.get("id"),
                               "warehouse": r.get("warehouse").filter(|v| !v.is_null()).or(doc.get("set_warehouse"))})
                    })
                    .collect();
                if lines.is_empty() {
                    return Err(Error::invalid("everything on this order was received"));
                }
                let mut input = Doc::new();
                input.insert("company".into(), doc.get("company").cloned().unwrap_or(Value::Null));
                input.insert("subcontracting_order".into(), json!(sco.to_string()));
                input.insert("items".into(), Value::Array(lines));
                let r = create_for_user(hc, "SubcontractingReceipt", input).await?;
                Ok(Some(created("SubcontractingReceipt", &r)))
            }
            _ => Ok(None),
        }
    }
}

/// Books a submitted (`sign` 1) or cancelled (-1) stock transfer against its order: lines into
/// the supplier warehouse count as sent, lines out of it as returned. The supplier can never
/// hold a negative quantity for the order.
pub(crate) async fn book_transfer(hc: &mut HookCtx<'_>, entry: &Doc, sign: Decimal) -> Result<()> {
    let Some(sco) = id(entry, "mfo_subcontracting_order") else { return Ok(()) };
    if s(entry, "purpose") != Some(TRANSFER) {
        return Err(Error::field("purpose", "subcontracting materials move by material transfer"));
    }
    let sctx = sys(hc.ctx);
    let order = hc.kernel.get_tx(&sctx, hc.tx, "SubcontractingOrder", sco, true).await?;
    if sign > Decimal::ZERO && d(&order, "docstatus") != Decimal::ONE {
        return Err(Error::field("mfo_subcontracting_order", "the subcontracting order is not submitted"));
    }
    let wh = id(&order, "supplier_warehouse");
    let before: HashMap<String, Doc> = rows(&order, "supplied_items").into_iter().filter_map(|r| Some((s(&r, "id")?.to_string(), r))).collect();
    let mut mats = before.clone();
    let mut touched = Vec::new();
    for (i, l) in rows(entry, "items").iter().enumerate() {
        let Some(key) = s(l, "mfo_supplied_item") else { continue };
        let m = mats
            .get_mut(key)
            .ok_or_else(|| Error::Validation(vec![FieldError::new("item", "is not a material of the subcontracting order").in_row("items", i)]))?;
        if id(l, "item") != id(m, "rm_item") {
            return Err(Error::Validation(vec![FieldError::new("item", "differs from the order's material").in_row("items", i)]));
        }
        let q = sign * d(l, "qty");
        let field = if id(l, "target_warehouse") == wh {
            "supplied_qty"
        } else if id(l, "source_warehouse") == wh {
            "returned_qty"
        } else {
            return Err(Error::Validation(vec![
                FieldError::new("target_warehouse", "must be the supplier warehouse (or the source, for a return)").in_row("items", i),
            ]));
        };
        set(m, field, money(d(m, field) + q));
        touched.push(key.to_string());
    }
    for key in touched.iter().collect::<std::collections::BTreeSet<_>>() {
        let m = &mats[key.as_str()];
        if at_supplier(m) < Decimal::ZERO || d(m, "returned_qty") < Decimal::ZERO {
            return Err(Error::invalid(if sign > Decimal::ZERO {
                format!(
                    "the supplier holds only {} of {} for this order",
                    at_supplier(&before[key.as_str()]).max(Decimal::ZERO).normalize(),
                    s(m, "rm_item_name").unwrap_or("the material")
                )
            } else {
                format!("{} sent by this transfer was already used or returned; cancel those documents first", s(m, "rm_item_name").unwrap_or("the material"))
            }));
        }
        let mut v = Doc::new();
        v.insert("supplied_qty".into(), money(d(m, "supplied_qty")));
        v.insert("returned_qty".into(), money(d(m, "returned_qty")));
        hc.kernel.set_child_values_tx(&sctx, hc.tx, "SubcontractingOrder", "supplied_items", id(m, "id").unwrap_or_default(), v).await?;
    }
    refresh_order(hc, sco).await
}

// ───────────────────────────── receipt ─────────────────────────────

pub struct SubcontractingReceiptHook;

/// The service charge is credited to the account the service purchase invoice debits (the
/// service item's expense account, else the company default), so it moves into stock.
async fn service_account(hc: &mut HookCtx<'_>, co: &Company, service_item: Option<Uuid>) -> Result<Uuid> {
    if let Some(item) = service_item
        && let Some(a) = id(&get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?, "expense_account")
    {
        return Ok(a);
    }
    co.require("default_expense_account", "default expense account")
}

async fn stock_account(hc: &mut HookCtx<'_>, co: &Company, warehouse: Uuid, cache: &mut HashMap<Uuid, Uuid>) -> Result<Uuid> {
    if let Some(a) = cache.get(&warehouse) {
        return Ok(*a);
    }
    let a = match id(&get(hc.kernel, hc.ctx, hc.tx, "Warehouse", warehouse).await?, "account") {
        Some(a) => a,
        None => co.require("default_inventory_account", "default inventory account")?,
    };
    cache.insert(warehouse, a);
    Ok(a)
}

/// Adds (`sign` 1) or removes (-1) a receipt's goods and consumption on its order.
async fn book_receipt(hc: &mut HookCtx<'_>, doc: &Doc, sign: Decimal) -> Result<()> {
    let sco = id(doc, "subcontracting_order").ok_or_else(|| Error::field("subcontracting_order", "is required"))?;
    let sctx = sys(hc.ctx);
    let order = hc.kernel.get_tx(&sctx, hc.tx, "SubcontractingOrder", sco, true).await?;
    let mut received: HashMap<String, Decimal> = HashMap::new();
    for r in rows(doc, "items") {
        *received.entry(s(&r, "sco_line").unwrap_or_default().to_string()).or_default() += d(&r, "qty");
    }
    let mut consumed: HashMap<String, Decimal> = HashMap::new();
    for r in rows(doc, "supplied_items") {
        *consumed.entry(s(&r, "sco_supplied_item").unwrap_or_default().to_string()).or_default() += d(&r, "consumed_qty");
    }
    for l in rows(&order, "items") {
        let Some(q) = s(&l, "id").and_then(|k| received.get(k)) else { continue };
        let now = d(&l, "received_qty") + sign * q;
        if now > d(&l, "qty") {
            return Err(Error::invalid(format!(
                "this would receive {} of {} ordered for {}",
                now.normalize(),
                d(&l, "qty").normalize(),
                s(&l, "item_name").unwrap_or("the item")
            )));
        }
        let mut v = Doc::new();
        v.insert("received_qty".into(), money(now.max(Decimal::ZERO)));
        hc.kernel.set_child_values_tx(&sctx, hc.tx, "SubcontractingOrder", "items", id(&l, "id").unwrap_or_default(), v).await?;
    }
    for m in rows(&order, "supplied_items") {
        let Some(q) = s(&m, "id").and_then(|k| consumed.get(k)) else { continue };
        if sign > Decimal::ZERO && *q > at_supplier(&m) {
            return Err(Error::invalid(format!(
                "the supplier holds only {} of {} for this order (sent {}, returned {}, used {})",
                at_supplier(&m).max(Decimal::ZERO).normalize(),
                s(&m, "rm_item_name").unwrap_or("the material"),
                d(&m, "supplied_qty").normalize(),
                d(&m, "returned_qty").normalize(),
                d(&m, "consumed_qty").normalize()
            )));
        }
        let mut v = Doc::new();
        v.insert("consumed_qty".into(), money((d(&m, "consumed_qty") + sign * q).max(Decimal::ZERO)));
        hc.kernel.set_child_values_tx(&sctx, hc.tx, "SubcontractingOrder", "supplied_items", id(&m, "id").unwrap_or_default(), v).await?;
    }
    refresh_order(hc, sco).await
}

#[async_trait]
impl DocHook for SubcontractingReceiptHook {
    /// Takes the supplier and service charges from the order, and rebuilds one consumption row
    /// per material of each received line (what the BOM requires for the quantity; a consumed
    /// quantity typed by the user is kept).
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let Some(sco) = id(doc, "subcontracting_order") else { return Ok(()) };
        let order = get(hc.kernel, hc.ctx, hc.tx, "SubcontractingOrder", sco).await?;
        for k in ["company", "supplier", "supplier_name", "supplier_warehouse"] {
            set(doc, k, order.get(k).cloned().unwrap_or(Value::Null));
        }
        let precision = match id(doc, "company") {
            Some(c) => company(hc.kernel, hc.ctx, hc.tx, c).await?.precision,
            None => 2,
        };
        let order_lines = rows(&order, "items");
        let mats = rows(&order, "supplied_items");
        let mut items = rows(doc, "items");
        let (mut total_qty, mut service_total) = (Decimal::ZERO, Decimal::ZERO);
        for r in items.iter_mut() {
            let line = match s(r, "sco_line") {
                Some(l) => order_lines.iter().find(|x| s(x, "id") == Some(l)),
                None => {
                    let same: Vec<&Doc> = order_lines.iter().filter(|x| id(x, "item") == id(r, "item")).collect();
                    same.iter().find(|x| d(x, "qty") > d(x, "received_qty")).or(same.first()).copied()
                }
            };
            if let Some(line) = line {
                set(r, "sco_line", line.get("id").cloned().unwrap_or(Value::Null));
                set(r, "service_rate", line.get("service_rate").cloned().unwrap_or(Value::Null));
                if id(r, "warehouse").is_none() {
                    set(r, "warehouse", line.get("warehouse").filter(|v| !v.is_null()).or(order.get("set_warehouse")).cloned().unwrap_or(Value::Null));
                }
            }
            let cost = round(d(r, "qty") * d(r, "service_rate"), precision);
            set(r, "service_cost", money(cost));
            total_qty += d(r, "qty");
            service_total += cost;
        }
        let kept: HashMap<(String, String), Doc> = rows(doc, "supplied_items")
            .into_iter()
            .filter_map(|r| Some(((s(&r, "receipt_line")?.to_string(), s(&r, "sco_supplied_item")?.to_string()), r)))
            .collect();
        let mut supplied = Vec::new();
        for r in &items {
            let (Some(rl), Some(line)) = (s(r, "id"), s(r, "sco_line").and_then(|l| order_lines.iter().find(|x| s(x, "id") == Some(l)))) else { continue };
            let ordered = d(line, "qty");
            for m in mats.iter().filter(|m| s(m, "sco_line") == s(line, "id")) {
                let mut row = kept.get(&(rl.to_string(), s(m, "id").unwrap_or_default().to_string())).cloned().unwrap_or_default();
                let need = if ordered.is_zero() { Decimal::ZERO } else { (d(m, "required_qty") / ordered * d(r, "qty")).round_dp(9) };
                set(&mut row, "receipt_line", rl);
                set(&mut row, "sco_supplied_item", m.get("id").cloned().unwrap_or(Value::Null));
                set(&mut row, "main_item", m.get("main_item").cloned().unwrap_or(Value::Null));
                set(&mut row, "rm_item", m.get("rm_item").cloned().unwrap_or(Value::Null));
                set(&mut row, "required_qty", money(need));
                supplied.push(row);
            }
        }
        let additional: Decimal = rows(doc, "additional_costs").iter().map(|r| d(r, "amount")).sum();
        set(doc, "items", rows_value(items));
        set(doc, "supplied_items", rows_value(supplied));
        set(doc, "total_qty", money(total_qty));
        set(doc, "service_cost_total", money(service_total));
        set(doc, "additional_cost_total", money(additional));
        set(doc, "status", "Draft");
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let Some(sco) = id(doc, "subcontracting_order") else { return Ok(()) };
        let order = get(hc.kernel, hc.ctx, hc.tx, "SubcontractingOrder", sco).await?;
        if d(&order, "docstatus") != Decimal::ONE {
            return Err(Error::field("subcontracting_order", "must be a submitted order"));
        }
        let cid = id(doc, "company");
        let lines = rows(&order, "items");
        let mut errors = Vec::new();
        let mut warehouses = HashMap::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            match s(r, "sco_line").and_then(|l| lines.iter().find(|x| s(x, "id") == Some(l))) {
                None => errors.push(FieldError::new("item", "is not on the subcontracting order").in_row("items", i)),
                Some(l) if id(l, "item") != id(r, "item") => errors.push(FieldError::new("item", "differs from the order line").in_row("items", i)),
                Some(_) => {}
            }
            match id(r, "warehouse") {
                None => errors.push(FieldError::new("warehouse", "is required").in_row("items", i)),
                Some(w) => {
                    if id(&cached(hc, &mut warehouses, "Warehouse", w).await?, "company") != cid {
                        errors.push(FieldError::new("warehouse", "belongs to another company").in_row("items", i));
                    }
                }
            }
        }
        for (i, r) in rows(doc, "additional_costs").iter().enumerate() {
            if let Some(a) = id(r, "account")
                && id(&get(hc.kernel, hc.ctx, hc.tx, "Account", a).await?, "company") != cid
            {
                errors.push(FieldError::new("account", "belongs to another company").in_row("additional_costs", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    /// Consumption left empty is what the BOM requires.
    async fn before_submit(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let mut supplied = rows(doc, "supplied_items");
        for r in supplied.iter_mut() {
            if r.get("consumed_qty").is_none_or(Value::is_null) {
                let need = r.get("required_qty").cloned().unwrap_or(Value::Null);
                set(r, "consumed_qty", need);
            }
        }
        set(doc, "supplied_items", rows_value(supplied));
        set(doc, "status", "Completed");
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let ts = posting_ts(doc)?;
        let sw = id(doc, "supplier_warehouse").ok_or_else(|| Error::field("supplier_warehouse", "is required"))?;
        let items = rows(doc, "items");
        let supplied: Vec<Doc> = rows(doc, "supplied_items").into_iter().filter(|r| d(r, "consumed_qty") > Decimal::ZERO).collect();

        // Stock balances, then the order — the order stock transfers lock them in.
        let mut keys: Vec<(Uuid, Uuid)> = supplied.iter().filter_map(|r| Some((id(r, "rm_item")?, sw))).collect();
        keys.extend(items.iter().filter_map(|r| Some((id(r, "item")?, id(r, "warehouse")?))));
        stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;
        // Order counters before moving stock: refuses over-receipt and consuming more than was sent.
        book_receipt(hc, doc, Decimal::ONE).await?;

        let issues: Vec<MoveIn> = supplied
            .iter()
            .map(|r| MoveIn {
                item: id(r, "rm_item").unwrap_or_default(),
                warehouse: sw,
                qty: -d(r, "consumed_qty"),
                batch_no: s(r, "batch_no").map(str::to_string),
                serials: parse_serials(s(r, "serial_no")),
                source_line: id(r, "id"),
                ..Default::default()
            })
            .collect();
        let issued = stock::post_moves(hc.tx, hc.ctx, &co, "SubcontractingReceipt", rid, on, ts, issues, "consumed").await?;
        let value_of: HashMap<Uuid, Decimal> = issued.iter().filter_map(|m| m.source_line.map(|l| (l, -m.value))).collect();
        let mut rm_cost: HashMap<String, Decimal> = HashMap::new();
        for r in &supplied {
            *rm_cost.entry(s(r, "receipt_line").unwrap_or_default().to_string()).or_default() +=
                id(r, "id").and_then(|x| value_of.get(&x)).copied().unwrap_or_default();
        }
        let additional: Decimal = rows(doc, "additional_costs").iter().map(|r| d(r, "amount")).sum();
        let weights: Vec<Decimal> = items.iter().map(|r| d(r, "qty")).collect();
        let shares = ferrum_calc::allocate(round(additional, co.precision), &weights, co.precision, co.rounding).map_err(|e| Error::invalid(e.to_string()))?;
        let mut values = Vec::new();
        let mut receipts = Vec::new();
        for (i, r) in items.iter().enumerate() {
            let rm = rm_cost.get(s(r, "id").unwrap_or_default()).copied().unwrap_or_default();
            let service = round(d(r, "qty") * d(r, "service_rate"), co.precision);
            let extra = shares.get(i).copied().unwrap_or_default();
            let value = rm + service + extra;
            values.push((rm, service, extra, value));
            receipts.push(MoveIn {
                item: id(r, "item").unwrap_or_default(),
                warehouse: id(r, "warehouse").ok_or_else(|| Error::Validation(vec![FieldError::new("warehouse", "is required").in_row("items", i)]))?,
                qty: d(r, "qty"),
                exact_value: Some(value),
                batch_no: s(r, "batch_no").map(str::to_string),
                serials: parse_serials(s(r, "serial_no")),
                source_line: id(r, "id"),
                ..Default::default()
            });
        }
        let received = stock::post_moves(hc.tx, hc.ctx, &co, "SubcontractingReceipt", rid, on, ts, receipts, "consumed").await?;

        // Goods at full value; materials out of the supplier warehouse; service and additional
        // costs out of the accounts that carry them until now.
        let mut entry = Entry::new(co.id, on, "SubcontractingReceipt", rid, s(doc, "number").map(str::to_string));
        entry.memo = s(doc, "remarks").map(str::to_string);
        let mut accounts = HashMap::new();
        for m in issued.iter().chain(received.iter()) {
            let acc = stock_account(hc, &co, m.warehouse, &mut accounts).await?;
            entry.lines.push(Line::new(acc, m.value, &co.currency_code));
        }
        let order_lines = rows(&get(hc.kernel, hc.ctx, hc.tx, "SubcontractingOrder", id(doc, "subcontracting_order").unwrap_or_default()).await?, "items");
        let cc = co.account("default_cost_center");
        for (r, (_, service, _, _)) in items.iter().zip(&values) {
            if service.is_zero() {
                continue;
            }
            let svc_item = s(r, "sco_line").and_then(|l| order_lines.iter().find(|x| s(x, "id") == Some(l))).and_then(|x| id(x, "service_item"));
            let acc = service_account(hc, &co, svc_item).await?;
            entry.lines.push(Line::new(acc, -*service, &co.currency_code).cost_center(cc).remarks("Subcontracting service"));
        }
        for c in rows(doc, "additional_costs") {
            let acc = id(&c, "account").ok_or_else(|| Error::field("additional_costs", "a row has no account"))?;
            entry.lines.push(Line::new(acc, -d(&c, "amount"), &co.currency_code).cost_center(cc).remarks(s(&c, "description").unwrap_or("Additional cost")));
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;

        let sctx = sys(hc.ctx);
        for (r, (rm, service, extra, value)) in items.iter().zip(&values) {
            let q = d(r, "qty");
            let mut v = Doc::new();
            v.insert("rm_cost".into(), money(*rm));
            v.insert("service_cost".into(), money(*service));
            v.insert("additional_cost".into(), money(*extra));
            v.insert("amount".into(), money(*value));
            v.insert("valuation_rate".into(), money(if q.is_zero() { Decimal::ZERO } else { (*value / q).round_dp(9) }));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "SubcontractingReceipt", "items", id(r, "id").unwrap_or_default(), v).await?;
        }
        for m in &issued {
            let Some(row) = m.source_line else { continue };
            let mut v = Doc::new();
            v.insert("rate".into(), money(m.rate.round_dp(9)));
            v.insert("amount".into(), money(-m.value));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "SubcontractingReceipt", "supplied_items", row, v).await?;
        }
        let mut v = Doc::new();
        v.insert("rm_cost_total".into(), money(values.iter().map(|x| x.0).sum()));
        v.insert("total_amount".into(), money(values.iter().map(|x| x.3).sum()));
        hc.kernel.set_values_tx(&sctx, hc.tx, "SubcontractingReceipt", rid, v).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        let keys: Vec<(Uuid, Uuid)> =
            sqlx::query_as("select item_id, warehouse_id from stock_move where tenant_id = $1 and source_entity = 'SubcontractingReceipt' and source_id = $2")
                .bind(hc.ctx.tenant)
                .bind(rid)
                .fetch_all(&mut **hc.tx)
                .await?;
        stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;
        stock::reverse_moves(hc.tx, hc.ctx, &co, "SubcontractingReceipt", rid, on, ts).await?;
        ledger::reverse(hc.tx, hc.ctx, "SubcontractingReceipt", rid, on, co.precision).await?;
        book_receipt(hc, doc, -Decimal::ONE).await?;
        let mut v = Doc::new();
        v.insert("status".into(), json!("Cancelled"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SubcontractingReceipt", rid, v).await
    }
}
