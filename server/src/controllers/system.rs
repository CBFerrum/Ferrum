//! Health, readiness, version, Prometheus metrics, tenant bootstrap, reports, files, OpenAPI.

use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Action, Error};
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
    let packages: Vec<Value> = st.kernel.registry.catalog().packages().map(|p| json!({"name": p.name(), "version": p.manifest.version})).collect();
    let uploads = json!({
        "max_bytes": st.settings.upload_max_bytes,
        "direct": st.objects.direct_uploads(),
        "direct_max_bytes": st.objects.direct_uploads().then_some(st.settings.direct_upload_max_bytes),
        // In parts through the server, on any store, resuming after a dropped connection.
        "parts": true,
        "parts_max_bytes": st.settings.direct_upload_max_bytes,
        "part_bytes": crate::objects::PART_BYTES,
    });
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "catalog": st.kernel.registry.catalog().fingerprint(),
        "packages": packages,
        "uploads": uploads,
        "pdf": st.kernel.pdf_enabled(),
        "mail": st.kernel.mailer.is_some(),
        "mail_attachments": st.kernel.mailer.as_ref().is_some_and(|m| m.can_attach()),
        // `POST /api/v1/{E}/{id}/email` takes `message_html` (rich text, cleaned here).
        "mail_html": true,
        // Print formats (BD-28): 1 formats, letter heads, settings and the print dialog; 2 layouts, the designer's
        // canvas, barcodes, versions.
        "print": 2,
    }))
    .into_response())
}

