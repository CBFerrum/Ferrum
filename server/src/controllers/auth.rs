//! Sign-in (with its second step), session info, two-factor setup, password changes, API keys.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, BrowserSession, SetupAuth, client_ip};
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
    /// `cookie`: a page that keeps its session in the cookie alone and wants no token back.
    #[serde(default)]
    session: Option<String>,
}

/// The ticket between the two steps of signing in lasts five minutes.
const TICKET_SECS: u64 = 300;
/// A session that may only set up two-factor sign-in lasts fifteen.
const SETUP_SECS: u64 = 900;

/// A signed token for a session; `extra` marks a ticket (`mfa`) or a setup-only session.
fn token(st: &AppState, s: &Session, ttl: u64, extra: Option<&str>) -> Result<String, ApiError> {
    signed(st, s, ttl, extra.map(|flag| (flag, json!(true))))
}

/// A token for a browser session: the cookie's (`sid`) or a page's bearer token bound to it
/// (`bsid`), both ending when the session does.
fn device_token(st: &AppState, s: &Session, ttl: u64, kind: &str, device: Uuid) -> Result<String, ApiError> {
    signed(st, s, ttl, Some((kind, json!(device.to_string()))))
}

fn signed(st: &AppState, s: &Session, ttl: u64, extra: Option<(&str, Value)>) -> Result<String, ApiError> {
    let mut claims = Map::new();
    claims.insert("tenant".into(), json!(s.tenant.to_string()));
    claims.insert("tv".into(), json!(s.token_version));
    if let Some((k, v)) = extra {
        claims.insert(k.into(), v);
    }
    JWT::new(&st.jwt_secret).generate_token(ttl, s.user.to_string(), claims).map_err(|e| ApiError(Error::Internal(format!("token: {e}"))))
}

/// A request made by a page of this site in a browser (Fetch Metadata), as opposed to an API
/// client: it gets a browser session in the cookie.
fn from_page(headers: &HeaderMap) -> bool {
    headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("same-origin")
}

/// A finished sign-in. API clients get a bearer token, as always. A page of this site also gets
/// a browser session in the cookie (docs/plan/apps-ui.md §12.3), and, unless it asked for the
/// cookie alone, a bearer token bound to that session for the pages that still send one.
async fn session_response(st: &AppState, s: &Session, headers: &HeaderMap, cookie_only: bool) -> ApiResult<Response> {
    if !from_page(headers) {
        if cookie_only {
            return Err(ApiError(Error::Forbidden("sign in from a page of this site".into())));
        }
        return Ok(Json(json!({"token": token(st, s, st.jwt_expiration, None)?, "token_type": "Bearer", "expires_in": st.jwt_expiration})).into_response());
    }
    st.shell.check_host(headers, s.tenant).await?;
    let ua = headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let ttl = st.shell.session_ttl;
    let sid = st.kernel.open_session(s, ttl, ua, &client_ip(headers), None).await?;
    let mut body = json!({"cookie": true, "session_expires_in": ttl.as_secs()});
    if !cookie_only {
        body["token"] = json!(device_token(st, s, st.jwt_expiration, "bsid", sid)?);
        body["token_type"] = json!("Bearer");
        body["expires_in"] = json!(st.jwt_expiration);
    }
    let mut res = Json(body).into_response();
    res.headers_mut().append(axum::http::header::SET_COOKIE, crate::shell::set_cookie(st.shell.secure, &device_token(st, s, ttl.as_secs(), "sid", sid)?, ttl));
    Ok(res)
}

