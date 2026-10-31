//! Request extractors: authentication (a bearer token — session JWT or API key — or the
//! browser's session cookie) → kernel `Ctx`.

use crate::error::ApiError;
use crate::state::app_state;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue};
use ferrum_kernel::{Ctx, Error};
use loco_rs::app::AppContext;
use loco_rs::auth::jwt::JWT;
use std::sync::Arc;
use uuid::Uuid;

/// An authenticated caller. A call from an app's screen (the web app's bridge names the app in
/// `X-Ferrum-App`) is narrowed to what that app may do: see [`screen_ctx`].
pub struct Auth(pub Ctx);

pub fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::now_v7().to_string())
}

pub fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let token = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// What a token says about who is calling.
struct TokenSession {
    session: ferrum_kernel::auth::Session,
    /// It may only set up two-factor sign-in (the workspace requires it and the person has yet to).
    setup_only: bool,
    /// The browser session a cookie's token names (`sid`, docs/plan/apps-ui.md §12.3).
    sid: Option<Uuid>,
    /// The browser session a bearer token given to a page is bound to (`bsid`): signing that
    /// device out ends the token too.
    bound: Option<Uuid>,
    /// An app token's app (`app`, docs/plan/apps-ui.md §12.3): it works only through [`Auth`],
    /// narrowed to that app.
    app: Option<String>,
}

impl TokenSession {
    /// The browser session behind the token, whichever kind it is.
    fn device(&self) -> Option<Uuid> {
        self.sid.or(self.bound)
    }
}

/// The session a token (session JWT, cookie token or `fk_` API key) carries. The ticket between
/// the two steps of signing in is no session at all; a cookie's token names a browser session
/// that must still be valid.
async fn session_of(st: &crate::state::AppState, token: &str) -> Result<TokenSession, ApiError> {
    if token.starts_with("fk_") {
        let (s, scope) = st.kernel.verify_api_key(token).await?;
        if scope.is_some() {
            return Err(ApiError(Error::Unauthorized("this key is an app password for calendar and contacts apps, and works only there".into())));
        }
        st.user_limiter.check_local(&format!("{}|{}", s.tenant, s.user))?;
        return Ok(TokenSession { session: s, setup_only: false, sid: None, bound: None, app: None });
    }
    let data =
        JWT::new(&st.jwt_secret).validate(token).map_err(|_| ApiError(Error::Unauthorized("session expired or invalid; please sign in again".into())))?;
    let claims = &data.claims.claims;
    if claims.contains_key("mfa") {
        return Err(ApiError(Error::Unauthorized("finish signing in first".into())));
    }
    let user: Uuid = data.claims.pid.parse().map_err(|_| ApiError(Error::Unauthorized("invalid session".into())))?;
    let tenant: Uuid =
        claims.get("tenant").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).ok_or_else(|| ApiError(Error::Unauthorized("invalid session".into())))?;
    let tv = claims.get("tv").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
    let setup_only = claims.get("setup_2fa").and_then(|v| v.as_bool()).unwrap_or(false);
    let claim = |name: &str| -> Result<Option<Uuid>, ApiError> {
        match claims.get(name) {
            None => Ok(None),
            Some(v) => Ok(Some(v.as_str().and_then(|s| s.parse::<Uuid>().ok()).ok_or_else(|| ApiError(Error::Unauthorized("invalid session".into())))?)),
        }
    };
    let (sid, bound) = (claim("sid")?, claim("bsid")?);
    st.user_limiter.check_local(&format!("{tenant}|{user}"))?;
    if let Some(device) = sid.or(bound) {
        st.kernel.check_session(tenant, user, device).await?;
    }
    let app = match claims.get("app") {
        None => None,
        Some(v) => Some(v.as_str().filter(|a| crate::appmgr::valid_id(a)).ok_or_else(|| ApiError(Error::Unauthorized("invalid session".into())))?.to_string()),
    };
    Ok(TokenSession { session: ferrum_kernel::auth::Session { tenant, user, token_version: tv }, setup_only, sid, bound, app })
}

