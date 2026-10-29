//! The Apps page API (docs/api.md, "Apps"), administrators only: installed and available apps,
//! store search, installation plans, install / upgrade with consent, enable, disable, remove,
//! and bundle uploads.

use crate::appmgr::{MAX_BUNDLE, Refusal};
use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use crate::state::{Uploads, app_state};
use axum::Json;
use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::json;

async fn list(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.apps.overview(&ctx).await?).into_response())
}

#[derive(Deserialize)]
struct Search {
    #[serde(default)]
    q: String,
    #[serde(default)]
    category: String,
}

async fn store(Auth(ctx): Auth, State(app): State<AppContext>, Query(s): Query<Search>) -> ApiResult<Response> {
    let apps = app_state(&app)?.apps.search(&ctx, &s.q, &s.category).await?;
    Ok(Json(json!({"apps": apps})).into_response())
}

#[derive(Deserialize)]
struct Wanted {
    version: Option<String>,
}

async fn plan(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<String>, Query(w): Query<Wanted>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.apps.plan(&ctx, &id, w.version.as_deref()).await?).into_response())
}

#[derive(Deserialize)]
struct Consent {
    #[serde(default)]
    version: Option<String>,
    /// The plan's `digest`.
    consent: String,
}

/// Runs an app change to its end even when the client goes away: a request timeout or a closed
/// tab must not stop an installation half-way (it finishes, or takes itself back).
async fn detached<T: Send + 'static>(work: impl Future<Output = T> + Send + 'static) -> Result<T, ApiError> {
    tokio::spawn(work).await.map_err(|e| ApiError(Error::internal(format!("app change: {e}"))))
}

async fn apply(app: &AppContext, ctx: Ctx, id: String, body: Consent, upgrade: bool) -> ApiResult<Response> {
    let apps = app_state(app)?.apps;
    match detached(async move { apps.install(&ctx, &id, body.version.as_deref(), &body.consent, upgrade).await }).await? {
        Ok(plan) => Ok(Json(plan).into_response()),
        // The plan changed (or was never reviewed): answer with the fresh one to review.
        Err(Refusal::Stale(plan)) => {
            Ok((StatusCode::CONFLICT, Json(json!({"error": "conflict", "message": plan.changed_message(), "plan": plan}))).into_response())
        }
        Err(Refusal::Failed(e)) => Err(e.into()),
    }
}

async fn install(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<String>, Json(body): Json<Consent>) -> ApiResult<Response> {
    apply(&app, ctx, id, body, false).await
}

async fn upgrade(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<String>, Json(body): Json<Consent>) -> ApiResult<Response> {
    apply(&app, ctx, id, body, true).await
}

async fn toggle(app: &AppContext, ctx: Ctx, id: String, enabled: bool) -> ApiResult<Response> {
    let apps = app_state(app)?.apps;
    detached(async move { apps.set_enabled(&ctx, &id, enabled).await }).await??;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn enable(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<String>) -> ApiResult<Response> {
    toggle(&app, ctx, id, true).await
}

async fn disable(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<String>) -> ApiResult<Response> {
    toggle(&app, ctx, id, false).await
}

#[derive(Deserialize)]
struct Remove {
    #[serde(default)]
    delete_data: bool,
}

/// Also mounted on `/store` and `/upload`, whose static routes would otherwise hide the apps
/// with those ids: the id is the last path segment.
async fn uninstall(Auth(ctx): Auth, State(app): State<AppContext>, uri: Uri, Query(r): Query<Remove>) -> ApiResult<Response> {
    let id = uri.path().trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string();
    let apps = app_state(&app)?.apps;
    let removed = id.clone();
    let deleted = detached(async move { apps.uninstall(&ctx, &removed, r.delete_data).await }).await??;
    Ok(Json(json!({"removed": id, "deleted_data": deleted})).into_response())
}

fn bad_form(e: MultipartError) -> ApiError {
    ApiError(Error::invalid(format!("upload: {}", e.body_text())))
}

/// `multipart/form-data` with the bundle in the field `bundle` (≤ 50 MB).
async fn upload(Auth(ctx): Auth, State(app): State<AppContext>, mut form: Multipart) -> ApiResult<Response> {
    let st = app_state(&app)?;
    // Refuse before reading up to 50 MB.
    if !ctx.is_admin {
        return Err(ApiError(Error::Forbidden("administrator access required".into())));
    }
    if st.apps.uploads == Uploads::Off {
        return Err(ApiError(Error::Forbidden("uploading apps is turned off on this server (apps.uploads)".into())));
    }
    let mut bundle = None;
    while let Some(mut field) = form.next_field().await.map_err(bad_form)? {
        if field.name() != Some("bundle") {
            continue;
        }
        let mut data = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(bad_form)? {
            if data.len() + chunk.len() > MAX_BUNDLE {
                return Err(ApiError(Error::invalid("app bundles are limited to 50 MB")));
            }
            data.extend_from_slice(&chunk);
        }
        bundle = Some(data);
    }
    let bytes = bundle.ok_or_else(|| ApiError(Error::field("bundle", "attach the .fapp file as the form field 'bundle'")))?;
    let apps = st.apps;
    let (summary, plan) = detached(async move { apps.upload(&ctx, bytes.into()).await }).await??;
    Ok((StatusCode::CREATED, Json(json!({"app": summary, "plan": plan}))).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{delete, get, post};
    Routes::new()
        .prefix("/api/admin/apps")
        .add("/", get(list))
        .add("/store", get(store).delete(uninstall))
        // Room for the multipart framing around a 50 MB bundle (the server-wide limit is lower).
        .add("/upload", post(upload).delete(uninstall).layer(DefaultBodyLimit::max(MAX_BUNDLE + (1 << 20))))
        .add("/{id}", delete(uninstall))
        .add("/{id}/plan", get(plan))
        .add("/{id}/install", post(install))
        .add("/{id}/upgrade", post(upgrade))
        .add("/{id}/enable", post(enable))
        .add("/{id}/disable", post(disable))
}
