//! Being called (BD-29): an inbound address per connection (`POST /api/connections/inbound/{id}`). The caller is
//! verified as the connection describes (a signature over the raw body with a timestamp, or a secret both sides hold),
//! a replay is refused, and what is accepted is recorded once and processed once by a job: mapped to "find a record of
//! this entity by this key and change it, or make it", or handed to an app registered for the connection.
//!
//! Unknown, unsigned, wrongly signed, stale, too large and replayed calls are answered alike, and counted.

use super::{Encoding, Loaded, at};
use crate::Kernel;
use crate::automation::{AutomationAction, Step};
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use uuid::Uuid;

pub const INBOUND_KIND: &str = "platform.connection.inbound";
const DEFAULT_MAX_BYTES: u64 = 256 << 10;
const MAX_BYTES: u64 = 1 << 20;
const DEFAULT_WINDOW_SECS: i64 = 300;
const MAX_WINDOW_SECS: i64 = 3600;
/// The most items one call or one page maps.
pub(crate) const MAX_ITEMS: usize = 500;
/// Inbound calls refused because they named no connection that takes them.
static REFUSED_UNKNOWN: AtomicU64 = AtomicU64::new(0);

/// How many inbound calls named no connection that takes them.
pub fn refused_unknown() -> u64 {
    REFUSED_UNKNOWN.load(Ordering::Relaxed)
}

/// The one answer a refused call gets, whatever the reason.
fn refused() -> Error {
    Error::Unauthorized("this call was not accepted".into())
}

/// What a connection takes from outside.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inbound {
    pub verify: Verify,
    /// Where the caller's own id for the delivery is: a header, or a pointer into the payload. Without one, a digest of
    /// what was signed stands for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// What becomes of a payload: a mapping onto records ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<Mapping>,
    /// ... or an app (one the connection lists) that registered for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler: Option<String>,
}

/// How a caller is verified. `secret` names one of the connection's sealed values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Verify {
    /// HMAC-SHA256 under the secret of `<timestamp>.<raw body>`, in `header` as `<prefix><hex or base64>`; the Unix time
    /// in `timestamp_header`, within `window_secs` (300) of now.
    Signature {
        header: String,
        secret: String,
        timestamp_header: String,
        #[serde(default)]
        encoding: Encoding,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prefix: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window_secs: Option<i64>,
    },
    /// A secret both sides hold, sent as it is in `header`.
    Token { header: String, secret: String },
}

/// A payload, or each item of it, onto records: the one record of `entity` whose `by` fields hold the values given is
/// changed with `set`; none, and one is made with `by`, `set` and `create` (unless `update_only`). Each value an
/// expression over `payload` (all of it) and `item` (the item, or all of it).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub entity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_at: Option<String>,
    pub by: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub create: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub update_only: bool,
}

/// An app's code for what a connection accepts. Runs in the transaction that marks the call processed: what it changes
/// commits with that, once; an error that says it cannot succeed marks the call failed, any other is tried again.
#[async_trait]
pub trait InboundHandler: Send + Sync {
    async fn handle(&self, k: &Kernel, tx: &mut Tx, ctx: &Ctx, connection: &str, payload: &Value) -> Result<()>;
}

impl Mapping {
    /// The effect it is: a `find` that changes or makes.
    fn step(&self) -> Step {
        let mut create = self.by.clone();
        create.extend(self.set.clone());
        create.extend(self.create.clone());
        let action = AutomationAction::Find {
            entity: self.entity.clone(),
            by: self.by.clone(),
            create: (!self.update_only).then_some(create),
            update: (!self.set.is_empty()).then(|| self.set.clone()),
        };
        Step { when: None, action }
    }

    pub(crate) fn problem(&self, schema: &ferrum_meta::Schema) -> Option<String> {
        if schema.entity(&self.entity).is_none() {
            return Some(format!("map.entity: {} is not installed", self.entity));
        }
        if self.by.is_empty() {
            return Some("map.by names the fields that find the record".into());
        }
        if let Some(p) = self.items_at.as_deref().and_then(super::pointer_problem) {
            return Some(p);
        }
        crate::automation::check_step(&self.step()).err().map(|e| format!("map: {e}"))
    }
}

