//! The accounts host, `accounts.{base}` (docs/plan/apps-ui.md §9.3, §12.3; shapes B and C). People
//! sign in here once, as their Ferrum account (an identity: a person of the cluster's directory),
//! and go on to their workspaces' hosts: a workspace's host without a session sends the browser
//! to `/authorize`, which sends it back with a one-use code for a session of that host's own.
//! A person already signed in sees no page. Here too: the account chooser, the account's own
//! password, two-step sign-in, passkeys and devices, and adding a workspace to the account once
//! the person proves it is theirs.

use crate::controllers::auth::{session_response, signed};
use crate::controllers::shell::percent_decode;
use crate::error::{ApiError, ApiResult};
use crate::extract::{caller_on, client_ip};
use crate::shell::{self, Place, status_page};
use crate::state::{AppState, app_state};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use dashmap::DashMap;
use ferrum_kernel::auth::Session;
use ferrum_kernel::identity::{AccountSignIn, Entry, Proof};
use ferrum_kernel::shell::{HostRoute, normalize_host};
use ferrum_kernel::{Ctx, Error};
use loco_rs::app::AppContext;
use loco_rs::auth::jwt::JWT;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// A step of signing in at accounts lasts five minutes (an authenticator's code, the address's).
const TICKET_SECS: u64 = 300;
/// The least an answer to "email me a code" takes, so how long it takes says nothing about the
/// address either.
const CODE_ANSWER: Duration = Duration::from_millis(600);

fn query<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Percent-encodes a query value.
fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn not_found() -> Response {
    status_page(StatusCode::NOT_FOUND, "Not found", "There is nothing at this address.", &[("Your workspaces", "/")], None)
}

/// The workspace's host a sign-in goes back to, as the browser sent it: a host name, with a port
/// only on a development server (plain http).
fn host_param(secure: bool, host: &str) -> Option<(String, String)> {
    let host = host.trim().to_ascii_lowercase();
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host.as_str(), None),
    };
    if host.len() > 260 || port.is_some_and(|p| secure || p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let bare = normalize_host(name)?;
    Some((host.clone(), bare))
}

