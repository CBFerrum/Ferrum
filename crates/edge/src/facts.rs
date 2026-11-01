//! What gateways report. Facts arrive at least once and can be late by days, so each is applied by
//! when it happened: a late attribute does not replace a newer one, and an alarm's clearing that
//! arrives before its raising leaves it cleared.

use chrono::{DateTime, Utc};
use ferrum_domain::util::{elevated, sys};
use ferrum_kernel::{Ctx, Doc, Kernel, Result, Tx, notify};
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
        _ => {}
    }
    Ok(())
}

async fn alarm(k: &Kernel, ctx: &Ctx, tx: &mut Tx, change: &AlarmChange, at: DateTime<Utc>, site: Option<Uuid>) -> Result<()> {
    // One open alarm per rule and device. The lock is on the rule, so two reports cannot both open it.
    let rule: Option<(String, Option<String>)> = sqlx::query_as("select rule_name, severity from e_edge_rule where tenant_id = $1 and id = $2 for update")
        .bind(ctx.tenant)
        .bind(change.rule)
        .fetch_optional(&mut **tx)
        .await?;
    let open: Option<(Uuid, DateTime<Utc>)> = sqlx::query_as(
        "select id, raised_at from e_edge_alarm where tenant_id = $1 and rule = $2 and device = $3 and status <> 'Cleared' order by raised_at desc limit 1",
    )
    .bind(ctx.tenant)
    .bind(change.rule)
    .bind(change.device)
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
    let cleared_since: Option<i32> = sqlx::query_scalar("select 1 from e_edge_alarm where tenant_id = $1 and rule = $2 and device = $3 and raised_at = $4")
        .bind(ctx.tenant)
        .bind(change.rule)
        .bind(change.device)
        .bind(at)
        .fetch_optional(&mut **tx)
        .await?;
    if cleared_since.is_some() {
        return Ok(());
    }
    let (name, severity) = rule.unwrap_or_else(|| ("A rule since removed".to_string(), None));
    let severity = severity.unwrap_or_else(|| "Warning".into());
    let code: Option<String> = sqlx::query_scalar("select device_code from e_device where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(change.device)
        .fetch_optional(&mut **tx)
        .await?;
    let summary = format!("{name} on {}", code.unwrap_or_else(|| "a device".into()));
    let doc = json!({"summary": summary, "rule": change.rule, "device": change.device, "site": site, "severity": severity, "status": "Active", "value": change.value, "raised_at": at.to_rfc3339()});
    let made = k.insert_tx(&elevated(ctx), tx, "EdgeAlarm", doc.as_object().cloned().unwrap_or_default()).await?;
    if severity == "Critical" {
        let id = made.get("id").and_then(Value::as_str).and_then(|i| i.parse().ok());
        let people: Vec<Uuid> =
            sqlx::query_scalar("select user_id from user_role where tenant_id = $1 and role = 'Edge Engineer'").bind(ctx.tenant).fetch_all(&mut **tx).await?;
        for user in people {
            notify::send(tx, ctx.tenant, user, &summary, None, Some("EdgeAlarm"), id).await?;
        }
    }
    Ok(())
}