impl Inbound {
    pub(crate) fn problem(&self, apps: &[String]) -> Option<String> {
        let (header, secret) = match &self.verify {
            Verify::Signature { header, secret, timestamp_header, window_secs, .. } => {
                if let Some(p) = super::header_problem(timestamp_header) {
                    return Some(p);
                }
                if window_secs.is_some_and(|w| !(1..=MAX_WINDOW_SECS).contains(&w)) {
                    return Some(format!("inbound.verify.window_secs is 1 to {MAX_WINDOW_SECS}"));
                }
                (header, secret)
            }
            Verify::Token { header, secret } => (header, secret),
        };
        if let Some(p) = super::header_problem(header) {
            return Some(p);
        }
        if secret.is_empty() {
            return Some("inbound.verify.secret names one of the connection's secrets".into());
        }
        if let Some(p) =
            self.delivery_header.as_deref().and_then(super::header_problem).or_else(|| self.delivery_at.as_deref().and_then(super::pointer_problem))
        {
            return Some(p);
        }
        if self.max_bytes.is_some_and(|b| b == 0 || b > MAX_BYTES) {
            return Some(format!("inbound.max_bytes is 1 to {MAX_BYTES}"));
        }
        match (&self.map, &self.handler) {
            (Some(_), None) => None,
            (None, Some(h)) if apps.contains(h) => None,
            (None, Some(h)) => Some(format!("inbound.handler: {h} is not one of the apps the connection lists")),
            _ => Some("inbound has a map or a handler, one of them".into()),
        }
    }
}

/// HMAC-SHA256 of `parts` under `secret`, checked against `given` (hex or base64) in constant time.
fn signed(secret: &str, parts: &[&[u8]], given: &str, encoding: Encoding) -> bool {
    use base64::Engine as _;
    use hmac::Mac;
    let Ok(mut mac) = hmac::Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else { return false };
    for p in parts {
        mac.update(p);
    }
    let given = match encoding {
        Encoding::Hex => hex::decode(given.trim()),
        Encoding::Base64 => base64::engine::general_purpose::STANDARD.decode(given.trim()).map_err(|_| hex::FromHexError::InvalidStringLength),
    };
    given.is_ok_and(|g| mac.verify_slice(&g).is_ok())
}

/// Whether the call is the caller the connection describes, and when; the id that stands for its delivery.
fn verified(conn: &Loaded, inbound: &Inbound, header: &dyn Fn(&str) -> Option<String>, body: &[u8], now: i64) -> Option<String> {
    let secret = |name: &str| conn.secrets.get(name).filter(|s| !s.is_empty()).cloned();
    let stands_for = match &inbound.verify {
        Verify::Signature { header: h, secret: s, timestamp_header, encoding, prefix, window_secs } => {
            let secret = secret(s)?;
            let ts = header(timestamp_header)?;
            let when: i64 = ts.trim().parse().ok()?;
            if (now - when).abs() > window_secs.unwrap_or(DEFAULT_WINDOW_SECS) {
                return None;
            }
            let given = header(h)?;
            let given = given.strip_prefix(prefix.as_str()).unwrap_or(&given).to_string();
            if !signed(&secret, &[ts.trim().as_bytes(), b".", body], &given, *encoding) {
                return None;
            }
            hex::encode(Sha256::digest([ts.trim().as_bytes(), b".", body].concat()))
        }
        Verify::Token { header: h, secret: s } => {
            let secret = secret(s)?;
            let given = header(h)?;
            if !crate::messages::subtle_eq(given.as_bytes(), secret.as_bytes()) {
                return None;
            }
            hex::encode(Sha256::digest(body))
        }
    };
    Some(stands_for)
}

