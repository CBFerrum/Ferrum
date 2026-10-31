//! Package endpoints (`ferrum_kernel::Site`): `/api/app/{package}/…` for signed-in users and
//! `/api/site/{tenant}/{package}/…` for the public. Only tenants that installed the package reach it.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, client_ip};
use crate::state::app_state;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error, Kernel, Site, SiteRequest, SiteResponse};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// No package may accept more than this per request. Each route's own limit
/// (`Site::max_body_for`) is usually far lower, and a body is read only up to it.
const MAX_BODY: usize = 1 << 30;

/// Reads a request body up to the route's `limit`: refused at once when its declared length is
/// larger, and as soon as it grows past the limit otherwise, so no route buffers more than it takes.
pub(crate) async fn read_body(headers: &HeaderMap, body: Body, limit: usize) -> Result<Bytes, ApiError> {
    let limit = limit.min(MAX_BODY);
    let too_large = || ApiError(Error::invalid(format!("request body larger than {} KB", limit / 1024)));
    let declared = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > limit as u64) {
        return Err(too_large());
    }
    axum::body::to_bytes(body, limit).await.map_err(|_| too_large())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn request(
    tenant: Uuid,
    user: Option<Ctx>,
    method: Method,
    uri: &Uri,
    path: String,
    query: HashMap<String, String>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<SiteRequest, ApiError> {
    let mut safe: Map<String, Value> = headers
        .iter()
        .filter(|(k, _)| {
            !matches!(k.as_str(), "authorization" | "cookie" | "proxy-authorization" | "x-ferrum-uri" | "x-ferrum-site-api" | "x-ferrum-site-origin")
        })
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), json!(v))))
        .collect();
    // The path and query exactly as requested, set here and never by the caller: a signature over
    // the request URL (such as a WOPI proof) is checked against it.
    if let Some(pq) = uri.path_and_query() {
        safe.insert("x-ferrum-uri".into(), json!(pq.as_str()));
    }
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

/// Runs a site's handler. A request that changes something finishes once its body is here, even
/// if the browser goes away meanwhile (a page left for another app's): never half an upload, never
/// stored bytes that no record holds. Reads stop with their request.
pub(crate) async fn handle(kernel: &Arc<Kernel>, site: Arc<dyn Site>, req: SiteRequest) -> Result<SiteResponse, ApiError> {
    if matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
        return Ok(site.handle(kernel, req).await?);
    }
    let k = kernel.clone();
    let done = tokio::spawn(async move { site.handle(&k, req).await }).await.map_err(|e| ApiError(Error::internal(format!("the request stopped: {e}"))))?;
    Ok(done?)
}

pub(crate) fn respond(r: SiteResponse) -> Response {
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
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let site = st.kernel.hooks.site(&package).filter(|_| ctx.schema.packages.contains(&package)).cloned();
    let site = site.ok_or_else(|| ApiError(Error::NotFound(format!("package {package}"))))?;
    let tenant = ctx.tenant;
    let body = read_body(&headers, body, site.max_body_for(method.as_str(), &path)).await?;
    let req = request(tenant, Some(ctx), method, &uri, path, query, &headers, body)?;
    Ok(respond(handle(&st.kernel, site, req).await?))
}

async fn public(
    State(app): State<AppContext>,
    Path((tenant, package, path)): Path<(String, String, String)>,
    method: Method,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.public_limiter.check(&format!("{}|{}", client_ip(&headers), tenant.to_lowercase())).await?;
    let (tenant, status) = st.kernel.tenant_by_slug(&tenant).await?;
    if status != "active" {
        return Err(ApiError(Error::NotFound("tenant".into())));
    }
    let schema = st.kernel.registry.schema(tenant).await?;
    let site = st.kernel.hooks.site(&package).filter(|_| schema.packages.contains(&package)).cloned();
    let site = site.ok_or_else(|| ApiError(Error::NotFound(format!("package {package}"))))?;
    let body = read_body(&headers, body, site.max_body_for(method.as_str(), &path)).await?;
    let req = request(tenant, None, method, &uri, path, query, &headers, body)?;
    Ok(respond(handle(&st.kernel, site, req).await?))
}

pub fn routes() -> Routes {
    use axum::routing::any;
    // Bodies are read by `read_body`, each up to its route's own limit.
    Routes::new().add("/api/app/{package}/{*path}", any(app)).add("/api/site/{tenant}/{package}/{*path}", any(public))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bodies_are_read_only_up_to_their_route_limit() {
        let declared = |n: usize| {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(n));
            h
        };
        // Refused on its declared length, before a byte is read.
        assert!(read_body(&declared(2_000), Body::from(vec![0u8; 10]), 1_000).await.is_err());
        // Without a length, refused once it grows past the limit.
        assert!(read_body(&HeaderMap::new(), Body::from(vec![0u8; 2_000]), 1_000).await.is_err());
        let ok = read_body(&declared(10), Body::from(vec![7u8; 10]), 1_000).await.unwrap();
        assert_eq!(ok.as_ref(), [7u8; 10]);
        // The ceiling holds whatever a route asks for.
        assert!(read_body(&declared(MAX_BODY + 1), Body::empty(), usize::MAX).await.is_err());
    }
}
