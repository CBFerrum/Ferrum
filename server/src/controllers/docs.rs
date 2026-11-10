//! Generic document API for every entity: `/api/v1/{entity}[/{id}[/...]]`.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, if_match};
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::value::canonical_json;
use ferrum_kernel::{Action, Doc, Error, ListQuery};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The most rows a workbook export holds: it is made whole in memory. CSV streams every row.
const EXPORT_MAX_ROWS: usize = 100_000;

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub filters: Option<String>,
    pub fields: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub cursor: Option<String>,
    pub search: Option<String>,
    pub titles: Option<bool>,
    pub count: Option<bool>,
    /// Export only: `csv` (default) or `xlsx`.
    pub format: Option<String>,
}

impl ListParams {
    pub fn to_query(&self) -> Result<ListQuery, ApiError> {
        let filters = match self.filters.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => parse_filters(&serde_json::from_str::<Value>(s)?)?,
            _ => vec![],
        };
        let fields = self.fields.as_ref().map(|f| f.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
        Ok(ListQuery {
            filters,
            fields,
            sort: self.sort.clone(),
            limit: self.limit,
            offset: self.offset,
            cursor: self.cursor.clone(),
            search: self.search.clone(),
            with_titles: self.titles.unwrap_or(false),
            with_count: self.count.unwrap_or(false),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ReadParams {
    pub titles: Option<bool>,
}

pub fn body_doc(v: Value) -> Result<Doc, ApiError> {
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(ApiError(Error::invalid("request body must be a JSON object"))),
    }
}

/// JSON response with `ETag: "v<version>"` so clients can send it back as `If-Match`.
pub fn doc_response(doc: Doc, status: StatusCode) -> Response {
    let etag = doc.get("version").and_then(Value::as_i64).map(|v| format!("\"v{v}\""));
    let mut resp = (status, Json(Value::Object(doc))).into_response();
    if let Some(e) = etag.and_then(|e| HeaderValue::from_str(&e).ok()) {
        resp.headers_mut().insert(header::ETAG, e);
    }
    resp
}

async fn list(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Query(p): Query<ListParams>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut q = p.to_query()?;
    // A record headed by a link is named by it, so its rows always say what it links to.
    q.with_titles |= ctx.schema.entity(&entity).is_some_and(|e| e.headed_by_link());
    Ok(Json(st.kernel.list(&ctx, &entity, &q).await?).into_response())
}

#[derive(Deserialize, Default)]
struct CreateParams {
    /// Submit in the same transaction (submittable documents).
    submit: Option<bool>,
}

async fn create(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path(entity): Path<String>,
    Query(p): Query<CreateParams>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let doc = body_doc(body)?;
    let submit = p.submit.unwrap_or(false);
    if let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
        let mut h = Sha256::new();
        h.update(entity.as_bytes());
        h.update(if submit { b"+submit".as_slice() } else { b"".as_slice() });
        h.update(canonical_json(&Value::Object(doc.clone())));
        let (d, replayed) = st.kernel.insert_idempotent_with(&ctx, &entity, doc, key, &hex::encode(h.finalize()), submit).await?;
        let mut resp = doc_response(d, if replayed { StatusCode::OK } else { StatusCode::CREATED });
        if replayed {
            resp.headers_mut().insert("idempotent-replayed", HeaderValue::from_static("true"));
        }
        return Ok(resp);
    }
    let d = if submit { st.kernel.insert_submit(&ctx, &entity, doc).await? } else { st.kernel.insert(&ctx, &entity, doc).await? };
    Ok(doc_response(d, StatusCode::CREATED))
}

async fn read(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, Query(p): Query<ReadParams>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let titles = p.titles.unwrap_or(true) || ctx.schema.entity(&entity).is_some_and(|e| e.headed_by_link());
    Ok(doc_response(st.kernel.get(&ctx, &entity, id, titles).await?, StatusCode::OK))
}

/// The receipt a write is sent under (`Idempotency-Key`, and the client's `Idempotency-Digest` when its body
/// is rebuilt on each try): the key, and the digest bound to `target` ([`ferrum_kernel::receipt`]).
fn receipt<'h>(headers: &'h HeaderMap, target: &str, body: &Value) -> Option<(&'h str, String)> {
    let key = headers.get("idempotency-key").and_then(|v| v.to_str().ok())?;
    let client = headers.get("idempotency-digest").and_then(|v| v.to_str().ok());
    Some((key, ferrum_kernel::receipt::digest(target, client, body)))
}

fn replayed(mut resp: Response) -> Response {
    resp.headers_mut().insert("idempotent-replayed", HeaderValue::from_static("true"));
    resp
}

async fn update(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((entity, id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let expected = if_match(&headers)?;
    if let Some((key, digest)) = receipt(&headers, &format!("PATCH {entity}/{id}"), &body) {
        let (doc, again) = st.kernel.update_idempotent(&ctx, &entity, id, body_doc(body)?, expected, key, &digest).await?;
        let resp = doc_response(doc, StatusCode::OK);
        return Ok(if again { replayed(resp) } else { resp });
    }
    Ok(doc_response(st.kernel.update(&ctx, &entity, id, body_doc(body)?, expected).await?, StatusCode::OK))
}

async fn remove(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.kernel.delete(&ctx, &entity, id, if_match(&headers)?).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn submit(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(doc_response(st.kernel.submit(&ctx, &entity, id, if_match(&headers)?).await?, StatusCode::OK))
}

async fn cancel(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(doc_response(st.kernel.cancel(&ctx, &entity, id, if_match(&headers)?).await?, StatusCode::OK))
}

async fn amend(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(doc_response(st.kernel.amend(&ctx, &entity, id).await?, StatusCode::CREATED))
}

async fn action(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((entity, id, action)): Path<(String, Uuid, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let params = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let (mut done, again) = match receipt(&headers, &format!("POST {entity}/{id}/actions/{action}"), &params) {
        Some((key, digest)) => st.kernel.run_action_idempotent(&ctx, &entity, id, &action, params, key, &digest).await?,
        None => (st.kernel.run_action(&ctx, &entity, id, &action, params).await?, false),
    };
    said(&mut done, |e, n| ctx.schema.entity(e).map(|x| if n > 1 { x.def.plural_label() } else { x.def.label.clone() }).unwrap_or_else(|| e.to_string()));
    let resp = Json(done).into_response();
    Ok(if again { replayed(resp) } else { resp })
}

/// An action that does not say what it did is described from its answer: the record's new status
/// and the records it made ("Now Admitted; made STU-2026-00001", "Made 2 Kitchen Tickets"), so a
/// page never only says "done". `label(entity, how_many)` names a kind of record.
fn said(done: &mut Value, label: impl Fn(&str, usize) -> String) {
    let Value::Object(m) = done else { return };
    if m.get("message").and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty()) {
        return;
    }
    let (mut numbered, mut unnumbered): (Vec<String>, Vec<(String, usize)>) = (Vec::new(), Vec::new());
    for v in m.values() {
        let one = std::slice::from_ref(v);
        for c in v.as_array().map(Vec::as_slice).unwrap_or(one).iter().filter_map(Value::as_object) {
            let (Some(entity), Some(_)) = (c.get("entity").and_then(Value::as_str), c.get("id").and_then(Value::as_str)) else { continue };
            match c.get("number").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                Some(n) => numbered.push(n.to_string()),
                None => match unnumbered.iter_mut().find(|(e, _)| e == entity) {
                    Some((_, n)) => *n += 1,
                    None => unnumbered.push((entity.to_string(), 1)),
                },
            }
        }
    }
    let one = |e: &str| {
        let l = label(e, 1);
        let an = l.starts_with(['A', 'E', 'I', 'O', 'U', 'a', 'e', 'i', 'o', 'u']);
        format!("{} {l}", if an { "an" } else { "a" })
    };
    let made: Vec<String> =
        numbered.into_iter().chain(unnumbered.into_iter().map(|(e, n)| if n == 1 { one(&e) } else { format!("{n} {}", label(&e, n)) })).collect();
    let mut parts = Vec::new();
    if let Some(s) = m.get("status").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        parts.push(format!("now {s}"));
    }
    if !made.is_empty() {
        parts.push(format!("made {}", made.join(", ")));
    }
    let text = parts.join("; ");
    let mut chars = text.chars();
    if let Some(first) = chars.next() {
        m.insert("message".into(), Value::String(first.to_uppercase().chain(chars).collect()));
    }
}

async fn transition(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((entity, id, action)): Path<(String, Uuid, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(doc_response(st.kernel.transition(&ctx, &entity, id, &action, if_match(&headers)?).await?, StatusCode::OK))
}

async fn print(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((entity, id)): Path<(String, Uuid)>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    if q.get("format").map(String::as_str) == Some("pdf") {
        let pdf = st.kernel.print_pdf(&ctx, &entity, id).await?;
        let mut resp = pdf.into_response();
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
        if let Ok(v) = HeaderValue::from_str(&format!("inline; filename=\"{}-{}.pdf\"", entity.replace(['"', '\\'], ""), id)) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
        return Ok(resp);
    }
    let html = st.kernel.render_print(&ctx, &entity, id).await?;
    let mut resp = Html(html).into_response();
    resp.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; img-src data: https:; base-uri 'none'; form-action 'none'"),
    );
    Ok(resp)
}

#[derive(serde::Deserialize)]
struct EmailBody {
    to: String,
    subject: Option<String>,
    message: Option<String>,
    /// Attach the print format as a PDF (the default).
    pdf: Option<bool>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Emails a document through the server's mailer: its print format as a PDF attachment (kept on
/// the record), a message, and a note on the record's timeline. Sending a document out of the
/// workspace is exporting it, so it needs export permission.
async fn email_doc(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, Json(b): Json<EmailBody>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let k = &st.kernel;
    ctx.require(&entity, Action::Export)?;
    if k.mailer.is_none() {
        return Err(ApiError(Error::Unavailable("no mailer is configured on this server".into())));
    }
    st.mail_limiter.check(&format!("{}|{}", ctx.tenant, ctx.user)).await.map_err(ApiError)?;
    let doc = k.get(&ctx, &entity, id, false).await?;
    let label = k.entity(&ctx, &entity)?.def.label.clone();
    let number = doc.get("number").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| id.to_string());
    let title = format!("{label} {number}");
    let subject = b.subject.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&title).to_string();
    ferrum_kernel::outbox::check_mail(&b.to, &subject)?;
    let mut files = Vec::new();
    if b.pdf.unwrap_or(true) {
        if !k.mailer.as_ref().is_some_and(|m| m.can_attach()) {
            return Err(ApiError(Error::Unavailable("this server's mailer cannot attach files; send without the PDF".into())));
        }
        let pdf = k.print_pdf(&ctx, &entity, id).await?;
        let name: String = number.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
        let stored = k.store_file(&ctx, Some((&entity, id)), &format!("{name}.pdf"), "application/pdf", &pdf).await?;
        files.extend(stored.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()));
    }
    let message = b.message.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let html = match message {
        Some(m) => format!("<p>{}</p>", esc(m).replace('\n', "<br>")),
        None if files.is_empty() => format!("<p>{}</p>", esc(&title)),
        None => format!("<p>Please find {} attached.</p>", esc(&title)),
    };
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    k.queue_mail_with(&mut tx, ctx.tenant, &b.to, &subject, &html, &files).await?;
    tx.commit().await.map_err(Error::from)?;
    let to = b.to.trim().to_string();
    if let Err(e) = k.add_comment(&ctx, &entity, id, &format!("Emailed to {to}: {subject}"), &[]).await {
        tracing::warn!(%entity, %id, "could not note the email on the record: {e}");
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"queued": true, "to": to, "subject": subject, "attachments": files}))).into_response())
}

