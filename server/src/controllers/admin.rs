//! Tenant administration: users, roles, permissions, packages, customizations, webhooks,
//! automations, business calendars. Everything requires an administrator (people's own notifications are in
//! notifications.rs).

use crate::error::{ApiError, ApiResult};
use crate::extract::{Auth, BrowserSession};
use crate::state::app_state;
use axum::Json;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::retention::RetentionInput;
use ferrum_kernel::support::GrantInput;
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
         'two_factor', u.totp_enabled_at is not null, 'sso', u.sso_subject is not null, 'kind', u.kind, \
         'roles', coalesce((select jsonb_agg(r.role order by r.role) from user_role r where r.tenant_id = u.tenant_id and r.user_id = u.id \
                   and (r.expires_at is null or r.expires_at > now())), '[]'::jsonb), \
         'role_until', coalesce((select jsonb_object_agg(r.role, r.expires_at) from user_role r where r.tenant_id = u.tenant_id and r.user_id = u.id \
                   and r.expires_at > now()), '{}'::jsonb), \
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
    /// `member` (the default) or `portal`, `guest`, `service` (docs/plan/apps-ui.md §9.1).
    #[serde(default)]
    kind: Option<String>,
}

async fn create_user(Auth(ctx): Auth, State(app): State<AppContext>, Json(u): Json<NewUser>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let kind = u.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()).unwrap_or("member");
    if !ferrum_kernel::shell::USER_KINDS.contains(&kind) {
        return Err(ApiError(Error::field("kind", "must be member, guest, portal or service")));
    }
    // Portal people and guests are never administrators; guests hold no roles, portal people only
    // portal roles (docs/builder/decisions.md BD-02), given once they are portal people.
    if matches!(kind, "portal" | "guest") && u.is_admin {
        return Err(ApiError(Error::field("is_admin", "portal people and guests are not administrators")));
    }
    if kind == "guest" && !u.roles.is_empty() {
        return Err(ApiError(Error::field("roles", "guests hold no roles")));
    }
    let member_roles: &[String] = if kind == "portal" { &[] } else { &u.roles };
    let id = st.kernel.create_user(&ctx, &u.email, &u.full_name, u.password.as_deref(), member_roles, u.is_admin).await?;
    if kind != "member" {
        st.kernel.set_user_kind(&ctx, id, kind).await?;
    }
    if kind == "portal" && !u.roles.is_empty() {
        st.kernel.set_user_roles_until(&ctx, id, &u.roles, &Default::default()).await?;
    }
    Ok((StatusCode::CREATED, Json(json!({"id": id, "kind": kind}))).into_response())
}

#[derive(Deserialize)]
struct Kind {
    kind: String,
}

