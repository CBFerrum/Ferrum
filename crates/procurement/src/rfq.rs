//! Requests for quotation and the supplier page.
//!
//! `send` signs one link per invited supplier — `{rfq}.{supplier row}.{expiry}.{HMAC-SHA256}`
//! under a per-workspace key kept as a sealed secret — and queues one email each through the
//! outbox, so mail leaves after the transaction commits (never inside it) and a failed delivery
//! retries without mailing twice. The link opens `/api/site/{workspace}/procurement/rfq/{token}`,
//! where the supplier enters rates and lead times or declines; their answer becomes a draft
//! Supplier Quotation for the buyer to review. Tampered, expired or withdrawn links are refused.

use crate::material_request::{days_after, uuid_list, uuid_param};
use crate::{PACKAGE, installed, scorecard, settings};
use async_trait::async_trait;
use chrono::{NaiveDate, TimeZone, Utc};
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{d, date, get, id, rows, s, set, sys};
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::value::{parse_date, parse_datetime, parse_decimal};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, EventSink, FieldError, Filter, HookCtx, Kernel, Op, Result, Site, SiteRequest, SiteResponse, Tx, with_retry};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use uuid::Uuid;

/// Outbox topic of one supplier email.
const TOPIC: &str = "procurement.rfq_email";

// ───────────────────────────── signed links ─────────────────────────────

type HmacSha256 = Hmac<Sha256>;

fn mac(key: &[u8], tenant: Uuid, body: &str) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes keys of any length");
    m.update(b"ferrum-procurement-rfq-v1|");
    m.update(tenant.as_bytes());
    m.update(b"|");
    m.update(body.as_bytes());
    m
}

