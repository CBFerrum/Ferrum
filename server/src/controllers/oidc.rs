//! OpenID Connect for the programs that run beside Ferrum (crates/kernel/src/oidc.rs; docs/api.md, "Signing in to
//! programs beside Ferrum"): the discovery document, the published keys and the user-info, a companion's way through the
//! authorization endpoint (no consent page; a page that asks the person to confirm it is them when the request wants a
//! newer proof) and through the token endpoint (its secret, then its code). They answer only on an address that is one
//! workspace's own ([`issuer`]); everywhere else, nothing.

use crate::error::{ApiError, ApiResult};
use crate::shell::{Place, request_host};
use crate::state::{AppState, app_state};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine as _;
use ferrum_kernel::Error;
use ferrum_kernel::oidc::{AuthRequest, Authorized, CLIENT_PREFIX, Client, SCOPES, TOKEN_SECS};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// The workspace a request's address is one of, and that address as the issuer (`https://{host}`, its port on a
/// development server): a host that serves one workspace's shell (its subdomain, a name under the base domain, a domain
/// of its own, an operator's address, or the installation's one workspace on its own host). Not one in its leaving
/// period, a forwarding address (it serves nothing), a shared host (no workspace's own), the accounts or hooks host, a
/// portal or a site, or a sandbox (nothing it does reaches outside, and a companion is outside).
pub(crate) async fn issuer(st: &AppState, headers: &HeaderMap) -> ApiResult<Option<(Uuid, String)>> {
    if !on(st) {
        return Ok(None);
    }
    let Place::Workspace(tenant) = st.shell.place(headers).await? else { return Ok(None) };
    if st.shell.leaving(headers).await?.is_some() || st.shell.workspace(tenant).await?.sandbox.is_some() {
        return Ok(None);
    }
    // Its name, without its port (`localhost` too, a development server's); never an IPv6 literal.
    let host = request_host(headers).and_then(|h| h.split(':').next()).map(str::to_ascii_lowercase);
    let Some(host) = host.filter(|h| h == "localhost" || ferrum_kernel::shell::normalize_host(h).as_deref() == Some(h.as_str())) else { return Ok(None) };
    Ok(st.shell.origin_of(&host, headers).map(|origin| (tenant, origin)))
}

/// Whether programs beside Ferrum sign anyone in through it (the setting `openid`, which applies at once): off, every
/// route here answers as if it were not there, and the authorization and token endpoints as for a client they do not
/// know (oauth.rs asks this first).
pub(crate) fn on(st: &AppState) -> bool {
    st.live_settings.now().snapshot.get("openid").1.is_none_or(|v| !v.trim().eq_ignore_ascii_case("false"))
}

fn not_here() -> Response {
    ApiError(Error::NotFound("this address signs nobody in to other programs: use one of the workspace's own addresses".into())).into_response()
}

fn no_store(mut res: Response) -> Response {
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res.headers_mut().insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    res
}

/// `GET /.well-known/openid-configuration`: what a relying party needs, for this workspace's address.
async fn discovery(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let Some((_, iss)) = issuer(&st, &headers).await? else { return Ok(not_here()) };
    Ok(Json(json!({
        "issuer": iss,
        "authorization_endpoint": format!("{iss}/oauth/authorize"),
        "token_endpoint": format!("{iss}/oauth/token"),
        "userinfo_endpoint": format!("{iss}/oauth/userinfo"),
        "jwks_uri": format!("{iss}/oauth/jwks"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["EdDSA"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": SCOPES,
        "claims_supported": ["iss", "sub", "aud", "azp", "exp", "iat", "auth_time", "nonce", "amr", "name", "email", "email_verified"],
        "prompt_values_supported": ["none", "login"],
        "authorization_response_iss_parameter_supported": true,
        "claims_parameter_supported": false,
        "request_parameter_supported": false,
        "request_uri_parameter_supported": false,
    }))
    .into_response())
}

/// `GET /oauth/jwks`: the workspace's published signing keys, the one that signs first.
async fn jwks(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let Some((tenant, _)) = issuer(&st, &headers).await? else { return Ok(not_here()) };
    let keys: Vec<Value> = st.kernel.openid_keys(tenant).await?.iter().map(|k| k.jwk()).collect();
    let mut res = Json(json!({"keys": keys})).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
    Ok(res)
}

/// A refusal of the token or user-info endpoint, counted against the caller's address like a failed sign-in: past the
/// limit, `429` in its place.
async fn counted(st: &AppState, headers: &HeaderMap, what: &str, answer: Response) -> Response {
    if st.login_limiter.check_from(headers, &format!("oidc-{what}")).await.is_err() {
        return oauth_error(StatusCode::TOO_MANY_REQUESTS, "slow_down", "too many failed requests from here; try again in a minute");
    }
    answer
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    let mut res = no_store((status, Json(json!({"error": error, "error_description": description}))).into_response());
    if error == "invalid_client" {
        res.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"ferrum\""));
    }
    res
}

