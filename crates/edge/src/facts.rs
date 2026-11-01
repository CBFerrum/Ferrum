//! What gateways report. Facts arrive at least once and can be late by days, so each is applied by
//! when it happened: a late attribute does not replace a newer one, and an alarm's clearing that
//! arrives before its raising leaves it cleared.

use chrono::{DateTime, Utc};
use ferrum_domain::util::{elevated, sys};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx, notify};
use ferrum_ops::commands::{self, Issuer, NewCommand};
use ferrum_ops::contract::edge::AlarmChange;
use serde_json::{Value, json};
use uuid::Uuid;

/// One fact, by what comes after `ops.fact.edge.`.
pub async fn apply(k: &Kernel, ctx: &Ctx, tx: &mut Tx, kind: &str, payload: &Value) -> Result<()> {
    let Some(at) = payload["at"].as_str().and_then(|t| t.parse::<DateTime<Utc>>().ok()) else { return Ok(()) };
    let body = &payload["body"];
    match kind {
        "attribute" => {
            let (Some(device), Some(key)) = (body["device"].as_str().and_then(|d| d.parse::<Uuid>().ok()), body["key"].as_str()) else { return Ok(()) };
            sqlx::query(
                "insert into edge_attribute (tenant_id, device_id, key, value, reported_at) values ($1, $2, $3, $4, $5) \
                 on conflict (tenant_id, device_id, key) do update set value = excluded.value, reported_at = excluded.reported_at \
                 where edge_attribute.reported_at is null or edge_attribute.reported_at < excluded.reported_at",
            )
            .bind(ctx.tenant)
            .bind(device)
            .bind(key)
            .bind(&body["value"])
            .bind(at)
            .execute(&mut **tx)
            .await?;
        }
        "event" => {
            let (Some(device), Some(name), Some(id)) = (
                body["device"].as_str().and_then(|d| d.parse::<Uuid>().ok()),
                body["name"].as_str(),
                payload["fact"].as_str().and_then(|f| f.parse::<Uuid>().ok()),
            ) else {
                return Ok(());
            };
            sqlx::query("insert into edge_event (tenant_id, id, device_id, name, value, at) values ($1, $2, $3, $4, $5, $6) on conflict do nothing")
                .bind(ctx.tenant)
                .bind(id)
                .bind(device)
                .bind(name)
                .bind(&body["value"])
                .bind(at)
                .execute(&mut **tx)
                .await?;
        }
        "alarm" => {
            let Ok(change) = serde_json::from_value::<AlarmChange>(body.clone()) else { return Ok(()) };
            let site = payload["site"].as_str().and_then(|s| s.parse::<Uuid>().ok());
            alarm(k, ctx, tx, &change, at, site).await?;
        }
        "notify" => {
            let (Some(title), flow) = (body["title"].as_str(), body["flow"].as_str().and_then(|f| f.parse::<Uuid>().ok())) else { return Ok(()) };
            for user in engineers(ctx, tx).await? {
                notify::send(tx, ctx.tenant, user, title, None, Some("EdgeFlow"), flow).await?;
            }
        }
        "request" => request(ctx, tx, payload, at).await?,
        _ => {}
    }
    Ok(())
}

async fn engineers(ctx: &Ctx, tx: &mut Tx) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar("select user_id from user_role where tenant_id = $1 and role = 'Edge Engineer'").bind(ctx.tenant).fetch_all(&mut **tx).await?)
}

