//! Metadata as the caller may see it: entities, fields, actions, workflow and permissions. Used by
//! the REST metadata endpoints, the MCP server and anything else that explains the schema.

use crate::ctx::{Access, Action, Ctx};
use ferrum_meta::{EffEntity, FieldDef, FieldType};
use serde_json::{Map, Value, json};

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
        "actions": ctx.perms.named_actions(entity),
    })
}

pub fn field_json(ctx: &Ctx, ent: &EffEntity, f: &FieldDef) -> Value {
    let mut v = serde_json::to_value(f).unwrap_or(Value::Null);
    if let Value::Object(m) = &mut v {
        m.insert("label".into(), json!(f.label()));
        m.insert("storage".into(), json!(ent.storage_of(&f.name)));
        m.insert("can_read".into(), json!(ctx.perms.can_read_field(&ent.def.name, f)));
        m.insert("can_write".into(), json!(ctx.perms.can_write_field(&ent.def.name, f)));
        // Kept by a service target, as a total is: what people send for it is ignored.
        m.insert("kept_by_target".into(), json!(ent.target_keeping(&f.name).map(|t| &t.name)));
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

/// An entity's actions as the caller sees them: each says whether they may press it (`may_run`: its permission or
/// a rule naming it, and its roles, as the run path asks); what a configured action writes (`effects`) is for an
/// administrator's designer, not for the person pressing it.
fn actions(ctx: &Ctx, ent: &EffEntity) -> Vec<Value> {
    ent.def
        .actions
        .iter()
        .map(|a| {
            let mut v = serde_json::to_value(a).unwrap_or(Value::Null);
            if let Value::Object(m) = &mut v {
                if !ctx.is_admin {
                    m.remove("effects");
                }
                m.insert("may_run".into(), json!(crate::ctx::action_gate(ctx, ent, a).is_ok()));
            }
            v
        })
        .collect()
}

/// The entity's workflow as the caller sees it: each transition says whether they may take it (`may_take`, as the
/// run path asks; the record's own scope, state and guard are still asked when taken).
fn workflow(ctx: &Ctx, ent: &EffEntity) -> Option<Value> {
    let def = &ent.workflow.as_ref()?.def;
    let mut v = serde_json::to_value(def).ok()?;
    if let Some(ts) = v.get_mut("transitions").and_then(Value::as_array_mut) {
        for (t, def) in ts.iter_mut().zip(&def.transitions) {
            t["may_take"] = json!(crate::ctx::transition_gate(ctx, ent, def).is_ok());
        }
    }
    Some(v)
}

/// An entity's tables: each one's entity, label and fields, and for a line that holds lines of its own (BD-06) its
/// `children` the same way.
fn children(ctx: &Ctx, ent: &EffEntity) -> serde_json::Map<String, Value> {
    ent.children
        .iter()
        .filter_map(|(field, info)| {
            let c = ctx.schema.entity(&info.entity)?;
            let mut v = json!({"entity": c.def.name, "label": c.def.label, "fields": c.def.fields.iter().map(|f| field_json(ctx, c, f)).collect::<Vec<_>>()});
            if !c.children.is_empty() {
                v["children"] = Value::Object(children(ctx, c));
            }
            Some((field.clone(), v))
        })
        .collect()
}

pub fn describe(ctx: &Ctx, ent: &EffEntity) -> Value {
    let children = children(ctx, ent);
    let mut v = json!({
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
        "actions": actions(ctx, ent),
        "targets": ent.def.targets,
        "workflow": workflow(ctx, ent),
        "fields": ent.def.fields.iter().map(|f| field_json(ctx, ent, f)).collect::<Vec<_>>(),
        "children": children,
        "permissions": permissions(ctx, &ent.def.name),
    });
    // What anyone may read of it without signing in: its administrators' to know.
    if ctx.is_admin {
        let lists: Vec<Value> = ctx
            .schema
            .published_lists
            .iter()
            .filter(|l| l.entity == ent.def.name)
            .map(|l| json!({"name": l.list.name, "label": l.list.label, "fields": l.list.fields, "csv": l.list.csv}))
            .collect();
        v["published_lists"] = json!(lists);
    }
    v
}

/// JSON Schema for writing a record of `ent` (child tables nest their row schema).
pub fn entity_schema(ctx: &Ctx, ent: &EffEntity, depth: usize) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for f in &ent.def.fields {
        if f.hidden && f.ftype != FieldType::Table {
            continue;
        }
        let mut schema = field_schema(ctx, f, depth);
        if let Some(t) = ent.target_keeping(&f.name) {
            schema["readOnly"] = json!(true);
            schema["description"] = json!(format!("{} (kept by the target {})", schema["description"].as_str().unwrap_or_default(), t.label));
        }
        props.insert(f.name.clone(), schema);
        if f.required && f.default.is_none() && f.default_expr.is_none() && f.computed.is_none() && !f.read_only {
            required.push(f.name.clone());
        }
    }
    json!({"type": "object", "title": ent.def.label, "properties": props, "required": required})
}

fn field_schema(ctx: &Ctx, f: &FieldDef, depth: usize) -> Value {
    let mut v = match f.ftype {
        FieldType::Int => json!({"type": "integer"}),
        FieldType::Decimal | FieldType::Currency | FieldType::Percent => {
            json!({"type": ["string", "number"], "description": "Exact decimal; send it as a string"})
        }
        FieldType::Check => json!({"type": "boolean"}),
        FieldType::Date => json!({"type": "string", "format": "date"}),
        FieldType::Datetime => json!({"type": "string", "format": "date-time"}),
        FieldType::Select => json!({"type": "string", "enum": f.options}),
        FieldType::Email => json!({"type": "string", "format": "email"}),
        FieldType::Url => json!({"type": "string", "format": "uri"}),
        FieldType::Json => json!({"type": ["object", "array"]}),
        FieldType::Secret => json!({"type": ["string", "boolean", "null"], "writeOnly": true,
            "description": "Write-only secret: send text to set it, null to remove it; reads show true when one is set"}),
        FieldType::Link => match f.to.as_deref() {
            // The kind it links to, for a checker as well as a reader (FLC-COMP-002).
            Some(to) => json!({"type": "string", "format": "uuid", "x-ferrum-link": to, "description": format!("Id of a {to} record")}),
            None => json!({"type": "string", "format": "uuid", "description": "Id of a linked record"}),
        },
        FieldType::Table => match f.of.as_deref().and_then(|c| ctx.schema.entity(c)) {
            Some(child) if depth < 2 => json!({"type": "array", "items": entity_schema(ctx, child, depth + 1)}),
            _ => json!({"type": "array", "items": {"type": "object"}}),
        },
        _ => match f.max_length {
            Some(n) => json!({"type": "string", "maxLength": n}),
            None => json!({"type": "string"}),
        },
    };
    let mut desc = f.label();
    if let Some(d) = &f.description {
        desc = format!("{desc}: {d}");
    }
    if let Some(existing) = v.get("description").and_then(Value::as_str) {
        desc = format!("{desc}. {existing}");
    }
    if f.read_only || f.computed.is_some() {
        v["readOnly"] = json!(true);
        desc.push_str(" (set by the system)");
    }
    if let Some(d) = &f.default {
        v["default"] = d.clone();
    }
    v["description"] = json!(desc);
    v
}