/// Makes someone a member, a portal person, a guest or a service account; they are signed out
/// everywhere when it changes.
async fn set_kind(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(k): Json<Kind>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_user_kind(&ctx, id, k.kind.trim()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct Roles {
    roles: Vec<String>,
    /// The roles held only until a moment, and that moment; the others are held for good.
    #[serde(default)]
    until: std::collections::BTreeMap<String, chrono::DateTime<chrono::Utc>>,
}

async fn set_roles(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(r): Json<Roles>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_user_roles_until(&ctx, id, &r.roles, &r.until).await?;
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

/// Unlinks someone from their identity at a single sign-on provider.
async fn unlink_sso(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.unlink_sso(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── single sign-on providers ──

async fn list_sso(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let kernel = &app_state(&app)?.kernel;
    Ok(Json(json!({"providers": kernel.sso_providers(&ctx).await?, "saml": kernel.saml_service_provider(ctx.tenant)})).into_response())
}

async fn create_sso(Auth(ctx): Auth, State(app): State<AppContext>, Json(p): Json<ferrum_kernel::sso::SsoProviderInput>) -> ApiResult<Response> {
    let id = app_state(&app)?.kernel.save_sso_provider(&ctx, None, p).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

async fn update_sso(
    Auth(ctx): Auth,
    State(app): State<AppContext>,
    Path(id): Path<Uuid>,
    Json(p): Json<ferrum_kernel::sso::SsoProviderInput>,
) -> ApiResult<Response> {
    app_state(&app)?.kernel.save_sso_provider(&ctx, Some(id), p).await?;
    Ok(Json(json!({"id": id})).into_response())
}

async fn delete_sso(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.delete_sso_provider(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Turns someone's two-factor sign-in off (a lost device) and signs them out everywhere.
async fn reset_two_factor(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.reset_two_factor(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── roles & permissions ──

async fn list_roles(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let rows = query_json(
        &app,
        &ctx,
        "select jsonb_build_object('name', r.name, 'description', r.description, 'package', r.package, 'portal', r.portal, 'permissions', \
         coalesce((select jsonb_agg(to_jsonb(p) - 'tenant_id' - 'role' order by p.entity) from role_permission p \
                   where p.tenant_id = r.tenant_id and p.role = r.name), '[]'::jsonb)) \
         from role r where r.tenant_id = $1 order by r.name",
    )
    .await?;
    Ok(Json(json!({"roles": rows})).into_response())
}

#[derive(Deserialize)]
struct Portal {
    portal: bool,
}

/// Marks a role for the portal or not (docs/builder/decisions.md BD-02).
async fn set_role_portal(Auth(ctx): Auth, State(app): State<AppContext>, Path(role): Path<String>, Json(p): Json<Portal>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_role_portal(&ctx, role.trim(), p.portal).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn set_permission(Auth(ctx): Auth, State(app): State<AppContext>, Path(role): Path<String>, Json(p): Json<PermissionDef>) -> ApiResult<Response> {
    if role.trim().is_empty() || role.len() > 100 {
        return Err(ApiError(Error::field("role", "invalid role name")));
    }
    app_state(&app)?.kernel.set_role_permission(&ctx, role.trim(), &p).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── personal data ──

/// Everything kept about a person (their email, and their phone number and name when given):
/// records and login account.
async fn personal_data(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<ferrum_kernel::privacy::Person>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.personal_data_of(&ctx, &q).await?).into_response())
}

#[derive(Deserialize)]
struct EraseReq {
    #[serde(flatten)]
    person: ferrum_kernel::privacy::Person,
    reason: String,
}

async fn erase_personal_data(Auth(ctx): Auth, State(app): State<AppContext>, Json(req): Json<EraseReq>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.erase_personal_data_of(&ctx, &req.person, &req.reason).await?).into_response())
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
    let done = app_state(&app)?.kernel.install_packages_reporting(ctx.tenant, &req.packages).await?;
    Ok(Json(json!({"installed": done.packages, "kept_newer": done.kept_newer})).into_response())
}

// ── retention and restriction of processing ──

async fn list_retention(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"rules": app_state(&app)?.kernel.retention_rules(&ctx).await?})).into_response())
}

async fn add_retention(Auth(ctx): Auth, State(app): State<AppContext>, Json(r): Json<RetentionInput>) -> ApiResult<Response> {
    let id = app_state(&app)?.kernel.save_retention_rule(&ctx, None, r).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

async fn save_retention(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(r): Json<RetentionInput>) -> ApiResult<Response> {
    app_state(&app)?.kernel.save_retention_rule(&ctx, Some(id), r).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_retention(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.delete_retention_rule(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn preview_retention(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.retention_preview(&ctx, id).await?).into_response())
}

#[derive(Deserialize)]
struct Restriction {
    email: String,
    #[serde(default)]
    reason: String,
}

async fn list_restrictions(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"restrictions": app_state(&app)?.kernel.restrictions(&ctx).await?})).into_response())
}

async fn restrict(Auth(ctx): Auth, State(app): State<AppContext>, Json(r): Json<Restriction>) -> ApiResult<Response> {
    let addresses = app_state(&app)?.kernel.restrict_processing(&ctx, &r.email, &r.reason).await?;
    Ok((StatusCode::CREATED, Json(json!({"addresses": addresses}))).into_response())
}

async fn lift_restriction(Auth(ctx): Auth, State(app): State<AppContext>, Json(r): Json<Restriction>) -> ApiResult<Response> {
    app_state(&app)?.kernel.lift_restriction(&ctx, &r.email).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── support access and the account ──

async fn support_grants(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"grants": app_state(&app)?.kernel.support_grants(&ctx).await?})).into_response())
}