/// `GET`/`POST /oauth/userinfo`: who the access token's person is, while the browser session it came from is live.
async fn userinfo(State(app): State<AppContext>, headers: HeaderMap) -> Response {
    let st = match app_state(&app) {
        Ok(st) => st,
        Err(e) => return e.into_response(),
    };
    let bearer_error = |error: &str| {
        let mut res = no_store(StatusCode::UNAUTHORIZED.into_response());
        let value = if error.is_empty() { "Bearer".to_string() } else { format!("Bearer error=\"{error}\"") };
        res.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("Bearer")));
        res
    };
    let (tenant, iss) = match issuer(&st, &headers).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_here(),
        Err(e) => return e.into_response(),
    };
    let Some(token) = crate::extract::bearer(&headers) else { return bearer_error("") };
    match st.kernel.openid_userinfo(tenant, &token, &iss).await {
        Ok(claims) => no_store(Json(Value::Object(claims)).into_response()),
        Err(Error::Unauthorized(_)) => counted(&st, &headers, "userinfo", bearer_error("invalid_token")).await,
        Err(e) => ApiError(e).into_response(),
    }
}

/// The client's return address with `params` added (oauth.rs, `back_to`), as a redirect that nothing keeps.
fn back(redirect_uri: &str, params: &[(&str, &str)]) -> Response {
    let mut res = no_store(Redirect::to(&super::oauth::back_to(redirect_uri, params)).into_response());
    res.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    res
}

/// The authorization endpoint for a companion's client `client_id` (oauth.rs sends it here): no consent page, the code
/// only to a member its app lets in, through a browser session of theirs at one of the workspace's addresses. Until its
/// return address is known to be the client's own, a refusal is a page; after, the client is told at that address.
pub(crate) async fn authorize(st: &AppState, headers: &HeaderMap, uri: &Uri, client_id: &str) -> ApiResult<Response> {
    let refused = super::oauth::refused;
    let Some((tenant, iss)) = issuer(st, headers).await? else { return Ok(refused("This address signs nobody in to other programs.")) };
    // Only this workspace's: a client's id is unique within its workspace, never across them.
    let Some(client) = st.kernel.companion_client(tenant, client_id).await? else { return Ok(refused("This app is not registered with Ferrum.")) };
    let Ok(Query(r)) = Query::<AuthRequest>::try_from_uri(uri) else { return Ok(refused("This sign-in link is not valid.")) };
    if !client.redirect_uris.contains(&r.redirect_uri) {
        return Ok(refused("This app asked to return to an address it did not register. For your safety, nothing was shared."));
    }
    let told =
        |error: &str, description: &str| back(&r.redirect_uri, &[("error", error), ("error_description", description), ("state", &r.state), ("iss", &iss)]);
    if st.login_limiter.check_from(headers, "oidc-authorize").await.is_err() {
        return Ok(told("temporarily_unavailable", "too many sign-ins from here; try again in a minute"));
    }
    let signed = match crate::shell::session_cookie(headers, st.shell.secure) {
        None => None,
        Some(_) => match crate::extract::caller_on(st, &Method::GET, headers).await {
            Ok((ctx, Some(session))) if ctx.tenant == tenant => Some((ctx, session)),
            Ok(_) | Err(ApiError(Error::Unauthorized(_))) => None,
            Err(e) => return Err(e),
        },
    };
    let Some((ctx, session)) = signed else {
        if r.silent() {
            return Ok(told("login_required", "sign in to Ferrum first"));
        }
        let here = uri.path_and_query().map_or("/oauth/authorize", |p| p.as_str());
        return Ok(Redirect::to(&format!("/login?next={}", super::oauth::urlencode(here))).into_response());
    };
    match st.kernel.openid_authorize(&ctx, session, &client, &r, &iss).await {
        Ok(Ok(Authorized::Code(code))) => Ok(back(&r.redirect_uri, &[("code", &code), ("state", &r.state), ("iss", &iss)])),
        Ok(Ok(Authorized::Confirm(waiting))) => confirm_page(st, headers, &ctx, session, &client, &waiting).await,
        Ok(Err(no)) => Ok(told(no.error, &no.description)),
        Err(Error::Unauthorized(_)) if r.silent() => Ok(told("login_required", "sign in to Ferrum first")),
        Err(Error::Unauthorized(_)) => {
            let here = uri.path_and_query().map_or("/oauth/authorize", |p| p.as_str());
            Ok(Redirect::to(&format!("/login?next={}", super::oauth::urlencode(here))).into_response())
        }
        Err(e) => Err(e.into()),
    }
}

