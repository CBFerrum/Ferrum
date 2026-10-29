//! Material requests and their fulfilment.
//!
//! A request line's `ordered_qty` and `received_qty` (stock units) are moved only by the
//! documents that fulfil it, inside their submit/cancel transactions and under a row lock on the
//! request: purchase order lines order, receipt lines (and stock-updating invoice lines) receive,
//! and stock entry lines both order and receive (a transfer, issue or customer delivery is done
//! when the goods move). Ordering more than requested (plus the configured allowance) or against
//! a stopped request is refused; receiving is never refused, since the goods are already there.

use crate::{installed, scorecard, settings};
use async_trait::async_trait;
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{b, d, get, id, money, rows, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

pub const PURCHASE: &str = "Purchase";
pub const TRANSFER: &str = "Material Transfer";
pub const ISSUE: &str = "Material Issue";
pub const CUSTOMER_PROVIDED: &str = "Customer Provided";
const STOPPED: &str = "Stopped";

fn kind(doc: &Doc) -> &str {
    s(doc, "material_request_type").unwrap_or(PURCHASE)
}

fn pct(done: Decimal, total: Decimal) -> Decimal {
    if total.is_zero() { Decimal::ZERO } else { (done / total * Decimal::ONE_HUNDRED).round_dp(2).min(Decimal::ONE_HUNDRED) }
}

/// The status a submitted, running request's fulfilment gives it. Transfers and customer-provided
/// goods are received when moved, issues issued; purchases go ordered → received.
pub fn status_of(kind: &str, per_ordered: Decimal, per_received: Decimal) -> &'static str {
    let full = Decimal::ONE_HUNDRED;
    let some = |p: Decimal| p > Decimal::ZERO;
    match kind {
        PURCHASE if per_received >= full => "Received",
        PURCHASE if some(per_received) => "Partially Received",
        TRANSFER if per_ordered >= full => "Transferred",
        ISSUE if per_ordered >= full => "Issued",
        CUSTOMER_PROVIDED if per_ordered >= full => "Received",
        TRANSFER | CUSTOMER_PROVIDED if some(per_ordered) => "Partially Received",
        _ if per_ordered >= full => "Ordered",
        _ if some(per_ordered) => "Partially Ordered",
        _ => "Pending",
    }
}

/// Percentages and status from the lines (which already carry their new quantities).
fn status_values(doc: &Doc, lines: &[Doc], stopped: bool) -> Doc {
    let total: Decimal = lines.iter().map(|l| d(l, "stock_qty")).sum();
    let ordered: Decimal = lines.iter().map(|l| d(l, "ordered_qty").min(d(l, "stock_qty"))).sum();
    let received: Decimal = lines.iter().map(|l| d(l, "received_qty").min(d(l, "stock_qty"))).sum();
    let (po, pr) = (pct(ordered, total), pct(received, total));
    let status = match d(doc, "docstatus") {
        x if x.is_zero() => "Draft",
        x if x == Decimal::TWO => "Cancelled",
        _ if stopped => STOPPED,
        _ => status_of(kind(doc), po, pr),
    };
    let mut v = Doc::new();
    v.insert("per_ordered".into(), money(po));
    v.insert("per_received".into(), money(pr));
    v.insert("status".into(), json!(status));
    v
}

// ───────────────────────────── fulfilment ─────────────────────────────

/// How a linked document line moves its request line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fulfil {
    /// Purchase order lines: ordered quantity.
    Order,
    /// Receipt lines and stock-updating invoice lines: received quantity.
    Receive,
    /// Stock entry lines move the goods: ordered and received, for requests of this type.
    Move(&'static str),
}

struct Delta {
    line: Uuid,
    item: Option<Uuid>,
    qty: Decimal,
    /// Row of the linked document, for error messages.
    row: usize,
}

/// The request lines a document's lines fulfil, per request in id order (so documents touching
/// several requests lock them in the same order and cannot deadlock each other).
fn linked(doc: &Doc, qty: impl Fn(&Doc) -> Decimal) -> BTreeMap<Uuid, Vec<Delta>> {
    let mut out: BTreeMap<Uuid, Vec<Delta>> = BTreeMap::new();
    for (i, r) in rows(doc, "items").iter().enumerate() {
        if let (Some(mr), Some(line)) = (id(r, "proc_material_request"), id(r, "proc_mr_line")) {
            out.entry(mr).or_default().push(Delta { line, item: id(r, "item"), qty: qty(r), row: i });
        }
    }
    out
}

