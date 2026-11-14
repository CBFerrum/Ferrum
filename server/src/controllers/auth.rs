//! Sign-in (with its second step), session info, two-factor setup, password changes, API keys.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, BrowserSession, SetupAuth, client_ip};
use crate::state::{AppState, app_state};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
use ferrum_kernel::auth::{Session, SignIn};
use ferrum_kernel::identity::DIRECTORY;
use ferrum_kernel::portal_signup::SignupRequest;
use ferrum_kernel::shell::StepUp;
use ferrum_kernel::{Error, Kernel};
use loco_rs::app::AppContext;
use loco_rs::auth::jwt::JWT;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
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
pub(crate) fn device_token(st: &AppState, s: &Session, ttl: u64, kind: &str, device: Uuid) -> Result<String, ApiError> {
    signed(st, s, ttl, Some((kind, json!(device.to_string()))))
}

pub(crate) fn signed(st: &AppState, s: &Session, ttl: u64, extra: Option<(&str, Value)>) -> Result<String, ApiError> {
    let mut claims = Map::new();
    claims.insert("tenant".into(), json!(s.tenant.to_string()));
    claims.insert("tv".into(), json!(s.token_version));
    if let Some((k, v)) = extra {
        claims.insert(k.into(), v);
    }
    JWT::new(&st.jwt_secret).generate_token(ttl, s.user.to_string(), claims).map_err(|e| ApiError(Error::Internal(format!("token: {e}"))))
}

/// How long an app token lasts (docs/plan/apps-ui.md §12.3): its page asks for another before then.
pub(crate) const APP_TOKEN_SECS: u64 = 300;

/// An app token: the API as `app` for a few minutes, the person's rights narrowed to what the app
/// declared (extract.rs), bound to the browser session `device` (signing it out ends the token).
pub(crate) fn app_token(st: &AppState, s: &Session, device: Uuid, app: &str) -> Result<String, ApiError> {
    let mut claims = Map::new();
    claims.insert("tenant".into(), json!(s.tenant.to_string()));
    claims.insert("tv".into(), json!(s.token_version));
    claims.insert("bsid".into(), json!(device.to_string()));
    claims.insert("app".into(), json!(app));
    JWT::new(&st.jwt_secret).generate_token(APP_TOKEN_SECS, s.user.to_string(), claims).map_err(|e| ApiError(Error::Internal(format!("token: {e}"))))
}

/// A request made by a page of this site in a browser (Fetch Metadata), as opposed to an API
/// client: it gets a browser session in the cookie.
fn from_page(headers: &HeaderMap) -> bool {
    headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("same-origin")
}

/// The accounts host's origin when this request's host hands sign-in to it: every host but a shared
/// one (shape A′), where each workspace's people keep signing in with its own credentials, since no
/// accounts host hands a sign-in to a host that serves several workspaces.
pub(crate) async fn accounts_for(st: &AppState, headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(origin) = st.shell.accounts_origin(headers) else { return Ok(None) };
    Ok((!matches!(st.shell.place(headers).await?, crate::shell::Place::Any)).then_some(origin))
}

/// A finished sign-in. API clients get a bearer token, as always. A page of this site also gets
/// a browser session in the cookie (docs/plan/apps-ui.md §12.3), and, unless it asked for the
/// cookie alone, a bearer token bound to that session for the pages that still send one.
/// `method` names how they signed in, for the audit trail (`password`, `passkey`, `sso`, `code`, `accounts`).
pub(crate) async fn session_response(st: &AppState, s: &Session, headers: &HeaderMap, cookie_only: bool, method: &str) -> ApiResult<Response> {
    session_response_with(st, s, headers, cookie_only, method, None).await
}

