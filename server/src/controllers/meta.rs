//! Metadata for the UI renderer: modules, entities, fields, permissions — all for the caller.

use crate::error::{ApiError, ApiResult};
use crate::extract::Auth;
use axum::Json;
use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Access, Action, Ctx, Error};
use ferrum_meta::{EffEntity, FieldDef};
use loco_rs::controller::Routes;
use serde_json::{Value, json};

fn access(a: Access) -> &'static str {
    match a {
        Access::None => "none",
        Access::Own => "own",
        Access::All => "all",
    }
}

fn permissions(ctx: &Ctx, entity: &str) -> Value {
    let p = |a| access(ctx.perms.access(entity, a));
    json!({
        "read": p(Action::Read), "write": p(Action::Write), "create": p(Action::Create), "delete": p(Action::Delete),
        "submit": p(Action::Submit), "cancel": p(Action::Cancel), "export": p(Action::Export), "import": p(Action::Import),
    })
}

fn field_json(ctx: &Ctx, ent: &EffEntity, f: &FieldDef) -> Value {
    let mut v = serde_json::to_value(f).unwrap_or(Value::Null);
    if let Value::Object(m) = &mut v {
        m.insert("label".into(), json!(f.label()));
        m.insert("storage".into(), json!(ent.storage_of(&f.name)));
        m.insert("can_read".into(), json!(ctx.perms.can_read_field(&ent.def.name, f)));
        m.insert("can_write".into(), json!(ctx.perms.can_write_field(&ent.def.name, f)));
        m.retain(|_, x| !x.is_null() && x != &json!(false) && x != &json!([]));
    }
    v
}

pub fn describe(ctx: &Ctx, ent: &EffEntity) -> Value {
    let children: serde_json::Map<String, Value> = ent
        .children
        .iter()
        .filter_map(|(field, info)| {
            let c = ctx.schema.entity(&info.entity)?;
            Some((
                field.clone(),
                json!({"entity": c.def.name, "label": c.def.label, "fields": c.def.fields.iter().map(|f| field_json(ctx, c, f)).collect::<Vec<_>>()}),
            ))
        })
        .collect();
    json!({
        "name": ent.def.name,
        "label": ent.def.label,
        "label_plural": ent.def.label_plural.clone().unwrap_or_else(|| format!("{}s", ent.def.label)),
        "kind": ent.def.kind,
        "module": ent.def.module,
        "icon": ent.def.icon,
        "description": ent.def.description,
        "package": ent.package,
        "custom": ent.custom,
        "title_field": ent.def.title_field,
        "submittable": ent.def.submittable,
        "company_scoped": ent.def.company_scoped,
        "numbering": ent.def.numbering,
        "list": ent.def.list,
        "form": ent.def.form,
        "search_fields": ent.def.search_fields,
        "rules": ent.def.rules,
        "actions": ent.def.actions,
        "workflow": ent.workflow.as_ref().map(|w| &w.def),
        "fields": ent.def.fields.iter().map(|f| field_json(ctx, ent, f)).collect::<Vec<_>>(),
        "children": children,
        "permissions": permissions(ctx, &ent.def.name),
    })
}

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
                "label_plural": e.def.label_plural.clone().unwrap_or_else(|| format!("{}s", e.def.label)),
                "kind": e.def.kind, "module": e.def.module, "icon": e.def.icon, "submittable": e.def.submittable,
                "custom": e.custom, "permissions": permissions(&ctx, &e.def.name),
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

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new().prefix("/api/meta").add("/", get(index)).add("/{entity}", get(entity))
}