fn row_error(field: &str, row: usize, msg: impl Into<String>) -> Error {
    Error::Validation(vec![FieldError::new(field, msg).in_row("items", row)])
}

/// Applies `sign` × the deltas to one request, or with `write == false` only checks that they
/// would be accepted (drafts are checked against what submitted documents already ordered).
async fn progress(hc: &mut HookCtx<'_>, mr: Uuid, deltas: &[Delta], fulfil: Fulfil, sign: Decimal, write: bool) -> Result<()> {
    let sctx = sys(hc.ctx);
    let doc = hc.kernel.get_tx(&sctx, hc.tx, "MaterialRequest", mr, write).await?;
    let number = s(&doc, "number").unwrap_or_default().to_string();
    if d(&doc, "docstatus") != Decimal::ONE {
        return Err(Error::invalid(format!("Material Request {number} is not submitted")));
    }
    let request_kind = kind(&doc).to_string();
    match fulfil {
        Fulfil::Order if request_kind != PURCHASE => {
            return Err(Error::invalid(format!("Material Request {number} is a {request_kind} request; only purchase requests are ordered from suppliers")));
        }
        Fulfil::Move(expected) if request_kind != expected => {
            return Err(Error::invalid(format!("Material Request {number} is a {request_kind} request, which this kind of stock entry does not fulfil")));
        }
        _ => {}
    }
    let stopped = s(&doc, "status") == Some(STOPPED);
    if stopped && sign > Decimal::ZERO && fulfil != Fulfil::Receive {
        return Err(Error::invalid(format!("Material Request {number} is stopped; resume it before ordering against it")));
    }
    let allowance = settings(hc.tx, hc.ctx.tenant).await?.over_order_allowance;
    let mut lines = rows(&doc, "items");
    let mut touched: Vec<Uuid> = Vec::new();
    for dl in deltas {
        let Some(line) = lines.iter_mut().find(|l| id(l, "id") == Some(dl.line)) else {
            return Err(row_error("item", dl.row, format!("the line of Material Request {number} this row fulfils no longer exists")));
        };
        if dl.item.is_some() && id(line, "item") != dl.item {
            return Err(row_error("item", dl.row, format!("the item differs from the one on Material Request {number}")));
        }
        let q = sign * dl.qty;
        let name = s(line, "item_name").unwrap_or("item").to_string();
        if fulfil != Fulfil::Receive {
            let new = d(line, "ordered_qty") + q;
            let requested = d(line, "stock_qty");
            let limit = (requested * (Decimal::ONE + allowance / Decimal::ONE_HUNDRED)).round_dp(9);
            if q > Decimal::ZERO && new > limit {
                return Err(row_error(
                    "qty",
                    dl.row,
                    format!(
                        "{name}: this brings the quantity ordered against Material Request {number} to {} while {} was requested",
                        new.normalize(),
                        requested.normalize()
                    ),
                ));
            }
            set(line, "ordered_qty", money(new.max(Decimal::ZERO)));
        }
        if fulfil != Fulfil::Order {
            let new = (d(line, "received_qty") + q).max(Decimal::ZERO);
            set(line, "received_qty", money(new));
        }
        if !touched.contains(&dl.line) {
            touched.push(dl.line);
        }
    }
    if !write {
        return Ok(());
    }
    for line_id in touched {
        let Some(line) = lines.iter().find(|l| id(l, "id") == Some(line_id)) else { continue };
        let mut v = Doc::new();
        v.insert("ordered_qty".into(), money(d(line, "ordered_qty")));
        v.insert("received_qty".into(), money(d(line, "received_qty")));
        hc.kernel.set_child_values_tx(&sctx, hc.tx, "MaterialRequest", "items", line_id, v).await?;
    }
    let values = status_values(&doc, &lines, stopped);
    hc.kernel.set_values_tx(&sctx, hc.tx, "MaterialRequest", mr, values).await
}

