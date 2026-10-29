//! Request extractors: authentication (JWT session or API key) → kernel `Ctx`.

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

/// The session a bearer token (session JWT or `fk_` API key) carries, and whether it may only set
/// up two-factor sign-in (the workspace requires it and the person has yet to). The ticket between
/// the two steps of signing in is no session at all.
async fn session_of(st: &crate::state::AppState, token: &str) -> Result<(ferrum_kernel::auth::Session, bool), ApiError> {
    if token.starts_with("fk_") {
        let s = st.kernel.verify_api_key(token).await?;
        st.user_limiter.check_local(&format!("{}|{}", s.tenant, s.user))?;
        return Ok((s, false));
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
    st.user_limiter.check_local(&format!("{tenant}|{user}"))?;
    Ok((ferrum_kernel::auth::Session { tenant, user, token_version: tv }, setup_only))
}

/// Resolves a bearer token (session JWT or `fk_` API key) to the caller's context.
pub async fn ctx_from_token(st: &crate::state::AppState, token: &str, request_id: String) -> Result<Ctx, ApiError> {
    let (session, setup_only) = session_of(st, token).await?;
    if setup_only {
        return Err(ApiError(Error::Forbidden("set up two-factor sign-in first; this workspace requires it".into())));
    }
    Ok(st.kernel.load_ctx(session.tenant, session.user, Some(session.token_version), request_id).await?)
}

impl FromRequestParts<AppContext> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppContext) -> Result<Self, Self::Rejection> {
        let st = app_state(state)?;
        let rid = request_id(&parts.headers);
        let token = bearer(&parts.headers).ok_or_else(|| ApiError(Error::Unauthorized("sign in required".into())))?;
        let ctx = ctx_from_token(&st, &token, rid).await?;
        match parts.headers.get("x-ferrum-app") {
            Some(app) => Ok(Auth(screen_ctx(&st, ctx, app, parts.uri.path()).await?)),
            None => Ok(Auth(ctx)),
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
        let token = bearer(&parts.headers).ok_or_else(|| ApiError(Error::Unauthorized("sign in required".into())))?;
        let (s, setup_only) = session_of(&st, &token).await?;
        let ctx = st.kernel.load_ctx(s.tenant, s.user, Some(s.token_version), rid).await?;
        Ok(SetupAuth(ctx, setup_only.then_some(s)))
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

pub fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit(',').next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "local".into())
}

#[cfg(test)]
mod tests {
    use super::*;

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
