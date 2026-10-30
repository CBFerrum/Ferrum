//! Live updates over one WebSocket per signed-in client (`/ws/live`). A client subscribes to
//! entities and receives `{"event", "entity", "id", "actor"}` for every committed change it may
//! read, plus its in-app notifications and package pushes (`{"push": {"kind", "data"}}`, e.g. chat).
//! Messages carry ids, not data: the client refetches through
//! the API, so field permissions still apply. Changes reach every replica over PostgreSQL NOTIFY
//! and each replica checks permissions for the sockets it holds.
//!
//! Protocol: the first message is `{"auth": "<token>"}` (tokens stay out of URLs and logs; an
//! `Authorization` header works too), answered by `{"ready": true}`. Then
//! `{"subscribe": ["SalesOrder"]}` / `{"unsubscribe": [...]}` → `{"subscribed": [...], "denied": [...]}`.

use crate::error::{ApiError, ApiResult};
use crate::extract::ctx_from_token;
use crate::state::{AppState, app_state};
use async_trait::async_trait;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use dashmap::DashMap;
use ferrum_kernel::notify::LIVE_CHANNEL;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Action, Ctx, EventSink, Kernel, row_scope};
use futures_util::{SinkExt, StreamExt};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Value, json};
use sqlx::postgres::PgListener;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const MAX_SUBSCRIPTIONS: usize = 100;
/// Messages buffered for a slow client; beyond that it is disconnected and resyncs on reconnect.
const BUFFER: usize = 512;
const KEEPALIVE: Duration = Duration::from_secs(30);
/// Re-validates the session: revoked or expired tokens close the socket, role changes apply.
const RECHECK: Duration = Duration::from_secs(60);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_BURST: u32 = 30;
const CLIENT_WINDOW: Duration = Duration::from_secs(10);

struct Conn {
    ctx: Ctx,
    entities: HashSet<String>,
    tx: mpsc::Sender<String>,
}

#[derive(Default)]
pub struct LiveHub {
    conns: DashMap<Uuid, Conn>,
}

impl LiveHub {
    pub fn new() -> Arc<LiveHub> {
        Arc::new(LiveHub::default())
    }

    fn register(&self, id: Uuid, ctx: Ctx) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(BUFFER);
        self.conns.insert(id, Conn { ctx, entities: HashSet::new(), tx });
        rx
    }

    fn unregister(&self, id: Uuid) {
        self.conns.remove(&id);
    }

    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    /// Applies a subscription change; returns (granted, denied) for the requested additions.
    fn subscribe(&self, id: Uuid, add: &[String], remove: &[String]) -> (Vec<String>, Vec<String>) {
        let Some(mut c) = self.conns.get_mut(&id) else { return (vec![], add.to_vec()) };
        for e in remove {
            c.entities.remove(e);
        }
        let (mut granted, mut denied) = (vec![], vec![]);
        for e in add {
            let readable = c.ctx.schema.entity(e).is_some() && c.ctx.require(e, Action::Read).is_ok();
            if readable && (c.entities.contains(e) || c.entities.len() < MAX_SUBSCRIPTIONS) {
                c.entities.insert(e.clone());
                granted.push(e.clone());
            } else {
                denied.push(e.clone());
            }
        }
        (granted, denied)
    }

    fn set_ctx(&self, id: Uuid, ctx: Ctx) {
        if let Some(mut c) = self.conns.get_mut(&id) {
            // Entities no longer readable are dropped with the old permissions.
            c.entities.retain(|e| ctx.require(e, Action::Read).is_ok());
            c.ctx = ctx;
        }
    }

    /// Queues a message; a client that cannot keep up is disconnected (its receiver closes).
    fn push(&self, id: Uuid, tx: &mpsc::Sender<String>, msg: &str) {
        if tx.try_send(msg.to_string()).is_err() {
            self.unregister(id);
        }
    }

    async fn dispatch(&self, k: &Kernel, payload: &str) {
        let Ok(v) = serde_json::from_str::<Value>(payload) else { return };
        let Some(tenant) = v.get("tenant").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return };
        // Messages for one user: notifications and package pushes (chat...).
        let personal = v.get("notification").map(|n| json!({"notification": n})).or_else(|| v.get("push").map(|p| json!({"push": p})));
        if let Some(msg) = personal {
            let Some(user) = v.get("user").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return };
            let msg = msg.to_string();
            let targets: Vec<(Uuid, mpsc::Sender<String>)> =
                self.conns.iter().filter(|c| c.ctx.tenant == tenant && c.ctx.user == user).map(|c| (*c.key(), c.tx.clone())).collect();
            for (id, tx) in targets {
                self.push(id, &tx, &msg);
            }
            return;
        }
        let (Some(entity), Some(record)) = (v.get("entity").and_then(Value::as_str), v.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()))
        else {
            return;
        };
        let msg = json!({"event": v.get("event"), "entity": entity, "id": record, "actor": v.get("actor")}).to_string();
        // Guards are not held across awaits: collect the candidates first.
        let targets: Vec<(Uuid, Ctx, mpsc::Sender<String>)> =
            self.conns.iter().filter(|c| c.ctx.tenant == tenant && c.entities.contains(entity)).map(|c| (*c.key(), c.ctx.clone(), c.tx.clone())).collect();
        for (id, ctx, tx) in targets {
            let Some(ent) = ctx.schema.entity(entity) else { continue };
            let visible = match row_scope(&ctx, ent, Action::Read) {
                Ok(scope) if scope.is_unrestricted() => true,
                // Own-record or scoped access: only what this user can actually open.
                Ok(_) => k.get(&ctx, entity, record, false).await.is_ok(),
                Err(_) => false,
            };
            if visible {
                self.push(id, &tx, &msg);
            }
        }
    }

    /// Delivers changes announced by any replica to the sockets held here.
    pub fn spawn_listener(self: &Arc<Self>, k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                let mut listener = match PgListener::connect_with(&k.db.pool).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("live listener: {e}");
                        tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(Duration::from_secs(2)) => continue }
                    }
                };
                if let Err(e) = listener.listen(LIVE_CHANNEL).await {
                    tracing::warn!("live listener: {e}");
                    tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(Duration::from_secs(2)) => continue }
                }
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        n = listener.recv() => match n {
                            Ok(n) => hub.dispatch(&k, n.payload()).await,
                            Err(e) => {
                                tracing::warn!("live listener lost its connection: {e}");
                                break;
                            }
                        },
                    }
                }
            }
        })
    }
}

