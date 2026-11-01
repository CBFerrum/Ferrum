//! What an AI agent may do with physical work: look, explain, and propose. There is no tool for
//! anything else. A proposal is a command that waits for a person, unless an envelope a person
//! approved covers it, and an agent cannot say otherwise, because this is the only way it gets in.
//!
//! Instructions in manuals, logs or device strings are data. No text an agent reads can change what
//! these tools do, because what they do does not depend on the model.

use crate::commands::{self, Issuer, NewCommand};
use crate::site;
use ferrum_kernel::{Ctx, Error, Kernel, Result};
use ferrum_mcp::{ToolProvider, tool_definition};
use serde_json::{Value, json};
use uuid::Uuid;

pub struct OpsTools;

fn text(args: &Value, key: &str) -> Result<String> {
    args.get(key).and_then(Value::as_str).filter(|v| !v.trim().is_empty()).map(str::to_string).ok_or_else(|| Error::field(key, "is required"))
}

fn uuid(args: &Value, key: &str) -> Result<Uuid> {
    text(args, key)?.parse().map_err(|_| Error::field(key, "is not an id"))
}

async fn run(k: &Kernel, ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        "ops_overview" => {
            site::may(ctx, site::READ)?;
            site::overview(k, ctx).await
        }
        "ops_command" => {
            site::may(ctx, site::READ)?;
            let id = uuid(args, "id")?;
            site::command_site(k, ctx, id).await?;
            commands::describe(k, ctx, id).await
        }
        _ => {
            site::may(ctx, site::DISPATCH)?;
            let target = uuid(args, "target")?;
            site::device_site(k, ctx, target).await?;
            let why = text(args, "reason")?;
            let cmd = NewCommand {
                target,
                operation: text(args, "operation")?,
                schema_version: 1,
                payload: args.get("payload").cloned().unwrap_or(Value::Null),
                expires_in_secs: args.get("expires_in_secs").and_then(Value::as_i64).unwrap_or(3600),
                correlation_id: Some(format!("agent: {why}")),
                issuer: Issuer::Ai,
                ..Default::default()
            };
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            let issued = commands::issue(&mut tx, ctx, cmd).await?;
            tx.commit().await?;
            Ok(json!({"id": issued.id, "state": issued.state.as_str(), "note": "A proposal: a person or an approved envelope decides. Nothing has happened."}))
        }
    }
}

#[async_trait::async_trait]
impl ToolProvider for OpsTools {
    fn tools(&self, ctx: &Ctx) -> Vec<Value> {
        if !ctx.schema.packages.iter().any(|p| p == "ops") {
            return vec![];
        }
        vec![
            tool_definition("ops_overview", "Sites and what needs a person", "Each site: its gateway, what is waiting, what is running and what needs a person.", json!({}), &[], true, false, true),
            tool_definition("ops_command", "A command and its history", "One command and everything recorded about it. A state says only what was established.", json!({"id": {"type": "string"}}), &["id"], true, false, true),
            tool_definition(
                "ops_propose",
                "Propose work for a device",
                "Writes a proposal for a person to approve. It cannot start anything itself, and a device accepts only what it was commissioned for.",
                json!({"target": {"type": "string", "description": "The device's id"}, "operation": {"type": "string"}, "payload": {"type": "object"}, "expires_in_secs": {"type": "integer"}, "reason": {"type": "string", "description": "Why, for the person deciding"}}),
                &["target", "operation", "reason"],
                false,
                false,
                false,
            ),
        ]
    }

    async fn call(&self, k: &Kernel, ctx: &Ctx, name: &str, args: &Value) -> Option<Result<Value>> {
        if !matches!(name, "ops_overview" | "ops_command" | "ops_propose") {
            return None;
        }
        Some(run(k, ctx, name, args).await)
    }
}
