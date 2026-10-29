//! Model Context Protocol server: AI agents discover and operate everything a user may use.
//!
//! [`handle`] takes one JSON-RPC message and returns its response (`None` for notifications); the
//! HTTP server exposes it as stateless Streamable HTTP at `POST /mcp`. Tools are generic and
//! metadata-driven, so every entity, action, workflow and report of every installed package —
//! tenant customizations and plugin entities included — is covered without per-entity code. Every
//! call runs through the same permission-checked kernel paths as the REST API, as the caller.

use ferrum_kernel::describe::{describe, permissions};
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::value::canonical_json;
use ferrum_kernel::{Access, Action, Ctx, Doc, Error, Kernel, ListQuery};
use ferrum_meta::{EffEntity, EntityKind, FieldDef, FieldType, OverlayOp};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

/// Newest first; the first one is offered when the client asks for something we do not know.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_LIMIT: u32 = 200;
const DEFAULT_LIMIT: u32 = 20;

const INSTRUCTIONS: &str = "Ferrum is a metadata-driven ERP. Start with `ferrum_overview` to see the modules, entities, \
reports and your permissions, then `describe_entity` before creating or changing records of an entity: it lists fields \
(with JSON Schema), child tables, actions and workflow. Money and quantities are exact decimals: send them as strings \
(\"1250.50\"). Links are record ids (UUIDs): find them with `search_records`. Documents have a lifecycle: docstatus 0 = \
draft (editable), 1 = submitted (posted to ledgers, immutable), 2 = cancelled; use `submit_record`, `cancel_record` and \
`amend_record` to move between them. Use `preview_record` to see computed totals and taxes before saving, and pass an \
`idempotency_key` to `create_record` when retrying. Follow-up documents (delivery note from an order, payment from an \
invoice...) come from `run_action`. Every change is permission-checked and recorded in the audit trail under your user.";

#[derive(Debug)]
enum Fail {
    /// JSON-RPC protocol error (unknown method or tool, malformed params).
    Rpc(i64, String),
    /// A tool ran and failed: reported to the model as a tool result with `isError`.
    Tool(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Tool(e)
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Handles one JSON-RPC message. Returns `None` for notifications and client responses.
pub async fn handle(k: &Kernel, ctx: &Ctx, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned().filter(|v| v.is_string() || v.is_number());
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return id.map(|id| rpc_error(id, -32600, "invalid request: missing method"));
    };
    let id = id?;
    if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(rpc_error(id, -32600, "invalid request: jsonrpc must be \"2.0\""));
    }
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let outcome = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" | "logging/setLevel" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools(ctx)})),
        "tools/call" => call_tool(k, ctx, &params).await,
        "resources/list" => Ok(resources(ctx)),
        "resources/templates/list" => Ok(json!({"resourceTemplates": [
            {"uriTemplate": "ferrum://entity/{name}", "name": "Entity schema", "mimeType": "application/json",
             "description": "Fields, JSON Schema, child tables, actions and workflow of an entity"},
            {"uriTemplate": "ferrum://record/{entity}/{id}", "name": "Record", "mimeType": "application/json",
             "description": "One record with link titles"},
        ]})),
        "resources/read" => read_resource(k, ctx, &params).await,
        "prompts/list" => Ok(prompts()),
        "prompts/get" => get_prompt(&params),
        "completion/complete" => Ok(json!({"completion": {"values": [], "hasMore": false}})),
        other => Err(Fail::Rpc(-32601, format!("method not found: {other}"))),
    };
    Some(match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(Fail::Rpc(code, message)) => rpc_error(id, code, &message),
        Err(Fail::Tool(e)) => json!({"jsonrpc": "2.0", "id": id, "result": tool_result(&error_json(&e), true)}),
    })
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
    let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": {"listChanged": false},
            "resources": {"listChanged": false, "subscribe": false},
            "prompts": {"listChanged": false},
            "logging": {},
        },
        "serverInfo": {"name": "ferrum", "title": "Ferrum ERP", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}