/// What a flow asked for, which becomes a command only by the ledger's rules: as automation, it runs
/// under an envelope someone approved, or waits for a person. It may have waited out an outage on the
/// way here, so what is too old, or from a flow that has since been stopped, is not issued: a request
/// is a thought at a moment, and that moment has passed. Whatever is refused says why on the device,
/// and is not tried again, because nothing about it will change.
async fn request(ctx: &Ctx, tx: &mut Tx, payload: &Value, at: DateTime<Utc>) -> Result<()> {
    let body = &payload["body"];
    let (Some(device), Some(flow), Some(operation), Some(fact)) = (
        body["device"].as_str().and_then(|d| d.parse::<Uuid>().ok()),
        body["flow"].as_str().and_then(|f| f.parse::<Uuid>().ok()),
        body["operation"].as_str(),
        payload["fact"].as_str().and_then(|f| f.parse::<Uuid>().ok()),
    ) else {
        return Ok(());
    };
    let live: Option<bool> =
        sqlx::query_scalar("select live from e_edge_flow where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(flow).fetch_optional(&mut **tx).await?;
    let left = body["expires_secs"].as_i64().unwrap_or(300) - (Utc::now() - at).num_seconds();
    let refused = if live != Some(true) {
        Some("the flow that asked for this has been stopped".to_string())
    } else if left <= 0 {
        Some("it took too long to get here".to_string())
    } else {
        let cmd = NewCommand {
            target: device,
            operation: operation.to_string(),
            schema_version: 1,
            payload: body["payload"].clone(),
            expected_state: Value::Null,
            expires_in_secs: left,
            dedupe_key: Some(format!("flow:{fact}")),
            workflow_id: Some(format!("flow:{flow}")),
            step_id: body["node"].as_str().map(str::to_string),
            correlation_id: None,
            issuer: Issuer::Automation,
        };
        match commands::issue(tx, ctx, cmd).await {
            Ok(_) => None,
            Err(e @ (Error::Validation(_) | Error::Forbidden(_) | Error::NotFound(_))) => Some(e.to_string()),
            Err(e) => return Err(e),
        }
    };
    if let Some(why) = refused {
        sqlx::query("insert into edge_event (tenant_id, id, device_id, name, value, at) values ($1, $2, $3, $4, $5, $6) on conflict do nothing")
            .bind(ctx.tenant)
            .bind(fact)
            .bind(device)
            .bind("flow.request.refused")
            .bind(json!({"operation": operation, "why": why}))
            .bind(at)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn alarm(k: &Kernel, ctx: &Ctx, tx: &mut Tx, change: &AlarmChange, at: DateTime<Utc>, site: Option<Uuid>) -> Result<()> {
    // One open alarm per rule or flow node, and device. The lock is on the rule or flow, so two
    // reports cannot both open it.
    let node = change.node.as_deref();
    let origin: Option<(String, Option<String>)> = match node {
        None => sqlx::query_as("select rule_name, severity from e_edge_rule where tenant_id = $1 and id = $2 for update"),
        Some(_) => sqlx::query_as("select flow_name, null::text from e_edge_flow where tenant_id = $1 and id = $2 for update"),
    }
    .bind(ctx.tenant)
    .bind(change.rule)
    .fetch_optional(&mut **tx)
    .await?;
    const SAME: &str = "tenant_id = $1 and device = $3 and case when $4::text is null then rule = $2 else flow = $2 and flow_node = $4 end";
    let open: Option<(Uuid, DateTime<Utc>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select id, raised_at from e_edge_alarm where {SAME} and status <> 'Cleared' order by raised_at desc limit 1"
    )))
    .bind(ctx.tenant)
    .bind(change.rule)
    .bind(change.device)
    .bind(node)
    .fetch_optional(&mut **tx)
    .await?;
    if !change.raised {
        // Only the alarm this clears: one raised after it is another alarm.
        if let Some((id, raised_at)) = open
            && raised_at <= at
        {
            let mut v = Doc::new();
            v.insert("status".into(), json!("Cleared"));
            v.insert("cleared_at".into(), json!(at.to_rfc3339()));
            k.set_values_tx(&sys(ctx), tx, "EdgeAlarm", id, v).await?;
        }
        return Ok(());
    }
    if open.is_some() {
        return Ok(());
    }
    let cleared_since: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select 1 from e_edge_alarm where {SAME} and raised_at = $5")))
        .bind(ctx.tenant)
        .bind(change.rule)
        .bind(change.device)
        .bind(node)
        .bind(at)
        .fetch_optional(&mut **tx)
        .await?;
    if cleared_since.is_some() {
        return Ok(());
    }
    let (origin_name, origin_severity) = origin.unwrap_or_else(|| ("A rule since removed".to_string(), None));
    let name = change.name.clone().unwrap_or(origin_name);
    let severity = change.severity.clone().or(origin_severity).unwrap_or_else(|| "Warning".into());
    let code: Option<String> = sqlx::query_scalar("select device_code from e_device where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(change.device)
        .fetch_optional(&mut **tx)
        .await?;
    let summary = format!("{name} on {}", code.unwrap_or_else(|| "a device".into()));
    let mut doc = json!({"summary": summary, "device": change.device, "site": site, "severity": severity, "status": "Active", "value": change.value, "raised_at": at.to_rfc3339()});
    match node {
        None => doc["rule"] = json!(change.rule),
        Some(node) => (doc["flow"], doc["flow_node"]) = (json!(change.rule), json!(node)),
    }
    let made = k.insert_tx(&elevated(ctx), tx, "EdgeAlarm", doc.as_object().cloned().unwrap_or_default()).await?;
    if severity == "Critical" {
        let id = made.get("id").and_then(Value::as_str).and_then(|i| i.parse().ok());
        for user in engineers(ctx, tx).await? {
            notify::send(tx, ctx.tenant, user, &summary, None, Some("EdgeAlarm"), id).await?;
        }
    }
    Ok(())
}
