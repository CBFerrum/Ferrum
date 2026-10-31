//! OAuth 2.1 authorization server for MCP and other API clients: discovery (RFC 8414, RFC 9728),
//! dynamic client registration (RFC 7591), authorization code + PKCE S256, rotating refresh
//! tokens and revocation (RFC 7009). The user approves on the consent page the server draws
//! (`/oauth/authorize`, signed in with the browser's session cookie); `POST /api/oauth/authorize`
//! does the same for a page of its own. Access tokens are ordinary Ferrum session JWTs, so `/mcp`
//! and the REST API both accept them.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, client_ip};
use crate::state::app_state;
use axum::Json;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use ferrum_kernel::Error;
use ferrum_kernel::oauth::{Grant, SCOPE};
use loco_rs::app::AppContext;
use loco_rs::auth::jwt::JWT;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Map, json};

const ACCESS_TTL_SECS: u64 = 3600;

/// The public origin as the client sees it (honours the proxy's forwarded headers).
pub fn base_url(headers: &HeaderMap) -> String {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(|v| v.split(',').next().unwrap_or(v).trim().to_string());
    let host = get("x-forwarded-host").or_else(|| get("host")).unwrap_or_else(|| "localhost".into());
    let proto =
        get("x-forwarded-proto").unwrap_or_else(|| if host.starts_with("localhost") || host.starts_with("127.") { "http".into() } else { "https".into() });
    format!("{proto}://{host}")
}

/// RFC 9728 metadata location advertised in `WWW-Authenticate` by protected endpoints.
pub fn resource_metadata_url(headers: &HeaderMap) -> String {
    format!("{}/.well-known/oauth-protected-resource", base_url(headers))
}

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp.headers_mut().insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    resp
}

/// RFC 6749 §5.2 error response.
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    no_store((status, Json(json!({"error": error, "error_description": description}))).into_response())
}

async fn protected_resource(headers: HeaderMap) -> Response {
    let base = base_url(&headers);
    Json(json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "scopes_supported": [SCOPE],
        "resource_name": "Ferrum",
    }))
    .into_response()
}

async fn authorization_server(headers: HeaderMap) -> Response {
    let base = base_url(&headers);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "revocation_endpoint": format!("{base}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [SCOPE],
    }))
    .into_response()
}

#[derive(Deserialize)]
struct RegisterReq {
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: Option<String>,
}

async fn register(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<RegisterReq>) -> Response {
    let st = match app_state(&app) {
        Ok(st) => st,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = st.login_limiter.check(&format!("oauth-register|{}", client_ip(&headers))).await {
        return ApiError(e).into_response();
    }
    match st.kernel.oauth_register(req.client_name, req.redirect_uris).await {
        Ok(c) => no_store(
            (
                StatusCode::CREATED,
                Json(json!({
                    "client_id": c.id,
                    "client_id_issued_at": chrono::Utc::now().timestamp(),
                    "client_name": c.name,
                    "redirect_uris": c.redirect_uris,
                    "token_endpoint_auth_method": "none",
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                    "scope": SCOPE,
                })),
            )
                .into_response(),
        ),
        Err(Error::Invalid(m)) => oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", &m),
        Err(e) => ApiError(e).into_response(),
    }
}

/// For the consent page: who is asking, and where the answer goes.
async fn client_info(State(app): State<AppContext>, Path(id): Path<String>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let c = st.kernel.oauth_client(&id).await?;
    Ok(Json(json!({"client_id": c.id, "client_name": c.name, "redirect_uris": c.redirect_uris})).into_response())
}

#[derive(Deserialize)]
struct AuthorizeReq {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    #[serde(default = "s256")]
    code_challenge_method: String,
    #[serde(default)]
    state: Option<String>,
}

fn s256() -> String {
    "S256".into()
}

/// The signed-in user approved the client: returns where to send the browser.
async fn authorize(Auth(ctx): Auth, State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<AuthorizeReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let code = st.kernel.oauth_issue_code(&ctx, &req.client_id, &req.redirect_uri, &req.code_challenge, &req.code_challenge_method).await?;
    let url = back_to(&req.redirect_uri, &[("code", &code), ("state", req.state.as_deref().unwrap_or_default()), ("iss", &base_url(&headers))]);
    Ok(no_store(Json(json!({"redirect_to": url})).into_response()))
}

/// The client's redirect URI with `params` added (empty values left out).
fn back_to(redirect_uri: &str, params: &[(&str, &str)]) -> String {
    let mut url = redirect_uri.to_string();
    for (k, v) in params.iter().filter(|(_, v)| !v.is_empty()) {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&format!("{k}={}", urlencode(v)));
    }
    url
}