// ───────────────────────────── tools ─────────────────────────────

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

fn tool(name: &str, title: &str, description: &str, properties: Value, required: &[&str], read_only: bool, destructive: bool, idempotent: bool) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required, "additionalProperties": false},
        "annotations": {"title": title, "readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": idempotent, "openWorldHint": false},
    })
}

fn tools(ctx: &Ctx) -> Vec<Value> {
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
            "Administrators: change an entity without code. op is one of {\"op\":\"add_field\",\"field\":{name (x_ prefix), type, label, options...}}, {\"op\":\"set_property\",\"field\",\"property\",\"value\"}, {\"op\":\"add_rule\",\"rule\":{name, check (CEL), message}}, {\"op\":\"add_options\",\"field\",\"options\":[...]}, {\"op\":\"set_workflow\",\"workflow\":{...}}, {\"op\":\"set_print_template\",\"template\":\"<html jinja>\"}.",
            json!({"entity": entity_prop(), "op": {"type": "object"}, "note": s("Why (kept with the change)")}),
            &["entity", "op"],
            false,
            true,
            false,
        ));
    }
    out
}

fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    let structured = if value.is_object() { value.clone() } else { json!({"result": value}) };
    json!({"content": [{"type": "text", "text": text}], "structuredContent": structured, "isError": is_error})
}

fn error_json(e: &Error) -> Value {
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

fn arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, Fail> {
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

fn entity_of(ctx: &Ctx, name: &str) -> Result<std::sync::Arc<EffEntity>, Fail> {
    let ent = ctx.schema.entity(name).cloned().ok_or_else(|| Fail::Tool(Error::NotFound(format!("entity {name} (see ferrum_overview)"))))?;
    if ent.def.kind != EntityKind::Child && ctx.perms.access(name, Action::Read) == Access::None {
        return Err(Fail::Tool(Error::Forbidden(format!("not permitted to read {name}"))));
    }
    Ok(ent)
}

async fn call_tool(k: &Kernel, ctx: &Ctx, params: &Value) -> Result<Value, Fail> {
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| Fail::Rpc(-32602, "tool name is required".into()))?;
    let empty = json!({});
    let args = params.get("arguments").filter(|a| a.is_object()).unwrap_or(&empty);
    let value = run_tool(k, ctx, name, args).await?;
    Ok(tool_result(&value, false))
}

async fn run_tool(k: &Kernel, ctx: &Ctx, name: &str, args: &Value) -> Result<Value, Fail> {
    tracing::info!(tenant = %ctx.tenant, user = %ctx.user, tool = name, "mcp tool call");
    let record = |d: Doc| Value::Object(d);
    match name {
        "ferrum_overview" => Ok(overview(ctx)),
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

fn overview(ctx: &Ctx) -> Value {
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
    let modules: Vec<Value> = ctx.schema.modules.iter().map(|m| json!({"name": m.name, "label": m.label, "items": m.items})).collect();
    json!({
        "user": {"id": ctx.user, "email": ctx.email, "full_name": ctx.full_name, "roles": ctx.roles, "is_admin": ctx.is_admin,
                 "default_company": ctx.default_company},
        "packages": ctx.schema.packages,
        "modules": modules,
        "entities": entities,
        "reports": ferrum_domain::reports::catalog_for(ctx),
    })
}

/// JSON Schema for writing a record of `ent` (child tables nest their row schema).
pub fn entity_schema(ctx: &Ctx, ent: &EffEntity, depth: usize) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for f in &ent.def.fields {
        if f.hidden && f.ftype != FieldType::Table {
            continue;
        }
        props.insert(f.name.clone(), field_schema(ctx, f, depth));
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
        FieldType::Link => {
            json!({"type": "string", "format": "uuid", "description": format!("Id of a {} record", f.to.as_deref().unwrap_or("linked"))})
        }
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

// ───────────────────────────── resources ─────────────────────────────

fn resources(ctx: &Ctx) -> Value {
    let mut list = vec![json!({"uri": "ferrum://overview", "name": "Overview", "mimeType": "application/json",
        "description": "Your user, packages, modules, entities and reports"})];
    for e in ctx.schema.entities.values() {
        if e.def.kind != EntityKind::Child && ctx.perms.access(&e.def.name, Action::Read) != Access::None {
            list.push(json!({"uri": format!("ferrum://entity/{}", e.def.name), "name": e.def.label, "mimeType": "application/json",
                "description": format!("Schema of {}", e.def.label)}));
        }
    }
    json!({"resources": list})
}

async fn read_resource(k: &Kernel, ctx: &Ctx, params: &Value) -> Result<Value, Fail> {
    let uri = arg(params, "uri")?;
    let rest = uri.strip_prefix("ferrum://").ok_or_else(|| Fail::Rpc(-32602, format!("unknown resource {uri}")))?;
    let parts: Vec<&str> = rest.split('/').collect();
    let value = match parts.as_slice() {
        ["overview"] => overview(ctx),
        ["entity", name] => {
            let ent = entity_of(ctx, name)?;
            json!({"entity": describe(ctx, &ent), "json_schema": entity_schema(ctx, &ent, 0)})
        }
        ["record", entity, id] => {
            let id: Uuid = id.parse().map_err(|_| Fail::Rpc(-32602, "record id must be a UUID".into()))?;
            Value::Object(k.get(ctx, entity, id, true).await?)
        }
        _ => return Err(Fail::Rpc(-32602, format!("unknown resource {uri}"))),
    };
    Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}]}))
}

