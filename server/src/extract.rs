//! Request extractors: authentication (a bearer token — session JWT or API key — or the
//! browser's session cookie) → kernel `Ctx`.

use crate::error::ApiError;
use crate::state::app_state;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue};
use ferrum_kernel::zones::{Credential, Entry};
use ferrum_kernel::{Ctx, Error};
use loco_rs::app::AppContext;
use loco_rs::auth::jwt::JWT;
use std::collections::{BTreeMap, BTreeSet};
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
    /// A records key's grants (`records:…`): it works only through [`Auth`], on records and their
    /// metadata, narrowed to them.
    records: Option<BTreeMap<String, BTreeSet<String>>>,
    /// What kind of credential it is (zones count refusals by it).
    credential: Credential,
    /// When the browser session behind it last proved who its person is (crates/kernel/src/controls.rs).
    step_up_at: Option<chrono::DateTime<chrono::Utc>>,
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
/// `active`: whether this use is the person's activity, which keeps a browser session from going idle
/// (a request is; a socket's recheck is not).
async fn session_of(st: &crate::state::AppState, token: &str, address: &str, active: bool) -> Result<TokenSession, ApiError> {
    if token.starts_with("fk_") {
        let (s, scope) = st.kernel.verify_api_key_from(token, address).await?;
        // Support keys go through like anyone's: the role behind them decides what they reach.
        let records = match scope.as_deref() {
            None | Some("support") => None,
            Some(named) => Some(ferrum_kernel::auth::records_scope(named).ok_or_else(|| {
                ApiError(Error::Unauthorized("this key has a single purpose (calendar and contacts apps, or an identity provider) and works only there".into()))
            })?),
        };
        st.user_limiter.check_local(&format!("{}|{}", s.tenant, s.user))?;
        return Ok(TokenSession {
            session: s,
            setup_only: false,
            sid: None,
            bound: None,
            app: None,
            records,
            credential: Credential::ApiKey,
            step_up_at: None,
        });
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
    let mut step_up_at = None;
    if let Some(device) = sid.or(bound) {
        step_up_at = st.kernel.check_session_as(tenant, user, device, active).await?.step_up_at;
    }
    let app = match claims.get("app") {
        None => None,
        Some(v) => Some(v.as_str().filter(|a| crate::appmgr::valid_id(a)).ok_or_else(|| ApiError(Error::Unauthorized("invalid session".into())))?.to_string()),
    };
    let credential = if app.is_some() {
        Credential::AppToken
    } else if claims.contains_key("client_id") {
        Credential::OAuth
    } else if sid.is_some() {
        Credential::Cookie
    } else {
        Credential::Token
    };
    Ok(TokenSession {
        session: ferrum_kernel::auth::Session { tenant, user, token_version: tv },
        setup_only,
        sid,
        bound,
        app,
        records: None,
        credential,
        step_up_at,
    })
}

/// Who a request's credential names, from its token alone (signature and expiry; no database, nothing counted): the
/// workspace, the person and the browser session behind it, if any. Only for saying more about an answer the request
/// was already admitted for (a step-up refusal's ways); never to admit anything. Keys and tickets name nobody here.
pub(crate) fn token_person(st: &crate::state::AppState, headers: &HeaderMap) -> Option<(Uuid, Uuid, Option<Uuid>)> {
    let token = bearer(headers).or_else(|| crate::shell::session_cookie(headers, st.shell.secure))?;
    if token.starts_with("fk_") {
        return None;
    }
    let data = JWT::new(&st.jwt_secret).validate(&token).ok()?;
    let claims = &data.claims.claims;
    if claims.contains_key("mfa") {
        return None;
    }
    let tenant = claims.get("tenant")?.as_str()?.parse().ok()?;
    let device = ["sid", "bsid"].iter().find_map(|k| claims.get(*k)?.as_str()?.parse().ok());
    Some((tenant, data.claims.pid.parse().ok()?, device))
}

/// App tokens work only for their app's calls to the API ([`Auth`]), and records keys only for
/// records; everything else refuses them.
fn no_app_token(t: &TokenSession) -> Result<(), ApiError> {
    if t.records.is_some() {
        return Err(ApiError(Error::Unauthorized(RECORDS_ONLY.into())));
    }
    match t.app {
        Some(_) => Err(ApiError(Error::Unauthorized("an app token works only for its app's calls to the API".into()))),
        None => Ok(()),
    }
}

/// Support access goes through [`Auth`], where the customer sees it request by request. Anywhere else
/// it is refused, and the attempt is on their list as well.
async fn only_through_the_api(st: &crate::state::AppState, ctx: &Ctx, method: &str) -> Result<(), ApiError> {
    if !ctx.email.ends_with(ferrum_kernel::support::SUPPORT_DOMAIN) {
        return Ok(());
    }
    if let Some((grant, _)) = st.kernel.support_level(ctx).await? {
        st.kernel.support_tried(ctx, grant, method, "outside the API", true).await?;
    }
    Err(ApiError(Error::Forbidden("support access works only through the API".into())))
}

/// Resolves a token (session JWT, cookie token or `fk_` API key) to the caller's context. Sockets
/// use it for both kinds, in the zone their upgrade request came in by (`entry`); HTTP requests go
/// through [`Auth`], which also checks where each kind may come from. A socket is not activity of its
/// own: `active` only when the person did something through it (a co-editing change).
pub async fn ctx_from_token(st: &crate::state::AppState, token: &str, request_id: String, entry: &Arc<Entry>, active: bool) -> Result<Ctx, ApiError> {
    let t = session_of(st, token, &entry.address, active).await?;
    no_app_token(&t)?;
    if t.setup_only {
        return Err(ApiError(Error::Forbidden("set up two-factor sign-in first; this workspace requires it".into())));
    }
    // A key on a socket is a machine's, as anywhere: a workspace's staff networks do not hold it (its own do).
    let keyed;
    let entry = if t.credential == Credential::ApiKey {
        keyed = Arc::new(Entry { credential: Credential::ApiKey, ..(**entry).clone() });
        &keyed
    } else {
        entry
    };
    // A Ferrum account (the directory's) has no live channel, documents or apps.
    if st.kernel.directory().await? == Some(t.session.tenant) {
        return Err(ApiError(Error::Unauthorized("this sign-in works only on the accounts host".into())));
    }
    let ctx = st.kernel.request_ctx(t.session.tenant, t.session.user, Some(t.session.token_version), request_id, entry).await?.proved(t.step_up_at);
    only_through_the_api(st, &ctx, "SOCKET").await?;
    Ok(ctx)
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
    let (ctx, device, app, records) = caller_with_app(st, method, headers, None).await?;
    if app.is_some() {
        return Err(ApiError(Error::Unauthorized("an app token works only for its app's calls to the API".into())));
    }
    if records.is_some() {
        return Err(ApiError(Error::Unauthorized(RECORDS_ONLY.into())));
    }
    only_through_the_api(st, &ctx, method.as_str()).await?;
    Ok((ctx, device))
}

/// What a records key may not reach.
const RECORDS_ONLY: &str = "this key reaches records and their metadata only, through the records API";

/// [`caller_on`], where an app token is welcome: `.2` names its app and `.3` holds a records key's
/// grants (the caller narrows to them).
/// With a `place`, the request also waits for its turn among its workspace's, before anything of the
/// workspace's is read.
async fn caller_with_app(
    st: &crate::state::AppState,
    method: &axum::http::Method,
    headers: &HeaderMap,
    place: Option<&crate::state::Place>,
) -> Result<(Ctx, Option<Uuid>, Option<String>, Option<BTreeMap<String, BTreeSet<String>>>), ApiError> {
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
    let t = session_of(st, &token, &client_ip(headers), true).await?;
    let mismatch = if from_cookie { t.sid.is_none() || t.bound.is_some() } else { t.sid.is_some() };
    if mismatch {
        return Err(ApiError(Error::Unauthorized(
            if from_cookie { "invalid session cookie" } else { "this token works only as the browser's session cookie" }.into(),
        )));
    }
    if t.setup_only {
        return Err(ApiError(Error::Forbidden("set up two-factor sign-in first; this workspace requires it".into())));
    }
    if let Some(place) = place {
        st.shares.take(place, t.session.tenant).await?;
    }
    if from_cookie {
        st.shell.check_host(headers, t.session.tenant).await?;
    } else {
        st.shell.check_directory(headers, t.session.tenant).await?;
    }
    let entry = crate::zone::entry(st, headers, t.credential).await;
    let ctx = st.kernel.request_ctx(t.session.tenant, t.session.user, Some(t.session.token_version), rid, &entry).await?.proved(t.step_up_at);
    Ok((ctx, t.device(), t.app, t.records))
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
/// asking is on this host (`Origin`). `None`: the client sends `{"auth": token}` first. Either way
/// in the zone of `entry`, the upgrade request's ([`socket_entry`]).
pub async fn socket_credential(st: &crate::state::AppState, headers: &HeaderMap, entry: &Arc<Entry>) -> Result<Option<(String, Ctx)>, ApiError> {
    let rid = request_id(headers);
    if let Some(token) = bearer(headers) {
        let ctx = ctx_from_token(st, &token, rid, entry, false).await?;
        return Ok(Some((token, ctx)));
    }
    match crate::shell::session_cookie(headers, st.shell.secure) {
        Some(token) if crate::shell::origin_is_host(headers) => {
            let t = session_of(st, &token, &entry.address, false).await?;
            no_app_token(&t)?;
            if t.sid.is_none() || t.bound.is_some() || t.setup_only {
                return Err(ApiError(Error::Unauthorized("invalid session cookie".into())));
            }
            st.shell.check_host(headers, t.session.tenant).await?;
            let ctx = st.kernel.request_ctx(t.session.tenant, t.session.user, Some(t.session.token_version), rid, entry).await?.proved(t.step_up_at);
            Ok(Some((token, ctx)))
        }
        _ => Ok(None),
    }
}

/// How a socket's upgrade request came in: its credential, whichever it turns out to be, is checked
/// in this zone, at the upgrade and every time it is checked again.
pub async fn socket_entry(st: &crate::state::AppState, headers: &HeaderMap) -> Arc<Entry> {
    crate::zone::entry(st, headers, Credential::Socket).await
}

impl FromRequestParts<AppContext> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppContext) -> Result<Self, Self::Rejection> {
        let st = app_state(state)?;
        let place = parts.extensions.get::<crate::state::Place>().cloned();
        let (mut ctx, _, token_app, records) = caller_with_app(&st, &parts.method, &parts.headers, place.as_ref()).await?;
        // Administration where the zone keeps administrators out (crate::zones, level 2): refused as level 1 refuses,
        // naming where the whole workspace opens. Nobody is an administrator there, so nothing else is lost.
        if parts.uri.path().starts_with("/api/admin/") && !ctx.is_admin && ctx.zone.as_ref().is_some_and(|e| e.keeps_admins_out()) {
            return Err(ApiError(st.kernel.zone_refusal(ctx.tenant, &st.zones).await));
        }
        // A records key: its owner's rights narrowed to what it names, on records and metadata only.
        if let Some(grants) = records {
            if !records_may_call(parts.uri.path()) {
                return Err(ApiError(Error::Forbidden(RECORDS_ONLY.into())));
            }
            ctx.perms = Arc::new(ctx.perms.limited_to(&grants, &BTreeSet::new()));
            ctx.is_admin = false;
        }
        // Support access is seen by the customer, request by request (crates/kernel/src/support.rs).
        if ctx.email.ends_with(ferrum_kernel::support::SUPPORT_DOMAIN) {
            let (grant, level) = st.kernel.support_level(&ctx).await?.ok_or_else(|| ApiError(Error::Unauthorized("this support access has ended".into())))?;
            let refused = !ferrum_kernel::support::reaches_records(&level) && parts.uri.path() != "/api/support/report";
            st.kernel.support_tried(&ctx, grant, parts.method.as_str(), parts.uri.path(), refused).await?;
            if refused {
                return Err(ApiError(Error::Forbidden(format!("{level} access reaches no records"))));
            }
        }
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
        let t = session_of(&st, &token, &client_ip(&parts.headers), true).await?;
        no_app_token(&t)?;
        if t.sid.is_some() {
            return Err(ApiError(Error::Unauthorized("this token works only as the browser's session cookie".into())));
        }
        let entry = crate::zone::entry(&st, &parts.headers, t.credential).await;
        let ctx = st.kernel.request_ctx(t.session.tenant, t.session.user, Some(t.session.token_version), rid, &entry).await?.proved(t.step_up_at);
        only_through_the_api(&st, &ctx, parts.method.as_str()).await?;
        Ok(SetupAuth(ctx, t.setup_only.then_some(t.session)))
    }
}

/// Where a records key may send requests: records and their metadata.
pub(crate) fn records_may_call(path: &str) -> bool {
    path.starts_with("/api/v1/") || path.starts_with("/api/meta/")
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
    fn records_keys_call_records_and_metadata() {
        assert!(records_may_call("/api/v1/Customer") && records_may_call("/api/meta/Customer"));
        for path in ["/api/admin/users", "/api/auth/api_keys", "/api/ext/board/cards", "/api/reports/receivables", "/api/v1", "/api/apps"] {
            assert!(!records_may_call(path), "{path}");
        }
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
