//! Connectors: what links a workspace to an outside system, kept apart from the apps whose workflows
//! use it. An app owns a workflow and its records; a connector translates a provider's model,
//! credentials, events and limits into them, so that a provider's change does not mean rewriting
//! the app.
//!
//! This is what every connector shares. A provider implements [`Connector`] and describes what its
//! API really offers ([`ConnectorInfo`]), not a lowest common denominator; the rest is here:
//!
//! - **Connections**: one per workspace and account, with credentials sealed apart from the record
//!   and never read back, and a test that proves them.
//! - **Mapping**: which stream lands in which entity, under which field names and rules.
//! - **Synchronization**: a job per run, resuming from its cursor, a page at a time. External ids are
//!   kept in `connection_link`; each record is one transaction, so a failure loses nothing and
//!   repeating a page changes nothing.
//! - **Delivery**: a local change is a job with a key that makes it one change however often it is
//!   retried, with backoff, a dead letter when it gives up, and replay.
//! - **Webhooks**: authenticated by the connector, deduplicated by event id, and answered at once:
//!   the work happens in a job.
//! - **Reconciliation**: compares what the provider has with what we hold, and records drift for a
//!   person to look at rather than repairing it by guesswork.
//! - **Operations**: authentication, freshness, backlog, drift and quota, each told apart: a
//!   connection is never just "connected".
//! - **Being a good client**: a provider that says to wait is left alone, and one that keeps failing
//!   is left alone for longer. Every job of the connection waits together, at no cost in attempts.
//!
//! A connector is compiled in, or comes with a workspace's plugin (`crates/plugins`): the contract is
//! the same, and only [`ConnectorSource`] knows the difference.
//!
//! A connector is finished only with evidence against the real provider or its sandbox. Passing here
//! with a stand-in proves this machinery, and nothing about any provider.

use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob, enqueue};
use crate::outbox::{Event, PRIORITY_BULK};
use crate::{Doc, Kernel, audit};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub const SYNC_KIND: &str = "platform.connector.sync";
pub const DELIVER_KIND: &str = "platform.connector.deliver";
pub const RECONCILE_KIND: &str = "platform.connector.reconcile";
/// Attempts at one outgoing change before it is a dead letter (`GET /api/jobs`, retried from there).
const DELIVERY_ATTEMPTS: i32 = 8;
/// Webhook events remembered to tell a repeat from a new one.
const SEEN_EVENT_DAYS: i32 = 30;
/// Differences one reconciliation records.
const MAX_DRIFT: usize = 1000;
/// Failures in a row before a provider is left alone.
const BREAK_AFTER: i32 = 3;
/// The longest a provider is left alone on our own account. It may ask for longer.
const MAX_PAUSE_SECS: u64 = 3600;
/// How long to wait when a provider says "too many" and not for how long.
const TOO_MANY_SECS: u64 = 60;

/// What a provider's API offers, as it is: no connector pretends to more.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectorInfo {
    pub provider: String,
    pub label: String,
    /// `payments`, `calendar`, `marketplace` ...
    pub family: String,
    /// The API version this connector was written against.
    pub api_version: String,
    /// Streams `pull` reads.
    pub streams: Vec<String>,
    /// Whether `push` writes, and of which entities.
    pub writes: Vec<String>,
    pub webhooks: bool,
    /// What it takes to use: partner approval, certification, a contract. Empty when an account will do.
    pub requirements: String,
    /// What this account can really do (`publish_text`, `read_comments`, `insights`), each named, so
    /// that nothing promises what a provider grants only some accounts.
    pub capabilities: Vec<String>,
}

/// A stream's shape at the provider, for mapping it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSchema {
    pub stream: String,
    /// Field names, including what the account has customized.
    pub fields: Vec<String>,
}

/// A connection as a connector sees it: its settings and credentials, for the length of a call.
#[derive(Clone)]
pub struct Connection {
    pub id: Uuid,
    pub tenant: Uuid,
    pub provider: String,
    pub config: Value,
    pub credentials: Value,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").field("id", &self.id).field("provider", &self.provider).finish_non_exhaustive()
    }
}

/// One record from outside.
#[derive(Debug, Clone, Default)]
pub struct Incoming {
    pub external_id: String,
    /// The provider's version of it (an updated-at, an etag), when it has one.
    pub version: Option<String>,
    /// Gone at the provider.
    pub deleted: bool,
    pub fields: serde_json::Map<String, Value>,
}

/// A page of a stream.
#[derive(Debug, Clone, Default)]
pub struct Pulled {
    pub records: Vec<Incoming>,
    /// Where to carry on; `None` when this was the last page.
    pub next: Option<String>,
}

/// A local change going out.
#[derive(Debug, Clone)]
pub struct Outgoing {
    /// The same for every attempt at this change: providers that take an idempotency key get it.
    pub key: String,
    pub entity: String,
    pub record: Uuid,
    pub external_id: Option<String>,
    pub fields: serde_json::Map<String, Value>,
}

/// An event a webhook carried.
#[derive(Debug, Clone)]
pub struct Hook {
    pub event_id: String,
    pub stream: String,
    pub record: Incoming,
}

/// Connectors that come with a workspace's plugins, found by provider.
#[async_trait]
pub trait ConnectorSource: Send + Sync {
    async fn find(&self, kernel: &Kernel, tenant: Uuid, provider: &str) -> Result<Option<Arc<dyn Connector>>>;
    async fn list(&self, kernel: &Kernel, tenant: Uuid) -> Result<Vec<ConnectorInfo>>;
}

/// A provider. Everything it does is outside any transaction.
#[async_trait]
pub trait Connector: Send + Sync {
    fn info(&self) -> ConnectorInfo;

    /// Proves the credentials work, without changing anything there.
    async fn check(&self, k: &Kernel, c: &Connection) -> Result<String>;

