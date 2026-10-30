//! Sign-in (with its second step), session info, two-factor setup, password changes, API keys.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, SetupAuth, client_ip};
use crate::state::{AppState, app_state};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::Error;
use ferrum_kernel::auth::{Session, SignIn};
use loco_rs::app::AppContext;
use loco_rs::auth::jwt::JWT;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
struct LoginReq {
    tenant: String,
    email: String,
    password: String,
}

/// The ticket between the two steps of signing in lasts five minutes.
const TICKET_SECS: u64 = 300;
/// A session that may only set up two-factor sign-in lasts fifteen.
const SETUP_SECS: u64 = 900;

/// A signed token for a session; `extra` marks a ticket (`mfa`) or a setup-only session.
fn token(st: &AppState, s: &Session, ttl: u64, extra: Option<&str>) -> Result<String, ApiError> {
    let mut claims = Map::new();
    claims.insert("tenant".into(), json!(s.tenant.to_string()));
    claims.insert("tv".into(), json!(s.token_version));
    if let Some(flag) = extra {
        claims.insert(flag.into(), json!(true));
    }
    JWT::new(&st.jwt_secret).generate_token(ttl, s.user.to_string(), claims).map_err(|e| ApiError(Error::Internal(format!("token: {e}"))))
}

fn session_response(st: &AppState, s: &Session) -> ApiResult<Response> {
    Ok(Json(json!({"token": token(st, s, st.jwt_expiration, None)?, "token_type": "Bearer", "expires_in": st.jwt_expiration})).into_response())
}

async fn login(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<LoginReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|{}", client_ip(&headers), req.tenant.to_lowercase())).await?;
    if req.password.len() > 256 || req.email.len() > 254 || req.tenant.len() > 63 {
        return Err(ApiError(Error::Unauthorized("invalid tenant, email or password".into())));
    }
    match st.kernel.sign_in(&req.tenant, &req.email, &req.password).await? {
        SignIn::Session(s) => session_response(&st, &s),
        SignIn::SecondFactor(s) => {
            Ok(Json(json!({"second_factor": true, "ticket": token(&st, &s, TICKET_SECS, Some("mfa"))?, "expires_in": TICKET_SECS})).into_response())
        }
        SignIn::SetupRequired(s) => Ok(Json(json!({
            "two_factor_setup": true,
            "token": token(&st, &s, SETUP_SECS, Some("setup_2fa"))?,
            "token_type": "Bearer",
            "expires_in": SETUP_SECS,
        }))
        .into_response()),
    }
}

#[derive(Deserialize)]
struct SecondFactorReq {
    ticket: String,
    code: String,
}

/// The second step of signing in: the ticket from the first and a code from the authenticator
/// app (or a recovery code).
async fn login_second_factor(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<SecondFactorReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|second-factor", client_ip(&headers))).await?;
    let again = || ApiError(Error::Unauthorized("sign in again".into()));
    if req.code.len() > 64 {
        return Err(ApiError(Error::field("code", "is too long")));
    }
    let data = JWT::new(&st.jwt_secret).validate(&req.ticket).map_err(|_| again())?;
    let claims = &data.claims.claims;
    if claims.get("mfa").and_then(Value::as_bool) != Some(true) {
        return Err(again());
    }
    let user: Uuid = data.claims.pid.parse().map_err(|_| again())?;
    let tenant: Uuid = claims.get("tenant").and_then(Value::as_str).and_then(|s| s.parse().ok()).ok_or_else(again)?;
    let tv = claims.get("tv").and_then(Value::as_i64).ok_or_else(again)? as i32;
    let s = st.kernel.second_factor(tenant, user, tv, &req.code).await?;
    session_response(&st, &s)
}

#[derive(Deserialize)]
struct SsoOptionsQuery {
    tenant: String,
}

/// The single sign-on buttons a workspace's sign-in page shows.
async fn sso_options(State(app): State<AppContext>, Query(q): Query<SsoOptionsQuery>) -> ApiResult<Response> {
    if q.tenant.len() > 63 {
        return Ok(Json(json!({"providers": []})).into_response());
    }
    Ok(Json(json!({"providers": app_state(&app)?.kernel.sso_sign_in_options(&q.tenant).await?})).into_response())
}

#[derive(Deserialize)]
struct SsoStartReq {
    tenant: String,
    provider: Uuid,
    redirect_uri: String,
    binding: String,
}

/// Starts signing in through a provider → `{url}` to send the browser to. The provider sends it
/// back to `redirect_uri` only if that is registered with it, and only the browser holding
/// `binding` can finish.
async fn sso_start(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<SsoStartReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|sso", client_ip(&headers))).await?;
    let url = st.kernel.sso_begin(&req.tenant, req.provider, &req.redirect_uri, &req.binding).await?;
    Ok(Json(json!({"url": url})).into_response())
}

#[derive(Deserialize)]
struct SsoFinishReq {
    state: String,
    code: String,
    binding: String,
}

