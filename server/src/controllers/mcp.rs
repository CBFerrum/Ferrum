//! `/mcp`: the Model Context Protocol over stateless Streamable HTTP. Each POST carries one
//! JSON-RPC message (or a batch) and gets a JSON reply, so any replica can serve any call. Clients
//! authenticate exactly like the REST API: `Authorization: Bearer <session token or API key>`.

use crate::error::ApiError;
use crate::extract::Auth;
use crate::state::app_state;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Value, json};

const MAX_BATCH: usize = 50;

/// 401 that tells OAuth-capable clients where to find the authorization server (RFC 9728).
fn unauthorized(headers: &HeaderMap, message: String) -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "unauthorized", "message": message}))).into_response();
    let challenge = format!("Bearer realm=\"ferrum\", resource_metadata=\"{}\"", super::oauth::resource_metadata_url(headers));
    if let Ok(v) = HeaderValue::from_str(&challenge) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

async fn post(State(app): State<AppContext>, headers: HeaderMap, auth: Result<Auth, ApiError>, body: Bytes) -> Response {
    let ctx = match auth {
        Ok(Auth(ctx)) => ctx,
        Err(ApiError(e)) if e.status() == 401 => return unauthorized(&headers, e.to_string()),
        Err(e) => return e.into_response(),
    };
    let st = match app_state(&app) {
        Ok(st) => st,
        Err(e) => return e.into_response(),
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let err = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}});
            return (StatusCode::BAD_REQUEST, axum::Json(err)).into_response();
        }
    };
    let reply = match msg {
        Value::Array(batch) if batch.len() > MAX_BATCH => {
            let err = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": format!("at most {MAX_BATCH} messages per batch")}});
            return (StatusCode::BAD_REQUEST, axum::Json(err)).into_response();
        }
        Value::Array(batch) => {
            let mut out = Vec::new();
            for m in batch {
                if let Some(r) = ferrum_mcp::handle(&st.kernel, &ctx, m).await {
                    out.push(r);
                }
            }
            (!out.is_empty()).then_some(Value::Array(out))
        }
        single => ferrum_mcp::handle(&st.kernel, &ctx, single).await,
    };
    match reply {
        Some(v) => axum::Json(v).into_response(),
        // Notifications and client responses are accepted without a body.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// No server-initiated stream (stateless server): the spec's answer is 405.
async fn not_allowed() -> Response {
    let mut resp = StatusCode::METHOD_NOT_ALLOWED.into_response();
    resp.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST"));
    resp
}

pub fn routes() -> Routes {
    use axum::routing::post as p;
    Routes::new().add("/mcp", p(post).get(not_allowed).delete(not_allowed))
}