async fn grant_support(BrowserSession(ctx, device): BrowserSession, State(app): State<AppContext>, Json(g): Json<GrantInput>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    // The strongest access there is asks the person to prove it is them, again.
    if g.level == "incident" {
        let device = device.ok_or_else(|| ApiError(Error::Forbidden("incident access is given from a signed-in browser".into())))?;
        if !st.kernel.check_session(ctx.tenant, ctx.user, device).await?.stepped_up() {
            return Err(ApiError(Error::Forbidden("confirm your sign-in first".into())));
        }
    }
    let (id, key) = st.kernel.grant_support(&ctx, g).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id, "key": key}))).into_response())
}

async fn revoke_support(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.revoke_support(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn support_activity(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    Ok(Json(json!({"requests": app_state(&app)?.kernel.support_activity(&ctx, id).await?})).into_response())
}

async fn usage(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.usage(&ctx).await?).into_response())
}

#[derive(Deserialize)]
struct CloseReq {
    confirm: String,
}

/// `POST /api/admin/close`: its administrator closes the workspace, its name typed to confirm.
async fn close_workspace(Auth(ctx): Auth, State(app): State<AppContext>, Json(req): Json<CloseReq>) -> ApiResult<Response> {
    let until = app_state(&app)?.kernel.close_own_workspace(&ctx, &req.confirm).await?;
    Ok(Json(json!({"state": "closed", "retain_until": until})).into_response())
}

/// `GET /api/support/report`: what a support key sees of how the workspace is doing.
async fn support_report(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let (_, level) = st.kernel.support_level(&ctx).await?.ok_or_else(|| ApiError(Error::Forbidden("this is for support access".into())))?;
    Ok(Json(st.kernel.support_report(&ctx, &level).await?).into_response())
}

// ── customizations ──

async fn custom_conflicts(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    Ok(Json(json!({"conflicts": app_state(&app)?.kernel.customization_conflicts(&ctx).await?})).into_response())
}

async fn keep_custom(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    admin(&ctx)?;
    app_state(&app)?.kernel.keep_customization(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

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

/// `POST /api/customize/batch`: every operation in order in one transaction, or none; a refusal
/// is the single endpoint's, with the refused operation's `index`, one that does not parse included.
async fn batch_custom(Auth(ctx): Auth, State(app): State<AppContext>, body: axum::body::Bytes) -> ApiResult<Response> {
    let (ops, dry_run) = match ferrum_kernel::customize::parse_batch(&body) {
        Ok(b) => b,
        Err((index, e)) => return Ok(refused(index, e)),
    };
    match app_state(&app)?.kernel.batch_customizations(&ctx, ops, dry_run).await {
        Ok(_) if dry_run => Ok(Json(json!({"ok": true})).into_response()),
        Ok(n) => Ok(Json(json!({"applied": n})).into_response()),
        Err((index, e)) => Ok(refused(index, e)),
    }
}

fn refused(index: Option<usize>, e: Error) -> Response {
    ApiError(e).with(index.map(|i| ("index".to_string(), json!(i))).into_iter().collect())
}

async fn toggle_custom(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(a): Json<Active>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_customization_active(&ctx, id, a.active).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn export_custom(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.export_customizations(&ctx).await?).into_response())
}

/// `GET /api/customize/definition/{Entity}`: a custom entity's definition as it is now, for a designer to start from.
async fn custom_definition(Auth(ctx): Auth, State(app): State<AppContext>, Path(entity): Path<String>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.custom_definition(&ctx, &entity)?).into_response())
}

async fn import_custom(Auth(ctx): Auth, State(app): State<AppContext>, Json(bundle): Json<Value>) -> ApiResult<Response> {
    let n = app_state(&app)?.kernel.import_customizations(&ctx, &bundle).await?;
    Ok(Json(json!({"imported": n})).into_response())
}

// ── packs ──

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackQuery {
    /// Comma-separated: entities whose records travel as reference data, in this order.
    #[serde(default)]
    pub records: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub version: Option<i64>,
}

