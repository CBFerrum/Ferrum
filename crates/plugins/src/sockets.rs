//! WebSocket connections held by plugins (device protocols such as OCPP, live feeds). Each
//! replica keeps its own sockets; a message for a socket on another replica travels through
//! PostgreSQL `NOTIFY`, so plugins address connections by id without knowing where they live.

use dashmap::DashMap;
use ferrum_kernel::{Db, Error, Result};
use serde_json::{Value, json};
use sqlx::postgres::PgListener;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const CHANNEL: &str = "ferrum_plugin_ws";
/// NOTIFY payloads are capped at 8000 bytes; keep a margin for the envelope.
pub const MAX_CROSS_REPLICA_MESSAGE: usize = 7_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Text(String),
    Close(String),
}

#[derive(Default)]
pub struct SocketHub {
    local: DashMap<Uuid, (Uuid, mpsc::UnboundedSender<Outgoing>)>,
}

impl SocketHub {
    pub fn new() -> Arc<SocketHub> {
        Arc::new(SocketHub::default())
    }

    /// A socket opened on this replica; its writer task reads the receiver.
    pub fn register(&self, tenant: Uuid, connection: Uuid) -> mpsc::UnboundedReceiver<Outgoing> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.local.insert(connection, (tenant, tx));
        rx
    }

    pub fn unregister(&self, connection: Uuid) {
        self.local.remove(&connection);
    }

    pub fn local_count(&self) -> usize {
        self.local.len()
    }

    fn deliver_local(&self, tenant: Uuid, connection: Uuid, msg: Outgoing) -> bool {
        match self.local.get(&connection) {
            Some(entry) if entry.0 == tenant => entry.1.send(msg).is_ok(),
            _ => false,
        }
    }

    /// Sends to a connection on any replica. Delivery to other replicas is best effort: a socket
    /// that closed in the meantime simply never receives it.
    pub async fn send(&self, db: &Db, tenant: Uuid, connection: Uuid, msg: Outgoing) -> Result<()> {
        if self.deliver_local(tenant, connection, msg.clone()) {
            return Ok(());
        }
        let (kind, text) = match &msg {
            Outgoing::Text(t) => ("text", t.as_str()),
            Outgoing::Close(r) => ("close", r.as_str()),
        };
        if text.len() > MAX_CROSS_REPLICA_MESSAGE {
            return Err(Error::invalid(format!("messages to sockets on other replicas are limited to {MAX_CROSS_REPLICA_MESSAGE} bytes")));
        }
        let payload = json!({"tenant": tenant, "connection": connection, "kind": kind, "text": text}).to_string();
        let mut tx = db.system_tx().await?;
        sqlx::query("select pg_notify($1, $2)").bind(CHANNEL).bind(payload).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Delivers messages that other replicas published for sockets held here.
    pub fn spawn_listener(self: &Arc<Self>, db: Db, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                let mut listener = match PgListener::connect_with(&db.pool).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("socket listener: {e}");
                        tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => continue }
                    }
                };
                if let Err(e) = listener.listen(CHANNEL).await {
                    tracing::warn!("socket listener: {e}");
                    continue;
                }
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        n = listener.recv() => match n {
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
        self.deliver_local(tenant, connection, msg);
    }
}
