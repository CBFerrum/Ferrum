//! Metadata as the caller may see it: entities, fields, actions, workflow and permissions. Used by
//! the REST metadata endpoints, the MCP server and anything else that explains the schema.

use crate::ctx::{Access, Action, Ctx};
use ferrum_meta::{EffEntity, FieldDef, FieldType};
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

/// What links to `entity`, as far as the caller may read it: the entity, and the filter that finds
/// a record's links there (`items.item` for a link in a child table).
fn linked_from(ctx: &Ctx, entity: &str) -> Vec<Value> {
    let links = |e: &EffEntity| -> Vec<FieldDef> {
        e.value_fields()
            .filter(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some(entity) && ctx.perms.can_read_field(&e.def.name, f))
            .cloned()
            .collect()
    };
    let mut out = Vec::new();
    for e in ctx.schema.entities.values().filter(|e| !e.table.is_empty() && ctx.perms.access(&e.def.name, Action::Read) != Access::None) {
        for f in links(e) {
            out.push(json!({"entity": e.def.name, "label": e.def.plural_label(), "field": f.name, "field_label": f.label()}));
        }
        for (table, info) in &e.children {
            let Some(c) = ctx.schema.entity(&info.entity) else { continue };
            for f in links(c) {
                out.push(json!({"entity": e.def.name, "label": e.def.plural_label(), "field": format!("{table}.{}", f.name), "field_label": f.label()}));
            }
        }
    }
    out.sort_by_key(|v| (v["label"].as_str().unwrap_or_default().to_string(), v["field"].as_str().unwrap_or_default().to_string()));
    out
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
        "label_plural": ent.def.plural_label(),
        "kind": ent.def.kind,
        "module": ent.def.module,
        "icon": ent.def.icon,
        "description": ent.def.description,
        "package": ent.package,
        "custom": ent.custom,
        "title_field": ent.def.title_field,
        "submittable": ent.def.submittable,
        "mergeable": ent.def.mergeable,
        "linked_from": linked_from(ctx, &ent.def.name),
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