/// `GET /api/admin/pack`: this workspace's configuration as a `ferrum.pack/1` document.
pub async fn export_pack(k: &ferrum_kernel::Kernel, ctx: &Ctx, q: &PackQuery) -> ApiResult<Value> {
    admin(ctx)?;
    let records = q.records.as_deref().unwrap_or_default().split(',').map(str::trim).filter(|r| !r.is_empty()).map(str::to_string).collect();
    let o = ferrum_kernel::pack::ExportOptions {
        name: q.name.clone().unwrap_or_else(|| "workspace".into()),
        label: q.label.clone().unwrap_or_else(|| "Workspace configuration".into()),
        version: q.version.unwrap_or(1),
        records,
    };
    Ok(k.export_pack(ctx, &o).await?)
}

/// The pack a request carries, and nothing else beside it but what `allowed` names.
fn pack_body<'a>(body: &'a Value, allowed: &[&str]) -> ApiResult<&'a Value> {
    let m = body.as_object().ok_or_else(|| ApiError(Error::invalid("expected an object holding the pack")))?;
    if let Some(k) = m.keys().find(|k| *k != "pack" && !allowed.contains(&k.as_str())) {
        return Err(ApiError(Error::field(k.clone(), "is not part of this request")));
    }
    m.get("pack").ok_or_else(|| ApiError(Error::field("pack", "is required")))
}

/// `POST /api/admin/pack/plan` `{pack}`: what installing would do here, doing nothing.
pub async fn plan_pack(k: &ferrum_kernel::Kernel, ctx: &Ctx, body: &Value) -> ApiResult<Value> {
    admin(ctx)?;
    Ok(k.plan_pack(ctx, pack_body(body, &[])?).await?)
}

/// `POST /api/admin/pack/install` `{pack, digest}`: installs the pack whose plan has that digest.
pub async fn install_pack(k: &ferrum_kernel::Kernel, ctx: &Ctx, body: &Value) -> ApiResult<Value> {
    admin(ctx)?;
    let pack = pack_body(body, &["digest"])?;
    let digest = body.get("digest").and_then(Value::as_str).ok_or_else(|| ApiError(Error::field("digest", "is required: the digest of the plan installed")))?;
    Ok(k.install_pack(ctx, pack, digest).await?)
}

/// `GET /api/admin/capabilities`: what a configured action or an automation may `invoke` (docs/api.md), from the
/// kernel's catalog: each capability's id, title, inputs, effect and whether it needs an effect key.
pub fn capabilities(k: &ferrum_kernel::Kernel, ctx: &Ctx) -> ApiResult<Value> {
    admin(ctx)?;
    let list: Vec<Value> = k
        .catalog(ctx)
        .into_iter()
        .map(|c| {
            let required: Vec<&str> = c.input.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            let inputs: Vec<Value> = c.input.get("properties").and_then(Value::as_object).into_iter().flatten()
                .map(|(name, s)| json!({"name": name, "type": s.get("type"), "required": required.contains(&name.as_str()), "description": s.get("description")}))
                .collect();
            json!({"id": c.id, "title": c.label, "entity": c.entity, "effect": c.effect, "needs_key": c.effect.changes(), "inputs": inputs})
        })
        .collect();
    Ok(json!({"capabilities": list}))
}

async fn list_capabilities(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(capabilities(&app_state(&app)?.kernel, &ctx)?).into_response())
}

async fn get_pack(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<PackQuery>) -> ApiResult<Response> {
    Ok(Json(export_pack(&app_state(&app)?.kernel, &ctx, &q).await?).into_response())
}

async fn post_pack_plan(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<Value>) -> ApiResult<Response> {
    Ok(Json(plan_pack(&app_state(&app)?.kernel, &ctx, &body).await?).into_response())
}

async fn post_pack_install(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<Value>) -> ApiResult<Response> {
    Ok(Json(install_pack(&app_state(&app)?.kernel, &ctx, &body).await?).into_response())
}

async fn list_packs(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    Ok(Json(json!({"packs": app_state(&app)?.kernel.installed_packs(&ctx).await?})).into_response())
}

// ── legal holds ──

#[derive(Deserialize)]
struct NewHold {
    email: Option<String>,
    entity: Option<String>,
    record_id: Option<Uuid>,
    reason: String,
}