/// [`session_response`], keeping on the browser session what a SAML sign-in's logout needs.
async fn session_response_with(st: &AppState, s: &Session, headers: &HeaderMap, cookie_only: bool, method: &str, saml: Option<&Value>) -> ApiResult<Response> {
    // Zones (crate::zone): every way of signing in ends here, so a staff account gets no session and
    // no token where its zone admits no staff; refused as a wrong password is, before anything below
    // could say more about the account.
    let entry = crate::zone::sign_in(st, headers, s.tenant, s.user).await.map_err(|()| crate::zone::sign_in_failed())?;
    // No browser session starts on an address in its leaving period (its sign-in page sends people on to
    // the new one); refused as a wrong password is. API clients' tokens are not the host's, and go on as before.
    if from_page(headers) && st.shell.leaving(headers).await?.is_some() {
        return Err(crate::zone::sign_in_failed());
    }
    // Shapes B and C: someone whose workspace account joined their Ferrum account signs in at the
    // accounts host, with that account's credentials; the workspace's own, unused since, open
    // nothing (for pages and API clients alike).
    if let Some(accounts) = accounts_for(st, headers).await?
        && st.kernel.directory().await? != Some(s.tenant)
        && st.kernel.identity_of(s.tenant, s.user).await?.is_some()
    {
        return Err(ApiError(Error::Forbidden(format!("this account signs in with its Ferrum account, at {accounts}"))));
    }
    st.kernel.record_sign_in(s.tenant, s.user, "signed_in", method, &crate::zone::arrival(st, headers).await).await?;
    if !from_page(headers) {
        if cookie_only {
            return Err(ApiError(Error::Forbidden("sign in from a page of this site".into())));
        }
        return Ok(Json(json!({"token": token(st, s, st.jwt_expiration, None)?, "token_type": "Bearer", "expires_in": st.jwt_expiration})).into_response());
    }
    st.shell.check_host(headers, s.tenant).await?;
    let ua = headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let ttl = st.shell.session_ttl;
    let sid = st.kernel.open_session_with(s, ttl, ua, &client_ip(headers), None, entry.name(), saml).await?;
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
    // The directory's people sign in on the accounts host's own page.
    if req.password.len() > 256 || req.email.len() > 254 || req.tenant.len() > 63 || req.tenant.trim().eq_ignore_ascii_case(DIRECTORY) {
        return Err(ApiError(Error::Unauthorized("invalid tenant, email or password".into())));
    }
    // Where staff are refused (by zone, or the workspace's networks), a staff account is one that does not exist (crate::zone).
    let from = crate::zone::arrival(&st, &headers).await;
    match st.kernel.sign_in_at(&req.tenant, &req.email, &req.password, from).await? {
        SignIn::Session(s) => session_response(&st, &s, &headers, req.session.as_deref() == Some("cookie"), "password").await,
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
    // A ticket made where staff are admitted, brought here: refused before its code is checked.
    if crate::zone::refuses_sign_in(&st, &headers, tenant, user).await {
        return Err(again());
    }
    let from = crate::zone::arrival(&st, &headers).await;
    let s = st.kernel.second_factor_from(tenant, user, tv, &req.code, &from).await?;
    st.kernel.record_sign_in(tenant, user, "second_step", "authenticator", &from).await?;
    session_response(&st, &s, &headers, req.session.as_deref() == Some("cookie"), "password").await
}

#[derive(Deserialize)]
struct SendCodeReq {
    tenant: String,
    email: String,
}

/// The least an answer to "email me a code" takes, so how long it takes says nothing about the
/// address either.
const CODE_ANSWER: std::time::Duration = std::time::Duration::from_millis(600);

/// Emails a portal person a code to sign in with (docs/plan/apps-ui.md §9.1). The answer is the
/// same whether or not the address belongs to anyone.
async fn send_code(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<SendCodeReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|{}", client_ip(&headers), req.tenant.to_lowercase())).await?;
    if req.tenant.len() > 63 || req.tenant.trim().eq_ignore_ascii_case(DIRECTORY) {
        return Err(ApiError(Error::field("tenant", "names no workspace")));
    }
    let until = tokio::time::Instant::now() + CODE_ANSWER;
    let sent = st.kernel.send_sign_in_code(&req.tenant, &req.email).await;
    tokio::time::sleep_until(until).await;
    sent?;
    Ok(Json(json!({"sent": true})).into_response())
}

/// `POST /api/auth/code/sign-up {tenant, role, name, email, passes?}`: someone outside signs themselves up to a portal
/// role that takes it, and is mailed a code to confirm at `/code/sign-in`. `{"sent": true}` whether or not the address
/// is someone's; limited per address and per network address.
pub async fn sign_up_with_code(k: &Kernel, req: &SignupRequest, network: &str) -> ApiResult<Value> {
    if req.tenant.len() > 63 || req.tenant.trim().eq_ignore_ascii_case(DIRECTORY) {
        return Err(ApiError(Error::field("tenant", "names no workspace")));
    }
    let until = tokio::time::Instant::now() + CODE_ANSWER;
    let sent = k.request_portal_signup(req, network).await;
    tokio::time::sleep_until(until).await;
    sent?;
    Ok(json!({"sent": true}))
}

async fn sign_up(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<SignupRequest>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(sign_up_with_code(&st.kernel, &req, &client_ip(&headers)).await?).into_response())
}

#[derive(Deserialize)]
struct CodeSignInReq {
    tenant: String,
    email: String,
    code: String,
    #[serde(default)]
    session: Option<String>,
}

/// Signs in with an emailed code; the second step follows for someone with an authenticator app.
async fn sign_in_with_code(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<CodeSignInReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|{}", client_ip(&headers), req.tenant.to_lowercase())).await?;
    if req.tenant.len() > 63 || req.email.len() > 254 || req.code.len() > 32 {
        return Err(ApiError(Error::Unauthorized("that code is wrong or has expired; ask for a new one".into())));
    }
    match st.kernel.sign_in_with_code(&req.tenant, &req.email, &req.code).await? {
        SignIn::SecondFactor(s) => {
            Ok(Json(json!({"second_factor": true, "ticket": token(&st, &s, TICKET_SECS, Some("mfa"))?, "expires_in": TICKET_SECS})).into_response())
        }
        SignIn::Session(s) | SignIn::SetupRequired(s) => session_response(&st, &s, &headers, req.session.as_deref() == Some("cookie"), "code").await,
    }
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
    let (s, saml) = st.kernel.sso_finish_full(&req.state, &req.code, &req.binding).await?;
    session_response_with(&st, &s, &headers, req.session.as_deref() == Some("cookie"), "sso", saml.as_ref()).await
}

/// `GET /api/auth/saml/{workspace}`: the workspace's SAML metadata, for identity providers to load:
/// the addresses on the accounts host when asked there, else PUBLIC_URL's.
async fn saml_metadata(State(app): State<AppContext>, headers: HeaderMap, Path(tenant): Path<Uuid>) -> ApiResult<Response> {
    let xml = app_state(&app)?.kernel.saml_metadata_at(tenant, crate::shell::request_host(&headers)).await?;
    Ok(([(header::CONTENT_TYPE, "application/samlmetadata+xml"), (header::CACHE_CONTROL, "no-cache")], xml).into_response())
}

/// `POST /api/auth/saml/{workspace}/acs`: a SAML provider posts its answer here (HTTP-POST binding).
/// A good one sends the browser on to finish signing in where it started; a refused one is said
/// plainly, on a page with the way back.
async fn saml_acs(
    State(app): State<AppContext>,
    headers: HeaderMap,
    Path(tenant): Path<Uuid>,
    Form(form): Form<HashMap<String, String>>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|sso", client_ip(&headers))).await?;
    let (relay, response) = (form.get("RelayState").map(String::as_str).unwrap_or_default(), form.get("SAMLResponse").map(String::as_str).unwrap_or_default());
    // Signing in again to confirm it is you (a step-up) happens in a small window, which a page of its own ends.
    let step_up = relay.starts_with(ferrum_kernel::sso::STEP_UP_STATE);
    let refused = |why: &str| {
        if step_up {
            return step_up_refused(why);
        }
        let mut page = crate::shell::status_page(StatusCode::UNAUTHORIZED, "Single sign-on did not work", why, &[("Sign in", "/app/login")], None);
        page.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        page
    };
    if relay.is_empty() || relay.len() > 80 || response.is_empty() {
        return Ok(refused("The identity provider's answer is incomplete; sign in again."));
    }
    // A browser comes here, not a program: whatever happens is a page.
    Ok(match st.kernel.saml_acs_from(tenant, relay, response, &crate::zone::arrival(&st, &headers).await).await {
        Ok(next) => (StatusCode::SEE_OTHER, [(header::LOCATION, next.as_str()), (header::CACHE_CONTROL, "no-store")]).into_response(),
        Err(Error::Validation(fields)) => refused(&fields.iter().map(|f| f.message.as_str()).collect::<Vec<_>>().join("; ")),
        Err(e @ (Error::Db(_) | Error::Internal(_))) => {
            tracing::error!("a SAML answer: {e}");
            refused("Something went wrong on the server; try again.")
        }
        Err(e) => refused(&e.to_string()),
    })
}