    /// What each stream holds in this account, for mapping it. Without one, its streams, unnamed.
    async fn discover(&self, _k: &Kernel, _c: &Connection) -> Result<Vec<StreamSchema>> {
        Ok(self.info().streams.into_iter().map(|stream| StreamSchema { stream, fields: vec![] }).collect())
    }

    /// A page of a stream's changes after `cursor`, or from the beginning. `Error::Wait` says how
    /// long the provider wants left alone; `TooManyRequests` and `Unavailable` mean soon.
    async fn pull(&self, k: &Kernel, c: &Connection, stream: &str, cursor: Option<&str>) -> Result<Pulled>;

    /// Sends one change; answers the provider's id for the record.
    async fn push(&self, _k: &Kernel, _c: &Connection, _change: &Outgoing) -> Result<String> {
        Err(Error::invalid("this provider's API does not take changes"))
    }

    /// Authenticates a webhook (signature, freshness) and says what it carried. Refuses anything it
    /// cannot prove came from the provider.
    async fn webhook(&self, _k: &Kernel, _c: &Connection, _headers: &BTreeMap<String, String>, _body: &[u8]) -> Result<Vec<Hook>> {
        Err(Error::invalid("this provider sends no webhooks"))
    }
}

/// How one stream lands in the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamMap {
    pub entity: String,
    /// External field name → local field name. Only these are read in.
    pub fields: BTreeMap<String, String>,
    /// Values a local field takes, by the provider's: an enum's translation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, BTreeMap<String, Value>>,
    /// When both sides changed: `source` (the provider wins), `local` (ours stays), `review` (ours
    /// stays, and a person is told).
    #[serde(default = "default_conflict")]
    pub conflict: String,
    /// What a record gone at the provider does here: `keep`, or `delete`.
    #[serde(default = "default_tombstone")]
    pub tombstone: String,
    /// Local changes go out to the provider.
    #[serde(default)]
    pub push: bool,
    /// For a stream that sends people to a marketing service: the field holding the person's address,
    /// whose marketing consent each record sent asks first (FEO-015). One who withdrew is not sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent: Option<String>,
}

fn default_conflict() -> String {
    "review".into()
}

fn default_tombstone() -> String {
    "keep".into()
}

/// What an administrator sets up.
#[derive(Debug, Clone, Deserialize)]
pub struct ConnectionInput {
    pub provider: String,
    pub name: String,
    #[serde(default)]
    pub config: Value,
    /// `None` keeps what is stored.
    #[serde(default)]
    pub credentials: Option<Value>,
    #[serde(default)]
    pub mapping: BTreeMap<String, StreamMap>,
}

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("connections are for administrators".into())) }
}

pub(crate) fn sealed_label(tenant: Uuid, connection: Uuid) -> String {
    format!("connection:{tenant}:{connection}")
}

/// What a record's mapped fields come to, to tell a real change from a repeat.
fn digest(fields: &serde_json::Map<String, Value>) -> String {
    let sorted: BTreeMap<&String, &Value> = fields.iter().collect();
    hex::encode(Sha256::digest(serde_json::to_vec(&sorted).unwrap_or_default()))
}

/// Translates one external record's fields by a stream's map.
pub fn mapped(map: &StreamMap, fields: &serde_json::Map<String, Value>) -> Doc {
    let mut doc = Doc::new();
    for (external, local) in &map.fields {
        let Some(value) = fields.get(external) else { continue };
        let value = value.as_str().and_then(|v| map.values.get(local)?.get(v).cloned()).unwrap_or_else(|| value.clone());
        doc.insert(local.clone(), value);
    }
    doc
}

/// How long to leave a provider alone after `failures` failures in a row: not at all for the first
/// few, then doubling up to an hour.
fn pause_after(failures: i32) -> Option<u64> {
    (failures >= BREAK_AFTER).then(|| (30u64 << (failures - BREAK_AFTER).min(10)).min(MAX_PAUSE_SECS))
}

/// Whether a connection's credentials are known to work, which is one thing a connection can be.
fn authentication(c: &Value) -> &'static str {
    if c["has_credentials"] != json!(true) {
        "none"
    } else if c["last_checked_at"].is_null() {
        "untested"
    } else if c["enabled"] == json!(true) {
        "ok"
    } else {
        "failing"
    }
}

#[derive(Debug, sqlx::FromRow)]
struct Row {
    id: Uuid,
    provider: String,
    config: Value,
    mapping: Value,
    sealed: Option<String>,
    enabled: bool,
}

struct Link {
    record_id: Uuid,
    hash: Option<String>,
    local_version: Option<i32>,
}

/// What one record's arrival did.
#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Created,
    Updated,
    Unchanged,
    Deleted,
    Conflict,
}