async fn legal_holds(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"holds": app_state(&app)?.kernel.legal_holds(&ctx).await?})).into_response())
}

async fn place_legal_hold(Auth(ctx): Auth, State(app): State<AppContext>, Json(h): Json<NewHold>) -> ApiResult<Response> {
    let record = h.entity.as_deref().zip(h.record_id);
    let id = app_state(&app)?.kernel.place_legal_hold(&ctx, h.email.as_deref(), record, &h.reason).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

async fn release_legal_hold(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.release_legal_hold(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── segregation of duties ──

async fn list_role_conflicts(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"conflicts": app_state(&app)?.kernel.role_conflicts(&ctx).await?})).into_response())
}

#[derive(Deserialize)]
struct RoleConflict {
    role_a: String,
    role_b: String,
    #[serde(default)]
    reason: String,
}

async fn set_role_conflict(Auth(ctx): Auth, State(app): State<AppContext>, Json(c): Json<RoleConflict>) -> ApiResult<Response> {
    app_state(&app)?.kernel.set_role_conflict(&ctx, &c.role_a, &c.role_b, &c.reason).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn delete_role_conflict(Auth(ctx): Auth, State(app): State<AppContext>, Query(c): Query<RoleConflict>) -> ApiResult<Response> {
    app_state(&app)?.kernel.delete_role_conflict(&ctx, &c.role_a, &c.role_b).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── backfills: data a release moves in batches ──

async fn list_backfills(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"backfills": app_state(&app)?.kernel.backfills(&ctx).await?})).into_response())
}

async fn rerun_backfill(Auth(ctx): Auth, State(app): State<AppContext>, Path((package, name)): Path<(String, String)>) -> ApiResult<Response> {
    app_state(&app)?.kernel.rerun_backfill(&ctx, &package, &name).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

// ── why someone may or may not do something ──

#[derive(Deserialize)]
struct ExplainAccess {
    user: Uuid,
    entity: String,
    action: String,
    record: Option<Uuid>,
    /// Comma-separated: answers for these roles instead of the person's own.
    roles: Option<String>,
}

async fn explain_access(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<ExplainAccess>) -> ApiResult<Response> {
    let action = ferrum_kernel::Action::parse(&q.action)
        .ok_or_else(|| ApiError(Error::field("action", "must be read, write, create, delete, submit, cancel, export or import")))?;
    let roles = q.roles.map(|r| r.split(',').map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).collect());
    Ok(Json(app_state(&app)?.kernel.explain_access(&ctx, q.user, &q.entity, action, q.record, roles).await?).into_response())
}

// ── people leaving, and the identity provider's groups ──

#[derive(Deserialize)]
struct Deprovision {
    reassign_to: Option<Uuid>,
    reason: String,
}

async fn deprovision_user(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(b): Json<Deprovision>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.deprovision_user(&ctx, id, b.reassign_to, &b.reason).await?).into_response())
}

#[derive(Deserialize)]
struct HandOver {
    to: Uuid,
    reason: String,
}

async fn hand_over_work(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(b): Json<HandOver>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.hand_over_work(&ctx, id, b.to, &b.reason).await?).into_response())
}

async fn scim_groups(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(json!({"groups": app_state(&app)?.kernel.scim_group_map(&ctx).await?})).into_response())
}

#[derive(Deserialize)]
struct GroupRoles {
    roles: Vec<String>,
}

async fn map_scim_group(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>, Json(b): Json<GroupRoles>) -> ApiResult<Response> {
    app_state(&app)?.kernel.scim_map_group(&ctx, id, &b.roles).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── events that failed ──

#[derive(Deserialize)]
struct DeadEvents {
    limit: Option<i64>,
}

async fn dead_events(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<DeadEvents>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(json!({"events": st.kernel.dead_events(&ctx, q.limit.unwrap_or(100)).await?})).into_response())
}

async fn replay_event(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<i64>) -> ApiResult<Response> {
    app_state(&app)?.kernel.replay_event(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn operation(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<String>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.operation(&ctx, &id).await?).into_response())
}

