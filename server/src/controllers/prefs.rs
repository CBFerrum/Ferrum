//! The signed-in user's interface preferences (`ferrum_kernel::prefs`): list settings, saved
//! filters, panel states. Each user only ever sees their own.

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

pub fn routes() -> Routes {
    use axum::routing::{get, put};
    Routes::new().prefix("/api").add("/prefs", get(all)).add("/prefs/{scope}", put(set).delete(reset))
}
