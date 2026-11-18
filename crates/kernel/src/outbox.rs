//! Transactional outbox: events are written in the same transaction as the change that caused
//! them, then dispatched at-least-once by any replica (`FOR UPDATE SKIP LOCKED`). Each consumer
//! stands alone: one that fails makes the event retry for itself, and those that handled it are
//! not called again (unique delivery and run keys, `event_receipt` for sinks). A crash between
//! "handled" and "marked" can repeat a sink, which deduplicates what must happen once.
//! No NOTIFY from writers (it would serialize their commits): a replica's own writes wake its dispatcher in-process, and
//! idle dispatchers look less and less often, up to every five seconds ([`Pace`]).
//!
//! Each batch takes a few events from every workspace that has work, most urgent first, and handles
//! workspaces side by side, so one workspace's backlog or slow provider cannot hold up another's
//! password resets. Events carry their origin (docs/events.md). One that fails too often stays in
//! the table as dead, for an administrator to see and replay.

use crate::Kernel;
use crate::ctx::Ctx;
use crate::db::{Trace, Tx};
use crate::error::{Error, Result};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

/// How the replica's loops that look for work in the database (events, webhook deliveries, jobs) pace their looks:
/// soon after a wake or after finding work, then twice as long after each look that finds nothing, up to [`Pace::CAP`].
/// A wake comes from this replica's own writes ([`wake`]); what another replica wrote, or a retry coming due, is found
/// by the looks, within the cap.
pub(crate) struct Pace {
    wait: Duration,
    /// Until when it keeps looking every [`Pace::SETTLE`]: a write that woke it may not have committed at the first look.
    chase: Option<tokio::time::Instant>,
}

impl Pace {
    /// The wait after a wake or after work was found: what a burst of writes is gathered in.
    pub(crate) const SETTLE: Duration = Duration::from_millis(50);
    /// How long after a wake it keeps looking every [`Pace::SETTLE`] while it finds nothing: the time the waking write's
    /// transaction has to commit before the looks back off again.
    pub(crate) const CHASE: Duration = Duration::from_secs(1);
    /// The longest an idle loop waits between looks, and so the most an event written by a replica that died right
    /// after its commit, or a retry coming due, waits beyond its time (docs/events.md).
    pub(crate) const CAP: Duration = Duration::from_secs(5);

    pub(crate) fn new() -> Pace {
        Pace { wait: Self::SETTLE, chase: None }
    }

    /// How long to wait after a look: [`Pace::SETTLE`] if it found work or a wake is being chased, otherwise the last
    /// wait, doubled for the next.
    pub(crate) fn after(&mut self, found: bool) -> Duration {
        let chasing = self.chase.is_some_and(|until| tokio::time::Instant::now() < until);
        if found || chasing {
            self.wait = Self::SETTLE;
            return self.wait;
        }
        self.chase = None;
        let wait = self.wait;
        self.wait = (wait * 2).min(Self::CAP);
        wait
    }

    /// A wake: the next look comes after [`Pace::SETTLE`], and the looks stay that close for [`Pace::CHASE`].
    pub(crate) fn woken(&mut self) -> Duration {
        self.wait = Self::SETTLE;
        self.chase = Some(tokio::time::Instant::now() + Self::CHASE);
        self.wait
    }
}

/// The longest an idle dispatcher, webhook sender or job runner waits between looks ([`Pace::CAP`]).
pub const IDLE_CAP: Duration = Pace::CAP;

/// What a loop of this process is woken by when this process writes work for it. A watch: every loop sees the latest
/// count, so a wake that comes while a loop is busy is still seen when it next waits.
pub(crate) static EVENTS: std::sync::LazyLock<watch::Sender<u64>> = std::sync::LazyLock::new(|| watch::channel(0).0);
pub(crate) static WEBHOOKS: std::sync::LazyLock<watch::Sender<u64>> = std::sync::LazyLock::new(|| watch::channel(0).0);
pub(crate) static JOBS: std::sync::LazyLock<watch::Sender<u64>> = std::sync::LazyLock::new(|| watch::channel(0).0);

/// Waits `wait`, or less if woken (then [`Pace::SETTLE`]); false once the process is stopping.
pub(crate) async fn rest(wait: Duration, pace: &mut Pace, woken: &mut watch::Receiver<u64>, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = shutdown.changed() => return false,
        _ = tokio::time::sleep(wait) => return true,
        _ = woken.changed() => {}
    }
    let settle = pace.woken();
    tokio::select! {
        _ = shutdown.changed() => false,
        _ = tokio::time::sleep(settle) => true,
    }
}

/// Wakes the loops of this process that `which` stands for.
pub(crate) fn wake(which: &watch::Sender<u64>) {
    which.send_modify(|n| *n = n.wrapping_add(1));
}
const MAX_ATTEMPTS: i32 = 15;
const WEBHOOK_MAX_ATTEMPTS: i32 = 12;
const MAX_WEBHOOK_BODY: usize = 256 * 1024;

/// What someone is waiting for: sign-in codes, invitations, notifications.
pub const PRIORITY_URGENT: i16 = 0;
/// Imports, mass changes: behind everything else of their workspace.
pub const PRIORITY_BULK: i16 = 2;
/// Events one workspace gets in a batch, however many are waiting ...
const TENANT_SHARE: i64 = 10;
/// ... of this many in all.
const BATCH: i64 = 50;
/// The longest one event's handlers may take before it counts as failed and is retried.
const EVENT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long an event waits when a sink of it was at its share of the turns ([`Sinks`]).
const DEFER_SECS: u64 = 5;

/// Outbox topic of mail queued with [`Kernel::queue_mail`]; never fanned out to webhooks.
pub const MAIL_TOPIC: &str = "ferrum.mail";

/// Stored objects whose records are gone, deleted from storage once their transaction commits.
pub const RELEASE_TOPIC: &str = "ferrum.storage.release";

/// The most a queued mail may attach.
const MAX_ATTACHED: i64 = 20 << 20;

