//! Document-to-document flows: progress counters on order lines, statuses, and mapping one
//! document into the next (order → delivery → invoice) for the quantities still open.

use crate::util::{d, id, money, rows, s, set, sys};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// Adds `delta` to a progress field on one order line, refusing to exceed the ordered quantity.
#[allow(clippy::too_many_arguments)]
pub async fn add_progress(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, doc_id: Uuid, line_id: Uuid, field: &str, delta: Decimal) -> Result<()> {
    let sctx = sys(ctx);
    let order = k.get_tx(&sctx, tx, entity, doc_id, true).await?;
    if d(&order, "docstatus") != Decimal::ONE {
        return Err(Error::invalid(format!("{entity} {} is not submitted", s(&order, "number").unwrap_or_default())));
    }
    let line = rows(&order, "items").into_iter().find(|r| id(r, "id") == Some(line_id)).ok_or_else(|| Error::invalid(format!("line not found on {entity}")))?;
    let qty = d(&line, "qty");
    let new = d(&line, field) + delta;
    if new > qty {
        let what = s(&line, "item_name").unwrap_or("item");
        return Err(Error::invalid(format!(
            "{what}: this would exceed the quantity on {entity} {} ({} > {})",
            s(&order, "number").unwrap_or_default(),
            new.normalize(),
            qty.normalize()
        )));
    }
    if new < Decimal::ZERO {
        return Err(Error::internal(format!("{field} would become negative on {entity} line {line_id}")));
    }
    let mut v = Doc::new();
    v.insert(field.into(), money(new));
    k.set_child_values_tx(&sctx, tx, entity, "items", line_id, v).await
}

fn pct(done: Decimal, total: Decimal) -> Decimal {
    if total.is_zero() { Decimal::ZERO } else { (done / total * Decimal::ONE_HUNDRED).round_dp(2).min(Decimal::ONE_HUNDRED) }
}

/// Recomputes completion percentages and status of a sales/purchase order.
pub async fn refresh_order(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, doc_id: Uuid, fulfil_field: &str, pct_field: &str) -> Result<()> {
    let sctx = sys(ctx);
    let order = k.get_tx(&sctx, tx, entity, doc_id, false).await?;
    let lines = rows(&order, "items");
    let total: Decimal = lines.iter().map(|l| d(l, "qty")).sum();
    let done: Decimal = lines.iter().map(|l| d(l, fulfil_field).min(d(l, "qty"))).sum();
    let billed: Decimal = lines.iter().map(|l| d(l, "billed_qty").min(d(l, "qty"))).sum();
    let (pf, pb) = (pct(done, total), pct(billed, total));
    let docstatus = d(&order, "docstatus");
    let current = s(&order, "status").unwrap_or_default();
    let (to_fulfil, both) = if fulfil_field == "delivered_qty" { ("To Deliver", "To Deliver and Bill") } else { ("To Receive", "To Receive and Bill") };
    let status = if docstatus == Decimal::TWO {
        "Cancelled"
    } else if docstatus.is_zero() {
        "Draft"
    } else if current == "Closed" {
        "Closed"
    } else if pf >= Decimal::ONE_HUNDRED && pb >= Decimal::ONE_HUNDRED {
        "Completed"
    } else if pf >= Decimal::ONE_HUNDRED {
        "To Bill"
    } else if pb >= Decimal::ONE_HUNDRED {
        to_fulfil
    } else {
        both
    };
    let mut v = Doc::new();
    v.insert(pct_field.into(), money(pf));
    v.insert("per_billed".into(), money(pb));
    v.insert("status".into(), json!(status));
    k.set_values_tx(&sctx, tx, entity, doc_id, v).await?;
    if entity == "SalesOrder" {
        crate::reservation::refresh_for_order(tx, ctx.tenant, doc_id).await?;
    }
    Ok(())
}

/// Recomputes `per_billed` and status of a delivery note / purchase receipt.
pub async fn refresh_billing(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, doc_id: Uuid) -> Result<()> {
    let sctx = sys(ctx);
    let doc = k.get_tx(&sctx, tx, entity, doc_id, false).await?;
    let lines = rows(&doc, "items");
    let total: Decimal = lines.iter().map(|l| d(l, "qty")).sum();
    let billed: Decimal = lines.iter().map(|l| d(l, "billed_qty").min(d(l, "qty"))).sum();
    let pb = pct(billed, total);
    let status = match (d(&doc, "docstatus"), crate::util::b(&doc, "is_return")) {
        (x, _) if x == Decimal::TWO => "Cancelled",
        (x, _) if x.is_zero() => "Draft",
        (_, true) => "Return",
        _ if pb >= Decimal::ONE_HUNDRED => "Completed",
        _ => "To Bill",
    };
    let mut v = Doc::new();
    v.insert("per_billed".into(), money(pb));
    v.insert("status".into(), json!(status));
    k.set_values_tx(&sctx, tx, entity, doc_id, v).await
}

