//! WebSocket connections held by plugins (device protocols such as OCPP, live feeds). Each
//! replica keeps its own sockets; a message for a socket on another replica travels through
//! PostgreSQL `NOTIFY`, so plugins address connections by id without knowing where they live.
//!
//! This is an interactive channel, not a command channel. A successful send means the message was
//! queued or published, never that a device received it, let alone acted on it. Work with a
//! physical effect goes through the command ledger (`ferrum-ops`).

use dashmap::DashMap;
use ferrum_kernel::{Db, Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const CHANNEL: &str = "ferrum_plugin_ws";
/// NOTIFY payloads are capped at 8000 bytes; keep a margin for the envelope.
pub const MAX_CROSS_REPLICA_MESSAGE: usize = 7_000;
/// Messages waiting for one socket. Memory is bounded by this and the 1 MB message limit.
pub const QUEUE: usize = 256;
/// Bytes waiting across every socket on a replica. [`QUEUE`] bounds one slow client, and this bounds
/// many at once. Past it, a socket with anything still waiting is treated as if its queue were full.
pub const QUEUED_BYTES: usize = 256 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Text(String),
    Close(String),
}

/// What a full queue means for a message, said by whoever sends it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Overflow {
    /// The message matters: the socket is closed, so its peer reconnects and resynchronises, and the
    /// sender is told. It is never dropped silently.
    #[default]
    Fail,
    /// A snapshot the next one replaces: dropped and counted.
    Drop,
}

/// What became of a message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sent {
    /// In this socket's queue on this replica.
    Queued,
    /// Published for the replica that holds it, or for none: nobody can tell.
    Published,
    /// Dropped under [`Overflow::Drop`].
    Dropped,
}

enum Local {
    Queued,
    Full,
    Absent,
}

fn weight(msg: &Outgoing) -> usize {
    match msg {
        Outgoing::Text(t) | Outgoing::Close(t) => t.len(),
    }
}

/// What a socket's writer reads. Whatever it never reads comes off the replica's total when it goes.
pub struct Outbox {
    rx: mpsc::Receiver<Outgoing>,
    queued: Arc<AtomicUsize>,
}

impl Outbox {
    pub async fn recv(&mut self) -> Option<Outgoing> {
        let msg = self.rx.recv().await?;
        self.queued.fetch_sub(weight(&msg), Ordering::Relaxed);
        Some(msg)
    }

    fn try_recv(&mut self) -> std::result::Result<Outgoing, mpsc::error::TryRecvError> {
        let msg = self.rx.try_recv()?;
        self.queued.fetch_sub(weight(&msg), Ordering::Relaxed);
        Ok(msg)
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        self.rx.close();
        while self.try_recv().is_ok() {}
    }
}

pub struct SocketHub {
    local: DashMap<Uuid, (Uuid, mpsc::Sender<Outgoing>)>,
    dropped: AtomicU64,
    closed_slow: AtomicU64,
    queued: Arc<AtomicUsize>,
    budget: usize,
}

impl Default for SocketHub {
    fn default() -> Self {
        SocketHub { local: DashMap::new(), dropped: AtomicU64::new(0), closed_slow: AtomicU64::new(0), queued: Arc::default(), budget: QUEUED_BYTES }
    }
}

impl SocketHub {
    pub fn new() -> Arc<SocketHub> {
        Arc::new(SocketHub::default())
    }

    /// A socket opened on this replica; its writer task reads the receiver.
    pub fn register(&self, tenant: Uuid, connection: Uuid) -> Outbox {
        let (tx, rx) = mpsc::channel(QUEUE);
        self.local.insert(connection, (tenant, tx));
        Outbox { rx, queued: self.queued.clone() }
    }

    pub fn unregister(&self, connection: Uuid) {
        self.local.remove(&connection);
    }

    pub fn local_count(&self) -> usize {
        self.local.len()
    }

