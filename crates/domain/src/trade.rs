//! Selling and buying documents, implemented once and parameterized by side:
//! orders (SalesOrder / PurchaseOrder), shipments (DeliveryNote / PurchaseReceipt) and invoices
//! (SalesInvoice / PurchaseInvoice), including stock, ledger postings, open items, returns and
//! the "create next document" actions.

use crate::flow::{self, MapSpec, add_progress, created, map_document, refresh_billing, refresh_order, sync_invoice_outstanding};
use crate::ledger::{self, Entry, Line, OpenItem};
use crate::pricing::{Side, breakup, calculate};
use crate::stock::{self, MoveIn, Posted, parse_serials};
use crate::util::{Company, b, company, d, date, elevated, get, id, money, posting_ts, round, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_kernel::{Action, Doc, DocHook, Error, FieldError, Filter, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

struct Names {
    order: &'static str,
    shipment: &'static str,
    invoice: &'static str,
    party: &'static str,
    party_entity: &'static str,
    fulfil_field: &'static str,
    fulfil_pct: &'static str,
    order_link: &'static str,
    order_line: &'static str,
    shipment_link: &'static str,
    shipment_line: &'static str,
    make_shipment: &'static str,
    make_invoice: &'static str,
    serial_status: &'static str,
}

const SELLING: Names = Names {
    order: "SalesOrder",
    shipment: "DeliveryNote",
    invoice: "SalesInvoice",
    party: "customer",
    party_entity: "Customer",
    fulfil_field: "delivered_qty",
    fulfil_pct: "per_delivered",
    order_link: "sales_order",
    order_line: "so_line",
    shipment_link: "delivery_note",
    shipment_line: "dn_line",
    make_shipment: "make_delivery_note",
    make_invoice: "make_sales_invoice",
    serial_status: "delivered",
};

const BUYING: Names = Names {
    order: "PurchaseOrder",
    shipment: "PurchaseReceipt",
    invoice: "PurchaseInvoice",
    party: "supplier",
    party_entity: "Supplier",
    fulfil_field: "received_qty",
    fulfil_pct: "per_received",
    order_link: "purchase_order",
    order_line: "po_line",
    shipment_link: "purchase_receipt",
    shipment_line: "pr_line",
    make_shipment: "make_purchase_receipt",
    make_invoice: "make_purchase_invoice",
    serial_status: "consumed",
};

fn names(side: Side) -> &'static Names {
    match side {
        Side::Selling => &SELLING,
        Side::Buying => &BUYING,
    }
}

fn header_fields(side: Side) -> Vec<&'static str> {
    let mut h = vec!["company", "currency", "conversion_rate", "tax_template", "set_warehouse", "discount_percent", "project"];
    h.push(names(side).party);
    if side == Side::Selling {
        h.extend(["price_list", "po_no"]);
    }
    h
}

const LINE_FIELDS: &[&str] = &["item", "price_list_rate", "discount_percent", "warehouse", "batch_no", "serial_no"];

async fn stock_account(hc: &mut HookCtx<'_>, co: &Company, warehouse: Uuid) -> Result<Uuid> {
    let w = get(hc.kernel, hc.ctx, hc.tx, "Warehouse", warehouse).await?;
    match id(&w, "account") {
        Some(a) => Ok(a),
        None => co.require("default_inventory_account", "default inventory account"),
    }
}

async fn is_stock_item(hc: &mut HookCtx<'_>, cache: &mut HashMap<Uuid, bool>, item: Uuid) -> Result<bool> {
    if let Some(v) = cache.get(&item) {
        return Ok(*v);
    }
    let it = get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?;
    let v = b(&it, "is_stock_item");
    cache.insert(item, v);
    Ok(v)
}

/// Creates a follow-up document for the user. The links back to the source lines are read-only
/// fields, which only the engine may set, so the insert runs elevated after checking that the
/// user may create the target.
async fn create_for_user(hc: &mut HookCtx<'_>, entity: &str, input: Doc) -> Result<Doc> {
    hc.ctx.require(entity, Action::Create)?;
    hc.kernel.insert_tx(&elevated(hc.ctx), hc.tx, entity, input).await
}