/// Where a single logout sends the browser next: to the provider (a redirect, or a page that posts
/// the form), or back where the person signed out from.
fn slo_step(step: ferrum_kernel::saml_logout::SloStep) -> Response {
    use ferrum_kernel::saml_logout::{Outgoing, SloStep};
    let to = match step {
        SloStep::Send(Outgoing { url, fields: Some(fields) }) => return post_page(&url, &fields),
        SloStep::Send(out) => out.url,
        SloStep::Back(url) => url,
    };
    match HeaderValue::from_str(&to) {
        Ok(location) => (StatusCode::SEE_OTHER, [(header::LOCATION, location), (header::CACHE_CONTROL, HeaderValue::from_static("no-store"))]).into_response(),
        Err(_) => slo_refused(),
    }
}

/// A page that posts `fields` to `url` (the HTTP-POST binding), by itself where scripts run and with
/// a button where they do not; its one script allowed by its digest.
fn post_page(url: &str, fields: &[(String, String)]) -> Response {
    use crate::shell::esc;
    use base64::Engine as _;
    use sha2::Digest as _;
    const SCRIPT: &str = "document.forms[0].submit()";
    let inputs: String = fields.iter().map(|(k, v)| format!(r#"<input type="hidden" name="{}" value="{}">"#, esc(k), esc(v))).collect();
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Signing out · Ferrum</title></head><body><form method="post" action="{}">{inputs}<noscript><button type="submit">Continue</button></noscript></form><script>{SCRIPT}</script></body></html>"#,
        esc(url)
    );
    let origin = reqwest::Url::parse(url).map(|u| u.origin().ascii_serialization()).unwrap_or_else(|_| "'none'".into());
    let digest = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(SCRIPT.as_bytes()));
    let mut page = crate::shell::page_response(StatusCode::OK, html, &format!("default-src 'none'; script-src 'sha256-{digest}'; form-action {origin}"));
    page.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    page
}

/// A logout message refused, or a sign-out that cannot go on to the provider: said plainly, the same
/// for every reason.
fn slo_refused() -> Response {
    let mut page = crate::shell::status_page(
        StatusCode::BAD_REQUEST,
        "Sign-out not confirmed",
        "The sign-out could not go through the identity provider, so nothing was done there. If you were signing out of Ferrum, you are signed out here; to be sure you are signed out at the identity provider, sign out there.",
        &[("Sign in", "/app/login")],
        None,
    );
    page.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    page
}

#[derive(Deserialize)]
struct TicketQuery {
    #[serde(default)]
    ticket: String,
}

/// `GET /api/auth/saml/{workspace}/logout?ticket=…`: the address `POST /logout` gave, once: to the
/// provider with a signed logout request. Not served where the zone admits no staff.
async fn saml_logout_out(State(app): State<AppContext>, headers: HeaderMap, Path(tenant): Path<Uuid>, Query(q): Query<TicketQuery>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    if crate::zone::refuses_staff(&st, &headers).await {
        return Err(ApiError(Error::NotFound("no such address".into())));
    }
    st.login_limiter.check(&format!("{}|sso-logout", client_ip(&headers))).await?;
    if q.ticket.is_empty() || q.ticket.len() > 200 {
        return Ok(slo_refused());
    }
    Ok(match st.kernel.saml_logout_send(tenant, &q.ticket).await {
        Ok(step) => slo_step(step),
        Err(e) => {
            if matches!(e, Error::Db(_) | Error::Internal(_)) {
                tracing::error!("a SAML logout: {e}");
            }
            slo_refused()
        }
    })
}

/// `GET|POST /api/auth/saml/{workspace}/slo`: the workspace's logout service, where a SAML provider
/// sends its logout requests and its answers to ours (HTTP-Redirect or HTTP-POST), by the browser,
/// with no Ferrum cookie needed. Not served where the zone admits no staff.
async fn saml_slo(
    State(app): State<AppContext>,
    headers: HeaderMap,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path(tenant): Path<Uuid>,
    Form(form): Form<HashMap<String, String>>,
) -> ApiResult<Response> {
    use ferrum_kernel::saml_logout::Incoming;
    let st = app_state(&app)?;
    if crate::zone::refuses_staff(&st, &headers).await {
        return Err(ApiError(Error::NotFound("no such address".into())));
    }
    st.login_limiter.check(&format!("{}|sso-logout", client_ip(&headers))).await?;
    let incoming = match (method == axum::http::Method::POST, form.get("SAMLRequest").or(form.get("SAMLResponse"))) {
        (true, Some(message)) => Incoming::Post { message, relay_state: form.get("RelayState").map(String::as_str) },
        (false, _) => Incoming::Redirect(uri.query().unwrap_or_default()),
        (true, None) => return Ok(slo_refused()),
    };
    Ok(match st.kernel.saml_slo(tenant, incoming).await {
        Ok(step) => slo_step(step),
        Err(e) => {
            if matches!(e, Error::Db(_) | Error::Internal(_)) {
                tracing::error!("a SAML logout message: {e}");
            }
            slo_refused()
        }
    })
}

