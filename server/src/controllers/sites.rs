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
/// larger, and as soon as it grows past the limit otherwise, so no route buffers more than it takes
/// (`413`). Sites, DAV and plugins read theirs here.
pub(crate) async fn read_body(headers: &HeaderMap, body: Body, limit: usize) -> Result<Bytes, ApiError> {
    let limit = limit.min(MAX_BODY);
    let too_large = || ApiError(crate::error::too_large(format!("request body larger than {} KB", limit / 1024)));
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
            !matches!(
                k.as_str(),
                "authorization"
                    | "cookie"
                    | "proxy-authorization"
                    | "x-ferrum-uri"
                    | "x-ferrum-site-api"
                    | "x-ferrum-site-origin"
                    | "x-ferrum-brand"
                    | "x-ferrum-language"
                    | "x-ferrum-languages"
            )
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
    let done = ferrum_kernel::Trace::spawn(async move { site.handle(&k, req).await })
        .await
        .map_err(|e| ApiError(Error::internal(format!("the request stopped: {e}"))))?;
    Ok(done?)
}

/// The `Range` a request asks for, if any.
pub(crate) fn range(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::RANGE).and_then(|v| v.to_str().ok())
}

/// The stored file a site's answer names ([`SiteResponse::file`]), taken out of its headers: the
/// workspace's own, as a stream (only the `range` asked for, when the file holds it), its length
/// and that range. None when it names none. A kind of file an app keeps for its own work goes out
/// only from a site of that app's (`package`'s owner): not found from any other.
pub async fn named_file(
    kernel: &Kernel,
    objects: &crate::objects::Objects,
    tenant: uuid::Uuid,
    package: &str,
    r: &mut SiteResponse,
    range: Option<&str>,
) -> ferrum_kernel::Result<Option<(u64, Option<std::ops::Range<u64>>, Body)>> {
    let named = r.headers.iter().position(|(k, _)| k.eq_ignore_ascii_case(ferrum_kernel::hooks::STREAM_FILE_HEADER));
    let Some(id) = named.map(|i| r.headers.remove(i).1) else { return Ok(None) };
    let id: uuid::Uuid = id.parse().map_err(|_| Error::internal("a site named a file that is no file id"))?;
    let mut tx = kernel.db.tenant_tx(tenant).await?;
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("select storage_key, entity from file_object where tenant_id = $1 and id = $2").bind(tenant).bind(id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    let key = row.filter(|(_, entity)| kernel.site_may_send(package, entity.as_deref())).map(|r| r.0).ok_or_else(|| Error::NotFound("file".into()))?;
    Ok(Some(objects.stream(&key, range).await?))
}

/// A site's answer as the server sends it. One that names a stored file of the workspace's gets
/// that file as its body, streamed from storage, or the `range` of it the request asks for when the
/// answer is a whole one (200).
pub(crate) async fn respond(
    st: &crate::state::AppState,
    tenant: uuid::Uuid,
    package: &str,
    mut r: SiteResponse,
    range: Option<&str>,
) -> Result<Response, ApiError> {
    let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK);
    let range = range.filter(|_| status == StatusCode::OK);
    let streamed = named_file(&st.kernel, &st.objects, tenant, package, &mut r, range).await?;
    let mut resp = match streamed {
        Some(s) => crate::objects::streamed_response(status, s),
        None => (status, r.body).into_response(),
    };
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
    Ok(resp)
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
    // The signed-in endpoints of a package the caller's zone leaves out (crate::zones, level 2) run its own SQL, which
    // no narrowing reaches: refused outright, naming where the whole workspace opens. Its public ones are untouched.
    if !ctx.sees_package(&package) {
        return Err(ApiError(st.kernel.zone_refusal(ctx.tenant, &st.zones).await));
    }
    let tenant = ctx.tenant;
    let body = read_body(&headers, body, site.max_body_for(method.as_str(), &path)).await?;
    let req = request(tenant, Some(ctx), method, &uri, path, query, &headers, body)?;
    let mut res = respond(&st, tenant, &package, handle(&st.kernel, site, req).await?, range(&headers)).await?;
    super::system::contain(res.headers_mut());
    Ok(res)
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
    // Counted per network and workspace once found; a workspace that does not exist counts on the network alone (BD-86).
    let found = st.kernel.tenant_by_slug(&tenant).await;
    st.public_limiter.check_from(&headers, &found.as_ref().map(|(id, _)| id.to_string()).unwrap_or_default()).await?;
    let (tenant, status) = found?;
    if status != "active" || !st.shell.answers_for(&headers, tenant).await? {
        return Err(ApiError(Error::NotFound("tenant".into())));
    }
    let schema = st.kernel.registry.schema(tenant).await?;
    let site = st.kernel.hooks.site(&package).filter(|_| schema.packages.contains(&package)).cloned();
    let site = site.ok_or_else(|| ApiError(Error::NotFound(format!("package {package}"))))?;
    let user = site_user(&st, &headers, &method, tenant, Some(&package)).await?;
    let body = read_body(&headers, body, site.max_body_for(method.as_str(), &path)).await?;
    let req = request(tenant, user, method, &uri, path, query, &headers, body)?;
    let mut res = respond(&st, tenant, &package, handle(&st.kernel, site, req).await?, range(&headers)).await?;
    super::system::contain(res.headers_mut());
    Ok(res)
}

