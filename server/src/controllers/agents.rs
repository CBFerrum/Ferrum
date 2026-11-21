//! The agents' console (gapclose/design/A2-A3-agents.md 2.5, 2.11; AG-2): making, changing, pausing, resuming and
//! revoking agents and their keys, and reading what each did. Every change is a person's at a screen, which the kernel
//! asks (crate::agent: 403 `at_screen` for a key, the owner's own included); an agent's owner and administrators read.

use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use crate::state::{AppState, app_state};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// The server's state while agents are let in; else 404: no part of the console is there before the door, in any build
/// (AG-1-2-fix 5).
fn console(app: &AppContext) -> ApiResult<AppState> {
    let st = app_state(app)?;
    st.kernel.agents_admitted().map_err(|_| ApiError(ferrum_kernel::Error::NotFound("page".into())))?;
    Ok(st)
}

#[derive(Deserialize)]
struct Which {
    owner: Option<Uuid>,
    state: Option<String>,
}

async fn list(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<Which>) -> ApiResult<Response> {
    let agents = console(&app)?.kernel.agents(&ctx, q.owner, q.state.as_deref()).await?;
    Ok(Json(json!({"agents": agents})).into_response())
}

async fn create(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<Value>) -> ApiResult<Response> {
    let agent = console(&app)?.kernel.create_agent(&ctx, &body).await?;
    Ok((StatusCode::CREATED, Json(agent)).into_response())
}

async fn one(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(console(&app)?.kernel.agent(&ctx, id).await?).into_response())
}

async fn change(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(body): Json<Value>) -> ApiResult<Response> {
    Ok(Json(console(&app)?.kernel.update_agent(&ctx, id, &body).await?).into_response())
}

async fn state(ctx: &ferrum_kernel::Ctx, app: &AppContext, id: Uuid, to: &str) -> ApiResult<Response> {
    Ok(Json(console(app)?.kernel.set_agent_state(ctx, id, to).await?).into_response())
}

async fn pause(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    state(&ctx, &app, id, "pause").await
}

async fn resume(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    state(&ctx, &app, id, "resume").await
}

async fn revoke(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    state(&ctx, &app, id, "revoke").await
}

async fn keys(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(json!({"keys": console(&app)?.kernel.agent_keys(&ctx, id).await?})).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewKey {
    name: String,
    expires_at: Option<DateTime<Utc>>,
}

async fn make_key(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(body): Json<NewKey>) -> ApiResult<Response> {
    let (key, secret) = console(&app)?.kernel.create_agent_key(&ctx, id, &body.name, body.expires_at).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": key, "key": secret, "note": "Store this key now; it cannot be shown again."}))).into_response())
}

async fn revoke_key(Auth(ctx): Auth, State(app): State<AppContext>, Path((id, key)): Path<(Uuid, Uuid)>) -> ApiResult<Response> {
    console(&app)?.kernel.revoke_agent_key(&ctx, id, key).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct Days {
    days: Option<u32>,
}

async fn usage(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Query(q): Query<Days>) -> ApiResult<Response> {
    Ok(Json(console(&app)?.kernel.agent_usage(&ctx, id, q.days.unwrap_or(30)).await?).into_response())
}

#[derive(Deserialize)]
struct Before {
    before: Option<i64>,
    limit: Option<u32>,
}

async fn activity(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Query(q): Query<Before>) -> ApiResult<Response> {
    Ok(Json(console(&app)?.kernel.agent_activity(&ctx, id, q.before, q.limit.unwrap_or(50)).await?).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{delete, get, post};
    Routes::new()
        .prefix("/api/agents")
        .add("/", get(list).post(create))
        .add("/{id}", get(one).patch(change))
        .add("/{id}/pause", post(pause))
        .add("/{id}/resume", post(resume))
        .add("/{id}/revoke", post(revoke))
        .add("/{id}/keys", get(keys).post(make_key))
        .add("/{id}/keys/{key}", delete(revoke_key))
        .add("/{id}/usage", get(usage))
        .add("/{id}/activity", get(activity))
}