/// Announces committed record changes on the live channel (best effort: a missed announcement
/// only means a client refreshes later; it must not make the outbox redeliver the event).
pub struct LiveEvents;

#[async_trait]
impl EventSink for LiveEvents {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> ferrum_kernel::Result<()> {
        let (Some(entity), Some(id)) = (&ev.entity, ev.record_id) else { return Ok(()) };
        let payload = json!({"tenant": ev.tenant_id, "entity": entity, "id": id, "event": ev.topic, "actor": ev.payload.get("actor")}).to_string();
        if let Err(e) = sqlx::query("select pg_notify($1, $2)").bind(LIVE_CHANNEL).bind(payload).execute(&k.db.pool).await {
            tracing::warn!(topic = %ev.topic, "live announcement failed: {e}");
        }
        Ok(())
    }
}

async fn live(State(app): State<AppContext>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let pre = crate::extract::socket_credential(&st, &headers).await?;
    Ok(upgrade.max_message_size(16 << 10).on_upgrade(move |ws| serve(st, pre, ws)))
}

fn close(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame { code, reason: reason.to_string().into() }))
}

fn names(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

async fn serve(st: AppState, pre: Option<(String, Ctx)>, ws: WebSocket) {
    let (mut sink, mut stream) = ws.split();
    let (token, ctx) = match pre {
        Some(p) => p,
        None => {
            let first = tokio::time::timeout(AUTH_TIMEOUT, stream.next()).await;
            let token = match first {
                Ok(Some(Ok(Message::Text(t)))) => {
                    serde_json::from_str::<Value>(&t).ok().and_then(|v| v.get("auth").and_then(Value::as_str).map(str::to_string))
                }
                _ => None,
            };
            let Some(token) = token else {
                let _ = sink.send(close(4401, "send {\"auth\": token} first")).await;
                return;
            };
            match ctx_from_token(&st, &token, Uuid::now_v7().to_string()).await {
                Ok(ctx) => (token, ctx),
                Err(ApiError(e)) => {
                    // Close reasons are limited to 123 bytes.
                    let reason: String = e.to_string().chars().take(100).collect();
                    let _ = sink.send(close(4401, &reason)).await;
                    return;
                }
            }
        }
    };
    let id = Uuid::now_v7();
    let mut outgoing = st.live.register(id, ctx.clone());
    if sink.send(Message::Text(json!({"ready": true, "user": ctx.user}).to_string().into())).await.is_err() {
        st.live.unregister(id);
        return;
    }
    let (mut window_start, mut in_window) = (Instant::now(), 0u32);
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    let mut recheck = tokio::time::interval(RECHECK);
    keepalive.tick().await;
    recheck.tick().await;
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if window_start.elapsed() > CLIENT_WINDOW {
                        (window_start, in_window) = (Instant::now(), 0);
                    }
                    in_window += 1;
                    if in_window > CLIENT_BURST {
                        let _ = sink.send(close(1008, "too many messages")).await;
                        break;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(&text) else {
                        let _ = sink.send(close(1003, "messages are JSON")).await;
                        break;
                    };
                    let (granted, denied) = st.live.subscribe(id, &names(v.get("subscribe")), &names(v.get("unsubscribe")));
                    let reply = json!({"subscribed": granted, "denied": denied}).to_string();
                    if sink.send(Message::Text(reply.into())).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    let _ = sink.send(close(1003, "text messages only")).await;
                    break;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            },
            out = outgoing.recv() => match out {
                Some(text) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = sink.send(close(1013, "too slow; reconnect")).await;
                    break;
                }
            },
            _ = keepalive.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            _ = recheck.tick() => match ctx_from_token(&st, &token, Uuid::now_v7().to_string()).await {
                Ok(ctx) => st.live.set_ctx(id, ctx),
                Err(_) => {
                    let _ = sink.send(close(4401, "session expired")).await;
                    break;
                }
            },
        }
    }
    st.live.unregister(id);
}

pub fn routes() -> Routes {
    Routes::new().add("/ws/live", axum::routing::get(live))
}