/// Plain text as a mail's HTML: escaped, its line breaks kept. What a record or a caller says is never markup.
pub fn text_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    crate::coedit::esc(text, &mut out);
    out.replace("\r\n", "\n").replace('\n', "<br>")
}

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
        self.queue_mail_for(tx, tenant, to, subject, html, files, crate::retention::Purpose::Transactional).await
    }

    /// [`queue_mail_with`](Self::queue_mail_with) for a purpose: marketing is not sent to someone who
    /// withdrew their consent, asked again when it goes out (FEO-015), as a restriction is.
    #[allow(clippy::too_many_arguments)]
    pub async fn queue_mail_for(
        &self,
        tx: &mut Tx,
        tenant: Uuid,
        to: &str,
        subject: &str,
        html: &str,
        files: &[Uuid],
        purpose: crate::retention::Purpose,
    ) -> Result<()> {
        self.queue_mail_send(tx, tenant, to, subject, html, files, purpose, None, &[]).await
    }

    /// [`queue_mail_for`](Self::queue_mail_for), and the message send it is (messages.rs), told when it goes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn queue_mail_send(
        &self,
        tx: &mut Tx,
        tenant: Uuid,
        to: &str,
        subject: &str,
        html: &str,
        files: &[Uuid],
        purpose: crate::retention::Purpose,
        send: Option<Uuid>,
        mask: &[String],
    ) -> Result<()> {
        let to = to.trim();
        check_mail(to, subject)?;
        if !self.may_contact_for(tx, tenant, to, purpose).await? {
            tracing::info!(%tenant, ?purpose, "a queued mail was not sent: its recipient may not be contacted for it");
            return Ok(());
        }
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
        let mut payload = serde_json::json!({"to": to, "subject": subject.trim(), "html": html, "files": files, "purpose": purpose});
        // Secret values wait sealed, and only a placeholder shows where they go.
        if !mask.is_empty() {
            let plain = serde_json::json!({"subject": subject.trim(), "html": html, "mask": mask});
            payload["sealed"] = Value::String(self.seal_message(tenant, &plain).await?);
            payload["subject"] = crate::messages::masked(&payload["subject"], mask);
            payload["html"] = crate::messages::masked(&payload["html"], mask);
        }
        if let Some(send) = send {
            payload["send"] = serde_json::json!(send);
        }
        enqueue(tx, tenant, MAIL_TOPIC, None, None, payload).await
    }

    /// A queued mail's message send, once its mail went or did not.
    async fn mail_settled(&self, tenant: Uuid, send: Option<Uuid>, state: &str, reason: Option<&str>) -> Result<()> {
        let Some(send) = send else { return Ok(()) };
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query("update message_send set state = $3, reason = $4, updated_at = now() where tenant_id = $1 and id = $2 and state = 'queued'")
            .bind(tenant)
            .bind(send)
            .bind(state)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
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
    // Mail and push carry what someone is waiting for, whatever work queued them; a send to a whole
    // list carries nothing anyone is waiting for, and stays behind that.
    let urgent = matches!(topic, MAIL_TOPIC | crate::notify::MAIL_TOPIC | crate::webpush::PUSH_TOPIC);
    sqlx::query(
        "insert into outbox (tenant_id, topic, entity, record_id, payload, priority, build_date) \
         values ($1, $2, $3, $4, $5, case when $6 and coalesce(nullif(current_setting('ferrum.priority', true), '')::smallint, 1) < 2 then 0 \
         else coalesce(nullif(current_setting('ferrum.priority', true), '')::smallint, 1) end, $7)",
    )
    .bind(tenant)
    .bind(topic)
    .bind(entity)
    .bind(record_id)
    .bind(payload)
    .bind(urgent)
    .bind(crate::window::build_date())
    .execute(&mut **tx)
    .await?;
    // Written in a transaction not yet committed: the dispatcher looks after [`Pace::SETTLE`], and on as it backs off.
    wake(&EVENTS);
    Ok(())
}

/// The record an event is about.
pub struct Source<'a> {
    pub entity: &'a str,
    pub record_id: Uuid,
    pub actor: Option<Uuid>,
    /// The company the record belongs to, when it has one.
    pub legal_entity: Option<Uuid>,
    pub version: Option<i32>,
}

/// [`enqueue`] for a change to a record, saying who made it and which version it made.
pub async fn enqueue_change(tx: &mut Tx, tenant: Uuid, topic: &str, source: Source<'_>, payload: Value) -> Result<()> {
    sqlx::query(
        "insert into outbox (tenant_id, topic, entity, record_id, payload, actor, legal_entity, source_version, build_date) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(tenant)
    .bind(topic)
    .bind(source.entity)
    .bind(source.record_id)
    .bind(payload)
    .bind(source.actor)
    .bind(source.legal_entity)
    .bind(source.version)
    .bind(crate::window::build_date())
    .execute(&mut **tx)
    .await?;
    wake(&EVENTS);
    Ok(())
}

#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct Event {
    pub id: i64,
    pub tenant_id: Uuid,
    pub topic: String,
    pub entity: Option<String>,
    pub record_id: Option<Uuid>,
    pub payload: Value,
    pub attempts: i32,
    pub correlation_id: Option<String>,
    pub traceparent: Option<String>,
    pub actor: Option<Uuid>,
    pub legal_entity: Option<Uuid>,
    pub source_version: Option<i32>,
    pub schema_version: i16,
    /// Events between this one and the change that started its chain.
    #[sqlx(default)]
    pub depth: i16,
    /// The claim this worker holds: `available_at` as it set it, which changes if the claim passes on.
    #[sqlx(default)]
    pub lease: chrono::DateTime<chrono::Utc>,
    /// The zone of the request whose change this is, when that zone narrows staff (crate::zones, level 2):
    /// what acts on it for that request acts within the zone.
    #[sqlx(default)]
    pub zone: Option<String>,
}

/// Whether this consumer should act on this event, asked in the transaction that does it. A repeat, even
/// one arriving at the same moment, waits for the first to commit and is told no, and that it came stays
/// on record. If the first rolls back, nothing happened, and the repeat is told yes.
///
/// Before a call outside, which no transaction holds: claim (`first_time`), commit, then call. A repeat is told no,
/// for the earlier delivery may have reached the other side. Only when the call provably did nothing (refused before a
/// byte was sent, or answered with a definite "not accepted" such as 429) does the consumer [`give_back`] its claim, so
/// the retry is told yes again; never after a timeout or a connection dropped once the request was sent.
pub async fn first_time(tx: &mut Tx, tenant: Uuid, consumer: &str, event: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "insert into event_receipt (tenant_id, consumer, event_id, claimed) values ($1, $2, $3, true) \
         on conflict (tenant_id, consumer, event_id) do update set duplicates = event_receipt.duplicates + 1 returning duplicates = 0",
    )
    .bind(tenant)
    .bind(consumer)
    .bind(event)
    .fetch_one(&mut **tx)
    .await?)
}

/// Gives back this consumer's [`first_time`] claim on this event, so that the next `first_time` for it is told yes: for a
/// call outside that provably did nothing (refused before a byte was sent, or answered with a definite "not accepted"),
/// never for a timeout or a connection dropped after the request was sent, when the other side may have acted. Only a
/// claim goes, of exactly this workspace, consumer and event: never the dispatcher's record that the consumer finished.
/// Whether there was one to give back.
pub async fn give_back(tx: &mut Tx, tenant: Uuid, consumer: &str, event: i64) -> Result<bool> {
    Ok(sqlx::query("delete from event_receipt where tenant_id = $1 and consumer = $2 and event_id = $3 and claimed")
        .bind(tenant)
        .bind(consumer)
        .bind(event)
        .execute(&mut **tx)
        .await?
        .rows_affected()
        > 0)
}

