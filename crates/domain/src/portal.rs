//! ERP's page of the workspace's portal (docs/plan/apps-ui.md §9.1): a customer's own sales
//! invoices, at `/api/app/selling/portal/invoices…` for ERP's interface ("Your invoices" at
//! `/portal/invoices/`). Portal people hold no roles: their invoices are those of the customers
//! their address belongs to (the customer's own email, or one of its contacts'), read as the
//! system once that is settled. Drafts and cancelled invoices are not theirs to see.

use crate::util::{b, d, id, rows, s, sys};
use async_trait::async_trait;
use ferrum_kernel::{Ctx, Doc, Error, Filter, Kernel, ListQuery, Op, Result, Site, SiteRequest, SiteResponse, Tx};
use serde_json::{Value, json};
use uuid::Uuid;

pub struct SellingPortal;

#[async_trait]
impl Site for SellingPortal {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let user = req.user.as_ref().ok_or_else(|| Error::Unauthorized("sign in first".into()))?;
        let segs: Vec<&str> = req.path.split('/').filter(|p| !p.is_empty()).collect();
        let query = |n: &str| req.query.get(n).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
        let invoice = |raw: &str| raw.parse::<Uuid>().map_err(|_| Error::NotFound("invoice".into()));
        let out = match (req.method.as_str(), segs.as_slice()) {
            ("GET", ["portal", "invoices"]) => list(k, user, query("cursor")).await?,
            ("GET", ["portal", "invoices", inv]) => detail(k, user, invoice(inv)?).await?,
            ("GET", ["portal", "invoices", inv, "print"]) => return print(k, user, invoice(inv)?, query("format") == Some("pdf")).await,
            _ => return Err(Error::NotFound(format!("selling endpoint {} {}", req.method, req.path))),
        };
        let mut r = SiteResponse::json(200, &out);
        r.headers.push(("cache-control".into(), "no-store".into()));
        Ok(r)
    }
}

/// The customers an address belongs to: those with it as their email, and those of contacts
/// with it; never disabled ones, nor the guests a web shop records unchecked (anyone may type
/// anyone's address at a guest checkout).
pub async fn customers_of(k: &Kernel, ctx: &Ctx, tx: &mut Tx, email: &str) -> Result<Vec<Uuid>> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return Ok(vec![]);
    }
    let sctx = sys(ctx);
    let usable = |c: &Doc| !b(c, "disabled") && !b(c, "commerce_guest");
    let mut out: Vec<Uuid> = k
        .find_tx(&sctx, tx, "Customer", &[Filter::eq("email", email.clone())], Some("created_at asc"), 50)
        .await?
        .iter()
        .filter(|c| usable(c))
        .filter_map(|c| id(c, "id"))
        .collect();
    if sctx.schema.entity("Contact").is_some_and(|c| c.field("emails").is_some() && c.field("customer").is_some()) {
        for c in k.find_tx(&sctx, tx, "Contact", &[Filter::eq("emails.email", email)], Some("created_at asc"), 50).await? {
            let Some(cust) = id(&c, "customer").filter(|c| !out.contains(c)) else { continue };
            if let Ok(doc) = k.get_tx(&sctx, tx, "Customer", cust, false).await
                && usable(&doc)
            {
                out.push(cust);
            }
        }
    }
    Ok(out)
}

fn titled<'a>(doc: &'a Doc, field: &str) -> Option<&'a str> {
    doc.get("_titles").and_then(|t| t.get(field)).and_then(Value::as_str)
}

fn summary(inv: &Doc, today: &str) -> Value {
    let total = if inv.get("rounded_total").is_some_and(|v| !v.is_null()) && !d(inv, "rounded_total").is_zero() {
        d(inv, "rounded_total")
    } else {
        d(inv, "grand_total")
    };
    let outstanding = d(inv, "outstanding_amount");
    json!({
        "id": s(inv, "id"),
        "number": s(inv, "number"),
        "date": s(inv, "posting_date"),
        "due_date": s(inv, "due_date"),
        "status": s(inv, "status"),
        "is_return": b(inv, "is_return"),
        "customer": titled(inv, "customer").or(s(inv, "customer_name")),
        "currency": titled(inv, "currency"),
        "total": total.normalize().to_string(),
        "outstanding": outstanding.normalize().to_string(),
        "overdue": outstanding > rust_decimal::Decimal::ZERO && s(inv, "due_date").is_some_and(|due| due < today),
    })
}

