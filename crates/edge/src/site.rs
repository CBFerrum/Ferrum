//! HTTP endpoints.
//!
//! Under `/api/app/edge/`, for signed-in people:
//!
//! | Method & path | Who | What |
//! |---|---|---|
//! | `GET /overview` | Edge Viewer | gateways with their connections, what is alarmed, strangers |
//! | `GET /templates` | Edge Viewer | definitions to start from |
//! | `POST /definitions/try` | Edge Engineer | `{body, topic?, sample?, sample_hex?}`: checks a definition and reads a sample |
//! | `POST /flows/try` | Edge Engineer or Reviewer | `{body, script?, until?}`: checks a flow and walks a script through it |
//! | `POST /flows/{id}/simulate` | Edge Engineer | `{script, until?}`: the same for what is saved, which is what lets it go for review |
//! | `GET /devices/{id}` | Edge Viewer | attributes, newest values, open alarms, recent events |
//! | `PUT /devices/{id}/desired` | Edge Engineer | `{key, value}`: asks for an attribute through the ledger |
//! | `POST /devices/{id}/token` | Edge Engineer | a token for a device that talks to us directly, shown once |
//! | `POST /strangers/claim` | Edge Engineer | `{gateway, code, name?, definition?}`: a device heard on a shared connection becomes a record |
//! | `POST /alarms/{id}/acknowledge` | Edge Engineer | `{note?}` |
//! | `GET /robots` | Edge Viewer | robots, where each is and what it is doing |
//!
//! Under `/api/site/{tenant}/edge/`:
//!
//! | Method & path | Who | What |
//! |---|---|---|
//! | `POST /config` | a gateway, signed | [`ConfigRequest`]: what it should be, if that changed |
//! | `POST /ingest` | a device, by token | `{values, state?, attributes?, at?}` |

use crate::config;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrum_domain::util::sys;
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Site, SiteRequest, SiteResponse, audit};
use ferrum_ops::commands::{self, Issuer, NewCommand};
use ferrum_ops::contract::driver;
use ferrum_ops::contract::edge::{ConfigRequest, ConfigResponse};
use ferrum_ops::contract::flow;
use ferrum_ops::contract::telemetry::{Class, Quality};
use ferrum_ops::contract::{Fact, Measurement};
use ferrum_ops::identity::{self, Refused};
use ferrum_ops::telemetry;
use rand::RngCore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub struct EdgeSite;

const READ: &[&str] = &["Edge Viewer", "Edge Engineer", "Edge Reviewer"];
const ENGINEER: &[&str] = &["Edge Engineer"];
const FLOWS: &[&str] = &["Edge Engineer", "Edge Reviewer"];

/// What a device sends in one call, at most.
const MAX_INGEST: usize = 256 << 10;

fn may(ctx: &Ctx, roles: &[&str]) -> Result<()> {
    if ctx.is_admin || roles.iter().any(|r| ctx.has_role(r)) {
        return Ok(());
    }
    Err(Error::Forbidden(format!("this takes one of: {}", roles.join(", "))))
}

fn ok(v: Value) -> SiteResponse {
    SiteResponse::json(200, &v)
}

fn body(req: &SiteRequest) -> Result<Value> {
    serde_json::from_slice(&req.body).map_err(|e| Error::invalid(format!("the request body: {e}")))
}

fn uuid(seg: &str) -> Result<Uuid> {
    seg.parse().map_err(|_| Error::NotFound("record".into()))
}