/// Finishes signing in with what the provider sent the browser back with.
async fn sso_finish(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<SsoFinishReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|sso", client_ip(&headers))).await?;
    if req.state.len() > 200 || req.code.len() > 4096 || req.binding.len() > 200 {
        return Err(ApiError(Error::Unauthorized("sign in again".into())));
    }
    let s = st.kernel.sso_finish(&req.state, &req.code, &req.binding).await?;
    session_response(&st, &s)
}

async fn me(SetupAuth(ctx, setup_only): SetupAuth) -> ApiResult<Response> {
    Ok(Json(json!({
        "tenant": ctx.tenant,
        "user": {"id": ctx.user, "email": ctx.email, "full_name": ctx.full_name, "is_admin": ctx.is_admin, "roles": ctx.roles},
        "default_company": ctx.default_company,
        "scopes": ctx.scopes,
        "fiscal_year_start_month": ctx.fy_start_month,
        "timezone": ctx.schema.timezone,
        "today": ctx.today(),
        "packages": ctx.schema.packages,
        "two_factor_setup": setup_only.is_some(),
    }))
    .into_response())
}

async fn two_factor_status(SetupAuth(ctx, _): SetupAuth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.two_factor_status(&ctx).await?).into_response())
}

async fn two_factor_begin(SetupAuth(ctx, _): SetupAuth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.begin_two_factor(&ctx).await?).into_response())
}

#[derive(Deserialize)]
struct CodeReq {
    code: String,
}

/// Turns two-factor sign-in on; a session that could only set it up gets a full one.
async fn two_factor_enable(SetupAuth(ctx, setup_only): SetupAuth, State(app): State<AppContext>, Json(req): Json<CodeReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let codes = st.kernel.enable_two_factor(&ctx, &req.code).await?;
    let mut out = json!({"recovery_codes": codes});
    if let Some(s) = setup_only {
        out["token"] = json!(token(&st, &s, st.jwt_expiration, None)?);
        out["expires_in"] = json!(st.jwt_expiration);
    }
    Ok(Json(out).into_response())
}

async fn two_factor_recovery_codes(Auth(ctx): Auth, State(app): State<AppContext>, Json(req): Json<CodeReq>) -> ApiResult<Response> {
    Ok(Json(json!({"recovery_codes": app_state(&app)?.kernel.new_recovery_codes(&ctx, &req.code).await?})).into_response())
}

#[derive(Deserialize)]
struct DisableReq {
    password: Option<String>,
    code: Option<String>,
}

async fn two_factor_disable(Auth(ctx): Auth, State(app): State<AppContext>, Json(req): Json<DisableReq>) -> ApiResult<Response> {
    app_state(&app)?.kernel.disable_two_factor(&ctx, req.password.as_deref(), req.code.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn logout_all(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    app_state(&app)?.kernel.logout_everywhere(&ctx).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct PasswordReq {
    current_password: Option<String>,
    new_password: String,
}

async fn change_password(Auth(ctx): Auth, State(app): State<AppContext>, Json(req): Json<PasswordReq>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_password(&ctx, ctx.user, req.current_password.as_deref(), &req.new_password).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct KeyReq {
    name: String,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `dav`: an app password for calendar and contacts apps; none: a key to the whole API.
    scope: Option<String>,
}

async fn create_key(Auth(ctx): Auth, State(app): State<AppContext>, Json(req): Json<KeyReq>) -> ApiResult<Response> {
    if req.name.trim().is_empty() || req.name.len() > 100 {
        return Err(ApiError(Error::field("name", "is required (max 100 characters)")));
    }
    let (id, key) = app_state(&app)?.kernel.create_api_key(&ctx, ctx.user, req.name.trim(), req.expires_at, req.scope.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id, "key": key, "note": "Store this key now; it cannot be shown again."}))).into_response())
}

async fn list_keys(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<(
        Uuid,
        String,
        String,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
    )> = sqlx::query_as(
        "select id, name, prefix, created_at, expires_at, revoked_at, last_used_at, scope from api_key where tenant_id = $1 and user_id = $2 order by created_at desc",
    )
    .bind(ctx.tenant)
    .bind(ctx.user)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    let keys: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, prefix, created, expires, revoked, used, scope)| {
            json!({"id": id, "name": name, "prefix": format!("fk_{prefix}_…"), "created_at": created, "expires_at": expires, "revoked_at": revoked, "last_used_at": used, "scope": scope})
        })
        .collect();
    Ok(Json(json!({"keys": keys})).into_response())
}

async fn revoke_key(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.revoke_api_key(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{delete, get, post};
    Routes::new()
        .prefix("/api/auth")
        .add("/login", post(login))
        .add("/login/second-factor", post(login_second_factor))
        .add("/sso", get(sso_options))
        .add("/sso/start", post(sso_start))
        .add("/sso/finish", post(sso_finish))
        .add("/me", get(me))
        .add("/2fa", get(two_factor_status))
        .add("/2fa/begin", post(two_factor_begin))
        .add("/2fa/enable", post(two_factor_enable))
        .add("/2fa/disable", post(two_factor_disable))
        .add("/2fa/recovery-codes", post(two_factor_recovery_codes))
        .add("/logout_all", post(logout_all))
        .add("/password", post(change_password))
        .add("/api_keys", get(list_keys).post(create_key))
        .add("/api_keys/{id}", delete(revoke_key))
}
