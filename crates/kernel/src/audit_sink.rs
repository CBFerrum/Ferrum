//! Sending the audit log to another system (docs/operations.md, "Sending the audit log elsewhere"): the
//! operator's sink, `FERRUM_AUDIT_SINK_URL` with `FERRUM_AUDIT_SINK_SECRET`. A background loop reads audit
//! events of every workspace in order from a cursor kept in the database (`cluster_state`) and POSTs them in
//! batches, as newline-delimited JSON, signed. A workspace's own administrators send theirs through webhooks
//! (topics `audit.sign_in`, `audit.administration`, `audit.records`; crate::audit).
//!
//! * **Order.** Events go by (transaction, id): every audit event carries the id of the transaction that wrote
//!   it (`audit_event.xid`), and only events of transactions older than every transaction still running are
//!   read. Those can no longer be joined by an earlier one committing late, so the cursor never steps over an
//!   event that appears afterwards.
//! * **At least once.** The cursor moves, in the same transaction that holds it, only after the receiver said
//!   yes. A crash between its answer and the cursor's write sends that batch again: the receiver drops events
//!   whose `id` it has seen.
//! * **One sender.** The cursor's row is locked for a batch (`for update skip locked`): a replica that finds it
//!   locked waits for its next turn. Writers of audit events never touch it, so sending never slows a write.
//! * **The window.** A build that finds a newer build's schema (a guest, crate::window) sends nothing: the
//!   newer build's events may be ones it cannot read, and the newer build's replicas send them.

use crate::Kernel;
use crate::audit::{EVENT_COLUMNS, EventRow, event_json};
use crate::error::{Error, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const CURSOR: &str = "audit_sink_cursor";
/// Events in one POST.
const BATCH: i64 = 500;
/// The longest wait between tries while the receiver fails.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// `FERRUM_AUDIT_SINK_URL` and `FERRUM_AUDIT_SINK_SECRET`. The secret is never shown.
#[derive(Clone)]
pub struct Sink {
    pub url: String,
    pub secret: String,
}

impl std::fmt::Debug for Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sink").field("url", &self.url).field("secret", &"…").finish()
    }
}

impl Sink {
    /// The settings, checked at start: none when the URL is empty; a secret is required with a URL; https, or
    /// plain http to this machine only (a collector beside the server).
    pub fn parse(url: &str, secret: &str) -> std::result::Result<Option<Sink>, String> {
        let url = url.trim();
        if url.is_empty() {
            return Ok(None);
        }
        let parsed = reqwest::Url::parse(url).map_err(|_| format!("FERRUM_AUDIT_SINK_URL is not a URL: {url}"))?;
        let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if parsed.scheme() != "https" && !(parsed.scheme() == "http" && local) {
            return Err("FERRUM_AUDIT_SINK_URL must use https (plain http only to localhost)".into());
        }
        if secret.trim().len() < 16 {
            return Err("FERRUM_AUDIT_SINK_SECRET must be set, at least 16 characters, with FERRUM_AUDIT_SINK_URL".into());
        }
        Ok(Some(Sink { url: url.to_string(), secret: secret.trim().to_string() }))
    }
}

/// What one turn of the sender did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// No sink, or this build is a guest of a newer one.
    Off,
    /// Another replica holds the cursor.
    Busy,
    /// This many events were sent and acknowledged (0: nothing new).
    Sent(usize),
}

fn cursor_parts(v: &str) -> Result<(String, i64)> {
    let (xid, id) = v.split_once('/').ok_or_else(|| Error::internal(format!("the audit sink's cursor is not one: {v}")))?;
    if xid.is_empty() || !xid.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::internal(format!("the audit sink's cursor is not one: {v}")));
    }
    Ok((xid.to_string(), id.parse().map_err(|_| Error::internal(format!("the audit sink's cursor is not one: {v}")))?))
}

