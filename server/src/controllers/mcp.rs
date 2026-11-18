//! `/mcp`: the Model Context Protocol over stateless Streamable HTTP (revisions 2026-07-28, 2025-11-25 and
//! 2025-06-18; `ferrum_mcp::serve` checks the protocol's headers and answers). Each POST carries one JSON-RPC message
//! and gets a JSON reply, so any replica can serve any call. A client is a program, never a browser page:
//! `Authorization: Bearer <session token, API key or OAuth token>` only, and a page of another origin is refused
//! (`Origin`). It reads under the workspace's policy for the assistant (`Assistant::ai_policy`).

use crate::error::ApiError;
use crate::extract::BearerAuth;
use crate::state::{AppState, app_state};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::json;

/// The largest message taken.
const MAX_BODY: usize = 1 << 20;

/// 401 that tells OAuth-capable clients where to find the authorization server (RFC 9728).
fn unauthorized(st: &AppState, headers: &HeaderMap, message: String) -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "unauthorized", "message": message}))).into_response();
    let challenge = format!("Bearer realm=\"ferrum\", resource_metadata=\"{}\"", super::oauth::resource_metadata_url(st, headers));
    if let Ok(v) = HeaderValue::from_str(&challenge) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

async fn post(State(app): State<AppContext>, mut parts: Parts, body: Bytes) -> Response {
    let st = match app_state(&app) {
        Ok(st) => st,
        Err(e) => return e.into_response(),
    };
    // Against DNS rebinding: a page of another origin never reaches the server through a browser.
    if parts.headers.contains_key(header::ORIGIN) && !crate::shell::origin_is_host(&parts.headers) {
        let err = json!({"jsonrpc": "2.0", "error": {"code": -32600, "message": "this Origin may not call the server"}});
        return (StatusCode::FORBIDDEN, axum::Json(err)).into_response();
    }
    let ctx = match BearerAuth::from_request_parts(&mut parts, &app).await {
        Ok(BearerAuth(ctx)) => ctx,
        Err(ApiError(e)) if e.status() == 401 => return unauthorized(&st, &parts.headers, e.to_string()),
        Err(e) => return e.into_response(),
    };
    // An MCP client is an AI agent: what it reads goes on to its provider. The workspace's policy for the assistant is
    // its too: shut where the assistant is turned off, and what may not go to a provider is not read.
    let ctx = match st.assistant.ai_policy(&st.kernel, ctx.tenant).await {
        Ok(policy) => ctx.for_provider(policy.egress),
        Err(e) => return ApiError(e).into_response(),
    };
    let h = |name: &str| parts.headers.get(name).map(HeaderValue::as_bytes);
    let headers = ferrum_mcp::Headers { protocol_version: h("mcp-protocol-version"), method: h("mcp-method"), name: h("mcp-name") };
    // The App Manager's tools (administrators only) beside the platform's.
    let tools = crate::appmgr_mcp::AppTools(st.apps.clone());
    let search = crate::search::SearchTool(st.kernel.clone());
    let reply = ferrum_mcp::serve(&st.kernel, &ctx, headers, &body, &[&tools, &search]).await;
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match reply.body {
        Some(v) => (status, axum::Json(v)).into_response(),
        None => status.into_response(),
    }
}

/// No server-initiated stream (stateless server): the spec's answer is 405. `Mcp-Session-Id` and `Last-Event-ID` are
/// never read and never sent.
async fn not_allowed() -> Response {
    let mut resp = StatusCode::METHOD_NOT_ALLOWED.into_response();
    resp.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST"));
    resp
}

pub fn routes() -> Routes {
    use axum::routing::post as p;
    Routes::new().add("/mcp", p(post).get(not_allowed).delete(not_allowed).layer(DefaultBodyLimit::max(MAX_BODY)))
}
