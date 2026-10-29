//! Plugin endpoints: HTTP routes (`/api/ext/{plugin}/…` for signed-in callers,
//! `/api/public/{tenant}/{plugin}/…` for public ones such as third-party webhooks) and the
//! WebSocket gateway (`/ws/ext/{tenant}/{plugin}/…`) for device protocols like OCPP; and the
//! files of apps' screens (`/api/ext-ui/{tenant}/{app}/{version}/…`, docs/plugins.md "Screens").

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, bearer, ctx_from_token, request_id};
use crate::state::{AppState, app_state};
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error};
use ferrum_plugins::http::{HttpCall, HttpReply};
use ferrum_plugins::{Loaded, Outgoing};
use futures_util::{SinkExt, StreamExt};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

const MAX_BODY: usize = 1 << 20;
const MAX_SOCKET_MESSAGE: usize = 64 << 10;
/// Messages a socket may send per window before it is closed as abusive.
const SOCKET_BURST: u32 = 100;
const SOCKET_WINDOW: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(30);

/// Headers passed to plugins: everything except credentials.
fn plugin_headers(headers: &HeaderMap) -> Map<String, Value> {
    headers
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "authorization" | "cookie" | "proxy-authorization"))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), json!(v))))
        .collect()
}

fn to_response(r: HttpReply) -> Response {
    let mut resp = (StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK), r.body).into_response();
    for (k, v) in r.headers {
        // Plugins may not set cookies or framing headers on Ferrum's origin.
        if matches!(k.as_str(), "set-cookie" | "content-length" | "transfer-encoding" | "connection") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k), HeaderValue::from_str(&v)) {
            resp.headers_mut().insert(name, value);
        }
    }
    resp
}

