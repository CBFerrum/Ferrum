//! Metadata as the caller may see it: entities, fields, actions, workflow and permissions. Used by
//! the REST metadata endpoints, the MCP server and anything else that explains the schema.

use crate::ctx::{Access, Action, Ctx};
use ferrum_meta::{EffEntity, FieldDef};
use serde_json::{Value, json};

pub fn access(a: Access) -> &'static str {
    match a {
        Access::None => "none",
        Access::Own => "own",
        Access::All => "all",
    }
}

pub fn permissions(ctx: &Ctx, entity: &str) -> Value {
    let p = |a| access(ctx.perms.access(entity, a));
    json!({
        "read": p(Action::Read), "write": p(Action::Write), "create": p(Action::Create), "delete": p(Action::Delete),
        "submit": p(Action::Submit), "cancel": p(Action::Cancel), "export": p(Action::Export), "import": p(Action::Import),
    })
}

pub fn field_json(ctx: &Ctx, ent: &EffEntity, f: &FieldDef) -> Value {
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
