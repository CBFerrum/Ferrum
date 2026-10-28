//! Collaboration on any record (`ferrum_kernel::collab`): comments, assignments (to-dos), tags,
//! following and attachments. Access always follows the record.

use crate::error::ApiResult;
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

type RecordPath = Path<(String, Uuid)>;

async fn comments(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): RecordPath) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    Ok(Json(json!({"comments": st.kernel.comments(&ctx, &entity, id).await?})))
}

#[derive(Deserialize)]
struct NewComment {
    body: String,
    #[serde(default)]
    mentions: Vec<Uuid>,
}

async fn add_comment(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): RecordPath, Json(b): Json<NewComment>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let cid = st.kernel.add_comment(&ctx, &entity, id, &b.body, &b.mentions).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": cid}))).into_response())
}

#[derive(Deserialize)]
struct EditComment {
    body: String,
}

async fn edit_comment(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(b): Json<EditComment>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.kernel.edit_comment(&ctx, id, &b.body).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_comment(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.kernel.delete_comment(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn assignments(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): RecordPath) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    Ok(Json(json!({"assignments": st.kernel.assignments(&ctx, &entity, id).await?})))
}

#[derive(Deserialize)]
struct NewAssignment {
    user: Uuid,
    note: Option<String>,
    due_date: Option<chrono::NaiveDate>,
    priority: Option<String>,
}

async fn assign(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): RecordPath, Json(b): Json<NewAssignment>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let aid = st.kernel.assign(&ctx, &entity, id, b.user, b.note.as_deref(), b.due_date, b.priority.as_deref()).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": aid}))).into_response())
}

#[derive(Deserialize)]
struct Close {
    status: String,
}

async fn close_assignment(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(b): Json<Close>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.kernel.close_assignment(&ctx, id, &b.status).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct MineQuery {
    /// `Open` (default), `Done`, `Cancelled` or `all`.
    status: Option<String>,
}

async fn my_assignments(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<MineQuery>) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    let rows = match q.status.as_deref() {
        Some("all") => {
            let mut all = st.kernel.my_assignments(&ctx, Some("Open")).await?;
            all.extend(st.kernel.my_assignments(&ctx, Some("Done")).await?);
            all
        }
        other => st.kernel.my_assignments(&ctx, other).await?,
    };
    Ok(Json(json!({"assignments": rows})))
}

async fn tags(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): RecordPath) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    Ok(Json(json!({"tags": st.kernel.tags(&ctx, &entity, id).await?})))
}

#[derive(Deserialize)]
struct Tags {
    tags: Vec<String>,
}

async fn set_tags(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): RecordPath, Json(b): Json<Tags>) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    Ok(Json(json!({"tags": st.kernel.set_tags(&ctx, &entity, id, &b.tags).await?})))
}

async fn tag_counts(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    Ok(Json(json!({"tags": st.kernel.tag_counts(&ctx, &entity).await?})))
}

async fn following(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): RecordPath) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    Ok(Json(json!({"following": st.kernel.following(&ctx, &entity, id).await?})))
}

#[derive(Deserialize)]
struct Follow {
    following: bool,
}

async fn set_following(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): RecordPath, Json(b): Json<Follow>) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    st.kernel.set_following(&ctx, &entity, id, b.following).await?;
    Ok(Json(json!({"following": b.following})))
}

async fn attachments(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): RecordPath) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    Ok(Json(json!({"attachments": st.kernel.attachments(&ctx, &entity, id).await?})))
}

async fn delete_file(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let key = st.kernel.delete_file(&ctx, id).await?;
    // The record is gone already; an orphaned object is harmless and only logged.
    if let Err(e) = app.storage.delete(std::path::Path::new(&key)).await {
        tracing::warn!(%key, "could not delete a file's object: {e}");
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn directory(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    Ok(Json(json!({"users": st.kernel.user_directory(&ctx).await?})))
}

pub fn routes() -> Routes {
    use axum::routing::{delete, get, patch, post};
    Routes::new()
        .prefix("/api")
        .add("/v1/{entity}/{id}/comments", get(comments).post(add_comment))
        .add("/comments/{id}", patch(edit_comment).delete(delete_comment))
        .add("/v1/{entity}/{id}/assignments", get(assignments).post(assign))
        .add("/assignments", get(my_assignments))
        .add("/assignments/{id}/close", post(close_assignment))
        .add("/v1/{entity}/{id}/tags", get(tags).put(set_tags))
        .add("/tags/{entity}", get(tag_counts))
        .add("/v1/{entity}/{id}/follow", get(following).put(set_following))
        .add("/v1/{entity}/{id}/attachments", get(attachments))
        .add("/files/{id}", delete(delete_file))
        .add("/users", get(directory))
}
