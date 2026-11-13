//! Connections to outside services (crates/kernel/src/connections, BD-29 to BD-33): administrators describe, test,
//! connect and read the log of each here, shaped for an authoring screen; services call in on an inbound address; a
//! person connects their own account; the provider sends the browser back to one exact address.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, BrowserSession, client_ip};
use crate::state::RateLimiter;
use crate::state::app_state;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::connections::ConnectionInput;
use ferrum_kernel::{Ctx, Error, Kernel};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

/// `GET /api/admin/connections`: every connection, never a secret (the names of its secrets, which are set).
async fn list(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"connections": app_state(&app)?.kernel.connection_defs(&ctx).await?})).into_response())
}

async fn add(Auth(ctx): Auth, State(app): State<AppContext>, Json(c): Json<ConnectionInput>) -> ApiResult<Response> {
    let id = app_state(&app)?.kernel.save_connection_def(&ctx, None, &c).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

async fn change(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(c): Json<ConnectionInput>) -> ApiResult<Response> {
    app_state(&app)?.kernel.save_connection_def(&ctx, Some(id), &c).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn remove(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.delete_connection_def(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Try {
    operation: String,
    #[serde(default)]
    inputs: Value,
    #[serde(default)]
    key: Option<String>,
}

/// `POST /api/admin/connections/{id}/try {operation, inputs, key?}`: an administrator runs one operation deliberately.
async fn try_it(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(t): Json<Try>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.try_connection(&ctx, id, &t.operation, &t.inputs, t.key.as_deref()).await?).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Debug {
    minutes: i64,
}

/// `PUT /api/admin/connections/{id}/debug {minutes}`: the redacted capture, for at most an hour (0 turns it off).
async fn debug(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(d): Json<Debug>) -> ApiResult<Response> {
    app_state(&app)?.kernel.debug_connection(&ctx, id, d.minutes).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<i64>,
}

/// `GET /api/admin/connections/{id}/log?limit=`: the calls made, the calls received and the files' public addresses the
/// calls made (BD-64), newest first.
async fn log(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Query(q): Query<Limit>) -> ApiResult<Response> {
    let k = &app_state(&app)?.kernel;
    let limit = q.limit.unwrap_or(100);
    Ok(Json(json!({"calls": k.connection_log(&ctx, id, limit).await?, "received": k.connection_inbound_log(&ctx, id, limit).await?,
        "addresses": k.connection_addresses(&ctx, id, limit).await?}))
    .into_response())
}

/// `GET /api/admin/connections/{id}/pulls`: each pull's cursor, last and next run, and last error.
async fn pulls(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(json!({"pulls": app_state(&app)?.kernel.connection_pulls(&ctx, id).await?})).into_response())
}

/// `POST /api/admin/connections/{id}/pulls/{name}`: pulls now (queued) → `{job}`.
async fn pull_now(Auth(ctx): Auth, State(app): State<AppContext>, Path((id, name)): Path<(Uuid, String)>) -> ApiResult<Response> {
    Ok((StatusCode::ACCEPTED, Json(json!({"job": app_state(&app)?.kernel.pull_now(&ctx, id, &name).await?}))).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Secrets {
    #[serde(default)]
    set: BTreeMap<String, String>,
    #[serde(default)]
    remove: Vec<String>,
}

/// `PATCH /api/admin/connections/{id}/secrets {set, remove}`: sets or replaces the named secrets and removes others,
/// keeping the rest → `{secrets: [names]}`.
async fn secrets(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(s): Json<Secrets>) -> ApiResult<Response> {
    Ok(Json(json!({"secrets": app_state(&app)?.kernel.merge_connection_secrets(&ctx, id, &s.set, &s.remove).await?})).into_response())
}

/// `POST /api/admin/connections/{id}/session`: a connection that signs in with a session signs in now, afresh →
/// `{status, account, expires_at}`.
async fn session_now(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.connect_session(&ctx, id).await?).into_response())
}

/// `GET /api/connections`: the connections a person signs in to with their own account, and whether they have.
async fn personal(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"connections": app_state(&app)?.kernel.personal_connections(&ctx).await?})).into_response())
}

fn session(sid: Option<Uuid>) -> Result<Uuid, ApiError> {
    sid.ok_or_else(|| ApiError(Error::Unauthorized("connecting an account is done in the browser, signed in".into())))
}

/// `POST /api/connections/{id}/connect`: from the browser, signed in → `{url}` to send the browser to. The workspace's
/// account is an administrator's; a person's own is theirs.
async fn connect(BrowserSession(ctx, sid): BrowserSession, State(app): State<AppContext>, headers: HeaderMap, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let url = app_state(&app)?.kernel.connect_start_on(&ctx, id, session(sid)?, crate::shell::request_host(&headers)).await?;
    Ok(Json(json!({"url": url})).into_response())
}

/// `DELETE /api/connections/{id}/connect`: disconnects (revoked at the provider where it offers that) → `{revoked}`.
async fn disconnect(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(json!({"revoked": app_state(&app)?.kernel.connect_remove(&ctx, id).await?})).into_response())
}

#[derive(Deserialize, Default)]
pub struct Back {
    pub state: Option<String>,
    pub code: Option<String>,
    pub error: Option<String>,
}

fn page(text: &str, status: StatusCode) -> Response {
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><meta name=referrer content=no-referrer>\
         <body style=\"font-family:system-ui;max-width:32rem;margin:3rem auto;padding:0 1rem\"><p>{text}</p></body>"
    );
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], html).into_response()
}