async fn audit_trail(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(st.kernel.audit_trail(&ctx, &entity, id).await?).into_response())
}

async fn calc(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Json(body): Json<Value>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(Value::Object(st.kernel.preview(&ctx, &entity, body_doc(body)?).await?)).into_response())
}

// A CSV cell, through the platform's one guard against spreadsheet formulas.
use ferrum_kernel::published::csv_cell;

async fn export(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Query(p): Query<ListParams>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    ctx.require(&entity, Action::Export)?;
    let ent = st.kernel.entity(&ctx, &entity)?;
    let mut q = p.to_query()?;
    q.limit = Some(500);
    q.offset = None;
    q.sort = Some("created_at desc".into());
    q.with_count = false;
    let columns = export_columns(&ent, q.fields.as_deref());
    let xlsx = match p.format.as_deref() {
        None | Some("csv") => false,
        Some("xlsx") => true,
        Some(other) => return Err(ApiError(Error::invalid(format!("unknown export format '{other}' (csv or xlsx)")))),
    };
    let fetch = {
        let (k, ctx, entity, q) = (st.kernel.clone(), ctx.clone(), entity.clone(), q.clone());
        move |cursor: Option<String>| {
            let (k, ctx, entity, mut q) = (k.clone(), ctx.clone(), entity.clone(), q.clone());
            async move {
                q.cursor = cursor;
                let page = k.list(&ctx, &entity, &q).await?;
                Ok((page.rows, page.next_cursor))
            }
        }
    };
    let (body, content_type, ext) = if xlsx {
        let records = up_to(EXPORT_MAX_ROWS, fetch).await?;
        (axum::body::Body::from(xlsx_sheet(&ent, &columns, &records)?), "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "xlsx")
    } else {
        (axum::body::Body::from_stream(csv_stream(columns, fetch)), "text/csv; charset=utf-8", "csv")
    };
    let mut resp = body.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{entity}.{ext}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

/// A page of an export: its rows, and where the next begins.
type Page = (Vec<Doc>, Option<String>);

/// Rows for a workbook, which is made whole in memory: at most `cap`, refusing more rather than
/// cutting the list short without a word (FEO-066).
async fn up_to<F, Fut>(cap: usize, mut fetch: F) -> Result<Vec<Doc>, Error>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Page, Error>>,
{
    let (mut rows, mut cursor) = (Vec::new(), None);
    loop {
        let (page, next) = fetch(cursor).await?;
        rows.extend(page);
        match next {
            Some(_) if rows.len() >= cap => {
                return Err(Error::invalid(format!("more than {cap} rows for a workbook: export them as CSV, which has no limit, or narrow the list")));
            }
            Some(c) => cursor = Some(c),
            None => return Ok(rows),
        }
    }
}

/// Every row of a list as CSV, a page at a time, however many there are (FEO-066). A page that fails
/// ends the download with an error, so a cut file is never taken for a whole one.
fn csv_stream<F, Fut>(columns: Vec<String>, fetch: F) -> impl futures_util::Stream<Item = std::io::Result<bytes::Bytes>> + Send + 'static
where
    F: FnMut(Option<String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Page, Error>> + Send,
{
    let header = format!("{}\n", columns.iter().map(|c| csv_cell(&Value::String(c.clone()))).collect::<Vec<_>>().join(","));
    // The cursor: Some(None) for the first page, Some(Some(c)) for the next, None when done.
    futures_util::stream::unfold((Some(header), Some(None::<String>), fetch, columns), |(header, cursor, mut fetch, columns)| async move {
        if let Some(h) = header {
            return Some((Ok(bytes::Bytes::from(h)), (None, cursor, fetch, columns)));
        }
        let cursor = cursor?;
        match fetch(cursor).await {
            Ok((rows, next)) => {
                let mut out = String::new();
                for r in &rows {
                    out.push_str(&columns.iter().map(|c| csv_cell(r.get(c).unwrap_or(&Value::Null))).collect::<Vec<_>>().join(","));
                    out.push('\n');
                }
                Some((Ok(bytes::Bytes::from(out)), (None, next.map(Some), fetch, columns)))
            }
            Err(e) => Some((Err(std::io::Error::other(e.to_string())), (None, None, fetch, columns))),
        }
    })
}

/// The columns an export writes: those asked for, else the number and every field shown, never a
/// field kept out of exports (`no_export`), even when asked for.
fn export_columns(ent: &ferrum_meta::EffEntity, asked: Option<&[String]>) -> Vec<String> {
    let kept = |name: &str| ent.field(name).is_none_or(|f| !f.no_export);
    match asked {
        Some(f) => f.iter().filter(|c| kept(c)).cloned().collect(),
        None => std::iter::once("number".to_string()).chain(ent.value_fields().filter(|f| !f.hidden && !f.no_export).map(|f| f.name.clone())).collect(),
    }
}

/// One worksheet: a bold header of field labels, numbers as numbers (Excel's own precision),
/// checks as booleans, everything else as text exactly as the API returns it.
fn xlsx_sheet(ent: &ferrum_meta::EffEntity, columns: &[String], records: &[Doc]) -> Result<Vec<u8>, ApiError> {
    use ferrum_meta::FieldType;
    use rust_xlsxwriter::{Format, Workbook};
    let fail = |e: rust_xlsxwriter::XlsxError| ApiError(Error::internal(format!("xlsx: {e}")));
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    let bold = Format::new().set_bold();
    for (c, name) in columns.iter().enumerate() {
        let label = ent.field(name).map(|f| f.label().to_string()).unwrap_or_else(|| name.clone());
        ws.write_string_with_format(0, c as u16, label, &bold).map_err(fail)?;
    }
    for (r, rec) in records.iter().enumerate() {
        let row = r as u32 + 1;
        for (c, name) in columns.iter().enumerate() {
            let col = c as u16;
            let numeric = ent.field(name).is_some_and(|f| matches!(f.ftype, FieldType::Int | FieldType::Decimal | FieldType::Currency | FieldType::Percent));
            match rec.get(name).unwrap_or(&Value::Null) {
                Value::Null => {}
                Value::Bool(b) => {
                    ws.write_boolean(row, col, *b).map_err(fail)?;
                }
                Value::Number(n) if n.as_f64().is_some() => {
                    ws.write_number(row, col, n.as_f64().unwrap_or_default()).map_err(fail)?;
                }
                Value::String(s) if numeric && s.parse::<f64>().is_ok() => {
                    ws.write_number(row, col, s.parse::<f64>().unwrap_or_default()).map_err(fail)?;
                }
                // Excel cells hold at most 32,767 characters.
                Value::String(s) => {
                    ws.write_string(row, col, s.chars().take(32_767).collect::<String>()).map_err(fail)?;
                }
                other => {
                    ws.write_string(row, col, other.to_string().chars().take(32_767).collect::<String>()).map_err(fail)?;
                }
            }
        }
    }
    wb.save_to_buffer().map_err(fail)
}

async fn import(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Json(body): Json<Value>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    ctx.require(&entity, Action::Import)?;
    let rows = body.as_array().ok_or_else(|| ApiError(Error::invalid("expected a JSON array of records")))?;
    let (created, errors) = st.kernel.import_rows(&ctx, &entity, rows).await?;
    let status = if errors.is_empty() { StatusCode::CREATED } else { StatusCode::MULTI_STATUS };
    Ok((status, Json(json!({"created": created.len(), "ids": created, "errors": errors}))).into_response())
}

#[derive(Deserialize)]
struct BulkRequest {
    #[serde(default)]
    ids: Option<Vec<Uuid>>,
    #[serde(default)]
    filters: Option<Value>,
    #[serde(flatten)]
    op: ferrum_kernel::bulk::BulkOp,
}

/// One operation on a selection (`ids`) or on everything `filters` match, record by record.
async fn bulk(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Json(b): Json<BulkRequest>) -> ApiResult<Response> {
    use ferrum_kernel::bulk::BulkTarget;
    let st = app_state(&app)?;
    let target = match (b.ids, b.filters) {
        (Some(ids), None) => BulkTarget::Ids(ids),
        (None, Some(f)) => BulkTarget::Filters(parse_filters(&f)?),
        _ => return Err(ApiError(Error::invalid("send either ids or filters"))),
    };
    Ok(Json(st.kernel.bulk(&ctx, &entity, target, &b.op).await?).into_response())
}

#[derive(Deserialize)]
struct MergeRequest {
    duplicate: Uuid,
    reason: String,
    /// Values chosen from the duplicate to stay on this record, written in the merge's own transaction.
    #[serde(default)]
    values: Option<serde_json::Map<String, Value>>,
    /// The version of this record the values were chosen against.
    #[serde(default)]
    version: Option<i64>,
}

/// Merges a duplicate into this record ([`ferrum_kernel::merge`]): all its links come here, with the
/// values chosen to stay, whole or not at all.
async fn merge(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, Json(m): Json<MergeRequest>) -> ApiResult<Response> {
    let values = m.values.map(|v| (v, m.version));
    Ok(Json(app_state(&app)?.kernel.merge_records_with(&ctx, &entity, id, m.duplicate, &m.reason, values).await?).into_response())
}

#[derive(Deserialize)]
struct Duplicates {
    limit: Option<i64>,
}

async fn duplicates(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Query(d): Query<Duplicates>) -> ApiResult<Response> {
    Ok(Json(json!({"groups": app_state(&app)?.kernel.duplicates(&ctx, &entity, d.limit.unwrap_or(50)).await?})).into_response())
}

/// The same over more records than a request should wait for: queued as a job (`/api/jobs`).
async fn bulk_job(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Json(b): Json<BulkRequest>) -> ApiResult<Response> {
    let id = app_state(&app)?.kernel.bulk_job(&ctx, &entity, b.ids, b.filters, &b.op).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"job": id}))).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .prefix("/api/v1")
        .add("/{entity}", get(list).post(create))
        .add("/{entity}/_calc", post(calc))
        .add("/{entity}/_export", get(export))
        .add("/{entity}/_import", post(import))
        .add("/{entity}/_bulk", post(bulk))
        .add("/{entity}/_bulk/jobs", post(bulk_job))
        .add("/{entity}/_duplicates", get(duplicates))
        .add("/{entity}/{id}/merge", post(merge))
        .add("/{entity}/{id}", get(read).patch(update).put(update).delete(remove))
        .add("/{entity}/{id}/submit", post(submit))
        .add("/{entity}/{id}/cancel", post(cancel))
        .add("/{entity}/{id}/amend", post(amend))
        .add("/{entity}/{id}/actions/{action}", post(action))
        .add("/{entity}/{id}/workflow/{action}", post(transition))
        .add("/{entity}/{id}/print", get(print))
        .add("/{entity}/{id}/email", post(email_doc))
        .add("/{entity}/{id}/audit", get(audit_trail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_action_is_described_from_its_answer() {
        let label = |e: &str, n: usize| if n > 1 { format!("{e}s") } else { e.to_string() };
        let mut admitted = json!({"status": "Admitted", "student": {"entity": "Student", "id": "s1", "number": "STU-2026-00001"}});
        said(&mut admitted, label);
        assert_eq!(admitted["message"], "Now Admitted; made STU-2026-00001");
        let mut sent = json!({"tickets": [{"entity": "Ticket", "id": "a", "number": null}, {"entity": "Ticket", "id": "b", "number": null}]});
        said(&mut sent, label);
        assert_eq!(sent["message"], "Made 2 Tickets");
        let mut billed = json!({"status": "Billed", "sales_invoice": {"entity": "Invoice", "id": "i", "number": null}});
        said(&mut billed, label);
        assert_eq!(billed["message"], "Now Billed; made an Invoice");
        let mut own = json!({"status": "Ready", "message": "Ready for Table 1"});
        said(&mut own, label);
        assert_eq!(own["message"], "Ready for Table 1", "an action's own words stand");
        let mut nothing = json!({"ok": true});
        said(&mut nothing, label);
        assert!(nothing.get("message").is_none(), "nothing to say: the page says done");
    }

    /// Pages of `n` rows, `per` a page, as the list hands them out.
    fn pages(n: usize, per: usize) -> impl FnMut(Option<String>) -> std::future::Ready<Result<Page, Error>> {
        move |cursor: Option<String>| {
            let from: usize = cursor.map_or(0, |c| c.parse().unwrap());
            let to = (from + per).min(n);
            let rows = (from..to).map(|i| json!({"n": i}).as_object().cloned().unwrap()).collect();
            std::future::ready(Ok((rows, (to < n).then(|| to.to_string()))))
        }
    }

    #[tokio::test]
    async fn exports_hold_every_row_or_say_they_cannot() {
        use futures_util::StreamExt;
        // CSV: every row of 1,201, past any cap, in pages of 500.
        let chunks: Vec<_> = csv_stream(vec!["n".into()], pages(1201, 500)).collect().await;
        let text: String = chunks.into_iter().map(|c| String::from_utf8(c.unwrap().to_vec()).unwrap()).collect();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!((lines.len(), lines[0], lines[1201]), (1202, "n", "1200"), "the header and every row");
        // A workbook: up to its cap, and refused past it rather than cut short.
        assert_eq!(up_to(1201, pages(1201, 500)).await.unwrap().len(), 1201);
        let refused = up_to(1000, pages(1201, 500)).await.unwrap_err();
        assert!(refused.to_string().contains("more than 1000 rows"), "{refused}");
    }

    #[test]
    fn csv_cells_are_safe() {
        assert_eq!(csv_cell(&json!("a,b")), "\"a,b\"");
        assert_eq!(csv_cell(&json!("=SUM(A1)")), "'=SUM(A1)");
        assert_eq!(csv_cell(&json!("-12.5")), "-12.5");
        assert_eq!(csv_cell(&json!("say \"hi\"")), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn exports_leave_out_what_is_kept_out_of_them() {
        let mut packages = ferrum_domain::packages().unwrap();
        for f in packages.iter_mut().flat_map(|p| p.entities.iter_mut()).filter(|e| e.name == "Customer").flat_map(|e| e.fields.iter_mut()) {
            f.no_export |= f.name == "tax_id";
        }
        let catalog = ferrum_meta::Catalog::from_packages(packages).unwrap();
        let schema = ferrum_meta::effective::build(&catalog, &["parties".into()], &[], 1).unwrap();
        let ent = schema.entity("Customer").unwrap();
        let all = export_columns(ent, None);
        assert!(all.contains(&"customer_name".to_string()) && !all.contains(&"tax_id".to_string()), "{all:?}");
        let asked = export_columns(ent, Some(&["customer_name".to_string(), "tax_id".to_string()]));
        assert_eq!(asked, vec!["customer_name".to_string()], "not even when asked for");
    }

    #[test]
    fn xlsx_export_is_a_workbook() {
        let mut packages = ferrum_domain::packages().unwrap();
        packages.extend(ferrum_app_erp::packages().unwrap());
        let catalog = ferrum_meta::Catalog::from_packages(packages).unwrap();
        let schema = ferrum_meta::effective::build(&catalog, &["selling".into()], &[], 1).unwrap();
        let ent = schema.entity("Customer").unwrap();
        let cols = vec!["number".to_string(), "customer_name".to_string(), "credit_limit".to_string(), "disabled".to_string()];
        let rec = json!({"number": "CUST-00001", "customer_name": "=cmd|' /C calc'!A0", "credit_limit": "1250.50", "disabled": false});
        let bytes = xlsx_sheet(ent, &cols, &[rec.as_object().cloned().unwrap()]).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"), "an xlsx file is a zip archive");
        let long = json!({cols[0].clone(): "x".repeat(40_000)});
        assert!(xlsx_sheet(ent, &cols, &[long.as_object().cloned().unwrap()]).is_ok(), "text past Excel's cell limit is cut, not fatal");
        let empty = xlsx_sheet(ent, &cols, &[]).unwrap();
        assert!(bytes.len() > empty.len(), "the row was written");
    }
}