async fn apply(hc: &mut HookCtx<'_>, links: BTreeMap<Uuid, Vec<Delta>>, fulfil: Fulfil, sign: Decimal, write: bool) -> Result<()> {
    for (mr, deltas) in links {
        progress(hc, mr, &deltas, fulfil, sign, write).await?;
    }
    Ok(())
}

// ───────────────────────────── the request ─────────────────────────────

pub struct MaterialRequestHook;

#[async_trait]
impl DocHook for MaterialRequestHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let k = kind(doc).to_string();
        if k != CUSTOMER_PROVIDED {
            doc.insert("customer".into(), Value::Null);
        }
        let header = |f: &str, doc: &Doc| doc.get(f).filter(|v| !v.is_null()).cloned();
        let (to, from, by) = (header("set_warehouse", doc), header("set_from_warehouse", doc), header("schedule_date", doc));
        let mut items = rows(doc, "items");
        for r in items.iter_mut() {
            if id(r, "warehouse").is_none()
                && let Some(w) = &to
            {
                r.insert("warehouse".into(), w.clone());
            }
            if k == TRANSFER {
                if id(r, "from_warehouse").is_none()
                    && let Some(w) = &from
                {
                    r.insert("from_warehouse".into(), w.clone());
                }
            } else {
                r.insert("from_warehouse".into(), Value::Null);
            }
            if s(r, "schedule_date").is_none()
                && let Some(day) = &by
            {
                r.insert("schedule_date".into(), day.clone());
            }
            if id(r, "uom").is_none()
                && let Some(u) = r.get("stock_uom").filter(|v| !v.is_null()).cloned()
            {
                r.insert("uom".into(), u);
            }
            if id(r, "uom").is_some() && id(r, "uom") == id(r, "stock_uom") {
                r.insert("conversion_factor".into(), json!("1"));
            }
            let cf = d(r, "conversion_factor");
            let cf = if cf > Decimal::ZERO { cf } else { Decimal::ONE };
            set(r, "stock_qty", money((d(r, "qty") * cf).round_dp(9)));
        }
        set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
        if d(doc, "docstatus").is_zero() {
            set(doc, "status", "Draft");
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let k = kind(doc).to_string();
        let moves_stock = matches!(k.as_str(), TRANSFER | ISSUE | CUSTOMER_PROVIDED);
        let company = id(doc, "company");
        let requested_on = s(doc, "transaction_date").unwrap_or_default().to_string();
        let mut errors = Vec::new();
        let mut items: HashMap<Uuid, Doc> = HashMap::new();
        let mut warehouses: HashMap<Uuid, Doc> = HashMap::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            let mut err = |f: &str, m: String| errors.push(FieldError::new(f, m).in_row("items", i));
            if let Some(item) = id(r, "item") {
                if let Entry::Vacant(e) = items.entry(item) {
                    e.insert(get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?);
                }
                let it = &items[&item];
                if b(it, "disabled") {
                    err("item", format!("{} is disabled", s(it, "item_name").unwrap_or("this item")));
                } else if moves_stock && !b(it, "is_stock_item") {
                    err("item", format!("{} is not a stock item, so it cannot be moved by a stock entry", s(it, "item_name").unwrap_or("this item")));
                }
            }
            let Some(wh) = id(r, "warehouse") else {
                err("warehouse", "is required".into());
                continue;
            };
            for w in [Some(wh), id(r, "from_warehouse")].into_iter().flatten() {
                if let Entry::Vacant(e) = warehouses.entry(w) {
                    e.insert(get(hc.kernel, hc.ctx, hc.tx, "Warehouse", w).await?);
                }
                if id(&warehouses[&w], "company") != company {
                    let f = if w == wh { "warehouse" } else { "from_warehouse" };
                    err(f, format!("{} belongs to another company", s(&warehouses[&w], "warehouse_name").unwrap_or("this warehouse")));
                }
            }
            if id(r, "from_warehouse") == Some(wh) {
                err("from_warehouse", "must differ from the target warehouse".into());
            }
            if s(r, "schedule_date").is_some_and(|day| day < requested_on.as_str()) {
                err("schedule_date", "cannot be before the request date".into());
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mr = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let values = status_values(doc, &rows(doc, "items"), false);
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "MaterialRequest", mr, values).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if rows(doc, "items").iter().any(|r| d(r, "ordered_qty") > Decimal::ZERO || d(r, "received_qty") > Decimal::ZERO) {
            return Err(Error::invalid("cancel the purchase orders, receipts and stock entries made against this request first"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mr = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let values = status_values(doc, &rows(doc, "items"), false);
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "MaterialRequest", mr, values).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let mr = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let out = match name {
            "make_purchase_order" => make_purchase_order(hc, doc, params).await?,
            "make_rfq" => make_rfq(hc, doc, params).await?,
            "make_stock_entry" => make_stock_entry(hc, doc, params).await?,
            "stop" | "resume" => {
                let stop = name == "stop";
                if stop == (s(doc, "status") == Some(STOPPED)) {
                    return Err(Error::invalid(if stop { "this request is already stopped" } else { "this request is not stopped" }));
                }
                let values = status_values(doc, &rows(doc, "items"), stop);
                let status = values["status"].clone();
                hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "MaterialRequest", mr, values).await?;
                json!({"status": status})
            }
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}

// ───────────────────────────── actions ─────────────────────────────

fn ensure_open(doc: &Doc, kinds: &[&str], what: &str) -> Result<()> {
    if s(doc, "status") == Some(STOPPED) {
        return Err(Error::invalid("this request is stopped; resume it first"));
    }
    if !kinds.contains(&kind(doc)) {
        return Err(Error::invalid(format!("{what} is not available for {} requests", kind(doc))));
    }
    Ok(())
}

pub(crate) fn uuid_param(params: &Value, key: &str) -> Result<Option<Uuid>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(v)) if v.trim().is_empty() => Ok(None),
        Some(Value::String(v)) => v.trim().parse().map(Some).map_err(|_| Error::field(key, "is not a valid id")),
        Some(_) => Err(Error::field(key, "is not a valid id")),
    }
}