/// The `state` a workspace's host sent: what it gets back, unchanged.
fn state_param(state: &str) -> Option<&str> {
    (!state.is_empty() && state.len() <= 128 && state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then_some(state)
}

/// The workspace a host serves the shell for.
async fn workspace_at(st: &AppState, bare: &str) -> Result<Option<Uuid>, ApiError> {
    Ok(match st.kernel.route_host(bare).await? {
        HostRoute::Serve { tenant, target } if target == "shell" => Some(tenant),
        _ => None,
    })
}

fn callback(st: &AppState, host: &str, code: &str, state: &str) -> String {
    let scheme = if st.shell.secure { "https" } else { "http" };
    format!("{scheme}://{host}/_auth/callback?code={code}&state={state}")
}

/// A workspace's address with this request's port (on a development server), to open it.
fn workspace_url(st: &AppState, headers: &HeaderMap, host: &str) -> String {
    let scheme = if st.shell.secure { "https" } else { "http" };
    let port = shell::request_host(headers).and_then(|h| h.rsplit_once(':')).map(|(_, p)| p).filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    match port {
        Some(p) => format!("{scheme}://{host}:{p}/"),
        None => format!("{scheme}://{host}/"),
    }
}

/// The Ferrum account signed in on this browser, if any.
async fn signed_in(st: &AppState, headers: &HeaderMap) -> Option<(Ctx, Option<Uuid>)> {
    shell::session_cookie(headers, st.shell.secure)?;
    let (ctx, device) = caller_on(st, &Method::GET, headers).await.ok()?;
    (st.kernel.directory().await.ok()? == Some(ctx.tenant)).then_some((ctx, device))
}

fn regions(st: &AppState) -> Value {
    json!(st.shell.regions.iter().map(|(name, url)| json!({"name": name, "url": url})).collect::<Vec<_>>())
}

// ───────────────────────── pages ─────────────────────────

/// `GET /`: the account chooser and the account's own security; signing in first.
pub(crate) async fn home(st: &AppState, headers: &HeaderMap) -> ApiResult<Response> {
    match signed_in(st, headers).await {
        Some((ctx, _)) => interface(st, &ctx, Value::Null).await,
        None => Ok(Redirect::to("/login").into_response()),
    }
}

/// Every other page of the accounts host.
pub(crate) async fn page(st: &AppState, headers: &HeaderMap, uri: &Uri, first: &str, rest: Option<&str>, q: &[(String, String)]) -> ApiResult<Response> {
    match (first, rest) {
        ("login", None) => login(st, headers, q).await,
        ("authorize", None) => authorize(st, headers, uri, q).await,
        // Where single sign-on providers send people back (the address they have registered).
        ("app", Some("login")) => Ok(Redirect::to(&format!("/login{}", uri.query().map(|q| format!("?{q}")).unwrap_or_default())).into_response()),
        ("account", _) => Ok(Redirect::permanent("/").into_response()),
        ("regions", Some("authorize")) => region_authorize(st, headers, uri, q).await,
        ("regions", Some("start")) => region_start(st, headers, q).await,
        ("regions", Some("callback")) => region_callback(st, headers, q).await,
        _ => Ok(not_found()),
    }
}

/// The shell's interface, its `accounts` entry, for a signed-in account: the chooser, or a step
/// of entering a workspace (`request`).
async fn interface(st: &AppState, ctx: &Ctx, request: Value) -> ApiResult<Response> {
    let me = st.kernel.identity(ctx).await?;
    let Some(release) = st.shell.release_for(ctx.tenant, shell::SHELL_APP, shell::SHELL_VERSION).await? else {
        return Ok(status_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Your account cannot open",
            "This server has no interface for the shell. An operator can add it (settings.ui_dir, which the image sets).",
            &[],
            None,
        ));
    };
    let entry: ferrum_apps::ui::EntryTemplate =
        serde_json::from_value(release.entry.clone()).map_err(|e| ApiError(Error::internal(format!("the shell's release {}: {e}", release.digest))))?;
    let boot = json!({
        "v": 1,
        // The directory stands where a workspace would: pages shared with workspaces read it.
        "workspace": {"id": null, "slug": ferrum_kernel::identity::DIRECTORY, "name": "Ferrum", "host": null},
        "person": {"id": ctx.user, "name": if me.name.is_empty() { &me.email } else { &me.name }, "email": me.email, "kind": "member", "admin": false, "actor": null},
        "app": {"id": shell::SHELL_APP, "version": shell::SHELL_VERSION, "release": release.digest, "routes": {"": "/", "accounts": "/"}, "entry": "accounts", "title": "Your Ferrum account", "compact": false, "scope": null},
        "host": {"kind": "accounts", "shell": shell::SHELL_VERSION},
        "api": {"base": "/api/", "live": null, "auth": "cookie"},
        "locale": {"language": "en", "timezone": "UTC", "dir": "ltr"},
        "theme": {"appearance": "system", "hash": shell::theme_hash()},
        "company": null,
        "flags": {},
        "accounts": {"email": me.email, "verified": me.verified, "two_factor": me.two_factor, "region": st.shell.region, "regions": regions(st), "mail": st.kernel.mailer.is_some()},
        "request": request,
    });
    let mut res = shell::accounts_page(&entry, &boot, &shell::csp(None, &Value::Null));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// The workspace a sign-in at accounts is on its way to (`next` = `/authorize?host=…`): its name
/// and slug, for the heading and its single sign-on.
async fn target_of(st: &AppState, next: &str) -> Option<Value> {
    let q = next.strip_prefix("/authorize?")?;
    let host = q.split('&').find_map(|p| p.strip_prefix("host="))?;
    let host = percent_decode(host);
    let (_, bare) = host_param(st.shell.secure, &host)?;
    let tenant = workspace_at(st, &bare).await.ok()??;
    let ws = st.shell.workspace(tenant).await.ok()?;
    Some(json!({"name": ws.name, "slug": ws.slug}))
}

/// `/login?next=…`: signing in to the Ferrum account; someone signed in already goes on.
/// `sso=1`: only the workspace's single sign-on (it requires it).
pub(crate) async fn login(st: &AppState, headers: &HeaderMap, q: &[(String, String)]) -> ApiResult<Response> {
    let next = shell::safe_next(query(q, "next").unwrap_or("/"));
    let sso_only = query(q, "sso") == Some("1");
    if !sso_only && signed_in(st, headers).await.is_some() {
        return Ok(Redirect::to(&next).into_response());
    }
    let target = target_of(st, &next).await;
    let boot = json!({
        "v": 1,
        "next": next,
        "accounts": {"target": target, "regions": regions(st), "mail": st.kernel.mailer.is_some(), "sso_only": sso_only && target.is_some()},
    });
    let title =
        target.as_ref().and_then(|t| t["name"].as_str()).map_or_else(|| "Sign in to Ferrum".to_string(), |name| format!("Sign in to continue to {name}"));
    let mut res = shell::login_html(&boot, &title);
    if !sso_only && shell::session_cookie(headers, st.shell.secure).is_some() {
        res.headers_mut().append(header::SET_COOKIE, shell::clear_cookie(st.shell.secure));
    }
    Ok(res)
}

/// What a step of entering a workspace needs, for the interface.
fn need(entry: &Entry) -> Value {
    match entry {
        Entry::Ready(_) => json!({"need": "continue"}),
        Entry::Join { click, password, two_factor } => json!({"need": "join", "join": {"click": click, "password": password, "two_factor": two_factor}}),
        Entry::Sso => json!({"need": "sso"}),
        Entry::TwoFactor => json!({"need": "two_factor"}),
        Entry::NoAccess => json!({"need": "no_access"}),
    }
}

/// `GET /authorize?host=…&state=…&prompt=none|select_account`: a workspace's host asks for a
/// sign-in. Signed in, with a membership there and nothing to ask: straight back with a code.
/// Otherwise the step it takes: signing in, choosing to go on as this account, adding the
/// workspace, the workspace's single sign-on or two-step sign-in, or no access.
async fn authorize(st: &AppState, headers: &HeaderMap, uri: &Uri, q: &[(String, String)]) -> ApiResult<Response> {
    let bad = || {
        status_page(
            StatusCode::BAD_REQUEST,
            "This sign-in link does not work",
            "Go back to the workspace's address and sign in from there.",
            &[("Your workspaces", "/")],
            None,
        )
    };
    let (Some((host, bare)), Some(state)) = (query(q, "host").and_then(|h| host_param(st.shell.secure, h)), query(q, "state").and_then(state_param)) else {
        return Ok(bad());
    };
    let Some(tenant) = workspace_at(st, &bare).await? else {
        // Another region's workspace: its accounts host takes the same request (asking the
        // regions costs requests: a budget per address).
        st.public_limiter.check(&format!("{}|authorize", client_ip(headers))).await?;
        if let Some(there) = elsewhere(st, &bare).await {
            return Ok(Redirect::to(&format!("{there}/authorize?{}", uri.query().unwrap_or_default())).into_response());
        }
        return Ok(status_page(StatusCode::NOT_FOUND, "No workspace here", "This address does not lead to a workspace.", &[("Your workspaces", "/")], None));
    };
    let Some((ctx, _)) = signed_in(st, headers).await else {
        return Ok(Redirect::to(&format!("/login?next={}", enc(&format!("/authorize?{}", uri.query().unwrap_or_default())))).into_response());
    };
    let entry = st.kernel.enter(&ctx, tenant).await?;
    if let Entry::Ready(m) = &entry
        && query(q, "prompt") != Some("select_account")
    {
        let code = st.kernel.mint_host_code(m, Some(ctx.user), &bare).await?;
        let mut res = Redirect::to(&callback(st, &host, &code, state)).into_response();
        res.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        return Ok(res);
    }
    let ws = st.shell.workspace(tenant).await?;
    let mut request = need(&entry);
    request["host"] = json!(host);
    request["state"] = json!(state);
    request["workspace"] = json!({"name": ws.name, "slug": ws.slug, "url": workspace_url(st, headers, &bare)});
    interface(st, &ctx, request).await
}

/// Regions this cluster's directory knows: a host that is not here may be another region's.
/// Each region answers `/_directory?host=` for its own hosts; answers are kept five minutes.
async fn elsewhere(st: &AppState, bare: &str) -> Option<String> {
    static SEEN: LazyLock<DashMap<String, (Instant, Option<String>)>> = LazyLock::new(DashMap::new);
    if st.shell.regions.is_empty() {
        return None;
    }
    if let Some(e) = SEEN.get(bare).filter(|e| e.0.elapsed() < Duration::from_secs(300)) {
        return e.1.clone();
    }
    let mut found = None;
    for (_, url) in &st.shell.regions {
        let Some(client) = region_client(url, Duration::from_secs(3)) else { continue };
        let asked = client.get(format!("{url}/_directory?host={}", enc(bare))).send().await;
        if asked.is_ok_and(|r| r.status().is_success()) {
            found = Some(url.clone());
            break;
        }
    }
    if SEEN.len() > 10_000 {
        SEEN.clear();
    }
    SEEN.insert(bare.to_string(), (Instant::now(), found.clone()));
    found
}

/// A client for another region's accounts host: no redirects followed; names under `localhost`
/// are this machine (RFC 6761), for regions run side by side in development.
fn region_client(url: &str, timeout: Duration) -> Option<reqwest::Client> {
    let mut b = reqwest::Client::builder().timeout(timeout).redirect(reqwest::redirect::Policy::none());
    if let Ok(u) = reqwest::Url::parse(url)
        && let Some(host) = u.host_str()
        && (host == "localhost" || host.ends_with(".localhost"))
    {
        b = b.resolve(host, std::net::SocketAddr::from(([127, 0, 0, 1], u.port_or_known_default().unwrap_or(80))));
    }
    b.build().ok()
}

/// `GET /_directory?host=`: whether a host is this region's (a workspace's, or the cluster's
/// own), for other regions' accounts hosts and edges. Public: it says what DNS says.
async fn directory(State(app): State<AppContext>, Query(q): Query<Vec<(String, String)>>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let Some(bare) = query(&q, "host").and_then(normalize_host) else { return Ok(StatusCode::NOT_FOUND.into_response()) };
    let ours = match st.kernel.route_host(&bare).await? {
        HostRoute::Serve { .. } | HostRoute::Moved { .. } => true,
        HostRoute::Unknown => st.shell.base().is_some_and(|b| bare == format!("{}.{b}", ferrum_kernel::identity::DIRECTORY) || bare == format!("hooks.{b}")),
    };
    if !ours {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let mut res = Json(json!({"region": st.shell.region})).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
    Ok(res)
}

// ───────────────────────── signing in ─────────────────────────

/// The state of a request made to the accounts host's API: on any other host it is not there.
async fn here(app: &AppContext, headers: &HeaderMap) -> Result<AppState, ApiError> {
    let st = app_state(app)?;
    if !matches!(st.shell.place(headers).await?, Place::Accounts) {
        return Err(ApiError(Error::NotFound("no such endpoint here".into())));
    }
    Ok(st)
}

/// A ticket for the next step of signing in (`kind`: `mfa`, `found` or `verify`).
fn ticket(st: &AppState, s: &Session, kind: &str) -> Result<String, ApiError> {
    signed(st, s, TICKET_SECS, Some(("acct", json!(kind))))
}

/// The session a ticket of `kind` is for.
fn from_ticket(st: &AppState, ticket: &str, kind: &str) -> Result<Session, ApiError> {
    let again = || ApiError(Error::Unauthorized("sign in again".into()));
    if ticket.len() > 4096 {
        return Err(again());
    }
    let data = JWT::new(&st.jwt_secret).validate(ticket).map_err(|_| again())?;
    let claims = &data.claims.claims;
    if claims.get("acct").and_then(Value::as_str) != Some(kind) {
        return Err(again());
    }
    let user: Uuid = data.claims.pid.parse().map_err(|_| again())?;
    let tenant: Uuid = claims.get("tenant").and_then(Value::as_str).and_then(|s| s.parse().ok()).ok_or_else(again)?;
    let token_version = i32::try_from(claims.get("tv").and_then(Value::as_i64).ok_or_else(again)?).map_err(|_| again())?;
    Ok(Session { tenant, user, token_version })
}

/// The answer to a step of signing in: the cookie when signed in, else the next step's ticket.
async fn answer(st: &AppState, headers: &HeaderMap, outcome: AccountSignIn) -> ApiResult<Response> {
    match outcome {
        AccountSignIn::Signed(s) => session_response(st, &s, headers, true).await,
        AccountSignIn::SecondFactor { session, found } => {
            Ok(Json(json!({"second_factor": true, "ticket": ticket(st, &session, if found { "found" } else { "mfa" })?, "expires_in": TICKET_SECS}))
                .into_response())
        }
        AccountSignIn::Verify(m) => Ok(Json(json!({"verify": true, "ticket": ticket(st, &m, "verify")?, "expires_in": TICKET_SECS})).into_response()),
    }
}

#[derive(Deserialize)]
struct SignInReq {
    email: String,
    password: String,
}

/// `POST /api/accounts/sign-in`: the password step.
async fn sign_in(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<SignInReq>) -> ApiResult<Response> {
    let st = here(&app, &headers).await?;
    st.login_limiter.check(&format!("{}|accounts|{}", client_ip(&headers), req.email.trim().to_lowercase())).await?;
    if req.password.len() > 256 || req.email.len() > 254 {
        return Err(ApiError(Error::Unauthorized("wrong email or password".into())));
    }
    let outcome = st.kernel.account_sign_in(&req.email, &req.password).await?;
    answer(&st, &headers, outcome).await
}

#[derive(Deserialize)]
struct StepReq {
    ticket: String,
    code: String,
}

/// `POST /api/accounts/sign-in/second-factor`: the authenticator's code (or a recovery code).
async fn second_factor(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<StepReq>) -> ApiResult<Response> {
    let st = here(&app, &headers).await?;
    st.login_limiter.check(&format!("{}|accounts|second-factor", client_ip(&headers))).await?;
    if req.code.len() > 64 {
        return Err(ApiError(Error::field("code", "is too long")));
    }
    if let Ok(s) = from_ticket(&st, &req.ticket, "found") {
        let outcome = st.kernel.account_found_second_factor(s.tenant, s.user, s.token_version, &req.code).await?;
        return answer(&st, &headers, outcome).await;
    }
    let s = from_ticket(&st, &req.ticket, "mfa")?;
    let s = st.kernel.second_factor(s.tenant, s.user, s.token_version, &req.code).await?;
    answer(&st, &headers, AccountSignIn::Signed(s)).await
}

/// `POST /api/accounts/sign-in/verify`: the code sent to the address finishes making the account.
async fn verify(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<StepReq>) -> ApiResult<Response> {
    let st = here(&app, &headers).await?;
    st.login_limiter.check(&format!("{}|accounts|verify", client_ip(&headers))).await?;
    let m = from_ticket(&st, &req.ticket, "verify")?;
    let s = st.kernel.account_verify(&m, &req.code).await?;
    answer(&st, &headers, AccountSignIn::Signed(s)).await
}

#[derive(Deserialize)]
struct CodeReq {
    email: String,
}

/// `POST /api/accounts/code`: a code to the address, to set a password with (a forgotten one, or
/// a first one). The answer says nothing about the address.
async fn send_code(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<CodeReq>) -> ApiResult<Response> {
    let st = here(&app, &headers).await?;
    st.login_limiter.check(&format!("{}|accounts|code", client_ip(&headers))).await?;
    let until = tokio::time::Instant::now() + CODE_ANSWER;
    let sent = st.kernel.send_email_code(&req.email, "set your Ferrum password").await;
    tokio::time::sleep_until(until).await;
    sent?;
    Ok(Json(json!({"sent": true})).into_response())
}

#[derive(Deserialize)]
struct PasswordReq {
    email: String,
    code: String,
    password: String,
}

/// `POST /api/accounts/password`: sets the account's password with the code sent to the address,
/// and signs in (the authenticator's code next, when the account has one).
async fn set_password(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<PasswordReq>) -> ApiResult<Response> {
    let st = here(&app, &headers).await?;
    st.login_limiter.check(&format!("{}|accounts|{}", client_ip(&headers), req.email.trim().to_lowercase())).await?;
    if req.code.len() > 32 || req.email.len() > 254 {
        return Err(ApiError(Error::Unauthorized("that code is wrong or has expired; ask for a new one".into())));
    }
    let outcome = st.kernel.account_password(&req.email, &req.code, &req.password).await?;
    answer(&st, &headers, outcome).await
}

#[derive(Deserialize)]
struct SsoFinishReq {
    state: String,
    code: String,
    binding: String,
    next: String,
}

/// `POST /api/accounts/sso/finish`: a workspace's single sign-on proved its member (the provider
/// sent the browser back here, the fixed address registered with it). Back to the workspace's
/// host with a code; the membership joins the Ferrum account signed in here, if they share an
/// address.
async fn sso_finish(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<SsoFinishReq>) -> ApiResult<Response> {
    let st = here(&app, &headers).await?;
    st.login_limiter.check(&format!("{}|sso", client_ip(&headers))).await?;
    let again = || ApiError(Error::Unauthorized("sign in again".into()));
    if req.state.len() > 200 || req.code.len() > 4096 || req.binding.len() > 200 || req.next.len() > 2000 {
        return Err(again());
    }
    let pairs: Vec<(String, String)> = req
        .next
        .strip_prefix("/authorize?")
        .ok_or_else(again)?
        .split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_string(), percent_decode(v)))
        .collect();
    let (Some((host, bare)), Some(state)) = (query(&pairs, "host").and_then(|h| host_param(st.shell.secure, h)), query(&pairs, "state").and_then(state_param))
    else {
        return Err(again());
    };
    let m = st.kernel.sso_finish(&req.state, &req.code, &req.binding).await?;
    if workspace_at(&st, &bare).await? != Some(m.tenant) {
        return Err(ApiError(Error::Forbidden("this single sign-on is another workspace's".into())));
    }
    if let Some((ctx, _)) = signed_in(&st, &headers).await {
        st.kernel.join_proven(&ctx, &m).await?;
    }
    let code = st.kernel.mint_host_code(&m, None, &bare).await?;
    Ok(Json(json!({"redirect": callback(&st, &host, &code, state)})).into_response())
}

