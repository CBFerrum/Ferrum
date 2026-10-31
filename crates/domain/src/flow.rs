//! Document-to-document flows shared by every app: progress counters on order lines, mapping one
//! document into the next (order → delivery → invoice) for the quantities still open, and making
//! a follow-up document for the user. ERP's statuses of its orders, deliveries and invoices are
//! ferrum-app-erp's.

use crate::util::{d, elevated, id, money, rows, s, set, sys, unit_factor};
use ferrum_kernel::{Action, Ctx, Doc, Error, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// Quantities counted through another unit can land a hair off; this close, they are equal.
pub const QTY_TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 6);

/// Adds `delta` to a progress field on one order line, refusing to exceed the ordered quantity.
#[allow(clippy::too_many_arguments)]
pub async fn add_progress(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, doc_id: Uuid, line_id: Uuid, field: &str, delta: Decimal) -> Result<()> {
    add_progress_in(k, ctx, tx, entity, doc_id, line_id, field, delta, None).await
}

/// [`add_progress`] for `delta` counted on a line whose unit holds `factor` stock units: a box of
/// 12 delivered against an order line in pieces counts 12.
#[allow(clippy::too_many_arguments)]
pub async fn add_progress_in(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    entity: &str,
    doc_id: Uuid,
    line_id: Uuid,
    field: &str,
    delta: Decimal,
    factor: Option<Decimal>,
) -> Result<()> {
    let sctx = sys(ctx);
    let order = k.get_tx(&sctx, tx, entity, doc_id, true).await?;
    if d(&order, "docstatus") != Decimal::ONE {
        return Err(Error::invalid(format!("{entity} {} is not submitted", s(&order, "number").unwrap_or_default())));
    }
    let line = rows(&order, "items").into_iter().find(|r| id(r, "id") == Some(line_id)).ok_or_else(|| Error::invalid(format!("line not found on {entity}")))?;
    let own = unit_factor(&line);
    let delta = match factor {
        Some(f) if f > Decimal::ZERO && f != own => (delta * f / own).round_dp(9),
        _ => delta,
    };
    let qty = d(&line, "qty");
    let mut new = d(&line, field) + delta;
    if (new - qty).abs() <= QTY_TOLERANCE {
        new = qty;
    } else if new.abs() <= QTY_TOLERANCE {
        new = Decimal::ZERO;
    }
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
        if open <= QTY_TOLERANCE {
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

/// Creates a follow-up document for the user. The links back to the source lines are read-only
/// fields, which only the engine may set, so the insert runs elevated after checking that the
/// user may create the target.
pub async fn create_for_user(hc: &mut HookCtx<'_>, entity: &str, input: Doc) -> Result<Doc> {
    hc.ctx.require(entity, Action::Create)?;
    hc.kernel.insert_tx(&elevated(hc.ctx), hc.tx, entity, input).await
}

pub fn created(entity: &str, doc: &Doc) -> Value {
    json!({"entity": entity, "id": doc.get("id"), "number": doc.get("number")})
}

pub fn set_status(doc: &mut Doc, status: &str) {
    set(doc, "status", json!(status));
}
