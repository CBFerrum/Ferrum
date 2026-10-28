//! Tenant administration: users, roles, permissions, packages, customizations, webhooks,
//! automations, notifications. Everything except notifications requires an administrator.

use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error};
use ferrum_meta::{OverlayOp, PermissionDef};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

fn admin(ctx: &Ctx) -> Result<(), ApiError> {
    if ctx.is_admin { Ok(()) } else { Err(ApiError(Error::Forbidden("administrator access required".into()))) }
}

async fn query_json(app: &AppContext, ctx: &Ctx, sql: &'static str) -> ApiResult<Vec<Value>> {
    let st = app_state(app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<Value> = sqlx::query_scalar(sql).bind(ctx.tenant).fetch_all(&mut *tx).await.map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(rows)
}

// ── users ──

async fn list_users(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let rows = query_json(
        &app,
        &ctx,
        "select jsonb_build_object('id', u.id, 'email', u.email, 'full_name', u.full_name, 'is_active', u.is_active, \
         'is_admin', u.is_admin, 'last_login_at', u.last_login_at, 'locked', coalesce(u.locked_until > now(), false), \
         'roles', coalesce((select jsonb_agg(r.role order by r.role) from user_role r where r.tenant_id = u.tenant_id and r.user_id = u.id), '[]'::jsonb), \
         'scopes', coalesce((select jsonb_agg(jsonb_build_object('entity', s.entity, 'value', s.value, 'is_default', s.is_default)) \
                    from user_scope s where s.tenant_id = u.tenant_id and s.user_id = u.id), '[]'::jsonb)) \
         from app_user u where u.tenant_id = $1 order by u.email",
    )
    .await?;
    Ok(Json(json!({"users": rows})).into_response())
}

#[derive(Deserialize)]
struct NewUser {
    email: String,
    full_name: String,
    password: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    is_admin: bool,
}

async fn create_user(Auth(ctx): Auth, State(app): State<AppContext>, Json(u): Json<NewUser>) -> ApiResult<Response> {
    let id = app_state(&app)?.kernel.create_user(&ctx, &u.email, &u.full_name, u.password.as_deref(), &u.roles, u.is_admin).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

#[derive(Deserialize)]
struct Roles {
    roles: Vec<String>,
}

async fn set_roles(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(r): Json<Roles>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_user_roles(&ctx, id, &r.roles).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct Scopes {
    entity: String,
    ids: Vec<Uuid>,
    default: Option<Uuid>,
}

async fn set_scopes(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(s): Json<Scopes>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_user_scopes(&ctx, id, &s.entity, &s.ids, s.default).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct Active {
    active: bool,
}

async fn set_active(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(a): Json<Active>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_user_active(&ctx, id, a.active).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct Password {
    password: String,
}

async fn reset_password(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(p): Json<Password>) -> ApiResult<Response> {
    admin(&ctx)?;
    app_state(&app)?.kernel.set_password(&ctx, id, None, &p.password).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── roles & permissions ──

async fn list_roles(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let rows = query_json(
        &app,
        &ctx,
        "select jsonb_build_object('name', r.name, 'description', r.description, 'package', r.package, 'permissions', \
         coalesce((select jsonb_agg(to_jsonb(p) - 'tenant_id' - 'role' order by p.entity) from role_permission p \
                   where p.tenant_id = r.tenant_id and p.role = r.name), '[]'::jsonb)) \
         from role r where r.tenant_id = $1 order by r.name",
    )
    .await?;
    Ok(Json(json!({"roles": rows})).into_response())
}

async fn set_permission(Auth(ctx): Auth, State(app): State<AppContext>, Path(role): Path<String>, Json(p): Json<PermissionDef>) -> ApiResult<Response> {
    if role.trim().is_empty() || role.len() > 100 {
        return Err(ApiError(Error::field("role", "invalid role name")));
    }
    app_state(&app)?.kernel.set_role_permission(&ctx, role.trim(), &p).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── packages ──

async fn list_packages(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let all: Vec<Value> = st
        .kernel
        .registry
        .catalog()
        .packages()
        .map(|p| {
            json!({"name": p.name(), "version": p.manifest.version, "label": p.manifest.label, "description": p.manifest.description,
                   "depends": p.manifest.depends, "installed": ctx.schema.packages.iter().any(|x| x == p.name())})
        })
        .collect();
    Ok(Json(json!({"packages": all})).into_response())
}

#[derive(Deserialize)]
struct Install {
    packages: Vec<String>,
}

async fn install(Auth(ctx): Auth, State(app): State<AppContext>, Json(req): Json<Install>) -> ApiResult<Response> {
    admin(&ctx)?;
    let installed = app_state(&app)?.kernel.install_packages(ctx.tenant, &req.packages).await?;
    Ok(Json(json!({"installed": installed})).into_response())
}

// ── customizations ──

async fn list_custom(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let rows = app_state(&app)?.kernel.list_customizations(&ctx).await?;
    Ok(Json(json!({"customizations": rows, "warnings": ctx.schema.warnings})).into_response())
}

#[derive(Deserialize)]
struct Customize {
    entity: String,
    op: OverlayOp,
    note: Option<String>,
}

async fn add_custom(Auth(ctx): Auth, State(app): State<AppContext>, Json(c): Json<Customize>) -> ApiResult<Response> {
    let id = app_state(&app)?.kernel.add_customization(&ctx, &c.entity, c.op, c.note).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

async fn toggle_custom(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(a): Json<Active>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_customization_active(&ctx, id, a.active).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn export_custom(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.export_customizations(&ctx).await?).into_response())
}

async fn import_custom(Auth(ctx): Auth, State(app): State<AppContext>, Json(bundle): Json<Value>) -> ApiResult<Response> {
    let n = app_state(&app)?.kernel.import_customizations(&ctx, &bundle).await?;
    Ok(Json(json!({"imported": n})).into_response())
}

// ── webhooks ──

async fn list_webhooks(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let rows = query_json(
        &app,
        &ctx,
        "select jsonb_build_object('id', w.id, 'name', w.name, 'url', w.url, 'topics', w.topics, 'is_active', w.is_active, 'created_at', w.created_at, \
         'pending', (select count(*) from webhook_delivery d where d.tenant_id = w.tenant_id and d.webhook_id = w.id and d.status = 'pending'), \
         'dead', (select count(*) from webhook_delivery d where d.tenant_id = w.tenant_id and d.webhook_id = w.id and d.status = 'dead')) \
         from webhook w where w.tenant_id = $1 order by w.created_at",
    )
    .await?;
    Ok(Json(json!({"webhooks": rows})).into_response())
}

#[derive(Deserialize)]
struct NewWebhook {
    name: String,
    url: String,
    topics: Vec<String>,
}

async fn create_webhook(Auth(ctx): Auth, State(app): State<AppContext>, Json(w): Json<NewWebhook>) -> ApiResult<Response> {
    admin(&ctx)?;
    ferrum_kernel::outbox::ensure_webhook_url(&w.url)?;
    if w.topics.is_empty() || w.topics.len() > 50 {
        return Err(ApiError(Error::field("topics", "give 1 to 50 topics, e.g. SalesInvoice.submitted or SalesInvoice.* or *")));
    }
    let st = app_state(&app)?;
    let id = Uuid::now_v7();
    let secret: String = hex::encode(uuid::Uuid::new_v4().as_bytes()) + &hex::encode(uuid::Uuid::new_v4().as_bytes());
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    sqlx::query("insert into webhook (tenant_id, id, name, url, secret, topics, created_by) values ($1, $2, $3, $4, $5, $6, $7)")
        .bind(ctx.tenant)
        .bind(id)
        .bind(w.name.trim())
        .bind(&w.url)
        .bind(&secret)
        .bind(&w.topics)
        .bind(ctx.user)
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": id, "secret": secret, "note": "Verify X-Ferrum-Signature (t=<ts>,v1=<hex HMAC-SHA256 of '<ts>.<body>'>) with this secret."})),
    )
        .into_response())
}

async fn delete_webhook(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    sqlx::query("delete from webhook where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await.map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── automations ──

async fn list_automations(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let rows = query_json(&app, &ctx, "select to_jsonb(a) - 'tenant_id' from automation a where a.tenant_id = $1 order by a.created_at").await?;
    Ok(Json(json!({"automations": rows})).into_response())
}

async fn save_automation(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<Value>) -> ApiResult<Response> {
    admin(&ctx)?;
    let input = body.as_object().cloned().ok_or_else(|| ApiError(Error::invalid("expected an object")))?;
    let id = ferrum_kernel::automation::save(&app_state(&app)?.kernel, &ctx, &input).await?;
    Ok(Json(json!({"id": id})).into_response())
}

async fn delete_automation(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    sqlx::query("delete from automation where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await.map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── notifications (any user, own only) ──

async fn notifications(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(n) - 'tenant_id' from notification n where n.tenant_id = $1 and n.user_id = $2 order by n.created_at desc limit 100",
    )
    .bind(ctx.tenant)
    .bind(ctx.user)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(Json(json!({"notifications": rows})).into_response())
}

async fn mark_read(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    sqlx::query("update notification set read_at = now() where tenant_id = $1 and user_id = $2 and id = $3 and read_at is null")
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct PluginUpload {
    #[serde(flatten)]
    plugin: ferrum_plugins::NewPlugin,
    /// The module, base64-encoded (`.wasm`, or `.wat` text for small plugins).
    wasm_base64: String,
}

async fn list_plugins(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(json!({"plugins": st.plugins.list(&st.kernel, &ctx).await?})).into_response())
}

async fn install_plugin(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<PluginUpload>) -> ApiResult<Response> {
    use base64::Engine as _;
    let st = app_state(&app)?;
    let wasm =
        base64::engine::general_purpose::STANDARD.decode(body.wasm_base64.trim()).map_err(|_| ApiError(Error::field("wasm_base64", "is not valid base64")))?;
    let id = st.plugins.install(&st.kernel, &ctx, body.plugin, &wasm).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

#[derive(Deserialize)]
struct Enabled {
    enabled: bool,
}

async fn toggle_plugin(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(body): Json<Enabled>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.plugins.set_enabled(&st.kernel, &ctx, id, body.enabled).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct PluginConfig {
    config: serde_json::Map<String, Value>,
}

/// `PUT` replaces the configuration; `PATCH` merges it (`null` removes a key).
async fn configure_plugin(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    method: Method,
    Path(id): Path<Uuid>,
    Json(body): Json<PluginConfig>,
) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.plugins.set_config(&st.kernel, &ctx, id, body.config, method == Method::PATCH).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn remove_plugin(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    st.plugins.remove(&st.kernel, &ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{delete, get, post, put};
    Routes::new()
        .prefix("/api")
        .add("/admin/users", get(list_users).post(create_user))
        .add("/admin/users/{id}/roles", put(set_roles))
        .add("/admin/users/{id}/scopes", put(set_scopes))
        .add("/admin/users/{id}/active", put(set_active))
        .add("/admin/users/{id}/password", put(reset_password))
        .add("/admin/roles", get(list_roles))
        .add("/admin/roles/{role}/permissions", put(set_permission))
        .add("/admin/packages", get(list_packages).post(install))
        .add("/admin/webhooks", get(list_webhooks).post(create_webhook))
        .add("/admin/webhooks/{id}", delete(delete_webhook))
        .add("/admin/automations", get(list_automations).post(save_automation))
        .add("/admin/automations/{id}", delete(delete_automation))
        .add("/admin/plugins", get(list_plugins).post(install_plugin))
        .add("/admin/plugins/{id}", delete(remove_plugin))
        .add("/admin/plugins/{id}/enabled", put(toggle_plugin))
        .add("/admin/plugins/{id}/config", put(configure_plugin).patch(configure_plugin))
        .add("/customize", get(list_custom).post(add_custom))
        .add("/customize/export", get(export_custom))
        .add("/customize/import", post(import_custom))
        .add("/customize/{id}/active", put(toggle_custom))
        .add("/notifications", get(notifications))
        .add("/notifications/{id}/read", post(mark_read))
}