/// The page where the person confirms it is them before `client`'s companion signs them in: the shell's bar and runtime
/// (whose dialog asks them, in their usual ways, when the server says so), and `companion.js`, which finishes the request
/// `waiting` and goes on. Never in a frame; nothing keeps it.
async fn confirm_page(st: &AppState, headers: &HeaderMap, ctx: &ferrum_kernel::Ctx, session: Uuid, client: &Client, waiting: &str) -> ApiResult<Response> {
    use crate::shell::{esc, html_open, json_in_html, shell_file_url, shell_head};
    let (boot, _) = match super::shell::page_bootstrap(st, headers, ctx, Some(session), json!(null), "status").await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    let lang = crate::shell_words::page_language(Some(&boot));
    let tr = |s: &str| crate::shell_words::tr(&lang, s);
    let program =
        st.apps.shell_apps(ctx.tenant).await?.into_iter().find(|a| a.id == client.app).map_or_else(|| client.app.clone(), |a| a.manifest.name.clone());
    let why = crate::shell_words::tr_with(
        &lang,
        "{app} asks you to confirm it is you before it signs you in.",
        &[("app", &format!("<strong>{}</strong>", esc(&program)))],
        true,
    );
    let data = json!({"waiting": waiting, "failed": tr("Signing in did not finish. Start again from the program.")});
    let html = format!(
        r#"<!doctype html>{open}<head>{head}<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{title} · Ferrum</title><link rel="stylesheet" href="{pages_css}"><script type="application/json" id="ferrum-companion">{data}</script><script type="module" src="{js}"></script></head><body>{bar}<main class="ferrum-status" id="main"><h1>{title}</h1><p>{why}</p><p class="ferrum-error" role="alert" id="ferrum-companion-error"></p><p class="ferrum-actions"><button class="ferrum-button" type="button" id="ferrum-companion-go">{again}</button></p></main></body></html>"#,
        open = html_open(&lang),
        head = shell_head(&boot),
        title = esc(&tr("Confirm it is you")),
        pages_css = shell_file_url("pages.css"),
        data = json_in_html(&data),
        js = shell_file_url("companion.js"),
        bar = crate::shell::SHELL_BODY,
        again = esc(&tr("Confirm it is you")),
    );
    let csp = crate::shell::csp(None, &Value::Null).replace("frame-ancestors 'self'", "frame-ancestors 'none'");
    Ok(no_store(crate::shell::page_response(StatusCode::OK, html, &csp)))
}

#[derive(Deserialize)]
struct ConfirmReq {
    waiting: String,
}

/// `POST /api/oauth/authorize/confirm`: the confirmation page's request, from the person's browser session. Until they
/// have confirmed it is them as recently as the request asks, the step-up refusal (`403`, `step_up`, with their ways);
/// then where the browser goes: the client's address with the code.
async fn confirm(State(app): State<AppContext>, headers: HeaderMap, Json(req): Json<ConfirmReq>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let Some((tenant, iss)) = issuer(&st, &headers).await? else { return Ok(not_here()) };
    let (ctx, device) = crate::extract::caller_on(&st, &Method::POST, &headers).await?;
    let Some(session) = device.filter(|_| ctx.tenant == tenant) else {
        return Err(ApiError(Error::NotFound("this sign-in has expired: start it again from the program".into())));
    };
    let (redirect_uri, code, state) = st.kernel.openid_confirm(&ctx, session, &req.waiting, &iss).await?;
    let to = super::oauth::back_to(&redirect_uri, &[("code", &code), ("state", &state), ("iss", &iss)]);
    Ok(no_store(Json(json!({"redirect_to": to})).into_response()))
}