pub(crate) fn uuid_list(params: &Value, key: &str) -> Result<Option<Vec<Uuid>>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            a.iter().map(|v| v.as_str().and_then(|x| x.parse().ok()).ok_or_else(|| Error::field(key, "must be a list of ids"))).collect::<Result<_>>().map(Some)
        }
        Some(_) => Err(Error::field(key, "must be a list of ids")),
    }
}

/// Lines still to be ordered (or moved), with their open stock quantity; `only` picks lines by id.
fn open_lines(doc: &Doc, only: Option<&[Uuid]>) -> Vec<(Doc, Decimal)> {
    rows(doc, "items")
        .into_iter()
        .filter(|l| only.is_none_or(|ids| id(l, "id").is_some_and(|i| ids.contains(&i))))
        .map(|l| {
            let open = d(&l, "stock_qty") - d(&l, "ordered_qty");
            (l, open)
        })
        .filter(|(_, open)| *open > Decimal::ZERO)
        .collect()
}

/// The item's approved suppliers in priority order, leaving out disabled ones and those whose
/// scorecard refuses `purpose` ("pos" or "rfqs").
async fn approved_suppliers(hc: &mut HookCtx<'_>, item: &Doc, purpose: &str, cache: &mut HashMap<Uuid, bool>) -> Result<Vec<Uuid>> {
    let mut ranked: Vec<(i64, usize, Uuid)> = rows(item, "suppliers")
        .iter()
        .enumerate()
        .filter(|(_, r)| r.get("approved").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|(i, r)| Some((r.get("priority").and_then(Value::as_i64).unwrap_or(1), i, id(r, "supplier")?)))
        .collect();
    ranked.sort();
    let mut out = Vec::new();
    for (_, _, sup) in ranked {
        let usable = match cache.get(&sup) {
            Some(u) => *u,
            None => {
                let supplier = get(hc.kernel, hc.ctx, hc.tx, "Supplier", sup).await?;
                let st = scorecard::standing(hc.kernel, hc.ctx, hc.tx, sup).await?;
                let blocked = st.is_some_and(|st| if purpose == "pos" { st.prevent_pos } else { st.prevent_rfqs });
                let u = !b(&supplier, "disabled") && !blocked;
                cache.insert(sup, u);
                u
            }
        };
        if usable && !out.contains(&sup) {
            out.push(sup);
        }
    }
    Ok(out)
}