/// `POST /api/auth/sso/{provider}/backchannel-logout`: a provider says someone signed out there. The
/// answers are the standard's: 200 when done, 400 with `invalid_request` when the token is refused.
async fn sso_backchannel_logout(
    State(app): State<AppContext>,
    headers: HeaderMap,
    Path(provider): Path<Uuid>,
    Form(form): Form<HashMap<String, String>>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|sso-logout", client_ip(&headers))).await?;
    let token = form.get("logout_token").map(String::as_str).filter(|t| t.len() <= 16 * 1024).unwrap_or_default();
    let store = [(header::CACHE_CONTROL, "no-store")];
    Ok(match st.kernel.sso_backchannel_logout(provider, token).await {
        Ok(()) => (StatusCode::OK, store).into_response(),
        Err(Error::Unauthorized(why)) => (StatusCode::BAD_REQUEST, store, Json(json!({"error": "invalid_request", "error_description": why}))).into_response(),
        Err(e) => return Err(ApiError(e)),
    })
}

async fn me(SetupAuth(ctx, setup_only): SetupAuth, State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let shell_v2 = st.shell.workspace(ctx.tenant).await?.shell_v2;
    // Shapes B and C: a joined account's password, two-step sign-in and passkeys are its Ferrum
    // account's, kept at the accounts host.
    let accounts = match accounts_for(&st, &headers).await? {
        Some(origin) if st.kernel.identity_of(ctx.tenant, ctx.user).await?.is_some() => Some(origin),
        _ => None,
    };
    Ok(Json(json!({
        "tenant": ctx.tenant,
        "user": {"id": ctx.user, "email": ctx.email, "full_name": ctx.full_name, "is_admin": ctx.is_admin, "roles": ctx.roles},
        "default_company": ctx.default_company,
        "scopes": ctx.scopes,
        "fiscal_year_start_month": ctx.fy_start_month,
        "timezone": ctx.schema.timezone,
        "display_timezone": st.kernel.time_zone_of(ctx.tenant, ctx.user).await?.unwrap_or_else(|| ctx.schema.timezone.clone()),
        "today": ctx.today(),
        "packages": ctx.schema.packages,
        "two_factor_setup": setup_only.is_some(),
        "shell_v2": shell_v2,
        "accounts": accounts,
    }))
    .into_response())
}

async fn two_factor_status(SetupAuth(ctx, _): SetupAuth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.two_factor_status(&ctx).await?).into_response())
}

async fn two_factor_begin(SetupAuth(ctx, _): SetupAuth, State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    kept_by_identity(&st, &headers, &ctx).await?;
    Ok(Json(st.kernel.begin_two_factor(&ctx).await?).into_response())
}

#[derive(Deserialize)]
struct CodeReq {
    code: String,
}

/// Turns two-factor sign-in on; a session that could only set it up gets a full one.
async fn two_factor_enable(
    SetupAuth(ctx, setup_only): SetupAuth,
    State(app): State<AppContext>,
    headers: HeaderMap,
    Json(req): Json<CodeReq>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    kept_by_identity(&st, &headers, &ctx).await?;
    let codes = st.kernel.enable_two_factor(&ctx, &req.code).await?;
    let mut out = json!({"recovery_codes": codes});
    if let Some(s) = setup_only {
        out["token"] = json!(token(&st, &s, st.jwt_expiration, None)?);
        out["expires_in"] = json!(st.jwt_expiration);
    }
    Ok(Json(out).into_response())
}

async fn two_factor_recovery_codes(Auth(ctx): Auth, State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<CodeReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    kept_by_identity(&st, &headers, &ctx).await?;
    Ok(Json(json!({"recovery_codes": st.kernel.new_recovery_codes(&ctx, &req.code).await?})).into_response())
}

#[derive(Deserialize)]
struct DisableReq {
    password: Option<String>,
    code: Option<String>,
}

