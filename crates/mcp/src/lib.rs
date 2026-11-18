//! Model Context Protocol server: AI agents discover and operate everything a user may use.
//!
//! [`handle`] takes one JSON-RPC message and returns its response (`None` for notifications); the
//! HTTP server exposes it as stateless Streamable HTTP at `POST /mcp`. Tools are generic and
//! metadata-driven, so every entity, action, workflow and report of every installed package —
//! tenant customizations and plugin entities included — is covered without per-entity code. Every
//! call runs through the same permission-checked kernel paths as the REST API, as the caller.

mod provider;
mod resources;
mod tools;

pub use provider::{ToolProvider, tool_definition};

use ferrum_kernel::{Ctx, Error, Kernel};
use serde_json::{Value, json};

/// Newest first; the first one is offered when the client asks for something we do not know.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

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
    handle_with(k, ctx, msg, &[]).await
}

/// [`handle`] with tools from the host's `providers` too.
pub async fn handle_with(k: &Kernel, ctx: &Ctx, msg: Value, providers: &[&dyn ToolProvider]) -> Option<Value> {
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
        "tools/list" => Ok(json!({"tools": tools::tools(ctx).into_iter().chain(providers.iter().flat_map(|p| p.tools(ctx))).collect::<Vec<_>>()})),
        "tools/call" => tools::call_tool(k, ctx, &params, providers).await,
        "resources/list" => Ok(resources::resources(ctx)),
        "resources/templates/list" => Ok(json!({"resourceTemplates": [
            {"uriTemplate": "ferrum://entity/{name}", "name": "Entity schema", "mimeType": "application/json",
             "description": "Fields, JSON Schema, child tables, actions and workflow of an entity"},
            {"uriTemplate": "ferrum://record/{entity}/{id}", "name": "Record", "mimeType": "application/json",
             "description": "One record with link titles"},
        ]})),
        "resources/read" => resources::read_resource(k, ctx, &params).await,
        "prompts/list" => Ok(resources::prompts()),
        "prompts/get" => resources::get_prompt(&params),
        "completion/complete" => Ok(json!({"completion": {"values": [], "hasMore": false}})),
        other => Err(Fail::Rpc(-32601, format!("method not found: {other}"))),
    };
    Some(match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(Fail::Rpc(code, message)) => rpc_error(id, code, &message),
        Err(Fail::Tool(e)) => json!({"jsonrpc": "2.0", "id": id, "result": tools::tool_result(&tools::error_json(&e), true)}),
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