/// One purchase order per supplier: each line goes to its item's preferred usable supplier, or
/// to `supplier` (param) when the item has none. `items` (param) limits the lines.
async fn make_purchase_order(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    ensure_open(doc, &[PURCHASE], "Ordering")?;
    let mr = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let fallback = uuid_param(params, "supplier")?;
    let only = uuid_list(params, "items")?;
    let lines = open_lines(doc, only.as_deref());
    if lines.is_empty() {
        return Err(Error::invalid("nothing is left to order on this request"));
    }
    let mut groups: Vec<(Uuid, Vec<Value>)> = Vec::new();
    let mut missing = Vec::new();
    let mut usable = HashMap::new();
    for (l, open) in lines {
        let Some(item_id) = id(&l, "item") else { continue };
        let item = get(hc.kernel, hc.ctx, hc.tx, "Item", item_id).await?;
        let preferred = approved_suppliers(hc, &item, "pos", &mut usable).await?.first().copied();
        let Some(supplier) = preferred.or(fallback) else {
            missing.push(s(&item, "item_name").unwrap_or("an item").to_string());
            continue;
        };
        let line = json!({
            "item": item_id.to_string(), "qty": money(open), "warehouse": l.get("warehouse"), "schedule_date": l.get("schedule_date"),
            "proc_material_request": mr.to_string(), "proc_mr_line": l.get("id"),
        });
        match groups.iter_mut().find(|(s, _)| *s == supplier) {
            Some((_, g)) => g.push(line),
            None => groups.push((supplier, vec![line])),
        }
    }
    if !missing.is_empty() {
        return Err(Error::field("supplier", format!("no usable approved supplier for {}: choose a supplier for them", missing.join(", "))));
    }
    let mut documents = Vec::new();
    for (supplier, lines) in groups {
        let input = json!({"company": doc.get("company"), "supplier": supplier.to_string(), "schedule_date": doc.get("schedule_date"), "items": lines});
        let po = create_for_user(hc, "PurchaseOrder", input.as_object().cloned().unwrap_or_default()).await?;
        let mut c = created("PurchaseOrder", &po);
        c["supplier"] = json!(supplier.to_string());
        documents.push(c);
    }
    let mut out = documents.first().cloned().unwrap_or(Value::Null);
    out["documents"] = Value::Array(documents);
    Ok(out)
}

/// A request for quotation for the open lines, inviting `suppliers` (param) or the items'
/// approved suppliers that may receive RFQs.
async fn make_rfq(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    ensure_open(doc, &[PURCHASE], "A request for quotation")?;
    let mr = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let only = uuid_list(params, "items")?;
    let lines = open_lines(doc, only.as_deref());
    if lines.is_empty() {
        return Err(Error::invalid("nothing is left to order on this request"));
    }
    let mut suppliers = uuid_list(params, "suppliers")?.unwrap_or_default();
    let defaults = suppliers.is_empty();
    let mut usable = HashMap::new();
    let mut items = Vec::new();
    for (l, open) in lines {
        let Some(item_id) = id(&l, "item") else { continue };
        if defaults {
            let item = get(hc.kernel, hc.ctx, hc.tx, "Item", item_id).await?;
            for sup in approved_suppliers(hc, &item, "rfqs", &mut usable).await? {
                if !suppliers.contains(&sup) {
                    suppliers.push(sup);
                }
            }
        }
        items.push(json!({
            "item": item_id.to_string(), "qty": money(open), "warehouse": l.get("warehouse"), "schedule_date": l.get("schedule_date"),
            "material_request": mr.to_string(), "mr_line": l.get("id"),
        }));
    }
    let input = json!({
        "company": doc.get("company"), "schedule_date": doc.get("schedule_date"), "items": items,
        "suppliers": suppliers.iter().map(|s| json!({"supplier": s.to_string()})).collect::<Vec<_>>(),
    });
    let rfq = create_for_user(hc, "RequestForQuotation", input.as_object().cloned().unwrap_or_default()).await?;
    Ok(created("RequestForQuotation", &rfq))
}

