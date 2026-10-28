//! Request extractors: authentication (JWT session or API key) → kernel `Ctx`.

use crate::error::ApiError;
use crate::state::app_state;
use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use ferrum_kernel::{Ctx, Error};
use loco_rs::app::AppContext;
use loco_rs::auth::jwt::JWT;
use uuid::Uuid;

/// An authenticated caller.
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

/// Resolves a bearer token (session JWT or `fk_` API key) to the caller's context.
pub async fn ctx_from_token(st: &crate::state::AppState, token: &str, request_id: String) -> Result<Ctx, ApiError> {
    let session = if token.starts_with("fk_") {
        st.kernel.verify_api_key(token).await?
    } else {
        let data =
            JWT::new(&st.jwt_secret).validate(token).map_err(|_| ApiError(Error::Unauthorized("session expired or invalid; please sign in again".into())))?;
        let user: Uuid = data.claims.pid.parse().map_err(|_| ApiError(Error::Unauthorized("invalid session".into())))?;
        let tenant: Uuid = data
            .claims
            .claims
            .get("tenant")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ApiError(Error::Unauthorized("invalid session".into())))?;
        let tv = data.claims.claims.get("tv").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
        ferrum_kernel::auth::Session { tenant, user, token_version: tv }
    };
    Ok(st.kernel.load_ctx(session.tenant, session.user, Some(session.token_version), request_id).await?)
}

impl FromRequestParts<AppContext> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppContext) -> Result<Self, Self::Rejection> {
        let st = app_state(state)?;
        let rid = request_id(&parts.headers);
        let token = bearer(&parts.headers).ok_or_else(|| ApiError(Error::Unauthorized("sign in required".into())))?;
        Ok(Auth(ctx_from_token(&st, &token, rid).await?))
    }
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