/// Unit cost at which an item left on the original document (None: use the current valuation).
async fn returned_cost(hc: &mut HookCtx<'_>, entity: &str, original: Uuid, item: Uuid) -> Result<Option<Decimal>> {
    let r: Option<Decimal> = sqlx::query_scalar(
        "select sum(value) / nullif(sum(qty), 0) from stock_move where tenant_id = $1 and source_entity = $2 and source_id = $3 \
         and item_id = $4 and qty < 0 and reversal_of is null",
    )
    .bind(hc.ctx.tenant)
    .bind(entity)
    .bind(original)
    .bind(item)
    .fetch_one(&mut **hc.tx)
    .await?;
    Ok(r.map(|x| x.abs().round_dp(12)))
}

/// Posts stock moves for document lines. `direction` is +1 for receipts, -1 for issues.
async fn post_line_stock(hc: &mut HookCtx<'_>, co: &Company, entity: &str, doc: &Doc, direction: Decimal, serial_status: &str) -> Result<Vec<Posted>> {
    let doc_id = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let rate = d(doc, "conversion_rate");
    let rate = if rate.is_zero() { Decimal::ONE } else { rate };
    let sales_return = b(doc, "is_return") && matches!(entity, "DeliveryNote" | "SalesInvoice");
    let mut cache = HashMap::new();
    let mut moves = Vec::new();
    for (i, r) in rows(doc, "items").iter().enumerate() {
        let Some(item) = id(r, "item") else { continue };
        if !is_stock_item(hc, &mut cache, item).await? {
            continue;
        }
        let wh = id(r, "warehouse").ok_or_else(|| Error::Validation(vec![FieldError::new("warehouse", "is required for stock items").in_row("items", i)]))?;
        let qty = d(r, "qty") * direction;
        if qty.is_zero() {
            continue;
        }
        let incoming = if qty <= Decimal::ZERO {
            None
        } else if sales_return {
            // Goods come back at the cost they left at, never at the selling price.
            match id(doc, "return_against") {
                Some(orig) => returned_cost(hc, entity, orig, item).await?,
                None => None,
            }
        } else {
            let net = if r.get("base_net_amount").is_some_and(|v| !v.is_null()) { d(r, "base_net_amount") } else { d(r, "net_amount") * rate };
            let q = d(r, "qty").abs();
            Some(if q.is_zero() { Decimal::ZERO } else { (net.abs() / q).round_dp(12) })
        };
        moves.push(MoveIn {
            item,
            warehouse: wh,
            qty,
            rate: incoming,
            batch_no: s(r, "batch_no").map(str::to_string),
            serials: parse_serials(s(r, "serial_no")),
            source_line: id(r, "id"),
            ..Default::default()
        });
    }
    if moves.is_empty() {
        return Ok(vec![]);
    }
    let on = date(doc, "posting_date")?;
    stock::post_moves(hc.tx, hc.ctx, co, entity, doc_id, on, posting_ts(doc)?, moves, serial_status).await
}

async fn write_valuation(hc: &mut HookCtx<'_>, entity: &str, posted: &[Posted]) -> Result<()> {
    let ent = hc.kernel.entity(hc.ctx, entity)?;
    let child = ent.children.get("items").and_then(|c| hc.ctx.schema.entity(&c.entity)).cloned();
    if child.is_none_or(|c| c.field("valuation_rate").is_none()) {
        return Ok(());
    }
    for p in posted {
        if let Some(line) = p.source_line {
            let mut v = Doc::new();
            v.insert("valuation_rate".into(), money(p.rate.round_dp(9)));
            hc.kernel.set_child_values_tx(&sys(hc.ctx), hc.tx, entity, "items", line, v).await?;
        }
    }
    Ok(())
}

// ───────────────────────────── orders ─────────────────────────────

pub struct OrderHook(pub Side);