impl Kernel {
    /// One turn of the sender: the next batch after the cursor, sent and acknowledged, and the cursor moved.
    /// The first turn ever starts the cursor at now: events written before the sink was set are not sent.
    pub async fn audit_sink_once(&self) -> Result<Turn> {
        let Some(sink) = self.settings.audit_sink.clone() else { return Ok(Turn::Off) };
        if self.newer_build_here().await? {
            return Ok(Turn::Off);
        }
        let mut tx = self.db.system_tx().await?;
        const HELD: &str = "select value from cluster_state where key = $1 for update skip locked";
        let mut at: Option<String> = sqlx::query_scalar(HELD).bind(CURSOR).fetch_optional(&mut *tx).await?;
        if at.is_none() {
            let there: bool = sqlx::query_scalar("select exists (select 1 from cluster_state where key = $1)").bind(CURSOR).fetch_one(&mut *tx).await?;
            if there {
                return Ok(Turn::Busy);
            }
            // The first turn ever: the cursor starts at now. Two first turns at once: one row, one of them waits.
            sqlx::query(
                "insert into cluster_state (key, value) values ($1, pg_snapshot_xmin(pg_current_snapshot())::text || '/0') on conflict (key) do nothing",
            )
            .bind(CURSOR)
            .execute(&mut *tx)
            .await?;
            at = sqlx::query_scalar(HELD).bind(CURSOR).fetch_optional(&mut *tx).await?;
        }
        let Some(at) = at else { return Ok(Turn::Busy) };
        let (xid, id) = cursor_parts(&at)?;
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            uuid::Uuid,
            i64,
            chrono::DateTime<chrono::Utc>,
            Option<uuid::Uuid>,
            String,
            uuid::Uuid,
            String,
            Option<serde_json::Value>,
            Option<String>,
            Vec<u8>,
            String,
        )> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {EVENT_COLUMNS}, e.xid::text from audit_event e \
                 where e.xid is not null and (e.xid, e.id) > ($1::text::xid8, $2) and e.xid < pg_snapshot_xmin(pg_current_snapshot()) \
                 order by e.xid, e.id limit $3"
        )))
        .bind(&xid)
        .bind(id)
        .bind(BATCH)
        .fetch_all(&mut *tx)
        .await?;
        let Some(last) = rows.last().map(|r| format!("{}/{}", r.10, r.1)) else {
            tx.commit().await?;
            return Ok(Turn::Sent(0));
        };
        let mut body = Vec::new();
        for r in &rows {
            let row: EventRow = (r.0, r.1, r.2, r.3, r.4.clone(), r.5, r.6.clone(), r.7.clone(), r.8.clone(), r.9.clone());
            serde_json::to_writer(&mut body, &event_json(&row)).map_err(Error::internal)?;
            body.push(b'\n');
        }
        // While the cursor is held: a receiver's yes and the cursor's move are one step for every other replica.
        self.post_audit_batch(&sink, &body).await?;
        sqlx::query("update cluster_state set value = $2, updated_at = now() where key = $1").bind(CURSOR).bind(&last).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Turn::Sent(rows.len()))
    }

    async fn post_audit_batch(&self, sink: &Sink, body: &[u8]) -> Result<()> {
        // The operator's own setting: it may name an address inside their network (checked at start).
        let (client, url) = self.vetted_client(&sink.url, true, Duration::from_secs(10), "Ferrum-Audit/1").await?;
        let signature = crate::outbox::signature(&sink.secret, chrono::Utc::now().timestamp(), body)?;
        let resp = client
            .post(url)
            .header("content-type", "application/x-ndjson")
            .header("x-ferrum-signature", signature)
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| Error::Unavailable(format!("the audit sink cannot be reached: {e}")))?;
        if resp.status().is_success() { Ok(()) } else { Err(Error::Unavailable(format!("the audit sink answered {}", resp.status().as_u16()))) }
    }

    /// How far behind the sink is: seconds since the oldest event not yet sent was written (0: nothing waits),
    /// or `None` without a sink or before its first turn.
    pub async fn audit_sink_behind(&self) -> Result<Option<f64>> {
        if self.settings.audit_sink.is_none() {
            return Ok(None);
        }
        let mut tx = self.db.system_tx().await?;
        let at: Option<String> = sqlx::query_scalar("select value from cluster_state where key = $1").bind(CURSOR).fetch_optional(&mut *tx).await?;
        let Some(at) = at else { return Ok(None) };
        let (xid, id) = cursor_parts(&at)?;
        let behind: Option<f64> = sqlx::query_scalar(
            "select extract(epoch from now() - e.at)::float8 from audit_event e where e.xid is not null and (e.xid, e.id) > ($1::text::xid8, $2) \
             order by e.xid, e.id limit 1",
        )
        .bind(&xid)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(behind.unwrap_or(0.0).max(0.0)))
    }

    /// The sender, where background work runs: a turn as soon as the last one sent something, every two
    /// seconds while there is nothing, and with growing pauses (up to five minutes) while the receiver fails.
    pub fn spawn_audit_sink(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> Option<tokio::task::JoinHandle<()>> {
        self.settings.audit_sink.as_ref()?;
        let k = self.clone();
        Some(tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                let pause = match k.audit_sink_once().await {
                    Ok(Turn::Sent(n)) if n > 0 => {
                        failures = 0;
                        Duration::ZERO
                    }
                    Ok(Turn::Sent(_)) | Ok(Turn::Off) => {
                        failures = 0;
                        Duration::from_secs(2)
                    }
                    Ok(Turn::Busy) => Duration::from_secs(5),
                    Err(e) => {
                        failures = failures.saturating_add(1);
                        tracing::warn!(failures, "sending the audit log: {e}");
                        backoff(failures)
                    }
                };
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(pause) => {}
                }
            }
        }))
    }
}

/// The pause after `failures` failed turns in a row: 2, 4, 8 … seconds, at most [`MAX_BACKOFF`].
pub fn backoff(failures: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(failures.min(16))).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_checked() {
        assert!(Sink::parse("", "").unwrap().is_none(), "off by default");
        assert!(Sink::parse("https://siem.example.com/in", "a-secret-of-16-chars").unwrap().is_some());
        assert!(Sink::parse("http://localhost:9000/in", "a-secret-of-16-chars").unwrap().is_some(), "a local collector");
        assert!(Sink::parse("http://siem.example.com/in", "a-secret-of-16-chars").is_err(), "plain http elsewhere");
        assert!(Sink::parse("https://siem.example.com/in", "").is_err(), "a secret is required");
        assert!(Sink::parse("not a url", "a-secret-of-16-chars").is_err());
        let s = Sink::parse("https://siem.example.com/in", "a-secret-of-16-chars").unwrap().unwrap();
        assert!(!format!("{s:?}").contains("a-secret"), "the secret is never shown");
    }

    #[test]
    fn backoff_grows_to_five_minutes() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(40), MAX_BACKOFF);
        assert!(cursor_parts("123/45").is_ok() && cursor_parts("x/1").is_err() && cursor_parts("12").is_err());
    }
}