// ───────────────────────── the signed-in account ─────────────────────────

/// The signed-in account of a request to the accounts host's API.
async fn account(app: &AppContext, headers: &HeaderMap, method: &Method) -> Result<(AppState, Ctx), ApiError> {
    let st = here(app, headers).await?;
    let (ctx, _) = caller_on(&st, method, headers).await?;
    if st.kernel.directory().await? != Some(ctx.tenant) {
        return Err(ApiError(Error::Unauthorized("sign in again".into())));
    }
    Ok((st, ctx))
}

/// `GET /api/accounts/me`: the account and its workspaces (the chooser).
async fn me(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let (st, ctx) = account(&app, &headers, &Method::GET).await?;
    let me = st.kernel.identity(&ctx).await?;
    let mut workspaces = st.kernel.identity_workspaces(&ctx).await?;
    for w in &mut workspaces {
        let url = w["host"].as_str().map(|h| workspace_url(&st, &headers, h));
        w["url"] = json!(url);
    }
    Ok(Json(json!({
        "account": {"id": me.id, "email": me.email, "name": me.name, "verified": me.verified, "two_factor": me.two_factor, "home_region": me.home_region,
            "home": me.home_region.as_deref().and_then(|r| st.shell.regions.iter().find(|(n, _)| n == r).map(|(_, url)| format!("{url}/")))},
        "workspaces": workspaces,
        "region": st.shell.region,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct Onward {
    host: String,
    state: String,
}

/// The workspace a step names, and its host as sent and bare.
async fn onward(st: &AppState, host: &str, state: &str) -> Result<(Uuid, String, String), ApiError> {
    let (Some((host, bare)), Some(_)) = (host_param(st.shell.secure, host), state_param(state)) else {
        return Err(ApiError(Error::invalid("go back to the workspace and sign in from there")));
    };
    let tenant = workspace_at(st, &bare).await?.ok_or_else(|| ApiError(Error::NotFound("no workspace at this address".into())))?;
    Ok((tenant, host, bare))
}

/// Into the workspace when nothing is left to ask, else the step it needs.
async fn go_on(st: &AppState, ctx: &Ctx, tenant: Uuid, host: &str, bare: &str, state: &str) -> ApiResult<Response> {
    let entry = st.kernel.enter(ctx, tenant).await?;
    if let Entry::Ready(m) = &entry {
        let code = st.kernel.mint_host_code(m, Some(ctx.user), bare).await?;
        return Ok(Json(json!({"redirect": callback(st, host, &code, state)})).into_response());
    }
    Ok(Json(need(&entry)).into_response())
}

/// `POST /api/accounts/continue`: on into the workspace as this account.
async fn proceed(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<Onward>) -> ApiResult<Response> {
    let (st, ctx) = account(&app, &headers, &Method::POST).await?;
    let (tenant, host, bare) = onward(&st, &req.host, &req.state).await?;
    go_on(&st, &ctx, tenant, &host, &bare, &req.state).await
}

#[derive(Deserialize)]
struct JoinReq {
    host: String,
    state: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    totp: Option<String>,
}

/// `POST /api/accounts/join`: adds the workspace to the account, proven by its password, a code
/// sent to the address, or (an address already proven) confirming; then on into it.
async fn join(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<JoinReq>) -> ApiResult<Response> {
    let (st, ctx) = account(&app, &headers, &Method::POST).await?;
    st.login_limiter.check(&format!("{}|accounts|join|{}", client_ip(&headers), ctx.user)).await?;
    let (tenant, host, bare) = onward(&st, &req.host, &req.state).await?;
    let long = |v: &Option<String>, max: usize| v.as_ref().is_some_and(|v| v.len() > max);
    if long(&req.password, 256) || long(&req.code, 32) || long(&req.totp, 64) {
        return Err(ApiError(Error::Unauthorized("that did not prove it; check the password or the code".into())));
    }
    let blank = |v: &Option<String>| v.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let (password, code, totp) = (req.password.clone().filter(|p| !p.is_empty()), blank(&req.code), blank(&req.totp));
    st.kernel.join(&ctx, tenant, Proof { password: password.as_deref(), email_code: code.as_deref(), totp: totp.as_deref() }).await?;
    go_on(&st, &ctx, tenant, &host, &bare, &req.state).await
}

#[derive(Deserialize)]
struct JoinCodeReq {
    host: String,
}

/// `POST /api/accounts/join/code`: a code to the account's address, to add a workspace with.
async fn join_code(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<JoinCodeReq>) -> ApiResult<Response> {
    let (st, ctx) = account(&app, &headers, &Method::POST).await?;
    st.login_limiter.check(&format!("{}|accounts|code", client_ip(&headers))).await?;
    let Some((_, bare)) = host_param(st.shell.secure, &req.host) else {
        return Err(ApiError(Error::invalid("go back to the workspace and sign in from there")));
    };
    let name = match workspace_at(&st, &bare).await? {
        Some(t) => st.shell.workspace(t).await?.name.clone(),
        None => return Err(ApiError(Error::NotFound("no workspace at this address".into()))),
    };
    let me = st.kernel.identity(&ctx).await?;
    let until = tokio::time::Instant::now() + CODE_ANSWER;
    let sent = st.kernel.send_email_code(&me.email, &format!("add {name} to your Ferrum account")).await;
    tokio::time::sleep_until(until).await;
    sent?;
    Ok(Json(json!({"sent": true})).into_response())
}

/// `POST /api/accounts/sign-out-everywhere`: ends every session of the account and of every
/// workspace it joined, on every device.
async fn sign_out_everywhere(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let (st, ctx) = account(&app, &headers, &Method::POST).await?;
    st.kernel.identity_sign_out_everywhere(&ctx).await?;
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut().append(header::SET_COOKIE, shell::clear_cookie(st.shell.secure));
    Ok(res)
}

// ───────────────────────── accounts across regions ─────────────────────────

/// The region whose accounts host has this callback address (`{its accounts host}/regions/callback`).
fn region_of_callback<'a>(st: &'a AppState, redirect_uri: &str) -> Option<&'a (String, String)> {
    st.shell.regions.iter().find(|(_, url)| redirect_uri == format!("{url}/regions/callback"))
}

/// The cookie that keeps a sign-in through another region's accounts host: `state`, the PKCE
/// verifier, the region and where to go back to.
fn region_cookie_name(secure: bool) -> &'static str {
    if secure { "__Host-ferrum-region" } else { "ferrum_region" }
}

/// Home region, `GET /regions/authorize?redirect_uri=…&state=…&code_challenge=…&code_challenge_method=S256`:
/// another region's accounts host asks who is signed in here (signing in first), and gets back a
/// one-use code bound to its challenge. Only the regions this cluster lists are answered.
async fn region_authorize(st: &AppState, headers: &HeaderMap, uri: &Uri, q: &[(String, String)]) -> ApiResult<Response> {
    let bad = || {
        status_page(
            StatusCode::BAD_REQUEST,
            "This sign-in link does not work",
            "Go back to where you came from and sign in again.",
            &[("Your workspaces", "/")],
            None,
        )
    };
    let (Some(redirect_uri), Some(state), Some(challenge)) = (query(q, "redirect_uri"), query(q, "state").and_then(state_param), query(q, "code_challenge"))
    else {
        return Ok(bad());
    };
    if query(q, "code_challenge_method") != Some("S256") || region_of_callback(st, redirect_uri).is_none() {
        return Ok(bad());
    }
    let Some((ctx, _)) = signed_in(st, headers).await else {
        return Ok(Redirect::to(&format!("/login?next={}", enc(&format!("/regions/authorize?{}", uri.query().unwrap_or_default())))).into_response());
    };
    let host = reqwest::Url::parse(redirect_uri).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let code = st.kernel.mint_region_code(&ctx, &host, challenge).await?;
    let mut res = Redirect::to(&format!("{redirect_uri}?code={code}&state={state}")).into_response();
    res.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    Ok(res)
}

#[derive(Deserialize)]
struct RegionTokenReq {
    code: String,
    code_verifier: String,
    redirect_uri: String,
}

/// Home region, `POST /api/accounts/region-token`: the visited region's server takes its code,
/// with the verifier of the challenge it was bound to, and learns who the account is.
async fn region_token(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<RegionTokenReq>) -> ApiResult<Response> {
    let st = here(&app, &headers).await?;
    st.public_limiter.check(&format!("{}|region-token", client_ip(&headers))).await?;
    if region_of_callback(&st, &req.redirect_uri).is_none() {
        return Err(ApiError(Error::Forbidden("not a region of this cluster's".into())));
    }
    let host = reqwest::Url::parse(&req.redirect_uri).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let visitor = st.kernel.redeem_region_code(&req.code, &host, &req.code_verifier).await?;
    let mut res = Json(visitor).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// Visited region, `GET /regions/start?region=…&next=…`: signing in with a Ferrum account that
/// lives in another region: off to its accounts host, with a PKCE challenge whose verifier and a
/// `state` this host keeps in a cookie (ten minutes).
async fn region_start(st: &AppState, headers: &HeaderMap, q: &[(String, String)]) -> ApiResult<Response> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let region = query(q, "region").unwrap_or_default().to_ascii_lowercase();
    let (Some((_, url)), Some(own)) = (st.shell.regions.iter().find(|(n, _)| *n == region), st.shell.accounts_origin(headers)) else {
        return Ok(not_found());
    };
    let next = shell::safe_next(query(q, "next").unwrap_or("/"));
    let verifier = b64.encode(rand_bytes());
    let state = Uuid::new_v4().simple().to_string();
    let to = format!(
        "{url}/regions/authorize?redirect_uri={}&state={state}&code_challenge={}&code_challenge_method=S256",
        enc(&format!("{own}/regions/callback")),
        ferrum_kernel::identity::pkce_challenge(&verifier)
    );
    let mut res = Redirect::to(&to).into_response();
    let secure = st.shell.secure;
    let kept = format!("{state}.{verifier}.{}.{}", b64.encode(&region), b64.encode(&next));
    res.headers_mut().append(header::SET_COOKIE, shell::host_cookie(secure, region_cookie_name(secure), &kept, Duration::from_secs(600)));
    Ok(res)
}

fn rand_bytes() -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    b[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    b
}

/// Visited region, `GET /regions/callback?code=…&state=…`: back from the account's home with a
/// code; this server takes it there (with the verifier) and signs the visiting account in here.
async fn region_callback(st: &AppState, headers: &HeaderMap, q: &[(String, String)]) -> ApiResult<Response> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let secure = st.shell.secure;
    let failed = |message: &str| {
        let mut res = status_page(StatusCode::BAD_REQUEST, "Signing in did not finish", message, &[("Sign in again", "/login")], None);
        res.headers_mut().append(header::SET_COOKIE, shell::host_cookie(secure, region_cookie_name(secure), "", Duration::ZERO));
        res
    };
    let kept = shell::cookie(headers, region_cookie_name(secure)).unwrap_or_default();
    let parts: Vec<&str> = kept.split('.').collect();
    let (Some(code), Some(state), [kept_state, verifier, region, next]) = (query(q, "code"), query(q, "state"), parts.as_slice()) else {
        return Ok(failed("It was started in another tab, or took too long. Sign in again."));
    };
    if !super::system::same_secret(kept_state, state) {
        return Ok(failed("It was started in another tab, or took too long. Sign in again."));
    }
    let decode = |s: &str| b64.decode(s).ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
    let (region, next) = (decode(region), decode(next));
    let (Some((_, url)), Some(own)) = (st.shell.regions.iter().find(|(n, _)| *n == region), st.shell.accounts_origin(headers)) else {
        return Ok(failed("That region is not one this service knows."));
    };
    let Some(client) = region_client(url, Duration::from_secs(10)) else { return Ok(failed("That region cannot be reached.")) };
    let asked = client
        .post(format!("{url}/api/accounts/region-token"))
        .json(&json!({"code": code, "code_verifier": verifier, "redirect_uri": format!("{own}/regions/callback")}))
        .send()
        .await;
    let visitor: ferrum_kernel::identity::Visitor = match asked {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(_) => return Ok(failed("The account's region answered something unexpected.")),
        },
        Ok(_) => return Ok(failed("The account's region did not confirm the sign-in. Sign in again.")),
        Err(e) => {
            tracing::warn!(%region, "a region's accounts host is unreachable: {e}");
            return Ok(failed("The account's region cannot be reached just now. Try again in a while."));
        }
    };
    let s = match st.kernel.visiting_identity(&region, &visitor).await {
        Ok(s) => s,
        Err(Error::Conflict(m) | Error::Forbidden(m)) => return Ok(failed(&m)),
        Err(e) => return Err(e.into()),
    };
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let ttl = st.shell.session_ttl;
    let sid = st.kernel.open_session(&s, ttl, ua, &client_ip(headers), None).await?;
    let mut res = Redirect::to(&shell::safe_next(&next)).into_response();
    let h = res.headers_mut();
    h.append(header::SET_COOKIE, shell::set_cookie(secure, &super::auth::device_token(st, &s, ttl.as_secs(), "sid", sid)?, ttl));
    h.append(header::SET_COOKIE, shell::host_cookie(secure, region_cookie_name(secure), "", Duration::ZERO));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .add("/_directory", get(directory))
        .add("/api/accounts/sign-in", post(sign_in))
        .add("/api/accounts/sign-in/second-factor", post(second_factor))
        .add("/api/accounts/sign-in/verify", post(verify))
        .add("/api/accounts/code", post(send_code))
        .add("/api/accounts/password", post(set_password))
        .add("/api/accounts/sso/finish", post(sso_finish))
        .add("/api/accounts/me", get(me))
        .add("/api/accounts/continue", post(proceed))
        .add("/api/accounts/join", post(join))
        .add("/api/accounts/join/code", post(join_code))
        .add("/api/accounts/sign-out-everywhere", post(sign_out_everywhere))
        .add("/api/accounts/region-token", post(region_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_in_and_out() {
        assert_eq!(enc("acme.localhost:5160"), "acme.localhost%3A5160");
        assert_eq!(host_param(false, "Acme.localhost:5160"), Some(("acme.localhost:5160".into(), "acme.localhost".into())));
        assert_eq!(host_param(true, "acme.ferrum.cloud"), Some(("acme.ferrum.cloud".into(), "acme.ferrum.cloud".into())));
        assert_eq!(host_param(true, "acme.ferrum.cloud:8443"), None, "no ports over https");
        assert_eq!(host_param(false, "acme.localhost:"), None);
        assert_eq!(host_param(false, "evil.com/x"), None);
        assert_eq!(host_param(false, "localhost"), None, "a host of ours has a dot");
        assert_eq!(state_param("abc-DEF_12"), Some("abc-DEF_12"));
        assert_eq!(state_param("a b"), None);
        assert_eq!(state_param(""), None);
    }
}
