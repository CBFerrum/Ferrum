//! OpenAPI 3.1 document generated at request time from the caller's effective schema, so every
//! customization and installed package appears in the API contract immediately.

use ferrum_kernel::{Access, Action, Ctx};
use ferrum_meta::{EffEntity, EntityKind, FieldDef, FieldType};
use serde_json::{Map, Value, json};

fn field_schema(f: &FieldDef) -> Value {
    let mut s = match f.ftype {
        FieldType::Int => json!({"type": "integer", "format": "int64"}),
        FieldType::Decimal | FieldType::Currency | FieldType::Percent => {
            json!({"type": "string", "format": "decimal", "pattern": "^-?[0-9]+(\\.[0-9]+)?$", "description": "Exact decimal as a string"})
        }
        FieldType::Date => json!({"type": "string", "format": "date"}),
        FieldType::Datetime => json!({"type": "string", "format": "date-time"}),
        FieldType::Check => json!({"type": "boolean"}),
        FieldType::Link => json!({"type": "string", "format": "uuid", "description": format!("Id of a {}", f.to.clone().unwrap_or_default())}),
        FieldType::Json => json!({}),
        FieldType::Secret => {
            json!({"type": ["string", "boolean", "null"], "writeOnly": true, "description": "Write-only secret: text sets it, null removes it; reads show true when set"})
        }
        FieldType::Select => json!({"type": "string", "enum": f.options}),
        FieldType::Email => json!({"type": "string", "format": "email"}),
        FieldType::Url => json!({"type": "string", "format": "uri"}),
        FieldType::Table => json!({"type": "array", "items": {"$ref": format!("#/components/schemas/{}", f.of.clone().unwrap_or_default())}}),
        _ => json!({"type": "string"}),
    };
    if let Value::Object(m) = &mut s {
        if let Some(d) = &f.description {
            m.insert("description".into(), json!(d));
        }
        if f.read_only || f.computed.is_some() {
            m.insert("readOnly".into(), json!(true));
        }
        if let Some(l) = f.max_length {
            m.insert("maxLength".into(), json!(l));
        }
    }
    s
}

fn entity_schema(e: &EffEntity) -> Value {
    let mut props = Map::new();
    if e.def.kind != EntityKind::Child {
        for (k, t) in [("id", "uuid"), ("created_at", "date-time"), ("updated_at", "date-time")] {
            props.insert(k.into(), json!({"type": "string", "format": t, "readOnly": true}));
        }
        props.insert("number".into(), json!({"type": ["string", "null"], "readOnly": true}));
        props.insert("version".into(), json!({"type": "integer", "readOnly": true}));
        props.insert("docstatus".into(), json!({"type": "integer", "enum": [0, 1, 2], "readOnly": true}));
    } else {
        props.insert("id".into(), json!({"type": "string", "format": "uuid"}));
    }
    let mut required = Vec::new();
    for f in &e.def.fields {
        props.insert(f.name.clone(), field_schema(f));
        if f.required && f.default.is_none() && f.default_expr.is_none() {
            required.push(f.name.clone());
        }
    }
    json!({"type": "object", "title": e.def.label, "properties": props, "required": required})
}

pub fn spec(ctx: &Ctx) -> Value {
    let mut paths = Map::new();
    let mut schemas = Map::new();
    let err = json!({"description": "Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}});
    for e in ctx.schema.entities.values() {
        schemas.insert(e.def.name.clone(), entity_schema(e));
        if e.def.kind == EntityKind::Child || ctx.perms.access(&e.def.name, Action::Read) == Access::None {
            continue;
        }
        let r = json!({"$ref": format!("#/components/schemas/{}", e.def.name)});
        let n = &e.def.name;
        paths.insert(format!("/api/v1/{n}"), json!({
            "get": {"tags": [n], "summary": format!("List {}", e.def.label), "parameters": [
                {"name": "filters", "in": "query", "schema": {"type": "string"}, "description": "JSON: [[field, op, value], ...]"},
                {"name": "fields", "in": "query", "schema": {"type": "string"}}, {"name": "sort", "in": "query", "schema": {"type": "string"}},
                {"name": "limit", "in": "query", "schema": {"type": "integer", "maximum": 500}}, {"name": "cursor", "in": "query", "schema": {"type": "string"}},
                {"name": "search", "in": "query", "schema": {"type": "string"}}, {"name": "count", "in": "query", "schema": {"type": "boolean"}}],
                "responses": {"200": {"description": "Page", "content": {"application/json": {"schema": {"type": "object", "properties": {
                    "rows": {"type": "array", "items": r}, "next_cursor": {"type": "string"}, "total": {"type": "integer"}}}}}}, "default": err}},
            "post": {"tags": [n], "summary": format!("Create {}", e.def.label),
                "parameters": [{"name": "Idempotency-Key", "in": "header", "schema": {"type": "string"}}],
                "requestBody": {"required": true, "content": {"application/json": {"schema": r}}},
                "responses": {"201": {"description": "Created", "content": {"application/json": {"schema": r}}}, "default": err}},
        }));
        paths.insert(
            format!("/api/v1/{n}/{{id}}"),
            json!({
                "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}],
                "get": {"tags": [n], "responses": {"200": {"description": "Record", "content": {"application/json": {"schema": r}}}, "default": err}},
                "patch": {"tags": [n], "parameters": [{"name": "If-Match", "in": "header", "schema": {"type": "string"}, "description": "\"v<version>\""}],
                    "requestBody": {"content": {"application/json": {"schema": r}}}, "responses": {"200": {"description": "Updated"}, "default": err}},
                "delete": {"tags": [n], "responses": {"204": {"description": "Deleted"}, "default": err}},
            }),
        );
        if e.def.submittable {
            for op in ["submit", "cancel", "amend"] {
                paths.insert(
                    format!("/api/v1/{n}/{{id}}/{op}"),
                    json!({"post": {"tags": [n], "parameters": [
                    {"name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}],
                    "responses": {"200": {"description": op}, "default": err}}}),
                );
            }
        }
        for a in &e.def.actions {
            paths.insert(
                format!("/api/v1/{n}/{{id}}/actions/{}", a.name),
                json!({"post": {"tags": [n], "summary": a.label, "parameters": [
                {"name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}],
                "responses": {"200": {"description": "Action result"}, "default": err}}}),
            );
        }
    }
    schemas.insert(
        "Error".into(),
        json!({"type": "object", "properties": {
        "error": {"type": "string"}, "message": {"type": "string"},
        "fields": {"type": "array", "items": {"type": "object", "properties": {"field": {"type": "string"}, "message": {"type": "string"}}}}}}),
    );
    json!({
        "openapi": "3.1.0",
        "info": {"title": "Ferrum API", "version": env!("CARGO_PKG_VERSION"), "description": "Generated from this tenant's effective metadata."},
        "security": [{"bearer": []}],
        "paths": paths,
        "components": {"schemas": schemas, "securitySchemes": {"bearer": {"type": "http", "scheme": "bearer", "description": "Session token or API key (fk_...)"}}},
    })
}