/// App tokens work only for their app's calls to the API ([`Auth`]); everything else refuses them.
fn no_app_token(t: &TokenSession) -> Result<(), ApiError> {
    match t.app {
        Some(_) => Err(ApiError(Error::Unauthorized("an app token works only for its app's calls to the API".into()))),
        None => Ok(()),
    }
}

/// Resolves a token (session JWT, cookie token or `fk_` API key) to the caller's context. Sockets
/// use it for both kinds; HTTP requests go through [`Auth`], which also checks where each kind
/// may come from.
pub async fn ctx_from_token(st: &crate::state::AppState, token: &str, request_id: String) -> Result<Ctx, ApiError> {
    let t = session_of(st, token).await?;
    no_app_token(&t)?;
    if t.setup_only {
        return Err(ApiError(Error::Forbidden("set up two-factor sign-in first; this workspace requires it".into())));
    }
    Ok(st.kernel.load_ctx(t.session.tenant, t.session.user, Some(t.session.token_version), request_id).await?)
}

/// The credential of an HTTP request: a bearer token, or the session cookie of a browser page.
/// A cookie's token works only as the cookie, and a bearer token never as one. Requests that
/// change something on the strength of the cookie must come from the site's own pages, and the
/// cookie must be of the workspace the host serves.
async fn caller(st: &crate::state::AppState, parts: &Parts) -> Result<(Ctx, Option<Uuid>), ApiError> {
    caller_on(st, &parts.method, &parts.headers).await
}

/// See [`caller`]; for handlers that read the request themselves. `.1` is the browser session
/// behind the credential (the cookie's, or the one a page's bearer token is bound to).
pub async fn caller_on(st: &crate::state::AppState, method: &axum::http::Method, headers: &HeaderMap) -> Result<(Ctx, Option<Uuid>), ApiError> {
    let (ctx, device, app) = caller_with_app(st, method, headers).await?;
    if app.is_some() {
        return Err(ApiError(Error::Unauthorized("an app token works only for its app's calls to the API".into())));
    }
    Ok((ctx, device))
}

/// [`caller_on`], where an app token is welcome: `.2` names its app (the caller narrows to it).
async fn caller_with_app(
    st: &crate::state::AppState,
    method: &axum::http::Method,
    headers: &HeaderMap,
) -> Result<(Ctx, Option<Uuid>, Option<String>), ApiError> {
    let rid = request_id(headers);
    let (token, from_cookie) = match bearer(headers) {
        Some(t) => (t, false),
        None => match crate::shell::session_cookie(headers, st.shell.secure) {
            Some(c) => (c, true),
            None => return Err(ApiError(Error::Unauthorized("sign in required".into()))),
        },
    };
    if from_cookie && !crate::shell::may_change(method, headers) {
        return Err(ApiError(Error::Forbidden("this request did not come from this site's own pages".into())));
    }
    let t = session_of(st, &token).await?;
    let mismatch = if from_cookie { t.sid.is_none() || t.bound.is_some() } else { t.sid.is_some() };
    if mismatch {
        return Err(ApiError(Error::Unauthorized(
            if from_cookie { "invalid session cookie" } else { "this token works only as the browser's session cookie" }.into(),
        )));
    }
    if t.setup_only {
        return Err(ApiError(Error::Forbidden("set up two-factor sign-in first; this workspace requires it".into())));
    }
    if from_cookie {
        st.shell.check_host(headers, t.session.tenant).await?;
    }
    let ctx = st.kernel.load_ctx(t.session.tenant, t.session.user, Some(t.session.token_version), rid).await?;
    Ok((ctx, t.device(), t.app))
}

/// The caller and the browser session behind its credential (`None` for tokens of API clients
/// and apps): "this device" on the devices page, signing out, step-up.
pub struct BrowserSession(pub Ctx, pub Option<Uuid>);

impl FromRequestParts<AppContext> for BrowserSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppContext) -> Result<Self, Self::Rejection> {
        let st = app_state(state)?;
        let (ctx, sid) = caller(&st, parts).await?;
        Ok(BrowserSession(ctx, sid))
    }
}

