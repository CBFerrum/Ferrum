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

async fn index(Auth(ctx): Auth) -> ApiResult<Response> {
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
                "kind": e.def.kind, "module": e.def.module, "icon": e.def.icon, "submittable": e.def.submittable,
                "custom": e.custom, "package": e.package, "permissions": permissions(&ctx, &e.def.name),
            })
        })
        .collect();
    Ok(Json(json!({
        "version": ctx.schema.version,
        "packages": ctx.schema.packages,
        "modules": ctx.schema.modules,
        "entities": entities,
        "warnings": if ctx.is_admin { json!(ctx.schema.warnings) } else { json!([]) },
    }))
    .into_response())
}

async fn entity(Auth(ctx): Auth, Path(name): Path<String>) -> ApiResult<Response> {
    let ent = ctx.schema.entity(&name).cloned().ok_or_else(|| ApiError(Error::NotFound(format!("entity {name}"))))?;
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
