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

#[derive(Deserialize)]
struct ShotQuery {
    version: String,
}

/// `GET /api/admin/apps/{id}/screenshots/{n}?version=`: one of an uploaded app's screenshots.
async fn screenshot(Auth(ctx): Auth, State(app): State<AppContext>, Path((id, n)): Path<(String, usize)>, Query(q): Query<ShotQuery>) -> ApiResult<Response> {
    let (bytes, kind) = app_state(&app)?.apps.screenshot(&ctx, &id, &q.version, n).await?;
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static(kind));
    h.insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("private, max-age=3600"));
    h.insert("x-content-type-options", axum::http::HeaderValue::from_static("nosniff"));
    // An SVG is a document: it runs nothing.
    h.insert("content-security-policy", axum::http::HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"));
    Ok(res)
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
    let tenant = ctx.tenant;
    let deleted = detached({
        let apps = apps.clone();
        async move { apps.uninstall(&ctx, &removed, r.delete_data).await }
    })
    .await??;
    // What it brought in that nothing needs now (a suite's members): the interface offers them.
    let leftovers = apps.leftovers(tenant, &id).await.unwrap_or_else(|e| {
        tracing::warn!(app = %id, "what it brought in could not be listed: {e}");
        Vec::new()
    });
    Ok(Json(json!({"removed": id, "deleted_data": deleted, "leftovers": leftovers})).into_response())
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
        .add("/{id}/screenshots/{n}", get(screenshot))
        .add("/{id}/install", post(install))
        .add("/{id}/upgrade", post(upgrade))
        .add("/{id}/enable", post(enable))
        .add("/{id}/disable", post(disable))
        .add("/{id}/dev-ui", get(dev_ui).put(set_dev_ui).delete(clear_dev_ui))
}

// ── development: an app's pages from its developer's Vite server (`ferrum-app dev --ui`) ──

/// Only on a development server (`FERRUM_DEV_UI`), only administrators.
fn dev_ui_allowed(st: &crate::state::AppState, ctx: &Ctx) -> Result<(), ApiError> {
    if !st.shell.dev_ui {
        return Err(ApiError(Error::NotFound("this is not a development server (FERRUM_DEV_UI)".into())));
    }
    if !ctx.is_admin {
        return Err(ApiError(Error::Forbidden("administrator access required".into())));
    }
    Ok(())
}

/// `GET /api/admin/apps/{id}/dev-ui` → `{app, dev_ui}`: where the app's pages come from in this
/// workspace (`null`: its release).
async fn dev_ui(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<String>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    dev_ui_allowed(&st, &ctx)?;
    Ok(Json(json!({"app": id, "dev_ui": st.shell.dev_ui(ctx.tenant, &id)})).into_response())
}

#[derive(Deserialize)]
struct DevUi {
    url: String,
}

/// `PUT /api/admin/apps/{id}/dev-ui` `{url}`: the app's pages in this workspace come from the Vite
/// server at `url`, on this machine, until `DELETE` or the server restarts.
async fn set_dev_ui(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<String>, Json(req): Json<DevUi>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    dev_ui_allowed(&st, &ctx)?;
    let origin = crate::shell::dev_origin(&req.url)
        .ok_or_else(|| ApiError(Error::field("url", "names a development server on this machine, such as http://localhost:5173")))?;
    if !st.apps.shell_apps(ctx.tenant).await?.iter().any(|a| a.id == id && a.manifest.ui.is_some()) {
        return Err(ApiError(Error::NotFound(format!("this workspace has no app {id} with an interface of its own"))));
    }
    st.shell.set_dev_ui(ctx.tenant, &id, Some(origin.clone()));
    tracing::warn!(app = %id, %origin, "an app's pages come from a development server");
    Ok(Json(json!({"app": id, "dev_ui": origin})).into_response())
}

/// `DELETE /api/admin/apps/{id}/dev-ui`: the app's pages come from its release again.
async fn clear_dev_ui(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<String>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    dev_ui_allowed(&st, &ctx)?;
    st.shell.set_dev_ui(ctx.tenant, &id, None);
    Ok(StatusCode::NO_CONTENT.into_response())
}