    /// Bytes waiting for sockets on this replica.
    pub fn queued_bytes(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    /// Snapshots dropped, and sockets closed for not keeping up, since this replica started.
    pub fn overflow_counts(&self) -> (u64, u64) {
        (self.dropped.load(Ordering::Relaxed), self.closed_slow.load(Ordering::Relaxed))
    }

    fn deliver_local(&self, tenant: Uuid, connection: Uuid, msg: Outgoing) -> Local {
        let Some(entry) = self.local.get(&connection) else { return Local::Absent };
        if entry.0 != tenant {
            return Local::Absent;
        }
        // A socket keeping up has nothing waiting, and is not to blame for another's backlog.
        let bytes = weight(&msg);
        let behind = entry.1.capacity() < entry.1.max_capacity();
        if behind && self.queued.load(Ordering::Relaxed) + bytes > self.budget {
            return Local::Full;
        }
        // Counted first: the writer may have it before this line would otherwise run.
        self.queued.fetch_add(bytes, Ordering::Relaxed);
        let sent = entry.1.try_send(msg);
        if sent.is_err() {
            self.queued.fetch_sub(bytes, Ordering::Relaxed);
        }
        match sent {
            Ok(()) => Local::Queued,
            Err(TrySendError::Full(_)) => Local::Full,
            Err(TrySendError::Closed(_)) => Local::Absent,
        }
    }

    /// Dropping the sender ends the writer loop once its queue drains, which closes the socket.
    fn close_slow(&self, connection: Uuid) {
        self.local.remove(&connection);
        self.closed_slow.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(%connection, "socket closed: it was not keeping up");
    }

    fn overflowed(&self, connection: Uuid, overflow: Overflow) -> Result<Sent> {
        match overflow {
            Overflow::Drop => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(Sent::Dropped)
            }
            Overflow::Fail => {
                self.close_slow(connection);
                Err(Error::Unavailable("this socket is not keeping up and was closed".into()))
            }
        }
    }

