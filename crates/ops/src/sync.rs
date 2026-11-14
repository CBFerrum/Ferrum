//! The one call a site makes. Everything arrives in one transaction and the answer goes back only
//! after it commits, so a lost answer means the site says it all again, and saying it again changes
//! nothing: reports by `report_id`, facts by `fact_id`, measurements by boot and sequence.

use crate::commands::{self, Applied};
use crate::{connections, devices, telemetry};
use chrono::{Duration, Utc};
use ferrum_kernel::notify::LIVE_CHANNEL;
use ferrum_kernel::{Error, Kernel, Result, Tx, outbox};
use ferrum_ops_contract::sync::{Lease, Operator, SiteDevice};
use ferrum_ops_contract::telemetry::Limits;
use ferrum_ops_contract::{Fact, SyncRequest, SyncResponse, VERSION};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

const MAX_REPORTS: usize = 2000;
const MAX_FACTS: usize = 2000;
const MAX_MEASUREMENTS: usize = 50_000;

fn valid_kind(kind: &str) -> bool {
    !kind.is_empty() && kind.len() <= 100 && kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

pub async fn apply_fact(tx: &mut Tx, tenant: Uuid, site: Uuid, f: &Fact) -> Result<()> {
    apply_fact_from(tx, tenant, site, None, f).await
}

/// The same, from a gateway, which may be saying who is connected to it.
pub async fn apply_fact_from(tx: &mut Tx, tenant: Uuid, site: Uuid, gateway: Option<Uuid>, f: &Fact) -> Result<()> {
    let new = sqlx::query(
        "insert into op_fact (tenant_id, id, site_id, kind, at_source, actor, body) values ($1, $2, $3, $4, $5, $6, $7) on conflict (tenant_id, id) do nothing",
    )
    .bind(tenant)
    .bind(f.fact_id)
    .bind(site)
    .bind(&f.kind)
    .bind(f.at_source)
    .bind(&f.actor)
    .bind(&f.body)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        > 0;
    if !new {
        return Ok(());
    }
    if let (Some(gateway), Some(device)) =
        (gateway, f.kind.starts_with("connection.").then(|| f.body.get("device").and_then(|v| v.as_str()).and_then(|v| v.parse::<Uuid>().ok())).flatten())
    {
        if f.kind == "connection.opened" {
            connections::opened(tx, tenant, device, gateway, site, f.at_source).await?;
        } else if f.kind == "connection.closed" {
            connections::closed(tx, tenant, device, gateway, f.at_source).await?;
        }
    }
    if f.kind == "equipment.state" {
        // Equipment's own history is kept here, for every app's OEE. Late ones land in their place.
        let (device, state) =
            (f.body.get("device").and_then(|v| v.as_str()).and_then(|v| v.parse::<Uuid>().ok()), f.body.get("state").and_then(|v| v.as_str()));
        if let (Some(device), Some(state)) = (device, state.filter(|s| matches!(*s, "Running" | "Idle" | "Stopped" | "Maintenance" | "Unknown"))) {
            sqlx::query(
                "insert into op_equipment_event (tenant_id, device_id, at, state, reason, fact_id) values ($1, $2, $3, $4, $5, $6) on conflict do nothing",
            )
            .bind(tenant)
            .bind(device)
            .bind(f.at_source)
            .bind(state)
            .bind(f.body.get("reason").and_then(|v| v.as_str()))
            .bind(f.fact_id)
            .execute(&mut **tx)
            .await?;
        }
    }
    let payload = json!({"fact": f.fact_id, "site": site, "kind": f.kind, "at": f.at_source, "actor": f.actor, "body": f.body});
    outbox::enqueue(tx, tenant, &format!("ops.fact.{}", f.kind), None, Some(f.fact_id), payload).await
}

/// What a site must know of its devices, and a version that changes when it does.
async fn site_devices(tx: &mut Tx, tenant: Uuid, domain: &str, site: Uuid) -> Result<(Vec<SiteDevice>, String)> {
    let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "select d.id, d.device_code, d.kind, d.lifecycle from e_device d join e_site s on s.tenant_id = d.tenant_id and s.id = d.site \
         where d.tenant_id = $1 and d.site = $2 and coalesce(nullif(d.domain, ''), s.site_code) = $3 and d.lifecycle not in ('Imported', 'Claimed', 'Retired') order by d.id",
    )
    .bind(tenant)
    .bind(site)
    .bind(domain)
    .fetch_all(&mut **tx)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let mut operations: BTreeMap<Uuid, Vec<String>> = BTreeMap::new();
    for (parent, op) in sqlx::query_as::<_, (Uuid, String)>(
        "select parent_id, operation from e_device__operations where tenant_id = $1 and parent_id = any($2) order by operation",
    )
    .bind(tenant)
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?
    {
        operations.entry(parent).or_default().push(op);
    }
    let mut limits: BTreeMap<Uuid, BTreeMap<String, Limits>> = BTreeMap::new();
    for (parent, metric, min, max, stuck) in sqlx::query_as::<_, (Uuid, String, Option<f64>, Option<f64>, Option<i64>)>(
        "select parent_id, metric, min_value::float8, max_value::float8, stuck_after from e_device__metrics where tenant_id = $1 and parent_id = any($2)",
    )
    .bind(tenant)
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?
    {
        limits.entry(parent).or_default().insert(metric, Limits { min, max, stuck_after: stuck.unwrap_or(0).max(0) as u32 });
    }
    let devices: Vec<SiteDevice> = rows
        .into_iter()
        .map(|(id, code, kind, lifecycle)| SiteDevice {
            id,
            code,
            kind,
            lifecycle,
            operations: operations.remove(&id).unwrap_or_default(),
            limits: limits.remove(&id).unwrap_or_default(),
        })
        .collect();
    let version = hex::encode(Sha256::digest(serde_json::to_vec(&devices).map_err(|e| Error::internal(e.to_string()))?));
    Ok((devices, version))
}

