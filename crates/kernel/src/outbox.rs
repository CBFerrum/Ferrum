//! Transactional outbox: events are written in the same transaction as the change that caused
//! them, then dispatched at-least-once by any replica (`FOR UPDATE SKIP LOCKED`). Consumers are
//! idempotent (unique delivery/run keys), so a crash between "handled" and "marked" is harmless.
//! Dispatchers poll (no NOTIFY from writers: it would serialize their commits).

use crate::Kernel;
use crate::db::Tx;
use crate::error::{Error, Result};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

/// How often an idle dispatcher looks for new events.
const OUTBOX_POLL: Duration = Duration::from_millis(250);
const MAX_ATTEMPTS: i32 = 15;
const WEBHOOK_MAX_ATTEMPTS: i32 = 12;
const MAX_WEBHOOK_BODY: usize = 256 * 1024;

/// Outbox topic of mail queued with [`Kernel::queue_mail`]; never fanned out to webhooks.
pub const MAIL_TOPIC: &str = "ferrum.mail";

/// Stored objects whose records are gone, deleted from storage once their transaction commits.
pub const RELEASE_TOPIC: &str = "ferrum.storage.release";

/// The most a queued mail may attach.
const MAX_ATTACHED: i64 = 20 << 20;

/// What [`Kernel::queue_mail`] accepts: one address, and a one-line subject of at most 300 characters.
pub fn check_mail(to: &str, subject: &str) -> Result<()> {
    let to = to.trim();
    let one_address = to.len() <= 254 && to.split('@').count() == 2 && !to.contains([',', ';', ' ', '<', '>', '\r', '\n']);
    if !one_address || to.starts_with('@') || to.ends_with('@') {
        return Err(Error::field("to", "must be one e-mail address"));
    }
    if subject.trim().is_empty() || subject.len() > 300 || subject.contains(['\r', '\n']) {
        return Err(Error::field("subject", "must be one line of at most 300 characters"));
    }
    Ok(())
}

impl Kernel {
    /// Queues an e-mail in the caller's transaction: it goes out through the server's mailer once
    /// the transaction commits (never if it rolls back), retried while the mailer fails. For
    /// apps' own messages (confirmations, reminders) whether or not the Mail app is installed.
    pub async fn queue_mail(&self, tx: &mut Tx, tenant: Uuid, to: &str, subject: &str, html: &str) -> Result<()> {
        self.queue_mail_with(tx, tenant, to, subject, html, &[]).await
    }

    /// [`queue_mail`](Self::queue_mail) with stored files attached (up to 10, 20 MB together). The
    /// files are read when the mail goes out; one deleted by then fails the delivery, never
    /// sends without it.
    pub async fn queue_mail_with(&self, tx: &mut Tx, tenant: Uuid, to: &str, subject: &str, html: &str, files: &[Uuid]) -> Result<()> {
        let to = to.trim();
        check_mail(to, subject)?;
        if html.len() > 1 << 20 {
            return Err(Error::field("html", "is larger than 1 MB"));
        }
        if files.len() > 10 {
            return Err(Error::field("attachments", "at most 10 files"));
        }
        if !files.is_empty() && !self.mailer.as_ref().is_some_and(|m| m.can_attach()) {
            return Err(Error::Unavailable("this server's mailer cannot attach files (it needs an SMTP server)".into()));
        }
        if !files.is_empty() {
            let (found, bytes): (i64, Option<i64>) =
                sqlx::query_as("select count(*), sum(size_bytes)::int8 from file_object where tenant_id = $1 and id = any($2)")
                    .bind(tenant)
                    .bind(files)
                    .fetch_one(&mut **tx)
                    .await?;
            if found as usize != files.iter().collect::<std::collections::HashSet<_>>().len() {
                return Err(Error::field("attachments", "a file to attach does not exist"));
            }
            if bytes.unwrap_or(0) > MAX_ATTACHED {
                return Err(Error::field("attachments", "attachments are larger than 20 MB together"));
            }
        }
        enqueue(tx, tenant, MAIL_TOPIC, None, None, serde_json::json!({"to": to, "subject": subject.trim(), "html": html, "files": files})).await
    }

    /// The files a queued mail attaches, read from storage.
    async fn mail_attachments(&self, tenant: Uuid, payload: &Value) -> Result<Vec<crate::MailAttachment>> {
        let ids: Vec<Uuid> =
            payload.get("files").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str()?.parse().ok()).collect()).unwrap_or_default();
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut tx = self.db.system_tx().await?;
        let rows: Vec<(Uuid, String, String, String)> =
            sqlx::query_as("select id, file_name, content_type, storage_key from file_object where tenant_id = $1 and id = any($2)")
                .bind(tenant)
                .bind(&ids)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let store = self.files.get().ok_or_else(|| Error::Unavailable("no file storage is configured".into()))?;
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            let (_, name, ctype, key) = rows.iter().find(|r| r.0 == *id).ok_or_else(|| Error::Invalid(format!("attachment {id} was deleted")))?;
            out.push(crate::MailAttachment { file_name: name.clone(), content_type: ctype.clone(), data: store.get(key).await? });
        }
        Ok(out)
    }
}

/// Deletes stored objects from storage once the caller's transaction commits (never if it rolls
/// back), retried while storage fails: for apps that delete `file_object` rows in their own
/// transactions. Only this tenant's objects (`tenants/{tenant}/...`).
pub async fn release_objects(tx: &mut Tx, tenant: Uuid, keys: &[String]) -> Result<()> {
    let own = format!("tenants/{tenant}/");
    if let Some(bad) = keys.iter().find(|k| !k.starts_with(&own) || k.contains("..")) {
        return Err(Error::internal(format!("not a stored object of this workspace: {bad}")));
    }
    for chunk in keys.chunks(500) {
        enqueue(tx, tenant, RELEASE_TOPIC, None, None, serde_json::json!({"keys": chunk})).await?;
    }
    Ok(())
}

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
        if ev.topic == RELEASE_TOPIC {
            let store = self.files.get().ok_or_else(|| Error::Unavailable("no file storage is configured".into()))?;
            let own = format!("tenants/{}/", ev.tenant_id);
            for key in ev.payload.get("keys").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                // Deleting an object that is already gone succeeds, so a retry is harmless.
                if key.starts_with(&own) {
                    store.delete(key).await?;
                }
            }
            return Ok(());
        }
        if ev.topic == crate::notify::MAIL_TOPIC {
            return self.mail_notification(ev.tenant_id, &ev.payload).await;
        }
        if ev.topic == MAIL_TOPIC {
            let mailer = self.mailer.as_ref().ok_or_else(|| Error::Unavailable("no mailer is configured; queued mail waits for one".into()))?;
            let field = |k: &str| ev.payload.get(k).and_then(Value::as_str).unwrap_or_default();
            let attachments = self.mail_attachments(ev.tenant_id, &ev.payload).await?;
            return mailer.send_with(field("to"), field("subject"), field("html"), &attachments).await;
        }
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

    /// Drains the outbox until shutdown, looking again every 250 ms when it is empty.
    pub fn spawn_outbox_dispatcher(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let k = self.clone();
        tokio::spawn(async move {
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
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(OUTBOX_POLL) => {}
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
