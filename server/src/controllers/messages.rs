//! Messages to people (crates/kernel/src/messages.rs): administrators edit templates and channel connections here,
//! anyone who reads a record sees what was sent about it, providers report deliveries and replies, and recipients
//! follow the link that opts them out.

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, client_ip};
use crate::state::app_state;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::messages::ChannelInput;
use ferrum_kernel::{Ctx, Error, Kernel};
use ferrum_meta::MessageDef;
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

fn id_of(body: &Value, key: &str) -> ApiResult<Option<Uuid>> {
    match body.get(key).filter(|v| !v.is_null()) {
        Some(v) => Ok(Some(v.as_str().and_then(|s| s.parse().ok()).ok_or_else(|| ApiError(Error::field(key, "must be an id")))?)),
        None => Ok(None),
    }
}

async fn list(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"templates": app_state(&app)?.kernel.message_templates(&ctx).await?})).into_response())
}

async fn read(Auth(ctx): Auth, State(app): State<AppContext>, Path(key): Path<String>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.message_template(&ctx, &key).await?).into_response())
}

async fn save(Auth(ctx): Auth, State(app): State<AppContext>, Json(def): Json<MessageDef>) -> ApiResult<Response> {
    app_state(&app)?.kernel.save_message_template(&ctx, &def).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn reset(Auth(ctx): Auth, State(app): State<AppContext>, Path(key): Path<String>) -> ApiResult<Response> {
    app_state(&app)?.kernel.reset_message_template(&ctx, &key).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/admin/messages/preview {template, channel, language?, record_id?}`: one channel's message as it would go.
pub async fn preview_message(k: &Kernel, ctx: &Ctx, body: &Value) -> ApiResult<Value> {
    let template = body.get("template").ok_or_else(|| ApiError(Error::field("template", "is required")))?;
    let channel = body.get("channel").and_then(Value::as_str).ok_or_else(|| ApiError(Error::field("channel", "is required")))?;
    Ok(k.preview_message(ctx, template, channel, body.get("language").and_then(Value::as_str), id_of(body, "record_id")?).await?)
}

async fn preview(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<Value>) -> ApiResult<Response> {
    Ok(Json(preview_message(&app_state(&app)?.kernel, &ctx, &body).await?).into_response())
}

/// `POST /api/admin/messages/test {template, channel, phone?, record_id?}`: to the caller themselves.
async fn test(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<Value>) -> ApiResult<Response> {
    let text = |k: &str| body.get(k).and_then(Value::as_str);
    let template = text("template").ok_or_else(|| ApiError(Error::field("template", "is required")))?;
    let channel = text("channel").ok_or_else(|| ApiError(Error::field("channel", "is required")))?;
    let sent = app_state(&app)?.kernel.send_test_message(&ctx, template, channel, text("phone"), id_of(&body, "record_id")?).await?;
    Ok(Json(json!({"sent": sent})).into_response())
}

#[derive(Deserialize)]
struct LogQuery {
    entity: Option<String>,
    record_id: Option<Uuid>,
    recipient: Option<String>,
    limit: Option<i64>,
}

/// `GET /api/messages/log?entity=&record_id=` (whoever may read the record) or `?recipient=` (administrators).
async fn log(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<LogQuery>) -> ApiResult<Response> {
    let record = q.entity.as_deref().zip(q.record_id);
    let sends = app_state(&app)?.kernel.message_log(&ctx, record, q.recipient.as_deref(), q.limit.unwrap_or(100)).await?;
    Ok(Json(json!({"sends": sends})).into_response())
}

#[derive(Deserialize)]
struct OptOut {
    address: String,
    channel: String,
    opted_out: bool,
}

async fn opt_out_by_staff(Auth(ctx): Auth, State(app): State<AppContext>, Json(b): Json<OptOut>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_message_opt_out(&ctx, &b.address, &b.channel, b.opted_out).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn channels(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"channels": app_state(&app)?.kernel.message_channels(&ctx).await?})).into_response())
}

async fn add_channel(Auth(ctx): Auth, State(app): State<AppContext>, Json(c): Json<ChannelInput>) -> ApiResult<Response> {
    let id = app_state(&app)?.kernel.save_message_channel(&ctx, None, &c).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

async fn change_channel(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(c): Json<ChannelInput>) -> ApiResult<Response> {
    app_state(&app)?.kernel.save_message_channel(&ctx, Some(id), &c).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn remove_channel(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.delete_message_channel(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/messages/inbound/{channel}`: no session; the channel's description says how its provider signs.
async fn inbound(
    State(app): State<AppContext>,
    Path(id): Path<Uuid>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.public_limiter.check(&format!("{}|messages", client_ip(&headers))).await?;
    let names: BTreeMap<String, String> = headers.iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
    Ok(Json(st.kernel.message_inbound(id, &names, &query, &body).await?).into_response())
}

#[derive(Deserialize)]
struct Token {
    token: String,
}

fn page(text: &str) -> Response {
    let mut esc = String::new();
    for c in text.chars() {
        match c {
            '<' => esc.push_str("&lt;"),
            '>' => esc.push_str("&gt;"),
            '&' => esc.push_str("&amp;"),
            '"' => esc.push_str("&quot;"),
            c => esc.push(c),
        }
    }
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><body style=\"font-family:system-ui;max-width:32rem;margin:3rem auto;padding:0 1rem\">{esc}</body>"
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// `GET /api/messages/opt-out?token=`: a page asking to confirm (a mail client opening links ahead changes nothing).
async fn opt_out_page(Query(t): Query<Token>) -> Response {
    let mut esc = String::new();
    for c in t.token.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)) {
        esc.push(c);
    }
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><body style=\"font-family:system-ui;max-width:32rem;margin:3rem auto;padding:0 1rem\">\
         <form method=post action=\"?token={esc}\"><p>Stop receiving these messages?</p><button type=submit>Stop them</button></form></body>"
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// `POST /api/messages/opt-out?token=`: the recipient's word, recorded.
async fn opt_out_by_link(State(app): State<AppContext>, Query(t): Query<Token>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.public_limiter.check(&format!("{}|opt-out", client_ip(&headers))).await?;
    st.kernel.opt_out_by_link(&t.token).await?;
    Ok(page("You will no longer receive these messages here."))
}

pub fn routes() -> Routes {
    use axum::routing::{get, post, put};
    Routes::new()
        .prefix("/api")
        .add("/admin/messages/templates", get(list).post(save))
        .add("/admin/messages/templates/{key}", get(read).delete(reset))
        .add("/admin/messages/preview", post(preview))
        .add("/admin/messages/test", post(test))
        .add("/admin/messages/opt-outs", put(opt_out_by_staff))
        .add("/admin/messages/channels", get(channels).post(add_channel))
        .add("/admin/messages/channels/{id}", put(change_channel).delete(remove_channel))
        .add("/messages/log", get(log))
        .add("/messages/inbound/{id}", post(inbound))
        .add("/messages/opt-out", get(opt_out_page).post(opt_out_by_link))
}