/// Builds the input for a follow-up document from the open quantity of each source line.
pub struct MapSpec<'a> {
    pub done_field: &'a str,
    /// Field on the target line linking to the source document (e.g. `sales_order`).
    pub link_field: &'a str,
    /// Field on the target line holding the source line id (e.g. `so_line`).
    pub line_field: &'a str,
    pub header: &'a [&'a str],
    pub line_fields: &'a [&'a str],
}

/// Line fields the source and target documents share by name that carry over with the line, as
/// ERPNext's mapper does: fields that apps or customizations add to both sides travel along.
/// Fields the target computes, fetches or keeps read-only, and no-copy or secret fields, stay out.
pub fn shared_line_fields(schema: &ferrum_meta::Schema, source: &str, target: &str) -> Vec<String> {
    use ferrum_meta::FieldType;
    let lines = |e: &str| schema.entity(e).and_then(|ent| ent.children.get("items")).and_then(|c| schema.entity(&c.entity));
    let (Some(src), Some(dst)) = (lines(source), lines(target)) else { return vec![] };
    let movable = |f: &ferrum_meta::FieldDef| {
        !f.no_copy && !f.read_only && f.computed.is_none() && f.fetch_from.is_none() && !matches!(f.ftype, FieldType::Table | FieldType::Secret)
    };
    src.def.fields.iter().filter(|f| movable(f) && dst.field(&f.name).is_some_and(movable)).map(|f| f.name.clone()).collect()
}

pub fn map_document(src: &Doc, spec: &MapSpec<'_>) -> Result<Doc> {
    map_document_with(src, spec, &[])
}

/// [`map_document`] also carrying `extra` line fields (see [`shared_line_fields`]).
pub fn map_document_with(src: &Doc, spec: &MapSpec<'_>, extra: &[String]) -> Result<Doc> {
    let src_id = id(src, "id").ok_or_else(|| Error::internal("source without id"))?;
    let mut out = Doc::new();
    for h in spec.header {
        if let Some(v) = src.get(*h).filter(|v| !v.is_null()) {
            out.insert((*h).to_string(), v.clone());
        }
    }
    let mut lines = Vec::new();
    for r in rows(src, "items") {
        let open = d(&r, "qty") - d(&r, spec.done_field);
        if open <= Decimal::ZERO {
            continue;
        }
        let mut line = Map::new();
        for f in spec.line_fields.iter().copied().chain(extra.iter().map(String::as_str)) {
            if let Some(v) = r.get(f).filter(|v| !v.is_null()) {
                line.insert(f.to_string(), v.clone());
            }
        }
        line.insert("qty".into(), money(open));
        line.insert(spec.link_field.into(), json!(src_id.to_string()));
        line.insert(spec.line_field.into(), r.get("id").cloned().unwrap_or(Value::Null));
        lines.push(Value::Object(line));
    }
    if lines.is_empty() {
        return Err(Error::invalid("nothing is left to process on this document"));
    }
    out.insert("items".into(), Value::Array(lines));
    let taxes: Vec<Value> = rows(src, "taxes")
        .into_iter()
        .map(|mut t| {
            t.remove("id");
            for k in ["tax_amount", "total", "base_tax_amount", "base_total"] {
                t.remove(k);
            }
            Value::Object(t)
        })
        .collect();
    if !taxes.is_empty() {
        out.insert("taxes".into(), Value::Array(taxes));
    }
    Ok(out)
}

pub fn created(entity: &str, doc: &Doc) -> Value {
    json!({"entity": entity, "id": doc.get("id"), "number": doc.get("number")})
}

pub fn set_status(doc: &mut Doc, status: &str) {
    set(doc, "status", json!(status));
}

pub fn invoice_status(amount: Decimal, outstanding: Decimal, is_return: bool) -> &'static str {
    if is_return {
        "Return"
    } else if outstanding.is_zero() {
        "Paid"
    } else if outstanding == amount {
        "Unpaid"
    } else {
        "Partly Paid"
    }
}

/// Mirrors an open item's outstanding amount onto its invoice document.
pub async fn sync_invoice_outstanding(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, invoice: Uuid) -> Result<()> {
    if !matches!(entity, "SalesInvoice" | "PurchaseInvoice") || ctx.schema.entity(entity).is_none() {
        return Ok(());
    }
    let Some(item) = crate::ledger::load_open_item(tx, ctx.tenant, invoice, false).await? else { return Ok(()) };
    let sctx = sys(ctx);
    let doc = k.get_tx(&sctx, tx, entity, invoice, false).await?;
    let is_return = crate::util::b(&doc, "is_return");
    let status = if item.status == "cancelled" { "Cancelled" } else { invoice_status(item.amount, item.outstanding, is_return) };
    let mut v = Doc::new();
    v.insert("outstanding_amount".into(), money(item.outstanding));
    v.insert("status".into(), json!(status));
    k.set_values_tx(&sctx, tx, entity, invoice, v).await
}