async fn two_factor_disable(Auth(ctx): Auth, State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<DisableReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    kept_by_identity(&st, &headers, &ctx).await?;
    st.kernel.disable_two_factor(&ctx, req.password.as_deref(), req.code.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn logout_all(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.kernel.logout_everywhere(&ctx).await?;
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut().append(axum::http::header::SET_COOKIE, crate::shell::clear_cookie(st.shell.secure));
    res.headers_mut().insert("clear-site-data", HeaderValue::from_static("\"cache\""));
    Ok(res)
}

/// Signs this browser out: its session ends (with every token bound to it) and the cookie goes.
/// Without a browser session there is nothing to end; the cookie is cleared all the same.
/// A session that began with a SAML provider whose single logout is on gets `200 {"next": address}`:
/// the page sends the browser there, which signs the person out at the provider too and comes back
/// to this host's sign-in page. Not where the zone admits no staff, where its addresses are not
/// served.
async fn logout(State(app): State<AppContext>, method: axum::http::Method, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut next = None;
    if let Ok((ctx, Some(device))) = crate::extract::caller_on(&st, &method, &headers).await {
        match crate::shell::request_host(&headers) {
            Some(host) if !crate::zone::refuses_staff(&st, &headers).await => {
                let back = format!("{}://{host}/app/login", if st.shell.secure { "https" } else { "http" });
                next = st.kernel.sign_out_session(ctx.tenant, device, &back).await?;
            }
            _ => st.kernel.end_session(ctx.tenant, device).await?,
        }
    }
    let mut res = match next {
        Some(next) => (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], Json(json!({"next": next}))).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    };
    res.headers_mut().append(axum::http::header::SET_COOKIE, crate::shell::clear_cookie(st.shell.secure));
    // Pages kept by the browser's cache go; what apps keep for working offline stays (§12.3, §16).
    res.headers_mut().insert("clear-site-data", HeaderValue::from_static("\"cache\""));
    // Shapes B and C: signed out of this host only; the next sign-in here asks which account
    // rather than going straight back in through the accounts host.
    if st.shell.accounts_origin(&headers).is_some() && matches!(st.shell.place(&headers).await?, crate::shell::Place::Workspace(_)) {
        let secure = st.shell.secure;
        res.headers_mut().append(
            axum::http::header::SET_COOKIE,
            crate::shell::host_cookie(secure, crate::shell::signed_out_cookie_name(secure), "1", std::time::Duration::from_secs(86_400)),
        );
    }
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
/// their authenticator app when they use one (their Ferrum account's, where it keeps them). Where the
/// workspace requires single sign-on of them, a password proves nothing: they sign in again there.
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
    let (keys, identity) = keys_of(&st, &headers, ctx.tenant, ctx.user).await?;
    if st.kernel.sso_only(ctx.tenant, ctx.user).await? {
        return Err(ApiError(Error::Forbidden(ferrum_kernel::gate::SSO_ONLY_STEP_UP.into())));
    }
    // Shapes B and C: a joined account steps up with its Ferrum account's password (and app).
    let (slug, email) = match identity {
        Some(email) => (DIRECTORY.to_string(), email),
        None => (st.shell.workspace(ctx.tenant).await?.slug.clone(), ctx.email.clone()),
    };
    // A wrong password or code is a failed step-up on the trail of the account it is, and counts toward its lockout.
    let from = step_up_from(&st, &headers).await;
    match st.kernel.sign_in_at(&slug, &email, &req.password, from.clone()).await? {
        SignIn::Session(_) | SignIn::SetupRequired(_) => {}
        SignIn::SecondFactor(t) => {
            let code = req
                .code
                .as_deref()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| ApiError(Error::field("code", "the code from your authenticator app is needed")))?;
            st.kernel.second_factor_from(t.tenant, t.user, t.token_version, code, &from).await?;
        }
    }
    st.kernel.stepped_up(&StepUp { tenant: ctx.tenant, user: ctx.user, session: device, keys }, "password", &from).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── the other ways to confirm it is you (docs/operations.md, "Asking again before sensitive work") ──

/// What the answers say when a person has no way at all to confirm it is them.
const NO_WAY: &str = "you have no way to confirm it is you here: ask an administrator to set up a password, a passkey or single sign-on for you";
/// …and to a credential with no browser session behind it, which cannot step up.
const NO_BROWSER: &str = "this sign-in cannot confirm it is you: do this from a page of this site";
/// Where the proof of signing in again is handed to the person's dialog, on the host they work on.
const STEP_UP_DONE: &str = "/api/auth/step-up/provider/done";

/// The account whose password and passkeys confirm it is `user` of `tenant` on this host: their own, or, for a workspace
/// account joined to a Ferrum account on a host that hands sign-in to the accounts host (shapes B and C), that account,
/// with its address.
async fn keys_of(st: &AppState, headers: &HeaderMap, tenant: Uuid, user: Uuid) -> Result<((Uuid, Uuid), Option<String>), ApiError> {
    if let Some((id, email)) = st.kernel.identity_of(tenant, user).await?
        && accounts_for(st, headers).await?.is_some()
        && let Some(directory) = st.kernel.directory().await?
    {
        return Ok(((directory, id), Some(email)));
    }
    Ok(((tenant, user), None))
}

/// The ways the person of `who` can confirm it is them on this host ([`ferrum_kernel::Kernel::step_up_ways`]).
async fn ways_of(st: &AppState, headers: &HeaderMap, who: &StepUp) -> Result<Vec<Value>, ApiError> {
    let host = crate::shell::request_host(headers).ok_or_else(|| ApiError(Error::invalid("no host")))?.to_ascii_lowercase();
    Ok(st.kernel.step_up_ways(who, &host).await?)
}

/// `{ways}` with, when there is none, a message that says so plainly (`browser`: the credential has a browser session).
fn ways_body(mut v: Value, ways: Vec<Value>, browser: bool) -> Value {
    if ways.is_empty() {
        v["message"] = json!(if browser { NO_WAY } else { NO_BROWSER });
    }
    v["ways"] = json!(ways);
    v
}

/// A step-up refusal (`403` with error `step_up`, marked by [`crate::error::StepUpAsked`]) with the ways its caller has
/// ([`ways_body`]). The caller is read from the request's token alone, which the request was just admitted with: nothing
/// is counted or recorded again. A caller who cannot be told, or ways that cannot be read, leave the answer as it is.
pub(crate) async fn with_ways(st: &AppState, headers: &HeaderMap, res: Response) -> Response {
    let Some((tenant, user, device)) = crate::extract::token_person(st, headers) else { return res };
    let ways = match device {
        Some(session) => {
            let read = async {
                let (keys, _) = keys_of(st, headers, tenant, user).await?;
                ways_of(st, headers, &StepUp { tenant, user, session, keys }).await
            };
            match read.await {
                Ok(ways) => ways,
                Err(e) => {
                    tracing::warn!("the ways of a step-up refusal could not be read: {}", e.0);
                    return res;
                }
            }
        }
        None => Vec::new(),
    };
    let (mut parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else { return Response::from_parts(parts, axum::body::Body::empty()) };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { return Response::from_parts(parts, axum::body::Body::from(bytes)) };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from(ways_body(v, ways, device.is_some()).to_string()))
}

/// Staff sign-in machinery, first among a handler's arguments: where the zone admits no staff it is not served (`404`),
/// before anything else of the request is read (docs/operations.md, "Zones").
struct StaffZone;

impl axum::extract::FromRequestParts<AppContext> for StaffZone {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, state: &AppContext) -> Result<Self, Self::Rejection> {
        if crate::zone::refuses_staff(&app_state(state)?, &parts.headers).await {
            return Err(ApiError(Error::NotFound("no such address".into())));
        }
        Ok(StaffZone)
    }
}

/// A browser session confirming it is its person: the session required, limited as the password step-up is (the same
/// budget, whichever way), and the account whose credentials count.
async fn step_up_caller(st: &AppState, headers: &HeaderMap, ctx: &ferrum_kernel::Ctx, device: Option<Uuid>) -> Result<StepUp, ApiError> {
    let session = device.ok_or_else(|| ApiError(Error::invalid(NO_BROWSER)))?;
    st.login_limiter.check(&format!("{}|step-up|{}", client_ip(headers), ctx.user)).await?;
    let (keys, _) = keys_of(st, headers, ctx.tenant, ctx.user).await?;
    Ok(StepUp { tenant: ctx.tenant, user: ctx.user, session, keys })
}

/// Where a step-up comes from, for the audit trail: the address, browser and zone, as a step-up.
async fn step_up_from(st: &AppState, headers: &HeaderMap) -> ferrum_kernel::gate::Arrival {
    ferrum_kernel::gate::Arrival { step_up: true, ..crate::zone::arrival(st, headers).await }
}