/// A socket's credential at the upgrade: a bearer token, or the session cookie when the page
/// asking is on this host (`Origin`). `None`: the client sends `{"auth": token}` first.
pub async fn socket_credential(st: &crate::state::AppState, headers: &HeaderMap) -> Result<Option<(String, Ctx)>, ApiError> {
    let rid = request_id(headers);
    if let Some(token) = bearer(headers) {
        let ctx = ctx_from_token(st, &token, rid).await?;
        return Ok(Some((token, ctx)));
    }
    match crate::shell::session_cookie(headers, st.shell.secure) {
        Some(token) if crate::shell::origin_is_host(headers) => {
            let t = session_of(st, &token).await?;
            no_app_token(&t)?;
            if t.sid.is_none() || t.bound.is_some() || t.setup_only {
                return Err(ApiError(Error::Unauthorized("invalid session cookie".into())));
            }
            st.shell.check_host(headers, t.session.tenant).await?;
            let ctx = st.kernel.load_ctx(t.session.tenant, t.session.user, Some(t.session.token_version), rid).await?;
            Ok(Some((token, ctx)))
        }
        _ => Ok(None),
    }
}

impl FromRequestParts<AppContext> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppContext) -> Result<Self, Self::Rejection> {
        let st = app_state(state)?;
        let (ctx, _, token_app) = caller_with_app(&st, &parts.method, &parts.headers).await?;
        match (token_app, parts.headers.get("x-ferrum-app")) {
            (Some(app), Some(header)) if header.as_bytes() != app.as_bytes() => {
                Err(ApiError(Error::Forbidden(format!("this token is {app}'s, not {}'s", header.to_str().unwrap_or("another app")))))
            }
            // An app token: as its app, as screens' calls are.
            (Some(app), _) => {
                let app = HeaderValue::from_str(&app).map_err(|_| ApiError(Error::Unauthorized("invalid session".into())))?;
                Ok(Auth(screen_ctx(&st, ctx, &app, parts.uri.path()).await?))
            }
            (None, Some(app)) => Ok(Auth(screen_ctx(&st, ctx, app, parts.uri.path()).await?)),
            (None, None) => Ok(Auth(ctx)),
        }
    }
}

/// A call made from an app's screen (docs/plugins.md, "Screens"): only to records, metadata
/// and the app's own endpoints; with the user's rights narrowed to what the app declared; never
/// as an administrator.
async fn screen_ctx(st: &crate::state::AppState, mut ctx: Ctx, app: &HeaderValue, path: &str) -> Result<Ctx, ApiError> {
    let app = app.to_str().map_err(|_| ApiError(Error::invalid("bad X-Ferrum-App header")))?;
    // The app.rs middleware refuses other paths before any handler runs; this keeps the rule
    // where the narrowing is, for routers built without it (tests).
    if !screen_may_call(app, path) {
        return Err(ApiError(Error::Forbidden(format!("screens of {app} may not call {path}"))));
    }
    let Some((entities, own_entities)) = st.apps.screen_scope(ctx.tenant, app).await? else {
        return Err(ApiError(Error::Forbidden(format!("{app} has no screens in this workspace"))));
    };
    ctx.perms = Arc::new(ctx.perms.limited_to(&entities, &own_entities));
    ctx.is_admin = false;
    Ok(ctx)
}

/// A caller whose session may also be one that can only set up two-factor sign-in (then `.1`
/// holds that session).
pub struct SetupAuth(pub Ctx, pub Option<ferrum_kernel::auth::Session>);

impl FromRequestParts<AppContext> for SetupAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppContext) -> Result<Self, Self::Rejection> {
        let st = app_state(state)?;
        let rid = request_id(&parts.headers);
        // A browser session is never setup-only: the cookie is set once sign-in is complete.
        let Some(token) = bearer(&parts.headers) else {
            let (ctx, _) = caller(&st, parts).await?;
            return Ok(SetupAuth(ctx, None));
        };
        let t = session_of(&st, &token).await?;
        no_app_token(&t)?;
        if t.sid.is_some() {
            return Err(ApiError(Error::Unauthorized("this token works only as the browser's session cookie".into())));
        }
        let ctx = st.kernel.load_ctx(t.session.tenant, t.session.user, Some(t.session.token_version), rid).await?;
        Ok(SetupAuth(ctx, t.setup_only.then_some(t.session)))
    }
}

