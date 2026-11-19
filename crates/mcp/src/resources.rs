//! Resources and prompts.

use crate::Fail;
use crate::tools::{entity_of, overview};
use ferrum_kernel::describe::{describe, entity_schema};
use ferrum_kernel::{Access, Action, Ctx, Kernel};
use ferrum_meta::EntityKind;
use serde_json::{Value, json};
use uuid::Uuid;

/// A parameter a resource or prompt request needs (a protocol error when it is not there).
fn param<'a>(params: &'a Value, name: &str) -> Result<&'a str, Fail> {
    params.get(name).and_then(Value::as_str).filter(|v| !v.trim().is_empty()).ok_or_else(|| Fail::Rpc(-32602, format!("argument '{name}' is required")))
}

pub(crate) fn resources(ctx: &Ctx) -> Value {
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

pub(crate) async fn read_resource(k: &Kernel, ctx: &Ctx, params: &Value) -> Result<Value, Fail> {
    let uri = param(params, "uri")?;
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

pub(crate) fn prompts() -> Value {
    json!({"prompts": PROMPTS.iter().map(|(name, description, args, _)| json!({
        "name": name,
        "description": description,
        "arguments": args.iter().map(|(n, d, r)| json!({"name": n, "description": d, "required": r})).collect::<Vec<_>>(),
    })).collect::<Vec<_>>()})
}

pub(crate) fn get_prompt(params: &Value) -> Result<Value, Fail> {
    let name = param(params, "name")?;
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
