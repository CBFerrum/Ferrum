//! The built-in tools: generic and metadata-driven, so every entity, action, workflow and report of every
//! installed package is covered without per-entity code.

use crate::{Fail, ToolProvider};
use ferrum_kernel::describe::{describe, entity_schema, permissions};
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::value::canonical_json;
use ferrum_kernel::{Access, Action, Ctx, Doc, Error, Kernel, ListQuery};
use ferrum_meta::{EffEntity, EntityKind, OverlayOp};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

const MAX_LIMIT: u32 = 200;
const DEFAULT_LIMIT: u32 = 20;

fn s(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn entity_prop() -> Value {
    s("Entity name, e.g. SalesInvoice (see ferrum_overview)")
}

fn id_prop() -> Value {
    json!({"type": "string", "format": "uuid", "description": "Record id"})
}

fn version_prop() -> Value {
    json!({"type": "integer", "description": "Optional: the version you read; the change is refused if the record changed since"})
}

pub(crate) fn tool(
    name: &str,
    title: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
    destructive: bool,
    idempotent: bool,
) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required, "additionalProperties": false},
        "annotations": {"title": title, "readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": idempotent, "openWorldHint": false},
    })
}

pub(crate) fn tools(ctx: &Ctx) -> Vec<Value> {
    let mut out = vec![
        tool(
            "ferrum_overview",
            "Overview",
            "Who you are (roles, default company), installed packages, modules, every entity you can see with your permissions, and the report catalog. Call this first.",
            json!({}),
            &[],
            true,
            false,
            true,
        ),
        tool(
            "describe_entity",
            "Describe entity",
            "Fields (types, options, links, required), a JSON Schema for creating records, child tables, actions, workflow, numbering and validation rules of one entity.",
            json!({"entity": entity_prop()}),
            &["entity"],
            true,
            false,
            true,
        ),
        tool(
            "search_records",
            "Search records",
            "List records of an entity with filters, full-text search, sorting and cursor pagination. Filters: {\"field\": value} or [[\"field\", \"op\", value]] with ops =, !=, >, >=, <, <=, in, not in, like, not like, between, is.",
            json!({
                "entity": entity_prop(),
                "filters": {"description": "Object {field: value} or list of [field, op, value]"},
                "search": s("Free-text search over the entity's title and search fields"),
                "fields": {"type": "array", "items": {"type": "string"}, "description": "Only these fields (default: list columns)"},
                "sort": s("e.g. \"posting_date desc\""),
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Default 20"},
                "cursor": s("next_cursor from the previous page"),
                "with_titles": {"type": "boolean", "description": "Add display titles of linked records"},
            }),
            &["entity"],
            true,
            false,
            true,
        ),
        tool(
            "get_record",
            "Get record",
            "One record with its child tables (and link titles).",
            json!({"entity": entity_prop(), "id": id_prop()}),
            &["entity", "id"],
            true,
            false,
            true,
        ),
        tool(
            "preview_record",
            "Preview record",
            "Compute what a record would look like (prices, taxes, totals, defaults) without saving it.",
            json!({"entity": entity_prop(), "data": {"type": "object", "description": "Field values (see describe_entity)"}}),
            &["entity", "data"],
            true,
            false,
            true,
        ),
        tool(
            "create_record",
            "Create record",
            "Create a record (documents start as drafts unless submit is true). Child tables are arrays of row objects. Pass idempotency_key to make retries safe.",
            json!({
                "entity": entity_prop(),
                "data": {"type": "object", "description": "Field values (see describe_entity)"},
                "idempotency_key": s("Optional unique key (8-200 chars); repeating it returns the first result"),
                "submit": {"type": "boolean", "description": "Also submit the document in the same transaction (never left as a draft)"},
            }),
            &["entity", "data"],
            false,
            false,
            false,
        ),
        tool(
            "update_record",
            "Update record",
            "Change fields of a draft document or a master record. Child tables are replaced by the given rows (send row ids to keep rows).",
            json!({"entity": entity_prop(), "id": id_prop(), "data": {"type": "object"}, "expected_version": version_prop()}),
            &["entity", "id", "data"],
            false,
            true,
            false,
        ),
        tool(
            "delete_record",
            "Delete record",
            "Delete a draft document or an unused master record.",
            json!({"entity": entity_prop(), "id": id_prop(), "expected_version": version_prop()}),
            &["entity", "id"],
            false,
            true,
            false,
        ),
        tool(
            "submit_record",
            "Submit document",
            "Submit a draft document: it gets its number, posts to the ledgers and becomes immutable.",
            json!({"entity": entity_prop(), "id": id_prop(), "expected_version": version_prop()}),
            &["entity", "id"],
            false,
            true,
            false,
        ),
        tool(
            "cancel_record",
            "Cancel document",
            "Cancel a submitted document: its postings are reversed.",
            json!({"entity": entity_prop(), "id": id_prop(), "expected_version": version_prop()}),
            &["entity", "id"],
            false,
            true,
            false,
        ),
        tool(
            "amend_record",
            "Amend document",
            "Create an editable draft copy of a cancelled document.",
            json!({"entity": entity_prop(), "id": id_prop()}),
            &["entity", "id"],
            false,
            false,
            false,
        ),
        tool(
            "run_action",
            "Run action",
            "Run a document action listed by describe_entity (e.g. make_delivery_note, make_payment, close). Actions that create documents return {entity, id, number}.",
            json!({"entity": entity_prop(), "id": id_prop(), "action": s("Action name"), "params": {"type": "object"}}),
            &["entity", "id", "action"],
            false,
            true,
            false,
        ),
        tool(
            "list_capabilities",
            "List business capabilities",
            "The business capabilities you may use, as processes and the builder see them: each with its version, effect (query, transactional_action, external_effect...), rights, input and output schemas and what done means. `q` narrows by words in the id or label; `entity` to one entity.",
            json!({"q": s("Words to look for"), "entity": s("Entity name"), "limit": {"type": "integer", "minimum": 1, "maximum": 200}}),
            &[],
            true,
            false,
            true,
        ),
        tool(
            "invoke_capability",
            "Use a business capability",
            "Run a capability from list_capabilities through the same gate processes use. One that changes something needs `effect_key`: the same key with the same input returns what was done, never does it twice; the same key with other input is refused. `version` refuses to run if the installed version differs.",
            json!({"capability": s("Capability id"), "input": {"type": "object"}, "effect_key": s("Logical effect key"), "version": s("Expected version")}),
            &["capability", "input"],
            false,
            true,
            true,
        ),
        tool(
            "workflow_transition",
            "Workflow transition",
            "Apply an approval workflow transition (e.g. approve, reject) to a document.",
            json!({"entity": entity_prop(), "id": id_prop(), "action": s("Transition action"), "expected_version": version_prop()}),
            &["entity", "id", "action"],
            false,
            true,
            false,
        ),
        tool(
            "run_report",
            "Run report",
            "Run a report from the catalog in ferrum_overview (trial_balance, profit_and_loss, receivables, stock_balance...). Dates are YYYY-MM-DD.",
            json!({"name": s("Report name"), "params": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Report parameters"}}),
            &["name"],
            true,
            false,
            true,
        ),
        tool(
            "get_audit_trail",
            "Audit trail",
            "Who changed a record, when and how, and whether its tamper-evident hash chain is intact.",
            json!({"entity": entity_prop(), "id": id_prop()}),
            &["entity", "id"],
            true,
            false,
            true,
        ),
        tool(
            "get_collaboration",
            "Comments, assignments and tags",
            "The discussion on a record: comments (oldest first), who it is assigned to and its tags.",
            json!({"entity": entity_prop(), "id": id_prop()}),
            &["entity", "id"],
            true,
            false,
            true,
        ),
        tool(
            "add_comment",
            "Comment on a record",
            "Adds a comment (markdown) to any record you can read. Mention people by user id in `mentions` to notify them; followers are notified too.",
            json!({"entity": entity_prop(), "id": id_prop(), "body": {"type": "string", "maxLength": 20000},
                   "mentions": {"type": "array", "items": {"type": "string", "format": "uuid"}, "maxItems": 20}}),
            &["entity", "id", "body"],
            false,
            false,
            false,
        ),
        tool(
            "assign_record",
            "Assign a record",
            "Assigns a record to a user as a to-do (they must be able to open it); they are notified.",
            json!({"entity": entity_prop(), "id": id_prop(), "user": {"type": "string", "format": "uuid"}, "note": {"type": "string", "maxLength": 2000},
                   "due_date": {"type": "string", "format": "date"}, "priority": {"type": "string", "enum": ["Low", "Medium", "High"]}}),
            &["entity", "id", "user"],
            false,
            false,
            false,
        ),
        tool(
            "my_assignments",
            "My to-do list",
            "Records assigned to the signed-in user (open ones by default), soonest due first.",
            json!({"status": {"type": "string", "enum": ["Open", "Done", "Cancelled"]}}),
            &[],
            true,
            false,
            true,
        ),
        tool(
            "set_tags",
            "Tag a record",
            "Replaces a record's tags (needs write access). List records by tag with the filter [\"_tag\", \"=\", \"name\"].",
            json!({"entity": entity_prop(), "id": id_prop(), "tags": {"type": "array", "items": {"type": "string", "maxLength": 60}, "maxItems": 20}}),
            &["entity", "id", "tags"],
            false,
            false,
            true,
        ),
    ];
    if ctx.is_admin {
        out.push(tool(
            "customize_entity",
            "Customize entity",
            "Administrators: change an entity without code. op is one of {\"op\":\"add_field\",\"field\":{name (x_ prefix), type, label, options...}}, {\"op\":\"set_property\",\"field\",\"property\",\"value\"}, {\"op\":\"add_rule\",\"rule\":{name, check (CEL), message}}, {\"op\":\"add_action\",\"action\":{name (x_ prefix on a packaged entity), label, condition?, roles?, params?, refuse_if?: [{when, message}], effects: [automation actions]}} (the name of an action a customization added replaces it), {\"op\":\"remove_action\",\"name\"} (an action a customization added), {\"op\":\"add_options\",\"field\",\"options\":[...]}, {\"op\":\"set_workflow\",\"workflow\":{...}}, {\"op\":\"set_print_template\",\"template\":\"<html jinja>\"}.",
            json!({"entity": entity_prop(), "op": {"type": "object"}, "note": s("Why (kept with the change)")}),
            &["entity", "op"],
            false,
            true,
            false,
        ));
    }
    out
}

pub(crate) fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    let structured = if value.is_object() { value.clone() } else { json!({"result": value}) };
    json!({"content": [{"type": "text", "text": text}], "structuredContent": structured, "isError": is_error})
}

pub(crate) fn error_json(e: &Error) -> Value {
    let mut v = json!({"error": e.code(), "message": e.to_string()});
    if let Error::Validation(fields) = e {
        v["message"] = json!("Please correct the highlighted fields");
        v["fields"] = json!(fields);
    }
    if matches!(e, Error::Db(_) | Error::Internal(_)) {
        let id = Uuid::now_v7();
        tracing::error!(error_id = %id, "mcp tool failed: {e}");
        v = json!({"error": e.code(), "message": "An internal error occurred", "error_id": id});
    }
    v
}

pub(crate) fn arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, Fail> {
    args.get(name).and_then(Value::as_str).filter(|v| !v.trim().is_empty()).ok_or_else(|| Fail::Rpc(-32602, format!("argument '{name}' is required")))
}

fn uuid_arg(args: &Value, name: &str) -> Result<Uuid, Fail> {
    arg(args, name)?.trim().parse().map_err(|_| Fail::Rpc(-32602, format!("argument '{name}' must be a record id (UUID)")))
}

fn object_arg(args: &Value, name: &str) -> Result<Doc, Fail> {
    match args.get(name) {
        Some(Value::Object(m)) => Ok(m.clone()),
        _ => Err(Fail::Rpc(-32602, format!("argument '{name}' must be an object"))),
    }
}

fn version_arg(args: &Value) -> Option<i64> {
    args.get("expected_version").and_then(Value::as_i64)
}

pub(crate) fn entity_of(ctx: &Ctx, name: &str) -> Result<std::sync::Arc<EffEntity>, Fail> {
    let ent = ctx.schema.entity(name).cloned().ok_or_else(|| Fail::Tool(Error::NotFound(format!("entity {name} (see ferrum_overview)"))))?;
    if ent.def.kind != EntityKind::Child && ctx.perms.access(name, Action::Read) == Access::None {
        return Err(Fail::Tool(Error::Forbidden(format!("not permitted to read {name}"))));
    }
    Ok(ent)
}

pub(crate) async fn call_tool(k: &Kernel, ctx: &Ctx, params: &Value, providers: &[&dyn ToolProvider]) -> Result<Value, Fail> {
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| Fail::Rpc(-32602, "tool name is required".into()))?;
    let empty = json!({});
    let args = params.get("arguments").filter(|a| a.is_object()).unwrap_or(&empty);
    let value = match run_tool(k, ctx, name, args).await {
        Err(Fail::Rpc(code, m)) if m.starts_with("unknown tool") => {
            let mut found = None;
            for p in providers {
                if let Some(out) = p.call(k, ctx, name, args).await {
                    tracing::info!(tenant = %ctx.tenant, user = %ctx.user, tool = name, "mcp tool call");
                    found = Some(out?);
                    break;
                }
            }
            found.ok_or(Fail::Rpc(code, m))?
        }
        other => other?,
    };
    Ok(tool_result(&value, false))
}

