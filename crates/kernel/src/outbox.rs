//! Transactional outbox: events are written in the same transaction as the change that caused
//! them, then dispatched at-least-once by any replica (`FOR UPDATE SKIP LOCKED`). Consumers are
//! idempotent (unique delivery/run keys), so a crash between "handled" and "marked" is harmless.

use crate::Kernel;
use crate::db::Tx;
use crate::error::{Error, Result};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use sqlx::postgres::PgListener;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

pub const OUTBOX_CHANNEL: &str = "ferrum_outbox";
const MAX_ATTEMPTS: i32 = 15;
const WEBHOOK_MAX_ATTEMPTS: i32 = 12;
const MAX_WEBHOOK_BODY: usize = 256 * 1024;

pub async fn enqueue(tx: &mut Tx, tenant: Uuid, topic: &str, entity: Option<&str>, record_id: Option<Uuid>, payload: Value) -> Result<()> {
    sqlx::query("insert into outbox (tenant_id, topic, entity, record_id, payload) values ($1, $2, $3, $4, $5)")
        .bind(tenant)
        .bind(topic)
        .bind(entity)
        .bind(record_id)
        .bind(payload)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Event {
    pub id: i64,
    pub tenant_id: Uuid,
    pub topic: String,
    pub entity: Option<String>,
    pub record_id: Option<Uuid>,
    pub payload: Value,
    pub attempts: i32,
}

/// How long a claimed batch is reserved for the worker handling it.
const LEASE_SECS: f64 = 600.0;

fn backoff(attempts: i32) -> i64 {
    // 5s, 10s, 20s ... capped at 1h.
    (5i64 << attempts.clamp(0, 10)).min(3600)
}

impl Kernel {
    /// Claims and handles one batch. Returns how many events were processed.
    ///
    /// The claim is a lease: a short transaction moves the batch's `available_at` into the
    /// future, then each event is handled outside any long transaction (handlers call webhooks,
    /// SMTP servers, plugins) and its outcome is recorded on its own. A worker that dies mid-batch
    /// leaves leases that simply expire, and another replica picks the events up — delivery stays
    /// at least once, never stuck behind a lock.
    pub async fn dispatch_outbox_once(&self) -> Result<usize> {
        let mut tx = self.db.system_tx().await?;
        let events: Vec<Event> = sqlx::query_as(
            "update outbox set available_at = now() + make_interval(secs => $1) where id in (\
               select id from outbox where processed_at is null and not dead and available_at <= now() \
               order by id limit 50 for update skip locked) \
             returning id, tenant_id, topic, entity, record_id, payload, attempts",
        )
        .bind(LEASE_SECS)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut events = events;
        events.sort_by_key(|e| e.id);
        for ev in &events {
            let outcome = self.handle_event(ev).await;
            let mut tx = self.db.system_tx().await?;
            match outcome {
                Ok(()) => {
                    sqlx::query("update outbox set processed_at = now(), last_error = null where id = $1").bind(ev.id).execute(&mut *tx).await?;
                }
                Err(e) => {
                    let attempts = ev.attempts + 1;
                    tracing::warn!(event = ev.id, topic = %ev.topic, attempts, "outbox handler failed: {e}");
                    sqlx::query(
                        "update outbox set attempts = $2, last_error = $3, dead = $4, \
                         available_at = now() + make_interval(secs => $5) where id = $1",
                    )
                    .bind(ev.id)
                    .bind(attempts)
                    .bind(e.to_string())
                    .bind(attempts >= MAX_ATTEMPTS)
                    .bind(backoff(attempts) as f64)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            tx.commit().await?;
        }
        Ok(events.len())
    }

    async fn handle_event(&self, ev: &Event) -> Result<()> {
        // Followers hear about changes made by others; first and best effort:
        // a retry must not notify twice.
        if let (Some(entity), Some(id)) = (&ev.entity, ev.record_id) {
            let event = ev.payload.get("event").and_then(Value::as_str).unwrap_or("updated");
            let actor = ev.payload.get("actor").and_then(Value::as_str).and_then(|s| s.parse().ok());
            let doc = ev.payload.get("doc").and_then(Value::as_object);
            if ev.attempts == 0
                && let Err(e) = self.notify_followers(ev.tenant_id, entity, id, event, actor, doc).await
            {
                tracing::warn!(topic = %ev.topic, "follower notifications failed: {e}");
            }
        }
        let mut tx = self.db.tenant_tx(ev.tenant_id).await?;
        let entity_wildcard = format!("{}.*", ev.topic.split('.').next().unwrap_or_default());
        sqlx::query(
            "insert into webhook_delivery (tenant_id, id, webhook_id, outbox_id, topic, payload) \
             select w.tenant_id, gen_random_uuid(), w.id, $2, $3, $4 from webhook w \
             where w.tenant_id = $1 and w.is_active and ($3 = any(w.topics) or '*' = any(w.topics) or $5 = any(w.topics)) \
             on conflict (tenant_id, webhook_id, outbox_id) do nothing",
        )
        .bind(ev.tenant_id)
        .bind(ev.id)
        .bind(&ev.topic)
        .bind(&ev.payload)
        .bind(entity_wildcard)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        crate::automation::run_for_event(self, ev).await?;
        for sink in self.hooks.event_sinks() {
            sink.on_event(self, ev).await?;
        }
        Ok(())
    }

    /// Wakes on NOTIFY (or every 2s) and drains the outbox until shutdown.
    pub fn spawn_outbox_dispatcher(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let k = self.clone();
        tokio::spawn(async move {
            let mut listener = PgListener::connect_with(&k.db.pool).await.ok();
            if let Some(l) = listener.as_mut()
                && l.listen(OUTBOX_CHANNEL).await.is_err()
            {
                listener = None;
            }
            loop {
                if *shutdown.borrow() {
                    return;
                }
                loop {
                    match k.dispatch_outbox_once().await {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::warn!("outbox dispatch error: {e}");
                            break;
                        }
                    }
                }
                let wait = tokio::time::sleep(Duration::from_secs(2));
                match listener.as_mut() {
                    Some(l) => tokio::select! {
                        _ = shutdown.changed() => return,
                        _ = wait => {}
                        r = l.recv() => if r.is_err() { listener = None; }
                    },
                    None => tokio::select! {
                        _ = shutdown.changed() => return,
                        _ = wait => {}
                    },
                }
            }
        })
    }

    pub async fn deliver_webhooks_once(&self) -> Result<usize> {
        #[derive(sqlx::FromRow)]
        struct Due {
            tenant_id: Uuid,
            id: Uuid,
            topic: String,
            payload: Value,
            attempts: i32,
            url: String,
            secret: String,
        }
        let mut tx = self.db.system_tx().await?;
        let due: Vec<Due> = sqlx::query_as(
            "select d.tenant_id, d.id, d.topic, d.payload, d.attempts, w.url, w.secret from webhook_delivery d \
             join webhook w on w.tenant_id = d.tenant_id and w.id = d.webhook_id \
             where d.status = 'pending' and d.next_attempt_at <= now() order by d.next_attempt_at limit 20 for update of d skip locked",
        )
        .fetch_all(&mut *tx)
        .await?;
        for d in &due {
            let outcome = self.post_webhook(&d.url, &d.secret, &d.topic, d.id, &d.payload).await;
            let attempts = d.attempts + 1;
            match outcome {
                Ok(code) => {
                    sqlx::query(
                        "update webhook_delivery set status = 'delivered', attempts = $3, response_code = $4, last_error = null, updated_at = now() \
                         where tenant_id = $1 and id = $2",
                    )
                    .bind(d.tenant_id)
                    .bind(d.id)
                    .bind(attempts)
                    .bind(code as i32)
                    .execute(&mut *tx)
                    .await?;
                }
                Err((code, err)) => {
                    let dead = attempts >= WEBHOOK_MAX_ATTEMPTS;
                    sqlx::query(
                        "update webhook_delivery set status = case when $5 then 'dead' else 'pending' end, attempts = $3, \
                         response_code = $4, last_error = $6, next_attempt_at = now() + make_interval(secs => $7), updated_at = now() \
                         where tenant_id = $1 and id = $2",
                    )
                    .bind(d.tenant_id)
                    .bind(d.id)
                    .bind(attempts)
                    .bind(code.map(|c| c as i32))
                    .bind(dead)
                    .bind(err)
                    .bind((30i64 << attempts.clamp(0, 9)).min(6 * 3600) as f64)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(due.len())
    }

    async fn post_webhook(&self, url: &str, secret: &str, topic: &str, delivery: Uuid, payload: &Value) -> std::result::Result<u16, (Option<u16>, String)> {
        let body = serde_json::to_vec(payload).map_err(|e| (None, e.to_string()))?;
        if body.len() > MAX_WEBHOOK_BODY {
            return Err((None, "payload too large".into()));
        }
        let parsed = reqwest::Url::parse(url).map_err(|e| (None, format!("bad url: {e}")))?;
        let host = parsed.host_str().ok_or((None, "url has no host".to_string()))?.to_string();
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| (None, format!("dns: {e}")))?.collect();
        if addrs.is_empty() {
            return Err((None, "host did not resolve".into()));
        }
        if !self.settings.webhooks_allow_private && addrs.iter().any(|a| is_private(a.ip())) {
            return Err((None, "webhook target resolves to a private address".into()));
        }
        // Pin the vetted addresses so a DNS change between check and connect cannot redirect us.
        let client = reqwest::Client::builder()
            .resolve_to_addrs(&host, &addrs)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .user_agent("Ferrum-Webhooks/1")
            .build()
            .map_err(|e| (None, e.to_string()))?;
        let ts = chrono::Utc::now().timestamp();
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|e| (None, e.to_string()))?;
        mac.update(format!("{ts}.").as_bytes());
        mac.update(&body);
        let sig = hex::encode(mac.finalize().into_bytes());
        let resp = client
            .post(parsed)
            .header("content-type", "application/json")
            .header("x-ferrum-event", topic)
            .header("x-ferrum-delivery", delivery.to_string())
            .header("x-ferrum-signature", format!("t={ts},v1={sig}"))
            .body(body)
            .send()
            .await
            .map_err(|e| (None, e.to_string()))?;
        let code = resp.status().as_u16();
        if resp.status().is_success() { Ok(code) } else { Err((Some(code), format!("HTTP {code}"))) }
    }

    pub fn spawn_webhook_sender(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let k = self.clone();
        tokio::spawn(async move {
            loop {
                let busy = match k.deliver_webhooks_once().await {
                    Ok(n) => n > 0,
                    Err(e) => {
                        tracing::warn!("webhook delivery error: {e}");
                        false
                    }
                };
                let pause = if busy { Duration::from_millis(50) } else { Duration::from_secs(3) };
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(pause) => {}
                }
            }
        })
    }

    /// Deletes processed outbox rows, delivered webhooks and expired idempotency keys.
    pub async fn purge_old_events(&self, keep_days: i32) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let mut n =
            sqlx::query("delete from outbox where processed_at < now() - make_interval(days => $1)").bind(keep_days).execute(&mut *tx).await?.rows_affected();
        n += sqlx::query("delete from webhook_delivery where status = 'delivered' and updated_at < now() - make_interval(days => $1)")
            .bind(keep_days)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        n += sqlx::query("delete from idempotency_key where created_at < now() - interval '24 hours'").execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(n)
    }
}

pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1])
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private(IpAddr::V4(v4)))
        }
    }
}

pub fn ensure_webhook_url(url: &str) -> Result<()> {
    let u = reqwest::Url::parse(url).map_err(|e| Error::invalid(format!("invalid URL: {e}")))?;
    if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() {
        return Err(Error::invalid("webhook URL must be http(s) with a host"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.1", "172.16.5.4", "169.254.169.254", "100.64.0.1", "::1", "fd00::1", "::ffff:10.0.0.1"] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
        assert_eq!(backoff(1), 10);
        assert_eq!(backoff(30), 3600);
    }
}