impl Kernel {
    /// Across workspaces, for monitoring: connections that are on, failing, waiting on their
    /// provider, with no success for a day, and differences waiting for a person.
    pub async fn connection_gauges(&self) -> Result<(i64, i64, i64, i64, i64)> {
        let mut tx = self.db.system_tx().await?;
        let gauges = sqlx::query_as(
            "select count(*) filter (where enabled), count(*) filter (where enabled and failures > 0), \
             count(*) filter (where enabled and waiting_until > now()), \
             count(*) filter (where enabled and coalesce(last_success_at, created_at) < now() - interval '1 day'), \
             (select count(*) from connection_drift where resolved_at is null) from connection",
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(gauges)
    }

    async fn connector(&self, tenant: Uuid, provider: &str) -> Result<Arc<dyn Connector>> {
        if let Some(c) = self.hooks.connector(provider) {
            return Ok(c.clone());
        }
        if let Some(source) = self.hooks.connector_source()
            && let Some(c) = source.find(self, tenant, provider).await?
        {
            return Ok(c);
        }
        Err(Error::invalid(format!("no connector for {provider} here")))
    }

    /// The providers this workspace has connectors for, as they describe themselves.
    pub async fn connectors(&self, ctx: &Ctx) -> Result<Vec<ConnectorInfo>> {
        admin_only(ctx)?;
        let mut all: Vec<ConnectorInfo> = self.hooks.connectors().map(|c| c.info()).collect();
        if let Some(source) = self.hooks.connector_source() {
            all.extend(source.list(self, ctx.tenant).await?);
        }
        Ok(all)
    }

    /// What a provider holds for this account, for mapping it.
    pub async fn connection_schema(&self, ctx: &Ctx, id: Uuid) -> Result<Vec<StreamSchema>> {
        admin_only(ctx)?;
        let row = self.row(ctx.tenant, id).await?;
        let connection = self.opened(ctx.tenant, &row)?;
        let connector = self.connector(ctx.tenant, &row.provider).await?;
        self.calling(ctx.tenant, id, connector.discover(self, &connection)).await
    }

    /// A call to a provider, unless it is being left alone, and what came of it remembered. Every
    /// job of a connection goes through here, so they all wait together.
    async fn calling<T>(&self, tenant: Uuid, connection: Uuid, call: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let waiting: Option<(f64, Option<String>)> = sqlx::query_as(
            "select extract(epoch from waiting_until - now())::float8, waiting_reason from connection \
             where tenant_id = $1 and id = $2 and waiting_until > now()",
        )
        .bind(tenant)
        .bind(connection)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        if let Some((seconds, why)) = waiting {
            return Err(Error::Wait(why.unwrap_or_else(|| "the provider is being left alone for a while".into()), seconds.ceil() as u64 + 1));
        }
        let outcome = call.await;
        match outcome {
            Ok(v) => {
                let mut tx = self.db.system_tenant_tx(tenant).await?;
                sqlx::query(
                    "update connection set failures = 0, waiting_until = null, waiting_reason = null where tenant_id = $1 and id = $2 and (failures <> 0 or waiting_until is not null)",
                )
                .bind(tenant)
                .bind(connection)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(v)
            }
            Err(Error::Wait(why, seconds)) => {
                self.leave_alone(tenant, connection, &why, seconds).await?;
                Err(Error::Wait(why, seconds))
            }
            Err(Error::TooManyRequests(why)) => {
                self.leave_alone(tenant, connection, &why, TOO_MANY_SECS).await?;
                Err(Error::Wait(why, TOO_MANY_SECS))
            }
            Err(Error::Unavailable(why)) => {
                let mut tx = self.db.system_tenant_tx(tenant).await?;
                let failures: i32 = sqlx::query_scalar("update connection set failures = failures + 1 where tenant_id = $1 and id = $2 returning failures")
                    .bind(tenant)
                    .bind(connection)
                    .fetch_one(&mut *tx)
                    .await?;
                tx.commit().await?;
                if let Some(seconds) = pause_after(failures) {
                    self.leave_alone(tenant, connection, &format!("{why} ({failures} failures in a row)"), seconds).await?;
                }
                // A call that failed costs an attempt, so what can never succeed still ends as a dead
                // letter. Only waiting for the pause is free.
                Err(Error::Unavailable(why))
            }
            // The provider answered: it refused this, and is working.
            Err(e) => Err(e),
        }
    }

    /// Leaves a connection alone for a while, and every other connection on the same provider account with it,
    /// in any workspace: a provider's limit is the account's, not one connection's (FEO-067).
    async fn leave_alone(&self, tenant: Uuid, connection: Uuid, why: &str, seconds: u64) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        sqlx::query(
            "update connection set waiting_until = greatest(waiting_until, now() + make_interval(secs => $3)), waiting_reason = $4 \
             where (tenant_id = $1 and id = $2) or account_key = (select account_key from connection where tenant_id = $1 and id = $2)",
        )
        .bind(tenant)
        .bind(connection)
        .bind(seconds as f64)
        .bind(why)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    fn seal(&self, tenant: Uuid, id: Uuid, credentials: &Value) -> Result<String> {
        let key = self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("connections need a configured secret key (FERRUM_SECRET_KEY)".into()))?;
        key.seal(&sealed_label(tenant, id), &credentials.to_string())
    }

    async fn row(&self, tenant: Uuid, id: Uuid) -> Result<Row> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let row: Option<Row> = sqlx::query_as("select id, provider, config, mapping, sealed, enabled from connection where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        row.ok_or_else(|| Error::NotFound("connection".into()))
    }

    fn opened(&self, tenant: Uuid, row: &Row) -> Result<Connection> {
        let credentials = match &row.sealed {
            Some(sealed) => serde_json::from_str(&self.open_sealed(&sealed_label(tenant, row.id), sealed)?).map_err(Error::internal)?,
            None => Value::Null,
        };
        Ok(Connection { id: row.id, tenant, provider: row.provider.clone(), config: row.config.clone(), credentials })
    }

