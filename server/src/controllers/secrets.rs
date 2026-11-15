//! A workspace's secrets, for its administrators (docs/api.md, "Secrets"; crates/kernel/src/vault): by owner, their
//! names, rules, when each was set and last used, never a value. Setting, rotating and removing one needs an
//! administrator who has just confirmed it is them: otherwise `403` with error `step_up` and its ways. A value the vault
//! makes is shown once, in the answer that made it.

use super::operator::{SecretBody, by_owner, changed_secret};
use crate::error::ApiResult;
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::json;

fn no_store(mut res: Response) -> Response {
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// `GET /api/admin/secrets`: the workspace's secrets, by owner.
async fn list(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(no_store(Json(json!({"owners": by_owner(st.kernel.vault_secrets(&ctx).await?)})).into_response()))
}

/// `PUT /api/admin/secrets/{owner}/{name}`: sets one, with its rule (none: the default, used and never revealed).
async fn set(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((owner, name)): Path<(String, String)>,
    Json(mut body): Json<SecretBody>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let value = body.new_value()?;
    Ok(changed_secret(st.kernel.vault_set(&ctx, &owner, &name, value, body.rule.take().unwrap_or_default()).await?))
}

/// `POST /api/admin/secrets/{owner}/{name}/rotate`: a new version under its rule; the one before verifies for its overlap.
async fn rotate(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path((owner, name)): Path<(String, String)>,
    Json(mut body): Json<SecretBody>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let value = body.new_value()?;
    Ok(changed_secret(st.kernel.vault_rotate(&ctx, &owner, &name, value).await?))
}

/// `DELETE /api/admin/secrets/{owner}/{name}`: removes it, every version.
async fn remove(Auth(ctx): Auth, State(app): State<AppContext>, Path((owner, name)): Path<(String, String)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(no_store(Json(json!({"removed": st.kernel.vault_remove(&ctx, &owner, &name).await?})).into_response()))
}

pub fn routes() -> Routes {
    use axum::routing::{get, post, put};
    Routes::new()
        .prefix("/api")
        .add("/admin/secrets", get(list))
        .add("/admin/secrets/{owner}/{name}", put(set).delete(remove))
        .add("/admin/secrets/{owner}/{name}/rotate", post(rotate))
}