/// The stock entry that moves the open quantities: a transfer (from each line's source, the
/// request's, or `from_warehouse` param), an issue, or a zero-cost receipt of customer goods.
async fn make_stock_entry(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    ensure_open(doc, &[TRANSFER, ISSUE, CUSTOMER_PROVIDED], "A stock entry")?;
    let mr = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let k = kind(doc).to_string();
    let source = uuid_param(params, "from_warehouse")?.or(id(doc, "set_from_warehouse"));
    let lines = open_lines(doc, uuid_list(params, "items")?.as_deref());
    if lines.is_empty() {
        return Err(Error::invalid("nothing is left to move on this request"));
    }
    let purpose = match k.as_str() {
        TRANSFER => "Material Transfer",
        ISSUE => "Material Issue",
        _ => "Material Receipt",
    };
    let mut items = Vec::new();
    for (l, open) in lines {
        let mut line = json!({"item": l.get("item"), "qty": money(open), "proc_material_request": mr.to_string(), "proc_mr_line": l.get("id")});
        match k.as_str() {
            TRANSFER => {
                let from = id(&l, "from_warehouse").or(source).ok_or_else(|| Error::field("from_warehouse", "choose the warehouse to transfer from"))?;
                line["source_warehouse"] = json!(from.to_string());
                line["target_warehouse"] = l.get("warehouse").cloned().unwrap_or(Value::Null);
            }
            ISSUE => line["source_warehouse"] = l.get("warehouse").cloned().unwrap_or(Value::Null),
            _ => {
                // The customer owns these goods: they come in at no cost.
                line["target_warehouse"] = l.get("warehouse").cloned().unwrap_or(Value::Null);
                line["basic_rate"] = json!("0");
            }
        }
        items.push(line);
    }
    let remarks = format!("Material Request {}", s(doc, "number").unwrap_or_default());
    let input = json!({"company": doc.get("company"), "purpose": purpose, "items": items, "remarks": remarks});
    let entry = create_for_user(hc, "StockEntry", input.as_object().cloned().unwrap_or_default()).await?;
    Ok(created("StockEntry", &entry))
}

// ───────────────────────── fulfilling documents ─────────────────────────

/// Purchase orders: order against requests and quotations; the supplier's scorecard may warn or refuse.
pub struct PurchaseOrderLinks;

#[async_trait]
impl DocHook for PurchaseOrderLinks {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let warning = match id(doc, "supplier") {
            Some(sup) => scorecard::standing(hc.kernel, hc.ctx, hc.tx, sup).await?.filter(|st| st.warn_pos || st.prevent_pos).map(|st| st.label()),
            None => None,
        };
        set(doc, "proc_supplier_standing", warning.map(Value::String).unwrap_or(Value::Null));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        if let Some(sup) = id(doc, "supplier")
            && let Some(st) = scorecard::standing(hc.kernel, hc.ctx, hc.tx, sup).await?
            && st.prevent_pos
        {
            return Err(Error::field(
                "supplier",
                format!(
                    "{} stands at {} on its supplier scorecard, which blocks new purchase orders",
                    s(doc, "supplier_name").unwrap_or("this supplier"),
                    st.label()
                ),
            ));
        }
        apply(hc, linked(doc, |r| d(r, "qty")), Fulfil::Order, Decimal::ONE, false).await?;
        crate::quotation::check_order(hc, doc).await
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        apply(hc, linked(doc, |r| d(r, "qty")), Fulfil::Order, Decimal::ONE, true).await?;
        crate::quotation::order_progress(hc, doc, Decimal::ONE).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        apply(hc, linked(doc, |r| d(r, "qty")), Fulfil::Order, -Decimal::ONE, true).await?;
        crate::quotation::order_progress(hc, doc, -Decimal::ONE).await
    }
}

/// Purchase receipts, and purchase invoices that update stock: receive against requests. Their
/// lines take the request link of the order line they come from.
pub struct ReceiptLinks;

impl ReceiptLinks {
    async fn apply(&self, hc: &mut HookCtx<'_>, doc: &Doc, sign: Decimal) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let invoice = hc.entity.def.name == "PurchaseInvoice";
        if invoice && !b(doc, "update_stock") {
            return Ok(());
        }
        // Receipt returns flag the document; invoices carry negative quantities instead.
        let ret = !invoice && b(doc, "is_return");
        apply(hc, linked(doc, |r| if ret { -d(r, "qty") } else { d(r, "qty") }), Fulfil::Receive, sign, true).await
    }
}