/// Exchanges everything with the gateway `device`.
pub async fn exchange(k: &Kernel, tenant: Uuid, device: Uuid, req: SyncRequest) -> Result<SyncResponse> {
    if req.contract != VERSION {
        return Err(Error::Conflict(format!("this platform speaks contract {VERSION} and the site {}: upgrade before exchanging work", req.contract)));
    }
    if req.reports.len() > MAX_REPORTS || req.facts.len() > MAX_FACTS || req.measurements.len() > MAX_MEASUREMENTS {
        return Err(Error::invalid(format!("one exchange takes at most {MAX_REPORTS} reports, {MAX_FACTS} facts and {MAX_MEASUREMENTS} measurements")));
    }
    if let Some(bad) = req.facts.iter().find(|f| !valid_kind(&f.kind)) {
        return Err(Error::field("facts", format!("{} is not a kind", bad.kind)));
    }
    let now = Utc::now();
    if !req.measurements.is_empty() {
        telemetry::ensure_partitions(k, now).await?;
    }
    let mut tx = k.db.tenant_tx(tenant).await?;
    let gateway = devices::target(&mut tx, tenant, device).await?;
    if !gateway.lifecycle.may_connect() {
        return Err(Error::Forbidden(format!("this gateway is {}", gateway.lifecycle.as_str())));
    }
    let (current, pooled): (Option<i64>, bool) =
        sqlx::query_as::<_, (i64, bool)>("select epoch, pooled from op_authority where tenant_id = $1 and domain = $2 and site_id = $3")
            .bind(tenant)
            .bind(&gateway.domain)
            .bind(gateway.site)
            .fetch_optional(&mut *tx)
            .await?
            .map_or((None, false), |(epoch, pooled)| (Some(epoch), pooled));
    // A pooled domain's gateways all work under its epoch, each for the devices connected to it.
    let held: Option<(String, i64)> = if pooled {
        current.map(|epoch| (gateway.domain.clone(), epoch))
    } else {
        sqlx::query_as("select domain, epoch from op_authority where tenant_id = $1 and holder = $2 for update")
            .bind(tenant)
            .bind(device)
            .fetch_optional(&mut *tx)
            .await?
    };
    let fenced = match &held {
        Some((_, epoch)) => req.epoch != 0 && req.epoch != *epoch,
        None => true,
    };

    let mut reports = req.reports;
    reports.sort_by_key(|r| (r.command_id, r.at_source));
    let (mut done, mut refused) = (Vec::new(), Vec::new());
    for r in &reports {
        // Reports from a holder that was replaced join the history and change nothing, however true
        // they may be. A person settles those commands.
        match commands::apply(&mut tx, tenant, r, current.unwrap_or(0), device).await? {
            Applied::Done(_) => done.push(r.report_id),
            Applied::Refused(why) => refused.push((r.report_id, why)),
        }
    }
    let mut facts_done = Vec::new();
    for f in &req.facts {
        apply_fact_from(&mut tx, tenant, gateway.site, Some(device), f).await?;
        facts_done.push(f.fact_id);
    }
    // Past a workspace's allowance measurements stay at the gateway, within its own budget, until
    // there is room. Everything else goes on: a lease or a command's report never runs out.
    let kept_back = !req.measurements.is_empty() && telemetry::over_allowance(&mut tx, tenant, now).await?;
    let (stored, skipped) = if kept_back { (0, 0) } else { telemetry::ingest(&mut tx, tenant, gateway.site, req.measurements, now).await? };
    if skipped > 0 {
        tracing::warn!(%tenant, %device, skipped, "measurements left out: another site's, or not storable");
    }
    if stored > 0 {
        // Whoever is looking at this site asks again at once. It goes at commit, so nobody hears
        // of what did not land.
        let said = json!({"tenant": tenant, "entity": "Site", "id": gateway.site, "event": "ops.measurements"}).to_string();
        sqlx::query("select pg_notify($1, $2)").bind(LIVE_CHANNEL).bind(said).execute(&mut *tx).await?;
    }

    sqlx::query(
        "insert into op_gateway (tenant_id, device_id, boot, last_seen_at, contract, config_version, health) values ($1, $2, $3, now(), $4, $5, $6) \
         on conflict (tenant_id, device_id) do update set boot = excluded.boot, last_seen_at = now(), contract = excluded.contract, \
         config_version = excluded.config_version, health = excluded.health",
    )
    .bind(tenant)
    .bind(device)
    .bind(req.boot)
    .bind(req.contract as i32)
    .bind(&req.config_version)
    .bind(serde_json::to_value(&req.health).map_err(|e| Error::internal(e.to_string()))?)
    .execute(&mut *tx)
    .await?;

    let (mut lease, mut commands_out, mut cancel, mut settled) = (None, vec![], vec![], vec![]);
    let (mut devices_out, mut config_version, mut operators) = (None, String::new(), None);
    if let Some((domain, epoch)) = held.as_ref().filter(|_| !fenced) {
        let max_hours: Option<i64> = sqlx::query_scalar("select max_offline_hours from e_site where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(gateway.site)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
        lease = Some(Lease { epoch: *epoch, valid_until: now + Duration::hours(max_hours.unwrap_or(72)) });
        commands::confirm_held(&mut tx, tenant, domain, &req.held).await?;
        if gateway.lifecycle.takes_commands() && !k.holding().await? && k.outward(tenant).await.is_ok() {
            commands_out = commands::take_for_site(&mut tx, tenant, domain, *epoch, i64::from(req.want), device, pooled).await?;
        }
        cancel = commands::cancels(&mut tx, tenant, domain).await?;
        settled = commands::settled(&mut tx, tenant, domain, &req.in_doubt).await?;
        let (devices, version) = site_devices(&mut tx, tenant, domain, gateway.site).await?;
        let (ops, ops_version) = operators_for(&mut tx, tenant, gateway.site).await?;
        config_version = format!("{version}.{ops_version}");
        if req.config_version != config_version {
            devices_out = Some(devices);
            operators = Some(ops);
        }
    }
    tx.commit().await?;
    if stored > 0 {
        telemetry::meter(k, tenant, now, stored).await;
    }
    Ok(SyncResponse {
        contract: VERSION,
        server_time: Utc::now(),
        lease,
        fenced,
        commands: commands_out,
        cancel,
        settled,
        reports_done: done,
        reports_refused: refused,
        facts_done,
        measurements_stored: stored,
        measurements_held: kept_back,
        devices: devices_out,
        config_version,
        operators,
    })
}

/// People who may work at this site's console with the link down. Only those with a PIN set, and
/// only for as long as they are active: the list is replaced at each change.
async fn operators_for(tx: &mut Tx, tenant: Uuid, site: Uuid) -> Result<(Vec<Operator>, String)> {
    let rows: Vec<(Uuid, String, String, Vec<String>)> = sqlx::query_as(
        "select u.id, coalesce(u.full_name, u.email), p.pin_hash, array(select r.role from user_role r where r.tenant_id = u.tenant_id and r.user_id = u.id and r.role like 'Ops %' and (r.expires_at is null or r.expires_at > now())) \
         from op_operator_pin p join app_user u on u.tenant_id = p.tenant_id and u.id = p.user_id \
         where p.tenant_id = $1 and p.site_id = $2 and u.is_active order by u.id",
    )
    .bind(tenant)
    .bind(site)
    .fetch_all(&mut **tx)
    .await?;
    let operators: Vec<Operator> = rows
        .into_iter()
        .map(|(user, name, pin_hash, roles)| {
            let mut may = Vec::new();
            if roles.iter().any(|r| r == "Ops Dispatcher" || r == "Ops Manager") {
                may.push("dispatch".to_string());
            }
            if roles.iter().any(|r| r == "Ops Recovery" || r == "Ops Manager") {
                may.push("recover".to_string());
            }
            Operator { user, name, pin_hash, may }
        })
        .collect();
    let version = hex::encode(Sha256::digest(serde_json::to_vec(&operators).map_err(|e| Error::internal(e.to_string()))?));
    Ok((operators, version))
}