/// An authorization request, as the client sends it to the consent page.
#[derive(Deserialize)]
struct ConsentReq {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default = "s256")]
    code_challenge_method: String,
    #[serde(default)]
    state: String,
    /// The person's answer (the page's form): `allow`, else denied.
    #[serde(default)]
    decision: String,
}

impl ConsentReq {
    fn query(&self) -> String {
        let p = [
            ("response_type", self.response_type.as_str()),
            ("client_id", &self.client_id),
            ("redirect_uri", &self.redirect_uri),
            ("code_challenge", &self.code_challenge),
            ("code_challenge_method", &self.code_challenge_method),
            ("state", &self.state),
        ];
        back_to("/oauth/authorize", &p)
    }
}

/// The client exists and returns only to an address it registered; else what the person is told
/// (and nothing is sent to that address).
async fn checked(st: &crate::state::AppState, r: &ConsentReq) -> ApiResult<Result<ferrum_kernel::oauth::OAuthClient, &'static str>> {
    if r.response_type != "code" {
        return Ok(Err("This sign-in link is not valid (response_type must be code)."));
    }
    if r.client_id.is_empty() || r.redirect_uri.is_empty() || r.code_challenge.is_empty() {
        return Ok(Err("This sign-in link is incomplete (client_id, redirect_uri and code_challenge are required)."));
    }
    if r.code_challenge_method != "S256" {
        return Ok(Err("This app uses an unsupported sign-in method (PKCE S256 is required)."));
    }
    let client = match st.kernel.oauth_client(&r.client_id).await {
        Ok(c) => c,
        Err(Error::NotFound(_)) => return Ok(Err("This app is not registered with Ferrum.")),
        Err(e) => return Err(e.into()),
    };
    if !ferrum_kernel::oauth::redirect_matches(&client.redirect_uris, &r.redirect_uri) {
        return Ok(Err("This app asked to return to an address it did not register. For your safety, nothing was shared."));
    }
    Ok(Ok(client))
}

/// Whether a person may use an isolated app of their workspace on its origin of its own
/// (`app:{app}:{workspace}` clients, docs/plan/apps-ui.md shape D): the workspace is theirs, the
/// app is installed there with its interface isolated, and they may use it. `Err`: what they are
/// told. Asked at consent and again whenever tokens are issued, so uninstalling the app or losing
/// access to it ends its sign-ins.
async fn app_usable(st: &crate::state::AppState, ctx: &ferrum_kernel::Ctx, app_id: &str, slug: &str) -> ApiResult<Result<String, &'static str>> {
    if st.settings.apps_base.trim().is_empty() {
        return Ok(Err("This server gives apps no addresses of their own."));
    }
    if st.shell.workspace(ctx.tenant).await?.slug != slug {
        return Ok(Err("This app belongs to another workspace. Sign in to that one to use it."));
    }
    let apps = st.apps.shell_apps(ctx.tenant).await?;
    let Some(app) = apps.iter().find(|a| a.id == app_id && a.isolated) else { return Ok(Err("This app is not installed in this workspace.")) };
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    if !app.items("shell.app").any(|i| crate::shell::may_use(ctx, &kind, i)) {
        return Ok(Err("You do not have access to this app."));
    }
    Ok(Ok(app.manifest.name.clone()))
}