#[async_trait]
impl DocHook for ReceiptLinks {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let mut orders: HashMap<Uuid, Doc> = HashMap::new();
        let mut items = rows(doc, "items");
        for r in items.iter_mut() {
            let (Some(po), Some(po_line)) = (id(r, "purchase_order"), id(r, "po_line")) else { continue };
            if let Entry::Vacant(e) = orders.entry(po) {
                e.insert(get(hc.kernel, hc.ctx, hc.tx, "PurchaseOrder", po).await?);
            }
            let line = rows(&orders[&po], "items").into_iter().find(|l| id(l, "id") == Some(po_line));
            for f in ["proc_material_request", "proc_mr_line"] {
                r.insert(f.into(), line.as_ref().and_then(|l| l.get(f)).cloned().unwrap_or(Value::Null));
            }
        }
        set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.apply(hc, doc, Decimal::ONE).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.apply(hc, doc, -Decimal::ONE).await
    }
}

/// Stock entries made from transfer, issue and customer-provided requests.
pub struct StockEntryLinks;

/// The request type a stock entry purpose fulfils.
fn fulfils(purpose: Option<&str>) -> Option<&'static str> {
    match purpose {
        Some("Material Transfer") => Some(TRANSFER),
        Some("Material Issue") => Some(ISSUE),
        Some("Material Receipt") => Some(CUSTOMER_PROVIDED),
        _ => None,
    }
}

impl StockEntryLinks {
    async fn apply(&self, hc: &mut HookCtx<'_>, doc: &Doc, sign: Decimal, write: bool) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let links = linked(doc, |r| d(r, "qty"));
        if links.is_empty() {
            return Ok(());
        }
        let Some(kind) = fulfils(s(doc, "purpose")) else {
            return Err(Error::field("purpose", "only transfers, issues and receipts can fulfil material requests"));
        };
        apply(hc, links, Fulfil::Move(kind), sign, write).await
    }
}

#[async_trait]
impl DocHook for StockEntryLinks {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.apply(hc, doc, Decimal::ONE, false).await
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.apply(hc, doc, Decimal::ONE, true).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.apply(hc, doc, -Decimal::ONE, true).await
    }
}

/// `day` plus `days`, as a document date.
pub(crate) fn days_after(day: chrono::NaiveDate, days: i64) -> String {
    (day + chrono::Duration::days(days.max(0))).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn statuses_follow_erpnext() {
        assert_eq!(status_of(PURCHASE, n("0"), n("0")), "Pending");
        assert_eq!(status_of(PURCHASE, n("60"), n("0")), "Partially Ordered");
        assert_eq!(status_of(PURCHASE, n("100"), n("0")), "Ordered");
        assert_eq!(status_of(PURCHASE, n("100"), n("40")), "Partially Received");
        assert_eq!(status_of(PURCHASE, n("100"), n("100")), "Received");
        assert_eq!(status_of(TRANSFER, n("50"), n("50")), "Partially Received");
        assert_eq!(status_of(TRANSFER, n("100"), n("100")), "Transferred");
        assert_eq!(status_of(ISSUE, n("30"), n("30")), "Partially Ordered");
        assert_eq!(status_of(ISSUE, n("100"), n("100")), "Issued");
        assert_eq!(status_of(CUSTOMER_PROVIDED, n("100"), n("100")), "Received");
        assert_eq!(status_of("Manufacture", n("100"), n("0")), "Ordered");
    }

    #[test]
    fn status_values_cap_and_respect_stop() {
        let doc = json!({"docstatus": 1, "material_request_type": PURCHASE}).as_object().cloned().unwrap();
        let lines: Vec<Doc> = [json!({"stock_qty": "100", "ordered_qty": "120", "received_qty": "0"}), json!({"stock_qty": "50", "ordered_qty": "0"})]
            .into_iter()
            .map(|v| v.as_object().cloned().unwrap())
            .collect();
        let v = status_values(&doc, &lines, false);
        assert_eq!((v["per_ordered"].clone(), v["status"].clone()), (json!("66.67"), json!("Partially Ordered")), "over-ordering counts as 100% of its line");
        assert_eq!(status_values(&doc, &lines, true)["status"], json!("Stopped"));
    }
}