fn call(method: Method, path: String, query: HashMap<String, String>, headers: &HeaderMap, body: Bytes) -> Result<HttpCall, ApiError> {
    if body.len() > MAX_BODY {
        return Err(ApiError(Error::invalid("request body larger than 1 MB")));
    }
    Ok(HttpCall {
        method: method.to_string(),
        path: format!("/{}", path.trim_start_matches('/')),
        query: query.into_iter().map(|(k, v)| (k, json!(v))).collect(),
        headers: plugin_headers(headers),
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

async fn ext(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((plugin, path)): Path<(String, String)>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let tenant = ctx.tenant;
    let reply = st.plugins.handle_http(&st.kernel, tenant, &plugin, call(method, path, query, &headers, body)?, Some(ctx)).await?;
    Ok(to_response(reply))
}

async fn public(
    State(app): State<AppContext>,
    Path((tenant, plugin, path)): Path<(String, String, String)>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let (tenant, status) = st.kernel.tenant_by_slug(&tenant).await?;
    if status != "active" {
        return Err(ApiError(Error::NotFound("tenant".into())));
    }
    let reply = st.plugins.handle_http(&st.kernel, tenant, &plugin, call(method, path, query, &headers, body)?, None).await?;
    Ok(to_response(reply))
}

async fn socket(
    State(app): State<AppContext>,
    Path((tenant_slug, plugin, path)): Path<(String, String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let (tenant, status) = st.kernel.tenant_by_slug(&tenant_slug).await?;
    if status != "active" {
        return Err(ApiError(Error::NotFound("tenant".into())));
    }
    let p = st.plugins.plugin(&st.kernel.db, tenant, &plugin).await?;
    let path = format!("/{}", path.trim_start_matches('/'));
    let def = p.manifest.socket(&path).cloned().ok_or_else(|| ApiError(Error::NotFound(format!("socket {path} of plugin {plugin}"))))?;
    let user = if def.public {
        None
    } else {
        // Browsers cannot set headers on WebSockets: accept the token as `access_token` too.
        let token = bearer(&headers).or_else(|| query.get("access_token").cloned()).ok_or_else(|| ApiError(Error::Unauthorized("sign in required".into())))?;
        let ctx = ctx_from_token(&st, &token, request_id(&headers)).await?;
        if ctx.tenant != tenant {
            return Err(ApiError(Error::Forbidden("this token belongs to another workspace".into())));
        }
        Some(ctx)
    };
    if !def.subprotocols.is_empty() {
        // Device protocols (OCPP...) must agree on a subprotocol; refuse before upgrading if none fits.
        let offered: Vec<String> =
            headers.get_all("sec-websocket-protocol").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).map(|p| p.trim().to_string()).collect();
        if !offered.iter().any(|o| def.subprotocols.contains(o)) {
            return Err(ApiError(Error::invalid(format!("this socket speaks {}", def.subprotocols.join(", ")))));
        }
    }
    let upgrade = if def.subprotocols.is_empty() { upgrade } else { upgrade.protocols(def.subprotocols.clone()) };
    let query: Map<String, Value> = query.into_iter().filter(|(k, _)| k != "access_token").map(|(k, v)| (k, json!(v))).collect();
    Ok(upgrade.max_message_size(MAX_SOCKET_MESSAGE).on_upgrade(move |ws| serve(st, tenant, p, path, query, user, ws)))
}

/// Sends the reply's `send` (one text or a list) and reports whether it asked to close.
async fn deliver(sink: &mut futures_util::stream::SplitSink<WebSocket, Message>, reply: &Value) -> bool {
    let texts: Vec<String> = match reply.get("send") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(list)) => list.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect(),
        _ => vec![],
    };
    for t in texts {
        if sink.send(Message::Text(t.into())).await.is_err() {
            return true;
        }
    }
    reply.get("close").and_then(Value::as_bool).unwrap_or(false)
}

async fn serve(st: AppState, tenant: Uuid, p: Loaded, path: String, query: Map<String, Value>, user: Option<Ctx>, ws: WebSocket) {
    let id = Uuid::now_v7();
    let subprotocol = ws.protocol().and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut outgoing = st.plugins.sockets.register(tenant, id);
    let (mut sink, mut stream) = ws.split();
    let event = |name: &str, message: Option<String>| json!({"event": name, "connection": id, "path": path, "query": query, "subprotocol": subprotocol, "message": message});
    let close = |code: u16, reason: &str| Message::Close(Some(CloseFrame { code, reason: reason.to_string().into() }));
    match st.plugins.socket_event(&st.kernel, tenant, &p, user.clone(), event("connect", None)).await {
        Ok(reply) => {
            if deliver(&mut sink, &reply).await {
                let _ = sink.send(close(1000, reply.get("reason").and_then(Value::as_str).unwrap_or("closed"))).await;
                st.plugins.sockets.unregister(id);
                return;
            }
        }
        Err(e) => {
            tracing::warn!(plugin = %p.name, %tenant, "socket refused: {e}");
            let _ = sink.send(close(1011, "refused")).await;
            st.plugins.sockets.unregister(id);
            return;
        }
    }
    let (mut window_start, mut in_window) = (Instant::now(), 0u32);
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await;
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if window_start.elapsed() > SOCKET_WINDOW {
                        (window_start, in_window) = (Instant::now(), 0);
                    }
                    in_window += 1;
                    if in_window > SOCKET_BURST {
                        let _ = sink.send(close(1008, "too many messages")).await;
                        break;
                    }
                    match st.plugins.socket_event(&st.kernel, tenant, &p, user.clone(), event("message", Some(text.to_string()))).await {
                        Ok(reply) => {
                            if deliver(&mut sink, &reply).await {
                                let _ = sink.send(close(1000, "closed")).await;
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(plugin = %p.name, %tenant, "socket message failed: {e}");
                            let _ = sink.send(close(1011, "plugin error")).await;
                            break;
                        }
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    let _ = sink.send(close(1003, "text messages only")).await;
                    break;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            },
            out = outgoing.recv() => match out {
                Some(Outgoing::Text(t)) => {
                    if sink.send(Message::Text(t.into())).await.is_err() {
                        break;
                    }
                }
                Some(Outgoing::Close(reason)) => {
                    let _ = sink.send(close(1000, &reason)).await;
                    break;
                }
                None => break,
            },
            _ = keepalive.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    st.plugins.sockets.unregister(id);
    if let Err(e) = st.plugins.socket_event(&st.kernel, tenant, &p, user, event("close", None)).await {
        tracing::warn!(plugin = %p.name, %tenant, "socket close handler failed: {e}");
    }
}

/// Every file of a screen runs sandboxed (scripts, but in an opaque origin without the app's
/// storage or session) wherever it is opened, framed only by the web app, loading only from this
/// server, posting no forms.
const SCREEN_CSP: &str = "sandbox allow-scripts allow-downloads; default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; media-src 'self' blob:; \
    connect-src 'self'; worker-src 'self' blob:; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";

fn screen_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map_or("", |(_, ext)| ext) {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// `GET /api/ext-ui/{tenant}/{app}/{version}/{*path}`: a file of an enabled app's screens.
/// Public, as a sandboxed frame sends no credentials; pages open only inside the web app.
async fn screen_file(
    State(app): State<AppContext>,
    Path((tenant, plugin, version, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let (tenant, status) = st.kernel.tenant_by_slug(&tenant).await?;
    if status != "active" {
        return Err(ApiError(Error::NotFound("tenant".into())));
    }
    // Opened on its own, a page would show this server's address: only frames get one.
    if path.ends_with(".html") && headers.get("sec-fetch-dest").is_some_and(|d| d != "iframe") {
        return Err(ApiError(Error::Forbidden("an app's screens open inside the web app".into())));
    }
    let Some((bytes, digest)) = st.apps.screen_file(tenant, &plugin, &version, &path).await? else {
        return Err(ApiError(Error::NotFound(format!("{plugin} {version}: {path}"))));
    };
    let etag = HeaderValue::from_str(&format!("\"{}\"", digest.trim_start_matches("sha256:"))).map_err(|e| ApiError(Error::internal(e.to_string())))?;
    let mut res = if headers.get(header::IF_NONE_MATCH) == Some(&etag) { StatusCode::NOT_MODIFIED.into_response() } else { bytes.into_response() };
    let h = res.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, screen_type(&path)),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONTENT_SECURITY_POLICY, SCREEN_CSP),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "sameorigin"),
        (header::REFERRER_POLICY, "no-referrer"),
        // Module scripts and WebAssembly are fetched with CORS from the frame's opaque origin.
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    h.insert(header::ETAG, etag);
    Ok(res)
}

pub fn routes() -> Routes {
    use axum::routing::{any, get};
    Routes::new()
        .add("/api/ext/{plugin}/{*path}", any(ext))
        .add("/api/ext-ui/{tenant}/{plugin}/{version}/{*path}", get(screen_file))
        .add("/api/public/{tenant}/{plugin}/{*path}", any(public))
        .add("/ws/ext/{tenant}/{plugin}/{*path}", get(socket))
}
