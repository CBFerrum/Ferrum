//! Background work: what the caller has running, and an administrator's view of the workspace's
//! (crates/kernel/src/jobs.rs).

use crate::error::ApiResult;
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

#[derive(Deserialize)]
struct ListJobs {
    limit: Option<i64>,
}

async fn list(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<ListJobs>) -> ApiResult<Response> {
    Ok(Json(json!({"jobs": app_state(&app)?.kernel.jobs(&ctx, q.limit.unwrap_or(50)).await?})).into_response())
}

async fn one(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.job(&ctx, id).await?).into_response())
}

async fn cancel(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.cancel_job(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn retry(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.retry_job(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// One zip of a finished export; the job's result lists them.
async fn export_part(Auth(ctx): Auth, State(app): State<AppContext>, Path((id, part)): Path<(Uuid, Uuid)>) -> ApiResult<Response> {
    let (name, bytes) = app_state(&app)?.kernel.export_part(&ctx, id, part).await?;
    Ok(([(header::CONTENT_TYPE, "application/zip".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))], bytes)
        .into_response())
}

async fn export(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let id = app_state(&app)?.kernel.export_workspace(&ctx).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"job": id}))).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .prefix("/api")
        .add("/jobs", get(list))
        .add("/jobs/{id}", get(one))
        .add("/jobs/{id}/cancel", post(cancel))
        .add("/jobs/{id}/retry", post(retry))
        .add("/jobs/{id}/parts/{part}", get(export_part))
        .add("/admin/export", post(export))
}