/// Who sees what is the Operations rule: a members-only site is closed to everyone not listed.
async fn open_gateways(k: &Kernel, ctx: &Ctx) -> Result<Value> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let gateways: Value = sqlx::query_scalar(
        "select coalesce(jsonb_agg(jsonb_build_object('id', d.id, 'code', d.device_code, 'name', d.device_name, 'lifecycle', d.lifecycle, 'site', s.site_name, \
           'last_seen_at', g.seen_at, 'silent_secs', extract(epoch from now() - o.last_seen_at)::int8, 'links', coalesce(g.links, '[]'), 'problems', coalesce(g.problems, '[]'), \
           'runtime', g.runtime, 'config_version', g.config_version, \
           'devices', (select count(*) from e_device c where c.tenant_id = d.tenant_id and c.gateway = d.id)) order by d.device_code), '[]') \
         from e_device d left join e_site s on s.tenant_id = d.tenant_id and s.id = d.site \
         left join edge_gateway g on g.tenant_id = d.tenant_id and g.device_id = d.id \
         left join op_gateway o on o.tenant_id = d.tenant_id and o.device_id = d.id \
         where d.tenant_id = $1 and d.kind = 'Gateway' and d.lifecycle <> 'Retired'",
    )
    .bind(ctx.tenant)
    .fetch_one(&mut *tx)
    .await?;
    let alarms: Vec<(Option<String>, i64)> =
        sqlx::query_as("select severity, count(*) from e_edge_alarm where tenant_id = $1 and status <> 'Cleared' group by severity")
            .bind(ctx.tenant)
            .fetch_all(&mut *tx)
            .await?;
    let counts: (i64, i64, i64) = sqlx::query_as(
        "select (select count(*) from e_device where tenant_id = $1 and lifecycle = 'Active'), \
                (select count(*) from e_device where tenant_id = $1 and edge_definition is not null), \
                (select count(*) from e_device_definition where tenant_id = $1 and status = 'Published')",
    )
    .bind(ctx.tenant)
    .fetch_one(&mut *tx)
    .await?;
    let strangers: Value = sqlx::query_scalar(
        "select coalesce(jsonb_agg(jsonb_build_object('gateway', s.gateway_id, 'gateway_name', d.device_name, 'code', s.code, 'last_seen_at', s.last_seen_at) order by s.last_seen_at desc), '[]') from edge_stranger s \
         join e_device d on d.tenant_id = s.tenant_id and d.id = s.gateway_id \
         where s.tenant_id = $1 and not exists (select 1 from e_device x where x.tenant_id = s.tenant_id and x.device_code = s.code)",
    )
    .bind(ctx.tenant)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(
        json!({"gateways": gateways, "alarms": alarms.into_iter().map(|(s, n)| (s.unwrap_or_default(), n)).collect::<std::collections::BTreeMap<_, _>>(), "devices": counts.0, "defined": counts.1, "definitions": counts.2, "strangers": strangers}),
    )
}

/// Reads a sample by a definition, with no device attached. What the screen shows a person writing
/// one, and the same code a gateway will run.
fn try_definition(req: &Value) -> Value {
    let text = req["body"].as_str().unwrap_or_default();
    let compiled = match driver::parse(text) {
        Ok(c) => c,
        Err(why) => return json!({"ok": false, "error": why}),
    };
    let sample: Vec<u8> = match (req["sample"].as_str(), req["sample_hex"].as_str()) {
        (_, Some(h)) => match hex::decode(h.replace(' ', "")) {
            Ok(b) => b,
            Err(e) => return json!({"ok": false, "error": format!("the sample: {e}")}),
        },
        (Some(t), None) => t.as_bytes().to_vec(),
        (None, None) => Vec::new(),
    };
    let decoded: Vec<Value> = compiled
        .decode(req["topic"].as_str().unwrap_or_default(), &sample)
        .into_iter()
        .map(|d| match d {
            driver::Decoded::Metric { device, metric, value, unit, .. } => {
                json!({"kind": "metric", "device": device, "name": metric, "value": value, "unit": unit})
            }
            driver::Decoded::State { device, state } => json!({"kind": "state", "device": device, "value": state}),
            driver::Decoded::Attribute { device, key, value } => json!({"kind": "attribute", "device": device, "name": key, "value": value}),
            driver::Decoded::Event { device, name, value } => json!({"kind": "event", "device": device, "name": name, "value": value}),
        })
        .collect();
    let d = &compiled.definition;
    json!({"ok": true, "name": d.name, "connect": d.connect, "commands": d.commands.keys().collect::<Vec<_>>(), "polls": d.poll.len(), "reads": d.read.len(), "decoded": decoded})
}