#[async_trait]
impl DocHook for OrderHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        calculate(hc.kernel, hc.ctx, hc.tx, hc.entity, doc, self.0).await?;
        if d(doc, "docstatus").is_zero() {
            flow::set_status(doc, "Draft");
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, id(doc, "id").unwrap_or_default(), n.fulfil_field, n.fulfil_pct).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        if rows(doc, "items").iter().any(|r| d(r, n.fulfil_field) > Decimal::ZERO || d(r, "billed_qty") > Decimal::ZERO) {
            return Err(Error::invalid("cancel the deliveries/receipts and invoices made against this order first"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, id(doc, "id").unwrap_or_default(), n.fulfil_field, n.fulfil_pct).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let n = names(self.0);
        let oid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        if name == "close" || name == "reopen" {
            let mut v = Doc::new();
            v.insert("status".into(), json!(if name == "close" { "Closed" } else { "To Bill" }));
            hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, n.order, oid, v).await?;
            if name == "reopen" {
                refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, oid, n.fulfil_field, n.fulfil_pct).await?;
            }
            return Ok(Some(json!({"status": if name == "close" { "Closed" } else { "reopened" }})));
        }
        let header = header_fields(self.0);
        let (target, done) = if name == n.make_shipment {
            (n.shipment, n.fulfil_field)
        } else if name == n.make_invoice {
            (n.invoice, "billed_qty")
        } else {
            return Ok(None);
        };
        let mut input =
            map_document(doc, &MapSpec { done_field: done, link_field: n.order_link, line_field: n.order_line, header: &header, line_fields: LINE_FIELDS })?;
        if target == n.invoice && self.0 == Side::Buying {
            input.insert("bill_no".into(), json!(format!("PENDING-{}", s(doc, "number").unwrap_or_default())));
        }
        let created_doc = create_for_user(hc, target, input).await?;
        Ok(Some(created(target, &created_doc)))
    }
}

// ─────────────────────────── shipments ───────────────────────────

pub struct ShipmentHook(pub Side);