impl Kernel {
    /// A call from outside to connection `id` (no session): verified, refused when replayed, recorded, and queued to be
    /// processed once. Every refusal is the same `Unauthorized`.
    pub async fn connection_inbound(&self, id: Uuid, headers: &BTreeMap<String, String>, body: &[u8]) -> Result<Value> {
        let mut tx = self.db.system_tx().await?;
        let tenant: Option<Uuid> = sqlx::query_scalar("select tenant_id from conn where id = $1 and enabled").bind(id).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let conn = match tenant {
            Some(t) => Some(self.load_connection(t, &id.to_string()).await?),
            None => None,
        };
        let Some((conn, inbound)) = conn.and_then(|c| Some((c.clone(), c.desc.inbound?))) else {
            REFUSED_UNKNOWN.fetch_add(1, Ordering::Relaxed);
            return Err(refused());
        };
        let header = |h: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(h)).map(|(_, v)| v.trim().to_string());
        let fits = body.len() as u64 <= inbound.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        let stands_for = if fits { verified(&conn, &inbound, &header, body, chrono::Utc::now().timestamp()) } else { None };
        let Some(stands_for) = stands_for else { return self.refuse_inbound(&conn).await };
        let form = header("content-type").is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"));
        let payload: Value = if form {
            let mut url = reqwest::Url::parse("http://form/").map_err(Error::internal)?;
            url.set_query(Some(&String::from_utf8_lossy(body)));
            Value::Object(url.query_pairs().map(|(k, v)| (k.into_owned(), Value::String(v.into_owned()))).collect())
        } else {
            serde_json::from_slice(body).map_err(|_| Error::invalid("the body is not JSON"))?
        };
        let delivery = inbound
            .delivery_header
            .as_deref()
            .and_then(header)
            .or_else(|| at(&payload, inbound.delivery_at.as_deref()))
            .filter(|d| !d.is_empty() && d.len() <= 200)
            .unwrap_or(stands_for);
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        let fresh = sqlx::query("insert into conn_inbound (tenant_id, conn_id, delivery, payload) values ($1, $2, $3, $4) on conflict do nothing")
            .bind(conn.tenant)
            .bind(conn.id)
            .bind(&delivery)
            .bind(&payload)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        if !fresh {
            tx.rollback().await?;
            return self.refuse_inbound(&conn).await;
        }
        let mut job = NewJob::new(INBOUND_KIND, json!({"connection": conn.id, "delivery": delivery}));
        job.max_attempts = 8;
        crate::jobs::enqueue(&mut tx, conn.tenant, None, job).await?;
        tx.commit().await?;
        Ok(json!({"accepted": true}))
    }

    async fn refuse_inbound(&self, conn: &Loaded) -> Result<Value> {
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        sqlx::query("update conn set refused = refused + 1 where tenant_id = $1 and id = $2").bind(conn.tenant).bind(conn.id).execute(&mut *tx).await?;
        tx.commit().await?;
        Err(refused())
    }

    /// Maps `payload` (each item at `items_at`, or all of it) onto records in `tx`. How many items.
    pub(crate) async fn apply_mapping(&self, ctx: &Ctx, tx: &mut Tx, map: &Mapping, payload: &Value) -> Result<usize> {
        let items = match map.items_at.as_deref() {
            Some(p) => payload.pointer(p).and_then(Value::as_array).cloned().unwrap_or_default(),
            None => vec![payload.clone()],
        };
        if items.len() > MAX_ITEMS {
            return Err(Error::invalid(format!("{} items, more than the {MAX_ITEMS} one call maps", items.len())));
        }
        let empty = json!({});
        for (i, item) in items.iter().enumerate() {
            let extra = [("payload", payload), ("item", item)];
            crate::automation::run_step(self, ctx, tx, &map.entity, None, (&empty, &empty), &empty, &extra, "inbound", map.step())
                .await
                .map_err(|e| crate::pack::about(&format!("item {}", i + 1), e))?;
        }
        Ok(items.len())
    }

    /// One accepted call, processed ([`INBOUND_KIND`]): its mapping or its app, and the call marked done, in one
    /// transaction; one that cannot succeed is marked failed, saying why.
    async fn process_inbound(&self, job: &Job) -> Result<Value> {
        let tenant = job.tenant_id;
        let conn_id: Uuid = job.payload["connection"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::invalid("names its connection"))?;
        let delivery = job.payload["delivery"].as_str().unwrap_or_default().to_string();
        let conn = self.load_connection(tenant, &conn_id.to_string()).await?;
        let inbound = conn.desc.inbound.clone().ok_or_else(|| Error::invalid("the connection no longer takes calls"))?;
        let ctx = Ctx::system(tenant, self.registry.schema(tenant).await?);
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let payload: Option<Value> =
            sqlx::query_scalar("select payload from conn_inbound where tenant_id = $1 and conn_id = $2 and delivery = $3 and state = 'accepted' for update")
                .bind(tenant)
                .bind(conn_id)
                .bind(&delivery)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
        let Some(payload) = payload else { return Ok(json!({"done": "already"})) };
        let done = match (&inbound.map, &inbound.handler) {
            (Some(map), _) => self.apply_mapping(&ctx, &mut tx, map, &payload).await.map(|n| json!({"items": n})),
            (None, Some(app)) if conn.desc.apps.contains(app) => match self.hooks.inbound_handler(app) {
                Some(h) => h.handle(self, &mut tx, &ctx, &conn.name, &payload).await.map(|()| json!({"handled": app})),
                None => Err(Error::invalid(format!("{app} has not registered for calls"))),
            },
            _ => Err(Error::invalid("the connection says nothing to do with calls")),
        };
        match done {
            Ok(out) => {
                sqlx::query("update conn_inbound set state = 'done', payload = null, error = null where tenant_id = $1 and conn_id = $2 and delivery = $3")
                    .bind(tenant)
                    .bind(conn_id)
                    .bind(&delivery)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(out)
            }
            Err(e @ (Error::Invalid(_) | Error::Validation(_) | Error::Forbidden(_) | Error::NotFound(_) | Error::Refused { .. })) => {
                tx.rollback().await?;
                let mut tx = self.db.system_tenant_tx(tenant).await?;
                let why: String = super::masked_text(&e.to_string(), &conn.mask());
                sqlx::query("update conn_inbound set state = 'failed', error = $4 where tenant_id = $1 and conn_id = $2 and delivery = $3")
                    .bind(tenant)
                    .bind(conn_id)
                    .bind(&delivery)
                    .bind(&why)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(json!({"failed": why}))
            }
            Err(e) => Err(e),
        }
    }

    /// Accepted calls of a connection, newest first, without their payloads: `{delivery, received_at, state, error}`.
    pub async fn connection_inbound_log(&self, ctx: &Ctx, id: Uuid, limit: i64) -> Result<Vec<Value>> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("connections are configured by administrators".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows = sqlx::query_scalar(
            "select jsonb_build_object('delivery', delivery, 'received_at', received_at, 'state', state, 'error', error) \
             from conn_inbound where tenant_id = $1 and conn_id = $2 order by received_at desc limit $3",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }
}