/// `GET /api/auth/step-up/ways` → `{ways}` as a step-up refusal gives them, for a page that asks before trying.
async fn step_up_ways(_: StaffZone, BrowserSession(ctx, device): BrowserSession, State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let ways = match device {
        Some(session) => {
            let (keys, _) = keys_of(&st, &headers, ctx.tenant, ctx.user).await?;
            ways_of(&st, &headers, &StepUp { tenant: ctx.tenant, user: ctx.user, session, keys }).await?
        }
        None => Vec::new(),
    };
    Ok(Json(ways_body(json!({}), ways, device.is_some())).into_response())
}

/// `POST /api/auth/step-up/passkey/options`: what the browser needs to confirm it is you with a passkey: a challenge for
/// this session and person, allowing only their passkeys, the device to verify the person.
async fn step_up_passkey_options(
    _: StaffZone,
    BrowserSession(ctx, device): BrowserSession,
    State(app): State<AppContext>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let who = step_up_caller(&st, &headers, &ctx, device).await?;
    let host = crate::shell::request_host(&headers).ok_or_else(|| ApiError(Error::invalid("no host")))?.to_ascii_lowercase();
    Ok(Json(st.kernel.passkey_step_up_options(&who, &host).await?).into_response())
}

#[derive(Deserialize)]
struct PasskeyStepUp {
    request: Uuid,
    credential: Value,
}

/// `POST /api/auth/step-up/passkey {request, credential}`: the browser's answer; `204` once it proves it is the person of
/// this session, which then counts as stepped up.
async fn step_up_passkey(
    _: StaffZone,
    BrowserSession(ctx, device): BrowserSession,
    State(app): State<AppContext>,
    headers: HeaderMap,
    Json(req): Json<PasskeyStepUp>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let who = step_up_caller(&st, &headers, &ctx, device).await?;
    let (_, origins) = relying_party(&st, &headers)?;
    st.kernel.passkey_step_up(&who, req.request, &origins, &req.credential, &step_up_from(&st, &headers).await).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// This server's address for `path` on the host of `headers`.
fn here_url(st: &AppState, headers: &HeaderMap, path: &str) -> Result<String, ApiError> {
    let host = crate::shell::request_host(headers).ok_or_else(|| ApiError(Error::invalid("no host")))?;
    Ok(format!("{}://{host}{path}", if st.shell.secure { "https" } else { "http" }))
}

#[derive(Deserialize)]
struct ProviderStepUp {
    dialog: String,
}

/// `POST /api/auth/step-up/provider {dialog}` → `{url}`: the address to open in a small window, where the identity
/// provider the person is linked to has them sign in again. The provider comes back to the return address registered
/// with it (`/app/login`; on the accounts host where sign-in happens there), then to this host's done page, which hands
/// the proof to the dialog that asked. `dialog` is a value of the dialog's own (16 to 64 letters, digits, `-` or `_`):
/// it goes to this server and that page, never to the provider, and the dialog takes a proof only with it.
async fn step_up_provider(
    _: StaffZone,
    BrowserSession(ctx, device): BrowserSession,
    State(app): State<AppContext>,
    headers: HeaderMap,
    Json(req): Json<ProviderStepUp>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let who = step_up_caller(&st, &headers, &ctx, device).await?;
    if !(16..=64).contains(&req.dialog.len()) || !req.dialog.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(ApiError(Error::field("dialog", "16 to 64 letters, digits, - or _")));
    }
    let redirect = match accounts_for(&st, &headers).await? {
        Some(accounts) => format!("{accounts}/app/login"),
        None => here_url(&st, &headers, "/app/login")?,
    };
    let back = here_url(&st, &headers, &format!("{STEP_UP_DONE}?dialog={}", req.dialog))?;
    Ok(Json(json!({"url": st.kernel.sso_step_up_begin(&who, &redirect, &back).await?})).into_response())
}

/// Where the return address registered with identity providers (`/app/login`, on any host) sends the answer to a
/// step-up's signing in again (its state says so, [`ferrum_kernel::sso::STEP_UP_STATE`]): on to
/// `/api/auth/step-up/provider/back` with the answer as it came. `None` for anything else, a sign-in's answer among them.
pub(crate) fn step_up_return(query: Option<&str>) -> Option<String> {
    let q = query?;
    let step_up = q
        .split('&')
        .filter_map(|p| p.split_once('='))
        .any(|(k, v)| k == "state" && crate::controllers::shell::percent_decode(v).starts_with(ferrum_kernel::sso::STEP_UP_STATE));
    step_up.then(|| format!("/api/auth/step-up/provider/back?{q}"))
}

/// A page in the small window saying plainly why signing in again did not confirm it is you.
fn step_up_refused(why: &str) -> Response {
    let mut page = crate::shell::status_page(StatusCode::UNAUTHORIZED, "Signing in again did not work", why, &[], None);
    page.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    page
}

#[derive(Deserialize)]
struct BackQuery {
    #[serde(default)]
    state: String,
    #[serde(default)]
    code: Option<String>,
}

/// `GET /api/auth/step-up/provider/back?state&code`: an OpenID Connect provider's answer to signing in again, by way of
/// the return address registered with it ([`step_up_return`]). On to the page that hands the proof to the dialog, or a
/// page that says plainly why not. Reachable wherever that return address is; it opens no session, and the proof is taken
/// only by the session that asked.
async fn step_up_provider_back(State(app): State<AppContext>, headers: HeaderMap, Query(q): Query<BackQuery>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|sso", client_ip(&headers))).await?;
    if q.state.is_empty() || q.state.len() > 200 || q.code.as_ref().is_some_and(|c| c.len() > 4096) {
        return Ok(step_up_refused("The identity provider's answer is not one this server asked for. Close this window and try again."));
    }
    let from = crate::zone::arrival(&st, &headers).await;
    Ok(match st.kernel.sso_step_up_back(&q.state, q.code.as_deref(), &from).await {
        Ok(next) => (StatusCode::SEE_OTHER, [(header::LOCATION, next.as_str()), (header::CACHE_CONTROL, "no-store"), (header::REFERRER_POLICY, "no-referrer")])
            .into_response(),
        Err(e @ (Error::Db(_) | Error::Internal(_))) => {
            tracing::error!("signing in again: {e}");
            step_up_refused("Something went wrong on the server. Close this window and try again.")
        }
        Err(e) => step_up_refused(&format!("{}. Close this window and try again.", e.to_string().trim_end_matches('.'))),
    })
}

