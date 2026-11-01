//! The signed-in user's interface preferences (`ferrum_kernel::prefs`): list settings, saved
//! filters, panel states. Each user only ever sees their own. Views a team shares
//! (`ferrum_kernel::views`) sit beside them.

use crate::error::ApiResult;
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

async fn all(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    Ok(Json(json!({"prefs": st.kernel.prefs(&ctx).await?})))
}

#[derive(Deserialize)]
struct SetPref {
    value: Value,
    /// The version read; a newer write elsewhere makes this a conflict (409).
    #[serde(default)]
    version: Option<i64>,
}

async fn set(Auth(ctx): Auth, State(app): State<AppContext>, Path(scope): Path<String>, Json(b): Json<SetPref>) -> ApiResult<Json<Value>> {
    let st = app_state(&app)?;
    let version = st.kernel.set_pref(&ctx, &scope, &b.value, b.version).await?;
    Ok(Json(json!({"scope": scope, "value": b.value, "version": version})))
}

async fn reset(Auth(ctx): Auth, State(app): State<AppContext>, Path(scope): Path<String>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.kernel.delete_pref(&ctx, &scope).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn views(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"views": app_state(&app)?.kernel.shared_views(&ctx, &entity).await?})))
}

#[derive(Deserialize)]
struct SharedView {
    name: String,
    value: Value,
    #[serde(default)]
    version: i32,
}

async fn share(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>, Json(b): Json<SharedView>) -> ApiResult<Response> {
    let view = app_state(&app)?.kernel.share_view(&ctx, &entity, &b.name, &b.value).await?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn change(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>, Json(b): Json<SharedView>) -> ApiResult<Json<Value>> {
    Ok(Json(app_state(&app)?.kernel.change_shared_view(&ctx, &entity, id, &b.name, &b.value, b.version).await?))
}

async fn unshare(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>) -> ApiResult<Response> {
    app_state(&app)?.kernel.unshare_view(&ctx, &entity, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{get, put};
    Routes::new()
        .prefix("/api")
        .add("/prefs", get(all))
        .add("/prefs/{scope}", put(set).delete(reset))
        .add("/views/{entity}", get(views).post(share))
        .add("/views/{entity}/{id}", put(change).delete(unshare))
}