/// `GET portal/invoices`: the caller's customers' invoices, newest first, twenty at a time.
async fn list(k: &Kernel, user: &Ctx, cursor: Option<&str>) -> Result<Value> {
    let mut tx = k.db.tenant_tx(user.tenant).await?;
    let mine = customers_of(k, user, &mut tx, &user.email).await?;
    tx.commit().await?;
    if mine.is_empty() {
        return Ok(json!({"invoices": [], "next_cursor": null}));
    }
    let q = ListQuery {
        filters: vec![
            Filter { field: "customer".into(), op: Op::In, value: json!(mine.iter().map(Uuid::to_string).collect::<Vec<_>>()) },
            Filter::eq("docstatus", 1),
        ],
        sort: Some("posting_date desc".into()),
        limit: Some(20),
        cursor: cursor.map(str::to_string),
        with_titles: true,
        ..Default::default()
    };
    let page = k.list(&sys(user), "SalesInvoice", &q).await?;
    let today = user.today().to_string();
    Ok(json!({"invoices": page.rows.iter().map(|r| summary(r, &today)).collect::<Vec<_>>(), "next_cursor": page.next_cursor}))
}

/// The invoice when it is one of the caller's customers' and submitted; anything else reads as missing.
async fn own(k: &Kernel, user: &Ctx, invoice: Uuid) -> Result<Doc> {
    let sctx = sys(user);
    let mut tx = k.db.tenant_tx(user.tenant).await?;
    let mine = customers_of(k, user, &mut tx, &user.email).await?;
    tx.commit().await?;
    let missing = || Error::NotFound("invoice".into());
    let inv = match k.get(&sctx, "SalesInvoice", invoice, true).await {
        Ok(inv) => inv,
        Err(Error::NotFound(_)) => return Err(missing()),
        Err(e) => return Err(e),
    };
    let theirs = id(&inv, "customer").is_some_and(|c| mine.contains(&c));
    if !theirs || inv.get("docstatus").and_then(Value::as_i64) != Some(1) {
        return Err(missing());
    }
    Ok(inv)
}

/// `GET portal/invoices/{id}`: an invoice with its lines and taxes.
async fn detail(k: &Kernel, user: &Ctx, invoice: Uuid) -> Result<Value> {
    let inv = own(k, user, invoice).await?;
    let mut view = summary(&inv, &user.today().to_string());
    let lines: Vec<Value> = rows(&inv, "items")
        .iter()
        .map(|l| {
            json!({
                "item": s(l, "item_name").or(titled(l, "item")),
                "description": s(l, "description"),
                "qty": d(l, "qty").normalize().to_string(),
                "unit": titled(l, "uom"),
                "rate": d(l, "rate").normalize().to_string(),
                "amount": d(l, "amount").normalize().to_string(),
            })
        })
        .collect();
    let taxes: Vec<Value> = rows(&inv, "taxes")
        .iter()
        .filter(|t| !d(t, "tax_amount").is_zero())
        .map(|t| json!({"description": s(t, "description"), "amount": (if b(t, "deduct") { -d(t, "tax_amount") } else { d(t, "tax_amount") }).normalize().to_string()}))
        .collect();
    let more = json!({
        "company": titled(&inv, "company"),
        "lines": lines,
        "taxes": taxes,
        "net_total": d(&inv, "net_total").normalize().to_string(),
        "total_taxes": d(&inv, "total_taxes").normalize().to_string(),
        "grand_total": d(&inv, "grand_total").normalize().to_string(),
        "pdf": k.pdf_enabled(),
    });
    if let (Value::Object(v), Value::Object(m)) = (&mut view, more) {
        v.extend(m);
    }
    Ok(view)
}

/// `GET portal/invoices/{id}/print[?format=pdf]`: the invoice as the workspace prints it.
async fn print(k: &Kernel, user: &Ctx, invoice: Uuid, pdf: bool) -> Result<SiteResponse> {
    let inv = own(k, user, invoice).await?;
    let sctx = sys(user);
    let name = s(&inv, "number").unwrap_or("invoice").replace(['"', '\\', '/', '\r', '\n'], "");
    let mut r = if pdf {
        let bytes = k.print_pdf(&sctx, "SalesInvoice", invoice).await?;
        let mut r = SiteResponse { status: 200, content_type: "application/pdf".into(), headers: vec![], body: bytes };
        r.headers.push(("content-disposition".into(), format!("inline; filename=\"{name}.pdf\"")));
        r
    } else {
        SiteResponse::text(200, "text/html; charset=utf-8", k.render_print(&sctx, "SalesInvoice", invoice).await?)
    };
    // A page of the workspace's own making, kept from running anything.
    r.headers.push((
        "content-security-policy".into(),
        "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data: https:; font-src 'self' data:; sandbox".into(),
    ));
    r.headers.push(("x-content-type-options".into(), "nosniff".into()));
    r.headers.push(("cache-control".into(), "private, no-store".into()));
    Ok(r)
}
