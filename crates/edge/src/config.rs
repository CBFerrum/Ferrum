//! What a gateway should be: worked out from the records each time it asks, and versioned by its
//! content, so an unchanged configuration costs one short answer.

use chrono::{DateTime, Utc};
use ferrum_kernel::{Result, Tx};
use ferrum_ops::contract::edge::{Comparison, DeviceLink, EdgeConfig, Export, Release, Rule};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

/// A decimal column, read as text so that nothing is rounded on the way.
fn number(text: Option<&str>) -> Option<f64> {
    text.and_then(|t| t.parse().ok())
}

#[derive(sqlx::FromRow)]
struct DeviceRow {
    device_code: String,
    definition_name: String,
    published_body: String,
    edge_connect: Option<String>,
    edge_settings: Option<Value>,
    edge_concurrency: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: Uuid,
    rule_name: String,
    metric: String,
    comparison: Option<String>,
    limit_value: Option<String>,
    clear_value: Option<String>,
    hold_secs: Option<i64>,
    severity: Option<String>,
    device_code: Option<String>,
    definition: Option<Uuid>,
}

#[derive(sqlx::FromRow)]
struct ExportRow {
    export_name: String,
    send_to: String,
    topic: Option<String>,
    only: Option<String>,
    sign_with: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ReleaseRow {
    version: String,
    url: String,
    sha256: String,
    signature: String,
    expires_at: DateTime<Utc>,
    percent: Option<String>,
}

/// Which share of gateways a gateway is in, the same each time: a rollout at 10% that goes to 20%
/// keeps the first 10%.
fn bucket(gateway: Uuid) -> f64 {
    f64::from(Sha256::digest(gateway.as_bytes())[0]) / 255.0 * 100.0
}

/// What `gateway` should be. Only what it reaches, and only what it needs.
pub async fn for_gateway(tx: &mut Tx, tenant: Uuid, gateway: Uuid, site: Option<Uuid>) -> Result<EdgeConfig> {
    let rows: Vec<DeviceRow> = sqlx::query_as(
        "select d.device_code, f.definition_name, f.published_body, d.edge_connect, d.edge_settings, d.edge_concurrency \
         from e_device d join e_device_definition f on f.tenant_id = d.tenant_id and f.id = d.edge_definition \
         where d.tenant_id = $1 and d.gateway = $2 and d.lifecycle not in ('Retired', 'Quarantined') and f.published_body is not null order by d.device_code",
    )
    .bind(tenant)
    .bind(gateway)
    .fetch_all(&mut **tx)
    .await?;
    let mut definitions = BTreeMap::new();
    let mut devices = Vec::new();
    for r in rows {
        definitions.insert(r.definition_name.clone(), r.published_body);
        devices.push(DeviceLink {
            code: r.device_code,
            definition: r.definition_name,
            connect: r.edge_connect.filter(|c| !c.trim().is_empty()),
            settings: r.edge_settings.unwrap_or(Value::Null),
            concurrency: r.edge_concurrency.unwrap_or(1).clamp(1, 100) as u32,
        });
    }
    let by_definition: BTreeMap<Uuid, Vec<String>> = {
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("select edge_definition, device_code from e_device where tenant_id = $1 and gateway = $2 and edge_definition is not null")
                .bind(tenant)
                .bind(gateway)
                .fetch_all(&mut **tx)
                .await?;
        let mut map: BTreeMap<Uuid, Vec<String>> = BTreeMap::new();
        for (definition, code) in rows {
            map.entry(definition).or_default().push(code);
        }
        map
    };
    let rules: Vec<RuleRow> = sqlx::query_as(
        "select r.id, r.rule_name, r.metric, r.comparison, r.limit_value::text as limit_value, r.clear_value::text as clear_value, r.hold_secs, r.severity, d.device_code, r.definition \
         from e_edge_rule r left join e_device d on d.tenant_id = r.tenant_id and d.id = r.device \
         where r.tenant_id = $1 and coalesce(r.enabled, true) and (r.site is null or r.site = $2) order by r.id",
    )
    .bind(tenant)
    .bind(site)
    .fetch_all(&mut **tx)
    .await?;
    let rules = rules
        .into_iter()
        .filter_map(|r| {
            let when = match r.comparison.as_deref() {
                Some("Below") => Comparison::Below,
                Some("Silent") => Comparison::Silent,
                _ => Comparison::Above,
            };
            let mut names: Vec<String> = r.device_code.into_iter().collect();
            if let Some(definition) = r.definition {
                // A rule for a definition no device here uses has nothing to watch.
                names.extend(by_definition.get(&definition)?.iter().cloned());
            }
            Some(Rule {
                id: r.id,
                name: r.rule_name,
                devices: names,
                metric: r.metric,
                when,
                value: number(r.limit_value.as_deref()).unwrap_or(0.0),
                hold_secs: r.hold_secs.unwrap_or(0),
                clear: number(r.clear_value.as_deref()),
                severity: r.severity.unwrap_or_else(|| "Warning".into()),
            })
        })
        .collect();
    let exports: Vec<ExportRow> = sqlx::query_as(
        "select export_name, send_to, topic, metrics as only, sign_with from e_edge_export where tenant_id = $1 and coalesce(enabled, true) and (site is null or site = $2) order by export_name",
    )
    .bind(tenant)
    .bind(site)
    .fetch_all(&mut **tx)
    .await?;
    let exports = exports
        .into_iter()
        .map(|e| Export {
            name: e.export_name,
            to: e.send_to,
            topic: e.topic.filter(|t| !t.is_empty()),
            only: e.only.unwrap_or_default().split(',').map(|o| o.trim().to_string()).filter(|o| !o.is_empty()).collect(),
            sign_with: e.sign_with.filter(|s| !s.is_empty()),
        })
        .collect();
    let release: Option<ReleaseRow> = sqlx::query_as(
        "select release_version as version, url, sha256, signature, expires_at, percent::text as percent from e_edge_release \
         where tenant_id = $1 and status = 'Rolling Out' and (site is null or site = $2) and expires_at > now() order by created_at desc limit 1",
    )
    .bind(tenant)
    .bind(site)
    .fetch_optional(&mut **tx)
    .await?;
    let release = release
        .filter(|r| number(r.percent.as_deref()).unwrap_or(0.0) > bucket(gateway) || number(r.percent.as_deref()) == Some(100.0))
        .map(|r| Release { version: r.version, url: r.url, sha256: r.sha256, signature: r.signature, expires_at: r.expires_at });
    let mut config = EdgeConfig { version: String::new(), definitions, devices, rules, exports, release };
    config.version = hex::encode(Sha256::digest(serde_json::to_vec(&config).unwrap_or_default()));
    Ok(config)
}