/// `GET /api/connections/callback?state=&code=`: where the provider sends the browser back, the one exact address
/// (on the public address, or the hooks host). A sign-in started on another host of the workspace goes on there, to
/// the same path with the same query, where the person's session is; the host comes from the signed `state` and must
/// still serve that workspace. Otherwise a page says how it went.
async fn callback(State(app): State<AppContext>, headers: HeaderMap, uri: axum::http::Uri, Query(b): Query<Back>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    if let Some(to) = started_elsewhere(&st.kernel, &st.shell, &headers, uri.query(), b.state.as_deref()).await? {
        let mut res = StatusCode::FOUND.into_response();
        res.headers_mut().insert(header::LOCATION, axum::http::HeaderValue::from_str(&to).map_err(|e| ApiError(Error::internal(e.to_string())))?);
        res.headers_mut().insert(header::REFERRER_POLICY, axum::http::HeaderValue::from_static("no-referrer"));
        return Ok(res);
    }
    let (ctx, sid) = crate::extract::caller_on(&st, &axum::http::Method::GET, &headers).await?;
    callback_page(&st.kernel, &ctx, sid, &b).await
}

/// The same return address (with the request's `query`) on the host the sign-in started from, when that is another
/// host that still serves the workspace the signed `state` names; `None`: answer here.
pub async fn started_elsewhere(
    k: &Kernel,
    shell: &crate::shell::Shell,
    headers: &HeaderMap,
    query: Option<&str>,
    state: Option<&str>,
) -> ApiResult<Option<String>> {
    let Some((tenant, host)) = state.filter(|s| s.len() <= 1024).and_then(|s| k.connection_state_host(s)) else { return Ok(None) };
    let here = crate::shell::request_host(headers).and_then(ferrum_kernel::shell::normalize_host);
    if here.as_deref() == Some(host.as_str()) || shell.workspace_at(&host).await? != Some(tenant) {
        return Ok(None);
    }
    let Some(origin) = shell.origin_of(&host, headers) else { return Ok(None) };
    Ok(Some(format!("{origin}{}?{}", ferrum_kernel::connections::oauth::CALLBACK, query.unwrap_or_default())))
}

/// What the callback answers the person `ctx` in browser session `sid`.
pub async fn callback_page(k: &Kernel, ctx: &Ctx, sid: Option<Uuid>, b: &Back) -> ApiResult<Response> {
    let Some(state) = b.state.as_deref().filter(|s| !s.is_empty() && s.len() <= 1024) else {
        return Ok(page("This sign-in was not started here.", StatusCode::BAD_REQUEST));
    };
    Ok(match k.connect_finish(ctx, session(sid)?, state, b.code.as_deref(), b.error.as_deref()).await {
        Ok(_) => page("The account is connected. You can close this window.", StatusCode::OK),
        Err(Error::Unauthorized(_)) => page("This sign-in was not started here, or has been used.", StatusCode::UNAUTHORIZED),
        Err(_) => page("The account was not connected. Try again from the app.", StatusCode::BAD_REQUEST),
    })
}

/// `POST /api/connections/inbound/{id}`: no session; the connection's description says how its caller proves itself.
async fn inbound(State(app): State<AppContext>, Path(id): Path<Uuid>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let st = app_state(&app)?;
    inbound_answer(&st.kernel, &st.public_limiter, &headers, id, &body).await
}

/// `GET /api/connections/inbound/{id}?…`: a service's check before it delivers (BD-78): the challenge as plain text, or
/// the same 401 as any refusal.
async fn handshake(State(app): State<AppContext>, Path(id): Path<Uuid>, Query(q): Query<BTreeMap<String, String>>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.public_limiter.check(&format!("{}|connections", client_ip(&headers))).await?;
    let challenge = st.kernel.connection_handshake(id, &q).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8"), (axum::http::header::HeaderName::from_static("x-content-type-options"), "nosniff")],
        challenge,
    )
        .into_response())
}

/// What an inbound call is answered: every refusal the same 401, counted; one accepted, `{accepted: true}`.
pub async fn inbound_answer(k: &Kernel, limiter: &RateLimiter, headers: &HeaderMap, id: Uuid, body: &[u8]) -> ApiResult<Response> {
    limiter.check(&format!("{}|connections", client_ip(headers))).await?;
    let names: BTreeMap<String, String> = headers.iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
    Ok(Json(k.connection_inbound(id, &names, body).await?).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{get, patch, post, put};
    Routes::new()
        .prefix("/api")
        .add("/admin/connections", get(list).post(add))
        .add("/admin/connections/{id}", put(change).delete(remove))
        .add("/admin/connections/{id}/try", post(try_it))
        .add("/admin/connections/{id}/debug", put(debug))
        .add("/admin/connections/{id}/log", get(log))
        .add("/admin/connections/{id}/pulls", get(pulls))
        .add("/admin/connections/{id}/pulls/{name}", post(pull_now))
        .add("/admin/connections/{id}/session", post(session_now))
        .add("/admin/connections/{id}/secrets", patch(secrets))
        .add("/connections", get(personal))
        .add("/connections/callback", get(callback))
        .add("/connections/{id}/connect", post(connect).delete(disconnect))
        .add("/connections/inbound/{id}", post(inbound).get(handshake))
}
