//! Connections to outside systems (crates/kernel/src/connect.rs): administrators set them up here,
//! and providers' webhooks arrive here.

use crate::error::ApiResult;
use crate::extract::{Auth, client_ip};
use crate::state::app_state;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::connect::ConnectionInput;
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

async fn connectors(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"connectors": app_state(&app)?.kernel.connectors(&ctx).await?})).into_response())
}

async fn list(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"connections": app_state(&app)?.kernel.connections(&ctx).await?})).into_response())
}

async fn create(Auth(ctx): Auth, State(app): State<AppContext>, Json(c): Json<ConnectionInput>) -> ApiResult<Response> {
    let id = app_state(&app)?.kernel.save_connection(&ctx, None, c).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

async fn update(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(c): Json<ConnectionInput>) -> ApiResult<Response> {
    app_state(&app)?.kernel.save_connection(&ctx, Some(id), c).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn disconnect(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.disconnect(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn test(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(json!({"ok": app_state(&app)?.kernel.test_connection(&ctx, id).await?})).into_response())
}

/// What each of the provider's streams holds in this account, for mapping it.
async fn schema(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(json!({"streams": app_state(&app)?.kernel.connection_schema(&ctx, id).await?})).into_response())
}

#[derive(Deserialize, Default)]
struct Sync {
    #[serde(default)]
    from_start: bool,
}

async fn sync(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, body: Option<Json<Sync>>) -> ApiResult<Response> {
    let job = app_state(&app)?.kernel.sync_connection(&ctx, id, body.is_some_and(|b| b.from_start)).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"job": job}))).into_response())
}

async fn reconcile(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let job = app_state(&app)?.kernel.reconcile_connection(&ctx, id).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"job": job}))).into_response())
}

async fn activity(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.connection_activity(&ctx, id, 50).await?).into_response())
}

async fn drift(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(json!({"drift": app_state(&app)?.kernel.connection_drift(&ctx, id).await?})).into_response())
}

async fn resolve(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.resolve_drift(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/connect/{connection}/webhook`: no session; the connector authenticates what arrives.
async fn webhook(State(app): State<AppContext>, Path(id): Path<Uuid>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.public_limiter.check(&format!("{}|connect", client_ip(&headers))).await?;
    let names: BTreeMap<String, String> = headers.iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
    let queued = st.kernel.connection_webhook(id, &names, &body).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"queued": queued}))).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{get, post, put};
    Routes::new()
        .prefix("/api")
        .add("/admin/connectors", get(connectors))
        .add("/admin/connector-instances", get(list).post(create))
        .add("/admin/connector-instances/{id}", put(update).delete(disconnect))
        .add("/admin/connector-instances/{id}/test", post(test))
        .add("/admin/connector-instances/{id}/schema", get(schema))
        .add("/admin/connector-instances/{id}/sync", post(sync))
        .add("/admin/connector-instances/{id}/reconcile", post(reconcile))
        .add("/admin/connector-instances/{id}/drift", get(drift))
        .add("/admin/connector-instances/{id}/activity", get(activity))
        .add("/admin/connection-drift/{id}/resolve", post(resolve))
        .add("/connect/{id}/webhook", post(webhook))
}
