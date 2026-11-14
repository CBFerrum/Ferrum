//! Metadata for the UI renderer: modules, entities, fields, permissions — all for the caller.

use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::describe::{describe, permissions};
use ferrum_kernel::{Access, Action, Error};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Value, json};

async fn index(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let entities: Vec<Value> = ctx
        .schema
        .entities
        .values()
        .filter(|e| ctx.perms.access(&e.def.name, Action::Read) != Access::None)
        .filter(|e| !matches!(e.def.kind, ferrum_meta::EntityKind::Child))
        .map(|e| {
            json!({
                "name": e.def.name, "label": e.def.label,
                "label_plural": e.def.plural_label(),
                "kind": e.def.kind, "module": e.def.module, "icon": e.def.icon, "description": e.def.description, "submittable": e.def.submittable,
                "custom": e.custom, "package": e.package, "permissions": permissions(&ctx, &e.def.name),
            })
        })
        .collect();
    // Each module with the rows of its app's own pages this person sees (`shell.menu`, BD-63).
    let st = crate::state::app_state(&app)?;
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    let mut pages = crate::shell::menu_pages(&st.apps.shell_apps_for(&ctx).await?, &ctx, &kind);
    let modules: Vec<Value> = ctx
        .schema
        .modules
        .iter()
        // Of a package the caller's zone leaves out (crate::zones, level 2): not shown.
        .filter(|m| ctx.sees_package(&m.package))
        .map(|m| {
            let mut v = json!(m);
            if let Some(rows) = pages.remove(&m.name) {
                v["pages"] = json!(rows);
            }
            v
        })
        .collect();
    Ok(Json(json!({
        "version": ctx.schema.version,
        "packages": ctx.schema.packages.iter().filter(|p| ctx.sees_package(p)).collect::<Vec<_>>(),
        "modules": modules,
        "entities": entities,
        "warnings": if ctx.is_admin { json!(ctx.schema.warnings) } else { json!([]) },
    }))
    .into_response())
}

async fn entity(Auth(ctx): Auth, Path(name): Path<String>) -> ApiResult<Response> {
    // One the caller's zone leaves out (crate::zones, level 2) is not there, child entities too.
    let ent = ctx.schema.entity(&name).filter(|_| ctx.sees(&name)).cloned().ok_or_else(|| ApiError(Error::NotFound(format!("entity {name}"))))?;
    if ctx.perms.access(&name, Action::Read) == Access::None && ent.def.kind != ferrum_meta::EntityKind::Child {
        return Err(ApiError(Error::Forbidden(format!("not permitted to read {name}"))));
    }
    Ok(Json(describe(&ctx, &ent)).into_response())
}

/// `GET /api/meta/contributions/{point}` (docs/api.md, "Extension points").
async fn contributions(Auth(ctx): Auth, State(app): State<AppContext>, Path(point): Path<String>) -> ApiResult<Response> {
    let items = crate::state::app_state(&app)?.apps.contributions(&ctx, &point).await?;
    Ok(Json(json!({"point": point, "items": items})).into_response())
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new().prefix("/api/meta").add("/", get(index)).add("/contributions/{point}", get(contributions)).add("/{entity}", get(entity))
}