/// An isolated app's client, made (or brought up to date) when first used: for an app the person
/// may use, returning only to that app's origin of its own. Other clients pass untouched.
async fn app_client(st: &crate::state::AppState, ctx: &ferrum_kernel::Ctx, headers: &HeaderMap, client_id: &str) -> ApiResult<Result<(), &'static str>> {
    let Some(rest) = client_id.strip_prefix("app:") else { return Ok(Ok(())) };
    let Some((app_id, slug)) = rest.split_once(':') else { return Ok(Err("This app is not registered with Ferrum.")) };
    let name = match app_usable(st, ctx, app_id, slug).await? {
        Ok(name) => name,
        Err(problem) => return Ok(Err(problem)),
    };
    let host = crate::shell::request_host(headers).unwrap_or_default();
    let Some(origin) = crate::shell::app_origin(app_id, slug, &st.settings.apps_base, host, st.shell.secure) else {
        return Ok(Err("This app has no address of its own here: open it in the workspace."));
    };
    st.kernel.oauth_ensure_client(client_id, &name, &[format!("{origin}/_ferrum/callback")]).await?;
    Ok(Ok(()))
}

/// The consent page: the theme and the sign-in page's look, no scripts, never in a frame.
fn consent_page(status: StatusCode, body: &str) -> Response {
    use crate::shell::{page_response, theme_hash};
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Allow access · Ferrum</title><link rel="icon" href="/_shell/v1/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_shell/theme/{}.css"><link rel="stylesheet" href="/_shell/v1/pages.css"></head><body><main class="ferrum-login" id="main"><img class="ferrum-mark" src="/_shell/v1/icon.svg" alt="" width="40" height="40"><h1>Allow access to Ferrum?</h1>{body}</main></body></html>"#,
        theme_hash()
    );
    // No form-action: after the answer the browser goes on to the client's own address.
    let csp = "default-src 'none'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; object-src 'none'";
    no_store(page_response(status, html, csp))
}