    /// Sends to a connection on any replica. Delivery to other replicas is best effort: a socket
    /// that closed in the meantime simply never receives it.
    pub async fn send(&self, db: &Db, tenant: Uuid, connection: Uuid, msg: Outgoing, overflow: Overflow) -> Result<Sent> {
        match self.deliver_local(tenant, connection, msg.clone()) {
            Local::Queued => return Ok(Sent::Queued),
            Local::Full => return self.overflowed(connection, overflow),
            Local::Absent => {}
        }
        let (kind, text) = match &msg {
            Outgoing::Text(t) => ("text", t.as_str()),
            Outgoing::Close(r) => ("close", r.as_str()),
        };
        if text.len() > MAX_CROSS_REPLICA_MESSAGE {
            return Err(Error::invalid(format!("messages to sockets on other replicas are limited to {MAX_CROSS_REPLICA_MESSAGE} bytes")));
        }
        let drop = matches!(overflow, Overflow::Drop);
        let payload = json!({"tenant": tenant, "connection": connection, "kind": kind, "text": text, "drop": drop}).to_string();
        let mut tx = db.system_tx().await?;
        sqlx::query("select pg_notify($1, $2)").bind(CHANNEL).bind(payload).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Sent::Published)
    }

    /// Delivers messages that other replicas published for sockets held here.
    pub fn spawn_listener(self: &Arc<Self>, db: Db, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                let mut listener = match db.listen(CHANNEL).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("socket listener: {e}");
                        tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => continue }
                    }
                };
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        n = ferrum_kernel::db::notification(&mut listener) => match n {
                            Ok(n) => hub.on_notification(n.payload()),
                            Err(e) => {
                                tracing::warn!("socket listener lost its connection: {e}");
                                break;
                            }
                        },
                    }
                }
            }
        })
    }

    fn on_notification(&self, payload: &str) {
        let Ok(v) = serde_json::from_str::<Value>(payload) else { return };
        let (Some(tenant), Some(connection)) =
            (v.get("tenant").and_then(Value::as_str).and_then(|s| s.parse().ok()), v.get("connection").and_then(Value::as_str).and_then(|s| s.parse().ok()))
        else {
            return;
        };
        let text = v.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
        let msg = if v.get("kind").and_then(Value::as_str) == Some("close") { Outgoing::Close(text) } else { Outgoing::Text(text) };
        let overflow = if v.get("drop").and_then(Value::as_bool).unwrap_or(false) { Overflow::Drop } else { Overflow::Fail };
        if let Local::Full = self.deliver_local(tenant, connection, msg) {
            // The sender has gone: the error has nobody to reach, the close still protects the replica.
            let _ = self.overflowed(connection, overflow);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(hub: &SocketHub, tenant: Uuid, connection: Uuid) {
        for i in 0..QUEUE {
            assert!(matches!(hub.deliver_local(tenant, connection, Outgoing::Text(i.to_string())), Local::Queued));
        }
    }

    #[test]
    fn a_full_queue_closes_the_socket_and_tells_the_sender() {
        let hub = SocketHub::default();
        let (tenant, connection) = (Uuid::now_v7(), Uuid::now_v7());
        let mut rx = hub.register(tenant, connection);
        fill(&hub, tenant, connection);
        assert!(matches!(hub.deliver_local(tenant, connection, Outgoing::Text("late".into())), Local::Full));
        assert!(matches!(hub.overflowed(connection, Overflow::Fail), Err(Error::Unavailable(_))));
        assert_eq!(hub.local_count(), 0, "closed, not held open behind a full queue");
        assert_eq!(hub.overflow_counts(), (0, 1));
        // What was queued still reaches the writer, which then sees the end and closes the socket.
        for _ in 0..QUEUE {
            assert!(rx.try_recv().is_ok());
        }
        assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)));
    }

    #[test]
    fn a_snapshot_is_dropped_and_counted_and_the_socket_stays() {
        let hub = SocketHub::default();
        let (tenant, connection) = (Uuid::now_v7(), Uuid::now_v7());
        let _rx = hub.register(tenant, connection);
        fill(&hub, tenant, connection);
        assert_eq!(hub.overflowed(connection, Overflow::Drop).unwrap(), Sent::Dropped);
        assert_eq!(hub.local_count(), 1);
        assert_eq!(hub.overflow_counts(), (1, 0));
    }

    #[test]
    fn a_replica_holds_only_so_much_for_all_its_sockets() {
        let hub = SocketHub { budget: 10, ..Default::default() };
        let (tenant, slow, quick) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let mut behind = hub.register(tenant, slow);
        let _quick = hub.register(tenant, quick);
        let text = |n: usize| Outgoing::Text("x".repeat(n));
        assert!(matches!(hub.deliver_local(tenant, slow, text(8)), Local::Queued), "under the budget");
        assert!(matches!(hub.deliver_local(tenant, slow, text(8)), Local::Full), "past it, a socket with a backlog is full");
        assert!(matches!(hub.deliver_local(tenant, quick, text(8)), Local::Queued), "and one keeping up is not punished for it");
        assert_eq!(hub.queued_bytes(), 16);
        behind.try_recv().unwrap();
        assert_eq!(hub.queued_bytes(), 8, "what left no longer counts");
        assert!(matches!(hub.deliver_local(tenant, slow, text(2)), Local::Queued));
        assert_eq!(hub.queued_bytes(), 10);
        drop(behind);
        assert_eq!(hub.queued_bytes(), 8, "nor does what a closed socket left behind");
    }

    #[test]
    fn another_workspace_cannot_reach_a_socket() {
        let hub = SocketHub::default();
        let (tenant, connection) = (Uuid::now_v7(), Uuid::now_v7());
        let _rx = hub.register(tenant, connection);
        assert!(matches!(hub.deliver_local(Uuid::now_v7(), connection, Outgoing::Text("x".into())), Local::Absent));
    }
}
