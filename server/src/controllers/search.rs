//! `GET /api/search?q=&limit=`: the workspace's records the person may read, by title
//! (server/src/search.rs). The bar asks this and the apps' `shell.search` providers together.

use crate::error::ApiResult;
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;

#[derive(Deserialize)]
struct Params {
    #[serde(default)]
    q: String,
    #[serde(default)]
    limit: Option<usize>,
}

async fn search(Auth(ctx): Auth, State(app): State<AppContext>, Query(p): Query<Params>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(crate::search::search(&st.kernel, &ctx, &p.q, p.limit.unwrap_or(20)).await?).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new().add("/api/search", get(search))
}