/// A supplier's link token for one invitation (`row`) of a request for quotation, valid until
/// `expires` (Unix seconds). Bound to the workspace, so it means nothing anywhere else.
pub fn sign_link(key: &[u8], tenant: Uuid, rfq: Uuid, row: Uuid, expires: i64) -> String {
    let body = format!("{}.{}.{expires}", rfq.simple(), row.simple());
    let tag = hex::encode(mac(key, tenant, &body).finalize().into_bytes());
    format!("{body}.{tag}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// Malformed, altered, or signed with another key.
    Invalid,
    Expired,
}

/// Checks a token's signature (in constant time) and then its expiry; returns `(rfq, row)`.
pub fn verify_link(key: &[u8], tenant: Uuid, token: &str, now: i64) -> std::result::Result<(Uuid, Uuid), LinkError> {
    let (body, tag) = token.rsplit_once('.').ok_or(LinkError::Invalid)?;
    let tag = hex::decode(tag).map_err(|_| LinkError::Invalid)?;
    mac(key, tenant, body).verify_slice(&tag).map_err(|_| LinkError::Invalid)?;
    let mut parts = body.split('.');
    let (Some(rfq), Some(row), Some(exp), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Err(LinkError::Invalid) };
    let rfq = Uuid::parse_str(rfq).map_err(|_| LinkError::Invalid)?;
    let row = Uuid::parse_str(row).map_err(|_| LinkError::Invalid)?;
    let exp: i64 = exp.parse().map_err(|_| LinkError::Invalid)?;
    if exp <= now {
        return Err(LinkError::Expired);
    }
    Ok((rfq, row))
}

/// The workspace's link-signing key: a secret field of Procurement Settings, made on first use
/// when `create` is set (sealing it needs the server's secret key, `FERRUM_SECRET_KEY`).
pub async fn signing_key(k: &Kernel, ctx: &Ctx, tx: &mut Tx, create: bool) -> Result<Option<Vec<u8>>> {
    let tenant = ctx.tenant;
    // Creating locks the settings, so two first sends cannot each make a key.
    let sql = if create {
        "select id from e_procurement_settings where tenant_id = $1 limit 1 for update"
    } else {
        "select id from e_procurement_settings where tenant_id = $1 limit 1"
    };
    let Some(settings_id) = sqlx::query_scalar::<_, Uuid>(sql).bind(tenant).fetch_optional(&mut **tx).await? else {
        return if create { Err(Error::invalid("save the Procurement Settings first")) } else { Ok(None) };
    };
    if let Some(key) = k.secret(tx, tenant, "ProcurementSettings", settings_id, "signing_key").await? {
        return hex::decode(key).map(Some).map_err(Error::internal);
    }
    if !create {
        return Ok(None);
    }
    // 244 random bits from two v4 UUIDs (the OS generator), as a 32-byte key.
    let fresh = [Uuid::new_v4().into_bytes(), Uuid::new_v4().into_bytes()].concat();
    let mut values = Doc::new();
    values.insert("signing_key".into(), json!(hex::encode(&fresh)));
    k.update_tx(&sys(ctx), tx, "ProcurementSettings", settings_id, values, None).await?;
    Ok(Some(fresh))
}

/// End of the deadline day where the workspace is, as Unix seconds.
fn link_expiry(ctx: &Ctx, deadline: NaiveDate) -> i64 {
    let midnight = deadline.succ_opt().unwrap_or(deadline).and_hms_opt(0, 0, 0).unwrap_or_default();
    ctx.now_local().offset().from_local_datetime(&midnight).single().map(|t| t.timestamp()).unwrap_or_else(|| midnight.and_utc().timestamp())
}

async fn tenant_slug(tx: &mut Tx, tenant: Uuid) -> Result<String> {
    Ok(sqlx::query_scalar("select slug from tenant where id = $1").bind(tenant).fetch_one(&mut **tx).await?)
}

fn link(base: &str, slug: &str, token: &str) -> String {
    format!("{base}/api/site/{slug}/{PACKAGE}/rfq/{token}")
}

// ───────────────────────────── the request ─────────────────────────────

pub struct RfqHook;

#[async_trait]
impl DocHook for RfqHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if s(doc, "response_deadline").is_none()
            && let Ok(day) = date(doc, "transaction_date")
        {
            let days = settings(hc.tx, hc.ctx.tenant).await?.response_days;
            set(doc, "response_deadline", days_after(day, days));
        }
        if let Some(by) = doc.get("schedule_date").filter(|v| !v.is_null()).cloned() {
            let mut items = rows(doc, "items");
            for r in items.iter_mut().filter(|r| s(r, "schedule_date").is_none()) {
                r.insert("schedule_date".into(), by.clone());
            }
            set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
        }
        let mut suppliers = rows(doc, "suppliers");
        for r in suppliers.iter_mut() {
            let Some(sup) = id(r, "supplier") else { continue };
            if s(r, "email").is_none() {
                let supplier = get(hc.kernel, hc.ctx, hc.tx, "Supplier", sup).await?;
                r.insert("email".into(), supplier.get("email").cloned().unwrap_or(Value::Null));
            }
            let st = scorecard::standing(hc.kernel, hc.ctx, hc.tx, sup).await?.filter(|st| st.warn_rfqs || st.prevent_rfqs);
            r.insert("scorecard_standing".into(), st.map(|st| json!(st.label())).unwrap_or(Value::Null));
        }
        set(doc, "suppliers", Value::Array(suppliers.into_iter().map(Value::Object).collect()));
        if d(doc, "docstatus").is_zero() {
            set(doc, "status", "Draft");
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let mut seen = Vec::new();
        for (i, r) in rows(doc, "suppliers").iter().enumerate() {
            let Some(sup) = id(r, "supplier") else { continue };
            if seen.contains(&sup) {
                errors.push(FieldError::new("supplier", "is invited twice").in_row("suppliers", i));
                continue;
            }
            seen.push(sup);
            if let Some(st) = scorecard::standing(hc.kernel, hc.ctx, hc.tx, sup).await?
                && st.prevent_rfqs
            {
                let name = s(r, "supplier_name").unwrap_or("this supplier");
                errors.push(
                    FieldError::new("supplier", format!("{name} stands at {} on its supplier scorecard, which blocks new requests for quotation", st.label()))
                        .in_row("suppliers", i),
                );
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Submitted"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "RequestForQuotation", id(doc, "id").ok_or_else(|| Error::internal("no id"))?, v).await
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let quoted: i64 = sqlx::query_scalar("select count(*) from e_supplier_quotation where tenant_id = $1 and request_for_quotation = $2 and docstatus = 1")
            .bind(hc.ctx.tenant)
            .bind(id(doc, "id"))
            .fetch_one(&mut **hc.tx)
            .await?;
        if quoted > 0 {
            return Err(Error::invalid("cancel the supplier quotations submitted against this request first"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Cancelled"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "RequestForQuotation", id(doc, "id").ok_or_else(|| Error::internal("no id"))?, v).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        match name {
            "send" => send(hc, doc, params).await.map(Some),
            "make_supplier_quotation" => make_supplier_quotation(hc, doc, params).await.map(Some),
            _ => Ok(None),
        }
    }
}

/// Signs a link for each chosen invitation (by default those still Pending), queues the emails
/// when a mailer is configured, and returns the links (for sharing them another way).
async fn send(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    let tenant = hc.ctx.tenant;
    let rfq = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let base = settings(hc.tx, tenant)
        .await?
        .public_url
        .ok_or_else(|| Error::invalid("set the Public URL in Procurement Settings first: supplier links are built from it"))?;
    let deadline = date(doc, "response_deadline")?;
    if deadline < hc.ctx.today() {
        return Err(Error::invalid("the response deadline has passed; amend the request with a later one"));
    }
    let only = uuid_list(params, "suppliers")?;
    let mailer = hc.kernel.mailer.is_some();
    let sctx = sys(hc.ctx);
    let key = signing_key(hc.kernel, hc.ctx, hc.tx, true).await?.ok_or_else(|| Error::internal("no signing key"))?;
    let slug = tenant_slug(hc.tx, tenant).await?;
    let expires = link_expiry(hc.ctx, deadline);
    let expires_at = Utc.timestamp_opt(expires, 0).single().ok_or_else(|| Error::internal("link expiry out of range"))?;
    let mut links = Vec::new();
    let mut errors = Vec::new();
    for (i, r) in rows(doc, "suppliers").iter().enumerate() {
        let (Some(sup), Some(row)) = (id(r, "supplier"), id(r, "id")) else { continue };
        let chosen = match &only {
            Some(list) => list.contains(&sup),
            None => s(r, "quote_status") == Some("Pending"),
        };
        if !chosen {
            continue;
        }
        let name = s(r, "supplier_name").unwrap_or("this supplier").to_string();
        if let Some(st) = scorecard::standing(hc.kernel, hc.ctx, hc.tx, sup).await?
            && st.prevent_rfqs
        {
            errors.push(
                FieldError::new("supplier", format!("{name} stands at {} on its supplier scorecard, which blocks requests for quotation", st.label()))
                    .in_row("suppliers", i),
            );
            continue;
        }
        let email = s(r, "email").map(str::to_string);
        if mailer && email.is_none() {
            errors.push(FieldError::new("email", format!("add {name}'s email address to send them the request")).in_row("suppliers", i));
            continue;
        }
        let token = sign_link(&key, tenant, rfq, row, expires);
        let send_id = Uuid::now_v7();
        let mut v = Doc::new();
        v.insert("link_expires_at".into(), json!(expires_at.to_rfc3339()));
        v.insert("send_id".into(), json!(send_id.to_string()));
        v.insert("email_status".into(), json!(if mailer { "Queued" } else { "Not Sent" }));
        v.insert("email_error".into(), Value::Null);
        hc.kernel.set_child_values_tx(&sctx, hc.tx, "RequestForQuotation", "suppliers", row, v).await?;
        if mailer {
            outbox::enqueue(hc.tx, tenant, TOPIC, None, None, json!({"rfq": rfq, "row": row, "send_id": send_id})).await?;
        }
        links.push(json!({"supplier": sup, "supplier_name": name, "email": email, "link": link(&base, &slug, &token)}));
    }
    if !errors.is_empty() {
        return Err(Error::Validation(errors));
    }
    if links.is_empty() {
        return Err(Error::invalid("every invited supplier has answered; choose suppliers to send the request to again"));
    }
    Ok(json!({"emailed": mailer, "expires_at": expires_at.to_rfc3339(), "links": links}))
}

/// A draft quotation from one invited supplier (`supplier` param; optional with a single one),
/// for answers that arrive by mail or phone.
async fn make_supplier_quotation(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    let rfq = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let invited: Vec<Uuid> = rows(doc, "suppliers").iter().filter_map(|r| id(r, "supplier")).collect();
    let supplier = match uuid_param(params, "supplier")? {
        Some(sup) if invited.contains(&sup) => sup,
        Some(_) => return Err(Error::field("supplier", "was not invited by this request")),
        None if invited.len() == 1 => invited[0],
        None => return Err(Error::field("supplier", "choose which supplier quoted")),
    };
    let items: Vec<Value> = rows(doc, "items")
        .iter()
        .map(|l| {
            json!({"item": l.get("item"), "qty": l.get("qty"), "warehouse": l.get("warehouse"), "schedule_date": l.get("schedule_date"),
                   "request_for_quotation": rfq.to_string(), "rfq_line": l.get("id"), "material_request": l.get("material_request"), "mr_line": l.get("mr_line")})
        })
        .collect();
    let input = json!({"company": doc.get("company"), "supplier": supplier.to_string(), "request_for_quotation": rfq.to_string(), "items": items});
    let sq = create_for_user(hc, "SupplierQuotation", input.as_object().cloned().unwrap_or_default()).await?;
    Ok(created("SupplierQuotation", &sq))
}

// ───────────────────────────── mail ─────────────────────────────

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `_titles` value of a link field (set by `Kernel::add_titles`).
fn title<'a>(doc: &'a Doc, field: &str) -> &'a str {
    doc.get("_titles").and_then(|t| t.get(field)).and_then(Value::as_str).unwrap_or_default()
}

fn qty_text(doc: &Doc) -> String {
    d(doc, "qty").normalize().to_string()
}

fn mail_body(rfq: &Doc, supplier: &str, url: &str) -> String {
    let mut lines = String::new();
    for l in rows(rfq, "items") {
        lines.push_str(&format!(
            "<tr><td style=\"padding:4px 8px\">{}</td><td style=\"padding:4px 8px;text-align:right\">{} {}</td><td style=\"padding:4px 8px\">{}</td></tr>",
            esc(s(&l, "item_name").unwrap_or_default()),
            esc(&qty_text(&l)),
            esc(title(&l, "uom")),
            esc(s(&l, "schedule_date").unwrap_or("")),
        ));
    }
    format!(
        "<p>Dear {supplier},</p><p>{message}</p>\
         <table style=\"border-collapse:collapse\"><tr><th style=\"padding:4px 8px;text-align:left\">Item</th><th style=\"padding:4px 8px\">Quantity</th>\
         <th style=\"padding:4px 8px;text-align:left\">Required by</th></tr>{lines}</table>\
         <p>Please send your rates by <b>{deadline}</b> on this page:</p><p><a href=\"{url}\">{url}</a></p>\
         <p>The link is personal to you and stops working after the deadline.</p><p>{company}</p>",
        supplier = esc(supplier),
        message = esc(s(rfq, "message").unwrap_or_default()).replace('\n', "<br>"),
        deadline = esc(s(rfq, "response_deadline").unwrap_or_default()),
        url = esc(url),
        company = esc(title(rfq, "company")),
    )
}

/// Sends the supplier emails queued by `send`, after their transaction committed. A retried
/// event never mails twice once sent, and a newer `send` supersedes an older queued one.
pub struct RfqMailer;

async fn mark(k: &Kernel, ctx: &Ctx, row: Uuid, send_id: &str, status: &str, error: Option<String>) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let current: Option<Option<String>> =
        sqlx::query_scalar("select send_id from e_request_for_quotation__suppliers where tenant_id = $1 and id = $2 for update")
            .bind(ctx.tenant)
            .bind(row)
            .fetch_optional(&mut *tx)
            .await?;
    if current.flatten().as_deref() == Some(send_id) {
        let mut v = Doc::new();
        v.insert("email_status".into(), json!(status));
        v.insert("email_error".into(), error.map(|e| json!(e.chars().take(900).collect::<String>())).unwrap_or(Value::Null));
        if status == "Sent" {
            v.insert("last_sent_at".into(), json!(Utc::now().to_rfc3339()));
        }
        k.set_child_values_tx(ctx, &mut tx, "RequestForQuotation", "suppliers", row, v).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[async_trait]
impl EventSink for RfqMailer {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != TOPIC {
            return Ok(());
        }
        let uuid = |key: &str| ev.payload.get(key).and_then(Value::as_str).and_then(|v| v.parse::<Uuid>().ok());
        let (Some(rfq), Some(row), Some(send_id)) = (uuid("rfq"), uuid("row"), uuid("send_id")) else { return Ok(()) };
        let send_id = send_id.to_string();
        let schema = k.registry.schema(ev.tenant_id).await?;
        if !schema.packages.iter().any(|p| p == PACKAGE) {
            return Ok(());
        }
        let ctx = Ctx::system(ev.tenant_id, schema);
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let mut doc = match k.get_tx(&ctx, &mut tx, "RequestForQuotation", rfq, false).await {
            Ok(doc) => doc,
            Err(Error::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let Some(invite) = rows(&doc, "suppliers").into_iter().find(|r| id(r, "id") == Some(row)) else { return Ok(()) };
        let pending = s(&invite, "send_id") == Some(send_id.as_str()) && matches!(s(&invite, "email_status"), Some("Queued" | "Failed"));
        if d(&doc, "docstatus") != Decimal::ONE || !pending {
            return Ok(());
        }
        let ent = k.entity(&ctx, "RequestForQuotation")?;
        k.add_titles(&ctx, &mut tx, &ent, std::slice::from_mut(&mut doc)).await?;
        let key = signing_key(k, &ctx, &mut tx, false).await?;
        let base = settings(&mut tx, ctx.tenant).await?.public_url;
        let slug = tenant_slug(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        let expires = s(&invite, "link_expires_at").and_then(|x| parse_datetime(x).ok()).map(|t| t.timestamp());
        let (Some(mailer), Some(to), Some(key), Some(base), Some(expires)) = (k.mailer.clone(), s(&invite, "email"), key, base, expires) else {
            return mark(k, &ctx, row, &send_id, "Failed", Some("no mailer, email address, public URL or link key to send it with".into())).await;
        };
        let url = link(&base, &slug, &sign_link(&key, ctx.tenant, rfq, row, expires));
        let subject = format!("Request for Quotation {} from {}", s(&doc, "number").unwrap_or_default(), title(&doc, "company"));
        let body = mail_body(&doc, s(&invite, "supplier_name").unwrap_or("Supplier"), &url);
        match mailer.send(to, &subject, &body).await {
            Ok(()) => mark(k, &ctx, row, &send_id, "Sent", None).await,
            Err(e) => {
                mark(k, &ctx, row, &send_id, "Failed", Some(e.to_string())).await?;
                Err(e)
            }
        }
    }
}

// ───────────────────────────── supplier page ─────────────────────────────

const CSS: &str = "body{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;background:#f6f7f9;color:#1f2933;margin:0}\
main{max-width:880px;margin:2rem auto;padding:1.5rem;background:#fff;border-radius:10px;box-shadow:0 1px 3px rgba(0,0,0,.08)}\
h1{font-size:1.4rem;margin:0 0 .25rem}.muted{color:#616e7c}table{width:100%;border-collapse:collapse;margin:1rem 0}\
th,td{padding:.55rem .5rem;border-bottom:1px solid #e4e7eb;text-align:left;vertical-align:middle}th{font-size:.85rem;color:#52606d}\
input,textarea{font:inherit;padding:.4rem .5rem;border:1px solid #cbd2d9;border-radius:6px;box-sizing:border-box}input.n{width:8rem}\
textarea{width:100%}label{display:block;margin:.75rem 0 .25rem}button{font:inherit;padding:.6rem 1.2rem;border-radius:6px;border:0;\
background:#2f6fde;color:#fff;cursor:pointer;margin:1rem .5rem 0 0}button.alt{background:#e4e7eb;color:#1f2933}\
.note{background:#eef4ff;border-radius:6px;padding:.75rem 1rem}.scroll{overflow-x:auto}";

fn headers() -> Vec<(String, String)> {
    [
        ("cache-control", "no-store"),
        // The token is in the URL: never leak it to other sites.
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("content-security-policy", "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn html(status: u16, heading: &str, body: &str) -> SiteResponse {
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\"><title>{t}</title><style>{CSS}</style></head><body><main><h1>{t}</h1>{body}</main></body></html>",
        t = esc(heading)
    );
    let mut r = SiteResponse::text(status, "text/html; charset=utf-8", page);
    r.headers = headers();
    r
}

fn json_response(status: u16, v: &Value) -> SiteResponse {
    let mut r = SiteResponse::json(status, v);
    r.headers = headers();
    r
}

/// A refusal shown to the supplier: a page, or the API's error shape for JSON clients.
fn refuse(json: bool, status: u16, message: &str) -> SiteResponse {
    if json {
        let code = match status {
            403 => "forbidden",
            404 => "not_found",
            409 => "conflict",
            410 => "gone",
            _ => "invalid",
        };
        json_response(status, &json!({"error": code, "message": message}))
    } else {
        html(status, "Request for Quotation", &format!("<p class=\"note\">{}</p>", esc(message)))
    }
}

fn wants_json(req: &SiteRequest) -> bool {
    let header = |k: &str| req.headers.get(k).and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    req.query.get("format").and_then(Value::as_str) == Some("json") || header("accept").contains("application/json") || header("content-type").contains("json")
}

/// One supplier's invitation, as a valid link identifies it.
struct Invite {
    ctx: Ctx,
    rfq_id: Uuid,
    rfq: Doc,
    row: Doc,
    supplier: Uuid,
}

/// `lock` holds the request until the transaction ends (while an answer is recorded).
async fn invite(k: &Kernel, ctx: &Ctx, tx: &mut Tx, token: &str, lock: bool) -> Result<std::result::Result<Invite, (u16, &'static str)>> {
    let invalid = (403, "This link is not valid. Please use the link from the email you received.");
    let Some(key) = signing_key(k, ctx, tx, false).await? else { return Ok(Err(invalid)) };
    let (rfq_id, row_id) = match verify_link(&key, ctx.tenant, token, Utc::now().timestamp()) {
        Ok(ids) => ids,
        Err(LinkError::Invalid) => return Ok(Err(invalid)),
        Err(LinkError::Expired) => return Ok(Err((410, "This link has expired. Please ask the buyer for a new one."))),
    };
    let mut rfq = match k.get_tx(ctx, tx, "RequestForQuotation", rfq_id, lock).await {
        Ok(doc) => doc,
        Err(Error::NotFound(_)) => return Ok(Err(invalid)),
        Err(e) => return Err(e),
    };
    if d(&rfq, "docstatus") != Decimal::ONE {
        return Ok(Err((410, "This request for quotation has been withdrawn.")));
    }
    if date(&rfq, "response_deadline").is_ok_and(|day| day < ctx.today()) {
        return Ok(Err((410, "The deadline for this request for quotation has passed.")));
    }
    let Some(row) = rows(&rfq, "suppliers").into_iter().find(|r| id(r, "id") == Some(row_id)) else { return Ok(Err(invalid)) };
    let supplier = id(&row, "supplier").ok_or_else(|| Error::internal("invitation without a supplier"))?;
    let ent = k.entity(ctx, "RequestForQuotation")?;
    k.add_titles(ctx, tx, &ent, std::slice::from_mut(&mut rfq)).await?;
    Ok(Ok(Invite { ctx: ctx.clone(), rfq_id, rfq, row, supplier }))
}

/// The supplier's own answer to this request, with its lines: a submitted quotation, else the
/// draft this page made. Drafts the buyer made (which may carry internal prices) stay private.
async fn their_quotation(k: &Kernel, inv: &Invite, tx: &mut Tx) -> Result<Option<Doc>> {
    let found = k
        .find_tx(
            &inv.ctx,
            tx,
            "SupplierQuotation",
            &[
                Filter::eq("request_for_quotation", inv.rfq_id.to_string()),
                Filter::eq("supplier", inv.supplier.to_string()),
                Filter { field: "docstatus".into(), op: Op::Ne, value: json!(2) },
            ],
            Some("created_at desc"),
            50,
        )
        .await?;
    let submitted = found.iter().find(|q| d(q, "docstatus") == Decimal::ONE);
    let own_draft = || found.iter().find(|q| d(q, "docstatus").is_zero() && q.get("created_by").is_none_or(Value::is_null));
    match submitted.or_else(own_draft).and_then(|q| id(q, "id")) {
        Some(q) => Ok(Some(k.get_tx(&inv.ctx, tx, "SupplierQuotation", q, false).await?)),
        None => Ok(None),
    }
}

/// The currency the supplier quotes in: theirs, else the company's.
async fn quote_currency(k: &Kernel, inv: &Invite, tx: &mut Tx) -> Result<String> {
    let supplier = get(k, &inv.ctx, tx, "Supplier", inv.supplier).await?;
    let currency = match id(&supplier, "default_currency") {
        Some(c) => Some(c),
        None => match id(&inv.rfq, "company") {
            Some(co) => id(&get(k, &inv.ctx, tx, "Company", co).await?, "default_currency"),
            None => None,
        },
    };
    match currency {
        Some(c) => Ok(ferrum_domain::util::currency(k, &inv.ctx, tx, c).await?.0),
        None => Ok(String::new()),
    }
}

fn form_page(inv: &Invite, quote: Option<&Doc>, currency: &str, notice: Option<&str>) -> SiteResponse {
    let number = s(&inv.rfq, "number").unwrap_or_default();
    let company = title(&inv.rfq, "company");
    let deadline = s(&inv.rfq, "response_deadline").unwrap_or_default();
    let supplier = s(&inv.row, "supplier_name").unwrap_or("Supplier");
    let mut body = format!("<p class=\"muted\">{} · for {} · please answer by <b>{}</b></p>", esc(company), esc(supplier), esc(deadline));
    if let Some(n) = notice {
        body.push_str(&format!("<p class=\"note\">{}</p>", esc(n)));
    }
    if let Some(q) = quote.filter(|q| d(q, "docstatus") == Decimal::ONE) {
        body.push_str(&format!(
            "<p class=\"note\">Your quotation {} is with the buyer. Please contact them to change it.</p>",
            esc(s(q, "number").unwrap_or_default())
        ));
        return html(200, &format!("Request for Quotation {number}"), &body);
    }
    if s(&inv.row, "quote_status") == Some("No Quote") {
        body.push_str("<p class=\"note\">You told us you cannot quote. You can still send rates below.</p>");
    }
    let message = esc(s(&inv.rfq, "message").unwrap_or_default()).replace('\n', "<br>");
    body.push_str(&format!("<p>{message}</p><form method=\"post\"><div class=\"scroll\"><table><thead><tr><th>Item</th><th>Quantity</th><th>Required by</th><th>Your rate ({})</th><th>Lead time (days)</th></tr></thead><tbody>", esc(currency)));
    let quoted: HashMap<String, Doc> =
        quote.map(|q| rows(q, "items").into_iter().filter_map(|l| Some((s(&l, "rfq_line")?.to_string(), l))).collect()).unwrap_or_default();
    for l in rows(&inv.rfq, "items") {
        let line = s(&l, "id").unwrap_or_default();
        let prior = quoted.get(line);
        let rate = prior.map(|p| d(p, "price_list_rate").normalize().to_string()).filter(|r| r != "0").unwrap_or_default();
        let lead = prior.and_then(|p| p.get("lead_time_days")).and_then(Value::as_i64).map(|x| x.to_string()).unwrap_or_default();
        body.push_str(&format!(
            "<tr><td>{}</td><td>{} {}</td><td>{}</td><td><input class=\"n\" name=\"rate_{line}\" inputmode=\"decimal\" value=\"{}\" aria-label=\"Rate\"></td>\
             <td><input class=\"n\" name=\"lead_{line}\" inputmode=\"numeric\" value=\"{}\" aria-label=\"Lead time\"></td></tr>",
            esc(s(&l, "item_name").unwrap_or_default()),
            esc(&qty_text(&l)),
            esc(title(&l, "uom")),
            esc(s(&l, "schedule_date").unwrap_or("")),
            esc(&rate),
            esc(&lead),
        ));
    }
    let field = |k: &str| quote.and_then(|q| s(q, k)).map(esc).unwrap_or_default();
    body.push_str(&format!(
        "</tbody></table></div><p class=\"muted\">Leave the rate empty for items you do not supply.</p>\
         <label for=\"valid_till\">Quotation valid until</label><input type=\"date\" id=\"valid_till\" name=\"valid_till\" value=\"{}\">\
         <label for=\"supplier_reference\">Your quotation number</label><input id=\"supplier_reference\" name=\"supplier_reference\" maxlength=\"140\" value=\"{}\">\
         <label for=\"notes\">Notes</label><textarea id=\"notes\" name=\"notes\" rows=\"3\" maxlength=\"2000\">{}</textarea>\
         <button type=\"submit\" name=\"action\" value=\"quote\">Send quotation</button><button class=\"alt\" type=\"submit\" name=\"action\" value=\"decline\">We cannot quote</button></form>",
        field("valid_till"),
        field("supplier_reference"),
        field("terms"),
    ));
    html(200, &format!("Request for Quotation {number}"), &body)
}

fn invite_json(inv: &Invite, quote: Option<&Doc>, currency: &str) -> Value {
    let items: Vec<Value> = rows(&inv.rfq, "items")
        .iter()
        .map(|l| json!({"line": l.get("id"), "item": l.get("item_name"), "qty": l.get("qty"), "uom": title(l, "uom"), "schedule_date": l.get("schedule_date")}))
        .collect();
    let quotation = quote.map(|q| {
        json!({"id": q.get("id"), "number": q.get("number"), "status": q.get("status"), "valid_till": q.get("valid_till"),
               "items": rows(q, "items").iter().map(|l| json!({"line": l.get("rfq_line"), "rate": l.get("price_list_rate"), "lead_time_days": l.get("lead_time_days")})).collect::<Vec<_>>()})
    });
    json!({
        "request_for_quotation": inv.rfq.get("number"), "company": title(&inv.rfq, "company"), "supplier": inv.row.get("supplier_name"),
        "currency": currency, "response_deadline": inv.rfq.get("response_deadline"), "message": inv.rfq.get("message"),
        "quote_status": inv.row.get("quote_status"), "items": items, "quotation": quotation,
    })
}

/// What a supplier sent: rates (and lead times) per request line, or a refusal to quote.
#[derive(Debug, Default)]
struct Answer {
    decline: bool,
    valid_till: Option<String>,
    reference: Option<String>,
    notes: Option<String>,
    lines: Vec<(Uuid, String, Option<String>)>,
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_answer(req: &SiteRequest, json: bool) -> std::result::Result<Answer, String> {
    let mut a = Answer::default();
    if json {
        let v: Value = serde_json::from_slice(&req.body).map_err(|_| "the request body is not valid JSON".to_string())?;
        a.decline = v.get("decline").and_then(Value::as_bool).unwrap_or(false);
        a.valid_till = v.get("valid_till").and_then(text);
        a.reference = v.get("supplier_reference").and_then(text);
        a.notes = v.get("notes").and_then(text);
        for it in v.get("items").and_then(Value::as_array).into_iter().flatten() {
            let line = it.get("line").and_then(Value::as_str).and_then(|x| x.parse().ok()).ok_or("each item needs the line it answers")?;
            if let Some(rate) = it.get("rate").and_then(text) {
                a.lines.push((line, rate, it.get("lead_time_days").and_then(text)));
            }
        }
    } else {
        let mut leads: HashMap<Uuid, String> = HashMap::new();
        for (k, v) in form_urlencoded::parse(&req.body) {
            let v = v.trim().to_string();
            let line = |prefix: &str| k.strip_prefix(prefix).and_then(|x| x.parse::<Uuid>().ok());
            match k.as_ref() {
                "action" => a.decline = v == "decline",
                "valid_till" => a.valid_till = Some(v).filter(|x| !x.is_empty()),
                "supplier_reference" => a.reference = Some(v).filter(|x| !x.is_empty()),
                "notes" => a.notes = Some(v).filter(|x| !x.is_empty()),
                _ => {
                    if let Some(l) = line("rate_") {
                        if !v.is_empty() {
                            a.lines.push((l, v, None));
                        }
                    } else if let Some(l) = line("lead_") {
                        leads.insert(l, v);
                    } else {
                        return Err(format!("unexpected field {k}"));
                    }
                }
            }
        }
        for (l, _, lead) in a.lines.iter_mut() {
            *lead = leads.remove(l).filter(|x| !x.is_empty());
        }
    }
    if a.notes.as_ref().is_some_and(|n| n.chars().count() > 2000) {
        return Err("notes are limited to 2000 characters".into());
    }
    if a.reference.as_ref().is_some_and(|n| n.chars().count() > 140) {
        return Err("the quotation number is limited to 140 characters".into());
    }
    Ok(a)
}

/// Records the supplier's answer: their draft quotation (created, or updated while still a draft)
/// or "No Quote", and tells whoever raised the request.
async fn answer(k: &Kernel, tx: &mut Tx, inv: &Invite, a: &Answer) -> Result<std::result::Result<(String, Option<Uuid>), (u16, String)>> {
    let row = id(&inv.row, "id").ok_or_else(|| Error::internal("invitation without id"))?;
    let existing = their_quotation(k, inv, tx).await?;
    if let Some(q) = existing.as_ref().filter(|q| d(q, "docstatus") == Decimal::ONE) {
        return Ok(Err((409, format!("Your quotation {} is already with the buyer; please contact them to change it.", s(q, "number").unwrap_or_default()))));
    }
    let number = s(&inv.rfq, "number").unwrap_or_default().to_string();
    let supplier_name = s(&inv.row, "supplier_name").unwrap_or("A supplier").to_string();
    let (status, quotation, title_text) = if a.decline {
        ("No Quote", existing.as_ref().and_then(|q| id(q, "id")), format!("{supplier_name} cannot quote for {number}"))
    } else {
        let lines: HashMap<Uuid, Doc> = rows(&inv.rfq, "items").into_iter().filter_map(|l| Some((id(&l, "id")?, l))).collect();
        let today = inv.ctx.today();
        let mut items = Vec::new();
        for (line, rate, lead) in &a.lines {
            let Some(l) = lines.get(line) else { return Ok(Err((422, "an answered line is not on this request".into()))) };
            let rate = match parse_decimal(&json!(rate)) {
                Ok(r) if r > Decimal::ZERO => r,
                _ => return Ok(Err((422, format!("the rate for {} must be a positive number", s(l, "item_name").unwrap_or("an item"))))),
            };
            let lead = match lead.as_deref().map(str::parse::<i64>) {
                None => None,
                Some(Ok(days)) if (0..=3650).contains(&days) => Some(days),
                Some(_) => return Ok(Err((422, format!("the lead time for {} must be a whole number of days", s(l, "item_name").unwrap_or("an item"))))),
            };
            items.push(json!({
                "item": l.get("item"), "qty": l.get("qty"), "price_list_rate": rate.to_string(), "lead_time_days": lead,
                "warehouse": l.get("warehouse"), "schedule_date": l.get("schedule_date"), "request_for_quotation": inv.rfq_id.to_string(),
                "rfq_line": line.to_string(), "material_request": l.get("material_request"), "mr_line": l.get("mr_line"),
            }));
        }
        if items.is_empty() {
            return Ok(Err((422, "enter a rate for at least one item".into())));
        }
        let valid_till = match &a.valid_till {
            None => None,
            Some(v) => match parse_date(v) {
                Ok(day) if day >= today => Some(day.to_string()),
                _ => return Ok(Err((422, "the validity date must be today or later (YYYY-MM-DD)".into()))),
            },
        };
        let mut input = Map::new();
        input.insert("company".into(), inv.rfq.get("company").cloned().unwrap_or(Value::Null));
        input.insert("supplier".into(), json!(inv.supplier.to_string()));
        input.insert("posting_date".into(), json!(today.to_string()));
        input.insert("valid_till".into(), valid_till.map(Value::String).unwrap_or(Value::Null));
        input.insert("supplier_reference".into(), a.reference.clone().map(Value::String).unwrap_or(Value::Null));
        input.insert("terms".into(), a.notes.clone().map(Value::String).unwrap_or(Value::Null));
        input.insert("request_for_quotation".into(), json!(inv.rfq_id.to_string()));
        input.insert("items".into(), Value::Array(items));
        let saved = match existing.as_ref().and_then(|q| id(q, "id")) {
            Some(q) => k.update_tx(&inv.ctx, tx, "SupplierQuotation", q, input, None).await,
            None => k.insert_tx(&inv.ctx, tx, "SupplierQuotation", input).await,
        };
        let saved = match saved {
            Ok(doc) => doc,
            Err(Error::Validation(errs)) => {
                let msg = errs.iter().map(|e| e.message.clone()).collect::<Vec<_>>().join("; ");
                return Ok(Err((422, msg)));
            }
            Err(e) => return Err(e),
        };
        ("Received", id(&saved, "id"), format!("{supplier_name} quoted for {number}"))
    };
    let mut v = Doc::new();
    v.insert("quote_status".into(), json!(status));
    if let Some(q) = quotation {
        v.insert("supplier_quotation".into(), json!(q.to_string()));
    }
    k.set_child_values_tx(&inv.ctx, tx, "RequestForQuotation", "suppliers", row, v).await?;
    if let Some(owner) = id(&inv.rfq, "created_by") {
        let (entity, record) = match quotation.filter(|_| !a.decline) {
            Some(q) => ("SupplierQuotation", q),
            None => ("RequestForQuotation", inv.rfq_id),
        };
        ferrum_kernel::notify::send(tx, inv.ctx.tenant, owner, &title_text, None, Some(entity), Some(record)).await?;
    }
    Ok(Ok((status.to_string(), quotation)))
}

/// The public endpoints: `GET /rfq/{token}` shows the request (a page, or JSON with
/// `?format=json`), `POST /rfq/{token}` takes the answer (form fields or JSON).
pub struct SupplierPortal;

#[async_trait]
impl Site for SupplierPortal {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let json = wants_json(&req);
        let token = req.path.strip_prefix("/rfq/").filter(|t| !t.is_empty() && t.len() <= 200 && !t.contains('/'));
        let (Some(token), None) = (token, req.user.as_ref()) else {
            return Ok(refuse(json, 404, "There is nothing here."));
        };
        if !matches!(req.method.as_str(), "GET" | "POST") {
            return Ok(refuse(json, 405, "Use GET or POST."));
        }
        let schema = k.registry.schema(req.tenant).await?;
        let ctx = Ctx::system(req.tenant, schema);
        if !installed(&ctx) {
            return Ok(refuse(json, 404, "There is nothing here."));
        }
        if req.method == "GET" {
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            let inv = match invite(k, &ctx, &mut tx, token, false).await? {
                Ok(inv) => inv,
                Err((status, msg)) => return Ok(refuse(json, status, msg)),
            };
            let quote = their_quotation(k, &inv, &mut tx).await?;
            let currency = quote_currency(k, &inv, &mut tx).await?;
            tx.rollback().await?;
            return Ok(if json { json_response(200, &invite_json(&inv, quote.as_ref(), &currency)) } else { form_page(&inv, quote.as_ref(), &currency, None) });
        }
        let a = match parse_answer(&req, json) {
            Ok(a) => a,
            Err(msg) => return Ok(refuse(json, 422, &msg)),
        };
        let outcome = with_retry(|| async {
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            let inv = match invite(k, &ctx, &mut tx, token, true).await? {
                Ok(inv) => inv,
                Err((status, msg)) => return Ok(Err((status, msg.to_string()))),
            };
            match answer(k, &mut tx, &inv, &a).await? {
                Ok((status, quotation)) => {
                    let quote = their_quotation(k, &inv, &mut tx).await?;
                    let currency = quote_currency(k, &inv, &mut tx).await?;
                    tx.commit().await?;
                    Ok(Ok((inv, status, quotation, quote, currency)))
                }
                Err(refusal) => {
                    tx.rollback().await?;
                    Ok(Err(refusal))
                }
            }
        })
        .await?;
        Ok(match outcome {
            Err((status, msg)) => refuse(json, status, &msg),
            Ok((_, status, quotation, _, _)) if json => json_response(200, &json!({"quote_status": status, "supplier_quotation": quotation})),
            Ok((inv, status, _, quote, currency)) => {
                let notice = if status == "No Quote" {
                    "Thank you for letting us know. If that changes, you can still send rates below."
                } else {
                    "Thank you, your quotation has been received. You can change it here until the deadline."
                };
                form_page(&inv, quote.as_ref(), &currency, Some(notice))
            }
        })
    }

    fn max_body(&self) -> usize {
        64 * 1024
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_verify_and_refuse_tampering_and_expiry() {
        let (key, tenant, rfq, row) = ([7u8; 32], Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let token = sign_link(&key, tenant, rfq, row, 2_000);
        assert_eq!(verify_link(&key, tenant, &token, 1_999), Ok((rfq, row)));
        assert_eq!(verify_link(&key, tenant, &token, 2_000), Err(LinkError::Expired));
        assert_eq!(verify_link(&[8u8; 32], tenant, &token, 1_000), Err(LinkError::Invalid), "another key");
        assert_eq!(verify_link(&key, Uuid::now_v7(), &token, 1_000), Err(LinkError::Invalid), "another workspace");
        let longer = token.replacen(".2000.", ".9999999999.", 1);
        assert_eq!(verify_link(&key, tenant, &longer, 1_000), Err(LinkError::Invalid), "extending the expiry breaks the signature");
        let other_row = sign_link(&key, tenant, rfq, Uuid::now_v7(), 2_000);
        let swapped = format!("{}.{}", token.rsplit_once('.').unwrap().0, other_row.rsplit_once('.').unwrap().1);
        assert_eq!(verify_link(&key, tenant, &swapped, 1_000), Err(LinkError::Invalid));
        for junk in ["", "abc", "a.b.c.d", &format!("{token}0"), &token[..token.len() - 2]] {
            assert_eq!(verify_link(&key, tenant, junk, 1_000), Err(LinkError::Invalid), "{junk}");
        }
    }

    #[test]
    fn pages_escape_what_they_show() {
        assert_eq!(esc("<script>alert('x')</script> & \"q\""), "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;q&quot;");
    }

    #[test]
    fn form_answers_parse() {
        let line = Uuid::now_v7();
        let req = |body: String| SiteRequest {
            tenant: Uuid::now_v7(),
            user: None,
            method: "POST".into(),
            path: "/rfq/x".into(),
            query: Map::new(),
            headers: Map::new(),
            body: body.into_bytes(),
            client_ip: "203.0.113.1".into(),
        };
        let a = parse_answer(&req(format!("rate_{line}=12.50&lead_{line}=7&valid_till=2030-01-31&notes=Net+30%2C+FOB&action=quote")), false).unwrap();
        assert!(!a.decline);
        assert_eq!(a.lines, vec![(line, "12.50".to_string(), Some("7".to_string()))]);
        assert_eq!(a.notes.as_deref(), Some("Net 30, FOB"));
        assert!(parse_answer(&req("action=decline".into()), false).unwrap().decline);
        assert!(parse_answer(&req("evil=1".into()), false).is_err(), "unknown fields are refused");
        let j = parse_answer(&req(json!({"items": [{"line": line.to_string(), "rate": 9.5, "lead_time_days": 3}]}).to_string()), true).unwrap();
        assert_eq!(j.lines, vec![(line, "9.5".to_string(), Some("3".to_string()))]);
    }
}
