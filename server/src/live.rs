//! Live updates over one WebSocket per signed-in client (`/ws/live`). A client subscribes to
//! entities and receives `{"event", "entity", "id", "actor"}` for every committed change it may
//! read, plus its in-app notifications and package pushes (`{"push": {"kind", "data"}}`, e.g. chat).
//! Messages carry ids, not data: the client refetches through
//! the API, so field permissions still apply. Changes reach every replica over PostgreSQL NOTIFY
//! and each replica checks permissions for the sockets it holds: in memory for who may read the entity, and with one
//! read of the record per announcement for those who may read only some of its records.
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
use ferrum_kernel::ctx::RowScope;
use ferrum_kernel::notify::LIVE_CHANNEL;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Action, Ctx, Doc, EventSink, Kernel, row_scope, store};
use ferrum_meta::{EffEntity, EntityKind, Schema};
use futures_util::{SinkExt, StreamExt};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const MAX_SUBSCRIPTIONS: usize = 100;
/// Messages buffered for a slow client; beyond that it is disconnected and resyncs on reconnect.
const BUFFER: usize = 512;
/// Announcements read and waiting for delivery on this replica; beyond that they are dropped.
const QUEUE: usize = 4096;
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

    /// Holds a socket's connection `id` as `ctx` (the socket's own context, in the zone it was opened in): what is
    /// pushed to it arrives on the receiver.
    pub fn register(&self, id: Uuid, ctx: Ctx) -> mpsc::Receiver<String> {
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
    pub fn subscribe(&self, id: Uuid, add: &[String], remove: &[String]) -> (Vec<String>, Vec<String>) {
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

    /// Delivers one announcement of the live channel (as `pg_notify` carries it) to the sockets held here.
    pub async fn dispatch(&self, k: &Kernel, payload: &str) {
        let Ok(v) = serde_json::from_str::<Value>(payload) else { return };
        let Some(tenant) = v.get("tenant").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return };
        // Messages for one user: notifications and package pushes (chat...).
        let personal = v.get("notification").map(|n| json!({"notification": n})).or_else(|| v.get("push").map(|p| json!({"push": p})));
        if let Some(msg) = personal {
            let Some(user) = v.get("user").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return };
            let msg = msg.to_string();
            // A notification about a record of an app the socket's zone leaves out (crate::zones, level 2) is not pushed.
            let about = v.pointer("/notification/entity").and_then(Value::as_str);
            let targets: Vec<(Uuid, mpsc::Sender<String>)> = self
                .conns
                .iter()
                .filter(|c| c.ctx.tenant == tenant && c.ctx.user == user && about.is_none_or(|e| c.ctx.sees(e)))
                .map(|c| (*c.key(), c.tx.clone()))
                .collect();
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
        // Who may read the entity at all, in their zone (`row_scope` asks `Ctx::require`), in memory. Guards are not
        // held across awaits or pushes: collect first.
        let (mut whole, mut scoped) = (vec![], vec![]);
        for c in self.conns.iter().filter(|c| c.ctx.tenant == tenant && c.entities.contains(entity)) {
            let Some(ent) = c.ctx.schema.entity(entity) else { continue };
            match row_scope(&c.ctx, ent, Action::Read) {
                Ok(scope) if scope.is_unrestricted() => whole.push((*c.key(), c.tx.clone())),
                Ok(scope) => scoped.push(Scoped { id: *c.key(), tx: c.tx.clone(), schema: c.ctx.schema.clone(), ent: ent.clone(), scope }),
                Err(_) => {}
            }
        }
        for (id, tx) in whole {
            self.push(id, &tx, &msg);
        }
        if scoped.is_empty() {
            return;
        }
        // Own-record or scoped access: only what each can actually open, with the record read once.
        match admitted(k, tenant, record, &scoped).await {
            Ok(seen) => {
                for (s, yes) in scoped.iter().zip(seen) {
                    if yes {
                        self.push(s.id, &s.tx, &msg);
                    }
                }
            }
            Err(e) => read_failed(&e),
        }
    }

    /// Delivers changes announced by any replica to the sockets held here. The listener only queues what it reads;
    /// one task delivers it, so reading a record never holds up the announcements behind it. A full queue drops the
    /// announcement ([`dropped`]): a missed one only means a client refreshes later.
    pub fn spawn_listener(self: &Arc<Self>, k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let (queue, mut queued) = mpsc::channel::<String>(QUEUE);
        let (hub, dk) = (self.clone(), k.clone());
        // Ends once the listener has stopped and the queue is empty.
        tokio::spawn(async move {
            while let Some(payload) = queued.recv().await {
                hub.dispatch(&dk, &payload).await;
            }
        });
        tokio::spawn(async move {
            loop {
                let mut listener = match k.db.listen(LIVE_CHANNEL).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("live listener: {e}");
                        tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(Duration::from_secs(2)) => continue }
                    }
                };
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        n = ferrum_kernel::db::notification(&mut listener) => match n {
                            Ok(n) => {
                                if queue.try_send(n.payload().to_string()).is_err() {
                                    DROPPED.fetch_add(1, Ordering::Relaxed);
                                }
                            }
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

/// A socket whose access to the announced entity is partial, as `dispatch` found it.
struct Scoped {
    id: Uuid,
    tx: mpsc::Sender<String>,
    schema: Arc<Schema>,
    ent: Arc<EffEntity>,
    scope: RowScope,
}

/// Which of `scoped` may open `record`, as [`Kernel::get`] decides past the checks `dispatch` made in memory: not a
/// child or ledger entity (which `get` never reads), the record as a merged id became, there, and admitted by the
/// socket's scope. The record is read once per schema the sockets hold (one, but for a moment after a change), and a
/// scope several sockets share (one person's tabs; equal by `==`) is tested once. Nothing is recorded: the push names the record,
/// never its fields.
async fn admitted(k: &Kernel, tenant: Uuid, record: Uuid, scoped: &[Scoped]) -> ferrum_kernel::Result<Vec<bool>> {
    let mut tx = k.db.tenant_tx(tenant).await?;
    let mut docs: Vec<(Arc<Schema>, Option<Doc>)> = vec![];
    // ponytail: searched linearly; distinct scopes per announcement are few (one per person with partial access).
    let mut tested: Vec<(usize, &RowScope, bool)> = vec![];
    let mut out = Vec::with_capacity(scoped.len());
    for s in scoped {
        if matches!(s.ent.def.kind, EntityKind::Child | EntityKind::Ledger) {
            out.push(false);
            continue;
        }
        let i = match docs.iter().position(|(schema, _)| Arc::ptr_eq(schema, &s.schema)) {
            Some(i) => i,
            None => {
                let name = &s.ent.def.name;
                let id = if s.ent.def.mergeable { ferrum_kernel::merge::canonical(&mut tx, tenant, name, record).await?.unwrap_or(record) } else { record };
                docs.push((s.schema.clone(), store::load(&mut tx, &s.schema, &s.ent, tenant, id, false).await?));
                docs.len() - 1
            }
        };
        let Some(doc) = &docs[i].1 else {
            out.push(false);
            continue;
        };
        // Equal scopes (`==`, field by field) against the same record get the same answer.
        let yes = match tested.iter().find(|(j, scope, _)| *j == i && *scope == &s.scope) {
            Some((_, _, yes)) => *yes,
            None => {
                let yes = s.scope.admits(&mut tx, tenant, doc).await?;
                tested.push((i, &s.scope, yes));
                yes
            }
        };
        out.push(yes);
    }
    tx.rollback().await?;
    Ok(out)
}

/// Announcements read from the live channel and dropped because the queue to delivery was full.
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// When a failed read of an announced record was last logged.
static READ_FAILED: Mutex<Option<Instant>> = Mutex::new(None);

/// How many announcements this replica has dropped since it started (its queue to delivery was full).
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Logs, at most once a minute, that an announced record could not be read: only sockets with whole access were told.
fn read_failed(e: &ferrum_kernel::Error) {
    let mut last = READ_FAILED.lock().unwrap_or_else(|p| p.into_inner());
    if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
        *last = Some(Instant::now());
        tracing::warn!("live: an announced record could not be read, so sockets with partial access were not told: {e}");
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
    let entry = crate::extract::socket_entry(&st, &headers).await;
    let pre = crate::extract::socket_credential(&st, &headers, &entry).await?;
    Ok(upgrade.max_message_size(16 << 10).on_upgrade(move |ws| serve(st, pre, entry, ws)))
}

fn close(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame { code, reason: reason.to_string().into() }))
}

fn names(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

async fn serve(st: AppState, pre: Option<(String, Ctx)>, entry: std::sync::Arc<ferrum_kernel::zones::Entry>, ws: WebSocket) {
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
            match ctx_from_token(&st, &token, Uuid::now_v7().to_string(), &entry, false).await {
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
            _ = recheck.tick() => match ctx_from_token(&st, &token, Uuid::now_v7().to_string(), &entry, false).await {
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