async fn device(k: &Kernel, ctx: &Ctx, id: Uuid) -> Result<Value> {
    ferrum_ops::site::device_site(k, ctx, id).await?;
    let latest = telemetry::latest(k, ctx, id).await?;
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let attributes: Value = sqlx::query_scalar(
        "select coalesce(jsonb_agg(jsonb_build_object('key', key, 'value', value, 'reported_at', reported_at, 'desired', desired, 'desired_at', desired_at, 'in_step', desired is null or value is not distinct from desired) order by key), '[]') \
         from edge_attribute where tenant_id = $1 and device_id = $2",
    )
    .bind(ctx.tenant)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let alarms: Value = sqlx::query_scalar(
        "select coalesce(jsonb_agg(jsonb_build_object('id', id, 'summary', summary, 'severity', severity, 'status', status, 'raised_at', raised_at) order by raised_at desc), '[]') \
         from e_edge_alarm where tenant_id = $1 and device = $2 and status <> 'Cleared'",
    )
    .bind(ctx.tenant)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let events: Value = sqlx::query_scalar("select coalesce(jsonb_agg(e order by at desc), '[]') from (select name, value, at from edge_event where tenant_id = $1 and device_id = $2 order by at desc limit 50) e")
        .bind(ctx.tenant)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"latest": latest, "attributes": attributes, "alarms": alarms, "events": events}))
}

/// Asks for an attribute. This is a command like any other: the device's definition says what
/// `set.{key}` means, it goes through the ledger, and the attribute stays out of step until the
/// device itself reports the new value.
async fn desire(k: &Kernel, ctx: &Ctx, id: Uuid, key: &str, value: &Value) -> Result<Value> {
    ferrum_ops::site::device_site(k, ctx, id).await?;
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    sqlx::query(
        "insert into edge_attribute (tenant_id, device_id, key, desired, desired_at, desired_by) values ($1, $2, $3, $4, now(), $5) \
         on conflict (tenant_id, device_id, key) do update set desired = excluded.desired, desired_at = excluded.desired_at, desired_by = excluded.desired_by",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(key)
    .bind(value)
    .bind(&ctx.email)
    .execute(&mut *tx)
    .await?;
    let issued = commands::issue(
        &mut tx,
        ctx,
        NewCommand {
            target: id,
            operation: format!("set.{key}"),
            schema_version: 1,
            payload: json!({"key": key, "value": value}),
            expires_in_secs: 3600,
            issuer: Issuer::Person,
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(json!({"command": issued.id, "state": issued.state.as_str()}))
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

async fn new_token(k: &Kernel, ctx: &Ctx, id: Uuid) -> Result<Value> {
    ferrum_ops::site::device_site(k, ctx, id).await?;
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let mut v = Doc::new();
    v.insert("edge_token_hash".into(), json!(token_hash(&token)));
    k.set_values_tx(&sys(ctx), &mut tx, "Device", id, v).await?;
    audit::record(&mut tx, ctx, "Device", id, "edge.token_issued", None, json!({})).await?;
    tx.commit().await?;
    Ok(json!({"token": token}))
}

async fn claim(k: &Kernel, ctx: &Ctx, b: &Value) -> Result<Value> {
    let gateway = uuid(b["gateway"].as_str().unwrap_or_default())?;
    let code = b["code"].as_str().unwrap_or_default().trim();
    if code.is_empty() {
        return Err(Error::field("code", "which device?"));
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let site: Option<Option<Uuid>> =
        sqlx::query_scalar("select site from e_device where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(gateway).fetch_optional(&mut *tx).await?;
    let site = site.ok_or_else(|| Error::NotFound("gateway".into()))?;
    let doc = json!({"device_name": b["name"].as_str().unwrap_or(code), "device_code": code, "kind": b["kind"].as_str().unwrap_or("Machine"), "site": site, "gateway": gateway, "edge_definition": b["definition"]});
    let made = k.insert_tx(ctx, &mut tx, "Device", doc.as_object().cloned().unwrap_or_default()).await?;
    sqlx::query("delete from edge_stranger where tenant_id = $1 and gateway_id = $2 and code = $3")
        .bind(ctx.tenant)
        .bind(gateway)
        .bind(code)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"id": made.get("id")}))
}

async fn acknowledge(k: &Kernel, ctx: &Ctx, id: Uuid, note: Option<&str>) -> Result<Value> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let status: Option<Option<String>> = sqlx::query_scalar("select status from e_edge_alarm where tenant_id = $1 and id = $2 for update")
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let status = status.ok_or_else(|| Error::NotFound("alarm".into()))?.unwrap_or_default();
    if status != "Active" {
        return Err(Error::invalid(format!("this alarm is {status}")));
    }
    let mut v = Doc::new();
    v.insert("status".into(), json!("Acknowledged"));
    v.insert("acknowledged_by".into(), json!(ctx.email));
    v.insert("acknowledged_at".into(), json!(Utc::now().to_rfc3339()));
    v.insert("note".into(), json!(note));
    k.set_values_tx(&sys(ctx), &mut tx, "EdgeAlarm", id, v).await?;
    tx.commit().await?;
    Ok(json!({"status": "Acknowledged"}))
}

/// Robots and what they last said. `position.x`, `position.y` and `position.theta` are metrics like
/// any other, so anything a definition can read can be drawn.
async fn robots(k: &Kernel, ctx: &Ctx) -> Result<Value> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let rows: Value = sqlx::query_scalar(
        "select coalesce(jsonb_agg(jsonb_build_object('id', d.id, 'code', d.device_code, 'name', d.device_name, 'lifecycle', d.lifecycle, 'site', s.site_name, \
           'metrics', (select coalesce(jsonb_object_agg(l.metric, jsonb_build_object('value', l.last_value, 'at', l.last_time)), '{}') from ( \
              select distinct on (r.metric) r.metric, r.last_value, r.last_time from op_rollup_minute r where r.tenant_id = d.tenant_id and r.device_id = d.id and r.last_time is not null order by r.metric, r.minute desc) l), \
           'state', (select e.state from op_equipment_event e where e.tenant_id = d.tenant_id and e.device_id = d.id order by e.at desc limit 1), \
           'running', (select count(*) from op_command c where c.tenant_id = d.tenant_id and c.target_id = d.id and c.state in ('Accepted', 'Executing'))) order by d.device_code), '[]') \
         from e_device d left join e_site s on s.tenant_id = d.tenant_id and s.id = d.site \
         where d.tenant_id = $1 and d.kind = 'Robot' and d.lifecycle <> 'Retired'",
    )
    .bind(ctx.tenant)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"robots": rows}))
}