async fn dead_webhook_deliveries(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<DeadEvents>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(json!({"deliveries": st.kernel.dead_webhook_deliveries(&ctx, q.limit.unwrap_or(100)).await?})).into_response())
}

async fn replay_webhook_delivery(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    app_state(&app)?.kernel.replay_webhook_delivery(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── webhooks ──

async fn list_webhooks(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let rows = query_json(
        &app,
        &ctx,
        "select jsonb_build_object('id', w.id, 'name', w.name, 'url', w.url, 'topics', w.topics, 'is_active', w.is_active, 'created_at', w.created_at, 'app', w.app, \
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
        .bind(st.kernel.seal_webhook_secret(ctx.tenant, id, &secret)?)
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
    let app: Option<Option<String>> = sqlx::query_scalar("select app from webhook where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::from)?;
    if let Some(Some(app)) = app {
        return Err(ApiError(Error::invalid(format!("this webhook is how the {app} app hears of events; turn the app off or remove it instead"))));
    }
    sqlx::query("delete from webhook where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await.map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── business calendars (the same kernel functions Studio's route calls) ──

/// `GET /admin/calendars`, or one by name: `{name, zone, weekends, holidays, hours, revision, updated_by, updated_at}`.
pub async fn calendars(k: &ferrum_kernel::Kernel, ctx: &Ctx, name: Option<&str>) -> ApiResult<Value> {
    admin(ctx)?;
    let all = k.logic_calendars(ctx).await?;
    Ok(match name {
        None => json!({"calendars": all}),
        Some(n) => all.into_iter().find(|c| c["name"] == n).ok_or_else(|| ApiError(Error::NotFound(format!("calendar {n}"))))?,
    })
}

/// `PUT /admin/calendars/{name}` `{zone, weekends, holidays, hours?}` → `{revision}`.
pub async fn put_calendar(k: &ferrum_kernel::Kernel, ctx: &Ctx, name: &str, body: &Value) -> ApiResult<Value> {
    Ok(json!({"revision": k.put_logic_calendar(ctx, name, body).await?}))
}

async fn list_calendars(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(calendars(&app_state(&app)?.kernel, &ctx, None).await?).into_response())
}

async fn get_calendar(Auth(ctx): Auth, State(app): State<AppContext>, Path(name): Path<String>) -> ApiResult<Response> {
    Ok(Json(calendars(&app_state(&app)?.kernel, &ctx, Some(&name)).await?).into_response())
}

async fn save_calendar(Auth(ctx): Auth, State(app): State<AppContext>, Path(name): Path<String>, Json(body): Json<Value>) -> ApiResult<Response> {
    Ok(Json(put_calendar(&app_state(&app)?.kernel, &ctx, &name, &body).await?).into_response())
}

async fn delete_calendar(Auth(ctx): Auth, State(app): State<AppContext>, Path(name): Path<String>) -> ApiResult<Response> {
    app_state(&app)?.kernel.delete_logic_calendar(&ctx, &name).await?;
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

/// `POST /admin/automations/preview`: a mail action's HTML as it would be sent, against one record.
pub async fn preview_automation_mail(k: &ferrum_kernel::Kernel, ctx: &Ctx, body: &Value) -> ApiResult<Value> {
    admin(ctx)?;
    let text = |key: &str| body.get(key).and_then(Value::as_str);
    let entity = text("entity").ok_or_else(|| ApiError(Error::field("entity", "is required")))?;
    let record = match body.get("record_id").filter(|v| !v.is_null()) {
        Some(v) => Some(v.as_str().and_then(|s| s.parse().ok()).ok_or_else(|| ApiError(Error::field("record_id", "must be an id")))?),
        None => None,
    };
    let (html, id) = ferrum_kernel::automation::preview_mail(k, ctx, entity, text("body"), text("template"), record).await?;
    Ok(json!({"html": html, "record_id": id}))
}

async fn preview_automation(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<Value>) -> ApiResult<Response> {
    Ok(Json(preview_automation_mail(&app_state(&app)?.kernel, &ctx, &body).await?).into_response())
}

async fn delete_automation(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    sqlx::query("delete from automation where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await.map_err(Error::from)?;
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

async fn get_settings(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(st.kernel.tenant_settings(&ctx).await?).into_response())
}

async fn put_settings(Auth(ctx): Auth, State(app): State<AppContext>, Json(body): Json<serde_json::Map<String, Value>>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    Ok(Json(st.kernel.update_tenant_settings(&ctx, &body).await?).into_response())
}

/// `GET /api/admin/evidence`: what a reviewer asks for, as a file (`ferrum_kernel::evidence`).
async fn evidence(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let pack = app_state(&app)?.kernel.evidence_pack(&ctx).await?;
    let name = format!("attachment; filename=\"evidence-{}.json\"", chrono::Utc::now().format("%Y-%m-%d"));
    Ok(([(header::CONTENT_DISPOSITION, name)], Json(pack)).into_response())
}

/// `GET /api/admin/services` → `{services: [{check, level, detail, fix?}]}`: whether the online editor
/// (Word, Excel and PowerPoint files in Drive) and the PDF renderer are set up and answer, so that an
/// administrator can see why files do not open. Addresses and tokens are never part of it.
async fn services(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    admin(&ctx)?;
    let st = app_state(&app)?;
    Ok(Json(json!({"services": crate::doctor::service_findings(&st).await})).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{delete, get, post, put};
    Routes::new()
        .prefix("/api")
        .add("/admin/services", get(services))
        .add("/admin/evidence", get(evidence))
        .add("/admin/settings", get(get_settings).put(put_settings))
        .add("/admin/users", get(list_users).post(create_user))
        .add("/admin/users/{id}/roles", put(set_roles))
        .add("/admin/users/{id}/scopes", put(set_scopes))
        .add("/admin/users/{id}/active", put(set_active))
        .add("/admin/users/{id}/kind", put(set_kind))
        .add("/admin/users/{id}/password", put(reset_password))
        .add("/admin/users/{id}/two-factor/reset", post(reset_two_factor))
        .add("/admin/users/{id}/sso/unlink", post(unlink_sso))
        .add("/admin/sso", get(list_sso).post(create_sso))
        .add("/admin/sso/{id}", put(update_sso).delete(delete_sso))
        .add("/admin/roles", get(list_roles))
        .add("/admin/roles/{role}/permissions", put(set_permission))
        .add("/admin/roles/{role}/portal", put(set_role_portal))
        .add("/admin/personal-data", get(personal_data))
        .add("/admin/personal-data/erase", post(erase_personal_data))
        .add("/admin/packages", get(list_packages).post(install))
        .add("/admin/legal-holds", get(legal_holds).post(place_legal_hold))
        .add("/admin/legal-holds/{id}/release", post(release_legal_hold))
        .add("/admin/access/explain", get(explain_access))
        .add("/admin/role-conflicts", get(list_role_conflicts).put(set_role_conflict).delete(delete_role_conflict))
        .add("/admin/backfills", get(list_backfills))
        .add("/admin/backfills/{package}/{name}/rerun", post(rerun_backfill))
        .add("/admin/users/{id}/deprovision", post(deprovision_user))
        .add("/admin/users/{id}/hand-over", post(hand_over_work))
        .add("/admin/scim/groups", get(scim_groups))
        .add("/admin/scim/groups/{id}/roles", put(map_scim_group))
        .add("/admin/events/dead", get(dead_events))
        .add("/admin/events/{id}/replay", post(replay_event))
        .add("/admin/webhook-deliveries/dead", get(dead_webhook_deliveries))
        .add("/admin/operations/{id}", get(operation))
        .add("/admin/webhook-deliveries/{id}/replay", post(replay_webhook_delivery))
        .add("/admin/webhooks", get(list_webhooks).post(create_webhook))
        .add("/admin/webhooks/{id}", delete(delete_webhook))
        .add("/admin/calendars", get(list_calendars))
        .add("/admin/calendars/{name}", get(get_calendar).put(save_calendar).delete(delete_calendar))
        .add("/admin/automations", get(list_automations).post(save_automation))
        .add("/admin/automations/preview", post(preview_automation))
        .add("/admin/automations/{id}", delete(delete_automation))
        .add("/admin/plugins", get(list_plugins).post(install_plugin))
        .add("/admin/plugins/{id}", delete(remove_plugin))
        .add("/admin/plugins/{id}/enabled", put(toggle_plugin))
        .add("/admin/plugins/{id}/config", put(configure_plugin).patch(configure_plugin))
        .add("/customize", get(list_custom).post(add_custom))
        .add("/customize/export", get(export_custom))
        .add("/customize/batch", post(batch_custom))
        .add("/customize/definition/{entity}", get(custom_definition))
        .add("/customize/import", post(import_custom))
        .add("/admin/pack", get(get_pack))
        // A pack is at most 5 MiB; a larger body still reaches the pack's own refusal, which says why.
        .add("/admin/pack/plan", post(post_pack_plan).layer(DefaultBodyLimit::max(2 * ferrum_kernel::pack::MAX_PACK)))
        .add("/admin/pack/install", post(post_pack_install).layer(DefaultBodyLimit::max(2 * ferrum_kernel::pack::MAX_PACK)))
        .add("/admin/packs", get(list_packs))
        .add("/admin/capabilities", get(list_capabilities))
        .add("/admin/support", get(support_grants).post(grant_support))
        .add("/admin/support/{id}", delete(revoke_support))
        .add("/admin/support/{id}/requests", get(support_activity))
        .add("/admin/usage", get(usage))
        .add("/admin/close", post(close_workspace))
        .add("/support/report", get(support_report))
        .add("/admin/retention", get(list_retention).post(add_retention))
        .add("/admin/retention/{id}", put(save_retention).delete(delete_retention))
        .add("/admin/retention/{id}/preview", post(preview_retention))
        .add("/admin/restrictions", get(list_restrictions).post(restrict))
        .add("/admin/restrictions/lift", post(lift_restriction))
        .add("/customize/conflicts", get(custom_conflicts))
        .add("/customize/{id}/active", put(toggle_custom))
        .add("/customize/{id}/keep", post(keep_custom))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body(r: Response) -> (StatusCode, Value) {
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn a_refused_batch_is_the_single_endpoints_refusal_with_its_index() {
        let (status, json) = body(refused(Some(1), Error::invalid("customization rejected: XLine: nope"))).await;
        let (single, alone) = body(ApiError(Error::invalid("customization rejected: XLine: nope")).into_response()).await;
        assert_eq!(status, single);
        assert_eq!((&json["error"], &json["message"]), (&alone["error"], &alone["message"]));
        assert_eq!(json["index"], json!(1));
        let (status, json) = body(refused(None, Error::Forbidden("only administrators can customize".into()))).await;
        assert_eq!((status, json.get("index")), (StatusCode::FORBIDDEN, None));
    }

    #[tokio::test]
    async fn a_batch_names_its_operations_and_nothing_else() {
        use ferrum_kernel::customize::parse_batch;
        let parse = |v: Value| parse_batch(v.to_string().as_bytes());
        let (ops, dry_run) =
            parse(json!({"ops": [{"entity": "Customer", "op": {"op": "add_options", "field": "tier", "options": ["Gold"]}}], "dry_run": true})).unwrap();
        assert!(dry_run && ops.len() == 1 && ops[0].note.is_none());
        assert!(parse(json!({"ops": [], "dryrun": true})).is_err());
        assert!(parse(json!({"ops": [{"entity": "Customer", "op": {"op": "drop_entity"}}]})).is_err());
        // An operation that does not parse is refused in the route's own shape, with its index and the property.
        let (index, e) = parse(json!({"ops": [{"entity": "X", "op": {"op": "add_options", "field": "f", "options": []}},
            {"entity": "X", "op": {"op": "add_action", "action": {"name": "x_go", "label": "Go", "may_run": true}}}]}))
        .unwrap_err();
        let (status, json) = body(refused(index, e)).await;
        assert_eq!((status, &json["error"], &json["index"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid"), &json!(1)));
        assert!(json["message"].as_str().unwrap().contains("may_run"), "{json}");
        let (index, e) = parse(json!({"dry_run": true})).unwrap_err();
        let (status, json) = body(refused(index, e)).await;
        assert_eq!((status, &json["error"], json.get("index")), (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid"), None));
    }
}
