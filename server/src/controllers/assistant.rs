//! The assistant (server/src/assistant.rs): its page at `/assistant/` (the bar's side panel, and
//! the whole page on a phone), and its API at `/api/assistant`. Sending a message, or deciding
//! about the changes the assistant proposed, answers with the turn's events (server-sent); the
//! turn goes on when the page leaves, and `…/live` follows it again from this replica.

use crate::assistant::{self, Input};
use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, caller_on};
use crate::shell::{self as sh, SHELL_BODY};
use crate::state::{AppState, app_state};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use ferrum_kernel::Error;
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// `GET /assistant/` (the shell's pages route it here): the page, under the bar (which a side
/// panel's frame hides).
pub(crate) async fn page(st: &AppState, headers: &HeaderMap, uri: &Uri) -> ApiResult<Response> {
    let Ok((ctx, device)) = caller_on(st, &Method::GET, headers).await else {
        let next = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/assistant/");
        return Ok(Redirect::to(&format!("/login?next={}", urlencoding(next))).into_response());
    };
    let app_json = json!({"id": sh::SHELL_APP, "version": sh::SHELL_VERSION, "routes": {"assistant": "/assistant/"}, "entry": "assistant", "title": "Assistant", "compact": false});
    let (bootstrap, _) = match super::shell::page_bootstrap(st, headers, &ctx, device, app_json, "page").await {
        Ok(b) => b,
        Err(res) => return Ok(res),
    };
    let html = format!(
        r#"<!doctype html><html lang="en"><head>{}<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Assistant · Ferrum</title><link rel="stylesheet" href="/_shell/v1/assistant.css"><script type="module" src="/_shell/v1/assistant.js"></script></head><body>{SHELL_BODY}<main id="main" class="fa" aria-label="Assistant"></main></body></html>"#,
        sh::shell_head(&bootstrap)
    );
    Ok(sh::page_response(StatusCode::OK, html, &sh::csp(None, &Value::Null)))
}

fn urlencoding(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// `GET /api/assistant`: whether the person may use it, and their recent conversations.
async fn status(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    let available = st.assistant.available(&st.kernel, &ctx, &kind).await;
    let why = if available {
        None
    } else if !st.assistant.configured() {
        Some("The assistant is not set up on this server.")
    } else if kind != "member" {
        Some("The assistant is for the workspace's own people.")
    } else {
        Some("The workspace's administrators turned the assistant off.")
    };
    let threads = if available { assistant::threads(&st.kernel, &ctx).await? } else { vec![] };
    Ok(Json(json!({"available": available, "reason": why, "admin": ctx.is_admin, "threads": threads})).into_response())
}

async fn usable(st: &crate::state::AppState, ctx: &ferrum_kernel::Ctx) -> Result<(), ApiError> {
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    if st.assistant.available(&st.kernel, ctx, &kind).await { Ok(()) } else { Err(ApiError(Error::Forbidden("the assistant is not available here".into()))) }
}

async fn create(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    usable(&st, &ctx).await?;
    let id = assistant::create_thread(&st.kernel, &ctx).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

async fn read(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(assistant::thread(&assistant::Env::of(&st), &ctx, id).await?).into_response())
}

async fn remove(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    assistant::delete_thread(&assistant::Env::of(&st), &ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct Message {
    text: String,
    /// Where the person is (their page, the record they look at, their time): said to the model.
    #[serde(default)]
    context: String,
}

const MAX_TEXT: usize = 20_000;
const MAX_CONTEXT: usize = 2_000;

async fn send(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(m): Json<Message>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let text = m.text.trim();
    if text.is_empty() {
        return Err(ApiError(Error::field("text", "write something to ask")));
    }
    if text.chars().count() > MAX_TEXT {
        return Err(ApiError(Error::field("text", "this is too long for one message")));
    }
    let context: String = m.context.chars().take(MAX_CONTEXT).collect();
    let rx = assistant::start(&st, &ctx, id, Input::Message { text: text.to_string(), context }).await?;
    Ok(assistant::events(None, rx))
}

#[derive(Deserialize)]
struct Decision {
    allow: bool,
}

async fn decide(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(d): Json<Decision>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let rx = assistant::start(&st, &ctx, id, Input::Decision { allow: d.allow }).await?;
    Ok(assistant::events(None, rx))
}

/// Follows a turn under way from this replica; 404 when it runs elsewhere (the page asks again
/// now and then instead).
async fn live(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    // The thread is the person's (or this is a 404 as well).
    assistant::thread(&assistant::Env::of(&st), &ctx, id).await?;
    assistant::follow(&assistant::Env::of(&st), &ctx, id).ok_or_else(|| ApiError(Error::NotFound("a turn under way on this server".into())))
}

async fn stop(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    assistant::stop(&assistant::Env::of(&st), &ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .add("/api/assistant", get(status))
        .add("/api/assistant/threads", post(create))
        .add("/api/assistant/threads/{id}", get(read).delete(remove))
        .add("/api/assistant/threads/{id}/messages", post(send))
        .add("/api/assistant/threads/{id}/decide", post(decide))
        .add("/api/assistant/threads/{id}/live", get(live))
        .add("/api/assistant/threads/{id}/stop", post(stop))
}