/// Constant-time comparison for shared secrets.
pub(crate) fn same_secret(given: &str, expected: &str) -> bool {
    given.len() == expected.len() && given.bytes().zip(expected.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

/// The operations on records that did not go through, per package, entity and operation.
fn failures_text(failures: &[(String, String, String, &'static str, u64)]) -> String {
    let mut out = String::from(
        "# HELP ferrum_operation_failures_total Submits, cancels, actions and workflow steps that did not go through since the process \
         started: refused by their own rules (a posting with no account to post to), or a fault.\n# TYPE ferrum_operation_failures_total counter\n",
    );
    // A workspace names its own workflow steps: quoted as the exposition format says.
    let label = |v: &str| v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
    for (package, entity, operation, kind, n) in failures {
        out.push_str(&format!(
            "ferrum_operation_failures_total{{package=\"{}\",entity=\"{}\",operation=\"{}\",kind=\"{kind}\"}} {n}\n",
            label(package),
            label(entity),
            label(operation)
        ));
    }
    out
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
    // Only what this build may take: the worker pool scales on these, and a newer build's work waits for a replica of it
    // (crates/kernel/src/window.rs), counted apart below.
    let takes = format!("processed_at is null and not dead and not {}", ferrum_kernel::window::NOT_YET);
    let (pending, dead, oldest): (i64, i64, Option<f64>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select count(*) filter (where {takes}), count(*) filter (where dead), \
         extract(epoch from now() - min(created_at) filter (where {takes}))::float8 from outbox o"
    )))
    .bind(ferrum_kernel::window::build_date())
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::from)?;
    let (wh_pending, wh_dead): (i64, i64) =
        sqlx::query_as("select count(*) filter (where status = 'pending'), count(*) filter (where status = 'dead') from webhook_delivery")
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::from)?;
    let tenants: i64 = sqlx::query_scalar("select count(*) from tenant").fetch_one(&mut *tx).await.map_err(Error::from)?;
    let (jobs_queued, jobs_running, jobs_failed, job_wait): (i64, i64, i64, Option<f64>) = sqlx::query_as(
        "select count(*) filter (where status = 'queued'), count(*) filter (where status = 'running'), \
         count(*) filter (where status = 'failed' and finished_at > now() - interval '1 day'), \
         extract(epoch from now() - min(run_at) filter (where status = 'queued' and run_at <= now() and (build_date is null or build_date <= $1)))::float8 from job",
    )
    .bind(ferrum_kernel::window::build_date())
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::from)?;
    // Processes (Ferrum's docs/logic/processes.md): how many stand where, across workspaces, and how long the
    // oldest start has waited for the runtime. Nothing about any one workspace.
    let (runs_starting, runs_running, runs_attention, start_wait): (i64, i64, i64, Option<f64>) = sqlx::query_as(
        "select count(*) filter (where status = 'starting'), count(*) filter (where status = 'running'), \
         count(*) filter (where status = 'needs_attention'), \
         extract(epoch from now() - min(accepted_at) filter (where status = 'starting'))::float8 from logic_run",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::from)?;
    let (tasks_open, tasks_overdue, recovery_open): (i64, i64, i64) = sqlx::query_as(
        "select count(*) filter (where status = 'open' and kind in ('task', 'case')), \
         count(*) filter (where status = 'open' and kind in ('task', 'case') and deadline < now()), \
         count(*) filter (where status = 'open' and kind in ('recovery', 'attention')) from logic_task",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    let (newer_events, newer_jobs) = st.kernel.waiting_for_newer().await?;
    let logic = format!(
        "# HELP ferrum_logic_runs Process runs by where they stand.\n# TYPE ferrum_logic_runs gauge\n\
         ferrum_logic_runs{{status=\"starting\"}} {runs_starting}\nferrum_logic_runs{{status=\"running\"}} {runs_running}\n\
         ferrum_logic_runs{{status=\"needs_attention\"}} {runs_attention}\n\
         # HELP ferrum_logic_start_wait_seconds How long the oldest accepted run has waited for the runtime to start it.\n\
         # TYPE ferrum_logic_start_wait_seconds gauge\nferrum_logic_start_wait_seconds {}\n\
         # TYPE ferrum_logic_tasks_open gauge\nferrum_logic_tasks_open {tasks_open}\n\
         # HELP ferrum_logic_tasks_overdue People's tasks past their deadline and still open.\n# TYPE ferrum_logic_tasks_overdue gauge\nferrum_logic_tasks_overdue {tasks_overdue}\n\
         # HELP ferrum_logic_recovery_open Items waiting for an administrator to recover or acknowledge.\n# TYPE ferrum_logic_recovery_open gauge\nferrum_logic_recovery_open {recovery_open}\n",
        start_wait.unwrap_or(0.0)
    );
    // This replica's worker: effects in hand, the workspaces holding them, and steps told to wait their turn.
    #[cfg(feature = "process")]
    let logic = match ferrum_process::budget::STEPS.get() {
        Some(b) => {
            let ((effects, workspaces), deferred, l) = (b.in_hand(), b.deferred(), b.limits);
            format!(
                "{logic}# HELP ferrum_logic_steps_in_hand Effects this replica's worker has in hand.\n# TYPE ferrum_logic_steps_in_hand gauge\n\
                 ferrum_logic_steps_in_hand {effects}\n# TYPE ferrum_logic_steps_workspaces gauge\nferrum_logic_steps_workspaces {workspaces}\n\
                 # HELP ferrum_logic_steps_deferred_total Steps told to wait their turn: their workspace held its share, or every effect slot was taken.\n\
                 # TYPE ferrum_logic_steps_deferred_total counter\nferrum_logic_steps_deferred_total {deferred}\n\
                 # TYPE ferrum_logic_steps_limit gauge\nferrum_logic_steps_limit{{kind=\"slots\"}} {}\nferrum_logic_steps_limit{{kind=\"effects\"}} {}\n\
                 ferrum_logic_steps_limit{{kind=\"per_workspace\"}} {}\n",
                l.slots, l.effects, l.per_workspace
            )
        }
        None => logic,
    };
    let (conn_on, conn_failing, conn_waiting, conn_stale, drift_open) = st.kernel.connection_gauges().await?;
    let (held, fenced) = (u8::from(st.kernel.holding().await?), u8::from(st.kernel.db.fenced()));
    let pool = &st.kernel.db.pool;
    let (socket_dropped, socket_closed_slow) = st.plugins.sockets.overflow_counts();
    let socket_queued = st.plugins.sockets.queued_bytes();
    let mut apps = String::new();
    for app in crate::extensions::all() {
        apps.push_str(&(app.metrics)(&st).await?);
    }
    apps.push_str(&failures_text(&st.kernel.failures()));
    apps.push_str(&logic);
    let body = format!(
        "# HELP ferrum_up 1 when the process is serving.\n# TYPE ferrum_up gauge\nferrum_up 1\n{apps}\
         # TYPE ferrum_restore_held gauge\nferrum_restore_held {held}\n\
         # TYPE ferrum_region_fenced gauge\nferrum_region_fenced {fenced}\n\
         # TYPE ferrum_uptime_seconds counter\nferrum_uptime_seconds {}\n\
         # TYPE ferrum_db_pool_connections gauge\nferrum_db_pool_connections {}\n\
         # TYPE ferrum_db_pool_idle gauge\nferrum_db_pool_idle {}\n\
         # TYPE ferrum_outbox_pending gauge\nferrum_outbox_pending {pending}\n\
         # TYPE ferrum_outbox_dead gauge\nferrum_outbox_dead {dead}\n\
         # TYPE ferrum_outbox_oldest_pending_seconds gauge\nferrum_outbox_oldest_pending_seconds {}\n\
         # TYPE ferrum_webhook_pending gauge\nferrum_webhook_pending {wh_pending}\n\
         # TYPE ferrum_webhook_dead gauge\nferrum_webhook_dead {wh_dead}\n\
         # TYPE ferrum_tenants gauge\nferrum_tenants {tenants}\n\
         # TYPE ferrum_jobs_queued gauge\nferrum_jobs_queued {jobs_queued}\n\
         # TYPE ferrum_jobs_running gauge\nferrum_jobs_running {jobs_running}\n\
         # HELP ferrum_jobs_failed_day Jobs that gave up in the last day.\n# TYPE ferrum_jobs_failed_day gauge\nferrum_jobs_failed_day {jobs_failed}\n\
         # TYPE ferrum_jobs_oldest_waiting_seconds gauge\nferrum_jobs_oldest_waiting_seconds {}\n\
         # HELP ferrum_outbox_waiting_newer_build Events this build leaves for a newer one (written by it, or after one of its in their workspace).\n\
         # TYPE ferrum_outbox_waiting_newer_build gauge\nferrum_outbox_waiting_newer_build {newer_events}\n\
         # HELP ferrum_jobs_waiting_newer_build Jobs a newer build queued, which this build leaves for it.\n\
         # TYPE ferrum_jobs_waiting_newer_build gauge\nferrum_jobs_waiting_newer_build {newer_jobs}\n\
         # HELP ferrum_connections Connections to outside systems that are on.\n# TYPE ferrum_connections gauge\nferrum_connections {conn_on}\n\
         # HELP ferrum_connections_failing Connections whose last calls failed.\n# TYPE ferrum_connections_failing gauge\nferrum_connections_failing {conn_failing}\n\
         # HELP ferrum_connections_waiting Connections left alone because the provider said to wait.\n# TYPE ferrum_connections_waiting gauge\nferrum_connections_waiting {conn_waiting}\n\
         # HELP ferrum_connections_stale Connections with no success for a day.\n# TYPE ferrum_connections_stale gauge\nferrum_connections_stale {conn_stale}\n\
         # HELP ferrum_connection_drift_open Differences from a provider waiting for a person.\n# TYPE ferrum_connection_drift_open gauge\nferrum_connection_drift_open {drift_open}\n\
         # HELP ferrum_tenant_refused_total Requests refused because their workspace already had its share running, on this replica.\n# TYPE ferrum_tenant_refused_total counter\nferrum_tenant_refused_total {}\n\
         # HELP ferrum_plugin_socket_dropped_total Snapshots dropped because a socket's queue was full, on this replica.\n# TYPE ferrum_plugin_socket_dropped_total counter\nferrum_plugin_socket_dropped_total {socket_dropped}\n\
         # HELP ferrum_plugin_socket_closed_slow_total Sockets closed for not keeping up, on this replica.\n# TYPE ferrum_plugin_socket_closed_slow_total counter\nferrum_plugin_socket_closed_slow_total {socket_closed_slow}\n\
         # HELP ferrum_plugin_socket_queued_bytes Bytes waiting for sockets on this replica.\n# TYPE ferrum_plugin_socket_queued_bytes gauge\nferrum_plugin_socket_queued_bytes {socket_queued}\n",
        STARTED.elapsed().as_secs(),
        pool.size(),
        pool.num_idle(),
        oldest.unwrap_or(0.0),
        job_wait.unwrap_or(0.0),
        st.shares.refused.load(std::sync::atomic::Ordering::Relaxed)
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
    let slug = t.slug.clone();
    let id = st.kernel.create_tenant(t).await?;
    // Shapes B and C: its subdomain at once.
    if let Some(base) = st.shell.base() {
        st.kernel.add_subdomain(id, &format!("{slug}.{base}")).await?;
    }
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

async fn list_reports(Auth(ctx): Auth) -> ApiResult<Response> {
    Ok(Json(json!({"reports": ferrum_domain::reports::catalog_for(&ctx)})).into_response())
}

async fn openapi(Auth(ctx): Auth) -> ApiResult<Response> {
    Ok(Json(crate::openapi::spec(&ctx)).into_response())
}

const BLOCKED_TYPES: &[&str] = &["text/html", "application/xhtml+xml", "image/svg+xml", "application/javascript", "text/javascript"];

async fn upload(Auth(ctx): Auth, State(app): State<AppContext>, mut mp: Multipart) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut entity: Option<String> = None;
    let mut record: Option<Uuid> = None;
    let mut public = false;
    let mut file: Option<(String, String, bytes::Bytes)> = None;
    while let Some(field) = mp.next_field().await.map_err(|e| ApiError(Error::invalid(format!("multipart: {e}"))))? {
        match field.name().unwrap_or_default() {
            "entity" => entity = Some(field.text().await.map_err(|e| ApiError(Error::invalid(e.to_string())))?),
            "record_id" => record = field.text().await.ok().and_then(|s| s.parse().ok()),
            "public" => public = field.text().await.ok().is_some_and(|s| s == "true" || s == "1"),
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
    match (&entity, record) {
        (Some(e), Some(r)) => st.kernel.may_attach(&ctx, e, r).await?,
        _ => st.kernel.count_loose_upload(&ctx).await?,
    }
    // Public files (served without signing in) must be images or PDFs attached to a record the
    // uploader may edit, so nothing private or active leaks onto the open internet.
    if public {
        let (Some(e), Some(_)) = (&entity, record) else {
            return Err(ApiError(Error::field("public", "public files must be attached to a record")));
        };
        ctx.require(e, Action::Write)?;
        if !(PUBLIC_TYPES.contains(&ctype.as_str())) {
            return Err(ApiError(Error::field("public", "only images (PNG, JPEG, WebP, GIF, AVIF) and PDFs can be public")));
        }
    }
    let id = Uuid::now_v7();
    let key = format!("tenants/{}/{}", ctx.tenant, id);
    app.storage.upload(std::path::Path::new(&key), &data).await.map_err(|e| ApiError(Error::Unavailable(format!("storage: {e}"))))?;
    let sha = hex::encode(Sha256::digest(&data));
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    sqlx::query(
        "insert into file_object (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, sha256, created_by, is_public, shared) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
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
    .bind(public)
    // What an outside person uploads is shared by nature (BD-22), as the kernel's own uploads keep it.
    .bind(ctx.outside)
    .execute(&mut *tx)
    .await
    .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    let url = if public { format!("/files/public/{id}") } else { format!("/files/{id}") };
    Ok((StatusCode::CREATED, Json(json!({"id": id, "url": url, "file_name": name, "size": data.len(), "sha256": sha, "public": public}))).into_response())
}

const PUBLIC_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif", "image/avif", "application/pdf"];

/// A public file, without signing in. Content never changes for an id, so it caches forever.
async fn download_public(State(app): State<AppContext>, Path(id): Path<Uuid>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.system_tx().await?;
    let row: Option<(String, String, String)> = sqlx::query_as(
        "select f.file_name, f.content_type, f.storage_key from file_object f join tenant t on t.id = f.tenant_id \
                        where f.id = $1 and f.is_public and t.status = 'active'",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    let (name, ctype, key) = row.ok_or_else(|| ApiError(Error::NotFound("file".into())))?;
    let mut resp = streamed(&st, &key, &headers).await?;
    let h = resp.headers_mut();
    file_headers(h, &ctype, &name, true);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    h.insert("content-security-policy", HeaderValue::from_static(FILE_SANDBOX));
    Ok(resp)
}

/// `GET /files/pull/{id}?s=…`: a file's short-lived public address a connection's call made (BD-64), for the service
/// pulling it: no session, its type and length, ranges, never cached; not found alike when the signature is wrong, the
/// address ended or past its time.
async fn download_pulled(
    State(app): State<AppContext>,
    Path(id): Path<Uuid>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    pulled_response(&st.kernel, &st.objects, id, q.get("s").map_or("", String::as_str), &headers).await
}

/// The answer [`download_pulled`] gives, from the kernel and the object store (tests mount it on their own).
pub async fn pulled_response(
    k: &ferrum_kernel::Kernel,
    objects: &crate::objects::Objects,
    id: Uuid,
    signature: &str,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let f = k.pulled_file(id, signature).await?;
    let mut resp = crate::objects::streamed_response(StatusCode::OK, objects.stream(&f.key, super::sites::range(headers)).await?);
    let h = resp.headers_mut();
    file_headers(h, &f.content_type, &f.name, true);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow"));
    Ok(resp)
}

/// What a file a browser would run is served under: it runs nothing and loads nothing but images of this origin.
pub const FILE_SANDBOX: &str = "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox";

/// The headers a stored file is served with: its type (one kept without a type as bytes), never sniffed, by its
/// name; `inline` where the route shows files in place, but a type a browser would run (HTML, SVG, XML, script)
/// always as an attachment under [`FILE_SANDBOX`], so it never runs on the application's own origin.
pub fn file_headers(h: &mut HeaderMap, ctype: &str, name: &str, inline: bool) {
    let ctype = if ctype.trim().is_empty() { "application/octet-stream" } else { ctype };
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(ctype).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    let runs = ferrum_kernel::files::runs_in_browser(ctype);
    if runs {
        h.insert("content-security-policy", HeaderValue::from_static(FILE_SANDBOX));
    }
    let how = if inline && !runs { "inline" } else { "attachment" };
    if let Ok(v) = HeaderValue::from_str(&format!("{how}; filename=\"{}\"", name.replace(['"', '\\'], ""))) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
}

/// A stored object as a streamed response, so a large file never sits whole in memory: the range
/// of it the request asks for, when it asks one (a player seeking in a video).
async fn streamed(st: &crate::state::AppState, key: &str, headers: &HeaderMap) -> ApiResult<Response> {
    Ok(crate::objects::streamed_response(StatusCode::OK, st.objects.stream(key, super::sites::range(headers)).await?))
}

#[derive(serde::Deserialize)]
struct NewUploadBody {
    file_name: String,
    #[serde(default)]
    content_type: String,
    size: u64,
    entity: Option<String>,
    record_id: Option<Uuid>,
    /// Through the server in parts (any store), resuming after a dropped connection.
    #[serde(default)]
    parts: bool,
}

/// Starts an upload straight to object storage, or in parts through the server, for files larger
/// than `POST /api/files` takes.
async fn start_upload(Auth(ctx): Auth, State(app): State<AppContext>, Json(b): Json<NewUploadBody>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let attach_to = match (b.entity, b.record_id) {
        (Some(e), Some(r)) => Some((e, r)),
        (None, None) => None,
        _ => return Err(ApiError(Error::field("record_id", "entity and record_id go together"))),
    };
    let up = crate::objects::NewUpload { file_name: b.file_name, content_type: b.content_type, size: b.size, attach_to, parts: b.parts };
    let v = crate::objects::start(&st.kernel, &st.objects, &ctx, st.settings.direct_upload_max_bytes, up).await?;
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

/// Where an upload stands: the offset its next part starts at.
async fn upload_status(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(crate::objects::status(&st.kernel, &ctx, id).await?).into_response())
}

/// A part of an upload sent in parts, as the raw request body, starting at byte `offset`.
async fn upload_part(Auth(ctx): Auth, State(app): State<AppContext>, Path((id, offset)): Path<(Uuid, u64)>, part: bytes::Bytes) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(crate::objects::append(&st.kernel, &st.objects, &ctx, id, offset, part).await?).into_response())
}

/// Cancels an upload in parts: its parts go now (`204`, also when there is nothing to cancel).
async fn cancel_upload(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    crate::objects::cancel(&st.kernel, &st.objects, &ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn complete_upload(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok((StatusCode::CREATED, Json(crate::objects::complete(&st.kernel, &st.objects, &ctx, id).await?)).into_response())
}

async fn download(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, headers: HeaderMap) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    let row: Option<(String, String, String, Option<String>, Option<Uuid>, Option<Uuid>, bool)> =
        sqlx::query_as("select file_name, content_type, storage_key, entity, record_id, created_by, shared from file_object where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    let (name, ctype, key, entity, record, by, shared) = row.ok_or_else(|| ApiError(Error::NotFound("file".into())))?;
    st.kernel.may_download_file(&ctx, id, entity.as_deref(), record, by, shared).await?;
    let mut resp = streamed(&st, &key, &headers).await?;
    file_headers(resp.headers_mut(), &ctype, &name, false);
    Ok(resp)
}

pub fn routes() -> Routes {
    use axum::routing::{get, post, put};
    Routes::new()
        .add("/api/ready", get(ready))
        .add("/api/version", get(version))
        .add("/metrics", get(metrics))
        .add("/api/tenants", post(create_tenant))
        .add("/api/reports", get(list_reports))
        .add("/api/reports/{name}", get(report))
        .add("/api/openapi.json", get(openapi))
        .add("/api/files", post(upload))
        .add("/api/files/uploads", post(start_upload))
        .add("/api/files/uploads/{id}", get(upload_status).delete(cancel_upload))
        .add("/api/files/uploads/{id}/parts/{offset}", put(upload_part).layer(DefaultBodyLimit::max(crate::objects::PART_BYTES as usize + (64 << 10))))
        .add("/api/files/uploads/{id}/complete", post(complete_upload))
        .add("/files/{id}", get(download))
        .add("/files/public/{id}", get(download_public))
        .add("/files/pull/{id}", get(download_pulled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_counters_with_quoted_labels() {
        let text = failures_text(&[
            ("accounting".into(), "SalesInvoice".into(), "submit".into(), "refused", 3),
            ("tenant".into(), "Claim".into(), "Say \"yes\"\n".into(), "fault", 1),
        ]);
        assert!(text.contains("# TYPE ferrum_operation_failures_total counter\n"));
        assert!(text.contains("ferrum_operation_failures_total{package=\"accounting\",entity=\"SalesInvoice\",operation=\"submit\",kind=\"refused\"} 3\n"));
        assert!(text.contains(r#"operation="Say \"yes\"\n",kind="fault"} 1"#), "a workspace's own step name stays inside its quotes: {text}");
    }
}