    /// Adds a connection, or changes one. It starts disabled until it has been tested.
    pub async fn save_connection(&self, ctx: &Ctx, id: Option<Uuid>, input: ConnectionInput) -> Result<Uuid> {
        admin_only(ctx)?;
        let name = input.name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(Error::field("name", "give it a name of at most 100 characters"));
        }
        let info = self.connector(ctx.tenant, &input.provider).await?.info();
        for (stream, map) in &input.mapping {
            if !info.streams.contains(stream) {
                return Err(Error::field("mapping", format!("{} has no stream {stream}", info.label)));
            }
            let ent = self.entity(ctx, &map.entity)?;
            if let Some(f) = map.fields.values().chain(map.consent.iter()).find(|f| ent.field(f).is_none()) {
                return Err(Error::field("mapping", format!("{} has no field {f}", map.entity)));
            }
            if !matches!(map.conflict.as_str(), "source" | "local" | "review") || !matches!(map.tombstone.as_str(), "keep" | "delete") {
                return Err(Error::field("mapping", "conflict is source, local or review; tombstone is keep or delete"));
            }
            if map.push && !info.writes.contains(&map.entity) {
                return Err(Error::field("mapping", format!("{} does not take changes to {}", info.label, map.entity)));
            }
        }
        let connection = id.unwrap_or_else(Uuid::now_v7);
        let sealed = input.credentials.as_ref().map(|c| self.seal(ctx.tenant, connection, c)).transpose()?;
        // The account they open, known by a fingerprint: connections on one account wait together (FEO-067).
        let account_key = match (&input.credentials, self.settings.secret_key.as_ref()) {
            (Some(c), Some(key)) => {
                Some(key.fingerprint("connection account", &format!("{}:{}", input.provider, String::from_utf8_lossy(&crate::value::canonical_json(c)))))
            }
            _ => None,
        };
        let mapping = serde_json::to_value(&input.mapping).map_err(Error::internal)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query(
            "insert into connection (tenant_id, id, provider, name, config, mapping, sealed, created_by, account_key) values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             on conflict (tenant_id, id) do update set name = excluded.name, config = excluded.config, mapping = excluded.mapping, \
             sealed = coalesce(excluded.sealed, connection.sealed), account_key = coalesce(excluded.account_key, connection.account_key), enabled = false \
             where connection.provider = excluded.provider",
        )
        .bind(ctx.tenant)
        .bind(connection)
        .bind(&input.provider)
        .bind(name)
        .bind(if input.config.is_null() { json!({}) } else { input.config.clone() })
        .bind(mapping)
        .bind(sealed)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .bind(&account_key)
        .execute(&mut *tx)
        .await?;
        if n.rows_affected() == 0 {
            return Err(Error::invalid("a connection cannot change provider"));
        }
        audit::record_admin(
            &mut tx,
            ctx,
            "Connection",
            connection,
            "save",
            None,
            json!({"provider": input.provider, "name": name, "credentials": input.credentials.is_some()}),
        )
        .await?;
        tx.commit().await?;
        Ok(connection)
    }

    /// Proves a connection works, and enables it if so.
    pub async fn test_connection(&self, ctx: &Ctx, id: Uuid) -> Result<String> {
        admin_only(ctx)?;
        let row = self.row(ctx.tenant, id).await?;
        let connection = self.opened(ctx.tenant, &row)?;
        let connector = self.connector(ctx.tenant, &row.provider).await?;
        let outcome = connector.check(self, &connection).await;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("update connection set enabled = $3, last_error = $4, last_checked_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .bind(outcome.is_ok())
            .bind(outcome.as_ref().err().map(ToString::to_string))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        outcome
    }

    /// Connections and how they are doing, with no credential in sight.
    pub async fn connections(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', c.id, 'provider', c.provider, 'name', c.name, 'enabled', c.enabled, 'config', c.config, 'mapping', c.mapping, \
             'has_credentials', c.sealed is not null, 'failures', c.failures, 'waiting_until', c.waiting_until, 'waiting_reason', c.waiting_reason, \
             'last_success_at', c.last_success_at, 'last_checked_at', c.last_checked_at, 'last_error', c.last_error, 'cursors', c.cursors, \
             'linked', (select count(*) from connection_link l where l.tenant_id = c.tenant_id and l.connection_id = c.id), \
             'drift', (select count(*) from connection_drift d where d.tenant_id = c.tenant_id and d.connection_id = c.id and d.resolved_at is null), \
             'backlog', (select count(*) from job j where j.tenant_id = c.tenant_id and j.status in ('queued', 'running') and j.kind like 'platform.connector.%' and j.payload->>'connection' = c.id::text), \
             'dead_letters', (select count(*) from job j where j.tenant_id = c.tenant_id and j.status = 'failed' and j.kind = $2 and j.payload->>'connection' = c.id::text)) \
             from connection c where c.tenant_id = $1 order by c.created_at",
        )
        .bind(ctx.tenant)
        .bind(DELIVER_KIND)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        // Told apart, because each goes wrong on its own: a working token on a stale feed is not
        // a healthy connection, and an upstream outage is not ours.
        Ok(rows
            .into_iter()
            .map(|mut c| {
                c["health"] = json!({
                    "authentication": authentication(&c),
                    "freshness": c["last_success_at"],
                    "backlog": c["backlog"],
                    "dead_letters": c["dead_letters"],
                    "drift": c["drift"],
                    "quota": {"waiting_until": c["waiting_until"], "reason": c["waiting_reason"], "failures": c["failures"]},
                });
                c
            })
            .collect())
    }

    /// Stops a connection and forgets its credentials. The links stay: reconnecting finds what it knew.
    pub async fn disconnect(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("update connection set enabled = false, sealed = null where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("connection".into()));
        }
        audit::record_admin(&mut tx, ctx, "Connection", id, "disconnect", None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Queues a run of a connection's streams from where the last one stopped (`from_start` reads
    /// everything again: repeats change nothing).
    pub async fn sync_connection(&self, ctx: &Ctx, id: Uuid, from_start: bool) -> Result<Uuid> {
        admin_only(ctx)?;
        let row = self.row(ctx.tenant, id).await?;
        if !row.enabled {
            return Err(Error::invalid("test this connection first"));
        }
        let mut job = NewJob::new(SYNC_KIND, json!({"connection": id, "from_start": from_start}));
        job.priority = PRIORITY_BULK;
        job.dedupe_key = Some(format!("sync-{id}"));
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let id = enqueue(&mut tx, ctx.tenant, Some(ctx.user), job).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Queues a comparison of what the provider has with what we hold.
    pub async fn reconcile_connection(&self, ctx: &Ctx, id: Uuid) -> Result<Uuid> {
        admin_only(ctx)?;
        self.row(ctx.tenant, id).await?;
        let mut job = NewJob::new(RECONCILE_KIND, json!({"connection": id}));
        job.priority = PRIORITY_BULK;
        job.dedupe_key = Some(format!("reconcile-{id}"));
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let id = enqueue(&mut tx, ctx.tenant, Some(ctx.user), job).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// What a connection did lately, newest first (its reads, sends and comparisons, each with how
    /// it went, so a failed one can be tried again with `retry_job`), and how many records each of
    /// its streams links.
    pub async fn connection_activity(&self, ctx: &Ctx, id: Uuid, limit: i64) -> Result<Value> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let known: bool = sqlx::query_scalar("select exists (select 1 from connection where tenant_id = $1 and id = $2)")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        if !known {
            return Err(Error::NotFound("connection".into()));
        }
        let runs: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'kind', kind, 'status', status, 'attempts', attempts, 'max_attempts', max_attempts, 'last_error', last_error, \
             'result', result, 'stream', payload->>'stream', 'record', payload->>'record', 'created_at', created_at, 'finished_at', finished_at) \
             from job where tenant_id = $1 and kind like 'platform.connector.%' and payload->>'connection' = $2::text order by created_at desc, id desc limit $3",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(limit.clamp(1, 200))
        .fetch_all(&mut *tx)
        .await?;
        let streams: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('stream', stream, 'entity', min(entity), 'records', count(*), 'last_synced', max(synced_at)) \
             from connection_link where tenant_id = $1 and connection_id = $2 group by stream order by stream",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(json!({"runs": runs, "streams": streams}))
    }

    /// What reconciliation or a conflict left for a person.
    pub async fn connection_drift(&self, ctx: &Ctx, id: Uuid) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar("select to_jsonb(d) - 'tenant_id' from connection_drift d where tenant_id = $1 and connection_id = $2 and resolved_at is null order by created_at limit 500").bind(ctx.tenant).bind(id).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows)
    }

    pub async fn resolve_drift(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("update connection_drift set resolved_at = now(), resolved_by = $3 where tenant_id = $1 and id = $2 and resolved_at is null")
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        if n == 0 { Err(Error::NotFound("difference".into())) } else { Ok(()) }
    }

    async fn note_drift(&self, tenant: Uuid, connection: Uuid, stream: &str, external_id: &str, kind: &str, detail: Value) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        sqlx::query(
            "insert into connection_drift (tenant_id, id, connection_id, stream, external_id, kind, detail) values ($1, $2, $3, $4, $5, $6, $7) \
             on conflict (tenant_id, connection_id, stream, external_id, kind) where resolved_at is null do update set detail = excluded.detail",
        )
        .bind(tenant)
        .bind(Uuid::now_v7())
        .bind(connection)
        .bind(stream)
        .bind(external_id)
        .bind(kind)
        .bind(detail)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn connection_link(&self, tx: &mut Tx, tenant: Uuid, connection: Uuid, stream: &str, external_id: &str) -> Result<Option<Link>> {
        let row: Option<(Uuid, Option<String>, Option<i32>)> = sqlx::query_as(
            "select record_id, hash, local_version from connection_link where tenant_id = $1 and connection_id = $2 and stream = $3 and external_id = $4",
        )
        .bind(tenant)
        .bind(connection)
        .bind(stream)
        .bind(external_id)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(row.map(|(record_id, hash, local_version)| Link { record_id, hash, local_version }))
    }

    /// One record from outside: its own transaction, so a failure loses nothing else, and a repeat
    /// (a replayed page, a webhook after a sync) changes nothing.
    async fn apply_incoming(&self, sys: &Ctx, connection: Uuid, stream: &str, map: &StreamMap, item: &Incoming) -> Result<Applied> {
        let doc = mapped(map, &item.fields);
        let hash = digest(&doc);
        let mut tx = self.db.system_tenant_tx(sys.tenant).await?;
        let link = self.connection_link(&mut tx, sys.tenant, connection, stream, &item.external_id).await?;
        let outcome = match (&link, item.deleted) {
            (None, true) => Applied::Unchanged,
            (Some(l), true) => {
                if map.tombstone == "delete" {
                    self.delete_tx(sys, &mut tx, &map.entity, l.record_id, None).await?;
                }
                sqlx::query("delete from connection_link where tenant_id = $1 and connection_id = $2 and stream = $3 and external_id = $4")
                    .bind(sys.tenant)
                    .bind(connection)
                    .bind(stream)
                    .bind(&item.external_id)
                    .execute(&mut *tx)
                    .await?;
                Applied::Deleted
            }
            (Some(l), false) if l.hash.as_deref() == Some(hash.as_str()) => Applied::Unchanged,
            (Some(l), false) => {
                let current = self.get_tx(sys, &mut tx, &map.entity, l.record_id, true).await?;
                let version = current.get("version").and_then(Value::as_i64).map(|v| v as i32);
                let changed_here = l.local_version.is_some() && version != l.local_version;
                if changed_here && map.conflict != "source" {
                    tx.rollback().await?;
                    if map.conflict == "review" {
                        self.note_drift(sys.tenant, connection, stream, &item.external_id, "conflict", json!({"record": l.record_id, "theirs": doc})).await?;
                    }
                    return Ok(Applied::Conflict);
                }
                let saved = self.update_tx(sys, &mut tx, &map.entity, l.record_id, doc.clone(), None).await?;
                self.save_link(&mut tx, sys.tenant, connection, stream, item, &map.entity, l.record_id, &hash, &saved).await?;
                Applied::Updated
            }
            (None, false) => {
                let saved = self.insert_tx(sys, &mut tx, &map.entity, doc.clone()).await?;
                let id =
                    saved.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).ok_or_else(|| Error::internal("a record came back without an id"))?;
                self.save_link(&mut tx, sys.tenant, connection, stream, item, &map.entity, id, &hash, &saved).await?;
                Applied::Created
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    async fn save_link(
        &self,
        tx: &mut Tx,
        tenant: Uuid,
        connection: Uuid,
        stream: &str,
        item: &Incoming,
        entity: &str,
        record: Uuid,
        hash: &str,
        saved: &Doc,
    ) -> Result<()> {
        sqlx::query(
            "insert into connection_link (tenant_id, connection_id, stream, external_id, entity, record_id, external_version, hash, local_version, synced_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, now()) \
             on conflict (tenant_id, connection_id, stream, external_id) do update set external_version = excluded.external_version, hash = excluded.hash, \
             local_version = excluded.local_version, synced_at = now()",
        )
        .bind(tenant)
        .bind(connection)
        .bind(stream)
        .bind(&item.external_id)
        .bind(entity)
        .bind(record)
        .bind(&item.version)
        .bind(hash)
        .bind(saved.get("version").and_then(Value::as_i64).map(|v| v as i32))
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn run_sync(&self, job: &Job) -> Result<Value> {
        let connection: Uuid = job.payload["connection"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::invalid("a sync names its connection"))?;
        let row = self.row(job.tenant_id, connection).await?;
        if !row.enabled {
            return Err(Error::invalid("this connection is off"));
        }
        let conn = self.opened(job.tenant_id, &row)?;
        let connector = self.connector(job.tenant_id, &row.provider).await?;
        let maps: BTreeMap<String, StreamMap> =
            serde_json::from_value(row.mapping.clone()).map_err(|e| Error::invalid(format!("this connection's mapping: {e}")))?;
        let sys = Ctx::system(job.tenant_id, self.registry.schema(job.tenant_id).await?);
        let (mut created, mut updated, mut unchanged, mut deleted, mut conflicts, mut failed) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
        for (stream, map) in &maps {
            // A worker lost part-way carries on from the last cursor it stored, whichever run that was.
            let mut cursor: Option<String> = if job.payload["from_start"] == json!(true) && job.attempts == 1 {
                None
            } else {
                self.stored_cursor(job.tenant_id, connection, stream).await?
            };
            loop {
                let page = self.calling(job.tenant_id, connection, connector.pull(self, &conn, stream, cursor.as_deref())).await?;
                for item in &page.records {
                    match self.apply_incoming(&sys, connection, stream, map, item).await {
                        Ok(Applied::Created) => created += 1,
                        Ok(Applied::Updated) => updated += 1,
                        Ok(Applied::Unchanged) => unchanged += 1,
                        Ok(Applied::Deleted) => deleted += 1,
                        Ok(Applied::Conflict) => conflicts += 1,
                        // One bad record does not stop the rest, and is there for a person.
                        Err(e) if !e.is_retryable() && !matches!(e, Error::Unavailable(_)) => {
                            failed += 1;
                            self.note_drift(job.tenant_id, connection, stream, &item.external_id, "refused", json!({"why": e.to_string()})).await?;
                        }
                        Err(e) => return Err(e),
                    }
                }
                self.store_cursor(job.tenant_id, connection, stream, page.next.as_deref().or(cursor.as_deref())).await?;
                job.progress(self, json!({"stream": stream, "created": created, "updated": updated, "unchanged": unchanged})).await?;
                match page.next {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => break,
                }
            }
        }
        let mut tx = self.db.system_tenant_tx(job.tenant_id).await?;
        sqlx::query("update connection set last_success_at = now(), last_error = null where tenant_id = $1 and id = $2")
            .bind(job.tenant_id)
            .bind(connection)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(json!({"created": created, "updated": updated, "unchanged": unchanged, "deleted": deleted, "conflicts": conflicts, "failed": failed}))
    }

    async fn stored_cursor(&self, tenant: Uuid, connection: Uuid, stream: &str) -> Result<Option<String>> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let cursor: Option<Option<String>> = sqlx::query_scalar("select cursors->>$3 from connection where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(connection)
            .bind(stream)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(cursor.flatten())
    }

    async fn store_cursor(&self, tenant: Uuid, connection: Uuid, stream: &str, cursor: Option<&str>) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        sqlx::query(
            "update connection set cursors = jsonb_set(cursors, array[$3], to_jsonb($4::text)) where tenant_id = $1 and id = $2 and $4::text is not null",
        )
        .bind(tenant)
        .bind(connection)
        .bind(stream)
        .bind(cursor)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn run_delivery(&self, job: &Job) -> Result<Value> {
        let p = &job.payload;
        let connection: Uuid = p["connection"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::invalid("a delivery names its connection"))?;
        let record: Uuid = p["record"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::invalid("a delivery names its record"))?;
        let (stream, key) = (p["stream"].as_str().unwrap_or_default(), p["key"].as_str().unwrap_or_default());
        let row = self.row(job.tenant_id, connection).await?;
        if !row.enabled {
            return Err(Error::invalid("this connection is off"));
        }
        let conn = self.opened(job.tenant_id, &row)?;
        let maps: BTreeMap<String, StreamMap> = serde_json::from_value(row.mapping.clone()).map_err(|e| Error::invalid(e.to_string()))?;
        let map = maps.get(stream).ok_or_else(|| Error::invalid(format!("this connection no longer maps {stream}")))?;
        let sys = Ctx::system(job.tenant_id, self.registry.schema(job.tenant_id).await?);
        let mut tx = self.db.system_tenant_tx(job.tenant_id).await?;
        // Gone since the change was queued: there is nothing to send, and nobody to tell.
        let doc = match self.get_tx(&sys, &mut tx, &map.entity, record, false).await {
            Ok(doc) => doc,
            Err(Error::NotFound(_)) => return Ok(json!({"skipped": "the record is gone"})),
            Err(e) => return Err(e),
        };
        // A marketing stream sends no one who withdrew their consent, asked as it goes (FEO-015).
        if let Some(field) = &map.consent {
            let email = doc.get(field).and_then(Value::as_str).unwrap_or_default();
            if !self.may_contact_for(&mut tx, job.tenant_id, email, crate::retention::Purpose::Marketing).await? {
                tx.commit().await?;
                return Ok(json!({"skipped": "the person may not be sent marketing"}));
            }
        }
        let external: Option<String> =
            sqlx::query_scalar("select external_id from connection_link where tenant_id = $1 and connection_id = $2 and stream = $3 and record_id = $4")
                .bind(job.tenant_id)
                .bind(connection)
                .bind(stream)
                .bind(record)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let mut fields = serde_json::Map::new();
        for (theirs, ours) in &map.fields {
            if let Some(v) = doc.get(ours) {
                fields.insert(theirs.clone(), v.clone());
            }
        }
        let change = Outgoing { key: key.to_string(), entity: map.entity.clone(), record, external_id: external, fields };
        // No transaction here: a provider's slowness must not hold one (docs/events.md).
        let connector = self.connector(job.tenant_id, &row.provider).await?;
        let external_id = self.calling(job.tenant_id, connection, connector.push(self, &conn, &change)).await?;
        let item = Incoming { external_id: external_id.clone(), ..Default::default() };
        let mut tx = self.db.system_tenant_tx(job.tenant_id).await?;
        self.save_link(&mut tx, job.tenant_id, connection, stream, &item, &map.entity, record, &digest(&mapped(map, &change.fields)), &doc).await?;
        tx.commit().await?;
        Ok(json!({"external_id": external_id}))
    }

    async fn run_reconcile(&self, job: &Job) -> Result<Value> {
        let connection: Uuid =
            job.payload["connection"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::invalid("a reconciliation names its connection"))?;
        let row = self.row(job.tenant_id, connection).await?;
        let conn = self.opened(job.tenant_id, &row)?;
        let connector = self.connector(job.tenant_id, &row.provider).await?;
        let maps: BTreeMap<String, StreamMap> = serde_json::from_value(row.mapping.clone()).map_err(|e| Error::invalid(e.to_string()))?;
        let (mut compared, mut drift) = (0u64, 0usize);
        for (stream, map) in &maps {
            let mut seen = std::collections::BTreeSet::new();
            let mut cursor = None;
            loop {
                let page = self.calling(job.tenant_id, connection, connector.pull(self, &conn, stream, cursor.as_deref())).await?;
                let mut tx = self.db.system_tenant_tx(job.tenant_id).await?;
                for item in page.records.iter().filter(|i| !i.deleted) {
                    compared += 1;
                    seen.insert(item.external_id.clone());
                    let link = self.connection_link(&mut tx, job.tenant_id, connection, stream, &item.external_id).await?;
                    let theirs = digest(&mapped(map, &item.fields));
                    let kind = match link {
                        None => Some("missing_here"),
                        Some(l) if l.hash.as_deref() != Some(theirs.as_str()) => Some("differs"),
                        Some(_) => None,
                    };
                    if let Some(kind) = kind.filter(|_| drift < MAX_DRIFT) {
                        drift += 1;
                        self.note_drift(job.tenant_id, connection, stream, &item.external_id, kind, json!({})).await?;
                    }
                }
                tx.commit().await?;
                match page.next {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => break,
                }
            }
            let mut tx = self.db.system_tenant_tx(job.tenant_id).await?;
            let held: Vec<String> = sqlx::query_scalar("select external_id from connection_link where tenant_id = $1 and connection_id = $2 and stream = $3")
                .bind(job.tenant_id)
                .bind(connection)
                .bind(stream)
                .fetch_all(&mut *tx)
                .await?;
            tx.commit().await?;
            for external_id in held.iter().filter(|e| !seen.contains(*e)) {
                if drift < MAX_DRIFT {
                    drift += 1;
                    self.note_drift(job.tenant_id, connection, stream, external_id, "missing_there", json!({})).await?;
                }
            }
        }
        Ok(json!({"compared": compared, "drift": drift}))
    }

    /// A webhook from a provider: authenticated by its connector, remembered by event id, applied in
    /// a job. Answers at once: providers do not wait.
    pub async fn connection_webhook(&self, connection: Uuid, headers: &BTreeMap<String, String>, body: &[u8]) -> Result<usize> {
        let mut tx = self.db.system_tx().await?;
        let tenant: Option<Uuid> =
            sqlx::query_scalar("select tenant_id from connection where id = $1 and enabled").bind(connection).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let tenant = tenant.ok_or_else(|| Error::NotFound("connection".into()))?;
        let row = self.row(tenant, connection).await?;
        let conn = self.opened(tenant, &row)?;
        let hooks = self.connector(tenant, &row.provider).await?.webhook(self, &conn, headers, body).await?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let mut queued = 0;
        for hook in hooks {
            let fresh = sqlx::query("insert into connection_event (tenant_id, connection_id, event_id) values ($1, $2, $3) on conflict do nothing")
                .bind(tenant)
                .bind(connection)
                .bind(&hook.event_id)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                > 0;
            if !fresh {
                continue;
            }
            let record =
                json!({"external_id": hook.record.external_id, "version": hook.record.version, "deleted": hook.record.deleted, "fields": hook.record.fields});
            enqueue(&mut tx, tenant, None, NewJob::new(WEBHOOK_KIND, json!({"connection": connection, "stream": hook.stream, "record": record}))).await?;
            queued += 1;
        }
        tx.commit().await?;
        Ok(queued)
    }

    async fn run_webhook(&self, job: &Job) -> Result<Value> {
        let p = &job.payload;
        let connection: Uuid = p["connection"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::invalid("names its connection"))?;
        let stream = p["stream"].as_str().unwrap_or_default();
        let row = self.row(job.tenant_id, connection).await?;
        let maps: BTreeMap<String, StreamMap> = serde_json::from_value(row.mapping.clone()).map_err(|e| Error::invalid(e.to_string()))?;
        let Some(map) = maps.get(stream) else { return Ok(json!({"ignored": stream})) };
        let r = &p["record"];
        let item = Incoming {
            external_id: r["external_id"].as_str().unwrap_or_default().to_string(),
            version: r["version"].as_str().map(str::to_string),
            deleted: r["deleted"] == json!(true),
            fields: r["fields"].as_object().cloned().unwrap_or_default(),
        };
        let sys = Ctx::system(job.tenant_id, self.registry.schema(job.tenant_id).await?);
        Ok(json!({"applied": format!("{:?}", self.apply_incoming(&sys, connection, stream, map, &item).await?)}))
    }

    /// Queues a change for every connection that sends this entity out, unless the change is one
    /// that came from there.
    pub(crate) async fn queue_deliveries(&self, ev: &Event) -> Result<()> {
        if !(ev.topic.ends_with(".created") || ev.topic.ends_with(".updated"))
            || (self.hooks.connectors().next().is_none() && self.hooks.connector_source().is_none())
        {
            return Ok(());
        }
        let (Some(entity), Some(record)) = (&ev.entity, ev.record_id) else { return Ok(()) };
        let mut tx = self.db.system_tenant_tx(ev.tenant_id).await?;
        let rows: Vec<(Uuid, Value)> =
            sqlx::query_as("select id, mapping from connection where tenant_id = $1 and enabled").bind(ev.tenant_id).fetch_all(&mut *tx).await?;
        for (connection, mapping) in rows {
            let Ok(maps) = serde_json::from_value::<BTreeMap<String, StreamMap>>(mapping) else { continue };
            for (stream, _) in maps.iter().filter(|(_, m)| m.push && &m.entity == entity) {
                let synced: Option<Option<i32>> = sqlx::query_scalar(
                    "select local_version from connection_link where tenant_id = $1 and connection_id = $2 and stream = $3 and record_id = $4",
                )
                .bind(ev.tenant_id)
                .bind(connection)
                .bind(stream)
                .bind(record)
                .fetch_optional(&mut *tx)
                .await?;
                // What arrived from there, or was sent since this event, does not go back.
                if synced.flatten().zip(ev.source_version).is_some_and(|(synced, version)| synced >= version) {
                    continue;
                }
                let key = format!("{connection}:{record}:{}", ev.source_version.unwrap_or_default());
                let mut job = NewJob::new(DELIVER_KIND, json!({"connection": connection, "stream": stream, "record": record, "key": key}));
                job.max_attempts = DELIVERY_ATTEMPTS;
                job.dedupe_key = Some(key);
                enqueue(&mut tx, ev.tenant_id, None, job).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Forgets webhook events past the time a provider would resend them.
    pub(crate) async fn purge_connection_events(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let n = sqlx::query("delete from connection_event where received_at < now() - make_interval(days => $1)")
            .bind(SEEN_EVENT_DAYS)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(n)
    }
}

const WEBHOOK_KIND: &str = "platform.connector.webhook";

/// The four jobs a connection runs on.
struct ConnectorJob(&'static str, &'static str);

#[async_trait]
impl JobHandler for ConnectorJob {
    fn kind(&self) -> &'static str {
        self.0
    }

    fn title(&self) -> &'static str {
        self.1
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(4 * 3600)
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        match self.0 {
            SYNC_KIND => k.run_sync(job).await,
            DELIVER_KIND => k.run_delivery(job).await,
            RECONCILE_KIND => k.run_reconcile(job).await,
            _ => k.run_webhook(job).await,
        }
    }
}

pub(crate) fn handler(kind: &str) -> Option<Arc<dyn JobHandler>> {
    let (kind, title) = match kind {
        SYNC_KIND => (SYNC_KIND, "Synchronization"),
        DELIVER_KIND => (DELIVER_KIND, "Sending a change"),
        RECONCILE_KIND => (RECONCILE_KIND, "Reconciliation"),
        WEBHOOK_KIND => (WEBHOOK_KIND, "A provider's event"),
        _ => return None,
    };
    Some(Arc::new(ConnectorJob(kind, title)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> StreamMap {
        serde_json::from_value(json!({
            "entity": "Customer",
            "fields": {"display_name": "customer_name", "state": "status"},
            "values": {"status": {"ACTIVE": "Open", "ARCHIVED": "Closed"}},
        }))
        .unwrap()
    }

    #[test]
    fn only_mapped_fields_come_in_and_values_are_translated() {
        let fields = json!({"display_name": "Acme", "state": "ARCHIVED", "secret_flag": true}).as_object().cloned().unwrap();
        assert_eq!(Value::Object(mapped(&map(), &fields)), json!({"customer_name": "Acme", "status": "Closed"}));
        let unknown = json!({"state": "SOMETHING_NEW"}).as_object().cloned().unwrap();
        assert_eq!(Value::Object(mapped(&map(), &unknown)), json!({"status": "SOMETHING_NEW"}), "an unknown value is passed for validation to refuse");
    }

    #[test]
    fn the_same_fields_are_the_same_change() {
        let a = json!({"x": 1, "y": 2}).as_object().cloned().unwrap();
        let b = json!({"y": 2, "x": 1}).as_object().cloned().unwrap();
        assert_eq!(digest(&a), digest(&b), "order does not matter");
        assert_ne!(digest(&a), digest(&json!({"x": 1}).as_object().cloned().unwrap()));
    }

    #[test]
    fn credentials_do_not_appear_in_logs() {
        let c = Connection { id: Uuid::nil(), tenant: Uuid::nil(), provider: "x".into(), config: json!({}), credentials: json!({"token": "s3cret"}) };
        assert!(!format!("{c:?}").contains("s3cret"));
    }
}