async fn signed_in(k: &Kernel, ctx: &Ctx, req: &SiteRequest) -> Result<SiteResponse> {
    let path = req.path.trim_matches('/').to_string();
    let parts: Vec<&str> = path.split('/').collect();
    let value = match (req.method.as_str(), parts.as_slice()) {
        ("GET", ["overview"]) => {
            may(ctx, READ)?;
            open_gateways(k, ctx).await?
        }
        ("GET", ["templates"]) => {
            may(ctx, READ)?;
            json!({"templates": crate::templates::all()})
        }
        ("POST", ["definitions", "try"]) => {
            may(ctx, ENGINEER)?;
            try_definition(&body(req)?)
        }
        ("POST", ["flows", "try"]) => {
            may(ctx, FLOWS)?;
            let b = body(req)?;
            return Ok(ok(try_flow(b["body"].as_str().unwrap_or_default(), &b)));
        }
        ("POST", ["flows", id, "simulate"]) => {
            may(ctx, ENGINEER)?;
            return simulate_flow(k, ctx, uuid(id)?, &body(req)?).await.map(ok);
        }
        ("GET", ["devices", id]) => {
            may(ctx, READ)?;
            device(k, ctx, uuid(id)?).await?
        }
        ("PUT", ["devices", id, "desired"]) => {
            may(ctx, ENGINEER)?;
            let b = body(req)?;
            let key = b["key"].as_str().filter(|k| !k.is_empty() && k.len() <= 200).ok_or_else(|| Error::field("key", "which attribute?"))?;
            desire(k, ctx, uuid(id)?, key, &b["value"]).await?
        }
        ("POST", ["devices", id, "token"]) => {
            may(ctx, ENGINEER)?;
            new_token(k, ctx, uuid(id)?).await?
        }
        ("POST", ["strangers", "claim"]) => {
            may(ctx, ENGINEER)?;
            claim(k, ctx, &body(req)?).await?
        }
        ("POST", ["alarms", id, "acknowledge"]) => {
            may(ctx, ENGINEER)?;
            let b = body(req)?;
            acknowledge(k, ctx, uuid(id)?, b["note"].as_str()).await?
        }
        ("GET", ["robots"]) => {
            may(ctx, READ)?;
            robots(k, ctx).await?
        }
        _ => return Err(Error::NotFound("route".into())),
    };
    Ok(ok(value))
}

