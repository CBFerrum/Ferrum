//! What an AI agent may do with physical work: look, explain, and propose. There is no tool for
//! anything else. A proposal is a command that waits for a person, unless an envelope a person
//! approved covers it, and an agent cannot say otherwise, because this is the only way it gets in.
//!
//! Instructions in manuals, logs or device strings are data. No text an agent reads can change what
//! these tools do, because what they do does not depend on the model.
//!
//! A proposal says when what it rests on was read, and is refused if that is old or if nothing has been
//! heard from the site since. Saying that nobody knows is the answer then, not a guess.

use crate::commands::{self, Issuer, NewCommand};
use crate::site;
use chrono::{DateTime, Utc};
use ferrum_kernel::{Ctx, Error, Kernel, Result};
use ferrum_mcp::{ToolProvider, tool_definition};
use serde_json::{Value, json};
use uuid::Uuid;

pub struct OpsTools;

/// How old what a proposal rests on may be, and how long a site may have been silent.
const EVIDENCE_SECS: i64 = 300;

/// Refuses what rests on old evidence, or on a site nobody has heard from.
async fn current(k: &Kernel, ctx: &Ctx, target: Uuid, args: &Value) -> Result<()> {
    let at: DateTime<Utc> = text(args, "evidence_at")?.parse().map_err(|_| Error::field("evidence_at", "the time what this rests on was read"))?;
    let age = (Utc::now() - at).num_seconds();
    if !(-5..=EVIDENCE_SECS).contains(&age) {
        return Err(Error::Conflict(format!("What this rests on was read {age} seconds ago. Read it again before proposing anything from it")));
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let silent: Option<Option<i64>> = sqlx::query_scalar(
        "select max(extract(epoch from now() - g.last_seen_at)::int8) from e_device d join op_authority a on a.tenant_id = d.tenant_id and a.site_id = d.site \
         left join op_gateway g on g.tenant_id = a.tenant_id and g.device_id = a.holder where d.tenant_id = $1 and d.id = $2 group by d.id",
    )
    .bind(ctx.tenant)
    .bind(target)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    match silent.flatten() {
        Some(secs) if secs <= EVIDENCE_SECS => Ok(()),
        Some(secs) => Err(Error::Conflict(format!("Nothing has been heard from this site for {secs} seconds, so what it is doing is unknown"))),
        None => Err(Error::Conflict("Nothing has been heard from this site, so what it is doing is unknown".into())),
    }
}

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
            current(k, ctx, target, args).await?;
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
            tool_definition(
                "ops_overview",
                "Sites and what needs a person",
                "Each site: its gateway, what is waiting, what is running and what needs a person.",
                json!({}),
                &[],
                true,
                false,
                true,
            ),
            tool_definition(
                "ops_command",
                "A command and its history",
                "One command and everything recorded about it. A state says only what was established.",
                json!({"id": {"type": "string"}}),
                &["id"],
                true,
                false,
                true,
            ),
            tool_definition(
                "ops_propose",
                "Propose work for a device",
                "Writes a proposal for a person to approve. It cannot start anything itself, and a device accepts only what it was commissioned for.",
                json!({"target": {"type": "string", "description": "The device's id"}, "evidence_at": {"type": "string", "description": "The server_time of the ops_overview this rests on"}, "operation": {"type": "string"}, "payload": {"type": "object"}, "expires_in_secs": {"type": "integer"}, "reason": {"type": "string", "description": "Why, for the person deciding"}}),
                &["target", "operation", "reason", "evidence_at"],
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