/// Where an app's screens may send requests: records, metadata and the app's own endpoints.
pub(crate) fn screen_may_call(app: &str, path: &str) -> bool {
    path.starts_with("/api/v1/")
        || path.starts_with("/api/meta/")
        || path.strip_prefix("/api/ext/").and_then(|r| r.strip_prefix(app)).is_some_and(|r| r.starts_with('/'))
}

/// `If-Match: "v3"` / `If-Match: 3` → expected document version.
pub fn if_match(headers: &HeaderMap) -> Result<Option<i64>, ApiError> {
    let Some(v) = headers.get(axum::http::header::IF_MATCH) else { return Ok(None) };
    let s = v.to_str().map_err(|_| ApiError(Error::invalid("bad If-Match header")))?.trim().trim_matches('"');
    let n = s.trim_start_matches("W/").trim_matches('"').trim_start_matches('v');
    n.parse::<i64>().map(Some).map_err(|_| ApiError(Error::invalid("If-Match must carry the document version, e.g. \"v3\"")))
}

/// How many proxies in front of the server append to `X-Forwarded-For`: the gateway alone is 1; a
/// CDN or a load balancer before it makes 2 (`settings.proxy_hops`, set once at boot).
pub static PROXY_HOPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// The client's address as the outermost trusted proxy saw it (`"local"` without proxies).
pub fn client_ip(headers: &HeaderMap) -> String {
    client_ip_behind(headers, PROXY_HOPS.load(std::sync::atomic::Ordering::Relaxed))
}

/// The entry `hops` from the right of `X-Forwarded-For`: what a client puts there itself lies to
/// its left and is never taken; behind fewer proxies than that, the leftmost entry.
fn client_ip_behind(headers: &HeaderMap, hops: usize) -> String {
    let entries: Vec<&str> =
        headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).map(str::trim).filter(|e| !e.is_empty()).collect();
    entries.get(entries.len().saturating_sub(hops.max(1))).map_or_else(|| "local".into(), |e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_is_where_the_trusted_proxies_say() {
        let headers = |values: &[&str]| {
            let mut h = HeaderMap::new();
            for v in values {
                h.append("x-forwarded-for", v.parse().unwrap());
            }
            h
        };
        assert_eq!(client_ip_behind(&headers(&[]), 1), "local");
        assert_eq!(client_ip_behind(&headers(&["203.0.113.7"]), 1), "203.0.113.7");
        // The client wrote 198.51.100.1 itself; the gateway appended what it saw.
        assert_eq!(client_ip_behind(&headers(&["198.51.100.1, 203.0.113.7"]), 1), "203.0.113.7");
        // Behind a CDN: the client, then the CDN's edge as the gateway saw it.
        let cdn = headers(&["198.51.100.1, 203.0.113.7", "192.0.2.10"]);
        assert_eq!((client_ip_behind(&cdn, 1), client_ip_behind(&cdn, 2)), ("192.0.2.10".into(), "203.0.113.7".into()));
        assert_eq!(client_ip_behind(&headers(&["203.0.113.7"]), 2), "203.0.113.7", "fewer proxies than configured");
        assert_eq!(client_ip_behind(&headers(&[" , "]), 1), "local");
    }

    #[test]
    fn screens_call_records_metadata_and_their_own_endpoints() {
        for path in ["/api/v1/Customer", "/api/meta/Customer", "/api/meta/contributions/ui.page", "/api/ext/board/cards"] {
            assert!(screen_may_call("board", path), "{path}");
        }
        for path in ["/api/admin/users", "/api/auth/me", "/api/apps", "/api/ext/other/x", "/api/ext/boardx/x", "/api/ext/board", "/api/v1"] {
            assert!(!screen_may_call("board", path), "{path}");
        }
    }
}