async fn run_tool(k: &Kernel, ctx: &Ctx, name: &str, args: &Value) -> Result<Value, Fail> {
    tracing::info!(tenant = %ctx.tenant, user = %ctx.user, tool = name, "mcp tool call");
    let record = |d: Doc| Value::Object(d);
    match name {
        "ferrum_overview" => Ok(overview(ctx)),
        "list_capabilities" => {
            let q = args.get("q").and_then(Value::as_str).map(str::to_lowercase);
            let entity = args.get("entity").and_then(Value::as_str);
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50).clamp(1, 200) as usize;
            let found: Vec<_> = k
                .catalog(ctx)
                .into_iter()
                .filter(|c| entity.is_none_or(|e| c.entity.as_deref() == Some(e)))
                .filter(|c| q.as_deref().is_none_or(|q| c.id.contains(q) || c.label.to_lowercase().contains(q)))
                .take(limit)
                .collect();
            Ok(json!({"capabilities": found}))
        }
        "invoke_capability" => {
            let mut inv = ferrum_kernel::capability::Invocation::new(arg(args, "capability")?, args.get("input").cloned().unwrap_or(json!({})));
            inv.effect_key = args.get("effect_key").and_then(Value::as_str).map(str::to_string);
            inv.version = args.get("version").and_then(Value::as_str).map(str::to_string);
            Ok(serde_json::to_value(k.invoke(ctx, inv).await?).unwrap_or(Value::Null))
        }
        "describe_entity" => {
            let ent = entity_of(ctx, arg(args, "entity")?)?;
            Ok(json!({"entity": describe(ctx, &ent), "json_schema": entity_schema(ctx, &ent, 0)}))
        }
        "search_records" => {
            let entity = arg(args, "entity")?;
            entity_of(ctx, entity)?;
            let filters = match args.get("filters") {
                None | Some(Value::Null) => vec![],
                Some(v) => parse_filters(v)?,
            };
            let fields = args.get("fields").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
            let q = ListQuery {
                filters,
                fields,
                sort: args.get("sort").and_then(Value::as_str).map(str::to_string),
                limit: Some(args.get("limit").and_then(Value::as_u64).map(|l| l.clamp(1, MAX_LIMIT as u64) as u32).unwrap_or(DEFAULT_LIMIT)),
                offset: None,
                cursor: args.get("cursor").and_then(Value::as_str).map(str::to_string),
                search: args.get("search").and_then(Value::as_str).map(str::to_string),
                with_titles: args.get("with_titles").and_then(Value::as_bool).unwrap_or(false),
                with_count: false,
            };
            let page = k.list(ctx, entity, &q).await?;
            Ok(serde_json::to_value(page).map_err(Error::internal)?)
        }
        "get_record" => Ok(record(k.get(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, true).await?)),
        "preview_record" => Ok(record(k.preview(ctx, arg(args, "entity")?, object_arg(args, "data")?).await?)),
        "create_record" => {
            let entity = arg(args, "entity")?;
            let data = object_arg(args, "data")?;
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
            match args.get("idempotency_key").and_then(Value::as_str).filter(|k| !k.is_empty()) {
                Some(key) => {
                    let mut h = Sha256::new();
                    h.update(entity.as_bytes());
                    h.update(if submit { b"+submit".as_slice() } else { b"".as_slice() });
                    h.update(canonical_json(&Value::Object(data.clone())));
                    let (doc, replayed) = k.insert_idempotent_with(ctx, entity, data, key, &hex::encode(h.finalize()), submit).await?;
                    Ok(json!({"record": doc, "replayed": replayed}))
                }
                None if submit => Ok(json!({"record": k.insert_submit(ctx, entity, data).await?, "replayed": false})),
                None => Ok(json!({"record": k.insert(ctx, entity, data).await?, "replayed": false})),
            }
        }
        "update_record" => Ok(record(k.update(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, object_arg(args, "data")?, version_arg(args)).await?)),
        "delete_record" => {
            k.delete(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, version_arg(args)).await?;
            Ok(json!({"deleted": true}))
        }
        "submit_record" => Ok(record(k.submit(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, version_arg(args)).await?)),
        "cancel_record" => Ok(record(k.cancel(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, version_arg(args)).await?)),
        "amend_record" => Ok(record(k.amend(ctx, arg(args, "entity")?, uuid_arg(args, "id")?).await?)),
        "run_action" => {
            let params = args.get("params").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
            let out = k.run_action(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, arg(args, "action")?, params).await?;
            Ok(if out.is_object() { out } else { json!({"result": out}) })
        }
        "workflow_transition" => Ok(record(k.transition(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, arg(args, "action")?, version_arg(args)).await?)),
        "run_report" => {
            let params: HashMap<String, String> = args
                .get("params")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| match v {
                            Value::String(s) => Some((k.clone(), s.clone())),
                            Value::Null => None,
                            other => Some((k.clone(), other.to_string())),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(ferrum_domain::reports::run(k, ctx, arg(args, "name")?, &params).await?)
        }
        "get_audit_trail" => Ok(k.audit_trail(ctx, arg(args, "entity")?, uuid_arg(args, "id")?).await?),
        "get_collaboration" => {
            let (entity, id) = (arg(args, "entity")?, uuid_arg(args, "id")?);
            Ok(
                json!({"comments": k.comments(ctx, entity, id).await?, "assignments": k.assignments(ctx, entity, id).await?, "tags": k.tags(ctx, entity, id).await?}),
            )
        }
        "add_comment" => {
            let mentions: Vec<Uuid> =
                args.get("mentions").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str()?.parse().ok()).collect()).unwrap_or_default();
            let id = k.add_comment(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, arg(args, "body")?, &mentions).await?;
            Ok(json!({"comment_id": id}))
        }
        "assign_record" => {
            let due = match args.get("due_date").and_then(Value::as_str) {
                Some(d) => Some(d.parse::<chrono::NaiveDate>().map_err(|_| Fail::Rpc(-32602, "due_date must be YYYY-MM-DD".into()))?),
                None => None,
            };
            let note = args.get("note").and_then(Value::as_str);
            let priority = args.get("priority").and_then(Value::as_str);
            let id = k.assign(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, uuid_arg(args, "user")?, note, due, priority).await?;
            Ok(json!({"assignment_id": id}))
        }
        "my_assignments" => Ok(json!({"assignments": k.my_assignments(ctx, args.get("status").and_then(Value::as_str)).await?})),
        "set_tags" => {
            let tags: Vec<String> =
                args.get("tags").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            Ok(json!({"tags": k.set_tags(ctx, arg(args, "entity")?, uuid_arg(args, "id")?, &tags).await?}))
        }
        "customize_entity" if ctx.is_admin => {
            let op: OverlayOp = serde_json::from_value(args.get("op").cloned().unwrap_or(Value::Null))
                .map_err(|e| Fail::Rpc(-32602, format!("invalid customization op: {e}")))?;
            let note = args.get("note").and_then(Value::as_str).map(str::to_string);
            let id = k.add_customization(ctx, arg(args, "entity")?, op, note).await?;
            Ok(json!({"customization_id": id, "message": "applied; describe_entity shows the new shape"}))
        }
        other => Err(Fail::Rpc(-32602, format!("unknown tool: {other}"))),
    }
}

pub(crate) fn overview(ctx: &Ctx) -> Value {
    let entities: Vec<Value> = ctx
        .schema
        .entities
        .values()
        .filter(|e| e.def.kind != EntityKind::Child && ctx.perms.access(&e.def.name, Action::Read) != Access::None)
        .map(|e| {
            json!({"name": e.def.name, "label": e.def.label, "kind": e.def.kind, "module": e.def.module, "submittable": e.def.submittable,
                   "description": e.def.description, "permissions": permissions(ctx, &e.def.name)})
        })
        .collect();
    // A module of a package the caller's zone leaves out (zones, level 2) is not named.
    let modules: Vec<Value> =
        ctx.schema.modules.iter().filter(|m| ctx.sees_package(&m.package)).map(|m| json!({"name": m.name, "label": m.label, "items": m.items})).collect();
    json!({
        "user": {"id": ctx.user, "email": ctx.email, "full_name": ctx.full_name, "roles": ctx.roles, "is_admin": ctx.is_admin,
                 "default_company": ctx.default_company},
        "packages": ctx.schema.packages,
        "modules": modules,
        "entities": entities,
        "reports": ferrum_domain::reports::catalog_for(ctx),
    })
}