/// A replica's event sinks at work (`Kernel::sinks`): while other workspaces' events wait for a turn, one sink may be
/// running in at most half of the dispatcher's turns, so one app's slow provider, called from many workspaces at once,
/// cannot hold them all. With nothing else waiting it may use them all. A call over the share waits a moment for one of
/// the sink's calls to end, so a fast sink that is merely busy goes on; only one whose calls are held is put off.
#[derive(Default)]
pub(crate) struct Sinks {
    /// Calls in progress, by sink.
    running: std::sync::Mutex<HashMap<String, usize>>,
    /// Calls put off because the sink was at its share, by sink, since the process started.
    deferred: std::sync::Mutex<BTreeMap<String, u64>>,
    /// Workspaces' shares on this replica ready to begin their next event and waiting for a turn: the work a sink past
    /// its share would keep waiting.
    waiting: AtomicUsize,
    /// Told when a call ends.
    ended: tokio::sync::Notify,
    /// The dispatcher's looks that found nothing to claim since the process started ([`Kernel::outbox_empty_looks`]).
    empty: std::sync::atomic::AtomicU64,
}

/// How long a call over its sink's share waits for one of the sink's calls to end before it is put off.
const SHARE_GRACE: Duration = Duration::from_millis(100);

/// A sink's call in progress, counted until it is dropped.
struct Slot<'a>(&'a Sinks, String);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if let Some(n) = self.0.running.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&self.1) {
            *n -= 1;
        }
        self.0.ended.notify_waiters();
    }
}

/// A share waiting for a turn, counted in [`Sinks::waiting`] until dropped.
struct Waiting<'a>(&'a Sinks);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Sinks {
    fn waiting(&self) -> Waiting<'_> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        Waiting(self)
    }

    /// A call of `sink` may begin, out of `share`: below it, or with no share waiting for a turn. Over it, it waits
    /// [`SHARE_GRACE`] for one of the sink's calls to end; if none does, it is counted as put off.
    async fn slot(&self, sink: &str, share: usize) -> Option<Slot<'_>> {
        let until = tokio::time::Instant::now() + SHARE_GRACE;
        loop {
            let ended = self.ended.notified();
            {
                let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
                let n = running.entry(sink.to_string()).or_default();
                if *n < share || self.waiting.load(Ordering::SeqCst) == 0 {
                    *n += 1;
                    return Some(Slot(self, sink.to_string()));
                }
            }
            if tokio::time::timeout_at(until, ended).await.is_err() {
                *self.deferred.lock().unwrap_or_else(|e| e.into_inner()).entry(sink.to_string()).or_default() += 1;
                return None;
            }
        }
    }
}

/// How long a claimed batch is reserved for the worker that claimed it: what it has not begun by then may pass to
/// another replica, so a replica that dies holding a batch delays it this long at most.
const CLAIM_SECS: f64 = 30.0;
/// How long an event is its worker's once begun: what its handlers may take, and the claim's time again to say how it went.
const EVENT_LEASE_SECS: f64 = EVENT_TIMEOUT.as_secs() as f64 + CLAIM_SECS;
/// The same for webhooks: requests run side by side and time out after 10 seconds.
const WEBHOOK_LEASE_SECS: f64 = 60.0;

/// Why a dead event or delivery of a record erased since is not replayed: it would go out without the record.
const ERASED_SINCE: &str = "the record this is about has been erased since: it is not sent again";

fn require_admin(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("this is for administrators".into())) }
}

fn backoff(attempts: i32) -> f64 {
    // 5s, 10s, 20s ... capped at 1h, less up to a fifth at random, so that what failed together
    // does not all come back together.
    (5i64 << attempts.clamp(0, 10)).min(3600) as f64 * (1.0 - rand::random::<f64>() / 5.0)
}