fn refused(problem: &str) -> Response {
    consent_page(StatusCode::BAD_REQUEST, &format!(r#"<p class="ferrum-error" role="alert">{}</p>"#, crate::shell::esc(problem)))
}

/// The person signed in with the browser's session cookie; `None` sends them to sign in first.
async fn signed_in(st: &crate::state::AppState, method: &Method, headers: &HeaderMap) -> ApiResult<Option<ferrum_kernel::Ctx>> {
    if crate::shell::session_cookie(headers, st.shell.secure).is_none() {
        return Ok(None);
    }
    match crate::extract::caller_on(st, method, headers).await {
        Ok((ctx, _)) => Ok(Some(ctx)),
        Err(ApiError(Error::Unauthorized(_))) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `GET /oauth/authorize`: the authorization endpoint, a consent page for the signed-in person
/// (anyone else signs in first and comes back).
async fn consent(State(app): State<AppContext>, headers: HeaderMap, uri: Uri, Query(req): Query<ConsentReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let Some(ctx) = signed_in(&st, &Method::GET, &headers).await? else {
        let here = uri.path_and_query().map_or("/oauth/authorize", |p| p.as_str());
        return Ok(Redirect::to(&format!("/login?next={}", urlencode(here))).into_response());
    };
    if let Err(problem) = app_client(&st, &ctx, &headers, &req.client_id).await? {
        return Ok(refused(problem));
    }
    let client = match checked(&st, &req).await? {
        Ok(c) => c,
        Err(problem) => return Ok(refused(problem)),
    };
    let esc = crate::shell::esc;
    let workspace = st.shell.workspace(ctx.tenant).await?;
    let host = req.redirect_uri.split_once("://").map_or(req.redirect_uri.as_str(), |(_, rest)| rest.split(['/', '?']).next().unwrap_or(rest));
    let hidden: String = [
        ("client_id", &req.client_id),
        ("redirect_uri", &req.redirect_uri),
        ("code_challenge", &req.code_challenge),
        ("code_challenge_method", &req.code_challenge_method),
        ("state", &req.state),
    ]
    .iter()
    .map(|(k, v)| format!(r#"<input type="hidden" name="{k}" value="{}">"#, esc(v)))
    .collect();
    // An installed app's own page asks for no more than the workspace allowed it.
    let reach = if req.client_id.starts_with("app:") {
        "<li>It can do what this workspace allowed when it installed the app, within your permissions.</li><li>Everything it does is recorded in the audit trail under your name.</li><li>It stays signed in while you use it; signing out everywhere ends it.</li>"
    } else {
        "<li>It can read and change everything you can, with your permissions.</li><li>Everything it does is recorded in the audit trail under your name.</li><li>Access lasts until you revoke it or sign out everywhere.</li>"
    };
    let body = format!(
        r#"<p><strong>{}</strong> wants to use Ferrum as <strong>{}</strong> in the <strong>{}</strong> workspace.</p><ul>{reach}</ul><p class="ferrum-hint">It will return to {}.</p><form method="post" action="/oauth/authorize" class="ferrum-consent">{hidden}<button class="ferrum-button ferrum-button-quiet" type="submit" name="decision" value="deny">Deny</button><button class="ferrum-button" type="submit" name="decision" value="allow">Allow</button></form>"#,
        esc(&client.name),
        esc(&ctx.email),
        esc(&workspace.name),
        esc(host)
    );
    Ok(consent_page(StatusCode::OK, &body))
}

/// The consent page's answer: back to the client with a one-time code, or with `access_denied`.
async fn consent_answer(State(app): State<AppContext>, headers: HeaderMap, Form(mut req): Form<ConsentReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    req.response_type = "code".into();
    let Some(ctx) = signed_in(&st, &Method::POST, &headers).await? else {
        return Ok(Redirect::to(&format!("/login?next={}", urlencode(&req.query()))).into_response());
    };
    if let Err(problem) = app_client(&st, &ctx, &headers, &req.client_id).await? {
        return Ok(refused(problem));
    }
    let client = match checked(&st, &req).await? {
        Ok(c) => c,
        Err(problem) => return Ok(refused(problem)),
    };
    let to = if req.decision == "allow" {
        let code = st.kernel.oauth_issue_code(&ctx, &client.id, &req.redirect_uri, &req.code_challenge, &req.code_challenge_method).await?;
        back_to(&req.redirect_uri, &[("code", &code), ("state", &req.state), ("iss", &base_url(&headers))])
    } else {
        back_to(&req.redirect_uri, &[("error", "access_denied"), ("state", &req.state)])
    };
    Ok(no_store(Redirect::to(&to).into_response()))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Deserialize)]
struct TokenReq {
    grant_type: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
}

async fn token(State(app): State<AppContext>, headers: HeaderMap, Form(req): Form<TokenReq>) -> Response {
    let st = match app_state(&app) {
        Ok(st) => st,
        Err(e) => return e.into_response(),
    };
    if st.login_limiter.check(&format!("oauth-token|{}", client_ip(&headers))).await.is_err() {
        return oauth_error(StatusCode::TOO_MANY_REQUESTS, "slow_down", "too many token requests; try again shortly");
    }
    let Some(client_id) = req.client_id.as_deref().filter(|c| !c.is_empty()) else {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "client_id is required");
    };
    let grant = match req.grant_type.as_str() {
        "authorization_code" => {
            let (Some(code), Some(redirect), Some(verifier)) = (req.code.as_deref(), req.redirect_uri.as_deref(), req.code_verifier.as_deref()) else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "code, redirect_uri and code_verifier are required");
            };
            st.kernel.oauth_redeem_code(client_id, code, redirect, verifier).await
        }
        "refresh_token" => {
            let Some(refresh) = req.refresh_token.as_deref() else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "refresh_token is required");
            };
            st.kernel.oauth_refresh(client_id, refresh).await
        }
        other => return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", &format!("grant_type {other} is not supported")),
    };
    let g = match grant {
        Ok(g) => g,
        Err(Error::Invalid(_)) | Err(Error::NotFound(_)) => {
            return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "the code or refresh token is invalid, expired or already used");
        }
        Err(e) => return ApiError(e).into_response(),
    };
    // An app's own page (shape D) gets app tokens: narrowed to the app, a few minutes long.
    let (minted, ttl) = match client_id.strip_prefix("app:").and_then(|r| r.split_once(':')) {
        Some((app, _)) => (app_access_token(&st.jwt_secret, &g, client_id, app), super::auth::APP_TOKEN_SECS),
        None => (access_token(&st.jwt_secret, &g, client_id), ACCESS_TTL_SECS),
    };
    let mut answer = match minted {
        Ok(access) => json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ttl,
            "refresh_token": g.refresh_token,
            "scope": g.scope,
        }),
        Err(e) => return ApiError(e).into_response(),
    };
    // An app's page: only while the person may still use the app, and it greets them (it can
    // read nothing else about them).
    if let Some((app, slug)) = client_id.strip_prefix("app:").and_then(|r| r.split_once(':')) {
        let ctx = match st.kernel.load_ctx(g.tenant, g.user, Some(g.token_version), String::new()).await {
            Ok(ctx) => ctx,
            Err(e) => return ApiError(e).into_response(),
        };
        match app_usable(&st, &ctx, app, slug).await {
            Ok(Ok(_)) => answer["user"] = json!({"email": ctx.email, "name": ctx.full_name}),
            Ok(Err(problem)) => return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", problem),
            Err(e) => return e.into_response(),
        }
    }
    no_store(Json(answer).into_response())
}