/// Checks a flow and, given a script, shows what it would do, by the code a gateway runs.
fn try_flow(text: &str, b: &Value) -> Value {
    let compiled = match flow::parse(text) {
        Ok(f) => f,
        Err(why) => return json!({"ok": false, "error": why}),
    };
    let script: Vec<flow::Scripted> = match serde_json::from_value(b.get("script").cloned().unwrap_or(json!([]))) {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": format!("the script: {e}")}),
    };
    match flow::simulate(&compiled, &script, b["until"].as_i64().unwrap_or(0)) {
        Ok(moments) => json!({"ok": true, "digest": compiled.digest, "moments": moments}),
        Err(why) => json!({"ok": false, "error": why}),
    }
}

/// Simulating a version is what lets it go for review, and only a run that worked counts.
async fn simulate_flow(k: &Kernel, ctx: &Ctx, id: Uuid, b: &Value) -> Result<Value> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let text: Option<Option<String>> =
        sqlx::query_scalar("select body from e_edge_flow where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).fetch_optional(&mut *tx).await?;
    let text = text.ok_or_else(|| Error::NotFound("flow".into()))?.unwrap_or_default();
    let result = try_flow(&text, b);
    let moments = result["moments"].as_array().map_or(0, Vec::len);
    if result["ok"] == json!(true) {
        if moments == 0 {
            return Ok(json!({"ok": false, "error": "nothing happened: give it a script that reaches its nodes"}));
        }
        let mut values = Doc::new();
        values.insert("simulated_digest".into(), result["digest"].clone());
        k.set_values_tx(&sys(ctx), &mut tx, "EdgeFlow", id, values).await?;
    }
    tx.commit().await?;
    Ok(result)
}

