//! Document templates (crates/kernel/src/documents.rs, BD-21): administrators list, read, save, reset and preview
//! them here; people printing see the ones they may pick and render one for a record, or for many (labels).

use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::documents::{QueryRun, Rendered};
use ferrum_kernel::{Ctx, Error, Kernel};
use ferrum_meta::{DocumentDef, DocumentOutput};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

async fn list(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"templates": app_state(&app)?.kernel.document_templates(&ctx).await?})).into_response())
}

async fn read(Auth(ctx): Auth, State(app): State<AppContext>, Path(key): Path<String>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.document_template(&ctx, &key).await?).into_response())
}

async fn save(Auth(ctx): Auth, State(app): State<AppContext>, Json(def): Json<DocumentDef>) -> ApiResult<Response> {
    app_state(&app)?.kernel.save_document_template(&ctx, &def).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn reset(Auth(ctx): Auth, State(app): State<AppContext>, Path(key): Path<String>) -> ApiResult<Response> {
    app_state(&app)?.kernel.reset_document_template(&ctx, &key).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/admin/documents/preview {template, language?, record_id?}`: a key or an unsaved template, rendered.
pub async fn preview_document(k: &Kernel, ctx: &Ctx, body: &Value) -> ApiResult<Value> {
    let template = body.get("template").ok_or_else(|| ApiError(Error::field("template", "is required")))?;
    let record = match body.get("record_id").filter(|v| !v.is_null()) {
        Some(v) => Some(v.as_str().and_then(|s| s.parse().ok()).ok_or_else(|| ApiError(Error::field("record_id", "must be an id")))?),
        None => None,
    };
    Ok(k.preview_document(ctx, template, body.get("language").and_then(Value::as_str), record).await?)
}

async fn preview(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<Value>) -> ApiResult<Response> {
    Ok(Json(preview_document(&app_state(&app)?.kernel, &ctx, &body).await?).into_response())
}

async fn choices(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.document_choices(&ctx, &entity).await?).into_response())
}

/// A rendered document as a download: a PDF or a file of data as an attachment, a page as HTML that runs nothing (as
/// a print format is served), never cached.
pub fn file_response(r: Rendered) -> Response {
    let page = r.content_type.starts_with("text/html");
    let mut resp = r.bytes.into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&r.content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    if page {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; img-src data: https:; base-uri 'none'; form-action 'none'"),
        );
    } else {
        let how = if r.output == DocumentOutput::Page { "inline" } else { "attachment" };
        if let Ok(v) = HeaderValue::from_str(&format!("{how}; filename=\"{}\"", r.file_name.replace(['"', '\\'], ""))) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    resp
}

#[derive(Deserialize, Default)]
pub struct RenderQuery {
    pub language: Option<String>,
    /// `pdf` or `html` (a page); a page is a PDF where the server makes PDFs unless `html` is asked for.
    pub format: Option<String>,
}

fn wants_pdf(k: &Kernel, format: Option<&str>) -> ApiResult<bool> {
    match format {
        None => Ok(k.pdf_enabled()),
        Some("pdf") => Ok(true),
        Some("html") => Ok(false),
        Some(other) => Err(ApiError(Error::field("format", format!("pdf or html, not {other}")))),
    }
}

/// `GET /api/documents/{key}/render/{id}?language=&format=`: template `key` for one record, as the caller may read it.
pub async fn render_one(k: &Kernel, ctx: &Ctx, key: &str, id: Uuid, q: &RenderQuery) -> ApiResult<Response> {
    let pdf = wants_pdf(k, q.format.as_deref())?;
    Ok(file_response(k.render_document(ctx, key, id, q.language.as_deref(), pdf).await?))
}

async fn render(Auth(ctx): Auth, State(app): State<AppContext>, Path((key, id)): Path<(String, Uuid)>, Query(q): Query<RenderQuery>) -> ApiResult<Response> {
    render_one(&app_state(&app)?.kernel, &ctx, &key, id, &q).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelsBody {
    ids: Vec<Uuid>,
    #[serde(default)]
    copies: Option<u32>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

/// `POST /api/documents/{key}/labels {ids, copies?, language?, format?}`: one document for many records.
async fn labels(Auth(ctx): Auth, State(app): State<AppContext>, Path(key): Path<String>, Json(b): Json<LabelsBody>) -> ApiResult<Response> {
    let k = &app_state(&app)?.kernel;
    let pdf = wants_pdf(k, b.format.as_deref())?;
    Ok(file_response(k.render_documents(&ctx, &key, &b.ids, b.copies.unwrap_or(1), b.language.as_deref(), pdf).await?))
}

/// `POST /api/documents/{key}/query {params?, values?, language?}`: template `key`, about a saved report's rows, as
/// the caller may read them: one file, or (grouping by a link) `{files, next}`, a file kept on each record.
pub async fn run_query(k: &Kernel, ctx: &Ctx, key: &str, body: &Value) -> ApiResult<Response> {
    let mut body = if body.is_null() { json!({}) } else { body.clone() };
    body["template"] = json!(key);
    let run: QueryRun = serde_json::from_value(body).map_err(|e| ApiError(Error::field("body", e.to_string())))?;
    if k.query_document_groups(ctx, key).await? {
        return Ok(Json(k.generate_query_documents(ctx, &run).await?).into_response());
    }
    Ok(file_response(k.render_query_document(ctx, &run).await?))
}

async fn query(Auth(ctx): Auth, State(app): State<AppContext>, Path(key): Path<String>, body: Option<Json<Value>>) -> ApiResult<Response> {
    run_query(&app_state(&app)?.kernel, &ctx, &key, &body.map(|b| b.0).unwrap_or_default()).await
}

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .prefix("/api")
        .add("/admin/documents/templates", get(list).post(save))
        .add("/admin/documents/templates/{key}", get(read).delete(reset))
        .add("/admin/documents/preview", post(preview))
        .add("/documents/choices/{entity}", get(choices))
        .add("/documents/{key}/render/{id}", get(render))
        .add("/documents/{key}/labels", post(labels))
        .add("/documents/{key}/query", post(query))
}