#[async_trait]
impl DocHook for ShipmentHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        calculate(hc.kernel, hc.ctx, hc.tx, hc.entity, doc, self.0).await?;
        if d(doc, "docstatus").is_zero() {
            flow::set_status(doc, "Draft");
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            if id(r, "warehouse").is_none() {
                errors.push(FieldError::new("warehouse", "is required").in_row("items", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let sid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let ret = b(doc, "is_return");
        // Selling issues stock (returns receive it back); buying receives (returns issue).
        let direction = match (self.0, ret) {
            (Side::Selling, false) | (Side::Buying, true) => -Decimal::ONE,
            _ => Decimal::ONE,
        };
        let posted = post_line_stock(hc, &co, n.shipment, doc, direction, n.serial_status).await?;
        write_valuation(hc, n.shipment, &posted).await?;
        let on = date(doc, "posting_date")?;
        let mut entry = Entry::new(co.id, on, n.shipment, sid, s(doc, "number").map(str::to_string));
        for p in &posted {
            let inv = stock_account(hc, &co, p.warehouse).await?;
            let offset = match self.0 {
                Side::Selling => co.require("cogs_account", "cost of goods sold account")?,
                Side::Buying => co.require("srbnb_account", "stock received but not billed account")?,
            };
            entry.lines.push(Line::new(inv, p.value, &co.currency_code));
            entry.lines.push(Line::new(offset, -p.value, &co.currency_code));
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        let sign = if ret { -Decimal::ONE } else { Decimal::ONE };
        let mut orders = Vec::new();
        for r in rows(doc, "items") {
            if let (Some(o), Some(line)) = (id(&r, n.order_link), id(&r, n.order_line)) {
                add_progress(hc.kernel, hc.ctx, hc.tx, n.order, o, line, n.fulfil_field, sign * d(&r, "qty")).await?;
                if !orders.contains(&o) {
                    orders.push(o);
                }
            }
        }
        for o in orders {
            refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, o, n.fulfil_field, n.fulfil_pct).await?;
        }
        refresh_billing(hc.kernel, hc.ctx, hc.tx, n.shipment, sid).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if rows(doc, "items").iter().any(|r| d(r, "billed_qty") > Decimal::ZERO) {
            return Err(Error::invalid("cancel the invoices made against this document first"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let sid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        stock::reverse_moves(hc.tx, hc.ctx, &co, n.shipment, sid, on, ts).await?;
        ledger::reverse(hc.tx, hc.ctx, n.shipment, sid, on, co.precision).await?;
        let sign = if b(doc, "is_return") { Decimal::ONE } else { -Decimal::ONE };
        let mut orders = Vec::new();
        for r in rows(doc, "items") {
            if let (Some(o), Some(line)) = (id(&r, n.order_link), id(&r, n.order_line)) {
                add_progress(hc.kernel, hc.ctx, hc.tx, n.order, o, line, n.fulfil_field, sign * d(&r, "qty")).await?;
                if !orders.contains(&o) {
                    orders.push(o);
                }
            }
        }
        for o in orders {
            refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, o, n.fulfil_field, n.fulfil_pct).await?;
        }
        refresh_billing(hc.kernel, hc.ctx, hc.tx, n.shipment, sid).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let n = names(self.0);
        if name != n.make_invoice {
            return Ok(None);
        }
        let header = header_fields(self.0);
        let mut line_fields = LINE_FIELDS.to_vec();
        line_fields.extend([n.order_link, n.order_line]);
        let mut input = map_document(
            doc,
            &MapSpec { done_field: "billed_qty", link_field: n.shipment_link, line_field: n.shipment_line, header: &header, line_fields: &line_fields },
        )?;
        if self.0 == Side::Buying {
            input.insert("bill_no".into(), json!(format!("PENDING-{}", s(doc, "number").unwrap_or_default())));
        }
        let created_doc = create_for_user(hc, n.invoice, input).await?;
        Ok(Some(created(n.invoice, &created_doc)))
    }
}

// ─────────────────────────── invoices ───────────────────────────

pub struct InvoiceHook(pub Side);

impl InvoiceHook {
    fn party_account_field(&self) -> &'static str {
        match self.0 {
            Side::Selling => "debit_to",
            Side::Buying => "credit_to",
        }
    }
}

#[async_trait]
impl DocHook for InvoiceHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let n = names(self.0);
        let co = calculate(hc.kernel, hc.ctx, hc.tx, hc.entity, doc, self.0).await?;
        if let Some(pid) = id(doc, n.party) {
            let p = get(hc.kernel, hc.ctx, hc.tx, n.party_entity, pid).await?;
            let acc = match self.0 {
                Side::Selling => id(&p, "receivable_account").or(co.account("default_receivable_account")),
                Side::Buying => id(&p, "payable_account").or(co.account("default_payable_account")),
            };
            set(doc, self.party_account_field(), acc.map(|a| json!(a.to_string())).unwrap_or(Value::Null));
            if s(doc, "due_date").is_none() {
                let days = p.get("payment_terms_days").and_then(Value::as_i64).unwrap_or(0);
                let due = date(doc, "posting_date")? + chrono::Duration::days(days);
                set(doc, "due_date", due.to_string());
            }
        }
        if d(doc, "docstatus").is_zero() {
            flow::set_status(doc, "Draft");
            set(doc, "outstanding_amount", money(d(doc, "rounded_total")));
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let ret = b(doc, "is_return");
        let mut errors = Vec::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            let q = d(r, "qty");
            if q.is_zero() || (ret && q > Decimal::ZERO) || (!ret && q < Decimal::ZERO) {
                errors.push(FieldError::new("qty", if ret { "returns need negative quantities" } else { "quantity must be positive" }).in_row("items", i));
            }
            if b(doc, "update_stock") && id(r, "warehouse").is_none() {
                errors.push(FieldError::new("warehouse", "is required when updating stock").in_row("items", i));
            }
        }
        if id(doc, self.party_account_field()).is_none() {
            errors.push(FieldError::new(self.party_account_field(), "no receivable/payable account found; set the company defaults"));
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        if self.0 == Side::Buying
            && let (Some(supplier), Some(bill)) = (id(doc, "supplier"), s(doc, "bill_no"))
        {
            let dupes = hc
                .kernel
                .find_tx(
                    &sys(hc.ctx),
                    hc.tx,
                    n.invoice,
                    &[
                        Filter::eq("supplier", supplier.to_string()),
                        Filter::eq("bill_no", bill),
                        Filter { field: "docstatus".into(), op: ferrum_kernel::Op::Ne, value: json!(2) },
                    ],
                    None,
                    2,
                )
                .await?;
            if dupes.iter().any(|x| id(x, "id") != id(doc, "id")) {
                return Err(Error::field("bill_no", "this supplier invoice number was already entered"));
            }
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if self.0 != Side::Selling || b(doc, "is_return") {
            return Ok(());
        }
        let Some(cust) = id(doc, "customer") else { return Ok(()) };
        let c = get(hc.kernel, hc.ctx, hc.tx, "Customer", cust).await?;
        let limit = d(&c, "credit_limit");
        if limit <= Decimal::ZERO {
            return Ok(());
        }
        let exposure: Decimal = sqlx::query_scalar(
            "select coalesce(sum(base_outstanding), 0) from open_item where tenant_id = $1 and customer_id = $2 and company_id = $3 and status = 'open'",
        )
        .bind(hc.ctx.tenant)
        .bind(cust)
        .bind(id(doc, "company"))
        .fetch_one(&mut **hc.tx)
        .await?;
        let this = d(doc, "base_rounded_total");
        if exposure + this > limit {
            return Err(Error::invalid(format!(
                "credit limit exceeded for {}: outstanding {} + this invoice {} > limit {}",
                s(&c, "customer_name").unwrap_or_default(),
                exposure.normalize(),
                this.normalize(),
                limit.normalize()
            )));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let number = s(doc, "number").map(str::to_string);
        let rate = d(doc, "conversion_rate");
        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(doc, "currency").unwrap_or(co.currency_id)).await?;
        let party_id = id(doc, n.party);
        let party_account = id(doc, self.party_account_field()).ok_or_else(|| Error::field(self.party_account_field(), "is required"))?;
        // Debit-side sign: selling debits the customer, buying credits the supplier.
        let sign = match self.0 {
            Side::Selling => Decimal::ONE,
            Side::Buying => -Decimal::ONE,
        };
        let total_txn = d(doc, "rounded_total");
        let total_base = d(doc, "base_rounded_total");
        let mut entry = Entry::new(co.id, on, n.invoice, iid, number.clone());
        entry.memo = s(doc, "remarks").map(str::to_string);
        let mut party_line = Line::new(party_account, sign * total_base, &co.currency_code).txn(sign * total_txn, &code, rate).against(n.invoice, iid);
        party_line = match self.0 {
            Side::Selling => party_line.customer(party_id),
            Side::Buying => party_line.supplier(party_id),
        };
        entry.lines.push(party_line);
        let bk = breakup(doc);
        let update_stock = b(doc, "update_stock");
        let mut stock_cache = HashMap::new();
        for (r, base) in &bk.lines {
            let account_field = if self.0 == Side::Selling { "income_account" } else { "expense_account" };
            let mut account = id(r, account_field);
            let mut amount = *base;
            if self.0 == Side::Buying
                && let Some(item) = id(r, "item")
            {
                let stock = is_stock_item(hc, &mut stock_cache, item).await?;
                if stock && update_stock {
                    // Valued by the stock moves below; nothing to book here.
                    continue;
                }
                if stock {
                    if let (Some(pr), Some(pr_line)) = (id(r, "purchase_receipt"), id(r, "pr_line")) {
                        // Clear SRBNB at the receipt's value; the difference is a price variance.
                        let received: Option<(Decimal, Decimal)> = sqlx::query_as(
                            "select coalesce(sum(value), 0), coalesce(sum(qty), 0) from stock_move where tenant_id = $1 and source_entity = 'PurchaseReceipt' \
                                 and source_id = $2 and source_line_id = $3 and reversal_of is null",
                        )
                        .bind(hc.ctx.tenant)
                        .bind(pr)
                        .bind(pr_line)
                        .fetch_optional(&mut **hc.tx)
                        .await?;
                        if let Some((v, q)) = received.filter(|(_, q)| !q.is_zero()) {
                            let srbnb_amount = round(v / q * d(r, "qty"), co.precision);
                            let srbnb = co.require("srbnb_account", "stock received but not billed account")?;
                            entry.lines.push(Line::new(srbnb, srbnb_amount, &co.currency_code).remarks("SRBNB clearing"));
                            let variance = *base - srbnb_amount;
                            if !variance.is_zero() {
                                let adj = co.require("stock_adjustment_account", "stock adjustment account")?;
                                entry.lines.push(Line::new(adj, variance, &co.currency_code).remarks("Purchase price variance"));
                            }
                            continue;
                        }
                    }
                    account = account.or(co.account("srbnb_account"));
                }
            }
            let account = match account {
                Some(a) => a,
                None => match self.0 {
                    Side::Selling => co.require("default_income_account", "default income account")?,
                    Side::Buying => co.require("default_expense_account", "default expense account")?,
                },
            };
            amount *= -sign;
            entry.lines.push(Line::new(account, amount, &co.currency_code).cost_center(id(r, "cost_center")).project(id(doc, "project")));
        }
        for (t, base) in &bk.taxes {
            let acc = id(t, "account").ok_or_else(|| Error::field("taxes", "a tax row has no account"))?;
            entry.lines.push(Line::new(acc, -sign * *base, &co.currency_code).cost_center(id(t, "cost_center")).remarks(s(t, "description").unwrap_or("Tax")));
        }
        let adj = total_base - d(doc, "base_grand_total");
        if !adj.is_zero() {
            let ro = co.require("round_off_account", "round off account")?;
            entry.lines.push(Line::new(ro, -sign * adj, &co.currency_code).remarks("Rounding"));
        }
        if update_stock {
            let direction = match (self.0, b(doc, "is_return")) {
                (Side::Selling, _) => -Decimal::ONE,
                (Side::Buying, _) => Decimal::ONE,
            };
            let posted = post_line_stock(hc, &co, n.invoice, doc, direction, n.serial_status).await?;
            write_valuation(hc, n.invoice, &posted).await?;
            for p in &posted {
                let inv = stock_account(hc, &co, p.warehouse).await?;
                entry.lines.push(Line::new(inv, p.value, &co.currency_code));
                match self.0 {
                    Side::Selling => entry.lines.push(Line::new(co.require("cogs_account", "cost of goods sold account")?, -p.value, &co.currency_code)),
                    Side::Buying => {
                        // The supplier credit already carries the line value; the stock debit replaces the expense debit.
                        let line_base = bk.lines.iter().find(|(r, _)| id(r, "id") == p.source_line).map(|(_, b)| *b).unwrap_or(p.value);
                        let diff = line_base - p.value;
                        if !diff.is_zero() {
                            entry.lines.push(Line::new(co.require("stock_adjustment_account", "stock adjustment account")?, diff, &co.currency_code));
                        }
                    }
                }
            }
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;

        // Open item (negative for credit/debit notes) and settlement of the original by a return.
        let due = s(doc, "due_date").and_then(|x| ferrum_kernel::value::parse_date(x).ok());
        let (customer, supplier) = match self.0 {
            Side::Selling => (party_id, None),
            Side::Buying => (None, party_id),
        };
        ledger::open_item(
            hc.tx,
            hc.ctx,
            &OpenItem {
                id: iid,
                company: co.id,
                entity: n.invoice.into(),
                number,
                customer,
                supplier,
                account: party_account,
                posting_date: on,
                due_date: due,
                currency: code,
                exchange_rate: rate,
                amount: total_txn,
                base_amount: total_base,
            },
        )
        .await?;
        if b(doc, "is_return")
            && let Some(orig) = id(doc, "return_against")
        {
            let item = ledger::load_open_item(hc.tx, hc.ctx.tenant, orig, true)
                .await?
                .ok_or_else(|| Error::field("return_against", "original invoice is not open"))?;
            let settle = (-total_txn).min(item.outstanding);
            if settle > Decimal::ZERO {
                let base = round(settle * item.exchange_rate, co.precision);
                ledger::allocate(hc.tx, hc.ctx, co.id, orig, n.invoice, iid, on, settle, base).await?;
                ledger::allocate(hc.tx, hc.ctx, co.id, iid, n.invoice, iid, on, -settle, -round(settle * rate, co.precision)).await?;
                sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, n.invoice, orig).await?;
            }
        }
        sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, n.invoice, iid).await?;

        // Progress on orders and shipments.
        let mut orders = Vec::new();
        let mut shipments = Vec::new();
        for r in rows(doc, "items") {
            let q = d(&r, "qty");
            if let (Some(o), Some(line)) = (id(&r, n.order_link), id(&r, n.order_line)) {
                add_progress(hc.kernel, hc.ctx, hc.tx, n.order, o, line, "billed_qty", q).await?;
                if update_stock {
                    add_progress(hc.kernel, hc.ctx, hc.tx, n.order, o, line, n.fulfil_field, q).await?;
                }
                if !orders.contains(&o) {
                    orders.push(o);
                }
            }
            if let (Some(sh), Some(line)) = (id(&r, n.shipment_link), id(&r, n.shipment_line)) {
                add_progress(hc.kernel, hc.ctx, hc.tx, n.shipment, sh, line, "billed_qty", q).await?;
                if !shipments.contains(&sh) {
                    shipments.push(sh);
                }
            }
        }
        for o in orders {
            refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, o, n.fulfil_field, n.fulfil_pct).await?;
        }
        for sh in shipments {
            refresh_billing(hc.kernel, hc.ctx, hc.tx, n.shipment, sh).await?;
        }
        if self.0 == Side::Buying && !b(doc, "is_return") {
            for (r, base) in &bk.lines {
                let (Some(item), q) = (id(r, "item"), d(r, "qty")) else { continue };
                if q > Decimal::ZERO {
                    let mut v = Doc::new();
                    v.insert("last_purchase_rate".into(), money((*base / q).round_dp(9)));
                    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "Item", item, v).await?;
                }
            }
        }
        Ok(())
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let payments: i64 = sqlx::query_scalar(
            "select count(*) from allocation a where a.tenant_id = $1 and a.open_item_id = $2 and a.payment_entity = 'PaymentEntry' \
             and a.reversal_of is null and not exists (select 1 from allocation r where r.tenant_id = a.tenant_id and r.reversal_of = a.id)",
        )
        .bind(hc.ctx.tenant)
        .bind(iid)
        .fetch_one(&mut **hc.tx)
        .await?;
        if payments > 0 {
            return Err(Error::invalid("payments are allocated to this invoice; cancel those payments first"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        let touched = ledger::unallocate(hc.tx, hc.ctx, n.invoice, iid, on).await?;
        ledger::cancel_open_item(hc.tx, hc.ctx, iid).await?;
        if b(doc, "update_stock") {
            stock::reverse_moves(hc.tx, hc.ctx, &co, n.invoice, iid, on, ts).await?;
        }
        ledger::reverse(hc.tx, hc.ctx, n.invoice, iid, on, co.precision).await?;
        for t in touched {
            if t != iid {
                sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, n.invoice, t).await?;
            }
        }
        sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, n.invoice, iid).await?;
        let mut orders = Vec::new();
        let mut shipments = Vec::new();
        for r in rows(doc, "items") {
            let q = -d(&r, "qty");
            if let (Some(o), Some(line)) = (id(&r, n.order_link), id(&r, n.order_line)) {
                add_progress(hc.kernel, hc.ctx, hc.tx, n.order, o, line, "billed_qty", q).await?;
                if b(doc, "update_stock") {
                    add_progress(hc.kernel, hc.ctx, hc.tx, n.order, o, line, n.fulfil_field, q).await?;
                }
                if !orders.contains(&o) {
                    orders.push(o);
                }
            }
            if let (Some(sh), Some(line)) = (id(&r, n.shipment_link), id(&r, n.shipment_line)) {
                add_progress(hc.kernel, hc.ctx, hc.tx, n.shipment, sh, line, "billed_qty", q).await?;
                if !shipments.contains(&sh) {
                    shipments.push(sh);
                }
            }
        }
        for o in orders {
            refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, o, n.fulfil_field, n.fulfil_pct).await?;
        }
        for sh in shipments {
            refresh_billing(hc.kernel, hc.ctx, hc.tx, n.shipment, sh).await?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let n = names(self.0);
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        match name {
            "make_payment" => {
                let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
                let outstanding = d(doc, "outstanding_amount");
                let kind = if self.0 == Side::Selling { "Sales Invoice" } else { "Purchase Invoice" };
                let mut input = Doc::new();
                input.insert("company".into(), json!(co.id.to_string()));
                input.insert("payment_type".into(), json!(if self.0 == Side::Selling { "Receive" } else { "Pay" }));
                input.insert(n.party.into(), doc.get(n.party).cloned().unwrap_or(Value::Null));
                input.insert("currency".into(), doc.get("currency").cloned().unwrap_or(Value::Null));
                input.insert("paid_amount".into(), money(outstanding));
                if let Some(bank) = co.account("default_bank_account") {
                    input.insert("bank_account".into(), json!(bank.to_string()));
                }
                input.insert("references".into(), json!([{"invoice_type": kind, "invoice": iid.to_string(), "allocated_amount": outstanding.to_string()}]));
                let p = create_for_user(hc, "PaymentEntry", input).await?;
                Ok(Some(created("PaymentEntry", &p)))
            }
            "make_credit_note" => {
                let mut input = map_document(
                    doc,
                    &MapSpec {
                        done_field: "__none__",
                        link_field: n.order_link,
                        line_field: n.order_line,
                        header: &header_fields(self.0),
                        line_fields: LINE_FIELDS,
                    },
                )?;
                if let Some(Value::Array(lines)) = input.get_mut("items") {
                    for l in lines.iter_mut() {
                        if let Value::Object(m) = l {
                            let q = d(m, "qty");
                            m.insert("qty".into(), money(-q));
                            m.remove(n.order_link);
                            m.remove(n.order_line);
                        }
                    }
                }
                input.insert("is_return".into(), json!(true));
                input.insert("return_against".into(), json!(iid.to_string()));
                let c = create_for_user(hc, n.invoice, input).await?;
                Ok(Some(created(n.invoice, &c)))
            }
            _ => Ok(None),
        }
    }
}