struct Process;

#[async_trait]
impl JobHandler for Process {
    fn kind(&self) -> &'static str {
        INBOUND_KIND
    }

    fn title(&self) -> &'static str {
        "A call to a connection, processed"
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        k.process_inbound(job).await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(300)
    }
}

pub(crate) fn handler(kind: &str) -> Option<Arc<dyn JobHandler>> {
    (kind == INBOUND_KIND).then(|| Arc::new(Process) as Arc<dyn JobHandler>)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(desc: Value) -> (Loaded, Inbound) {
        let desc: super::super::Description = serde_json::from_value(desc).unwrap();
        let inbound = desc.inbound.clone().unwrap();
        let secrets = BTreeMap::from([("hook".to_string(), "whsec-1234".to_string())]);
        (Loaded { tenant: Uuid::nil(), id: Uuid::nil(), name: "c".into(), desc, secrets, enabled: true, debug: false }, inbound)
    }

    #[test]
    fn signatures_and_tokens_are_checked() {
        use hmac::Mac;
        let (c, i) = conn(
            json!({"test": true, "inbound": {"verify": {"type": "signature", "header": "x-sig", "secret": "hook", "timestamp_header": "x-ts", "prefix": "v1="},
                                  "map": {"entity": "X", "by": {"a": "item.a"}}}}),
        );
        let body = br#"{"a":1}"#;
        let sign = |ts: i64, body: &[u8]| {
            let mut mac = hmac::Hmac::<Sha256>::new_from_slice(b"whsec-1234").unwrap();
            mac.update(format!("{ts}.").as_bytes());
            mac.update(body);
            format!("v1={}", hex::encode(mac.finalize().into_bytes()))
        };
        let with = |ts: i64, sig: String| {
            move |h: &str| match h {
                "x-ts" => Some(ts.to_string()),
                "x-sig" => Some(sig.clone()),
                _ => None,
            }
        };
        let now = 1_800_000_000;
        assert!(verified(&c, &i, &with(now, sign(now, body)), body, now).is_some());
        assert!(verified(&c, &i, &with(now - 299, sign(now - 299, body)), body, now).is_some(), "within the window");
        assert!(verified(&c, &i, &with(now - 301, sign(now - 301, body)), body, now).is_none(), "stale");
        assert!(verified(&c, &i, &with(now, sign(now, br#"{"a":2}"#)), body, now).is_none(), "another body");
        assert!(verified(&c, &i, &with(now, sign(now - 1, body)), body, now).is_none(), "another time");
        assert!(verified(&c, &i, &|_: &str| None, body, now).is_none(), "unsigned");
        let (c, i) = conn(
            json!({"test": true, "inbound": {"verify": {"type": "token", "header": "x-token", "secret": "hook"}, "map": {"entity": "X", "by": {"a": "item.a"}}}}),
        );
        assert!(verified(&c, &i, &|_: &str| Some("whsec-1234".into()), body, now).is_some());
        assert!(verified(&c, &i, &|_: &str| Some("whsec-1235".into()), body, now).is_none());
        assert!(verified(&c, &i, &|_: &str| Some(String::new()), body, now).is_none());
    }

    #[test]
    fn inbound_descriptions_are_checked() {
        let p = |v: Value| serde_json::from_value::<Inbound>(v).map(|i| i.problem(&["app".into()])).unwrap_or_else(|e| Some(e.to_string()));
        let token = json!({"type": "token", "header": "x-token", "secret": "hook"});
        assert_eq!(p(json!({"verify": token, "handler": "app"})), None);
        assert!(p(json!({"verify": token, "handler": "other"})).is_some(), "an app the connection does not list");
        assert!(p(json!({"verify": token})).is_some(), "nothing to do");
        assert!(p(json!({"verify": {"type": "signature", "header": "x-sig", "secret": "hook"}, "handler": "app"})).is_some(), "a signature has a timestamp");
        assert!(p(json!({"verify": token, "handler": "app", "max_bytes": 1 << 30})).is_some());
        assert!(p(json!({"verify": {"type": "none"}, "handler": "app"})).is_some(), "every call is verified");
    }
}