async fn login(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<LoginReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|{}", client_ip(&headers), req.tenant.to_lowercase())).await?;
    if req.password.len() > 256 || req.email.len() > 254 || req.tenant.len() > 63 {
        return Err(ApiError(Error::Unauthorized("invalid tenant, email or password".into())));
    }
    match st.kernel.sign_in(&req.tenant, &req.email, &req.password).await? {
        SignIn::Session(s) => session_response(&st, &s, &headers, req.session.as_deref() == Some("cookie")).await,
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
    #[serde(default)]
    session: Option<String>,
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
    session_response(&st, &s, &headers, req.session.as_deref() == Some("cookie")).await
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
    #[serde(default)]
    session: Option<String>,
}

/// Finishes signing in with what the provider sent the browser back with.
async fn sso_finish(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<SsoFinishReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|sso", client_ip(&headers))).await?;
    if req.state.len() > 200 || req.code.len() > 4096 || req.binding.len() > 200 {
        return Err(ApiError(Error::Unauthorized("sign in again".into())));
    }
    let s = st.kernel.sso_finish(&req.state, &req.code, &req.binding).await?;
    session_response(&st, &s, &headers, req.session.as_deref() == Some("cookie")).await
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
    let st = app_state(&app)?;
    st.kernel.logout_everywhere(&ctx).await?;
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut().append(axum::http::header::SET_COOKIE, crate::shell::clear_cookie(st.shell.secure));
    Ok(res)
}

/// Signs this browser out: its session ends (with every token bound to it) and the cookie goes.
/// Without a browser session there is nothing to end; the cookie is cleared all the same.
async fn logout(State(app): State<AppContext>, method: axum::http::Method, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    if let Ok((ctx, Some(device))) = crate::extract::caller_on(&st, &method, &headers).await {
        st.kernel.end_session(ctx.tenant, device).await?;
    }
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut().append(axum::http::header::SET_COOKIE, crate::shell::clear_cookie(st.shell.secure));
    Ok(res)
}

/// A bearer token for a page of this site that still sends one (today's web app), bound to the
/// browser session in the cookie: someone signed in on the new pages is signed in there too.
async fn page_token(BrowserSession(ctx, device): BrowserSession, State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let sid = match (device, crate::shell::session_cookie(&headers, st.shell.secure)) {
        (Some(d), Some(_)) => d,
        _ => return Err(ApiError(Error::Unauthorized("only the browser's session cookie can be exchanged".into()))),
    };
    let s = Session { tenant: ctx.tenant, user: ctx.user, token_version: ctx_token_version(&st, &ctx).await? };
    let ws = st.shell.workspace(ctx.tenant).await?;
    Ok(Json(json!({
        "token": device_token(&st, &s, st.jwt_expiration, "bsid", sid)?,
        "token_type": "Bearer",
        "expires_in": st.jwt_expiration,
        "tenant": ws.slug,
        "email": ctx.email,
    }))
    .into_response())
}

/// The person's current token version (part of every token, so "sign out everywhere" works).
async fn ctx_token_version(st: &AppState, ctx: &ferrum_kernel::Ctx) -> Result<i32, ApiError> {
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    let tv: i32 = sqlx::query_scalar("select token_version from app_user where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_one(&mut *tx)
        .await
        .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(tv)
}

#[derive(Deserialize)]
struct SessionsQuery {
    user: Option<Uuid>,
}

/// The devices signed in as the caller (or, for administrators, as `user`), this one marked.
async fn sessions(BrowserSession(ctx, device): BrowserSession, State(app): State<AppContext>, Query(q): Query<SessionsQuery>) -> ApiResult<Response> {
    let rows = app_state(&app)?.kernel.list_sessions(&ctx, q.user.unwrap_or(ctx.user)).await?;
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let mut v = serde_json::to_value(&r).unwrap_or_default();
            v["current"] = json!(Some(r.id) == device);
            v
        })
        .collect();
    Ok(Json(json!({"sessions": rows})).into_response())
}

/// Signs one device out.
async fn revoke_session(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.revoke_session(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct StepUpReq {
    password: String,
    code: Option<String>,
}

/// The person proves themselves again for a sensitive action: the password, and the code from
/// their authenticator app when they use one.
async fn step_up(
    BrowserSession(ctx, device): BrowserSession,
    State(app): State<AppContext>,
    headers: HeaderMap,
    Json(req): Json<StepUpReq>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let Some(device) = device else { return Err(ApiError(Error::invalid("step-up needs a browser session"))) };
    st.login_limiter.check(&format!("{}|step-up|{}", client_ip(&headers), ctx.user)).await?;
    if req.password.len() > 256 {
        return Err(ApiError(Error::Unauthorized("wrong password".into())));
    }
    let ws = st.shell.workspace(ctx.tenant).await?;
    match st.kernel.sign_in(&ws.slug, &ctx.email, &req.password).await? {
        SignIn::Session(_) | SignIn::SetupRequired(_) => {}
        SignIn::SecondFactor(t) => {
            let code = req
                .code
                .as_deref()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| ApiError(Error::field("code", "the code from your authenticator app is needed")))?;
            st.kernel.second_factor(t.tenant, t.user, t.token_version, code).await?;
        }
    }
    st.kernel.step_up_session(ctx.tenant, device).await?;
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
        .add("/logout", post(logout))
        .add("/token", post(page_token))
        .add("/sessions", get(sessions))
        .add("/sessions/{id}", delete(revoke_session))
        .add("/step-up", post(step_up))
        .add("/password", post(change_password))
        .add("/api_keys", get(list_keys).post(create_key))
        .add("/api_keys/{id}", delete(revoke_key))
}