/// `application/x-www-form-urlencoded` decoding of one part of HTTP Basic credentials (RFC 6749 §2.3.1).
fn form_decoded(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        out.push(match b {
            b'+' => b' ',
            b'%' => {
                let hex = [bytes.next()?, bytes.next()?];
                u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?
            }
            b => b,
        });
    }
    String::from_utf8(out).ok()
}

/// The client's id and secret as HTTP Basic credentials carry them.
fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic "))?.trim();
    let pair = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(encoded).ok()?).ok()?;
    let (id, secret) = pair.split_once(':')?;
    Some((form_decoded(id)?, form_decoded(secret)?))
}

/// The token endpoint for a companion's client (oauth.rs asks first). `None`: the request is not one (its client is
/// an API client's or an app's), and the endpoint goes on as it always did. A companion authenticates with its secret
/// (HTTP Basic, or `client_secret` in the form) and redeems its code once; a wrong one of either is answered as the
/// specification says, the same whatever was wrong, and counted against the caller's address.
pub(crate) async fn token(st: &AppState, headers: &HeaderMap, form: &super::oauth::TokenReq) -> Option<Response> {
    if !on(st) {
        return None;
    }
    let (client_id, secret) = match basic(headers) {
        Some((id, secret)) => (id, Some(secret)),
        None => (form.client_id.clone()?, form.client_secret.clone()),
    };
    if !client_id.starts_with(CLIENT_PREFIX) {
        return None;
    }
    Some(companion_token(st, headers, form, &client_id, secret.as_deref()).await)
}

async fn companion_token(st: &AppState, headers: &HeaderMap, form: &super::oauth::TokenReq, client_id: &str, secret: Option<&str>) -> Response {
    let (tenant, iss) = match issuer(st, headers).await {
        Ok(Some(found)) => found,
        Ok(None) => return not_here(),
        Err(e) => return e.into_response(),
    };
    let Some(secret) = secret else {
        return counted(st, headers, "token", oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "client authentication failed")).await;
    };
    if form.grant_type != "authorization_code" {
        return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "a program beside Ferrum redeems codes only");
    }
    let (Some(code), Some(redirect), Some(verifier)) = (form.code.as_deref(), form.redirect_uri.as_deref(), form.code_verifier.as_deref()) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "code, redirect_uri and code_verifier are required");
    };
    match st.kernel.openid_token(tenant, client_id, secret, code, redirect, verifier, &iss).await {
        Ok(t) => no_store(
            Json(json!({"access_token": t.access_token, "token_type": "Bearer", "expires_in": TOKEN_SECS, "id_token": t.id_token, "scope": t.scope}))
                .into_response(),
        ),
        Err(Error::Unauthorized(_)) => {
            counted(st, headers, "token", oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "client authentication failed")).await
        }
        Err(Error::Invalid(_)) => {
            counted(st, headers, "token", oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "the code is invalid, expired or already used")).await
        }
        Err(e) => ApiError(e).into_response(),
    }
}

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .add("/.well-known/openid-configuration", get(discovery))
        .add("/oauth/jwks", get(jwks))
        .add("/oauth/userinfo", get(userinfo).post(userinfo))
        .add("/api/oauth/authorize/confirm", post(confirm))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_credentials_are_form_encoded() {
        let mut h = HeaderMap::new();
        let pair = base64::engine::general_purpose::STANDARD.encode("fo_a%2Bb:s%3Ac+d");
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Basic {pair}")).unwrap());
        assert_eq!(basic(&h), Some(("fo_a+b".into(), "s:c d".into())));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic !!"));
        assert_eq!(basic(&h), None);
        assert_eq!(form_decoded("%4"), None);
    }
}
