//! OAuth 2.1 authorization server for MCP and other API clients: discovery (RFC 8414, RFC 9728),
//! dynamic client registration (RFC 7591), authorization code + PKCE S256, rotating refresh
//! tokens and revocation (RFC 7009). The user approves on the web app's consent page
//! (`/app/oauth/authorize`), which calls `POST /api/oauth/authorize` with their session.
//! Access tokens are ordinary Ferrum session JWTs, so `/mcp` and the REST API both accept them.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, client_ip};
use crate::state::app_state;
use axum::Json;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
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
        "authorization_endpoint": format!("{base}/app/oauth/authorize"),
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
    if let Err(e) = st.login_limiter.check(&format!("oauth-register|{}", client_ip(&headers))) {
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
    let mut url = req.redirect_uri.clone();
    url.push(if url.contains('?') { '&' } else { '?' });
    url.push_str(&format!("code={}", urlencode(&code)));
    if let Some(state) = req.state.filter(|s| !s.is_empty()) {
        url.push_str(&format!("&state={}", urlencode(&state)));
    }
    url.push_str(&format!("&iss={}", urlencode(&base_url(&headers))));
    Ok(no_store(Json(json!({"redirect_to": url})).into_response()))
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
    if st.login_limiter.check(&format!("oauth-token|{}", client_ip(&headers))).is_err() {
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
    match grant {
        Ok(g) => match access_token(&st.jwt_secret, &g, client_id) {
            Ok(access) => no_store(
                Json(json!({
                    "access_token": access,
                    "token_type": "Bearer",
                    "expires_in": ACCESS_TTL_SECS,
                    "refresh_token": g.refresh_token,
                    "scope": g.scope,
                }))
                .into_response(),
            ),
            Err(e) => ApiError(e).into_response(),
        },
        Err(Error::Invalid(_)) | Err(Error::NotFound(_)) => {
            oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "the code or refresh token is invalid, expired or already used")
        }
        Err(e) => ApiError(e).into_response(),
    }
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
        .add("/oauth/register", post(register))
        .add("/oauth/token", post(token))
        .add("/oauth/revoke", post(revoke))
        .add("/api/oauth/clients/{id}", get(client_info))
        .add("/api/oauth/authorize", post(authorize))
}