// ───────────────────────────── prompts ─────────────────────────────

const PROMPTS: &[(&str, &str, &[(&str, &str, bool)], &str)] = &[
    (
        "record-sale",
        "Record a sale: order or invoice, with delivery and payment as needed",
        &[("customer", "Customer name", true), ("items", "What was sold, with quantities", true)],
        "Record this sale in Ferrum for customer {customer}: {items}. Find the customer and items with search_records \
         (ask before creating new masters). Create a SalesInvoice draft with create_record, check prices and taxes with \
         preview_record, show me the totals, and submit only after I confirm.",
    ),
    (
        "month-end-review",
        "Review a month's books: trial balance, receivables, payables, stock",
        &[("company", "Company name (optional)", false), ("month", "Month, e.g. 2026-10", true)],
        "Review the books of {company} for {month}: run trial_balance, profit_and_loss, receivables and payables for that \
         period with run_report, flag anything unusual (overdue items, negative balances, large movements) and suggest the \
         adjusting entries I should consider. Do not post anything.",
    ),
    (
        "explain-record",
        "Explain a record: what it is, its lifecycle and its history",
        &[("entity", "Entity name", true), ("id", "Record id", true)],
        "Explain {entity} {id}: read it with get_record, describe its status and what it posted or affected, and summarise \
         its history from get_audit_trail.",
    ),
];

fn prompts() -> Value {
    json!({"prompts": PROMPTS.iter().map(|(name, description, args, _)| json!({
        "name": name,
        "description": description,
        "arguments": args.iter().map(|(n, d, r)| json!({"name": n, "description": d, "required": r})).collect::<Vec<_>>(),
    })).collect::<Vec<_>>()})
}

fn get_prompt(params: &Value) -> Result<Value, Fail> {
    let name = arg(params, "name")?;
    let (_, description, args, template) = PROMPTS.iter().find(|p| p.0 == name).ok_or_else(|| Fail::Rpc(-32602, format!("unknown prompt {name}")))?;
    let given = params.get("arguments").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut text = template.to_string();
    for (arg_name, _, required) in args.iter() {
        let value = given.get(*arg_name).and_then(Value::as_str).unwrap_or_default();
        if *required && value.is_empty() {
            return Err(Fail::Rpc(-32602, format!("prompt argument '{arg_name}' is required")));
        }
        text = text.replace(&format!("{{{arg_name}}}"), if value.is_empty() { "the default company" } else { value });
    }
    Ok(json!({"description": description, "messages": [{"role": "user", "content": {"type": "text", "text": text}}]}))
}
