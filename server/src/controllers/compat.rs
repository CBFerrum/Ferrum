//! ERPNext/Frappe REST compatibility shim so existing integrations keep working during migration:
//! `/api/resource/{DocType}` and `/api/resource/{DocType}/{name}`, plus `frappe.auth.get_logged_user`.
//! DocType names map by removing spaces (`Sales Invoice` → `SalesInvoice`); `name` is the
//! document number (or id). Responses use Frappe's `{"data": ...}` / `{"message": ...}` envelopes.

use crate::controllers::docs::body_doc;
use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::{Ctx, Doc, Error, Filter, Kernel, ListQuery};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

fn entity_name(doctype: &str) -> String {
    doctype.split_whitespace().collect::<String>()
}

fn frappe_view(mut d: Doc) -> Value {
    let name = d.get("number").filter(|v| !v.is_null()).cloned().or_else(|| d.get("id").cloned()).unwrap_or(Value::Null);
    d.insert("name".into(), name);
    if let Some(v) = d.get("created_at").cloned() {
        d.insert("creation".into(), v);
    }
    if let Some(v) = d.get("updated_at").cloned() {
        d.insert("modified".into(), v);
    }
    Value::Object(d)
}

async fn resolve(k: &Kernel, ctx: &Ctx, entity: &str, name: &str) -> Result<Uuid, ApiError> {
    if let Ok(id) = name.parse::<Uuid>() {
        return Ok(id);
    }
    let found = k.list(ctx, entity, &ListQuery { filters: vec![Filter::eq("number", name)], limit: Some(1), ..Default::default() }).await?;
    found
        .rows
        .first()
        .and_then(|d| d.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()))
        .ok_or_else(|| ApiError(Error::NotFound(format!("{entity} {name}"))))
}

#[derive(Deserialize)]
struct FrappeList {
    fields: Option<String>,
    filters: Option<String>,
    order_by: Option<String>,
    limit_start: Option<u32>,
    limit_page_length: Option<u32>,
}

async fn list(Auth(ctx): Auth, State(app): State<AppContext>, Path(doctype): Path<String>, Query(p): Query<FrappeList>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let entity = entity_name(&doctype);
    let filters = match p.filters.as_deref() {
        Some(s) if !s.trim().is_empty() => {
            let v: Value = serde_json::from_str(s)?;
            // Frappe filters may be [[doctype, field, op, value]]; drop the doctype element.
            let v = match v {
                Value::Array(items) => Value::Array(
                    items
                        .into_iter()
                        .map(|i| match i {
                            Value::Array(a) if a.len() == 4 => Value::Array(a[1..].to_vec()),
                            other => other,
                        })
                        .collect(),
                ),
                other => other,
            };
            parse_filters(&v)?
        }
        _ => vec![],
    };
    let fields: Option<Vec<String>> = p
        .fields
        .as_deref()
        .and_then(|f| serde_json::from_str::<Vec<String>>(f).ok())
        .map(|v| v.into_iter().filter(|x| x != "*" && x != "name").collect::<Vec<_>>())
        .filter(|v| !v.is_empty());
    let sort = p.order_by.map(|o| o.replace('`', "").replace("modified", "updated_at").replace("creation", "created_at"));
    let q = ListQuery {
        filters,
        fields: fields.map(|mut f| {
            f.push("number".into());
            f
        }),
        sort,
        limit: Some(p.limit_page_length.unwrap_or(20).clamp(1, 500)),
        offset: p.limit_start,
        ..Default::default()
    };
    let page = st.kernel.list(&ctx, &entity, &q).await?;
    Ok(Json(json!({"data": page.rows.into_iter().map(frappe_view).collect::<Vec<_>>()})).into_response())
}

async fn read(Auth(ctx): Auth, State(app): State<AppContext>, Path((doctype, name)): Path<(String, String)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let entity = entity_name(&doctype);
    let id = resolve(&st.kernel, &ctx, &entity, &name).await?;
    Ok(Json(json!({"data": frappe_view(st.kernel.get(&ctx, &entity, id, false).await?)})).into_response())
}

fn strip_frappe(mut d: Doc) -> Doc {
    for k in ["name", "doctype", "owner", "creation", "modified", "modified_by", "idx", "docstatus", "__islocal", "__unsaved"] {
        d.remove(k);
    }
    d
}

async fn create(Auth(ctx): Auth, State(app): State<AppContext>, Path(doctype): Path<String>, Json(body): Json<Value>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let doc = st.kernel.insert(&ctx, &entity_name(&doctype), strip_frappe(body_doc(body)?)).await?;
    Ok(Json(json!({"data": frappe_view(doc)})).into_response())
}

async fn update(Auth(ctx): Auth, State(app): State<AppContext>, Path((doctype, name)): Path<(String, String)>, Json(body): Json<Value>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let entity = entity_name(&doctype);
    let id = resolve(&st.kernel, &ctx, &entity, &name).await?;
    let doc = st.kernel.update(&ctx, &entity, id, strip_frappe(body_doc(body)?), None).await?;
    Ok(Json(json!({"data": frappe_view(doc)})).into_response())
}

async fn remove(Auth(ctx): Auth, State(app): State<AppContext>, Path((doctype, name)): Path<(String, String)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let entity = entity_name(&doctype);
    let id = resolve(&st.kernel, &ctx, &entity, &name).await?;
    st.kernel.delete(&ctx, &entity, id, None).await?;
    Ok(Json(json!({"message": "ok"})).into_response())
}

async fn logged_user(Auth(ctx): Auth) -> ApiResult<Response> {
    Ok(Json(json!({"message": ctx.email})).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new()
        .add("/api/resource/{doctype}", get(list).post(create))
        .add("/api/resource/{doctype}/{name}", get(read).put(update).delete(remove))
        .add("/api/method/frappe.auth.get_logged_user", get(logged_user))
}
