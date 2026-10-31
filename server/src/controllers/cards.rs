//! `GET /api/cards/{entity}/{id}`: a record's card (docs/plan/apps-ui.md §11.6), derived from its
//! metadata, for those who may read it (crates/kernel `Kernel::card`). An app that serves its own
//! cards (`shell.card`) is asked by the bar itself, with the same shape.

use crate::error::ApiResult;
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use uuid::Uuid;

async fn card(Auth(ctx): Auth, State(app): State<AppContext>, Path((entity, id)): Path<(String, Uuid)>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let card = st.kernel.card(&ctx, &entity, id).await?;
    // Briefly cached by the person's browser: hovering the same link again costs nothing.
    Ok(([(header::CACHE_CONTROL, "private, max-age=30")], Json(card)).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new().add("/api/cards/{entity}/{id}", get(card))
}