/// An app token for an app's own page (shape D): its claims say which app, so the server narrows
/// every call to it (extract.rs), and it lasts minutes; the refresh token renews it.
fn app_access_token(secret: &str, g: &Grant, client_id: &str, app: &str) -> Result<String, Error> {
    let mut claims = Map::new();
    claims.insert("tenant".into(), json!(g.tenant.to_string()));
    claims.insert("tv".into(), json!(g.token_version));
    claims.insert("client_id".into(), json!(client_id));
    claims.insert("app".into(), json!(app));
    JWT::new(secret).generate_token(super::auth::APP_TOKEN_SECS, g.user.to_string(), claims).map_err(|e| Error::Internal(format!("token: {e}")))
}

fn access_token(secret: &str, g: &Grant, client_id: &str) -> Result<String, Error> {
    let mut claims = Map::new();
    claims.insert("tenant".into(), json!(g.tenant.to_string()));
    claims.insert("tv".into(), json!(g.token_version));
    claims.insert("client_id".into(), json!(client_id));
    claims.insert("scope".into(), json!(g.scope));
    JWT::new(secret).generate_token(ACCESS_TTL_SECS, g.user.to_string(), claims).map_err(|e| Error::Internal(format!("token: {e}")))
}

#[derive(Deserialize)]
struct RevokeReq {
    token: String,
}

async fn revoke(State(app): State<AppContext>, Form(req): Form<RevokeReq>) -> Response {
    match app_state(&app) {
        Ok(st) => match st.kernel.oauth_revoke(&req.token).await {
            Ok(()) => no_store(StatusCode::OK.into_response()),
            Err(e) => ApiError(e).into_response(),
        },
        Err(e) => e.into_response(),
    }
}

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .add("/.well-known/oauth-protected-resource", get(protected_resource))
        .add("/.well-known/oauth-protected-resource/mcp", get(protected_resource))
        .add("/.well-known/oauth-authorization-server", get(authorization_server))
        .add("/oauth/authorize", get(consent).post(consent_answer))
        .add("/oauth/register", post(register))
        .add("/oauth/token", post(token))
        .add("/oauth/revoke", post(revoke))
        .add("/api/oauth/clients/{id}", get(client_info))
        .add("/api/oauth/authorize", post(authorize))
}

#[cfg(test)]
mod tests {
    use super::back_to;

    #[test]
    fn answers_go_back_with_their_parameters() {
        assert_eq!(
            back_to("http://127.0.0.1:3118/cb", &[("code", "a b"), ("state", ""), ("iss", "https://x.test")]),
            "http://127.0.0.1:3118/cb?code=a%20b&iss=https%3A%2F%2Fx.test"
        );
        assert_eq!(back_to("myapp://cb?x=1", &[("error", "access_denied"), ("state", "s/1")]), "myapp://cb?x=1&error=access_denied&state=s%2F1");
    }
}