/// `GET /api/auth/step-up/provider/done?dialog&state&code`: hands the proof to the dialog that asked, on this origin,
/// through a `BroadcastChannel` (which works whether or not the provider's pages cut the window off from the page that
/// opened it), takes it out of the window's history, and closes. Its one script is allowed by its digest.
async fn step_up_provider_done(_: StaffZone) -> Response {
    use base64::Engine as _;
    use sha2::Digest as _;
    const SCRIPT: &str = r#"const q=new URLSearchParams(location.search);history.replaceState(null,"",location.pathname);try{const c=new BroadcastChannel("ferrum-step-up");c.postMessage({dialog:q.get("dialog"),state:q.get("state"),code:q.get("code")});c.close()}catch(e){}setTimeout(()=>window.close(),50)"#;
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Signed in again · Ferrum</title></head><body style="font:16px system-ui,sans-serif;margin:2rem"><p>You can close this window and go back to your work.</p><script>{SCRIPT}</script></body></html>"#
    );
    let digest = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(SCRIPT.as_bytes()));
    let csp =
        format!("default-src 'none'; style-src 'unsafe-inline'; script-src 'sha256-{digest}'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'");
    let mut page = crate::shell::page_response(StatusCode::OK, html, &csp);
    page.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    page.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    page
}

#[derive(Deserialize)]
struct ProviderProof {
    state: String,
    code: String,
}

/// `POST /api/auth/step-up/provider/finish {state, code}`: `204` once the proof the done page handed over is this
/// session's and this person's, on this host, fresh and unused; the session then counts as stepped up.
async fn step_up_provider_finish(
    _: StaffZone,
    BrowserSession(ctx, device): BrowserSession,
    State(app): State<AppContext>,
    headers: HeaderMap,
    Json(req): Json<ProviderProof>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let who = step_up_caller(&st, &headers, &ctx, device).await?;
    if req.state.len() > 200 || req.code.len() > 200 {
        return Err(ApiError(Error::Unauthorized("signing in again did not finish; try again".into())));
    }
    let here = here_url(&st, &headers, "/")?;
    st.kernel.sso_step_up_finish(&who, &here, &req.state, &req.code, &step_up_from(&st, &headers).await).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Shapes B and C: someone whose workspace account joined their Ferrum account has that account's
/// password, two-step sign-in and passkeys, kept at the accounts host.
async fn kept_by_identity(st: &AppState, headers: &HeaderMap, ctx: &ferrum_kernel::Ctx) -> Result<(), ApiError> {
    if let Some(accounts) = accounts_for(st, headers).await?
        && st.kernel.identity_of(ctx.tenant, ctx.user).await?.is_some()
    {
        return Err(ApiError(Error::Forbidden(format!("your Ferrum account keeps this; change it at {accounts}"))));
    }
    // A Ferrum account visiting from another region keeps its credentials at home.
    if st.kernel.directory().await? == Some(ctx.tenant)
        && let Some(home) = st.kernel.identity(ctx).await?.home_region
    {
        return Err(ApiError(Error::Forbidden(format!("your Ferrum account lives in region {home}; change this there"))));
    }
    Ok(())
}

// ── passkeys (docs/plan/apps-ui.md §12.3, U6; the kernel's passkey.rs) ──

/// The relying party of a request ([`ferrum_kernel::Kernel::passkey_rp`]: its host, or
/// `FERRUM_PASSKEY_RP` for a host under it), and the one origin a ceremony on it may be answered
/// from: this host's own (the kernel also checks that host may use the party).
fn relying_party(st: &AppState, headers: &HeaderMap) -> Result<(String, Vec<String>), ApiError> {
    let host = crate::shell::request_host(headers).ok_or_else(|| ApiError(Error::invalid("no host")))?.to_ascii_lowercase();
    let rp = st.kernel.passkey_rp(&host);
    let scheme = if st.shell.secure { "https" } else { "http" };
    Ok((rp, vec![format!("{scheme}://{host}")]))
}

/// `POST /api/auth/passkeys/options`: what the browser needs to make a passkey for the caller on
/// this host. Changing how one signs in needs a recent sign-in or step-up (§9.2).
async fn passkey_options(BrowserSession(ctx, device): BrowserSession, State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let Some(device) = device else { return Err(ApiError(Error::invalid("make passkeys from a page of this site"))) };
    kept_by_identity(&st, &headers, &ctx).await?;
    if !st.kernel.check_session(ctx.tenant, ctx.user, device).await?.stepped_up() {
        return Err(ApiError(Error::Forbidden("confirm your password to change how you sign in".into())));
    }
    let (rp, _) = relying_party(&st, &headers)?;
    // At the accounts host the passkey is for Ferrum itself, not for a workspace.
    let name = if st.kernel.directory().await? == Some(ctx.tenant) { "Ferrum".to_string() } else { st.shell.workspace(ctx.tenant).await?.name.clone() };
    Ok(Json(st.kernel.passkey_options(&ctx, &rp, &name).await?).into_response())
}

#[derive(Deserialize)]
struct NewPasskey {
    request: Uuid,
    credential: Value,
    #[serde(default)]
    name: String,
}

/// `POST /api/auth/passkeys`: keeps the passkey the browser made.
async fn add_passkey(
    BrowserSession(ctx, _): BrowserSession,
    State(app): State<AppContext>,
    headers: HeaderMap,
    Json(req): Json<NewPasskey>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    kept_by_identity(&st, &headers, &ctx).await?;
    let (_, origins) = relying_party(&st, &headers)?;
    let made = st.kernel.add_passkey(&ctx, req.request, &origins, &req.credential, &req.name).await?;
    Ok((StatusCode::CREATED, Json(made)).into_response())
}

async fn list_passkeys(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"passkeys": app_state(&app)?.kernel.passkeys(&ctx).await?})).into_response())
}

