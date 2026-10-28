//! Health, readiness, version, Prometheus metrics, tenant bootstrap, reports, files, OpenAPI.

use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::Error;
use ferrum_kernel::tenant::NewTenant;
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;
use uuid::Uuid;

static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

async fn ready(State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.kernel.health().await?;
    Ok(Json(json!({"status": "ready"})).into_response())
}

async fn version(State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let packages: Vec<Value> = st.kernel.registry.catalog.packages().map(|p| json!({"name": p.name(), "version": p.manifest.version})).collect();
    Ok(Json(json!({"version": env!("CARGO_PKG_VERSION"), "catalog": st.kernel.registry.catalog.fingerprint(), "packages": packages})).into_response())
}

/// Constant-time comparison for shared secrets.
fn same_secret(given: &str, expected: &str) -> bool {
    given.len() == expected.len() && given.bytes().zip(expected.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

async fn metrics(State(app): State<AppContext>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let expected = st.settings.metrics_token.as_str();
    if !expected.is_empty() {
        let given = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or_default();
        if !same_secret(given, expected) {
            return Err(ApiError(Error::Unauthorized("metrics require the metrics token".into())));
        }
    }
    LazyLock::force(&STARTED);
    let mut tx = st.kernel.db.system_tx().await?;
    let (pending, dead, oldest): (i64, i64, Option<f64>) = sqlx::query_as(
        "select count(*) filter (where processed_at is null and not dead), count(*) filter (where dead), \
         extract(epoch from now() - min(created_at) filter (where processed_at is null and not dead))::float8 from outbox",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::from)?;
    let (wh_pending, wh_dead): (i64, i64) =
        sqlx::query_as("select count(*) filter (where status = 'pending'), count(*) filter (where status = 'dead') from webhook_delivery")
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::from)?;
    let tenants: i64 = sqlx::query_scalar("select count(*) from tenant").fetch_one(&mut *tx).await.map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    let pool = &st.kernel.db.pool;
    let body = format!(
        "# HELP ferrum_up 1 when the process is serving.\n# TYPE ferrum_up gauge\nferrum_up 1\n\
         # TYPE ferrum_uptime_seconds counter\nferrum_uptime_seconds {}\n\
         # TYPE ferrum_db_pool_connections gauge\nferrum_db_pool_connections {}\n\
         # TYPE ferrum_db_pool_idle gauge\nferrum_db_pool_idle {}\n\
         # TYPE ferrum_outbox_pending gauge\nferrum_outbox_pending {pending}\n\
         # TYPE ferrum_outbox_dead gauge\nferrum_outbox_dead {dead}\n\
         # TYPE ferrum_outbox_oldest_pending_seconds gauge\nferrum_outbox_oldest_pending_seconds {}\n\
         # TYPE ferrum_webhook_pending gauge\nferrum_webhook_pending {wh_pending}\n\
         # TYPE ferrum_webhook_dead gauge\nferrum_webhook_dead {wh_dead}\n\
         # TYPE ferrum_tenants gauge\nferrum_tenants {tenants}\n",
        STARTED.elapsed().as_secs(),
        pool.size(),
        pool.num_idle(),
        oldest.unwrap_or(0.0)
    );
    let mut resp = body.into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4"));
    Ok(resp)
}

async fn create_tenant(State(app): State<AppContext>, headers: HeaderMap, Json(t): Json<NewTenant>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let given = headers.get("x-ferrum-bootstrap-token").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let expected = st.settings.bootstrap_token.as_str();
    let ok = !expected.is_empty() && same_secret(given, expected);
    if !ok {
        return Err(ApiError(Error::Forbidden("tenant creation requires the bootstrap token".into())));
    }
    let id = st.kernel.create_tenant(t).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

async fn report(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(ferrum_domain::reports::run(&st.kernel, &ctx, &name, &params).await?).into_response())
}

async fn list_reports(Auth(_ctx): Auth) -> ApiResult<Response> {
    Ok(Json(json!({"reports": ferrum_domain::reports::catalog()})).into_response())
}

async fn openapi(Auth(ctx): Auth) -> ApiResult<Response> {
    Ok(Json(crate::openapi::spec(&ctx)).into_response())
}

const BLOCKED_TYPES: &[&str] = &["text/html", "application/xhtml+xml", "image/svg+xml", "application/javascript", "text/javascript"];

async fn upload(Auth(ctx): Auth, State(app): State<AppContext>, mut mp: Multipart) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut entity: Option<String> = None;
    let mut record: Option<Uuid> = None;
    let mut file: Option<(String, String, bytes::Bytes)> = None;
    while let Some(field) = mp.next_field().await.map_err(|e| ApiError(Error::invalid(format!("multipart: {e}"))))? {
        match field.name().unwrap_or_default() {
            "entity" => entity = Some(field.text().await.map_err(|e| ApiError(Error::invalid(e.to_string())))?),
            "record_id" => record = field.text().await.ok().and_then(|s| s.parse().ok()),
            "file" => {
                let name =
                    field.file_name().unwrap_or("upload.bin").chars().filter(|c| !c.is_control() && *c != '/' && *c != '\\').take(200).collect::<String>();
                let ctype = field.content_type().unwrap_or("application/octet-stream").to_lowercase();
                let data = field.bytes().await.map_err(|e| ApiError(Error::invalid(format!("upload: {e}"))))?;
                file = Some((name, ctype, data));
            }
            _ => {}
        }
    }
    let (name, ctype, data) = file.ok_or_else(|| ApiError(Error::field("file", "is required")))?;
    if data.len() > st.settings.upload_max_bytes {
        return Err(ApiError(Error::field("file", format!("larger than {} bytes", st.settings.upload_max_bytes))));
    }
    let ctype = if BLOCKED_TYPES.contains(&ctype.as_str()) { "application/octet-stream".to_string() } else { ctype };
    if let (Some(e), Some(r)) = (&entity, record) {
        st.kernel.get(&ctx, e, r, false).await?;
    }
    let id = Uuid::now_v7();
    let key = format!("tenants/{}/{}", ctx.tenant, id);
    app.storage.upload(std::path::Path::new(&key), &data).await.map_err(|e| ApiError(Error::Unavailable(format!("storage: {e}"))))?;
    let sha = hex::encode(Sha256::digest(&data));
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    sqlx::query(
        "insert into file_object (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, sha256, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(&entity)
    .bind(record)
    .bind(&name)
    .bind(&ctype)
    .bind(data.len() as i64)
    .bind(&key)
    .bind(&sha)
    .bind(ctx.user)
    .execute(&mut *tx)
    .await
    .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok((StatusCode::CREATED, Json(json!({"id": id, "url": format!("/files/{id}"), "file_name": name, "size": data.len(), "sha256": sha}))).into_response())
}

async fn download(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    let row: Option<(String, String, String, Option<String>, Option<Uuid>)> =
        sqlx::query_as("select file_name, content_type, storage_key, entity, record_id from file_object where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    let (name, ctype, key, entity, record) = row.ok_or_else(|| ApiError(Error::NotFound("file".into())))?;
    if let (Some(e), Some(r)) = (entity, record) {
        st.kernel.get(&ctx, &e, r, false).await?;
    }
    let data: Vec<u8> = app.storage.download(std::path::Path::new(&key)).await.map_err(|e| ApiError(Error::Unavailable(format!("storage: {e}"))))?;
    let mut resp = data.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&ctype).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", name.replace('"', ""))) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

pub fn routes() -> Routes {
    use axum::routing::{get, post};
    Routes::new()
        .add("/", get(|| async { axum::response::Redirect::permanent("/app/") }))
        .add("/api/ready", get(ready))
        .add("/api/version", get(version))
        .add("/metrics", get(metrics))
        .add("/api/tenants", post(create_tenant))
        .add("/api/reports", get(list_reports))
        .add("/api/reports/{name}", get(report))
        .add("/api/openapi.json", get(openapi))
        .add("/api/files", post(upload))
        .add("/files/{id}", get(download))
}
