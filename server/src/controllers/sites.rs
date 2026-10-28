//! Package endpoints (`ferrum_kernel::Site`): `/api/app/{package}/…` for signed-in users and
//! `/api/site/{tenant}/{package}/…` for the public. Only tenants that installed the package reach it.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, client_ip};
use crate::state::{RateLimiter, app_state};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error, SiteRequest, SiteResponse};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use uuid::Uuid;

/// No package may accept more than this per request.
const MAX_BODY: usize = 32 << 20;

/// Public endpoints are open to the internet: a generous per-address budget stops floods.
fn public_limiter() -> &'static RateLimiter {
    static L: OnceLock<RateLimiter> = OnceLock::new();
    L.get_or_init(|| RateLimiter::new(Duration::from_secs(60), 300))
}

#[allow(clippy::too_many_arguments)]
fn request(
    limit: usize,
    tenant: Uuid,
    user: Option<Ctx>,
    method: Method,
    path: String,
    query: HashMap<String, String>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<SiteRequest, ApiError> {
    let limit = limit.min(MAX_BODY);
    if body.len() > limit {
        return Err(ApiError(Error::invalid(format!("request body larger than {} KB", limit / 1024))));
    }
    let safe: Map<String, Value> = headers
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "authorization" | "cookie" | "proxy-authorization"))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), json!(v))))
        .collect();
    Ok(SiteRequest {
        tenant,
        user,
        method: method.to_string(),
        path: format!("/{}", path.trim_start_matches('/')),
        query: query.into_iter().map(|(k, v)| (k, json!(v))).collect(),
        headers: safe,
        body: body.to_vec(),
        client_ip: client_ip(headers),
    })
}

fn respond(r: SiteResponse) -> Response {
    let mut resp = (StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK), r.body).into_response();
    if let Ok(ct) = HeaderValue::from_str(&r.content_type) {
        resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    for (k, v) in r.headers {
        if matches!(k.to_ascii_lowercase().as_str(), "content-length" | "transfer-encoding" | "connection") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k), HeaderValue::from_str(&v)) {
            resp.headers_mut().append(name, value);
        }
    }
    resp
}

async fn app(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((package, path)): Path<(String, String)>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let site = st.kernel.hooks.site(&package).filter(|_| ctx.schema.packages.contains(&package)).cloned();
    let site = site.ok_or_else(|| ApiError(Error::NotFound(format!("package {package}"))))?;
    let tenant = ctx.tenant;
    let req = request(site.max_body(), tenant, Some(ctx), method, path, query, &headers, body)?;
    Ok(respond(site.handle(&st.kernel, req).await?))
}

async fn public(
    State(app): State<AppContext>,
    Path((tenant, package, path)): Path<(String, String, String)>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    public_limiter().check(&format!("{}|{}", client_ip(&headers), tenant.to_lowercase()))?;
    let (tenant, status) = st.kernel.tenant_by_slug(&tenant).await?;
    if status != "active" {
        return Err(ApiError(Error::NotFound("tenant".into())));
    }
    let schema = st.kernel.registry.schema(tenant).await?;
    let site = st.kernel.hooks.site(&package).filter(|_| schema.packages.contains(&package)).cloned();
    let site = site.ok_or_else(|| ApiError(Error::NotFound(format!("package {package}"))))?;
    let req = request(site.max_body(), tenant, None, method, path, query, &headers, body)?;
    Ok(respond(site.handle(&st.kernel, req).await?))
}

pub fn routes() -> Routes {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::any;
    Routes::new()
        .add("/api/app/{package}/{*path}", any(app).layer(DefaultBodyLimit::max(MAX_BODY)))
        .add("/api/site/{tenant}/{package}/{*path}", any(public).layer(DefaultBodyLimit::max(MAX_BODY)))
}