#[derive(Deserialize)]
struct PasskeyName {
    name: String,
}

async fn rename_passkey(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(req): Json<PasskeyName>) -> ApiResult<Response> {
    app_state(&app)?.kernel.change_passkey(&ctx, id, Some(&req.name)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn remove_passkey(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.change_passkey(&ctx, id, None).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/auth/passkey/options`: what the browser needs to sign in with a passkey here.
async fn passkey_sign_in_options(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|passkey", client_ip(&headers))).await?;
    let host = crate::shell::request_host(&headers).ok_or_else(|| ApiError(Error::invalid("no host")))?.to_ascii_lowercase();
    Ok(Json(st.kernel.passkey_sign_in_offer(&host).await?).into_response())
}

#[derive(Deserialize)]
struct PasskeySignIn {
    request: Uuid,
    credential: Value,
    #[serde(default)]
    session: Option<String>,
}

/// `POST /api/auth/passkey`: signs in with the browser's answer, to the account the passkey
/// belongs to (on a workspace's own host, only that workspace's).
async fn passkey_sign_in(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<PasskeySignIn>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.login_limiter.check(&format!("{}|passkey", client_ip(&headers))).await?;
    let (_, origins) = relying_party(&st, &headers)?;
    let tenant = match st.shell.place(&headers).await? {
        crate::shell::Place::Workspace(t) | crate::shell::Place::Portal(t) => Some(t),
        crate::shell::Place::Accounts => st.kernel.directory().await?,
        crate::shell::Place::Any => None,
        _ => return Err(ApiError(Error::Unauthorized("sign in on the workspace's address".into()))),
    };
    let from = crate::zone::arrival(&st, &headers).await;
    let s = st.kernel.passkey_sign_in_at(req.request, &origins, &req.credential, tenant, from).await?;
    session_response(&st, &s, &headers, req.session.as_deref() == Some("cookie"), "passkey").await
}

#[derive(Deserialize)]
struct PasswordReq {
    current_password: Option<String>,
    new_password: String,
}

async fn change_password(Auth(ctx): Auth, State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<PasswordReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    kept_by_identity(&st, &headers, &ctx).await?;
    st.kernel.set_password(&ctx, ctx.user, req.current_password.as_deref(), &req.new_password).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct KeyReq {
    name: String,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `dav`: an app password for calendar and contacts apps; `scim`: an administrator's key for an
    /// identity provider; none: a key to the whole API.
    scope: Option<String>,
    /// The networks it works from (`10.0.0.0/8`, `2001:db8::/32`); none: anywhere.
    #[serde(default)]
    networks: Vec<String>,
}

#[derive(Deserialize)]
struct KeyNetworks {
    networks: Vec<String>,
}

/// `PATCH /api/auth/api_keys/{id} {networks}`: where a key works from (none: anywhere). Audited.
async fn update_key(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(req): Json<KeyNetworks>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_api_key_networks(&ctx, id, &req.networks).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn create_key(Auth(ctx): Auth, State(app): State<AppContext>, Json(req): Json<KeyReq>) -> ApiResult<Response> {
    if req.name.trim().is_empty() || req.name.len() > 100 {
        return Err(ApiError(Error::field("name", "is required (max 100 characters)")));
    }
    let st = app_state(&app)?;
    // A Ferrum account is no workspace: keys are made in the workspace they open.
    if st.kernel.directory().await? == Some(ctx.tenant) {
        return Err(ApiError(Error::Forbidden("make API keys in the workspace they are for".into())));
    }
    let (id, key) = st.kernel.create_api_key_on(&ctx, ctx.user, req.name.trim(), req.expires_at, req.scope.as_deref(), &req.networks).await?;
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
        Option<Vec<String>>,
    )> = sqlx::query_as(
        "select id, name, prefix, created_at, expires_at, revoked_at, last_used_at, scope, networks from api_key where tenant_id = $1 and user_id = $2 order by created_at desc",
    )
    .bind(ctx.tenant)
    .bind(ctx.user)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    let keys: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, prefix, created, expires, revoked, used, scope, networks)| {
            json!({"id": id, "name": name, "prefix": format!("fk_{prefix}_…"), "created_at": created, "expires_at": expires, "revoked_at": revoked, "last_used_at": used, "scope": scope,
                   "networks": networks.unwrap_or_default()})
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
        .add("/passkey/options", post(passkey_sign_in_options))
        .add("/passkey", post(passkey_sign_in))
        .add("/passkeys/options", post(passkey_options))
        .add("/passkeys", get(list_passkeys).post(add_passkey))
        .add("/passkeys/{id}", delete(remove_passkey).patch(rename_passkey))
        .add("/login", post(login))
        .add("/login/second-factor", post(login_second_factor))
        .add("/code", post(send_code))
        .add("/code/sign-in", post(sign_in_with_code))
        .add("/code/sign-up", post(sign_up))
        .add("/sso", get(sso_options))
        .add("/sso/start", post(sso_start))
        .add("/sso/finish", post(sso_finish))
        .add("/sso/{provider}/backchannel-logout", post(sso_backchannel_logout))
        .add("/saml/{tenant}", get(saml_metadata))
        .add("/saml/{tenant}/acs", post(saml_acs))
        .add("/saml/{tenant}/logout", get(saml_logout_out))
        .add("/saml/{tenant}/slo", get(saml_slo).post(saml_slo))
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
        .add("/step-up/ways", get(step_up_ways))
        .add("/step-up/passkey/options", post(step_up_passkey_options))
        .add("/step-up/passkey", post(step_up_passkey))
        .add("/step-up/provider", post(step_up_provider))
        .add("/step-up/provider/back", get(step_up_provider_back))
        .add("/step-up/provider/done", get(step_up_provider_done))
        .add("/step-up/provider/finish", post(step_up_provider_finish))
        .add("/password", post(change_password))
        .add("/api_keys", get(list_keys).post(create_key))
        .add("/api_keys/{id}", delete(revoke_key).patch(update_key))
}