/// The customer a site session makes of this request (docs/builder/decisions.md BD-88): only on a site host of `tenant`,
/// and only for `package` when it is one of that host's app's packages (another app's routes on the same host see
/// nobody); a request that changes something must come from the site's own pages.
pub(crate) async fn site_user(
    st: &crate::state::AppState,
    headers: &HeaderMap,
    method: &Method,
    tenant: Uuid,
    package: Option<&str>,
) -> Result<Option<Ctx>, ApiError> {
    let Ok(crate::shell::Place::Site(t, app)) = st.shell.place(headers).await else { return Ok(None) };
    if t != tenant {
        return Ok(None);
    }
    if let Some(p) = package
        && !st.apps.shell_apps(tenant).await?.iter().any(|a| a.id == app && a.manifest.packages.iter().any(|x| x == p))
    {
        return Ok(None);
    }
    let Some((ctx, _)) = crate::extract::site_person(st, headers, tenant).await else { return Ok(None) };
    if !crate::shell::may_change(method, headers) {
        return Err(ApiError(Error::Forbidden("this request did not come from this site's own pages".into())));
    }
    Ok(Some(ctx))
}

pub fn routes() -> Routes {
    use axum::routing::any;
    // Bodies are read by `read_body`, each up to its route's own limit.
    Routes::new().add("/api/app/{package}/{*path}", any(app)).add("/api/site/{tenant}/{package}/{*path}", any(public))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_cannot_send_platform_headers() {
        let mut h = HeaderMap::new();
        for (k, v) in [
            ("x-ferrum-brand", "0190a0a0-0000-7000-8000-000000000001"),
            ("x-ferrum-language", "de"),
            ("x-ferrum-languages", "de,fr"),
            ("x-ferrum-site-origin", "https://evil.example"),
            ("x-other", "kept"),
        ] {
            h.insert(axum::http::HeaderName::from_static(k), axum::http::HeaderValue::from_static(v));
        }
        let uri: Uri = "/x".parse().unwrap();
        let req = request(Uuid::nil(), None, Method::GET, &uri, "x".into(), HashMap::new(), &h, Bytes::new()).unwrap();
        for k in ["x-ferrum-brand", "x-ferrum-language", "x-ferrum-languages", "x-ferrum-site-origin"] {
            assert!(!req.headers.contains_key(k), "{k} is the server's to set");
        }
        assert_eq!(req.headers["x-other"], serde_json::json!("kept"));
    }

    #[tokio::test]
    async fn bodies_are_read_only_up_to_their_route_limit() {
        let declared = |n: usize| {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(n));
            h
        };
        let refused = |r: Result<Bytes, ApiError>| r.err().map(|e| (e.0.status(), e.0.code()));
        let too_large = Some((413, "too_large"));
        // Refused on its declared length, before a byte is read.
        assert_eq!(refused(read_body(&declared(2_000), Body::from(vec![0u8; 10]), 1_000).await), too_large);
        // Without a length, refused once it grows past the limit.
        assert_eq!(refused(read_body(&HeaderMap::new(), Body::from(vec![0u8; 2_000]), 1_000).await), too_large);
        let ok = read_body(&declared(10), Body::from(vec![7u8; 10]), 1_000).await.unwrap();
        assert_eq!(ok.as_ref(), [7u8; 10]);
        // The ceiling holds whatever a route asks for.
        assert_eq!(refused(read_body(&declared(MAX_BODY + 1), Body::empty(), usize::MAX).await), too_large);
    }
}
