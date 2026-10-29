//! Supplier quotations: a supplier's priced offer (by the buying engine, taxes included), valid
//! until `valid_till`. Purchase orders made from it count against its lines, so a quotation is
//! never ordered twice; once expired (checked on every order, and marked by upkeep) it refuses
//! new orders. Submitting or cancelling one answers or reopens the supplier's RFQ invitation.

use crate::material_request::days_after;
use crate::settings;
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::flow::{MapSpec, add_progress, created, map_document};
use ferrum_domain::pricing::{Side, calculate};
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{d, date, get, id, money, rows, s, set, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

fn valid_till(doc: &Doc) -> Option<NaiveDate> {
    s(doc, "valid_till").and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok())
}

fn status_values(doc: &Doc, today: NaiveDate) -> Doc {
    let lines = rows(doc, "items");
    let total: Decimal = lines.iter().map(|l| d(l, "qty")).sum();
    let ordered: Decimal = lines.iter().map(|l| d(l, "ordered_qty").min(d(l, "qty"))).sum();
    let per = if total.is_zero() { Decimal::ZERO } else { (ordered / total * Decimal::ONE_HUNDRED).round_dp(2).min(Decimal::ONE_HUNDRED) };
    let status = match d(doc, "docstatus") {
        x if x.is_zero() => "Draft",
        x if x == Decimal::TWO => "Cancelled",
        _ if per >= Decimal::ONE_HUNDRED => "Ordered",
        _ if valid_till(doc).is_some_and(|v| v < today) => "Expired",
        _ => "Submitted",
    };
    let mut v = Doc::new();
    v.insert("per_ordered".into(), money(per));
    v.insert("status".into(), json!(status));
    v
}

async fn refresh(hc: &mut HookCtx<'_>, quotation: Uuid) -> Result<()> {
    let sctx = sys(hc.ctx);
    let doc = hc.kernel.get_tx(&sctx, hc.tx, "SupplierQuotation", quotation, false).await?;
    let values = status_values(&doc, hc.ctx.today());
    hc.kernel.set_values_tx(&sctx, hc.tx, "SupplierQuotation", quotation, values).await
}

/// Points the supplier's row on the quotation's RFQ at this quotation (`answered`), or — when it
/// is cancelled or deleted — at another submitted quotation of theirs, else back to Pending.
async fn sync_rfq(hc: &mut HookCtx<'_>, doc: &Doc, answered: bool) -> Result<()> {
    let (Some(rfq), Some(supplier), Some(me)) = (id(doc, "request_for_quotation"), id(doc, "supplier"), id(doc, "id")) else { return Ok(()) };
    let sctx = sys(hc.ctx);
    let request = hc.kernel.get_tx(&sctx, hc.tx, "RequestForQuotation", rfq, false).await?;
    let Some(row) = rows(&request, "suppliers").into_iter().find(|r| id(r, "supplier") == Some(supplier)) else { return Ok(()) };
    let row_id = id(&row, "id").ok_or_else(|| Error::internal("supplier row without id"))?;
    let mut v = Doc::new();
    if answered {
        v.insert("quote_status".into(), json!("Received"));
        v.insert("supplier_quotation".into(), json!(me.to_string()));
    } else {
        if id(&row, "supplier_quotation") != Some(me) {
            return Ok(());
        }
        let other: Option<Uuid> = sqlx::query_scalar(
            "select id from e_supplier_quotation where tenant_id = $1 and request_for_quotation = $2 and supplier = $3 and docstatus = 1 and id <> $4 \
             order by posting_date desc, created_at desc limit 1",
        )
        .bind(hc.ctx.tenant)
        .bind(rfq)
        .bind(supplier)
        .bind(me)
        .fetch_optional(&mut **hc.tx)
        .await?;
        v.insert("quote_status".into(), json!(if other.is_some() { "Received" } else { "Pending" }));
        v.insert("supplier_quotation".into(), other.map(|o| json!(o.to_string())).unwrap_or(Value::Null));
    }
    hc.kernel.set_child_values_tx(&sctx, hc.tx, "RequestForQuotation", "suppliers", row_id, v).await
}

