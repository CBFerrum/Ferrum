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

const EXPORT_MAX_ROWS: usize = 100_000;
const IMPORT_MAX_ROWS: usize = 5_000;

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
    Ok(Json(st.kernel.list(&ctx, &entity, &p.to_query()?).await?).into_response())
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
    Ok(doc_response(st.kernel.get(&ctx, &entity, id, p.titles.unwrap_or(true)).await?, StatusCode::OK))
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
    body: Option<Json<Value>>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let params = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    Ok(Json(st.kernel.run_action(&ctx, &entity, id, &action, params).await?).into_response())
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

async fn audit_trail(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(st.kernel.audit_trail(&ctx, &entity, id).await?).into_response())
}

async fn calc(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Json(body): Json<Value>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(Value::Object(st.kernel.preview(&ctx, &entity, body_doc(body)?).await?)).into_response())
}

fn csv_cell(v: &Value) -> String {
    let raw = match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => v.to_string(),
        other => other.to_string(),
    };
    // Neutralize spreadsheet formula injection.
    let safe = if raw.starts_with(['=', '+', '-', '@', '\t', '\r']) && raw.parse::<f64>().is_err() { format!("'{raw}") } else { raw };
    if safe.contains([',', '"', '\n', '\r']) { format!("\"{}\"", safe.replace('"', "\"\"")) } else { safe }
}

async fn export(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Query(p): Query<ListParams>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    ctx.require(&entity, Action::Export)?;
    let ent = st.kernel.entity(&ctx, &entity)?;
    let mut q = p.to_query()?;
    q.limit = Some(500);
    q.offset = None;
    q.sort = Some("created_at desc".into());
    q.with_count = false;
    let columns: Vec<String> = match &q.fields {
        Some(f) => f.clone(),
        None => std::iter::once("number".to_string()).chain(ent.value_fields().filter(|f| !f.hidden).map(|f| f.name.clone())).collect(),
    };
    let xlsx = match p.format.as_deref() {
        None | Some("csv") => false,
        Some("xlsx") => true,
        Some(other) => return Err(ApiError(Error::invalid(format!("unknown export format '{other}' (csv or xlsx)")))),
    };
    let mut records: Vec<Doc> = Vec::new();
    loop {
        let page = st.kernel.list(&ctx, &entity, &q).await?;
        records.extend(page.rows);
        match page.next_cursor {
            Some(c) if records.len() < EXPORT_MAX_ROWS => q.cursor = Some(c),
            _ => break,
        }
    }
    let (body, content_type, ext) = if xlsx {
        (xlsx_sheet(&ent, &columns, &records)?, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "xlsx")
    } else {
        let mut out = String::new();
        out.push_str(&columns.iter().map(|c| csv_cell(&Value::String(c.clone()))).collect::<Vec<_>>().join(","));
        out.push('\n');
        for r in &records {
            out.push_str(&columns.iter().map(|c| csv_cell(r.get(c).unwrap_or(&Value::Null))).collect::<Vec<_>>().join(","));
            out.push('\n');
        }
        (out.into_bytes(), "text/csv; charset=utf-8", "csv")
    };
    let mut resp = body.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{entity}.{ext}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
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
                Value::String(s) => {
                    ws.write_string(row, col, s).map_err(fail)?;
                }
                other => {
                    ws.write_string(row, col, other.to_string()).map_err(fail)?;
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
    if rows.len() > IMPORT_MAX_ROWS {
        return Err(ApiError(Error::invalid(format!("at most {IMPORT_MAX_ROWS} rows per request"))));
    }
    let mut created = Vec::new();
    let mut errors = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let Some(doc) = r.as_object().cloned() else {
            errors.push(json!({"row": i, "error": "not an object"}));
            continue;
        };
        match st.kernel.insert(&ctx, &entity, doc).await {
            Ok(d) => created.push(d.get("id").cloned().unwrap_or(Value::Null)),
            Err(Error::Validation(f)) => errors.push(json!({"row": i, "error": "validation_failed", "fields": f})),
            Err(e) => errors.push(json!({"row": i, "error": e.code(), "message": e.to_string()})),
        }
    }
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

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .prefix("/api/v1")
        .add("/{entity}", get(list).post(create))
        .add("/{entity}/_calc", post(calc))
        .add("/{entity}/_export", get(export))
        .add("/{entity}/_import", post(import))
        .add("/{entity}/_bulk", post(bulk))
        .add("/{entity}/{id}", get(read).patch(update).put(update).delete(remove))
        .add("/{entity}/{id}/submit", post(submit))
        .add("/{entity}/{id}/cancel", post(cancel))
        .add("/{entity}/{id}/amend", post(amend))
        .add("/{entity}/{id}/actions/{action}", post(action))
        .add("/{entity}/{id}/workflow/{action}", post(transition))
        .add("/{entity}/{id}/print", get(print))
        .add("/{entity}/{id}/audit", get(audit_trail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_cells_are_safe() {
        assert_eq!(csv_cell(&json!("a,b")), "\"a,b\"");
        assert_eq!(csv_cell(&json!("=SUM(A1)")), "'=SUM(A1)");
        assert_eq!(csv_cell(&json!("-12.5")), "-12.5");
        assert_eq!(csv_cell(&json!("say \"hi\"")), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn xlsx_export_is_a_workbook() {
        let catalog = ferrum_meta::Catalog::from_packages(ferrum_domain::packages().unwrap()).unwrap();
        let schema = ferrum_meta::effective::build(&catalog, &["core".into()], &[], 1).unwrap();
        let ent = schema.entity("Customer").unwrap();
        let cols = vec!["number".to_string(), "customer_name".to_string(), "credit_limit".to_string(), "disabled".to_string()];
        let rec = json!({"number": "CUST-00001", "customer_name": "=cmd|' /C calc'!A0", "credit_limit": "1250.50", "disabled": false});
        let bytes = xlsx_sheet(ent, &cols, &[rec.as_object().cloned().unwrap()]).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"), "an xlsx file is a zip archive");
        let empty = xlsx_sheet(ent, &cols, &[]).unwrap();
        assert!(bytes.len() > empty.len(), "the row was written");
    }
}
