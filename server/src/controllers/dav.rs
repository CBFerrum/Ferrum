//! Calendar and contacts apps (CalDAV, CardDAV; docs/plan/collaboration.md, C5). `/api/dav/{package}/…`
//! signs an app in with HTTP Basic, the person's email and an app password (an API key of scope
//! `dav`, or one to the whole API), and hands the request to the package's endpoints under `dav/`
//! as that person. The password never reaches the package. `/.well-known/caldav` and
//! `/.well-known/carddav` lead to Comms, which serves calendars and address books.

use super::sites::{read_body, request, respond};
use crate::error::ApiResult;
use crate::extract::{client_ip, request_id};
use crate::state::{AppState, app_state};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use ferrum_kernel::{Ctx, Error};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use std::collections::HashMap;

/// The person an app signs in as; `None` without credentials or with wrong ones, which count
/// against the address like failed sign-ins.
async fn person(st: &AppState, headers: &HeaderMap) -> ApiResult<Option<Ctx>> {
    let Some(basic) =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic ")))
    else {
        return Ok(None);
    };
    let decoded = base64::engine::general_purpose::STANDARD.decode(basic.trim()).ok().and_then(|b| String::from_utf8(b).ok());
    let mut found = None;
    if let Some((email, password)) = decoded.as_deref().and_then(|d| d.split_once(':'))
        && let Ok((s, scope)) = st.kernel.verify_api_key(password.trim()).await
        && scope.as_deref().is_none_or(|scope| scope == "dav")
        && let Ok(ctx) = st.kernel.load_ctx(s.tenant, s.user, Some(s.token_version), request_id(headers)).await
        && ctx.email.eq_ignore_ascii_case(email.trim())
    {
        found = Some(ctx);
    }
    match found {
        Some(ctx) => {
            st.user_limiter.check_local(&format!("{}|{}", ctx.tenant, ctx.user))?;
            Ok(Some(ctx))
        }
        None => {
            st.login_limiter.check(&format!("{}|dav", client_ip(headers))).await?;
            Ok(None)
        }
    }
}

/// 401 with the challenge apps answer with the person's email and app password.
fn challenge() -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, "Sign in with your email and an app password for calendar and contacts apps.\n").into_response();
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Ferrum\", charset=\"UTF-8\""));
    r
}

async fn dav(
    State(app): State<AppContext>,
    Path(rest): Path<String>,
    method: Method,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let Some(ctx) = person(&st, &headers).await? else { return Ok(challenge()) };
    // One route for `comms`, `comms/` and `comms/…`: a catch-all after the package name would
    // need something after its slash.
    let (package, path) = rest.split_once('/').unwrap_or((&rest, ""));
    let site = st.kernel.hooks.site(package).filter(|_| ctx.schema.packages.iter().any(|p| p == package)).cloned();
    let site = site.ok_or_else(|| Error::NotFound(format!("package {package}")))?;
    let path = format!("dav/{path}");
    let body = read_body(&headers, body, site.max_body_for(method.as_str(), &path)).await?;
    let tenant = ctx.tenant;
    let req = request(tenant, Some(ctx), method, &uri, path, query, &headers, body)?;
    Ok(respond(site.handle(&st.kernel, req).await?))
}

/// Where apps look first (RFC 6764).
async fn well_known() -> Response {
    (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "/api/dav/comms/")]).into_response()
}

pub fn routes() -> Routes {
    use axum::routing::any;
    Routes::new().add("/api/dav/{*rest}", any(dav)).add("/.well-known/caldav", any(well_known)).add("/.well-known/carddav", any(well_known))
}