pub struct QuotationHook;

#[async_trait]
impl DocHook for QuotationHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if s(doc, "valid_till").is_none()
            && let Ok(day) = date(doc, "posting_date")
        {
            let days = settings(hc.tx, hc.ctx.tenant).await?.validity_days;
            set(doc, "valid_till", days_after(day, days));
        }
        calculate(hc.kernel, hc.ctx, hc.tx, hc.entity, doc, Side::Buying).await?;
        if d(doc, "docstatus").is_zero() {
            set(doc, "status", "Draft");
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let header = id(doc, "request_for_quotation");
        let mut errors = Vec::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            if let Some(rfq) = id(r, "request_for_quotation")
                && Some(rfq) != header
            {
                errors.push(FieldError::new("request_for_quotation", "lines must answer the quotation's own request for quotation").in_row("items", i));
            }
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        let Some(rfq) = header else { return Ok(()) };
        let request = get(hc.kernel, hc.ctx, hc.tx, "RequestForQuotation", rfq).await?;
        let number = s(&request, "number").unwrap_or_default();
        if d(&request, "docstatus") != Decimal::ONE {
            return Err(Error::field("request_for_quotation", format!("Request for Quotation {number} is not submitted")));
        }
        if !rows(&request, "suppliers").iter().any(|r| id(r, "supplier") == id(doc, "supplier")) {
            return Err(Error::field("supplier", format!("this supplier was not invited by Request for Quotation {number}")));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        refresh(hc, id(doc, "id").ok_or_else(|| Error::internal("no id"))?).await?;
        sync_rfq(hc, doc, true).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if rows(doc, "items").iter().any(|r| d(r, "ordered_qty") > Decimal::ZERO) {
            return Err(Error::invalid("cancel the purchase orders made from this quotation first"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        refresh(hc, id(doc, "id").ok_or_else(|| Error::internal("no id"))?).await?;
        sync_rfq(hc, doc, false).await
    }

    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        sync_rfq(hc, doc, false).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "make_purchase_order" {
            return Ok(None);
        }
        if let Some(v) = valid_till(doc).filter(|v| *v < hc.ctx.today()) {
            return Err(Error::invalid(format!("this quotation expired on {v}; ask the supplier for a new one")));
        }
        if s(doc, "status") != Some("Submitted") {
            return Err(Error::invalid("only submitted quotations that are not fully ordered can be ordered"));
        }
        let header = ["company", "supplier", "currency", "conversion_rate", "tax_template", "discount_percent"];
        let spec = MapSpec {
            done_field: "ordered_qty",
            link_field: "proc_supplier_quotation",
            line_field: "proc_sq_line",
            header: &header,
            line_fields: &["item", "price_list_rate", "discount_percent", "warehouse", "schedule_date"],
        };
        let mut input = map_document(doc, &spec)?;
        // Order lines also fulfil the material request lines the quotation was asked for.
        let source: HashMap<String, Doc> = rows(doc, "items").into_iter().filter_map(|l| Some((s(&l, "id")?.to_string(), l))).collect();
        if let Some(Value::Array(lines)) = input.get_mut("items") {
            for l in lines.iter_mut().filter_map(Value::as_object_mut) {
                let src = l.get("proc_sq_line").and_then(Value::as_str).and_then(|x| source.get(x));
                if let Some(src) = src.filter(|x| id(x, "mr_line").is_some()) {
                    l.insert("proc_material_request".into(), src.get("material_request").cloned().unwrap_or(Value::Null));
                    l.insert("proc_mr_line".into(), src.get("mr_line").cloned().unwrap_or(Value::Null));
                }
            }
        }
        let po = create_for_user(hc, "PurchaseOrder", input).await?;
        Ok(Some(created("PurchaseOrder", &po)))
    }
}

/// Purchase order lines made from quotations: the quotation must be submitted, unexpired, from
/// the order's supplier, and have the quantity left.
pub(crate) async fn check_order(hc: &mut HookCtx<'_>, po: &Doc) -> Result<()> {
    let today = hc.ctx.today();
    let mut by_quote: BTreeMap<Uuid, Vec<(usize, Doc)>> = BTreeMap::new();
    for (i, r) in rows(po, "items").into_iter().enumerate() {
        if let Some(q) = id(&r, "proc_supplier_quotation") {
            by_quote.entry(q).or_default().push((i, r));
        }
    }
    for (q, lines) in by_quote {
        let quote = get(hc.kernel, hc.ctx, hc.tx, "SupplierQuotation", q).await?;
        let number = s(&quote, "number").unwrap_or_default().to_string();
        if d(&quote, "docstatus") != Decimal::ONE {
            return Err(Error::invalid(format!("Supplier Quotation {number} is not submitted")));
        }
        if let Some(v) = valid_till(&quote).filter(|v| *v < today) {
            return Err(Error::invalid(format!("Supplier Quotation {number} expired on {v}; ask the supplier for a new quotation")));
        }
        if id(&quote, "supplier") != id(po, "supplier") {
            return Err(Error::field("supplier", format!("Supplier Quotation {number} is from another supplier")));
        }
        let mut open: HashMap<Uuid, (Decimal, Doc)> =
            rows(&quote, "items").into_iter().filter_map(|l| Some((id(&l, "id")?, (d(&l, "qty") - d(&l, "ordered_qty"), l)))).collect();
        for (row, r) in lines {
            let err = |m: String| Error::Validation(vec![FieldError::new("qty", m).in_row("items", row)]);
            let Some((left, line)) = id(&r, "proc_sq_line").and_then(|l| open.get_mut(&l)) else {
                return Err(err(format!("the line of Supplier Quotation {number} this row orders no longer exists")));
            };
            if id(line, "item") != id(&r, "item") {
                return Err(err(format!("the item differs from the one on Supplier Quotation {number}")));
            }
            let qty = d(&r, "qty");
            if qty > *left {
                return Err(err(format!(
                    "{}: only {} of Supplier Quotation {number} is left to order",
                    s(line, "item_name").unwrap_or("item"),
                    (*left).max(Decimal::ZERO).normalize()
                )));
            }
            *left -= qty;
        }
    }
    Ok(())
}

/// Moves the ordered quantity of the quotations a purchase order draws on (`sign` -1 on cancel).
pub(crate) async fn order_progress(hc: &mut HookCtx<'_>, po: &Doc, sign: Decimal) -> Result<()> {
    let mut by_quote: BTreeMap<Uuid, Vec<(Uuid, Decimal)>> = BTreeMap::new();
    for r in rows(po, "items") {
        if let (Some(q), Some(line)) = (id(&r, "proc_supplier_quotation"), id(&r, "proc_sq_line")) {
            by_quote.entry(q).or_default().push((line, d(&r, "qty")));
        }
    }
    for (q, lines) in by_quote {
        for (line, qty) in lines {
            add_progress(hc.kernel, hc.ctx, hc.tx, "SupplierQuotation", q, line, "ordered_qty", sign * qty).await?;
        }
        refresh(hc, q).await?;
    }
    Ok(())
}

/// Upkeep: submitted quotations past their validity become Expired.
pub(crate) async fn expire(k: &Kernel, ctx: &Ctx) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let due: Vec<Uuid> = sqlx::query_scalar(
        "select id from e_supplier_quotation where tenant_id = $1 and docstatus = 1 and status = 'Submitted' and valid_till < $2 \
         order by valid_till limit 500",
    )
    .bind(ctx.tenant)
    .bind(ctx.today())
    .fetch_all(&mut *tx)
    .await?;
    for q in due {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Expired"));
        k.set_values_tx(ctx, &mut tx, "SupplierQuotation", q, v).await?;
    }
    tx.commit().await?;
    Ok(())
}
