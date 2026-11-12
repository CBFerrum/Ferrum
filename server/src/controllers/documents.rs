//! Document templates (crates/kernel/src/documents.rs, BD-21): administrators list, read, save, reset and preview
//! them here; people printing see the ones they may pick and render one for a record, or for many (labels).

use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::documents::{Preview, PreviewAsk, QueryRun, Rendered};
use ferrum_kernel::{Ctx, Error, Kernel};
use ferrum_meta::{DocumentDef, DocumentOutput};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize, Default)]
struct ListFilter {
    entity: Option<String>,
    output: Option<String>,
}

async fn list(Auth(ctx): Auth, State(app): State<AppContext>, Query(f): Query<ListFilter>) -> ApiResult<Response> {
    let k = &app_state(&app)?.kernel;
    Ok(Json(json!({"templates": k.document_templates_of(&ctx, f.entity.as_deref(), f.output.as_deref()).await?})).into_response())
}

fn with_etag(mut resp: Response, tag: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(tag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp
}

async fn read(Auth(ctx): Auth, State(app): State<AppContext>, Path(key): Path<String>) -> ApiResult<Response> {
    let v = app_state(&app)?.kernel.document_template(&ctx, &key).await?;
    let tag = v["etag"].as_str().unwrap_or_default().to_string();
    Ok(with_etag(Json(v).into_response(), &tag))
}

/// A template from a request's body: unknown keys and bad shapes refused as a field error on what they are in.
fn template_of(v: Value) -> ApiResult<DocumentDef> {
    let mut v = v;
    if let Some(m) = v.as_object_mut() {
        // Derived, never saved.
        m.remove("kind");
    }
    serde_json::from_value(v).map_err(|e| {
        let msg = e.to_string();
        let field = if msg.contains("block") || msg.contains("section") || msg.contains("layout") { "layout" } else { "template" };
        ApiError(Error::field(field, msg))
    })
}

async fn save(Auth(ctx): Auth, State(app): State<AppContext>, headers: HeaderMap, Json(body): Json<Value>) -> ApiResult<Response> {
    let def = template_of(body)?;
    let given =
        headers.get(header::IF_MATCH).map(|v| v.to_str().map(str::to_string)).transpose().map_err(|_| ApiError(Error::invalid("bad If-Match header")))?;
    let tag = app_state(&app)?.kernel.save_document_template_if(&ctx, &def, given.as_deref()).await?;
    Ok(with_etag(StatusCode::NO_CONTENT.into_response(), &tag))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DuplicateBody {
    key: String,
    label: String,
}

/// `POST /api/admin/documents/templates/{key}/duplicate {key, label}`: the workspace's own copy (Customize this format).
async fn duplicate(Auth(ctx): Auth, State(app): State<AppContext>, Path(from): Path<String>, Json(b): Json<DuplicateBody>) -> ApiResult<Response> {
    let def = app_state(&app)?.kernel.duplicate_document_template(&ctx, &from, b.key.trim(), &b.label).await?;
    let tag = ferrum_kernel::documents::etag(&def);
    let mut t = serde_json::to_value(&def).map_err(|e| ApiError(Error::internal(e)))?;
    t["kind"] = json!(if def.layout.is_some() { "layout" } else { "template" });
    Ok(with_etag((StatusCode::CREATED, Json(json!({"template": t}))).into_response(), &tag))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultBody {
    key: Option<String>,
}

/// `PUT /api/admin/documents/defaults/{Entity} {key}`: the workspace's default format (null: its choice removed).
async fn set_default(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Json(b): Json<DefaultBody>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_document_default(&ctx, &entity, b.key.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn fields(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.document_fields(&ctx, &entity)?).into_response())
}

async fn versions(Auth(ctx): Auth, State(app): State<AppContext>, Path(key): Path<String>) -> ApiResult<Response> {
    Ok(Json(json!({"versions": app_state(&app)?.kernel.document_template_versions(&ctx, &key).await?})).into_response())
}

async fn version(Auth(ctx): Auth, State(app): State<AppContext>, Path((key, n)): Path<(String, i32)>) -> ApiResult<Response> {
    Ok(Json(json!({"template": app_state(&app)?.kernel.document_template_version(&ctx, &key, n).await?})).into_response())
}

/// `GET /api/print/layout.css`: the layout renderer's CSS, to read (the canvas is the server's own render).
async fn layout_css() -> Response {
    let mut resp = ferrum_kernel::layout::BASE_CSS.to_string().into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/css; charset=utf-8"));
    resp
}

async fn reset(Auth(ctx): Auth, State(app): State<AppContext>, Path(key): Path<String>) -> ApiResult<Response> {
    app_state(&app)?.kernel.reset_document_template(&ctx, &key).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// What `POST /api/admin/documents/preview` is asked: `{template, record_id?, sample?, language?, letterhead?,
/// copies?, mode?: print|design, format?: html|pdf}`.
pub fn preview_ask(body: &Value) -> ApiResult<PreviewAsk> {
    let mut template = body.get("template").cloned().ok_or_else(|| ApiError(Error::field("template", "is required")))?;
    if template.is_object() {
        template = serde_json::to_value(template_of(template)?).map_err(|e| ApiError(Error::internal(e)))?;
    }
    let record = match body.get("record_id").filter(|v| !v.is_null()) {
        Some(v) => Some(v.as_str().and_then(|s| s.parse().ok()).ok_or_else(|| ApiError(Error::field("record_id", "must be an id")))?),
        None => None,
    };
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    let copies = match body.get("copies") {
        None | Some(Value::Null) => vec![],
        Some(v) => serde_json::from_value::<Vec<String>>(v.clone()).map_err(|_| ApiError(Error::field("copies", "a list of copy labels")))?,
    };
    let design = match text("mode").as_deref() {
        None | Some("print") => false,
        Some("design") => true,
        Some(o) => return Err(ApiError(Error::field("mode", format!("print or design, not {o}")))),
    };
    let pdf = match text("format").as_deref() {
        None | Some("html") => false,
        Some("pdf") => true,
        Some(o) => return Err(ApiError(Error::field("format", format!("html or pdf, not {o}")))),
    };
    Ok(PreviewAsk {
        template,
        record,
        sample: body.get("sample").and_then(Value::as_bool).unwrap_or(false),
        language: text("language"),
        letterhead: text("letterhead"),
        copies,
        design,
        pdf,
    })
}

/// `POST /api/admin/documents/preview`: a key or an unsaved template, rendered (callable without the HTTP layer).
pub async fn preview_document(k: &Kernel, ctx: &Ctx, body: &Value) -> ApiResult<Value> {
    match k.preview_print(ctx, &preview_ask(body)?).await? {
        Preview::Json(v) => Ok(v),
        Preview::Pdf(_) => Err(ApiError(Error::field("format", "a PDF preview is answered as a file"))),
    }
}

async fn preview(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<Value>) -> ApiResult<Response> {
    let k = &app_state(&app)?.kernel;
    match k.preview_print(&ctx, &preview_ask(&body)?).await? {
        Preview::Json(v) => Ok(Json(v).into_response()),
        Preview::Pdf(bytes) => Ok(file_response(Rendered {
            bytes,
            content_type: "application/pdf".into(),
            file_name: "preview.pdf".into(),
            output: DocumentOutput::Page,
            language: String::new(),
        })),
    }
}

#[derive(Deserialize, Default)]
struct ChoicesQuery {
    record: Option<Uuid>,
}

async fn choices(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Query(q): Query<ChoicesQuery>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.print_choices(&ctx, &entity, q.record).await?).into_response())
}

/// The sandboxing policy every printed page carries: it runs nothing, loads nothing but its own `data:` images.
pub const PRINT_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src data: https:; base-uri 'none'; form-action 'none'";

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
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(PRINT_CSP));
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
    use axum::routing::{get, post, put};
    Routes::new()
        .prefix("/api")
        .add("/admin/documents/templates", get(list).post(save))
        .add("/admin/documents/templates/{key}", get(read).delete(reset))
        .add("/admin/documents/templates/{key}/duplicate", post(duplicate))
        .add("/admin/documents/templates/{key}/versions", get(versions))
        .add("/admin/documents/templates/{key}/versions/{n}", get(version))
        .add("/admin/documents/defaults/{entity}", put(set_default))
        .add("/admin/documents/fields/{entity}", get(fields))
        .add("/admin/documents/preview", post(preview))
        .add("/print/layout.css", get(layout_css))
        .add("/documents/choices/{entity}", get(choices))
        .add("/documents/{key}/render/{id}", get(render))
        .add("/documents/{key}/labels", post(labels))
        .add("/documents/{key}/query", post(query))
}