impl Kernel {
    /// A turn at the pool for the dispatcher or the webhook sender ([`Kernel::background`]), held while one claim, event
    /// or webhook's secret holds a connection: together they never hold more than their share.
    async fn turn(&self) -> Result<tokio::sync::SemaphorePermit<'_>> {
        self.background.acquire().await.map_err(Error::internal)
    }

    /// How many times this replica's dispatcher has looked for events and found none to take since the process started
    /// (held, or none waiting and due): what an idle replica costs its database.
    pub fn outbox_empty_looks(&self) -> u64 {
        self.sinks.empty.load(Ordering::SeqCst)
    }

    /// How many times each event sink was put off at its share of this replica's turns since the process started ([`Sinks`]),
    /// for `/metrics` beside the outbox's figures: one that keeps growing is a sink whose provider is slow.
    pub fn sink_deferrals(&self) -> Vec<(String, u64)> {
        self.sinks.deferred.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Claims and handles one batch. Returns how many events were processed.
    ///
    /// The claim is a lease: a short transaction moves the batch's `available_at` half a minute
    /// into the future, then each event is taken for its handlers' time as it begins, handled
    /// outside any long transaction (handlers call webhooks, SMTP servers, plugins), and its outcome
    /// recorded on its own. A worker that dies mid-batch leaves leases that simply expire, and
    /// another replica picks the events up — delivery stays at least once, never stuck behind a lock.
    pub async fn dispatch_outbox_once(&self) -> Result<usize> {
        let (_running, stopping) = watch::channel(false);
        let batch = self.claim_events(&[]).await?;
        let claimed = batch.iter().map(Vec::len).sum();
        for outcome in futures::future::join_all(batch.into_iter().map(|events| self.handle_events(events, &stopping))).await {
            outcome?;
        }
        Ok(claimed)
    }

    /// Claims a batch, each workspace's share apart, leaving out the workspaces in `busy` (this replica is still at their
    /// earlier events).
    async fn claim_events(&self, busy: &[Uuid]) -> Result<Vec<Vec<Event>>> {
        match self.claim_events_now(busy).await {
            // A planned move makes the database read-only before the claim could say it holds.
            Err(_) if self.holding().await? => {
                self.sinks.empty.fetch_add(1, Ordering::SeqCst);
                Ok(vec![])
            }
            claimed => claimed,
        }
    }

    async fn claim_events_now(&self, busy: &[Uuid]) -> Result<Vec<Vec<Event>>> {
        // Held after a restore or for a move: asked in the claim itself (Kernel::holding).
        if self.db.fenced() || self.marker_waiting() {
            self.sinks.empty.fetch_add(1, Ordering::SeqCst);
            return Ok(vec![]);
        }
        let turn = self.turn().await?;
        let mut tx = self.db.system_tx().await?;
        // A loose index scan finds the workspaces with work, however long their backlogs. They come
        // in random order, so that more workspaces than a batch holds all get their turn.
        // A newer build's events wait for a replica of it: their topics may be ones this build does not know, which it
        // would mark done or send to webhooks that the newer build keeps them from (window.rs). Everything else goes on,
        // so a newer event may be handled after later ones of its workspace (docs/events.md, Order). Each workspace's
        // share comes most urgent first, then oldest, the order handle_events keeps.
        let events: Vec<Event> = sqlx::query_as(
            "with recursive waiting(tenant_id) as ( \
               (select tenant_id from outbox where processed_at is null and not dead order by tenant_id limit 1) \
               union all \
               select (select o.tenant_id from outbox o where o.tenant_id > w.tenant_id and o.processed_at is null and not o.dead \
                       order by o.tenant_id limit 1) \
               from waiting w where w.tenant_id is not null), \
             taken as (update outbox set available_at = now() + make_interval(secs => $1) where id in ( \
               select claimed.id from (select tenant_id from waiting where tenant_id is not null and not ferrum_workspace_held(tenant_id) \
                 and not exists (select 1 from platform_incarnation where held_since is not null) and not exists (select 1 from platform_move) \
                 and tenant_id <> all($5) order by random()) w \
               cross join lateral (select o.id from outbox o where o.tenant_id = w.tenant_id and o.processed_at is null and not o.dead \
                 and o.available_at <= now() and o.build_date <= $4 \
                 order by o.priority, o.id limit $2 for update skip locked) claimed \
               limit $3) \
             returning id, tenant_id, topic, entity, record_id, payload, attempts, correlation_id, traceparent, actor, legal_entity, source_version, schema_version, depth, available_at as lease, zone, priority) \
             select * from taken order by priority, id",
        )
        .bind(CLAIM_SECS)
        .bind(TENANT_SHARE)
        .bind(BATCH)
        .bind(crate::window::build_date())
        .bind(busy)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        drop(turn);
        let mut by_tenant: BTreeMap<Uuid, Vec<Event>> = BTreeMap::new();
        for ev in events {
            by_tenant.entry(ev.tenant_id).or_default().push(ev);
        }
        if by_tenant.is_empty() {
            self.sinks.empty.fetch_add(1, Ordering::SeqCst);
        }
        Ok(by_tenant.into_values().collect())
    }

    /// One workspace's share of a batch, one event at a time, in the order the claim gave it: most urgent first, then oldest.
    async fn handle_events(&self, events: Vec<Event>, stopping: &watch::Receiver<bool>) -> Result<()> {
        // The sinks put off for an event of this share are put off for the rest of it, so each sees them in order.
        let put_off = std::sync::Mutex::new(Vec::new());
        for (begun, ev) in events.iter().enumerate() {
            // Workspaces are handled side by side, as many at once as there are turns.
            let _turn = {
                let _waiting = self.sinks.waiting();
                self.turn().await?
            };
            // The replica is stopping: what it has not begun goes back now, not when the claim runs out.
            if *stopping.borrow() {
                let rest: Vec<i64> = events[begun..].iter().map(|e| e.id).collect();
                let mut tx = self.db.system_tx().await?;
                sqlx::query("update outbox set available_at = now() where id = any($1) and available_at = $2")
                    .bind(&rest)
                    .bind(ev.lease)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(());
            }
            // The claim covered the batch, and earlier events may have used it up: take this one
            // afresh. If it has passed to another worker meanwhile, it is theirs, and running it
            // here too would do the work twice. Once a move is planned, the rest of the batch waits for its copy.
            let mut tx = self.db.system_tx().await?;
            let lease: Option<(chrono::DateTime<chrono::Utc>, bool)> = sqlx::query_as(
                "update outbox set available_at = now() + make_interval(secs => $3) \
                 where id = $1 and available_at = $2 and processed_at is null and not dead \
                   and not exists (select 1 from platform_move) and not ferrum_workspace_held(tenant_id) \
                 returning available_at, attempts = 0 and last_error is null",
            )
            .bind(ev.id)
            .bind(ev.lease)
            .bind(EVENT_LEASE_SECS)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            // `first`: never tried before, nor put off.
            let Some((lease, first)) = lease else { continue };
            // A sandbox sends nothing outside: what it queued for outside is set aside, saying so (sandbox.rs).
            if let Some(why) = self.sandbox_holds(ev).await? {
                self.set_event_aside(ev.id, lease, &why).await?;
                continue;
            }
            if let Some(why) = self.chain_stops(ev, first).await? {
                // Stopped before any handler runs, and kept where administrators look, saying why.
                tracing::warn!(event = ev.id, topic = %ev.topic, "event chain stopped: {why}");
                let mut tx = self.db.system_tx().await?;
                sqlx::query("update outbox set dead = true, last_error = $2 where id = $1 and available_at = $3")
                    .bind(ev.id)
                    .bind(&why)
                    .bind(lease)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                continue;
            }
            let trace = Trace {
                correlation: ev.correlation_id.clone().unwrap_or_else(|| format!("event-{}", ev.id)),
                causation: Some(ev.id),
                traceparent: ev.traceparent.clone(),
                priority: None,
                depth: ev.depth.saturating_add(1),
                zone: Trace::in_zone(ev.zone.clone()),
                ..Default::default()
            };
            // The sinks that handle it, kept here so that they are known however the attempt ends.
            let handled = std::sync::Mutex::new(Vec::new());
            let outcome =
                trace.scope(tokio::time::timeout(EVENT_TIMEOUT, crate::sandbox::in_hand(ev.tenant_id, self.handle_event(ev, first, &handled, &put_off)))).await;
            let outcome = outcome.unwrap_or_else(|_| Err(Error::Unavailable(format!("handlers took longer than {} s", EVENT_TIMEOUT.as_secs()))));
            let mut tx = self.db.system_tx().await?;
            match outcome {
                Ok(()) => {
                    sqlx::query("update outbox set processed_at = now(), last_error = null where id = $1 and available_at = $2")
                        .bind(ev.id)
                        .bind(lease)
                        .execute(&mut *tx)
                        .await?;
                }
                Err(e) => {
                    // Put off for a sink's share of the turns: not an attempt, and back soon.
                    let (attempts, delay) = match &e {
                        Error::Wait(_, secs) => (ev.attempts, *secs as f64),
                        _ => (ev.attempts + 1, backoff(ev.attempts + 1)),
                    };
                    if attempts > ev.attempts {
                        tracing::warn!(event = ev.id, topic = %ev.topic, attempts, "outbox handler failed: {e}");
                    }
                    sqlx::query(
                        "update outbox set attempts = $2, last_error = $3, dead = $4, \
                         available_at = now() + make_interval(secs => $5) where id = $1 and available_at = $6",
                    )
                    .bind(ev.id)
                    .bind(attempts)
                    .bind(e.to_string())
                    .bind(attempts >= MAX_ATTEMPTS)
                    .bind(delay)
                    .bind(lease)
                    .execute(&mut *tx)
                    .await?;
                    // Those that handled it are not called again when it is tried again or replayed (handle_event).
                    let handled = handled.into_inner().unwrap_or_else(|e| e.into_inner());
                    // A sink that claimed it through first_time under its own name has finished: the claim is no longer given back.
                    sqlx::query(
                        "insert into event_receipt (tenant_id, consumer, event_id) select $1, unnest($2::text[]), $3 \
                         on conflict (tenant_id, consumer, event_id) do update set claimed = false",
                    )
                    .bind(ev.tenant_id)
                    .bind(handled)
                    .bind(ev.id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            tx.commit().await?;
        }
        Ok(())
    }

    /// Why an event's chain stops here, if it does: it is too many steps from the change that started it,
    /// or that change has been followed by too many events. Checked here, before any handler, so it holds
    /// whichever apps, automations, plugins or processes answer one another (FLC-HOOK-008). A request's or
    /// job's own events never count: an import of ten thousand rows is one step, not a loop.
    async fn chain_stops(&self, ev: &Event, first: bool) -> Result<Option<String>> {
        if ev.depth == 0 {
            return Ok(None);
        }
        let correlation = ev.correlation_id.clone().unwrap_or_else(|| format!("event-{}", ev.id));
        let mut tx = self.db.system_tx().await?;
        // Counted once per event, not once per attempt.
        let events: i32 = if first {
            sqlx::query_scalar(
                "insert into event_chain (tenant_id, correlation_id, events) values ($1, $2, 1) \
                 on conflict (tenant_id, correlation_id) do update set events = event_chain.events + 1 returning events",
            )
            .bind(ev.tenant_id)
            .bind(&correlation)
            .fetch_one(&mut *tx)
            .await?
        } else {
            sqlx::query_scalar("select events from event_chain where tenant_id = $1 and correlation_id = $2")
                .bind(ev.tenant_id)
                .bind(&correlation)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or(0)
        };
        let (depth, budget) = (self.settings.chain_depth, self.settings.chain_events);
        let why = if ev.depth >= depth {
            format!("stopped: this event is {} steps from the change that started its chain (at most {depth})", ev.depth)
        } else if events > budget {
            format!("stopped: {events} events have followed from one change (at most {budget})")
        } else {
            tx.commit().await?;
            return Ok(None);
        };
        // The chain as it went, oldest first, from each event to the one its writer was answering.
        let path: Vec<String> = sqlx::query_scalar(
            "with recursive up(id, topic, causation_id, n) as ( \
               select id, topic, causation_id, 0 from outbox where id = $1 \
               union all select o.id, o.topic, o.causation_id, up.n + 1 from outbox o join up on o.id = up.causation_id where up.n < 12) \
             select topic from up order by n desc",
        )
        .bind(ev.id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(format!(
            "{why}. Chain {correlation}: {}. Something answers its own changes: correct the rule, automation, plugin or app that does, then replay this event.",
            path.join(" → ")
        )))
    }

    /// The events this workspace's handlers gave up on, newest first: what they are and why, never
    /// their payloads, which can hold anything (administrators only).
    pub async fn dead_events(&self, ctx: &Ctx, limit: i64) -> Result<Vec<Value>> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'topic', topic, 'entity', entity, 'record_id', record_id, 'attempts', attempts, \
               'last_error', last_error, 'created_at', created_at, 'correlation_id', correlation_id) \
             from outbox where tenant_id = $1 and dead order by id desc limit $2",
        )
        .bind(ctx.tenant)
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Queues a dead event again, from the first attempt. The automations and sinks that handled it
    /// before it failed are not repeated (`automation_run`, `event_receipt`). One whose record has been erased since is not:
    /// it would go out without the record (crate::privacy). Audited (administrators only).
    pub async fn replay_event(&self, ctx: &Ctx, id: i64) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let erased: Option<bool> = sqlx::query_scalar("select payload ? 'erased' from outbox where tenant_id = $1 and id = $2 and dead")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if erased == Some(true) {
            return Err(Error::Conflict(ERASED_SINCE.into()));
        }
        // Replayed by a person's decision, it starts a chain of its own: its depth and its chain's count
        // start again, so it is not stopped at once for what it was stopped for before.
        let topic: Option<String> = sqlx::query_scalar(
            "with chain as (delete from event_chain c using outbox o where o.tenant_id = $1 and o.id = $2 and o.dead \
               and c.tenant_id = o.tenant_id and c.correlation_id = coalesce(o.correlation_id, 'event-' || o.id)) \
             update outbox set dead = false, attempts = 0, last_error = null, depth = 0, available_at = now() \
             where tenant_id = $1 and id = $2 and dead returning topic",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(topic) = topic else { return Err(Error::NotFound("dead event".into())) };
        crate::audit::record_admin(&mut tx, ctx, "OutboxEvent", Uuid::from_u128(id as u128), "replay", None, json!({"event": id, "topic": topic})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Everything one operation set off in this workspace, found by the id a person quotes (the request's,
    /// which its events, jobs, process runs, effects and deliveries carry): what each is, how it stands and
    /// why, never what it carried (administrators only, FEO-079).
    pub async fn operation(&self, ctx: &Ctx, id: &str) -> Result<Value> {
        require_admin(ctx)?;
        let id = id.trim();
        if id.is_empty() || id.len() > 100 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::field("id", "an operation's id, as a person was shown it"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let all = |sql: &'static str| sqlx::query_scalar::<_, Value>(sql).bind(ctx.tenant).bind(id);
        let events = all("select jsonb_build_object('id', id, 'topic', topic, 'entity', entity, 'record_id', record_id, \
               'state', case when dead then 'dead' when processed_at is not null then 'done' else 'waiting' end, \
               'attempts', attempts, 'last_error', last_error, 'created_at', created_at) \
             from outbox where tenant_id = $1 and correlation_id = $2 order by id limit 200")
        .fetch_all(&mut *tx)
        .await?;
        let jobs = all("select jsonb_build_object('id', id, 'kind', kind, 'status', status, 'attempts', attempts, 'last_error', last_error, \
               'created_at', created_at, 'finished_at', finished_at) from job where tenant_id = $1 and correlation_id = $2 order by created_at limit 200")
        .fetch_all(&mut *tx)
        .await?;
        let runs = all("select jsonb_build_object('id', id, 'solution', solution, 'process', process, 'status', status, 'accepted_at', accepted_at, \
               'finished_at', finished_at) from logic_run where tenant_id = $1 and correlation_id = $2 order by accepted_at limit 200")
        .fetch_all(&mut *tx)
        .await?;
        let effects = all("select jsonb_build_object('capability', capability, 'status', status, 'record_entity', record_entity, 'record_id', record_id, \
               'caller', caller, 'created_at', created_at) from logic_effect where tenant_id = $1 and correlation_id = $2 order by created_at limit 200")
        .fetch_all(&mut *tx)
        .await?;
        let deliveries = all("select jsonb_build_object('id', d.id, 'webhook_name', w.name, 'topic', d.topic, 'status', d.status, 'attempts', d.attempts, \
               'response_code', d.response_code, 'last_error', d.last_error) \
             from webhook_delivery d join outbox o on o.id = d.outbox_id join webhook w on w.tenant_id = d.tenant_id and w.id = d.webhook_id \
             where d.tenant_id = $1 and o.correlation_id = $2 order by d.created_at limit 200")
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(json!({"operation": id, "events": events, "jobs": jobs, "runs": runs, "effects": effects, "deliveries": deliveries}))
    }

    /// The webhook deliveries that gave up, newest first: where each was going and why, never its
    /// payload, which can hold anything (administrators only).
    pub async fn dead_webhook_deliveries(&self, ctx: &Ctx, limit: i64) -> Result<Vec<Value>> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', d.id, 'webhook', d.webhook_id, 'webhook_name', w.name, 'topic', d.topic, 'attempts', d.attempts, \
               'response_code', d.response_code, 'last_error', d.last_error, 'created_at', d.created_at, 'updated_at', d.updated_at) \
             from webhook_delivery d join webhook w on w.tenant_id = d.tenant_id and w.id = d.webhook_id \
             where d.tenant_id = $1 and d.status = 'dead' order by d.updated_at desc, d.id limit $2",
        )
        .bind(ctx.tenant)
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Queues a webhook delivery that gave up again, from the first attempt (administrators only); not one whose record
    /// has been erased since, as [`Kernel::replay_event`].
    pub async fn replay_webhook_delivery(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let erased: Option<bool> = sqlx::query_scalar("select payload ? 'erased' from webhook_delivery where tenant_id = $1 and id = $2 and status = 'dead'")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if erased == Some(true) {
            return Err(Error::Conflict(ERASED_SINCE.into()));
        }
        let topic: Option<String> = sqlx::query_scalar(
            "update webhook_delivery set status = 'pending', attempts = 0, last_error = null, next_attempt_at = now(), updated_at = now() \
             where tenant_id = $1 and id = $2 and status = 'dead' returning topic",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(topic) = topic else { return Err(Error::NotFound("dead webhook delivery".into())) };
        crate::audit::record_admin(&mut tx, ctx, "WebhookDelivery", id, "replay", None, json!({"topic": topic})).await?;
        tx.commit().await?;
        wake(&WEBHOOKS);
        Ok(())
    }

    /// Hands an event to everything that consumes it: webhooks, followers, automations and the event sinks. Each
    /// automation and each sink stands alone: one that fails makes the event fail for itself, the rest still run. The
    /// names of the sinks that handled it go into `handled`, which the caller records if the event is to be tried again.
    /// A sink at its share of the turns ([`Sinks`]) is put off: the event comes back for it after a few seconds, which is
    /// not an attempt (`Error::Wait`), and later events of the share put it off too (`put_off`).
    async fn handle_event(&self, ev: &Event, first: bool, handled: &std::sync::Mutex<Vec<String>>, put_off: &std::sync::Mutex<Vec<String>>) -> Result<()> {
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
        if ev.topic == crate::privacy::ERASED_TOPIC {
            return self.journal_erasure(ev.tenant_id, ev.id, &ev.payload).await;
        }
        if ev.topic == crate::lifecycle::ACCESS_TOPIC {
            return self.journal_access(ev.tenant_id, ev.id, &ev.payload).await;
        }
        if ev.topic == crate::webpush::PUSH_TOPIC {
            return self.push_notification(ev.tenant_id, &ev.payload).await;
        }
        if ev.topic == MAIL_TOPIC {
            let mailer = self.mailer.as_ref().ok_or_else(|| Error::Unavailable("no mailer is configured; queued mail waits for one".into()))?;
            let field = |k: &str| ev.payload.get(k).and_then(Value::as_str).unwrap_or_default();
            // Asked again as it goes: a restriction or a withdrawal since it was queued stops it (FEO-015).
            let purpose: crate::retention::Purpose = ev.payload.get("purpose").cloned().and_then(|p| serde_json::from_value(p).ok()).unwrap_or_default();
            let mut tx = self.db.tenant_tx(ev.tenant_id).await?;
            let may = self.may_contact_for(&mut tx, ev.tenant_id, field("to"), purpose).await?;
            tx.commit().await?;
            let send = ev.payload.get("send").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
            if !may {
                tracing::info!(tenant = %ev.tenant_id, ?purpose, "a queued mail was not sent: its recipient may no longer be contacted for it");
                return self.mail_settled(ev.tenant_id, send, "skipped", Some("they may no longer be contacted for it")).await;
            }
            let attachments = self.mail_attachments(ev.tenant_id, &ev.payload).await?;
            let Some(sealed) = ev.payload.get("sealed") else {
                mailer.send_with(field("to"), field("subject"), field("html"), &attachments).await?;
                return self.mail_settled(ev.tenant_id, send, "sent", None).await;
            };
            let (open, _) = self.open_message(ev.tenant_id, sealed).await?;
            let text = |k: &str| open[k].as_str().unwrap_or_default().to_string();
            mailer.send_with(field("to"), &text("subject"), &text("html"), &attachments).await?;
            // Handed over: what stays in the outbox is the placeholder alone.
            let mut tx = self.db.system_tx().await?;
            sqlx::query("update outbox set payload = payload - 'sealed' where id = $1").bind(ev.id).execute(&mut *tx).await?;
            tx.commit().await?;
            return self.mail_settled(ev.tenant_id, send, "sent", None).await;
        }
        // Its record was erased after the change and before this (crate::privacy): the event no longer carries it, and no
        // webhook, follower, automation or sink is handed a change to a record it cannot show. It is handled, as nothing.
        if ev.payload.get("erased") == Some(&Value::Bool(true)) {
            return Ok(());
        }
        self.queue_deliveries(ev).await?;
        if ev.topic == crate::lifecycle::DEPROVISIONED_TOPIC {
            self.hand_over(ev).await?;
        }
        // Followers hear about changes made by others; first and best effort:
        // a retry must not notify twice.
        if let (Some(entity), Some(id)) = (&ev.entity, ev.record_id) {
            let event = ev.payload.get("event").and_then(Value::as_str).unwrap_or("updated");
            let actor = ev.payload.get("actor").and_then(Value::as_str).and_then(|s| s.parse().ok());
            let doc = ev.payload.get("doc").and_then(Value::as_object);
            if first && let Err(e) = self.notify_followers(ev.tenant_id, entity, id, event, actor, doc).await {
                tracing::warn!(topic = %ev.topic, "follower notifications failed: {e}");
            }
        }
        // The workspace's sinks: an app's only where one of its packages is installed.
        let schema = self.registry.schema(ev.tenant_id).await?;
        let sinks: Vec<_> = self.hooks.event_sinks(&schema.packages).collect();
        let mut tx = self.db.tenant_tx(ev.tenant_id).await?;
        // An audit event (crate::audit) goes only to webhooks that name its topic or `audit.*`, never to `*` or an app's:
        // it holds people's data.
        let entity_wildcard = format!("{}.*", ev.topic.split('.').next().unwrap_or_default());
        // An app's webhook hears which record, never what it holds (nor what a change replaced): its
        // service reads that with its own key, as far as its rights go.
        let queued = sqlx::query(
            "insert into webhook_delivery (tenant_id, id, webhook_id, outbox_id, topic, payload) \
             select w.tenant_id, gen_random_uuid(), w.id, $2, $3, case when w.app is null then $4 else $4 - 'doc' - 'was' end from webhook w \
             where w.tenant_id = $1 and w.is_active and ($3 = any(w.topics) or ('*' = any(w.topics) and $3 not like 'audit.%') or $5 = any(w.topics)) \
               and (w.app is null or $3 not like 'audit.%') \
             on conflict (tenant_id, webhook_id, outbox_id) do nothing",
        )
        .bind(ev.tenant_id)
        .bind(ev.id)
        .bind(&ev.topic)
        .bind(&ev.payload)
        .bind(entity_wildcard)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // Those that handled it on an attempt that failed, or before it was dead and replayed.
        let names: Vec<&str> = sinks.iter().map(|(name, _)| *name).collect();
        let before: Vec<String> = sqlx::query_scalar("select consumer from event_receipt where tenant_id = $1 and consumer = any($2) and event_id = $3")
            .bind(ev.tenant_id)
            .bind(&names)
            .bind(ev.id)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        // Committed: the webhook sender of this process has work.
        if queued > 0 {
            wake(&WEBHOOKS);
        }
        self.set_deliveries_aside(ev).await?;
        // Automations keep their own record of each run (`automation_run`): one that ran is not run again.
        let mut failed = Vec::new();
        if let Err(e) = crate::automation::run_for_event(self, ev).await {
            failed.push(format!("automations: {e}"));
        }
        let share = (crate::db::background_connections(self.db.config.max_connections) as usize / 2).max(1);
        let mut waiting = Vec::new();
        for (name, sink) in sinks.into_iter().filter(|(name, _)| !before.iter().any(|b| b == name)) {
            let off = put_off.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|o| o == name);
            let slot = if off { None } else { self.sinks.slot(name, share).await };
            if slot.is_none() && !off {
                put_off.lock().unwrap_or_else(|e| e.into_inner()).push(name.to_string());
            }
            let Some(_slot) = slot else {
                waiting.push(name);
                continue;
            };
            match sink.on_event(self, ev).await {
                Ok(()) => handled.lock().unwrap_or_else(|e| e.into_inner()).push(name.to_string()),
                Err(e) => failed.push(format!("{name}: {e}")),
            }
        }
        if !failed.is_empty() {
            Err(Error::Unavailable(failed.join("; ")))
        } else if !waiting.is_empty() {
            Err(Error::Wait(format!("waiting for a turn: {} at its share of this replica's", waiting.join(", ")), DEFER_SECS))
        } else {
            Ok(())
        }
    }

    /// Drains the outbox until shutdown. Each workspace's share of a claim is handled in a task of its own, and the next
    /// claim does not wait for the slowest share: it leaves out the workspaces still in hand, so each one's events stay one
    /// at a time and in order, and it is made while fewer shares are in hand than the replica has turns. A sink held at
    /// a provider for its 120 seconds so holds its own workspace and one turn, not the replica. An event written by this
    /// process wakes it; with nothing to do it looks less and less often, up to every [`Pace::CAP`]. Told to stop, each
    /// share finishes the event it is at and hands back the rest.
    pub fn spawn_outbox_dispatcher(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        type Ended = std::result::Result<(tokio::task::Id, Result<()>), tokio::task::JoinError>;
        fn ended(done: Ended, in_hand: &mut HashMap<tokio::task::Id, Uuid>) {
            let id = match done {
                Ok((id, Ok(()))) => id,
                Ok((id, Err(e))) => {
                    tracing::warn!("outbox dispatch error: {e}");
                    id
                }
                Err(e) => {
                    tracing::warn!("outbox dispatch error: {e}");
                    e.id()
                }
            };
            in_hand.remove(&id);
        }
        let k = self.clone();
        let mut woken = EVENTS.subscribe();
        tokio::spawn(async move {
            let room = crate::db::background_connections(k.db.config.max_connections) as usize;
            let mut running = tokio::task::JoinSet::new();
            let mut in_hand = HashMap::new();
            let mut pace = Pace::new();
            // When it looks next: at once to begin with.
            let mut next = tokio::time::Instant::now();
            while !*shutdown.borrow() {
                while let Some(done) = running.try_join_next_with_id() {
                    ended(done, &mut in_hand);
                }
                if running.len() < room && tokio::time::Instant::now() >= next {
                    let mut claimed = 0;
                    let busy: Vec<Uuid> = in_hand.values().copied().collect();
                    match k.claim_events(&busy).await {
                        Ok(batch) => {
                            for events in batch {
                                claimed += events.len();
                                let (k, stopping, tenant) = (k.clone(), shutdown.clone(), events[0].tenant_id);
                                in_hand.insert(running.spawn(async move { k.handle_events(events, &stopping).await }).id(), tenant);
                            }
                        }
                        Err(e) => tracing::warn!("outbox dispatch error: {e}"),
                    }
                    // A full batch: claim again at once while there is room (a share that ends brings the next look too).
                    // Otherwise after the pace's wait.
                    let now = tokio::time::Instant::now();
                    next = if claimed as i64 >= BATCH && running.len() < room { now } else { now + pace.after(claimed > 0) };
                    continue;
                }
                tokio::select! {
                    changed = shutdown.changed() => if changed.is_err() { break },
                    // A share is done: there is room again, and its workspace may be claimed at the next look.
                    Some(done) = running.join_next_with_id(), if !running.is_empty() => {
                        ended(done, &mut in_hand);
                        next = tokio::time::Instant::now();
                    }
                    _ = woken.changed() => next = next.min(tokio::time::Instant::now() + pace.woken()),
                    _ = tokio::time::sleep_until(next), if running.len() < room => {}
                }
            }
            while let Some(done) = running.join_next_with_id().await {
                ended(done, &mut in_hand);
            }
        })
    }

    /// Sends one batch of webhooks. Like the outbox, the claim is a lease and no transaction stays
    /// open across a request: a slow endpoint holds no row, no connection and no turn at the pool,
    /// and the batch's requests run side by side, so it delays no other endpoint either.
    pub async fn deliver_webhooks_once(&self) -> Result<usize> {
        match self.deliver_webhooks_now().await {
            // A planned move makes the database read-only before the claim could say it holds.
            Err(_) if self.holding().await? => Ok(0),
            sent => sent,
        }
    }

    async fn deliver_webhooks_now(&self) -> Result<usize> {
        // Held after a restore or for a move: asked in the claim itself (Kernel::holding).
        if self.db.fenced() || self.marker_waiting() {
            return Ok(0);
        }
        #[derive(sqlx::FromRow)]
        struct Due {
            tenant_id: Uuid,
            id: Uuid,
            webhook_id: Uuid,
            topic: String,
            payload: Value,
            attempts: i32,
            url: String,
        }
        let turn = self.turn().await?;
        let mut tx = self.db.system_tx().await?;
        let due: Vec<Due> = sqlx::query_as(
            "with claimed as (select tenant_id, id from webhook_delivery where status = 'pending' and next_attempt_at <= now() \
               and not ferrum_workspace_held(tenant_id) and not exists (select 1 from platform_incarnation where held_since is not null) and not exists (select 1 from platform_move) \
               order by next_attempt_at limit 20 for update skip locked), \
             leased as (update webhook_delivery d set next_attempt_at = now() + make_interval(secs => $1) from claimed c \
               where d.tenant_id = c.tenant_id and d.id = c.id returning d.tenant_id, d.id, d.webhook_id, d.topic, d.payload, d.attempts) \
             select l.tenant_id, l.id, l.webhook_id, l.topic, l.payload, l.attempts, w.url from leased l \
             join webhook w on w.tenant_id = l.tenant_id and w.id = l.webhook_id",
        )
        .bind(WEBHOOK_LEASE_SECS)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        drop(turn);
        let outcomes = futures::future::join_all(due.iter().map(|d| async move {
            // A turn at the pool while it reads what it needs, never while it waits for the endpoint.
            let secret = {
                let _turn = self.turn().await.map_err(|e| (None, e.to_string()))?;
                // Nothing goes out for a sandbox (sandbox.rs).
                self.outward(d.tenant_id).await.map_err(|e| (None, e.to_string()))?;
                // One whose secret cannot be opened is not sent unsigned: it fails, and says why.
                self.webhook_secret(d.tenant_id, d.webhook_id).await.map_err(|e| (None, e.to_string()))?
            };
            self.post_webhook(&d.url, &secret, &d.topic, d.id, &d.payload).await
        }))
        .await;
        let _turn = self.turn().await?;
        let mut tx = self.db.system_tx().await?;
        for (d, outcome) in due.iter().zip(outcomes) {
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
                    let dead = attempts >= WEBHOOK_MAX_ATTEMPTS || err.contains(crate::sandbox::SANDBOX);
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
        let sig = signature(secret, chrono::Utc::now().timestamp(), &body).map_err(|e| (None, e.to_string()))?;
        let resp = client
            .post(parsed)
            .header("content-type", "application/json")
            .header("x-ferrum-event", topic)
            .header("x-ferrum-delivery", delivery.to_string())
            .header("x-ferrum-signature", sig)
            .body(body)
            .send()
            .await
            .map_err(|e| (None, e.to_string()))?;
        let code = resp.status().as_u16();
        if resp.status().is_success() { Ok(code) } else { Err((Some(code), format!("HTTP {code}"))) }
    }

    /// Sends webhooks until shutdown: woken by this process's deliveries, and with nothing to send looking less and less
    /// often, up to every [`Pace::CAP`] (deliveries due again, or queued by another replica).
    pub fn spawn_webhook_sender(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let k = self.clone();
        let mut woken = WEBHOOKS.subscribe();
        tokio::spawn(async move {
            let mut pace = Pace::new();
            loop {
                let found = match k.deliver_webhooks_once().await {
                    Ok(n) => n > 0,
                    Err(e) => {
                        tracing::warn!("webhook delivery error: {e}");
                        false
                    }
                };
                if !rest(pace.after(found), &mut pace, &mut woken, &mut shutdown).await {
                    return;
                }
            }
        })
    }

    /// Deletes processed outbox rows, delivered webhooks, the receipts of events that will not be handled again, and expired
    /// idempotency keys.
    pub async fn purge_old_events(&self, keep_days: i32) -> Result<u64> {
        let past = |column: &str, days: i32| format!("{column} < now() - make_interval(days => {days}) and not ferrum_workspace_held(tenant_id)");
        self.db
            .purge(&[
                ("outbox", past("processed_at", keep_days)),
                ("webhook_delivery", format!("status = 'delivered' and {}", past("updated_at", keep_days))),
                // A receipt stays while its event may still be handled (waiting, or dead and replayable): it is what
                // keeps a consumer that handled it from being called again.
                (
                    "event_receipt",
                    format!(
                        "{} and not exists (select 1 from outbox o where o.id = event_receipt.event_id and o.processed_at is null)",
                        past("received_at", keep_days)
                    ),
                ),
                // A receipt answers a request sent again for as long as a device's work may wait offline.
                ("idempotency_key", past("created_at", crate::receipt::RECEIPT_DAYS)),
                ("event_chain", past("first_at", keep_days)),
            ])
            .await
    }
}

/// An address nothing the server sends out reaches unless the operator allows it: private, loopback, link-local (the
/// cloud's metadata addresses among them), shared, multicast, reserved, benchmarking, and the v6 forms that carry one
/// of them (mapped, IPv4-compatible, NAT64). The one check for every way out: webhooks, the vetted client, plugins'
/// `http.fetch`, connections, and apps' own sockets.
pub fn is_private(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let carried = std::net::Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || ((s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || s[..6] == [0; 6]) && is_private(IpAddr::V4(carried)))
        }
    }
}

/// `t=<ts>,v1=<hex HMAC-SHA256 of '<ts>.<body>'>`: what `X-Ferrum-Signature` carries.
pub fn signature(secret: &str, ts: i64, body: &[u8]) -> Result<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(Error::internal)?;
    mac.update(format!("{ts}.").as_bytes());
    mac.update(body);
    Ok(format!("t={ts},v1={}", hex::encode(mac.finalize().into_bytes())))
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
        for ip in [
            // The cloud's metadata address through NAT64, as an IPv6-only cluster reaches it.
            "64:ff9b::a9fe:a9fe",
            "::127.0.0.1",
            "::ffff:198.18.0.1",
            "ff02::1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "192.0.0.170",
            "0.0.0.0",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "172.16.5.4",
            "169.254.169.254",
            "100.64.0.1",
            "100.100.100.200",
            "::1",
            "fe80::1",
            "fd00::1",
            "fd00:ec2::254",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700::1111", "64:ff9b::101:101", "::ffff:1.1.1.1"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn retries_double_up_to_an_hour_and_spread_out() {
        for _ in 0..200 {
            assert!((8.0..=10.0).contains(&backoff(1)));
            assert!((2880.0..=3600.0).contains(&backoff(30)));
        }
        assert_ne!(backoff(30), backoff(30), "what failed together does not come back together");
    }
}