async fn gateway_config(k: &Kernel, req: &SiteRequest) -> Result<SiteResponse> {
    let gateway = match identity::verify(k, req).await {
        Ok(device) => device,
        Err(Refused::Clock(now)) => {
            return Ok(SiteResponse::json(401, &json!({"error": "this site's clock is too far from the server's", "server_time": now})));
        }
        Err(Refused::Key(why)) => return Ok(SiteResponse::json(401, &json!({"error": why}))),
        Err(Refused::Other(e)) => return Err(e),
    };
    let request: ConfigRequest = serde_json::from_slice(&req.body).map_err(|e| Error::invalid(format!("the request body: {e}")))?;
    let mut tx = k.db.tenant_tx(req.tenant).await?;
    let site: Option<Option<Uuid>> =
        sqlx::query_scalar("select site from e_device where tenant_id = $1 and id = $2").bind(req.tenant).bind(gateway).fetch_optional(&mut *tx).await?;
    let site = site.flatten();
    let config = config::for_gateway(&mut tx, req.tenant, gateway, site).await?;
    sqlx::query(
        "insert into edge_gateway (tenant_id, device_id, config_version, runtime, links, problems, seen_at) values ($1, $2, $3, $4, $5, $6, now()) \
         on conflict (tenant_id, device_id) do update set config_version = excluded.config_version, runtime = excluded.runtime, links = excluded.links, problems = excluded.problems, seen_at = now()",
    )
    .bind(req.tenant)
    .bind(gateway)
    .bind(&request.have)
    .bind(&request.runtime)
    .bind(&request.links)
    .bind(json!(request.problems))
    .execute(&mut *tx)
    .await?;
    for code in request.strangers.iter().take(200) {
        sqlx::query("insert into edge_stranger (tenant_id, gateway_id, code) values ($1, $2, $3) on conflict (tenant_id, gateway_id, code) do update set last_seen_at = now()")
            .bind(req.tenant)
            .bind(gateway)
            .bind(code)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let response = ConfigResponse { config: (config.version != request.have).then_some(config) };
    Ok(ok(serde_json::to_value(response).map_err(|e| Error::internal(e.to_string()))?))
}

/// A device that talks to us directly: a cellular sensor, another cloud's webhook. It has a token and
/// no gateway, so it gets nothing back and can command nothing.
async fn ingest(k: &Kernel, req: &SiteRequest) -> Result<SiteResponse> {
    let token = req.headers.get("x-ferrum-device-token").and_then(Value::as_str).unwrap_or_default();
    if token.len() < 32 {
        return Ok(SiteResponse::json(401, &json!({"error": "this needs a device's token"})));
    }
    let b = body(req)?;
    let at: DateTime<Utc> = b["at"].as_str().and_then(|t| t.parse().ok()).unwrap_or_else(Utc::now);
    let mut tx = k.db.tenant_tx(req.tenant).await?;
    let row: Option<(Uuid, Option<Uuid>, String)> = sqlx::query_as("select id, site, lifecycle from e_device where tenant_id = $1 and edge_token_hash = $2")
        .bind(req.tenant)
        .bind(token_hash(token))
        .fetch_optional(&mut *tx)
        .await?;
    let Some((device, Some(site), lifecycle)) = row else {
        return Ok(SiteResponse::json(401, &json!({"error": "no device has that token"})));
    };
    if matches!(lifecycle.as_str(), "Quarantined" | "Retired") {
        return Ok(SiteResponse::json(403, &json!({"error": format!("this device is {lifecycle}")})));
    }
    let now = Utc::now();
    // A sequence from the clock, so a repeat of the same reading lands once.
    let boot = Uuid::from_u128(device.as_u128() ^ 0x1);
    let batch: Vec<Measurement> = b["values"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(metric, v)| {
                    Some(Measurement {
                        device_id: device,
                        metric: metric.clone(),
                        value: v.as_f64()?,
                        unit: None,
                        source_time: at,
                        boot,
                        seq: at.timestamp_micros(),
                        quality: Quality::Good,
                        class: Class::Sample,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let (stored, skipped) = {
        telemetry::ensure_partitions(k, now).await?;
        telemetry::ingest(&mut tx, req.tenant, site, batch, now).await?
    };
    let mut facts = Vec::new();
    if let Some(state) = b["state"].as_str() {
        facts.push(Fact {
            fact_id: Uuid::now_v7(),
            kind: "equipment.state".into(),
            at_source: at,
            actor: None,
            body: json!({"device": device, "state": state}),
        });
    }
    for (key, value) in b["attributes"].as_object().into_iter().flatten() {
        facts.push(Fact {
            fact_id: Uuid::now_v7(),
            kind: "edge.attribute".into(),
            at_source: at,
            actor: None,
            body: json!({"device": device, "key": key, "value": value}),
        });
    }
    for f in &facts {
        ferrum_ops::sync::apply_fact(&mut tx, req.tenant, site, f).await?;
    }
    tx.commit().await?;
    Ok(ok(json!({"stored": stored, "skipped": skipped})))
}

#[async_trait]
impl Site for EdgeSite {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        if let Some(ctx) = req.user.clone() {
            return signed_in(k, &ctx, &req).await;
        }
        match (req.method.as_str(), req.path.trim_matches('/')) {
            ("POST", "config") => gateway_config(k, &req).await,
            ("POST", "ingest") => ingest(k, &req).await,
            _ => Err(Error::NotFound("route".into())),
        }
    }

    fn max_body_for(&self, _method: &str, path: &str) -> usize {
        match path.trim_matches('/') {
            "ingest" => MAX_INGEST,
            _ => 1 << 20,
        }
    }
}
